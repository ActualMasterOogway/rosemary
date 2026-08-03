//! `std::string`, in the *guest's* layout rather than the host's.
//!
//! ## The fact this module exists for
//!
//! **libc++'s `basic_string` is laid out differently on x86-64 macOS and on
//! arm64 macOS.** Not the size — both are twenty-four bytes — the *order and
//! meaning of every field in them*:
//!
//! | | x86-64 | arm64 |
//! |---|---|---|
//! | long: capacity | `+0`, as `(cap+1) \| 1` | `+16`, as `(cap+1) \| 1<<63` |
//! | long: size | `+8` | `+8` |
//! | long: data | `+16` | `+0` |
//! | short: size | byte `0`, as `size << 1` | byte `23`, as `size` |
//! | short: data | `+1` | `+0` |
//! | "is long" | bit 0 of byte `0` | bit 7 of byte `23` |
//!
//! That is libc++'s `_LIBCPP_ABI_ALTERNATE_STRING_LAYOUT`, which Apple selected
//! for arm64 because arm64 was a new platform with no ABI to keep. It is
//! invisible to a normal program — you never mix architectures inside one
//! process — and it is the whole difficulty here, because native passthrough
//! does exactly that: it sends a guest `basic_string *` to a host arm64
//! `libc++`.
//!
//! Checked against the compiler rather than the documentation. `std::string
//! s = "google/protobuf"` built `-arch x86_64` has bytes
//! `1e 67 6f 6f 67 6c 65 2f 70 72 6f 74 6f 62 75 66 …` — `15 << 1`, then the
//! characters. Built `-arch arm64` it has
//! `67 6f 6f 67 6c 65 2f 70 72 6f 74 6f 62 75 66 00 … 0f` — the characters,
//! then `15` in the last byte.
//!
//! ## How it presented
//!
//! It is worth recording, because nothing in the failure named a string. A
//! `steamclient.dylib` initializer parses protobuf's own descriptors;
//! `ReadString` called the host's `assign`, which wrote an *arm64* string into
//! guest memory; the guest's own inline `data()` then read `+16` and got
//! `0x0f00000000000000` — the arm64 short-string's size byte, sitting where
//! x86-64 keeps the pointer — and `size()` read `+8` and got `0x6f746f72`,
//! `"roto"`, the middle of `"google/protobuf"`. protobuf logged an invalid-UTF-8
//! complaint about a string that was never invalid, and the next pass
//! dereferenced `0x0f00000000000000` and took `SIGSEGV` at `0xf00000000000004`.
//!
//! Three functions away from anything named `string`, with no refusal to hang
//! it on: exactly the shape the project's "never guess a signature" rule exists
//! to prevent, arriving through a signature that was *correct*. Every argument
//! really is a `this` pointer and a scalar. What the table could not say is that
//! the pointer addresses a structure the two sides read differently.
//!
//! ## What is implemented
//!
//! The out-of-line half of `basic_string<char>` and `basic_string<wchar_t>` —
//! the entry points that live in `libc++.dylib` rather than in the header, which
//! are exactly the ones a guest imports. Everything else about a string is
//! inlined into the guest and needs nothing from us; it already reads and writes
//! the x86-64 layout, and now so does this.
//!
//! Each operation loads the string's characters, works on them in Rust, and
//! stores them back. That is not a shortcut: it is what makes the layout appear
//! in exactly two functions, [`GuestStr::load`] and [`GuestStr::store`], instead
//! of once per entry point.
//!
//! **Pointer stability is preserved where libc++ preserves it.** A store that
//! fits inside the current capacity writes in place and updates the size word,
//! so a `c_str()` the guest is holding stays valid across an `erase` or a
//! shrinking `assign` — as it must, because that is a guarantee the standard
//! makes and guest code takes.
//!
//! ## Allocation
//!
//! A string's buffer is allocated by `operator new` and freed by `operator
//! delete`, and the guest inlines calls to both — `~basic_string` is often
//! inlined even where the dylib copy is also imported. So this module cannot
//! use the host's `malloc`: it has to use **the same allocator the guest's own
//! inline code uses**, or a buffer allocated here is freed by an allocator that
//! never saw it.
//!
//! [`Alloc::resolve`] answers that from the transcript rather than by choosing:
//! it asks what the guest's `_Znwm` import actually *resolved to*. For UNDERTALE
//! that is `Mac_Runner`'s own `operator new` — GameMaker replaces it — so the
//! shim re-enters the JIT to allocate, which is correct precisely because
//! replacing global `operator new` is a whole-program property in C++
//! ([replacement.functions]) and there is therefore one right answer per
//! process.
//!
//! ## Scope
//!
//! What is *not* here, and is refused by name instead: any host entry point that
//! takes a `basic_string` and is not itself a string operation —
//! `std::runtime_error(const string &)` and `std::random_device(const string &)`
//! are the two a guest reaches. They have the identical problem and a different
//! answer (the object they build is the host's, not the guest's), so each is a
//! work item of its own rather than a line here.
//!
//! `at`, `erase`, `insert`, `replace` and `compare` throw `out_of_range` on a
//! bad index. A shim cannot throw into the guest, so an out-of-range index is
//! **refused by name** — a diagnostic where the guest would have got an
//! exception, which is a difference worth having over a silent wrong answer.

use microlift::arch::x86_64::context::RegisterContext;

use crate::abi::{AbiError, GuestMemory, Result};
use crate::vm::Vm;

/// Longest string this module will materialize, as a sanity bound.
///
/// Totality rather than policy, in the house style: a size word read from a
/// corrupt object must end the call rather than the process, and a length that
/// large is a wrong pointer in every real program.
const MAX_LEN: u64 = 1 << 30;

/// `npos`.
const NPOS: u64 = u64::MAX;

// ------------------------------------------------------------- layout -------

/// A `basic_string` instantiation, described by its character width.
///
/// Every constant libc++ uses is derived from `sizeof(value_type)` and
/// `sizeof(__long)`, so one width is the whole difference between `string` and
/// `wstring`. Both derivations are checked against the compiler in
/// `tests/cxxstring.rs`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Layout {
    /// `sizeof(value_type)`: 1 for `char`, 4 for `wchar_t`.
    width: u64,
}

/// `basic_string<char>`.
const CHAR: Layout = Layout { width: 1 };
/// `basic_string<wchar_t>`. `wchar_t` is `int` on both architectures.
const WCHAR: Layout = Layout { width: 4 };

/// `sizeof(basic_string)`, which is `sizeof(__long)`: three words.
const OBJECT_SIZE: u64 = 24;

impl Layout {
    /// `__min_cap = (sizeof(__long) - 1) / sizeof(value_type)`.
    ///
    /// 23 for `char`, 5 for `wchar_t` — an integer division, so the `wchar_t`
    /// answer is not 5.75.
    const fn min_cap(self) -> u64 {
        (OBJECT_SIZE - 1) / self.width
    }

    /// The longest string that fits inside the object itself.
    const fn short_cap(self) -> u64 {
        self.min_cap() - 1
    }

