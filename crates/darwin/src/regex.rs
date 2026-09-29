use std::ffi::{CStr, c_char, c_int, c_void};

use fancy_regex::{
    BytesMode, Error as FancyError, ParseError, Regex as FancyRegex, RegexBuilder, RegexInput,
};
use posix_regex::{PosixRegex, PosixRegexBuilder, compile::Error as PosixError};

/// Darwin's `regmatch_t`: two 64-bit `regoff_t` values.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct GuestMatch {
    pub rm_so: i64,
    pub rm_eo: i64,
}

const _: () = assert!(size_of::<GuestMatch>() == 16);

/// Darwin's public `regex_t`. The compiled Rust value is held behind `re_g`.
#[repr(C)]
pub struct GuestRegex {
    pub re_magic: i32,
    pub _pad: u32,
    pub re_nsub: usize,
    pub re_endp: *const c_char,
    pub re_g: *mut c_void,
}

const _: () = assert!(size_of::<GuestRegex>() == 32);

const MAGIC: i32 = 0x5233_5245;

pub mod flags {
    pub const EXTENDED: i32 = 0o001;
    pub const ICASE: i32 = 0o002;
    pub const NOSUB: i32 = 0o004;
    pub const NEWLINE: i32 = 0o010;
    pub const NOSPEC: i32 = 0o020;
    pub const PEND: i32 = 0o040;
    pub const MINIMAL: i32 = 0o100;
    pub const DUMP: i32 = 0o200;
    pub const ENHANCED: i32 = 0o400;
    pub const NOTBOL: i32 = 0o001;
    pub const NOTEOL: i32 = 0o002;
    pub const STARTEND: i32 = 0o004;
    pub const TRACE: i32 = 0o400;
    pub const LARGE: i32 = 0o1000;
    pub const BACKR: i32 = 0o2000;
    pub const ITOA: i32 = 0o400;
    pub const ATOI: i32 = 255;
}

const COMPILE_FLAGS: i32 = flags::EXTENDED
    | flags::ICASE
    | flags::NOSUB
    | flags::NEWLINE
    | flags::NOSPEC
    | flags::PEND
    | flags::MINIMAL
    | flags::DUMP
    | flags::ENHANCED;
const EXEC_FLAGS: i32 =
    flags::NOTBOL | flags::NOTEOL | flags::STARTEND | flags::TRACE | flags::LARGE | flags::BACKR;

pub const REG_NOMATCH: i32 = 1;
const REG_BADPAT: i32 = 2;
const REG_ECTYPE: i32 = 4;
const REG_EESCAPE: i32 = 5;
const REG_ESUBREG: i32 = 6;
const REG_EBRACK: i32 = 7;
const REG_EPAREN: i32 = 8;
const REG_EBRACE: i32 = 9;
const REG_BADBR: i32 = 10;
const REG_ESPACE: i32 = 12;
const REG_BADRPT: i32 = 13;
const REG_EMPTY: i32 = 14;
const REG_ASSERT: i32 = 15;
const REG_INVARG: i32 = 16;
const REG_BADMAX: i32 = 18;

