use super::*;

impl Emitter {
    /// Emit one such instruction.
    pub(super) fn emit_memory(
        &mut self,
        function: &Function,
        instruction: &crate::ir::Instruction,
        values: &HashMap<ValueId, u32>,
        result_id: Option<u32>,
    ) -> Result<(), TranslateError> {
        match &instruction.kind {
            // Declared in the entry block by `materialize_local_variables`.
            InstructionKind::LocalVariable { .. } => {}
            InstructionKind::Load { pointer } => {
                let result = instruction
                    .result
                    .ok_or_else(|| emission("load has no result"))?;
                let ty = self.type_id(&function.values[result.0 as usize])?;
                let pointer = self.operand(pointer, values)?;
                self.builder
                    .load(ty, result_id, pointer, None, [])
                    .map_err(build_error)?;
            }
            InstructionKind::Store { pointer, value } => {
                let storage = self.pointer_operand_storage(pointer)?;
                let pointer = self.operand(pointer, values)?;
                // Aggregate types used in buffers carry explicit offsets and
                // strides. Function and private memory must use the parallel
                // undecorated type, including for a constant initializer.
                let value = match (storage, value) {
                    (StorageClass::StorageBuffer, _) | (_, Operand::Value(_)) => {
                        self.operand(value, values)?
                    }
                    (_, Operand::Composite { .. }) => self.constant_operand(value)?,
                    _ => self.operand(value, values)?,
                };
                self.builder
                    .store(pointer, value, None, [])
                    .map_err(build_error)?;
            }
            InstructionKind::ElementAddress { pointer, index } => {
                let (variable, storage, member) = self.pointer_base(pointer, values)?;
                let result = instruction
                    .result
                    .ok_or_else(|| emission("element address has no result"))?;
                let Type::Pointer { pointee, .. } = &function.values[result.0 as usize] else {
                    return Err(emission("element address result is not a pointer"));
                };
                let pointee = match storage {
                    StorageClass::StorageBuffer => self.type_id(pointee)?,
                    _ => self.unlaid_type_id(pointee)?,
                };
                let pointer_type = self.builder.type_pointer(None, storage, pointee);
                let mut chain = Vec::new();
                if member {
                    chain.push(self.constant(&Type::u32(), 0)?);
                }
                chain.push(self.operand(index, values)?);
                self.builder
                    .access_chain(pointer_type, result_id, variable, chain)
                    .map_err(build_error)?;
                self.pointer_storage.insert(result, storage);
            }
            InstructionKind::BufferAddress { pointer, indices } => {
                let (variable, storage, member) = self.pointer_base(pointer, values)?;
                let result = instruction
                    .result
                    .ok_or_else(|| emission("buffer address has no result"))?;
                let Type::Pointer { pointee, .. } = &function.values[result.0 as usize] else {
                    return Err(emission("buffer address result is not a pointer"));
                };
                let pointee = match storage {
                    StorageClass::StorageBuffer => self.type_id(pointee)?,
                    _ => self.unlaid_type_id(pointee)?,
                };
                let ty = self.builder.type_pointer(None, storage, pointee);
                let mut chain = Vec::new();
                // LLVM's first GEP index steps the pointer itself. A buffer
                // resource is a block whose one member is the array being
                // stepped, so there the index selects that member's element;
                // a variable pointing straight at an aggregate has nothing to
                // step, and SPIR-V's first index already selects a member.
                let indices = if member {
                    chain.push(self.constant(&Type::u32(), 0)?);
                    indices.as_slice()
                } else {
                    match indices.split_first() {
                        Some((Operand::Integer { value: 0, .. }, rest)) => rest,
                        _ => {
                            return Err(emission(
                                "an access chain into a global variable steps the pointer",
                            ));
                        }
                    }
                };
                for index in indices {
                    chain.push(self.operand(index, values)?);
                }
                self.builder
                    .access_chain(ty, result_id, variable, chain)
                    .map_err(build_error)?;
                self.pointer_storage.insert(result, storage);
            }
            InstructionKind::FieldAddress { pointer, indices } => {
                let (variable, storage, member) = self.pointer_base(pointer, values)?;
                let result = instruction
                    .result
                    .ok_or_else(|| emission("field address has no result"))?;
                let Type::Pointer { pointee, .. } = &function.values[result.0 as usize] else {
                    return Err(emission("field address result is not a pointer"));
                };
                let pointee = match storage {
                    StorageClass::StorageBuffer => self.type_id(pointee)?,
                    _ => self.unlaid_type_id(pointee)?,
                };
                let pointer_type = self.builder.type_pointer(None, storage, pointee);
                let mut chain = Vec::new();
                if member {
                    // A buffer block holds one runtime array, so a field of
                    // its element is reached past the member and the element.
                    let zero = self.constant(&Type::u32(), 0)?;
                    chain.push(zero);
                    chain.push(zero);
                }
                for index in indices {
                    chain.push(self.constant(&Type::u32(), u64::from(*index))?);
                }
                self.builder
                    .access_chain(pointer_type, result_id, variable, chain)
                    .map_err(build_error)?;
                self.pointer_storage.insert(result, storage);
            }
            _ => unreachable!("not a memory instruction"),
        }
        Ok(())
    }
}
