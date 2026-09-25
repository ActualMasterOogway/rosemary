use super::*;

/// Lower the scalar address expressions used by some apps buffer blits. This
/// deliberately accepts only named `uint3` components and `uint` arithmetic;
/// unlike a textual substitution it leaves every load and operation explicit
/// in Metal SSA.
pub(super) struct LoweredIndex {
    pub(super) value: ValueId,
    pub(super) instructions: Vec<Instruction>,
}

#[derive(Clone, Debug)]
pub(super) enum IndexExpression {
    Component {
        base: String,
        field: Option<String>,
        component: u32,
    },
    Add(Box<Self>, Box<Self>),
    Multiply(Box<Self>, Box<Self>),
}

pub(super) struct IndexBindings<'a> {
    pub(super) gid_name: &'a str,
    pub(super) gid: ValueId,
    pub(super) params_name: &'a str,
    pub(super) params: ValueId,
    pub(super) structure: &'a StructType,
}

pub(super) fn lower_blit_index(
    expression: &str,
    bindings: IndexBindings<'_>,
    values: &mut Vec<Type>,
    source: &str,
    offset: usize,
) -> Result<LoweredIndex, TranslateError> {
    let expression = IndexParser::new(expression).parse().map_err(|message| {
        syntax_at(
            source,
            offset,
            format!("unsupported MSL buffer element load ({message})"),
        )
    })?;
    let mut lowerer = IndexLowerer {
        bindings,
        values,
        instructions: Vec::new(),
    };
    let value = lowerer.lower(&expression).map_err(|message| {
        syntax_at(
            source,
            offset,
            format!("unsupported MSL buffer element load ({message})"),
        )
    })?;
    Ok(LoweredIndex {
        value,
        instructions: lowerer.instructions,
    })
}

pub(super) struct IndexLowerer<'a> {
    pub(super) bindings: IndexBindings<'a>,
    pub(super) values: &'a mut Vec<Type>,
    pub(super) instructions: Vec<Instruction>,
}

impl IndexLowerer<'_> {
    pub(super) fn add(&mut self, ty: Type) -> ValueId {
        let value = ValueId(self.values.len() as u32);
        self.values.push(ty);
        value
    }

    pub(super) fn lower(&mut self, expression: &IndexExpression) -> Result<ValueId, String> {
        match expression {
            IndexExpression::Add(lhs, rhs) | IndexExpression::Multiply(lhs, rhs) => {
                let lhs = self.lower(lhs)?;
                let rhs = self.lower(rhs)?;
                let result = self.add(Type::u32());
                self.instructions.push(Instruction {
                    result: Some(result),
                    kind: if matches!(expression, IndexExpression::Add(_, _)) {
                        InstructionKind::IntegerBinary {
                            op: IntegerBinaryOp::Add,
                            lhs: Operand::Value(lhs),
                            rhs: Operand::Value(rhs),
                        }
                    } else {
                        InstructionKind::IntegerBinary {
                            op: IntegerBinaryOp::Multiply,
                            lhs: Operand::Value(lhs),
                            rhs: Operand::Value(rhs),
                        }
                    },
                });
                Ok(result)
            }
            IndexExpression::Component {
                base,
                field,
                component,
            } if base == self.bindings.gid_name && field.is_none() => {
                let result = self.add(Type::u32());
                self.instructions.push(Instruction {
                    result: Some(result),
                    kind: InstructionKind::CompositeExtract {
                        composite: Operand::Value(self.bindings.gid),
                        index: *component,
                    },
                });
                Ok(result)
            }
            IndexExpression::Component {
                base,
                field: Some(field),
                component,
            } if base == self.bindings.params_name => {
                let (field_index, field_type) = self
                    .bindings
                    .structure
                    .fields
                    .iter()
                    .enumerate()
                    .find(|(_, candidate)| candidate.name.as_deref() == Some(field.as_str()))
                    .ok_or_else(|| format!("unknown parameter field `{field}`"))?;
                if field_type.ty
                    != (Type::Vector {
                        element: Box::new(Type::u32()),
                        lanes: 2,
                    })
                    && field_type.ty
                        != (Type::Vector {
                            element: Box::new(Type::u32()),
                            lanes: 3,
                        })
                {
                    return Err(format!("parameter field `{field}` is not uint2 or uint3"));
                }
                let vector = self.add(field_type.ty.clone());
                self.instructions.push(Instruction {
                    result: Some(vector),
                    kind: InstructionKind::CompositeExtract {
                        composite: Operand::Value(self.bindings.params),
                        index: field_index as u32,
                    },
                });
                let result = self.add(Type::u32());
                self.instructions.push(Instruction {
                    result: Some(result),
                    kind: InstructionKind::CompositeExtract {
                        composite: Operand::Value(vector),
                        index: *component,
                    },
                });
                Ok(result)
            }
            IndexExpression::Component { base, .. } => {
                Err(format!("unknown address base `{base}`"))
            }
        }
    }
}

