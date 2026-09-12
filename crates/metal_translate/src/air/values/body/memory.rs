//! Pointers, allocation, casts, loads and stores.

use super::*;

impl BodyLowering<'_> {
    pub(super) fn cast(
        &mut self,
        source_value: &u64,
        target: &u64,
        opcode: &u64,
        record: &llvm_bitcode::bitcode::Record,
        fields: &[u64],
    ) -> Result<Lowered, TranslateError> {
        let (ty, kind) = {
            let source_id = self.table.operand_id(*source_value)?;
            let (source, value) = self.operand(*source_value, None, record, fields)?;
            let target = type_reference(Some(target), self.types.len())?;
            // A cast to `i8*` names raw bytes, which SPIR-V's
            // logical addressing cannot address. Apple's compiler
            // emits one only to hand a local to a lifetime
            // marker, so the cast carries the pointer unchanged
            // and the marker itself is dropped.
            if *opcode == 11
                && let DecodedType::Pointer { pointee, .. } = self.types[target]
                && matches!(self.types[pointee], DecodedType::Integer(8))
            {
                let id = self.table.values.len();
                define(
                    &mut self.forward,
                    id,
                    value,
                    &mut self.operands,
                    &self.function,
                    &mut self.instructions,
                )?;
                self.table.values.push(Value {
                    ty: source,
                    constant: None,
                });
                return Ok(Lowered::Done);
            }
            // A `thread` array decays to a pointer to its first
            // element. SPIR-V spells that as an access chain, not
            // as a cast between pointer types.
            if *opcode == 11
                && let (
                    DecodedType::Pointer {
                        pointee: from,
                        address_space: 0,
                    },
                    DecodedType::Pointer {
                        pointee: to,
                        address_space: 0,
                    },
                ) = (&self.types[source], &self.types[target])
                && let DecodedType::Array { element, .. } = self.types[*from]
                && element == *to
            {
                (
                    target,
                    InstructionKind::BufferAddress {
                        pointer: value,
                        // The leading index steps the pointer, as
                        // in any LLVM `getelementptr`.
                        indices: vec![Operand::u32(0), Operand::u32(0)],
                    },
                )
            } else {
                // Apple's compiler reads a `float3` struct member
                // with a `float4` load, because Metal pads a `float3`
                // to sixteen bytes. Record the pointee the pointer
                // really addresses and adapt at the load rather than
                // retyping the pointer.
                if let (
                    DecodedType::Pointer {
                        pointee: from,
                        address_space,
                    },
                    DecodedType::Pointer {
                        pointee: to,
                        address_space: target_space,
                    },
                ) = (&self.types[source], &self.types[target])
                    && *opcode == 11
                    && address_space == target_space
                {
                    let actual = self
                        .aliases
                        .get(&source_id)
                        .map_or(*from, |alias| alias.pointee);
                    // A pointer to an array read as a pointer to
                    // something the shape of its element addresses
                    // element zero. Step into it, so that the load
                    // reads one element rather than the whole
                    // array, and adapt that element at the load.
                    let (actual, address) = match self.types[actual] {
                        DecodedType::Array { element, .. }
                            if reinterpretation(self.types, element, *to).is_ok() =>
                        {
                            let pointer = Type::Pointer {
                                space: address_space_of(*address_space)?,
                                pointee: Box::new(ir_type(element, self.types)?),
                            };
                            let stepped = emit(
                                &mut self.function,
                                &mut self.instructions,
                                pointer,
                                InstructionKind::BufferAddress {
                                    pointer: value.clone(),
                                    indices: vec![Operand::u32(0), Operand::u32(0)],
                                },
                            );
                            (element, stepped)
                        }
                        _ => (actual, value.clone()),
                    };
                    reinterpretation(self.types, actual, *to)?;
                    let id = self.table.values.len();
                    self.aliases.insert(
                        id,
                        PointerAlias {
                            pointee: actual,
                            operand: address,
                        },
                    );
                    define(
                        &mut self.forward,
                        id,
                        value,
                        &mut self.operands,
                        &self.function,
                        &mut self.instructions,
                    )?;
                    self.table.values.push(Value {
                        ty: target,
                        constant: None,
                    });
                    return Ok(Lowered::Done);
                }
                let op = cast_operation(*opcode, source, target, self.types)?;
                (target, InstructionKind::Convert { op, value })
            }
        };
        Ok(Lowered::Value(ty, kind))
    }

    pub(super) fn alloca(
        &mut self,
        allocated: &u64,
        size_type: &u64,
        size: &u64,
        packed: &u64,
    ) -> Result<Lowered, TranslateError> {
        let (ty, kind) = {
            let allocated = type_reference(Some(allocated), self.types.len())?;
            // Bit 6 of the packed field says the record names the
            // allocated type rather than a pointer to it.
            let (pointer, pointee) = if packed & (1 << 6) == 0 {
                match self.types[allocated] {
                    DecodedType::Pointer {
                        pointee,
                        address_space: 0,
                    } => (allocated, pointee),
                    _ => {
                        return Err(semantic(
                            "LLVM alloca names a type which is not a thread pointer",
                        ));
                    }
                }
            } else {
                let pointer = self
                    .types
                    .iter()
                    .position(|ty| {
                        matches!(
                            ty,
                            DecodedType::Pointer {
                                pointee,
                                address_space: 0,
                            } if *pointee == allocated
                        )
                    })
                    .ok_or_else(|| semantic("LLVM alloca has no thread pointer result type"))?;
                (pointer, allocated)
            };
            let _ = type_reference(Some(size_type), self.types.len())?;
            if self.table.integer(
                usize::try_from(*size)
                    .map_err(|_| semantic("LLVM alloca size does not fit in memory"))?,
                self.types,
            )? != 1
            {
                return Err(semantic(
                    "LLVM alloca of more than one element needs lowering",
                ));
            }
            (
                pointer,
                InstructionKind::LocalVariable {
                    ty: ir_type(pointee, self.types)?,
                },
            )
        };
        Ok(Lowered::Value(ty, kind))
    }

    pub(super) fn element_pointer(
        &mut self,
        pointer: &u64,
        indices: &[u64],
        record: &llvm_bitcode::bitcode::Record,
        fields: &[u64],
    ) -> Result<Lowered, TranslateError> {
        let (ty, kind) = {
            let (pointer_ty, pointer) = self.operand(*pointer, None, record, fields)?;
            let DecodedType::Pointer {
                mut pointee,
                address_space,
            } = self.types[pointer_ty]
            else {
                return Err(semantic("LLVM GEP requires pointer"));
            };
            let mut decoded = Vec::new();
            for (position, &index) in indices.iter().enumerate() {
                let (index_ty, value) = self.operand(index, None, record, fields)?;
                if !matches!(self.types[index_ty], DecodedType::Integer(_)) {
                    return Err(semantic("LLVM GEP index is not integer"));
                }
                if position > 0 {
                    pointee = match &self.types[pointee] {
                        DecodedType::Struct { fields, .. } => {
                            let field = self
                                .table
                                .integer(self.table.operand_id(index)?, self.types)?;
                            *fields
                                .get(field as usize)
                                .ok_or_else(|| semantic("LLVM GEP field out of bounds"))?
                        }
                        DecodedType::Array { element, .. }
                        | DecodedType::Vector { element, .. } => *element,
                        _ => return Err(semantic("LLVM GEP indexes a scalar")),
                    };
                }
                decoded.push(value);
            }
            let target = self.types.iter().position(|ty| matches!(ty, DecodedType::Pointer { pointee: p, address_space: a } if *p == pointee && *a == address_space)).ok_or_else(|| semantic("LLVM GEP result pointer type is missing"))?;
            (
                target,
                InstructionKind::BufferAddress {
                    pointer,
                    indices: decoded,
                },
            )
        };
        Ok(Lowered::Value(ty, kind))
    }

    pub(super) fn store(
        &mut self,
        pointer: &u64,
        value: &u64,
        record: &llvm_bitcode::bitcode::Record,
        fields: &[u64],
    ) -> Result<Lowered, TranslateError> {
        let pointer_id = self.table.operand_id(*pointer)?;
        let alias = self
            .aliases
            .get(&pointer_id)
            .map(|alias| (alias.pointee, alias.operand.clone()));
        let (pointee, pointer) = match alias {
            Some((pointee, operand)) => (pointee, operand),
            None => {
                let (pointer_ty, pointer) = self.operand(*pointer, None, record, fields)?;
                let DecodedType::Pointer { pointee, .. } = self.types[pointer_ty] else {
                    return Err(semantic("LLVM store requires a pointer"));
                };
                (pointee, pointer)
            }
        };
        let (value_ty, value) = self.operand(*value, None, record, fields)?;
        // A store through a reinterpreted pointer is the
        // mirror of the load: the value the guest wrote is
        // adapted to what the address really holds, rather
        // than the pointer being retyped.
        let value = narrow(
            value,
            value_ty,
            pointee,
            self.types,
            &mut self.function,
            &mut self.instructions,
        )?;
        self.instructions.push(Instruction {
            result: None,
            kind: InstructionKind::Store { pointer, value },
        });
        Ok(Lowered::Done)
    }

    pub(super) fn load(
        &mut self,
        pointer: &u64,
        record: &llvm_bitcode::bitcode::Record,
        fields: &[u64],
    ) -> Result<Lowered, TranslateError> {
        let pointer_id = self.table.operand_id(*pointer)?;
        let (ty, pointer) = self.operand(*pointer, None, record, fields)?;
        let DecodedType::Pointer { pointee, .. } = self.types[ty] else {
            return Err(semantic("LLVM load requires pointer"));
        };
        let Some(alias) = self.aliases.get(&pointer_id) else {
            let value = emit(
                &mut self.function,
                &mut self.instructions,
                ir_type(pointee, self.types)?,
                InstructionKind::Load { pointer },
            );
            define(
                &mut self.forward,
                self.table.values.len(),
                value,
                &mut self.operands,
                &self.function,
                &mut self.instructions,
            )?;
            self.table.values.push(Value {
                ty: pointee,
                constant: None,
            });
            return Ok(Lowered::Done);
        };
        let loaded = emit(
            &mut self.function,
            &mut self.instructions,
            ir_type(alias.pointee, self.types)?,
            InstructionKind::Load {
                pointer: alias.operand.clone(),
            },
        );
        let value = widen(
            loaded,
            alias.pointee,
            pointee,
            self.types,
            &mut self.function,
            &mut self.instructions,
        )?;
        define(
            &mut self.forward,
            self.table.values.len(),
            value,
            &mut self.operands,
            &self.function,
            &mut self.instructions,
        )?;
        self.table.values.push(Value {
            ty: pointee,
            constant: None,
        });
        Ok(Lowered::Done)
    }
}
