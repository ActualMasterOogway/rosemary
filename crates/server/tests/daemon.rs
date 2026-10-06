//! A real server on a private socket, with real processes checking in.

use std::path::{Path, PathBuf};
use std::time::Duration;

use server::client::Client;
use server::daemon::{self, Options, Outcome};
use server::location;
use server::protocol::{Checkin, Reply, Request, State, VERSION};

/// Held by every test here. A test that forks hands its child a copy of every
/// socket the process has open, other tests' among them, and a connection a
/// child still holds is a running guest to a server that is shutting down.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rosemary-server-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// Start a server in this process and wait until it listens.
fn start(directory: &Path, linger: Duration) -> std::thread::JoinHandle<Outcome> {
    let options = Options {
        directory: directory.to_path_buf(),
        linger,
        init: None,
    };
    let handle = std::thread::spawn(move || daemon::run(&options).unwrap());
    let socket = location::socket_in(directory);
    for _ in 0..500 {
        if Client::connect(&socket).is_ok() {
            return handle;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("the server never listened");
}

fn checkin(path: &str) -> Request {
    Request::Checkin(Checkin {
        version: VERSION,
        pid: starnix::process::me(),
        path: path.into(),
        argv: vec![path.rsplit('/').next().unwrap().as_bytes().to_vec()],
        ..Checkin::default()
    })
}

fn processes(client: &mut Client) -> Vec<server::protocol::Process> {
    match client.call(&Request::Processes).unwrap() {
        Reply::Processes(list) => list,
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_forked_child_is_listed_under_its_parent_and_dropped_once_collected() {
    let _serial = serial();
    let directory = scratch("fork");
    let server = start(&directory, Duration::from_millis(50));
    let socket = location::socket_in(&directory);

    let mut parent = Client::connect(&socket).unwrap();
    assert!(matches!(
        parent.call(&checkin("/bin/parent")).unwrap(),
        Reply::CheckedIn { .. }
    ));

    let mut go = [0i32; 2];
    let mut ready = [0i32; 2];
    unsafe {
        assert_eq!(libc::pipe(go.as_mut_ptr()), 0);
        assert_eq!(libc::pipe(ready.as_mut_ptr()), 0);
    }
    // SAFETY: the child only connects, writes, and exits without unwinding.
    let child = unsafe { libc::fork() };
    if child == 0 {
        let ok = Client::connect(&socket)
            .and_then(|mut client| client.call(&checkin("/usr/bin/child")))
            .is_ok_and(|reply| matches!(reply, Reply::CheckedIn { .. }));
        unsafe {
            libc::write(ready[1], [u8::from(ok)].as_ptr().cast(), 1);
            let mut byte = 0u8;
            libc::read(go[0], (&raw mut byte).cast(), 1);
            libc::_exit(0);
        }
    }
    let mut ok = 0u8;
    unsafe { libc::read(ready[0], (&raw mut ok).cast(), 1) };
    assert_eq!(ok, 1, "the child could not check in");

    let list = processes(&mut parent);
    let me = starnix::process::me();
    let parent_entry = list.iter().find(|p| p.pid == me).expect("parent listed");
    let child_entry = list.iter().find(|p| p.pid == child).expect("child listed");
    assert_eq!(parent_entry.command(), "parent");
    assert_eq!(child_entry.command(), "child");
    assert_eq!(child_entry.ppid, me, "a guest's guest parent is its parent");
    assert_eq!(
        parent_entry.ppid, 1,
        "a guest of a host process is launchd's"
    );

    // Exited but not collected: a zombie, and still listed as one.
    unsafe { libc::write(go[1], [1u8].as_ptr().cast(), 1) };
    let mut zombie = false;
    for _ in 0..200 {
        let list = processes(&mut parent);
        if list
            .iter()
            .any(|p| p.pid == child && p.state == State::Zombie)
        {
            zombie = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        zombie,
        "an exited, uncollected child was not shown as a zombie"
    );

    let mut status = 0;
    unsafe { libc::waitpid(child, &mut status, 0) };
    assert!(processes(&mut parent).iter().all(|p| p.pid != child));

    // Somebody else's pid is not a guest's to describe.
    assert!(matches!(
        parent.call(&Request::Usage { pid: 1 }).unwrap(),
        Reply::Usage(None)
    ));
    match parent.call(&Request::Usage { pid: me }).unwrap() {
        Reply::Usage(Some(usage)) => assert!(usage.resident_size > 0),
        other => panic!("{other:?}"),
    }

    drop(parent);
    assert!(matches!(server.join().unwrap(), Outcome::Finished));
    assert!(!socket.exists(), "the socket outlived its server");
}

#[test]
fn a_second_server_for_the_same_prefix_steps_aside() {
    let _serial = serial();
    let directory = scratch("twice");
    let first = start(&directory, Duration::from_millis(300));
    let second = daemon::run(&Options {
        directory: directory.clone(),
        linger: Duration::ZERO,
        init: None,
    })
    .unwrap();
    assert!(matches!(second, Outcome::AlreadyRunning));
    // The first is unharmed and still answers.
    let mut client = Client::connect(&location::socket_in(&directory)).unwrap();
    assert!(matches!(
        client.call(&Request::Processes).unwrap(),
        Reply::Processes(_)
    ));
    drop(client);
    first.join().unwrap();
}

#[test]
fn a_checkin_for_another_pid_is_refused() {
    let _serial = serial();
    let directory = scratch("liar");
    let server = start(&directory, Duration::from_millis(50));
    let mut client = Client::connect(&location::socket_in(&directory)).unwrap();
    let lie = Request::Checkin(Checkin {
        version: VERSION,
        pid: 1,
        path: "/sbin/launchd".into(),
        ..Checkin::default()
    });
    assert!(matches!(client.call(&lie).unwrap(), Reply::Refused(_)));
    let stale = Request::Checkin(Checkin {
        version: VERSION + 1,
        pid: starnix::process::me(),
        ..Checkin::default()
    });
    assert!(matches!(client.call(&stale).unwrap(), Reply::Refused(_)));
    assert!(processes(&mut client).is_empty());
    drop(client);
    server.join().unwrap();
}

#[test]
fn shutdown_ends_the_server_without_lingering() {
    let _serial = serial();
    let directory = scratch("stop");
    let server = start(&directory, Duration::from_secs(600));
    daemon::stop(&location::socket_in(&directory)).unwrap();
    // Joining at all is the assertion: with a ten-minute linger, only the
    // request can have ended it.
    server.join().unwrap();
}

/// Start a server with `init` and wait until it listens.
fn start_with(directory: &Path, init: daemon::Init) -> std::thread::JoinHandle<Outcome> {
    let options = Options {
        directory: directory.to_path_buf(),
        linger: Duration::from_secs(600),
        init: Some(init),
    };
    let handle = std::thread::spawn(move || daemon::run(&options).unwrap());
    let socket = location::socket_in(directory);
    for _ in 0..500 {
        if Client::connect(&socket).is_ok() {
            return handle;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("the server never listened");
}

#[test]
fn shutdown_kills_a_process_outside_launchd_that_ignores_sigterm() {
    let _serial = serial();
    let directory = scratch("stubborn");
    let server = start(&directory, Duration::from_secs(600));
    let socket = location::socket_in(&directory);

    let mut ready = [0i32; 2];
    let mut never = [0i32; 2];
    unsafe {
        assert_eq!(libc::pipe(ready.as_mut_ptr()), 0);
        assert_eq!(libc::pipe(never.as_mut_ptr()), 0);
    }
    // SAFETY: the child only connects, writes, and blocks without unwinding.
    let child = unsafe { libc::fork() };
    if child == 0 {
        // A server that never kills it fails the test rather than hanging it.
        unsafe {
            libc::signal(libc::SIGTERM, libc::SIG_IGN);
            libc::alarm(30);
        }
        let mut client = Client::connect(&socket).ok();
        let ok = client
            .as_mut()
            .and_then(|client| client.call(&checkin("/bin/stubborn")).ok())
            .is_some_and(|reply| matches!(reply, Reply::CheckedIn { .. }));
        unsafe {
            libc::write(ready[1], [u8::from(ok)].as_ptr().cast(), 1);
            let mut byte = 0u8;
            libc::read(never[0], (&raw mut byte).cast(), 1);
            libc::_exit(0);
        }
    }
    let mut ok = 0u8;
    unsafe { libc::read(ready[0], (&raw mut ok).cast(), 1) };
    assert_eq!(ok, 1, "the child could not check in");

    daemon::stop(&socket).unwrap();
    let mut status = 0;
    unsafe { libc::waitpid(child, &mut status, 0) };
    assert!(
        libc::WIFSIGNALED(status) && libc::WTERMSIG(status) == libc::SIGKILL,
        "a process ignoring SIGTERM was not killed: status {status:#x}"
    );
    assert!(matches!(server.join().unwrap(), Outcome::Finished));
}

#[test]
fn launchd_is_started_first_and_asked_to_stop_last() {
    let _serial = serial();
    let directory = scratch("launchd");
    let marker = directory.with_extension("stopped");
    let _ = std::fs::remove_file(&marker);
    let script = format!(
        "trap 'echo stopped > {}; exit 0' TERM; while :; do sleep 0.05; done",
        marker.display()
    );
    let server = start_with(
        &directory,
        daemon::Init {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), script.into()],
            current_dir: "/".into(),
            console: None,
            ready: None,
        },
    );
    let socket = location::socket_in(&directory);

    let mut client = Client::connect(&socket).unwrap();
    let launchd = match client.call(&checkin("/bin/user")).unwrap() {
        Reply::CheckedIn { launchd, .. } => launchd,
        other => panic!("{other:?}"),
    };
    assert!(launchd > 1, "the checkin did not name launchd");
    assert!(starnix::process::process(launchd).is_some());
    drop(client);

    daemon::stop(&socket).unwrap();
    assert!(matches!(server.join().unwrap(), Outcome::Finished));
    // The server only leaves once launchd is gone, and launchd left on the
    // termination request rather than being killed.
    assert!(starnix::process::process(launchd).is_none_or(|p| p.state == State::Zombie));
    assert_eq!(std::fs::read_to_string(&marker).unwrap().trim(), "stopped");
    let _ = std::fs::remove_file(&marker);
}