    /// Where a short string's characters begin, relative to the object.
    ///
    /// `__short` is `{union {unsigned char __size_; value_type __lx;}; value_type
    /// __data_[__min_cap];}`, and that leading union is `sizeof(value_type)`
    /// wide because it has to be aligned for one — so the data starts at 1 for
    /// `char` and at 4 for `wchar_t`.
    const fn short_data(self) -> u64 {
        self.width
    }

    /// `__recommend`'s alignment boundary, in characters.
    ///
    /// `sizeof(value_type) < __alignment ? __alignment / sizeof(value_type) : 1`,
    /// with `__alignment` 8.
    const fn boundary(self) -> u64 {
        match 8 / self.width {
            0 => 1,
            n => n,
        }
    }

    /// libc++'s `__recommend`: the capacity it will actually take for a
    /// requested one.
    ///
    /// Reproduced rather than approximated because it is cheap to, and because
    /// the `+= 2` in the middle is the kind of detail that is invisible until it
    /// matters: a capacity of exactly `__min_cap` would be stored as
    /// `(cap+1) | 1` with `cap+1` **odd**, and the low bit is the long/short
    /// flag — so the stored word would read back one short. libc++ steps over it
    /// by `__endian_factor`, which is 2 in this layout for the same reason a
    /// short size is stored doubled.
    ///
    /// Nothing here depends on matching the host's growth policy exactly; the
    /// guest only ever reads back the capacity this writes. It is matched
    /// anyway, so that a guest tuned to libc++'s reallocation points behaves the
    /// way it was tuned.
    fn recommend(self, requested: u64) -> u64 {
        if requested < self.min_cap() {
            return self.min_cap() - 1;
        }
        let guess = requested.saturating_add(1).next_multiple_of(self.boundary()) - 1;
        match guess == self.min_cap() {
            true => guess + 2,
            false => guess,
        }
    }
}

// ---------------------------------------------------------- allocation ------

/// The `operator new` / `operator delete` pair a guest string's buffer belongs
/// to.
#[derive(Clone, Copy)]
struct Alloc {
    /// `operator new(size_t)`.
    new: u64,
    /// `operator delete(void *)`.
    delete: u64,
    /// Whether those are guest addresses, and so have to be entered through the
    /// JIT rather than called.
    guest: bool,
}

/// The answer, once found. One process runs one guest, and replacing global
/// `operator new` is a whole-program property in C++, so there is exactly one
/// right answer and it does not change.
static RESOLVED: std::sync::OnceLock<Option<(u64, u64, bool)>> = std::sync::OnceLock::new();

impl Alloc {
    /// What the guest's own `operator new` and `operator delete` are.
    ///
    /// Read out of the link transcript — the value the guest's `_Znwm` import
    /// was bound to — so this answers whatever *the guest* would reach, host
    /// function or guest function, without deciding anything. The fallback to
    /// `dlsym` covers a program that imports neither, which is a program with no
    /// C++ in it and so no strings either; it is there for totality.
    ///
    /// Resolved on demand rather than per call, and only by the operations that
    /// can allocate: `find` and `compare` must keep working in a program whose
    /// `operator new` cannot be found, because they never needed it.
    fn resolve(vm: &Vm, symbol: &str) -> Result<Self> {
        let found = *RESOLVED.get_or_init(|| {
            let find = |name: &str| {
                vm.program
                    .resolved_symbol(name)
                    .or_else(|| crate::dyld::resolved_symbol(name))
                    .or_else(|| crate::dyld::host_symbol(name.strip_prefix('_')?))
            };
            let new = find("__Znwm")?;
            let delete = find("__ZdlPv")?;
            Some((new, delete, vm.program.is_guest_address(new)))
        });
        match found {
            Some((new, delete, guest)) => Ok(Self {
                new,
                delete,
                guest,
            }),
            None => Err(AbiError::Refused {
                symbol: symbol.to_string(),
                reason: "a string buffer needs the guest's operator new, and neither it \
                         nor the host's could be found",
            }),
        }
    }

    fn allocate(&self, vm: &Vm, symbol: &str, bytes: u64) -> Result<u64> {
        let p = match self.guest {
            true => vm
                .call(self.new, &[bytes])
                .map_err(|e| AbiError::Nested {
                    symbol: symbol.to_string(),
                    reason: format!("the guest's operator new failed: {e}"),
                })?,
            // SAFETY: `new` is what the guest's `_Znwm` import resolved to and
            // is not a guest address, so it is a native `void *(size_t)` — the
            // one prototype `operator new` has on either architecture.
            false => unsafe {
                core::mem::transmute::<u64, extern "C" fn(u64) -> u64>(self.new)(bytes)
            },
        };
        match p {
            0 => Err(AbiError::Refused {
                symbol: symbol.to_string(),
                reason: "operator new returned null for a string buffer",
            }),
            p => Ok(p),
        }
    }

    fn deallocate(&self, vm: &Vm, symbol: &str, p: u64) -> Result<()> {
        if p == 0 {
            return Ok(());
        }
        match self.guest {
            true => {
                vm.call(self.delete, &[p]).map_err(|e| AbiError::Nested {
                    symbol: symbol.to_string(),
                    reason: format!("the guest's operator delete failed: {e}"),
                })?;
            }
            // SAFETY: as in `allocate`; `void (void *)`.
            false => unsafe {
                core::mem::transmute::<u64, extern "C" fn(u64)>(self.delete)(p);
            },
        }
        Ok(())
    }
}

// ------------------------------------------------------------- the object ---

/// A `basic_string` at a guest address, read and written in the guest's layout.
///
/// The two functions that know the table at the top of this module are
/// [`GuestStr::load`] and [`GuestStr::store`]; everything else in the file
/// works on `Vec<u8>`.
pub(crate) struct GuestStr {
    /// The object's address in guest memory.
    this: u64,
    l: Layout,
}

impl GuestStr {
    fn new(this: u64, l: Layout) -> Self {
        Self { this, l }
    }

    /// Whether the buffer is out of line. Bit 0 of the first byte.
    fn is_long(&self, mem: &dyn GuestMemory, symbol: &str) -> Result<bool> {
        Ok(mem.read_u64(self.this, symbol)? & 1 == 1)
    }

    /// The characters' address, in the object or on the heap.
    fn data(&self, mem: &dyn GuestMemory, symbol: &str) -> Result<u64> {
        match self.is_long(mem, symbol)? {
            true => mem.read_u64(self.this + 16, symbol),
            false => Ok(self.this + self.l.short_data()),
        }
    }

    /// `size()`, in characters.
    fn size(&self, mem: &dyn GuestMemory, symbol: &str) -> Result<u64> {
        let n = match self.is_long(mem, symbol)? {
            true => mem.read_u64(self.this + 8, symbol)?,
            // `__get_short_size` is `__size_ >> 1`: the low bit is the flag.
            false => (mem.read_u64(self.this, symbol)? & 0xff) >> 1,
        };
        match n > MAX_LEN {
            true => Err(AbiError::Refused {
                symbol: symbol.to_string(),
                reason: "the string's size word is implausible; the object is not a \
                         basic_string, or is not the guest's",
            }),
            false => Ok(n),
        }
    }

