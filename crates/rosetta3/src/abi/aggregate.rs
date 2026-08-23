//! Structs by value, across two conventions that disagree about them.

use super::ArgType as T;

/// The eightbyte, which is SysV AMD64's unit of classification.
const EIGHTBYTE: usize = 8;

/// One scalar member of a flattened aggregate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Leaf {
    pub ty: T,
    /// Byte offset from the start of the outermost struct.
    pub offset: usize,
}

/// A struct or array passed or returned by value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Aggregate {
    pub size: usize,
    pub align: usize,
    pub leaves: Vec<Leaf>,
}

/// An argument or return type: one scalar, or one aggregate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Shape {
    Scalar(T),
    Struct(Aggregate),
}

impl Shape {
    pub fn scalar(&self) -> Option<T> {
        match self {
            Shape::Scalar(t) => Some(*t),
            Shape::Struct(_) => None,
        }
    }
}

/// How SysV AMD64 passes one eightbyte of an aggregate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Eightbyte {
    Integer,
    Sse,
}

/// How the *guest* passes or returns an aggregate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GuestClass {
    /// One or two eightbytes, in the argument registers of the named classes.
    Registers(Vec<Eightbyte>),
    /// On the stack, eight-byte aligned. For a return, this is the hidden pointer
    /// convention.
    Memory,
}

/// How the *host* passes or returns one.
///
/// Two vocabularies, one per host, and a given host produces only its own:
/// [`HostClass::Hfa`] and [`HostClass::Integer`] are AAPCS64's, and
/// [`HostClass::Eightbytes`] is SysV AMD64's. A reader of a `match` on this only ever
/// has to hold one of them in mind at a time; the other arm is unreachable there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostClass {
    /// A homogeneous float aggregate: `n` consecutive `v` registers, each
    /// holding one member of the named type.
    Hfa { ty: T, count: usize },
    /// One or two `x` registers holding the raw bytes.
    Integer { words: usize },
    /// One register per eightbyte, taken from the file that eightbyte's class
    /// names. At most two, because SysV sends anything larger to memory.
    ///
    /// The unit is the eightbyte and not the member, which is the whole
    /// difference from [`HostClass::Hfa`]: three floats are three `v` registers
    /// under AAPCS64 and two `xmm` ones here, the first holding a packed pair.
    Eightbytes {
        first: Eightbyte,
        second: Option<Eightbyte>,
    },
    /// Passed and returned through an address rather than in registers.
    Indirect,
}

impl HostClass {
    /// The eightbyte classes, for the arm that has them.
    pub fn slots(self) -> Vec<Eightbyte> {
        match self {
            HostClass::Eightbytes { first, second } => {
                std::iter::once(first).chain(second).collect()
            }
            _ => Vec::new(),
        }
    }
}

impl Aggregate {
    /// SysV AMD64's classification.
    ///
    /// The real algorithm merges the classes of every field overlapping each eightbyte
    /// and has a post-merge pass for `SSEUP` and for structs with unaligned members.
    pub fn guest_class(&self) -> GuestClass {
        if self.size > 2 * EIGHTBYTE {
            return GuestClass::Memory;
        }
        let count = self.size.div_ceil(EIGHTBYTE);
        let mut classes = vec![Eightbyte::Sse; count];
        for leaf in &self.leaves {
            let which = leaf.offset / EIGHTBYTE;
            if which < count && !is_fp(leaf.ty) {
                classes[which] = Eightbyte::Integer;
            }
        }
        GuestClass::Registers(classes)
    }

