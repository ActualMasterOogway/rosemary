//! Compare what a Vulkan driver's shader compiler makes of the translator's
//! output under two IR pass pipelines.
//!
//! SPIR-V size says little about what runs, because every driver optimizes a
//! module again. What a pass buys is what the driver's backend reports through
//! `VK_KHR_pipeline_executable_properties` - registers, spills, instruction
//! counts - so this translates every library in a pack twice, compiles a real
//! pipeline from each module on a real adapter, and totals the statistics the
//! driver returns for each.
//!
//! ```sh
//! cargo run --release -p metal_translate --example shader_stats -- \
//!     path/to/shaders_metal_osx.pack [--baseline none] [--candidate default] \
//!     [--device NVIDIA] [--limit N] [--csv stats.csv]
//! ```
//!
//! `--baseline` and `--candidate` take `ROSETTA3_METAL_PASSES` syntax. A
//! pipeline needs more than a module, so each stage is given state which makes
//! it compile:
//!
//! - a kernel is compiled for an 8x8x1 threadgroup;
//! - a vertex function is compiled alone, with rasterization discarded and an
//!   attribute of the shader's own type at every input location;
//! - a fragment function is compiled behind a generated vertex function which
//!   declares exactly its inputs, into a colour attachment of the shader's own
//!   component type at every output location.
//!
//! Only the statistics of the translated stage are reported. The formats are a
//! choice rather than what a game renders into, so the numbers compare two
//! translations with each other; they do not predict a game's.

use std::collections::{BTreeMap, HashMap};
use std::ffi::{CStr, CString};
use std::io::Write;
use std::time::{Duration, Instant};

use ash::vk;
use metal_translate::ir::{ResourceKind, Stage};
use metal_translate::{
    AirInput, EntryArtifact, PassPipeline, ResourceBinding, TargetProfile, WORKGROUP_SIZE_SPEC_IDS,
    metallib_air, translate_with,
};
use rspirv::binary::Assemble;
use rspirv::dr;
use rspirv::spirv::{self, Word};

struct Options {
    pack: String,
    baseline: PassPipeline,
    candidate: PassPipeline,
    device: Option<String>,
    limit: usize,
    csv: Option<String>,
}

fn options() -> Result<Options, String> {
    let mut pack = None;
    let mut options = Options {
        pack: String::new(),
        baseline: PassPipeline::none(),
        candidate: PassPipeline::default(),
        device: None,
        limit: usize::MAX,
        csv: None,
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || args.next().ok_or_else(|| format!("{arg} needs a value"));
        match arg.as_str() {
            "--baseline" => options.baseline = PassPipeline::parse(&value()?)?,
            "--candidate" => options.candidate = PassPipeline::parse(&value()?)?,
            "--device" => options.device = Some(value()?),
            "--limit" => options.limit = value()?.parse().map_err(|_| "--limit is a count")?,
            "--csv" => options.csv = Some(value()?),
            _ if pack.is_none() && !arg.starts_with("--") => pack = Some(arg),
            _ => return Err(format!("unexpected argument {arg}")),
        }
    }
    options.pack = pack.ok_or("expected a shader pack or metallib")?;
    Ok(options)
}

fn describe(pipeline: &PassPipeline) -> String {
    if pipeline.passes().is_empty() {
        return "none".to_owned();
    }
    let names: Vec<_> = pipeline.passes().iter().map(|pass| pass.name()).collect();
    names.join(",")
}

/// What the driver made of one module.
struct Measurement {
    words: usize,
    compile: Duration,
    statistics: BTreeMap<String, f64>,
}

/// The Vulkan objects one measurement makes, destroyed together whether or not
/// it succeeded.
#[derive(Default)]
struct Handles {
    modules: Vec<vk::ShaderModule>,
    set_layouts: Vec<vk::DescriptorSetLayout>,
    layout: vk::PipelineLayout,
    pipeline: vk::Pipeline,
}

impl Handles {
    fn destroy(self, device: &ash::Device) {
        // SAFETY: every handle was made on this device and nothing uses it.
        unsafe {
            if self.pipeline != vk::Pipeline::null() {
                device.destroy_pipeline(self.pipeline, None);
            }
            if self.layout != vk::PipelineLayout::null() {
                device.destroy_pipeline_layout(self.layout, None);
            }
            for layout in self.set_layouts {
                device.destroy_descriptor_set_layout(layout, None);
            }
            for module in self.modules {
                device.destroy_shader_module(module, None);
            }
        }
    }
}

struct Harness {
    _entry: ash::Entry,
    instance: ash::Instance,
    device: ash::Device,
    executables: ash::khr::pipeline_executable_properties::Device,
    target: TargetProfile,
    name: String,
}

