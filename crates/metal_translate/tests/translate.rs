use metal_translate::ir::{
    Access, Block, BlockId, EntryPoint, Function, InstructionKind, Module, Parameter, Resource,
    ResourceKind, Stage, Terminator, Type, ValueId,
};
use metal_translate::{
    AirInput, Phase, TargetProfile, emit_spirv, metallib_air, parse_air, parse_msl, translate,
    translate_msl,
};
use rspirv::spirv::Op;

const STORE: &str = include_str!("fixtures/kernel_store_const.ll");
const COPY: &str = include_str!("fixtures/kernel_copy_word.ll");
const INVERT: &str = include_str!("fixtures/kernel_invert_word.ll");
const LOOP: &str = include_str!("fixtures/kernel_bounded_loop_word.ll");

fn store_bitcode() -> Vec<u8> {
    let encoded = include_str!("fixtures/kernel_store_const.bc.hex")
        .trim()
        .as_bytes();
    let (pairs, remainder) = encoded.as_chunks::<2>();
    assert!(remainder.is_empty());
    pairs
        .iter()
        .map(|pair| {
            let text = std::str::from_utf8(pair).unwrap();
            u8::from_str_radix(text, 16).unwrap()
        })
        .collect()
}

#[test]
fn imports_the_four_core_compute_fixtures() {
    for (source, name) in [
        (STORE, "store_const"),
        (COPY, "copy_word"),
        (INVERT, "invert_word"),
        (LOOP, "bounded_loop_word"),
    ] {
        let module = parse_air(AirInput::Text(source)).unwrap();
        assert_eq!(module.functions[0].name, name);
        assert_eq!(module.entries[0].stage, Stage::Kernel);
    }
}

#[test]
fn preserves_metal_resource_access_and_indices() {
    let module = parse_air(AirInput::Text(COPY)).unwrap();
    let resources = &module.entries[0].resources;
    assert_eq!(resources.len(), 2);
    assert_eq!((resources[0].index, resources[0].access), (0, Access::Read));
    assert_eq!(
        (resources[1].index, resources[1].access),
        (1, Access::Write)
    );
}

#[test]
fn preserves_loop_ssa_before_spirv_lowering() {
    let module = parse_air(AirInput::Text(LOOP)).unwrap();
    assert_eq!(module.functions[0].blocks.len(), 4);
    assert!(matches!(
        module.functions[0].blocks[1].instructions[0].kind,
        InstructionKind::Phi { .. }
    ));
}

#[test]
fn emits_parseable_spirv_for_each_core_kernel() {
    for source in [STORE, COPY, INVERT, LOOP] {
        let library = translate(AirInput::Text(source), &TargetProfile::vulkan_1_3()).unwrap();
        assert_eq!(library.entries.len(), 1);
        let entry = &library.entries[0];
        assert_eq!(entry.words[0], rspirv::spirv::MAGIC_NUMBER);
        assert_eq!(entry.required.capabilities, ["Shader"]);
        let module = rspirv::dr::load_words(&entry.words).unwrap();
        assert_eq!(module.entry_points.len(), 1);
    }
}

#[test]
fn maps_buffer_indices_into_the_buffer_descriptor_set() {
    let library = translate(AirInput::Text(COPY), &TargetProfile::vulkan_1_3()).unwrap();
    let resources = &library.entries[0].resources;
    assert_eq!((resources[0].descriptor_set, resources[0].binding), (0, 0));
    assert_eq!((resources[1].descriptor_set, resources[1].binding), (0, 1));
}

#[test]
fn emits_loop_merge_and_phi_for_the_bounded_loop() {
    let library = translate(AirInput::Text(LOOP), &TargetProfile::vulkan_1_3()).unwrap();
    let module = rspirv::dr::load_words(&library.entries[0].words).unwrap();
    let opcodes: Vec<_> = module
        .functions
        .iter()
        .flat_map(|function| &function.blocks)
        .flat_map(|block| &block.instructions)
        .map(|instruction| instruction.class.opcode)
        .collect();
    assert!(opcodes.contains(&Op::Phi));
    assert!(opcodes.contains(&Op::LoopMerge));
}

#[test]
fn rejects_non_kernel_air_without_a_spirv_fallback() {
    let error = parse_air(AirInput::Text(
        "define void @f() { ret void }\n!air.fragment = !{!0}\n",
    ))
    .unwrap_err();
    assert_eq!(error.diagnostics[0].phase, Phase::AirSemantics);
}

