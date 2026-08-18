//! Starting the guest: initializers, then `main`.

use crate::vm::{Vm, VmError};

/// Run every initializer, in order.
///
/// Called with the same `(argc, argv, envp, apple)` as `main`:
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
    // so the first initializer is as sampled as anything an hour in.
    crate::profile::start();
    // And the overlay, which is the same figures where they can be read while
    // the program is running.
    crate::gui::install();
    say_image_map(vm);
    // Before any guest code, as dyld does it: `libobjc` realizes an image's
    // classes when the image is mapped, which is strictly before that image's
    // initializers run.
    let objc = vm.register_objc()?;
    // `+load` before the C constructors of the same image, which is dyld's
    // order and the one a program that uses both depends on. Resolved before
    // the guest can start a thread, let alone collect on one:
    crate::shims::machthread::warm();
    crate::objc::call_load_methods(vm, objc)?;
    run_initializers(vm)?;
    let entry = vm
        .program
        .entry()
        .ok_or(VmError::NoCode { pc: 0, from: None })?;
    let (argc, argv, envp, apple) = vm.main_args();
    // Which of the two entry-point encodings the root image used decides how it
    // is reached, and the difference is not cosmetic. An `LC_MAIN` entry is a C
    // function and takes its four arguments in registers.
    let status = if vm.program.entry_reads_stack() {
        let sp = vm
            .kernel_sp()
            .expect("an LC_UNIXTHREAD program starts on the stack its arguments were laid out on");
        vm.jump_to(
            entry,
            microlift::arch::x86_64::context::RegisterContext::new(),
            sp,
        )?
        .0
    } else {
        vm.call(entry, &[argc, argv, envp, apple])?
    };
    Ok((status & 0xff) as i32)
}

/// Where each image ended up, under `ROSETTA3_TRACE_VM`.
///
/// Every guest address this runtime reports is slid by whatever `mmap` chose that run,
/// and a fault inside guest code is reported by a debugger — or by the guest's own
/// crash handler — as a bare number.
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
