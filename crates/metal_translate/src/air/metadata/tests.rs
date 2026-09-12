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