impl Drop for Harness {
    fn drop(&mut self) {
        // SAFETY: nothing made on either outlives the harness.
        unsafe {
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}

fn vulkan(what: &str) -> impl Fn(vk::Result) -> String + '_ {
    move |error| format!("{what} failed with {error:?}")
}

impl Harness {
    fn new(filter: Option<&str>) -> Result<Self, String> {
        // SAFETY: loading the system's Vulkan loader.
        let entry = unsafe { ash::Entry::load() }.map_err(|error| error.to_string())?;
        let application = vk::ApplicationInfo::default().api_version(vk::API_VERSION_1_3);
        let info = vk::InstanceCreateInfo::default().application_info(&application);
        // SAFETY: a valid create info.
        let instance =
            unsafe { entry.create_instance(&info, None) }.map_err(vulkan("vkCreateInstance"))?;
        // SAFETY: a live instance.
        let physicals = unsafe { instance.enumerate_physical_devices() }
            .map_err(vulkan("vkEnumeratePhysicalDevices"))?;
        let properties = |physical| {
            // SAFETY: a device this instance enumerated.
            unsafe { instance.get_physical_device_properties(physical) }
        };
        let name_of = |physical| {
            properties(physical)
                .device_name_as_c_str()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default()
        };
        let physical = match filter {
            Some(filter) => physicals
                .iter()
                .copied()
                .find(|&physical| {
                    name_of(physical)
                        .to_lowercase()
                        .contains(&filter.to_lowercase())
                })
                .ok_or_else(|| format!("no Vulkan device is named like {filter}"))?,
            None => physicals
                .iter()
                .copied()
                .max_by_key(|&physical| match properties(physical).device_type {
                    vk::PhysicalDeviceType::DISCRETE_GPU => 3,
                    vk::PhysicalDeviceType::INTEGRATED_GPU => 2,
                    _ => 1,
                })
                .ok_or("Vulkan reports no device")?,
        };
        let name = name_of(physical);
        if properties(physical).api_version < vk::API_VERSION_1_3 {
            return Err(format!("{name} does not support Vulkan 1.3"));
        }

        // Enable everything the device reports, as the runtime does: a
        // translated module may need any of it.
        let mut v11 = vk::PhysicalDeviceVulkan11Features::default();
        let mut v12 = vk::PhysicalDeviceVulkan12Features::default();
        let mut v13 = vk::PhysicalDeviceVulkan13Features::default();
        let mut executable = vk::PhysicalDevicePipelineExecutablePropertiesFeaturesKHR::default();
        let features = {
            let mut features = vk::PhysicalDeviceFeatures2::default()
                .push_next(&mut v11)
                .push_next(&mut v12)
                .push_next(&mut v13)
                .push_next(&mut executable);
            // SAFETY: every chained structure lives across the call.
            unsafe { instance.get_physical_device_features2(physical, &mut features) };
            features.features
        };
        if executable.pipeline_executable_info != vk::TRUE {
            return Err(format!(
                "{name} does not report pipeline executable statistics"
            ));
        }
        let target = TargetProfile {
            shader_int64: features.shader_int64 == vk::TRUE,
            variable_pointers_storage_buffer: v11.variable_pointers_storage_buffer == vk::TRUE,
            variable_pointers: v11.variable_pointers == vk::TRUE,
            ..TargetProfile::vulkan_1_3()
        };

        // SAFETY: a device this instance enumerated.
        let families = unsafe { instance.get_physical_device_queue_family_properties(physical) };
        let family = families
            .iter()
            .position(|family| {
                family
                    .queue_flags
                    .contains(vk::QueueFlags::GRAPHICS | vk::QueueFlags::COMPUTE)
            })
            .ok_or("no graphics and compute queue")? as u32;
        let priorities = [1.0];
        let queues = [vk::DeviceQueueCreateInfo::default()
            .queue_family_index(family)
            .queue_priorities(&priorities)];
        let extensions = [ash::khr::pipeline_executable_properties::NAME.as_ptr()];
        // The query above linked these together; link them again from empty.
        v11.p_next = std::ptr::null_mut();
        v12.p_next = std::ptr::null_mut();
        v13.p_next = std::ptr::null_mut();
        executable.p_next = std::ptr::null_mut();
        let mut enabled = vk::PhysicalDeviceFeatures2::default()
            .features(features)
            .push_next(&mut v11)
            .push_next(&mut v12)
            .push_next(&mut v13)
            .push_next(&mut executable);
        let info = vk::DeviceCreateInfo::default()
            .queue_create_infos(&queues)
            .enabled_extension_names(&extensions)
            .push_next(&mut enabled);
        // SAFETY: every borrowed structure lives across the call.
        let device = unsafe { instance.create_device(physical, &info, None) }
            .map_err(vulkan("vkCreateDevice"))?;
        let executables = ash::khr::pipeline_executable_properties::Device::new(&instance, &device);
        Ok(Self {
            _entry: entry,
            instance,
            device,
            executables,
            target,
            name,
        })
    }

