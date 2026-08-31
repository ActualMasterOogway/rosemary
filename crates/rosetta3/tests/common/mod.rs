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
///   the host->guest thunk exists in M3.
/// - `strlen`, `abs`, `puts` are the ordinary fast-callable case.
///
/// No `<math.h>`, and the `%f` argument is a constant: `sqrt` compiles to
/// `sqrtsd` and `argc * 1.5` to `cvtsi2sd`, neither of which microlift lifts
/// yet. A lifting gap inside an ABI fixture would be a red suite about the
/// wrong thing - the gap is real and it belongs to the recompiler.
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
    BUILT
        .get_or_init(|| build_x86_64("hello", HELLO_C, &[]))
        .clone()
}

/// A program whose time goes into **leaving regions**, for the block-linking
/// tests.
///
/// Every fixture above is dominated by start-up, where each region entry is the
/// first one at that address and there is nothing yet to link to. This one runs
/// a warm loop of a few hundred thousand iterations whose body is three
/// transfers out of a region and back:
///
/// - a **direct call** to a `noinline` function, which the region walk puts in
///   a region of its own (a callee is never followed - see `crate::region`);
/// - the **`ret`** out of it, which is the commonest exit there is;
/// - an **indirect call** through a `volatile` function pointer, which is where
///   a C++ virtual call lands and which reaches the run loop by a different
///   route (the indirect-call bridge).
///
/// The accumulator is printed so that a run which skipped iterations - or
/// resumed one at the wrong address - differs in its output rather than only in
/// its timing. `argv[1]` scales the loop: the tests want a second, a measurement
/// wants a minute.
pub const LINK_C: &str = r#"
#include <stdio.h>
#include <stdlib.h>

long __attribute__((noinline)) step(long a) { return a * 3 + 1; }
long __attribute__((noinline)) other(long a) { return a ^ 0x5a5a; }

static long (* volatile through_a_pointer)(long) = other;

int main(int argc, char **argv) {
    long n = argc > 1 ? atol(argv[1]) : 400000;
    long acc = 1;
    for (long i = 0; i < n; i++) {
        acc = step(acc);
        acc = through_a_pointer(acc);
        acc &= 0xffffff;
    }
    printf("acc=%ld\n", acc);
    return 0;
}
"#;

/// The fixture [`LINK_C`] cannot be: a region whose exits do not all go to the
/// same place.
///
/// `LINK_C` has one successor per exit site, so its links are perfectly predicted
/// whether they are dispatched from one branch or from four. That is the best case
/// of the mechanism and it is what M8.5 and M8.6 were measured on. Three loops
/// here, each isolating one thing:
///
/// - `fan_out` is **one region with four out-of-region direct calls**, whose
///   targets are constants. Under one probe per region all four successors are
///   reached through a single `br`; under one probe per exit site each call is its
///   own branch with its own target.
/// - `leaf` is called from four sites in a row, so its single `ret` exit has four
///   successors *at one site*. Per-site probes cannot help this, and it is here so
///   that the half the change does not fix is measured beside the half it does.
/// - `ladder` is thirty-odd blocks ending in a `ret`, which is what the range check
///   in front of a region's in-block switch is about: leaving it should cost two
///   compares rather than a binary search over every block.
///
/// The accumulator is printed, so a run that skipped an iteration or resumed at the
/// wrong address differs in output and not only in timing. `argv[1]` scales the loop:
/// the tests want a second, a measurement wants a minute.
pub const POLYLINK_C: &str = r#"
#include <stdio.h>
#include <stdlib.h>

long __attribute__((noinline)) a(long x) { return x * 3 + 1; }
long __attribute__((noinline)) b(long x) { return x ^ 0x5a5a; }
long __attribute__((noinline)) c(long x) { return x + 7; }
long __attribute__((noinline)) d(long x) { return x - 11; }

long __attribute__((noinline)) fan_out(long x, int k) {
    if (k == 0) return a(x);
    if (k == 1) return b(x);
    if (k == 2) return c(x);
    return d(x);
}

long __attribute__((noinline)) leaf(long x) { return (x | 1) * 5; }

long __attribute__((noinline)) ladder(long x) {
    for (int i = 0; i < 16; i++) {
        if (x & 1) x = x * 3 + 1; else x >>= 1;
        if (x & 2) x ^= 0x2b; else x += 3;
    }
    return x;
}

int main(int argc, char **argv) {
    long n = argc > 1 ? atol(argv[1]) : 200000;
    long acc = 1;
    for (long i = 0; i < n; i++) {
        acc = fan_out(acc, (int)(i & 3));
        acc = leaf(acc) + leaf(acc >> 1) + leaf(acc >> 2) + leaf(acc >> 3);
        acc = ladder(acc);
        acc &= 0xffffff;
    }
    printf("acc=%ld\n", acc);
    return 0;
}
"#;

/// Milestone 3's callback program: a guest comparator called by the *host's*
/// `qsort`, and a guest handler called by the host's `exit`.
///
/// Both are host->guest calls, and both are falsifiable: a comparator that is
/// never really called leaves the array unsorted, and a thunk that is called
/// with the wrong arguments sorts it wrongly. `atexit` additionally pins down
/// *when* - its output has to come after `main`'s.
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
/// effect - which is the part that proves they ran *before* it.
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

/// The destructor program: `__cxa_atexit`, reached the way a real guest reaches
/// it.
///
/// Nothing here mentions `__cxa_atexit` and that is deliberate - clang has not
/// emitted `__mod_term_func` for years and registers a
/// `__attribute__((destructor))` function with `__cxa_atexit` from a constructor
/// instead, so this two-function C program imports exactly `_puts`,
/// `_atexit` and `___cxa_atexit`.
///
/// The `atexit` call is what makes the output falsifiable rather than merely
/// present. The destructor is registered before `main` and the handler during
/// it, so one reversed list prints `atexit` first; a runtime keeping the guest's
/// `__cxa_atexit` registrations in a list of its own, drained separately, gets
/// the two lines the other way round.
pub const DTOR_C: &str = r#"
#include <stdio.h>
#include <stdlib.h>

static void plain(void) { puts("atexit"); }

__attribute__((destructor)) static void late(void) { puts("destructor"); }

int main(void) {
    atexit(plain);
    puts("main");
    return 0;
}
"#;

/// A guest callback the runtime first sees *inside* the host's `exit`.
///
/// `by_value` is called from nowhere but the exit handler, so its thunk - and
/// the translation behind it - is made after macOS has torn the main thread's
/// TLS down. Anything in the compile path holding a Rust `thread_local!` panics
/// there, which is what
/// [`a_callback_is_translated_after_the_main_threads_tls_is_gone`] fails on.
pub const LATE_CALLBACK_C: &str = r#"
#include <stdio.h>
#include <stdlib.h>

static int by_value(const void *a, const void *b) {
    int x = *(const int *)a, y = *(const int *)b;
    return (x > y) - (x < y);
}

static void late(void) {
    int v[5] = {4, 2, 5, 1, 3};
    qsort(v, 5, sizeof v[0], by_value);
    printf("late=%d%d%d%d%d\n", v[0], v[1], v[2], v[3], v[4]);
}

