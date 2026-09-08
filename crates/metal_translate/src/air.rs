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
    let all_function_records: Vec<_> = module_block
        .elements
        .iter()
        .filter_map(BitcodeElement::as_record)
        .filter(|record| record.id == 8)
        .collect();
    // LLVM emits STRINGTABLE_BLOCK inside the module block.  Older producer
    // versions put it at the top level, so search the complete bitstream
    // rather than tying name resolution to one container layout.
    let string_table = descendant_block(&bitcode.elements, 23)
        .and_then(|block| first_payload(block, 1))
        .and_then(payload_bytes);
    let legacy_names = legacy_function_names(module_block);
    let function_records: Vec<_> = all_function_records
        .iter()
        .enumerate()
        // LLVM's older AIR writer puts `isproto` in field 2, while the
        // string-table layout used by the initial fixture puts it in field 4.
        .filter(|(_, record)| {
            if string_table.is_some() {
                record.fields().get(4) == Some(&0)
            } else {
                record.fields().get(2) == Some(&0)
            }
        })
        .collect();
    if function_blocks.len() != function_records.len() || function_records.is_empty() {
        return Err(semantic(
            "LLVM defined-function records do not match their function bodies",
        ));
    }
    // The libraries in Roblox's pack are written by the older AIR producer.
    // Decode the tables before looking at a body: unlike the tiny LLVM 19
    // fixture below, its function type and entry-point contract are not
    // recoverable from a few strings.
    if string_table.is_none()
        && child_block(&module_block.elements, 17).is_some()
        && metadata_entry_stage(module_block).is_some()
    {
        return parse_packed_air(
            module_block,
            &function_records,
            &function_blocks,
            &legacy_names,
        );
    }
    let entry_index = function_records
        .iter()
        .position(|(ordinal, record)| {
            resolve_function_name(record, *ordinal, string_table.as_deref(), &legacy_names)
                .as_deref()
                == Some("main0")
        })
        .or_else(|| (function_records.len() == 1).then_some(0))
        .ok_or_else(|| semantic("AIR bitcode has no main0 entry function"))?;
    let name = resolve_function_name(
        function_records[entry_index].1,
        function_records[entry_index].0,
        string_table.as_deref(),
        &legacy_names,
    )
    .ok_or_else(|| semantic("LLVM entry function has no resolvable name"))?;

    let function_block = function_blocks[entry_index];
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

/// Decode the table-oriented AIR layout used by Metal's packed libraries.
///
/// This deliberately performs the type and metadata passes independently of
/// instruction lowering.  A body is only accepted once every record has a
/// Metal IR lowering; retaining the decoded contract here prevents the old
/// string-search path from accidentally treating a fragment entry as a
/// compute kernel.
fn parse_packed_air(
    module_block: &BitcodeBlock,
    function_records: &[(usize, &&llvm_bitcode::bitcode::Record)],
    function_blocks: &[&BitcodeBlock],
    legacy_names: &HashMap<usize, String>,
) -> Result<Module, TranslateError> {
    let types = decode_type_table(
        child_block(&module_block.elements, 17)
            .ok_or_else(|| semantic("packed AIR has no type table"))?,
    )?;
    let stage = metadata_entry_stage(module_block)
        .ok_or_else(|| semantic("packed AIR has no supported entry metadata"))?;
    let entry = function_records
        .iter()
        .position(|(ordinal, _)| {
            legacy_names
                .get(ordinal)
                .is_some_and(|name| name == "main0")
        })
        .or_else(|| (function_records.len() == 1).then_some(0))
        .ok_or_else(|| semantic("packed AIR has no main0 entry function"))?;
    let (ordinal, record) = function_records[entry];
    let name = legacy_names
        .get(&ordinal)
        .cloned()
        .ok_or_else(|| semantic("packed AIR entry has no value-symbol name"))?;
    let signature_index = usize::try_from(
        *record
            .fields()
            .first()
            .ok_or_else(|| semantic("packed AIR function has no type"))?,
    )
    .map_err(|_| semantic("packed AIR function type does not fit in memory"))?;
    let signature = types
        .get(signature_index)
        .ok_or_else(|| semantic("packed AIR function refers to a missing type"))?;
    // Old LLVM module records name the pointer-to-function type. Newer
    // producers name the function type directly.
    let signature = match signature {
        DecodedType::Pointer { pointee, .. } => types
            .get(*pointee)
            .ok_or_else(|| semantic("packed AIR function pointer has no pointee type"))?,
        signature => signature,
    };
    let DecodedType::Function { parameters, .. } = signature else {
        return Err(semantic(format!(
            "packed AIR function type {signature_index} is {}, not a function",
            signature.description()
        )));
    };
    let parameter_types = parameters
        .iter()
        .map(|index| types[*index].description())
        .collect::<Vec<_>>()
        .join(", ");

    // Keep this boundary strict.  The diagnostic is intentionally expressed
    // in LLVM record terms so the next lowering can be added without guessing
    // whether a payload was a type, metadata, or instruction operand.
    let body = function_blocks[entry];
    let first_instruction = body
        .elements
        .iter()
        .filter_map(BitcodeElement::as_record)
        .find(|record| record.id != 1)
        .ok_or_else(|| semantic("packed AIR function has no body records"))?;
    Err(semantic(format!(
        "packed AIR {stage:?} entry {name} has decoded parameter types ({parameter_types}), but LLVM function record {} is not lowered into Metal IR yet",
        first_instruction.id
    )))
}

