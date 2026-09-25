mod cfg;
mod dce;
mod fold;
mod phi;

use std::collections::HashMap;
use std::sync::LazyLock;

use crate::ir::{
    Function, GlobalVariable, InstructionKind, Module, Operand, Terminator, Type, ValueId,
    instruction_operands_mut,
};
use crate::sha256::Sha256;
use crate::{Diagnostic, Phase, TranslateError};

/// Names the passes to run, overriding the default pipeline.
///
/// A comma-separated list of pass names runs exactly those. A list whose first
/// item is `-name` starts from the default pipeline and removes the named
/// passes. `default` adds the default passes, and `none`, `off`, `0` or the
/// empty string runs none at all.
const PASSES_VARIABLE: &str = "ROSETTA3_METAL_PASSES";

/// How many times the pipeline is repeated over one function before it is
/// left as it stands. Each pass exposes work for the others, and the result is
/// valid after any round, so this bounds time rather than correctness.
const MAX_ROUNDS: usize = 16;

/// One IR transformation.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Pass {
    /// Evaluate integer and boolean operations whose operands are constants,
    /// apply algebraic identities, and turn a branch on a constant into an
    /// unconditional one.
    FoldConstants,
    /// Replace a phi whose incoming values are all the same with that value.
    SimplifyPhis,
    /// Remove blocks the entry block cannot reach, and their phi inputs.
    RemoveUnreachableBlocks,
    /// Remove instructions whose results nothing uses and which have no
    /// effect, and `thread` locals which are written but never read.
    EliminateDeadCode,
}

impl Pass {
    /// Every pass, in the order the default pipeline runs them.
    pub const ALL: [Pass; 4] = [
        Pass::FoldConstants,
        Pass::SimplifyPhis,
        Pass::RemoveUnreachableBlocks,
        Pass::EliminateDeadCode,
    ];

    /// The name the pass has in `ROSETTA3_METAL_PASSES` and in the cache key.
    ///
    /// Renaming a pass changes the key of every entry it contributed to, which
    /// is correct: the key names what the translation did.
    pub fn name(self) -> &'static str {
        match self {
            Self::FoldConstants => "fold-constants",
            Self::SimplifyPhis => "simplify-phis",
            Self::RemoveUnreachableBlocks => "remove-unreachable-blocks",
            Self::EliminateDeadCode => "eliminate-dead-code",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|pass| pass.name() == name)
    }

    /// Run the pass over one function, answering whether anything changed.
    fn run(self, function: &mut Function, globals: &[GlobalVariable]) -> bool {
        match self {
            Self::FoldConstants => fold::run(function, globals),
            Self::SimplifyPhis => phi::run(function),
            Self::RemoveUnreachableBlocks => cfg::run(function),
            Self::EliminateDeadCode => dce::run(function),
        }
    }
}

/// An ordered list of passes, each run at most once per round.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PassPipeline {
    passes: Vec<Pass>,
}

impl Default for PassPipeline {
    fn default() -> Self {
        Self {
            passes: Pass::ALL.to_vec(),
        }
    }
}

impl PassPipeline {
    /// A pipeline of the given passes, in order. A pass named twice runs once,
    /// at its first position.
    pub fn new(passes: impl IntoIterator<Item = Pass>) -> Self {
        let mut pipeline = Self::none();
        for pass in passes {
            pipeline.add(pass);
        }
        pipeline
    }

    /// A pipeline which leaves the IR as the frontend produced it.
    pub fn none() -> Self {
        Self { passes: Vec::new() }
    }

    pub fn passes(&self) -> &[Pass] {
        &self.passes
    }

    fn add(&mut self, pass: Pass) {
        if !self.passes.contains(&pass) {
            self.passes.push(pass);
        }
    }

    /// Parse the pipeline syntax `ROSETTA3_METAL_PASSES` takes.
    pub fn parse(spec: &str) -> Result<Self, String> {
        let spec = spec.trim();
        if matches!(spec, "" | "0" | "off" | "none") {
            return Ok(Self::none());
        }
        let items: Vec<&str> = spec
            .split(',')
            .map(str::trim)
            .filter(|item| !item.is_empty())
            .collect();
        let mut pipeline = if items.first().is_some_and(|item| item.starts_with('-')) {
            Self::default()
        } else {
            Self::none()
        };
        for item in items {
            match item {
                "default" => Pass::ALL.into_iter().for_each(|pass| pipeline.add(pass)),
                "none" | "off" => pipeline.passes.clear(),
                _ => {
                    let (remove, name) = match item.strip_prefix('-') {
                        Some(name) => (true, name),
                        None => (false, item),
                    };
                    let pass = Pass::from_name(name).ok_or_else(|| {
                        let known: Vec<_> = Pass::ALL.iter().map(|pass| pass.name()).collect();
                        format!(
                            "unknown IR pass `{name}`; the passes are {}",
                            known.join(", ")
                        )
                    })?;
                    if remove {
                        pipeline.passes.retain(|existing| *existing != pass);
                    } else {
                        pipeline.add(pass);
                    }
                }
            }
        }
        Ok(pipeline)
    }