    /// `capacity()`, in characters.
    fn capacity(&self, mem: &dyn GuestMemory, symbol: &str) -> Result<u64> {
        match self.is_long(mem, symbol)? {
            // `__get_long_cap` masks the flag off and subtracts the NUL.
            true => Ok((mem.read_u64(self.this, symbol)? & !1u64).saturating_sub(1)),
            false => Ok(self.l.short_cap()),
        }
    }

    /// The characters, without the terminator.
    fn load(&self, mem: &dyn GuestMemory, symbol: &str) -> Result<Vec<u8>> {
        let size = self.size(mem, symbol)?;
        let at = self.data(mem, symbol)?;
        let len = usize::try_from(size * self.l.width).map_err(|_| AbiError::Memory {
            symbol: symbol.to_string(),
            addr: at,
        })?;
        mem.read_bytes(at, len, symbol)
    }

    /// Replace the characters, reallocating only if they no longer fit.
    ///
    /// The "only if" is the contract, not an optimisation: libc++ leaves
    /// `c_str()` valid across any operation that does not grow past the
    /// capacity, and guest code holds those pointers.
    fn store(&self, c: &Call<'_>, mem: &mut dyn GuestMemory, content: &[u8]) -> Result<()> {
        let symbol = c.symbol;
        let n = content.len() as u64 / self.l.width;
        if n > MAX_LEN {
            return Err(too_long(symbol));
        }
        if n <= self.capacity(mem, symbol)? {
            let at = self.data(mem, symbol)?;
            mem.write_bytes(at, content, symbol)?;
            mem.write_bytes(at + content.len() as u64, &nul(self.l), symbol)?;
            return self.set_size(mem, symbol, n);
        }
        let alloc = c.alloc()?;
        let was_long = self.is_long(mem, symbol)?;
        let old = self.data(mem, symbol)?;
        // libc++ reaches every growing operation through `__grow_by`, which asks
        // for `max(old_cap + delta, 2 * old_cap)` — geometric growth, so that
        // appending one character at a time is linear rather than quadratic.
        // Taking `recommend(n)` instead is correct and reallocates on nearly
        // every `push_back`; the fixture's `capacity()` column is what caught it.
        let old_cap = self.capacity(mem, symbol)?;
        let cap = self.l.recommend(n.max(old_cap.saturating_mul(2)));
        let p = alloc.allocate(c.vm, symbol, (cap + 1) * self.l.width)?;
        mem.write_bytes(p, content, symbol)?;
        mem.write_bytes(p + content.len() as u64, &nul(self.l), symbol)?;
        // The old buffer is released *after* the new one is populated, so a
        // failure anywhere above leaves the string exactly as it was.
        if was_long {
            alloc.deallocate(c.vm, symbol, old)?;
        }
        self.set_long(mem, symbol, p, cap, n)
    }

    /// Initialize an object whose contents are *not* yet a string.
    ///
    /// A constructor is handed raw storage, so nothing may be read out of it
    /// first — which is the only difference from [`GuestStr::store`], and the
    /// reason it is a separate function rather than a flag.
    fn init(&self, c: &Call<'_>, mem: &mut dyn GuestMemory, content: &[u8]) -> Result<()> {
        let symbol = c.symbol;
        let n = content.len() as u64 / self.l.width;
        if n > MAX_LEN {
            return Err(too_long(symbol));
        }
        if n <= self.l.short_cap() {
            let at = self.this + self.l.short_data();
            mem.write_bytes(at, content, symbol)?;
            mem.write_bytes(at + content.len() as u64, &nul(self.l), symbol)?;
            // A short string's size shares its word with the first seven
            // characters, so the flag byte is written *after* them.
            return self.set_short_size(mem, symbol, n);
        }
        let cap = self.l.recommend(n);
        let p = c.alloc()?.allocate(c.vm, symbol, (cap + 1) * self.l.width)?;
        mem.write_bytes(p, content, symbol)?;
        mem.write_bytes(p + content.len() as u64, &nul(self.l), symbol)?;
        self.set_long(mem, symbol, p, cap, n)
    }

    /// Write the size, in whichever of the two places currently applies.
    fn set_size(&self, mem: &mut dyn GuestMemory, symbol: &str, n: u64) -> Result<()> {
        match self.is_long(mem, symbol)? {
            true => mem.write_u64(self.this + 8, n, symbol),
            false => self.set_short_size(mem, symbol, n),
        }
    }

    /// `__set_short_size`: the size doubled, in the first byte, flag clear.
    ///
    /// A read-modify-write of the whole word rather than a byte store, because
    /// the other seven bytes of it are the string's first seven characters.
    fn set_short_size(&self, mem: &mut dyn GuestMemory, symbol: &str, n: u64) -> Result<()> {
        let word = mem.read_u64(self.this, symbol)?;
        mem.write_u64(self.this, (word & !0xff) | ((n << 1) & 0xff), symbol)
    }

    /// `__set_long_pointer` + `__set_long_cap` + `__set_long_size`, together,
    /// because a half-applied one is not a string.
    fn set_long(
        &self,
        mem: &mut dyn GuestMemory,
        symbol: &str,
        data: u64,
        cap: u64,
        size: u64,
    ) -> Result<()> {
        mem.write_u64(self.this, (cap + 1) | 1, symbol)?;
        mem.write_u64(self.this + 8, size, symbol)?;
        mem.write_u64(self.this + 16, data, symbol)
    }
}

/// The refusal for a length no runtime should turn into an allocation.
fn too_long(symbol: &str) -> AbiError {
    AbiError::Refused {
        symbol: symbol.to_string(),
        reason: "the resulting string is longer than this runtime will build; the \
                 length is a wrong value rather than a large string",
    }
}

/// One terminating character's worth of zero bytes.
fn nul(l: Layout) -> Vec<u8> {
    vec![0u8; l.width as usize]
}

/// A character repeated, as bytes. `wchar_t` is little-endian `int`.
fn repeat(l: Layout, c: u64, n: u64) -> Vec<u8> {
    let unit = &c.to_le_bytes()[..l.width as usize];
    unit.repeat(n as usize)
}

// ------------------------------------------------------------ the shims -----

/// What every entry point needs before it can do anything.
struct Call<'a> {
    vm: &'static Vm,
    symbol: &'a str,
    l: Layout,
}

impl<'a> Call<'a> {
    fn new(symbol: &'a str, l: Layout) -> Result<Self> {
        let vm = crate::thunk::current().ok_or_else(|| AbiError::Refused {
            symbol: symbol.to_string(),
            reason: "a string operation may have to allocate, which needs the JIT, \
                     and no VM is running on this thread",
        })?;
        Ok(Self { vm, symbol, l })
    }

    fn at(&self, this: u64) -> GuestStr {
        GuestStr::new(this, self.l)
    }

