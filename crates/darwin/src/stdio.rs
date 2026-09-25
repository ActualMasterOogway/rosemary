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

/// The host's `stdout`, as **this runtime's** own stream.
///
/// Distinct from the published symbol, which answers a guest's import: this is the
/// stream a shim writes a guest's `printf` output to, and it is a `FILE *` rather
/// than the address of one. Null if nothing exported it, which every caller has to
/// survive because a stream is not something to abort over.
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

exports::exports! {
    // The Mach-O underscore is added here; the C identifiers carry one of their
    // own, so the guest imports `___stdoutp`. Constants because the address is
    // the host stream's, fixed for the process - so the loader and a `dlsym`
    // asking separately get the same one.
    constants {
        "__stdinp" => crate::host_symbol(HOST_STDIN).unwrap_or(0),
        "__stdoutp" => crate::host_symbol(HOST_STDOUT).unwrap_or(0),
        "__stderrp" => crate::host_symbol(HOST_STDERR).unwrap_or(0),
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
        for darwin in names() {
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
        assert_eq!(
            unsafe { *(at as usize as *const *mut libc::FILE) },
            stdout()
        );
    }
}
