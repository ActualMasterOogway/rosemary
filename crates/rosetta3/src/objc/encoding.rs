//! Objective-C type encodings, read as signatures.

use crate::abi::ArgType as T;
use crate::abi::aggregate::{Aggregate, Leaf, Shape, width_of};

/// A method signature, as [`crate::abi::marshal::call_shapes`] wants it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signature {
    /// `[0]` is the return type, `[1..]` the parameters — `self` and the selector
    /// included, because those are the first two arguments a method really takes.
    pub shapes: Vec<Shape>,
    /// Zero-based parameter indices encoded `#`, i.e. holding a `Class`.
    pub classes: Vec<u8>,
    /// Zero-based parameter indices encoded `@?`, i.e. holding a block.
    pub blocks: Vec<u8>,
    /// Zero-based parameter indices encoded `@`, i.e.
    pub objects: Vec<u8>,
    /// Whether the method returns a `Class`.
    pub class_return: bool,
    /// Whether the method returns an `id` — the reverse of [`Signature::objects`],
    /// for a class handed back where the encoding says only "object".
    pub object_return: bool,
}

impl Signature {
    /// The number of arguments, `self` and `_cmd` included.
    pub fn arity(&self) -> usize {
        self.shapes.len().saturating_sub(1)
    }

    /// The scalar types, for a signature that has only scalars.
    pub fn types(&self) -> Option<Vec<T>> {
        self.shapes.iter().map(Shape::scalar).collect()
    }
}

/// Decode a method type encoding.
pub fn signature(encoding: &str) -> Result<Signature, &'static str> {
    let bytes = encoding.as_bytes();
    let mut at = 0usize;
    let mut sig = Signature {
        shapes: Vec::new(),
        classes: Vec::new(),
        blocks: Vec::new(),
        objects: Vec::new(),
        class_return: false,
        object_return: false,
    };

    while at < bytes.len() {
        let start = at;
        skip_qualifiers(bytes, &mut at);
        // Where the type itself begins, qualifiers behind us.
        let ty = at;
        // An aggregate is recognised *here* rather than inside the scalar
        // decoder, because it is the one type whose whole layout matters and
        // not merely its width. See [`crate::abi::aggregate`].
        let shape = match bytes.get(at) {
            Some(b'{' | b'[' | b'(') => Shape::Struct(aggregate(bytes, &mut at)?),
            _ => Shape::Scalar(scalar(bytes, &mut at)?),
        };
        if at == start {
            // No progress: a byte the scanner does not recognise. Bailing out is
            // the difference between a refusal and a hang.
            return Err("the type encoding contains a byte this decoder cannot advance past");
        }
        // The frame offset the encoding records after each type.
        while bytes.get(at).is_some_and(u8::is_ascii_digit) {
            at += 1;
        }
        if bytes[ty] == b'#' {
            match sig.shapes.is_empty() {
                true => sig.class_return = true,
                false => sig.classes.push((sig.shapes.len() - 1) as u8),
            }
        }
        // `@?` is a block; `@` alone, or `@"NSString"`, is an ordinary object.
        if bytes[ty] == b'@' {
            let block = bytes.get(ty + 1) == Some(&b'?');
            match (block, sig.shapes.is_empty()) {
                (true, false) => sig.blocks.push((sig.shapes.len() - 1) as u8),
                (true, true) => {}
                (false, true) => sig.object_return = true,
                (false, false) => sig.objects.push((sig.shapes.len() - 1) as u8),
            }
        }
        sig.shapes.push(shape);
    }

    if sig.shapes.is_empty() {
        return Err("the type encoding is empty");
    }
    Ok(sig)
}

/// Skip `const`, `in`, `inout`, `out`, `bycopy`, `byref`, `oneway`.
///
/// They describe how an argument is used, never how wide it is, so they are skipped
/// rather than recorded.
fn skip_qualifiers(b: &[u8], at: &mut usize) {
    while matches!(
        b.get(*at),
        Some(b'r' | b'n' | b'N' | b'o' | b'O' | b'R' | b'V')
    ) {
        if *at + 1 >= b.len() {
            break;
        }
        *at += 1;
    }
}

/// One *scalar* type, advancing `at` past it.
pub fn scalar(b: &[u8], at: &mut usize) -> Result<T, &'static str> {
    skip_qualifiers(b, at);
    let Some(&c) = b.get(*at) else {
        return Err("the type encoding ends mid-type");
    };
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
            // compiler recorded.
            match b.get(*at) {
                Some(b'?') => *at += 1,
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

/// Decode an aggregate encoding — `{Name=fields}` or `[N type]` — starting at `b[*at]`,
/// which must be the opening delimiter.
///
/// Offsets come out **flattened and absolute**, because that is what
/// [`Aggregate::guest_class`] needs and because nesting is a property of the source
/// language rather than of the registers.
pub fn aggregate(b: &[u8], at: &mut usize) -> Result<Aggregate, &'static str> {
    let agg = group(b, at, 0)?;
    if agg.leaves.is_empty() {
        return Err("an aggregate with no members is not describable as registers");
    }
    Ok(agg)
}

