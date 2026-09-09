//! SPIR-V emission from Metal IR.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use rspirv::binary::Assemble;
use rspirv::dr::{Builder, InsertPoint, Operand as SpirvOperand};
use rspirv::spirv::{
    AddressingModel, BuiltIn, Capability, Decoration, Dim, ExecutionMode, ExecutionModel,
    FunctionControl, GlslStd450Op, ImageFormat, ImageOperands, LoopControl, MemoryModel,
    SelectionControl, StorageClass,
};

use crate::ir::{
    Access, BlockId, Builtin, ConvertOp, EntryPoint, FloatBinaryOp, FloatPredicate, FloatUnaryOp,
    Function, InstructionKind, IntegerBinaryOp, IntegerPredicate, IntegerUnaryOp, Module, Operand,
    ResourceKind, Sampling, Stage, StageBinding, Terminator, TextureDimension, Type, ValueId,
    coordinate_lanes, instruction_operands, is_boolean_or_boolean_vector, operand_type,
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
}

impl TargetProfile {
    pub fn vulkan_1_3() -> Self {
        Self {
            vulkan_version: (1, 3),
            spirv_version: (1, 6),
            max_bound_descriptor_sets: 4,
            shader: true,
            shader_int64: false,
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
}

pub fn emit_spirv(
    module: &Module,
    target: &TargetProfile,
) -> Result<TranslatedLibrary, TranslateError> {
    module.verify()?;
    validate_target(target)?;
    let entries = module
        .entries
        .iter()
        .map(|entry| emit_entry(module, entry, target))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(TranslatedLibrary { entries })
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
    normalize_loop_headers(&mut function)?;
    separate_loop_latches(&mut function)?;
    split_shared_merges(&mut function)?;
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
    let capabilities = if emitter.uses_int64 {
        vec!["Shader".to_string(), "Int64".to_string()]
    } else {
        vec!["Shader".to_string()]
    };
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
            extensions: Vec::new(),
        },
    })
}

struct Emitter {
    shader_int64: bool,
    spirv_version: (u8, u8),
    uses_int64: bool,
    builder: Builder,
    types: HashMap<Type, u32>,
    constants: HashMap<(Type, u64), u32>,
    composites: HashMap<Operand, u32>,
    buffer_types: HashMap<Type, (u32, u32)>,
    interface: Vec<u32>,
    parameter_pointers: HashMap<ValueId, u32>,
    /// Each module global's variable and the storage class it lives in.
    globals: HashMap<u32, (u32, StorageClass)>,
    /// The storage class of every pointer an access chain produced, so a
    /// chained access chain keeps the class of the variable it started from.
    pointer_storage: HashMap<ValueId, StorageClass>,
    /// Array and structure types outside a buffer. Vulkan forbids explicit
    /// layout decorations there, so those types cannot share the ids the
    /// laid-out buffer types use.
    unlaid_types: HashMap<Type, u32>,
    unlaid_composites: HashMap<Operand, u32>,
    undefs: HashMap<Type, u32>,
    global_invocation_id: Option<u32>,
    glsl_std_450: u32,
    stage_inputs: HashMap<ValueId, u32>,
    /// Output variables in the order of the entry point's output list; each
    /// receives one component of the returned value.
    stage_outputs: Vec<u32>,
    /// The Metal result type of the entry function, needed to decompose a
    /// returned structure into stage outputs.
    return_type: Option<Type>,
    /// Vulkan requires a fragment shader which writes depth to say so.
    writes_depth: bool,
}

impl Emitter {
    fn new(target: &TargetProfile) -> Self {
        let mut builder = Builder::new();
        builder.set_version(target.spirv_version.0, target.spirv_version.1);
        builder.capability(Capability::Shader);
        builder.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
        let glsl_std_450 = builder.ext_inst_import("GLSL.std.450");
        Self {
            shader_int64: target.shader_int64,
            spirv_version: target.spirv_version,
            uses_int64: false,
            builder,
            types: HashMap::new(),
            constants: HashMap::new(),
            composites: HashMap::new(),
            buffer_types: HashMap::new(),
            interface: Vec::new(),
            parameter_pointers: HashMap::new(),
            globals: HashMap::new(),
            pointer_storage: HashMap::new(),
            unlaid_types: HashMap::new(),
            unlaid_composites: HashMap::new(),
            undefs: HashMap::new(),
            global_invocation_id: None,
            glsl_std_450,
            stage_inputs: HashMap::new(),
            stage_outputs: Vec::new(),
            return_type: None,
            writes_depth: false,
        }
    }

    fn declare_resources(
        &mut self,
        entry: &EntryPoint,
        function: &Function,
    ) -> Result<Vec<ResourceBinding>, TranslateError> {
        let mut reflected = Vec::new();
        for resource in &entry.resources {
            let parameter = function
                .parameters
                .get(resource.argument as usize)
                .ok_or_else(|| emission("resource refers to a missing function parameter"))?;
            let (variable, storage_class) = match (&resource.kind, &parameter.ty) {
                (ResourceKind::Buffer, Type::Pointer { pointee, .. }) => {
                    let (_block_type, pointer_type) = self.buffer_type(pointee)?;
                    (
                        self.builder.variable(
                            pointer_type,
                            None,
                            StorageClass::StorageBuffer,
                            None,
                        ),
                        StorageClass::StorageBuffer,
                    )
                }
                (ResourceKind::Texture, resource @ Type::Texture(_))
                | (ResourceKind::Sampler, resource @ Type::Sampler) => {
                    let image = self.type_id(resource)?;
                    let pointer =
                        self.builder
                            .type_pointer(None, StorageClass::UniformConstant, image);
                    (
                        self.builder
                            .variable(pointer, None, StorageClass::UniformConstant, None),
                        StorageClass::UniformConstant,
                    )
                }
                (ResourceKind::Buffer, _) => {
                    return Err(emission("buffer resource parameter is not a pointer"));
                }
                (ResourceKind::Texture, _) => {
                    return Err(emission("texture resource parameter is not a texture"));
                }
                (ResourceKind::Sampler, _) => {
                    return Err(emission("sampler resource parameter is not a sampler"));
                }
                _ => {
                    return Err(emission(
                        "resource kind is not implemented by the SPIR-V emitter",
                    ));
                }
            };
            let descriptor_set = descriptor_set(resource.kind);
            let binding = entry.stage.ordinal() * 256 + resource.index;
            self.builder.decorate(
                variable,
                Decoration::DescriptorSet,
                [SpirvOperand::LiteralBit32(descriptor_set)],
            );
            self.builder.decorate(
                variable,
                Decoration::Binding,
                [SpirvOperand::LiteralBit32(binding)],
            );
            match (storage_class, resource.access) {
                // SPIR-V only permits these decorations on memory objects.
                (StorageClass::StorageBuffer, Access::Read) => self.builder.decorate(
                    variable,
                    Decoration::NonWritable,
                    std::iter::empty::<SpirvOperand>(),
                ),
                (StorageClass::StorageBuffer, Access::Write) => self.builder.decorate(
                    variable,
                    Decoration::NonReadable,
                    std::iter::empty::<SpirvOperand>(),
                ),
                _ => (),
            }
            self.interface.push(variable);
            // The parameter pointer is materialized in the entry block after the
            // function has begun. Temporarily retain the descriptor variable.
            self.parameter_pointers.insert(parameter.value, variable);
            reflected.push(ResourceBinding {
                stage: entry.stage,
                kind: resource.kind,
                metal_index: resource.index,
                descriptor_set,
                binding,
                count: resource.count,
                access: resource.access,
            });
        }
        Ok(reflected)
    }

    /// Declare `gl_WorkGroupSize` as a specialization constant vector.
    ///
    /// The decoration supersedes the `LocalSize` execution mode, which stays
    /// in the module because a `GLCompute` entry point must declare one.
    fn declare_workgroup_size(&mut self) -> Result<(), TranslateError> {
        let uint = self.type_id(&Type::Int {
            bits: 32,
            signed: false,
        })?;
        let vector = self.builder.type_vector(uint, 3);
        let lanes = WORKGROUP_SIZE_SPEC_IDS.map(|spec_id| {
            let lane = self.builder.spec_constant_bit32(uint, 1);
            self.builder.decorate(
                lane,
                Decoration::SpecId,
                [SpirvOperand::LiteralBit32(spec_id)],
            );
            lane
        });
        let size = self.builder.spec_constant_composite(vector, lanes);
        self.builder.decorate(
            size,
            Decoration::BuiltIn,
            [SpirvOperand::BuiltIn(BuiltIn::WorkgroupSize)],
        );
        Ok(())
    }

    fn buffer_type(&mut self, pointee: &Type) -> Result<(u32, u32), TranslateError> {
        if let Some(ids) = self.buffer_types.get(pointee) {
            return Ok(*ids);
        }
        let element_type = self.type_id(pointee)?;
        let array_type = self.builder.type_runtime_array(element_type);
        self.builder.decorate(
            array_type,
            Decoration::ArrayStride,
            [SpirvOperand::LiteralBit32(type_stride(pointee)?)],
        );
        let block_type = self.builder.id();
        self.builder.type_struct_id(Some(block_type), [array_type]);
        self.builder.decorate(
            block_type,
            Decoration::Block,
            std::iter::empty::<SpirvOperand>(),
        );
        self.builder.member_decorate(
            block_type,
            0,
            Decoration::Offset,
            [SpirvOperand::LiteralBit32(0)],
        );
        let pointer_type = self
            .builder
            .type_pointer(None, StorageClass::StorageBuffer, block_type);
        self.buffer_types
            .insert(pointee.clone(), (block_type, pointer_type));
        Ok((block_type, pointer_type))
    }

    /// Declare Vulkan input and output variables for the entry point's Metal
    /// stage interface. Every binding comes from the entry point; nothing is
    /// inferred from parameter names.
    fn declare_stage_io(
        &mut self,
        entry: &EntryPoint,
        function: &Function,
    ) -> Result<(), TranslateError> {
        for input in &entry.inputs {
            let parameter = function
                .parameters
                .get(input.argument as usize)
                .ok_or_else(|| emission("stage input names a missing argument"))?;
            let ty = self.type_id(&parameter.ty)?;
            let pointer = self.builder.type_pointer(None, StorageClass::Input, ty);
            let variable = self
                .builder
                .variable(pointer, None, StorageClass::Input, None);
            self.decorate_binding(variable, entry.stage, input.binding)?;
            self.interface.push(variable);
            self.stage_inputs.insert(parameter.value, variable);
            // A kernel reaching the thread position through both a parameter
            // and the built-in instruction must share one Vulkan variable.
            if input.binding == StageBinding::Builtin(Builtin::ThreadPositionInGrid) {
                self.global_invocation_id = Some(variable);
            }
        }
        if !entry.outputs.is_empty() {
            self.return_type = Some(function.result.clone());
        }
        for (position, output) in entry.outputs.iter().enumerate() {
            let ty = output_type(function, output, position)?;
            let ty_id = self.type_id(&ty)?;
            let pointer = self.builder.type_pointer(None, StorageClass::Output, ty_id);
            let variable = self
                .builder
                .variable(pointer, None, StorageClass::Output, None);
            self.decorate_binding(variable, entry.stage, output.binding)?;
            self.interface.push(variable);
            self.stage_outputs.push(variable);
        }
        Ok(())
    }

