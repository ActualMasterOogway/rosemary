//! Fixture plumbing for the driver's tests.
//!
//! Mirrors `machish/tests/common/mod.rs`, with one difference that matters:
//! [`Program::load`] takes a *path*, because `@executable_path` expansion is
//! part of linking, so these return where the fixture was written rather than
//! its bytes.
//!
//! Milestone 1's acceptance test uses `/bin/ls`. Milestone 2 uses a fixture we
//! compile, because the ABI question is about *which symbols a program calls*
//! and a locally built program lets the test state exactly that — and because
//! `/bin/ls` embeds a jump table in `__text` that two of its functions do not
//! lift past, which would put an unrelated microlift gap in the middle of an
//! ABI test.

#![allow(dead_code)]

use std::path::PathBuf;
use std::process::Command;

/// Where compiled fixtures are cached.
fn fixture_dir() -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("fixtures");
    std::fs::create_dir_all(&dir).expect("create fixture dir");
    dir
}

/// Whether this host can build x86_64 Mach-O binaries.
pub fn can_build_x86_64() -> bool {
    static OK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *OK.get_or_init(|| {
        let dir = fixture_dir();
        let src = dir.join("probe.c");
        if std::fs::write(&src, "int main(void){return 0;}").is_err() {
            return false;
        }
        Command::new("cc")
            .args(["-arch", "x86_64", "-o"])
            .arg(dir.join("probe"))
            .arg(&src)
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    })
}

/// Compile `source` into an x86_64 Mach-O and return its path.
pub fn build_x86_64(name: &str, source: &str, extra: &[&str]) -> Option<String> {
    if !can_build_x86_64() {
        return None;
    }
    let dir = fixture_dir();
    let src = dir.join(format!("{name}.c"));
    let out = dir.join(name);
    std::fs::write(&src, source).ok()?;

    let status = Command::new("cc")
        .args(["-arch", "x86_64"])
        .args(extra)
        .arg("-o")
        .arg(&out)
        .arg(&src)
        .status()
        .ok()?;
    status.success().then(|| out.to_string_lossy().into_owned())
}

/// The milestone 2 program: it prints, formats, and registers a callback, so
/// each of the three ways an import can be classified is exercised by a real
/// program rather than only by the table's own tests.
///
/// Deliberately *not* `int main(){puts("hi");}`:
///
/// - `printf`/`snprintf` are **variadic**, the part of the seam with no general
///   mechanism (and, at default `-D_FORTIFY_SOURCE`, they are imported under
///   their `___chk` names, which is what a real guest looks like).
/// - `atexit` takes a **guest function pointer**, so it must be refused until
///   the host→guest thunk exists in M3.
/// - `strlen`, `abs`, `puts` are the ordinary fast-callable case.
///
/// No `<math.h>`, and the `%f` argument is a constant: `sqrt` compiles to
/// `sqrtsd` and `argc * 1.5` to `cvtsi2sd`, neither of which microlift lifts
/// yet. A lifting gap inside an ABI fixture would be a red suite about the
/// wrong thing — the gap is real and it belongs to the recompiler.
pub const HELLO_C: &str = r#"
#include <stdio.h>
#include <string.h>
#include <stdlib.h>

static void say_goodbye(void) { puts("goodbye"); }

int main(int argc, char **argv) {
    const char *who = argc > 1 ? argv[1] : "world";
    char buf[64];
    atexit(say_goodbye);
    snprintf(buf, sizeof buf, "%s (%zu)", who, strlen(who));
    printf("Hello, %s! %d %.2f\n", buf, argc, 1.5);
    puts("done");
    return abs(argc) - 1;
}
"#;

/// Build [`HELLO_C`], or `None` if this host cannot.
///
/// Built once per test binary: `cargo test` runs the tests in one binary
/// concurrently, and two of them compiling to the same path means one reads the
/// file while the other is writing it.
pub fn hello() -> Option<String> {
    static BUILT: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    BUILT.get_or_init(|| build_x86_64("hello", HELLO_C, &[])).clone()
}



/// Milestone 3's callback program: a guest comparator called by the *host's*
/// `qsort`, and a guest handler called by the host's `exit`.
///
/// Both are host→guest calls, and both are falsifiable: a comparator that is
/// never really called leaves the array unsorted, and a thunk that is called
/// with the wrong arguments sorts it wrongly. `atexit` additionally pins down
/// *when* — its output has to come after `main`'s.
pub const QSORT_C: &str = r#"
#include <stdio.h>
#include <stdlib.h>

static int calls = 0;

static int by_int(const void *a, const void *b) {
    calls++;
    int x = *(const int *)a, y = *(const int *)b;
    return (x > y) - (x < y);
}

static void bye(void) { puts("bye"); }