    /// The guest's allocator, found the first time one is needed.
    fn alloc(&self) -> Result<Alloc> {
        Alloc::resolve(self.vm, self.symbol)
    }

    fn store(&self, mem: &mut dyn GuestMemory, s: &GuestStr, content: &[u8]) -> Result<()> {
        s.store(self, mem, content)
    }

    fn init(&self, mem: &mut dyn GuestMemory, s: &GuestStr, content: &[u8]) -> Result<()> {
        s.init(self, mem, content)
    }

    /// Refuse a count that would make this runtime allocate on the guest's
    /// behalf without bound. Totality, as in [`MAX_LEN`].
    fn bounded(&self, n: u64) -> Result<u64> {
        match n > MAX_LEN {
            true => Err(too_long(self.symbol)),
            false => Ok(n),
        }
    }

    /// The guest indexed past the end. libc++ throws; see the module's scope
    /// note for why this reports instead.
    fn out_of_range(&self) -> AbiError {
        AbiError::Refused {
            symbol: self.symbol.to_string(),
            reason: "the guest passed an out-of-range index, where libc++ would have \
                     thrown std::out_of_range, and a shim cannot throw into the guest",
        }
    }

    /// A NUL-terminated run of characters at a guest address, in this
    /// instantiation's width.
    fn cstr(&self, mem: &dyn GuestMemory, at: u64) -> Result<Vec<u8>> {
        let width = self.l.width as usize;
        let mut out = Vec::new();
        for i in 0..crate::abi::MAX_CSTR as u64 {
            let unit = mem.read_bytes(at + i * self.l.width, width, self.symbol)?;
            if unit.iter().all(|&b| b == 0) {
                return Ok(out);
            }
            out.extend_from_slice(&unit);
        }
        Err(AbiError::Refused {
            symbol: self.symbol.to_string(),
            reason: "an unterminated character array was passed where a C string was \
                     expected",
        })
    }
}

/// Every argument SysV AMD64 puts in a register, in order.
fn args(ctx: &RegisterContext) -> [u64; 6] {
    [ctx.rdi, ctx.rsi, ctx.rdx, ctx.rcx, ctx.r8, ctx.r9]
}

/// Return `this`, which is what every mutating `basic_string` member does.
fn ret(ctx: &mut RegisterContext, value: u64) {
    ctx.rax = value;
}

// -- construction and destruction --------------------------------------------

/// `void __init(const char *s, size_t n)`
fn init_from_pointer(
    symbol: &str,
    ctx: &mut RegisterContext,
    mem: &mut dyn GuestMemory,
) -> Result<()> {
    let c = Call::new(symbol, CHAR)?;
    let [this, s, n, ..] = args(ctx);
    let content = mem.read_bytes(s, c.bounded(n)? as usize, symbol)?;
    c.init(mem, &c.at(this), &content)
}

/// `void __init(size_t n, charT c)` — the fill constructor's out-of-line half.
fn init_fill_as(
    symbol: &str,
    ctx: &mut RegisterContext,
    mem: &mut dyn GuestMemory,
    l: Layout,
) -> Result<()> {
    let c = Call::new(symbol, l)?;
    let [this, n, fill, ..] = args(ctx);
    let content = repeat(l, fill, c.bounded(n)?);
    c.init(mem, &c.at(this), &content)
}

/// `basic_string(const basic_string &)`
fn copy_construct_as(
    symbol: &str,
    ctx: &mut RegisterContext,
    mem: &mut dyn GuestMemory,
    l: Layout,
) -> Result<()> {
    let c = Call::new(symbol, l)?;
    let [this, other, ..] = args(ctx);
    let content = c.at(other).load(mem, symbol)?;
    c.init(mem, &c.at(this), &content)
}

/// `basic_string(const basic_string &str, size_type pos, size_type n, const
/// allocator_type &)`
fn substr_construct_as(
    symbol: &str,
    ctx: &mut RegisterContext,
    mem: &mut dyn GuestMemory,
    l: Layout,
) -> Result<()> {
    let c = Call::new(symbol, l)?;
    let [this, other, pos, n, ..] = args(ctx);
    let content = c.at(other).load(mem, symbol)?;
    let size = content.len() as u64 / l.width;
    if pos > size {
        return Err(c.out_of_range());
    }
    let take = n.min(size - pos);
    let from = (pos * l.width) as usize;
    let to = from + (take * l.width) as usize;
    c.init(mem, &c.at(this), &content[from..to])
}

/// `~basic_string()`
fn destroy_as(
    symbol: &str,
    ctx: &mut RegisterContext,
    mem: &mut dyn GuestMemory,
    l: Layout,
) -> Result<()> {
    let c = Call::new(symbol, l)?;
    let [this, ..] = args(ctx);
    let s = c.at(this);
    if s.is_long(mem, symbol)? {
        let data = s.data(mem, symbol)?;
        c.alloc()?.deallocate(c.vm, symbol, data)?;
    }
    // libc++ leaves the object alone afterwards, and so does this: a guest that
    // reads a destroyed string is already wrong, and zeroing here would hide it.
    Ok(())
}

/// `basic_string &operator=(const basic_string &)`
fn assign_string(symbol: &str, ctx: &mut RegisterContext, mem: &mut dyn GuestMemory) -> Result<()> {
    let c = Call::new(symbol, CHAR)?;
    let [this, other, ..] = args(ctx);
    if this != other {
        let content = c.at(other).load(mem, symbol)?;
        c.store(mem, &c.at(this), &content)?;
    }
    ret(ctx, this);
    Ok(())
}

// -- assignment and appending ------------------------------------------------

/// `basic_string &assign(const char *s)`
fn assign_cstr(symbol: &str, ctx: &mut RegisterContext, mem: &mut dyn GuestMemory) -> Result<()> {
    let c = Call::new(symbol, CHAR)?;
    let [this, s, ..] = args(ctx);
    let content = c.cstr(mem, s)?;
    c.store(mem, &c.at(this), &content)?;
    ret(ctx, this);
    Ok(())
}

/// `basic_string &assign(const char *s, size_t n)`
fn assign_counted(symbol: &str, ctx: &mut RegisterContext, mem: &mut dyn GuestMemory) -> Result<()> {
    let c = Call::new(symbol, CHAR)?;
    let [this, s, n, ..] = args(ctx);
    let content = mem.read_bytes(s, c.bounded(n)? as usize, symbol)?;
    c.store(mem, &c.at(this), &content)?;
    ret(ctx, this);
    Ok(())
}

/// `basic_string &append(const char *s)`
fn append_cstr(symbol: &str, ctx: &mut RegisterContext, mem: &mut dyn GuestMemory) -> Result<()> {
    let c = Call::new(symbol, CHAR)?;
    let [this, s, ..] = args(ctx);
    let mut content = c.at(this).load(mem, symbol)?;
    content.extend_from_slice(&c.cstr(mem, s)?);
    c.store(mem, &c.at(this), &content)?;
    ret(ctx, this);
    Ok(())
}