    /// Attach the Vulkan decoration for one Metal stage binding.
    fn decorate_binding(
        &mut self,
        variable: u32,
        stage: Stage,
        binding: StageBinding,
    ) -> Result<(), TranslateError> {
        match binding {
            StageBinding::Location {
                location,
                interpolation,
            } => {
                self.builder.decorate(
                    variable,
                    Decoration::Location,
                    [SpirvOperand::LiteralBit32(location)],
                );
                if let Some(interpolation) = interpolation {
                    if interpolation.flat {
                        self.builder.decorate(variable, Decoration::Flat, []);
                    } else if !interpolation.perspective {
                        self.builder
                            .decorate(variable, Decoration::NoPerspective, []);
                    }
                    match interpolation.sampling {
                        Sampling::Center => {}
                        Sampling::Centroid => {
                            self.builder.decorate(variable, Decoration::Centroid, []);
                        }
                        Sampling::Sample => {
                            self.builder.capability(Capability::SampleRateShading);
                            self.builder.decorate(variable, Decoration::Sample, []);
                        }
                    }
                }
            }
            StageBinding::RenderTarget { index } => {
                self.builder.decorate(
                    variable,
                    Decoration::Location,
                    [SpirvOperand::LiteralBit32(index)],
                );
            }
            StageBinding::Builtin(builtin) => {
                let builtin = self.spirv_builtin(builtin, stage)?;
                self.builder.decorate(
                    variable,
                    Decoration::BuiltIn,
                    [SpirvOperand::BuiltIn(builtin)],
                );
            }
        }
        Ok(())
    }

    /// Metal built-ins name a semantic; the SPIR-V built-in depends on the
    /// stage and on whether it is read or written. `[[position]]` is Vulkan's
    /// `Position` output in a vertex shader and `FragCoord` input in a
    /// fragment shader.
    fn spirv_builtin(&mut self, builtin: Builtin, stage: Stage) -> Result<BuiltIn, TranslateError> {
        Ok(match (builtin, stage) {
            (Builtin::Position, Stage::Fragment) => BuiltIn::FragCoord,
            (Builtin::Position, _) => BuiltIn::Position,
            (Builtin::PointSize, _) => BuiltIn::PointSize,
            (Builtin::ClipDistance, _) => {
                self.builder.capability(Capability::ClipDistance);
                BuiltIn::ClipDistance
            }
            (Builtin::VertexId, _) => BuiltIn::VertexIndex,
            (Builtin::InstanceId, _) => BuiltIn::InstanceIndex,
            (Builtin::BaseVertex, _) => {
                self.builder.capability(Capability::DrawParameters);
                BuiltIn::BaseVertex
            }
            (Builtin::BaseInstance, _) => {
                self.builder.capability(Capability::DrawParameters);
                BuiltIn::BaseInstance
            }
            (Builtin::FrontFacing, _) => BuiltIn::FrontFacing,
            (Builtin::PointCoord, _) => BuiltIn::PointCoord,
            (Builtin::SampleId, _) => {
                self.builder.capability(Capability::SampleRateShading);
                BuiltIn::SampleId
            }
            (Builtin::SampleMask, _) => BuiltIn::SampleMask,
            (Builtin::Depth, _) => {
                self.writes_depth = true;
                BuiltIn::FragDepth
            }
            (Builtin::RenderTargetArrayIndex, _) => {
                self.builder.capability(Capability::Geometry);
                BuiltIn::Layer
            }
            (Builtin::ViewportArrayIndex, _) => {
                self.builder.capability(Capability::MultiViewport);
                BuiltIn::ViewportIndex
            }
            (Builtin::ThreadPositionInGrid, _) => BuiltIn::GlobalInvocationId,
            (Builtin::ThreadPositionInThreadgroup, _) => BuiltIn::LocalInvocationId,
            (Builtin::ThreadgroupPositionInGrid, _) => BuiltIn::WorkgroupId,
            (Builtin::ThreadgroupsPerGrid, _) => BuiltIn::NumWorkgroups,
            (Builtin::ThreadIndexInThreadgroup, _) => BuiltIn::LocalInvocationIndex,
            (Builtin::ThreadsPerThreadgroup, _) => BuiltIn::WorkgroupSize,
        })
    }

    fn emit_function(
        &mut self,
        entry: &EntryPoint,
        function: &Function,
    ) -> Result<u32, TranslateError> {
        self.collect_constants(function)?;
        let merges = control_flow_merges(function)?;
        if entry.stage == Stage::Kernel {
            self.declare_global_invocation_id()?;
        }
        let void = self.type_id(&Type::Void)?;
        let function_type = self.builder.type_function(void, []);
        let function_id = self.builder.id();
        let block_ids: HashMap<_, _> = function
            .blocks
            .iter()
            .map(|block| (block.id, self.builder.id()))
            .collect();
        let value_ids: HashMap<_, _> = function
            .values
            .iter()
            .enumerate()
            .map(|(index, _)| (ValueId(index as u32), self.builder.id()))
            .collect();
        self.builder
            .begin_function(
                void,
                Some(function_id),
                FunctionControl::NONE,
                function_type,
            )
            .map_err(build_error)?;

        for (block_index, block) in function.blocks.iter().enumerate() {
            self.builder
                .begin_block(Some(block_ids[&block.id]))
                .map_err(build_error)?;
            if block_index == 0 {
                self.materialize_local_variables(function, &value_ids)?;
                self.materialize_parameter_pointers(function, &value_ids)?;
                self.materialize_stage_inputs(function, &value_ids)?;
            }
            for instruction in &block.instructions {
                self.emit_instruction(function, instruction, &block_ids, &value_ids)?;
            }
            match merges.get(&block.id) {
                Some(Merge::Loop {
                    merge,
                    continue_target,
                }) => self
                    .builder
                    .loop_merge(
                        block_ids[merge],
                        block_ids[continue_target],
                        LoopControl::NONE,
                        std::iter::empty::<SpirvOperand>(),
                    )
                    .map_err(build_error)?,
                Some(Merge::Selection { merge }) => self
                    .builder
                    .selection_merge(block_ids[merge], SelectionControl::NONE)
                    .map_err(build_error)?,
                None => {}
            }
            self.emit_terminator(&block.terminator, &block_ids, &value_ids)?;
        }
        self.builder.end_function().map_err(build_error)?;
        Ok(function_id)
    }

    /// Declare every `alloca` in the entry block.
    ///
    /// SPIR-V requires a function's `Function`-storage variables to be in its
    /// first block, whatever block LLVM wrote the `alloca` in.
    fn materialize_local_variables(
        &mut self,
        function: &Function,
        values: &HashMap<ValueId, u32>,
    ) -> Result<(), TranslateError> {
        for block in &function.blocks {
            for instruction in &block.instructions {
                let InstructionKind::LocalVariable { ty } = &instruction.kind else {
                    continue;
                };
                let result = instruction
                    .result
                    .ok_or_else(|| emission("a local variable has no result"))?;
                let pointee = self.unlaid_type_id(ty)?;
                let pointer = self
                    .builder
                    .type_pointer(None, StorageClass::Function, pointee);
                self.builder
                    .variable(pointer, Some(values[&result]), StorageClass::Function, None);
                self.pointer_storage.insert(result, StorageClass::Function);
            }
        }
        Ok(())
    }

    fn materialize_parameter_pointers(
        &mut self,
        function: &Function,
        values: &HashMap<ValueId, u32>,
    ) -> Result<(), TranslateError> {
        let zero = self.constant(&Type::u32(), 0)?;
        for parameter in &function.parameters {
            let Some(variable) = self.parameter_pointers.get(&parameter.value).copied() else {
                continue;
            };
            // A texture or sampler is a handle, not addressable memory: it
            // is loaded from its descriptor rather than indexed into.
            if matches!(parameter.ty, Type::Texture(_) | Type::Sampler) {
                let handle = self.type_id(&parameter.ty)?;
                self.builder
                    .load(handle, Some(values[&parameter.value]), variable, None, [])
                    .map_err(build_error)?;
                continue;
            }
            let Type::Pointer { pointee, .. } = &parameter.ty else {
                return Err(emission("resource parameter is not a pointer"));
            };
            let pointee_id = self.type_id(pointee)?;
            let pointer_type =
                self.builder
                    .type_pointer(None, StorageClass::StorageBuffer, pointee_id);
            self.builder
                .access_chain(
                    pointer_type,
                    Some(values[&parameter.value]),
                    variable,
                    [zero, zero],
                )
                .map_err(build_error)?;
        }
        Ok(())
    }

    fn materialize_stage_inputs(
        &mut self,
        function: &Function,
        values: &HashMap<ValueId, u32>,
    ) -> Result<(), TranslateError> {
        for parameter in &function.parameters {
            let Some(variable) = self.stage_inputs.get(&parameter.value).copied() else {
                continue;
            };
            let ty = self.type_id(&parameter.ty)?;
            self.builder
                .load(ty, Some(values[&parameter.value]), variable, None, [])
                .map_err(build_error)?;
        }
        Ok(())
    }

    fn declare_global_invocation_id(&mut self) -> Result<(), TranslateError> {
        if self.global_invocation_id.is_some() {
            return Ok(());
        }
        let vector = self.type_id(&Type::Vector {
            element: Box::new(Type::u32()),
            lanes: 3,
        })?;
        let pointer = self.builder.type_pointer(None, StorageClass::Input, vector);
        let variable = self
            .builder
            .variable(pointer, None, StorageClass::Input, None);
        self.builder.decorate(
            variable,
            Decoration::BuiltIn,
            [SpirvOperand::BuiltIn(BuiltIn::GlobalInvocationId)],
        );
        self.interface.push(variable);
        self.global_invocation_id = Some(variable);
        Ok(())
    }

    /// SPIR-V declares constants at module scope, so they must be created
    /// before the function body which uses them is emitted.
    fn collect_constants(&mut self, function: &Function) -> Result<(), TranslateError> {
        for block in &function.blocks {
            for instruction in &block.instructions {
                for operand in instruction_operands(&instruction.kind) {
                    self.declare_constant(operand)?;
                }
            }
            for operand in terminator_operands(&block.terminator) {
                self.declare_constant(operand)?;
            }
        }
        Ok(())
    }

    fn declare_constant(&mut self, operand: &Operand) -> Result<(), TranslateError> {
        match operand {
            Operand::Integer { ty, value } => {
                self.constant(ty, *value)?;
            }
            Operand::Float { ty, bits } => {
                self.constant(ty, *bits)?;
            }
            Operand::Composite { elements, .. } => {
                for element in elements {
                    self.declare_constant(element)?;
                }
                self.composite_constant(operand)?;
            }
            Operand::Undef(ty) => {
                self.undef(ty)?;
            }
            Operand::Value(_) | Operand::Bool(_) | Operand::Global { .. } => {}
        }
        Ok(())
    }

    /// An `OpUndef` of one type, declared once at module scope.
    ///
    /// SPIR-V allows `OpUndef` inside a block, but a phi must be the first
    /// instruction of its block, and an undefined phi operand would otherwise
    /// be created between the phis it feeds.
    fn undef(&mut self, ty: &Type) -> Result<u32, TranslateError> {
        if let Some(id) = self.undefs.get(ty) {
            return Ok(*id);
        }
        let ty_id = self.type_id(ty)?;
        let id = self.builder.id();
        self.builder
            .module_mut()
            .types_global_values
            .push(rspirv::dr::Instruction::new(
                rspirv::spirv::Op::Undef,
                Some(ty_id),
                Some(id),
                Vec::new(),
            ));
        self.undefs.insert(ty.clone(), id);
        Ok(id)
    }