    /// AAPCS64's classification, or why it cannot be expressed here.
    #[cfg(target_arch = "aarch64")]
    pub fn host_class(&self) -> Result<HostClass, &'static str> {
        if self.align > EIGHTBYTE {
            return Err("an aggregate aligned to sixteen bytes needs an even register pair");
        }
        if let Some(hfa) = self.hfa() {
            return Ok(hfa);
        }
        if self.size <= 2 * EIGHTBYTE {
            return Ok(HostClass::Integer {
                words: self.size.div_ceil(EIGHTBYTE),
            });
        }
        Ok(HostClass::Indirect)
    }

    /// The host's classification where the host's convention is the guest's.
    ///
    /// [`Aggregate::guest_class`] already is SysV AMD64, so this asks it rather than
    /// restating it, and the two cannot drift.
    #[cfg(target_arch = "x86_64")]
    pub fn host_class(&self) -> Result<HostClass, &'static str> {
        let classes = match self.guest_class() {
            // A return goes through the caller's hidden pointer and an argument
            // goes on the stack, which is what `Indirect` means on this side too.
            GuestClass::Memory => return Ok(HostClass::Indirect),
            GuestClass::Registers(classes) => classes,
        };
        let Some(&first) = classes.first() else {
            return Err("an aggregate with no eightbytes");
        };
        Ok(HostClass::Eightbytes {
            first,
            second: classes.get(1).copied(),
        })
    }

    /// The homogeneous-float-aggregate test: one to four members, all the same
    /// floating-point type, and nothing else in the struct.
    ///
    /// AAPCS64's question, and asked only there.
    #[cfg(target_arch = "aarch64")]
    fn hfa(&self) -> Option<HostClass> {
        let first = self.leaves.first()?.ty;
        if !is_fp(first) || self.leaves.is_empty() || self.leaves.len() > 4 {
            return None;
        }
        if !self.leaves.iter().all(|l| l.ty == first) {
            return None;
        }
        // Padding would make the members non-contiguous, and a struct that is
        // larger than its members is not homogeneous whatever they are.
        let width = width_of(first);
        if self.size != width * self.leaves.len() {
            return None;
        }
        Some(HostClass::Hfa {
            ty: first,
            count: self.leaves.len(),
        })
    }
}

pub fn is_fp(ty: T) -> bool {
    matches!(ty, T::SinglePrecisionFp | T::DoublePrecisionFp)
}