#[test]
fn rejects_truncated_bitcode_as_a_container_error() {
    let error = parse_air(AirInput::Bitcode(b"not bitcode")).unwrap_err();
    assert_eq!(error.diagnostics[0].phase, Phase::AirContainer);
}

#[test]
fn extracts_air_bitcode_from_a_metallib() {
    let payload = [b'B', b'C', 0xc0, 0xde, 1, 2, 3, 4];
    let mut library = vec![0; 88];
    library[..4].copy_from_slice(b"MTLB");
    library[72..80].copy_from_slice(&88_u64.to_le_bytes());
    library[80..88].copy_from_slice(&28_u64.to_le_bytes());
    library.extend_from_slice(&0x0b17_c0de_u32.to_le_bytes());
    library.extend_from_slice(&0_u32.to_le_bytes());
    library.extend_from_slice(&20_u32.to_le_bytes());
    library.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    library.extend_from_slice(&u32::MAX.to_le_bytes());
    library.extend_from_slice(&payload);

    assert_eq!(metallib_air(&library).unwrap(), payload);
}

#[test]
fn textual_and_bitcode_store_modules_are_equivalent() {
    let text = parse_air(AirInput::Text(STORE)).unwrap();
    let bitcode = parse_air(AirInput::Bitcode(&store_bitcode())).unwrap();
    assert_eq!(bitcode, text);
    translate(
        AirInput::Bitcode(&store_bitcode()),
        &TargetProfile::vulkan_1_3(),
    )
    .unwrap();
}

#[test]
fn emission_is_deterministic() {
    let first = translate(AirInput::Text(LOOP), &TargetProfile::vulkan_1_3()).unwrap();
    let second = translate(AirInput::Text(LOOP), &TargetProfile::vulkan_1_3()).unwrap();
    assert_eq!(first, second);
}

#[test]
fn rejects_malformed_resource_metadata() {
    let source = STORE.replace("air.location_index", "air.unknown_location");
    let error = parse_air(AirInput::Text(&source)).unwrap_err();
    assert_eq!(error.diagnostics[0].phase, Phase::AirSemantics);
}

#[test]
fn rejects_unsupported_air_instructions() {
    let source = INVERT.replace("xor i32", "mul i32");
    let error = parse_air(AirInput::Text(&source)).unwrap_err();
    assert_eq!(error.diagnostics[0].phase, Phase::AirSyntax);
}

#[test]
fn verifier_rejects_an_undefined_ssa_use() {
    let mut module = parse_air(AirInput::Text(STORE)).unwrap();
    let metal_translate::ir::InstructionKind::Store { value, .. } =
        &mut module.functions[0].blocks[0].instructions[0].kind
    else {
        unreachable!()
    };
    *value = metal_translate::ir::Operand::Value(metal_translate::ir::ValueId(99));
    let error = module.verify().unwrap_err();
    assert_eq!(error.diagnostics[0].phase, Phase::IrVerification);
}

#[test]
fn verifier_rejects_duplicate_metal_bindings() {
    let mut module = parse_air(AirInput::Text(COPY)).unwrap();
    module.entries[0].resources[1].index = 0;
    assert!(module.verify().is_err());
}

#[test]
fn rejects_a_target_without_required_capabilities() {
    let mut target = TargetProfile::vulkan_1_3();
    target.shader = false;
    let error = translate(AirInput::Text(STORE), &target).unwrap_err();
    assert_eq!(error.diagnostics[0].phase, Phase::SpirvEmission);
}

#[test]
fn compiles_a_source_msl_compute_kernel_to_spirv() {
    let source = "kernel void clear(device uint* output [[buffer(3)]]) { output[0] = 42; }";
    let module = parse_msl(source).unwrap();
    assert_eq!(module.functions[0].name, "clear");
    assert_eq!(module.entries[0].resources[0].index, 3);
    let library = translate_msl(source, &TargetProfile::vulkan_1_3()).unwrap();
    assert_eq!(library.entries[0].words[0], rspirv::spirv::MAGIC_NUMBER);
    assert_eq!(library.entries[0].resources[0].binding, 3);
}