    fn composite_constant(&mut self, operand: &Operand) -> Result<u32, TranslateError> {
        let Operand::Composite { ty, elements } = operand else {
            return Err(emission("composite constant has no elements"));
        };
        if let Some(id) = self.composites.get(operand) {
            return Ok(*id);
        }
        let ty_id = self.type_id(ty)?;
        let members = elements
            .iter()
            .map(|element| match element {
                Operand::Composite { .. } => self.composite_constant(element),
                Operand::Integer { ty, value } => self.constant(ty, *value),
                Operand::Float { ty, bits } => self.constant(ty, *bits),
                Operand::Bool(value) => {
                    let ty = self.type_id(&Type::Bool)?;
                    Ok(if *value {
                        self.builder.constant_true(ty)
                    } else {
                        self.builder.constant_false(ty)
                    })
                }
                Operand::Undef(ty) => self.undef(ty),
                Operand::Global { .. } => {
                    Err(emission("a global variable is not a constant element"))
                }
                Operand::Value(_) => Err(emission("composite constant has a non-constant element")),
            })
            .collect::<Result<Vec<_>, _>>()?;
        let id = self.builder.constant_composite(ty_id, members);
        self.composites.insert(operand.clone(), id);
        Ok(id)
    }

    fn emit_instruction(
        &mut self,
        function: &Function,
        instruction: &crate::ir::Instruction,
        blocks: &HashMap<BlockId, u32>,
        values: &HashMap<ValueId, u32>,
    ) -> Result<(), TranslateError> {
        let result_id = instruction.result.map(|result| values[&result]);
        match &instruction.kind {
            // Declared in the entry block by `materialize_local_variables`.
            InstructionKind::LocalVariable { .. } => {}
            InstructionKind::GlobalInvocationId => {
                let result = instruction
                    .result
                    .ok_or_else(|| emission("global invocation ID has no result"))?;
                let ty = self.type_id(&function.values[result.0 as usize])?;
                self.builder
                    .load(
                        ty,
                        result_id,
                        self.global_invocation_id
                            .ok_or_else(|| emission("global invocation ID was not declared"))?,
                        None,
                        [],
                    )
                    .map_err(build_error)?;
            }
            InstructionKind::Load { pointer } => {
                let result = instruction
                    .result
                    .ok_or_else(|| emission("load has no result"))?;
                let ty = self.type_id(&function.values[result.0 as usize])?;
                let pointer = self.operand(pointer, values)?;
                self.builder
                    .load(ty, result_id, pointer, None, [])
                    .map_err(build_error)?;
            }
            InstructionKind::Store { pointer, value } => {
                let pointer = self.operand(pointer, values)?;
                let value = self.operand(value, values)?;
                self.builder
                    .store(pointer, value, None, [])
                    .map_err(build_error)?;
            }
            InstructionKind::IntegerBinary {
                op:
                    op @ (IntegerBinaryOp::MinimumSigned
                    | IntegerBinaryOp::MinimumUnsigned
                    | IntegerBinaryOp::MaximumSigned
                    | IntegerBinaryOp::MaximumUnsigned),
                lhs,
                rhs,
            } => {
                let operation = match op {
                    IntegerBinaryOp::MinimumSigned => GlslStd450Op::SMin,
                    IntegerBinaryOp::MinimumUnsigned => GlslStd450Op::UMin,
                    IntegerBinaryOp::MaximumSigned => GlslStd450Op::SMax,
                    _ => GlslStd450Op::UMax,
                };
                self.emit_ext_inst(function, instruction, values, operation, [lhs, rhs])?;
            }
            InstructionKind::IntegerClamp {
                signed,
                value,
                min,
                max,
            } => {
                let operation = if *signed {
                    GlslStd450Op::SClamp
                } else {
                    GlslStd450Op::UClamp
                };
                self.emit_ext_inst(function, instruction, values, operation, [value, min, max])?;
            }
            InstructionKind::IntegerUnary { op, value } => match op {
                IntegerUnaryOp::AbsoluteSigned => {
                    self.emit_ext_inst(function, instruction, values, GlslStd450Op::SAbs, [value])?;
                }
                IntegerUnaryOp::SignSigned => {
                    self.emit_ext_inst(
                        function,
                        instruction,
                        values,
                        GlslStd450Op::SSign,
                        [value],
                    )?;
                }
                IntegerUnaryOp::PopulationCount | IntegerUnaryOp::Not => {
                    let result = instruction
                        .result
                        .ok_or_else(|| emission("integer math call has no result"))?;
                    let ty = self.type_id(&function.values[result.0 as usize])?;
                    let value = self.operand(value, values)?;
                    if *op == IntegerUnaryOp::PopulationCount {
                        self.builder.bit_count(ty, result_id, value)
                    } else {
                        self.builder.not(ty, result_id, value)
                    }
                    .map_err(build_error)?;
                }
            },
            // LLVM types Metal's `bool` as `i1`, so a bitwise operation on
            // booleans stands in for a logical one; SPIR-V keeps them apart.
            InstructionKind::IntegerBinary { op, lhs, rhs }
                if instruction
                    .result
                    .and_then(|result| function.values.get(result.0 as usize))
                    .is_some_and(is_boolean_or_boolean_vector) =>
            {
                self.emit_binary(
                    function,
                    instruction,
                    lhs,
                    rhs,
                    values,
                    |b, ty, id, x, y| {
                        match op {
                            IntegerBinaryOp::And => b.logical_and(ty, id, x, y),
                            IntegerBinaryOp::Or => b.logical_or(ty, id, x, y),
                            // Exclusive or on booleans is inequality.
                            _ => b.logical_not_equal(ty, id, x, y),
                        }
                    },
                )?;
            }
            InstructionKind::IntegerBinary { op, lhs, rhs } => {
                self.emit_binary(
                    function,
                    instruction,
                    lhs,
                    rhs,
                    values,
                    |b, ty, id, x, y| match op {
                        IntegerBinaryOp::Add => b.i_add(ty, id, x, y),
                        IntegerBinaryOp::Subtract => b.i_sub(ty, id, x, y),
                        IntegerBinaryOp::Multiply => b.i_mul(ty, id, x, y),
                        IntegerBinaryOp::DivideUnsigned => b.u_div(ty, id, x, y),
                        IntegerBinaryOp::DivideSigned => b.s_div(ty, id, x, y),
                        IntegerBinaryOp::RemainderUnsigned => b.u_mod(ty, id, x, y),
                        IntegerBinaryOp::RemainderSigned => b.s_rem(ty, id, x, y),
                        IntegerBinaryOp::ShiftLeft => b.shift_left_logical(ty, id, x, y),
                        IntegerBinaryOp::ShiftRightLogical => b.shift_right_logical(ty, id, x, y),
                        IntegerBinaryOp::ShiftRightArithmetic => {
                            b.shift_right_arithmetic(ty, id, x, y)
                        }
                        IntegerBinaryOp::And => b.bitwise_and(ty, id, x, y),
                        IntegerBinaryOp::Or => b.bitwise_or(ty, id, x, y),
                        IntegerBinaryOp::Xor => b.bitwise_xor(ty, id, x, y),
                        // Minimum and maximum are extended instructions,
                        // matched by the arm above.
                        _ => unreachable!(),
                    },
                )?;
            }
            InstructionKind::IntegerCompare {
                predicate,
                lhs,
                rhs,
            } => {
                self.emit_binary(
                    function,
                    instruction,
                    lhs,
                    rhs,
                    values,
                    |b, ty, id, x, y| match predicate {
                        IntegerPredicate::Equal => b.i_equal(ty, id, x, y),
                        IntegerPredicate::NotEqual => b.i_not_equal(ty, id, x, y),
                        IntegerPredicate::UnsignedLessThan => b.u_less_than(ty, id, x, y),
                        IntegerPredicate::UnsignedLessEqual => b.u_less_than_equal(ty, id, x, y),
                        IntegerPredicate::UnsignedGreaterThan => b.u_greater_than(ty, id, x, y),
                        IntegerPredicate::UnsignedGreaterEqual => {
                            b.u_greater_than_equal(ty, id, x, y)
                        }
                        IntegerPredicate::SignedLessThan => b.s_less_than(ty, id, x, y),
                        IntegerPredicate::SignedLessEqual => b.s_less_than_equal(ty, id, x, y),
                        IntegerPredicate::SignedGreaterThan => b.s_greater_than(ty, id, x, y),
                        IntegerPredicate::SignedGreaterEqual => {
                            b.s_greater_than_equal(ty, id, x, y)
                        }
                    },
                )?;
            }
            InstructionKind::FloatCompare {
                predicate,
                lhs,
                rhs,
            } => {
                self.emit_binary(
                    function,
                    instruction,
                    lhs,
                    rhs,
                    values,
                    |b, ty, id, x, y| match predicate {
                        FloatPredicate::OrderedEqual => b.f_ord_equal(ty, id, x, y),
                        FloatPredicate::OrderedNotEqual => b.f_ord_not_equal(ty, id, x, y),
                        FloatPredicate::OrderedLessThan => b.f_ord_less_than(ty, id, x, y),
                        FloatPredicate::OrderedLessEqual => b.f_ord_less_than_equal(ty, id, x, y),
                        FloatPredicate::OrderedGreaterThan => b.f_ord_greater_than(ty, id, x, y),
                        FloatPredicate::OrderedGreaterEqual => {
                            b.f_ord_greater_than_equal(ty, id, x, y)
                        }
                        FloatPredicate::Ordered => b.ordered(ty, id, x, y),
                        FloatPredicate::UnorderedEqual => b.f_unord_equal(ty, id, x, y),
                        FloatPredicate::UnorderedNotEqual => b.f_unord_not_equal(ty, id, x, y),
                        FloatPredicate::UnorderedLessThan => b.f_unord_less_than(ty, id, x, y),
                        FloatPredicate::UnorderedLessEqual => {
                            b.f_unord_less_than_equal(ty, id, x, y)
                        }
                        FloatPredicate::UnorderedGreaterThan => {
                            b.f_unord_greater_than(ty, id, x, y)
                        }
                        FloatPredicate::UnorderedGreaterEqual => {
                            b.f_unord_greater_than_equal(ty, id, x, y)
                        }
                        FloatPredicate::Unordered => b.unordered(ty, id, x, y),
                    },
                )?;
            }
            InstructionKind::Phi { incomings } => {
                let result = instruction
                    .result
                    .ok_or_else(|| emission("phi has no result"))?;
                let ty = self.type_id(&function.values[result.0 as usize])?;
                let incoming = incomings
                    .iter()
                    .map(|(block, operand)| Ok((self.operand(operand, values)?, blocks[block])))
                    .collect::<Result<Vec<_>, TranslateError>>()?;
                self.builder
                    .phi(ty, result_id, incoming)
                    .map_err(build_error)?;
            }
            InstructionKind::ElementAddress { pointer, index } => {
                let (variable, storage, member) = self.pointer_base(pointer, values)?;
                let result = instruction
                    .result
                    .ok_or_else(|| emission("element address has no result"))?;
                let Type::Pointer { pointee, .. } = &function.values[result.0 as usize] else {
                    return Err(emission("element address result is not a pointer"));
                };
                let pointee = match storage {
                    StorageClass::StorageBuffer => self.type_id(pointee)?,
                    _ => self.unlaid_type_id(pointee)?,
                };
                let pointer_type = self.builder.type_pointer(None, storage, pointee);
                let mut chain = Vec::new();
                if member {
                    chain.push(self.constant(&Type::u32(), 0)?);
                }
                chain.push(self.operand(index, values)?);
                self.builder
                    .access_chain(pointer_type, result_id, variable, chain)
                    .map_err(build_error)?;
                self.pointer_storage.insert(result, storage);
            }
            InstructionKind::BufferAddress { pointer, indices } => {
                let (variable, storage, member) = self.pointer_base(pointer, values)?;
                let result = instruction
                    .result
                    .ok_or_else(|| emission("buffer address has no result"))?;
                let Type::Pointer { pointee, .. } = &function.values[result.0 as usize] else {
                    return Err(emission("buffer address result is not a pointer"));
                };
                let pointee = match storage {
                    StorageClass::StorageBuffer => self.type_id(pointee)?,
                    _ => self.unlaid_type_id(pointee)?,
                };
                let ty = self.builder.type_pointer(None, storage, pointee);
                let mut chain = Vec::new();
                // LLVM's first GEP index steps the pointer itself. A buffer
                // resource is a block whose one member is the array being
                // stepped, so there the index selects that member's element;
                // a variable pointing straight at an aggregate has nothing to
                // step, and SPIR-V's first index already selects a member.
                let indices = if member {
                    chain.push(self.constant(&Type::u32(), 0)?);
                    indices.as_slice()
                } else {
                    match indices.split_first() {
                        Some((Operand::Integer { value: 0, .. }, rest)) => rest,
                        _ => {
                            return Err(emission(
                                "an access chain into a global variable steps the pointer",
                            ));
                        }
                    }
                };
                for index in indices {
                    chain.push(self.operand(index, values)?);
                }
                self.builder
                    .access_chain(ty, result_id, variable, chain)
                    .map_err(build_error)?;
                self.pointer_storage.insert(result, storage);
            }
            InstructionKind::FieldAddress { pointer, indices } => {
                let (variable, storage, member) = self.pointer_base(pointer, values)?;
                let result = instruction
                    .result
                    .ok_or_else(|| emission("field address has no result"))?;
                let Type::Pointer { pointee, .. } = &function.values[result.0 as usize] else {
                    return Err(emission("field address result is not a pointer"));
                };
                let pointee = match storage {
                    StorageClass::StorageBuffer => self.type_id(pointee)?,
                    _ => self.unlaid_type_id(pointee)?,
                };
                let pointer_type = self.builder.type_pointer(None, storage, pointee);
                let mut chain = Vec::new();
                if member {
                    // A buffer block holds one runtime array, so a field of
                    // its element is reached past the member and the element.
                    let zero = self.constant(&Type::u32(), 0)?;
                    chain.push(zero);
                    chain.push(zero);
                }
                for index in indices {
                    chain.push(self.constant(&Type::u32(), u64::from(*index))?);
                }
                self.builder
                    .access_chain(pointer_type, result_id, variable, chain)
                    .map_err(build_error)?;
                self.pointer_storage.insert(result, storage);
            }
            InstructionKind::CompositeExtract { composite, index } => {
                let result = instruction
                    .result
                    .ok_or_else(|| emission("composite extract has no result"))?;
                let ty = self.type_id(&function.values[result.0 as usize])?;
                let composite = self.operand(composite, values)?;
                self.builder
                    .composite_extract(ty, result_id, composite, [*index])
                    .map_err(build_error)?;
            }
            InstructionKind::All { vector } => {
                let result = instruction
                    .result
                    .ok_or_else(|| emission("all has no result"))?;
                let ty = self.type_id(&function.values[result.0 as usize])?;
                let vector = self.operand(vector, values)?;
                self.builder
                    .all(ty, result_id, vector)
                    .map_err(build_error)?;
            }
            InstructionKind::Any { vector } => {
                let result = instruction
                    .result
                    .ok_or_else(|| emission("any has no result"))?;
                let ty = self.type_id(&function.values[result.0 as usize])?;
                let vector = self.operand(vector, values)?;
                self.builder
                    .any(ty, result_id, vector)
                    .map_err(build_error)?;
            }
            // SPIR-V keeps booleans out of the numeric type system, so a
            // conversion between `i1` and an integer becomes a selection or a
            // comparison rather than a width change.
            InstructionKind::Convert {
                op:
                    op @ (ConvertOp::ZeroExtend
                    | ConvertOp::SignExtend
                    | ConvertOp::UnsignedToFloat
                    | ConvertOp::SignedToFloat),
                value,
            } if operand_type(function, value) == Some(Type::Bool) => {
                let result = instruction
                    .result
                    .ok_or_else(|| emission("conversion has no result"))?;
                let result_type = function.values[result.0 as usize].clone();
                let ty = self.type_id(&result_type)?;
                // A sign extension of a set bit is all ones, that is -1;
                // a widening or a conversion to floating point yields one.
                let set = match (op, &result_type) {
                    (ConvertOp::SignExtend, Type::Int { .. }) => u64::MAX,
                    (_, Type::Float { bits: 32 }) => u64::from(1.0f32.to_bits()),
                    _ => 1,
                };
                let one = self.constant(&result_type, set)?;
                let zero = self.constant(&result_type, 0)?;
                let value = self.operand(value, values)?;
                self.builder
                    .select(ty, result_id, value, one, zero)
                    .map_err(build_error)?;
            }
            InstructionKind::Convert {
                op: ConvertOp::IntegerTruncate,
                value,
            } if function
                .values
                .get(
                    instruction
                        .result
                        .ok_or_else(|| emission("conversion has no result"))?
                        .0 as usize,
                )
                .is_some_and(|ty| *ty == Type::Bool) =>
            {
                let ty = self.type_id(&Type::Bool)?;
                let source = operand_type(function, value)
                    .ok_or_else(|| emission("conversion operand has no type"))?;
                // Narrowing to `i1` keeps the low bit.
                let one = self.constant(&source, 1)?;
                let zero = self.constant(&source, 0)?;
                let value = self.operand(value, values)?;
                let source_id = self.type_id(&source)?;
                let masked = self
                    .builder
                    .bitwise_and(source_id, None, value, one)
                    .map_err(build_error)?;
                self.builder
                    .i_not_equal(ty, result_id, masked, zero)
                    .map_err(build_error)?;
            }
            InstructionKind::Convert { op, value } => {
                let result = instruction
                    .result
                    .ok_or_else(|| emission("conversion has no result"))?;
                let ty = self.type_id(&function.values[result.0 as usize])?;
                let value = self.operand(value, values)?;
                let b = &mut self.builder;
                match op {
                    ConvertOp::UnsignedToFloat => b.convert_u_to_f(ty, result_id, value),
                    ConvertOp::SignedToFloat => b.convert_s_to_f(ty, result_id, value),
                    ConvertOp::FloatToUnsigned => b.convert_f_to_u(ty, result_id, value),
                    ConvertOp::FloatToSigned => b.convert_f_to_s(ty, result_id, value),
                    ConvertOp::ZeroExtend => b.u_convert(ty, result_id, value),
                    ConvertOp::SignExtend => b.s_convert(ty, result_id, value),
                    // SPIR-V narrows integers with the same width-changing
                    // conversion; the result width discards the high bits.
                    ConvertOp::IntegerTruncate => b.u_convert(ty, result_id, value),
                    ConvertOp::FloatResize => b.f_convert(ty, result_id, value),
                    ConvertOp::Bitcast => b.bitcast(ty, result_id, value),
                }
                .map_err(build_error)?;
            }
            InstructionKind::FloatBinary { op, lhs, rhs } => match op {
                FloatBinaryOp::Add
                | FloatBinaryOp::Subtract
                | FloatBinaryOp::Multiply
                | FloatBinaryOp::Divide
                | FloatBinaryOp::Remainder => {
                    self.emit_binary(
                        function,
                        instruction,
                        lhs,
                        rhs,
                        values,
                        |b, ty, id, x, y| match op {
                            FloatBinaryOp::Add => b.f_add(ty, id, x, y),
                            FloatBinaryOp::Subtract => b.f_sub(ty, id, x, y),
                            FloatBinaryOp::Multiply => b.f_mul(ty, id, x, y),
                            FloatBinaryOp::Divide => b.f_div(ty, id, x, y),
                            _ => b.f_rem(ty, id, x, y),
                        },
                    )?;
                }
                FloatBinaryOp::Minimum => {
                    self.emit_ext_inst(
                        function,
                        instruction,
                        values,
                        GlslStd450Op::FMin,
                        [lhs, rhs],
                    )?;
                }
                FloatBinaryOp::Maximum => {
                    self.emit_ext_inst(
                        function,
                        instruction,
                        values,
                        GlslStd450Op::FMax,
                        [lhs, rhs],
                    )?;
                }
                FloatBinaryOp::Power => {
                    self.emit_ext_inst(
                        function,
                        instruction,
                        values,
                        GlslStd450Op::Pow,
                        [lhs, rhs],
                    )?;
                }
                FloatBinaryOp::Atan2 => {
                    self.emit_ext_inst(
                        function,
                        instruction,
                        values,
                        GlslStd450Op::Atan2,
                        [lhs, rhs],
                    )?;
                }
                FloatBinaryOp::Step => {
                    self.emit_ext_inst(
                        function,
                        instruction,
                        values,
                        GlslStd450Op::Step,
                        [lhs, rhs],
                    )?;
                }
            },
            InstructionKind::FloatUnary { op, value } => match op {
                FloatUnaryOp::Negate => {
                    let result = instruction
                        .result
                        .ok_or_else(|| emission("negation has no result"))?;
                    let ty = self.type_id(&function.values[result.0 as usize])?;
                    let value = self.operand(value, values)?;
                    self.builder
                        .f_negate(ty, result_id, value)
                        .map_err(build_error)?;
                }
                FloatUnaryOp::DerivativeX
                | FloatUnaryOp::DerivativeY
                | FloatUnaryOp::DerivativeWidth => {
                    let result = instruction
                        .result
                        .ok_or_else(|| emission("derivative has no result"))?;
                    let ty = self.type_id(&function.values[result.0 as usize])?;
                    let value = self.operand(value, values)?;
                    let b = &mut self.builder;
                    match op {
                        FloatUnaryOp::DerivativeX => b.d_pdx(ty, result_id, value),
                        FloatUnaryOp::DerivativeY => b.d_pdy(ty, result_id, value),
                        _ => b.fwidth(ty, result_id, value),
                    }
                    .map_err(build_error)?;
                }
                _ => {
                    let operation = match op {
                        FloatUnaryOp::Absolute => GlslStd450Op::FAbs,
                        FloatUnaryOp::Sign => GlslStd450Op::FSign,
                        FloatUnaryOp::Sqrt => GlslStd450Op::Sqrt,
                        FloatUnaryOp::InverseSqrt => GlslStd450Op::InverseSqrt,
                        FloatUnaryOp::Exp => GlslStd450Op::Exp,
                        FloatUnaryOp::Exp2 => GlslStd450Op::Exp2,
                        FloatUnaryOp::Log => GlslStd450Op::Log,
                        FloatUnaryOp::Log2 => GlslStd450Op::Log2,
                        FloatUnaryOp::Sin => GlslStd450Op::Sin,
                        FloatUnaryOp::Cos => GlslStd450Op::Cos,
                        FloatUnaryOp::Tan => GlslStd450Op::Tan,
                        FloatUnaryOp::Asin => GlslStd450Op::Asin,
                        FloatUnaryOp::Acos => GlslStd450Op::Acos,
                        FloatUnaryOp::Atan => GlslStd450Op::Atan,
                        FloatUnaryOp::Floor => GlslStd450Op::Floor,
                        FloatUnaryOp::Ceil => GlslStd450Op::Ceil,
                        FloatUnaryOp::Round => GlslStd450Op::RoundEven,
                        FloatUnaryOp::Truncate => GlslStd450Op::Trunc,
                        FloatUnaryOp::Fract => GlslStd450Op::Fract,
                        FloatUnaryOp::Negate
                        | FloatUnaryOp::DerivativeX
                        | FloatUnaryOp::DerivativeY
                        | FloatUnaryOp::DerivativeWidth => {
                            return Err(emission("unreachable floating unary lowering"));
                        }
                    };
                    self.emit_ext_inst(function, instruction, values, operation, [value])?;
                }
            },
            InstructionKind::FloatFma {
                first,
                second,
                addend,
            } => {
                self.emit_ext_inst(
                    function,
                    instruction,
                    values,
                    GlslStd450Op::Fma,
                    [first, second, addend],
                )?;
            }
            InstructionKind::FloatDot { lhs, rhs } => self.emit_binary(
                function,
                instruction,
                lhs,
                rhs,
                values,
                |builder, ty, id, a, b| builder.dot(ty, id, a, b),
            )?,
            InstructionKind::VectorShuffle {
                first,
                second,
                components,
            } => {
                let result = instruction
                    .result
                    .ok_or_else(|| emission("vector shuffle has no result"))?;
                let ty = self.type_id(&function.values[result.0 as usize])?;
                let first = self.operand(first, values)?;
                let second = self.operand(second, values)?;
                self.builder
                    .vector_shuffle(ty, result_id, first, second, components.iter().copied())
                    .map_err(build_error)?;
            }
            InstructionKind::CompositeInsert {
                composite,
                object,
                index,
            } => {
                let result = instruction
                    .result
                    .ok_or_else(|| emission("composite insert has no result"))?;
                let ty = self.type_id(&function.values[result.0 as usize])?;
                let composite = self.operand(composite, values)?;
                let object = self.operand(object, values)?;
                self.builder
                    .composite_insert(ty, result_id, object, composite, [*index])
                    .map_err(build_error)?;
            }
            InstructionKind::FloatClamp { value, min, max } => {
                self.emit_ext_inst(
                    function,
                    instruction,
                    values,
                    GlslStd450Op::FClamp,
                    [value, min, max],
                )?;
            }
            InstructionKind::FloatMix {
                first,
                second,
                weight,
            } => {
                self.emit_ext_inst(
                    function,
                    instruction,
                    values,
                    GlslStd450Op::FMix,
                    [first, second, weight],
                )?;
            }
            InstructionKind::Select {
                condition,
                on_true,
                on_false,
            } => {
                let result = instruction
                    .result
                    .ok_or_else(|| emission("select has no result"))?;
                let ty = self.type_id(&function.values[result.0 as usize])?;
                let condition = self.operand(condition, values)?;
                let on_true = self.operand(on_true, values)?;
                let on_false = self.operand(on_false, values)?;
                self.builder
                    .select(ty, result_id, condition, on_true, on_false)
                    .map_err(build_error)?;
            }
            InstructionKind::VectorSplat { value, lanes } => {
                let result = instruction
                    .result
                    .ok_or_else(|| emission("vector splat has no result"))?;
                let ty = self.type_id(&function.values[result.0 as usize])?;
                let value = self.operand(value, values)?;
                self.builder
                    .composite_construct(
                        ty,
                        result_id,
                        std::iter::repeat_n(value, usize::from(*lanes)),
                    )
                    .map_err(build_error)?;
            }
            InstructionKind::TextureWrite {
                texture,
                coordinate,
                array_index,
                value,
            } => {
                let image = self.operand(texture, values)?;
                let coordinate =
                    self.texture_coordinate(function, coordinate, array_index.as_ref(), values)?;
                let value = self.operand(value, values)?;
                self.builder
                    .image_write(image, coordinate, value, None, [])
                    .map_err(build_error)?;
            }
            InstructionKind::TextureSample {
                texture,
                sampler,
                coordinate,
                array_index,
                offset,
                lod,
                gather,
            } => {
                let result = instruction
                    .result
                    .ok_or_else(|| emission("texture sample has no result"))?;
                let ty = self.type_id(&function.values[result.0 as usize])?;
                let Some(Type::Texture(image_type)) = operand_type(function, texture) else {
                    return Err(emission("texture sample has no texture operand"));
                };
                let image_id = self.type_id(&Type::Texture(image_type))?;
                let sampled_type = self.builder.type_sampled_image(image_id);
                let image = self.operand(texture, values)?;
                let sampler = self.operand(sampler, values)?;
                let sampled = self
                    .builder
                    .sampled_image(sampled_type, None, image, sampler)
                    .map_err(build_error)?;
                let coordinate =
                    self.texture_coordinate(function, coordinate, array_index.as_ref(), values)?;
                let mut mask = ImageOperands::NONE;
                let mut extra = Vec::new();
                if let Some(lod) = lod {
                    mask |= ImageOperands::LOD;
                    extra.push(SpirvOperand::IdRef(self.operand(lod, values)?));
                }
                if let Some(offset) = offset {
                    // Metal requires a sampling offset to be a compile-time
                    // constant, which is also the only form Vulkan accepts
                    // outside a gather.
                    if !matches!(offset, Operand::Composite { .. } | Operand::Integer { .. }) {
                        return Err(emission(
                            "a sampling offset must be a constant to be emitted",
                        ));
                    }
                    mask |= if gather.is_some() {
                        self.builder.capability(Capability::ImageGatherExtended);
                        ImageOperands::OFFSET
                    } else {
                        ImageOperands::CONST_OFFSET
                    };
                    extra.push(SpirvOperand::IdRef(self.operand(offset, values)?));
                }
                match gather {
                    Some(component) => {
                        let component = self.constant(&Type::u32(), u64::from(*component))?;
                        self.builder
                            .image_gather(
                                ty,
                                result_id,
                                sampled,
                                coordinate,
                                component,
                                Some(mask),
                                extra,
                            )
                            .map_err(build_error)?;
                    }
                    // An implicit level of detail needs the fragment stage's
                    // derivatives; AIR requests an explicit level otherwise.
                    None if lod.is_some() => {
                        self.builder
                            .image_sample_explicit_lod(
                                ty, result_id, sampled, coordinate, mask, extra,
                            )
                            .map_err(build_error)?;
                    }
                    None => {
                        self.builder
                            .image_sample_implicit_lod(
                                ty,
                                result_id,
                                sampled,
                                coordinate,
                                Some(mask),
                                extra,
                            )
                            .map_err(build_error)?;
                    }
                }
            }
            InstructionKind::TextureRead {
                texture,
                coordinate,
                array_index,
                lod,
                sample,
            } => {
                let result = instruction
                    .result
                    .ok_or_else(|| emission("texture read has no result"))?;
                let ty = self.type_id(&function.values[result.0 as usize])?;
                let Some(Type::Texture(image_type)) = operand_type(function, texture) else {
                    return Err(emission("texture read has no texture operand"));
                };
                let image = self.operand(texture, values)?;
                let coordinate =
                    self.texture_coordinate(function, coordinate, array_index.as_ref(), values)?;
                let mut mask = ImageOperands::NONE;
                let mut extra = Vec::new();
                if let Some(lod) = lod {
                    mask |= ImageOperands::LOD;
                    extra.push(SpirvOperand::IdRef(self.operand(lod, values)?));
                }
                if let Some(sample) = sample {
                    mask |= ImageOperands::SAMPLE;
                    extra.push(SpirvOperand::IdRef(self.operand(sample, values)?));
                }
                // A sampled image is fetched; a storage image is read.
                if image_type.access == Access::Sample {
                    self.builder
                        .image_fetch(ty, result_id, image, coordinate, Some(mask), extra)
                        .map_err(build_error)?;
                } else {
                    self.builder
                        .image_read(ty, result_id, image, coordinate, Some(mask), extra)
                        .map_err(build_error)?;
                }
            }
            InstructionKind::TextureQuerySize {
                texture,
                lod,
                component,
            } => {
                let result = instruction
                    .result
                    .ok_or_else(|| emission("texture query has no result"))?;
                let ty = self.type_id(&function.values[result.0 as usize])?;
                let Some(Type::Texture(image_type)) = operand_type(function, texture) else {
                    return Err(emission("texture query has no texture operand"));
                };
                self.builder.capability(Capability::ImageQuery);
                let lanes = coordinate_lanes(&image_type) + u8::from(image_type.array);
                let size_type = self.type_id(&if lanes == 1 {
                    Type::u32()
                } else {
                    Type::Vector {
                        element: Box::new(Type::u32()),
                        lanes,
                    }
                })?;
                let image = self.operand(texture, values)?;
                let size = match lod {
                    Some(lod) => {
                        let lod = self.operand(lod, values)?;
                        self.builder
                            .image_query_size_lod(size_type, None, image, lod)
                            .map_err(build_error)?
                    }
                    None => self
                        .builder
                        .image_query_size(size_type, None, image)
                        .map_err(build_error)?,
                };
                if lanes == 1 {
                    self.builder
                        .copy_object(ty, result_id, size)
                        .map_err(build_error)?;
                } else {
                    self.builder
                        .composite_extract(ty, result_id, size, [*component])
                        .map_err(build_error)?;
                }
            }
            // `discard_fragment` keeps executing in Metal, so demote the
            // invocation rather than terminating the block with OpKill.
            InstructionKind::DiscardFragment => {
                self.builder
                    .capability(Capability::DemoteToHelperInvocation);
                // rspirv classifies this as a terminator and would close the
                // block. SPIR-V lets execution continue after it, as Metal
                // does, so append it without ending the block.
                self.builder
                    .insert_into_block(
                        InsertPoint::End,
                        rspirv::dr::Instruction::new(
                            rspirv::spirv::Op::DemoteToHelperInvocation,
                            None,
                            None,
                            Vec::new(),
                        ),
                    )
                    .map_err(build_error)?;
            }
        }
        Ok(())
    }