int main(void) {
    atexit(late);
    puts("main");
    return 0;
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
///   argument, and `pthread_join` is asserted to deliver it - the argument
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
/// guest "syscall" is really the host's libSystem call on the same host thread -
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

/// `open` and `openat` creating files, which is the one thing about them a
/// register cannot be observed to have carried any other way.
///
/// Both are declared variadic in C and described to the lifter as *fixed*, with
/// the `mode_t` always present - so the guest's mode is placed in a register the
/// callee reads as a parameter. See [`rosetta3::shims::openat`]. The
/// falsifiable claim is the permission bits on disk: a mode that arrived in the
/// wrong place, or was taken from a stale register, gives a file with
/// permissions nobody asked for, and the two modes here are deliberately odd
/// ones that no default produces.
///
/// `umask(0)` first, because the process's mask clears bits from whatever is
/// passed and would otherwise make a wrong answer look merely restrictive.
/// `argv[1]` is a directory the test owns.
pub const OPEN_C: &str = r#"
#include <stdio.h>
#include <fcntl.h>
#include <unistd.h>
#include <sys/stat.h>
#include <string.h>
#include <semaphore.h>

int main(int argc, char **argv) {
    if (argc < 2) return 2;
    umask(0);

    char path[1024];
    strcpy(path, argv[1]);
    strcat(path, "/created");
    unlink(path);
    int fd = open(path, O_WRONLY | O_CREAT | O_EXCL, 0604);
    if (fd < 0) { perror("open"); return 1; }
    close(fd);
    printf("open ok\n");

    // The same call without `O_CREAT`, whose third argument is not a mode at
    // all - it must not reach the kernel, and the file it opens must keep the
    // bits the call above gave it.
    fd = open(path, O_RDONLY, 0777);
    if (fd < 0) { perror("reopen"); return 1; }
    close(fd);
    printf("reopen ok\n");

    int dirfd = open(argv[1], O_RDONLY);
    if (dirfd < 0) { perror("opendir"); return 1; }
    fd = openat(dirfd, "created-at", O_WRONLY | O_CREAT | O_EXCL, 0641);
    if (fd < 0) { perror("openat"); return 1; }
    close(fd);
    close(dirfd);
    printf("openat ok\n");

    // `sem_open` is the same rule with the same flag and *two* optional
    // arguments. The count is what says they arrived: a semaphore created with
    // 2 gives two `sem_trywait`s and refuses the third.
    char name[32];
    snprintf(name, sizeof name, "/r3sem%d", getpid());
    sem_unlink(name);
    sem_t *sem = sem_open(name, O_CREAT | O_EXCL, 0600, 2);
    if (sem == SEM_FAILED) { perror("sem_open"); return 1; }
    printf("sem %d%d%d\n",
           sem_trywait(sem) == 0, sem_trywait(sem) == 0, sem_trywait(sem) == 0);
    sem_close(sem);
    sem_unlink(name);
    return 0;
}
"#;

/// Milestone 4's synchronisation program: the rest of the pthread surface.
///
/// A mutex-protected shared counter (four threads, a thousand increments each,
/// so a lost update shows up as a wrong total), `pthread_once` with a guest
/// initialiser, and `pthread_key_create` with a guest destructor that libpthread
/// calls on thread teardown - three host->guest callbacks the host invokes at
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

/// The stack half of the `pthread` surface: which stack a guest thread's
/// attributes were about, and who is allowed to ask about it.
///
/// Every line is written to be *identical* to the native run, which is why no
/// address is ever printed - only sizes, which the guest chose, and booleans
/// about addresses it can compute for itself. What each one falsifies:
///
/// - **`size`** - the guest's `pthread_attr_setstacksize` reached the stack its
///   own frames are on. A runtime that applied it to the host thread and gave
///   the guest a default reports the default.
/// - **`local_inside`** - a local variable of the start routine really is in
///   the range the same thread is told its stack occupies. This is the one that
///   catches a plausible-looking answer built out of the wrong mapping.
/// - **`other_thread_agrees`** - the *main* thread asks about the worker, by
///   `pthread_t`, and gets the worker's own answer. A shim that reads the
///   calling thread's state answers with the main thread's stack, which is the
///   bug this fixture exists for; it is also exactly what a garbage collector
///   does, once per thread, with the world stopped.
/// - **`supplied`** - a stack the guest allocated itself and handed over with
///   `pthread_attr_setstack` is the stack the guest's frames end up on, at the
///   size and address the guest named.
///
/// The main thread reports its own stack too, because "answers for the calling
/// thread" and "answers for the thread asked about" agree there and nowhere
/// else - a fixture that only ever asked from the worker would pass either way.
pub const PTHREAD_STACK_C: &str = r#"
#include <pthread.h>
#include <stdio.h>
#include <stdint.h>
#include <sys/mman.h>

static pthread_mutex_t m = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t cv = PTHREAD_COND_INITIALIZER;
/* 0: worker running, 1: worker has reported, 2: main is done asking */
static int state = 0;
static void *w_addr;
static size_t w_size;
static int w_local_inside;

static int inside(uintptr_t p, void *addr, size_t size) {
    return p < (uintptr_t)addr && p >= (uintptr_t)addr - size;
}

static void *worker(void *arg) {
    int local = 0;
    void *addr = pthread_get_stackaddr_np(pthread_self());
    size_t size = pthread_get_stacksize_np(pthread_self());
    pthread_mutex_lock(&m);
    w_addr = addr;
    w_size = size;
    w_local_inside = inside((uintptr_t)&local, addr, size);
    state = 1;
    pthread_cond_broadcast(&cv);
    while (state != 2) pthread_cond_wait(&cv, &m);
    pthread_mutex_unlock(&m);
    return arg;
}

static void probe(const char *what, pthread_attr_t *a, void *supplied, size_t supplied_size) {
    pthread_t t;
    state = 0;
    if (pthread_create(&t, a, worker, (void *)0x1234) != 0) {
        printf("%s: create failed\n", what);
        return;
    }
    pthread_mutex_lock(&m);
    while (state != 1) pthread_cond_wait(&cv, &m);
    pthread_mutex_unlock(&m);

    /* From another thread entirely, by pthread_t. */
    void *other_addr = pthread_get_stackaddr_np(t);
    size_t other_size = pthread_get_stacksize_np(t);
    printf("%s: size=%zu local_inside=%d other_thread_agrees=%d\n", what, w_size,
           w_local_inside, other_addr == w_addr && other_size == w_size);
    if (supplied) {
        printf("%s: supplied=%d\n", what,
               w_addr == (void *)((char *)supplied + supplied_size) && w_size == supplied_size);
    }

    pthread_mutex_lock(&m);
    state = 2;
    pthread_cond_broadcast(&cv);
    pthread_mutex_unlock(&m);
    void *r = 0;
    pthread_join(t, &r);
    printf("%s: joined %ld\n", what, (long)r);
}

int main(void) {
    int local = 0;
    void *addr = pthread_get_stackaddr_np(pthread_self());
    printf("main: size=%zu inside=%d\n", pthread_get_stacksize_np(pthread_self()),
           inside((uintptr_t)&local, addr, pthread_get_stacksize_np(pthread_self())));

    probe("default", 0, 0, 0);

    pthread_attr_t a;
    pthread_attr_init(&a);
    probe("initialised", &a, 0, 0);
    pthread_attr_setstacksize(&a, 1024 * 1024);
    probe("sized", &a, 0, 0);
    pthread_attr_destroy(&a);

    size_t size = 256 * 1024;
    void *mem = mmap(0, size, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANON, -1, 0);
    if (mem == MAP_FAILED) {
        printf("no mapping\n");
        return 1;
    }
    pthread_attr_t b;
    pthread_attr_init(&b);
    pthread_attr_setstack(&b, mem, size);
    probe("own", &b, mem, size);
    pthread_attr_destroy(&b);
    return 0;
}
"#;

/// Milestone 4's single-threaded thread-local program.
///
/// Separate from [`THREAD_C`] because it isolates a different failure: reading
/// a `_Thread_local` at all. It compiles to `leaq _x(%rip), %rdi; callq *(%rdi)`
/// - an indirect call through a descriptor - and every value here is distinct
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

/// A dylib with thread-locals of its own, for [`TLV_ROUTES_C`] to `dlopen`.
///
/// Its descriptors are not in the program's link report - the image did not
/// exist when it was built - so they are registered and trampolined by
/// [`rosetta3::dyld`] rather than at start-up. That is a second copy of the
/// whole path, and it has failed independently before.
pub const TLV_PLUGIN_C: &str = r#"
_Thread_local int plug_a = 77;
_Thread_local long plug_b;

int plug_sum(void) { plug_b += 1; return plug_a + (int)plug_b; }
"#;

/// Every route a `_Thread_local` read can reach `__tlv_bootstrap` by.
///
/// [`TLS_C`] covers the ordinary one - a `callq *(%rdi)` on the main thread
/// through a descriptor the loader bound. The three here are the ones that
/// reach the bootstrap without going past anything the start-up path
/// substituted, which is how a symbol quietly falls back to the marshaller:
///
/// - **A second thread**, which allocates its own block on first touch and is
///   where the process-global lock this path used to take actually hurt.
/// - **The thunk word called by hand**, through a `volatile` function pointer
///   the compiler cannot fold away. This is the shape any program that stores
///   a host pointer and calls it later has, and it arrives as an indirect call
///   with nothing at the call site to match.
/// - **A `dlopen`ed image's own thread-locals**, registered after the guest
///   started running.
///
/// Every printed value is distinct so a block-layout error reads a neighbour
/// rather than passing by luck, and the two `plug_sum` calls differ because the
/// plugin's variable is written through.
pub const TLV_ROUTES_C: &str = r#"
#include <dlfcn.h>
#include <pthread.h>
#include <stdio.h>

_Thread_local int a = 11;
_Thread_local long c = 22;

/* The descriptor as the ABI lays it out, under the same asm name the
   `_Thread_local` above emits - so `desc_a.thunk` is the very word the
   compiler's own `callq *(%rdi)` loads. */
struct tlv_descriptor {
    void *(*thunk)(struct tlv_descriptor *);
    unsigned long key;
    unsigned long offset;
};
extern struct tlv_descriptor desc_a __asm__("_a");

static void *(*volatile indirect)(struct tlv_descriptor *);

static void *worker(void *arg) {
    (void)arg;
    a += 100;
    c += 200;
    printf("worker a=%d c=%ld\n", a, c);
    return NULL;
}

int main(void) {
    printf("main a=%d c=%ld\n", a, c);

    pthread_t t;
    pthread_create(&t, NULL, worker, NULL);
    pthread_join(t, NULL);
    /* The worker wrote its own block, so this thread's values are untouched. */
    printf("after a=%d c=%ld\n", a, c);

    indirect = desc_a.thunk;
    int *p = (int *)indirect(&desc_a);
    printf("byhand a=%d\n", *p);

    void *h = dlopen("libtlvplugin.dylib", RTLD_NOW);
    if (!h) { printf("dlopen failed: %s\n", dlerror()); return 1; }
    int (*plug_sum)(void) = (int (*)(void))dlsym(h, "plug_sum");
    if (!plug_sum) { printf("dlsym failed: %s\n", dlerror()); return 2; }
    printf("plug %d %d\n", plug_sum(), plug_sum());
    return 0;
}
"#;

/// Build [`TLV_ROUTES_C`] and its plugin beside it, or `None` if this host
/// cannot.
pub fn tlv_routes_fixture() -> Option<String> {
    static BUILT: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    BUILT
        .get_or_init(|| {
            build_x86_64(
                "libtlvplugin.dylib",
                TLV_PLUGIN_C,
                &["-dynamiclib", "-install_name", "@rpath/libtlvplugin.dylib"],
            )?;
            build_x86_64("tlv_routes", TLV_ROUTES_C, &[])
        })
        .clone()
}

/// Milestone 5's `setjmp`/`longjmp` program.
///
/// Written so that each claim fails on its own:
///
/// - **The jump happens at all.** `level3` never returns, so any line printed
///   after its call site means the `longjmp` was a no-op.
/// - **Three frames are discarded.** `level1` and `level2` print after their
///   inner call, and neither line may appear.
/// - **The value comes back.** Two round trips through the same buffer with
///   different values, plus the C rule that zero arrives as one.
/// - **The stack is really restored.** `main` keeps a large local array and
///   long-lived counters across the jump; a `longjmp` that restored `rip` and
///   not `rsp` would run `main`'s tail on `level3`'s frame, and the array's
///   sentinel bytes are what say so.
/// - **Both spellings.** `_setjmp`/`_longjmp` and `sigsetjmp`/`siglongjmp` are
///   separate imports (`__setjmp`, `_sigsetjmp`) reaching the same shim.
///
/// Deliberately *not* jumping out of a callback the host called: that is a
/// `longjmp` across host frames, which the runtime refuses by name. See the
/// milestone note.
pub const SETJMP_C: &str = r#"
#include <setjmp.h>
#include <stdio.h>
#include <string.h>

static jmp_buf env;
static sigjmp_buf senv;

/* File scope, not locals: a non-volatile local modified between `setjmp` and
   `longjmp` has an indeterminate value afterwards - C says so - and a fixture
   whose expected output depends on undefined behaviour tests the compiler, not
   the runtime. */
static int trips = 0, total = 0;

static void level3(int v) { printf("level3\n"); longjmp(env, v); }
static void level2(int v) { printf("level2\n"); level3(v); printf("BUG: level2 returned\n"); }
static void level1(int v) { printf("level1\n"); level2(v); printf("BUG: level1 returned\n"); }

static void plain(void)  { _longjmp(env, 9); }
static void masked(void) { siglongjmp(senv, 11); }

int main(void) {
    char guard[256];
    memset(guard, 'g', sizeof guard);
    guard[255] = 0;

    int v = setjmp(env);
    if (v == 0) {
        printf("first return\n");
        level1(42);
        printf("BUG: setjmp fell through\n");
    }
    trips++;
    total += v;
    printf("came back with %d (trip %d)\n", v, trips);
    if (v == 42) level1(0);

    if (_setjmp(env) == 0) plain();
    else printf("plain spelling ok\n");

    if (sigsetjmp(senv, 1) == 0) masked();
    else printf("masked spelling ok\n");

    printf("guard %c%c len %zu trips %d total %d\n",
           guard[0], guard[254], strlen(guard), trips, total);
    return 0;
}
"#;

/// Build [`SETJMP_C`], or `None` if this host cannot.
pub fn setjmp_fixture() -> Option<String> {
    static BUILT: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    BUILT
        .get_or_init(|| build_x86_64("setjmp", SETJMP_C, &[]))
        .clone()
}

/// Milestone 5's refusal fixture: a `longjmp` out of a host->guest callback.
///
/// The comparator is called by the *host's* `qsort`, so the `setjmp` it jumps
/// back to is in an outer VM entry with host frames in between. Restoring the
/// guest context would abandon them silently, so the runtime refuses - and this
/// program is how the refusal is checked to be a named diagnostic rather than a
/// crash. It runs natively; under the driver it must stop and say why.
pub const LONGJMP_ACROSS_C: &str = r#"
#include <setjmp.h>
#include <stdio.h>
#include <stdlib.h>

static jmp_buf env;

static int by_int(const void *a, const void *b) {
    if (*(const int *)a == 9) longjmp(env, 5);
    return 0;
}

int main(void) {
    int v[4] = {5, 9, 1, 7};
    if (setjmp(env) == 0) {
        qsort(v, 4, sizeof(int), by_int);
        printf("no jump\n");
    } else {
        printf("jumped out of the comparator\n");
    }
    return 0;
}
"#;

/// Build [`LONGJMP_ACROSS_C`], or `None` if this host cannot.
pub fn longjmp_across_fixture() -> Option<String> {
    static BUILT: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    BUILT
        .get_or_init(|| build_x86_64("longjmp_across", LONGJMP_ACROSS_C, &[]))
        .clone()
}

/// Compile `source` as x86_64 C++ and return its path.
///
/// The `.c` extension the helper writes is overridden with `-x c++`, which is
/// also what makes the guest link against libc++ and import the Itanium ABI
/// surface [`rosetta3::unwind`] implements.
pub fn build_cpp(name: &str, source: &str) -> Option<String> {
    build_x86_64(name, source, &["-x", "c++", "-lc++"])
}

/// Compile `source` as x86_64 Objective-C++ and return its path.
///
/// The `++` is what puts a real `libc++abi` in the image, which is where an
/// Objective-C exception's `type_info` and unwinder come from - see
/// `rosetta3::objcexc`. A plain Objective-C fixture links neither.
pub fn build_objcpp(name: &str, source: &str) -> Option<String> {
    build_x86_64(
        name,
        source,
        &["-x", "objective-c++", "-lc++", "-framework", "Foundation"],
    )
}

/// Compile `source` as x86_64 Objective-C and return its path.
///
/// `-framework Foundation` is not optional even for a fixture that only
/// declares a root class: there is no `NSObject` to derive from without it, and
/// a class with no superclass exercises a path real programs never take.
pub fn build_objc(name: &str, source: &str) -> Option<String> {
    build_x86_64(
        name,
        source,
        &["-x", "objective-c", "-framework", "Foundation"],
    )
}

/// M6.3's message-sending program: an Objective-C guest that actually runs.
///
/// Sent from *guest* code, which is the whole of M6.3, and written so that each
/// mechanism fails on its own rather than as one blob of output:
///
/// - **`+load`** runs before `main`, superclass first, and the counter says so:
///   11 means both ran and in that order, 10 means the superclass's did not.
/// - **guest -> guest** dispatch, which is a jump rather than a call
///   (`legs`, `says`).
/// - **overriding**, where `[d says]` and `[[Animal alloc] ...] says]` must
///   differ - a runtime that resolved the `IMP` on the wrong class prints the
///   same string twice.
/// - **`super`**, twice over: `Dog`'s initialiser calls `Animal`'s, which calls
///   `NSObject`'s. `objc_msgSendSuper2` naming the wrong class recurses forever.
/// - **a `double` return and a pointer argument**, so the marshaller is
///   exercised in both register classes.
/// - **guest -> host** dispatch by type encoding: `isKindOfClass:`,
///   `respondsToSelector:`, `stringWithUTF8String:`, `UTF8String`, `length`.
/// - **class identity in both directions**: `[d class] == [Dog class]` is only
///   true if a host `Class` coming back is translated to the address the guest
///   already has, and `!= [Animal class]` says the translation is not a
///   constant.
/// - **a category on a host class**, reached by messaging an `NSObject`.
/// - **a constant `@"..."` string**, which is an object living in *guest* memory
///   whose `isa` is the host's class.
/// - **messaging nil**, which is the language rather than an error.
/// - **`@synchronized` and `@autoreleasepool`**, the two runtime entry points a
///   program reaches without naming them.
pub const OBJC_MESSAGES_M: &str = r#"
#import <Foundation/Foundation.h>
#import <objc/runtime.h>

static int loaded = 0;

@protocol Speaks
- (const char *)says;
@end

@interface Animal : NSObject { int legs; }
- (instancetype)initWithLegs:(int)n;
- (int)legs;
- (const char *)says;
- (int)twiceLegs;
@end

@implementation Animal
+ (void)load { loaded += 1; }
- (instancetype)initWithLegs:(int)n { self = [super init]; if (self) legs = n; return self; }
- (int)legs { return legs; }
- (const char *)says { return "..."; }
/* One message and nothing else, so clang tail-jumps to objc_msgSend at -O2. */
- (int)twiceLegs { return [self legs] * 2; }
@end

@interface Dog : Animal <Speaks> { double weight; }
- (instancetype)initWithWeight:(double)w;
- (double)weight;
- (const char *)describeWith:(const char *)prefix;
+ (const char *)species;
@end

@implementation Dog
+ (void)load { loaded += 10; }
- (instancetype)initWithWeight:(double)w {
    self = [super initWithLegs:4];
    if (self) weight = w;
    return self;
}
- (double)weight { return weight; }
- (const char *)says { return "woof"; }
- (const char *)describeWith:(const char *)prefix { return prefix; }
+ (const char *)species { return "canis"; }
@end

@interface NSObject (Rosetta)
- (const char *)rosetta;
@end
@implementation NSObject (Rosetta)
- (const char *)rosetta { return "category on a host class"; }
@end

int main(void) {
    Dog *d = [[Dog alloc] initWithWeight:12.5];
    printf("load %d\n", loaded);
    printf("legs %d says %s\n", [d legs], [d says]);
    printf("inherited %s\n", [[[Animal alloc] initWithLegs:2] says]);
    printf("twice %d\n", [d twiceLegs]);
    printf("weight %.1f\n", [d weight]);
    printf("prefix %s\n", [d describeWith:"argument"]);
    printf("species %s\n", [Dog species]);
    printf("identity %d %d\n", [d class] == [Dog class], [d class] == [Animal class]);
    printf("kind %d\n", [d isKindOfClass:[Animal class]]);
    printf("name %s super %s\n",
           class_getName([d class]), class_getName(class_getSuperclass([d class])));
    printf("responds %d %d\n",
           [d respondsToSelector:@selector(weight)], [d respondsToSelector:@selector(fly)]);
    printf("conforms %d\n", [d conformsToProtocol:@protocol(Speaks)]);
    printf("category %s\n", [d rosetta]);
    NSString *s = [NSString stringWithUTF8String:"a host object"];
    printf("host %s %lu\n", [s UTF8String], (unsigned long)[s length]);
    printf("constant %s\n", [@"a constant string" UTF8String]);
    printf("nil %d %p\n", [(Dog *)nil legs], (void *)[(Dog *)nil says]);
    /* Aggregates by value, in the three shapes the two ABIs disagree about
       most. An NSRect is thirty-two bytes: the guest passes it on its *stack*
       and returns it through a hidden buffer (`objc_msgSend_stret`), while the
       host takes it in v0-v3 and returns it there. An NSPoint is two SSE
       eightbytes on one side and a two-member HFA on the other. An NSRange is
       two integer eightbytes and two `x` registers, which is the one case the
       two conventions agree on - and is worth having next to the others so a
       failure says which of the three broke. */
    NSRect rc = [[NSValue valueWithRect:NSMakeRect(1.5, 2.5, 3.5, 4.5)] rectValue];
    printf("rect %.1f %.1f %.1f %.1f\n", rc.origin.x, rc.origin.y, rc.size.width, rc.size.height);
    NSPoint pt = [[NSValue valueWithPoint:NSMakePoint(6.5, 7.5)] pointValue];
    printf("point %.1f %.1f\n", pt.x, pt.y);
    NSRange rg = [[NSValue valueWithRange:NSMakeRange(8, 9)] rangeValue];
    printf("range %lu %lu\n", (unsigned long)rg.location, (unsigned long)rg.length);
    /* A variadic method, rendered by the runtime and re-sent with the result
       standing in for its format - including `%@`, which is a message. */
    printf("format %s\n",
           [[NSString stringWithFormat:@"%d/%@/%.2f/%%", 42, @"x", 1.5] UTF8String]);
    @synchronized (d) { printf("synchronized\n"); }
    @autoreleasepool { printf("pool\n"); }
    return 0;
}
"#;

/// Build [`OBJC_MESSAGES_M`], or `None` if this host cannot.
///
/// `extra` is threaded so the same source can be built at `-O2`, where clang
/// tail-jumps to `objc_msgSend` instead of calling it - a different path through
/// the runtime for identical output.
pub fn objc_messages_fixture(name: &str, extra: &[&str]) -> Option<String> {
    let mut flags = vec!["-x", "objective-c", "-framework", "Foundation"];
    flags.extend_from_slice(extra);
    build_x86_64(name, OBJC_MESSAGES_M, &flags)
}

/// A guest method the **host** calls with an aggregate by value.
///
/// The direction M6.3 did not cover and DELTARUNE found. Every other aggregate
/// test in this suite is guest->host - the guest sends `rectValue` and the
/// runtime marshals the result back. This is the mirror, and it is a different
/// piece of machinery: AAPCS64 hands a callback an `NSRect` in `v0`–`v3`, SysV
/// AMD64 classifies the same rectangle MEMORY, and the guest therefore reads it
/// from its own **stack** at `0x10(%rbp)`. A trampoline forwarding only
/// registers gives it whatever was already there.
///
/// Key-value coding is what makes this reachable with Foundation alone.
/// `setValue:forKey:` on a struct-typed property unwraps the `NSValue` and
/// invokes the setter *from the host*, with the struct by value - which is
/// exactly what AppKit does to `-[NSView initWithFrame:]` when it instantiates
/// one from a nib, without needing a nib or a window server.
///
/// Three shapes, so a failure says which:
///
/// - **`NSRect`** - thirty-two bytes, MEMORY on the guest side and an HFA of
///   four on the host's. The one that has to reach the guest's stack.
/// - **`NSPoint`** - sixteen bytes: two SSE eightbytes in `xmm0`/`xmm1` for the
///   guest, an HFA of two for the host. Registers on both sides, different
///   registers.
/// - **`double`** - a floating-point *return*, which comes back in `xmm0` and
///   not in `rax`. A trampoline typed to return an integer reports whatever the
///   last instruction left behind.
pub const OBJC_HOST_AGGREGATE_M: &str = r#"
#import <Foundation/Foundation.h>

/* Ivars declared rather than synthesized: writing both accessors by hand
   suppresses synthesis, and `bounds` collides with a Carbon typedef if the
   compiler is left to guess. */
@interface Canvas : NSObject {
    NSRect _bounds;
    NSPoint _origin;
    double _scale;
}
@property (nonatomic) NSRect bounds;
@property (nonatomic) NSPoint origin;
@property (nonatomic) double scale;
@end

@implementation Canvas
/* Written out rather than @synthesize'd so the setter is unambiguously a guest
   method taking the struct by value, and so a reader can see the shape. */
- (void)setBounds:(NSRect)r { _bounds = r; }
- (NSRect)bounds { return _bounds; }
- (void)setOrigin:(NSPoint)p { _origin = p; }
- (NSPoint)origin { return _origin; }
- (void)setScale:(double)s { _scale = s; }
- (double)scale { return _scale; }
@end

int main(void) {
    Canvas *c = [[Canvas alloc] init];

    /* Host -> guest, struct by value: KVC invokes the setter itself. */
    [c setValue:[NSValue valueWithRect:NSMakeRect(1.5, 2.5, 3.5, 4.5)] forKey:@"bounds"];
    NSRect b = [c bounds];
    printf("bounds %.1f %.1f %.1f %.1f\n", b.origin.x, b.origin.y, b.size.width, b.size.height);

    [c setValue:[NSValue valueWithPoint:NSMakePoint(6.5, 7.5)] forKey:@"origin"];
    NSPoint o = [c origin];
    printf("origin %.1f %.1f\n", o.x, o.y);

    /* Host -> guest with a floating-point *return*: KVC calls the getter and
       boxes what it finds in `d0`. */
    [c setValue:@(9.25) forKey:@"scale"];
    printf("scale %.2f\n", [[c valueForKey:@"scale"] doubleValue]);

    return 0;
}
"#;

/// An argument *after* an aggregate the guest passed in memory.
///
/// `-[NSTrackingArea initWithRect:options:owner:userInfo:]`, which is the shape
/// SysV places one way and AAPCS64 another: the `NSRect` is thirty-two bytes and
/// goes to the guest's outgoing stack, and the three arguments behind it still
/// take the integer registers it never touched. A marshaller that appends an
/// argument to whatever it wrote last puts `options` behind the rect instead,
/// and the tracking area comes back with none of the options it was given.
///
/// The rect is checked as well as the options: the two travel by different
/// routes, and only the second one moved.
pub const OBJC_AFTER_MEMORY_ARG_M: &str = r#"
#import <Cocoa/Cocoa.h>

int main(void) {
    NSTrackingAreaOptions opts =
        NSTrackingMouseEnteredAndExited | NSTrackingMouseMoved | NSTrackingActiveAlways;
    id owner = [[NSObject alloc] init];
    NSTrackingArea *area = [[NSTrackingArea alloc] initWithRect:NSMakeRect(1.5, 2.5, 30.5, 40.5)
                                                        options:opts
                                                          owner:owner
                                                       userInfo:nil];
    NSRect r = [area rect];
    printf("rect %.1f %.1f %.1f %.1f\n", r.origin.x, r.origin.y, r.size.width, r.size.height);
    printf("options %d owner %d\n", [area options] == opts, [area owner] == owner);
    return 0;
}
"#;

/// Build [`OBJC_AFTER_MEMORY_ARG_M`], or `None` if this host cannot.
pub fn objc_after_memory_arg_fixture() -> Option<String> {
    static BUILT: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    BUILT
        .get_or_init(|| {
            build_x86_64(
                "objc_after_memory_arg",
                OBJC_AFTER_MEMORY_ARG_M,
                &["-x", "objective-c", "-framework", "Cocoa"],
            )
        })
        .clone()
}

/// Build [`OBJC_HOST_AGGREGATE_M`], or `None` if this host cannot.
pub fn objc_host_aggregate_fixture() -> Option<String> {
    static BUILT: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    BUILT
        .get_or_init(|| build_objc("objc_host_aggregate", OBJC_HOST_AGGREGATE_M))
        .clone()
}

/// A guest method whose encoding this runtime's decoder refuses, sent guest->guest.
///
/// `encoding::signature` will not classify a union - its members overlap, so its
/// eightbytes are undecidable - and [`crate::objc::add_method`] installs such a method
/// anyway, recording `MethodNotHostCallable` and moving on. That is only sound while
/// the send path leaves the encoding alone when the `IMP` is the guest's own, which is
/// what this fixture is: a union returned by value, from one guest method to another.
///
/// Roblox's `AppDelegate` is where the encoding was decoded eagerly and the send
/// refused.
pub const OBJC_GUEST_UNION_M: &str = r#"
#import <Foundation/Foundation.h>

union Word {
    double d;
    struct { int lo; int hi; } halves;
};

/* Large enough to be MEMORY-class on both conventions, so the send is
   `objc_msgSend_stret` rather than the ordinary trampoline. */
struct Wide {
    union Word a;
    union Word b;
    union Word c;
};

@interface Packer : NSObject
- (struct Wide)wide;
- (NSString *)describe;
@end

@implementation Packer
- (struct Wide)wide {
    struct Wide w;
    w.a.d = 1.5;
    w.b.halves.lo = 7;
    w.b.halves.hi = 9;
    w.c.d = 2.25;
    return w;
}
/* Guest -> guest: the sender and `wide` are both this class's. */
- (NSString *)describe {
    struct Wide w = [self wide];
    return [NSString stringWithFormat:@"%.2f %d %d %.2f",
            w.a.d, w.b.halves.lo, w.b.halves.hi, w.c.d];
}
@end

int main(void) {
    printf("%s\n", [[[Packer alloc] init] describe].UTF8String);
    return 0;
}
"#;

/// Build [`OBJC_GUEST_UNION_M`], or `None` if this host cannot.
pub fn objc_guest_union_fixture() -> Option<String> {
    static BUILT: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    BUILT
        .get_or_init(|| build_objc("objc_guest_union", OBJC_GUEST_UNION_M))
        .clone()
}

/// M6.3's boundary program: the three messages that must be *refused*.
///
/// One binary selected by argument, as [`UNWIND_REFUSALS_CPP`] is, and for the
/// same reason: a refusal is only worth anything if the program that provokes it
/// is otherwise ordinary, which the shared `main` shows.
pub const OBJC_REFUSALS_M: &str = r#"
#import <Foundation/Foundation.h>
#include <string.h>

int main(int argc, char **argv) {
    const char *which = argc > 1 ? argv[1] : "";
    printf("before\n");
    if (!strcmp(which, "variadic")) {
        /* `appendFormat:` takes an ellipsis, and no type encoding records it. */
        NSMutableString *s = [[NSMutableString alloc] init];
        [s appendFormat:@"%d", 3];
        printf("appended %s\n", [s UTF8String]);
    } else if (!strcmp(which, "aggregate")) {
        /* `rangeOfString:` returns an NSRange by value. */
        NSRange r = [@"hello world" rangeOfString:@"world"];
        printf("range %lu\n", (unsigned long)r.location);
    } else if (!strcmp(which, "throw")) {
        @throw [NSException exceptionWithName:@"E" reason:@"r" userInfo:nil];
    }
    printf("after\n");
    return 0;
}
"#;

/// An Objective-C exception thrown and caught, which is the whole of the path
/// `rosetta3::objcexc` builds.
///
/// Each clause is its own claim:
///
/// - **the handler runs at all** - `_OBJC_EHTYPE_$_NSException` has to be a
///   real `type_info`, and an unresolved one binds to zero.
/// - **name and reason survive** - the object reaches the handler, which is
///   `objc_begin_catch` returning the `id` rather than the buffer holding it.
/// - **a subclass is caught by its base** - the record's base chain is the
///   class chain, and nothing else makes this work.
/// - **the wrong class is not caught** - a chain that matched everything would
///   pass every clause above.
/// - **`@finally` runs either way** - a cleanup landing pad, not a handler.
pub const OBJC_EXCEPTIONS_MM: &str = r#"
#import <Foundation/Foundation.h>
#include <stdio.h>

@interface Narrower : NSException @end
@implementation Narrower @end

int main(void) {
    @try {
        @throw [NSException exceptionWithName:@"E" reason:@"r" userInfo:nil];
    } @catch (NSException *e) {
        printf("caught %s %s\n", [[e name] UTF8String], [[e reason] UTF8String]);
    } @finally {
        printf("finally\n");
    }

    @try {
        @throw [Narrower exceptionWithName:@"N" reason:@"n" userInfo:nil];
    } @catch (NSException *e) {
        printf("subclass %s\n", [[e name] UTF8String]);
    }

    @try {
        @try {
            @throw [NSException exceptionWithName:@"O" reason:@"o" userInfo:nil];
        } @catch (Narrower *e) {
            printf("wrong clause\n");
        }
    } @catch (NSException *e) {
        printf("outer %s\n", [[e name] UTF8String]);
    }

    printf("done\n");
    return 0;
}
"#;

/// Build [`OBJC_EXCEPTIONS_MM`], or `None` if this host cannot.
pub fn objc_exceptions_fixture() -> Option<String> {
    static BUILT: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    BUILT
        .get_or_init(|| build_objcpp("objc_exceptions", OBJC_EXCEPTIONS_MM))
        .clone()
}

/// Build [`OBJC_REFUSALS_M`], or `None` if this host cannot.
pub fn objc_refusals_fixture() -> Option<String> {
    static BUILT: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    BUILT
        .get_or_init(|| build_objc("objc_refusals", OBJC_REFUSALS_M))
        .clone()
}

/// Milestone 5's exception program: everything about unwinding that works.
///
/// Each clause is a separate claim, and each one fails on its own:
///
/// - **three frames and their destructors** - the walk itself. `~level3`,
///   `~level2`, `~level1` must appear in that order and before the handler: a
///   runtime that entered the handler without the cleanups prints them late or
///   not at all.
/// - **the object's own destructor** - run by `__cxa_end_catch`, which is a
///   host->guest call from inside a shim.
/// - **nested `try`** - the inner handler must *not* take an exception of the
///   wrong type, which is the difference between matching a type and matching
///   the first clause found.
/// - **`catch (...)`** - a null type-table entry, and the one case where a
///   pc-relative decode of zero would produce a plausible wrong address.
/// - **base classes** - both a guest base and `std::exception`, whose
///   `type_info` lives in the host's libc++.
/// - **rethrow** - `throw;` continues the *same* exception past the handler that
///   caught it, without freeing it at the end of that handler.
/// - **a second thread** - its own stack, its own walk, its own in-flight state.
pub const EXCEPTIONS_CPP: &str = r#"
#include <cstdio>
#include <exception>
#include <pthread.h>

struct Trace { const char *n; ~Trace() { printf("~%s\n", n); } };
struct Boom { int v; ~Boom() { printf("~Boom(%d)\n", v); } };
struct Other { const char *why; };
struct Base { int v; Base(int x) : v(x) {} };
struct Derived : Base { Derived(int x) : Base(x) {} };
struct MyError : std::exception {
    const char *what() const noexcept override { return "my error"; }
};

static int level3(int n) { Trace t{"level3"}; if (n > 0) throw Boom{n * 2}; return 1; }
static int level2(int n) { Trace t{"level2"}; return level3(n) + 1; }
static int level1(int n) { Trace t{"level1"}; return level2(n) + 1; }

static void nested() {
    try {
        try { throw Other{"inner"}; }
        catch (const Boom &) { printf("BUG: matched the wrong clause\n"); }
    } catch (const Other &o) { printf("outer caught %s\n", o.why); }
}

static int same_frame() { try { throw 11; } catch (int i) { return i; } }

static void rethrower() {
    try { throw Boom{1}; }
    catch (const Boom &b) { printf("rethrowing %d\n", b.v); throw; }
}

static void *worker(void *arg) {
    try { Trace t{"thread"}; throw Boom{(int)(long)arg}; }
    catch (const Boom &b) { printf("thread caught %d\n", b.v); }
    return (void *)1;
}

int main(void) {
    try { level1(3); } catch (const Boom &b) { printf("caught Boom %d\n", b.v); }
    nested();
    try { throw 3.5; } catch (...) { printf("caught something\n"); }
    printf("same frame %d\n", same_frame());
    try { throw Other{"by value"}; } catch (Other o) { printf("by value %s\n", o.why); }
    try { throw Derived(4); } catch (const Base &b) { printf("base caught %d\n", b.v); }
    try { throw MyError{}; } catch (const std::exception &e) { printf("std %s\n", e.what()); }
    try { rethrower(); } catch (const Boom &b) { printf("finally %d\n", b.v); }

    pthread_t t;
    pthread_create(&t, 0, worker, (void *)7L);
    void *r = 0;
    pthread_join(t, &r);
    printf("thread returned %ld\n", (long)r);

    printf("done\n");
    return 0;
}
"#;

/// Build [`EXCEPTIONS_CPP`], or `None` if this host cannot.
pub fn exceptions_fixture() -> Option<String> {
    static BUILT: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    BUILT
        .get_or_init(|| build_cpp("exceptions", EXCEPTIONS_CPP))
        .clone()
}

/// Milestone 5's boundary program: the three throws that must be *refused*.
///
/// One binary, selected by argument, because each case ends the process - and
/// because a refusal is only worth anything if the program that provokes it runs
/// correctly everywhere else, which the shared `main` here demonstrates.
pub const UNWIND_REFUSALS_CPP: &str = r#"
#include <cstdio>
#include <cstdlib>
#include <cstring>

struct Trace { const char *n; ~Trace() { printf("~%s\n", n); } };
struct Boom { int v; };
struct A { int a; };
struct B { int b; };
struct AB : A, B { };

static void uncaught() { Trace t{"uncaught"}; throw Boom{1}; }

static int compare(const void *a, const void *b) {
    if (*(const int *)a == 9) throw Boom{2};
    return 0;
}

int main(int argc, char **argv) {
    const char *which = argc > 1 ? argv[1] : "";
    printf("before\n");
    if (!strcmp(which, "uncaught")) {
        uncaught();
    } else if (!strcmp(which, "across")) {
        int v[2] = {9, 1};
        try { qsort(v, 2, sizeof(int), compare); }
        catch (const Boom &) { printf("caught across a host frame\n"); }
    } else if (!strcmp(which, "multiple")) {
        try { throw AB{}; } catch (const B &) { printf("multiple inheritance\n"); }
    }
    printf("after\n");
    return 0;
}
"#;

/// Build [`UNWIND_REFUSALS_CPP`], or `None` if this host cannot.
pub fn unwind_refusals_fixture() -> Option<String> {
    static BUILT: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    BUILT
        .get_or_init(|| build_cpp("unwind_refusals", UNWIND_REFUSALS_CPP))
        .clone()
}

/// Milestone 5's signal program.
///
/// The kernel calling a guest function, which is the third distinct caller of a
/// host->guest thunk after `qsort` and `pthread_create` - and the first that
/// arrives without the runtime being anywhere nearby.
///
/// - Both installers, because they substitute the handler differently:
///   `signal` returns the old disposition in a register, `sigaction` writes it
///   into a struct the guest owns.
/// - The handler is **saved and restored**, which is the direction a runtime
///   that only substituted outbound pointers would get wrong: the guest would
///   be handed an arm64 thunk and would install it as its own function.
/// - `SIG_IGN` and `SIG_DFL` are installed too. They are dispositions, not
///   addresses, and a runtime that wrapped them in thunks would run a handler
///   where the program asked for the signal to be ignored.
pub const SIGNAL_C: &str = r#"
#include <signal.h>
#include <stdio.h>

static volatile sig_atomic_t hits = 0;

static void on_usr1(int sig) { hits++; printf("handler saw %d\n", sig); }

int main(void) {
    printf("layout %zu %zu\n", sizeof(struct sigaction), sizeof(sigset_t));

    void (*previous)(int) = signal(SIGUSR1, on_usr1);
    printf("previous was default %d\n", previous == SIG_DFL);
    raise(SIGUSR1);

    /* The round trip: what `signal` gives back must be the guest's own
       function, or restoring it installs something else entirely. */
    void (*saved)(int) = signal(SIGUSR1, SIG_IGN);
    printf("saved is mine %d\n", saved == on_usr1);
    raise(SIGUSR1);
    signal(SIGUSR1, saved);
    raise(SIGUSR1);

    struct sigaction sa, old;
    sa.sa_handler = on_usr1;
    sa.sa_flags = 0;
    sigemptyset(&sa.sa_mask);
    sigaction(SIGUSR2, &sa, &old);
    printf("old was default %d\n", old.sa_handler == SIG_DFL);
    raise(SIGUSR2);

    sigaction(SIGUSR2, 0, &old);
    printf("queried is mine %d\n", old.sa_handler == on_usr1);

    signal(SIGUSR1, SIG_DFL);
    printf("hits %d\n", hits);
    return 0;
}
"#;

/// Build [`SIGNAL_C`], or `None` if this host cannot.
pub fn signal_fixture() -> Option<String> {
    static BUILT: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    BUILT
        .get_or_init(|| build_x86_64("signals", SIGNAL_C, &[]))
        .clone()
}

/// Milestone 5's refusal fixture for signals: a handler wanting a `ucontext_t`.
///
/// `SA_SIGINFO` asks for the machine state at the interrupted instruction, which
/// on this runtime is inside a Cranelift-compiled translation with no map back
/// to the guest's x86-64 registers. The refusal is the deliverable; the program
/// exists so it can be checked as a named diagnostic.
pub const SIGINFO_C: &str = r#"
#include <signal.h>
#include <stdio.h>

static void detailed(int sig, siginfo_t *info, void *uc) {
    (void)uc;
    printf("detailed %d %p\n", sig, info->si_addr);
}

int main(void) {
    struct sigaction sa;
    sa.sa_sigaction = detailed;
    sa.sa_flags = SA_SIGINFO;
    sigemptyset(&sa.sa_mask);
    sigaction(SIGUSR1, &sa, 0);
    raise(SIGUSR1);
    printf("done\n");
    return 0;
}
"#;

/// Build [`SIGINFO_C`], or `None` if this host cannot.
pub fn siginfo_fixture() -> Option<String> {
    static BUILT: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    BUILT
        .get_or_init(|| build_x86_64("siginfo", SIGINFO_C, &[]))
        .clone()
}

/// Build [`THREAD_C`], or `None` if this host cannot.
pub fn thread_fixture() -> Option<String> {
    static BUILT: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    BUILT
        .get_or_init(|| build_x86_64("threads", THREAD_C, &[]))
        .clone()
}

/// Build [`ERRNO_C`], or `None` if this host cannot.
pub fn errno_fixture() -> Option<String> {
    static BUILT: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    BUILT
        .get_or_init(|| build_x86_64("errno_thread", ERRNO_C, &[]))
        .clone()
}

/// Build [`OPEN_C`], or `None` if this host cannot.
pub fn open_fixture() -> Option<String> {
    static BUILT: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    BUILT
        .get_or_init(|| build_x86_64("open_mode", OPEN_C, &[]))
        .clone()
}

/// Build [`PTHREAD_STACK_C`], or `None` if this host cannot.
pub fn pthread_stack_fixture() -> Option<String> {
    static BUILT: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    BUILT
        .get_or_init(|| build_x86_64("pthread_stack", PTHREAD_STACK_C, &[]))
        .clone()
}

/// Build [`PTHREAD_SYNC_C`], or `None` if this host cannot.
pub fn pthread_sync_fixture() -> Option<String> {
    static BUILT: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    BUILT
        .get_or_init(|| build_x86_64("pthread_sync", PTHREAD_SYNC_C, &[]))
        .clone()
}

/// A guest that divides 128-bit integers, which no machine here can do.
///
/// `unsigned __int128 / unsigned __int128` compiles to a call to compiler-rt's
/// `__udivti3` on both architectures, and each argument spans a *register pair*
/// - so a shim that read only the low half of each would answer plausibly and
/// wrongly. The dividend is `2^100 + 12345` for exactly that reason: it does not
/// fit in sixty-four bits, so losing the high half changes the answer rather
/// than hiding in it.
///
/// The divisor comes from `argc` so the optimiser cannot fold the division away,
/// and the signed forms are exercised too because they truncate toward zero and
/// carry the numerator's sign into the remainder.
pub const INT128_C: &str = r#"
#include <stdio.h>

static void show(const char *tag, unsigned __int128 v) {
    printf("%s %llu:%llu\n", tag, (unsigned long long)(v >> 64), (unsigned long long)v);
}

int main(int argc, char **argv) {
    (void)argv;
    unsigned __int128 a = ((unsigned __int128)1 << 100) + 12345;
    unsigned __int128 b = (unsigned __int128)argc * 3;
    show("uq", a / b);
    show("ur", a % b);
    __int128 c = -(__int128)a;
    __int128 d = (__int128)argc * 3;
    show("sq", (unsigned __int128)(c / d));
    show("sr", (unsigned __int128)(c % d));
    return 0;
}
"#;

/// Build [`INT128_C`], or `None` if this host cannot.
pub fn int128_fixture() -> Option<String> {
    static BUILT: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    BUILT
        .get_or_init(|| build_x86_64("int128", INT128_C, &[]))
        .clone()
}

/// A guest that converts a `CFString` to bytes the way Foundation callers do.
///
/// `CFStringGetCString`'s last parameter is the encoding, and an entry that
/// omitted it made the host read register garbage and answer `false` - an empty
/// string, no error, no fault. Unity built a plugin path out of that empty
/// string and every `.bundle` plugin in Below Zero failed to load.
///
/// The fixture prints the length, the buffer size and the result together,
/// because each is a different failure: a wrong `CFStringGetLength` gives the
/// wrong length, a wrong `CFStringGetMaximumSizeForEncoding` gives a buffer too
/// small, and a wrong `CFStringGetCString` gives the empty string that was
/// actually happening.
pub const CFSTRING_C: &str = r#"
#include <CoreFoundation/CoreFoundation.h>
#include <stdio.h>

int main(void) {
    CFStringRef s = CFStringCreateWithCString(NULL, "fmodstudio", kCFStringEncodingUTF8);
    CFIndex len = CFStringGetLength(s);
    CFIndex max = CFStringGetMaximumSizeForEncoding(len, kCFStringEncodingUTF8);
    char buf[256];
    buf[0] = 0;
    Boolean ok = CFStringGetCString(s, buf, max + 1, kCFStringEncodingUTF8);
    printf("len %ld max %ld ok %d buf '%s'\n", (long)len, (long)max, (int)ok, buf);
    CFRelease(s);
    return 0;
}
"#;

/// A guest that schedules a run loop source and a timer, and is called back
/// through both.
///
/// The claim is the one thing a run loop is for: *guest* code runs from inside
/// `CFRunLoopRunInMode`. Both callbacks are guest functions, so both have to
/// have become trampolines on the way in - the source's from inside a structure
/// this runtime copied, the timer's from a register.
pub const RUNLOOP_C: &str = r#"
#include <CoreFoundation/CoreFoundation.h>
#include <stdio.h>

static int source_calls = 0;
static int timer_calls = 0;
static CFRunLoopSourceRef the_source;

static void performed(void *info) {
    source_calls++;
    printf("source %d info %ld\n", source_calls, (long)(intptr_t)info);
}

static void fired(CFRunLoopTimerRef timer, void *info) {
    timer_calls++;
    printf("timer %d info %ld\n", timer_calls, (long)(intptr_t)info);
    /* Signalled from inside a timer callback, so the next turn of the loop
       services it: a source and a timer in the same loop, in order. */
    CFRunLoopSourceSignal(the_source);
    CFRunLoopWakeUp(CFRunLoopGetCurrent());
}

int main(void) {
    CFRunLoopSourceContext sc;
    memset(&sc, 0, sizeof sc);
    sc.info = (void *)(intptr_t)42;
    sc.perform = performed;
    the_source = CFRunLoopSourceCreate(NULL, 0, &sc);
    CFRunLoopAddSource(CFRunLoopGetCurrent(), the_source, kCFRunLoopDefaultMode);

    CFRunLoopTimerContext tc;
    memset(&tc, 0, sizeof tc);
    tc.info = (void *)(intptr_t)7;
    CFRunLoopTimerRef timer = CFRunLoopTimerCreate(
        NULL, CFAbsoluteTimeGetCurrent() + 0.01, 0.01, 0, 0, fired, &tc);
    CFRunLoopAddTimer(CFRunLoopGetCurrent(), timer, kCFRunLoopDefaultMode);

    while (timer_calls < 2 || source_calls < 2) {
        if (CFRunLoopRunInMode(kCFRunLoopDefaultMode, 1.0, true) == kCFRunLoopRunFinished) {
            printf("the loop emptied\n");
            break;
        }
    }
    CFRunLoopTimerInvalidate(timer);
    printf("done %d %d\n", source_calls >= 2, timer_calls >= 2);
    return 0;
}
"#;

/// A guest that `dlopen`s a framework by its install name and asks it for a
/// symbol.
///
/// The frameworks this tree implements have no file on disk, so the filesystem
/// search a `dlopen` ends in can never find one. Every line here is a claim
/// about the handle a guest gets instead.
pub const DLOPEN_FRAMEWORK_C: &str = r#"
#include <dlfcn.h>
#include <stdio.h>
int main(void) {
    const char *cf = "/System/Library/Frameworks/CoreFoundation.framework/Versions/A/CoreFoundation";
    void *h = dlopen(cf, RTLD_LAZY);
    printf("handle %d\n", h != NULL);
    void *f = h ? dlsym(h, "CFStringGetLength") : NULL;
    printf("scoped %d\n", f != NULL);
    printf("absent %d\n", h ? dlsym(h, "NoSuchFunctionAnywhere") != NULL : 0);
    printf("global %d\n", dlsym(RTLD_DEFAULT, "CFStringGetLength") != NULL);
    return 0;
}
"#;

/// Build [`DLOPEN_FRAMEWORK_C`], or `None` if this host cannot.
pub fn dlopen_framework_fixture() -> Option<String> {
    static BUILT: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    BUILT
        .get_or_init(|| build_x86_64("dlopen_framework", DLOPEN_FRAMEWORK_C, &[]))
        .clone()
}

/// Build [`RUNLOOP_C`], or `None` if this host cannot.
pub fn runloop_fixture() -> Option<String> {
    static BUILT: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    BUILT
        .get_or_init(|| build_x86_64("runloop", RUNLOOP_C, &["-framework", "CoreFoundation"]))
        .clone()
}

/// Build [`CFSTRING_C`], or `None` if this host cannot.
pub fn cfstring_fixture() -> Option<String> {
    static BUILT: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    BUILT
        .get_or_init(|| build_x86_64("cfstring", CFSTRING_C, &["-framework", "CoreFoundation"]))
        .clone()
}

/// A guest that reaches an import by **tail jump**, with an argument on the
/// stack.
///
/// `musttail` rather than trusting the optimiser: clang will not tail-call when
/// the callee needs more outgoing stack than the caller was given, so the
/// forwarding wrapper takes the same seven arguments it passes on. That is not a
/// contrived shape - a forwarding wrapper is the ordinary reason a tail call to
/// an import exists at all.
///
/// The seventh argument is the one that matters: SysV puts six integers in
/// registers, so `flags` is on the stack, and at a tail `jmp` it sits one slot
/// above where an intercepted `call` would leave it. Losing it turns
/// `NI_NUMERICHOST | NI_NUMERICSERV` into zero, and the guest prints `localhost`
/// and `http` where it asked for `127.0.0.1` and `80` - a wrong answer with no
/// fault anywhere, which is exactly the failure this fixture exists to make
/// loud.
pub const TAILCALL_C: &str = r#"
#include <netdb.h>
#include <netinet/in.h>
#include <stdio.h>
#include <string.h>

__attribute__((noinline))
static int fwd(const struct sockaddr *sa, socklen_t sl, char *h, socklen_t hl,
               char *s, socklen_t sl2, int flags) {
    __attribute__((musttail)) return getnameinfo(sa, sl, h, hl, s, sl2, flags);
}

static char host[128], serv[64];

int main(int argc, char **argv) {
    (void)argv;
    struct sockaddr_in a;
    memset(&a, 0, sizeof a);
    a.sin_len = sizeof a;
    a.sin_family = AF_INET;
    a.sin_port = htons(80);
    a.sin_addr.s_addr = htonl(0x7f000001);
    /* Opaque to the optimiser, so the flags word really travels. */
    int flags = argc > 99 ? 0 : (NI_NUMERICHOST | NI_NUMERICSERV);
    int rc = fwd((struct sockaddr *)&a, sizeof a, host, sizeof host,
                 serv, sizeof serv, flags);
    printf("rc %d host %s serv %s\n", rc, host, serv);
    return 0;
}
"#;

/// Build [`TAILCALL_C`], or `None` if this host cannot.
pub fn tailcall_fixture() -> Option<String> {
    static BUILT: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    BUILT
        .get_or_init(|| build_x86_64("tailcall", TAILCALL_C, &[]))
        .clone()
}

/// A guest that calls host functions through **pointers** rather than at call
/// sites, which is the shape [`rosetta3::trampoline`] exists for.
///
/// Every call here is one microlift cannot see: the value is loaded out of an
/// import slot or handed back by `dlsym`, so nothing at the call site names a
/// symbol. Each of the three answers a different thing:
///
/// - **The slot**, which is the value the loader wrote and this runtime
///   substitutes.
/// - **`dlsym` against the host's own namespace**, and the *identity* of the two
///   answers. A program that resolves a symbol twice by two routes must get one
///   pointer, and a substitution that produced a fresh stub per route would be
///   observably wrong here rather than three months later.
/// - **Seven arguments**, so a stack argument really travels. This is the whole
///   correctness argument for encoding the stub as a tail `jmp` - the seventh
///   integer argument of a SysV call is on the stack, and a stub that pushed a
///   return address under it would shift every one of them by a slot. It is the
///   same failure `TAILCALL_C` was written for, reached from the other side.
pub const TRAMPOLINE_C: &str = r#"
#include <arpa/inet.h>
#include <dlfcn.h>
#include <netdb.h>
#include <netinet/in.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>

/* `volatile`, so the call really goes through the pointer and clang does not
   turn it back into a call site - which would test the path that already
   worked. */
static size_t (*volatile len_slot)(const char *) = strlen;
static int (*volatile gni_slot)(const struct sockaddr *, socklen_t, char *,
                                socklen_t, char *, socklen_t, int) = getnameinfo;

int main(void) {
    printf("slot len=%zu\n", len_slot("abcdef"));

    size_t (*len_dl)(const char *) =
        (size_t (*)(const char *))dlsym(RTLD_DEFAULT, "strlen");
    printf("dlsym len=%zu same=%d\n", len_dl ? len_dl("abcdefgh") : 0,
           (void *)len_dl == (void *)len_slot);

    char host[64], serv[32];
    struct sockaddr_in a;
    memset(&a, 0, sizeof a);
    a.sin_len = sizeof a;
    a.sin_family = AF_INET;
    a.sin_port = htons(80);
    a.sin_addr.s_addr = htonl(0x7f000001);
    int rc = gni_slot((struct sockaddr *)&a, sizeof a, host, sizeof host,
                      serv, sizeof serv, NI_NUMERICHOST | NI_NUMERICSERV);
    printf("gni rc=%d host=%s serv=%s\n", rc, host, serv);
    return 0;
}
"#;

/// Build [`TRAMPOLINE_C`], or `None` if this host cannot.
pub fn trampoline_fixture() -> Option<String> {
    static BUILT: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    BUILT
        .get_or_init(|| build_x86_64("trampoline", TRAMPOLINE_C, &[]))
        .clone()
}

/// A guest that shares a 128-bit location between threads, as Unity does.
///
/// `__sync_bool_compare_and_swap` on a `__int128` with `-mcx16` compiles to
/// `lock cmpxchg16b`, which is the instruction the whole of
/// [`crate::atomic128`] exists for. Four threads each add one to the low half
/// ten thousand times through the CAS, and the total is checked - so a
/// non-atomic emulation loses updates and prints the wrong number rather than
/// passing quietly.
///
/// The pair is also read back as *two* halves whose relationship must hold:
/// the high half counts successful swaps, so `lo == hi` says no thread ever saw
/// a torn pair. A load/compare/store emulation can produce equal-but-wrong
/// totals under luck; it cannot keep both invariants across forty thousand
/// contended swaps.
pub const CMPXCHG16B_C: &str = r#"
#include <pthread.h>
#include <stdio.h>

typedef struct { unsigned long lo, hi; } pair;
static _Alignas(16) pair shared;

static void *worker(void *unused) {
    (void)unused;
    for (int i = 0; i < 10000; i++) {
        for (;;) {
            pair seen = shared;
            pair next = { seen.lo + 1, seen.hi + 1 };
            __int128 want, give;
            __builtin_memcpy(&want, &seen, 16);
            __builtin_memcpy(&give, &next, 16);
            if (__sync_bool_compare_and_swap((__int128 *)&shared, want, give)) break;
        }
    }
    return 0;
}

int main(void) {
    pthread_t t[4];
    for (int i = 0; i < 4; i++) pthread_create(&t[i], 0, worker, 0);
    for (int i = 0; i < 4; i++) pthread_join(t[i], 0);
    printf("lo %lu hi %lu torn %d\n", shared.lo, shared.hi, shared.lo != shared.hi);
    return 0;
}
"#;

/// Build [`CMPXCHG16B_C`], or `None` if this host cannot.
pub fn cmpxchg16b_fixture() -> Option<String> {
    static BUILT: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    BUILT
        .get_or_init(|| build_x86_64("cmpxchg16b", CMPXCHG16B_C, &["-mcx16"]))
        .clone()
}

/// A guest that faults, on purpose, at an address it chose.
///
/// The sentinel is ASCII on purpose too: `crate::fault`'s report says so when a
/// faulting address is printable, because that is what turned Below Zero's
/// crash from "a bad pointer" into "a pointer read out of a string". A fixture
/// that only faulted at 1 would leave that branch untested.
///
/// `volatile` so the store is not folded away, and the pointer arrives through
/// `strtoull` so nothing can constant-fold the dereference into a trap the
/// compiler emits itself.
pub const FAULT_C: &str = r#"
#include <stdio.h>
#include <stdlib.h>

int main(void) {
    printf("about to fault\n");
    fflush(stdout);
    volatile long *p = (volatile long *)strtoull("0x4141414141414141", NULL, 16);
    *p = 1;
    printf("still here\n");
    return 0;
}
"#;

/// A guest that issues a raw `syscall`, which is the one thing high-level
/// emulation cannot forward: the host's own libSystem makes the real calls, so
/// a `syscall` in guest code is hand-written assembly with no shim behind it.
///
/// `write(2)` rather than something exotic, so that a runtime which ever does
/// translate one has an obvious first case - and so a *failure* to refuse it is
/// visible on stdout rather than silent.
pub const SYSCALL_C: &str = r#"
#include <stdio.h>

int main(void) {
    printf("before\n");
    fflush(stdout);
    __asm__ volatile(
        "movq $0x2000004, %%rax\n\t"
        "movq $1, %%rdi\n\t"
        "leaq 1f(%%rip), %%rsi\n\t"
        "movq $4, %%rdx\n\t"
        "syscall\n\t"
        "jmp 2f\n\t"
        "1: .ascii \"raw\\n\"\n\t"
        "2:"
        ::: "rax", "rdi", "rsi", "rdx", "rcx", "r11", "memory");
    printf("after\n");
    return 0;
}
"#;

/// Build [`SYSCALL_C`], or `None` if this host cannot.
pub fn syscall_fixture() -> Option<String> {
    static BUILT: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    BUILT
        .get_or_init(|| build_x86_64("guest_syscall", SYSCALL_C, &[]))
        .clone()
}

/// Build [`FAULT_C`], or `None` if this host cannot.
pub fn fault_fixture() -> Option<String> {
    static BUILT: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    BUILT
        .get_or_init(|| build_x86_64("guest_fault", FAULT_C, &[]))
        .clone()
}

/// A program with one loop the profiler has to find, and a second one it has to
/// *not* attribute to the guest.
///
/// `spin` is arithmetic microlift translates, so its samples are in a
/// translation and must resolve to a guest instruction inside this image.
/// `chatty` leaves the guest on every iteration, so its cost belongs to the
/// runtime and to the host's libraries and must not be counted as translated.
/// A profiler that mixed the two would be answering the one question M8.2 was
/// opened to ask - is this exit-bound or code-quality-bound - with a coin toss.
///
/// The iteration counts are chosen for about a second of work on an M-series
/// host, which at the default rate is hundreds of samples: enough for a share
/// to mean something, short enough to sit in a test suite.
pub const PROFILE_C: &str = r#"
#include <stdio.h>
#include <stdlib.h>

static unsigned long spin(unsigned long n) {
    unsigned long a = 1;
    for (unsigned long i = 0; i < n; i++) {
        a = a * 6364136223846793005UL + 1442695040888963407UL;
        a ^= a >> 33;
    }
    return a;
}

static unsigned long chatty(unsigned long n) {
    unsigned long a = 0;
    char buf[32];
    for (unsigned long i = 0; i < n; i++) {
        snprintf(buf, sizeof buf, "%lu", i);
        a += (unsigned long)atoi(buf);
    }
    return a;
}

int main(void) {
    printf("%lu %lu\n", spin(200000000UL), chatty(100000UL));
    return 0;
}
"#;

/// Build [`PROFILE_C`], or `None` if this host cannot.
pub fn profile_fixture() -> Option<String> {
    static BUILT: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    BUILT
        .get_or_init(|| build_x86_64("guest_profile", PROFILE_C, &["-O1"]))
        .clone()
}

/// Build [`TLS_C`], or `None` if this host cannot.
pub fn tls_fixture() -> Option<String> {
    static BUILT: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    BUILT
        .get_or_init(|| build_x86_64("thread_locals", TLS_C, &[]))
        .clone()
}

/// Compile `source` for the **host's** architecture and return its path.
///
/// The counterpart to [`build_x86_64`], for tests whose claim is that the two
/// architectures agree about something.
pub fn build_native(name: &str, source: &str) -> Option<String> {
    build_native_with(name, source, &[])
}

/// [`build_native`], with extra compiler arguments.
///
/// A fixture that links a framework needs them, and a native build that quietly
/// fails to link is worse than no comparison at all: the test that would have
/// caught the difference skips instead, and reports success.
pub fn build_native_with(name: &str, source: &str, extra: &[&str]) -> Option<String> {
    let dir = fixture_dir();
    let src = dir.join(format!("{name}.c"));
    let out = dir.join(name);
    std::fs::write(&src, source).ok()?;

    // Before the source, as [`build_x86_64`] has them: `cc` ignores a `-x c++`
    // that comes after the last input file, and says so only as a warning.
    let status = Command::new("cc")
        .args(extra)
        .arg("-o")
        .arg(&out)
        .arg(&src)
        .status()
        .ok()?;
    if !status.success() {
        return None;
    }
    // And it has to be a Mach-O, which is the whole point of a "native" fixture:
    // it exists to be run by the native backend, and that backend loads Mach-O. On a
    // host that is not a Mac, `cc` succeeds and produces an ELF, and a test handed one
    // of those fails on a magic number rather than skipping.
    host_macho(out.to_str()?).then(|| out.to_string_lossy().into_owned())
}

/// Run a program **natively**, as the host's own copy of it.
///
/// The only assertion worth making about a pthread or `errno` fixture: what the
/// program prints on this machine when nothing translates it. Rosetta 2 runs the
/// x86_64 build directly, so the comparison is against the same binary rather
/// than a recompilation of the same source - and when it is not available the
/// caller skips rather than fails.
pub fn run_native(path: &str) -> Option<(Option<i32>, String)> {
    run_native_with(path, &[])
}

/// [`run_native`], with arguments - for a fixture that selects a case by one.
pub fn run_native_with(path: &str, args: &[&str]) -> Option<(Option<i32>, String)> {
    let out = Command::new(path).args(args).output().ok()?;
    out.status.code().is_some().then(|| {
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
        )
    })
}

