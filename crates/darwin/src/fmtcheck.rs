//! `fmtcheck`, which glibc never had. ncurses calls it on every format string
//! it reads out of a terminal description.
//!
//! Ported from Apple's `gen/FreeBSD/fmtcheck.c`: the two formats' conversions
//! are compared in order, and the first is returned only if every one of its
//! arguments has the type the second's would.

#![cfg(not(target_os = "macos"))]

use std::ffi::{CStr, c_char};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Start,
    Int,
    WintT,
    Long,
    Quad,
    IntmaxT,
    PtrdiffT,
    SizeT,
    CharPointer,
    ShortPointer,
    IntPointer,
    LongPointer,
    QuadPointer,
    IntmaxTPointer,
    PtrdiffTPointer,
    SizeTPointer,
    Double,
    LongDouble,
    String,
    WString,
    Width,
    Precision,
    Done,
    Unknown,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Modifier {
    None,
    Char,
    Short,
    Long,
    Quad,
    IntmaxT,
    LongDouble,
    PtrdiffT,
    SizeT,
}

/// A format and a cursor into it; reading past the end yields NUL, as C does.
struct Cursor<'a> {
    f: &'a [u8],
    at: usize,
}

impl Cursor<'_> {
    fn peek(&self) -> u8 {
        self.f.get(self.at).copied().unwrap_or(0)
    }

    /// The conversion after any length modifier. Leaves the cursor on the
    /// conversion character, as Apple's does.
    fn after_precision(&mut self) -> Kind {
        let modifier = match self.peek() {
            b'h' => {
                self.at += 1;
                if self.peek() == 0 {
                    return Kind::Unknown;
                }
                if self.peek() == b'h' {
                    self.at += 1;
                    Modifier::Char
                } else {
                    Modifier::Short
                }
            }
            b'j' => {
                self.at += 1;
                Modifier::IntmaxT
            }
            b'l' => {
                self.at += 1;
                if self.peek() == 0 {
                    return Kind::Unknown;
                }
                if self.peek() == b'l' {
                    self.at += 1;
                    Modifier::Quad
                } else {
                    Modifier::Long
                }
            }
            b'q' => {
                self.at += 1;
                Modifier::Quad
            }
            b't' => {
                self.at += 1;
                Modifier::PtrdiffT
            }
            b'z' => {
                self.at += 1;
                Modifier::SizeT
            }
            b'L' => {
                self.at += 1;
                Modifier::LongDouble
            }
            _ => Modifier::None,
        };
        let c = self.peek();
        if c == 0 {
            return Kind::Unknown;
        }
        use Modifier as M;
        match c {
            b'd' | b'i' | b'o' | b'u' | b'x' | b'X' => match modifier {
                M::Long => Kind::Long,
                M::Quad => Kind::Quad,
                M::IntmaxT => Kind::IntmaxT,
                M::PtrdiffT => Kind::PtrdiffT,
                M::SizeT => Kind::SizeT,
                M::Char | M::Short | M::None => Kind::Int,
                M::LongDouble => Kind::Unknown,
            },
            b'n' => match modifier {
                M::Char => Kind::CharPointer,
                M::Short => Kind::ShortPointer,
                M::Long => Kind::LongPointer,
                M::Quad => Kind::QuadPointer,
                M::IntmaxT => Kind::IntmaxTPointer,
                M::PtrdiffT => Kind::PtrdiffTPointer,
                M::SizeT => Kind::SizeTPointer,
                M::None => Kind::IntPointer,
                M::LongDouble => Kind::Unknown,
            },
            b'D' | b'O' | b'U' => match modifier {
                M::None => Kind::Long,
                _ => Kind::Unknown,
            },
            b'a' | b'A' | b'e' | b'E' | b'f' | b'F' | b'g' | b'G' => match modifier {
                M::LongDouble => Kind::LongDouble,
                M::Long | M::None => Kind::Double,
                _ => Kind::Unknown,
            },
            b'c' => match modifier {
                M::Long => Kind::WintT,
                M::None => Kind::Int,
                _ => Kind::Unknown,
            },
            b'C' => match modifier {
                M::None => Kind::WintT,
                _ => Kind::Unknown,
            },
            b's' => match modifier {
                M::Long => Kind::WString,
                M::None => Kind::String,
                _ => Kind::Unknown,
            },
            b'S' => match modifier {
                M::None => Kind::WString,
                _ => Kind::Unknown,
            },
            b'p' => match modifier {
                M::None => Kind::Long,
                _ => Kind::Unknown,
            },
            _ => Kind::Unknown,
        }
    }

    fn after_width(&mut self) -> Kind {
        if self.peek() == b'.' {
            self.at += 1;
            if self.peek() == b'*' {
                return Kind::Precision;
            }
            while self.peek().is_ascii_digit() {
                self.at += 1;
            }
            if self.peek() == 0 {
                return Kind::Unknown;
            }
        }
        self.after_precision()
    }

    fn next(&mut self, previous: Kind) -> Kind {
        match previous {
            Kind::Width => {
                self.at += 1;
                return self.after_width();
            }
            Kind::Precision => {
                self.at += 1;
                return self.after_precision();
            }
            _ => {}
        }
        loop {
            let Some(pct) = self.f[self.at.min(self.f.len())..]
                .iter()
                .position(|&b| b == b'%')
            else {
                self.at = self.f.len();
                return Kind::Done;
            };
            self.at += pct + 1;
            match self.peek() {
                0 => return Kind::Unknown,
                b'%' => self.at += 1,
                _ => break,
            }
        }
        while matches!(self.peek(), b'#' | b'\'' | b'0' | b'-' | b' ' | b'+') {
            self.at += 1;
        }
        if self.peek() == b'*' {
            return Kind::Width;
        }
        while self.peek().is_ascii_digit() {
            self.at += 1;
        }
        if self.peek() == 0 {
            return Kind::Unknown;
        }
        self.after_width()
    }
}

