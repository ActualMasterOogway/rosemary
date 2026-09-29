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
