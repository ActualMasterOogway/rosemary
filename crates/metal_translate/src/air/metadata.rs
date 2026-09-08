//! LLVM metadata decoding for AIR entry-point interfaces.
//!
//! AIR describes a shader's stage interface entirely in module metadata: which
//! argument carries which vertex attribute, buffer, texture or sampler, and
//! which part of the returned value is which render target. Decoding the
//! metadata graph keeps that mapping independent of parameter spellings and of
//! any particular shader compiler's argument order.

use super::values::ValueTable;
use super::*;

/// One decoded metadata value. Only the shapes AIR emits are represented;
/// debug-information nodes are recorded as `Other` and never interpreted.
#[derive(Clone, Debug)]
pub(super) enum MetadataValue {
    String(String),
    Node(Vec<Option<MetadataOperand>>),
    Other,
}

#[derive(Clone, Copy, Debug)]
pub(super) enum MetadataOperand {
    /// Another metadata value, by metadata index.
    Metadata(usize),
    /// A module value, by LLVM value ID. AIR uses these for the entry
    /// function and for the integer literals in interface descriptors.
    Value(usize),
}

pub(super) struct Metadata {
    values: Vec<MetadataValue>,
    named: HashMap<String, Vec<usize>>,
}

impl Metadata {
    /// Decode every module-level metadata block.
    ///
    /// AIR is written by an older LLVM, which numbers metadata values by the
    /// order of the records that create them. `NAME` and `KIND` records do not
    /// create values and must not advance that numbering.
    pub(super) fn decode(
        module_block: &BitcodeBlock,
        types: &[DecodedType],
    ) -> Result<Self, TranslateError> {
        let mut values = Vec::new();
        let mut named = HashMap::new();
        let mut pending_name: Option<String> = None;
        for block in module_block
            .elements
            .iter()
            .filter_map(BitcodeElement::as_block)
            .filter(|block| block.id == 15)
        {
            for record in block.elements.iter().filter_map(BitcodeElement::as_record) {
                match record.id {
                    // METADATA_STRING_OLD
                    1 => {
                        let text = record
                            .clone()
                            .take_payload()
                            .and_then(payload_string)
                            .or_else(|| record_ascii(record))
                            .ok_or_else(|| semantic("AIR metadata string is not UTF-8"))?;
                        values.push(MetadataValue::String(text));
                    }
                    // METADATA_NAME: names the following NAMED_NODE.
                    4 => {
                        pending_name = record
                            .clone()
                            .take_payload()
                            .and_then(payload_string)
                            .or_else(|| record_ascii(record));
                    }
                    // METADATA_NAMED_NODE: its fields are metadata indices.
                    10 => {
                        let Some(name) = pending_name.take() else {
                            return Err(semantic("AIR named metadata has no preceding name"));
                        };
                        let members = record_values(record)
                            .iter()
                            .map(|index| {
                                usize::try_from(*index).map_err(|_| {
                                    semantic("AIR named metadata index does not fit in memory")
                                })
                            })
                            .collect::<Result<_, _>>()?;
                        named.insert(name, members);
                    }
                    // METADATA_KIND declares an attachment kind, not a value.
                    6 => {}
                    // METADATA_OLD_NODE: (type, value) operand pairs.
                    8 => {
                        let fields = record_values(record);
                        if !fields.len().is_multiple_of(2) {
                            return Err(semantic("AIR metadata node has an odd operand count"));
                        }
                        let mut operands = Vec::new();
                        for pair in fields.as_chunks::<2>().0 {
                            let (ty, value) = (pair[0], pair[1]);
                            let index = usize::try_from(value).map_err(|_| {
                                semantic("AIR metadata operand does not fit in memory")
                            })?;
                            let ty = usize::try_from(ty).map_err(|_| {
                                semantic("AIR metadata operand type does not fit in memory")
                            })?;
                            operands.push(match types.get(ty) {
                                // A null operand is written as type 0, value 0.
                                None => None,
                                Some(DecodedType::Metadata) => {
                                    Some(MetadataOperand::Metadata(index))
                                }
                                Some(_) => Some(MetadataOperand::Value(index)),
                            });
                        }
                        values.push(MetadataValue::Node(operands));
                    }
                    _ => values.push(MetadataValue::Other),
                }
            }
        }
        Ok(Self { values, named })
    }