    fn measure(&self, entry: &EntryArtifact) -> Result<Measurement, String> {
        let mut handles = Handles::default();
        let result = self.compile(entry, &mut handles);
        handles.destroy(&self.device);
        result
    }

    fn compile(&self, entry: &EntryArtifact, handles: &mut Handles) -> Result<Measurement, String> {
        let stage = match entry.stage {
            Stage::Kernel => vk::ShaderStageFlags::COMPUTE,
            Stage::Vertex => vk::ShaderStageFlags::VERTEX,
            Stage::Fragment => vk::ShaderStageFlags::FRAGMENT,
            other => return Err(format!("{other:?} functions are not measured")),
        };
        let layout = self.layout(&entry.resources, stage, handles)?;
        let module = self.module(&entry.words, handles)?;
        let name = CString::new(entry.name.as_str()).map_err(|_| "a NUL in an entry name")?;
        let start = Instant::now();
        handles.pipeline = match entry.stage {
            Stage::Kernel => self.compute(module, &name, layout)?,
            Stage::Vertex => self.vertex(module, &name, layout, &entry.words)?,
            _ => self.fragment(module, &name, layout, &entry.words, handles)?,
        };
        let compile = start.elapsed();
        Ok(Measurement {
            words: entry.words.len(),
            compile,
            statistics: self.statistics(handles.pipeline, stage)?,
        })
    }

    fn module(&self, words: &[u32], handles: &mut Handles) -> Result<vk::ShaderModule, String> {
        let info = vk::ShaderModuleCreateInfo::default().code(words);
        // SAFETY: a valid create info over live words.
        let module = unsafe { self.device.create_shader_module(&info, None) }
            .map_err(vulkan("vkCreateShaderModule"))?;
        handles.modules.push(module);
        Ok(module)
    }

    /// The four descriptor sets of the Metal resource ABI, as the runtime
    /// builds them from reflection.
    fn layout(
        &self,
        resources: &[ResourceBinding],
        stages: vk::ShaderStageFlags,
        handles: &mut Handles,
    ) -> Result<vk::PipelineLayout, String> {
        let mut per_set: [Vec<vk::DescriptorSetLayoutBinding>; 4] = Default::default();
        for resource in resources {
            let ty = match resource.kind {
                ResourceKind::Buffer => vk::DescriptorType::STORAGE_BUFFER,
                ResourceKind::Texture if resource.sampled => vk::DescriptorType::SAMPLED_IMAGE,
                ResourceKind::Texture => vk::DescriptorType::STORAGE_IMAGE,
                ResourceKind::Sampler => vk::DescriptorType::SAMPLER,
                other => return Err(format!("{other:?} has no descriptor type")),
            };
            per_set
                .get_mut(resource.descriptor_set as usize)
                .ok_or("a descriptor set outside the Metal ABI")?
                .push(
                    vk::DescriptorSetLayoutBinding::default()
                        .binding(resource.binding)
                        .descriptor_type(ty)
                        .descriptor_count(resource.count)
                        .stage_flags(stages),
                );
        }
        for bindings in &per_set {
            let info = vk::DescriptorSetLayoutCreateInfo::default().bindings(bindings);
            // SAFETY: a valid create info.
            let layout = unsafe { self.device.create_descriptor_set_layout(&info, None) }
                .map_err(vulkan("vkCreateDescriptorSetLayout"))?;
            handles.set_layouts.push(layout);
        }
        let info = vk::PipelineLayoutCreateInfo::default().set_layouts(&handles.set_layouts);
        // SAFETY: as above.
        handles.layout = unsafe { self.device.create_pipeline_layout(&info, None) }
            .map_err(vulkan("vkCreatePipelineLayout"))?;
        Ok(handles.layout)
    }

    fn compute(
        &self,
        module: vk::ShaderModule,
        name: &CStr,
        layout: vk::PipelineLayout,
    ) -> Result<vk::Pipeline, String> {
        let entries: Vec<_> = WORKGROUP_SIZE_SPEC_IDS
            .iter()
            .enumerate()
            .map(|(index, id)| {
                vk::SpecializationMapEntry::default()
                    .constant_id(*id)
                    .offset(index as u32 * 4)
                    .size(4)
            })
            .collect();
        let size: Vec<u8> = [8u32, 8, 1]
            .iter()
            .flat_map(|lane| lane.to_le_bytes())
            .collect();
        let specialization = vk::SpecializationInfo::default()
            .map_entries(&entries)
            .data(&size);
        let stage = vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::COMPUTE)
            .module(module)
            .name(name)
            .specialization_info(&specialization);
        let info = vk::ComputePipelineCreateInfo::default()
            .flags(vk::PipelineCreateFlags::CAPTURE_STATISTICS_KHR)
            .stage(stage)
            .layout(layout);
        // SAFETY: every borrowed structure lives across the call.
        unsafe {
            self.device
                .create_compute_pipelines(vk::PipelineCache::null(), &[info], None)
        }
        .map(|pipelines| pipelines[0])
        .map_err(|(_, error)| format!("vkCreateComputePipelines failed with {error:?}"))
    }

