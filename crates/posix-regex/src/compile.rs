#[cfg(feature = "no_std")]
use alloc::collections::BTreeMap as HashMap;

#[cfg(not(feature = "no_std"))]
use std::collections::HashMap;

use alloc::{borrow::Cow, vec::Vec};
use core::fmt;

use crate::{ctype, tree::*, PosixRegex};

/// Repetition bounds, for example + is (1, None), and ? is (0, Some(1))
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Range(pub u32, pub Option<u32>);
impl fmt::Debug for Range {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Range(start, None) => write!(f, "{}..", start),
            Range(start, Some(end)) => write!(f, "{}..{}", start, end),
        }
    }
}

/// An item inside square brackets, like `[abc]` or `[[:digit:]]`
#[derive(Clone, PartialEq, Eq)]
#[allow(unpredictable_function_pointer_comparisons)]
pub enum Collation {
    Char(u8),
    Class(fn(u8) -> bool),
}
impl Collation {
    /// Compare this collation to a character
    pub fn matches(&self, other: u8, insensitive: bool) -> bool {
        match *self {
            Collation::Char(me) if insensitive => {
                if ctype::is_alpha(me) && ctype::is_alpha(other) {
                    me | 32 == other | 32
                } else {
                    me == other
                }
            }
            Collation::Char(me) => me == other,
            Collation::Class(f) => f(other),
        }
    }
}
impl fmt::Debug for Collation {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            Collation::Char(c) => write!(f, "{:?}", c as char),
            Collation::Class(c) => write!(f, "{:p}", c),
        }
    }
}

/// A single "compiled" token, such as a `.` or a character literal
#[derive(Clone, PartialEq, Eq)]
pub enum Token {
    /// Internal token used to find matches that might be anywhere in the text
    InternalStart,

    Alternative,
    Any,
    BackRef(u32),
    Char(u8),
    End,
    Group(usize),
    OneOf {
        invert: bool,
        list: Vec<Collation>,
    },
    Root,
    Start,
    WordEnd,
    WordStart,
}
impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            Token::InternalStart => write!(f, "<START>"),

            Token::Alternative => write!(f, "Alternative"),
            Token::Any => write!(f, "."),
            Token::BackRef(id) => write!(f, "\\{}", id),
            Token::Char(c) => write!(f, "{:?}", c as char),
            Token::End => write!(f, "$"),
            Token::Group(id) => write!(f, "Group({})", id),
            Token::OneOf { invert, ref list } => write!(f, "{{invert: {}, {:?}}}", invert, list),
            Token::Root => write!(f, "Root"),
            Token::Start => write!(f, "^"),
            Token::WordEnd => write!(f, ">"),
            Token::WordStart => write!(f, "<"),
        }
    }
}
/// An error that occurred while compiling the regex
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    EOF,
    EmptyRepetition,
    EmptyAlternative,
    Expected(u8, Option<u8>),
    IllegalRange,
    InvalidCharacterRange,
    IntegerOverflow,
    InvalidBackRef(u32),
    LeadingRepetition,
    UnclosedRepetition,
    UnexpectedToken(u8),
    UnknownClass(Vec<u8>),
    UnknownCollation,
}

