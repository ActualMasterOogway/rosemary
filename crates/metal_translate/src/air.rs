//! AIR container and LLVM representation import.

use std::collections::{BTreeMap, HashMap};

use llvm_bitcode::Bitcode;
use llvm_bitcode::bitcode::{BitcodeElement, Block as BitcodeBlock, Payload};

use crate::ir::{
    Access, AddressSpace, Block, BlockId, EntryPoint, Function, Instruction, InstructionKind,
    IntegerPredicate, Module, Operand, Parameter, Resource, ResourceKind, Stage, Terminator, Type,
    ValueId,
};
use crate::{Diagnostic, Phase, TranslateError};

/// An AIR module in one of the representations accepted by the translator.
#[derive(Clone, Copy, Debug)]
pub enum AirInput<'a> {
    Text(&'a str),
    Bitcode(&'a [u8]),
}

/// Parse AIR and verify the resulting Metal IR.
pub fn parse_air(input: AirInput<'_>) -> Result<Module, TranslateError> {
    let module = match input {
        AirInput::Text(text) => parse_text(text)?,
        AirInput::Bitcode(bytes) => parse_bitcode(bytes)?,
    };
    module.verify()?;
    Ok(module)
}

fn parse_bitcode(bytes: &[u8]) -> Result<Module, TranslateError> {
    let bitcode = Bitcode::new(bytes).map_err(|error| {
        TranslateError::one(Diagnostic::new(
            Phase::AirContainer,
            format!("invalid LLVM bitcode: {error}"),
        ))
    })?;
    parse_minimal_store_bitcode(&bitcode)
}

/// Import the first production-oriented bitcode shape without depending on
/// libLLVM. More function records are added alongside the corresponding Metal
/// operation; accepting an unfamiliar record here would silently miscompile it.
fn parse_minimal_store_bitcode(bitcode: &Bitcode) -> Result<Module, TranslateError> {
    let module_block = child_block(&bitcode.elements, 8)
        .ok_or_else(|| semantic("LLVM bitcode has no module block"))?;
    let function_blocks: Vec<_> = module_block
        .elements
        .iter()
        .filter_map(BitcodeElement::as_block)
        .filter(|block| block.id == 12)
        .collect();
    let function_records: Vec<_> = module_block
        .elements
        .iter()
        .filter_map(BitcodeElement::as_record)
        .filter(|record| record.id == 8)
        .collect();
    if function_blocks.len() != 1 || function_records.len() != 1 {
        return Err(semantic(
            "the initial bitcode importer requires exactly one defined AIR function",
        ));
    }
    let string_table = child_block(&bitcode.elements, 23)
        .and_then(|block| first_payload(block, 1))
        .and_then(payload_bytes)
        .ok_or_else(|| semantic("LLVM bitcode has no string table"))?;
    let function_fields = function_records[0].fields();
    let offset = usize::try_from(
        *function_fields
            .first()
            .ok_or_else(|| semantic("LLVM function record has no name offset"))?,
    )
    .map_err(|_| semantic("LLVM function name offset does not fit in memory"))?;
    let length = usize::try_from(
        *function_fields
            .get(1)
            .ok_or_else(|| semantic("LLVM function record has no name length"))?,
    )
    .map_err(|_| semantic("LLVM function name length does not fit in memory"))?;
    let name = std::str::from_utf8(
        string_table
            .get(offset..offset + length)
            .ok_or_else(|| semantic("LLVM function name is outside the string table"))?,
    )
    .map_err(|_| semantic("LLVM function name is not UTF-8"))?
    .to_string();

    let function_block = function_blocks[0];
    let records: Vec<_> = function_block
        .elements
        .iter()
        .filter_map(BitcodeElement::as_record)
        .collect();
    let declared_blocks = records
        .iter()
        .find(|record| record.id == 1)
        .and_then(|record| record.fields().first())
        .copied();
    let stores = records.iter().filter(|record| record.id == 44).count();
    let returns = records.iter().filter(|record| record.id == 10).count();
    if declared_blocks != Some(1) || stores != 1 || returns != 1 || records.len() != 3 {
        return Err(semantic(
            "unsupported LLVM function records in the initial bitcode importer",
        ));
    }
    let constants = child_block(&function_block.elements, 11)
        .ok_or_else(|| semantic("constant-store function has no constants block"))?;
    let encoded = constants
        .elements
        .iter()
        .filter_map(BitcodeElement::as_record)
        .find(|record| record.id == 4)
        .and_then(|record| record.fields().first())
        .copied()
        .ok_or_else(|| semantic("constant-store function has no integer constant"))?;
    let value = decode_sign_rotated(encoded) as u64;
    let symbol_table = child_block(&function_block.elements, 14)
        .ok_or_else(|| semantic("AIR function has no value symbol table"))?;
    let parameter_name = first_payload(symbol_table, 1)
        .and_then(payload_string)
        .ok_or_else(|| semantic("AIR buffer argument has no name"))?;

    let payload_text = collect_payload_text(&bitcode.elements);
    for marker in [
        "air.kernel",
        "air.buffer",
        "air.location_index",
        "air.arg_type_name",
    ] {
        if !payload_text.contains(marker) {
            return Err(semantic(format!(
                "AIR bitcode metadata is missing {marker}"
            )));
        }
    }
    let access = if payload_text.contains("air.read_write") {
        Access::ReadWrite
    } else if payload_text.contains("air.write") {
        Access::Write
    } else if payload_text.contains("air.read") {
        Access::Read
    } else {
        return Err(semantic("AIR buffer metadata has no access mode"));
    };
    let element = if payload_text.contains("uint") {
        Type::u32()
    } else {
        Type::i32()
    };
    let pointer = Type::Pointer {
        space: AddressSpace::Device,
        pointee: Box::new(element.clone()),
    };
    let parameter_value = ValueId(0);
    let parameter = Parameter {
        value: parameter_value,
        name: parameter_name.clone(),
        ty: pointer.clone(),
    };
    let function = Function {
        name,
        parameters: vec![parameter],
        values: vec![pointer.clone()],
        blocks: vec![Block {
            id: BlockId(0),
            name: "entry".to_string(),
            instructions: vec![Instruction {
                result: None,
                kind: InstructionKind::Store {
                    pointer: Operand::Value(parameter_value),
                    value: Operand::Integer { ty: element, value },
                },
            }],
            terminator: Terminator::Return(None),
        }],
    };
    Ok(Module {
        functions: vec![function],
        entries: vec![EntryPoint {
            function: 0,
            stage: Stage::Kernel,
            resources: vec![Resource {
                argument: 0,
                kind: ResourceKind::Buffer,
                index: 0,
                count: 1,
                access,
                ty: pointer,
                name: Some(parameter_name),
            }],
        }],
    })
}

fn child_block(elements: &[BitcodeElement], id: u32) -> Option<&BitcodeBlock> {
    elements
        .iter()
        .filter_map(BitcodeElement::as_block)
        .find(|block| block.id == id)
}

fn first_payload(block: &BitcodeBlock, record_id: u64) -> Option<Payload> {
    let mut record = block
        .elements
        .iter()
        .filter_map(BitcodeElement::as_record)
        .find(|record| record.id == record_id)?
        .clone();
    record.take_payload()
}

fn payload_bytes(payload: Payload) -> Option<Vec<u8>> {
    match payload {
        Payload::Blob(bytes) => Some(bytes),
        Payload::Array(values) => values
            .into_iter()
            .map(|value| u8::try_from(value).ok())
            .collect(),
        Payload::Char6String(text) => Some(text.into_bytes()),
    }
}

fn payload_string(payload: Payload) -> Option<String> {
    match payload {
        Payload::Char6String(text) => Some(text),
        Payload::Array(values) => String::from_utf8(
            values
                .into_iter()
                .map(|value| u8::try_from(value).ok())
                .collect::<Option<_>>()?,
        )
        .ok(),
        Payload::Blob(bytes) => String::from_utf8(bytes).ok(),
    }
}

fn collect_payload_text(elements: &[BitcodeElement]) -> String {
    let mut output = String::new();
    for element in elements {
        match element {
            BitcodeElement::Block(block) => output.push_str(&collect_payload_text(&block.elements)),
            BitcodeElement::Record(record) => {
                let mut record = record.clone();
                if let Some(payload) = record.take_payload()
                    && let Some(bytes) = payload_bytes(payload)
                {
                    output.push_str(&String::from_utf8_lossy(&bytes));
                }
            }
        }
    }
    output
}

fn decode_sign_rotated(value: u64) -> i64 {
    let magnitude = (value >> 1) as i64;
    if value & 1 == 0 {
        magnitude
    } else {
        -magnitude
    }
}

fn parse_text(text: &str) -> Result<Module, TranslateError> {
    let metadata = Metadata::parse(text)?;
    let definition = text
        .lines()
        .enumerate()
        .find(|(_, line)| line.trim_start().starts_with("define "))
        .ok_or_else(|| syntax("AIR module has no function definition", None))?;
    let (definition_line, header) = definition;
    let (name, raw_parameters) = parse_header(header.trim())
        .map_err(|message| syntax(message, Some(definition_line + 1)))?;
    if name != metadata.function_name {
        return Err(semantic(format!(
            "AIR entry metadata names {}, but the definition is {name}",
            metadata.function_name
        )));
    }

    let raw_parameters = split_top_level(raw_parameters, ',');
    let mut parameter_specs = Vec::new();
    for (argument, raw) in raw_parameters.iter().enumerate() {
        let resource = metadata
            .resources
            .iter()
            .find(|resource| resource.argument == argument as u32);
        parameter_specs.push(
            parse_parameter(raw.trim(), resource)
                .map_err(|message| syntax(message, Some(definition_line + 1)))?,
        );
    }

    let body_lines = function_body(text, definition_line)?;
    let mut block_names = Vec::new();
    if body_lines
        .first()
        .is_some_and(|(_, line)| !line.trim().ends_with(':'))
    {
        block_names.push("entry".to_string());
    }
    for (_, line) in &body_lines {
        let line = line.trim();
        if let Some(label) = line.strip_suffix(':') {
            block_names.push(label.to_string());
        }
    }
    if block_names.is_empty() {
        block_names.push("entry".to_string());
    }
    let block_ids: HashMap<_, _> = block_names
        .iter()
        .enumerate()
        .map(|(index, name)| (name.clone(), BlockId(index as u32)))
        .collect();

    let mut value_ids = HashMap::new();
    let mut values = Vec::new();
    let mut parameters = Vec::new();
    for (name, ty) in parameter_specs {
        let value = ValueId(values.len() as u32);
        value_ids.insert(name.clone(), value);
        values.push(ty.clone());
        parameters.push(Parameter { value, name, ty });
    }
    for (_, line) in &body_lines {
        if let Some((result, _)) = line.trim().split_once(" = ") {
            let result = result.trim_start_matches('%');
            if !value_ids.contains_key(result) {
                let value = ValueId(values.len() as u32);
                value_ids.insert(result.to_string(), value);
                values.push(Type::Void);
            }
        }
    }

    let mut blocks = Vec::new();
    let mut current_name = block_names[0].clone();
    let mut current_instructions = Vec::new();
    let mut current_terminator = None;
    for (line_number, raw_line) in body_lines {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with(';') {
            continue;
        }
        if let Some(label) = line.strip_suffix(':') {
            if !current_instructions.is_empty() || current_terminator.is_some() {
                finish_block(
                    &mut blocks,
                    &block_ids,
                    &current_name,
                    std::mem::take(&mut current_instructions),
                    current_terminator.take(),
                )?;
            }
            current_name = label.to_string();
            continue;
        }
        if is_terminator(line) {
            current_terminator = Some(
                parse_terminator(line, &value_ids, &block_ids)
                    .map_err(|message| syntax(message, Some(line_number)))?,
            );
        } else {
            let instruction = parse_instruction(line, &value_ids, &block_ids, &mut values)
                .map_err(|message| syntax(message, Some(line_number)))?;
            current_instructions.push(instruction);
        }
    }
    finish_block(
        &mut blocks,
        &block_ids,
        &current_name,
        current_instructions,
        current_terminator,
    )?;

    let function = Function {
        name,
        parameters,
        values,
        blocks,
    };
    let resources = metadata
        .resources
        .into_iter()
        .map(|resource| resource.into_ir(&function.parameters))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Module {
        functions: vec![function],
        entries: vec![EntryPoint {
            function: 0,
            stage: Stage::Kernel,
            resources,
        }],
    })
}

