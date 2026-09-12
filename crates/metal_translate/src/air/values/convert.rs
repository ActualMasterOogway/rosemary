//! LLVM cast lowering and the pointer adaptations a widening bitcast forces.

use super::*;

/// LLVM cast opcodes. Each becomes an explicit Metal IR conversion; the IR
/// verifier then checks the operand and result shapes.
/// Decode an LLVM cast opcode and check that it agrees with the operand and
/// result types. Vector casts convert lanewise and keep their lane count.
pub(super) fn cast_operation(
    opcode: u64,
    source: usize,
    target: usize,
    types: &[DecodedType],
) -> Result<ConvertOp, TranslateError> {
    let op = match opcode {
        0 => ConvertOp::IntegerTruncate,
        1 => ConvertOp::ZeroExtend,
        2 => ConvertOp::SignExtend,
        3 => ConvertOp::FloatToUnsigned,
        4 => ConvertOp::FloatToSigned,
        5 => ConvertOp::UnsignedToFloat,
        6 => ConvertOp::SignedToFloat,
        7 | 8 => ConvertOp::FloatResize,
        11 => ConvertOp::Bitcast,
        _ => return Err(semantic(format!("unsupported LLVM cast opcode {opcode}"))),
    };
    // A pointer cast reinterprets addressable memory. SPIR-V's logical
    // addressing has no such operation; the body decoder handles the widening
    // form separately and anything else is rejected here.
    if matches!(types[source], DecodedType::Pointer { .. })
        || matches!(types[target], DecodedType::Pointer { .. })
    {
        return Err(semantic(
            "LLVM pointer cast needs explicit buffer reinterpretation",
        ));
    }
    // A bitcast reinterprets bits, so it alone may change the lane count as
    // long as the value's total width is unchanged.
    if op == ConvertOp::Bitcast {
        let width = |index: usize| -> Option<u32> {
            match &types[index] {
                DecodedType::Integer(bits) | DecodedType::Float(bits) => Some(u32::from(*bits)),
                DecodedType::Vector { element, lanes } => match &types[*element] {
                    DecodedType::Integer(bits) | DecodedType::Float(bits) => {
                        Some(u32::from(*bits) * lanes)
                    }
                    _ => None,
                },
                _ => None,
            }
        };
        return match (width(source), width(target)) {
            (Some(from), Some(to)) if from == to => Ok(op),
            _ => Err(semantic(
                "LLVM bitcast changes a value's total width or is not numeric",
            )),
        };
    }
    let (source, target) = match (&types[source], &types[target]) {
        (
            DecodedType::Vector {
                element: source,
                lanes,
            },
            DecodedType::Vector {
                element: target,
                lanes: target_lanes,
            },
        ) if lanes == target_lanes => (&types[*source], &types[*target]),
        (DecodedType::Vector { .. }, _) | (_, DecodedType::Vector { .. }) => {
            return Err(semantic("LLVM cast changes a value's lane count"));
        }
        (source, target) => (source, target),
    };
    let valid = match (op, source, target) {
        (ConvertOp::IntegerTruncate, DecodedType::Integer(from), DecodedType::Integer(to)) => {
            to < from
        }
        (
            ConvertOp::ZeroExtend | ConvertOp::SignExtend,
            DecodedType::Integer(from),
            DecodedType::Integer(to),
        ) => to > from,
        (
            ConvertOp::FloatToUnsigned | ConvertOp::FloatToSigned,
            DecodedType::Float(_),
            DecodedType::Integer(_),
        )
        | (
            ConvertOp::UnsignedToFloat | ConvertOp::SignedToFloat,
            DecodedType::Integer(_),
            DecodedType::Float(_),
        ) => true,
        (ConvertOp::FloatResize, DecodedType::Float(from), DecodedType::Float(to)) => from != to,
        (ConvertOp::Bitcast, source, target) => scalar_width(source) == scalar_width(target),
        _ => false,
    };
    if !valid {
        return Err(semantic(format!(
            "LLVM cast opcode {opcode} does not apply to its operand and result types"
        )));
    }
    Ok(op)
}

pub(super) fn scalar_width(ty: &DecodedType) -> Option<u8> {
    match ty {
        DecodedType::Integer(bits) | DecodedType::Float(bits) => Some(*bits),
        _ => None,
    }
}

/// A pointer whose LLVM type was widened by a bitcast. `pointee` is the type
/// the address really holds.
pub(super) struct PointerAlias {
    pub(super) pointee: usize,
    pub(super) operand: Operand,
}

