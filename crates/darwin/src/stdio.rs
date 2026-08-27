//! The three `FILE *` globals, which are the same object under a different name.
//!
//! Darwin spells them `__stdinp`/`__stdoutp`/`__stderrp` and the host spells them
//! `stdin`/`stdout`/`stderr`. Both are *pointer objects*: the symbol's address is a
//! `FILE **`, and the guest loads through it and hands the result to `fwrite`. `FILE`
//! itself never crosses the seam - the guest has no declaration of it and never reads
//! a field - so there is nothing to bridge beyond the name.
//!
//! Data rather than functions, which is why `rosetta3::abi::table` refuses to *call*
//! them and this module still resolves them: the two are different questions about the
//! same symbol.

use machish::resolve::SyntheticDylib;

/// Darwin's name for each stream, and the host's.
///
/// The Mach-O spelling carries one more underscore than the C identifier, which is
/// why the table is written in C names and [`crate::c_name`] has already run.
const STREAMS: &[(&str, &str)] = &[
    ("__stdinp", HOST_STDIN),
    ("__stdoutp", HOST_STDOUT),
    ("__stderrp", HOST_STDERR),
];

#[cfg(target_os = "macos")]
const HOST_STDIN: &str = "__stdinp";
#[cfg(target_os = "macos")]
const HOST_STDOUT: &str = "__stdoutp";
#[cfg(target_os = "macos")]
const HOST_STDERR: &str = "__stderrp";

#[cfg(not(target_os = "macos"))]
const HOST_STDIN: &str = "stdin";
#[cfg(not(target_os = "macos"))]
const HOST_STDOUT: &str = "stdout";
#[cfg(not(target_os = "macos"))]
const HOST_STDERR: &str = "stderr";

/// The address of the `FILE *` object `name` denotes, if this module knows it.
pub fn symbol(name: &str) -> Option<u64> {
    let (_, host) = STREAMS.iter().find(|(darwin, _)| *darwin == name)?;
    crate::host_symbol(host)
}

/// The host's `stdout`, as **this runtime's** own stream.
///
/// Distinct from [`symbol`], which answers a guest's import: this is the stream a shim
/// writes a guest's `printf` output to, and it is a `FILE *` rather than the address
/// of one. Null if nothing exported it, which every caller has to survive because a
/// stream is not something to abort over.
pub fn stdout() -> *mut libc::FILE {
    stream(HOST_STDOUT)
}

/// `stderr`, for the same reason.
pub fn stderr() -> *mut libc::FILE {
    stream(HOST_STDERR)
}

/// Load a `FILE *` out of the host's pointer object named `host`.
///
/// Resolved rather than declared as an `extern`, because the name is not the same on
/// both hosts and a link-time reference to the wrong one does not build.
fn stream(host: &str) -> *mut libc::FILE {
    let Some(at) = crate::host_symbol(host) else {
        return core::ptr::null_mut();
    };
    // SAFETY: the host exports this symbol as `FILE *`, so its address is a valid
    // `FILE **` for the life of the process, and libc initialises it before any user
    // code runs.
    unsafe { *(at as usize as *const *mut libc::FILE) }
}

pub fn provide(lib: &mut SyntheticDylib) {
    for (darwin, host) in STREAMS {
        // The Mach-O underscore, put back: the guest imports `___stdoutp`.
        crate::define_data(
            lib,
            &format!("_{darwin}"),
            crate::host_symbol(host).unwrap_or(0),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The address must be a `FILE **` whose contents are a live stream - which is
    /// the whole claim this module makes, and the one way the rename could be wrong
    /// is by naming a symbol that is a `FILE` rather than a pointer to one.
    #[test]
    fn each_stream_resolves_to_a_pointer_to_a_live_file() {
        for (darwin, _) in STREAMS {
            let at = symbol(darwin).unwrap_or_else(|| panic!("{darwin} resolves"));
            // SAFETY: the host exports this symbol as `FILE *`, so its address is a
            // valid `FILE **` for the life of the process.
            let stream = unsafe { *(at as usize as *const *mut libc::FILE) };
            assert!(!stream.is_null(), "{darwin} points at a live stream");
        }
    }

    /// `___stdoutp` really is the stream this process writes to, checked by asking
    /// for its descriptor rather than by comparing addresses.
    #[test]
    fn the_streams_are_the_ones_this_process_writes_to() {
        // SAFETY: `fileno` only reads a live stream's descriptor.
        unsafe {
            assert_eq!(libc::fileno(stdout()), 1);
            assert_eq!(libc::fileno(stderr()), 2);
        }
    }

    /// The guest's import and this runtime's own accessor must reach one stream: a
    /// shim's output and the guest's own must interleave the way they would natively.
    #[test]
    fn a_guest_s_stream_and_this_runtime_s_are_the_same_stream() {
        let at = symbol("__stdoutp").expect("stdout resolves");
        // SAFETY: `at` is the address of the host's `FILE *`.
        assert_eq!(unsafe { *(at as usize as *const *mut libc::FILE) }, stdout());
    }
}
