mod common;

const SOURCE: &str = r#"
#include <dirent.h>
#include <fcntl.h>
#include <glob.h>
#include <libproc.h>
#include <mach/mach.h>
#if defined(__x86_64__)
#include <mach/i386/thread_status.h>
#endif
#include <pthread.h>
#include <pwd.h>
#include <spawn.h>
#include <stdio.h>
#include <string.h>
#include <sys/sysctl.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

extern char **environ;
#define CHECK(expr) do { if (!(expr)) { fprintf(stderr, "failed line %d: %s\n", __LINE__, #expr); return __LINE__; } } while (0)

int main(int argc, char **argv) {
    CHECK(geteuid() == 0);
    struct passwd *root = getpwnam("root");
    CHECK(root != NULL && root->pw_gid == 0);
    if (argc > 1) {
        int fd = open("/tmp/child-ready", O_CREAT | O_WRONLY, 0600);
        CHECK(fd >= 0);
        close(fd);
        for (int i = 0; i < 10000 && access("/tmp/child-release", F_OK); i++) usleep(1000);
        CHECK(access("/tmp/child-release", F_OK) == 0);
        CHECK(setuid(getuid()) == 0);
        CHECK(geteuid() == getuid());
        return 0;
    }
    CHECK(issetugid());
    uint64_t tid;
    CHECK(pthread_threadid_np(NULL, &tid) == 0);
    mach_port_t thread = mach_thread_self();
    thread_identifier_info_data_t identity;
    mach_msg_type_number_t words = THREAD_IDENTIFIER_INFO_COUNT;
    CHECK(thread_info(thread, THREAD_IDENTIFIER_INFO, (thread_info_t)&identity, &words) == KERN_SUCCESS);
    CHECK(identity.thread_id == tid);
    words = THREAD_IDENTIFIER_INFO_COUNT;
    CHECK(thread_info(pthread_mach_thread_np(pthread_self()), THREAD_IDENTIFIER_INFO, (thread_info_t)&identity, &words) == KERN_SUCCESS);
    CHECK(identity.thread_id == tid);
#if defined(__x86_64__)
    x86_thread_state64_t registers;
    words = x86_THREAD_STATE64_COUNT;
    CHECK(thread_get_state(thread, x86_THREAD_STATE64, (thread_state_t)&registers, &words) == KERN_SUCCESS);
    CHECK(registers.__rip != 0 && registers.__rsp != 0);
#endif
    CHECK(mach_port_deallocate(mach_task_self(), thread) == KERN_SUCCESS);
    CHECK(chdir("/tmp") == 0);
    char cwd[1024];
    CHECK(getcwd(cwd, sizeof cwd) != NULL);
    CHECK(!strcmp(cwd, "/tmp") || !strcmp(cwd, "/private/tmp"));
    int fd = open("accounting", O_CREAT | O_EXCL | O_RDWR, 0600);
    CHECK(fd >= 0);
    CHECK(fcntl(fd, F_SETFD, FD_CLOEXEC) == 0);
    CHECK(fcntl(fd, F_GETFD) & FD_CLOEXEC);
    struct stat st;
    CHECK(fstat(fd, &st) == 0 && st.st_uid == 0);
    CHECK(fchown(fd, 1234, 80) == 0);
    CHECK(fstat(fd, &st) == 0 && st.st_uid == 1234 && st.st_gid == 80);
    char path[1024];
    CHECK(fcntl(fd, F_GETPATH, path) == 0);
    CHECK(!strcmp(path, "/tmp/accounting") || !strcmp(path, "/private/tmp/accounting"));
    close(fd);
    int directory = open("/tmp", O_RDONLY | O_DIRECTORY);
    CHECK(directory >= 0);
    fd = openat(directory, "from-at", O_CREAT | O_EXCL | O_RDWR, 0640);
    CHECK(fd >= 0 && fstat(fd, &st) == 0 && st.st_uid == 0);
    CHECK(unlinkat(directory, "from-at", 0) == 0);
    close(fd);
    close(directory);
    DIR *dir = opendir("/tmp");
    CHECK(dir != NULL);
    struct dirent *entry;
    int seen = 0;
    while ((entry = readdir(dir))) if (!strcmp(entry->d_name, "accounting")) seen++;
    CHECK(closedir(dir) == 0 && seen == 1);
    glob_t files = {0};
    CHECK(glob("/tmp/acc*", 0, NULL, &files) == 0 && files.gl_pathc == 1);
    CHECK(!strcmp(files.gl_pathv[0], "/tmp/accounting"));
    globfree(&files);

    pid_t child;
    char *args[] = {"/bin/child", "child", NULL};
    CHECK(posix_spawn(&child, args[0], NULL, NULL, args, environ) == 0);
    for (int i = 0; i < 5000 && access("/tmp/child-ready", F_OK); i++) usleep(1000);
    CHECK(access("/tmp/child-ready", F_OK) == 0);
    int mib[] = {CTL_KERN, KERN_PROC, KERN_PROC_PID, child};
    struct kinfo_proc info;
    size_t size = sizeof info;
    CHECK(sysctl(mib, 4, &info, &size, NULL, 0) == 0 && size == sizeof info);
    CHECK(info.kp_eproc.e_ppid == getpid() && info.kp_eproc.e_ucred.cr_uid == 0);
    char executable[1024];
    CHECK(proc_pidpath(child, executable, sizeof executable) > 0);
    CHECK(!strcmp(executable, "/bin/child"));
    mach_port_t task;
    CHECK(task_for_pid(mach_task_self(), child, &task) == KERN_SUCCESS);
    pid_t task_pid;
    CHECK(pid_for_task(task, &task_pid) == KERN_SUCCESS && task_pid == child);
    CHECK(mach_port_deallocate(mach_task_self(), task) == KERN_SUCCESS);
    fd = open("/tmp/child-release", O_CREAT | O_WRONLY, 0600);
    CHECK(fd >= 0);
    close(fd);
    int status;
    CHECK(waitpid(child, &status, 0) == child && status == 0);
    puts("prefix-server=ok");
    return 0;
}
"#;

