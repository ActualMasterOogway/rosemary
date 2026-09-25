/// What a call left behind: the four registers a return value can arrive in.
#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct Returned {
    pub rax: u64,
    pub rdx: u64,
    /// The low eight bytes of `xmm0`, which is where a `float` or `double`
    /// return lands.
    pub xmm0: u64,
    pub xmm1: u64,
}

/// The arguments of one call, sorted into the places the ABI puts them.
#[derive(Default)]
pub struct Args {
    ints: [u64; 6],
    nint: usize,
    sses: [u64; 8],
    nsse: usize,
    stack: Vec<u64>,
}

impl Args {
    /// One general-purpose register's worth.
    pub fn int(&mut self, value: u64) {
        match self.nint < self.ints.len() {
            true => {
                self.ints[self.nint] = value;
                self.nint += 1;
            }
            false => self.stack.push(value),
        }
    }

    /// One vector register's worth: a `float` in the low four bytes, or a
    /// `double`.
    pub fn sse(&mut self, value: u64) {
        match self.nsse < self.sses.len() {
            true => {
                self.sses[self.nsse] = value;
                self.nsse += 1;
            }
            false => self.stack.push(value),
        }
    }

    /// A 16-byte aggregate of two INTEGER eightbytes.
    ///
    /// All or nothing: an aggregate that does not fit in the registers left
    /// goes to the stack whole, rather than being split across the boundary.
    pub fn int_pair(&mut self, low: u64, high: u64) {
        if self.nint + 2 <= self.ints.len() {
            self.int(low);
            self.int(high);
            return;
        }
        self.nint = self.ints.len();
        self.stack.push(low);
        self.stack.push(high);
    }

    /// An aggregate too large for registers, passed as a stack copy.
    pub fn memory(&mut self, bytes: &[u8]) {
        for word in bytes.chunks(8) {
            let mut buf = [0u8; 8];
            buf[..word.len()].copy_from_slice(word);
            self.stack.push(u64::from_le_bytes(buf));
        }
    }

    /// Call `func` with these arguments.
    ///
    /// # Safety
    ///
    /// `func` must be a live function whose signature is the one these
    /// arguments were built for. Nothing here can check that: the signature
    /// came out of the dispatch table and the address out of the host client's
    /// vtable.
    pub unsafe fn call(&self, func: *const core::ffi::c_void) -> Returned {
        let mut out = Returned::default();
        // SAFETY: the caller states the signature matches. The four pointers
        // are to live storage of the sizes the trampoline reads: six words,
        // eight words, `stack.len()` words, and one `Returned`.
        unsafe {
            call_raw(
                func,
                self.ints.as_ptr(),
                self.sses.as_ptr(),
                self.stack.as_ptr(),
                self.stack.len(),
                &mut out,
            );
        }
        out
    }
}

