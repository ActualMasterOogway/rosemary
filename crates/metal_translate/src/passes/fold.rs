use std::collections::BTreeSet;

use super::Substitution;
use crate::ir::{
    AddressSpace, BlockId, ConvertOp, Function, GlobalVariable, InstructionKind, IntegerBinaryOp,
    IntegerPredicate, Operand, Terminator, Type, instruction_operands_mut,
};

pub(super) fn run(function: &mut Function, globals: &[GlobalVariable]) -> bool {
    let mut substitution = Substitution::default();
    let values = &function.values;
    for block in &mut function.blocks {
        block.instructions.retain_mut(|instruction| {
            for operand in instruction_operands_mut(&mut instruction.kind) {
                substitution.resolve(operand);
            }
            let Some(result) = instruction.result else {
                return true;
            };
            let Some(ty) = values.get(result.0 as usize) else {
                return true;
            };
            match fold(&instruction.kind, ty, globals) {
                Some(folded) => {
                    substitution.insert(result, folded);
                    false
                }
                None => true,
            }
        });
    }
    let folded = !substitution.is_empty();
    substitution.apply(function);
    fold_branches(function) || folded
}

pub(super) fn fold(
    kind: &InstructionKind,
    ty: &Type,
    globals: &[GlobalVariable],
) -> Option<Operand> {
    match kind {
        // A `constant` global is never written once a function runs, so a
        // load of all of it is its initializer. That is how a specialized
        // function constant reaches the code which reads it. Only scalars and
        // vectors are folded: an aggregate constant would need a type of its
        // own at emission, and nothing reads a whole table at once.
        InstructionKind::Load {
            pointer: Operand::Global { index, .. },
        } if matches!(
            ty,
            Type::Int { .. } | Type::Float { .. } | Type::Bool | Type::Vector { .. }
        ) =>
        {
            let global = globals.get(*index as usize)?;
            let value = global.initializer.as_ref()?;
            (global.space == AddressSpace::Constant
                && !matches!(value, Operand::Undef(_))
                && constant_type(value).as_ref() == Some(ty))
            .then(|| value.clone())
        }
        InstructionKind::IntegerClamp {
            signed,
            value,
            min,
            max,
        } => {
            let Type::Int { bits, .. } = ty else {
                return None;
            };
            let bits = u32::from(*bits);
            let (value, low, high) = (
                integer(value, bits)?,
                integer(min, bits)?,
                integer(max, bits)?,
            );
            // Metal leaves `clamp` undefined when the bounds are reversed.
            let clamped = if *signed {
                let (value, low, high) = (
                    sign_extend(value, bits),
                    sign_extend(low, bits),
                    sign_extend(high, bits),
                );
                if low > high {
                    return None;
                }
                value.clamp(low, high) as u64
            } else {
                if low > high {
                    return None;
                }
                value.clamp(low, high)
            };
            Some(Operand::Integer {
                ty: ty.clone(),
                value: clamped & mask(bits),
            })
        }
        InstructionKind::IntegerBinary { op, lhs, rhs } => integer_binary(*op, lhs, rhs, ty),
        InstructionKind::IntegerCompare {
            predicate,
            lhs,
            rhs,
        } => integer_compare(*predicate, lhs, rhs),
        InstructionKind::Select {
            condition,
            on_true,
            on_false,
        } => match condition {
            Operand::Bool(true) => Some(on_true.clone()),
            Operand::Bool(false) => Some(on_false.clone()),
            _ if on_true == on_false => Some(on_true.clone()),
            _ => None,
        },
        InstructionKind::Convert { op, value } => convert(*op, value, ty),
        InstructionKind::CompositeExtract {
            composite: Operand::Composite { elements, .. },
            index,
        } => {
            let element = elements.get(*index as usize)?;
            (constant_type(element).as_ref() == Some(ty)).then(|| element.clone())
        }
        _ => None,
    }
}

/// The type of an operand which carries its own, which is every one but an
/// SSA value.
fn constant_type(operand: &Operand) -> Option<Type> {
    match operand {
        Operand::Bool(_) => Some(Type::Bool),
        Operand::Integer { ty, .. }
        | Operand::Float { ty, .. }
        | Operand::Composite { ty, .. }
        | Operand::Global { ty, .. }
        | Operand::Undef(ty) => Some(ty.clone()),
        Operand::Value(_) => None,
    }
}

fn mask(bits: u32) -> u64 {
    if bits >= 64 {
        u64::MAX
    } else {
        (1u64 << bits) - 1
    }
}

fn sign_extend(value: u64, bits: u32) -> i64 {
    if bits >= 64 {
        value as i64
    } else {
        let shift = 64 - bits;
        ((value << shift) as i64) >> shift
    }
}

fn integer(operand: &Operand, bits: u32) -> Option<u64> {
    match operand {
        Operand::Integer { value, .. } => Some(value & mask(bits)),
        _ => None,
    }
}

