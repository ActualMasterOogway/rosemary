//! A deliberately small, semantics-preserving MSL source frontend.
//!
//! The first supported production shape is a compute kernel which writes an
//! integer literal to element zero of a `device uint*` buffer.  It is useful as
//! a vertical source-compiler slice and, importantly, rejects every construct
//! whose Metal semantics have not been implemented yet.

use std::collections::HashMap;

use crate::ir::{
    Access, AddressSpace, Block, BlockId, EntryPoint, Function, Instruction, InstructionKind,
    Module, Operand, Parameter, Resource, ResourceKind, Stage, StructField, StructType, Terminator,
    Type, ValueId,
};
use crate::{Diagnostic, Phase, TranslateError};

/// Parse the source-MSL subset currently implemented by the translator.
pub fn parse_msl(source: &str) -> Result<Module, TranslateError> {
    let source = strip_comments(source);
    let structs = parse_structs(&source)?;
    let kernel = source
        .find("kernel void")
        .ok_or_else(|| syntax_at(&source, 0, "MSL source has no `kernel void` entry point"))?;
    let after_kernel = source[kernel + "kernel void".len()..].trim_start();
    let name_end = after_kernel
        .find(|character: char| !character.is_ascii_alphanumeric() && character != '_')
        .ok_or_else(|| syntax_at(&source, kernel, "MSL kernel has no parameter list"))?;
    let name = after_kernel[..name_end].trim();
    if name.is_empty() {
        return Err(syntax_at(&source, kernel, "MSL kernel has no name"));
    }
    let after_name = &after_kernel[name_end..];
    let open = after_name
        .find('(')
        .ok_or_else(|| syntax_at(&source, kernel, "MSL kernel has no parameter list"))?;
    let close = matching(after_name, open, '(', ')').ok_or_else(|| {
        syntax_at(
            &source,
            kernel + "kernel void".len() + name_end + open,
            "MSL kernel has an unterminated parameter list",
        )
    })?;
    let body_open = after_name[close + 1..]
        .find('{')
        .map(|offset| close + 1 + offset)
        .ok_or_else(|| {
            syntax_at(
                &source,
                kernel + "kernel void".len() + name_end + close,
                "MSL kernel has no body",
            )
        })?;
    let body_close = matching(after_name, body_open, '{', '}').ok_or_else(|| {
        syntax_at(
            &source,
            kernel + "kernel void".len() + name_end + body_open,
            "MSL kernel has an unterminated body",
        )
    })?;
    let parameters = &after_name[open + 1..close];
    let body = &after_name[body_open + 1..body_close];

    let mut resources = Vec::new();
    let mut function_parameters = Vec::new();
    let mut values = Vec::new();
    let mut builtins = HashMap::new();
    for raw in split_parameters(parameters) {
        let offset = source.find(raw).unwrap_or(0);
        let (declaration, attribute) = raw
            .split_once("[[")
            .ok_or_else(|| syntax_at(&source, offset, "MSL parameter has no attribute"))?;
        let attribute = attribute
            .strip_suffix("]]")
            .ok_or_else(|| syntax_at(&source, offset, "MSL parameter attribute is malformed"))?
            .trim();
        let declaration = declaration.trim();
        let name = declaration
            .split_whitespace()
            .last()
            .ok_or_else(|| syntax_at(&source, offset, "MSL parameter has no name"))?;
        let ty = parse_parameter_type(declaration, name, &structs)
            .map_err(|message| syntax_at(&source, offset, message))?;
        let value = ValueId(values.len() as u32);
        values.push(ty.clone());
        if attribute.starts_with("thread_position_in_grid") {
            builtins.insert(name.to_string(), value);
            continue;
        }
        let argument = function_parameters.len() as u32;
        function_parameters.push(Parameter {
            value,
            name: name.to_string(),
            ty: ty.clone(),
        });
        if let Some((kind, index, access)) = parse_resource_attribute(attribute, declaration, &ty) {
            resources.push(Resource {
                argument,
                kind,
                index,
                count: 1,
                access,
                ty,
                name: Some(name.to_string()),
            });
        } else {
            return Err(syntax_at(
                &source,
                offset,
                "unsupported MSL parameter attribute",
            ));
        }
    }
    if body.trim_start().starts_with("if") {
        return parse_blit_kernel(
            &source,
            name,
            body,
            function_parameters,
            values,
            resources,
            builtins,
        );
    }
    let Some(writable_buffer) = resources.iter().find(|resource| {
        resource.kind == ResourceKind::Buffer
            && resource.access != Access::Read
            && resource.ty
                == Type::Pointer {
                    space: AddressSpace::Device,
                    pointee: Box::new(Type::u32()),
                }
    }) else {
        return Err(syntax_at(
            &source,
            source.find(body).unwrap_or(0),
            "MSL function body requires operations not implemented by the source frontend",
        ));
    };
    let parameter_value = function_parameters[writable_buffer.argument as usize].value;
    let prefix = format!(
        "{}[0] =",
        function_parameters[writable_buffer.argument as usize].name
    );
    let statement = body.trim().trim_end_matches(';').trim();
    let value = statement
        .strip_prefix(&prefix)
        .ok_or_else(|| {
            syntax_at(
                &source,
                source.find(body).unwrap_or(0),
                "only `buffer[0] = integer_literal;` is supported",
            )
        })?
        .trim()
        .parse::<u32>()
        .map_err(|_| {
            syntax_at(
                &source,
                source.find(body).unwrap_or(0),
                "MSL store value is not a `uint` literal",
            )
        })?;

    let module = Module {
        functions: vec![Function {
            name: name.to_string(),
            parameters: function_parameters,
            values,
            blocks: vec![Block {
                id: BlockId(0),
                name: "entry".to_string(),
                instructions: vec![Instruction {
                    result: None,
                    kind: InstructionKind::Store {
                        pointer: Operand::Value(parameter_value),
                        value: Operand::u32(value),
                    },
                }],
                terminator: Terminator::Return(None),
            }],
        }],
        entries: vec![EntryPoint {
            function: 0,
            stage: Stage::Kernel,
            resources,
        }],
    };
    module.verify()?;
    Ok(module)
}