/// Place the registers and the stack arguments, call, and report what came back.
///
/// `al` is set to eight because a variadic callee reads it as the number of
/// vector registers in use, and a non-variadic one ignores it - neither the
/// dispatch table nor the SDK headers say which a given slot is.
#[cfg(all(target_arch = "x86_64", not(target_os = "macos")))]
#[unsafe(naked)]
unsafe extern "C" fn call_raw(
    func: *const core::ffi::c_void,
    ints: *const u64,
    sses: *const u64,
    stack: *const u64,
    nstack: usize,
    out: *mut Returned,
) {
    core::arch::naked_asm!(
        "push rbp",
        "mov rbp, rsp",
        // Four saved registers, so that the stack is 16-byte aligned here and
        // stays so once the argument area below is subtracted.
        "push rbx",
        "push r12",
        "push r13",
        "push r14",
        "mov rbx, r9",
        "mov r12, rsi",
        "mov r13, rdx",
        "mov r14, rdi",
        // The argument area, rounded up to a whole number of 16-byte units.
        "lea rax, [r8 + 1]",
        "and rax, -2",
        "shl rax, 3",
        "sub rsp, rax",
        "xor rax, rax",
        "2:",
        "cmp rax, r8",
        "jae 3f",
        "mov r10, [rcx + rax * 8]",
        "mov [rsp + rax * 8], r10",
        "inc rax",
        "jmp 2b",
        "3:",
        "movsd xmm0, qword ptr [r13]",
        "movsd xmm1, qword ptr [r13 + 8]",
        "movsd xmm2, qword ptr [r13 + 16]",
        "movsd xmm3, qword ptr [r13 + 24]",
        "movsd xmm4, qword ptr [r13 + 32]",
        "movsd xmm5, qword ptr [r13 + 40]",
        "movsd xmm6, qword ptr [r13 + 48]",
        "movsd xmm7, qword ptr [r13 + 56]",
        "mov rdi, [r12]",
        "mov rsi, [r12 + 8]",
        "mov rdx, [r12 + 16]",
        "mov rcx, [r12 + 24]",
        "mov r8, [r12 + 32]",
        "mov r9, [r12 + 40]",
        "mov eax, 8",
        "call r14",
        "mov [rbx], rax",
        "mov [rbx + 8], rdx",
        "movsd qword ptr [rbx + 16], xmm0",
        "movsd qword ptr [rbx + 24], xmm1",
        "lea rsp, [rbp - 32]",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop rbx",
        "pop rbp",
        "ret",
    )
}

// The bridge calls a Linux x86-64 Steam client using the System V ABI. It is
// unavailable on macOS (and on non-x86 hosts), but the shared module still
// needs this symbol to type-check on every target.
#[cfg(any(not(target_arch = "x86_64"), target_os = "macos"))]
unsafe extern "C" fn call_raw(
    _func: *const core::ffi::c_void,
    _ints: *const u64,
    _sses: *const u64,
    _stack: *const u64,
    _nstack: usize,
    out: *mut Returned,
) {
    // The Steam host client is never available for this target.
    unsafe { out.write(Returned::default()) };
}

#[cfg(all(test, target_arch = "x86_64"))]
mod tests {
    use super::*;

    /// More integer arguments than there are registers, so the last two are the
    /// stack half of the call.
    unsafe extern "C" fn eight_ints(
        a: u64,
        b: u64,
        c: u64,
        d: u64,
        e: u64,
        f: u64,
        g: u64,
        h: u64,
    ) -> u64 {
        a + b * 2 + c * 3 + d * 4 + e * 5 + f * 6 + g * 7 + h * 8
    }

    unsafe extern "C" fn mixed(a: u64, x: f64, b: u32, y: f32) -> f64 {
        a as f64 + x * 2.0 + b as f64 * 3.0 + y as f64 * 4.0
    }

    #[repr(C)]
    #[derive(Clone, Copy, PartialEq, Debug)]
    struct Pair {
        a: i32,
        b: f32,
        c: f32,
        d: bool,
    }

    /// The `InputAnalogActionData_t` shape: 16 bytes, two INTEGER eightbytes in
    /// and two registers out.
    unsafe extern "C" fn round_trip_pair(before: u64, p: Pair) -> Pair {
        assert_eq!(before, 0x1234);
        Pair {
            a: p.a + 1,
            b: p.b + 1.0,
            c: p.c + 1.0,
            d: !p.d,
        }
    }

    #[repr(C)]
    #[derive(Clone, Copy, PartialEq, Debug)]
    struct Big {
        v: [f32; 10],
    }

    /// The `InputMotionData_t` shape: forty bytes, so a stack copy in and a
    /// hidden pointer out.
    unsafe extern "C" fn round_trip_big(b: Big) -> Big {
        let mut out = b;
        out.v[9] += 1.0;
        out
    }

    fn call_of(f: *const core::ffi::c_void, args: &Args) -> Returned {
        // SAFETY: each test builds the arguments the named function declares.
        unsafe { args.call(f) }
    }