    /// Vulkan carries the array slice as an extra coordinate component;
    /// Metal passes it separately.
    fn texture_coordinate(
        &mut self,
        function: &Function,
        coordinate: &Operand,
        array_index: Option<&Operand>,
        values: &HashMap<ValueId, u32>,
    ) -> Result<u32, TranslateError> {
        let base = self.operand(coordinate, values)?;
        let Some(array_index) = array_index else {
            return Ok(base);
        };
        let element = match operand_type(function, coordinate) {
            Some(Type::Vector { element, .. }) => *element,
            Some(scalar) => scalar,
            None => return Err(emission("texture coordinate has no type")),
        };
        let lanes = match operand_type(function, coordinate) {
            Some(Type::Vector { lanes, .. }) => lanes,
            _ => 1,
        };
        // The slice joins the coordinate in the coordinate's own element
        // type, which is integer for reads and floating for samples.
        let index = self.operand(array_index, values)?;
        let index = match &element {
            Type::Float { .. } => {
                let float = self.type_id(&element)?;
                self.builder
                    .convert_u_to_f(float, None, index)
                    .map_err(build_error)?
            }
            _ => index,
        };
        let combined = self.type_id(&Type::Vector {
            element: Box::new(element.clone()),
            lanes: lanes + 1,
        })?;
        let scalar = self.type_id(&element)?;
        let mut components = Vec::new();
        for lane in 0..lanes {
            components.push(if lanes == 1 {
                base
            } else {
                self.builder
                    .composite_extract(scalar, None, base, [u32::from(lane)])
                    .map_err(build_error)?
            });
        }
        components.push(index);
        self.builder
            .composite_construct(combined, None, components)
            .map_err(build_error)
    }