fn parse_blit_kernel(
    source: &str,
    name: &str,
    body: &str,
    parameters: Vec<Parameter>,
    mut values: Vec<Type>,
    resources: Vec<Resource>,
    builtins: HashMap<String, ValueId>,
) -> Result<Module, TranslateError> {
    let guard = body.trim_start();
    let open = guard.find('(').ok_or_else(|| {
        syntax_at(
            source,
            source.find(body).unwrap_or(0),
            "MSL blit guard is malformed",
        )
    })?;
    let close = matching(guard, open, '(', ')').ok_or_else(|| {
        syntax_at(
            source,
            source.find(guard).unwrap_or(0),
            "MSL blit guard is malformed",
        )
    })?;
    let condition = guard[open + 1..close].trim();
    let (gid_name, size_expression) = condition
        .strip_prefix("all(")
        .and_then(|value| value.strip_suffix(')'))
        .and_then(|value| value.split_once('<'))
        .ok_or_else(|| {
            syntax_at(
                source,
                source.find(guard).unwrap_or(0),
                "unsupported MSL conditional expression",
            )
        })?;
    let gid = builtins.get(gid_name.trim()).copied().ok_or_else(|| {
        syntax_at(
            source,
            source.find(guard).unwrap_or(0),
            "blit guard must use thread_position_in_grid",
        )
    })?;
    let (params_name, field_name) = size_expression.trim().split_once('.').ok_or_else(|| {
        syntax_at(
            source,
            source.find(guard).unwrap_or(0),
            "blit guard must compare against a parameter field",
        )
    })?;
    let params = parameters
        .iter()
        .find(|parameter| parameter.name == params_name.trim())
        .ok_or_else(|| {
            syntax_at(
                source,
                source.find(guard).unwrap_or(0),
                "blit guard parameter is missing",
            )
        })?;
    let Type::Pointer {
        pointee: params_pointee,
        ..
    } = &params.ty
    else {
        return Err(syntax_at(
            source,
            source.find(guard).unwrap_or(0),
            "blit guard parameter is not a buffer",
        ));
    };
    let Type::Struct(structure) = params_pointee.as_ref() else {
        return Err(syntax_at(
            source,
            source.find(guard).unwrap_or(0),
            "blit guard parameter is not a struct",
        ));
    };
    let field = structure
        .fields
        .iter()
        .enumerate()
        .find(|(_, field)| field.name.as_deref() == Some(field_name.trim()))
        .ok_or_else(|| {
            syntax_at(
                source,
                source.find(guard).unwrap_or(0),
                "blit size field is missing",
            )
        })?;
    if field.1.ty
        != (Type::Vector {
            element: Box::new(Type::u32()),
            lanes: 3,
        })
    {
        return Err(syntax_at(
            source,
            source.find(guard).unwrap_or(0),
            "blit size field must be uint3",
        ));
    }
    let guarded = guard[close + 1..].trim_start();
    let body_open = guarded.find('{').ok_or_else(|| {
        syntax_at(
            source,
            source.find(guard).unwrap_or(0),
            "MSL blit guard has no body",
        )
    })?;
    let body_close = matching(guarded, body_open, '{', '}').ok_or_else(|| {
        syntax_at(
            source,
            source.find(guard).unwrap_or(0),
            "MSL blit guard has no body",
        )
    })?;
    let inner = &guarded[body_open + 1..body_close];
    let load = inner.split(';').next().unwrap_or("").trim();
    let (_, index_expression) = load
        .split_once('[')
        .and_then(|(left, right)| right.strip_suffix(']').map(|right| (left, right)))
        .ok_or_else(|| {
            syntax_at(
                source,
                source.find(inner).unwrap_or(0),
                format!(
                    "unsupported MSL buffer element load ({})",
                    blit_statement_id(load)
                ),
            )
        })?;
    let input_name = load
        .split_once('=')
        .map(|(_, right)| right.trim().split('[').next().unwrap_or("").trim())
        .unwrap_or("");
    let input = parameters
        .iter()
        .find(|parameter| parameter.name == input_name)
        .ok_or_else(|| {
            syntax_at(
                source,
                source.find(inner).unwrap_or(0),
                "blit input buffer is missing",
            )
        })?;
    let Type::Pointer {
        space: AddressSpace::Device,
        pointee: input_pointee,
    } = &input.ty
    else {
        return Err(syntax_at(
            source,
            source.find(inner).unwrap_or(0),
            "blit input must be a device buffer",
        ));
    };
    let element_type = input_pointee.as_ref().clone();
    if element_type
        != (Type::Vector {
            element: Box::new(Type::Int {
                bits: 8,
                signed: false,
            }),
            lanes: 4,
        })
    {
        return Err(syntax_at(
            source,
            source.find(inner).unwrap_or(0),
            format!(
                "unsupported MSL buffer element load ({})",
                blit_statement_id(load)
            ),
        ));
    }
    let write = inner.split(';').nth(1).unwrap_or("").trim();
    let output_name = write.split(".write(").next().unwrap_or("").trim();
    let output = parameters
        .iter()
        .find(|parameter| parameter.name == output_name)
        .ok_or_else(|| {
            syntax_at(
                source,
                source.find(inner).unwrap_or(0),
                "blit output texture is missing",
            )
        })?;
    if output.ty != Type::Texture
        || !write.contains("float4(")
        || !write.ends_with(&format!(", {})", gid_name.trim()))
            && !write.ends_with(&format!(", {});", gid_name.trim()))
    {
        return Err(syntax_at(
            source,
            source.find(inner).unwrap_or(0),
            "unsupported MSL blit output operation",
        ));
    }
    let add = |values: &mut Vec<Type>, ty: Type| {
        let value = ValueId(values.len() as u32);
        values.push(ty);
        value
    };
    let uint3 = Type::Vector {
        element: Box::new(Type::u32()),
        lanes: 3,
    };
    let bool3 = Type::Vector {
        element: Box::new(Type::Bool),
        lanes: 3,
    };
    let uchar4 = element_type;
    let float4 = Type::Vector {
        element: Box::new(Type::Float { bits: 32 }),
        lanes: 4,
    };
    let params_value = add(&mut values, params_pointee.as_ref().clone());
    let size = add(&mut values, uint3.clone());
    let in_bounds = add(&mut values, bool3);
    let all = add(&mut values, Type::Bool);
    let index = lower_blit_index(
        index_expression.trim(),
        IndexBindings {
            gid_name: gid_name.trim(),
            gid,
            params_name: params_name.trim(),
            params: params_value,
            structure,
        },
        &mut values,
        source,
        source.find(inner).unwrap_or(0),
    )?;
    let element_pointer = add(
        &mut values,
        Type::Pointer {
            space: AddressSpace::Device,
            pointee: Box::new(uchar4.clone()),
        },
    );
    let element = add(&mut values, uchar4);
    let converted = add(&mut values, float4.clone());
    let divisor = add(&mut values, float4.clone());
    let normalized = add(&mut values, float4);
    let entry = Block {
        id: BlockId(0),
        name: "entry".to_string(),
        instructions: vec![
            Instruction {
                result: Some(gid),
                kind: InstructionKind::GlobalInvocationId,
            },
            Instruction {
                result: Some(params_value),
                kind: InstructionKind::Load {
                    pointer: Operand::Value(params.value),
                },
            },
            Instruction {
                result: Some(size),
                kind: InstructionKind::CompositeExtract {
                    composite: Operand::Value(params_value),
                    index: field.0 as u32,
                },
            },
            Instruction {
                result: Some(in_bounds),
                kind: InstructionKind::VectorUnsignedLessThan {
                    lhs: Operand::Value(gid),
                    rhs: Operand::Value(size),
                },
            },
            Instruction {
                result: Some(all),
                kind: InstructionKind::All {
                    vector: Operand::Value(in_bounds),
                },
            },
        ],
        terminator: Terminator::CondBranch {
            condition: Operand::Value(all),
            true_block: BlockId(1),
            false_block: BlockId(2),
        },
    };
    let write_block = Block {
        id: BlockId(1),
        name: "write".to_string(),
        instructions: {
            let mut instructions = index.instructions;
            instructions.extend([
                Instruction {
                    result: Some(element_pointer),
                    kind: InstructionKind::ElementAddress {
                        pointer: Operand::Value(input.value),
                        index: Operand::Value(index.value),
                    },
                },
                Instruction {
                    result: Some(element),
                    kind: InstructionKind::Load {
                        pointer: Operand::Value(element_pointer),
                    },
                },
                Instruction {
                    result: Some(converted),
                    kind: InstructionKind::ConvertUToF {
                        value: Operand::Value(element),
                    },
                },
                Instruction {
                    result: Some(divisor),
                    kind: InstructionKind::VectorSplat {
                        value: Operand::f32(255.0),
                        lanes: 4,
                    },
                },
                Instruction {
                    result: Some(normalized),
                    kind: InstructionKind::FloatDiv {
                        lhs: Operand::Value(converted),
                        rhs: Operand::Value(divisor),
                    },
                },
                Instruction {
                    result: None,
                    kind: InstructionKind::TextureWrite {
                        texture: Operand::Value(output.value),
                        coordinate: Operand::Value(gid),
                        value: Operand::Value(normalized),
                    },
                },
            ]);
            instructions
        },
        terminator: Terminator::Return(None),
    };
    let module = Module {
        functions: vec![Function {
            name: name.to_string(),
            parameters,
            values,
            blocks: vec![
                entry,
                write_block,
                Block {
                    id: BlockId(2),
                    name: "out_of_bounds".to_string(),
                    instructions: vec![],
                    terminator: Terminator::Return(None),
                },
            ],
        }],
        entries: vec![EntryPoint {
            function: 0,
            stage: Stage::Kernel,
            resources,
        }],
    };
    module.verify()?;
    Ok(module)
}

