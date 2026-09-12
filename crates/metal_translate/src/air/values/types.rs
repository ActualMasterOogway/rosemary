//! Mapping LLVM's decoded types and predicates onto Metal IR ones.

use super::*;

pub(super) fn integer_predicate(predicate: u64) -> Result<IntegerPredicate, TranslateError> {
    Ok(match predicate {
        32 => IntegerPredicate::Equal,
        33 => IntegerPredicate::NotEqual,
        34 => IntegerPredicate::UnsignedGreaterThan,
        35 => IntegerPredicate::UnsignedGreaterEqual,
        36 => IntegerPredicate::UnsignedLessThan,
        37 => IntegerPredicate::UnsignedLessEqual,
        38 => IntegerPredicate::SignedGreaterThan,
        39 => IntegerPredicate::SignedGreaterEqual,
        40 => IntegerPredicate::SignedLessThan,
        41 => IntegerPredicate::SignedLessEqual,
        _ => {
            return Err(semantic(format!(
                "unsupported LLVM integer comparison predicate {predicate}"
            )));
        }
    })
}

pub(super) fn float_predicate(predicate: u64) -> Result<FloatPredicate, TranslateError> {
    Ok(match predicate {
        1 => FloatPredicate::OrderedEqual,
        2 => FloatPredicate::OrderedGreaterThan,
        3 => FloatPredicate::OrderedGreaterEqual,
        4 => FloatPredicate::OrderedLessThan,
        5 => FloatPredicate::OrderedLessEqual,
        6 => FloatPredicate::OrderedNotEqual,
        7 => FloatPredicate::Ordered,
        8 => FloatPredicate::Unordered,
        9 => FloatPredicate::UnorderedEqual,
        10 => FloatPredicate::UnorderedGreaterThan,
        11 => FloatPredicate::UnorderedGreaterEqual,
        12 => FloatPredicate::UnorderedLessThan,
        13 => FloatPredicate::UnorderedLessEqual,
        14 => FloatPredicate::UnorderedNotEqual,
        // Constant-valued comparisons would need a materialized constant
        // rather than an instruction; AIR does not emit them.
        _ => {
            return Err(semantic(format!(
                "unsupported LLVM floating comparison predicate {predicate}"
            )));
        }
    })
}

/// The `i1` result type of a comparison, keeping the operand's vector shape.
pub(super) fn boolean_type(operand: usize, types: &[DecodedType]) -> Result<usize, TranslateError> {
    let scalar = types
        .iter()
        .position(|ty| matches!(ty, DecodedType::Integer(1)))
        .ok_or_else(|| semantic("LLVM comparison has no boolean type"))?;
    match types[operand] {
        DecodedType::Vector { lanes, .. } => types
            .iter()
            .position(
                |ty| matches!(ty, DecodedType::Vector { element, lanes: n } if *element == scalar && *n == lanes),
            )
            .ok_or_else(|| semantic("LLVM comparison has no boolean vector type")),
        _ => Ok(scalar),
    }
}

pub(super) fn member_type(ty: &DecodedType, index: u32) -> Result<usize, TranslateError> {
    match ty {
        DecodedType::Struct { fields, .. } => fields
            .get(index as usize)
            .copied()
            .ok_or_else(|| semantic("LLVM aggregate index is out of range")),
        DecodedType::Array { element, count } if index < *count => Ok(*element),
        DecodedType::Vector { element, lanes } if index < *lanes => Ok(*element),
        _ => Err(semantic("LLVM aggregate index applies to a non-aggregate")),
    }
}

pub(super) fn aggregate_types(ty: &DecodedType) -> Result<Vec<usize>, TranslateError> {
    match ty {
        DecodedType::Vector { element, lanes } => Ok(vec![*element; *lanes as usize]),
        DecodedType::Array { element, count } => Ok(vec![*element; *count as usize]),
        DecodedType::Struct { fields, .. } => Ok(fields.clone()),
        _ => Err(semantic("LLVM aggregate constant has nonaggregate type")),
    }
}

pub(super) fn emit(
    function: &mut Function,
    instructions: &mut Vec<Instruction>,
    ty: Type,
    kind: InstructionKind,
) -> Operand {
    let result = ValueId(function.values.len() as u32);
    function.values.push(ty);
    instructions.push(Instruction {
        result: Some(result),
        kind,
    });
    Operand::Value(result)
}