/// Whether `f1` may be used where `f2` is expected.
fn compatible(f1: &[u8], f2: &[u8]) -> bool {
    let mut a = Cursor { f: f1, at: 0 };
    let mut b = Cursor { f: f2, at: 0 };
    let mut at = Kind::Start;
    let mut bt = Kind::Start;
    loop {
        at = a.next(at);
        if at == Kind::Done {
            return true;
        }
        if at == Kind::Unknown {
            return false;
        }
        bt = b.next(bt);
        if at != bt {
            return false;
        }
    }
}

/// `const char *fmtcheck(const char *f1, const char *f2)`
///
/// # Safety
///
/// `f2` is NUL-terminated, and `f1` is null or NUL-terminated.
pub unsafe extern "C" fn fmtcheck(f1: *const c_char, f2: *const c_char) -> *const c_char {
    if f1.is_null() {
        return f2;
    }
    // SAFETY: the caller's obligation.
    let (a, b) = unsafe { (CStr::from_ptr(f1).to_bytes(), CStr::from_ptr(f2).to_bytes()) };
    match compatible(a, b) {
        true => f1,
        false => f2,
    }
}

exports::exports! {
    functions {
        "fmtcheck" | "__fmtcheck" => fmtcheck,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matching_conversions_keep_the_first_format() {
        assert!(compatible(b"%d items", b"count: %i"));
        assert!(compatible(b"%5.2f%%", b"%f"));
        assert!(compatible(b"%*d", b"%*x"));
        assert!(compatible(b"no conversions", b"%d"));
    }

    #[test]
    fn a_mismatch_or_an_unknown_conversion_falls_back() {
        assert!(!compatible(b"%s", b"%d"));
        assert!(!compatible(b"%ld", b"%d"));
        assert!(!compatible(b"%d %d", b"%d"));
        assert!(!compatible(b"%y", b"%y"));
        assert!(!compatible(b"%", b"%d"));
    }

    #[test]
    fn fmtcheck_returns_one_of_its_arguments() {
        let good = c"%d";
        let bad = c"%s";
        let template = c"%i";
        // SAFETY: three literal strings.
        unsafe {
            assert_eq!(fmtcheck(good.as_ptr(), template.as_ptr()), good.as_ptr());
            assert_eq!(fmtcheck(bad.as_ptr(), template.as_ptr()), template.as_ptr());
        }
    }
}