    fn vertex(
        &self,
        module: vk::ShaderModule,
        name: &CStr,
        layout: vk::PipelineLayout,
        words: &[u32],
    ) -> Result<vk::Pipeline, String> {
        let spirv = dr::load_words(words).map_err(|error| error.to_string())?;
        let definitions = definitions(&spirv);
        let mut attributes = Vec::new();
        for input in interface(&spirv, spirv::StorageClass::Input) {
            let (Some(location), None) = (input.location, input.builtin) else {
                continue;
            };
            let Some(shape) = shape(&definitions, input.pointee) else {
                continue;
            };
            let Some(format) = attribute_format(shape) else {
                continue;
            };
            for column in 0..shape.columns {
                attributes.push(
                    vk::VertexInputAttributeDescription::default()
                        .location(location + column)
                        .binding(0)
                        .format(format),
                );
            }
        }
        let stages = [vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::VERTEX)
            .module(module)
            .name(name)];
        self.graphics(&stages, &attributes, &[], vk::Format::UNDEFINED, layout)
    }

    fn fragment(
        &self,
        module: vk::ShaderModule,
        name: &CStr,
        layout: vk::PipelineLayout,
        words: &[u32],
        handles: &mut Handles,
    ) -> Result<vk::Pipeline, String> {
        let spirv = dr::load_words(words).map_err(|error| error.to_string())?;
        let definitions = definitions(&spirv);
        let inputs = interface(&spirv, spirv::StorageClass::Input);
        let vertex = self.module(&passthrough_vertex(&definitions, &inputs)?, handles)?;

        let mut colors = Vec::new();
        let mut depth = vk::Format::UNDEFINED;
        for output in interface(&spirv, spirv::StorageClass::Output) {
            if output.builtin == Some(spirv::BuiltIn::FragDepth) {
                depth = vk::Format::D32_SFLOAT;
            }
            let (Some(location), None) = (output.location, output.builtin) else {
                continue;
            };
            let shape = shape(&definitions, output.pointee)
                .ok_or("a fragment output of a type no attachment has")?;
            let index = location as usize;
            if colors.len() <= index {
                colors.resize(index + 1, vk::Format::UNDEFINED);
            }
            colors[index] = color_format(shape);
        }
        let stages = [
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::VERTEX)
                .module(vertex)
                .name(c"main"),
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::FRAGMENT)
                .module(module)
                .name(name),
        ];
        self.graphics(&stages, &[], &colors, depth, layout)
    }

    fn graphics(
        &self,
        stages: &[vk::PipelineShaderStageCreateInfo<'_>],
        attributes: &[vk::VertexInputAttributeDescription],
        colors: &[vk::Format],
        depth: vk::Format,
        layout: vk::PipelineLayout,
    ) -> Result<vk::Pipeline, String> {
        let vertex_only = stages.len() == 1;
        let streams = [vk::VertexInputBindingDescription::default()
            .binding(0)
            .stride(256)
            .input_rate(vk::VertexInputRate::VERTEX)];
        let vertex_input = vk::PipelineVertexInputStateCreateInfo::default()
            .vertex_binding_descriptions(if attributes.is_empty() { &[] } else { &streams })
            .vertex_attribute_descriptions(attributes);
        let assembly = vk::PipelineInputAssemblyStateCreateInfo::default()
            .topology(vk::PrimitiveTopology::TRIANGLE_LIST);
        let viewport = vk::PipelineViewportStateCreateInfo::default()
            .viewport_count(1)
            .scissor_count(1);
        let raster = vk::PipelineRasterizationStateCreateInfo::default()
            .rasterizer_discard_enable(vertex_only)
            .polygon_mode(vk::PolygonMode::FILL)
            .cull_mode(vk::CullModeFlags::NONE)
            .front_face(vk::FrontFace::CLOCKWISE)
            .line_width(1.0);
        let multisample = vk::PipelineMultisampleStateCreateInfo::default()
            .rasterization_samples(vk::SampleCountFlags::TYPE_1);
        let writes_depth = depth != vk::Format::UNDEFINED;
        let depth_stencil = vk::PipelineDepthStencilStateCreateInfo::default()
            .depth_test_enable(writes_depth)
            .depth_write_enable(writes_depth)
            .depth_compare_op(vk::CompareOp::ALWAYS);
        let blends: Vec<_> = colors
            .iter()
            .map(|_| {
                vk::PipelineColorBlendAttachmentState::default()
                    .color_write_mask(vk::ColorComponentFlags::RGBA)
            })
            .collect();
        let blend = vk::PipelineColorBlendStateCreateInfo::default().attachments(&blends);
        let dynamic_states = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
        let dynamic = vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic_states);
        let mut rendering = vk::PipelineRenderingCreateInfo::default()
            .color_attachment_formats(colors)
            .depth_attachment_format(depth);
        let info = vk::GraphicsPipelineCreateInfo::default()
            .flags(vk::PipelineCreateFlags::CAPTURE_STATISTICS_KHR)
            .stages(stages)
            .vertex_input_state(&vertex_input)
            .input_assembly_state(&assembly)
            .viewport_state(&viewport)
            .rasterization_state(&raster)
            .multisample_state(&multisample)
            .depth_stencil_state(&depth_stencil)
            .color_blend_state(&blend)
            .dynamic_state(&dynamic)
            .layout(layout)
            .push_next(&mut rendering);
        // SAFETY: every borrowed structure lives across the call.
        unsafe {
            self.device
                .create_graphics_pipelines(vk::PipelineCache::null(), &[info], None)
        }
        .map(|pipelines| pipelines[0])
        .map_err(|(_, error)| format!("vkCreateGraphicsPipelines failed with {error:?}"))
    }

    /// The statistics of every executable which runs `stage`, summed by name.
    fn statistics(
        &self,
        pipeline: vk::Pipeline,
        stage: vk::ShaderStageFlags,
    ) -> Result<BTreeMap<String, f64>, String> {
        let info = vk::PipelineInfoKHR::default().pipeline(pipeline);
        // SAFETY: a pipeline made with CAPTURE_STATISTICS on this device.
        let executables = unsafe { self.executables.get_pipeline_executable_properties(&info) }
            .map_err(vulkan("vkGetPipelineExecutablePropertiesKHR"))?;
        let mut totals = BTreeMap::new();
        for (index, executable) in executables.iter().enumerate() {
            if !executable.stages.intersects(stage) {
                continue;
            }
            let info = vk::PipelineExecutableInfoKHR::default()
                .pipeline(pipeline)
                .executable_index(index as u32);
            // SAFETY: as above, for an executable it reported.
            let statistics = unsafe { self.executables.get_pipeline_executable_statistics(&info) }
                .map_err(vulkan("vkGetPipelineExecutableStatisticsKHR"))?;
            for statistic in statistics {
                let name = statistic
                    .name_as_c_str()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default();
                // SAFETY: `format` names the union member which is set.
                let value = unsafe {
                    match statistic.format {
                        vk::PipelineExecutableStatisticFormatKHR::BOOL32 => {
                            f64::from(statistic.value.b32)
                        }
                        vk::PipelineExecutableStatisticFormatKHR::INT64 => {
                            statistic.value.i64 as f64
                        }
                        vk::PipelineExecutableStatisticFormatKHR::UINT64 => {
                            statistic.value.u64 as f64
                        }
                        _ => statistic.value.f64,
                    }
                };
                *totals.entry(name).or_insert(0.0) += value;
            }
        }
        Ok(totals)
    }
}

