use super::*;

impl Emitter {
    /// Emit one such instruction.
    pub(super) fn emit_arithmetic(
        &mut self,
        function: &Function,
        instruction: &crate::ir::Instruction,
        values: &HashMap<ValueId, u32>,
        result_id: Option<u32>,
    ) -> Result<(), TranslateError> {
        match &instruction.kind {
            InstructionKind::IntegerBinary {
                op:
                    op @ (IntegerBinaryOp::MinimumSigned
                    | IntegerBinaryOp::MinimumUnsigned
                    | IntegerBinaryOp::MaximumSigned
                    | IntegerBinaryOp::MaximumUnsigned),
                lhs,
                rhs,
            } => {
                let operation = match op {
                    IntegerBinaryOp::MinimumSigned => GlslStd450Op::SMin,
                    IntegerBinaryOp::MinimumUnsigned => GlslStd450Op::UMin,
                    IntegerBinaryOp::MaximumSigned => GlslStd450Op::SMax,
                    _ => GlslStd450Op::UMax,
                };
                self.emit_ext_inst(function, instruction, values, operation, [lhs, rhs])?;
            }
            // LLVM types Metal's `bool` as `i1`, so a bitwise operation on
            // booleans stands in for a logical one; SPIR-V keeps them apart.
            InstructionKind::IntegerBinary { op, lhs, rhs }
                if instruction
                    .result
                    .and_then(|result| function.values.get(result.0 as usize))
                    .is_some_and(is_boolean_or_boolean_vector) =>
            {
                self.emit_binary(
                    function,
                    instruction,
                    lhs,
                    rhs,
                    values,
                    |b, ty, id, x, y| {
                        match op {
                            IntegerBinaryOp::And => b.logical_and(ty, id, x, y),
                            IntegerBinaryOp::Or => b.logical_or(ty, id, x, y),
                            // Exclusive or on booleans is inequality.
                            _ => b.logical_not_equal(ty, id, x, y),
                        }
                    },
                )?;
            }
            InstructionKind::IntegerBinary { op, lhs, rhs } => {
                self.emit_binary(
                    function,
                    instruction,
                    lhs,
                    rhs,
                    values,
                    |b, ty, id, x, y| match op {
                        IntegerBinaryOp::Add => b.i_add(ty, id, x, y),
                        IntegerBinaryOp::Subtract => b.i_sub(ty, id, x, y),
                        IntegerBinaryOp::Multiply => b.i_mul(ty, id, x, y),
                        IntegerBinaryOp::DivideUnsigned => b.u_div(ty, id, x, y),
                        IntegerBinaryOp::DivideSigned => b.s_div(ty, id, x, y),
                        IntegerBinaryOp::RemainderUnsigned => b.u_mod(ty, id, x, y),
                        IntegerBinaryOp::RemainderSigned => b.s_rem(ty, id, x, y),
                        IntegerBinaryOp::ShiftLeft => b.shift_left_logical(ty, id, x, y),
                        IntegerBinaryOp::ShiftRightLogical => b.shift_right_logical(ty, id, x, y),
                        IntegerBinaryOp::ShiftRightArithmetic => {
                            b.shift_right_arithmetic(ty, id, x, y)
                        }
                        IntegerBinaryOp::And => b.bitwise_and(ty, id, x, y),
                        IntegerBinaryOp::Or => b.bitwise_or(ty, id, x, y),
                        IntegerBinaryOp::Xor => b.bitwise_xor(ty, id, x, y),
                        // Minimum and maximum are extended instructions,
                        // matched by the arm above.
                        _ => unreachable!(),
                    },
                )?;
            }
            InstructionKind::IntegerClamp {
                signed,
                value,
                min,
                max,
            } => {
                let operation = if *signed {
                    GlslStd450Op::SClamp
                } else {
                    GlslStd450Op::UClamp
                };
                self.emit_ext_inst(function, instruction, values, operation, [value, min, max])?;
            }
            InstructionKind::IntegerUnary { op, value } => match op {
                IntegerUnaryOp::AbsoluteSigned => {
                    self.emit_ext_inst(function, instruction, values, GlslStd450Op::SAbs, [value])?;
                }
                IntegerUnaryOp::SignSigned => {
                    self.emit_ext_inst(
                        function,
                        instruction,
                        values,
                        GlslStd450Op::SSign,
                        [value],
                    )?;
                }
                // SPIR-V has no zero count. `FindUMsb` answers -1 for zero,
                // so subtracting it from the last bit index gives Metal's full
                // width there and the leading zero count everywhere else.
                // `FindILsb` answers -1 for zero too, but subtraction does not
                // rescue it, so zero is selected for.
                IntegerUnaryOp::CountLeadingZeros | IntegerUnaryOp::CountTrailingZeros => {
                    let result = instruction
                        .result
                        .ok_or_else(|| emission("a zero count has no result"))?;
                    let result_type = &function.values[result.0 as usize];
                    let Type::Int { bits, .. } = result_type else {
                        return Err(emission("a zero count applies to an integer"));
                    };
                    let width = u64::from(*bits);
                    let ty = self.type_id(result_type)?;
                    let scalar = result_type.clone();
                    let value = self.operand(value, values)?;
                    let leading = *op == IntegerUnaryOp::CountLeadingZeros;
                    let found = self.builder.id();
                    self.builder
                        .ext_inst(
                            ty,
                            Some(found),
                            self.glsl_std_450,
                            match leading {
                                true => GlslStd450Op::FindUMsb,
                                false => GlslStd450Op::FindILsb,
                            } as u32,
                            [SpirvOperand::IdRef(value)],
                        )
                        .map_err(build_error)?;
                    if leading {
                        let last = self.constant(&scalar, width - 1)?;
                        self.builder
                            .i_sub(ty, result_id, last, found)
                            .map_err(build_error)?;
                    } else {
                        let zero = self.constant(&scalar, 0)?;
                        let full = self.constant(&scalar, width)?;
                        let bool_type = self.type_id(&Type::Bool)?;
                        let empty = self.builder.id();
                        self.builder
                            .i_equal(bool_type, Some(empty), value, zero)
                            .map_err(build_error)?;
                        self.builder
                            .select(ty, result_id, empty, full, found)
                            .map_err(build_error)?;
                    }
                }
                IntegerUnaryOp::FindMostSignificantBitUnsigned => {
                    self.emit_ext_inst(
                        function,
                        instruction,
                        values,
                        GlslStd450Op::FindUMsb,
                        [value],
                    )?;
                }
                IntegerUnaryOp::PopulationCount | IntegerUnaryOp::Not => {
                    let result = instruction
                        .result
                        .ok_or_else(|| emission("integer math call has no result"))?;
                    let ty = self.type_id(&function.values[result.0 as usize])?;
                    let value = self.operand(value, values)?;
                    if *op == IntegerUnaryOp::PopulationCount {
                        self.builder.bit_count(ty, result_id, value)
                    } else {
                        self.builder.not(ty, result_id, value)
                    }
                    .map_err(build_error)?;
                }
            },
            InstructionKind::IntegerCompare {
                predicate,
                lhs,
                rhs,
            } => {
                self.emit_binary(
                    function,
                    instruction,
                    lhs,
                    rhs,
                    values,
                    |b, ty, id, x, y| match predicate {
                        IntegerPredicate::Equal => b.i_equal(ty, id, x, y),
                        IntegerPredicate::NotEqual => b.i_not_equal(ty, id, x, y),
                        IntegerPredicate::UnsignedLessThan => b.u_less_than(ty, id, x, y),
                        IntegerPredicate::UnsignedLessEqual => b.u_less_than_equal(ty, id, x, y),
                        IntegerPredicate::UnsignedGreaterThan => b.u_greater_than(ty, id, x, y),
                        IntegerPredicate::UnsignedGreaterEqual => {
                            b.u_greater_than_equal(ty, id, x, y)
                        }
                        IntegerPredicate::SignedLessThan => b.s_less_than(ty, id, x, y),
                        IntegerPredicate::SignedLessEqual => b.s_less_than_equal(ty, id, x, y),
                        IntegerPredicate::SignedGreaterThan => b.s_greater_than(ty, id, x, y),
                        IntegerPredicate::SignedGreaterEqual => {
                            b.s_greater_than_equal(ty, id, x, y)
                        }
                    },
                )?;
            }
            InstructionKind::FloatCompare {
                predicate,
                lhs,
                rhs,
            } => {
                self.emit_binary(
                    function,
                    instruction,
                    lhs,
                    rhs,
                    values,
                    |b, ty, id, x, y| match predicate {
                        FloatPredicate::OrderedEqual => b.f_ord_equal(ty, id, x, y),
                        FloatPredicate::OrderedNotEqual => b.f_ord_not_equal(ty, id, x, y),
                        FloatPredicate::OrderedLessThan => b.f_ord_less_than(ty, id, x, y),
                        FloatPredicate::OrderedLessEqual => b.f_ord_less_than_equal(ty, id, x, y),
                        FloatPredicate::OrderedGreaterThan => b.f_ord_greater_than(ty, id, x, y),
                        FloatPredicate::OrderedGreaterEqual => {
                            b.f_ord_greater_than_equal(ty, id, x, y)
                        }
                        FloatPredicate::Ordered => b.ordered(ty, id, x, y),
                        FloatPredicate::UnorderedEqual => b.f_unord_equal(ty, id, x, y),
                        FloatPredicate::UnorderedNotEqual => b.f_unord_not_equal(ty, id, x, y),
                        FloatPredicate::UnorderedLessThan => b.f_unord_less_than(ty, id, x, y),
                        FloatPredicate::UnorderedLessEqual => {
                            b.f_unord_less_than_equal(ty, id, x, y)
                        }
                        FloatPredicate::UnorderedGreaterThan => {
                            b.f_unord_greater_than(ty, id, x, y)
                        }
                        FloatPredicate::UnorderedGreaterEqual => {
                            b.f_unord_greater_than_equal(ty, id, x, y)
                        }
                        FloatPredicate::Unordered => b.unordered(ty, id, x, y),
                    },
                )?;
            }
            InstructionKind::FloatBinary { op, lhs, rhs } => match op {
                FloatBinaryOp::Add
                | FloatBinaryOp::Subtract
                | FloatBinaryOp::Multiply
                | FloatBinaryOp::Divide
                | FloatBinaryOp::Remainder => {
                    self.emit_binary(
                        function,
                        instruction,
                        lhs,
                        rhs,
                        values,
                        |b, ty, id, x, y| match op {
                            FloatBinaryOp::Add => b.f_add(ty, id, x, y),
                            FloatBinaryOp::Subtract => b.f_sub(ty, id, x, y),
                            FloatBinaryOp::Multiply => b.f_mul(ty, id, x, y),
                            FloatBinaryOp::Divide => b.f_div(ty, id, x, y),
                            _ => b.f_rem(ty, id, x, y),
                        },
                    )?;
                }
                FloatBinaryOp::Minimum => {
                    self.emit_ext_inst(
                        function,
                        instruction,
                        values,
                        GlslStd450Op::FMin,
                        [lhs, rhs],
                    )?;
                }
                FloatBinaryOp::Maximum => {
                    self.emit_ext_inst(
                        function,
                        instruction,
                        values,
                        GlslStd450Op::FMax,
                        [lhs, rhs],
                    )?;
                }
                FloatBinaryOp::Power => {
                    self.emit_ext_inst(
                        function,
                        instruction,
                        values,
                        GlslStd450Op::Pow,
                        [lhs, rhs],
                    )?;
                }
                FloatBinaryOp::Atan2 => {
                    self.emit_ext_inst(
                        function,
                        instruction,
                        values,
                        GlslStd450Op::Atan2,
                        [lhs, rhs],
                    )?;
                }
                FloatBinaryOp::Step => {
                    self.emit_ext_inst(
                        function,
                        instruction,
                        values,
                        GlslStd450Op::Step,
                        [lhs, rhs],
                    )?;
                }
            },
            InstructionKind::FloatUnary { op, value } => match op {
                FloatUnaryOp::Negate => {
                    let result = instruction
                        .result
                        .ok_or_else(|| emission("negation has no result"))?;
                    let ty = self.type_id(&function.values[result.0 as usize])?;
                    let value = self.operand(value, values)?;
                    self.builder
                        .f_negate(ty, result_id, value)
                        .map_err(build_error)?;
                }
                FloatUnaryOp::DerivativeX
                | FloatUnaryOp::DerivativeY
                | FloatUnaryOp::DerivativeWidth => {
                    let result = instruction
                        .result
                        .ok_or_else(|| emission("derivative has no result"))?;
                    let ty = self.type_id(&function.values[result.0 as usize])?;
                    let value = self.operand(value, values)?;
                    let b = &mut self.builder;
                    match op {
                        FloatUnaryOp::DerivativeX => b.d_pdx(ty, result_id, value),
                        FloatUnaryOp::DerivativeY => b.d_pdy(ty, result_id, value),
                        _ => b.fwidth(ty, result_id, value),
                    }
                    .map_err(build_error)?;
                }
                _ => {
                    let operation = match op {
                        FloatUnaryOp::Absolute => GlslStd450Op::FAbs,
                        FloatUnaryOp::Sign => GlslStd450Op::FSign,
                        FloatUnaryOp::Sqrt => GlslStd450Op::Sqrt,
                        FloatUnaryOp::InverseSqrt => GlslStd450Op::InverseSqrt,
                        FloatUnaryOp::Exp => GlslStd450Op::Exp,
                        FloatUnaryOp::Exp2 => GlslStd450Op::Exp2,
                        FloatUnaryOp::Log => GlslStd450Op::Log,
                        FloatUnaryOp::Log2 => GlslStd450Op::Log2,
                        FloatUnaryOp::Sin => GlslStd450Op::Sin,
                        FloatUnaryOp::Cos => GlslStd450Op::Cos,
                        FloatUnaryOp::Tan => GlslStd450Op::Tan,
                        FloatUnaryOp::Asin => GlslStd450Op::Asin,
                        FloatUnaryOp::Acos => GlslStd450Op::Acos,
                        FloatUnaryOp::Atan => GlslStd450Op::Atan,
                        FloatUnaryOp::Floor => GlslStd450Op::Floor,
                        FloatUnaryOp::Ceil => GlslStd450Op::Ceil,
                        FloatUnaryOp::Round => GlslStd450Op::RoundEven,
                        FloatUnaryOp::Truncate => GlslStd450Op::Trunc,
                        FloatUnaryOp::Fract => GlslStd450Op::Fract,
                        FloatUnaryOp::Negate
                        | FloatUnaryOp::DerivativeX
                        | FloatUnaryOp::DerivativeY
                        | FloatUnaryOp::DerivativeWidth => {
                            return Err(emission("unreachable floating unary lowering"));
                        }
                    };
                    self.emit_ext_inst(function, instruction, values, operation, [value])?;
                }
            },
            InstructionKind::FloatFma {
                first,
                second,
                addend,
            } => {
                self.emit_ext_inst(
                    function,
                    instruction,
                    values,
                    GlslStd450Op::Fma,
                    [first, second, addend],
                )?;
            }
            InstructionKind::FloatDot { lhs, rhs } => self.emit_binary(
                function,
                instruction,
                lhs,
                rhs,
                values,
                |builder, ty, id, a, b| builder.dot(ty, id, a, b),
            )?,
            InstructionKind::FloatClamp { value, min, max } => {
                self.emit_ext_inst(
                    function,
                    instruction,
                    values,
                    GlslStd450Op::FClamp,
                    [value, min, max],
                )?;
            }
            InstructionKind::FloatMix {
                first,
                second,
                weight,
            } => {
                self.emit_ext_inst(
                    function,
                    instruction,
                    values,
                    GlslStd450Op::FMix,
                    [first, second, weight],
                )?;
            }
            _ => unreachable!("not an arithmetic instruction"),
        }
        Ok(())
    }
}