enum Engine {
    Posix(PosixRegex<'static>),
    Enhanced(FancyRegex),
    Literal(Vec<u8>),
}

struct Compiled {
    engine: Engine,
    cflags: i32,
}

fn posix_error(error: PosixError) -> i32 {
    match error {
        PosixError::EOF => REG_EESCAPE,
        PosixError::EmptyAlternative => REG_EMPTY,
        PosixError::EmptyRepetition | PosixError::UnclosedRepetition => REG_EBRACE,
        PosixError::Expected(b']', _) => REG_EBRACK,
        PosixError::Expected(b')', _) => REG_EPAREN,
        PosixError::Expected(_, _) => REG_BADPAT,
        PosixError::IllegalRange => REG_BADBR,
        PosixError::InvalidCharacterRange => 11,
        PosixError::IntegerOverflow => REG_BADMAX,
        PosixError::InvalidBackRef(_) => REG_ESUBREG,
        PosixError::LeadingRepetition => REG_BADRPT,
        PosixError::UnexpectedToken(b')') => REG_EPAREN,
        PosixError::UnexpectedToken(_) => REG_BADPAT,
        PosixError::UnknownClass(_) => REG_ECTYPE,
        PosixError::UnknownCollation => REG_BADPAT,
    }
}

fn fancy_error(error: FancyError) -> i32 {
    match error {
        FancyError::ParseError(_, ParseError::UnclosedOpenParen) => REG_EPAREN,
        FancyError::ParseError(_, ParseError::TrailingBackslash) => REG_EESCAPE,
        FancyError::ParseError(_, ParseError::InvalidClass) => REG_EBRACK,
        FancyError::ParseError(_, ParseError::InvalidRepeat | ParseError::TargetNotRepeatable) => {
            REG_BADRPT
        }
        FancyError::ParseError(_, ParseError::InvalidBackref)
        | FancyError::ParseError(_, ParseError::InvalidGroupNameBackref(_)) => REG_ESUBREG,
        FancyError::CompileError(error) => match *error {
            fancy_regex::CompileError::InvalidBackref(_)
            | fancy_regex::CompileError::InvalidGroupNameBackref(_) => REG_ESUBREG,
            _ => REG_BADPAT,
        },
        _ => REG_BADPAT,
    }
}

fn fancy_pattern(input: &[u8], basic: bool, enhanced: bool) -> String {
    let mut output = String::with_capacity(input.len());
    let mut bracket = false;
    let mut quoted = false;
    let mut index = 0;
    while index < input.len() {
        let byte = input[index];
        if enhanced && !bracket && byte == b'\\' && index + 1 < input.len() {
            if input[index + 1] == b'Q' && !quoted {
                quoted = true;
                index += 2;
                continue;
            }
            if input[index + 1] == b'E' && quoted {
                quoted = false;
                index += 2;
                continue;
            }
        }
        if quoted {
            use std::fmt::Write;
            write!(output, "\\x{byte:02X}").expect("write to String");
            index += 1;
            continue;
        }
        if byte == b'[' {
            bracket = true;
        } else if byte == b']' {
            bracket = false;
        }
        if basic && !bracket && byte == b'\\' && index + 1 < input.len() {
            let next = input[index + 1];
            if matches!(next, b'(' | b')' | b'|' | b'+' | b'?' | b'{' | b'}') {
                output.push(next as char);
                index += 2;
                continue;
            }
        }
        if basic && !bracket && matches!(byte, b'(' | b')' | b'|' | b'+' | b'?' | b'{' | b'}') {
            output.push('\\');
        }
        if byte.is_ascii() {
            output.push(byte as char);
        } else {
            use std::fmt::Write;
            write!(output, "\\x{byte:02X}").expect("write to String");
        }
        index += 1;
    }
    output
}

fn minimal_repetitions(pattern: &str) -> String {
    let mut output = String::with_capacity(pattern.len());
    let bytes = pattern.as_bytes();
    let mut index = 0;
    let mut bracket = false;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte == b'\\' && index + 1 < bytes.len() {
            output.push('\\');
            output.push(bytes[index + 1] as char);
            index += 2;
            continue;
        }
        if byte == b'[' {
            bracket = true;
        }
        if byte == b']' {
            bracket = false;
        }
        output.push(byte as char);
        let bound = byte == b'}'
            && pattern[..index].rfind('{').is_some_and(|open| {
                pattern.as_bytes()[open + 1..index]
                    .iter()
                    .all(|part| part.is_ascii_digit() || *part == b',')
            });
        let optional = byte == b'?' && index > 0 && bytes[index - 1] != b'(';
        if !bracket && (matches!(byte, b'*' | b'+') || optional || bound) {
            if bytes.get(index + 1) == Some(&b'?') {
                index += 1;
            } else {
                output.push('?');
            }
        }
        index += 1;
    }
    output
}

