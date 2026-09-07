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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Access {
    Read,
    Write,
    ReadWrite,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResourceKind {
    Buffer,
    /// A writable, non-arrayed 3D texture with `float4` texels.
    ///
    /// The initial image slice is intentionally specific. Other texture
    /// shapes need their own Metal type before they can be emitted.
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
    Texture,
    Sampler,
    AccelerationStructure,
    VisibleFunctionTable,
    IntersectionFunctionTable,
    Imageblock,
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EntryPoint {
    pub function: usize,
    pub stage: Stage,
    pub resources: Vec<Resource>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Module {
    pub functions: Vec<Function>,
    pub entries: Vec<EntryPoint>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Function {
    pub name: String,
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Operand {
    Value(ValueId),
    Integer { ty: Type, value: u64 },
}

impl Operand {
    pub fn u32(value: u32) -> Self {
        Self::Integer {
            ty: Type::u32(),
            value: u64::from(value),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IntegerPredicate {
    UnsignedLessThan,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InstructionKind {
    Load {
        pointer: Operand,
    },
    Store {
        pointer: Operand,
        value: Operand,
    },
    IntegerAdd {
        lhs: Operand,
        rhs: Operand,
    },
    IntegerXor {
        lhs: Operand,
        rhs: Operand,
    },
    IntegerCompare {
        predicate: IntegerPredicate,
        lhs: Operand,
        rhs: Operand,
    },
    ElementAddress {
        pointer: Operand,
        index: Operand,
    },
    /// Store one texel through a writable Metal texture resource.
    ///
    /// Coordinates and texels remain explicit SSA values: Metal's texture
    /// namespace is represented by the resource, not by a Vulkan descriptor.
    TextureWrite {
        texture: Operand,
        coordinate: Operand,
        value: Operand,
    },
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
        if matches!(block.terminator, Terminator::Return(Some(_))) {
            diagnostics.push(Diagnostic::new(
                Phase::IrVerification,
                format!("{} returns a value from a void function", function.name),
            ));
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
        InstructionKind::IntegerAdd { lhs, rhs } | InstructionKind::IntegerXor { lhs, rhs } => {
            result
                .as_ref()
                .is_some_and(|ty| matches!(ty, Type::Int { .. }))
                && result == operand_type(function, lhs)
                && result == operand_type(function, rhs)
        }
        InstructionKind::IntegerCompare { lhs, rhs, .. } => {
            result == Some(Type::Bool)
                && operand_type(function, lhs) == operand_type(function, rhs)
                && operand_type(function, lhs).is_some_and(|ty| matches!(ty, Type::Int { .. }))
        }
        InstructionKind::ElementAddress { pointer, index } => {
            matches!(result, Some(Type::Pointer { .. }))
                && matches!(operand_type(function, pointer), Some(Type::Pointer { .. }))
                && operand_type(function, index).is_some_and(|ty| matches!(ty, Type::Int { .. }))
        }
        InstructionKind::TextureWrite {
            texture,
            coordinate,
            value,
        } => {
            result.is_none()
                && operand_type(function, texture) == Some(Type::Texture)
                && matches!(
                    operand_type(function, coordinate),
                    Some(Type::Vector { element, lanes: 3 }) if *element == Type::u32()
                )
                && matches!(
                    operand_type(function, value),
                    Some(Type::Vector { element, lanes: 4 })
                        if *element == Type::Float { bits: 32 }
                )
        }
        InstructionKind::Phi { incomings } => {
            result.is_some()
                && incomings
                    .iter()
                    .all(|(_, value)| operand_type(function, value) == result)
        }
    };
    if !valid {
        diagnostics.push(Diagnostic::new(
            Phase::IrVerification,
            format!("{} has an ill-typed instruction", function.name),
        ));
    }
}

fn operand_type(function: &Function, operand: &Operand) -> Option<Type> {
    match operand {
        Operand::Value(value) => function.values.get(value.0 as usize).cloned(),
        Operand::Integer { ty, .. } => Some(ty.clone()),
    }
}

fn instruction_operands(kind: &InstructionKind) -> Vec<&Operand> {
    match kind {
        InstructionKind::Load { pointer } => vec![pointer],
        InstructionKind::Store { pointer, value } => vec![pointer, value],
        InstructionKind::IntegerAdd { lhs, rhs }
        | InstructionKind::IntegerXor { lhs, rhs }
        | InstructionKind::IntegerCompare { lhs, rhs, .. } => vec![lhs, rhs],
        InstructionKind::ElementAddress { pointer, index } => vec![pointer, index],
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