fn parse_header(header: &str) -> Result<(String, &str), String> {
    let at = header.find('@').ok_or("function header has no name")?;
    let open = header[at..]
        .find('(')
        .map(|offset| at + offset)
        .ok_or("function header has no parameter list")?;
    let close = header
        .rfind(')')
        .ok_or("function header has no closing parenthesis")?;
    if close < open {
        return Err("malformed function parameter list".into());
    }
    Ok((header[at + 1..open].to_string(), &header[open + 1..close]))
}

fn parse_parameter(
    raw: &str,
    resource: Option<&MetadataResource>,
) -> Result<(String, Type), String> {
    let percent = raw
        .rfind('%')
        .ok_or_else(|| format!("parameter has no name: {raw}"))?;
    let name = raw[percent + 1..].trim().to_string();
    let raw_ty = raw[..percent].trim();
    let ty = if raw_ty.starts_with("ptr") {
        let space = parse_address_space(raw_ty)?;
        let pointee = resource.map_or_else(Type::u32, MetadataResource::element_type);
        Type::Pointer {
            space,
            pointee: Box::new(pointee),
        }
    } else {
        parse_scalar_type(raw_ty, false)?
    };
    Ok((name, ty))
}

fn parse_address_space(raw: &str) -> Result<AddressSpace, String> {
    let number = raw
        .split("addrspace(")
        .nth(1)
        .and_then(|tail| tail.split(')').next())
        .map(|number| number.parse::<u32>())
        .transpose()
        .map_err(|_| format!("invalid address space in {raw}"))?
        .unwrap_or(0);
    match number {
        0 => Ok(AddressSpace::Thread),
        1 => Ok(AddressSpace::Device),
        2 => Ok(AddressSpace::Constant),
        3 => Ok(AddressSpace::Threadgroup),
        4 => Ok(AddressSpace::ThreadgroupImageblock),
        _ => Err(format!("unsupported AIR address space {number}")),
    }
}