fn compile(input: &[u8], cflags: i32) -> Result<(Engine, usize), i32> {
    if cflags & !COMPILE_FLAGS != 0 || cflags & flags::NOSPEC != 0 && cflags & flags::EXTENDED != 0
    {
        return Err(REG_INVARG);
    }
    if input.is_empty() {
        return Err(REG_EMPTY);
    }
    if cflags & flags::NOSPEC != 0 {
        return Ok((Engine::Literal(input.to_vec()), 0));
    }
    let empty_group = if cflags & flags::EXTENDED != 0 {
        input.windows(2).any(|bytes| bytes == b"()")
    } else {
        input.windows(4).any(|bytes| bytes == br"\(\)")
    };
    if cflags & flags::ENHANCED != 0 || empty_group {
        let pattern = fancy_pattern(
            input,
            cflags & flags::EXTENDED == 0,
            cflags & flags::ENHANCED != 0,
        );
        let pattern = if cflags & flags::MINIMAL != 0 && cflags & flags::EXTENDED != 0 {
            minimal_repetitions(&pattern)
        } else {
            pattern
        };
        let mut builder = RegexBuilder::new(&pattern);
        builder
            .bytes_mode(BytesMode::Ascii)
            .unicode_mode(false)
            .case_insensitive(cflags & flags::ICASE != 0)
            .dot_matches_new_line(cflags & flags::NEWLINE == 0)
            .multi_line(cflags & flags::NEWLINE != 0)
            .allow_input_assertion_overrides(true)
            .leftmost_longest(
                cflags & flags::MINIMAL == 0 && !pattern.contains("*?") && !pattern.contains("+?"),
            );
        let regex = builder.build().map_err(fancy_error)?;
        let groups = regex.captures_len() - 1;
        return Ok((Engine::Enhanced(regex), groups));
    }
    let regex = PosixRegexBuilder::new(input)
        .with_default_classes()
        .extended(cflags & flags::EXTENDED != 0)
        .compile()
        .map_err(|error| {
            if error == PosixError::EOF && !input.ends_with(b"\\") {
                REG_EBRACK
            } else {
                posix_error(error)
            }
        })?;
    let groups = regex.count_groups() - 1;
    Ok((Engine::Posix(regex), groups))
}

unsafe fn compile_into(preg: *mut GuestRegex, pattern: &[u8], cflags: i32) -> c_int {
    if preg.is_null() {
        return REG_INVARG;
    }
    let (engine, groups) = match std::panic::catch_unwind(|| compile(pattern, cflags)) {
        Ok(Ok(compiled)) => compiled,
        Ok(Err(error)) => {
            unsafe {
                (*preg).re_magic = 0;
                (*preg).re_g = std::ptr::null_mut();
            }
            return error;
        }
        Err(_) => return REG_ASSERT,
    };
    let pointer = Box::into_raw(Box::new(Compiled { engine, cflags }));
    unsafe {
        (*preg).re_magic = MAGIC;
        (*preg)._pad = 0;
        (*preg).re_nsub = groups;
        (*preg).re_endp = std::ptr::null();
        (*preg).re_g = pointer.cast();
    }
    0
}

/// # Safety
///
/// `preg` is writable Darwin regex storage and `pattern` is a C string.
pub unsafe extern "C" fn darwin_regcomp(
    preg: *mut GuestRegex,
    pattern: *const c_char,
    cflags: c_int,
) -> c_int {
    if preg.is_null() || pattern.is_null() {
        return REG_INVARG;
    }
    let bytes = if cflags & flags::PEND != 0 {
        let end = unsafe { (*preg).re_endp };
        let start = pattern.addr();
        let finish = end.addr();
        if end.is_null() || finish < start {
            return REG_INVARG;
        }
        unsafe { std::slice::from_raw_parts(pattern.cast::<u8>(), finish - start) }
    } else {
        unsafe { CStr::from_ptr(pattern).to_bytes() }
    };
    unsafe { compile_into(preg, bytes, cflags) }
}

/// # Safety
///
/// `pattern` is readable for `len` bytes and `preg` is writable regex storage.
pub unsafe extern "C" fn darwin_regncomp(
    preg: *mut GuestRegex,
    pattern: *const c_char,
    len: usize,
    cflags: c_int,
) -> c_int {
    if preg.is_null() || pattern.is_null() || cflags & flags::PEND != 0 {
        return REG_INVARG;
    }
    let bytes = unsafe { std::slice::from_raw_parts(pattern.cast::<u8>(), len) };
    unsafe { compile_into(preg, bytes, cflags) }
}

fn literal_match(pattern: &[u8], haystack: &[u8], icase: bool) -> Option<(usize, usize)> {
    haystack
        .windows(pattern.len())
        .position(|window| {
            window.iter().zip(pattern).all(|(&a, &b)| {
                if icase {
                    a.eq_ignore_ascii_case(&b)
                } else {
                    a == b
                }
            })
        })
        .map(|start| (start, start + pattern.len()))
}

