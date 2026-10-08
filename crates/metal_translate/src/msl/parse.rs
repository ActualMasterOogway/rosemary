//! A recursive-descent parser for the C++ subset Metal source is written in.

use super::ast::*;
use super::lex::{Kind, Token};
use super::syntax_error;
use crate::TranslateError;
use crate::ir::{Access, Stage, TextureDimension, TextureType, Type};

pub(super) fn parse(tokens: &[Token]) -> Result<Program> {
    let mut parser = Parser {
        tokens,
        at: 0,
        program: Program::default(),
    };
    parser.program()?;
    Ok(parser.program)
}

struct Parser<'a> {
    tokens: &'a [Token],
    at: usize,
    program: Program,
}

type Result<T> = std::result::Result<T, TranslateError>;

impl Parser<'_> {
    fn line(&self) -> usize {
        self.tokens
            .get(self.at)
            .or(self.tokens.last())
            .map_or(1, |token| token.line)
    }

    fn peek(&self) -> Option<&Kind> {
        self.tokens.get(self.at).map(|token| &token.kind)
    }

    fn peek_at(&self, offset: usize) -> Option<&Kind> {
        self.tokens.get(self.at + offset).map(|token| &token.kind)
    }

    fn is_punct(&self, punct: &str) -> bool {
        matches!(self.peek(), Some(Kind::Punct(p)) if *p == punct)
    }

    fn is_word(&self, word: &str) -> bool {
        matches!(self.peek(), Some(Kind::Ident(w)) if w == word)
    }

    fn eat(&mut self, punct: &str) -> bool {
        if self.is_punct(punct) {
            self.at += 1;
            true
        } else {
            false
        }
    }

    fn eat_word(&mut self, word: &str) -> bool {
        if self.is_word(word) {
            self.at += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, punct: &str) -> Result<()> {
        if self.eat(punct) {
            Ok(())
        } else {
            Err(self.error(format!("expected `{punct}`")))
        }
    }

    fn ident(&mut self) -> Result<String> {
        match self.peek() {
            Some(Kind::Ident(name)) => {
                let name = name.clone();
                self.at += 1;
                Ok(name)
            }
            _ => Err(self.error("expected an identifier")),
        }
    }

    fn error(&self, message: impl Into<String>) -> TranslateError {
        let found = match self.peek() {
            Some(Kind::Ident(name)) => format!(" at `{name}`"),
            Some(Kind::Punct(punct)) => format!(" at `{punct}`"),
            Some(Kind::Int { value, .. }) => format!(" at `{value}`"),
            Some(Kind::Float { value, .. }) => format!(" at `{value}`"),
            None => " at the end of the source".to_string(),
        };
        syntax_error(self.line(), format!("{}{found}", message.into()))
    }

    fn program(&mut self) -> Result<()> {
        while self.peek().is_some() {
            if self.eat(";") {
                continue;
            }
            if self.eat_word("using") {
                // `using namespace metal;` is the only form Metal source uses.
                while !self.eat(";") {
                    if self.peek().is_none() {
                        return Err(self.error("unterminated using declaration"));
                    }
                    self.at += 1;
                }
                continue;
            }
            if self.is_word("struct") && matches!(self.peek_at(2), Some(Kind::Punct("{"))) {
                self.structure()?;
                continue;
            }
            self.top_level()?;
        }
        Ok(())
    }

    fn structure(&mut self) -> Result<()> {
        self.at += 1;
        let name = self.ident()?;
        self.expect("{")?;
        let mut fields = Vec::new();
        while !self.eat("}") {
            let line = self.line();
            self.skip_qualifiers();
            let base = self.ty()?;
            loop {
                let name = self.ident()?;
                let ty = self.array_suffix(base.clone())?;
                let attributes = self.attributes()?;
                fields.push(Field {
                    name,
                    ty,
                    attributes,
                    line,
                });
                if !self.eat(",") {
                    break;
                }
            }
            self.expect(";")?;
        }
        self.expect(";")?;
        self.program.structs.push(StructDef { name, fields });
        Ok(())
    }

    /// A global variable or a function definition.
    fn top_level(&mut self) -> Result<()> {
        let line = self.line();
        // Function attributes such as `[[early_fragment_tests]]` precede the
        // stage; none of them changes what this frontend emits but one.
        let leading = self.attributes()?;
        if let Some(attribute) = leading.first() {
            return Err(syntax_error(
                attribute.line,
                format!("unsupported function attribute [[{}]]", attribute.name),
            ));
        }
        let stage = if self.eat_word("vertex") {
            Some(Stage::Vertex)
        } else if self.eat_word("fragment") {
            Some(Stage::Fragment)
        } else if self.eat_word("kernel") {
            Some(Stage::Kernel)
        } else {
            None
        };
        let space = self.skip_qualifiers();
        let ty = self.ty()?;
        let name = self.ident()?;
        if self.is_punct("(") {
            let params = self.parameters()?;
            if self.eat(";") {
                // A prototype; the definition follows.
                return Ok(());
            }
            let body = self.block()?;
            self.program.functions.push(FunctionDef {
                stage,
                ret: ty,
                name,
                params,
                body,
                line,
            });
            return Ok(());
        }
        if stage.is_some() {
            return Err(syntax_error(
                line,
                "a stage qualifier on something not a function",
            ));
        }
        if space != Some(Space::Constant) {
            return Err(syntax_error(
                line,
                "a file-scope variable outside the constant address space",
            ));
        }
        let ty = self.array_suffix(ty)?;
        self.expect("=")?;
        let init = self.initializer()?;
        self.expect(";")?;
        // `T name[] = {...}` takes its length from the initializer.
        let ty = match (ty, &init.kind) {
            (Ty::Array(element, 0), ExprKind::InitList(items)) => {
                Ty::Array(element, items.len() as u32)
            }
            (ty, _) => ty,
        };
        self.program.globals.push(Global {
            name,
            ty,
            init,
            line,
        });
        Ok(())
    }

    fn initializer(&mut self) -> Result<Expr> {
        if self.is_punct("{") {
            let line = self.line();
            self.at += 1;
            let mut items = Vec::new();
            while !self.eat("}") {
                items.push(self.initializer()?);
                if !self.eat(",") {
                    self.expect("}")?;
                    break;
                }
            }
            return Ok(Expr {
                kind: ExprKind::InitList(items),
                line,
            });
        }
        self.assignment()
    }

    /// Skip declaration qualifiers, answering the address space among them.
    fn skip_qualifiers(&mut self) -> Option<Space> {
        let mut space = None;
        loop {
            match self.peek() {
                Some(Kind::Ident(word)) => match word.as_str() {
                    "const" | "constexpr" | "static" | "inline" | "volatile" | "metal" => {}
                    "constant" => space = Some(Space::Constant),
                    "device" => space = Some(Space::Device),
                    "thread" => space = Some(Space::Thread),
                    "threadgroup" => space = Some(Space::Threadgroup),
                    _ => return space,
                },
                Some(Kind::Punct("::")) => {}
                _ => return space,
            }
            self.at += 1;
        }
    }

    fn array_suffix(&mut self, ty: Ty) -> Result<Ty> {
        let mut dimensions = Vec::new();
        while self.is_punct("[") && !matches!(self.peek_at(1), Some(Kind::Punct("["))) {
            self.at += 1;
            if self.eat("]") {
                dimensions.push(0);
                continue;
            }
            let count = match self.peek() {
                Some(Kind::Int { value, .. }) => *value,
                _ => return Err(self.error("an array length must be an integer literal")),
            };
            self.at += 1;
            self.expect("]")?;
            dimensions.push(u32::try_from(count).map_err(|_| self.error("array too long"))?);
        }
        Ok(dimensions
            .into_iter()
            .rev()
            .fold(ty, |ty, count| Ty::Array(Box::new(ty), count)))
    }

    /// Zero or more `[[ ... ]]` groups.
    fn attributes(&mut self) -> Result<Vec<Attribute>> {
        let mut attributes = Vec::new();
        while self.is_punct("[") && matches!(self.peek_at(1), Some(Kind::Punct("["))) {
            self.at += 2;
            loop {
                let line = self.line();
                let name = self.ident()?;
                let mut args = Vec::new();
                if self.eat("(") {
                    while !self.eat(")") {
                        args.push(self.assignment()?);
                        if !self.eat(",") {
                            self.expect(")")?;
                            break;
                        }
                    }
                }
                attributes.push(Attribute { name, args, line });
                if !self.eat(",") {
                    break;
                }
            }
            self.expect("]")?;
            self.expect("]")?;
        }
        Ok(attributes)
    }

    fn parameters(&mut self) -> Result<Vec<Param>> {
        self.expect("(")?;
        let mut params = Vec::new();
        if self.eat(")") {
            return Ok(params);
        }
        loop {
            let line = self.line();
            let space = self.skip_qualifiers();
            let ty = self.ty()?;
            // `const` may follow the type, before the indirection.
            self.skip_qualifiers();
            let indirection = if self.eat("&") {
                Indirection::Reference
            } else if self.eat("*") {
                Indirection::Pointer
            } else {
                Indirection::Value
            };
            self.skip_qualifiers();
            let name = self.ident()?;
            let ty = self.array_suffix(ty)?;
            let attributes = self.attributes()?;
            params.push(Param {
                ty,
                name,
                space,
                indirection,
                attributes,
                line,
            });
            if !self.eat(",") {
                break;
            }
        }
        self.expect(")")?;
        Ok(params)
    }

    /// Whether the tokens at the cursor begin a type.
    fn at_type(&self) -> bool {
        match self.peek() {
            Some(Kind::Ident(word)) => {
                scalar_named(word).is_some()
                    || vector_named(word).is_some()
                    || texture_dimension(word).is_some()
                    || matches!(word.as_str(), "sampler" | "void" | "unsigned" | "vec")
                    || self.struct_named(word).is_some()
            }
            _ => false,
        }
    }

    fn struct_named(&self, name: &str) -> Option<usize> {
        self.program
            .structs
            .iter()
            .position(|structure| structure.name == name)
    }

    fn ty(&mut self) -> Result<Ty> {
        let line = self.line();
        let word = self.ident()?;
        if word == "void" {
            return Ok(Ty::Void);
        }
        if word == "unsigned" {
            // `unsigned int`, `unsigned short`, `unsigned char`, or `unsigned`.
            let bits = if self.eat_word("int") {
                32
            } else if self.eat_word("short") {
                16
            } else if self.eat_word("char") {
                8
            } else {
                32
            };
            return Ok(Ty::Scalar(Scalar::Int {
                signed: false,
                bits,
            }));
        }
        if let Some(scalar) = scalar_named(&word) {
            return Ok(Ty::Scalar(scalar));
        }
        if let Some((scalar, lanes)) = vector_named(&word) {
            return Ok(Ty::Vector(scalar, lanes));
        }
        if word == "sampler" {
            return Ok(Ty::Sampler);
        }
        if word == "vec" {
            self.expect("<")?;
            let element = self.ty()?;
            self.expect(",")?;
            let lanes = match self.peek() {
                Some(Kind::Int { value, .. }) if (2..=4).contains(value) => *value as u8,
                _ => return Err(self.error("a vec<T, N> lane count must be 2, 3 or 4")),
            };
            self.at += 1;
            self.expect(">")?;
            let Ty::Scalar(scalar) = element else {
                return Err(syntax_error(line, "a vec<T, N> element must be a scalar"));
            };
            return Ok(Ty::Vector(scalar, lanes));
        }
        if let Some((dimension, array, multisampled, depth)) = texture_dimension(&word) {
            return self.texture(line, dimension, array, multisampled, depth);
        }
        if let Some(index) = self.struct_named(&word) {
            return Ok(Ty::Struct(index));
        }
        if word.starts_with("packed_") {
            return Err(syntax_error(
                line,
                format!("packed type {word} is not supported"),
            ));
        }
        if word.contains('x') && word.starts_with(|c: char| c == 'f' || c == 'h') {
            return Err(syntax_error(
                line,
                format!("matrix type {word} is not supported"),
            ));
        }
        Err(syntax_error(line, format!("unknown type {word}")))
    }

    fn texture(
        &mut self,
        line: usize,
        dimension: TextureDimension,
        array: bool,
        multisampled: bool,
        depth: bool,
    ) -> Result<Ty> {
        self.expect("<")?;
        let element = self.ty()?;
        let mut access = Access::Sample;
        if self.eat(",") {
            self.eat_word("access");
            self.expect("::")?;
            access = match self.ident()?.as_str() {
                "sample" => Access::Sample,
                "read" => Access::Read,
                "write" => Access::Write,
                "read_write" => Access::ReadWrite,
                other => {
                    return Err(syntax_error(
                        line,
                        format!("unknown texture access {other}"),
                    ));
                }
            };
        }
        self.expect(">")?;
        // Vulkan's sampled type is a 32-bit scalar, so a `half` or `short`
        // texture reads and writes 32-bit texels, which an assignment or a
        // constructor then narrows.
        // FIXME (ACCURACY): arithmetic directly on a `half` texture's result
        // runs at float precision rather than half.
        let (texel, signed) = match element {
            Ty::Scalar(Scalar::Float { .. }) => (Type::Float { bits: 32 }, false),
            Ty::Scalar(Scalar::Int { signed, bits }) if bits >= 16 => (
                Type::Int {
                    bits: 32,
                    signed: false,
                },
                signed,
            ),
            _ => return Err(syntax_error(line, "unsupported texture element type")),
        };
        if depth && element != Ty::Scalar(Scalar::FLOAT) {
            return Err(syntax_error(
                line,
                "a depth texture's element must be float",
            ));
        }
        Ok(Ty::Texture(TextureType {
            dimension,
            array,
            multisampled,
            depth,
            texel: Box::new(texel),
            signed,
            access,
        }))
    }

    fn block(&mut self) -> Result<Vec<Stmt>> {
        self.expect("{")?;
        let mut statements = Vec::new();
        while !self.eat("}") {
            if self.peek().is_none() {
                return Err(self.error("unterminated block"));
            }
            self.statement(&mut statements)?;
        }
        Ok(statements)
    }

    /// One statement, or a body: a block or a single statement.
    fn body(&mut self) -> Result<Vec<Stmt>> {
        if self.is_punct("{") {
            return self.block();
        }
        let mut statements = Vec::new();
        self.statement(&mut statements)?;
        Ok(statements)
    }

    fn statement(&mut self, out: &mut Vec<Stmt>) -> Result<()> {
        let line = self.line();
        if self.eat(";") {
            return Ok(());
        }
        if self.is_punct("{") {
            out.push(Stmt::Block(self.block()?));
            return Ok(());
        }
        if self.eat_word("if") {
            self.expect("(")?;
            let condition = self.expression()?;
            self.expect(")")?;
            let then = self.body()?;
            let otherwise = if self.eat_word("else") {
                Some(self.body()?)
            } else {
                None
            };
            out.push(Stmt::If {
                condition,
                then,
                otherwise,
            });
            return Ok(());
        }
        if self.eat_word("while") {
            self.expect("(")?;
            let condition = self.expression()?;
            self.expect(")")?;
            let body = self.body()?;
            out.push(Stmt::While { condition, body });
            return Ok(());
        }
        if self.eat_word("do") {
            let body = self.body()?;
            if !self.eat_word("while") {
                return Err(self.error("expected `while`"));
            }
            self.expect("(")?;
            let condition = self.expression()?;
            self.expect(")")?;
            self.expect(";")?;
            out.push(Stmt::DoWhile { body, condition });
            return Ok(());
        }
        if self.eat_word("for") {
            self.expect("(")?;
            let mut init = Vec::new();
            if !self.eat(";") {
                self.simple_statement(&mut init)?;
                self.expect(";")?;
            }
            let condition = if self.is_punct(";") {
                None
            } else {
                Some(self.expression()?)
            };
            self.expect(";")?;
            let step = if self.is_punct(")") {
                None
            } else {
                Some(self.expression()?)
            };
            self.expect(")")?;
            let body = self.body()?;
            out.push(Stmt::For {
                init,
                condition,
                step,
                body,
            });
            return Ok(());
        }
        if self.eat_word("break") {
            self.expect(";")?;
            out.push(Stmt::Break(line));
            return Ok(());
        }
        if self.eat_word("continue") {
            self.expect(";")?;
            out.push(Stmt::Continue(line));
            return Ok(());
        }
        if self.eat_word("return") {
            let value = if self.is_punct(";") {
                None
            } else {
                Some(self.expression()?)
            };
            self.expect(";")?;
            out.push(Stmt::Return(value, line));
            return Ok(());
        }
        if self.is_word("switch") || self.is_word("goto") {
            return Err(self.error("unsupported statement"));
        }
        self.simple_statement(out)?;
        self.expect(";")
    }

    /// A declaration or an expression, without its `;`.
    fn simple_statement(&mut self, out: &mut Vec<Stmt>) -> Result<()> {
        let line = self.line();
        let start = self.at;
        let space = self.skip_qualifiers();
        if self.at_type() {
            let mark = self.at;
            let ty = self.ty()?;
            if matches!(self.peek(), Some(Kind::Ident(_))) {
                if space == Some(Space::Threadgroup) {
                    loop {
                        let line = self.line();
                        let name = self.ident()?;
                        let ty = self.array_suffix(ty.clone())?;
                        if self.is_punct("=") || self.is_punct("(") {
                            return Err(syntax_error(line, "an initialized threadgroup variable"));
                        }
                        out.push(Stmt::Threadgroup { ty, name, line });
                        if !self.eat(",") {
                            return Ok(());
                        }
                    }
                }
                if space == Some(Space::Device) {
                    return Err(syntax_error(line, "a local in a non-thread address space"));
                }
                loop {
                    let line = self.line();
                    let name = self.ident()?;
                    let ty = self.array_suffix(ty.clone())?;
                    let init = if self.eat("=") {
                        Some(self.initializer()?)
                    } else if self.is_punct("(") {
                        // `float4 x(a, b)`: direct initialization.
                        let args = self.arguments()?;
                        Some(Expr {
                            kind: ExprKind::Construct(ty.clone(), args),
                            line,
                        })
                    } else {
                        None
                    };
                    out.push(Stmt::Decl {
                        ty,
                        name,
                        init,
                        line,
                    });
                    if !self.eat(",") {
                        return Ok(());
                    }
                }
            }
            self.at = mark;
        }
        self.at = start;
        out.push(Stmt::Expr(self.expression()?));
        Ok(())
    }

    fn arguments(&mut self) -> Result<Vec<Expr>> {
        self.expect("(")?;
        let mut args = Vec::new();
        if self.eat(")") {
            return Ok(args);
        }
        loop {
            args.push(self.assignment()?);
            if !self.eat(",") {
                break;
            }
        }
        self.expect(")")?;
        Ok(args)
    }

    fn expression(&mut self) -> Result<Expr> {
        let first = self.assignment()?;
        if self.is_punct(",") {
            return Err(self.error("the comma operator is not supported"));
        }
        Ok(first)
    }

    fn assignment(&mut self) -> Result<Expr> {
        let line = self.line();
        let lhs = self.ternary()?;
        let op = match self.peek() {
            Some(Kind::Punct("=")) => None,
            Some(Kind::Punct(op))
                if op.len() >= 2
                    && op.ends_with('=')
                    && !matches!(*op, "==" | "!=" | "<=" | ">=") =>
            {
                Some(&op[..op.len() - 1])
            }
            _ => return Ok(lhs),
        };
        self.at += 1;
        let op = op.map(static_op);
        let rhs = self.assignment()?;
        Ok(Expr {
            kind: ExprKind::Assign(op, Box::new(lhs), Box::new(rhs)),
            line,
        })
    }

    fn ternary(&mut self) -> Result<Expr> {
        let line = self.line();
        let condition = self.binary(0)?;
        if !self.eat("?") {
            return Ok(condition);
        }
        let yes = self.assignment()?;
        self.expect(":")?;
        let no = self.assignment()?;
        Ok(Expr {
            kind: ExprKind::Ternary(Box::new(condition), Box::new(yes), Box::new(no)),
            line,
        })
    }

    fn binary(&mut self, minimum: u8) -> Result<Expr> {
        let mut lhs = self.unary()?;
        loop {
            let Some(Kind::Punct(op)) = self.peek() else {
                return Ok(lhs);
            };
            let op = *op;
            let precedence = match op {
                "||" => 1,
                "&&" => 2,
                "|" => 3,
                "^" => 4,
                "&" => 5,
                "==" | "!=" => 6,
                "<" | ">" | "<=" | ">=" => 7,
                "<<" | ">>" => 8,
                "+" | "-" => 9,
                "*" | "/" | "%" => 10,
                _ => return Ok(lhs),
            };
            if precedence < minimum {
                return Ok(lhs);
            }
            let line = self.line();
            self.at += 1;
            let rhs = self.binary(precedence + 1)?;
            lhs = Expr {
                kind: ExprKind::Binary(op, Box::new(lhs), Box::new(rhs)),
                line,
            };
        }
    }

    fn unary(&mut self) -> Result<Expr> {
        let line = self.line();
        for op in ["-", "+", "!", "~", "++", "--", "&"] {
            if self.eat(op) {
                let operand = self.unary()?;
                return Ok(Expr {
                    kind: ExprKind::Unary(static_op(op), Box::new(operand)),
                    line,
                });
            }
        }
        if self.is_punct("(") {
            // A cast or a compound literal: `(` type `)`.
            let start = self.at;
            self.at += 1;
            if self.at_type() {
                let ty = self.ty()?;
                if self.eat(")") {
                    if self.is_punct("{") {
                        return self.compound_literal(ty, line);
                    }
                    let operand = self.unary()?;
                    return Ok(Expr {
                        kind: ExprKind::Cast(ty, Box::new(operand)),
                        line,
                    });
                }
            }
            self.at = start;
        }
        self.postfix()
    }

    fn compound_literal(&mut self, ty: Ty, line: usize) -> Result<Expr> {
        self.expect("{")?;
        let mut items = Vec::new();
        while !self.eat("}") {
            let name = if self.eat(".") {
                let name = self.ident()?;
                self.expect("=")?;
                Some(name)
            } else {
                None
            };
            items.push((name, self.initializer()?));
            if !self.eat(",") {
                self.expect("}")?;
                break;
            }
        }
        Ok(Expr {
            kind: ExprKind::CompoundLiteral(ty, items),
            line,
        })
    }

    fn postfix(&mut self) -> Result<Expr> {
        let mut expr = self.primary()?;
        loop {
            let line = self.line();
            if self.eat(".") {
                let name = self.ident()?;
                if self.is_punct("(") {
                    let args = self.arguments()?;
                    expr = Expr {
                        kind: ExprKind::Method {
                            object: Box::new(expr),
                            name,
                            args,
                        },
                        line,
                    };
                } else {
                    expr = Expr {
                        kind: ExprKind::Member(Box::new(expr), name),
                        line,
                    };
                }
            } else if self.is_punct("->") {
                return Err(self.error("`->` is not supported"));
            } else if self.is_punct("[") && !matches!(self.peek_at(1), Some(Kind::Punct("["))) {
                self.at += 1;
                let index = self.expression()?;
                self.expect("]")?;
                expr = Expr {
                    kind: ExprKind::Index(Box::new(expr), Box::new(index)),
                    line,
                };
            } else if self.is_punct("++") || self.is_punct("--") {
                let op = if self.eat("++") {
                    "++"
                } else {
                    self.at += 1;
                    "--"
                };
                expr = Expr {
                    kind: ExprKind::Postfix(op, Box::new(expr)),
                    line,
                };
            } else {
                return Ok(expr);
            }
        }
    }

    fn primary(&mut self) -> Result<Expr> {
        let line = self.line();
        match self.peek().cloned() {
            Some(Kind::Int { value, unsigned }) => {
                self.at += 1;
                Ok(Expr {
                    kind: ExprKind::Int { value, unsigned },
                    line,
                })
            }
            Some(Kind::Float { value, half }) => {
                self.at += 1;
                Ok(Expr {
                    kind: ExprKind::Float { value, half },
                    line,
                })
            }
            Some(Kind::Punct("(")) => {
                self.at += 1;
                let inner = self.expression()?;
                self.expect(")")?;
                Ok(inner)
            }
            Some(Kind::Ident(word)) => {
                if word == "true" || word == "false" {
                    self.at += 1;
                    return Ok(Expr {
                        kind: ExprKind::Bool(word == "true"),
                        line,
                    });
                }
                if self.at_type() && matches!(self.peek_at(1), Some(Kind::Punct("(" | "<"))) {
                    let ty = self.ty()?;
                    let args = self.arguments()?;
                    return Ok(Expr {
                        kind: ExprKind::Construct(ty, args),
                        line,
                    });
                }
                self.at += 1;
                // `metal::` and `access::` qualify names this frontend knows
                // unqualified.
                if self.eat("::") {
                    return self.primary();
                }
                if word == "reinterpret_cast" && self.is_punct("<") {
                    let mark = self.at;
                    self.at += 1;
                    self.skip_qualifiers();
                    let scalar = match self.peek() {
                        Some(Kind::Ident(name)) if name == "atomic_uint" => Some(Scalar::UINT),
                        Some(Kind::Ident(name)) if name == "atomic_int" => Some(Scalar::INT),
                        _ => None,
                    };
                    if let Some(scalar) = scalar {
                        self.at += 1;
                        self.expect("*")?;
                        self.expect(">")?;
                        self.expect("(")?;
                        let pointer = self.expression()?;
                        self.expect(")")?;
                        return Ok(Expr {
                            kind: ExprKind::AtomicCast(scalar, Box::new(pointer)),
                            line,
                        });
                    }
                    self.at = mark;
                }
                if self.is_punct("<")
                    && matches!(
                        word.as_str(),
                        "as_type" | "static_cast" | "reinterpret_cast"
                    )
                {
                    self.at += 1;
                    let template = self.ty()?;
                    self.expect(">")?;
                    let args = self.arguments()?;
                    return Ok(Expr {
                        kind: ExprKind::Call {
                            name: word,
                            template: Some(template),
                            args,
                        },
                        line,
                    });
                }
                if self.is_punct("(") {
                    let args = self.arguments()?;
                    return Ok(Expr {
                        kind: ExprKind::Call {
                            name: word,
                            template: None,
                            args,
                        },
                        line,
                    });
                }
                Ok(Expr {
                    kind: ExprKind::Ident(word),
                    line,
                })
            }
            _ => Err(self.error("expected an expression")),
        }
    }
}