// ------------------------------------------------------------ interfaces

struct Variable {
    pointee: Word,
    location: Option<u32>,
    component: Option<u32>,
    builtin: Option<spirv::BuiltIn>,
}

fn definitions(module: &dr::Module) -> HashMap<Word, &dr::Instruction> {
    module
        .types_global_values
        .iter()
        .filter_map(|instruction| Some((instruction.result_id?, instruction)))
        .collect()
}

/// The module's variables of one storage class, with the decorations that
/// place them.
fn interface(module: &dr::Module, class: spirv::StorageClass) -> Vec<Variable> {
    let definitions = definitions(module);
    let mut decorations: HashMap<Word, Vec<&dr::Instruction>> = HashMap::new();
    for annotation in &module.annotations {
        if let (spirv::Op::Decorate, Some(dr::Operand::IdRef(target))) =
            (annotation.class.opcode, annotation.operands.first())
        {
            decorations.entry(*target).or_default().push(annotation);
        }
    }
    module
        .types_global_values
        .iter()
        .filter(|instruction| instruction.class.opcode == spirv::Op::Variable)
        .filter(|variable| variable.operands.first() == Some(&dr::Operand::StorageClass(class)))
        .filter_map(|variable| {
            let pointer = definitions.get(&variable.result_type?)?;
            let Some(dr::Operand::IdRef(pointee)) = pointer.operands.get(1) else {
                return None;
            };
            let mut found = Variable {
                pointee: *pointee,
                location: None,
                component: None,
                builtin: None,
            };
            for decoration in decorations.get(&variable.result_id?).into_iter().flatten() {
                match (decoration.operands.get(1), decoration.operands.get(2)) {
                    (
                        Some(dr::Operand::Decoration(spirv::Decoration::Location)),
                        Some(dr::Operand::LiteralBit32(location)),
                    ) => found.location = Some(*location),
                    (
                        Some(dr::Operand::Decoration(spirv::Decoration::Component)),
                        Some(dr::Operand::LiteralBit32(component)),
                    ) => found.component = Some(*component),
                    (
                        Some(dr::Operand::Decoration(spirv::Decoration::BuiltIn)),
                        Some(dr::Operand::BuiltIn(builtin)),
                    ) => found.builtin = Some(*builtin),
                    _ => {}
                }
            }
            Some(found)
        })
        .collect()
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Float,
    Signed,
    Unsigned,
}