#[test]
fn source_msl_rejects_unimplemented_operations() {
    let error = parse_msl(
        "\n\
         kernel void f(device uint* output [[buffer(0)]]) {\n\
             output[1] = 1;\n\
         }",
    )
    .unwrap_err();
    assert_eq!(error.diagnostics[0].phase, Phase::AirSyntax);
    assert_eq!(error.diagnostics[0].line, Some(2));
}

#[test]
fn source_msl_translation_errors_include_nearby_source() {
    let source = "// Prelude\n\
                  // More prelude\n\
                  kernel void f(\n\
                      texture2d<float, access::write> output [[texture(0)]]\n\
                  ) { return; }\n\
                  // Epilogue";
    let error = translate_msl(source, &TargetProfile::vulkan_1_3()).unwrap_err();

    assert_eq!(error.diagnostics[0].line, Some(4));
    assert_eq!(
        error.to_string(),
        concat!(
            "AirSyntax at line 4: unsupported MSL texture type\n\n",
            "MSL source context:\n",
            "  2 | // More prelude\n",
            "  3 | kernel void f(\n",
            "> 4 | texture2d<float, access::write> output [[texture(0)]]\n",
            "  5 | ) { return; }\n",
            "  6 | // Epilogue",
        )
    );
}

#[test]
fn source_msl_rejects_texture_shapes_without_an_ir_type() {
    let error = parse_msl(
        "kernel void f(texture2d<float, access::write> output [[texture(0)]]) { return; }",
    )
    .unwrap_err();
    assert_eq!(error.diagnostics[0].message, "unsupported MSL texture type");
}

#[test]
fn source_msl_compiles_a_bounded_typed_blit_kernel() {
    let source = "struct BlitParams { uint3 offset; uint3 size; uint2 stride; };\n\
         kernel void blit(\n\
           uint3 gid [[thread_position_in_grid]],\n\
           constant BlitParams& params [[buffer(0)]],\n\
           device uchar4* input [[buffer(1)]],\n\
         texture3d<float, access::write> output [[texture(0)]])\n\
         {\n\
           if (all(gid < params.size)) {\n\
             uchar4 element = input[(gid.z * params.stride.y + gid.y) * params.stride.x + gid.x + params.offset.x];\n\
             output.write(float4(element) / 255.f, gid + params.offset);\n\
           }\n\
         }";
    let library = translate_msl(source, &TargetProfile::vulkan_1_3()).unwrap();
    let module = rspirv::dr::load_words(&library.entries[0].words).unwrap();
    for operation in [
        Op::ImageWrite,
        Op::ULessThan,
        Op::All,
        Op::ConvertUToF,
        Op::FDiv,
        Op::IMul,
        Op::IAdd,
    ] {
        assert!(
            module
                .functions
                .iter()
                .flat_map(|function| &function.blocks)
                .flat_map(|block| &block.instructions)
                .any(|instruction| instruction.class.opcode == operation)
        );
    }
}

#[test]
fn emits_a_writable_3d_texture_descriptor_in_its_metal_namespace() {
    let module = Module {
        functions: vec![Function {
            name: "clear_volume".to_string(),
            result: Type::Void,
            parameters: vec![Parameter {
                value: ValueId(0),
                name: "output".to_string(),
                ty: Type::Texture,
            }],
            values: vec![Type::Texture],
            blocks: vec![Block {
                id: BlockId(0),
                name: "entry".to_string(),
                instructions: Vec::new(),
                terminator: Terminator::Return(None),
            }],
        }],
        entries: vec![EntryPoint {
            function: 0,
            stage: Stage::Kernel,
            resources: vec![Resource {
                argument: 0,
                kind: ResourceKind::Texture,
                index: 7,
                count: 1,
                access: Access::Write,
                ty: Type::Texture,
                name: Some("output".to_string()),
            }],
        }],
    };

    let artifact = emit_spirv(&module, &TargetProfile::vulkan_1_3())
        .unwrap()
        .entries
        .remove(0);
    assert_eq!(artifact.resources[0].descriptor_set, 1);
    assert_eq!(artifact.resources[0].binding, 7);
    let parsed = rspirv::dr::load_words(&artifact.words).unwrap();
    assert!(
        parsed
            .types_global_values
            .iter()
            .any(|instruction| instruction.class.opcode == Op::TypeImage)
    );
}
