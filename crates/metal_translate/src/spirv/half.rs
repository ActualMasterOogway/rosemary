//! Compute `half` as `float` on targets without `shaderFloat16`.
//!
//! `half` in a buffer keeps the guest's layout, so a module with one is left
//! unchanged and still needs `shaderFloat16`.

use super::*;
use crate::ir::{StructField, StructType, TextureType};
use crate::passes::Substitution;

/// The module with every `half` as a `float`, or `None` if there is nothing
/// to change or it can't be changed.
pub(super) fn widen(module: &Module) -> Option<Module> {
    let any = module.globals.iter().any(|global| has_half(&global.ty))
        || module
            .functions
            .iter()
            .any(|function| has_half(&function.result) || function.values.iter().any(has_half));
    if !any {
        return None;
    }
    let mut module = module.clone();
    for global in &mut module.globals {
        if global.space == AddressSpace::Constant && has_half(&global.ty) {
            return None;
        }
        global.ty = widen_type(&global.ty)?;
        if let Some(initializer) = &mut global.initializer {
            widen_operand(initializer)?;
        }
    }
    for function in &mut module.functions {
        widen_function(function)?;
    }
    Some(module)
}

fn widen_function(function: &mut Function) -> Option<()> {
    let mut substitution = Substitution::default();
    let values = &function.values;
    for block in &mut function.blocks {
        let mut kept = Vec::with_capacity(block.instructions.len());
        for mut instruction in std::mem::take(&mut block.instructions) {
            if let InstructionKind::Convert { op, value } = &mut instruction.kind {
                let source = type_of(values, value)?;
                let result = values.get(instruction.result?.0 as usize)?.clone();
                match op {
                    ConvertOp::Bitcast if has_half(&source) || has_half(&result) => {
                        *op = match (&source, &result) {
                            (Type::Int { bits: 32, .. }, half2) if is_half2(half2) => {
                                ConvertOp::UnpackHalf2x16
                            }
                            (half2, Type::Int { bits: 32, .. }) if is_half2(half2) => {
                                ConvertOp::PackHalf2x16
                            }
                            _ => return None,
                        };
                    }
                    // Once both sides are `float` the conversion does nothing.
                    ConvertOp::FloatResize if widen_type(&source)? == widen_type(&result)? => {
                        substitution.insert(instruction.result?, value.clone());
                        continue;
                    }
                    _ => {}
                }
            }
            kept.push(instruction);
        }
        block.instructions = kept;
    }
    substitution.apply(function);

    function.result = widen_type(&function.result)?;
    for parameter in &mut function.parameters {
        parameter.ty = widen_type(&parameter.ty)?;
    }
    for ty in &mut function.values {
        *ty = widen_type(ty)?;
    }
    for block in &mut function.blocks {
        for instruction in &mut block.instructions {
            if let InstructionKind::LocalVariable { ty } = &mut instruction.kind {
                *ty = widen_type(ty)?;
            }
            for operand in instruction_operands_mut(&mut instruction.kind) {
                widen_operand(operand)?;
            }
        }
        match &mut block.terminator {
            Terminator::CondBranch { condition, .. } => widen_operand(condition)?,
            Terminator::Return(Some(value)) => widen_operand(value)?,
            _ => {}
        }
    }
    Some(())
}

fn widen_operand(operand: &mut Operand) -> Option<()> {
    match operand {
        Operand::Float { ty, bits } if *ty == Type::Float { bits: 16 } => {
            *bits = u64::from(half_to_float_bits(*bits as u16));
            *ty = Type::Float { bits: 32 };
        }
        Operand::Composite { ty, elements } => {
            *ty = widen_type(ty)?;
            for element in elements {
                widen_operand(element)?;
            }
        }
        Operand::Global { ty, .. } | Operand::Undef(ty) => *ty = widen_type(ty)?,
        _ => {}
    }
    Some(())
}