/// Build [`LINK_C`], or `None` if this host cannot.
///
/// `-O1`, unlike most fixtures here: at `-O0` the calls this exists for are
/// still there, but so is a stack frame's worth of spill around each of them,
/// and the point is a loop whose cost is the transfers.
pub fn link_fixture() -> Option<String> {
    static BUILT: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    BUILT
        .get_or_init(|| build_x86_64("blocklink", LINK_C, &["-O1"]))
        .clone()
}

/// Build [`POLYLINK_C`], or `None` if this host cannot.
///
/// `-O1` for the same reason [`link_fixture`] uses it.
pub fn polylink_fixture() -> Option<String> {
    static BUILT: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    BUILT
        .get_or_init(|| build_x86_64("polylink", POLYLINK_C, &["-O1"]))
        .clone()
}

/// Build [`QSORT_C`], or `None` if this host cannot.
pub fn qsort_fixture() -> Option<String> {
    static BUILT: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    BUILT
        .get_or_init(|| build_x86_64("qsort_cb", QSORT_C, &[]))
        .clone()
}

/// Build [`CTOR_C`], or `None` if this host cannot.
pub fn ctor_fixture() -> Option<String> {
    static BUILT: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    BUILT
        .get_or_init(|| build_x86_64("ctors", CTOR_C, &[]))
        .clone()
}