    fn emit_binary<F>(
        &mut self,
        function: &Function,
        instruction: &crate::ir::Instruction,
        lhs: &Operand,
        rhs: &Operand,
        values: &HashMap<ValueId, u32>,
        emit: F,
    ) -> Result<(), TranslateError>
    where
        F: FnOnce(&mut Builder, u32, Option<u32>, u32, u32) -> Result<u32, rspirv::dr::Error>,
    {
        let result = instruction
            .result
            .ok_or_else(|| emission("binary instruction has no result"))?;
        let ty = self.type_id(&function.values[result.0 as usize])?;
        let lhs = self.operand(lhs, values)?;
        let rhs = self.operand(rhs, values)?;
        emit(&mut self.builder, ty, Some(values[&result]), lhs, rhs).map_err(build_error)?;
        Ok(())
    }

    fn emit_ext_inst<const N: usize>(
        &mut self,
        function: &Function,
        instruction: &crate::ir::Instruction,
        values: &HashMap<ValueId, u32>,
        operation: GlslStd450Op,
        operands: [&Operand; N],
    ) -> Result<(), TranslateError> {
        let result = instruction
            .result
            .ok_or_else(|| emission("extended instruction has no result"))?;
        let ty = self.type_id(&function.values[result.0 as usize])?;
        let operands = operands
            .into_iter()
            .map(|operand| self.operand(operand, values).map(SpirvOperand::IdRef))
            .collect::<Result<Vec<_>, _>>()?;
        self.builder
            .ext_inst(
                ty,
                Some(values[&result]),
                self.glsl_std_450,
                operation as u32,
                operands,
            )
            .map_err(build_error)?;
        Ok(())
    }

