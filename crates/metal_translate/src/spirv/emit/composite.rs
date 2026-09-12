//! Vector and aggregate construction, extraction and selection.

use super::*;

impl Emitter {
    /// Emit one such instruction.
    pub(super) fn emit_composite(
        &mut self,
        function: &Function,
        instruction: &crate::ir::Instruction,
        blocks: &HashMap<BlockId, u32>,
        values: &HashMap<ValueId, u32>,
        result_id: Option<u32>,
    ) -> Result<(), TranslateError> {
        match &instruction.kind {
            InstructionKind::Phi { incomings } => {
                let result = instruction
                    .result
                    .ok_or_else(|| emission("phi has no result"))?;
                let ty = self.value_type_id(
                    function,
                    result,
                    incomings.iter().map(|(_, operand)| operand),
                    "phi",
                )?;
                let incoming = incomings
                    .iter()
                    .map(|(block, operand)| Ok((self.operand(operand, values)?, blocks[block])))
                    .collect::<Result<Vec<_>, TranslateError>>()?;
                self.builder
                    .phi(ty, result_id, incoming)
                    .map_err(build_error)?;
            }
            InstructionKind::CompositeExtract { composite, index } => {
                let result = instruction
                    .result
                    .ok_or_else(|| emission("composite extract has no result"))?;
                let ty = self.type_id(&function.values[result.0 as usize])?;
                let composite = self.operand(composite, values)?;
                self.builder
                    .composite_extract(ty, result_id, composite, [*index])
                    .map_err(build_error)?;
            }
            InstructionKind::CompositeInsert {
                composite,
                object,
                index,
            } => {
                let result = instruction
                    .result
                    .ok_or_else(|| emission("composite insert has no result"))?;
                let ty = self.type_id(&function.values[result.0 as usize])?;
                let composite = self.operand(composite, values)?;
                let object = self.operand(object, values)?;
                self.builder
                    .composite_insert(ty, result_id, object, composite, [*index])
                    .map_err(build_error)?;
            }
            InstructionKind::VectorExtractDynamic { vector, index } => {
                let result = instruction
                    .result
                    .ok_or_else(|| emission("dynamic vector extract has no result"))?;
                let ty = self.type_id(&function.values[result.0 as usize])?;
                let vector = self.operand(vector, values)?;
                let index = self.operand(index, values)?;
                self.builder
                    .vector_extract_dynamic(ty, result_id, vector, index)
                    .map_err(build_error)?;
            }
            InstructionKind::VectorInsertDynamic {
                vector,
                object,
                index,
            } => {
                let result = instruction
                    .result
                    .ok_or_else(|| emission("dynamic vector insert has no result"))?;
                let ty = self.type_id(&function.values[result.0 as usize])?;
                let vector = self.operand(vector, values)?;
                let object = self.operand(object, values)?;
                let index = self.operand(index, values)?;
                self.builder
                    .vector_insert_dynamic(ty, result_id, vector, object, index)
                    .map_err(build_error)?;
            }
            InstructionKind::VectorShuffle {
                first,
                second,
                components,
            } => {
                let result = instruction
                    .result
                    .ok_or_else(|| emission("vector shuffle has no result"))?;
                let ty = self.type_id(&function.values[result.0 as usize])?;
                let first = self.operand(first, values)?;
                let second = self.operand(second, values)?;
                self.builder
                    .vector_shuffle(ty, result_id, first, second, components.iter().copied())
                    .map_err(build_error)?;
            }
            InstructionKind::VectorSplat { value, lanes } => {
                let result = instruction
                    .result
                    .ok_or_else(|| emission("vector splat has no result"))?;
                let ty = self.type_id(&function.values[result.0 as usize])?;
                let value = self.operand(value, values)?;
                self.builder
                    .composite_construct(
                        ty,
                        result_id,
                        std::iter::repeat_n(value, usize::from(*lanes)),
                    )
                    .map_err(build_error)?;
            }
            InstructionKind::All { vector } => {
                let result = instruction
                    .result
                    .ok_or_else(|| emission("all has no result"))?;
                let ty = self.type_id(&function.values[result.0 as usize])?;
                let vector = self.operand(vector, values)?;
                self.builder
                    .all(ty, result_id, vector)
                    .map_err(build_error)?;
            }
            InstructionKind::Any { vector } => {
                let result = instruction
                    .result
                    .ok_or_else(|| emission("any has no result"))?;
                let ty = self.type_id(&function.values[result.0 as usize])?;
                let vector = self.operand(vector, values)?;
                self.builder
                    .any(ty, result_id, vector)
                    .map_err(build_error)?;
            }
            InstructionKind::Select {
                condition,
                on_true,
                on_false,
            } => {
                let result = instruction
                    .result
                    .ok_or_else(|| emission("select has no result"))?;
                let ty = self.value_type_id(
                    function,
                    result,
                    [on_true, on_false].into_iter(),
                    "select",
                )?;
                let condition = self.operand(condition, values)?;
                let on_true = self.operand(on_true, values)?;
                let on_false = self.operand(on_false, values)?;
                self.builder
                    .select(ty, result_id, condition, on_true, on_false)
                    .map_err(build_error)?;
            }
            _ => unreachable!("not a composite instruction"),
        }
        Ok(())
    }
}
