//! The Mach thread-state layouts, checked against the x86-64 SDK headers.
//!
//! [`rosetta3::shims::machthread`] writes an `x86_thread_state64_t` and an
//! `x86_float_state64_t` into guest memory from constants — a flavour number, a
//! word count, and the offset of `__fpu_xmm0`. Nothing on this machine can
//! verify those by inspection: the host's own `<mach/thread_status.h>`
//! describes **arm64**, and asking it about `x86_thread_state64_t` is a
//! compile error rather than a wrong answer.
//!
//! So the fixture is built with `-arch x86_64`, which is the same trick every
//! other cross-architecture claim in this suite rests on, and it prints what
//! the headers say. A constant copied from the wrong architecture — or from
//! memory — fails here rather than as a collector reading a stack pointer out
//! of padding.

mod common;

/// The constants under test, mirrored from the shim.
///
/// Duplicated deliberately: a test that imported them could only check they
/// equal themselves. These are the numbers the module claims, written out
/// again, and the fixture is the independent authority.
const X86_THREAD_STATE64: u32 = 4;
const X86_THREAD_STATE64_COUNT: u32 = 42;
const X86_FLOAT_STATE64: u32 = 5;
const X86_FLOAT_STATE64_COUNT: u32 = 131;
const FPU_XMM0: usize = 168;
const FPU_MXCSR: usize = 32;

const SRC: &str = r#"
#include <stdio.h>
#include <stddef.h>
#include <mach/mach.h>
#include <mach/i386/thread_status.h>
int main(void) {
    printf("thread_flavor %d\n", x86_THREAD_STATE64);
    printf("thread_count %d\n", x86_THREAD_STATE64_COUNT);
    printf("float_flavor %d\n", x86_FLOAT_STATE64);
    printf("float_count %d\n", x86_FLOAT_STATE64_COUNT);
    printf("xmm0 %zu\n", offsetof(x86_float_state64_t, __fpu_xmm0));
    printf("mxcsr %zu\n", offsetof(x86_float_state64_t, __fpu_mxcsr));
    /* The register order the shim writes, as offsets. A transposition of two
       neighbours is the failure this catches, and it is invisible in a size. */
    printf("rax %zu\n", offsetof(x86_thread_state64_t, __rax));
    printf("rbx %zu\n", offsetof(x86_thread_state64_t, __rbx));
    printf("rcx %zu\n", offsetof(x86_thread_state64_t, __rcx));
    printf("rdx %zu\n", offsetof(x86_thread_state64_t, __rdx));
    printf("rdi %zu\n", offsetof(x86_thread_state64_t, __rdi));
    printf("rsi %zu\n", offsetof(x86_thread_state64_t, __rsi));
    printf("rbp %zu\n", offsetof(x86_thread_state64_t, __rbp));
    printf("rsp %zu\n", offsetof(x86_thread_state64_t, __rsp));
    printf("r8 %zu\n", offsetof(x86_thread_state64_t, __r8));
    printf("r15 %zu\n", offsetof(x86_thread_state64_t, __r15));
    printf("rip %zu\n", offsetof(x86_thread_state64_t, __rip));
    printf("size %zu\n", sizeof(x86_thread_state64_t));
    return 0;
}
"#;

#[test]
fn the_x86_thread_state_constants_are_the_headers_own() {
    let Some(path) = common::build_x86_64("machthread_layout", SRC, &[]) else {
        skip!("cannot build x86_64 fixtures")
    };
    let Some((status, out)) = common::run_native(&path) else {
        skip!("cannot run an x86_64 binary on this host")
    };
    if status != Some(0) {
        skip!("the layout fixture did not run on this host")
    }

    let value = |key: &str| -> u64 {
        out.lines()
            .find_map(|l| l.strip_prefix(&format!("{key} ")))
            .unwrap_or_else(|| panic!("fixture printed no {key}: {out}"))
            .trim()
            .parse()
            .expect("a number")
    };

    assert_eq!(value("thread_flavor"), u64::from(X86_THREAD_STATE64));
    assert_eq!(value("thread_count"), u64::from(X86_THREAD_STATE64_COUNT));
    assert_eq!(value("float_flavor"), u64::from(X86_FLOAT_STATE64));
    assert_eq!(value("float_count"), u64::from(X86_FLOAT_STATE64_COUNT));
    assert_eq!(value("xmm0"), FPU_XMM0 as u64);
    assert_eq!(value("mxcsr"), FPU_MXCSR as u64);

    // The shim writes twenty-one doublewords in this order; each register's
    // offset must be its index times eight.
    for (index, name) in [
        (0, "rax"),
        (1, "rbx"),
        (2, "rcx"),
        (3, "rdx"),
        (4, "rdi"),
        (5, "rsi"),
        (6, "rbp"),
        (7, "rsp"),
        (8, "r8"),
        (15, "r15"),
        (16, "rip"),
    ] {
        assert_eq!(value(name), index * 8, "{name} is not slot {index}");
    }

    // Twenty-one doublewords is the whole structure, so nothing the shim leaves
    // zero sits outside it.
    assert_eq!(value("size"), 21 * 8);
    assert_eq!(value("size"), u64::from(X86_THREAD_STATE64_COUNT) * 4);
}
