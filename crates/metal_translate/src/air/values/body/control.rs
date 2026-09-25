use super::*;

impl BodyLowering<'_> {
    pub(super) fn return_void(&mut self) -> Result<Lowered, TranslateError> {
        if !matches!(self.types[self.result_type], DecodedType::Void) {
            return Err(semantic("LLVM void return from a non-void function"));
        }
        self.terminator = Some(Terminator::Return(None));
        Ok(Lowered::Done)
    }

    pub(super) fn return_value(
        &mut self,
        value: &u64,
        record: &llvm_bitcode::bitcode::Record,
        fields: &[u64],
    ) -> Result<Lowered, TranslateError> {
        let (ty, value) = self.operand(*value, None, record, fields)?;
        if ty != self.result_type {
            return Err(semantic("LLVM return type mismatch"));
        }
        self.terminator = Some(Terminator::Return(Some(value)));
        Ok(Lowered::Done)
    }

    pub(super) fn branch(&mut self, target: &u64) -> Result<Lowered, TranslateError> {
        self.terminator = Some(Terminator::Branch(block_id(*target, self.block_count)?));
        Ok(Lowered::Done)
    }

    pub(super) fn cond_branch(
        &mut self,
        true_block: &u64,
        false_block: &u64,
        condition: &u64,
        record: &llvm_bitcode::bitcode::Record,
        fields: &[u64],
    ) -> Result<Lowered, TranslateError> {
        let (ty, condition) = self.operand(*condition, None, record, fields)?;
        if !matches!(self.types[ty], DecodedType::Integer(1)) {
            return Err(semantic("LLVM conditional branch requires i1"));
        }
        self.terminator = Some(Terminator::CondBranch {
            condition,
            true_block: block_id(*true_block, self.block_count)?,
            false_block: block_id(*false_block, self.block_count)?,
        });
        Ok(Lowered::Done)
    }

    pub(super) fn switch(
        &mut self,
        condition: &u64,
        default: &u64,
        cases: &[u64],
        record: &llvm_bitcode::bitcode::Record,
        fields: &[u64],
    ) -> Result<Lowered, TranslateError> {
        let (condition_type, condition) = self.operand(*condition, None, record, fields)?;
        if !matches!(self.types[condition_type], DecodedType::Integer(_)) {
            return Err(semantic("LLVM switch condition is not an integer"));
        }
        let mut decoded = Vec::new();
        for case in cases.as_chunks::<2>().0 {
            // A case value is an absolute value reference,
            // unlike the relatively numbered condition.
            let value = self.table.integer(
                usize::try_from(case[0])
                    .map_err(|_| semantic("LLVM switch case does not fit in memory"))?,
                self.types,
            )?;
            decoded.push((value, block_id(case[1], self.block_count)?));
        }
        self.switches.push(PendingSwitch {
            block: self.block_index,
            condition,
            ty: ir_type(condition_type, self.types)?,
            default: block_id(*default, self.block_count)?,
            cases: decoded,
        });
        self.terminator = Some(Terminator::Unreachable);
        Ok(Lowered::Done)
    }

    pub(super) fn unreachable_record(&mut self) -> Result<Lowered, TranslateError> {
        self.terminator = Some(Terminator::Unreachable);
        Ok(Lowered::Done)
    }

    pub(super) fn phi(&mut self, ty: &u64, incoming: &[u64]) -> Result<Lowered, TranslateError> {
        if !incoming.len().is_multiple_of(2) {
            return Err(semantic("LLVM phi has an odd operand count"));
        }
        let ty = type_reference(Some(ty), self.types.len())?;
        let mut sources = Vec::new();
        for pair in incoming.as_chunks::<2>().0 {
            // Phi values use signed relative references, because
            // an incoming value may be defined after the phi.
            let delta = decode_sign_rotated(pair[0]);
            let id = i64::try_from(self.table.values.len())
                .map_err(|_| semantic("LLVM value table exceeds 64 bits"))?
                - delta;
            let id =
                usize::try_from(id).map_err(|_| semantic("LLVM phi refers to a negative value"))?;
            sources.push((block_id(pair[1], self.block_count)?, id));
        }
        let value = emit(
            &mut self.function,
            &mut self.instructions,
            ir_type(ty, self.types)?,
            InstructionKind::Phi {
                incomings: Vec::new(),
            },
        );
        self.phi_fixups.push(PhiFixup {
            block: self.block_index,
            instruction: self.instructions.len() - 1,
            sources,
        });
        define(
            &mut self.forward,
            self.table.values.len(),
            value,
            &mut self.operands,
            &self.function,
            &mut self.instructions,
        )?;
        self.table.values.push(Value { ty, constant: None });
        Ok(Lowered::Done)
    }
}