/// Build [`DTOR_C`], or `None` if this host cannot.
pub fn dtor_fixture() -> Option<String> {
    static BUILT: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    BUILT
        .get_or_init(|| build_x86_64("dtors", DTOR_C, &[]))
        .clone()
}

/// Build [`LATE_CALLBACK_C`], or `None` if this host cannot.
pub fn late_callback_fixture() -> Option<String> {
    static BUILT: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    BUILT
        .get_or_init(|| build_x86_64("late_callback", LATE_CALLBACK_C, &[]))
        .clone()
}

/// The `v`-prefixed formatting family, against a guest-built `va_list`.
///
/// Nine of each class in one call is the point: SysV AMD64's register save area
/// holds six integers and eight vectors, so the ninth of each is in the
/// *overflow* area, reached by a different pointer that advances by a different
/// step - eight bytes rather than the sixteen a saved vector slot occupies. A
/// walker that used one step for both prints the later arguments from between
/// two others, which looks like corruption rather than like an ABI bug.
///
/// The three destinations are here because each writes somewhere different: a
/// stream, a bounded guest buffer that must truncate, and a host allocation
/// whose pointer is written back through a guest pointer.
pub const VALIST_C: &str = r#"
#include <stdio.h>
#include <stdarg.h>
#include <stdlib.h>