/// A regex builder struct
pub struct PosixRegexBuilder<'a> {
    input: &'a [u8],
    classes: HashMap<&'a [u8], fn(u8) -> bool>,
    group_id: usize,
    builder: TreeBuilder,
    extended: bool,
}
impl<'a> PosixRegexBuilder<'a> {
    /// Create a new instance that is ready to parse the regex `input`
    pub fn new(input: &'a [u8]) -> Self {
        Self {
            input,
            classes: HashMap::new(),
            group_id: 1,
            builder: TreeBuilder::default(),
            extended: false,
        }
    }
    /// Add a custom collation class, for use within square brackets (such as `[[:digit:]]`)
    pub fn with_class(mut self, name: &'a [u8], callback: fn(u8) -> bool) -> Self {
        self.classes.insert(name, callback);
        self
    }
    /// Add all the default collation classes, like `[[:digit:]]` and `[[:alnum:]]`
    pub fn with_default_classes(mut self) -> Self {
        #[cfg(not(feature = "no_std"))]
        self.classes.reserve(12);

        self.classes.insert(b"alnum", ctype::is_alnum);
        self.classes.insert(b"alpha", ctype::is_alpha);
        self.classes.insert(b"blank", ctype::is_blank);
        self.classes.insert(b"cntrl", ctype::is_cntrl);
        self.classes.insert(b"digit", ctype::is_digit);
        self.classes.insert(b"graph", ctype::is_graph);
        self.classes.insert(b"lower", ctype::is_lower);
        self.classes.insert(b"print", ctype::is_print);
        self.classes.insert(b"punct", ctype::is_punct);
        self.classes.insert(b"space", ctype::is_space);
        self.classes.insert(b"upper", ctype::is_upper);
        self.classes.insert(b"xdigit", ctype::is_xdigit);

        self
    }
    /// Use POSIX extended regex
    pub fn extended(mut self, extended: bool) -> Self {
        self.extended = extended;
        self
    }
    /// "Compile" this regex to a struct ready to match input
    pub fn compile(self) -> Result<PosixRegex<'static>, Error> {
        let tree = self.compile_tokens()?;
        Ok(PosixRegex::new(Cow::Owned(tree)))
    }
    pub fn compile_tokens(mut self) -> Result<Tree, Error> {
        self.builder.start_internal(Token::Root, Range(1, Some(1)));
        self.parse(0)?;
        self.builder.finish_internal();
        Ok(self.builder.finish())
    }

    fn consume(&mut self, amount: usize) {
        self.input = &self.input[amount..];
    }
    fn take_int(&mut self) -> Result<Option<u32>, Error> {
        let mut out: Option<u32> = None;
        while let Some(&c @ b'0'..=b'9') = self.input.first() {
            self.consume(1);
            out = Some(
                out.unwrap_or(0)
                    .checked_mul(10)
                    .and_then(|out| out.checked_add((c - b'0') as u32))
                    .ok_or(Error::IntegerOverflow)?,
            );
        }
        Ok(out)
    }
    fn next(&mut self) -> Result<u8, Error> {
        self.input
            .first()
            .map(|&c| {
                self.consume(1);
                c
            })
            .ok_or(Error::EOF)
    }
    fn expect(&mut self, c: u8) -> Result<(), Error> {
        if self.input.first() != Some(&c) {
            return Err(Error::Expected(c, self.input.first().cloned()));
        }
        self.consume(1);
        Ok(())
    }
    fn parse_range(&mut self) -> Result<Range, Error> {
        let mut range = Range(1, Some(1));
        if let Some(&c) = self.input.first() {
            let new = match c {
                b'*' => Some((1, Range(0, None))),
                b'\\' => match self.input.get(1) {
                    Some(b'{') if !self.extended => {
                        self.consume(2);
                        let first = self.take_int()?.ok_or(Error::EmptyRepetition)?;
                        let mut second = Some(first);
                        if let Some(b',') = self.input.first() {
                            self.consume(1);
                            second = self.take_int()?;
                        }
                        if self.input.first() == Some(&b'}') {
                            self.consume(1);
                        } else if self.input.starts_with(br"\}") {
                            self.consume(2);
                        } else {
                            return Err(Error::UnclosedRepetition);
                        }
                        if second.map(|second| first > second).unwrap_or(false) {
                            return Err(Error::IllegalRange);
                        }
                        range = Range(first, second);
                        None
                    }
                    _ => None,
                },
                b'?' if self.extended => Some((1, Range(0, Some(1)))),
                b'+' if self.extended => Some((1, Range(1, None))),
                b'{' if self.extended => {
                    self.consume(1);
                    let first = self.take_int()?.ok_or(Error::EmptyRepetition)?;
                    let mut second = Some(first);
                    if let Some(b',') = self.input.first() {
                        self.consume(1);
                        second = self.take_int()?;
                    }
                    if self.input.first() == Some(&b'}') {
                        self.consume(1);
                    } else if self.input.starts_with(br"\}") {
                        self.consume(2);
                    } else {
                        return Err(Error::UnclosedRepetition);
                    }
                    if second.map(|second| first > second).unwrap_or(false) {
                        return Err(Error::IllegalRange);
                    }
                    range = Range(first, second);
                    None
                }
                _ => None,
            };
            if let Some((consume, new)) = new {
                range = new;
                self.consume(consume);
            }
        }
        Ok(range)
    }
    fn parse(&mut self, depth: usize) -> Result<(), Error> {
        self.builder
            .start_internal(Token::Alternative, Range(1, Some(1)));
        let mut closed = false;
        let mut branch_has_token = false;
        let mut had_alternative = false;
        while let Ok(c) = self.next() {
            let token = match c {
                b'^' => Token::Start,
                b'$' => Token::End,
                b'.' => Token::Any,
                b'[' if self.input.starts_with(b"[:<:]]") => {
                    self.consume(6);
                    Token::WordStart
                }
                b'[' if self.input.starts_with(b"[:>:]]") => {
                    self.consume(6);
                    Token::WordEnd
                }
                b'[' => {
                    let mut list = Vec::new();
                    let invert = self.input.first() == Some(&b'^');

                    if invert {
                        self.consume(1);
                    }

                    loop {
                        let mut c = self.next()?;

                        let mut push = true;

                        if c == b'[' {
                            // TODO: Handle collation characters properly,
                            // because currently idk what they are and only
                            // have the behavior of `grep` to go on.
                            match self.next()? {
                                b'.' => {
                                    c = self.next()?;
                                    self.expect(b'.')?;
                                    self.expect(b']')?;
                                }
                                b'=' => {
                                    c = self.next()?;
                                    self.expect(b'=')?;
                                    self.expect(b']')?;
                                }
                                b':' => {
                                    let end = self
                                        .input
                                        .iter()
                                        .position(|&c| c == b':')
                                        .ok_or(Error::EOF)?;
                                    let key = &self.input[..end];
                                    let class = *self
                                        .classes
                                        .get(key)
                                        .ok_or_else(|| Error::UnknownClass(key.to_vec()))?;
                                    self.consume(end + 1);
                                    self.expect(b']')?;

                                    list.push(Collation::Class(class));
                                    push = false;
                                }
                                _ => return Err(Error::UnknownCollation),
                            }
                        }

                        if push {
                            list.push(Collation::Char(c));

                            if self.input.first() == Some(&b'-') && self.input.get(1) != Some(&b']')
                            {
                                self.consume(1);
                                let dest = self.next()?;
                                if dest < c {
                                    return Err(Error::InvalidCharacterRange);
                                }
                                for c in (c as u16 + 1)..=dest as u16 {
                                    list.push(Collation::Char(c as u8));
                                }
                            }
                        }

                        if self.input.first() == Some(&b']') {
                            self.consume(1);
                            break;
                        }
                    }

                    Token::OneOf { invert, list }
                }
                b'\\'
                    if self
                        .input
                        .first()
                        .map(|&c| c.is_ascii_digit())
                        .unwrap_or(false) =>
                {
                    let id = self.take_int()?.unwrap();
                    if (id as usize) >= self.group_id {
                        return Err(Error::InvalidBackRef(id));
                    }
                    Token::BackRef(id)
                }
                b'\\' => match self.next()? {
                    b'(' if !self.extended => {
                        let id = self.group_id;
                        self.group_id += 1;
                        let checkpoint = self.builder.checkpoint();
                        self.parse(depth + 1)?;
                        let range = self.parse_range()?;
                        self.builder
                            .start_internal_at(checkpoint, Token::Group(id), range);
                        self.builder.finish_internal();
                        branch_has_token = true;
                        continue;
                    }
                    b')' if !self.extended => {
                        if depth == 0 {
                            Token::Char(b')')
                        } else {
                            closed = true;
                            break;
                        }
                    }
                    c => Token::Char(c),
                },
                b'(' if self.extended => {
                    let id = self.group_id;
                    self.group_id += 1;
                    let checkpoint = self.builder.checkpoint();
                    self.parse(depth + 1)?;
                    let range = self.parse_range()?;
                    self.builder
                        .start_internal_at(checkpoint, Token::Group(id), range);
                    self.builder.finish_internal();
                    branch_has_token = true;
                    continue;
                }
                b')' if self.extended => {
                    if depth == 0 {
                        Token::Char(b')')
                    } else {
                        closed = true;
                        break;
                    }
                }
                b'|' if self.extended => {
                    if !branch_has_token {
                        return Err(Error::EmptyAlternative);
                    }
                    self.builder.finish_internal();
                    self.builder
                        .start_internal(Token::Alternative, Range(1, Some(1)));
                    branch_has_token = false;
                    had_alternative = true;
                    continue;
                }
                b'*' | b'+' | b'?' if self.extended => return Err(Error::LeadingRepetition),
                c => Token::Char(c),
            };
            let range = self.parse_range()?;
            self.builder.leaf(token, range);
            branch_has_token = true;
        }
        self.builder.finish_internal();
        if had_alternative && !branch_has_token {
            return Err(Error::EmptyAlternative);
        }
        if depth > 0 && !closed {
            return Err(Error::Expected(b')', None));
        }
        Ok(())
    }
}