    pub(super) fn named(&self, name: &str) -> Option<&[usize]> {
        self.named.get(name).map(Vec::as_slice)
    }

    pub(super) fn node(&self, index: usize) -> Option<&[Option<MetadataOperand>]> {
        match self.values.get(index)? {
            MetadataValue::Node(operands) => Some(operands),
            _ => None,
        }
    }

    pub(super) fn string(&self, index: usize) -> Option<&str> {
        match self.values.get(index)? {
            MetadataValue::String(text) => Some(text),
            _ => None,
        }
    }
}

fn record_ascii(record: &llvm_bitcode::bitcode::Record) -> Option<String> {
    String::from_utf8(
        record
            .fields()
            .iter()
            .map(|byte| u8::try_from(*byte).ok())
            .collect::<Option<Vec<_>>>()?,
    )
    .ok()
}

/// One operand of an interface descriptor node, resolved to the two shapes AIR
/// uses: a literal string key or an integer constant.
#[derive(Clone, Copy, Debug)]
pub(super) enum Item<'a> {
    Text(&'a str),
    Number(u64),
}

impl Metadata {
    /// Flatten a descriptor node into strings and integers, resolving module
    /// constants through the value table.
    pub(super) fn items<'a>(
        &'a self,
        node: usize,
        values: &ValueTable,
        types: &[DecodedType],
    ) -> Result<Vec<Item<'a>>, TranslateError> {
        let operands = self
            .node(node)
            .ok_or_else(|| semantic("AIR interface descriptor is not a metadata node"))?;
        let mut items = Vec::new();
        for operand in operands {
            match operand {
                // A nested node, such as `air.struct_type_info`, carries
                // reflection this translator does not need.
                Some(MetadataOperand::Metadata(index)) => {
                    if let Some(text) = self.string(*index) {
                        items.push(Item::Text(text));
                    }
                }
                Some(MetadataOperand::Value(id)) => {
                    items.push(Item::Number(values.integer(*id, types)?));
                }
                None => {}
            }
        }
        Ok(items)
    }
}

/// A resource binding described by AIR metadata, before its Metal IR type is
/// known. The type comes from the entry function's parameter list.
#[derive(Clone, Debug)]
pub(super) struct MetadataResource {
    pub argument: u32,
    pub kind: ResourceKind,
    pub index: u32,
    pub count: u32,
    pub access: Access,
    pub name: Option<String>,
    /// The Metal type spelled by `air.arg_type_name`, for arguments whose
    /// LLVM type is opaque. A buffer's type comes from LLVM instead.
    pub ty: Option<Type>,
}

/// The complete stage interface of one AIR entry point.
#[derive(Debug)]
pub(super) struct StageInterface {
    /// LLVM value ID of the entry function.
    pub function: usize,
    pub resources: Vec<MetadataResource>,
    pub inputs: Vec<StageInput>,
    pub outputs: Vec<StageOutput>,
}