fn execute(
    compiled: &Compiled,
    input: &[u8],
    eflags: i32,
) -> Result<Option<Vec<Option<(usize, usize)>>>, i32> {
    if eflags & !EXEC_FLAGS != 0 {
        return Err(REG_INVARG);
    }
    match &compiled.engine {
        Engine::Posix(base) => {
            let regex = base
                .clone()
                .case_insensitive(compiled.cflags & flags::ICASE != 0)
                .newline(compiled.cflags & flags::NEWLINE != 0)
                .no_start(eflags & flags::NOTBOL != 0)
                .no_end(eflags & flags::NOTEOL != 0);
            Ok(regex
                .matches(input, Some(1))
                .into_iter()
                .next()
                .map(|groups| groups.into_vec()))
        }
        Engine::Enhanced(regex) => {
            let search = RegexInput::new(input)
                .start_text(eflags & flags::NOTBOL == 0)
                .end_text(eflags & flags::NOTEOL == 0);
            let captures = regex.captures_input(search).map_err(|_| REG_ESPACE)?;
            Ok(captures.map(|captures| {
                (0..regex.captures_len())
                    .map(|index| {
                        captures
                            .get(index)
                            .map(|value| (value.start(), value.end()))
                    })
                    .collect()
            }))
        }
        Engine::Literal(pattern) => {
            Ok(
                literal_match(pattern, input, compiled.cflags & flags::ICASE != 0)
                    .map(|span| vec![Some(span)]),
            )
        }
    }
}

unsafe fn execute_from(
    preg: *const GuestRegex,
    string: *const c_char,
    len: Option<usize>,
    nmatch: usize,
    pmatch: *mut GuestMatch,
    eflags: c_int,
) -> c_int {
    if preg.is_null() || string.is_null() {
        return REG_INVARG;
    }
    let regex = unsafe { &*preg };
    if regex.re_magic != MAGIC || regex.re_g.is_null() {
        return REG_INVARG;
    }
    let compiled = unsafe { &*regex.re_g.cast::<Compiled>() };
    let (start, end) = if eflags & flags::STARTEND != 0 {
        if pmatch.is_null() {
            return REG_INVARG;
        }
        let bounds = unsafe { *pmatch };
        let (Ok(start), Ok(end)) = (usize::try_from(bounds.rm_so), usize::try_from(bounds.rm_eo))
        else {
            return REG_INVARG;
        };
        if start > end || len.is_some_and(|len| end > len) {
            return REG_INVARG;
        }
        (start, end)
    } else {
        (
            0,
            len.unwrap_or_else(|| unsafe { CStr::from_ptr(string).to_bytes().len() }),
        )
    };
    let input = unsafe { std::slice::from_raw_parts(string.cast::<u8>().add(start), end - start) };
    let groups = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        execute(compiled, input, eflags)
    })) {
        Ok(Ok(Some(groups))) => groups,
        Ok(Ok(None)) => return REG_NOMATCH,
        Ok(Err(error)) => return error,
        Err(_) => return REG_ASSERT,
    };
    if compiled.cflags & flags::NOSUB == 0 && !pmatch.is_null() {
        for index in 0..nmatch {
            let span = groups.get(index).copied().flatten();
            let output = match span {
                Some((so, eo)) => GuestMatch {
                    rm_so: i64::try_from(start + so).unwrap_or(-1),
                    rm_eo: i64::try_from(start + eo).unwrap_or(-1),
                },
                None => GuestMatch {
                    rm_so: -1,
                    rm_eo: -1,
                },
            };
            unsafe { *pmatch.add(index) = output };
        }
    }
    0
}

/// # Safety
///
/// The pointers follow Darwin's `regexec` contract.
pub unsafe extern "C" fn darwin_regexec(
    preg: *const GuestRegex,
    string: *const c_char,
    nmatch: usize,
    pmatch: *mut GuestMatch,
    eflags: c_int,
) -> c_int {
    unsafe { execute_from(preg, string, None, nmatch, pmatch, eflags) }
}

