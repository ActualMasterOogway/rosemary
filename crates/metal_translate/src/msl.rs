//! A deliberately small, semantics-preserving MSL source frontend.
//!
//! The first supported production shape is a compute kernel which writes an
//! integer literal to element zero of a `device uint*` buffer.  It is useful as
//! a vertical source-compiler slice and, importantly, rejects every construct
//! whose Metal semantics have not been implemented yet.

use crate::ir::{
    Access, AddressSpace, Block, BlockId, EntryPoint, Function, Instruction, InstructionKind,
    Module, Operand, Parameter, Resource, ResourceKind, Stage, Terminator, Type, ValueId,
};
use crate::{Diagnostic, Phase, TranslateError};

/// Parse the source-MSL subset currently implemented by the translator.
pub fn parse_msl(source: &str) -> Result<Module, TranslateError> {
    let source = strip_comments(source);
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
    for raw in parameters
        .split(',')
        .map(str::trim)
        .filter(|raw| !raw.is_empty())
    {
        let Some(buffer) = raw.find("[[buffer(") else {
            return Err(syntax_at(
                &source,
                source.find(raw).unwrap_or(0),
                "only `device uint*` buffer parameters are supported",
            ));
        };
        let index_end = raw[buffer + 9..]
            .find(")]]")
            .map(|offset| buffer + 9 + offset)
            .ok_or_else(|| {
                syntax_at(
                    &source,
                    source.find(raw).unwrap_or(0) + buffer,
                    "MSL buffer attribute is malformed",
                )
            })?;
        let index = raw[buffer + 9..index_end]
            .trim()
            .parse::<u32>()
            .map_err(|_| {
                syntax_at(
                    &source,
                    source.find(raw).unwrap_or(0) + buffer + 9,
                    "MSL buffer index is not an unsigned integer",
                )
            })?;
        let declaration = raw[..buffer].trim();
        let tokens: Vec<_> = declaration.split_whitespace().collect();
        if tokens.len() != 3 || tokens[0] != "device" || tokens[1] != "uint*" {
            return Err(syntax_at(
                &source,
                source.find(raw).unwrap_or(0),
                "only `device uint* name [[buffer(n)]]` is supported",
            ));
        }
        let value = ValueId(values.len() as u32);
        let ty = Type::Pointer {
            space: AddressSpace::Device,
            pointee: Box::new(Type::u32()),
        };
        values.push(ty.clone());
        function_parameters.push(Parameter {
            value,
            name: tokens[2].to_string(),
            ty: ty.clone(),
        });
        resources.push(Resource {
            argument: value.0,
            kind: ResourceKind::Buffer,
            index,
            count: 1,
            access: Access::Write,
            ty,
            name: Some(tokens[2].to_string()),
        });
    }
    if function_parameters.len() != 1 {
        return Err(syntax_at(
            &source,
            kernel,
            "the initial MSL frontend requires exactly one buffer parameter",
        ));
    }
    let parameter_value = function_parameters[0].value;
    let prefix = format!("{}[0] =", function_parameters[0].name);
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