static void logline(const char *fmt, ...) {
    va_list ap; va_start(ap, fmt);
    vfprintf(stdout, fmt, ap);
    va_end(ap);
}
static void tobuf(char *out, size_t n, const char *fmt, ...) {
    va_list ap; va_start(ap, fmt);
    vsnprintf(out, n, fmt, ap);
    va_end(ap);
}
static char *alloced(const char *fmt, ...) {
    va_list ap; va_start(ap, fmt); char *p = NULL;
    vasprintf(&p, fmt, ap);
    va_end(ap);
    return p;
}

int main(void) {
    logline("ints %d %d %d %d %d %d %d %d %d\n", 1, 2, 3, 4, 5, 6, 7, 8, 9);
    logline("fps %.1f %.1f %.1f %.1f %.1f %.1f %.1f %.1f %.1f\n",
            1.5, 2.5, 3.5, 4.5, 5.5, 6.5, 7.5, 8.5, 9.5);
    logline("mixed %s %d %.2f %s %d\n", "a", 7, 2.25, "b", 8);
    char buf[16];
    tobuf(buf, sizeof buf, "%s-%d-%s", "truncate", 42, "me");
    printf("bounded %s\n", buf);
    char *p = alloced("alloc %d %s", 5, "ok");
    printf("%s\n", p);
    free(p);
    return 0;
}
"#;