    #[test]
    fn spills_integer_arguments_past_the_sixth_to_the_stack() {
        let mut args = Args::default();
        for n in 1..=8u64 {
            args.int(n);
        }
        let got = call_of(eight_ints as *const _, &args);
        assert_eq!(got.rax, 1 + 4 + 9 + 16 + 25 + 36 + 49 + 64);
    }

    #[test]
    fn puts_floating_arguments_in_vector_registers() {
        let mut args = Args::default();
        args.int(1);
        args.sse(2.5f64.to_bits());
        args.int(3);
        args.sse(0.5f32.to_bits() as u64);
        let got = call_of(mixed as *const _, &args);
        assert_eq!(f64::from_bits(got.xmm0), 1.0 + 5.0 + 9.0 + 2.0);
    }

    #[test]
    fn passes_and_returns_a_sixteen_byte_aggregate_in_two_registers() {
        let value = Pair {
            a: 7,
            b: 1.5,
            c: 2.5,
            d: false,
        };
        // SAFETY: `Pair` is 16 bytes of plain data with no padding to read.
        let words: [u64; 2] = unsafe { core::mem::transmute(value) };
        let mut args = Args::default();
        args.int(0x1234);
        args.int_pair(words[0], words[1]);
        let got = call_of(round_trip_pair as *const _, &args);
        // SAFETY: the two return registers hold the aggregate, in order.
        let back: Pair = unsafe { core::mem::transmute([got.rax, got.rdx]) };
        assert_eq!(
            back,
            Pair {
                a: 8,
                b: 2.5,
                c: 3.5,
                d: true
            }
        );
    }

    /// The aggregate must not be split across the register/stack boundary: with
    /// five integers already placed, both of its eightbytes go to the stack.
    #[test]
    fn keeps_an_aggregate_whole_when_only_one_register_is_left() {
        unsafe extern "C" fn five_then_pair(
            a: u64,
            b: u64,
            c: u64,
            d: u64,
            e: u64,
            p: Pair,
        ) -> u64 {
            assert_eq!((a, b, c, d, e), (1, 2, 3, 4, 5));
            p.a as u64
        }
        let value = Pair {
            a: 99,
            b: 0.0,
            c: 0.0,
            d: false,
        };
        let words: [u64; 2] = unsafe { core::mem::transmute(value) };
        let mut args = Args::default();
        for n in 1..=5u64 {
            args.int(n);
        }
        args.int_pair(words[0], words[1]);
        assert_eq!(call_of(five_then_pair as *const _, &args).rax, 99);
    }

    #[test]
    fn passes_a_large_aggregate_on_the_stack_and_returns_it_through_a_pointer() {
        let value = Big {
            v: [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0],
        };
        // SAFETY: `Big` is forty bytes of plain data.
        let bytes: [u8; 40] = unsafe { core::mem::transmute(value) };
        let mut out = Big { v: [0.0; 10] };
        let mut args = Args::default();
        args.int(&mut out as *mut Big as usize as u64);
        args.memory(&bytes);
        let got = call_of(round_trip_big as *const _, &args);
        assert_eq!(out.v[9], 11.0);
        // And the hidden pointer comes back in `rax`, as the ABI requires.
        assert_eq!(got.rax, &mut out as *mut Big as usize as u64);
    }

    /// An odd number of stack words still has to leave the stack 16-byte
    /// aligned at the call, or a callee using aligned vector stores faults.
    #[test]
    fn keeps_the_stack_aligned_for_an_odd_number_of_stack_words() {
        unsafe extern "C" fn seven(a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) -> u64 {
            let mut v = [0.0f64; 2];
            // A 16-byte aligned store, which is what a misaligned stack breaks.
            v[0] = a as f64;
            v[1] = g as f64;
            (v[0] + v[1]) as u64 + b + c + d + e + f
        }
        let mut args = Args::default();
        for n in 1..=7u64 {
            args.int(n);
        }
        assert_eq!(call_of(seven as *const _, &args).rax, 28);
    }
}