pub(super) fn integer_binary(
    op: IntegerBinaryOp,
    lhs: &Operand,
    rhs: &Operand,
    ty: &Type,
) -> Option<Operand> {
    use IntegerBinaryOp as Op;

    match ty {
        Type::Bool => boolean_binary(op, lhs, rhs),
        Type::Int { bits, .. } => {
            let bits = u32::from(*bits);
            let all = mask(bits);
            let constant = |value: u64| Operand::Integer {
                ty: ty.clone(),
                value: value & all,
            };
            let (a, b) = (integer(lhs, bits), integer(rhs, bits));
            if let (Some(a), Some(b)) = (a, b) {
                return evaluate(op, a, b, bits).map(constant);
            }

            // An identity keeps the other operand, whose type the verifier
            // has already made the result's.
            match (op, a, b) {
                (Op::Add | Op::Or | Op::Xor, Some(0), _) => return Some(rhs.clone()),
                (
                    Op::Add
                    | Op::Subtract
                    | Op::Or
                    | Op::Xor
                    | Op::ShiftLeft
                    | Op::ShiftRightLogical
                    | Op::ShiftRightArithmetic,
                    _,
                    Some(0),
                ) => return Some(lhs.clone()),
                (Op::Multiply, Some(1), _) => return Some(rhs.clone()),
                (Op::Multiply | Op::DivideUnsigned | Op::DivideSigned, _, Some(1)) => {
                    return Some(lhs.clone());
                }
                (Op::Multiply | Op::And, Some(0), _) | (Op::Multiply | Op::And, _, Some(0)) => {
                    return Some(constant(0));
                }
                (Op::And, Some(a), _) if a == all => return Some(rhs.clone()),
                (Op::And, _, Some(b)) if b == all => return Some(lhs.clone()),
                (Op::Or, Some(a), _) | (Op::Or, _, Some(a)) if a == all => {
                    return Some(constant(all));
                }
                _ => {}
            }

            if matches!(lhs, Operand::Value(_)) && lhs == rhs {
                return match op {
                    Op::Subtract | Op::Xor => Some(constant(0)),
                    Op::And
                    | Op::Or
                    | Op::MinimumSigned
                    | Op::MinimumUnsigned
                    | Op::MaximumSigned
                    | Op::MaximumUnsigned => Some(lhs.clone()),
                    _ => None,
                };
            }
            None
        }
        _ => None,
    }
}

/// LLVM types a Metal `bool` as `i1`, so bitwise operations stand in for
/// logical ones.
fn boolean_binary(op: IntegerBinaryOp, lhs: &Operand, rhs: &Operand) -> Option<Operand> {
    use IntegerBinaryOp as Op;

    let constant = |operand: &Operand| match operand {
        Operand::Bool(value) => Some(*value),
        _ => None,
    };
    match (op, constant(lhs), constant(rhs)) {
        (Op::And, Some(a), Some(b)) => Some(Operand::Bool(a && b)),
        (Op::Or, Some(a), Some(b)) => Some(Operand::Bool(a || b)),
        (Op::Xor, Some(a), Some(b)) => Some(Operand::Bool(a != b)),
        (Op::And, Some(true), _) | (Op::Or | Op::Xor, Some(false), _) => Some(rhs.clone()),
        (Op::And, _, Some(true)) | (Op::Or | Op::Xor, _, Some(false)) => Some(lhs.clone()),
        (Op::And, Some(false), _) | (Op::And, _, Some(false)) => Some(Operand::Bool(false)),
        (Op::Or, Some(true), _) | (Op::Or, _, Some(true)) => Some(Operand::Bool(true)),
        (Op::And | Op::Or, None, None) if matches!(lhs, Operand::Value(_)) && lhs == rhs => {
            Some(lhs.clone())
        }
        (Op::Xor, None, None) if matches!(lhs, Operand::Value(_)) && lhs == rhs => {
            Some(Operand::Bool(false))
        }
        _ => None,
    }
}

/// Evaluate an operation on two constants, or `None` where LLVM gives the
/// result no value - a division by zero, a signed division which overflows,
/// or a shift by the width or more. Those are left for the device rather than
/// given a value the source never promised.
fn evaluate(op: IntegerBinaryOp, a: u64, b: u64, bits: u32) -> Option<u64> {
    use IntegerBinaryOp as Op;

    let (sa, sb) = (sign_extend(a, bits), sign_extend(b, bits));
    let signed_overflow = sb == -1 && sa == sign_extend(1u64 << (bits - 1), bits);
    Some(match op {
        Op::Add => a.wrapping_add(b),
        Op::Subtract => a.wrapping_sub(b),
        Op::Multiply => a.wrapping_mul(b),
        Op::DivideUnsigned => a.checked_div(b)?,
        Op::RemainderUnsigned => a.checked_rem(b)?,
        Op::DivideSigned if !signed_overflow => sa.checked_div(sb)? as u64,
        Op::RemainderSigned if !signed_overflow => sa.checked_rem(sb)? as u64,
        Op::DivideSigned | Op::RemainderSigned => return None,
        Op::ShiftLeft if b < u64::from(bits) => a << b,
        Op::ShiftRightLogical if b < u64::from(bits) => a >> b,
        Op::ShiftRightArithmetic if b < u64::from(bits) => (sa >> b) as u64,
        Op::ShiftLeft | Op::ShiftRightLogical | Op::ShiftRightArithmetic => return None,
        Op::And => a & b,
        Op::Or => a | b,
        Op::Xor => a ^ b,
        Op::MinimumSigned => {
            if sa <= sb {
                a
            } else {
                b
            }
        }
        Op::MinimumUnsigned => a.min(b),
        Op::MaximumSigned => {
            if sa >= sb {
                a
            } else {
                b
            }
        }
        Op::MaximumUnsigned => a.max(b),
    })
}