fn parse_scalar_type(raw: &str, signed: bool) -> Result<Type, String> {
    let raw = raw.trim();
    if raw == "i1" {
        return Ok(Type::Bool);
    }
    if let Some(bits) = raw
        .strip_prefix('i')
        .and_then(|bits| bits.parse::<u8>().ok())
    {
        return Ok(Type::Int { bits, signed });
    }
    Err(format!("unsupported AIR type {raw}"))
}

fn function_body(text: &str, definition_line: usize) -> Result<Vec<(usize, &str)>, TranslateError> {
    let mut body = Vec::new();
    let mut opened = false;
    for (index, line) in text.lines().enumerate().skip(definition_line) {
        if !opened {
            opened = line.contains('{');
            continue;
        }
        if line.trim() == "}" {
            return Ok(body);
        }
        body.push((index + 1, line));
    }
    Err(syntax(
        "unterminated function definition",
        Some(definition_line + 1),
    ))
}

fn finish_block(
    blocks: &mut Vec<Block>,
    ids: &HashMap<String, BlockId>,
    name: &str,
    instructions: Vec<Instruction>,
    terminator: Option<Terminator>,
) -> Result<(), TranslateError> {
    let id = ids
        .get(name)
        .copied()
        .ok_or_else(|| semantic(format!("unknown block {name}")))?;
    let terminator =
        terminator.ok_or_else(|| semantic(format!("block {name} has no terminator")))?;
    blocks.push(Block {
        id,
        name: name.to_string(),
        instructions,
        terminator,
    });
    Ok(())
}

