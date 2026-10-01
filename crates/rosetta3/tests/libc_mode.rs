mod common;

#[cfg(target_os = "macos")]
#[test]
fn builtin_libc_agrees_with_native_for_small_guest_calls() {
    const SOURCE: &str = r#"
#include <glob.h>
#include <resolv.h>
#include <regex.h>
#include <stdio.h>
#include <string.h>
#include <sys/sysctl.h>
#include <time.h>

extern int timingsafe_bcmp(const void *, const void *, size_t);

int main(int argc, char **argv) {
    if (argc != 2) return 10;
    glob_t paths = {0};
    if (glob(argv[1], 0, NULL, &paths) != 0) return 11;
    printf("glob=%zu:%s\n", paths.gl_pathc, paths.gl_pathv[0]);
    globfree(&paths);

    char machine[32];
    size_t length = sizeof machine;
    if (sysctlbyname("hw.machine", machine, &length, NULL, 0) != 0) return 12;
    printf("machine=%s\n", machine);

    struct timespec now;
    if (clock_gettime(CLOCK_REALTIME, &now) != 0 || now.tv_sec <= 0) return 13;
    printf("clock=ok\n");

    if (timingsafe_bcmp("same", "same", 4) != 0 ||
        timingsafe_bcmp("same", "else", 4) == 0) return 14;
    printf("string=ok\n");

    if (res_init() != 0) return 15;
    res_close();
    printf("resolver=ok\n");

    regex_t re = {0};
    regmatch_t found[3];
    if (regcomp(&re, "(ab|a)(b)", REG_EXTENDED) != 0) return 16;
    if (regexec(&re, "abb", 3, found, 0) != 0) return 17;
    printf("regex=%zu:%lld-%lld:%lld-%lld\n", re.re_nsub,
           (long long)found[0].rm_so, (long long)found[0].rm_eo,
           (long long)found[1].rm_so, (long long)found[1].rm_eo);
    regfree(&re);

    char bounded[] = {'a', '\0', 'b'};
    if (regncomp(&re, bounded, sizeof bounded, REG_EXTENDED) != 0) return 18;
    if (regnexec(&re, bounded, sizeof bounded, 3, found, 0) != 0) return 19;
    printf("regex-bounded=%lld-%lld\n", (long long)found[0].rm_so, (long long)found[0].rm_eo);
    regfree(&re);
    return 0;
}
"#;
    let Some(path) = common::build_x86_64("libc_mode_smoke", SOURCE, &["-lresolv"]) else {
        skip!("cannot build x86_64 fixtures");
    };
    let dir = std::env::temp_dir().join(format!("r3-libc-mode-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("alpha");
    std::fs::write(&file, b"x").unwrap();
    let pattern = format!("{}/*", dir.display());
    let environment = [("ROSETTA3_PREFIX", "off"), ("ROSETTA3_LIBC", "native")];
    let native = common::run_guest_env(&environment, &path, &[&pattern]);
    let environment = [("ROSETTA3_PREFIX", "off"), ("ROSETTA3_LIBC", "builtin")];
    let builtin = common::run_guest_env(&environment, &path, &[&pattern]);
    assert_eq!(native.0, Some(0), "native: {}", native.2);
    assert_eq!(builtin.0, Some(0), "builtin: {}", builtin.2);
    assert_eq!(builtin.1, native.1);
    assert!(builtin.1.contains("glob=1:"), "{}", builtin.1);
    assert!(builtin.1.contains("machine=x86_64"), "{}", builtin.1);
    assert!(builtin.1.contains("regex=2:0-3:0-2"), "{}", builtin.1);
    assert!(builtin.1.contains("regex-bounded=0-3"), "{}", builtin.1);
}

#[cfg(target_os = "macos")]
#[test]
fn builtin_pthreads_run_with_guest_attributes_and_callbacks() {
    const SOURCE: &str = r#"
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>

static pthread_mutex_t mutex = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t cond = PTHREAD_COND_INITIALIZER;
static pthread_rwlock_t rwlock = PTHREAD_RWLOCK_INITIALIZER;
static pthread_once_t once = PTHREAD_ONCE_INIT;
static pthread_key_t key;
static int ready, initialized, destroyed;

static void initialize(void) { initialized++; }
static void destroy(void *value) { if (value == (void *)42) destroyed++; }
static void *worker(void *arg) {
    if (pthread_once(&once, initialize) || pthread_once(&once, initialize)) return (void *)1;
    if (pthread_setspecific(key, (void *)42) || pthread_getspecific(key) != (void *)42) return (void *)2;
    if (pthread_rwlock_wrlock(&rwlock) || pthread_rwlock_unlock(&rwlock)) return (void *)3;
    if (pthread_mutex_lock(&mutex)) return (void *)4;
    ready = 1;
    if (pthread_cond_signal(&cond) || pthread_mutex_unlock(&mutex)) return (void *)5;
    return arg;
}
int main(void) {
    pthread_attr_t attr;
    size_t size = 0;
    if (pthread_attr_init(&attr) || pthread_attr_setstacksize(&attr, 1024 * 1024)) return 10;
    if (pthread_attr_getstacksize(&attr, &size) || size != 1024 * 1024) return 11;
    if (pthread_key_create(&key, destroy) || pthread_mutex_lock(&mutex)) return 12;
    pthread_t thread;
    if (pthread_create(&thread, &attr, worker, (void *)99)) return 13;
    if (pthread_attr_destroy(&attr)) return 14;
    while (!ready) if (pthread_cond_wait(&cond, &mutex)) return 15;
    if (pthread_mutex_unlock(&mutex)) return 16;
    void *result = 0;
    if (pthread_join(thread, &result) || result != (void *)99) return 17;
    if (initialized != 1 || destroyed != 1) return 18;
    if (pthread_key_delete(key) || pthread_cond_destroy(&cond) ||
        pthread_mutex_destroy(&mutex) || pthread_rwlock_destroy(&rwlock)) return 19;
    puts("pthread=ok");
    return 0;
}
"#;
    let Some(emulated) = common::build_x86_64("libc_mode_pthread", SOURCE, &[]) else {
        skip!("cannot build x86_64 fixtures");
    };
    let Some(native) = common::build_native("libc_mode_pthread_native", SOURCE) else {
        skip!("cannot build native fixtures");
    };
    for mode in ["native", "builtin"] {
        for (path, flags) in [(&emulated, &["--emulated"][..]), (&native, &[][..])] {
            let output = common::driver_command()
                .env("ROSETTA3_PREFIX", "off")
                .env("ROSETTA3_LIBC", mode)
                .env("ROSETTA3_DEBUG_MUTEX", "1")
                .arg("run")
                .args(flags)
                .arg(path)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{mode} {flags:?}: {:?}: {}",
                output.status.code(),
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(String::from_utf8_lossy(&output.stdout), "pthread=ok\n");
        }
    }
}