/// `basic_string &append(const char *s, size_t n)` — and the `wchar_t` form,
/// which differs only in the width of the characters counted.
fn append_counted_as(
    symbol: &str,
    ctx: &mut RegisterContext,
    mem: &mut dyn GuestMemory,
    l: Layout,
) -> Result<()> {
    let c = Call::new(symbol, l)?;
    let [this, s, n, ..] = args(ctx);
    let bytes = (c.bounded(n)? * l.width) as usize;
    let mut content = c.at(this).load(mem, symbol)?;
    content.extend_from_slice(&mem.read_bytes(s, bytes, symbol)?);
    c.store(mem, &c.at(this), &content)?;
    ret(ctx, this);
    Ok(())
}

/// `basic_string &append(const basic_string &str, size_t pos, size_t n)`
fn append_substr(symbol: &str, ctx: &mut RegisterContext, mem: &mut dyn GuestMemory) -> Result<()> {
    let c = Call::new(symbol, CHAR)?;
    let [this, other, pos, n, ..] = args(ctx);
    let from_them = c.at(other).load(mem, symbol)?;
    let size = from_them.len() as u64;
    if pos > size {
        return Err(c.out_of_range());
    }
    let take = n.min(size - pos);
    let mut content = c.at(this).load(mem, symbol)?;
    content.extend_from_slice(&from_them[pos as usize..(pos + take) as usize]);
    c.store(mem, &c.at(this), &content)?;
    ret(ctx, this);
    Ok(())
}

/// `basic_string &append(size_t n, char c)`
fn append_fill(symbol: &str, ctx: &mut RegisterContext, mem: &mut dyn GuestMemory) -> Result<()> {
    let c = Call::new(symbol, CHAR)?;
    let [this, n, fill, ..] = args(ctx);
    let mut content = c.at(this).load(mem, symbol)?;
    content.extend_from_slice(&repeat(CHAR, fill, c.bounded(n)?));
    c.store(mem, &c.at(this), &content)?;
    ret(ctx, this);
    Ok(())
}

/// `void push_back(charT c)`
fn push_back_as(
    symbol: &str,
    ctx: &mut RegisterContext,
    mem: &mut dyn GuestMemory,
    l: Layout,
) -> Result<()> {
    let c = Call::new(symbol, l)?;
    let [this, ch, ..] = args(ctx);
    let mut content = c.at(this).load(mem, symbol)?;
    content.extend_from_slice(&repeat(l, ch, 1));
    c.store(mem, &c.at(this), &content)
}

// -- size and capacity -------------------------------------------------------

/// `void resize(size_t n, charT c)`
fn resize_as(
    symbol: &str,
    ctx: &mut RegisterContext,
    mem: &mut dyn GuestMemory,
    l: Layout,
) -> Result<()> {
    let c = Call::new(symbol, l)?;
    let [this, n, fill, ..] = args(ctx);
    let mut content = c.at(this).load(mem, symbol)?;
    let want = (c.bounded(n)? * l.width) as usize;
    match want <= content.len() {
        true => content.truncate(want),
        false => content.extend_from_slice(&repeat(l, fill, n - content.len() as u64 / l.width)),
    }
    c.store(mem, &c.at(this), &content)
}

/// `void reserve(size_t n)`
///
/// Never shrinks — the guarantee current libc++ makes — so a request that
/// already fits is a no-op, and one that does not reallocates without changing
/// what the string holds.
fn reserve(symbol: &str, ctx: &mut RegisterContext, mem: &mut dyn GuestMemory) -> Result<()> {
    let c = Call::new(symbol, CHAR)?;
    let [this, requested, ..] = args(ctx);
    let s = c.at(this);
    let content = s.load(mem, symbol)?;
    let target = requested.max(content.len() as u64);
    if target <= s.capacity(mem, symbol)? {
        return Ok(());
    }
    let cap = CHAR.recommend(c.bounded(target)?);
    let alloc = c.alloc()?;
    let was_long = s.is_long(mem, symbol)?;
    let old = s.data(mem, symbol)?;
    let p = alloc.allocate(c.vm, symbol, cap + 1)?;
    mem.write_bytes(p, &content, symbol)?;
    mem.write_bytes(p + content.len() as u64, &[0], symbol)?;
    if was_long {
        alloc.deallocate(c.vm, symbol, old)?;
    }
    s.set_long(mem, symbol, p, cap, content.len() as u64)
}

/// `void __grow_by(size_t old_cap, size_t delta_cap, size_t old_sz, size_t
/// n_copy, size_t n_del, size_t n_add)`
///
/// The one entry point that cannot go through [`GuestStr::store`], because its
/// whole job is to leave a **hole**: it reallocates, copies `n_copy` characters
/// to the front and the tail past `n_del` to `n_copy + n_add`, and returns with
/// the gap uninitialized for its caller to fill. It also deliberately does not
/// set the size — the inline caller does that afterwards, which is why a size
/// written here would be overwritten and a size *not* written here is correct.
fn grow_by(symbol: &str, ctx: &mut RegisterContext, mem: &mut dyn GuestMemory) -> Result<()> {
    let c = Call::new(symbol, CHAR)?;
    let [this, old_cap, delta_cap, old_sz, n_copy, n_del] = args(ctx);
    // The sixth argument, `n_add`, is the seventh register SysV does not have:
    // the guest spilled it to its own stack, immediately above the return
    // address the intercepted `CALL` never pushed.
    let n_add = mem.read_u64(ctx.rsp, symbol)?;
    if n_copy > old_sz || n_del > old_sz - n_copy {
        return Err(c.out_of_range());
    }
    c.bounded(old_sz)?;
    c.bounded(n_add)?;
    let s = c.at(this);
    let old = s.data(mem, symbol)?;
    let want = old_cap.saturating_add(delta_cap).max(old_cap.saturating_mul(2));
    let cap = CHAR.recommend(c.bounded(want)?);
    let alloc = c.alloc()?;
    let p = alloc.allocate(c.vm, symbol, cap + 1)?;
    if n_copy != 0 {
        let head = mem.read_bytes(old, n_copy as usize, symbol)?;
        mem.write_bytes(p, &head, symbol)?;
    }
    let tail = old_sz - n_del - n_copy;
    if tail != 0 {
        let bytes = mem.read_bytes(old + n_copy + n_del, tail as usize, symbol)?;
        mem.write_bytes(p + n_copy + n_add, &bytes, symbol)?;
    }
    // libc++'s own test for "was this out of line", phrased against the
    // *argument* rather than against the object, exactly as it is there.
    if old_cap + 1 != CHAR.min_cap() {
        alloc.deallocate(c.vm, symbol, old)?;
    }
    mem.write_u64(this, (cap + 1) | 1, symbol)?;
    mem.write_u64(this + 16, p, symbol)
}

// -- editing -----------------------------------------------------------------

/// `basic_string &erase(size_t pos, size_t n)`
fn erase(symbol: &str, ctx: &mut RegisterContext, mem: &mut dyn GuestMemory) -> Result<()> {
    let c = Call::new(symbol, CHAR)?;
    let [this, pos, n, ..] = args(ctx);
    let mut content = c.at(this).load(mem, symbol)?;
    let size = content.len() as u64;
    if pos > size {
        return Err(c.out_of_range());
    }
    let take = n.min(size - pos);
    content.drain(pos as usize..(pos + take) as usize);
    c.store(mem, &c.at(this), &content)?;
    ret(ctx, this);
    Ok(())
}