fn is_terminator(line: &str) -> bool {
    line.starts_with("br ") || line.starts_with("ret ") || line == "unreachable"
}

fn parse_terminator(
    line: &str,
    values: &HashMap<String, ValueId>,
    blocks: &HashMap<String, BlockId>,
) -> Result<Terminator, String> {
    if line == "ret void" {
        return Ok(Terminator::Return(None));
    }
    if line == "unreachable" {
        return Ok(Terminator::Unreachable);
    }
    if let Some(target) = line.strip_prefix("br label %") {
        return blocks
            .get(target)
            .copied()
            .map(Terminator::Branch)
            .ok_or_else(|| format!("unknown branch target {target}"));
    }
    if let Some(rest) = line.strip_prefix("br i1 ") {
        let parts: Vec<_> = rest.split(',').map(str::trim).collect();
        if parts.len() != 3 {
            return Err(format!("malformed conditional branch: {line}"));
        }
        let condition = parse_operand(parts[0], &Type::Bool, values)?;
        let true_name = parts[1]
            .strip_prefix("label %")
            .ok_or("malformed true target")?;
        let false_name = parts[2]
            .strip_prefix("label %")
            .ok_or("malformed false target")?;
        return Ok(Terminator::CondBranch {
            condition,
            true_block: *blocks.get(true_name).ok_or("unknown true target")?,
            false_block: *blocks.get(false_name).ok_or("unknown false target")?,
        });
    }
    Err(format!("unsupported AIR terminator: {line}"))
}

