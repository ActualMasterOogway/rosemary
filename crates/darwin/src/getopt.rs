#![cfg(target_os = "linux")]

use std::ffi::{CStr, c_char, c_int};
use std::sync::atomic::{AtomicPtr, Ordering};

unsafe extern "C" {
    static mut optind: c_int;
    static mut opterr: c_int;
    static mut optopt: c_int;
    static mut optarg: *mut c_char;
}

static mut OPTRESET: c_int = 0;

/// Where in the current argument the next option letter is, as BSD's `place`.
static PLACE: AtomicPtr<c_char> = AtomicPtr::new(std::ptr::null_mut());

const BADCH: c_int = b'?' as c_int;
const BADARG: c_int = b':' as c_int;

fn complain(what: &str, letter: c_int) {
    // SAFETY: the runtime's own NUL-terminated name for the program.
    let name = unsafe { CStr::from_ptr(crate::progname::getprogname()) };
    let text = format!(
        "{}: {what} -- {}\n",
        name.to_string_lossy(),
        letter as u8 as char
    );
    // SAFETY: a buffer of the length given; stderr is unbuffered on Darwin,
    // so writing it directly keeps the order the guest would see.
    unsafe { libc::write(libc::STDERR_FILENO, text.as_ptr().cast(), text.len()) };
}

/// `int getopt(int argc, char * const argv[], const char *optstring)`
///
/// # Safety
///
/// `argv` holds `argc` NUL-terminated strings and `optstring` is
/// NUL-terminated, as `getopt`'s own contract.
pub unsafe extern "C" fn getopt(
    argc: c_int,
    argv: *const *mut c_char,
    optstring: *const c_char,
) -> c_int {
    // SAFETY: the guest's globals and arguments, used the way BSD's `getopt`
    // uses them, on the guest's one thread of argument parsing.
    unsafe {
        let ostr = CStr::from_ptr(optstring).to_bytes();
        let mut place = PLACE.load(Ordering::Relaxed);
        let empty = place.is_null() || *place == 0;
        if OPTRESET != 0 || empty {
            OPTRESET = 0;
            if optind >= argc || optind < 0 {
                PLACE.store(std::ptr::null_mut(), Ordering::Relaxed);
                return -1;
            }
            place = *argv.add(optind as usize);
            if place.is_null() || *place != b'-' as c_char {
                // An operand, or the end: options are over.
                PLACE.store(std::ptr::null_mut(), Ordering::Relaxed);
                return -1;
            }
            place = place.add(1);
            optopt = *place as u8 as c_int;
            place = place.add(1);
            if optopt == b'-' as c_int && *place.sub(1) != 0 && *place == 0 {
                // "--" ends the options.
                optind += 1;
                PLACE.store(std::ptr::null_mut(), Ordering::Relaxed);
                return -1;
            }
            if optopt == 0 {
                // A lone "-": an option only for a program that asks for one.
                place = place.sub(1);
                if !ostr.contains(&b'-') {
                    PLACE.store(std::ptr::null_mut(), Ordering::Relaxed);
                    return -1;
                }
                optopt = b'-' as c_int;
            }
        } else {
            optopt = *place as u8 as c_int;
            place = place.add(1);
        }

        let wanted = (optopt != b':' as c_int)
            .then(|| ostr.iter().position(|b| *b as c_int == optopt))
            .flatten();
        let Some(at) = wanted else {
            if *place == 0 {
                optind += 1;
            }
            PLACE.store(place, Ordering::Relaxed);
            if opterr != 0 && ostr.first() != Some(&b':') {
                complain("illegal option", optopt);
            }
            return BADCH;
        };

        if ostr.get(at + 1) != Some(&b':') {
            optarg = std::ptr::null_mut();
            if *place == 0 {
                optind += 1;
            }
            PLACE.store(place, Ordering::Relaxed);
            return optopt;
        }
        if *place != 0 {
            // The rest of this argument is the option's.
            optarg = place;
        } else if ostr.get(at + 2) == Some(&b':') {
            // An optional argument, which is never the next one.
            optarg = std::ptr::null_mut();
        } else {
            optind += 1;
            if argc > optind {
                optarg = *argv.add(optind as usize);
            } else {
                PLACE.store(std::ptr::null_mut(), Ordering::Relaxed);
                if ostr.first() == Some(&b':') {
                    return BADARG;
                }
                if opterr != 0 {
                    complain("option requires an argument", optopt);
                }
                return BADCH;
            }
        }
        PLACE.store(std::ptr::null_mut(), Ordering::Relaxed);
        optind += 1;
        optopt
    }
}

exports::exports! {
    functions {
        "getopt" => getopt,
    }
    data {
        "optreset" => &raw const OPTRESET as usize as u64,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;

    /// Parse `args` with `optstring`, returning each answer with its
    /// argument, and the index the operands start at.
    fn parse(args: &[&str], optstring: &str) -> (Vec<(char, Option<String>)>, usize) {
        let owned: Vec<CString> = args.iter().map(|a| CString::new(*a).unwrap()).collect();
        let mut argv: Vec<*mut c_char> = owned.iter().map(|a| a.as_ptr().cast_mut()).collect();
        argv.push(std::ptr::null_mut());
        let optstring = CString::new(optstring).unwrap();
        let mut out = Vec::new();
        // SAFETY: the globals are this test's, one parse at a time.
        unsafe {
            optind = 1;
            OPTRESET = 1;
            opterr = 0;
            loop {
                optarg = std::ptr::null_mut();
                let c = getopt(args.len() as c_int, argv.as_ptr(), optstring.as_ptr());
                if c == -1 {
                    break;
                }
                let arg = (!optarg.is_null())
                    .then(|| CStr::from_ptr(optarg).to_string_lossy().into_owned());
                out.push((c as u8 as char, arg));
            }
            (out, optind as usize)
        }
    }

    /// One test: the globals are the process's, and tests run in parallel.
    #[test]
    fn options_parse_the_bsd_way() {
        // A lone "-" is su's login flag.
        assert_eq!(parse(&["su", "-"], "-flm"), (vec![('-', None)], 2));
        assert_eq!(parse(&["su", "-", "root"], "-flm"), (vec![('-', None)], 2));
        // Options stop at the first operand; nothing is permuted.
        assert_eq!(parse(&["su", "root", "-c", "id"], "-flm"), (vec![], 1));
        assert_eq!(parse(&["ls", "x", "-l"], "l"), (vec![], 1));
        // Without '-' in the string, a lone "-" is an operand.
        assert_eq!(parse(&["cat", "-"], "u"), (vec![], 1));
        // Clustered letters, arguments joined and apart, and "--".
        assert_eq!(
            parse(&["t", "-ab", "-cx", "-c", "y", "--", "-a"], "abc:"),
            (
                vec![
                    ('a', None),
                    ('b', None),
                    ('c', Some("x".into())),
                    ('c', Some("y".into()))
                ],
                6
            )
        );
        // Unknown letters and missing arguments.
        assert_eq!(parse(&["t", "-z"], "a"), (vec![('?', None)], 2));
        assert_eq!(parse(&["t", "-c"], ":c:"), (vec![(':', None)], 2));
        assert_eq!(parse(&["t", "-c"], "c:"), (vec![('?', None)], 2));
    }
}