/// `basic_string &insert(size_t pos, const char *s)`
fn insert_cstr(symbol: &str, ctx: &mut RegisterContext, mem: &mut dyn GuestMemory) -> Result<()> {
    let c = Call::new(symbol, CHAR)?;
    let [this, pos, s, ..] = args(ctx);
    let mut content = c.at(this).load(mem, symbol)?;
    if pos > content.len() as u64 {
        return Err(c.out_of_range());
    }
    let inserted = c.cstr(mem, s)?;
    content.splice(pos as usize..pos as usize, inserted);
    c.store(mem, &c.at(this), &content)?;
    ret(ctx, this);
    Ok(())
}

/// `basic_string &replace(size_t pos, size_t n1, const char *s)`
fn replace_cstr(symbol: &str, ctx: &mut RegisterContext, mem: &mut dyn GuestMemory) -> Result<()> {
    let c = Call::new(symbol, CHAR)?;
    let [this, pos, n1, s, ..] = args(ctx);
    let mut content = c.at(this).load(mem, symbol)?;
    let size = content.len() as u64;
    if pos > size {
        return Err(c.out_of_range());
    }
    let take = n1.min(size - pos);
    let with = c.cstr(mem, s)?;
    content.splice(pos as usize..(pos + take) as usize, with);
    c.store(mem, &c.at(this), &content)?;
    ret(ctx, this);
    Ok(())
}

// -- inspection --------------------------------------------------------------

/// `char &at(size_t pos)`, and the `const` overload, which is the same address.
fn at(symbol: &str, ctx: &mut RegisterContext, mem: &mut dyn GuestMemory) -> Result<()> {
    let c = Call::new(symbol, CHAR)?;
    let [this, pos, ..] = args(ctx);
    let s = c.at(this);
    if pos >= s.size(mem, symbol)? {
        return Err(c.out_of_range());
    }
    ret(ctx, s.data(mem, symbol)? + pos);
    Ok(())
}

/// `size_t find(char c, size_t pos) const`
fn find_char(symbol: &str, ctx: &mut RegisterContext, mem: &mut dyn GuestMemory) -> Result<()> {
    let c = Call::new(symbol, CHAR)?;
    let [this, needle, pos, ..] = args(ctx);
    let content = c.at(this).load(mem, symbol)?;
    let found = match pos >= content.len() as u64 {
        true => NPOS,
        false => content[pos as usize..]
            .iter()
            .position(|&b| b == needle as u8)
            .map_or(NPOS, |i| pos + i as u64),
    };
    ret(ctx, found);
    Ok(())
}

/// `size_t rfind(char c, size_t pos) const`
fn rfind_char(symbol: &str, ctx: &mut RegisterContext, mem: &mut dyn GuestMemory) -> Result<()> {
    let c = Call::new(symbol, CHAR)?;
    let [this, needle, pos, ..] = args(ctx);
    let content = c.at(this).load(mem, symbol)?;
    let found = match content.is_empty() {
        true => NPOS,
        false => {
            let last = pos.min(content.len() as u64 - 1) as usize;
            content[..=last]
                .iter()
                .rposition(|&b| b == needle as u8)
                .map_or(NPOS, |i| i as u64)
        }
    };
    ret(ctx, found);
    Ok(())
}

/// `int compare(size_t pos1, size_t n1, const char *s, size_t n2) const`
fn compare(symbol: &str, ctx: &mut RegisterContext, mem: &mut dyn GuestMemory) -> Result<()> {
    let c = Call::new(symbol, CHAR)?;
    let [this, pos1, n1, s, n2, _] = args(ctx);
    let content = c.at(this).load(mem, symbol)?;
    let size = content.len() as u64;
    if pos1 > size {
        return Err(c.out_of_range());
    }
    let rlen = n1.min(size - pos1);
    let theirs = mem.read_bytes(s, c.bounded(n2)? as usize, symbol)?;
    let common = rlen.min(n2) as usize;
    let mine = &content[pos1 as usize..pos1 as usize + common];
    // `char_traits<char>::compare` is `memcmp`, so the comparison is on
    // *unsigned* bytes whatever `char`'s signedness is.
    let mut r = match mine.cmp(&theirs[..common]) {
        core::cmp::Ordering::Less => -1i32,
        core::cmp::Ordering::Equal => 0,
        core::cmp::Ordering::Greater => 1,
    };
    if r == 0 {
        r = match rlen.cmp(&n2) {
            core::cmp::Ordering::Less => -1,
            core::cmp::Ordering::Equal => 0,
            core::cmp::Ordering::Greater => 1,
        };
    }
    crate::shims::set_int_return(ctx, r);
    Ok(())
}

// -- the two that return a string by value -----------------------------------

/// `basic_string operator+(const char *lhs, const basic_string &rhs)`
///
/// A twenty-four byte return is MEMORY to SysV AMD64, so the caller passed the
/// address to build into in `rdi` and expects it back in `rax`.
fn concat(symbol: &str, ctx: &mut RegisterContext, mem: &mut dyn GuestMemory) -> Result<()> {
    let c = Call::new(symbol, CHAR)?;
    let [out, lhs, rhs, ..] = args(ctx);
    let mut content = c.cstr(mem, lhs)?;
    content.extend_from_slice(&c.at(rhs).load(mem, symbol)?);
    c.init(mem, &c.at(out), &content)?;
    ret(ctx, out);
    Ok(())
}

/// `basic_string to_string(unsigned)` and `to_string(int)`, which differ only in
/// how the same thirty-two bits are read.
fn to_string_u32(symbol: &str, ctx: &mut RegisterContext, mem: &mut dyn GuestMemory) -> Result<()> {
    let [_, value, ..] = args(ctx);
    to_string_as(symbol, ctx, mem, (value as u32).to_string())
}

fn to_string_i32(symbol: &str, ctx: &mut RegisterContext, mem: &mut dyn GuestMemory) -> Result<()> {
    let [_, value, ..] = args(ctx);
    to_string_as(symbol, ctx, mem, (value as i32).to_string())
}

fn to_string_as(
    symbol: &str,
    ctx: &mut RegisterContext,
    mem: &mut dyn GuestMemory,
    text: String,
) -> Result<()> {
    let c = Call::new(symbol, CHAR)?;
    let [out, ..] = args(ctx);
    c.init(mem, &c.at(out), text.as_bytes())?;
    ret(ctx, out);
    Ok(())
}

// ------------------------------------------------------------- dispatch -----