fn parse_instruction(
    line: &str,
    values: &HashMap<String, ValueId>,
    blocks: &HashMap<String, BlockId>,
    types: &mut [Type],
) -> Result<Instruction, String> {
    if let Some(rest) = line.strip_prefix("store ") {
        let parts = split_top_level(rest, ',');
        if parts.len() < 2 {
            return Err(format!("malformed store: {line}"));
        }
        let (raw_ty, value) = typed_operand(parts[0].trim(), values)?;
        let pointer_name = value_name(parts[1]).ok_or("store pointer has no SSA value")?;
        let pointer_value = *values.get(pointer_name).ok_or("unknown store pointer")?;
        let ty = match &types[pointer_value.0 as usize] {
            Type::Pointer { pointee, .. } => pointee.as_ref().clone(),
            _ => raw_ty,
        };
        return Ok(Instruction {
            result: None,
            kind: InstructionKind::Store {
                pointer: Operand::Value(pointer_value),
                value: parse_operand(value, &ty, values)?,
            },
        });
    }

    let (result_name, expression) = line
        .split_once(" = ")
        .ok_or_else(|| format!("unsupported AIR instruction: {line}"))?;
    let result = *values
        .get(result_name.trim_start_matches('%'))
        .ok_or("unknown instruction result")?;
    let kind;
    let result_type;
    if let Some(rest) = expression.strip_prefix("load ") {
        let parts = split_top_level(rest, ',');
        let ty = parse_scalar_type(parts.first().ok_or("load has no type")?.trim(), false)?;
        let pointer_name = parts
            .get(1)
            .and_then(|part| value_name(part))
            .ok_or("load has no pointer")?;
        kind = InstructionKind::Load {
            pointer: Operand::Value(*values.get(pointer_name).ok_or("unknown load pointer")?),
        };
        result_type = ty;
    } else if expression.starts_with("add ") || expression.starts_with("xor ") {
        let (operator, rest) = expression.split_once(' ').unwrap();
        let parts = split_top_level(rest, ',');
        let (ty, lhs) = typed_operand(
            parts.first().ok_or("binary operation has no lhs")?.trim(),
            values,
        )?;
        let rhs = parts.get(1).ok_or("binary operation has no rhs")?.trim();
        let lhs = parse_operand(lhs, &ty, values)?;
        let rhs = parse_operand(rhs, &ty, values)?;
        kind = if operator == "add" {
            InstructionKind::IntegerAdd { lhs, rhs }
        } else {
            InstructionKind::IntegerXor { lhs, rhs }
        };
        result_type = ty;
    } else if let Some(rest) = expression.strip_prefix("icmp ult ") {
        let parts = split_top_level(rest, ',');
        let (ty, lhs) = typed_operand(parts.first().ok_or("compare has no lhs")?.trim(), values)?;
        kind = InstructionKind::IntegerCompare {
            predicate: IntegerPredicate::UnsignedLessThan,
            lhs: parse_operand(lhs, &ty, values)?,
            rhs: parse_operand(
                parts.get(1).ok_or("compare has no rhs")?.trim(),
                &ty,
                values,
            )?,
        };
        result_type = Type::Bool;
    } else if let Some(rest) = expression.strip_prefix("phi ") {
        let type_end = rest.find(' ').ok_or("phi has no type")?;
        let ty = parse_scalar_type(&rest[..type_end], false)?;
        let mut incomings = Vec::new();
        for incoming in split_top_level(&rest[type_end + 1..], ',') {
            let incoming = incoming
                .trim()
                .trim_start_matches('[')
                .trim_end_matches(']');
            let (value, block) = incoming.split_once(',').ok_or("malformed phi incoming")?;
            let block = block
                .trim()
                .strip_prefix('%')
                .ok_or("malformed phi block")?;
            incomings.push((
                *blocks.get(block).ok_or("unknown phi block")?,
                parse_operand(value.trim(), &ty, values)?,
            ));
        }
        kind = InstructionKind::Phi { incomings };
        result_type = ty;
    } else {
        return Err(format!("unsupported AIR instruction: {expression}"));
    }
    types[result.0 as usize] = result_type;
    Ok(Instruction {
        result: Some(result),
        kind,
    })
}

