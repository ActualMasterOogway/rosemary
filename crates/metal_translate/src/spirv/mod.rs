//! SPIR-V emission from Metal IR.
//!
//! Emission assumes structured control flow, so a function is restructured
//! into it first: [`structurize`] rewrites the shape and [`cfg`] is the
//! analysis both it and emission read.

mod cfg;
mod emit;
mod structurize;

use cfg::{Merge, control_flow_merges};
use emit::Emitter;
use structurize::{
    hoist_phis, isolate_loop_merges, isolate_shared_selection_paths, limit, normalize_loop_headers,
    reverse_post_order, separate_loop_latches, single_exit, split_shared_merges,
};

use std::collections::{BTreeMap, BTreeSet, HashMap};

use rspirv::binary::Assemble;
use rspirv::dr::{Builder, InsertPoint, Operand as SpirvOperand};
use rspirv::spirv::{
    AddressingModel, BuiltIn, Capability, Decoration, Dim, ExecutionMode, ExecutionModel,
    FunctionControl, GlslStd450Op, ImageFormat, ImageOperands, LoopControl, MemoryModel,
    MemorySemantics, SelectionControl, StorageClass,
};

use crate::ir::{
    Access, AddressSpace, AtomicOp, BarrierMemory, BarrierScope, BlockId, Builtin, ConvertOp,
    EntryPoint, FloatBinaryOp, FloatPredicate, FloatUnaryOp, Function, InstructionKind,
    IntegerBinaryOp, IntegerPredicate, IntegerUnaryOp, MemoryOrder, Module, Operand, ResourceKind,
    Sampling, Stage, StageBinding, Terminator, TextureDimension, Type, ValueId, coordinate_lanes,
    instruction_operands, instruction_operands_mut, is_boolean_or_boolean_vector, operand_type,
    terminator_operands,
};
use crate::{Diagnostic, Phase, TranslateError};

/// Vulkan and SPIR-V capabilities available to an emitter invocation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TargetProfile {
    pub vulkan_version: (u8, u8),
    pub spirv_version: (u8, u8),
    pub max_bound_descriptor_sets: u32,
    pub shader: bool,
    pub shader_int64: bool,
    /// `VkPhysicalDeviceVariablePointersFeatures::variablePointersStorageBuffer`,
    /// which a pointer into a buffer needs before it can be a phi or a select.
    pub variable_pointers_storage_buffer: bool,
    /// `variablePointers`, which widens the above to `Workgroup` memory - a
    /// pointer into a Metal `threadgroup` array.
    pub variable_pointers: bool,
}

impl TargetProfile {
    pub fn vulkan_1_3() -> Self {
        Self {
            vulkan_version: (1, 3),
            spirv_version: (1, 6),
            max_bound_descriptor_sets: 4,
            shader: true,
            shader_int64: false,
            variable_pointers_storage_buffer: false,
            variable_pointers: false,
        }
    }
}

impl Default for TargetProfile {
    fn default() -> Self {
        Self::vulkan_1_3()
    }
}