#[derive(Clone, Copy)]
struct Shape {
    kind: Kind,
    width: u32,
    lanes: u32,
    columns: u32,
}

fn literal(instruction: &dr::Instruction, index: usize) -> Option<u32> {
    match instruction.operands.get(index) {
        Some(dr::Operand::LiteralBit32(value)) => Some(*value),
        _ => None,
    }
}

fn id(instruction: &dr::Instruction, index: usize) -> Option<Word> {
    match instruction.operands.get(index) {
        Some(dr::Operand::IdRef(id)) => Some(*id),
        _ => None,
    }
}

/// A numeric scalar, vector or matrix type; anything else has no format.
fn shape(definitions: &HashMap<Word, &dr::Instruction>, ty: Word) -> Option<Shape> {
    let instruction = definitions.get(&ty)?;
    match instruction.class.opcode {
        spirv::Op::TypeFloat => Some(Shape {
            kind: Kind::Float,
            width: literal(instruction, 0)?,
            lanes: 1,
            columns: 1,
        }),
        spirv::Op::TypeInt => Some(Shape {
            kind: if literal(instruction, 1)? == 1 {
                Kind::Signed
            } else {
                Kind::Unsigned
            },
            width: literal(instruction, 0)?,
            lanes: 1,
            columns: 1,
        }),
        spirv::Op::TypeVector => Some(Shape {
            lanes: literal(instruction, 1)?,
            ..shape(definitions, id(instruction, 0)?)?
        }),
        spirv::Op::TypeMatrix => Some(Shape {
            columns: literal(instruction, 1)?,
            ..shape(definitions, id(instruction, 0)?)?
        }),
        _ => None,
    }
}

fn attribute_format(shape: Shape) -> Option<vk::Format> {
    use vk::Format as F;
    let formats = match (shape.kind, shape.width) {
        (Kind::Float, 32) => [
            F::R32_SFLOAT,
            F::R32G32_SFLOAT,
            F::R32G32B32_SFLOAT,
            F::R32G32B32A32_SFLOAT,
        ],
        (Kind::Float, 16) => [
            F::R16_SFLOAT,
            F::R16G16_SFLOAT,
            F::R16G16B16_SFLOAT,
            F::R16G16B16A16_SFLOAT,
        ],
        (Kind::Signed, 32) => [
            F::R32_SINT,
            F::R32G32_SINT,
            F::R32G32B32_SINT,
            F::R32G32B32A32_SINT,
        ],
        (Kind::Unsigned, 32) => [
            F::R32_UINT,
            F::R32G32_UINT,
            F::R32G32B32_UINT,
            F::R32G32B32A32_UINT,
        ],
        (Kind::Signed, 16) => [
            F::R16_SINT,
            F::R16G16_SINT,
            F::R16G16B16_SINT,
            F::R16G16B16A16_SINT,
        ],
        (Kind::Unsigned, 16) => [
            F::R16_UINT,
            F::R16G16_UINT,
            F::R16G16B16_UINT,
            F::R16G16B16A16_UINT,
        ],
        (Kind::Signed, 8) => [F::R8_SINT, F::R8G8_SINT, F::R8G8B8_SINT, F::R8G8B8A8_SINT],
        (Kind::Unsigned, 8) => [F::R8_UINT, F::R8G8_UINT, F::R8G8B8_UINT, F::R8G8B8A8_UINT],
        _ => return None,
    };
    formats.get(shape.lanes.checked_sub(1)? as usize).copied()
}

/// A four-component attachment of the output's component type. A half-float
/// target is what a game's colour passes mostly are.
fn color_format(shape: Shape) -> vk::Format {
    match (shape.kind, shape.width) {
        (Kind::Float, _) => vk::Format::R16G16B16A16_SFLOAT,
        (Kind::Signed, 8 | 16) => vk::Format::R16G16B16A16_SINT,
        (Kind::Signed, _) => vk::Format::R32G32B32A32_SINT,
        (Kind::Unsigned, 8 | 16) => vk::Format::R16G16B16A16_UINT,
        (Kind::Unsigned, _) => vk::Format::R32G32B32A32_UINT,
    }
}