/// The mangled prefix of every `basic_string<char>` member, non-`const`.
const CHAR_MEMBER: &str = "_ZNSt3__112basic_stringIcNS_11char_traitsIcEENS_9allocatorIcEEE";
/// The same, `const`-qualified — a different mangling, and half the surface.
const CHAR_CONST_MEMBER: &str = "_ZNKSt3__112basic_stringIcNS_11char_traitsIcEENS_9allocatorIcEEE";
/// The `wchar_t` instantiation.
const WCHAR_MEMBER: &str = "_ZNSt3__112basic_stringIwNS_11char_traitsIwEENS_9allocatorIwEEE";

/// `basic_string operator+<char>(const char *, const basic_string &)`.
const CONCAT: &str =
    "_ZNSt3__1plIcNS_11char_traitsIcEENS_9allocatorIcEEEENS_12basic_stringIT_T0_T1_EEPKS6_RKS9_";
/// `std::__1::to_string(unsigned)`.
const TO_STRING_U32: &str = "_ZNSt3__19to_stringEj";
/// `std::__1::to_string(int)`.
const TO_STRING_I32: &str = "_ZNSt3__19to_stringEi";

/// Whether `name` (underscore already stripped) is one of this module's.
pub fn shim_for(name: &str) -> Option<crate::jmpbuf::Shim> {
    if name == CONCAT {
        return Some(concat);
    }
    if name == TO_STRING_U32 {
        return Some(to_string_u32);
    }
    if name == TO_STRING_I32 {
        return Some(to_string_i32);
    }
    if let Some(member) = name
        .strip_prefix(CHAR_MEMBER)
        .or_else(|| name.strip_prefix(CHAR_CONST_MEMBER))
    {
        return char_member(member);
    }
    if let Some(member) = name.strip_prefix(WCHAR_MEMBER) {
        return wchar_member(member);
    }
    None
}

/// One `basic_string<char>` member, by the part of the mangling that names it.
///
/// Matched on the suffix rather than on whole symbols because the prefix is
/// sixty characters of `char_traits` and `allocator` that say nothing: spelling
/// it once is what makes the list below readable as the class's interface.
fn char_member(member: &str) -> Option<crate::jmpbuf::Shim> {
    Some(match member {
        "2atEm" => at,
        "4findEcm" => find_char,
        "5rfindEcm" => rfind_char,
        "7compareEmmPKcm" => compare,
        "5eraseEmm" => erase,
        "6__initEPKcm" => init_from_pointer,
        "6__initEmc" => {
            |s, c: &mut RegisterContext, m: &mut dyn GuestMemory| init_fill_as(s, c, m, CHAR)
        }
        "6appendEPKc" => append_cstr,
        "6appendEPKcm" => |s, c: &mut RegisterContext, m: &mut dyn GuestMemory| {
            append_counted_as(s, c, m, CHAR)
        },
        "6appendERKS5_mm" => append_substr,
        "6appendEmc" => append_fill,
        "6assignEPKc" => assign_cstr,
        "6assignEPKcm" => assign_counted,
        "6insertEmPKc" => insert_cstr,
        "6resizeEmc" => {
            |s, c: &mut RegisterContext, m: &mut dyn GuestMemory| resize_as(s, c, m, CHAR)
        }
        "7replaceEmmPKc" => replace_cstr,
        "7reserveEm" => reserve,
        "9__grow_byEmmmmmm" => grow_by,
        "9push_backEc" => {
            |s, c: &mut RegisterContext, m: &mut dyn GuestMemory| push_back_as(s, c, m, CHAR)
        }
        // `C1` is the complete object, `C2` the base subobject. `basic_string`
        // has no bases and no virtual anything, so the two are the same
        // function; a guest emits whichever its call site asked for.
        "C1ERKS5_" | "C2ERKS5_" => {
            |s, c: &mut RegisterContext, m: &mut dyn GuestMemory| copy_construct_as(s, c, m, CHAR)
        }
        "C1ERKS5_mmRKS4_" | "C2ERKS5_mmRKS4_" => {
            |s, c: &mut RegisterContext, m: &mut dyn GuestMemory| substr_construct_as(s, c, m, CHAR)
        }
        "D1Ev" | "D2Ev" => {
            |s, c: &mut RegisterContext, m: &mut dyn GuestMemory| destroy_as(s, c, m, CHAR)
        }
        "aSERKS5_" => assign_string,
        _ => return None,
    })
}

