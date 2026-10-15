//! The syntax tree of a Metal source library.

use crate::ir::{Stage, TextureType};

/// A scalar as Metal source spells it. The IR's integers are signless, so
/// signedness lives here and chooses the operation each use lowers to.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) enum Scalar {
    Bool,
    Int { signed: bool, bits: u8 },
    Float { bits: u8 },
}

impl Scalar {
    pub const INT: Scalar = Scalar::Int {
        signed: true,
        bits: 32,
    };
    pub const UINT: Scalar = Scalar::Int {
        signed: false,
        bits: 32,
    };
    pub const FLOAT: Scalar = Scalar::Float { bits: 32 };

    pub fn is_integer(self) -> bool {
        matches!(self, Scalar::Int { .. })
    }

    pub fn is_signed(self) -> bool {
        matches!(self, Scalar::Int { signed: true, .. })
    }

    pub fn bits(self) -> u8 {
        match self {
            Scalar::Bool => 1,
            Scalar::Int { bits, .. } | Scalar::Float { bits } => bits,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum Ty {
    Void,
    Scalar(Scalar),
    Vector(Scalar, u8),
    Array(Box<Ty>, u32),
    /// An index into [`Program::structs`].
    Struct(usize),
    Texture(TextureType),
    Sampler,
}

impl Ty {
    /// The scalar of a scalar or vector type.
    pub fn scalar(&self) -> Option<Scalar> {
        match self {
            Ty::Scalar(scalar) | Ty::Vector(scalar, _) => Some(*scalar),
            _ => None,
        }
    }

    /// One for a scalar, the lane count for a vector.
    pub fn lanes(&self) -> Option<u8> {
        match self {
            Ty::Scalar(_) => Some(1),
            Ty::Vector(_, lanes) => Some(*lanes),
            _ => None,
        }
    }

    /// The scalar or vector of `lanes` lanes of `scalar`.
    pub fn of(scalar: Scalar, lanes: u8) -> Ty {
        if lanes == 1 {
            Ty::Scalar(scalar)
        } else {
            Ty::Vector(scalar, lanes)
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct Expr {
    pub kind: ExprKind,
    pub line: usize,
}

#[derive(Clone, Debug)]
pub(super) enum ExprKind {
    Ident(String),
    Int {
        value: u64,
        unsigned: bool,
    },
    Float {
        value: f64,
        half: bool,
    },
    Bool(bool),
    /// `-`, `+`, `!`, `~`, and the prefix `++` and `--`.
    Unary(&'static str, Box<Expr>),
    /// The postfix `++` and `--`.
    Postfix(&'static str, Box<Expr>),
    Binary(&'static str, Box<Expr>, Box<Expr>),
    /// `=` with `None`, or a compound assignment with its operator.
    Assign(Option<&'static str>, Box<Expr>, Box<Expr>),
    Ternary(Box<Expr>, Box<Expr>, Box<Expr>),
    /// A free function: Metal's library, or a template such as `as_type<T>`.
    Call {
        name: String,
        template: Option<Ty>,
        args: Vec<Expr>,
    },
    /// `float4(...)` and the like.
    Construct(Ty, Vec<Expr>),
    /// `(T)x`.
    Cast(Ty, Box<Expr>),
    /// `reinterpret_cast<SPACE atomic_T *>(pointer)`: the one pointer cast
    /// Metal source needs, to reach memory through its atomic functions.
    AtomicCast(Scalar, Box<Expr>),
    /// `(T){ .a = x, .b = y }`, or with the designators left out.
    CompoundLiteral(Ty, Vec<(Option<String>, Expr)>),
    /// `{ a, b, c }` initializing an array or a structure.
    InitList(Vec<Expr>),
    Member(Box<Expr>, String),
    Index(Box<Expr>, Box<Expr>),
    Method {
        object: Box<Expr>,
        name: String,
        args: Vec<Expr>,
    },
}

#[derive(Clone, Debug)]
pub(super) enum Stmt {
    Decl {
        ty: Ty,
        name: String,
        init: Option<Expr>,
        line: usize,
    },
    /// A `threadgroup` variable, shared by the threadgroup and therefore
    /// never initialized where it is declared.
    Threadgroup {
        ty: Ty,
        name: String,
        line: usize,
    },
    Expr(Expr),
    If {
        condition: Expr,
        then: Vec<Stmt>,
        otherwise: Option<Vec<Stmt>>,
    },
    While {
        condition: Expr,
        body: Vec<Stmt>,
    },
    DoWhile {
        body: Vec<Stmt>,
        condition: Expr,
    },
    For {
        init: Vec<Stmt>,
        condition: Option<Expr>,
        step: Option<Expr>,
        body: Vec<Stmt>,
    },
    Break(usize),
    Continue(usize),
    Return(Option<Expr>, usize),
    Block(Vec<Stmt>),
}

#[derive(Clone, Debug)]
pub(super) struct Attribute {
    pub name: String,
    pub args: Vec<Expr>,
    pub line: usize,
}

#[derive(Clone, Debug)]
pub(super) struct Field {
    pub name: String,
    pub ty: Ty,
    pub attributes: Vec<Attribute>,
    pub line: usize,
}

#[derive(Clone, Debug)]
pub(super) struct StructDef {
    pub name: String,
    pub fields: Vec<Field>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Space {
    Constant,
    Device,
    Thread,
    Threadgroup,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Indirection {
    Value,
    Reference,
    Pointer,
}

#[derive(Clone, Debug)]
pub(super) struct Param {
    pub ty: Ty,
    pub name: String,
    pub space: Option<Space>,
    pub indirection: Indirection,
    pub attributes: Vec<Attribute>,
    pub line: usize,
}

#[derive(Clone, Debug)]
pub(super) struct FunctionDef {
    pub stage: Option<Stage>,
    pub ret: Ty,
    pub name: String,
    pub params: Vec<Param>,
    pub body: Vec<Stmt>,
    pub line: usize,
}

/// A `constant` variable at file scope: a lookup table or a remapping array.
#[derive(Clone, Debug)]
pub(super) struct Global {
    pub name: String,
    pub ty: Ty,
    pub init: Expr,
    pub line: usize,
}

#[derive(Clone, Debug, Default)]
pub(super) struct Program {
    pub structs: Vec<StructDef>,
    pub globals: Vec<Global>,
    pub functions: Vec<FunctionDef>,
    /// Every `constexpr sampler`, which an entry point that can see one takes
    /// as a hidden `[[__inline_sampler(n)]]` parameter indexing this list.
    pub inline_samplers: Vec<crate::ir::InlineSampler>,
}

/// The attribute that marks a hidden parameter as an inline sampler.
pub(super) const INLINE_SAMPLER: &str = "__inline_sampler";