/// The size and alignment of a scalar, in bytes.
pub fn width_of(ty: T) -> usize {
    match ty {
        T::SignedByte | T::UnsignedByte => 1,
        T::SignedHalfword | T::UnsignedHalfword => 2,
        T::SignedWord | T::UnsignedWord | T::SinglePrecisionFp => 4,
        _ => 8,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::objc::encoding::aggregate as parse;

    /// The SysV placement of an aggregate small enough for registers, for the
    /// assertions below - which say what *this* host does, because a
    /// classification is an answer about a host and not a fact about a struct.
    #[cfg(target_arch = "x86_64")]
    fn eightbytes(classes: &[Eightbyte]) -> Result<HostClass, &'static str> {
        Ok(HostClass::Eightbytes {
            first: classes[0],
            second: classes.get(1).copied(),
        })
    }

    fn agg(encoding: &str) -> Aggregate {
        let mut at = 0;
        let a = parse(encoding.as_bytes(), &mut at).expect(encoding);
        assert_eq!(at, encoding.len(), "{encoding}: cursor left mid-encoding");
        a
    }

    #[test]
    fn a_nested_struct_flattens_to_its_leaves() {
        // `NSRect`, verbatim from `method_getTypeEncoding`. Four doubles, which
        // is the whole point.
        let rect = agg("{CGRect={CGPoint=dd}{CGSize=dd}}");
        assert_eq!(rect.size, 32);
        assert_eq!(rect.align, 8);
        assert_eq!(
            rect.leaves.iter().map(|l| l.offset).collect::<Vec<_>>(),
            vec![0, 8, 16, 24]
        );
        assert!(rect.leaves.iter().all(|l| l.ty == T::DoublePrecisionFp));
    }

    #[test]
    fn nsrect_is_memory_to_the_guest_and_the_host_answers_for_itself() {
        // The disagreement this module exists for, as one assertion - and the
        // arm64 host is the one that has it. Thirty-two bytes of `double` is
        // four `v` registers to AAPCS64 and memory to SysV, so on an x86-64
        // host there is no disagreement left to carry.
        let rect = agg("{CGRect={CGPoint=dd}{CGSize=dd}}");
        assert_eq!(rect.guest_class(), GuestClass::Memory);
        #[cfg(target_arch = "aarch64")]
        assert_eq!(
            rect.host_class(),
            Ok(HostClass::Hfa {
                ty: T::DoublePrecisionFp,
                count: 4
            })
        );
        #[cfg(target_arch = "x86_64")]
        assert_eq!(rect.host_class(), Ok(HostClass::Indirect));
    }

    #[test]
    fn a_point_is_two_sse_eightbytes_and_a_range_is_two_integer_ones() {
        let point = agg("{CGPoint=dd}");
        assert_eq!(
            point.guest_class(),
            GuestClass::Registers(vec![Eightbyte::Sse, Eightbyte::Sse])
        );
        #[cfg(target_arch = "aarch64")]
        assert_eq!(
            point.host_class(),
            Ok(HostClass::Hfa {
                ty: T::DoublePrecisionFp,
                count: 2
            })
        );
        #[cfg(target_arch = "x86_64")]
        assert_eq!(
            point.host_class(),
            eightbytes(&[Eightbyte::Sse, Eightbyte::Sse])
        );

        let range = agg("{_NSRange=QQ}");
        assert_eq!(
            range.guest_class(),
            GuestClass::Registers(vec![Eightbyte::Integer, Eightbyte::Integer])
        );
        #[cfg(target_arch = "aarch64")]
        assert_eq!(range.host_class(), Ok(HostClass::Integer { words: 2 }));
        #[cfg(target_arch = "x86_64")]
        assert_eq!(
            range.host_class(),
            eightbytes(&[Eightbyte::Integer, Eightbyte::Integer])
        );
    }

    #[test]
    fn one_integer_member_moves_its_whole_eightbyte_to_a_general_register() {
        // `{S=fi}` is eight bytes: a float and an int sharing one eightbyte.
        let mixed = agg("{S=fi}");
        assert_eq!(mixed.size, 8);
        assert_eq!(
            mixed.guest_class(),
            GuestClass::Registers(vec![Eightbyte::Integer])
        );
        // And it is not an HFA, so the host takes it in one general register -
        // by that name on either side, `x0` or `rax`.
        #[cfg(target_arch = "aarch64")]
        assert_eq!(mixed.host_class(), Ok(HostClass::Integer { words: 1 }));
        #[cfg(target_arch = "x86_64")]
        assert_eq!(mixed.host_class(), eightbytes(&[Eightbyte::Integer]));
    }

    #[test]
    fn padding_disqualifies_a_homogeneous_aggregate() {
        // `{S=dfd}` is 24 bytes with four bytes of padding after the float.
        let padded = agg("{S=dfd}");
        assert_eq!(padded.size, 24);
        assert_eq!(padded.host_class(), Ok(HostClass::Indirect));
    }

    #[test]
    fn an_array_member_repeats_its_element() {
        let four = agg("{S=[4f]}");
        assert_eq!(four.size, 16);
        // Four `s` registers to AAPCS64; to SysV two `xmm` ones, each holding a
        // packed pair - the difference the eightbyte vocabulary exists to say.
        #[cfg(target_arch = "aarch64")]
        assert_eq!(
            four.host_class(),
            Ok(HostClass::Hfa {
                ty: T::SinglePrecisionFp,
                count: 4
            })
        );
        #[cfg(target_arch = "x86_64")]
        assert_eq!(
            four.host_class(),
            eightbytes(&[Eightbyte::Sse, Eightbyte::Sse])
        );
        // Sixteen bytes of floats: two SSE eightbytes to the guest.
        assert_eq!(
            four.guest_class(),
            GuestClass::Registers(vec![Eightbyte::Sse, Eightbyte::Sse])
        );
    }

    #[test]
    fn a_five_member_float_aggregate_is_not_homogeneous() {
        // The HFA rule is *at most four*, and `CGAffineTransform` is six. Over
        // sixteen bytes and not an HFA travels by pointer to a caller's copy.
        let transform = agg("{CGAffineTransform=dddddd}");
        assert_eq!(transform.size, 48);
        assert_eq!(transform.host_class(), Ok(HostClass::Indirect));
        assert_eq!(transform.guest_class(), GuestClass::Memory);
    }

    #[test]
    fn a_nested_group_starts_at_its_own_alignment_not_its_first_members() {
        // `struct { char c; struct { int i; double d; } t; }`.
        let s = agg("{S=c{T=id}}");
        assert_eq!(
            s.leaves.iter().map(|l| l.offset).collect::<Vec<_>>(),
            vec![0, 8, 16]
        );
        assert_eq!(s.size, 24);
        assert_eq!(s.align, 8);
    }

    #[test]
    fn a_union_is_refused_rather_than_laid_out() {
        let mut at = 0;
        assert!(parse(b"(u=id)", &mut at).is_err());
    }

    #[test]
    fn a_malformed_aggregate_never_hangs_or_allocates_without_bound() {
        for encoding in [
            "{",
            "{S=",
            "{S=i",
            "[999999999i]",
            "{}",
            "[0i]",
            "{S=[4{T=d}]}",
            "[",
            "{S=\"unterminated}",
            "{S=z}",
        ] {
            let mut at = 0;
            let _ = parse(encoding.as_bytes(), &mut at);
        }
    }
}