/// The `wchar_t` instantiation's members. A shorter list because a guest reaches
/// fewer of them, and every one here is the `char` implementation with a
/// different width rather than a second implementation.
fn wchar_member(member: &str) -> Option<crate::jmpbuf::Shim> {
    Some(match member {
        "6__initEmw" => {
            |s, c: &mut RegisterContext, m: &mut dyn GuestMemory| init_fill_as(s, c, m, WCHAR)
        }
        "6appendEPKwm" => |s, c: &mut RegisterContext, m: &mut dyn GuestMemory| {
            append_counted_as(s, c, m, WCHAR)
        },
        "6resizeEmw" => {
            |s, c: &mut RegisterContext, m: &mut dyn GuestMemory| resize_as(s, c, m, WCHAR)
        }
        "9push_backEw" => {
            |s, c: &mut RegisterContext, m: &mut dyn GuestMemory| push_back_as(s, c, m, WCHAR)
        }
        "C1ERKS5_" | "C2ERKS5_" => {
            |s, c: &mut RegisterContext, m: &mut dyn GuestMemory| copy_construct_as(s, c, m, WCHAR)
        }
        "C1ERKS5_mmRKS4_" | "C2ERKS5_mmRKS4_" => {
            |s, c: &mut RegisterContext, m: &mut dyn GuestMemory| substr_construct_as(s, c, m, WCHAR)
        }
        "D1Ev" | "D2Ev" => {
            |s, c: &mut RegisterContext, m: &mut dyn GuestMemory| destroy_as(s, c, m, WCHAR)
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The layout constants, against the values measured from a
    /// `cc -arch x86_64` build. `tests/cxxstring.rs` re-measures them; these are
    /// the arithmetic that has to reproduce them.
    #[test]
    fn char_layout_constants_match_libcxx() {
        assert_eq!(CHAR.min_cap(), 23);
        assert_eq!(CHAR.short_cap(), 22);
        assert_eq!(CHAR.short_data(), 1);
        assert_eq!(CHAR.boundary(), 8);
    }

    #[test]
    fn wchar_layout_constants_match_libcxx() {
        assert_eq!(WCHAR.min_cap(), 5);
        assert_eq!(WCHAR.short_cap(), 4);
        assert_eq!(WCHAR.short_data(), 4);
        assert_eq!(WCHAR.boundary(), 2);
    }

    /// `__recommend`, against `reserve(n); capacity()` measured on x86-64.
    ///
    /// The interesting entry is 23: the naive answer is 23, which is
    /// `__min_cap`, which would be stored as an odd `cap+1` and collide with the
    /// long/short flag. libc++ steps to 25, not 24.
    #[test]
    fn recommend_matches_libcxx_for_char() {
        for n in 0..=22 {
            assert_eq!(CHAR.recommend(n), 22, "recommend({n})");
        }
        for (n, want) in [
            (23, 25),
            (24, 31),
            (31, 31),
            (32, 39),
            (39, 39),
            (40, 47),
            (47, 47),
            (48, 55),
            (63, 63),
            (64, 71),
            (100, 103),
        ] {
            assert_eq!(CHAR.recommend(n), want, "recommend({n})");
        }
    }

    #[test]
    fn recommend_matches_libcxx_for_wchar() {
        for n in 0..=4 {
            assert_eq!(WCHAR.recommend(n), 4, "recommend({n})");
        }
        for (n, want) in [
            (5, 7),
            (6, 7),
            (7, 7),
            (8, 9),
            (9, 9),
            (10, 11),
            (16, 17),
            (32, 33),
        ] {
            assert_eq!(WCHAR.recommend(n), want, "recommend({n})");
        }
    }

    /// Every capacity a long string can hold must survive the round trip
    /// through the flag bit — which is the property `recommend`'s `+= 2` exists
    /// to guarantee, and the one a plain `+= 1` would break.
    #[test]
    fn a_recommended_capacity_survives_the_flag_bit() {
        for l in [CHAR, WCHAR] {
            for n in 0..4096u64 {
                let cap = l.recommend(n);
                if cap == l.short_cap() {
                    continue;
                }
                let stored = (cap + 1) | 1;
                assert_eq!(
                    (stored & !1u64) - 1,
                    cap,
                    "capacity {cap} for {n} does not read back"
                );
                assert!(cap >= n, "capacity {cap} is smaller than the requested {n}");
            }
        }
    }

    #[test]
    fn every_symbol_steamclient_imports_is_answered() {
        // Exactly the `basic_string` surface `nm -u` reports for
        // `steamclient.dylib`'s x86-64 slice, minus the leading underscore the
        // Mach-O adds and `shims::dispatch` strips.
        for name in [
            "_ZNKSt3__112basic_stringIcNS_11char_traitsIcEENS_9allocatorIcEEE2atEm",
            "_ZNKSt3__112basic_stringIcNS_11char_traitsIcEENS_9allocatorIcEEE4findEcm",
            "_ZNKSt3__112basic_stringIcNS_11char_traitsIcEENS_9allocatorIcEEE5rfindEcm",
            "_ZNKSt3__112basic_stringIcNS_11char_traitsIcEENS_9allocatorIcEEE7compareEmmPKcm",
            "_ZNSt3__112basic_stringIcNS_11char_traitsIcEENS_9allocatorIcEEE2atEm",
            "_ZNSt3__112basic_stringIcNS_11char_traitsIcEENS_9allocatorIcEEE5eraseEmm",
            "_ZNSt3__112basic_stringIcNS_11char_traitsIcEENS_9allocatorIcEEE6__initEPKcm",
            "_ZNSt3__112basic_stringIcNS_11char_traitsIcEENS_9allocatorIcEEE6appendEPKc",
            "_ZNSt3__112basic_stringIcNS_11char_traitsIcEENS_9allocatorIcEEE6appendEPKcm",
            "_ZNSt3__112basic_stringIcNS_11char_traitsIcEENS_9allocatorIcEEE6appendERKS5_mm",
            "_ZNSt3__112basic_stringIcNS_11char_traitsIcEENS_9allocatorIcEEE6appendEmc",
            "_ZNSt3__112basic_stringIcNS_11char_traitsIcEENS_9allocatorIcEEE6assignEPKc",
            "_ZNSt3__112basic_stringIcNS_11char_traitsIcEENS_9allocatorIcEEE6assignEPKcm",
            "_ZNSt3__112basic_stringIcNS_11char_traitsIcEENS_9allocatorIcEEE6insertEmPKc",
            "_ZNSt3__112basic_stringIcNS_11char_traitsIcEENS_9allocatorIcEEE6resizeEmc",
            "_ZNSt3__112basic_stringIcNS_11char_traitsIcEENS_9allocatorIcEEE7replaceEmmPKc",
            "_ZNSt3__112basic_stringIcNS_11char_traitsIcEENS_9allocatorIcEEE7reserveEm",
            "_ZNSt3__112basic_stringIcNS_11char_traitsIcEENS_9allocatorIcEEE9__grow_byEmmmmmm",
            "_ZNSt3__112basic_stringIcNS_11char_traitsIcEENS_9allocatorIcEEE9push_backEc",
            "_ZNSt3__112basic_stringIcNS_11char_traitsIcEENS_9allocatorIcEEEC1ERKS5_",
            "_ZNSt3__112basic_stringIcNS_11char_traitsIcEENS_9allocatorIcEEEC1ERKS5_mmRKS4_",
            "_ZNSt3__112basic_stringIcNS_11char_traitsIcEENS_9allocatorIcEEED1Ev",
            "_ZNSt3__112basic_stringIcNS_11char_traitsIcEENS_9allocatorIcEEED2Ev",
            "_ZNSt3__112basic_stringIcNS_11char_traitsIcEENS_9allocatorIcEEEaSERKS5_",
            "_ZNSt3__112basic_stringIwNS_11char_traitsIwEENS_9allocatorIwEEE6__initEmw",
            "_ZNSt3__112basic_stringIwNS_11char_traitsIwEENS_9allocatorIwEEE6appendEPKwm",
            "_ZNSt3__112basic_stringIwNS_11char_traitsIwEENS_9allocatorIwEEE6resizeEmw",
            "_ZNSt3__112basic_stringIwNS_11char_traitsIwEENS_9allocatorIwEEE9push_backEw",
            "_ZNSt3__112basic_stringIwNS_11char_traitsIwEENS_9allocatorIwEEEC1ERKS5_",
            "_ZNSt3__112basic_stringIwNS_11char_traitsIwEENS_9allocatorIwEEEC1ERKS5_mmRKS4_",
            "_ZNSt3__112basic_stringIwNS_11char_traitsIwEENS_9allocatorIwEEED1Ev",
            CONCAT,
            TO_STRING_U32,
            TO_STRING_I32,
        ] {
            assert!(shim_for(name).is_some(), "no shim for {name}");
            // `is_shimmed` and `dispatch` are handed the name as the *Mach-O*
            // spells it, which for a C++ mangling is one underscore more.
            let mach_o = format!("_{name}");
            assert!(
                crate::shims::is_shimmed(&mach_o),
                "{mach_o} is not reported shimmed"
            );
            assert_eq!(
                crate::abi::classify(0, &mach_o),
                crate::abi::Classification::Slow,
                "{mach_o} would not be serviced at an exit"
            );
        }
    }

    /// A name that is not a string member must not be captured by the prefix
    /// match — the failure mode of matching on a prefix at all.
    #[test]
    fn an_unrelated_symbol_is_not_claimed() {
        for name in [
            "_ZNSt3__112basic_stringIcNS_11char_traitsIcEENS_9allocatorIcEEE5c_strEv",
            "_ZNSt3__16vectorIiNS_9allocatorIiEEE9push_backERKi",
            "_ZNSt3__112__next_primeEm",
            "strlen",
        ] {
            assert!(shim_for(name).is_none(), "{name} was wrongly claimed");
        }
    }
}