/// Build [`VALIST_C`], or `None` if this host cannot.
pub fn valist_fixture() -> Option<String> {
    static BUILT: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    BUILT
        .get_or_init(|| build_x86_64("valist", VALIST_C, &[]))
        .clone()
}

/// A dylib the guest loads *itself*, after it has started running.
///
/// Three things in it are load-bearing, and each fails differently:
///
/// - **A constructor**, so the test can tell "mapped and bound" from "mapped,
///   bound, and started" - `dlopen` must run initializers before it returns.
/// - **A call to `strlen`**, which is the whole reason this is hard. This
///   image's call sites were never registered with the lifter, so the call
///   leaves microlift as an `UnknownJump` and is answered from the import table
///   at the exit. Without that the recompiler decodes the `__stubs` trampoline
///   as guest code and follows it into a native arm64 `strlen`.
/// - **A function pointer handed back**, so the `dlsym` result is entered
///   through the JIT rather than called as arm64.
pub const PLUGIN_C: &str = r#"
#include <stdio.h>
#include <string.h>

__attribute__((constructor)) static void plugin_init(void) { puts("plugin loaded"); }

int plugin_len(const char *s) { return (int)strlen(s); }
"#;

/// The program that loads [`PLUGIN_C`], named without a directory - which is
/// what a bundled application does, and what makes the search beside the
/// executable load-bearing rather than a convenience.
pub const DLOPEN_C: &str = r#"
#include <dlfcn.h>
#include <stdio.h>