fn integer_compare(predicate: IntegerPredicate, lhs: &Operand, rhs: &Operand) -> Option<Operand> {
    use IntegerPredicate as P;

    if matches!(lhs, Operand::Value(_)) && lhs == rhs {
        return Some(Operand::Bool(matches!(
            predicate,
            P::Equal
                | P::UnsignedLessEqual
                | P::UnsignedGreaterEqual
                | P::SignedLessEqual
                | P::SignedGreaterEqual
        )));
    }
    let (
        Operand::Integer {
            ty: Type::Int { bits, .. },
            ..
        },
        Operand::Integer { .. },
    ) = (lhs, rhs)
    else {
        return None;
    };
    let bits = u32::from(*bits);
    let (a, b) = (integer(lhs, bits)?, integer(rhs, bits)?);
    let (sa, sb) = (sign_extend(a, bits), sign_extend(b, bits));
    Some(Operand::Bool(match predicate {
        P::Equal => a == b,
        P::NotEqual => a != b,
        P::UnsignedLessThan => a < b,
        P::UnsignedLessEqual => a <= b,
        P::UnsignedGreaterThan => a > b,
        P::UnsignedGreaterEqual => a >= b,
        P::SignedLessThan => sa < sb,
        P::SignedLessEqual => sa <= sb,
        P::SignedGreaterThan => sa > sb,
        P::SignedGreaterEqual => sa >= sb,
    }))
}

fn convert(op: ConvertOp, value: &Operand, ty: &Type) -> Option<Operand> {
    let to = match ty {
        Type::Int { bits, .. } => u32::from(*bits),
        Type::Bool => {
            return match (op, value) {
                (ConvertOp::IntegerTruncate, Operand::Integer { value, .. }) => {
                    Some(Operand::Bool(value & 1 != 0))
                }
                _ => None,
            };
        }
        _ => return None,
    };
    let result = |value: u64| Operand::Integer {
        ty: ty.clone(),
        value: value & mask(to),
    };
    match (op, value) {
        (ConvertOp::ZeroExtend, Operand::Bool(value)) => Some(result(u64::from(*value))),
        (ConvertOp::SignExtend, Operand::Bool(value)) => {
            Some(result(if *value { u64::MAX } else { 0 }))
        }
        (
            ConvertOp::ZeroExtend | ConvertOp::IntegerTruncate,
            Operand::Integer {
                ty: Type::Int { bits: from, .. },
                value,
            },
        ) => Some(result(value & mask(u32::from(*from)))),
        (
            ConvertOp::SignExtend,
            Operand::Integer {
                ty: Type::Int { bits: from, .. },
                value,
            },
        ) => Some(result(sign_extend(*value, u32::from(*from)) as u64)),
        // Only between integers of one width, which differ in signedness and
        // nothing else.
        (
            ConvertOp::Bitcast,
            Operand::Integer {
                ty: Type::Int { bits: from, .. },
                value,
            },
        ) if u32::from(*from) == to => Some(result(*value)),
        _ => None,
    }
}

/// Turn a conditional branch whose condition is a constant, or whose two
/// targets are one block, into an unconditional one.
///
/// The block stops being a predecessor of the target it no longer reaches,
/// so that target's phis lose their input from it. A target nothing else
/// reaches is left for `remove-unreachable-blocks`.
fn fold_branches(function: &mut Function) -> bool {
    let mut dropped: BTreeSet<(BlockId, BlockId)> = BTreeSet::new();
    let mut changed = false;
    for block in &mut function.blocks {
        let Terminator::CondBranch {
            condition,
            true_block,
            false_block,
        } = &block.terminator
        else {
            continue;
        };
        let (taken, other) = match condition {
            _ if true_block == false_block => (*true_block, *false_block),
            Operand::Bool(true) => (*true_block, *false_block),
            Operand::Bool(false) => (*false_block, *true_block),
            _ => continue,
        };
        if taken != other {
            dropped.insert((block.id, other));
        }
        block.terminator = Terminator::Branch(taken);
        changed = true;
    }
    for block in &mut function.blocks {
        for instruction in &mut block.instructions {
            if let InstructionKind::Phi { incomings } = &mut instruction.kind {
                incomings.retain(|(from, _)| !dropped.contains(&(*from, block.id)));
            }
        }
    }
    changed
}