/// A vertex function which declares every input `inputs` names as an output of
/// the same type at the same location, and writes nothing but the position.
fn passthrough_vertex(
    definitions: &HashMap<Word, &dr::Instruction>,
    inputs: &[Variable],
) -> Result<Vec<u32>, String> {
    let build = |error: rspirv::dr::Error| error.to_string();
    let mut builder = dr::Builder::new();
    builder.set_version(1, 3);
    builder.memory_model(spirv::AddressingModel::Logical, spirv::MemoryModel::GLSL450);
    let mut capabilities = vec![spirv::Capability::Shader];
    let mut copied = HashMap::new();
    let mut interface = Vec::new();
    for input in inputs {
        let (Some(location), None) = (input.location, input.builtin) else {
            continue;
        };
        let ty = copy_type(
            definitions,
            input.pointee,
            &mut builder,
            &mut copied,
            &mut capabilities,
        )?;
        let pointer = builder.type_pointer(None, spirv::StorageClass::Output, ty);
        let variable = builder.variable(pointer, None, spirv::StorageClass::Output, None);
        builder.decorate(
            variable,
            spirv::Decoration::Location,
            [dr::Operand::LiteralBit32(location)],
        );
        if let Some(component) = input.component {
            builder.decorate(
                variable,
                spirv::Decoration::Component,
                [dr::Operand::LiteralBit32(component)],
            );
        }
        interface.push(variable);
    }
    let float = builder.type_float(32, None);
    let vec4 = builder.type_vector(float, 4);
    let pointer = builder.type_pointer(None, spirv::StorageClass::Output, vec4);
    let position = builder.variable(pointer, None, spirv::StorageClass::Output, None);
    builder.decorate(
        position,
        spirv::Decoration::BuiltIn,
        [dr::Operand::BuiltIn(spirv::BuiltIn::Position)],
    );
    interface.push(position);
    let zero = builder.constant_null(vec4);
    let void = builder.type_void();
    let signature = builder.type_function(void, []);
    let main = builder
        .begin_function(void, None, spirv::FunctionControl::NONE, signature)
        .map_err(build)?;
    builder.begin_block(None).map_err(build)?;
    builder.store(position, zero, None, []).map_err(build)?;
    builder.ret().map_err(build)?;
    builder.end_function().map_err(build)?;
    builder.entry_point(spirv::ExecutionModel::Vertex, main, "main", interface);
    for capability in capabilities {
        builder.capability(capability);
    }
    Ok(builder.module().assemble())
}

fn copy_type(
    definitions: &HashMap<Word, &dr::Instruction>,
    ty: Word,
    builder: &mut dr::Builder,
    copied: &mut HashMap<Word, Word>,
    capabilities: &mut Vec<spirv::Capability>,
) -> Result<Word, String> {
    if let Some(id) = copied.get(&ty) {
        return Ok(*id);
    }
    let malformed = || "a fragment input has a malformed type".to_owned();
    let instruction = definitions.get(&ty).ok_or_else(malformed)?;
    let mut need = |capability| {
        if !capabilities.contains(&capability) {
            capabilities.push(capability);
        }
    };
    let result = match instruction.class.opcode {
        spirv::Op::TypeFloat => {
            let width = literal(instruction, 0).ok_or_else(malformed)?;
            match width {
                16 => {
                    need(spirv::Capability::Float16);
                    need(spirv::Capability::StorageInputOutput16);
                }
                64 => need(spirv::Capability::Float64),
                _ => {}
            }
            builder.type_float(width, None)
        }
        spirv::Op::TypeInt => {
            let width = literal(instruction, 0).ok_or_else(malformed)?;
            match width {
                16 => {
                    need(spirv::Capability::Int16);
                    need(spirv::Capability::StorageInputOutput16);
                }
                64 => need(spirv::Capability::Int64),
                _ => {}
            }
            builder.type_int(width, literal(instruction, 1).ok_or_else(malformed)?)
        }
        spirv::Op::TypeVector | spirv::Op::TypeMatrix => {
            let element = id(instruction, 0).ok_or_else(malformed)?;
            let element = copy_type(definitions, element, builder, copied, capabilities)?;
            let count = literal(instruction, 1).ok_or_else(malformed)?;
            if instruction.class.opcode == spirv::Op::TypeVector {
                builder.type_vector(element, count)
            } else {
                builder.type_matrix(element, count)
            }
        }
        spirv::Op::TypeArray => {
            let element = id(instruction, 0).ok_or_else(malformed)?;
            let element = copy_type(definitions, element, builder, copied, capabilities)?;
            let length = definitions
                .get(&id(instruction, 1).ok_or_else(malformed)?)
                .and_then(|constant| literal(constant, 0))
                .ok_or_else(malformed)?;
            let uint = builder.type_int(32, 0);
            let length = builder.constant_bit32(uint, length);
            builder.type_array(element, length)
        }
        op => {
            return Err(format!(
                "a fragment input of type {op:?} cannot be passed through"
            ));
        }
    };
    copied.insert(ty, result);
    Ok(result)
}

