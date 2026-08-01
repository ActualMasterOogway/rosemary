//! Objective-C type encodings, read as signatures.
//!
//! Every other import in this project needs a signature the embedder wrote down
//! by hand ([`crate::abi::table`]), because a Mach-O records names and not
//! types. An Objective-C method is the one call whose signature the *image*
//! answers: `method_t::types` is `i20@0:8i16` for `- (int)twice:(int)x` — the
//! return type, then `self`, the selector, and each argument, each followed by
//! the frame offset a long-dead calling convention would have put it at.
//!
//! That is what makes `objc_msgSend` marshallable at all. It has no signature of
//! its own — it has *whichever* signature the selector's method declares — so
//! the alternative to reading the encoding is guessing, which is the one thing
//! this project refuses everywhere else. For a *host* method the same string
//! comes back from `method_getTypeEncoding`, so both directions are described by
//! the same source.
//!
//! ## What is refused, and why each refusal is not laziness
//!
//! - **Aggregates** (`{CGRect=...}`, `[4i]`, `(u=...)`). Passing or returning a
//!   struct by value is decided by rules — splitting across registers, the
//!   indirect-result register, memory — that [`crate::abi::marshal`]'s scalar
//!   prototype cannot reproduce in either ABI, let alone across two. This is
//!   M2's "aggregates by value" refusal coming due exactly where the roadmap
//!   said it would.
//! - **Blocks** (`@?`). A block is an object whose `invoke` field is a guest
//!   function pointer the host would call as arm64. Substituting it means
//!   rewriting a structure the guest owns, which is a different piece of work
//!   from marshalling a call.
//! - **`long double`** (`D`). x86-64 returns it in `st0`, which has no arm64
//!   counterpart and no register in the guest context to read it from.
//! - **Bitfields** (`b3`) and the unknown type (`?`), which are not describable
//!   rather than not implemented.
//!
//! Each is a `&'static str` reason that reaches the guest as a named stop, in
//! the register the rest of the project uses.
//!
//! ## `l` is not `long`
//!
//! On LP64 clang encodes `long` as `q`, not `l` — `l` is the 32-bit spelling
//! that survives from 32-bit Darwin. Reading `l` as 64 bits would sign-extend
//! from the wrong bit for a negative value and produce a plausible wrong number,
//! so it is read as the `int32_t` it is. `a_long_is_encoded_q_on_lp64` checks
//! that against the compiler rather than against the documentation.

use crate::abi::ArgType as T;

/// A method signature, as [`crate::abi::marshal::call_types`] wants it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signature {
    /// `[0]` is the return type, `[1..]` the parameters — `self` and the
    /// selector included, because those are the first two arguments a method
    /// really takes.
    pub types: Vec<T>,
    /// Zero-based parameter indices encoded `#`, i.e. holding a `Class`.
    ///
    /// The one kind of pointer that means something different on each side of
    /// the seam; see [`crate::objc`].
    pub classes: Vec<u8>,
    /// Whether the method returns a `Class`.
    pub class_return: bool,
}

impl Signature {
    /// The number of arguments, `self` and `_cmd` included.
    pub fn arity(&self) -> usize {
        self.types.len().saturating_sub(1)
    }
}

/// Decode a method type encoding.
///
/// `Err` is a reason in the shape a refusal wants: a `&'static str` naming what
/// about *this* method cannot be called, so the diagnostic says "returns a
/// struct by value" rather than "unsupported".
pub fn signature(encoding: &str) -> Result<Signature, &'static str> {
    let bytes = encoding.as_bytes();
    let mut at = 0usize;
    let mut sig = Signature { types: Vec::new(), classes: Vec::new(), class_return: false };

    while at < bytes.len() {
        let start = at;
        let ty = one(bytes, &mut at)?;
        if at == start {
            // No progress: a byte the scanner does not recognise. Bailing out is
            // the difference between a refusal and a hang.
            return Err("the type encoding contains a byte this decoder cannot advance past");
        }
        // The frame offset the encoding records after each type. A relic of a
        // calling convention nobody uses, and skipped rather than read: where an
        // argument really lives is `place_args`'s answer, not this number's.
        while bytes.get(at).is_some_and(u8::is_ascii_digit) {
            at += 1;
        }
        if bytes[start] == b'#' {
            match sig.types.is_empty() {
                true => sig.class_return = true,
                false => sig.classes.push((sig.types.len() - 1) as u8),
            }
        }
        sig.types.push(ty);
    }

    if sig.types.is_empty() {
        return Err("the type encoding is empty");
    }
    Ok(sig)
}

