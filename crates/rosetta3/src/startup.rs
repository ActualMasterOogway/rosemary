//! Starting the guest: initializers, then `main`.
//!
//! ## What dyld would have done, and which parts are ours
//!
//! On a real launch, dyld maps the images, binds them, runs every image's
//! initializers, and calls the `LC_MAIN` entry point as
//! `int main(int argc, char **argv, char **envp, char **apple)`, handing the
//! result to `exit`. Milestone 1 did the mapping and the binding and collected
//! the initializers without calling them, because calling one means executing
//! guest code. This is where they are called.
//!
//! The C runtime's *own* setup is not reproduced and must not be: under
//! high-level emulation the guest's `libSystem` is the host's, already
//! initialized by this process's own launch. Running a guest copy of
//! `libSystem`'s initializers would initialize a second, unrelated runtime.
//!
//! ## Order
//!
//! Within an image, section order — the loader preserves it. Across images,
//! **dependencies before dependents**, which means walking the link report
//! backwards: it lists images in load order, root first, so the last-loaded
//! dependency is the first whose constructors must run. This is approximately
//! dyld's ordering rather than exactly it, for the reason `machish` states: the
//! loader walks the dependency graph breadth-first instead of reproducing
//! dyld's two-phase upward pass.
//!
//! ## Exit
//!
//! `main`'s return value is the process's exit status, and the guest's `atexit`
//! handlers run when the *host* exits, because `atexit` was forwarded to the
//! host's implementation with a thunk in place of the guest pointer. That is
//! the passthrough design working as intended: there is one exit-handler list,
//! it is the host's, and it contains entries that re-enter the JIT. It is also
//! the reason [`run`] returns rather than calling `exit` itself — the caller
//! must be able to drop the VM before the handlers fire, or not, deliberately.

use crate::vm::{Vm, VmError};

/// Run every initializer, in order.
///
/// Called with the same `(argc, argv, envp, apple)` as `main`: a macOS
/// constructor declared `__attribute__((constructor))` may take them, and one
/// that does not simply ignores four registers.
pub fn run_initializers(vm: &Vm) -> Result<usize, VmError> {
    let (argc, argv, envp, apple) = vm.main_args();
    let mut ran = 0;
    // Reverse load order: see the module note.
    let initializers: Vec<u64> = vm
        .program
        .report
        .images
        .iter()
        .rev()
        .flat_map(|image| image.initializers.iter().copied())
        .collect();
    for init in initializers {
        vm.call(init, &[argc, argv, envp, apple])?;
        ran += 1;
    }
    Ok(ran)
}

/// Run initializers and then the entry point, returning `main`'s status.
///
/// The status is truncated to the low eight bits the way `exit` truncates it,
/// so a guest returning `-1` reports 255 here exactly as it would have to the
/// shell.
pub fn run(vm: &Vm) -> Result<i32, VmError> {
    // Before anything the guest can fault in, which is everything below. The
    // image map goes with it: a fault reports a guest address, and an address
    // with no image name is a number.
    crate::fault::install();
    crate::fault::note_images(&vm.program.report);
    // Before any framework can ask a question about the main executable and get
    // an answer about *this* binary rather than the guest. See [`crate::compat`].
    crate::compat::install();
    // Beside the fault reporter and for the same reason: before any guest code,
    // so the first initializer is as sampled as anything an hour in. A
    // start-up that loads forty thousand shaders is a thing worth seeing the
    // shape of. See [`crate::profile`].
    crate::profile::start();
    // And the overlay, which is the same figures where they can be read while
    // the program is running. Installed here rather than lazily because what it
    // installs is a *key binding*, and a binding that appears once the guest has
    // drawn its first frame is one that does not work during a slow start-up —
    // which is a window worth looking at. See [`crate::gui`].
    crate::gui::install();
    say_image_map(vm);
    // Before any guest code, as dyld does it: `libobjc` realizes an image's
    // classes when the image is mapped, which is strictly before that image's
    // initializers run. A constructor may reference a class, and a `+load` --
    // which is itself a method -- certainly does.
    let objc = vm.register_objc()?;
    // `+load` before the C constructors of the same image, which is dyld's
    // order and the one a program that uses both depends on.
    // Resolved before the guest can start a thread, let alone collect on one:
    // see [`crate::shims::machthread::warm`] for why on demand is too late.
    crate::shims::machthread::warm();
    crate::objc::call_load_methods(vm, objc)?;
    run_initializers(vm)?;
    let entry = vm
        .program
        .entry()
        .ok_or(VmError::NoCode { pc: 0, from: None })?;
    let (argc, argv, envp, apple) = vm.main_args();
    let status = vm.call(entry, &[argc, argv, envp, apple])?;
    Ok((status & 0xff) as i32)
}

/// Where each image ended up, under `ROSETTA3_TRACE_VM`.
///
/// Every guest address this runtime reports is slid by whatever `mmap` chose
/// that run, and a fault inside guest code is reported by a debugger — or by
/// the guest's own crash handler — as a bare number. This is the table that
/// turns one back into `image+offset`, printed before any guest code runs so
/// it is above whatever the failure prints.
fn say_image_map(vm: &Vm) {
    if !crate::shims::vmmap::tracing() {
        return;
    }
    for image in &vm.program.report.images {
        for seg in &image.segments {
            if seg.vmsize == 0 {
                continue;
            }
            eprintln!(
                "rosetta3: image {:#x}..{:#x} {} {} (slide {:#x})",
                seg.vmaddr,
                seg.vmaddr.wrapping_add(seg.vmsize),
                seg.name,
                image.name.rsplit('/').next().unwrap_or(&image.name),
                image.slide,
            );
        }
    }
}