impl Metadata {
    /// Decode the entry point declared by `air.vertex`, `air.fragment` or
    /// `air.kernel`.
    pub(super) fn stage_interface(
        &self,
        stage: Stage,
        values: &ValueTable,
        types: &[DecodedType],
    ) -> Result<StageInterface, TranslateError> {
        let name = match stage {
            Stage::Vertex => "air.vertex",
            Stage::Fragment => "air.fragment",
            Stage::Kernel => "air.kernel",
            _ => return Err(semantic("AIR entry stage has no metadata name")),
        };
        let [entry] = self
            .named(name)
            .ok_or_else(|| semantic("AIR has no entry-point metadata"))?
        else {
            return Err(semantic("AIR declares more than one entry point"));
        };
        let [function, outputs, inputs] = self
            .node(*entry)
            .ok_or_else(|| semantic("AIR entry metadata is not a node"))?
        else {
            return Err(semantic(
                "AIR entry metadata does not have a function, output and input list",
            ));
        };
        let Some(MetadataOperand::Value(function)) = function else {
            return Err(semantic("AIR entry metadata has no function"));
        };
        let outputs = match outputs {
            Some(MetadataOperand::Metadata(index)) => self.list(*index)?,
            // A kernel has no returned value and no output list.
            None => Vec::new(),
            _ => return Err(semantic("AIR entry output list is not metadata")),
        };
        let inputs = match inputs {
            Some(MetadataOperand::Metadata(index)) => self.list(*index)?,
            None => Vec::new(),
            _ => return Err(semantic("AIR entry input list is not metadata")),
        };
        let mut interface = StageInterface {
            function: *function,
            resources: Vec::new(),
            inputs: Vec::new(),
            outputs: Vec::new(),
        };
        for (position, node) in outputs.into_iter().enumerate() {
            let items = self.items(node, values, types)?;
            interface.outputs.push(StageOutput {
                field: Some(position as u32),
                binding: output_binding(&items)?,
                name: argument_name(&items),
            });
        }
        for node in inputs {
            let items = self.items(node, values, types)?;
            let (&Item::Number(argument), rest) = items
                .split_first()
                .ok_or_else(|| semantic("AIR interface descriptor is empty"))?
            else {
                return Err(semantic("AIR interface descriptor has no argument index"));
            };
            let argument = u32::try_from(argument)
                .map_err(|_| semantic("AIR argument index does not fit in 32 bits"))?;
            let Some(Item::Text(tag)) = rest.first() else {
                return Err(semantic("AIR interface descriptor has no kind"));
            };
            match resource_kind(tag) {
                Some(kind) => {
                    let access = access(rest);
                    interface.resources.push(MetadataResource {
                        argument,
                        kind,
                        index: location_index(rest)?,
                        count: location_count(rest)?,
                        access,
                        name: argument_name(&items),
                        ty: match kind {
                            ResourceKind::Texture => Some(texture_type(&items, access)?),
                            ResourceKind::Sampler => Some(Type::Sampler),
                            _ => None,
                        },
                    });
                }
                None => interface.inputs.push(StageInput {
                    argument,
                    binding: input_binding(tag, rest)?,
                    name: argument_name(&items),
                }),
            }
        }
        Ok(interface)
    }

    /// Read a metadata node whose operands are all metadata references.
    fn list(&self, index: usize) -> Result<Vec<usize>, TranslateError> {
        self.node(index)
            .ok_or_else(|| semantic("AIR interface list is not a metadata node"))?
            .iter()
            .map(|operand| match operand {
                Some(MetadataOperand::Metadata(index)) => Ok(*index),
                _ => Err(semantic("AIR interface list has a non-metadata member")),
            })
            .collect()
    }
}

fn resource_kind(tag: &str) -> Option<ResourceKind> {
    match tag {
        "air.buffer" => Some(ResourceKind::Buffer),
        "air.indirect_buffer" | "air.argument_buffer" => Some(ResourceKind::ArgumentBuffer),
        "air.texture" | "air.depth_texture" => Some(ResourceKind::Texture),
        "air.sampler" => Some(ResourceKind::Sampler),
        _ => None,
    }
}