/// The most leaves any aggregate this runtime will describe may have.
///
/// Totality.
const MAX_LEAVES: usize = 64;

/// How deep a nested aggregate may go before this decoder gives up.
const MAX_DEPTH: usize = 16;

/// One `{...}` or `[...]`, with offsets relative to its own start.
fn group(b: &[u8], at: &mut usize, depth: usize) -> Result<Aggregate, &'static str> {
    if depth > MAX_DEPTH {
        return Err("an aggregate in the type encoding nests deeper than this decoder follows");
    }
    let open = *b
        .get(*at)
        .ok_or("an aggregate encoding ends where it opens")?;
    let close = match open {
        b'{' => b'}',
        b'[' => b']',
        b'(' => return Err("a union's members overlap, so its eightbytes are undecidable"),
        _ => return Err("an aggregate encoding does not begin with a delimiter"),
    };
    *at += 1;

    let mut agg = Aggregate {
        size: 0,
        align: 1,
        leaves: Vec::new(),
    };

    if open == b'[' {
        let mut count = 0usize;
        while b.get(*at).is_some_and(u8::is_ascii_digit) {
            count = count.saturating_mul(10) + usize::from(b[*at] - b'0');
            if count > MAX_LEAVES {
                return Err("an array in an aggregate has more elements than this decoder places");
            }
            *at += 1;
        }
        if count == 0 {
            return Err("an array in an aggregate has an unusable element count");
        }
        // Decoded once and repeated: a nested array must not multiply the parse
        // cost by its own count.
        let element_at = *at;
        let mut cursor = element_at;
        let element = one_member(b, &mut cursor, depth)?;
        for _ in 0..count {
            place(&mut agg, &element)?;
        }
        *at = cursor;
        expect(b, at, close)?;
        agg.size = agg.size.next_multiple_of(agg.align);
        return Ok(agg);
    }

    // `{Name=` — the name runs to the first `=`, and may be absent (`{?=...}`).
    loop {
        let Some(&c) = b.get(*at) else {
            return Err("an unterminated aggregate in the type encoding");
        };
        *at += 1;
        if c == b'=' {
            break;
        }
        if c == close {
            return Err("an aggregate with no members is not describable as registers");
        }
    }

    while let Some(&c) = b.get(*at) {
        if c == close {
            *at += 1;
            agg.size = agg.size.next_multiple_of(agg.align);
            return Ok(agg);
        }
        // A field name, which clang emits for a struct's members.
        if c == b'"' {
            skip_quoted(b, at);
            continue;
        }
        let member = one_member(b, at, depth)?;
        place(&mut agg, &member)?;
    }
    Err("an unterminated aggregate in the type encoding")
}

/// One member of a struct or one element of an array, as an aggregate of its
/// own so that scalars and nested groups compose the same way.
fn one_member(b: &[u8], at: &mut usize, depth: usize) -> Result<Aggregate, &'static str> {
    match b.get(*at) {
        Some(b'{' | b'[' | b'(') => group(b, at, depth + 1),
        _ => {
            let ty = scalar(b, at)?;
            let width = width_of(ty);
            Ok(Aggregate {
                size: width,
                align: width,
                leaves: vec![Leaf { ty, offset: 0 }],
            })
        }
    }
}

/// Append `member` to `agg` at the next offset its alignment allows.
fn place(agg: &mut Aggregate, member: &Aggregate) -> Result<(), &'static str> {
    if agg.leaves.len() + member.leaves.len() > MAX_LEAVES {
        return Err("an aggregate has more members than this runtime will describe");
    }
    let at = agg.size.next_multiple_of(member.align);
    agg.leaves.extend(member.leaves.iter().map(|l| Leaf {
        ty: l.ty,
        offset: at + l.offset,
    }));
    agg.align = agg.align.max(member.align);
    agg.size = at + member.size;
    Ok(())
}