    fn emit_terminator(
        &mut self,
        terminator: &Terminator,
        blocks: &HashMap<BlockId, u32>,
        values: &HashMap<ValueId, u32>,
    ) -> Result<(), TranslateError> {
        match terminator {
            Terminator::Branch(target) => self.builder.branch(blocks[target]).map_err(build_error),
            Terminator::CondBranch {
                condition,
                true_block,
                false_block,
            } => {
                let condition = self.operand(condition, values)?;
                self.builder
                    .branch_conditional(
                        condition,
                        blocks[true_block],
                        blocks[false_block],
                        std::iter::empty(),
                    )
                    .map_err(build_error)
            }
            Terminator::Return(None) => self.builder.ret().map_err(build_error),
            // A SPIR-V entry point returns void. Metal returns the stage's
            // outputs as a value, so decompose it into the output variables.
            Terminator::Return(Some(value)) => {
                let operand_type = self.return_type.clone();
                let value = self.operand(value, values)?;
                if self.stage_outputs.is_empty() {
                    return self.builder.ret_value(value).map_err(build_error);
                }
                for (position, variable) in self.stage_outputs.clone().into_iter().enumerate() {
                    let component = match &operand_type {
                        Some(Type::Struct(structure)) => {
                            let field = structure.fields.get(position).ok_or_else(|| {
                                emission("stage output has no matching returned field")
                            })?;
                            let ty = self.type_id(&field.ty)?;
                            self.builder
                                .composite_extract(ty, None, value, [position as u32])
                                .map_err(build_error)?
                        }
                        _ => value,
                    };
                    self.builder
                        .store(variable, component, None, [])
                        .map_err(build_error)?;
                }
                self.builder.ret().map_err(build_error)
            }
            Terminator::Unreachable => self.builder.unreachable().map_err(build_error),
        }
    }

    fn operand(
        &mut self,
        operand: &Operand,
        values: &HashMap<ValueId, u32>,
    ) -> Result<u32, TranslateError> {
        match operand {
            Operand::Bool(value) => {
                let ty = self.type_id(&Type::Bool)?;
                Ok(if *value {
                    self.builder.constant_true(ty)
                } else {
                    self.builder.constant_false(ty)
                })
            }
            Operand::Value(value) => values
                .get(value)
                .copied()
                .ok_or_else(|| emission("operand refers to a missing value")),
            Operand::Integer { ty, value } => self.constant(ty, *value),
            Operand::Float { ty, bits } => self.constant(ty, *bits),
            Operand::Composite { .. } => self.composite_constant(operand),
            Operand::Global { index, .. } => self
                .globals
                .get(index)
                .map(|(variable, _)| *variable)
                .ok_or_else(|| emission("operand refers to a missing global variable")),
            Operand::Undef(ty) => self.undef(ty),
        }
    }

    fn constant(&mut self, ty: &Type, value: u64) -> Result<u32, TranslateError> {
        let key = (ty.clone(), value);
        if let Some(id) = self.constants.get(&key) {
            return Ok(*id);
        }
        let ty_id = self.type_id(ty)?;
        let id = match ty {
            Type::Int { bits: 1..=32, .. } | Type::Float { bits: 32 } => {
                self.builder.constant_bit32(ty_id, value as u32)
            }
            Type::Int { bits: 33..=64, .. } => self.builder.constant_bit64(ty_id, value),
            _ => return Err(emission("unsupported SPIR-V constant type")),
        };
        self.constants.insert(key, id);
        Ok(id)
    }

    /// The type of a value which does not live in a buffer.
    ///
    /// Vulkan forbids `ArrayStride` and `Offset` outside the storage classes
    /// which have an explicit layout, so a module global and a constant
    /// composite need their own aggregate types.
    fn unlaid_type_id(&mut self, ty: &Type) -> Result<u32, TranslateError> {
        if let Some(id) = self.unlaid_types.get(ty) {
            return Ok(*id);
        }
        let id = match ty {
            Type::Array { element, count } => {
                let element_id = self.unlaid_type_id(element)?;
                let count = self.constant(&Type::u32(), u64::from(*count))?;
                let id = self.builder.id();
                self.builder.type_array_id(Some(id), element_id, count);
                id
            }
            Type::Struct(structure) => {
                let fields = structure
                    .fields
                    .iter()
                    .map(|field| self.unlaid_type_id(&field.ty))
                    .collect::<Result<Vec<_>, _>>()?;
                let id = self.builder.id();
                self.builder.type_struct_id(Some(id), fields);
                id
            }
            _ => return self.type_id(ty),
        };
        self.unlaid_types.insert(ty.clone(), id);
        Ok(id)
    }

    /// Declare the module's global variables.
    ///
    /// Metal's `constant` tables become `Private` variables with their
    /// initializer, and its `threadgroup` arrays become `Workgroup` variables,
    /// which Vulkan does not let an initializer reach.
    fn declare_globals(&mut self, module: &Module) -> Result<(), TranslateError> {
        for (index, global) in module.globals.iter().enumerate() {
            let storage = match global.space {
                crate::ir::AddressSpace::Constant | crate::ir::AddressSpace::Thread => {
                    StorageClass::Private
                }
                crate::ir::AddressSpace::Threadgroup
                | crate::ir::AddressSpace::ThreadgroupImageblock => StorageClass::Workgroup,
                crate::ir::AddressSpace::Device => {
                    return Err(emission(
                        "a module global in the device address space has no Vulkan form",
                    ));
                }
            };
            let pointee = self.unlaid_type_id(&global.ty)?;
            let pointer = self.builder.type_pointer(None, storage, pointee);
            let initializer = match &global.initializer {
                Some(operand) if storage == StorageClass::Private => {
                    Some(self.constant_operand(operand)?)
                }
                // A `threadgroup` variable is uninitialized in Metal too.
                _ => None,
            };
            let variable = self.builder.variable(pointer, None, storage, initializer);
            if !global.name.is_empty() {
                self.builder.name(variable, global.name.clone());
            }
            // SPIR-V 1.4 widened an entry point's interface from its stage
            // variables to every global one it can reach.
            if self.spirv_version >= (1, 4) {
                self.interface.push(variable);
            }
            self.globals.insert(index as u32, (variable, storage));
        }
        Ok(())
    }

    /// One constant operand, as the initializer of a global variable.
    fn constant_operand(&mut self, operand: &Operand) -> Result<u32, TranslateError> {
        match operand {
            Operand::Integer { ty, value } => self.constant(ty, *value),
            Operand::Float { ty, bits } => self.constant(ty, *bits),
            Operand::Bool(value) => {
                let ty = self.type_id(&Type::Bool)?;
                Ok(if *value {
                    self.builder.constant_true(ty)
                } else {
                    self.builder.constant_false(ty)
                })
            }
            Operand::Composite { ty, elements } => {
                if let Some(id) = self.unlaid_composites.get(operand) {
                    return Ok(*id);
                }
                let ty_id = self.unlaid_type_id(ty)?;
                let members = elements
                    .iter()
                    .map(|element| self.constant_operand(element))
                    .collect::<Result<Vec<_>, _>>()?;
                let id = self.builder.constant_composite(ty_id, members);
                self.unlaid_composites.insert(operand.clone(), id);
                Ok(id)
            }
            Operand::Undef(ty) => self.undef(ty),
            Operand::Value(_) | Operand::Global { .. } => Err(emission(
                "a global variable's initializer is not a constant",
            )),
        }
    }

    /// The variable an access chain starts from, and the storage class its
    /// result pointer lives in.
    fn pointer_base(
        &mut self,
        pointer: &Operand,
        values: &HashMap<ValueId, u32>,
    ) -> Result<(u32, StorageClass, bool), TranslateError> {
        match pointer {
            Operand::Global { index, .. } => self
                .globals
                .get(index)
                // A global's variable points straight at what it holds, so an
                // access chain into it needs no leading member index.
                .map(|(variable, storage)| (*variable, *storage, false))
                .ok_or_else(|| emission("access chain base is a missing global variable")),
            Operand::Value(value) => {
                if let Some(variable) = self.parameter_pointers.get(value).copied() {
                    // A buffer resource is a block with the buffer as its one
                    // member, so every chain into it starts with that member.
                    return Ok((variable, StorageClass::StorageBuffer, true));
                }
                let storage =
                    self.pointer_storage.get(value).copied().ok_or_else(|| {
                        emission("access chain base is not a resource or a global")
                    })?;
                let id = values
                    .get(value)
                    .copied()
                    .ok_or_else(|| emission("access chain base refers to a missing value"))?;
                Ok((id, storage, false))
            }
            _ => Err(emission("access chain base is not a pointer")),
        }
    }

