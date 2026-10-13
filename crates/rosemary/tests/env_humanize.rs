mod common;

#[cfg(target_os = "macos")]
const SOURCE: &str = r#"
#include <crt_externs.h>
#include <dlfcn.h>
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static char *find(char **env, const char *name) {
    size_t n = strlen(name);
    for (; env && *env; env++)
        if (!strncmp(*env, name, n) && (*env)[n] == '=') return *env + n + 1;
    return NULL;
}

__attribute__((constructor)) static void initialize_environment(void) {
    setenv("PORT_CONSTRUCTOR", "initialized", 1);
}

int main(int argc, char **argv, char **envp) {
    char ***(*get_slot)(void) = dlsym(RTLD_DEFAULT, "_NSGetEnviron");
    char *(*get_value)(const char *) = dlsym(RTLD_DEFAULT, "getenv");
    int (*set_value)(const char *, const char *, int) = dlsym(RTLD_DEFAULT, "setenv");
    char *(*get_size)(int *, long *) = dlsym(RTLD_DEFAULT, "getbsize");
    if (!get_slot || get_slot() != _NSGetEnviron() || !get_value || !set_value || !get_size) return 9;
    void *system = dlopen("/usr/lib/libSystem.B.dylib", RTLD_LAZY);
    char ***(*scoped_slot)(void) = system ? dlsym(system, "_NSGetEnviron") : NULL;
    if (!scoped_slot || scoped_slot() != _NSGetEnviron()) return 8;
    // main's envp remains the startup snapshot after constructor mutations.
    if (!getenv("PORT_CONSTRUCTOR") || strcmp(getenv("PORT_CONSTRUCTOR"), "initialized") ||
        find(envp, "PORT_CONSTRUCTOR")) return 7;
    const char *names[] = {"HOME", "TMPDIR", "PATH", "PWD", "PORT_INITIAL"};
    for (size_t i = 0; i < sizeof names / sizeof *names; i++) {
        char *a = find(envp, names[i]), *b = getenv(names[i]);
        if (!!a != !!b || (a && strcmp(a, b))) return 10;
        char *c = find(*_NSGetEnviron(), names[i]);
        if (!!a != !!c || (a && strcmp(a, c))) return 11;
    }
    if (!getenv("PORT_INITIAL") || strcmp(getenv("PORT_INITIAL"), "inherited")) return 12;
    if (set_value("PORT_DYNAMIC", "lookup", 1) || strcmp(getenv("PORT_DYNAMIC"), "lookup") ||
        get_value("PORT_DYNAMIC") != getenv("PORT_DYNAMIC") || unsetenv("PORT_DYNAMIC")) return 14;
    if (argc > 1) {
        if (getenv("ROSEMARY_ENVIRON") || getenv("PORT_HOST_ONLY") ||
            strcmp(getenv("PATH"), "/guest/bin")) return 13;
        puts("prefix environment agrees");
        return 0;
    }

    char name[] = "PORT_SET", value[] = "first";
    if (setenv(name, value, 1)) return 20;
    value[0] = 'X';
    if (strcmp(getenv(name), "first") || setenv(name, "second", 0) ||
        strcmp(getenv(name), "first") || setenv(name, "second", -1) ||
        strcmp(getenv(name), "second")) return 21;
    if (strcmp(getenv("PORT_SET=ignored"), "second")) return 22;
    getenv(name)[0] = 'S';
    if (strcmp(getenv(name), "Second")) return 23;
    if (unsetenv(name) || getenv(name) || unsetenv(name)) return 24;
    errno = 0;
    if (setenv("", "bad", 1) != -1 || errno != EINVAL) return 25;
    errno = 0;
    if (setenv("bad=name", "bad", 1) != -1 || errno != EINVAL) return 26;
    errno = 0;
    if (unsetenv("bad=name") != -1 || errno != EINVAL) return 27;

    char entry[] = "PORT_PUT1=one";
    if (putenv(entry) || getenv("PORT_PUT1") != entry + 10) return 30;
    memcpy(entry + 10, "two", 3);
    entry[8] = '2';
    if (getenv("PORT_PUT1") || strcmp(getenv("PORT_PUT2"), "two")) return 31;
    errno = 0;
    char bare[] = "PORT_PUT2";
    if (putenv(bare) != -1 || errno != EINVAL || strcmp(getenv(bare), "two")) return 32;
    if (setenv(bare, "copied", 1)) return 33;
    memcpy(entry + 10, "old", 3);
    if (strcmp(getenv(bare), "copied") || unsetenv(bare)) return 34;
    char empty_name[] = "=empty";
    if (putenv(empty_name) || strcmp(getenv(""), "empty")) return 35;

    char **saved = *_NSGetEnviron();
    char first[] = "PORT_DUP=first", second[] = "PORT_DUP=second";
    char other[] = "PORT_OTHER=kept";
    char *replacement[] = {first, second, other, NULL};
    *_NSGetEnviron() = replacement;
    if (strcmp(getenv("PORT_DUP"), "first") || setenv("PORT_DUP", "changed", 1) ||
        strcmp(getenv("PORT_DUP"), "changed")) return 40;
    if (unsetenv("PORT_DUP") || getenv("PORT_DUP") ||
        strcmp(getenv("PORT_OTHER"), "kept")) return 41;
    if (unsetenv("PORT_OTHER") || **_NSGetEnviron() || getenv("PORT_INITIAL")) return 42;
    if (setenv("PORT_ONLY", "only", 1) || getenv("PORT_INITIAL") ||
        strcmp(getenv("PORT_ONLY"), "only")) return 43;
    *_NSGetEnviron() = saved;
    puts("environment=ok");

    const char *sizes[] = {NULL, "", "512", "4096", "k", "K", "m", "g", "0K",
        "1k", "2M", "2G", "1073741825", "100", "0", "-1", "+2k", " 2k",
        "\t\n2k", "2k ", "1KB", "invalid", "  k", "0x400", "--1",
        "9223372036854775807", "9223372036854775808", "-9223372036854775808",
        "-9223372036854775809", "999999999999999999999G", "\xc2\xa0" "2K", "\xff"};
    for (size_t i = 0; i < sizeof sizes / sizeof *sizes; i++) {
        if (sizes[i]) setenv("BLOCKSIZE", sizes[i], 1); else unsetenv("BLOCKSIZE");
        errno = 0;
        int length = -1;
        long size = -1;
        char *header = i % 2 ? get_size(&length, &size) : getbsize(&length, &size);
        if (!header || length != strlen(header)) return 50;
        printf("block%zu=%s:%ld:%d\n", i, header, size, errno);
    }
    return 0;
}
"#;