/// Lower the scalar address expressions used by some apps buffer blits. This
/// deliberately accepts only named `uint3` components and `uint` arithmetic;
/// unlike a textual substitution it leaves every load and operation explicit
/// in Metal SSA.
struct LoweredIndex {
    value: ValueId,
    instructions: Vec<Instruction>,
}

#[derive(Clone, Debug)]
enum IndexExpression {
    Component {
        base: String,
        field: Option<String>,
        component: u32,
    },
    Add(Box<Self>, Box<Self>),
    Multiply(Box<Self>, Box<Self>),
}

struct IndexBindings<'a> {
    gid_name: &'a str,
    gid: ValueId,
    params_name: &'a str,
    params: ValueId,
    structure: &'a StructType,
}

fn lower_blit_index(
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

struct IndexLowerer<'a> {
    bindings: IndexBindings<'a>,
    values: &'a mut Vec<Type>,
    instructions: Vec<Instruction>,
}

impl IndexLowerer<'_> {
    fn add(&mut self, ty: Type) -> ValueId {
        let value = ValueId(self.values.len() as u32);
        self.values.push(ty);
        value
    }

    fn lower(&mut self, expression: &IndexExpression) -> Result<ValueId, String> {
        match expression {
            IndexExpression::Add(lhs, rhs) | IndexExpression::Multiply(lhs, rhs) => {
                let lhs = self.lower(lhs)?;
                let rhs = self.lower(rhs)?;
                let result = self.add(Type::u32());
                self.instructions.push(Instruction {
                    result: Some(result),
                    kind: if matches!(expression, IndexExpression::Add(_, _)) {
                        InstructionKind::IntegerAdd {
                            lhs: Operand::Value(lhs),
                            rhs: Operand::Value(rhs),
                        }
                    } else {
                        InstructionKind::IntegerMultiply {
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

struct IndexParser<'a> {
    input: &'a [u8],
    position: usize,
}

impl<'a> IndexParser<'a> {
    fn new(input: &'a str) -> Self {
        Self {
            input: input.as_bytes(),
            position: 0,
        }
    }

    fn parse(mut self) -> Result<IndexExpression, String> {
        let expression = self.addition()?;
        self.space();
        if self.position != self.input.len() {
            return Err("unexpected token".to_string());
        }
        Ok(expression)
    }

    fn addition(&mut self) -> Result<IndexExpression, String> {
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

    fn multiplication(&mut self) -> Result<IndexExpression, String> {
        let mut expression = self.primary()?;
        loop {
            self.space();
            if !self.take(b'*') {
                return Ok(expression);
            }
            expression = IndexExpression::Multiply(Box::new(expression), Box::new(self.primary()?));
        }
    }

    fn primary(&mut self) -> Result<IndexExpression, String> {
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

    fn identifier(&mut self) -> Result<String, String> {
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
    fn dot(&mut self) -> Result<(), String> {
        self.space();
        if self.take(b'.') {
            Ok(())
        } else {
            Err("expected component selector".to_string())
        }
    }
    fn space(&mut self) {
        while self.position < self.input.len() && self.input[self.position].is_ascii_whitespace() {
            self.position += 1;
        }
    }
    fn take(&mut self, byte: u8) -> bool {
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

/// A bounded diagnostic identifier for a rejected source statement. Source
/// text is intentionally never included in diagnostics or tracing.
fn blit_statement_id(statement: &str) -> String {
    let hash = statement
        .bytes()
        .fold(0xcbf29ce484222325_u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
        });
    format!("{} bytes, fnv1a64={hash:016x}", statement.len())
}

fn parse_resource_attribute(
    attribute: &str,
    declaration: &str,
    ty: &Type,
) -> Option<(ResourceKind, u32, Access)> {
    let (kind, index) = if let Some(index) = attribute.strip_prefix("buffer(") {
        (ResourceKind::Buffer, index.strip_suffix(')')?)
    } else {
        let index = attribute.strip_prefix("texture(")?;
        (ResourceKind::Texture, index.strip_suffix(')')?)
    };
    let index = index.trim().parse().ok()?;
    let access = match ty {
        Type::Pointer {
            space: AddressSpace::Constant,
            ..
        } => Access::Read,
        Type::Texture if declaration.contains("access::write") => Access::Write,
        Type::Texture => Access::Read,
        _ => Access::ReadWrite,
    };
    Some((kind, index, access))
}

fn parse_parameter_type(
    declaration: &str,
    name: &str,
    structs: &HashMap<String, Type>,
) -> Result<Type, String> {
    let ty = declaration
        .strip_suffix(name)
        .ok_or_else(|| "MSL parameter declaration is malformed".to_string())?
        .trim();
    if let Some(pointee) = ty
        .strip_prefix("device ")
        .and_then(|ty| ty.strip_suffix('*'))
    {
        return Ok(Type::Pointer {
            space: AddressSpace::Device,
            pointee: Box::new(parse_value_type(pointee.trim(), structs)?),
        });
    }
    if let Some(pointee) = ty
        .strip_prefix("constant ")
        .and_then(|ty| ty.strip_suffix('&'))
    {
        return Ok(Type::Pointer {
            space: AddressSpace::Constant,
            pointee: Box::new(parse_value_type(pointee.trim(), structs)?),
        });
    }
    if ty.starts_with("texture3d<float,") && ty.contains("access::write") {
        return Ok(Type::Texture);
    }
    if ty.starts_with("texture") {
        return Err("unsupported MSL texture type".to_string());
    }
    parse_value_type(ty, structs)
}

fn parse_structs(source: &str) -> Result<HashMap<String, Type>, TranslateError> {
    let mut structs = HashMap::new();
    let mut remaining = source;
    while let Some(start) = remaining.find("struct ") {
        remaining = &remaining[start + "struct ".len()..];
        let location = source.len() - remaining.len();
        let name_end = remaining
            .find(|character: char| !character.is_ascii_alphanumeric() && character != '_')
            .ok_or_else(|| syntax_at(source, location, "MSL struct has no body"))?;
        let name = remaining[..name_end].trim();
        let open = remaining[name_end..]
            .find('{')
            .map(|offset| name_end + offset)
            .ok_or_else(|| syntax_at(source, location, "MSL struct has no body"))?;
        let close = matching(remaining, open, '{', '}').ok_or_else(|| {
            syntax_at(
                source,
                location + open,
                "MSL struct has an unterminated body",
            )
        })?;
        let mut fields = Vec::new();
        let mut offset = 0;
        let mut alignment = 1;
        for field in remaining[open + 1..close]
            .split(';')
            .map(str::trim)
            .filter(|field| !field.is_empty())
        {
            let mut tokens = field.split_whitespace();
            let ty = tokens.next().ok_or_else(|| {
                syntax_at(source, location + open, "MSL struct field has no type")
            })?;
            let field_name = tokens.next().ok_or_else(|| {
                syntax_at(source, location + open, "MSL struct field has no name")
            })?;
            if tokens.next().is_some() {
                return Err(syntax_at(
                    source,
                    location + open,
                    "unsupported MSL struct field declaration",
                ));
            }
            let ty = parse_value_type(ty, &structs)
                .map_err(|message| syntax_at(source, location + open, message))?;
            let (size, field_alignment) = type_layout(&ty).ok_or_else(|| {
                syntax_at(source, location + open, "unsupported MSL struct field type")
            })?;
            offset = align_to(offset, field_alignment);
            alignment = alignment.max(field_alignment);
            fields.push(StructField {
                name: Some(field_name.to_string()),
                ty,
                offset,
            });
            offset += size;
        }
        let size = align_to(offset, alignment);
        structs.insert(
            name.to_string(),
            Type::Struct(StructType {
                fields,
                size,
                alignment,
                packed: false,
            }),
        );
        remaining = &remaining[close + 1..];
    }
    Ok(structs)
}

fn parse_value_type(name: &str, structs: &HashMap<String, Type>) -> Result<Type, String> {
    if let Some(ty) = structs.get(name) {
        return Ok(ty.clone());
    }
    let base_end = name
        .trim_end_matches(|character: char| character.is_ascii_digit())
        .len();
    let (base, suffix) = name.split_at(base_end);
    let lanes = (!suffix.is_empty())
        .then(|| suffix.parse::<u8>())
        .transpose()
        .map_err(|_| "invalid MSL vector width")?;
    let scalar = match base {
        "uint" => Type::u32(),
        "int" => Type::i32(),
        "uchar" => Type::Int {
            bits: 8,
            signed: false,
        },
        "float" => Type::Float { bits: 32 },
        _ => return Err("unsupported MSL parameter type".to_string()),
    };
    Ok(lanes.map_or(scalar.clone(), |lanes| Type::Vector {
        element: Box::new(scalar),
        lanes,
    }))
}

fn type_layout(ty: &Type) -> Option<(u32, u32)> {
    match ty {
        Type::Int { bits, .. } | Type::Float { bits } => {
            Some((u32::from(*bits) / 8, u32::from(*bits) / 8))
        }
        Type::Vector { element, lanes } => {
            let (size, alignment) = type_layout(element)?;
            Some((
                size * u32::from(*lanes),
                if *lanes == 3 {
                    alignment * 4
                } else {
                    alignment * u32::from(*lanes)
                },
            ))
        }
        _ => None,
    }
}

fn align_to(value: u32, alignment: u32) -> u32 {
    value.next_multiple_of(alignment)
}

fn split_parameters(parameters: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut angle_depth = 0_u32;
    for (offset, character) in parameters.char_indices() {
        match character {
            '<' => angle_depth += 1,
            '>' => angle_depth = angle_depth.saturating_sub(1),
            ',' if angle_depth == 0 => {
                let part = parameters[start..offset].trim();
                if !part.is_empty() {
                    parts.push(part);
                }
                start = offset + character.len_utf8();
            }
            _ => {}
        }
    }
    let part = parameters[start..].trim();
    if !part.is_empty() {
        parts.push(part);
    }
    parts
}

fn matching(text: &str, open: usize, left: char, right: char) -> Option<usize> {
    let mut depth = 0_u32;
    for (offset, character) in text[open..].char_indices() {
        if character == left {
            depth += 1;
        } else if character == right {
            depth = depth.checked_sub(1)?;
            if depth == 0 {
                return Some(open + offset);
            }
        }
    }
    None
}

fn strip_comments(source: &str) -> String {
    source
        .lines()
        .map(|line| line.split_once("//").map_or(line, |(code, _)| code))
        .collect::<Vec<_>>()
        .join("\n")
}

fn syntax_at(source: &str, offset: usize, message: impl Into<String>) -> TranslateError {
    let line = source[..offset.min(source.len())]
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count()
        + 1;
    TranslateError::one(Diagnostic::new(Phase::AirSyntax, message).at_line(line))
}
