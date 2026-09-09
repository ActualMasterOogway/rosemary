use std::collections::{BTreeMap, BTreeSet};

use crate::{Diagnostic, Phase, TranslateError};

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ValueId(pub u32);

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct BlockId(pub u32);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Stage {
    Vertex,
    Fragment,
    Kernel,
    Visible,
    Intersection,
    Mesh,
    Object,
    Tile,
}

impl Stage {
    pub fn ordinal(self) -> u32 {
        match self {
            Self::Kernel => 0,
            Self::Vertex => 1,
            Self::Fragment => 2,
            Self::Visible => 3,
            Self::Intersection => 4,
            Self::Mesh => 5,
            Self::Object => 6,
            Self::Tile => 7,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum AddressSpace {
    Thread,
    Device,
    Constant,
    Threadgroup,
    ThreadgroupImageblock,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Access {
    /// Metal `access::sample`: read through a sampler. Only textures have it.
    Sample,
    Read,
    Write,
    ReadWrite,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResourceKind {
    Buffer,
    Texture,
    Sampler,
    AccelerationStructure,
    VisibleFunctionTable,
    IntersectionFunctionTable,
    ArgumentBuffer,
    Imageblock,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum Type {
    Void,
    Bool,
    Int {
        bits: u8,
        signed: bool,
    },
    Float {
        bits: u8,
    },
    Vector {
        element: Box<Type>,
        lanes: u8,
    },
    Matrix {
        column: Box<Type>,
        columns: u8,
    },
    Array {
        element: Box<Type>,
        count: u32,
    },
    Struct(StructType),
    Pointer {
        space: AddressSpace,
        pointee: Box<Type>,
    },
    Texture(TextureType),
    Sampler,
    AccelerationStructure,
    VisibleFunctionTable,
    IntersectionFunctionTable,
    Imageblock,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum TextureDimension {
    D1,
    D2,
    D3,
    Cube,
}

/// A Metal texture type. Metal spells the element type, dimensionality and
/// access mode in the type itself; Vulkan splits the same information across
/// an image type, a descriptor type and a capability.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct TextureType {
    pub dimension: TextureDimension,
    pub array: bool,
    pub multisampled: bool,
    /// A `depthNd` texture, which Metal only allows to be sampled as depth.
    pub depth: bool,
    /// The scalar type of one texel component.
    pub texel: Box<Type>,
    pub access: Access,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct StructType {
    pub fields: Vec<StructField>,
    pub size: u32,
    pub alignment: u32,
    pub packed: bool,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct StructField {
    pub name: Option<String>,
    pub ty: Type,
    pub offset: u32,
}

impl Type {
    pub fn u32() -> Self {
        Self::Int {
            bits: 32,
            signed: false,
        }
    }

    pub fn i32() -> Self {
        Self::Int {
            bits: 32,
            signed: true,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Resource {
    pub argument: u32,
    pub kind: ResourceKind,
    pub index: u32,
    pub count: u32,
    pub access: Access,
    pub ty: Type,
    pub name: Option<String>,
}

/// How a fragment stage input is interpolated across a primitive.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Interpolation {
    pub sampling: Sampling,
    pub perspective: bool,
    pub flat: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Sampling {
    Center,
    Centroid,
    Sample,
}

/// A Metal shader built-in. These name Metal's semantics; the SPIR-V built-in
/// each maps to is a property of emission.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Builtin {
    /// `[[position]]`: a vertex output, and the fragment stage's window
    /// coordinate input.
    Position,
    PointSize,
    ClipDistance,
    VertexId,
    InstanceId,
    BaseVertex,
    BaseInstance,
    FrontFacing,
    PointCoord,
    SampleId,
    SampleMask,
    Depth,
    RenderTargetArrayIndex,
    ViewportArrayIndex,
    ThreadPositionInGrid,
    ThreadPositionInThreadgroup,
    ThreadgroupPositionInGrid,
    ThreadgroupsPerGrid,
    ThreadIndexInThreadgroup,
    ThreadsPerThreadgroup,
}

/// Where one stage input or output is delivered.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StageBinding {
    /// A user-defined attribute at a Metal location index.
    Location {
        location: u32,
        interpolation: Option<Interpolation>,
    },
    /// A fragment colour attachment.
    RenderTarget {
        index: u32,
    },
    Builtin(Builtin),
}

/// One entry-point parameter delivered by the stage rather than by a resource
/// binding.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StageInput {
    pub argument: u32,
    pub binding: StageBinding,
    pub name: Option<String>,
}

/// One component of an entry point's returned value.
///
/// `field` selects a member of a returned structure; `None` means the whole
/// result is this single output.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StageOutput {
    pub field: Option<u32>,
    pub binding: StageBinding,
    pub name: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EntryPoint {
    pub function: usize,
    pub stage: Stage,
    pub resources: Vec<Resource>,
    pub inputs: Vec<StageInput>,
    pub outputs: Vec<StageOutput>,
}

/// A module-scope variable.
///
/// Metal puts a shader's constant lookup tables and its `threadgroup` arrays
/// outside every function, so they belong to the module rather than to the
/// entry point which reads them.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GlobalVariable {
    pub name: String,
    pub space: AddressSpace,
    /// The type of what the variable holds, not the pointer to it.
    pub ty: Type,
    /// `None` for a `threadgroup` variable, which has no initial value.
    pub initializer: Option<Operand>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Module {
    pub functions: Vec<Function>,
    pub entries: Vec<EntryPoint>,
    pub globals: Vec<GlobalVariable>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Function {
    pub name: String,
    pub result: Type,
    pub parameters: Vec<Parameter>,
    pub values: Vec<Type>,
    pub blocks: Vec<Block>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Parameter {
    pub value: ValueId,
    pub name: String,
    pub ty: Type,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Block {
    pub id: BlockId,
    pub name: String,
    pub instructions: Vec<Instruction>,
    pub terminator: Terminator,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Instruction {
    pub result: Option<ValueId>,
    pub kind: InstructionKind,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum Operand {
    Bool(bool),
    Value(ValueId),
    Integer {
        ty: Type,
        value: u64,
    },
    Float {
        ty: Type,
        bits: u64,
    },
    /// A constant vector, array or structure. Metal spells these as values,
    /// and some of its operations, such as a sampling offset, require one.
    Composite {
        ty: Type,
        elements: Vec<Operand>,
    },
    /// A pointer to one of the module's global variables. The type is the
    /// pointer's, carried here so an operand's type never needs the module.
    Global {
        index: u32,
        ty: Type,
    },
    Undef(Type),
}

impl Operand {
    pub fn u32(value: u32) -> Self {
        Self::Integer {
            ty: Type::u32(),
            value: u64::from(value),
        }
    }

    pub fn f32(value: f32) -> Self {
        Self::Float {
            ty: Type::Float { bits: 32 },
            bits: u64::from(value.to_bits()),
        }
    }
}

/// Integer comparison predicates, matching LLVM's `icmp` set.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IntegerPredicate {
    Equal,
    NotEqual,
    UnsignedLessThan,
    UnsignedLessEqual,
    UnsignedGreaterThan,
    UnsignedGreaterEqual,
    SignedLessThan,
    SignedLessEqual,
    SignedGreaterThan,
    SignedGreaterEqual,
}

/// Floating comparison predicates, matching LLVM's `fcmp` set. Ordered
/// comparisons are false when either operand is NaN; unordered are true.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FloatPredicate {
    OrderedEqual,
    OrderedNotEqual,
    OrderedLessThan,
    OrderedLessEqual,
    OrderedGreaterThan,
    OrderedGreaterEqual,
    Ordered,
    UnorderedEqual,
    UnorderedNotEqual,
    UnorderedLessThan,
    UnorderedLessEqual,
    UnorderedGreaterThan,
    UnorderedGreaterEqual,
    Unordered,
}

/// Elementwise integer arithmetic. Signedness is carried by the operation
/// rather than by the operand type: Metal's `uint`/`int` distinction is
/// already lost in AIR, where LLVM picks `udiv`/`sdiv` instead.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IntegerBinaryOp {
    Add,
    Subtract,
    Multiply,
    DivideUnsigned,
    DivideSigned,
    RemainderUnsigned,
    RemainderSigned,
    ShiftLeft,
    ShiftRightLogical,
    ShiftRightArithmetic,
    And,
    Or,
    Xor,
    MinimumSigned,
    MinimumUnsigned,
    MaximumSigned,
    MaximumUnsigned,
}

/// Elementwise integer math library calls with one operand.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IntegerUnaryOp {
    AbsoluteSigned,
    SignSigned,
    PopulationCount,
    Not,
}

/// Elementwise floating arithmetic and two-operand math library calls.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FloatBinaryOp {
    Add,
    Subtract,
    Multiply,
    Divide,
    Remainder,
    Minimum,
    Maximum,
    Power,
    Atan2,
    Step,
}

/// Elementwise floating math library calls with one operand.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FloatUnaryOp {
    Negate,
    Absolute,
    Sign,
    Sqrt,
    InverseSqrt,
    Exp,
    Exp2,
    Log,
    Log2,
    Sin,
    Cos,
    Tan,
    Asin,
    Acos,
    Atan,
    Floor,
    Ceil,
    Round,
    Truncate,
    Fract,
    /// Screen-space derivatives, valid only in a fragment entry point.
    DerivativeX,
    DerivativeY,
    DerivativeWidth,
}

/// Numeric conversions. Each keeps the source interpretation explicit so the
/// backend never has to infer signedness from a result type alone.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConvertOp {
    UnsignedToFloat,
    SignedToFloat,
    FloatToUnsigned,
    FloatToSigned,
    /// Widen an integer, filling new bits with zero.
    ZeroExtend,
    /// Widen an integer, replicating its sign bit.
    SignExtend,
    /// Narrow an integer, discarding high bits.
    IntegerTruncate,
    /// Change floating precision in either direction.
    FloatResize,
    /// Reinterpret the bits of an equally sized value.
    Bitcast,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InstructionKind {
    GlobalInvocationId,
    /// A function-scope variable: LLVM's `alloca`, which Metal spells as a
    /// `thread` local. The type is what the variable holds.
    LocalVariable {
        ty: Type,
    },
    Load {
        pointer: Operand,
    },
    Store {
        pointer: Operand,
        value: Operand,
    },
    IntegerBinary {
        op: IntegerBinaryOp,
        lhs: Operand,
        rhs: Operand,
    },
    IntegerCompare {
        predicate: IntegerPredicate,
        lhs: Operand,
        rhs: Operand,
    },
    FloatCompare {
        predicate: FloatPredicate,
        lhs: Operand,
        rhs: Operand,
    },
    ElementAddress {
        pointer: Operand,
        index: Operand,
    },
    FieldAddress {
        pointer: Operand,
        indices: Vec<u32>,
    },
    BufferAddress {
        pointer: Operand,
        indices: Vec<Operand>,
    },
    CompositeExtract {
        composite: Operand,
        index: u32,
    },
    All {
        vector: Operand,
    },
    Any {
        vector: Operand,
    },
    Convert {
        op: ConvertOp,
        value: Operand,
    },
    Select {
        condition: Operand,
        on_true: Operand,
        on_false: Operand,
    },
    FloatBinary {
        op: FloatBinaryOp,
        lhs: Operand,
        rhs: Operand,
    },
    FloatUnary {
        op: FloatUnaryOp,
        value: Operand,
    },
    /// Fused multiply-add: `first * second + addend`.
    FloatFma {
        first: Operand,
        second: Operand,
        addend: Operand,
    },
    VectorShuffle {
        first: Operand,
        second: Operand,
        components: Vec<u32>,
    },
    CompositeInsert {
        composite: Operand,
        object: Operand,
        index: u32,
    },
    IntegerUnary {
        op: IntegerUnaryOp,
        value: Operand,
    },
    FloatClamp {
        value: Operand,
        min: Operand,
        max: Operand,
    },
    IntegerClamp {
        signed: bool,
        value: Operand,
        min: Operand,
        max: Operand,
    },
    FloatMix {
        first: Operand,
        second: Operand,
        weight: Operand,
    },
    FloatDot {
        lhs: Operand,
        rhs: Operand,
    },
    VectorSplat {
        value: Operand,
        lanes: u8,
    },
    /// Read one texel through a Metal sampler.
    ///
    /// Metal keeps the array slice, the sampling offset and an explicit level
    /// of detail as separate arguments; combining the coordinate with the
    /// slice is a Vulkan detail and belongs to emission.
    TextureSample {
        texture: Operand,
        sampler: Operand,
        coordinate: Operand,
        array_index: Option<Operand>,
        offset: Option<Operand>,
        /// An explicit level of detail. Metal samples with an implicit level
        /// when this is absent, which is only valid in a fragment stage.
        lod: Option<Operand>,
        /// `gather` selects one component of the four sampled texels.
        gather: Option<u32>,
    },
    /// Read one texel by integer coordinate, without a sampler.
    TextureRead {
        texture: Operand,
        coordinate: Operand,
        array_index: Option<Operand>,
        lod: Option<Operand>,
        /// The sample index of a multisampled texture.
        sample: Option<Operand>,
    },
    /// Store one texel through a writable Metal texture resource.
    ///
    /// Coordinates and texels remain explicit SSA values: Metal's texture
    /// namespace is represented by the resource, not by a Vulkan descriptor.
    TextureWrite {
        texture: Operand,
        coordinate: Operand,
        array_index: Option<Operand>,
        value: Operand,
    },
    /// One component of a texture's dimensions at a level of detail.
    TextureQuerySize {
        texture: Operand,
        lod: Option<Operand>,
        component: u32,
    },
    /// Metal `discard_fragment()`. It does not terminate the block.
    DiscardFragment,
    Phi {
        incomings: Vec<(BlockId, Operand)>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Terminator {
    Branch(BlockId),
    CondBranch {
        condition: Operand,
        true_block: BlockId,
        false_block: BlockId,
    },
    Return(Option<Operand>),
    Unreachable,
}

impl Module {
    pub fn verify(&self) -> Result<(), TranslateError> {
        let mut diagnostics = Vec::new();
        for entry in &self.entries {
            let Some(function) = self.functions.get(entry.function) else {
                diagnostics.push(Diagnostic::new(
                    Phase::IrVerification,
                    "entry point refers to a missing function",
                ));
                continue;
            };
            let mut bindings = BTreeSet::new();
            for resource in &entry.resources {
                if resource.count == 0 {
                    diagnostics.push(Diagnostic::new(
                        Phase::IrVerification,
                        format!("{} has an empty resource array", function.name),
                    ));
                }
                if !bindings.insert((resource_namespace(resource.kind), resource.index)) {
                    diagnostics.push(Diagnostic::new(
                        Phase::IrVerification,
                        format!("{} has a duplicate Metal resource index", function.name),
                    ));
                }
                if resource.argument as usize >= function.parameters.len() {
                    diagnostics.push(Diagnostic::new(
                        Phase::IrVerification,
                        format!("{} resource refers to a missing argument", function.name),
                    ));
                }
            }
        }
        for function in &self.functions {
            verify_function(function, &mut diagnostics);
        }
        if diagnostics.is_empty() {
            Ok(())
        } else {
            Err(TranslateError { diagnostics })
        }
    }
}

fn verify_function(function: &Function, diagnostics: &mut Vec<Diagnostic>) {
    let blocks: BTreeSet<_> = function.blocks.iter().map(|block| block.id).collect();
    if blocks.len() != function.blocks.len() || function.blocks.is_empty() {
        diagnostics.push(Diagnostic::new(
            Phase::IrVerification,
            format!("{} has duplicate block IDs or no blocks", function.name),
        ));
        return;
    }
    let mut predecessors: BTreeMap<BlockId, BTreeSet<BlockId>> = BTreeMap::new();
    for block in &function.blocks {
        for successor in successors(&block.terminator) {
            if !blocks.contains(&successor) {
                diagnostics.push(Diagnostic::new(
                    Phase::IrVerification,
                    format!("{} branches to a missing block", function.name),
                ));
            }
            predecessors.entry(successor).or_default().insert(block.id);
        }
    }
    let entry = function.blocks[0].id;
    let dominators = dominators(&blocks, &predecessors, entry);
    let mut definitions = BTreeMap::new();
    for parameter in &function.parameters {
        if function.values.get(parameter.value.0 as usize) != Some(&parameter.ty) {
            diagnostics.push(Diagnostic::new(
                Phase::IrVerification,
                format!(
                    "{} has a parameter whose value type does not match",
                    function.name
                ),
            ));
        }
        if definitions.insert(parameter.value, None).is_some() {
            diagnostics.push(Diagnostic::new(
                Phase::IrVerification,
                format!("{} defines an SSA value more than once", function.name),
            ));
        }
    }
    for block in &function.blocks {
        let mut saw_non_phi = false;
        for (instruction_index, instruction) in block.instructions.iter().enumerate() {
            let is_phi = matches!(instruction.kind, InstructionKind::Phi { .. });
            if is_phi && saw_non_phi {
                diagnostics.push(Diagnostic::new(
                    Phase::IrVerification,
                    format!("{} has a phi after a non-phi instruction", function.name),
                ));
            }
            saw_non_phi |= !is_phi;
            if let Some(value) = instruction.result
                && value.0 as usize >= function.values.len()
            {
                diagnostics.push(Diagnostic::new(
                    Phase::IrVerification,
                    format!("{} defines an out-of-range value", function.name),
                ));
            }
            if let Some(value) = instruction.result
                && definitions
                    .insert(value, Some((block.id, instruction_index)))
                    .is_some()
            {
                diagnostics.push(Diagnostic::new(
                    Phase::IrVerification,
                    format!("{} defines an SSA value more than once", function.name),
                ));
            }
            if let InstructionKind::Phi { incomings } = &instruction.kind {
                let actual: BTreeSet<_> = incomings.iter().map(|(from, _)| *from).collect();
                let expected = predecessors.get(&block.id).cloned().unwrap_or_default();
                if actual != expected || actual.len() != incomings.len() {
                    diagnostics.push(Diagnostic::new(
                        Phase::IrVerification,
                        format!(
                            "{} has phi inputs which do not match its predecessors",
                            function.name
                        ),
                    ));
                }
            }
        }
    }
    for block in &function.blocks {
        for (instruction_index, instruction) in block.instructions.iter().enumerate() {
            verify_instruction_types(function, instruction, diagnostics);
            match &instruction.kind {
                InstructionKind::Phi { incomings } => {
                    for (predecessor, operand) in incomings {
                        verify_use(
                            function,
                            operand,
                            *predecessor,
                            usize::MAX,
                            &definitions,
                            &dominators,
                            diagnostics,
                        );
                    }
                }
                kind => {
                    for operand in instruction_operands(kind) {
                        verify_use(
                            function,
                            operand,
                            block.id,
                            instruction_index,
                            &definitions,
                            &dominators,
                            diagnostics,
                        );
                    }
                }
            }
        }
        for operand in terminator_operands(&block.terminator) {
            verify_use(
                function,
                operand,
                block.id,
                usize::MAX,
                &definitions,
                &dominators,
                diagnostics,
            );
        }
        if let Terminator::CondBranch { condition, .. } = &block.terminator
            && operand_type(function, condition) != Some(Type::Bool)
        {
            diagnostics.push(Diagnostic::new(
                Phase::IrVerification,
                format!("{} branches on a non-boolean value", function.name),
            ));
        }
        match &block.terminator {
            Terminator::Return(None) if function.result != Type::Void => {
                diagnostics.push(Diagnostic::new(
                    Phase::IrVerification,
                    format!(
                        "{} returns no value from a non-void function",
                        function.name
                    ),
                ))
            }
            Terminator::Return(Some(value))
                if operand_type(function, value) != Some(function.result.clone()) =>
            {
                diagnostics.push(Diagnostic::new(
                    Phase::IrVerification,
                    format!("{} returns a value of the wrong type", function.name),
                ));
            }
            _ => {}
        }
    }
}

type Definition = Option<(BlockId, usize)>;

fn dominators(
    blocks: &BTreeSet<BlockId>,
    predecessors: &BTreeMap<BlockId, BTreeSet<BlockId>>,
    entry: BlockId,
) -> BTreeMap<BlockId, BTreeSet<BlockId>> {
    let mut result = BTreeMap::new();
    for block in blocks {
        result.insert(
            *block,
            if *block == entry {
                BTreeSet::from([entry])
            } else {
                blocks.clone()
            },
        );
    }
    loop {
        let previous = result.clone();
        for block in blocks.iter().copied().filter(|block| *block != entry) {
            let incoming = predecessors.get(&block).cloned().unwrap_or_default();
            let mut next = if incoming.is_empty() {
                BTreeSet::new()
            } else {
                let mut incoming = incoming.into_iter();
                let first = incoming.next().unwrap();
                incoming.fold(previous[&first].clone(), |set, predecessor| {
                    set.intersection(&previous[&predecessor]).copied().collect()
                })
            };
            next.insert(block);
            result.insert(block, next);
        }
        if result == previous {
            return result;
        }
    }
}

fn verify_use(
    function: &Function,
    operand: &Operand,
    use_block: BlockId,
    use_index: usize,
    definitions: &BTreeMap<ValueId, Definition>,
    dominators: &BTreeMap<BlockId, BTreeSet<BlockId>>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let Operand::Value(value) = operand else {
        return;
    };
    let Some(definition) = definitions.get(value) else {
        diagnostics.push(Diagnostic::new(
            Phase::IrVerification,
            format!("{} uses an undefined SSA value", function.name),
        ));
        return;
    };
    if let Some((definition_block, definition_index)) = definition {
        let dominates = if *definition_block == use_block {
            *definition_index < use_index
        } else {
            dominators
                .get(&use_block)
                .is_some_and(|set| set.contains(definition_block))
        };
        if !dominates {
            diagnostics.push(Diagnostic::new(
                Phase::IrVerification,
                format!(
                    "{} has an SSA use not dominated by its definition",
                    function.name
                ),
            ));
        }
    }
}

fn verify_instruction_types(
    function: &Function,
    instruction: &Instruction,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let result = instruction
        .result
        .and_then(|value| function.values.get(value.0 as usize))
        .cloned();
    let valid = match &instruction.kind {
        InstructionKind::GlobalInvocationId => {
            result
                == Some(Type::Vector {
                    element: Box::new(Type::u32()),
                    lanes: 3,
                })
        }
        InstructionKind::LocalVariable { ty } => matches!(
            &result,
            Some(Type::Pointer { space, pointee })
                if *space == AddressSpace::Thread && pointee.as_ref() == ty
        ),
        InstructionKind::Load { pointer } => match operand_type(function, pointer) {
            Some(Type::Pointer { pointee, .. }) => result.as_ref() == Some(pointee.as_ref()),
            _ => false,
        },
        InstructionKind::Store { pointer, value } => match operand_type(function, pointer) {
            Some(Type::Pointer { pointee, .. }) => {
                Some(pointee.as_ref().clone()) == operand_type(function, value)
            }
            _ => false,
        },
        InstructionKind::IntegerBinary { op, lhs, rhs } => {
            // LLVM types a Metal `bool` as `i1`, so bitwise operations stand
            // in for logical ones.
            let boolean = matches!(
                op,
                IntegerBinaryOp::And | IntegerBinaryOp::Or | IntegerBinaryOp::Xor
            ) && result.as_ref().is_some_and(is_boolean_or_boolean_vector);
            (boolean || result.as_ref().is_some_and(is_integer_or_integer_vector))
                && result == operand_type(function, lhs)
                && result == operand_type(function, rhs)
        }
        InstructionKind::IntegerCompare { lhs, rhs, .. } => {
            operand_type(function, lhs) == operand_type(function, rhs)
                && operand_type(function, lhs)
                    .as_ref()
                    .is_some_and(is_integer_or_integer_vector)
                && result == operand_type(function, lhs).map(boolean_shape)
        }
        InstructionKind::FloatCompare { lhs, rhs, .. } => {
            operand_type(function, lhs) == operand_type(function, rhs)
                && operand_type(function, lhs)
                    .as_ref()
                    .is_some_and(is_float_or_float_vector)
                && result == operand_type(function, lhs).map(boolean_shape)
        }
        InstructionKind::ElementAddress { pointer, index } => {
            matches!(result, Some(Type::Pointer { .. }))
                && matches!(operand_type(function, pointer), Some(Type::Pointer { .. }))
                && operand_type(function, index).is_some_and(|ty| matches!(ty, Type::Int { .. }))
        }
        InstructionKind::FieldAddress { pointer, .. } => {
            matches!(result, Some(Type::Pointer { .. }))
                && matches!(operand_type(function, pointer), Some(Type::Pointer { .. }))
        }
        InstructionKind::BufferAddress { pointer, indices } => (|| {
            let Some(Type::Pointer { space, pointee }) = operand_type(function, pointer) else {
                return false;
            };
            let mut ty = *pointee;
            let mut valid = !indices.is_empty();
            for (position, index) in indices.iter().enumerate() {
                valid &= matches!(operand_type(function, index), Some(Type::Int { .. }));
                if position == 0 {
                    continue;
                }
                ty = match ty {
                    Type::Struct(ref structure) => {
                        if let Operand::Integer { value, .. } = index {
                            if let Some(field) = structure.fields.get(*value as usize) {
                                field.ty.clone()
                            } else {
                                valid = false;
                                Type::Void
                            }
                        } else {
                            valid = false;
                            Type::Void
                        }
                    }
                    Type::Array { element, .. } | Type::Vector { element, .. } => *element,
                    _ => {
                        valid = false;
                        Type::Void
                    }
                };
            }
            valid
                && result
                    == Some(Type::Pointer {
                        space,
                        pointee: Box::new(ty),
                    })
        })(),
        InstructionKind::CompositeExtract { composite, index } => {
            match operand_type(function, composite) {
                Some(Type::Struct(structure)) => {
                    structure
                        .fields
                        .get(*index as usize)
                        .map(|field| field.ty.clone())
                        == result
                }
                Some(Type::Vector { element, lanes }) => {
                    *index < u32::from(lanes) && result.as_ref() == Some(element.as_ref())
                }
                _ => false,
            }
        }
        InstructionKind::All { vector } | InstructionKind::Any { vector } => {
            matches!(operand_type(function, vector), Some(Type::Vector { element, .. }) if *element == Type::Bool)
                && result == Some(Type::Bool)
        }
        InstructionKind::Convert { op, value } => {
            verify_convert(*op, operand_type(function, value), result.as_ref())
        }
        InstructionKind::Select {
            condition,
            on_true,
            on_false,
        } => {
            result == operand_type(function, on_true)
                && result == operand_type(function, on_false)
                && (operand_type(function, condition) == Some(Type::Bool)
                    || matches!((&result, operand_type(function, condition)), (Some(Type::Vector { lanes, .. }), Some(Type::Vector { element, lanes: n })) if *element == Type::Bool && *lanes == n))
        }
        InstructionKind::FloatBinary { lhs, rhs, .. } => {
            result == operand_type(function, lhs)
                && result == operand_type(function, rhs)
                && result.as_ref().is_some_and(is_float_or_float_vector)
        }
        InstructionKind::FloatUnary { value, .. } => {
            result == operand_type(function, value)
                && result.as_ref().is_some_and(is_float_or_float_vector)
        }
        InstructionKind::FloatFma {
            first,
            second,
            addend,
        } => {
            result == operand_type(function, first)
                && result == operand_type(function, second)
                && result == operand_type(function, addend)
                && result.as_ref().is_some_and(is_float_or_float_vector)
        }
        InstructionKind::FloatDot { lhs, rhs } => {
            operand_type(function, lhs) == operand_type(function, rhs)
                && matches!(operand_type(function, lhs), Some(Type::Vector { element, .. }) if matches!(element.as_ref(), Type::Float { .. }))
                && result == Some(Type::Float { bits: 32 })
        }
        InstructionKind::VectorShuffle {
            first,
            second,
            components,
        } => match operand_type(function, first) {
            Some(Type::Vector { element, lanes }) => {
                operand_type(function, second)
                    == Some(Type::Vector {
                        element: element.clone(),
                        lanes,
                    })
                    && result
                        == Some(Type::Vector {
                            element,
                            lanes: components.len() as u8,
                        })
            }
            _ => false,
        },
        InstructionKind::CompositeInsert {
            composite,
            object,
            index,
        } => match operand_type(function, composite) {
            Some(Type::Vector { element, lanes }) => {
                *index < u32::from(lanes)
                    && operand_type(function, object) == Some(*element)
                    && result == operand_type(function, composite)
            }
            Some(Type::Struct(structure)) => {
                structure
                    .fields
                    .get(*index as usize)
                    .map(|field| field.ty.clone())
                    == operand_type(function, object)
                    && result == operand_type(function, composite)
            }
            _ => false,
        },
        InstructionKind::FloatClamp { value, min, max } => {
            result == operand_type(function, value)
                && result == operand_type(function, min)
                && result == operand_type(function, max)
                && result.as_ref().is_some_and(is_float_or_float_vector)
        }
        InstructionKind::IntegerClamp {
            value, min, max, ..
        } => {
            result == operand_type(function, value)
                && result == operand_type(function, min)
                && result == operand_type(function, max)
                && result.as_ref().is_some_and(is_integer_or_integer_vector)
        }
        InstructionKind::IntegerUnary { value, .. } => {
            result == operand_type(function, value)
                && result.as_ref().is_some_and(is_integer_or_integer_vector)
        }
        InstructionKind::FloatMix {
            first,
            second,
            weight,
        } => {
            result == operand_type(function, first)
                && result == operand_type(function, second)
                && result == operand_type(function, weight)
                && result.as_ref().is_some_and(is_float_or_float_vector)
        }
        InstructionKind::VectorSplat { value, lanes } => {
            result
                == operand_type(function, value).map(|element| Type::Vector {
                    element: Box::new(element),
                    lanes: *lanes,
                })
        }
        InstructionKind::TextureSample {
            texture,
            sampler,
            coordinate,
            array_index,
            offset,
            lod,
            ..
        } => {
            let Some(Type::Texture(texture)) = operand_type(function, texture) else {
                return invalid(function, instruction, diagnostics);
            };
            operand_type(function, sampler) == Some(Type::Sampler)
                && texture.access == Access::Sample
                && coordinate_shape(function, coordinate, &texture, Type::Float { bits: 32 })
                && optional_is(function, array_index.as_ref(), &Type::u32())
                && optional_integer_vector(function, offset.as_ref())
                && optional_is(function, lod.as_ref(), &Type::Float { bits: 32 })
                && result == Some(texel_vector(&texture))
        }
        InstructionKind::TextureRead {
            texture,
            coordinate,
            array_index,
            lod,
            sample,
        } => {
            let Some(Type::Texture(texture)) = operand_type(function, texture) else {
                return invalid(function, instruction, diagnostics);
            };
            coordinate_shape(function, coordinate, &texture, Type::u32())
                && optional_is(function, array_index.as_ref(), &Type::u32())
                && optional_is(function, lod.as_ref(), &Type::u32())
                && optional_is(function, sample.as_ref(), &Type::u32())
                && result == Some(texel_vector(&texture))
        }
        InstructionKind::TextureQuerySize { texture, lod, .. } => {
            matches!(operand_type(function, texture), Some(Type::Texture(_)))
                && optional_is(function, lod.as_ref(), &Type::u32())
                && result == Some(Type::u32())
        }
        InstructionKind::DiscardFragment => result.is_none(),
        InstructionKind::TextureWrite {
            texture,
            coordinate,
            array_index,
            value,
        } => {
            let Some(Type::Texture(texture)) = operand_type(function, texture) else {
                return invalid(function, instruction, diagnostics);
            };
            result.is_none()
                && texture.access != Access::Sample
                && coordinate_shape(function, coordinate, &texture, Type::u32())
                && optional_is(function, array_index.as_ref(), &Type::u32())
                && operand_type(function, value) == Some(texel_vector(&texture))
        }
        InstructionKind::Phi { incomings } => {
            result.is_some()
                && incomings
                    .iter()
                    .all(|(_, value)| operand_type(function, value) == result)
        }
    };
    if !valid {
        invalid(function, instruction, diagnostics);
    }
}

/// Report an instruction whose operands do not have the required shape and
/// stop checking it.
fn invalid(function: &Function, instruction: &Instruction, diagnostics: &mut Vec<Diagnostic>) {
    diagnostics.push(Diagnostic::new(
        Phase::IrVerification,
        format!(
            "{} has an ill-typed {}",
            function.name,
            instruction_name(&instruction.kind)
        ),
    ));
}

/// The instruction's kind, for diagnostics. `Debug` would print every
/// operand, which is far more than a reader of a diagnostic needs.
fn instruction_name(kind: &InstructionKind) -> &'static str {
    match kind {
        InstructionKind::GlobalInvocationId => "global invocation ID",
        InstructionKind::LocalVariable { .. } => "local variable",
        InstructionKind::Load { .. } => "load",
        InstructionKind::Store { .. } => "store",
        InstructionKind::IntegerBinary { .. } => "integer arithmetic",
        InstructionKind::IntegerCompare { .. } => "integer comparison",
        InstructionKind::FloatCompare { .. } => "floating comparison",
        InstructionKind::ElementAddress { .. } => "element address",
        InstructionKind::FieldAddress { .. } => "field address",
        InstructionKind::BufferAddress { .. } => "buffer address",
        InstructionKind::CompositeExtract { .. } => "composite extraction",
        InstructionKind::CompositeInsert { .. } => "composite insertion",
        InstructionKind::All { .. } => "all",
        InstructionKind::Any { .. } => "any",
        InstructionKind::Convert { .. } => "conversion",
        InstructionKind::Select { .. } => "select",
        InstructionKind::FloatBinary { .. } => "floating arithmetic",
        InstructionKind::FloatUnary { .. } => "floating math call",
        InstructionKind::FloatFma { .. } => "fused multiply-add",
        InstructionKind::VectorShuffle { .. } => "vector shuffle",
        InstructionKind::FloatClamp { .. } => "clamp",
        InstructionKind::IntegerClamp { .. } => "integer clamp",
        InstructionKind::IntegerUnary { .. } => "integer math call",
        InstructionKind::FloatMix { .. } => "mix",
        InstructionKind::FloatDot { .. } => "dot product",
        InstructionKind::VectorSplat { .. } => "vector splat",
        InstructionKind::TextureSample { .. } => "texture sample",
        InstructionKind::TextureRead { .. } => "texture read",
        InstructionKind::TextureWrite { .. } => "texture write",
        InstructionKind::TextureQuerySize { .. } => "texture size query",
        InstructionKind::DiscardFragment => "fragment discard",
        InstructionKind::Phi { .. } => "phi",
    }
}

fn optional_is(function: &Function, operand: Option<&Operand>, expected: &Type) -> bool {
    operand.is_none_or(|operand| operand_type(function, operand).as_ref() == Some(expected))
}

fn optional_integer_vector(function: &Function, operand: Option<&Operand>) -> bool {
    operand.is_none_or(|operand| {
        operand_type(function, operand)
            .as_ref()
            .is_some_and(is_integer_or_integer_vector)
    })
}

/// The number of coordinate components a texture dimension takes, excluding
/// the array slice, which Metal passes separately.
pub(crate) fn coordinate_lanes(texture: &TextureType) -> u8 {
    match texture.dimension {
        TextureDimension::D1 => 1,
        TextureDimension::D2 => 2,
        TextureDimension::D3 | TextureDimension::Cube => 3,
    }
}

fn coordinate_shape(
    function: &Function,
    coordinate: &Operand,
    texture: &TextureType,
    element: Type,
) -> bool {
    let lanes = coordinate_lanes(texture);
    let expected = if lanes == 1 {
        element
    } else {
        Type::Vector {
            element: Box::new(element),
            lanes,
        }
    };
    operand_type(function, coordinate) == Some(expected)
}

/// Metal always returns four components from a texture access, whatever the
/// texture's storage format.
fn texel_vector(texture: &TextureType) -> Type {
    Type::Vector {
        element: texture.texel.clone(),
        lanes: 4,
    }
}

pub(crate) fn operand_type(function: &Function, operand: &Operand) -> Option<Type> {
    match operand {
        Operand::Composite { ty, .. } => Some(ty.clone()),
        Operand::Value(value) => function.values.get(value.0 as usize).cloned(),
        Operand::Integer { ty, .. } => Some(ty.clone()),
        Operand::Float { ty, .. } => Some(ty.clone()),
        Operand::Global { ty, .. } => Some(ty.clone()),
        Operand::Undef(ty) => Some(ty.clone()),
        Operand::Bool(_) => Some(Type::Bool),
    }
}

pub(crate) fn instruction_operands(kind: &InstructionKind) -> Vec<&Operand> {
    match kind {
        InstructionKind::LocalVariable { .. } => Vec::new(),
        InstructionKind::Load { pointer } => vec![pointer],
        InstructionKind::Store { pointer, value } => vec![pointer, value],
        InstructionKind::IntegerBinary { lhs, rhs, .. }
        | InstructionKind::IntegerCompare { lhs, rhs, .. }
        | InstructionKind::FloatCompare { lhs, rhs, .. } => vec![lhs, rhs],
        InstructionKind::ElementAddress { pointer, index } => vec![pointer, index],
        InstructionKind::FieldAddress { pointer, .. } => vec![pointer],
        InstructionKind::CompositeExtract { composite, .. } => vec![composite],
        InstructionKind::BufferAddress { pointer, indices } => {
            std::iter::once(pointer).chain(indices).collect()
        }
        InstructionKind::FloatBinary { lhs, rhs, .. } | InstructionKind::FloatDot { lhs, rhs } => {
            vec![lhs, rhs]
        }
        InstructionKind::FloatFma {
            first,
            second,
            addend,
        } => vec![first, second, addend],
        InstructionKind::Select {
            condition,
            on_true,
            on_false,
        } => vec![condition, on_true, on_false],
        InstructionKind::VectorShuffle { first, second, .. } => vec![first, second],
        InstructionKind::CompositeInsert {
            composite, object, ..
        } => vec![composite, object],
        InstructionKind::FloatClamp { value, min, max }
        | InstructionKind::IntegerClamp {
            value, min, max, ..
        } => vec![value, min, max],
        InstructionKind::FloatMix {
            first,
            second,
            weight,
        } => vec![first, second, weight],
        InstructionKind::All { vector }
        | InstructionKind::Any { vector }
        | InstructionKind::Convert { value: vector, .. }
        | InstructionKind::FloatUnary { value: vector, .. }
        | InstructionKind::IntegerUnary { value: vector, .. } => {
            vec![vector]
        }
        InstructionKind::VectorSplat { value, .. } => vec![value],
        InstructionKind::GlobalInvocationId => vec![],
        InstructionKind::TextureWrite {
            texture,
            coordinate,
            array_index,
            value,
        } => [texture, coordinate, value]
            .into_iter()
            .chain(array_index)
            .collect(),
        InstructionKind::TextureSample {
            texture,
            sampler,
            coordinate,
            array_index,
            offset,
            lod,
            ..
        } => [texture, sampler, coordinate]
            .into_iter()
            .chain(array_index)
            .chain(offset)
            .chain(lod)
            .collect(),
        InstructionKind::TextureRead {
            texture,
            coordinate,
            array_index,
            lod,
            sample,
        } => [texture, coordinate]
            .into_iter()
            .chain(array_index)
            .chain(lod)
            .chain(sample)
            .collect(),
        InstructionKind::TextureQuerySize { texture, lod, .. } => {
            [texture].into_iter().chain(lod).collect()
        }
        InstructionKind::DiscardFragment => Vec::new(),
        InstructionKind::Phi { incomings } => incomings.iter().map(|(_, value)| value).collect(),
    }
}

fn is_float_or_float_vector(ty: &Type) -> bool {
    matches!(ty, Type::Float { .. })
        || matches!(ty, Type::Vector { element, .. } if matches!(element.as_ref(), Type::Float { .. }))
}

pub(crate) fn is_boolean_or_boolean_vector(ty: &Type) -> bool {
    matches!(ty, Type::Bool)
        || matches!(ty, Type::Vector { element, .. } if matches!(element.as_ref(), Type::Bool))
}

fn is_integer_or_integer_vector(ty: &Type) -> bool {
    matches!(ty, Type::Int { .. })
        || matches!(ty, Type::Vector { element, .. } if matches!(element.as_ref(), Type::Int { .. }))
}

/// The boolean result of an elementwise comparison keeps the operand's shape:
/// a scalar comparison yields `bool`, a vector comparison yields `bool` lanes.
fn boolean_shape(ty: Type) -> Type {
    match ty {
        Type::Vector { lanes, .. } => Type::Vector {
            element: Box::new(Type::Bool),
            lanes,
        },
        _ => Type::Bool,
    }
}

/// Conversions keep the operand shape and only change the element type.
///
/// A bitcast is the exception: it reinterprets the same bits, so it may change
/// the lane count as long as the total width is unchanged. Metal reads a
/// `float4` back as a `half8` or a `uint2` as a `float2` this way.
fn verify_convert(op: ConvertOp, source: Option<Type>, result: Option<&Type>) -> bool {
    let (Some(source), Some(result)) = (source, result) else {
        return false;
    };
    if op == ConvertOp::Bitcast && reinterpretable(&source) && reinterpretable(result) {
        return total_bits(&source) == total_bits(result) && total_bits(&source).is_some();
    }
    let (source, result) = match (source, result) {
        (
            Type::Vector {
                element: source,
                lanes,
            },
            Type::Vector {
                element: result,
                lanes: result_lanes,
            },
        ) if lanes == *result_lanes => (*source, result.as_ref().clone()),
        (source @ (Type::Int { .. } | Type::Float { .. } | Type::Bool), result) => {
            (source, result.clone())
        }
        _ => return false,
    };
    match (op, &source, &result) {
        // Metal's `bool` is LLVM's `i1`, which widens and narrows like any
        // other integer even though SPIR-V keeps booleans a separate type.
        (
            ConvertOp::ZeroExtend | ConvertOp::SignExtend,
            Type::Bool,
            Type::Int { .. } | Type::Float { .. },
        )
        | (ConvertOp::UnsignedToFloat | ConvertOp::SignedToFloat, Type::Bool, Type::Float { .. })
        | (ConvertOp::IntegerTruncate, Type::Int { .. }, Type::Bool) => true,
        (
            ConvertOp::UnsignedToFloat | ConvertOp::SignedToFloat,
            Type::Int { .. },
            Type::Float { .. },
        )
        | (
            ConvertOp::FloatToUnsigned | ConvertOp::FloatToSigned,
            Type::Float { .. },
            Type::Int { .. },
        ) => true,
        (
            ConvertOp::ZeroExtend | ConvertOp::SignExtend,
            Type::Int { bits: from, .. },
            Type::Int { bits: to, .. },
        ) => to > from,
        (ConvertOp::IntegerTruncate, Type::Int { bits: from, .. }, Type::Int { bits: to, .. }) => {
            to < from
        }
        (ConvertOp::FloatResize, Type::Float { bits: from }, Type::Float { bits: to }) => {
            from != to
        }
        // A bitcast is only meaningful between equally sized representations.
        (ConvertOp::Bitcast, _, _) => scalar_bits(&source) == scalar_bits(&result),
        _ => false,
    }
}

/// Whether a type is a numeric scalar or vector, whose bits a bitcast may
/// reinterpret as another such type of the same total width.
fn reinterpretable(ty: &Type) -> bool {
    match ty {
        Type::Int { .. } | Type::Float { .. } => true,
        Type::Vector { element, .. } => {
            matches!(**element, Type::Int { .. } | Type::Float { .. })
        }
        _ => false,
    }
}

fn total_bits(ty: &Type) -> Option<u32> {
    match ty {
        Type::Int { bits, .. } | Type::Float { bits } => Some(u32::from(*bits)),
        Type::Vector { element, lanes } => Some(total_bits(element)? * u32::from(*lanes)),
        _ => None,
    }
}

fn scalar_bits(ty: &Type) -> Option<u8> {
    match ty {
        Type::Int { bits, .. } | Type::Float { bits } => Some(*bits),
        _ => None,
    }
}

pub(crate) fn terminator_operands(terminator: &Terminator) -> Vec<&Operand> {
    match terminator {
        Terminator::CondBranch { condition, .. } => vec![condition],
        Terminator::Return(Some(value)) => vec![value],
        _ => Vec::new(),
    }
}

fn resource_namespace(kind: ResourceKind) -> u8 {
    match kind {
        ResourceKind::Buffer
        | ResourceKind::AccelerationStructure
        | ResourceKind::VisibleFunctionTable
        | ResourceKind::IntersectionFunctionTable
        | ResourceKind::ArgumentBuffer => 0,
        ResourceKind::Texture => 1,
        ResourceKind::Sampler => 2,
        ResourceKind::Imageblock => 3,
    }
}

fn successors(terminator: &Terminator) -> Vec<BlockId> {
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