    fn type_id(&mut self, ty: &Type) -> Result<u32, TranslateError> {
        if let Some(id) = self.types.get(ty) {
            return Ok(*id);
        }
        let id = match ty {
            Type::Void => self.builder.type_void(),
            Type::Bool => self.builder.type_bool(),
            Type::Int { bits, signed } => {
                match bits {
                    64 => {
                        if !self.shader_int64 {
                            return Err(emission("target profile does not support Int64"));
                        }
                        if !self.uses_int64 {
                            self.builder.capability(Capability::Int64);
                            self.uses_int64 = true;
                        }
                    }
                    // Metal's `short` and `char` need their own Vulkan
                    // capabilities; a shader using them says so.
                    16 => self.builder.capability(Capability::Int16),
                    8 => self.builder.capability(Capability::Int8),
                    _ => {}
                }
                self.builder.type_int(u32::from(*bits), u32::from(*signed))
            }
            Type::Float { bits } => {
                if *bits == 16 {
                    self.builder.capability(Capability::Float16);
                }
                self.builder.type_float(u32::from(*bits), None)
            }
            Type::Vector { element, lanes } => {
                let element = self.type_id(element)?;
                self.builder.type_vector(element, u32::from(*lanes))
            }
            Type::Struct(structure) => {
                let fields = structure
                    .fields
                    .iter()
                    .map(|field| self.type_id(&field.ty))
                    .collect::<Result<Vec<_>, _>>()?;
                // Distinct Metal structures can have identical member types
                // and different offsets. rspirv would reuse one SPIR-V type
                // for both and decorate it twice, so force a fresh id.
                let id = self.builder.id();
                self.builder.type_struct_id(Some(id), fields);
                for (index, field) in structure.fields.iter().enumerate() {
                    self.builder.member_decorate(
                        id,
                        index as u32,
                        Decoration::Offset,
                        [SpirvOperand::LiteralBit32(field.offset)],
                    );
                }
                id
            }
            Type::Array { element, count } => {
                let element_id = self.type_id(element)?;
                let count = self.constant(&Type::u32(), u64::from(*count))?;
                // As for a structure: a decorated type must not be shared with
                // the undecorated one a module global needs.
                let id = self.builder.id();
                self.builder.type_array_id(Some(id), element_id, count);
                self.builder.decorate(
                    id,
                    Decoration::ArrayStride,
                    [SpirvOperand::LiteralBit32(type_stride(element)?)],
                );
                id
            }
            Type::Texture(texture) => {
                let texture = texture.clone();
                let scalar = self.type_id(&texture.texel)?;
                let dimension = match texture.dimension {
                    TextureDimension::D1 => Dim::Dim1D,
                    TextureDimension::D2 => Dim::Dim2D,
                    TextureDimension::D3 => Dim::Dim3D,
                    TextureDimension::Cube => Dim::DimCube,
                };
                // `Sampled` distinguishes an image read through a sampler
                // from a storage image; Metal's access mode decides which.
                let sampled = if texture.access == Access::Sample {
                    1
                } else {
                    2
                };
                if texture.dimension == TextureDimension::D1 && sampled == 2 {
                    self.builder.capability(Capability::Image1D);
                }
                if texture.multisampled && sampled == 2 {
                    self.builder.capability(Capability::StorageImageMultisample);
                }
                if texture.dimension == TextureDimension::Cube && texture.array {
                    self.builder.capability(Capability::SampledCubeArray);
                }
                // A storage image with an unknown format needs an explicit
                // capability for each direction it is accessed in.
                if sampled == 2 {
                    match texture.access {
                        Access::Read | Access::ReadWrite => {
                            self.builder
                                .capability(Capability::StorageImageReadWithoutFormat);
                        }
                        _ => {}
                    }
                    match texture.access {
                        Access::Write | Access::ReadWrite => {
                            self.builder
                                .capability(Capability::StorageImageWriteWithoutFormat);
                        }
                        _ => {}
                    }
                }
                self.builder.type_image(
                    scalar,
                    dimension,
                    u32::from(texture.depth),
                    u32::from(texture.array),
                    u32::from(texture.multisampled),
                    sampled,
                    ImageFormat::Unknown,
                    None,
                )
            }
            Type::Sampler => self.builder.type_sampler(),
            _ => return Err(emission(format!("unsupported SPIR-V value type {ty:?}"))),
        };
        self.types.insert(ty.clone(), id);
        Ok(id)
    }
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

/// Give a function a single returning block.
///
/// A Metal shader commonly returns from both sides of a bounds test. SPIR-V
/// needs every conditional branch to name the block where its paths rejoin,
/// and two returning blocks have none, so route both through one exit.
fn single_exit(mut function: Function) -> Function {
    let returning: Vec<_> = function
        .blocks
        .iter()
        .filter(|block| matches!(block.terminator, Terminator::Return(_)))
        .map(|block| block.id)
        .collect();
    if returning.len() < 2 {
        return function;
    }
    let exit = BlockId(
        function
            .blocks
            .iter()
            .map(|block| block.id.0)
            .max()
            .unwrap_or(0)
            + 1,
    );
    let mut incomings = Vec::new();
    for block in &mut function.blocks {
        let Terminator::Return(value) = block.terminator.clone() else {
            continue;
        };
        if let Some(value) = value {
            incomings.push((block.id, value));
        }
        block.terminator = Terminator::Branch(exit);
    }
    let (instructions, terminator) = if incomings.is_empty() {
        (Vec::new(), Terminator::Return(None))
    } else {
        let result = ValueId(function.values.len() as u32);
        function.values.push(function.result.clone());
        (
            vec![crate::ir::Instruction {
                result: Some(result),
                kind: InstructionKind::Phi { incomings },
            }],
            Terminator::Return(Some(Operand::Value(result))),
        )
    };
    function.blocks.push(crate::ir::Block {
        id: exit,
        name: "exit".to_string(),
        instructions,
        terminator,
    });
    function
}

/// Order blocks so that a block always precedes those it dominates, which
/// SPIR-V requires of a function's block list.
fn reverse_post_order(function: &mut Function) {
    let graph = ControlFlow::new(function);
    let entry = function.blocks[0].id;
    let mut order = Vec::new();
    let mut visited = BTreeSet::new();
    let mut stack = vec![(entry, 0usize)];
    while let Some((block, index)) = stack.pop() {
        if index == 0 && !visited.insert(block) {
            continue;
        }
        match graph.successors[&block].get(index) {
            Some(successor) => {
                let successor = *successor;
                stack.push((block, index + 1));
                if !visited.contains(&successor) {
                    stack.push((successor, 0));
                }
            }
            None => order.push(block),
        }
    }
    order.reverse();
    // Unreachable blocks keep their original relative position at the end.
    let rank = |id: BlockId| {
        order
            .iter()
            .position(|block| *block == id)
            .unwrap_or(usize::MAX)
    };
    function.blocks.sort_by_key(|block| rank(block.id));
}

/// Give each structured header its own merge block.
///
/// An `else if` chain reaches one rejoining block from several conditional
/// branches, and SPIR-V forbids two headers from naming the same merge. Route
/// the inner construct's edges through a new block, which then becomes its
/// merge.
fn split_shared_merges(function: &mut Function) -> Result<(), TranslateError> {
    for _ in 0..function.blocks.len().max(1) {
        let merges = control_flow_merges(function)?;
        let mut claims: BTreeMap<BlockId, Vec<BlockId>> = BTreeMap::new();
        for (header, merge) in &merges {
            let block = match merge {
                Merge::Selection { merge } | Merge::Loop { merge, .. } => *merge,
            };
            claims.entry(block).or_default().push(*header);
        }
        let Some((merge, headers)) = claims.into_iter().find(|(_, headers)| headers.len() > 1)
        else {
            return Ok(());
        };
        let graph = ControlFlow::new(function);
        let dominators = graph.dominance(&[function.blocks[0].id], &graph.predecessors);
        // The innermost header is the one every other claimant dominates.
        let inner = headers
            .iter()
            .copied()
            .max_by_key(|header| {
                headers
                    .iter()
                    .filter(|other| dominators.dominates(**other, *header))
                    .count()
            })
            .ok_or_else(|| emission("a merge block has no claiming header"))?;
        let sources: Vec<BlockId> = graph
            .predecessors
            .get(&merge)
            .into_iter()
            .flatten()
            .copied()
            .filter(|predecessor| dominators.dominates(inner, *predecessor))
            .collect();
        if sources.is_empty() || sources.len() == graph.predecessors[&merge].len() {
            return Err(emission(
                "a shared merge block cannot be split into nested constructs",
            ));
        }
        split_merge(function, merge, &sources);
    }
    Err(emission(
        "control flow needs more restructuring than this translator performs",
    ))
}

/// Give a loop whose body rejoins at its latch an empty continue target.
///
/// SPIR-V's loop construct does not contain the continue construct, so a
/// selection inside the body may not merge at the continue target. An
/// `if`/`else` at the end of a loop body rejoins at exactly that block, so
/// route the back edge through a new block and leave the old latch in the
/// body, where it is a legal merge.
fn separate_loop_latches(function: &mut Function) -> Result<(), TranslateError> {
    for _ in 0..function.blocks.len().max(1) {
        let graph = ControlFlow::new(function);
        let entry = function.blocks[0].id;
        let dominators = graph.dominance(&[entry], &graph.predecessors);
        let exits: Vec<_> = graph
            .blocks
            .iter()
            .copied()
            .filter(|block| graph.successors[block].is_empty())
            .collect();
        let post_dominators = graph.dominance(&exits, &graph.successors);
        let mut loops = Vec::new();
        for block in &function.blocks {
            let latches: Vec<_> = graph
                .predecessors
                .get(&block.id)
                .into_iter()
                .flatten()
                .copied()
                .filter(|predecessor| dominators.dominates(block.id, *predecessor))
                .collect();
            let [continue_target] = latches.as_slice() else {
                continue;
            };
            loops.push((
                block.id,
                natural_loop(&graph, block.id, *continue_target),
                *continue_target,
            ));
        }
        let mut split = None;
        for block in &function.blocks {
            if !matches!(block.terminator, Terminator::CondBranch { .. }) {
                continue;
            }
            if loops.iter().any(|(header, ..)| *header == block.id) {
                continue;
            }
            let Some((header, body, continue_target)) = loops
                .iter()
                .filter(|(_, body, _)| body.contains(&block.id))
                .min_by_key(|(_, body, _)| body.len())
            else {
                continue;
            };
            if post_dominators.immediate(block.id) != Some(*continue_target) {
                continue;
            }
            // The emitter merges such a conditional at the arm which keeps
            // running the body. When both arms do, or neither does, there is
            // no such arm, and the latch is where they rejoin.
            let staying = successors_of(&block.terminator)
                .into_iter()
                .filter(|successor| successor != continue_target)
                .filter(|successor| reaches(&graph, *successor, *continue_target, body))
                .count();
            if staying == 1 {
                continue;
            }
            split = Some((*header, *continue_target));
            break;
        }
        let Some((header, latch)) = split else {
            return Ok(());
        };
        split_merge(function, header, &[latch]);
    }
    Err(emission(
        "a loop's latch could not be separated from its body",
    ))
}

/// Give a loop whose header also chooses between two body paths a header of
/// its own.
///
/// A block declares one merge instruction. When a loop header's conditional
/// branch goes to two blocks inside the body, the header would have to be both
/// the loop's header and the selection's, and SPIR-V has no way to say so.
/// Routing every edge into the header through a new block leaves that block
/// carrying the loop and the old header carrying the selection.
fn normalize_loop_headers(function: &mut Function) -> Result<(), TranslateError> {
    for _ in 0..function.blocks.len().max(1) {
        let graph = ControlFlow::new(function);
        let entry = function.blocks[0].id;
        let dominators = graph.dominance(&[entry], &graph.predecessors);
        let mut split = None;
        for block in &function.blocks {
            if !matches!(block.terminator, Terminator::CondBranch { .. }) {
                continue;
            }
            let predecessors: Vec<_> = graph
                .predecessors
                .get(&block.id)
                .into_iter()
                .flatten()
                .copied()
                .collect();
            let latches: Vec<_> = predecessors
                .iter()
                .copied()
                .filter(|predecessor| dominators.dominates(block.id, *predecessor))
                .collect();
            let [continue_target] = latches.as_slice() else {
                continue;
            };
            let body = natural_loop(&graph, block.id, *continue_target);
            let mut leaving: Vec<_> = body
                .iter()
                .flat_map(|member| graph.successors[member].iter().copied())
                .filter(|successor| !body.contains(successor))
                .collect();
            leaving.sort_unstable();
            leaving.dedup();
            let [merge] = leaving.as_slice() else {
                continue;
            };
            // A header which leaves the loop or reaches its latch directly is
            // already the loop's own test and needs no separate block.
            if successors_of(&block.terminator)
                .iter()
                .any(|successor| successor == merge || successor == continue_target)
            {
                continue;
            }
            split = Some((block.id, predecessors));
            break;
        }
        let Some((header, predecessors)) = split else {
            return Ok(());
        };
        split_merge(function, header, &predecessors);
    }
    Err(emission(
        "a loop header could not be separated from the selection it also makes",
    ))
}

/// Move every phi to the head of its block.
///
/// SPIR-V requires it, and restructuring adds phis to blocks which already
/// have instructions. A phi only names values from its predecessors, so its
/// position within its own block never changes what it computes.
fn hoist_phis(function: &mut Function) {
    for block in &mut function.blocks {
        let instructions = std::mem::take(&mut block.instructions);
        let (phis, rest): (Vec<_>, Vec<_>) = instructions
            .into_iter()
            .partition(|instruction| matches!(instruction.kind, InstructionKind::Phi { .. }));
        block.instructions = phis;
        block.instructions.extend(rest);
    }
}

/// Insert a block between `sources` and `merge`, moving the phi inputs which
/// arrive from those predecessors into it.
fn split_merge(function: &mut Function, merge: BlockId, sources: &[BlockId]) {
    let inserted = BlockId(
        function
            .blocks
            .iter()
            .map(|block| block.id.0)
            .max()
            .unwrap_or(0)
            + 1,
    );
    for block in &mut function.blocks {
        if !sources.contains(&block.id) {
            continue;
        }
        block.terminator = match block.terminator.clone() {
            Terminator::Branch(target) if target == merge => Terminator::Branch(inserted),
            Terminator::CondBranch {
                condition,
                true_block,
                false_block,
            } => Terminator::CondBranch {
                condition,
                true_block: if true_block == merge {
                    inserted
                } else {
                    true_block
                },
                false_block: if false_block == merge {
                    inserted
                } else {
                    false_block
                },
            },
            other => other,
        };
    }
    let mut instructions = Vec::new();
    let merge_block = function
        .blocks
        .iter()
        .position(|block| block.id == merge)
        .expect("the merge block exists");
    let mut phis = std::mem::take(&mut function.blocks[merge_block].instructions);
    for instruction in &mut phis {
        let InstructionKind::Phi { incomings } = &mut instruction.kind else {
            continue;
        };
        let (inner, outer): (Vec<_>, Vec<_>) = incomings
            .iter()
            .cloned()
            .partition(|(from, _)| sources.contains(from));
        let replacement = match inner.as_slice() {
            [] => continue,
            [(_, value)] => value.clone(),
            _ => {
                let result = ValueId(function.values.len() as u32);
                let ty = instruction
                    .result
                    .and_then(|value| function.values.get(value.0 as usize))
                    .cloned()
                    .unwrap_or(Type::Void);
                function.values.push(ty);
                instructions.push(crate::ir::Instruction {
                    result: Some(result),
                    kind: InstructionKind::Phi { incomings: inner },
                });
                Operand::Value(result)
            }
        };
        *incomings = outer;
        incomings.push((inserted, replacement));
    }
    function.blocks[merge_block].instructions = phis;
    function.blocks.push(crate::ir::Block {
        id: inserted,
        name: format!("merge{}", inserted.0),
        instructions,
        terminator: Terminator::Branch(merge),
    });
}

/// The structured construct a block with two successors introduces.
///
/// SPIR-V has no unstructured control flow: every conditional branch must
/// declare where its two paths rejoin, and a loop must additionally name its
/// back edge. LLVM records neither, so both are recovered from the CFG.
#[derive(Clone, Copy, Debug)]
enum Merge {
    Selection {
        merge: BlockId,
    },
    Loop {
        merge: BlockId,
        continue_target: BlockId,
    },
}

fn successors_of(terminator: &Terminator) -> Vec<BlockId> {
    match terminator {
        Terminator::Branch(target) => vec![*target],
        Terminator::CondBranch {
            true_block,
            false_block,
            ..
        } => vec![*true_block, *false_block],
        Terminator::Return(_) | Terminator::Unreachable => Vec::new(),
    }
}

struct ControlFlow {
    blocks: Vec<BlockId>,
    successors: HashMap<BlockId, Vec<BlockId>>,
    predecessors: HashMap<BlockId, Vec<BlockId>>,
}

impl ControlFlow {
    fn new(function: &Function) -> Self {
        let mut graph = Self {
            blocks: function.blocks.iter().map(|block| block.id).collect(),
            successors: HashMap::new(),
            predecessors: HashMap::new(),
        };
        for block in &function.blocks {
            let successors = successors_of(&block.terminator);
            for successor in &successors {
                graph
                    .predecessors
                    .entry(*successor)
                    .or_default()
                    .push(block.id);
            }
            graph.successors.insert(block.id, successors);
        }
        graph
    }

