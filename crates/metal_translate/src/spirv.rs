//! SPIR-V emission from Metal IR.

use std::collections::HashMap;

use rspirv::binary::Assemble;
use rspirv::dr::{Builder, Operand as SpirvOperand};
use rspirv::spirv::{
    AddressingModel, BuiltIn, Capability, Decoration, Dim, ExecutionMode, ExecutionModel,
    FunctionControl, GlslStd450Op, ImageFormat, LoopControl, MemoryModel, StorageClass,
};

use crate::ir::{
    Access, BlockId, EntryPoint, Function, InstructionKind, IntegerPredicate, Module, Operand,
    ResourceKind, Stage, Terminator, Type, ValueId,
};
use crate::{Diagnostic, Phase, TranslateError};

/// Vulkan and SPIR-V capabilities available to an emitter invocation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TargetProfile {
    pub vulkan_version: (u8, u8),
    pub spirv_version: (u8, u8),
    pub max_bound_descriptor_sets: u32,
    pub shader: bool,
}

impl TargetProfile {
    pub fn vulkan_1_3() -> Self {
        Self {
            vulkan_version: (1, 3),
            spirv_version: (1, 6),
            max_bound_descriptor_sets: 4,
            shader: true,
        }
    }
}

impl Default for TargetProfile {
    fn default() -> Self {
        Self::vulkan_1_3()
    }
}

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
    if !matches!(entry.stage, Stage::Kernel | Stage::Fragment) {
        return Err(emission(
            "the emitter only supports kernel and fragment entry points",
        ));
    }
    let function = module
        .functions
        .get(entry.function)
        .ok_or_else(|| emission("entry point refers to a missing function"))?;
    let mut emitter = Emitter::new(target);
    let resources = emitter.declare_resources(entry, function)?;
    emitter.declare_stage_io(entry, function)?;
    let function_id = emitter.emit_function(entry, function)?;
    let execution_model = match entry.stage {
        Stage::Kernel => ExecutionModel::GLCompute,
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
            emitter
                .builder
                .execution_mode(function_id, ExecutionMode::LocalSize, [1, 1, 1])
        }
        Stage::Fragment => {
            emitter
                .builder
                .execution_mode(function_id, ExecutionMode::OriginUpperLeft, [])
        }
        _ => unreachable!(),
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
            capabilities: vec!["Shader".to_string()],
            extensions: Vec::new(),
        },
    })
}

struct Emitter {
    builder: Builder,
    types: HashMap<Type, u32>,
    constants: HashMap<(Type, u64), u32>,
    buffer_types: HashMap<Type, (u32, u32)>,
    interface: Vec<u32>,
    parameter_pointers: HashMap<ValueId, u32>,
    global_invocation_id: Option<u32>,
    glsl_std_450: u32,
    stage_inputs: HashMap<ValueId, u32>,
    fragment_output: Option<u32>,
}