int main(void) {
    void *h = dlopen("libplugin.dylib", RTLD_NOW);
    if (!h) { printf("dlopen failed: %s\n", dlerror()); return 1; }
    int (*len)(const char *) = (int (*)(const char *))dlsym(h, "plugin_len");
    if (!len) { printf("dlsym failed: %s\n", dlerror()); return 2; }
    printf("len=%d\n", len("abcdef"));
    /* A name nothing defines must be null *and* leave a message behind. */
    void *missing = dlsym(h, "plugin_no_such_symbol");
    const char *why = dlerror();
    printf("missing=%d reported=%d\n", missing == NULL, why != NULL);
    dlclose(h);
    return 0;
}
"#;

/// Build [`DLOPEN_C`] and the plugin beside it, or `None` if this host cannot.
///
/// The dylib is built first and into the same directory, because the program
/// finds it by name relative to its own path.
pub fn dlopen_fixture() -> Option<String> {
    static BUILT: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    BUILT
        .get_or_init(|| {
            build_x86_64(
                "libplugin.dylib",
                PLUGIN_C,
                &["-dynamiclib", "-install_name", "@rpath/libplugin.dylib"],
            )?;
            build_x86_64("dlopener", DLOPEN_C, &[])
        })
        .clone()
}

/// A dylib the guest loads itself, carrying **Objective-C classes of its own**.
///
/// The launch-time registration pass runs over the program's images, and an
/// image mapped later is not one of them - so until [`crate::dyld::open`] runs
/// the same pass, everything here fails, and each line fails differently:
///
/// - **`+load`** must run inside `dlopen`, before the C constructor of the same
///   image, which is dyld's order. Printing both is how the order is checked
///   rather than asserted.
/// - **A message to the plugin's own class** - `[Plugin greet]` - is the case
///   that was refused by name: the guest names its class by the dead
///   `objc_class` in its own `__objc_data`, and until that class is realized
///   there is nothing for the host runtime to look a method up on.
/// - **A subclass of a Foundation class**, whose superclass is an ordinary bind
///   the host answers, with an ivar read through the runtime's offset - the
///   half of registration that writes back into the image.
/// - **A message to Foundation**, which needs only the selectors uniqued and
///   worked before any of this.
pub const OBJC_PLUGIN_M: &str = r#"
#import <Foundation/Foundation.h>
#include <stdio.h>

@interface Plugin : NSObject { int count; }
+ (void)load;
+ (int)greet;
- (int)bump;
@end

@implementation Plugin
+ (void)load { puts("plugin +load"); }
+ (int)greet { return 41; }
- (int)bump { count += 1; return count; }
@end

__attribute__((constructor)) static void plugin_init(void) { puts("plugin ctor"); }

int plugin_run(void) {
    Plugin *p = [[Plugin alloc] init];
    int n = [Plugin greet] + [p bump];
    NSString *s = [NSString stringWithUTF8String:"twelve"];
    return n + (int)[s length];
}
"#;

/// The program that loads [`OBJC_PLUGIN_M`] and calls into it.
pub const OBJC_DLOPEN_M: &str = r#"
#import <Foundation/Foundation.h>
#include <dlfcn.h>
#include <stdio.h>

/* The program declares a class too, so the loaded image's registration has to
   *extend* an existing runtime rather than build one. */
@interface Host : NSObject
+ (int)base;
@end
@implementation Host
+ (int)base { return 100; }
@end

int main(void) {
    printf("host=%d\n", [Host base]);
    void *h = dlopen("@executable_path/libobjcplugin.dylib", RTLD_NOW);
    if (!h) { printf("dlopen failed: %s\n", dlerror()); return 1; }
    int (*run)(void) = (int (*)(void))dlsym(h, "plugin_run");
    if (!run) { printf("dlsym failed: %s\n", dlerror()); return 2; }
    printf("plugin=%d\n", run());
    return 0;
}
"#;