/// The specialization constants a compute entry point's threadgroup size uses.
///
/// The runtime sets these when it creates the Vulkan pipeline for a Metal
/// dispatch, because Metal names the threadgroup size at dispatch time.
pub const WORKGROUP_SIZE_SPEC_IDS: [u32; 3] = [0, 1, 2];

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RequiredFeatures {
    pub capabilities: Vec<String>,
    pub extensions: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResourceBinding {
    pub stage: Stage,
    pub kind: ResourceKind,
    pub metal_index: u32,
    pub descriptor_set: u32,
    pub binding: u32,
    pub count: u32,
    pub access: Access,
    /// For a texture, whether Vulkan carries it as a sampled image rather than
    /// a storage image. See [`ir::TextureType::sampled_image`]; a buffer or a
    /// sampler leaves it false.
    pub sampled: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EntryArtifact {
    pub name: String,
    pub stage: Stage,
    pub words: Vec<u32>,
    pub resources: Vec<ResourceBinding>,
    pub required: RequiredFeatures,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TranslatedLibrary {
    pub entries: Vec<EntryArtifact>,
    /// The library's `[[function_constant]]` declarations. A library with
    /// any has to be translated again for each set of values a function is
    /// made with.
    pub function_constants: Vec<FunctionConstantInfo>,
}

/// One function constant a library declares, as reflection reports it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FunctionConstantInfo {
    pub index: u32,
    pub name: String,
    /// The Itanium spelling of its Metal type.
    pub encoding: String,
}

pub fn emit_spirv(
    module: &Module,
    target: &TargetProfile,
) -> Result<TranslatedLibrary, TranslateError> {
    module.verify()?;
    validate_target(target)?;
    if !module.static_initializers.is_empty() {
        return Err(emission(
            "the module's static initializers have not been run; specialize it first",
        ));
    }
    let entries = module
        .entries
        .iter()
        .map(|entry| emit_entry(module, entry, target))
        .collect::<Result<Vec<_>, _>>()?;
    let function_constants = module
        .globals
        .iter()
        .filter_map(|global| global.function_constant.as_ref())
        .map(|slot| FunctionConstantInfo {
            index: slot.index,
            name: slot.name.clone(),
            encoding: slot.encoding.clone(),
        })
        .collect();
    Ok(TranslatedLibrary {
        entries,
        function_constants,
    })
}

fn validate_target(target: &TargetProfile) -> Result<(), TranslateError> {
    if !target.shader {
        return Err(emission(
            "target profile does not support the Shader capability",
        ));
    }
    if target.max_bound_descriptor_sets < 4 {
        return Err(emission(
            "Metal descriptor ABI requires four bound descriptor sets",
        ));
    }
    if target.vulkan_version < (1, 1) {
        return Err(emission(
            "the initial descriptor ABI requires at least Vulkan 1.1",
        ));
    }
    if target.spirv_version.0 != 1 || target.spirv_version.1 > 6 {
        return Err(emission("unsupported SPIR-V target version"));
    }
    if target.spirv_version < (1, 3) {
        return Err(emission(
            "the initial emitter requires SPIR-V 1.3 for StorageBuffer resources",
        ));
    }
    Ok(())
}

fn emit_entry(
    module: &Module,
    entry: &EntryPoint,
    target: &TargetProfile,
) -> Result<EntryArtifact, TranslateError> {
    if !matches!(entry.stage, Stage::Kernel | Stage::Fragment | Stage::Vertex) {
        return Err(emission(
            "the emitter only supports kernel, vertex and fragment entry points",
        ));
    }
    let function = module
        .functions
        .get(entry.function)
        .ok_or_else(|| emission("entry point refers to a missing function"))?;
    let mut function = single_exit(function.clone());
    // Each restructuring pass can expose work for the others: separating a
    // loop's merge from the code outside it leaves the header choosing between
    // two body paths, which is what normalizing headers answers, and that in
    // turn adds a block another construct may claim. Run them to a fixed
    // point rather than once each, and treat one that never settles as the
    // rejection it is.
    // Node splitting can expose several nested shared paths one at a time.
    // Leave generous linear headroom while still rejecting a pass cycle.
    let rounds = limit(&function).saturating_mul(8);
    let mut settled = false;
    for _ in 0..rounds {
        let mut changed = normalize_loop_headers(&mut function)?;
        changed |= separate_loop_latches(&mut function)?;
        changed |= isolate_loop_merges(&mut function)?;
        changed |= isolate_shared_selection_paths(&mut function)?;
        changed |= split_shared_merges(&mut function)?;
        if !changed {
            settled = true;
            break;
        }
    }
    if !settled {
        return Err(emission(
            "the control-flow restructuring passes never reached a fixed point",
        ));
    }
    hoist_phis(&mut function);
    reverse_post_order(&mut function);
    let function = &function;
    let mut emitter = Emitter::new(target);
    emitter.declare_globals(module)?;
    let resources = emitter.declare_resources(entry, function)?;
    emitter.declare_stage_io(entry, function)?;
    let function_id = emitter.emit_function(entry, function)?;
    let execution_model = match entry.stage {
        Stage::Kernel => ExecutionModel::GLCompute,
        Stage::Vertex => ExecutionModel::Vertex,
        Stage::Fragment => ExecutionModel::Fragment,
        _ => unreachable!(),
    };
    emitter.builder.entry_point(
        execution_model,
        function_id,
        &function.name,
        emitter.interface.clone(),
    );
    match entry.stage {
        Stage::Kernel => {
            // Metal has no threadgroup size in the shader: the host names it
            // at dispatch. SPIR-V needs one at compile time, so the size is
            // three specialization constants which the runtime supplies when
            // it creates the Vulkan pipeline for a dispatch.
            emitter.declare_workgroup_size()?;
            emitter
                .builder
                .execution_mode(function_id, ExecutionMode::LocalSize, [1, 1, 1])
        }
        Stage::Fragment => {
            emitter
                .builder
                .execution_mode(function_id, ExecutionMode::OriginUpperLeft, [])
        }
        // A vertex entry point declares no execution mode.
        _ => {}
    }
    if emitter.writes_depth {
        emitter
            .builder
            .execution_mode(function_id, ExecutionMode::DepthReplacing, []);
    }
    let mut capabilities = vec!["Shader".to_string()];
    if emitter.uses_int64 {
        capabilities.push("Int64".to_string());
    }
    let mut extensions = Vec::new();
    if let Some(capability) = emitter.uses_variable_pointers {
        capabilities.push(format!("{capability:?}"));
        // The capability is core from SPIR-V 1.3; before that it needs the
        // extension which introduced it.
        if emitter.spirv_version < (1, 3) {
            extensions.push("SPV_KHR_variable_pointers".to_string());
        }
    }
    let words = emitter.builder.module().assemble();
    rspirv::dr::load_words(&words)
        .map_err(|error| emission(format!("emitted SPIR-V could not be parsed: {error}")))?;
    Ok(EntryArtifact {
        name: function.name.clone(),
        stage: entry.stage,
        words,
        resources,
        required: RequiredFeatures {
            capabilities,
            extensions,
        },
    })
}

/// The Metal type of one stage output: a member of the returned structure, or
/// the whole result when the entry point has a single unwrapped output.
fn output_type(
    function: &Function,
    output: &crate::ir::StageOutput,
    position: usize,
) -> Result<Type, TranslateError> {
    match (&function.result, output.field) {
        (Type::Struct(structure), Some(field)) => structure
            .fields
            .get(field as usize)
            .map(|field| field.ty.clone())
            .ok_or_else(|| emission("stage output names a missing returned field")),
        (result, None) if position == 0 => Ok(result.clone()),
        _ => Err(emission("stage output does not match the returned type")),
    }
}

fn descriptor_set(kind: ResourceKind) -> u32 {
    match kind {
        ResourceKind::Texture => 1,
        ResourceKind::Sampler => 2,
        ResourceKind::Imageblock => 3,
        ResourceKind::Buffer
        | ResourceKind::AccelerationStructure
        | ResourceKind::VisibleFunctionTable
        | ResourceKind::IntersectionFunctionTable
        | ResourceKind::ArgumentBuffer => 0,
    }
}

fn type_stride(ty: &Type) -> Result<u32, TranslateError> {
    match ty {
        Type::Int { bits, .. } | Type::Float { bits } => Ok(u32::from(*bits) / 8),
        Type::Vector { element, lanes } => {
            let element = type_stride(element)?;
            Ok(element * u32::from(*lanes).next_power_of_two())
        }
        Type::Struct(structure) => Ok(structure.size),
        Type::Array { element, count } => type_stride(element)?
            .checked_mul(*count)
            .ok_or_else(|| emission("array stride overflows")),
        _ => Err(emission("buffer element type has no SPIR-V array stride")),
    }
}

fn build_error(error: rspirv::dr::Error) -> TranslateError {
    emission(format!("SPIR-V builder rejected the module: {error}"))
}

fn emission(message: impl Into<String>) -> TranslateError {
    TranslateError::one(Diagnostic::new(Phase::SpirvEmission, message))
}

/// The SPIR-V memory semantics Metal's `mem_flags` name.
fn memory_semantics(memory: BarrierMemory) -> u32 {
    let mut bits = 0;
    if memory.device {
        bits |= MemorySemantics::UNIFORM_MEMORY.bits();
    }
    if memory.threadgroup {
        bits |= MemorySemantics::WORKGROUP_MEMORY.bits();
    }
    if memory.texture {
        bits |= MemorySemantics::IMAGE_MEMORY.bits();
    }
    bits
}