// ---------------------------------------------------------------- report

#[derive(Default)]
struct Total {
    baseline: f64,
    candidate: f64,
    /// Entry points whose value differs between the two.
    differing: usize,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let options = options()?;
    let pack = std::fs::read(&options.pack)?;
    let harness = Harness::new(options.device.as_deref())?;
    let offsets: Vec<usize> = pack
        .windows(4)
        .enumerate()
        .filter_map(|(offset, window)| (window == b"MTLB").then_some(offset))
        .take(options.limit)
        .collect();
    let mut csv = options
        .csv
        .as_deref()
        .map(std::fs::File::create)
        .transpose()?;
    if let Some(csv) = &mut csv {
        writeln!(csv, "library,entry,stage,statistic,baseline,candidate")?;
    }

    let mut totals: BTreeMap<String, Total> = BTreeMap::new();
    let mut compared = 0usize;
    let mut failures = [0usize; 2];
    let mut first_failures = Vec::new();
    for (ordinal, offset) in offsets.iter().enumerate() {
        let Ok(air) = metallib_air(&pack[*offset..]) else {
            continue;
        };
        let sides = [&options.baseline, &options.candidate].map(|pipeline| {
            translate_with(AirInput::Bitcode(air), &harness.target, pipeline).map(|library| {
                library
                    .entries
                    .iter()
                    .map(|entry| (entry.name.clone(), entry.stage, harness.measure(entry)))
                    .collect::<Vec<_>>()
            })
        });
        let [baseline, candidate] = sides;
        let (baseline, candidate) = match (baseline, candidate) {
            (Ok(baseline), Ok(candidate)) => (baseline, candidate),
            (baseline, candidate) => {
                for (side, result) in [baseline.err(), candidate.err()].into_iter().enumerate() {
                    if let Some(error) = result {
                        failures[side] += 1;
                        first_failures.push(format!("library {ordinal}: {error}"));
                    }
                }
                continue;
            }
        };
        for ((name, stage, before), (_, _, after)) in baseline.into_iter().zip(candidate) {
            let (before, after) = match (before, after) {
                (Ok(before), Ok(after)) => (before, after),
                (before, after) => {
                    for (side, result) in [before.err(), after.err()].into_iter().enumerate() {
                        if let Some(error) = result {
                            failures[side] += 1;
                            first_failures.push(format!("library {ordinal} {name}: {error}"));
                        }
                    }
                    continue;
                }
            };
            compared += 1;
            let mut rows = vec![
                (
                    "SPIR-V words".to_owned(),
                    before.words as f64,
                    after.words as f64,
                ),
                (
                    "compile time (ms)".to_owned(),
                    before.compile.as_secs_f64() * 1e3,
                    after.compile.as_secs_f64() * 1e3,
                ),
            ];
            let names = before.statistics.keys().chain(after.statistics.keys());
            let mut seen = std::collections::BTreeSet::new();
            for statistic in names {
                // A driver's pipeline hash differs whenever anything does and
                // sums to nothing meaningful.
                if !seen.insert(statistic) || statistic.to_lowercase().contains("hash") {
                    continue;
                }
                rows.push((
                    statistic.clone(),
                    before.statistics.get(statistic).copied().unwrap_or(0.0),
                    after.statistics.get(statistic).copied().unwrap_or(0.0),
                ));
            }
            for (statistic, before, after) in rows {
                if let Some(csv) = &mut csv {
                    writeln!(
                        csv,
                        "{ordinal},{name},{stage:?},{statistic},{before},{after}"
                    )?;
                }
                let total = totals.entry(statistic).or_default();
                total.baseline += before;
                total.candidate += after;
                total.differing += usize::from(before != after);
            }
        }
    }

    println!("device:    {}", harness.name);
    println!("baseline:  {}", describe(&options.baseline));
    println!("candidate: {}", describe(&options.candidate));
    println!(
        "{compared} entry points compared across {} libraries; failures: baseline {}, candidate {}",
        offsets.len(),
        failures[0],
        failures[1]
    );
    for failure in first_failures.iter().take(8) {
        println!("  {failure}");
    }
    println!();
    println!(
        "{:<36} {:>16} {:>16} {:>9} {:>8}",
        "statistic", "baseline", "candidate", "change", "differ"
    );
    for (statistic, total) in &totals {
        let change = if total.baseline == 0.0 {
            0.0
        } else {
            (total.candidate - total.baseline) / total.baseline * 100.0
        };
        println!(
            "{statistic:<36} {:>16.1} {:>16.1} {change:>+8.2}% {:>8}",
            total.baseline, total.candidate, total.differing
        );
    }
    Ok(())
}
