//! AIR to SPIR-V translation through a Metal-shaped intermediate representation.
//!
//! The IR deliberately describes Metal's execution and resource model. Vulkan
//! descriptor locations and SPIR-V capabilities are products of emission, not
//! properties of the input program.

pub mod air;
pub mod ir;
pub mod spirv;

use std::fmt;

pub use air::{AirInput, parse_air};
pub use spirv::{EntryArtifact, ResourceBinding, TargetProfile, TranslatedLibrary, emit_spirv};

/// The phase which reported a translation diagnostic.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Phase {
    AirContainer,
    AirSyntax,
    AirSemantics,
    IrVerification,
    SpirvEmission,
}

/// A contextual, recoverable translation failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Diagnostic {
    pub phase: Phase,
    pub message: String,
    pub line: Option<usize>,
}

impl Diagnostic {
    pub(crate) fn new(phase: Phase, message: impl Into<String>) -> Self {
        Self {
            phase,
            message: message.into(),
            line: None,
        }
    }

    pub(crate) fn at_line(mut self, line: usize) -> Self {
        self.line = Some(line);
        self
    }
}

/// One or more diagnostics from a failed translation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TranslateError {
    pub diagnostics: Vec<Diagnostic>,
}

impl TranslateError {
    pub(crate) fn one(diagnostic: Diagnostic) -> Self {
        Self {
            diagnostics: vec![diagnostic],
        }
    }
}

impl fmt::Display for TranslateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, diagnostic) in self.diagnostics.iter().enumerate() {
            if index != 0 {
                writeln!(f)?;
            }
            write!(f, "{:?}", diagnostic.phase)?;
            if let Some(line) = diagnostic.line {
                write!(f, " at line {line}")?;
            }
            write!(f, ": {}", diagnostic.message)?;
        }
        Ok(())
    }
}

impl std::error::Error for TranslateError {}

/// Parse, verify, and emit every entry point in one AIR module.
pub fn translate(
    input: AirInput<'_>,
    target: &TargetProfile,
) -> Result<TranslatedLibrary, TranslateError> {
    let module = parse_air(input)?;
    emit_spirv(&module, target)
}