#[cfg(target_os = "macos")]
#[test]
fn environment_and_blocksize_agree_with_macos_on_both_backends() {
    let Some(emulated) = common::build_x86_64("env_humanize", SOURCE, &[]) else {
        skip!("cannot build x86_64 fixtures");
    };
    let Some(native) = common::build_native("env_humanize_native", SOURCE) else {
        skip!("cannot build native fixtures");
    };
    for (path, flags) in [
        (&emulated, &["--emulated"][..]),
        (&native, &["--native"][..]),
    ] {
        // A host-architecture fixture gives an independent libSystem oracle.
        let reference = std::process::Command::new(&native)
            .env("PORT_INITIAL", "inherited")
            .env_remove("PORT_CONSTRUCTOR")
            .output()
            .unwrap();
        assert!(reference.status.success(), "host: {reference:?}");
        for mode in ["native", "builtin"] {
            let output = common::driver_command()
                .env("ROSEMARY_PREFIX", "off")
                .env("ROSEMARY_LIBC", mode)
                .env("ROSEMARY_GUI", "0")
                .env("PORT_INITIAL", "inherited")
                // The builtin libc copies only what is asked for.
                .env("ROSEMARY_ENVIRON", "PORT_INITIAL")
                .env_remove("PORT_CONSTRUCTOR")
                .arg("run")
                .args(flags)
                .arg(path)
                .output()
                .unwrap();
            assert!(output.status.success(), "{mode} {flags:?}: {output:?}");
            assert_eq!(output.stdout, reference.stdout, "{mode} {flags:?}");
            // warnx prefixes each message with the executable basename.
            let messages = |bytes: &[u8]| {
                bytes
                    .split(|&b| b == b'\n')
                    .filter(|line| !line.is_empty())
                    .map(|line| line.splitn(2, |&b| b == b':').nth(1).unwrap().to_vec())
                    .collect::<Vec<_>>()
            };
            assert_eq!(
                messages(&output.stderr),
                messages(&reference.stderr),
                "{mode} {flags:?}"
            );
        }
        let output = common::driver_command()
            .env("ROSEMARY_LIBC", "builtin")
            .env("ROSEMARY_GUI", "0")
            .env("ROSEMARY_ENVIRON", "PORT_INITIAL\nPATH=/guest/bin")
            .env("PORT_HOST_ONLY", "leaked")
            .env("PORT_INITIAL", "inherited")
            .env_remove("PORT_CONSTRUCTOR")
            .arg("run")
            .args(flags)
            .args([path, "prefix"])
            .output()
            .unwrap();
        assert!(output.status.success(), "prefixed {flags:?}: {output:?}");
        assert_eq!(output.stdout, b"prefix environment agrees\n");
    }
}