pub(super) fn zero_composite(ty: &Type) -> Result<Operand, TranslateError> {
    let members: Vec<&Type> = match ty {
        Type::Bool => return Ok(Operand::Bool(false)),
        Type::Int { .. } => {
            return Ok(Operand::Integer {
                ty: ty.clone(),
                value: 0,
            });
        }
        Type::Float { .. } => {
            return Ok(Operand::Float {
                ty: ty.clone(),
                bits: 0,
            });
        }
        Type::Vector { element, lanes } => vec![element.as_ref(); usize::from(*lanes)],
        Type::Array { element, count } => vec![element.as_ref(); *count as usize],
        Type::Struct(structure) => structure.fields.iter().map(|field| &field.ty).collect(),
        _ => return Err(semantic("LLVM zero constant type needs lowering")),
    };
    Ok(Operand::Composite {
        ty: ty.clone(),
        elements: members
            .into_iter()
            .map(zero_composite)
            .collect::<Result<_, _>>()?,
    })
}

/// AIR numbers its address spaces the way Metal names them.
pub(super) fn address_space_of(space: u64) -> Result<AddressSpace, TranslateError> {
    Ok(match space {
        0 => AddressSpace::Thread,
        1 => AddressSpace::Device,
        2 => AddressSpace::Constant,
        3 => AddressSpace::Threadgroup,
        _ => return Err(semantic("unsupported AIR address space")),
    })
}

pub(super) fn ir_type(index: usize, types: &[DecodedType]) -> Result<Type, TranslateError> {
    fn convert(
        index: usize,
        types: &[DecodedType],
        active: &mut Vec<usize>,
    ) -> Result<Type, TranslateError> {
        if active.contains(&index) {
            return Err(semantic(
                "recursive LLVM type needs explicit layout support",
            ));
        }
        active.push(index);
        let ty = match types
            .get(index)
            .ok_or_else(|| semantic("missing LLVM type"))?
        {
            DecodedType::Void => Type::Void,
            DecodedType::Integer(1) => Type::Bool,
            DecodedType::Integer(bits) => Type::Int {
                bits: *bits,
                signed: false,
            },
            DecodedType::Float(bits) => Type::Float { bits: *bits },
            DecodedType::Vector { element, lanes } => Type::Vector {
                element: Box::new(convert(*element, types, active)?),
                lanes: u8::try_from(*lanes).map_err(|_| semantic("LLVM vector too wide"))?,
            },
            DecodedType::Array { element, count } => Type::Array {
                element: Box::new(convert(*element, types, active)?),
                count: *count,
            },
            DecodedType::Pointer {
                pointee,
                address_space,
            } => Type::Pointer {
                space: address_space_of(*address_space)?,
                pointee: Box::new(convert(*pointee, types, active)?),
            },
            DecodedType::Struct { fields, packed } => {
                let mut result = Vec::new();
                let mut size = 0u32;
                let mut alignment = 1;
                for &field in fields {
                    let ty = convert(field, types, active)?;
                    let (field_size, field_align) = layout(&ty)?;
                    let field_align = if *packed { 1 } else { field_align };
                    alignment = alignment.max(field_align);
                    size = align(size, field_align)?;
                    result.push(crate::ir::StructField {
                        name: None,
                        ty,
                        offset: size,
                    });
                    size = size
                        .checked_add(field_size)
                        .ok_or_else(|| semantic("LLVM struct size overflow"))?;
                }
                Type::Struct(crate::ir::StructType {
                    fields: result,
                    size: align(size, alignment)?,
                    alignment,
                    packed: *packed,
                })
            }
            _ => return Err(semantic("LLVM type has no Metal value representation")),
        };
        active.pop();
        Ok(ty)
    }
    convert(index, types, &mut Vec::new())
}

pub(super) fn align(size: u32, alignment: u32) -> Result<u32, TranslateError> {
    size.checked_add(alignment - 1)
        .map(|size| size / alignment * alignment)
        .ok_or_else(|| semantic("LLVM layout overflow"))
}

pub(super) fn layout(ty: &Type) -> Result<(u32, u32), TranslateError> {
    match ty {
        Type::Int { bits, .. } | Type::Float { bits } if matches!(bits, 8 | 16 | 32 | 64) => {
            Ok((u32::from(*bits) / 8, u32::from(*bits) / 8))
        }
        Type::Vector { element, lanes } if matches!(lanes, 2..=4) => {
            let (size, _) = layout(element)?;
            let alignment = size * u32::from(*lanes).next_power_of_two();
            Ok((alignment, alignment))
        }
        Type::Array { element, count } => {
            let (size, alignment) = layout(element)?;
            Ok((
                size.checked_mul(*count)
                    .ok_or_else(|| semantic("LLVM array size overflow"))?,
                alignment,
            ))
        }
        Type::Struct(s) => Ok((s.size, s.alignment)),
        _ => Err(semantic("unsupported AIR type layout")),
    }
}