/// How a value loaded through a reinterpreted pointer is adapted to the type
/// the load asked for.
#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum Adaptation {
    None,
    /// Metal pads a `float3` to sixteen bytes, and Apple's compiler reads
    /// such a member with a wider vector load.
    Widen,
    /// The same storage read as a different numeric type of equal width.
    Bitcast,
}

/// Decide how a pointer reinterpretation can be honoured without changing a
/// SPIR-V logical pointer's pointee type.
pub(super) fn reinterpretation(
    types: &[DecodedType],
    from: usize,
    to: usize,
) -> Result<Adaptation, TranslateError> {
    if from == to {
        return Ok(Adaptation::None);
    }
    let shape = |index: usize| match &types[index] {
        DecodedType::Vector { element, lanes } => (*element, *lanes),
        _ => (index, 1),
    };
    let (source, source_lanes) = shape(from);
    let (target, target_lanes) = shape(to);
    if source == target && source_lanes < target_lanes {
        return Ok(Adaptation::Widen);
    }
    let width = |index: usize| match types[index] {
        DecodedType::Integer(bits) | DecodedType::Float(bits) => Some(bits),
        _ => None,
    };
    if source_lanes == target_lanes && width(source).is_some() && width(source) == width(target) {
        return Ok(Adaptation::Bitcast);
    }
    Err(semantic(format!(
        "LLVM pointer cast needs explicit buffer reinterpretation: {} -> {}",
        types[from].description(),
        types[to].description(),
    )))
}

/// Adapt a value loaded through a reinterpreted pointer to the requested type.
///
/// FIXME (ACCURACY): lanes added by a widening load are undefined rather than
/// the bytes Metal would read from a `float3`'s padding. Shaders discard
/// them, and SPIR-V's logical addressing cannot name that padding.
pub(super) fn widen(
    value: Operand,
    from: usize,
    to: usize,
    types: &[DecodedType],
    function: &mut Function,
    instructions: &mut Vec<Instruction>,
) -> Result<Operand, TranslateError> {
    let target = ir_type(to, types)?;
    match reinterpretation(types, from, to)? {
        Adaptation::None => Ok(value),
        Adaptation::Bitcast => Ok(emit(
            function,
            instructions,
            target,
            InstructionKind::Convert {
                op: ConvertOp::Bitcast,
                value,
            },
        )),
        Adaptation::Widen => {
            let lanes = |index: usize| match types[index] {
                DecodedType::Vector { lanes, .. } => lanes,
                _ => 1,
            };
            let components = (0..lanes(to))
                .map(|lane| if lane < lanes(from) { lane } else { u32::MAX })
                .collect();
            let source = ir_type(from, types)?;
            Ok(emit(
                function,
                instructions,
                target,
                InstructionKind::VectorShuffle {
                    first: value,
                    second: Operand::Undef(source),
                    components,
                },
            ))
        }
    }
}

/// Adapt a value to the type the address it is stored through really holds.
///
/// The mirror of [`widen`]: where a load of a padded `float3` reads four lanes
/// and drops one, a store of four writes the three the storage has.
pub(super) fn narrow(
    value: Operand,
    from: usize,
    to: usize,
    types: &[DecodedType],
    function: &mut Function,
    instructions: &mut Vec<Instruction>,
) -> Result<Operand, TranslateError> {
    if from == to {
        return Ok(value);
    }
    let target = ir_type(to, types)?;
    // `reinterpretation` is asked in the direction the storage was declared
    // in, which is the one whose widening rule holds.
    match reinterpretation(types, to, from)? {
        Adaptation::None => Ok(value),
        Adaptation::Bitcast => Ok(emit(
            function,
            instructions,
            target,
            InstructionKind::Convert {
                op: ConvertOp::Bitcast,
                value,
            },
        )),
        Adaptation::Widen => {
            let lanes = |index: usize| match types[index] {
                DecodedType::Vector { lanes, .. } => lanes,
                _ => 1,
            };
            // A shuffle produces a vector; storage of one lane is a scalar and
            // takes the first component instead.
            if lanes(to) == 1 {
                return Ok(emit(
                    function,
                    instructions,
                    target,
                    InstructionKind::CompositeExtract {
                        composite: value,
                        index: 0,
                    },
                ));
            }
            let components = (0..lanes(to)).collect();
            let source = ir_type(from, types)?;
            Ok(emit(
                function,
                instructions,
                target,
                InstructionKind::VectorShuffle {
                    first: value,
                    second: Operand::Undef(source),
                    components,
                },
            ))
        }
    }
}