impl Emitter {
    fn new(target: &TargetProfile) -> Self {
        let mut builder = Builder::new();
        builder.set_version(target.spirv_version.0, target.spirv_version.1);
        builder.capability(Capability::Shader);
        builder.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
        let glsl_std_450 = builder.ext_inst_import("GLSL.std.450");
        Self {
            builder,
            types: HashMap::new(),
            constants: HashMap::new(),
            buffer_types: HashMap::new(),
            interface: Vec::new(),
            parameter_pointers: HashMap::new(),
            global_invocation_id: None,
            glsl_std_450,
            stage_inputs: HashMap::new(),
            fragment_output: None,
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
                (ResourceKind::Texture, Type::Texture) => {
                    let image = self.type_id(&Type::Texture)?;
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
        let block_type = self.builder.type_struct([array_type]);
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

    fn declare_stage_io(
        &mut self,
        entry: &EntryPoint,
        function: &Function,
    ) -> Result<(), TranslateError> {
        if entry.stage != Stage::Fragment {
            return Ok(());
        }
        for parameter in &function.parameters {
            if self.parameter_pointers.contains_key(&parameter.value) {
                continue;
            }
            let location = parameter
                .name
                .strip_prefix("locn")
                .and_then(|value| value.parse::<u32>().ok())
                .ok_or_else(|| emission("fragment input has no decoded location"))?;
            let ty = self.type_id(&parameter.ty)?;
            let pointer = self.builder.type_pointer(None, StorageClass::Input, ty);
            let variable = self
                .builder
                .variable(pointer, None, StorageClass::Input, None);
            self.builder.decorate(
                variable,
                Decoration::Location,
                [SpirvOperand::LiteralBit32(location)],
            );
            self.interface.push(variable);
            self.stage_inputs.insert(parameter.value, variable);
        }
        let output_ty = Type::Vector {
            element: Box::new(Type::Float { bits: 32 }),
            lanes: 4,
        };
        let output_ty = self.type_id(&output_ty)?;
        let pointer = self
            .builder
            .type_pointer(None, StorageClass::Output, output_ty);
        let variable = self
            .builder
            .variable(pointer, None, StorageClass::Output, None);
        self.builder.decorate(
            variable,
            Decoration::Location,
            [SpirvOperand::LiteralBit32(0)],
        );
        self.interface.push(variable);
        self.fragment_output = Some(variable);
        Ok(())
    }

    fn emit_function(
        &mut self,
        entry: &EntryPoint,
        function: &Function,
    ) -> Result<u32, TranslateError> {
        self.collect_constants(function)?;
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
                self.materialize_parameter_pointers(function, &value_ids)?;
                self.materialize_stage_inputs(function, &value_ids)?;
            }
            for instruction in &block.instructions {
                self.emit_instruction(function, instruction, &block_ids, &value_ids)?;
            }
            if let Some((merge, continue_target)) = loop_shape(function, block.id) {
                self.builder
                    .loop_merge(
                        block_ids[&merge],
                        block_ids[&continue_target],
                        LoopControl::NONE,
                        std::iter::empty::<SpirvOperand>(),
                    )
                    .map_err(build_error)?;
            }
            self.emit_terminator(&block.terminator, &block_ids, &value_ids)?;
        }
        self.builder.end_function().map_err(build_error)?;
        Ok(function_id)
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
            if parameter.ty == Type::Texture {
                let image = self.type_id(&Type::Texture)?;
                self.builder
                    .load(image, Some(values[&parameter.value]), variable, None, [])
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

    fn collect_constants(&mut self, function: &Function) -> Result<(), TranslateError> {
        for block in &function.blocks {
            for instruction in &block.instructions {
                for operand in instruction_operands(&instruction.kind) {
                    match operand {
                        Operand::Integer { ty, value } => {
                            self.constant(ty, *value)?;
                        }
                        Operand::Float { ty, bits } => {
                            self.constant(ty, *bits)?;
                        }
                        Operand::Value(_) => {}
                        Operand::Undef(_) => {}
                    }
                }
            }
            for operand in terminator_operands(&block.terminator) {
                match operand {
                    Operand::Integer { ty, value } => {
                        self.constant(ty, *value)?;
                    }
                    Operand::Float { ty, bits } => {
                        self.constant(ty, *bits)?;
                    }
                    Operand::Value(_) => {}
                    Operand::Undef(_) => {}
                }
            }
        }
        Ok(())
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
            InstructionKind::IntegerAdd { lhs, rhs } => {
                self.emit_binary(
                    function,
                    instruction,
                    lhs,
                    rhs,
                    values,
                    |builder, ty, id, a, b| builder.i_add(ty, id, a, b),
                )?;
            }
            InstructionKind::IntegerMultiply { lhs, rhs } => {
                self.emit_binary(
                    function,
                    instruction,
                    lhs,
                    rhs,
                    values,
                    |builder, ty, id, a, b| builder.i_mul(ty, id, a, b),
                )?;
            }
            InstructionKind::IntegerXor { lhs, rhs } => {
                self.emit_binary(
                    function,
                    instruction,
                    lhs,
                    rhs,
                    values,
                    |builder, ty, id, a, b| builder.bitwise_xor(ty, id, a, b),
                )?;
            }
            InstructionKind::IntegerCompare {
                predicate: IntegerPredicate::UnsignedLessThan,
                lhs,
                rhs,
            } => {
                let ty = self.type_id(&Type::Bool)?;
                let lhs = self.operand(lhs, values)?;
                let rhs = self.operand(rhs, values)?;
                self.builder
                    .u_less_than(ty, result_id, lhs, rhs)
                    .map_err(build_error)?;
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
                let Operand::Value(parameter) = pointer else {
                    return Err(emission("element address requires a buffer parameter"));
                };
                let variable = self
                    .parameter_pointers
                    .get(parameter)
                    .copied()
                    .ok_or_else(|| emission("element address base is not a buffer resource"))?;
                let result = instruction
                    .result
                    .ok_or_else(|| emission("element address has no result"))?;
                let Type::Pointer { pointee, .. } = &function.values[result.0 as usize] else {
                    return Err(emission("element address result is not a pointer"));
                };
                let pointee = self.type_id(pointee)?;
                let pointer_type =
                    self.builder
                        .type_pointer(None, StorageClass::StorageBuffer, pointee);
                let zero = self.constant(&Type::u32(), 0)?;
                let index = self.operand(index, values)?;
                self.builder
                    .access_chain(pointer_type, result_id, variable, [zero, index])
                    .map_err(build_error)?;
            }
            InstructionKind::FieldAddress { pointer, indices } => {
                let Operand::Value(parameter) = pointer else {
                    return Err(emission("field address requires a buffer parameter"));
                };
                let variable = self
                    .parameter_pointers
                    .get(parameter)
                    .copied()
                    .ok_or_else(|| emission("field address base is not a buffer resource"))?;
                let result = instruction
                    .result
                    .ok_or_else(|| emission("field address has no result"))?;
                let Type::Pointer { pointee, .. } = &function.values[result.0 as usize] else {
                    return Err(emission("field address result is not a pointer"));
                };
                let pointee = self.type_id(pointee)?;
                let pointer_type =
                    self.builder
                        .type_pointer(None, StorageClass::StorageBuffer, pointee);
                let mut chain = vec![
                    self.constant(&Type::u32(), 0)?,
                    self.constant(&Type::u32(), 0)?,
                ];
                for index in indices {
                    chain.push(self.constant(&Type::u32(), u64::from(*index))?);
                }
                self.builder
                    .access_chain(pointer_type, result_id, variable, chain)
                    .map_err(build_error)?;
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
            InstructionKind::VectorUnsignedLessThan { lhs, rhs } => self.emit_binary(
                function,
                instruction,
                lhs,
                rhs,
                values,
                |builder, ty, id, a, b| builder.u_less_than(ty, id, a, b),
            )?,
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
            InstructionKind::ConvertUToF { value } => {
                let result = instruction
                    .result
                    .ok_or_else(|| emission("conversion has no result"))?;
                let ty = self.type_id(&function.values[result.0 as usize])?;
                let value = self.operand(value, values)?;
                self.builder
                    .convert_u_to_f(ty, result_id, value)
                    .map_err(build_error)?;
            }
            InstructionKind::FloatDiv { lhs, rhs } => self.emit_binary(
                function,
                instruction,
                lhs,
                rhs,
                values,
                |builder, ty, id, a, b| builder.f_div(ty, id, a, b),
            )?,
            InstructionKind::FloatAdd { lhs, rhs } => self.emit_binary(
                function,
                instruction,
                lhs,
                rhs,
                values,
                |builder, ty, id, a, b| builder.f_add(ty, id, a, b),
            )?,
            InstructionKind::FloatSubtract { lhs, rhs } => self.emit_binary(
                function,
                instruction,
                lhs,
                rhs,
                values,
                |builder, ty, id, a, b| builder.f_sub(ty, id, a, b),
            )?,
            InstructionKind::FloatMultiply { lhs, rhs } => self.emit_binary(
                function,
                instruction,
                lhs,
                rhs,
                values,
                |builder, ty, id, a, b| builder.f_mul(ty, id, a, b),
            )?,
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
            InstructionKind::FloatSqrt { value } => {
                self.emit_ext_inst(function, instruction, values, GlslStd450Op::Sqrt, [value])?;
            }
            InstructionKind::FloatExp2 { value } => {
                self.emit_ext_inst(function, instruction, values, GlslStd450Op::Exp2, [value])?;
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
                value,
            } => {
                let texture = self.operand(texture, values)?;
                let coordinate = self.operand(coordinate, values)?;
                let value = self.operand(value, values)?;
                self.builder
                    .image_write(texture, coordinate, value, None, [])
                    .map_err(build_error)?;
            }
        }
        Ok(())
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
            Terminator::Return(Some(value)) => {
                let value = self.operand(value, values)?;
                if let Some(output) = self.fragment_output {
                    let float4 = self.type_id(&Type::Vector {
                        element: Box::new(Type::Float { bits: 32 }),
                        lanes: 4,
                    })?;
                    let extracted = self
                        .builder
                        .composite_extract(float4, None, value, [0])
                        .map_err(build_error)?;
                    self.builder
                        .store(output, extracted, None, [])
                        .map_err(build_error)?;
                    self.builder.ret().map_err(build_error)
                } else {
                    self.builder.ret_value(value).map_err(build_error)
                }
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
            Operand::Value(value) => values
                .get(value)
                .copied()
                .ok_or_else(|| emission("operand refers to a missing value")),
            Operand::Integer { ty, value } => self.constant(ty, *value),
            Operand::Float { ty, bits } => self.constant(ty, *bits),
            Operand::Undef(ty) => {
                let ty = self.type_id(ty)?;
                Ok(self.builder.undef(ty, None))
            }
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

    fn type_id(&mut self, ty: &Type) -> Result<u32, TranslateError> {
        if let Some(id) = self.types.get(ty) {
            return Ok(*id);
        }
        let id = match ty {
            Type::Void => self.builder.type_void(),
            Type::Bool => self.builder.type_bool(),
            Type::Int { bits, signed } => {
                self.builder.type_int(u32::from(*bits), u32::from(*signed))
            }
            Type::Float { bits } => self.builder.type_float(u32::from(*bits), None),
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
                let id = self.builder.type_struct(fields);
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
            Type::Texture => {
                let scalar = self.type_id(&Type::Float { bits: 32 })?;
                self.builder
                    .type_image(scalar, Dim::Dim3D, 0, 0, 0, 2, ImageFormat::Unknown, None)
            }
            _ => return Err(emission(format!("unsupported SPIR-V value type {ty:?}"))),
        };
        self.types.insert(ty.clone(), id);
        Ok(id)
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
            Ok(element * u32::from(*lanes))
        }
        Type::Struct(structure) => Ok(structure.size),
        _ => Err(emission("buffer element type has no SPIR-V array stride")),
    }
}

fn loop_shape(function: &Function, header: BlockId) -> Option<(BlockId, BlockId)> {
    let block = function.blocks.iter().find(|block| block.id == header)?;
    let Terminator::CondBranch {
        true_block,
        false_block,
        ..
    } = block.terminator
    else {
        return None;
    };
    let true_back = branches_to(function, true_block, header);
    let false_back = branches_to(function, false_block, header);
    match (true_back, false_back) {
        (true, false) => Some((false_block, true_block)),
        (false, true) => Some((true_block, false_block)),
        _ => None,
    }
}

fn branches_to(function: &Function, from: BlockId, target: BlockId) -> bool {
    function
        .blocks
        .iter()
        .find(|block| block.id == from)
        .is_some_and(
            |block| matches!(block.terminator, Terminator::Branch(value) if value == target),
        )
}

fn instruction_operands(kind: &InstructionKind) -> Vec<&Operand> {
    match kind {
        InstructionKind::Load { pointer } => vec![pointer],
        InstructionKind::Store { pointer, value } => vec![pointer, value],
        InstructionKind::IntegerAdd { lhs, rhs }
        | InstructionKind::IntegerMultiply { lhs, rhs }
        | InstructionKind::IntegerXor { lhs, rhs }
        | InstructionKind::IntegerCompare { lhs, rhs, .. } => vec![lhs, rhs],
        InstructionKind::ElementAddress { pointer, index } => vec![pointer, index],
        InstructionKind::FieldAddress { pointer, .. } => vec![pointer],
        InstructionKind::CompositeExtract { composite, .. } => vec![composite],
        InstructionKind::VectorUnsignedLessThan { lhs, rhs }
        | InstructionKind::FloatDiv { lhs, rhs }
        | InstructionKind::FloatAdd { lhs, rhs }
        | InstructionKind::FloatSubtract { lhs, rhs }
        | InstructionKind::FloatMultiply { lhs, rhs }
        | InstructionKind::FloatDot { lhs, rhs } => vec![lhs, rhs],
        InstructionKind::VectorShuffle { first, second, .. } => vec![first, second],
        InstructionKind::CompositeInsert {
            composite, object, ..
        } => vec![composite, object],
        InstructionKind::FloatClamp { value, min, max } => vec![value, min, max],
        InstructionKind::FloatMix {
            first,
            second,
            weight,
        } => vec![first, second, weight],
        InstructionKind::All { vector }
        | InstructionKind::ConvertUToF { value: vector }
        | InstructionKind::FloatSqrt { value: vector }
        | InstructionKind::FloatExp2 { value: vector } => {
            vec![vector]
        }
        InstructionKind::VectorSplat { value, .. } => vec![value],
        InstructionKind::GlobalInvocationId => vec![],
        InstructionKind::TextureWrite {
            texture,
            coordinate,
            value,
        } => vec![texture, coordinate, value],
        InstructionKind::Phi { incomings } => incomings.iter().map(|(_, value)| value).collect(),
    }
}

fn terminator_operands(terminator: &Terminator) -> Vec<&Operand> {
    match terminator {
        Terminator::CondBranch { condition, .. } => vec![condition],
        Terminator::Return(Some(value)) => vec![value],
        _ => Vec::new(),
    }
}

fn build_error(error: rspirv::dr::Error) -> TranslateError {
    emission(format!("SPIR-V builder rejected the module: {error}"))
}

fn emission(message: impl Into<String>) -> TranslateError {
    TranslateError::one(Diagnostic::new(Phase::SpirvEmission, message))
}