int main(void) {
    int v[8] = {5, 3, 9, 1, 7, 2, 8, 4};
    atexit(bye);
    qsort(v, 8, sizeof(int), by_int);
    for (int i = 0; i < 8; i++) printf("%d", v[i]);
    printf(" calls=%s\n", calls > 0 ? "yes" : "no");
    return 0;
}
"#;

/// Milestone 3's initializer program.
///
/// Three constructors with explicit priorities, so the test can check the order
/// rather than only that they ran, and a `main` that observes their combined
/// effect — which is the part that proves they ran *before* it.
pub const CTOR_C: &str = r#"
#include <stdio.h>

static char order[8];
static int n = 0;

__attribute__((constructor(101))) static void first(void)  { order[n++] = 'a'; }
__attribute__((constructor(102))) static void second(void) { order[n++] = 'b'; }
__attribute__((constructor(103))) static void third(void)  { order[n++] = 'c'; }

int main(void) {
    order[n] = 0;
    printf("order=%s\n", order);
    return n == 3 ? 0 : 1;
}
"#;

/// Milestone 4's thread program, written so each claim can fail on its own.
///
/// - Four threads each increment a `_Thread_local` ten times and print what
///   they see. Four independent counts is the whole thread-local claim; one
///   shared block would print 10/20/30/40 in some order.
/// - `main` prints its *own* counter afterwards. A runtime that gave the main
///   thread a spawned thread's block would print something other than zero.
/// - Each start routine returns a value derived from both its counter and its
///   argument, and `pthread_join` is asserted to deliver it — the argument
///   proves the thunk passed `rdi` through, the counter proves it ran on the
///   right block.
///
/// The output is deliberately order-independent per line but not per set: the
/// threads race, so the test sorts.
pub const THREAD_C: &str = r#"
#include <pthread.h>
#include <stdio.h>

_Thread_local int counter = 0;

static void *worker(void *arg) {
    long id = (long)arg;
    for (int i = 0; i < 10; i++) counter++;
    printf("thread %ld counter %d\n", id, counter);
    return (void *)(long)(counter * 100 + id);
}

int main(void) {
    pthread_t t[4];
    for (long i = 0; i < 4; i++) pthread_create(&t[i], 0, worker, (void *)i);
    for (long i = 0; i < 4; i++) {
        void *r = 0;
        pthread_join(t[i], &r);
        printf("joined %ld -> %ld\n", i, (long)r);
    }
    printf("main counter %d\n", counter);
    return 0;
}
"#;

/// Milestone 4's `errno` program.
///
/// `errno` is the case where passthrough is *correct* rather than merely
/// unimplemented: it is `__error()` returning a thread-local pointer, and a
/// guest "syscall" is really the host's libSystem call on the same host thread —
/// so the two agree without any work. What this checks is that they really do,
/// including that a failure on a spawned thread does not disturb the main
/// thread's value.
pub const ERRNO_C: &str = r#"
#include <pthread.h>
#include <stdio.h>
#include <errno.h>
#include <fcntl.h>
#include <string.h>

static void *worker(void *arg) {
    (void)arg;
    int fd = open("/nonexistent/definitely/not/here", O_RDONLY);
    printf("worker fd=%d errno=%d (%s)\n", fd, errno, strerror(errno));
    return 0;
}

int main(void) {
    int fd = open("/dev/null", O_RDONLY);
    printf("main fd ok=%d errno=%d\n", fd >= 0, errno);
    pthread_t t;
    pthread_create(&t, 0, worker, 0);
    pthread_join(t, 0);
    printf("main errno still %d\n", errno);
    return 0;
}
"#;

/// Milestone 4's synchronisation program: the rest of the pthread surface.
///
/// A mutex-protected shared counter (four threads, a thousand increments each,
/// so a lost update shows up as a wrong total), `pthread_once` with a guest
/// initialiser, and `pthread_key_create` with a guest destructor that libpthread
/// calls on thread teardown — three host→guest callbacks the host invokes at
/// three quite different moments.
pub const PTHREAD_SYNC_C: &str = r#"
#include <pthread.h>
#include <stdio.h>

static pthread_mutex_t m = PTHREAD_MUTEX_INITIALIZER;
static pthread_once_t once = PTHREAD_ONCE_INIT;
static pthread_key_t key;
static int shared = 0;
static int inits = 0;

static void init_once(void) { inits++; }
static void destroy(void *p) { printf("tsd destructor %ld\n", (long)p); }

static void *worker(void *arg) {
    pthread_once(&once, init_once);
    pthread_setspecific(key, (void *)((long)arg + 1));
    for (int i = 0; i < 1000; i++) {
        pthread_mutex_lock(&m);
        shared++;
        pthread_mutex_unlock(&m);
    }
    return (void *)(long)pthread_getspecific(key);
}