pub(super) struct IndexParser<'a> {
    pub(super) input: &'a [u8],
    pub(super) position: usize,
}

impl<'a> IndexParser<'a> {
    pub(super) fn new(input: &'a str) -> Self {
        Self {
            input: input.as_bytes(),
            position: 0,
        }
    }

    pub(super) fn parse(mut self) -> Result<IndexExpression, String> {
        let expression = self.addition()?;
        self.space();
        if self.position != self.input.len() {
            return Err("unexpected token".to_string());
        }
        Ok(expression)
    }

    pub(super) fn addition(&mut self) -> Result<IndexExpression, String> {
        let mut expression = self.multiplication()?;
        loop {
            self.space();
            if !self.take(b'+') {
                return Ok(expression);
            }
            expression =
                IndexExpression::Add(Box::new(expression), Box::new(self.multiplication()?));
        }
    }

    pub(super) fn multiplication(&mut self) -> Result<IndexExpression, String> {
        let mut expression = self.primary()?;
        loop {
            self.space();
            if !self.take(b'*') {
                return Ok(expression);
            }
            expression = IndexExpression::Multiply(Box::new(expression), Box::new(self.primary()?));
        }
    }

    pub(super) fn primary(&mut self) -> Result<IndexExpression, String> {
        self.space();
        if self.take(b'(') {
            let expression = self.addition()?;
            self.space();
            if !self.take(b')') {
                return Err("unclosed parenthesis".to_string());
            }
            return Ok(expression);
        }
        let base = self.identifier()?;
        self.dot()?;
        let second = self.identifier()?;
        self.space();
        if self.take(b'.') {
            let component = component(&self.identifier()?)?;
            Ok(IndexExpression::Component {
                base,
                field: Some(second),
                component,
            })
        } else {
            Ok(IndexExpression::Component {
                base,
                field: None,
                component: component(&second)?,
            })
        }
    }

    pub(super) fn identifier(&mut self) -> Result<String, String> {
        self.space();
        let start = self.position;
        while self.position < self.input.len()
            && (self.input[self.position].is_ascii_alphanumeric()
                || self.input[self.position] == b'_')
        {
            self.position += 1;
        }
        if start == self.position {
            return Err("expected identifier".to_string());
        }
        Ok(std::str::from_utf8(&self.input[start..self.position])
            .unwrap()
            .to_string())
    }
    pub(super) fn dot(&mut self) -> Result<(), String> {
        self.space();
        if self.take(b'.') {
            Ok(())
        } else {
            Err("expected component selector".to_string())
        }
    }
    pub(super) fn space(&mut self) {
        while self.position < self.input.len() && self.input[self.position].is_ascii_whitespace() {
            self.position += 1;
        }
    }
    pub(super) fn take(&mut self, byte: u8) -> bool {
        if self.input.get(self.position) == Some(&byte) {
            self.position += 1;
            true
        } else {
            false
        }
    }
}

fn component(name: &str) -> Result<u32, String> {
    match name {
        "x" => Ok(0),
        "y" => Ok(1),
        "z" => Ok(2),
        _ => Err("invalid vector component".to_string()),
    }
}