#[test]
fn prefix_paths_and_setid_credentials_survive_a_guest_spawn() {
    let Some(emulated) = common::build_x86_64("prefix_server", SOURCE, &[]) else {
        skip!("cannot build x86_64 fixtures");
    };
    let Some(native) = common::build_native("prefix_server_native", SOURCE) else {
        skip!("cannot build native fixtures");
    };
    for (name, image, engine) in [
        ("emulated", emulated, "--emulated"),
        ("native", native, "--native"),
    ] {
        let directory = std::env::temp_dir().join(format!(
            "rosemary-prefix-server-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&directory);
        prefix::layout::create(&directory).unwrap();
        let prefix = prefix::Prefix::at(&directory);
        std::fs::create_dir_all(prefix.drive().join("bin")).unwrap();
        for file in ["root-helper", "child"] {
            std::fs::copy(&image, prefix.drive().join("bin").join(file)).unwrap();
        }
        std::fs::write(
            prefix.control().join("setid.yaml"),
            "programs:\n  - {path: /bin/root-helper, owner: 0, group: 0, mode: '4755'}\n",
        )
        .unwrap();
        let output = common::driver_command()
            .env("ROSEMARY_PREFIX", &directory)
            .env("ROSEMARY_LIBC", "builtin")
            .env("ROSEMARY_OBJC", "builtin")
            .env("ROSEMARY_SERVER", "on")
            .env("ROSEMARY_SERVER_LINGER", "1")
            .args(["run", engine, "/bin/root-helper"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{name}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("prefix-server=ok"));
        use std::os::unix::fs::MetadataExt;
        assert_eq!(
            std::fs::metadata(prefix.drive().join("private/tmp/accounting"))
                .unwrap()
                .uid(),
            unsafe { libc::getuid() },
            "the guest's chown must leave host ownership alone"
        );
        std::fs::remove_dir_all(&directory).unwrap();
    }
}