/// Build [`OBJC_DLOPEN_M`] and its plugin beside it, or `None` if this host
/// cannot.
pub fn objc_dlopen_fixture() -> Option<String> {
    static BUILT: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    BUILT
        .get_or_init(|| {
            build_x86_64(
                "libobjcplugin.dylib",
                OBJC_PLUGIN_M,
                &[
                    "-x",
                    "objective-c",
                    "-framework",
                    "Foundation",
                    "-dynamiclib",
                    "-install_name",
                    "@rpath/libobjcplugin.dylib",
                ],
            )?;
            build_objc("objcdlopener", OBJC_DLOPEN_M)
        })
        .clone()
}

/// Run an x86-64 guest program under the driver, returning `(status, stdout, stderr)`.
/// through [`run_guest_with`] directly.
pub fn run_guest(path: &str, args: &[&str]) -> (Option<i32>, String, String) {
    run_guest_with(&["--emulated"], path, args)
}

/// [`run_guest`], with environment variables set for the driver.
///
/// Most of what this runtime can be asked to do differently is asked for that
/// way, through a `ROSETTA3_*` variable.
///
/// `--emulated`, for the reason [`run_guest`] gives.
pub fn run_guest_env(
    env: &[(&str, &str)],
    path: &str,
    args: &[&str],
) -> (Option<i32>, String, String) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_rosetta3"));
    command.arg("run").arg("--emulated").arg(path).args(args);
    for (key, value) in env {
        command.env(key, value);
    }
    let out = command.output().expect("running the driver");
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// The driver binary, for a test that has to launch it itself.
pub fn driver() -> &'static str {
    env!("CARGO_BIN_EXE_rosetta3")
}

/// [`run_guest`], with extra driver flags before the program path.
///
/// Unlike [`run_guest`] this states nothing of its own, so a caller handing it an
/// x86-64 fixture must pass `--emulated` itself - see [`run_guest`] for why that
/// matters on an x86-64 host.
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

/// Whether this host has a Mach-O at `path` to use as a fixture.
///
/// Two ways to have none, and the second is the one that matters: the file may be
/// missing, or - on a host that is not a Mac - it may be there and be an ELF. A test
/// that reads `/bin/ls` is reading *this machine's own* Mach-O, so on Linux the honest
/// answer is that there is not one and the test skips.
pub fn host_macho(path: &str) -> bool {
    let Ok(bytes) = std::fs::read(path) else {
        return false;
    };
    let Some(magic) = bytes.get(..4) else {
        return false;
    };
    let magic = u32::from_be_bytes([magic[0], magic[1], magic[2], magic[3]]);
    matches!(
        magic,
        // Thin, 32- and 64-bit, either endianness...
        0xfeed_face | 0xcefa_edfe | 0xfeed_facf | 0xcffa_edfe
        // ...and fat, likewise.
        | 0xcafe_babe | 0xbeba_feca | 0xcafe_babf | 0xbfba_feca
    )
}

/// The AppKit fixture: a program whose `main` is `NSApplicationMain`.
///
/// Written so each step of the launch fails on its own line rather than as one
/// blob:
///
/// - **the nib is read** - the delegate is wired in `MainMenu.nib` and nowhere
///   else, so a `delegate is self 1` means the archive was decoded, the custom
///   object was instantiated by name, and the outlet reached `-setDelegate:`;
/// - **the notifications arrive in order**, each carrying its own name, which is
///   what makes the argument a real `NSNotification` rather than a register;
/// - **the property metadata survived the replay**, which nothing else in this
///   suite asks: a `@property` compiles to a name and an attribute string beside
///   the accessors, and a runtime that drops them answers the same as a class
///   that never declared any.
///
/// `[NSApp stop:]` rather than `terminate:`, so that `-run` returns and `main`
/// returns a status the test can read.
pub const APPKIT_M: &str = r####"
#import <Cocoa/Cocoa.h>
#import <objc/runtime.h>
#include <stdio.h>

@interface R3Delegate : NSObject
@property (nonatomic, assign) int launches;
@end

@implementation R3Delegate
- (void)applicationWillFinishLaunching:(NSNotification *)note {
    printf("will %s\n", [[note name] UTF8String]);
}
- (void)applicationDidFinishLaunching:(NSNotification *)note {
    printf("did %s\n", [[note name] UTF8String]);
    printf("delegate is self %d\n", [NSApp delegate] == self);

    unsigned int count = 0;
    objc_property_t *properties = class_copyPropertyList([self class], &count);
    printf("properties %u\n", count);
    for (unsigned int i = 0; i < count; i++) {
        printf("property %s %s\n",
               property_getName(properties[i]),
               property_getAttributes(properties[i]));
        unsigned int fields = 0;
        objc_property_attribute_t *list =
            property_copyAttributeList(properties[i], &fields);
        printf("attributes %u first %s=%s\n", fields, list[0].name, list[0].value);
        free(list);
    }
    free(properties);

    fflush(stdout);
    [NSApp stop:nil];
}
@end

int main(int argc, const char *argv[]) {
    return NSApplicationMain(argc, argv);
}
"####;

/// `MainMenu.nib`: File's Owner is the application, its `delegate` outlet is an
/// `R3Delegate`.
///
/// The XML spelling of a keyed archive, which is what `plutil -convert xml1`
/// makes of the binary one Interface Builder writes, and which the same reader
/// decodes. A checked-in binary nib would say no more and could not be read.
pub const MAIN_NIB: &str = r####"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0">
<dict>
	<key>$archiver</key>
	<string>NSKeyedArchiver</string>
	<key>$objects</key>
	<array>
		<string>$null</string>
		<dict>
			<key>$class</key><dict><key>CF$UID</key><integer>8</integer></dict>
			<key>NSConnections</key><dict><key>CF$UID</key><integer>4</integer></dict>
			<key>NSRoot</key><dict><key>CF$UID</key><integer>2</integer></dict>
		</dict>
		<dict>
			<key>$class</key><dict><key>CF$UID</key><integer>9</integer></dict>
			<key>NSClassName</key><dict><key>CF$UID</key><integer>3</integer></dict>
		</dict>
		<string>NSApplication</string>
		<dict>
			<key>$class</key><dict><key>CF$UID</key><integer>10</integer></dict>
			<key>NS.objects</key>
			<array><dict><key>CF$UID</key><integer>5</integer></dict></array>
		</dict>
		<dict>
			<key>$class</key><dict><key>CF$UID</key><integer>11</integer></dict>
			<key>NSSource</key><dict><key>CF$UID</key><integer>2</integer></dict>
			<key>NSDestination</key><dict><key>CF$UID</key><integer>6</integer></dict>
			<key>NSLabel</key><dict><key>CF$UID</key><integer>7</integer></dict>
		</dict>
		<dict>
			<key>$class</key><dict><key>CF$UID</key><integer>9</integer></dict>
			<key>NSClassName</key><dict><key>CF$UID</key><integer>12</integer></dict>
		</dict>
		<string>delegate</string>
		<dict>
			<key>$classes</key><array><string>NSIBObjectData</string></array>
			<key>$classname</key><string>NSIBObjectData</string>
		</dict>
		<dict>
			<key>$classes</key><array><string>NSCustomObject</string></array>
			<key>$classname</key><string>NSCustomObject</string>
		</dict>
		<dict>
			<key>$classes</key><array><string>NSMutableArray</string><string>NSArray</string></array>
			<key>$classname</key><string>NSMutableArray</string>
		</dict>
		<dict>
			<key>$classes</key><array><string>NSNibOutletConnector</string></array>
			<key>$classname</key><string>NSNibOutletConnector</string>
		</dict>
		<string>R3Delegate</string>
	</array>
	<key>$top</key>
	<dict>
		<key>IB.objectdata</key><dict><key>CF$UID</key><integer>1</integer></dict>
	</dict>
	<key>$version</key>
	<integer>100000</integer>
</dict>
</plist>
"####;

/// Build `source` as an Objective-C `.app`, and return the path of the
/// executable *inside* it.
///
/// The layout is what `NSBundle` recognises by shape - `Contents/MacOS/<exe>`,
/// `Contents/Info.plist`, `Contents/Resources` - because that is what
/// `foundation::bundle` looks for, and a fixture laid out any other way would
/// test the fallback rather than the bundle.
pub fn build_app(name: &str, source: &str, info: &str, nib: Option<&str>) -> Option<String> {
    if !can_build_x86_64() {
        return None;
    }
    let app = fixture_dir().join(format!("{name}.app"));
    let contents = app.join("Contents");
    let resources = contents.join("Resources");
    std::fs::create_dir_all(contents.join("MacOS")).ok()?;
    std::fs::create_dir_all(&resources).ok()?;
    std::fs::write(contents.join("Info.plist"), info).ok()?;
    if let Some(nib) = nib {
        std::fs::write(resources.join("MainMenu.nib"), nib).ok()?;
    }

    let src = fixture_dir().join(format!("{name}.m"));
    std::fs::write(&src, source).ok()?;
    let out = contents.join("MacOS").join(name);
    let status = Command::new("cc")
        .args([
            "-arch",
            "x86_64",
            "-x",
            "objective-c",
            "-framework",
            "Cocoa",
        ])
        .arg("-o")
        .arg(&out)
        .arg(&src)
        .status()
        .ok()?;
    status.success().then(|| out.to_string_lossy().into_owned())
}

/// Build [`APPKIT_M`] into a `.app` whose `Info.plist` names the nib, or `None`
/// if this host cannot.
pub fn appkit_fixture() -> Option<String> {
    static BUILT: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    BUILT
        .get_or_init(|| {
            build_app(
                "appkitmain",
                APPKIT_M,
                r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0">
<dict>
	<key>CFBundleExecutable</key><string>appkitmain</string>
	<key>CFBundleIdentifier</key><string>org.rosetta3.appkitmain</string>
	<key>NSPrincipalClass</key><string>NSApplication</string>
	<key>NSMainNibFile</key><string>MainMenu</string>
</dict>
</plist>
"#,
                Some(MAIN_NIB),
            )
        })
        .clone()
}

/// The framework-globals fixture: read three `CFStringRef` and `CFAllocatorRef`
/// globals and use them.
///
/// Each is a *variable* the guest loads through, not the object itself, so a
/// provider that defines the symbol as the object hands the guest the object's
/// `isa` - and `kCFAllocatorDefault`, whose value is null, hands it address
/// zero. The failure is a segfault at zero, or a length read out of a class
/// structure, and neither says which symbol did it.
pub const GLOBALS_C: &str = r#"
#include <CoreFoundation/CoreFoundation.h>
#include <stdio.h>

int main(void) {
    printf("mode %ld\n", (long)CFStringGetLength(kCFRunLoopDefaultMode));
    printf("common %ld\n", (long)CFStringGetLength(kCFRunLoopCommonModes));
    printf("null allocator %d\n", kCFAllocatorDefault == NULL);
    CFStringRef made =
        CFStringCreateWithCString(kCFAllocatorDefault, "abc", kCFStringEncodingUTF8);
    printf("made %ld\n", (long)CFStringGetLength(made));
    CFRelease(made);
    return 0;
}
"#;

pub fn globals_fixture() -> Option<String> {
    static BUILT: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    BUILT
        .get_or_init(|| build_x86_64("cfglobals", GLOBALS_C, &["-framework", "CoreFoundation"]))
        .clone()
}

/// Skip the calling test with an explanatory note.
#[macro_export]
macro_rules! skip {
    ($($arg:tt)*) => {{
        eprintln!("skipping {}: {}", module_path!(), format!($($arg)*));
        return;
    }};
}
