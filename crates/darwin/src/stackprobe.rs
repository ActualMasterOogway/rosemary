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

#[cfg(target_arch = "x86_64")]
exports::exports! {
    functions {
        "___chkstk_darwin" => chkstk,
    }
}

#[cfg(not(target_arch = "x86_64"))]
exports::exports! {}
