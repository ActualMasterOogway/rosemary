//! `imp_implementationWithBlock` and its two inverses.
//!
//! An IMP made from a block is called as `(self, _cmd, args...)` and has to call
//! the block's `invoke` as `(block, self, args...)`: the block replaces `_cmd`
//! and swaps places with `self`. Each IMP is therefore a small stub that knows
//! its block, and the stubs come in pairs of pages the way libobjc lays them
//! out: a writable page of block pointers followed by an executable page of
//! identical stubs, each loading the word exactly one page behind itself. The
//! code page is written once and never again, so no stub is ever executable and
//! writable at the same time, and handing out an IMP is only a data store.

// The C spellings are the interface; renaming them would only hide it.
#![allow(non_snake_case)]

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::class::Imp;
use crate::object::Id;

/// One slot's share of each page: the plain stub, then (on x86-64) the
/// struct-returning one, both reading the same data word.
const SLOT: usize = 32;
/// Where the struct-returning stub starts within a slot.
#[cfg(target_arch = "x86_64")]
const STRET: usize = 16;

struct Pages {
    /// The data page of every pair; its code page is the next one.
    bases: Vec<usize>,
    /// Code addresses of slots no IMP currently uses.
    free: Vec<usize>,
}

static PAGES: Mutex<Pages> = Mutex::new(Pages {
    bases: Vec::new(),
    free: Vec::new(),
});

fn page_size() -> usize {
    // SAFETY: no preconditions.
    unsafe { libc::sysconf(libc::_SC_PAGESIZE) as usize }
}

/// The stubs for one slot, given how far behind each stub its data word is.
#[cfg(target_arch = "x86_64")]
fn stub(page: usize) -> [u8; SLOT] {
    // Both loads end 10 bytes into their stub; rip-relative is from there.
    let disp = |at: usize| (-((page + at + 10) as i64) as i32).to_le_bytes();
    let mut code = [0xcc; SLOT];
    // mov rsi, rdi; mov rdi, [rip - page]; jmp [rdi + 16]
    code[..3].copy_from_slice(&[0x48, 0x89, 0xfe]);
    code[3..6].copy_from_slice(&[0x48, 0x8b, 0x3d]);
    code[6..10].copy_from_slice(&disp(0));
    code[10..13].copy_from_slice(&[0xff, 0x67, 0x10]);
    // The hidden struct pointer stays in rdi, so self moves from rsi to rdx:
    // mov rdx, rsi; mov rsi, [rip - page - 16]; jmp [rsi + 16]
    code[STRET..STRET + 3].copy_from_slice(&[0x48, 0x89, 0xf2]);
    code[STRET + 3..STRET + 6].copy_from_slice(&[0x48, 0x8b, 0x35]);
    code[STRET + 6..STRET + 10].copy_from_slice(&disp(STRET));
    code[STRET + 10..STRET + 13].copy_from_slice(&[0xff, 0x66, 0x10]);
    code
}

#[cfg(target_arch = "aarch64")]
fn stub(page: usize) -> [u8; SLOT] {
    // A literal load is pc-relative in words from the load itself, at +4.
    let imm19 = ((-((page + 4) as i64) / 4) as u32) & 0x7ffff;
    let words: [u32; 4] = [
        0xaa0003e1,                // mov x1, x0
        0x58000000 | (imm19 << 5), // ldr x0, [pc - page]
        0xf9400810,                // ldr x16, [x0, #16]
        0xd61f0200,                // br x16
    ];
    // An indirect struct return is x8 on arm64, so one stub serves both.
    let mut code = [0; SLOT];
    for (i, w) in words.iter().enumerate() {
        code[i * 4..i * 4 + 4].copy_from_slice(&w.to_le_bytes());
    }
    code
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
fn stub(_page: usize) -> [u8; SLOT] {
    crate::fail!("imp_implementationWithBlock: no trampoline for this architecture")
}

/// Map one more pair of pages and add its slots to the free list.
fn grow(pages: &mut Pages) {
    let page = page_size();
    // SAFETY: a fresh private anonymous mapping.
    let base = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            page * 2,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    if base == libc::MAP_FAILED {
        crate::fail!("imp_implementationWithBlock: cannot map a trampoline page");
    }
    let code = base as usize + page;
    let template = stub(page);
    for slot in (0..page).step_by(SLOT) {
        // SAFETY: inside the code page, which is still writable.
        unsafe { std::ptr::copy_nonoverlapping(template.as_ptr(), (code + slot) as *mut u8, SLOT) };
    }
    // SAFETY: the code page of the mapping above.
    // A hardened macOS process needs the unsigned-executable-memory entitlement
    // for this; MAP_JIT would not do, since the page is never rewritten.
    if unsafe { libc::mprotect(code as *mut _, page, libc::PROT_READ | libc::PROT_EXEC) } != 0 {
        crate::fail!("imp_implementationWithBlock: cannot make a trampoline page executable");
    }
    #[cfg(target_arch = "aarch64")]
    {
        unsafe extern "C" {
            fn __clear_cache(start: *mut libc::c_char, end: *mut libc::c_char);
        }
        // SAFETY: the range just written.
        unsafe { __clear_cache(code as *mut _, (code + page) as *mut _) };
    }
    pages.bases.push(base as usize);
    // Popped from the end, so hand out the first slot first.
    pages
        .free
        .extend((0..page).step_by(SLOT).rev().map(|slot| code + slot));
}

/// The data word behind `imp`, if `imp` is a stub this module made.
fn data_word(pages: &Pages, imp: usize) -> Option<&'static AtomicUsize> {
    let page = page_size();
    let base = *pages
        .bases
        .iter()
        .find(|&&base| (base + page..base + 2 * page).contains(&imp))?;
    let slot = (imp - base - page) / SLOT * SLOT;
    let within = imp - base - page - slot;
    #[cfg(target_arch = "x86_64")]
    let ok = within == 0 || within == STRET;
    #[cfg(not(target_arch = "x86_64"))]
    let ok = within == 0;
    // SAFETY: the data page lives as long as the process; it is never unmapped.
    ok.then(|| unsafe { &*((base + slot) as *const AtomicUsize) })
}