fn typed_operand<'a>(
    raw: &'a str,
    _values: &HashMap<String, ValueId>,
) -> Result<(Type, &'a str), String> {
    let (ty, value) = raw
        .split_once(' ')
        .ok_or_else(|| format!("missing operand type: {raw}"))?;
    Ok((parse_scalar_type(ty, false)?, value.trim()))
}

fn parse_operand(
    raw: &str,
    ty: &Type,
    values: &HashMap<String, ValueId>,
) -> Result<Operand, String> {
    if let Some(name) = raw.strip_prefix('%') {
        return values
            .get(name)
            .copied()
            .map(Operand::Value)
            .ok_or_else(|| format!("unknown SSA value %{name}"));
    }
    let signed = raw
        .parse::<i64>()
        .map_err(|_| format!("unsupported operand {raw}"))?;
    Ok(Operand::Integer {
        ty: ty.clone(),
        value: signed as u64,
    })
}

fn value_name(raw: &str) -> Option<&str> {
    raw.split_whitespace()
        .find_map(|word| word.strip_prefix('%'))
        .map(|name| name.trim_end_matches(','))
}

fn split_top_level(input: &str, delimiter: char) -> Vec<&str> {
    let mut depth = 0i32;
    let mut start = 0;
    let mut parts = Vec::new();
    for (index, byte) in input.bytes().enumerate() {
        match byte as char {
            '(' | '[' | '{' | '<' => depth += 1,
            ')' | ']' | '}' | '>' => depth -= 1,
            value if value == delimiter && depth == 0 => {
                parts.push(&input[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    parts.push(&input[start..]);
    parts
}

struct Metadata {
    function_name: String,
    resources: Vec<MetadataResource>,
}

#[derive(Clone)]
struct MetadataResource {
    argument: u32,
    index: u32,
    count: u32,
    access: Access,
    type_name: String,
    name: Option<String>,
}

impl MetadataResource {
    fn element_type(&self) -> Type {
        if self.type_name.starts_with("int") {
            Type::i32()
        } else {
            Type::u32()
        }
    }

    fn into_ir(self, parameters: &[Parameter]) -> Result<Resource, TranslateError> {
        let parameter = parameters
            .get(self.argument as usize)
            .ok_or_else(|| semantic("resource metadata refers to a missing parameter"))?;
        Ok(Resource {
            argument: self.argument,
            kind: ResourceKind::Buffer,
            index: self.index,
            count: self.count,
            access: self.access,
            ty: parameter.ty.clone(),
            name: self.name,
        })
    }
}

impl Metadata {
    fn parse(text: &str) -> Result<Self, TranslateError> {
        if !text
            .lines()
            .any(|line| line.trim_start().starts_with("!air.kernel"))
        {
            return Err(semantic(
                "the first translation slice only accepts AIR kernel entries",
            ));
        }
        let mut nodes = BTreeMap::new();
        for line in text.lines().map(str::trim) {
            if !line.starts_with('!') || line.starts_with("!air.") {
                continue;
            }
            if let Some((number, contents)) = line.split_once(" = ") {
                nodes.insert(number.to_string(), contents.to_string());
            }
        }
        let root = nodes
            .get("!0")
            .ok_or_else(|| semantic("missing AIR entry metadata node !0"))?;
        let at = root
            .find('@')
            .ok_or_else(|| semantic("AIR entry metadata has no function"))?;
        let end = root[at + 1..]
            .find(',')
            .map(|offset| at + 1 + offset)
            .ok_or_else(|| semantic("malformed AIR entry metadata"))?;
        let function_name = root[at + 1..end].trim().to_string();
        let refs: Vec<_> = root
            .split(|character: char| !character.is_ascii_digit() && character != '!')
            .filter(|token| token.starts_with('!') && token.len() > 1)
            .collect();
        let argument_node = refs
            .last()
            .ok_or_else(|| semantic("AIR entry metadata has no arguments"))?;
        let arguments = nodes
            .get(*argument_node)
            .ok_or_else(|| semantic("missing AIR argument-list metadata"))?;
        let mut resources = Vec::new();
        for reference in arguments
            .split(|character: char| !character.is_ascii_digit() && character != '!')
            .filter(|token| token.starts_with('!') && token.len() > 1)
        {
            let node = nodes
                .get(reference)
                .ok_or_else(|| semantic(format!("missing resource metadata node {reference}")))?;
            resources.push(parse_resource(node)?);
        }
        Ok(Self {
            function_name,
            resources,
        })
    }
}

fn parse_resource(node: &str) -> Result<MetadataResource, TranslateError> {
    if !node.contains("!\"air.buffer\"") {
        return Err(semantic(
            "the first translation slice only supports buffer arguments",
        ));
    }
    let argument =
        first_i32(node).ok_or_else(|| semantic("buffer metadata has no argument index"))?;
    let index = i32_after(node, "!\"air.location_index\"")
        .ok_or_else(|| semantic("buffer metadata has no location index"))?;
    let count = i32_values_after(node, "!\"air.location_index\"")
        .get(1)
        .copied()
        .unwrap_or(1);
    let access = if node.contains("!\"air.read_write\"") {
        Access::ReadWrite
    } else if node.contains("!\"air.write\"") {
        Access::Write
    } else if node.contains("!\"air.read\"") {
        Access::Read
    } else {
        return Err(semantic("buffer metadata has no access mode"));
    };
    let type_name = string_after(node, "!\"air.arg_type_name\"")
        .ok_or_else(|| semantic("buffer metadata has no type name"))?;
    let name = string_after(node, "!\"air.arg_name\"");
    Ok(MetadataResource {
        argument,
        index,
        count,
        access,
        type_name,
        name,
    })
}

fn first_i32(input: &str) -> Option<u32> {
    i32_values_after(input, "").first().copied()
}

fn i32_after(input: &str, marker: &str) -> Option<u32> {
    i32_values_after(input, marker).first().copied()
}

fn i32_values_after(input: &str, marker: &str) -> Vec<u32> {
    let tail = if marker.is_empty() {
        input
    } else if let Some((_, tail)) = input.split_once(marker) {
        tail
    } else {
        return Vec::new();
    };
    let mut output = Vec::new();
    let mut rest = tail;
    while let Some((_, after)) = rest.split_once("i32 ") {
        let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
        if let Ok(value) = digits.parse() {
            output.push(value);
        }
        rest = after;
    }
    output
}

fn string_after(input: &str, marker: &str) -> Option<String> {
    let (_, tail) = input.split_once(marker)?;
    let (_, tail) = tail.split_once("!\"")?;
    Some(tail.split_once('"')?.0.to_string())
}

fn syntax(message: impl Into<String>, line: Option<usize>) -> TranslateError {
    let diagnostic = Diagnostic::new(Phase::AirSyntax, message);
    TranslateError::one(match line {
        Some(line) => diagnostic.at_line(line),
        None => diagnostic,
    })
}

fn semantic(message: impl Into<String>) -> TranslateError {
    TranslateError::one(Diagnostic::new(Phase::AirSemantics, message))
}