/// `air.location_index` is followed by the Metal resource index and the
/// number of consecutive indices the argument occupies.
fn location_index(items: &[Item<'_>]) -> Result<u32, TranslateError> {
    match keyed(items, "air.location_index") {
        [Item::Number(index), ..] => u32::try_from(*index)
            .map_err(|_| semantic("AIR location index does not fit in 32 bits")),
        _ => Err(semantic("AIR resource has no location index")),
    }
}

fn location_count(items: &[Item<'_>]) -> Result<u32, TranslateError> {
    match keyed(items, "air.location_index") {
        [_, Item::Number(count), ..] => u32::try_from(*count)
            .map_err(|_| semantic("AIR location count does not fit in 32 bits")),
        _ => Err(semantic("AIR resource has no location count")),
    }
}

fn access(items: &[Item<'_>]) -> Access {
    for item in items {
        match item {
            Item::Text("air.sample") => return Access::Sample,
            Item::Text("air.write") => return Access::Write,
            Item::Text("air.read_write") => return Access::ReadWrite,
            _ => {}
        }
    }
    Access::Read
}

/// Build a Metal texture type from `air.arg_type_name`, which spells the
/// complete Metal type, for instance `texture2d_array<half>` or
/// `depthcube<float>`. The LLVM type of the argument is opaque and carries
/// none of this.
fn texture_type(items: &[Item<'_>], access: Access) -> Result<Type, TranslateError> {
    let Some(Item::Text(name)) = keyed(items, "air.arg_type_name").first() else {
        return Err(semantic("AIR texture has no argument type name"));
    };
    let (kind, texel) = match name.split_once('<') {
        Some((kind, rest)) => (
            kind,
            rest.split(',')
                .next()
                .and_then(|texel| texel.strip_suffix('>').or(Some(texel)))
                .unwrap_or("float")
                .trim(),
        ),
        None => (*name, "float"),
    };
    // Metal spells a texture's element signedness in its type, but the AIR
    // body reads texels as signless LLVM integers, so keep the storage width
    // and let the reading operation carry the interpretation.
    let texel = match texel {
        "float" => Type::Float { bits: 32 },
        "half" => Type::Float { bits: 16 },
        "int" | "uint" => Type::u32(),
        "short" | "ushort" => Type::Int {
            bits: 16,
            signed: false,
        },
        texel => {
            return Err(semantic(format!(
                "AIR texture element type {texel} needs Metal IR lowering"
            )));
        }
    };
    let depth = kind.starts_with("depth");
    let body = kind
        .strip_prefix("texture")
        .or_else(|| kind.strip_prefix("depth"))
        .ok_or_else(|| semantic(format!("AIR texture type {name} needs Metal IR lowering")))?;
    let (body, array) = match body.strip_suffix("_array") {
        Some(body) => (body, true),
        None => (body, false),
    };
    let (body, multisampled) = match body.strip_suffix("_ms") {
        Some(body) => (body, true),
        None => (body, false),
    };
    let dimension = match body {
        "1d" => TextureDimension::D1,
        "2d" => TextureDimension::D2,
        "3d" => TextureDimension::D3,
        "cube" => TextureDimension::Cube,
        _ => {
            return Err(semantic(format!(
                "AIR texture type {name} needs Metal IR lowering"
            )));
        }
    };
    Ok(Type::Texture(TextureType {
        dimension,
        array,
        multisampled,
        depth,
        texel: Box::new(texel),
        access,
    }))
}

fn argument_name(items: &[Item<'_>]) -> Option<String> {
    match keyed(items, "air.arg_name") {
        [Item::Text(name), ..] => Some((*name).to_string()),
        _ => None,
    }
}

/// The items following a string key, up to the end of the descriptor.
fn keyed<'a, 'b>(items: &'a [Item<'b>], key: &str) -> &'a [Item<'b>] {
    items
        .iter()
        .position(|item| matches!(item, Item::Text(text) if *text == key))
        .map(|position| &items[position + 1..])
        .unwrap_or_default()
}

/// `user(locnN)` names a Metal user attribute location.
fn user_location(items: &[Item<'_>]) -> Option<u32> {
    items.iter().find_map(|item| match item {
        Item::Text(text) => text
            .strip_prefix("user(locn")
            .and_then(|rest| rest.strip_suffix(')'))
            .and_then(|digits| digits.parse().ok()),
        Item::Number(_) => None,
    })
}

fn interpolation(items: &[Item<'_>]) -> Option<Interpolation> {
    let mut sampling = None;
    let mut perspective = None;
    let mut flat = false;
    for item in items {
        match item {
            Item::Text("air.center") => sampling = Some(Sampling::Center),
            Item::Text("air.centroid") => sampling = Some(Sampling::Centroid),
            Item::Text("air.sample") => sampling = Some(Sampling::Sample),
            Item::Text("air.perspective") => perspective = Some(true),
            Item::Text("air.no_perspective") => perspective = Some(false),
            Item::Text("air.flat") => flat = true,
            _ => {}
        }
    }
    (sampling.is_some() || perspective.is_some() || flat).then(|| Interpolation {
        sampling: sampling.unwrap_or(Sampling::Center),
        perspective: perspective.unwrap_or(true),
        flat,
    })
}

fn builtin(tag: &str) -> Option<Builtin> {
    Some(match tag {
        "air.position" => Builtin::Position,
        "air.point_size" => Builtin::PointSize,
        "air.clip_distance" => Builtin::ClipDistance,
        "air.vertex_id" => Builtin::VertexId,
        "air.instance_id" => Builtin::InstanceId,
        "air.base_vertex" => Builtin::BaseVertex,
        "air.base_instance" => Builtin::BaseInstance,
        "air.front_facing" => Builtin::FrontFacing,
        "air.point_coord" => Builtin::PointCoord,
        "air.sample_id" => Builtin::SampleId,
        "air.sample_mask" => Builtin::SampleMask,
        "air.depth" => Builtin::Depth,
        "air.render_target_array_index" => Builtin::RenderTargetArrayIndex,
        "air.viewport_array_index" => Builtin::ViewportArrayIndex,
        "air.thread_position_in_grid" => Builtin::ThreadPositionInGrid,
        "air.thread_position_in_threadgroup" => Builtin::ThreadPositionInThreadgroup,
        "air.threadgroup_position_in_grid" => Builtin::ThreadgroupPositionInGrid,
        "air.threadgroups_per_grid" => Builtin::ThreadgroupsPerGrid,
        "air.thread_index_in_threadgroup" => Builtin::ThreadIndexInThreadgroup,
        "air.threads_per_threadgroup" => Builtin::ThreadsPerThreadgroup,
        _ => return None,
    })
}

fn input_binding(tag: &str, rest: &[Item<'_>]) -> Result<StageBinding, TranslateError> {
    match tag {
        "air.vertex_input" => Ok(StageBinding::Location {
            location: location_index(rest)?,
            interpolation: None,
        }),
        "air.fragment_input" => Ok(StageBinding::Location {
            location: user_location(rest)
                .ok_or_else(|| semantic("AIR fragment input has no user location"))?,
            interpolation: interpolation(rest),
        }),
        tag => builtin(tag)
            .map(StageBinding::Builtin)
            .ok_or_else(|| semantic(format!("AIR stage input {tag} needs Metal IR lowering"))),
    }
}

fn output_binding(items: &[Item<'_>]) -> Result<StageBinding, TranslateError> {
    let Some(Item::Text(tag)) = items.first() else {
        return Err(semantic("AIR stage output has no kind"));
    };
    match *tag {
        "air.render_target" => match items.get(1) {
            Some(Item::Number(index)) => Ok(StageBinding::RenderTarget {
                index: u32::try_from(*index)
                    .map_err(|_| semantic("AIR render target index does not fit in 32 bits"))?,
            }),
            _ => Err(semantic("AIR render target has no index")),
        },
        "air.vertex_output" => Ok(StageBinding::Location {
            location: user_location(&items[1..])
                .ok_or_else(|| semantic("AIR vertex output has no user location"))?,
            interpolation: None,
        }),
        tag => builtin(tag)
            .map(StageBinding::Builtin)
            .ok_or_else(|| semantic(format!("AIR stage output {tag} needs Metal IR lowering"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::air::fixtures::{bitcode_block, block, record};

    /// A module whose types are `void, i32, metadata, ptr, fn` and whose
    /// metadata describes one fragment entry point.
    fn module() -> BitcodeBlock {
        bitcode_block(block(
            8,
            vec![
                // MODULE_CODE_VERSION 0, so value references are absolute.
                record(1, &[0]),
                // The entry function occupies module value 0.
                record(8, &[3, 0, 0, 0]),
                block(
                    11,
                    vec![
                        // Constants: SETTYPE i32, then 0, 1 and 2.
                        record(1, &[1]),
                        record(4, &[0]),
                        record(4, &[2]),
                        record(4, &[4]),
                    ],
                ),
                block(
                    15,
                    vec![
                        record(1, string("air.render_target")),                 // 0
                        record(8, &[2, 0, 1, 1]),                               // 1: output
                        record(1, string("air.fragment_input")),                // 2
                        record(1, string("user(locn3)")),                       // 3
                        record(1, string("air.center")),                        // 4
                        record(1, string("air.perspective")),                   // 5
                        record(1, string("air.arg_name")),                      // 6
                        record(1, string("IN_Color")),                          // 7
                        record(8, &[1, 1, 2, 2, 2, 3, 2, 4, 2, 5, 2, 6, 2, 7]), // 8: input 0
                        record(1, string("air.buffer")),                        // 9
                        record(1, string("air.location_index")),                // 10
                        record(1, string("air.read")),                          // 11
                        record(8, &[1, 2, 2, 9, 2, 10, 1, 2, 1, 2, 2, 11]),     // 12: input 1
                        record(8, &[2, 1]),                                     // 13: output list
                        record(8, &[2, 8, 2, 12]),                              // 14: input list
                        record(8, &[3, 0, 2, 13, 2, 14]),                       // 15: entry
                        record(4, string("air.fragment")),
                        record(10, &[15]),
                    ],
                ),
            ],
        ))
    }

    fn string(text: &str) -> &[u64] {
        // Records store one byte per field; the fixture builder takes u64s.
        Box::leak(
            text.bytes()
                .map(u64::from)
                .collect::<Vec<_>>()
                .into_boxed_slice(),
        )
    }

    fn types() -> Vec<DecodedType> {
        vec![
            DecodedType::Void,
            DecodedType::Integer(32),
            DecodedType::Metadata,
            DecodedType::Pointer {
                pointee: 4,
                address_space: 0,
            },
            DecodedType::Function {
                result: 0,
                parameters: Vec::new(),
            },
        ]
    }

    #[test]
    fn decodes_a_fragment_stage_interface_from_metadata() {
        let module = module();
        let types = types();
        let metadata = Metadata::decode(&module, &types).unwrap();
        let values = ValueTable::module_values(&module, &types).unwrap();
        let interface = metadata
            .stage_interface(Stage::Fragment, &values, &types)
            .unwrap();
        assert_eq!(interface.function, 0);
        assert_eq!(
            interface.outputs,
            vec![StageOutput {
                field: Some(0),
                binding: StageBinding::RenderTarget { index: 0 },
                name: None,
            }]
        );
        assert_eq!(
            interface.inputs,
            vec![StageInput {
                argument: 0,
                binding: StageBinding::Location {
                    location: 3,
                    interpolation: Some(Interpolation {
                        sampling: Sampling::Center,
                        perspective: true,
                        flat: false,
                    }),
                },
                name: Some("IN_Color".to_string()),
            }]
        );
        let [buffer] = interface.resources.as_slice() else {
            panic!("expected one buffer resource");
        };
        assert_eq!(buffer.argument, 1);
        assert_eq!(buffer.kind, ResourceKind::Buffer);
        assert_eq!(buffer.index, 1);
        assert_eq!(buffer.count, 1);
        assert_eq!(buffer.access, Access::Read);
    }

    /// Metal spells a texture's complete shape in `air.arg_type_name`; the
    /// LLVM argument type is opaque and carries none of it.
    #[test]
    fn decodes_texture_shapes_from_their_metal_type_name() {
        let shape = |name: &str, access| {
            texture_type(&[Item::Text("air.arg_type_name"), Item::Text(name)], access)
        };
        assert_eq!(
            shape("texture2d<float>", Access::Sample).unwrap(),
            Type::Texture(TextureType {
                dimension: TextureDimension::D2,
                array: false,
                multisampled: false,
                depth: false,
                texel: Box::new(Type::Float { bits: 32 }),
                access: Access::Sample,
            })
        );
        assert_eq!(
            shape("depthcube<float>", Access::Sample).unwrap(),
            Type::Texture(TextureType {
                dimension: TextureDimension::Cube,
                array: false,
                multisampled: false,
                depth: true,
                texel: Box::new(Type::Float { bits: 32 }),
                access: Access::Sample,
            })
        );
        assert_eq!(
            shape("texture2d_ms<half>", Access::Read).unwrap(),
            Type::Texture(TextureType {
                dimension: TextureDimension::D2,
                array: false,
                multisampled: true,
                depth: false,
                texel: Box::new(Type::Float { bits: 16 }),
                access: Access::Read,
            })
        );
        let Type::Texture(array) = shape("texture2d_array<uint>", Access::Write).unwrap() else {
            panic!("expected a texture type");
        };
        assert!(array.array);
        // Metal's element signedness is not a storage difference, so it is
        // carried by the reading operation rather than by the type.
        assert_eq!(array.texel.as_ref(), &Type::u32());
        assert!(shape("texture4d<float>", Access::Sample).is_err());
        assert!(shape("texture2d<double>", Access::Sample).is_err());
    }

    /// Metadata numbering counts only value-producing records. A NAME record
    /// between nodes must not shift the indices its NAMED_NODE refers to.
    #[test]
    fn named_records_do_not_consume_metadata_indices() {
        let module = bitcode_block(block(
            8,
            vec![
                record(1, &[0]),
                block(
                    15,
                    vec![
                        record(1, string("first")),
                        record(4, string("unrelated")),
                        record(10, &[0]),
                        record(1, string("second")),
                    ],
                ),
            ],
        ));
        let metadata = Metadata::decode(&module, &types()).unwrap();
        assert_eq!(metadata.string(0), Some("first"));
        assert_eq!(metadata.string(1), Some("second"));
        assert_eq!(metadata.named("unrelated"), Some([0].as_slice()));
    }
}