    /// Iterative dataflow over a set lattice, used for both dominators and
    /// post-dominators by swapping the edge direction.
    fn dominance(&self, roots: &[BlockId], edges: &HashMap<BlockId, Vec<BlockId>>) -> Dominance {
        let all: BTreeSet<_> = self.blocks.iter().copied().collect();
        let mut sets: BTreeMap<BlockId, BTreeSet<BlockId>> = self
            .blocks
            .iter()
            .map(|block| {
                (
                    *block,
                    if roots.contains(block) {
                        BTreeSet::from([*block])
                    } else {
                        all.clone()
                    },
                )
            })
            .collect();
        loop {
            let previous = sets.clone();
            for block in self.blocks.iter().filter(|block| !roots.contains(block)) {
                let incoming = edges.get(block).cloned().unwrap_or_default();
                let mut next = match incoming.split_first() {
                    Some((first, rest)) => {
                        rest.iter().fold(previous[first].clone(), |set, edge| {
                            set.intersection(&previous[edge]).copied().collect()
                        })
                    }
                    None => BTreeSet::new(),
                };
                next.insert(*block);
                sets.insert(*block, next);
            }
            if sets == previous {
                return Dominance { sets };
            }
        }
    }
}

struct Dominance {
    sets: BTreeMap<BlockId, BTreeSet<BlockId>>,
}

impl Dominance {
    fn dominates(&self, dominator: BlockId, block: BlockId) -> bool {
        self.sets
            .get(&block)
            .is_some_and(|set| set.contains(&dominator))
    }

    /// The closest strict dominator: the one whose own dominator set is
    /// largest, since a nearer dominator is dominated by every farther one.
    fn immediate(&self, block: BlockId) -> Option<BlockId> {
        self.sets
            .get(&block)?
            .iter()
            .filter(|candidate| **candidate != block)
            .max_by_key(|candidate| self.sets.get(candidate).map_or(0, BTreeSet::len))
            .copied()
    }
}

/// Recover the structured constructs of a function's control-flow graph.
fn control_flow_merges(function: &Function) -> Result<HashMap<BlockId, Merge>, TranslateError> {
    let graph = ControlFlow::new(function);
    let entry = function.blocks[0].id;
    let dominators = graph.dominance(&[entry], &graph.predecessors);
    let exits: Vec<_> = graph
        .blocks
        .iter()
        .copied()
        .filter(|block| graph.successors[block].is_empty())
        .collect();
    let post_dominators = graph.dominance(&exits, &graph.successors);
    // Loops first: a selection inside one may branch out of it, and that
    // choice depends on where the loop continues.
    let mut loops = Vec::new();
    for block in &function.blocks {
        let latches: Vec<_> = graph
            .predecessors
            .get(&block.id)
            .into_iter()
            .flatten()
            .copied()
            .filter(|predecessor| dominators.dominates(block.id, *predecessor))
            .collect();
        if latches.is_empty() {
            continue;
        }
        let [continue_target] = latches.as_slice() else {
            return Err(emission(
                "a loop with several back edges needs restructuring",
            ));
        };
        let body = natural_loop(&graph, block.id, *continue_target);
        let mut leaving: Vec<_> = body
            .iter()
            .flat_map(|member| graph.successors[member].iter().copied())
            .filter(|successor| !body.contains(successor))
            .collect();
        leaving.sort_unstable();
        leaving.dedup();
        let [merge] = leaving.as_slice() else {
            return Err(emission("a loop with several exits needs restructuring"));
        };
        loops.push((block.id, body, *merge, *continue_target));
    }
    let mut merges = HashMap::new();
    for (header, _, merge, continue_target) in &loops {
        merges.insert(
            *header,
            Merge::Loop {
                merge: *merge,
                continue_target: *continue_target,
            },
        );
    }
    for block in &function.blocks {
        if !matches!(block.terminator, Terminator::CondBranch { .. })
            || merges.contains_key(&block.id)
        {
            continue;
        }
        let merge = post_dominators.immediate(block.id).ok_or_else(|| {
            emission("a conditional branch's paths never rejoin, which SPIR-V cannot express")
        })?;
        // A conditional which leaves an enclosing loop, whether by `break` or
        // by `continue`, has no rejoining block inside the loop body: its
        // paths meet only at the loop's merge or its continue target, and
        // SPIR-V accepts neither as a selection's merge. Use the arm which
        // keeps running the body instead.
        let enclosing = loops
            .iter()
            .filter(|(_, body, _, _)| body.contains(&block.id))
            .min_by_key(|(_, body, _, _)| body.len());
        let merge = match enclosing {
            Some((_, body, loop_merge, continue_target))
                if !body.contains(&merge) || merge == *continue_target =>
            {
                let successors = successors_of(&block.terminator);
                // A conditional whose two arms are the loop's own merge and
                // its continue target is a `break` against a `continue`. It
                // declares no selection: SPIR-V lets a branch leave a
                // construct by either of those without one.
                if successors
                    .iter()
                    .all(|successor| successor == loop_merge || successor == continue_target)
                {
                    continue;
                }
                let staying: Vec<_> = successors
                    .into_iter()
                    .filter(|successor| successor != continue_target)
                    .filter(|successor| reaches(&graph, *successor, *continue_target, body))
                    .collect();
                match staying.as_slice() {
                    [only] => *only,
                    // Two arms which both run more of the body before
                    // leaving it do not rejoin inside the loop. Expressing
                    // that needs real restructuring, not a merge choice.
                    _ => {
                        return Err(emission(
                            "a conditional which leaves its loop has no single continuing arm",
                        ));
                    }
                }
            }
            _ => merge,
        };
        merges.insert(block.id, Merge::Selection { merge });
    }
    Ok(merges)
}

/// Whether `target` is reachable from `from` without leaving `within`.
fn reaches(
    graph: &ControlFlow,
    from: BlockId,
    target: BlockId,
    within: &BTreeSet<BlockId>,
) -> bool {
    let mut seen = BTreeSet::new();
    let mut pending = vec![from];
    while let Some(block) = pending.pop() {
        if block == target {
            return true;
        }
        if !within.contains(&block) || !seen.insert(block) {
            continue;
        }
        pending.extend(graph.successors[&block].iter().copied());
    }
    false
}

/// The blocks of the natural loop formed by the back edge `latch -> header`.
fn natural_loop(graph: &ControlFlow, header: BlockId, latch: BlockId) -> BTreeSet<BlockId> {
    let mut body = BTreeSet::from([header]);
    let mut pending = vec![latch];
    while let Some(block) = pending.pop() {
        if !body.insert(block) {
            continue;
        }
        pending.extend(
            graph
                .predecessors
                .get(&block)
                .into_iter()
                .flatten()
                .copied(),
        );
    }
    body
}

fn build_error(error: rspirv::dr::Error) -> TranslateError {
    emission(format!("SPIR-V builder rejected the module: {error}"))
}

fn emission(message: impl Into<String>) -> TranslateError {
    TranslateError::one(Diagnostic::new(Phase::SpirvEmission, message))
}