fn static_op(op: &str) -> &'static str {
    const OPS: &[&str] = &[
        "+", "-", "*", "/", "%", "&", "|", "^", "<<", ">>", "!", "~", "++", "--",
    ];
    OPS.iter()
        .copied()
        .find(|known| *known == op)
        .expect("an operator the lexer produced")
}

fn scalar_named(word: &str) -> Option<Scalar> {
    Some(match word {
        "bool" => Scalar::Bool,
        "char" | "int8_t" => Scalar::Int {
            signed: true,
            bits: 8,
        },
        "uchar" | "uint8_t" => Scalar::Int {
            signed: false,
            bits: 8,
        },
        "short" | "int16_t" => Scalar::Int {
            signed: true,
            bits: 16,
        },
        "ushort" | "uint16_t" => Scalar::Int {
            signed: false,
            bits: 16,
        },
        "int" | "int32_t" => Scalar::INT,
        "uint" | "uint32_t" => Scalar::UINT,
        "half" => Scalar::Float { bits: 16 },
        "float" => Scalar::FLOAT,
        _ => return None,
    })
}

fn vector_named(word: &str) -> Option<(Scalar, u8)> {
    let lanes = match word.as_bytes().last()? {
        b'2' => 2,
        b'3' => 3,
        b'4' => 4,
        _ => return None,
    };
    Some((scalar_named(&word[..word.len() - 1])?, lanes))
}

/// The shape a texture type name spells: dimension, array, multisampled,
/// depth.
fn texture_dimension(word: &str) -> Option<(TextureDimension, bool, bool, bool)> {
    let (depth, body) = if let Some(body) = word.strip_prefix("texture") {
        (false, body)
    } else if let Some(body) = word.strip_prefix("depth") {
        (true, body)
    } else {
        return None;
    };
    let (body, array) = match body.strip_suffix("_array") {
        Some(body) => (body, true),
        None => (body, false),
    };
    let (body, multisampled) = match body.strip_suffix("_ms") {
        Some(body) => (body, true),
        None => (body, false),
    };
    let dimension = match body {
        "1d" => TextureDimension::D1,
        "2d" => TextureDimension::D2,
        "3d" => TextureDimension::D3,
        "cube" => TextureDimension::Cube,
        _ => return None,
    };
    Some((dimension, array, multisampled, depth))
}