#[derive(Clone, Debug)]
enum DecodedType {
    Void,
    Float(u8),
    Integer(u8),
    Vector {
        element: usize,
        lanes: u32,
    },
    Array {
        element: usize,
        count: u32,
    },
    Struct {
        fields: Vec<usize>,
        packed: bool,
    },
    Pointer {
        pointee: usize,
        address_space: u64,
    },
    Function {
        result: usize,
        parameters: Vec<usize>,
    },
    Metadata,
}

impl DecodedType {
    fn description(&self) -> String {
        match self {
            Self::Void => "void".to_string(),
            Self::Float(bits) => format!("f{bits}"),
            Self::Integer(bits) => format!("i{bits}"),
            Self::Vector { element, lanes } => format!("vector<{element}; {lanes}>"),
            Self::Array { element, count } => format!("array<{element}; {count}>"),
            Self::Struct { fields, packed } => {
                format!(
                    "{}struct[{}]",
                    if *packed { "packed " } else { "" },
                    fields.len()
                )
            }
            Self::Pointer {
                pointee,
                address_space,
            } => format!("ptr({address_space})<{pointee}>"),
            Self::Function { result, parameters } => {
                format!("fn({})->{result}", parameters.len())
            }
            Self::Metadata => "metadata".to_string(),
        }
    }
}

/// Decode only the LLVM type records represented by the first packed AIR
/// entry.  Referencing a type before it has been defined is rejected: AIR's
/// table is ordered, and accepting a forward reference here would change the
/// meaning of recursive structures.
fn decode_type_table(block: &BitcodeBlock) -> Result<Vec<DecodedType>, TranslateError> {
    let mut types = Vec::new();
    let mut pending_named_struct = None;
    for (record_ordinal, record) in block
        .elements
        .iter()
        .filter_map(BitcodeElement::as_record)
        .enumerate()
    {
        let fields = record_values(record);
        let minimum_fields = match record.id {
            7 | 8 => 1,
            11 | 12 | 21 => 2,
            _ => 0,
        };
        if fields.len() < minimum_fields {
            return Err(semantic(format!(
                "packed AIR type record {record_ordinal} (code {}) has {} field(s), expected at least {minimum_fields}",
                record.id,
                fields.len()
            )));
        }
        let type_record = match record.id {
            1 => continue, // NUMENTRY
            2 => DecodedType::Void,
            3 => DecodedType::Float(32),
            7 => DecodedType::Integer(
                u8::try_from(
                    *fields
                        .first()
                        .ok_or_else(|| semantic("integer type has no width"))?,
                )
                .map_err(|_| semantic("integer type width is too large"))?,
            ),
            10 => DecodedType::Float(16),
            11 => DecodedType::Array {
                count: u32::try_from(
                    *fields
                        .first()
                        .ok_or_else(|| semantic("array type has no count"))?,
                )
                .map_err(|_| semantic("array count is too large"))?,
                element: type_reference(fields.get(1), types.len())?,
            },
            12 => DecodedType::Vector {
                lanes: u32::try_from(
                    *fields
                        .first()
                        .ok_or_else(|| semantic("vector type has no lane count"))?,
                )
                .map_err(|_| semantic("vector lane count is too large"))?,
                element: type_reference(fields.get(1), types.len())?,
            },
            18 => DecodedType::Struct {
                packed: fields.first() == Some(&1),
                fields: fields[1..]
                    .iter()
                    .map(|field| type_reference(Some(field), types.len()))
                    .collect::<Result<_, _>>()?,
            },
            19 => {
                let _ = record
                    .clone()
                    .take_payload()
                    .and_then(payload_string)
                    .or_else(|| {
                        record
                            .fields()
                            .iter()
                            .map(|byte| u8::try_from(*byte).ok())
                            .collect::<Option<Vec<_>>>()
                            .and_then(|bytes| String::from_utf8(bytes).ok())
                    })
                    .ok_or_else(|| semantic("named struct has no UTF-8 name"))?;
                // STRUCT_NAME allocates the identified type; STRUCT_NAMED
                // fills that slot rather than allocating another entry.
                pending_named_struct = Some(types.len());
                types.push(DecodedType::Void);
                continue;
            }
            20 => {
                let index = pending_named_struct
                    .take()
                    .ok_or_else(|| semantic("named struct has no preceding name"))?;
                let packed = fields.first() == Some(&1);
                let fields = fields[1..]
                    .iter()
                    .map(|field| type_reference(Some(field), types.len()))
                    .collect::<Result<_, _>>()?;
                types[index] = DecodedType::Struct { packed, fields };
                continue;
            }
            8 => DecodedType::Pointer {
                pointee: type_reference(fields.first(), types.len())?,
                address_space: *fields.get(1).unwrap_or(&0),
            },
            21 => DecodedType::Function {
                result: type_reference(fields.get(1), types.len())?,
                parameters: fields[2..]
                    .iter()
                    .map(|field| type_reference(Some(field), types.len()))
                    .collect::<Result<_, _>>()?,
            },
            16 => DecodedType::Metadata,
            _ => {
                return Err(semantic(format!(
                    "unsupported packed AIR type record {}",
                    record.id
                )));
            }
        };
        types.push(type_record);
    }
    Ok(types)
}