/// `IMP imp_implementationWithBlock(id block)`
pub unsafe extern "C" fn imp_implementationWithBlock(block: Id) -> Imp {
    if block.is_null() {
        return std::ptr::null();
    }
    let block = crate::block::copy(block);
    let mut pages = PAGES.lock().unwrap_or_else(|e| e.into_inner());
    if pages.free.is_empty() {
        grow(&mut pages);
    }
    let code = pages.free.pop().expect("grow added slots");
    let page = page_size();
    // SAFETY: the data word of a slot nobody else holds.
    unsafe { &*((code - page) as *const AtomicUsize) }.store(block as usize, Ordering::Release);
    #[cfg(target_arch = "x86_64")]
    // SAFETY: `block` is a block, now a heap one.
    if unsafe { crate::block::uses_stret(block) } {
        return (code + STRET) as Imp;
    }
    code as Imp
}

/// `id imp_getBlock(IMP imp)`
pub unsafe extern "C" fn imp_getBlock(imp: Imp) -> Id {
    let pages = PAGES.lock().unwrap_or_else(|e| e.into_inner());
    data_word(&pages, imp as usize)
        .map_or(std::ptr::null_mut(), |w| w.load(Ordering::Acquire) as Id)
}

/// `BOOL imp_removeBlock(IMP imp)`
pub unsafe extern "C" fn imp_removeBlock(imp: Imp) -> bool {
    let mut pages = PAGES.lock().unwrap_or_else(|e| e.into_inner());
    let Some(word) = data_word(&pages, imp as usize) else {
        return false;
    };
    let block = word.swap(0, Ordering::AcqRel) as Id;
    if block.is_null() {
        return false;
    }
    let page = page_size();
    pages.free.push((word as *const _ as usize) + page);
    drop(pages);
    crate::block::release(block);
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::c_void;

    /// A global block literal with one captured word; global, so neither the
    /// copy nor the release has anything to do.
    #[repr(C)]
    struct Literal {
        isa: *const c_void,
        flags: i32,
        reserved: i32,
        invoke: *const c_void,
        descriptor: *const c_void,
        captured: i64,
    }

    const IS_GLOBAL: i32 = 1 << 28;

    fn literal(invoke: *const c_void, flags: i32, captured: i64) -> Box<Literal> {
        Box::new(Literal {
            isa: std::ptr::null(),
            flags: IS_GLOBAL | flags,
            reserved: 0,
            invoke,
            descriptor: std::ptr::null(),
            captured,
        })
    }

    unsafe extern "C" fn add(block: *const Literal, receiver: i64, a: i64) -> i64 {
        unsafe { (*block).captured + receiver * 100 + a }
    }

    #[test]
    fn a_block_imp_passes_self_and_drops_the_selector() {
        let block = literal(add as *const c_void, 0, 7);
        let imp = unsafe { imp_implementationWithBlock(&*block as *const Literal as Id) };
        let call: unsafe extern "C" fn(i64, usize, i64) -> i64 =
            unsafe { std::mem::transmute(imp) };
        assert_eq!(unsafe { call(3, 0xdead, 5) }, 7 + 300 + 5);
        assert_eq!(
            unsafe { imp_getBlock(imp) },
            &*block as *const Literal as Id
        );
        assert!(unsafe { imp_removeBlock(imp) });
        assert!(unsafe { imp_getBlock(imp) }.is_null());
        assert!(!unsafe { imp_removeBlock(imp) });
    }

    #[test]
    fn foreign_imps_have_no_block() {
        let imp = add as Imp;
        assert!(unsafe { imp_getBlock(imp) }.is_null());
        assert!(!unsafe { imp_removeBlock(imp) });
    }

    #[test]
    fn many_imps_each_keep_their_own_block() {
        let blocks: Vec<_> = (0..1000)
            .map(|i| literal(add as *const c_void, 0, i))
            .collect();
        let imps: Vec<_> = blocks
            .iter()
            .map(|b| unsafe { imp_implementationWithBlock(&**b as *const Literal as Id) })
            .collect();
        for (i, imp) in imps.iter().enumerate() {
            let call: unsafe extern "C" fn(i64, usize, i64) -> i64 =
                unsafe { std::mem::transmute(*imp) };
            assert_eq!(unsafe { call(0, 0, 0) }, i as i64);
        }
        for imp in imps {
            assert!(unsafe { imp_removeBlock(imp) });
        }
    }

    #[repr(C)]
    #[derive(Debug, PartialEq)]
    struct Big([i64; 4]);

    unsafe extern "C" fn big(block: *const Literal, receiver: i64, a: i64) -> Big {
        Big([unsafe { (*block).captured }, receiver, a, 4])
    }

    #[test]
    fn a_struct_returning_block_imp_keeps_the_hidden_pointer_first() {
        let block = literal(big as *const c_void, crate::block::USE_STRET_FLAG, 1);
        let imp = unsafe { imp_implementationWithBlock(&*block as *const Literal as Id) };
        let call: unsafe extern "C" fn(i64, usize, i64) -> Big =
            unsafe { std::mem::transmute(imp) };
        assert_eq!(unsafe { call(2, 0xdead, 3) }, Big([1, 2, 3, 4]));
        assert_eq!(
            unsafe { imp_getBlock(imp) },
            &*block as *const Literal as Id
        );
        assert!(unsafe { imp_removeBlock(imp) });
    }
}
