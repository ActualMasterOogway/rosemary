//! Conversion between scalar and vector types.

use super::*;

impl Emitter {
    /// Emit one such instruction.
    pub(super) fn emit_convert(
        &mut self,
        function: &Function,
        instruction: &crate::ir::Instruction,
        values: &HashMap<ValueId, u32>,
        result_id: Option<u32>,
    ) -> Result<(), TranslateError> {
        match &instruction.kind {
            // SPIR-V keeps booleans out of the numeric type system, so a
            // conversion between `i1` and an integer becomes a selection or a
            // comparison rather than a width change.
            InstructionKind::Convert {
                op:
                    op @ (ConvertOp::ZeroExtend
                    | ConvertOp::SignExtend
                    | ConvertOp::UnsignedToFloat
                    | ConvertOp::SignedToFloat),
                value,
            } if operand_type(function, value) == Some(Type::Bool) => {
                let result = instruction
                    .result
                    .ok_or_else(|| emission("conversion has no result"))?;
                let result_type = function.values[result.0 as usize].clone();
                let ty = self.type_id(&result_type)?;
                // A sign extension of a set bit is all ones, that is -1;
                // a widening or a conversion to floating point yields one.
                let set = match (op, &result_type) {
                    (ConvertOp::SignExtend, Type::Int { .. }) => u64::MAX,
                    (_, Type::Float { bits: 32 }) => u64::from(1.0f32.to_bits()),
                    _ => 1,
                };
                let one = self.constant(&result_type, set)?;
                let zero = self.constant(&result_type, 0)?;
                let value = self.operand(value, values)?;
                self.builder
                    .select(ty, result_id, value, one, zero)
                    .map_err(build_error)?;
            }
            InstructionKind::Convert {
                op: ConvertOp::IntegerTruncate,
                value,
            } if function
                .values
                .get(
                    instruction
                        .result
                        .ok_or_else(|| emission("conversion has no result"))?
                        .0 as usize,
                )
                .is_some_and(|ty| *ty == Type::Bool) =>
            {
                let ty = self.type_id(&Type::Bool)?;
                let source = operand_type(function, value)
                    .ok_or_else(|| emission("conversion operand has no type"))?;
                // Narrowing to `i1` keeps the low bit.
                let one = self.constant(&source, 1)?;
                let zero = self.constant(&source, 0)?;
                let value = self.operand(value, values)?;
                let source_id = self.type_id(&source)?;
                let masked = self
                    .builder
                    .bitwise_and(source_id, None, value, one)
                    .map_err(build_error)?;
                self.builder
                    .i_not_equal(ty, result_id, masked, zero)
                    .map_err(build_error)?;
            }
            InstructionKind::Convert { op, value } => {
                let result = instruction
                    .result
                    .ok_or_else(|| emission("conversion has no result"))?;
                let ty = self.type_id(&function.values[result.0 as usize])?;
                let value = self.operand(value, values)?;
                let b = &mut self.builder;
                match op {
                    ConvertOp::UnsignedToFloat => b.convert_u_to_f(ty, result_id, value),
                    ConvertOp::SignedToFloat => b.convert_s_to_f(ty, result_id, value),
                    ConvertOp::FloatToUnsigned => b.convert_f_to_u(ty, result_id, value),
                    ConvertOp::FloatToSigned => b.convert_f_to_s(ty, result_id, value),
                    ConvertOp::ZeroExtend => b.u_convert(ty, result_id, value),
                    ConvertOp::SignExtend => b.s_convert(ty, result_id, value),
                    // SPIR-V narrows integers with the same width-changing
                    // conversion; the result width discards the high bits.
                    ConvertOp::IntegerTruncate => b.u_convert(ty, result_id, value),
                    ConvertOp::FloatResize => b.f_convert(ty, result_id, value),
                    ConvertOp::Bitcast => b.bitcast(ty, result_id, value),
                }
                .map_err(build_error)?;
            }
            _ => unreachable!("not a convert instruction"),
        }
        Ok(())
    }
}