/// One type, advancing `at` past it.
fn one(b: &[u8], at: &mut usize) -> Result<T, &'static str> {
    // Qualifiers: `const`, `in`, `inout`, `out`, `bycopy`, `byref`, `oneway`.
    // They describe how the argument is used, never how wide it is, so they are
    // skipped rather than recorded.
    while matches!(b.get(*at), Some(b'r' | b'n' | b'N' | b'o' | b'O' | b'R' | b'V')) {
        // `r` is also nothing on its own: a bare `r` at the end of a string is
        // malformed, and stopping here keeps the caller's no-progress check able
        // to see it.
        if *at + 1 >= b.len() {
            break;
        }
        *at += 1;
    }
    let Some(&c) = b.get(*at) else { return Err("the type encoding ends mid-type") };
    *at += 1;
    Ok(match c {
        b'v' => T::None,
        b'c' => T::SignedByte,
        b'C' | b'B' => T::UnsignedByte,
        b's' => T::SignedHalfword,
        b'S' => T::UnsignedHalfword,
        // See the module note: `l` is the 32-bit spelling, and LP64 `long` is
        // encoded `q`.
        b'i' | b'l' => T::SignedWord,
        b'I' | b'L' => T::UnsignedWord,
        b'q' => T::SignedDoubleword,
        b'Q' => T::UnsignedDoubleword,
        b'f' => T::SinglePrecisionFp,
        b'd' => T::DoublePrecisionFp,
        b'*' | b'#' | b':' => T::Pointer,
        b'@' => {
            // `@?` is a block, and `@"NSString"` is an object whose class the
            // compiler recorded. The second is an ordinary `id`; the first is
            // not.
            match b.get(*at) {
                Some(b'?') => return Err("a block argument carries a guest function pointer"),
                Some(b'"') => skip_quoted(b, at),
                _ => {}
            }
            T::Pointer
        }
        b'^' => {
            // A pointer to anything is a pointer. The pointee is skipped rather
            // than decoded, because nothing about it reaches a register.
            skip_one(b, at)?;
            T::Pointer
        }
        b'D' => return Err("a long double lives in st0, which has no arm64 counterpart"),
        b'{' | b'(' | b'[' => return Err("passes or returns an aggregate by value"),
        b'b' => return Err("a bitfield argument is not describable as a register"),
        b'?' => return Err("the type encoding says the type is unknown"),
        _ => return Err("an unrecognised type encoding"),
    })
}

/// Advance past one whole type without classifying it.
///
/// Only reached for a pointee, where the *shape* matters and the type does not:
/// `^{CGRect=dddd}` is a pointer, and getting there means matching the brace.
fn skip_one(b: &[u8], at: &mut usize) -> Result<(), &'static str> {
    let Some(&c) = b.get(*at) else { return Ok(()) };
    match c {
        b'{' | b'(' | b'[' => skip_group(b, at),
        b'^' => {
            *at += 1;
            skip_one(b, at)
        }
        b'"' => {
            skip_quoted(b, at);
            Ok(())
        }
        _ => {
            *at += 1;
            if c == b'@' && b.get(*at) == Some(&b'"') {
                skip_quoted(b, at);
            }
            Ok(())
        }
    }
}

/// Advance past a balanced `{...}`, `(...)` or `[...]`, quoted names included.
///
/// Depth-counted rather than scanning for the closing byte: a struct's fields
/// are themselves encodings and nest arbitrarily, and a scanner that stopped at
/// the first `}` would leave the cursor inside `{A={B=i}i}` and read the rest of
/// the struct as arguments.
fn skip_group(b: &[u8], at: &mut usize) -> Result<(), &'static str> {
    let mut depth = 0usize;
    while let Some(&c) = b.get(*at) {
        match c {
            b'{' | b'(' | b'[' => depth += 1,
            b'}' | b')' | b']' => {
                depth -= 1;
                if depth == 0 {
                    *at += 1;
                    return Ok(());
                }
            }
            // A field name, which may contain any of the above.
            b'"' => {
                skip_quoted(b, at);
                continue;
            }
            _ => {}
        }
        *at += 1;
    }
    Err("an unterminated aggregate in the type encoding")
}