fn expect(b: &[u8], at: &mut usize, c: u8) -> Result<(), &'static str> {
    match b.get(*at) {
        Some(&got) if got == c => {
            *at += 1;
            Ok(())
        }
        _ => Err("an unterminated aggregate in the type encoding"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn types(encoding: &str) -> Vec<T> {
        signature(encoding)
            .expect(encoding)
            .types()
            .unwrap_or_else(|| panic!("{encoding} names an aggregate"))
    }

    #[test]
    fn a_method_encoding_is_a_return_type_then_self_and_the_selector() {
        // `- (int)twice:(int)x`, verbatim from a compiled fixture.
        let sig = signature("i20@0:8i16").expect("twice:");
        assert_eq!(
            sig.types().expect("all scalars"),
            vec![T::SignedWord, T::Pointer, T::Pointer, T::SignedWord]
        );
        assert_eq!(sig.arity(), 3, "self and _cmd are arguments");
        assert!(sig.classes.is_empty());
    }

    #[test]
    fn the_frame_offsets_are_skipped_rather_than_believed() {
        // The offsets describe a calling convention from the 32-bit era.
        assert_eq!(types("v24@0:8@16"), types("v@:@"));
    }

    #[test]
    fn a_class_argument_is_marked_by_index_and_the_return_separately() {
        // `+ (Class)classForThing:(Class)c` — self is parameter 0, so the
        // explicit argument is 2.
        let sig = signature("#@:#").expect("class");
        assert!(sig.class_return);
        assert_eq!(sig.classes, vec![2]);
        // A class *receiver* is `@` in the encoding, not `#`.
        assert!(signature("v@:").expect("plain").classes.is_empty());
    }

    #[test]
    fn qualifiers_and_object_class_names_do_not_shift_the_arguments() {
        assert_eq!(
            types("v@:r*"),
            vec![T::None, T::Pointer, T::Pointer, T::Pointer]
        );
        assert_eq!(
            types("@\"NSString\"16@0:8"),
            vec![T::Pointer, T::Pointer, T::Pointer],
            "a recorded class name is part of the `@`, not an argument"
        );
    }

    #[test]
    fn a_pointer_to_an_aggregate_is_a_pointer_and_a_nested_one_is_skipped_whole() {
        assert_eq!(
            types("v@:^{CGRect={CGPoint=dd}{CGSize=dd}}"),
            vec![T::None, T::Pointer, T::Pointer, T::Pointer]
        );
        assert_eq!(
            types("v@:^^i"),
            vec![T::None, T::Pointer, T::Pointer, T::Pointer]
        );
        // The quoted field names structs carry contain no braces here, but they
        // may, and the scanner has to survive one that does.
        assert_eq!(
            types("v@:^{S=\"}\"i}"),
            vec![T::None, T::Pointer, T::Pointer, T::Pointer]
        );
    }

    #[test]
    fn floating_point_widths_are_distinguished() {
        assert_eq!(
            types("d@:f"),
            vec![
                T::DoublePrecisionFp,
                T::Pointer,
                T::Pointer,
                T::SinglePrecisionFp
            ]
        );
    }

    #[test]
    fn an_aggregate_argument_is_decoded_rather_than_refused() {
        // `- (id)initWithFrame:(NSRect)frame`, verbatim. The refusal this
        // replaces is the one that stopped every Cocoa view from being built.
        let sig = signature("@24@0:8{CGRect={CGPoint=dd}{CGSize=dd}}16").expect("initWithFrame:");
        assert_eq!(sig.arity(), 3);
        assert!(
            sig.types().is_none(),
            "an aggregate has no flat scalar view"
        );
        let Shape::Struct(rect) = &sig.shapes[3] else {
            panic!("the frame is a struct")
        };
        assert_eq!(rect.size, 32);
        assert_eq!(sig.shapes[0], Shape::Scalar(T::Pointer));

        // And a struct return, which is the `objc_msgSend_stret` case.
        let frame = signature("{CGRect={CGPoint=dd}{CGSize=dd}}16@0:8").expect("frame");
        assert!(matches!(frame.shapes[0], Shape::Struct(_)));
    }

    #[test]
    fn every_refusal_names_what_it_is_refusing() {
        for (encoding, needle) in [
            ("v@:(u=id)", "union"),
            ("v@:{S=}", "no members"),
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
    fn a_block_is_a_pointer_that_says_it_is_a_block() {
        // `- (void)run:(void (^)(void))block` — return, `self`, `_cmd`, then
        // the block at parameter two.
        let sig = signature("v@:@?").expect("a block parameter decodes");
        assert_eq!(sig.blocks, vec![2]);
        assert_eq!(
            sig.types(),
            Some(vec![T::None, T::Pointer, T::Pointer, T::Pointer])
        );

        // `@` and `@"NSString"` are objects and must not be mistaken for one,
        // which is the failure that would substitute a thunk into a string.
        let plain = signature("v@:@\"NSString\"@").expect("objects decode");
        assert!(plain.blocks.is_empty());

        // Two blocks, and an object between them, so the index is a position
        // rather than a count.
        let two = signature("v@:@?@@?").expect("two blocks decode");
        assert_eq!(two.blocks, vec![2, 4]);
    }

    #[test]
    fn a_malformed_encoding_never_hangs_or_panics() {
        // Totality, as everywhere else: these strings come out of guest memory.
        for encoding in [
            "^",
            "r",
            "@\"",
            "{",
            "}",
            "^^^^^^",
            "@?",
            ":::",
            "\u{1f600}",
            "{S=[9999999999i]}",
            "v@:{{{{{{{{{{{{{{{{{{{{i}}}}}}}}}}}}}}}}}}}}",
            "v@:[4[4[4[4[4[4[4[4[4[4[4[4[4[4[4[4[4[4d]]]]]]]]]]]]]]]]]]",
        ] {
            let _ = signature(encoding);
        }
    }
}