int main(void) {
    pthread_key_create(&key, destroy);
    pthread_t t[4];
    for (long i = 0; i < 4; i++) pthread_create(&t[i], 0, worker, (void *)i);
    for (int i = 0; i < 4; i++) { void *r; pthread_join(t[i], &r); printf("tsd %ld\n", (long)r); }
    printf("shared=%d inits=%d\n", shared, inits);
    return 0;
}
"#;

/// Milestone 4's single-threaded thread-local program.
///
/// Separate from [`THREAD_C`] because it isolates a different failure: reading
/// a `_Thread_local` at all. It compiles to `leaq _x(%rip), %rdi; callq *(%rdi)`
/// — an indirect call through a descriptor — and every value here is distinct
/// and non-zero, so an off-by-one in the block layout reads a neighbour rather
/// than passing by luck. The zero-initialised variable is the one that must come
/// out of the `__thread_bss` half.
pub const TLS_C: &str = r#"
#include <stdio.h>

_Thread_local int a = 11;
_Thread_local int b;
_Thread_local long c = 22;
_Thread_local char pad[100];

int main(void) {
    b = 33;
    pad[99] = 'z';
    printf("%d %d %ld %c %d\n", a, b, c, pad[99], pad[0]);
    return 0;
}
"#;

/// Build [`THREAD_C`], or `None` if this host cannot.
pub fn thread_fixture() -> Option<String> {
    static BUILT: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    BUILT.get_or_init(|| build_x86_64("threads", THREAD_C, &[])).clone()
}

/// Build [`ERRNO_C`], or `None` if this host cannot.
pub fn errno_fixture() -> Option<String> {
    static BUILT: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    BUILT.get_or_init(|| build_x86_64("errno_thread", ERRNO_C, &[])).clone()
}

/// Build [`PTHREAD_SYNC_C`], or `None` if this host cannot.
pub fn pthread_sync_fixture() -> Option<String> {
    static BUILT: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    BUILT.get_or_init(|| build_x86_64("pthread_sync", PTHREAD_SYNC_C, &[])).clone()
}

/// Build [`TLS_C`], or `None` if this host cannot.
pub fn tls_fixture() -> Option<String> {
    static BUILT: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    BUILT.get_or_init(|| build_x86_64("thread_locals", TLS_C, &[])).clone()
}

/// Compile `source` for the **host's** architecture and return its path.
///
/// The counterpart to [`build_x86_64`], for tests whose claim is that the two
/// architectures agree about something.
pub fn build_native(name: &str, source: &str) -> Option<String> {
    let dir = fixture_dir();
    let src = dir.join(format!("{name}.c"));
    let out = dir.join(name);
    std::fs::write(&src, source).ok()?;

    let status = Command::new("cc").arg("-o").arg(&out).arg(&src).status().ok()?;
    status.success().then(|| out.to_string_lossy().into_owned())
}

/// Run a program **natively**, as the host's own copy of it.
///
/// The only assertion worth making about a pthread or `errno` fixture: what the
/// program prints on this machine when nothing translates it. Rosetta 2 runs the
/// x86_64 build directly, so the comparison is against the same binary rather
/// than a recompilation of the same source — and when it is not available the
/// caller skips rather than fails.
pub fn run_native(path: &str) -> Option<(Option<i32>, String)> {
    let out = Command::new(path).output().ok()?;
    out.status
        .code()
        .is_some()
        .then(|| (out.status.code(), String::from_utf8_lossy(&out.stdout).into_owned()))
}

/// Build [`QSORT_C`], or `None` if this host cannot.
pub fn qsort_fixture() -> Option<String> {
    static BUILT: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    BUILT.get_or_init(|| build_x86_64("qsort_cb", QSORT_C, &[])).clone()
}

/// Build [`CTOR_C`], or `None` if this host cannot.
pub fn ctor_fixture() -> Option<String> {
    static BUILT: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    BUILT.get_or_init(|| build_x86_64("ctors", CTOR_C, &[])).clone()
}

/// Run a guest program under the driver, returning `(status, stdout, stderr)`.
///
/// Through the built binary rather than the library, because that is what the
/// milestone claims: a program *runs*, exits with its own status, and its
/// `atexit` handlers fire from the host's exit — none of which is observable
/// from inside a test that only calls `Vm::call`.
pub fn run_guest(path: &str, args: &[&str]) -> (Option<i32>, String, String) {
    run_guest_with(&[], path, args)
}

/// [`run_guest`], with extra driver flags before the program path.
pub fn run_guest_with(flags: &[&str], path: &str, args: &[&str]) -> (Option<i32>, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_rosetta3"))
        .arg("run")
        .args(flags)
        .arg(path)
        .args(args)
        .output()
        .expect("running the driver");
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Skip the calling test with an explanatory note.
#[macro_export]
macro_rules! skip {
    ($($arg:tt)*) => {{
        eprintln!("skipping {}: {}", module_path!(), format!($($arg)*));
        return;
    }};
}