/// Advance past a `"quoted name"`, or to the end if it is unterminated.
fn skip_quoted(b: &[u8], at: &mut usize) {
    *at += 1;
    while let Some(&c) = b.get(*at) {
        *at += 1;
        if c == b'"' {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn types(encoding: &str) -> Vec<T> {
        signature(encoding).expect(encoding).types
    }

    #[test]
    fn a_method_encoding_is_a_return_type_then_self_and_the_selector() {
        // `- (int)twice:(int)x`, verbatim from a compiled fixture.
        let sig = signature("i20@0:8i16").expect("twice:");
        assert_eq!(sig.types, vec![T::SignedWord, T::Pointer, T::Pointer, T::SignedWord]);
        assert_eq!(sig.arity(), 3, "self and _cmd are arguments");
        assert!(sig.classes.is_empty());
    }

    #[test]
    fn the_frame_offsets_are_skipped_rather_than_believed() {
        // The offsets describe a calling convention from the 32-bit era. Where an
        // argument really is comes from `place_args`; a decoder that used these
        // would read the second argument out of the first's register.
        assert_eq!(types("v24@0:8@16"), types("v@:@"));
    }

    #[test]
    fn a_class_argument_is_marked_by_index_and_the_return_separately() {
        // `+ (Class)classForThing:(Class)c` — self is parameter 0, so the
        // explicit argument is 2.
        let sig = signature("#@:#").expect("class");
        assert!(sig.class_return);
        assert_eq!(sig.classes, vec![2]);
        // A class *receiver* is `@` in the encoding, not `#`: the runtime types
        // `self` as `id` even in a class method, which is why the substitution
        // of a receiver is the message shim's job and not this decoder's.
        assert!(signature("v@:").expect("plain").classes.is_empty());
    }

    #[test]
    fn qualifiers_and_object_class_names_do_not_shift_the_arguments() {
        assert_eq!(types("v@:r*"), vec![T::None, T::Pointer, T::Pointer, T::Pointer]);
        assert_eq!(
            types("@\"NSString\"16@0:8"),
            vec![T::Pointer, T::Pointer, T::Pointer],
            "a recorded class name is part of the `@`, not an argument"
        );
    }

    #[test]
    fn a_pointer_to_an_aggregate_is_a_pointer_and_a_nested_one_is_skipped_whole() {
        assert_eq!(types("v@:^{CGRect={CGPoint=dd}{CGSize=dd}}"), vec![
            T::None,
            T::Pointer,
            T::Pointer,
            T::Pointer
        ]);
        assert_eq!(types("v@:^^i"), vec![T::None, T::Pointer, T::Pointer, T::Pointer]);
        // The quoted field names structs carry contain no braces here, but they
        // may, and the scanner has to survive one that does.
        assert_eq!(types("v@:^{S=\"}\"i}"), vec![T::None, T::Pointer, T::Pointer, T::Pointer]);
    }

    #[test]
    fn floating_point_widths_are_distinguished() {
        assert_eq!(types("d@:f"), vec![
            T::DoublePrecisionFp,
            T::Pointer,
            T::Pointer,
            T::SinglePrecisionFp
        ]);
    }

    #[test]
    fn every_refusal_names_what_it_is_refusing() {
        for (encoding, needle) in [
            ("{CGRect=dddd}@:", "aggregate"),
            ("v@:{CGSize=dd}", "aggregate"),
            ("v@:[4i]", "aggregate"),
            ("v@:@?", "block"),
            ("D@:", "long double"),
            ("v@:b3", "bitfield"),
            ("?@:", "unknown"),
            ("", "empty"),
            ("v@:z", "unrecognised"),
            ("v@:^{unterminated=i", "unterminated"),
        ] {
            let err = signature(encoding).expect_err(encoding);
            assert!(err.contains(needle), "{encoding}: {err}");
        }
    }

    #[test]
    fn a_malformed_encoding_never_hangs_or_panics() {
        // Totality, as everywhere else: these strings come out of guest memory.
        for encoding in ["^", "r", "@\"", "{", "}", "^^^^^^", "@?", ":::", "\u{1f600}"] {
            let _ = signature(encoding);
        }
    }
}