    /// The pipeline `ROSETTA3_METAL_PASSES` names, or the default one.
    ///
    /// Read once per process: a pipeline which changed between two libraries
    /// would give one run two meanings of the same cache key.
    pub fn from_env() -> &'static Self {
        static PIPELINE: LazyLock<PassPipeline> = LazyLock::new(|| {
            let Some(spec) = std::env::var_os(PASSES_VARIABLE) else {
                return PassPipeline::default();
            };
            match PassPipeline::parse(&spec.to_string_lossy()) {
                Ok(pipeline) => pipeline,
                Err(error) => {
                    eprintln!(
                        "metal_translate: {PASSES_VARIABLE}: {error}; running the default passes"
                    );
                    PassPipeline::default()
                }
            }
        });
        &PIPELINE
    }

    /// A digest of which passes run and in what order, for the cache key.
    ///
    /// What each pass does is covered by the crate-content version the key
    /// already carries; this covers the choice made at run time.
    pub fn fingerprint(&self) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update_field(b"rosetta3 metal_translate ir passes v1");
        hasher.update(&(self.passes.len() as u64).to_le_bytes());
        for pass in &self.passes {
            hasher.update_field(pass.name().as_bytes());
        }
        hasher.finish()
    }

    /// Run the pipeline over every function of a verified module.
    ///
    /// Debug builds verify each function after every pass which changed it, so
    /// a pass which breaks the IR is named rather than found by emission.
    pub fn run(&self, module: &mut Module) -> Result<(), TranslateError> {
        if self.passes.is_empty() {
            return Ok(());
        }
        let Module {
            functions, globals, ..
        } = module;
        for function in functions.iter_mut() {
            for _ in 0..MAX_ROUNDS {
                let mut changed = false;
                for &pass in &self.passes {
                    if !pass.run(function, globals) {
                        continue;
                    }
                    changed = true;
                    if cfg!(debug_assertions) {
                        function.verify().map_err(|mut error| {
                            error.diagnostics.insert(
                                0,
                                Diagnostic::new(
                                    Phase::IrVerification,
                                    format!(
                                        "IR pass {} left {} invalid",
                                        pass.name(),
                                        function.name
                                    ),
                                ),
                            );
                            error
                        })?;
                    }
                }
                if !changed {
                    break;
                }
            }
        }
        Ok(())
    }
}

/// Values a pass has proven equal to an operand, and the rewrite of every use.
#[derive(Default)]
pub(crate) struct Substitution {
    map: HashMap<ValueId, Operand>,
}

impl Substitution {
    pub(crate) fn insert(&mut self, value: ValueId, operand: Operand) {
        self.map.insert(value, operand);
    }

    fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Rewrite one operand, following a value to what it was replaced with.
    ///
    /// A pass never maps a value to itself, but a chain is followed only as
    /// far as there are entries so a mistake cannot loop forever.
    fn resolve(&self, operand: &mut Operand) {
        match operand {
            Operand::Value(value) => {
                let mut current = *value;
                let mut replacement = None;
                for _ in 0..=self.map.len() {
                    match self.map.get(&current) {
                        Some(Operand::Value(next)) => {
                            current = *next;
                            replacement = Some(Operand::Value(current));
                        }
                        Some(other) => {
                            replacement = Some(other.clone());
                            break;
                        }
                        None => break,
                    }
                }
                if let Some(replacement) = replacement {
                    *operand = replacement;
                }
            }
            Operand::Composite { elements, .. } => elements
                .iter_mut()
                .for_each(|element| self.resolve(element)),
            _ => {}
        }
    }

    /// Rewrite every use in a function.
    pub(crate) fn apply(&self, function: &mut Function) {
        if self.is_empty() {
            return;
        }
        for block in &mut function.blocks {
            for instruction in &mut block.instructions {
                for operand in instruction_operands_mut(&mut instruction.kind) {
                    self.resolve(operand);
                }
            }
            match &mut block.terminator {
                Terminator::CondBranch { condition, .. } => self.resolve(condition),
                Terminator::Return(Some(value)) => self.resolve(value),
                _ => {}
            }
        }
    }
}

/// Evaluate one instruction the way `fold-constants` would, for code which
/// has to be evaluated whichever passes a process chose to run.
pub(crate) fn fold_instruction(
    kind: &InstructionKind,
    ty: &Type,
    globals: &[GlobalVariable],
) -> Option<Operand> {
    fold::fold(kind, ty, globals)
}

/// Visit every SSA value an operand names, including inside a composite.
fn operand_values(operand: &Operand, visit: &mut impl FnMut(ValueId)) {
    match operand {
        Operand::Value(value) => visit(*value),
        Operand::Composite { elements, .. } => elements
            .iter()
            .for_each(|element| operand_values(element, visit)),
        _ => {}
    }
}

#[cfg(test)]
mod tests;