/// # Safety
///
/// `string` is readable for `len` bytes; the other pointers follow Darwin's `regnexec` contract.
pub unsafe extern "C" fn darwin_regnexec(
    preg: *const GuestRegex,
    string: *const c_char,
    len: usize,
    nmatch: usize,
    pmatch: *mut GuestMatch,
    eflags: c_int,
) -> c_int {
    unsafe { execute_from(preg, string, Some(len), nmatch, pmatch, eflags) }
}

/// # Safety
///
/// `preg` is null or contains storage compiled by this module.
pub unsafe extern "C" fn darwin_regfree(preg: *mut GuestRegex) {
    if preg.is_null() {
        return;
    }
    let regex = unsafe { &mut *preg };
    if regex.re_magic == MAGIC && !regex.re_g.is_null() {
        unsafe { drop(Box::from_raw(regex.re_g.cast::<Compiled>())) };
        regex.re_magic = 0;
        regex.re_nsub = 0;
        regex.re_g = std::ptr::null_mut();
    }
}

const ERRORS: &[(i32, &str, &str)] = &[
    (1, "REG_NOMATCH", "regexec() failed to match"),
    (2, "REG_BADPAT", "invalid regular expression"),
    (3, "REG_ECOLLATE", "invalid collating element"),
    (4, "REG_ECTYPE", "invalid character class"),
    (5, "REG_EESCAPE", "trailing backslash (\\)"),
    (6, "REG_ESUBREG", "invalid backreference number"),
    (7, "REG_EBRACK", "brackets ([ ]) not balanced"),
    (8, "REG_EPAREN", "parentheses not balanced"),
    (9, "REG_EBRACE", "braces not balanced"),
    (10, "REG_BADBR", "invalid repetition count(s)"),
    (11, "REG_ERANGE", "invalid character range"),
    (12, "REG_ESPACE", "out of memory"),
    (13, "REG_BADRPT", "repetition-operator operand invalid"),
    (14, "REG_EMPTY", "empty (sub)expression"),
    (15, "REG_ASSERT", "\"can't happen\" -- you found a bug"),
    (16, "REG_INVARG", "invalid argument to regex routine"),
    (17, "REG_ILLSEQ", "illegal byte sequence"),
    (18, "REG_BADMAX", "maximum repetition exceeds 255"),
];

/// # Safety
///
/// `errbuf` is null or writable for `size` bytes. `preg` is required for `REG_ATOI`.
pub unsafe extern "C" fn darwin_regerror(
    code: c_int,
    preg: *const GuestRegex,
    errbuf: *mut c_char,
    size: usize,
) -> usize {
    let message = if code == flags::ATOI {
        let name = if preg.is_null() || unsafe { (*preg).re_endp.is_null() } {
            None
        } else {
            Some(unsafe { CStr::from_ptr((*preg).re_endp) }.to_bytes())
        };
        ERRORS
            .iter()
            .find(|(_, symbol, _)| name == Some(symbol.as_bytes()))
            .map_or_else(|| "0".to_owned(), |(number, _, _)| number.to_string())
    } else if code & flags::ITOA != 0 {
        if code == flags::ITOA {
            "REG_0x0".to_owned()
        } else {
            ERRORS
                .iter()
                .find(|(number, _, _)| *number == code & !flags::ITOA)
                .map_or("*** unknown regexp error code ***", |(_, symbol, _)| {
                    *symbol
                })
                .to_owned()
        }
    } else {
        ERRORS
            .iter()
            .find(|(number, _, _)| *number == code)
            .map_or(
                "*** unknown regexp error code ***",
                |(_, _, description)| *description,
            )
            .to_owned()
    };
    let needed = message.len() + 1;
    if !errbuf.is_null() && size != 0 {
        let copied = message.len().min(size - 1);
        unsafe {
            std::ptr::copy_nonoverlapping(message.as_ptr().cast::<c_char>(), errbuf, copied);
            *errbuf.add(copied) = 0;
        }
    }
    needed
}

exports::exports! {
    functions {
        "regcomp" => darwin_regcomp,
        "regncomp" => darwin_regncomp,
        "regexec" => darwin_regexec,
        "regnexec" => darwin_regnexec,
        "regfree" => darwin_regfree,
        "regerror" => darwin_regerror,
    }
}
