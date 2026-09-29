#[cfg(target_arch = "x86_64")]
#[unsafe(naked)]
unsafe extern "C" fn chkstk() {
    core::arch::naked_asm!(
        // `rsp` includes this helper's return address; the caller's future
        // stack begins eight bytes above it.
        "lea r10, [rsp + 8]",
        "mov r11, rax",
        "test r11, r11",
        "je 3f",
        "2:",
        "cmp r11, 4096",
        "jbe 3f",
        "sub r10, 4096",
        "test byte ptr [r10], al",
        "sub r11, 4096",
        "jmp 2b",
        "3:",
        "sub r10, r11",
        "test byte ptr [r10], al",
        "ret",
    );
}

#[cfg(target_arch = "aarch64")]
#[unsafe(naked)]
unsafe extern "C" fn chkstk() {
    core::arch::naked_asm!(
        // Darwin arm64 passes the number of bytes to probe in w9.
        //
        // Unlike x86-64, the return address lives in x30 rather than on the
        // stack, so SP already points at the caller's current stack boundary.
        //
        // x9 and x10 are scratch registers according to the special stack
        // probe calling convention. SP itself is not modified.
        "cmp w9, #4096",
        "mov x10, sp",
        "b.lo 3f",

        "2:",
        "sub x10, x10, #4096",
        "ldr xzr, [x10]",
        "sub w9, w9, #4096",
        "cmp w9, #4096",
        "b.hi 2b",

        "3:",
        "sub x10, x10, x9",
        "ldr xzr, [x10]",
        "ret",
    );
}

#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
exports::exports! {
    functions {
        "___chkstk_darwin" => chkstk,
    }
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
exports::exports! {}