/// `None` when a `half` is in buffer memory.
fn widen_type(ty: &Type) -> Option<Type> {
    if !has_half(ty) {
        return Some(ty.clone());
    }
    Some(match ty {
        Type::Float { bits: 16 } => Type::Float { bits: 32 },
        Type::Vector { element, lanes } => Type::Vector {
            element: Box::new(widen_type(element)?),
            lanes: *lanes,
        },
        Type::Matrix { column, columns } => Type::Matrix {
            column: Box::new(widen_type(column)?),
            columns: *columns,
        },
        Type::Array { element, count } => Type::Array {
            element: Box::new(widen_type(element)?),
            count: *count,
        },
        Type::Struct(structure) => Type::Struct(widen_struct(structure)?),
        Type::Pointer { space, pointee } => match space {
            AddressSpace::Device | AddressSpace::Constant => return None,
            _ => Type::Pointer {
                space: *space,
                pointee: Box::new(widen_type(pointee)?),
            },
        },
        Type::Texture(texture) => Type::Texture(TextureType {
            texel: Box::new(Type::Float { bits: 32 }),
            ..texture.clone()
        }),
        _ => return None,
    })
}

fn widen_struct(structure: &StructType) -> Option<StructType> {
    if structure.packed {
        return None;
    }
    let mut fields = Vec::with_capacity(structure.fields.len());
    let mut end = 0u32;
    let mut alignment = 1u32;
    for field in &structure.fields {
        let ty = widen_type(&field.ty)?;
        let (size, align) = size_and_alignment(&ty)?;
        let offset = end.next_multiple_of(align);
        end = offset + size;
        alignment = alignment.max(align);
        fields.push(StructField {
            name: field.name.clone(),
            ty,
            offset,
        });
    }
    Some(StructType {
        fields,
        size: end.next_multiple_of(alignment),
        alignment,
        packed: false,
    })
}

fn size_and_alignment(ty: &Type) -> Option<(u32, u32)> {
    Some(match ty {
        Type::Bool => (1, 1),
        Type::Int { bits, .. } | Type::Float { bits } => {
            let bytes = u32::from(*bits).div_ceil(8);
            (bytes, bytes)
        }
        Type::Vector { element, lanes } => {
            let (element, _) = size_and_alignment(element)?;
            let size = element * u32::from(*lanes).next_power_of_two();
            (size, size)
        }
        Type::Matrix { column, columns } => {
            let (column, align) = size_and_alignment(column)?;
            (column * u32::from(*columns), align)
        }
        Type::Array { element, count } => {
            let (size, align) = size_and_alignment(element)?;
            (size.next_multiple_of(align) * count, align)
        }
        Type::Struct(structure) => (structure.size, structure.alignment),
        _ => return None,
    })
}

fn type_of(values: &[Type], operand: &Operand) -> Option<Type> {
    match operand {
        Operand::Value(value) => values.get(value.0 as usize).cloned(),
        Operand::Bool(_) => Some(Type::Bool),
        Operand::Integer { ty, .. }
        | Operand::Float { ty, .. }
        | Operand::Composite { ty, .. }
        | Operand::Global { ty, .. }
        | Operand::Undef(ty) => Some(ty.clone()),
    }
}

fn has_half(ty: &Type) -> bool {
    match ty {
        Type::Float { bits: 16 } => true,
        Type::Vector { element, .. } | Type::Array { element, .. } => has_half(element),
        Type::Matrix { column, .. } => has_half(column),
        Type::Struct(structure) => structure.fields.iter().any(|field| has_half(&field.ty)),
        Type::Pointer { pointee, .. } => has_half(pointee),
        Type::Texture(texture) => has_half(&texture.texel),
        _ => false,
    }
}

fn is_half2(ty: &Type) -> bool {
    matches!(ty, Type::Vector { element, lanes: 2 } if **element == Type::Float { bits: 16 })
}

fn half_to_float_bits(half: u16) -> u32 {
    let sign = u32::from(half >> 15) << 31;
    let exponent = u32::from((half >> 10) & 0x1f);
    let mantissa = u32::from(half & 0x3ff);
    match (exponent, mantissa) {
        (0, 0) => sign,
        // Subnormal: normalise it.
        (0, _) => {
            let shift = mantissa.leading_zeros() - 21;
            let mantissa = (mantissa << shift) & 0x3ff;
            sign | ((113 - shift) << 23) | (mantissa << 13)
        }
        (0x1f, _) => sign | 0x7f80_0000 | (mantissa << 13),
        _ => sign | ((exponent + 112) << 23) | (mantissa << 13),
    }
}