/// Abbreviated LLVM records keep array operands in `Payload::Array` rather
/// than `Record::fields()`. They are still consecutive operands of the same
/// logical record (not an attached blob), notably for old AIR function types.
fn record_values(record: &llvm_bitcode::bitcode::Record) -> Vec<u64> {
    let mut values = record.fields().to_vec();
    if let Some(Payload::Array(array)) = record.clone().take_payload() {
        values.extend(array);
    }
    values
}

fn type_reference(field: Option<&u64>, defined: usize) -> Result<usize, TranslateError> {
    let index =
        usize::try_from(*field.ok_or_else(|| semantic("type record has no type reference"))?)
            .map_err(|_| semantic("type reference does not fit in memory"))?;
    if index >= defined {
        return Err(semantic(
            "packed AIR type has a forward or missing reference",
        ));
    }
    Ok(index)
}

/// Metadata names are records, not incidental string payloads.  Pair each
/// NAME with its following NAMED_NODE and select the entry stage from that
/// relationship.
fn metadata_entry_stage(module_block: &BitcodeBlock) -> Option<Stage> {
    for block in module_block
        .elements
        .iter()
        .filter_map(BitcodeElement::as_block)
        .filter(|block| block.id == 15)
    {
        let mut name = None;
        for record in block.elements.iter().filter_map(BitcodeElement::as_record) {
            match record.id {
                4 => {
                    name = record
                        .clone()
                        .take_payload()
                        .and_then(payload_string)
                        .or_else(|| {
                            record
                                .fields()
                                .iter()
                                .map(|byte| u8::try_from(*byte).ok())
                                .collect::<Option<Vec<_>>>()
                                .and_then(|bytes| String::from_utf8(bytes).ok())
                        });
                }
                10 if !record.fields().is_empty() => match name.as_deref() {
                    Some("air.vertex") => return Some(Stage::Vertex),
                    Some("air.fragment") => return Some(Stage::Fragment),
                    Some("air.kernel") => return Some(Stage::Kernel),
                    _ => name = None,
                },
                _ => {}
            }
        }
    }
    None
}

/// Resolve both LLVM function-name encodings used by AIR. Modern bitcode puts
/// name slices in STRINGTABLE_BLOCK. Apple's older AIR writer uses a
/// VALUE_SYMTAB_BLOCK entry whose payload is the name and whose first field is
/// the ordinal of its MODULE_CODE_FUNCTION record.
fn resolve_function_name(
    record: &llvm_bitcode::bitcode::Record,
    ordinal: usize,
    string_table: Option<&[u8]>,
    legacy_names: &HashMap<usize, String>,
) -> Option<String> {
    if let Some(string_table) = string_table {
        let offset = usize::try_from(*record.fields().first()?).ok()?;
        let length = usize::try_from(*record.fields().get(1)?).ok()?;
        return std::str::from_utf8(string_table.get(offset..offset.checked_add(length)?)?)
            .ok()
            .map(str::to_owned);
    }
    legacy_names.get(&ordinal).cloned()
}

fn legacy_function_names(module_block: &BitcodeBlock) -> HashMap<usize, String> {
    let Some(symbol_table) = child_block(&module_block.elements, 14) else {
        return HashMap::new();
    };
    symbol_table
        .elements
        .iter()
        .filter_map(BitcodeElement::as_record)
        .filter(|record| record.id == 1)
        .filter_map(|record| {
            let value = usize::try_from(*record.fields().first()?).ok()?;
            let mut record = record.clone();
            Some((value, payload_string(record.take_payload()?)?))
        })
        .collect()
}

fn child_block(elements: &[BitcodeElement], id: u32) -> Option<&BitcodeBlock> {
    elements
        .iter()
        .filter_map(BitcodeElement::as_block)
        .find(|block| block.id == id)
}

fn descendant_block(elements: &[BitcodeElement], id: u32) -> Option<&BitcodeBlock> {
    for block in elements.iter().filter_map(BitcodeElement::as_block) {
        if block.id == id {
            return Some(block);
        }
        if let Some(found) = descendant_block(&block.elements, id) {
            return Some(found);
        }
    }
    None
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
