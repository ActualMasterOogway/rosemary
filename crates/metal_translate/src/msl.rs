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
        function_parameters.push(Parameter {
            value,
            name: name.to_string(),
            ty: ty.clone(),
        });
        if let Some((kind, index, access)) = parse_resource_attribute(attribute, declaration, &ty) {
            resources.push(Resource {
                argument: value.0,
                kind,
                index,
                count: 1,
                access,
                ty,
                name: Some(name.to_string()),
            });
        } else if !attribute.starts_with("thread_position_in_grid") {
            return Err(syntax_at(
                &source,
                offset,
                "unsupported MSL parameter attribute",
            ));
        }
    }
    if let Some(error) = parse_blit_bounds_guard(&source, body) {
        return Err(error);
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

fn parse_resource_attribute(
    attribute: &str,
    declaration: &str,
    ty: &Type,
) -> Option<(ResourceKind, u32, Access)> {
    let (kind, index) = if let Some(index) = attribute.strip_prefix("buffer(") {
        (ResourceKind::Buffer, index.strip_suffix(')')?)
    } else if let Some(index) = attribute.strip_prefix("texture(") {
        (ResourceKind::Texture, index.strip_suffix(')')?)
    } else {
        return None;
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

/// Recognize the bounds guard used by the shipped blit kernels before lowering
/// their body. The guard is not accepted as a no-op: until vector comparison
/// and reduction have Metal IR and SPIR-V lowering, parsing continues only far
/// enough to identify the following operation accurately.
fn parse_blit_bounds_guard(source: &str, body: &str) -> Option<TranslateError> {
    let guard = body.trim_start();
    if !guard.starts_with("if") {
        return None;
    }
    let open = guard.find('(')?;
    let close = matching(guard, open, '(', ')')?;
    let condition = guard[open + 1..close].trim();
    if !condition.starts_with("all(") || !condition.contains('<') {
        return Some(syntax_at(
            source,
            source.find(guard).unwrap_or(0),
            "unsupported MSL conditional expression",
        ));
    }
    let body_open = guard[close + 1..]
        .find('{')
        .map(|offset| close + 1 + offset)?;
    let body_close = matching(guard, body_open, '{', '}')?;
    let guarded_body = &guard[body_open + 1..body_close];
    if let Some(load) = guarded_body.find("=") {
        let statement = guarded_body[..load].trim();
        if statement.split_whitespace().count() == 2 && guarded_body[load + 1..].contains('[') {
            return Some(syntax_at(
                source,
                source.find(guarded_body).unwrap_or(0) + load,
                "MSL buffer element loads are not implemented",
            ));
        }
    }
    Some(syntax_at(
        source,
        source.find(guard).unwrap_or(0),
        "MSL conditional body operations are not implemented",
    ))
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
    if ty.starts_with("texture2d<") || ty.starts_with("texture3d<") {
        return Ok(Type::Texture);
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
