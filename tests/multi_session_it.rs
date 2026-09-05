//! Multi-session mechanics through a real pty: the registry, keep-alive on
//! terminal loss, socket attach with replay+input, and `glm kill`.

#![cfg(unix)]

mod common;

use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::io::RawFd;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// A stub claude that echoes one line of input and stays alive for a while.
fn stub_claude(bin_dir: &Path) {
    let bin = bin_dir.join("claude");
    let mut f = std::fs::File::create(&bin).unwrap();
    write!(
        f,
        "#!/bin/sh\n\
         IFS= read -r line\n\
         printf 'got:%s\\n' \"$line\"\n\
         sleep 30\n"
    )
    .unwrap();
    drop(f);
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
}

struct Env {
    xdg: PathBuf,
}

fn setup(name: &str) -> Env {
    let xdg = common::scratch_xdg(name);
    let glm_dir = xdg.join("glm");
    std::fs::create_dir_all(&glm_dir).unwrap();
    std::fs::write(glm_dir.join("key"), "sk-it-test").unwrap();
    let bin_dir = xdg.join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    stub_claude(&bin_dir);
    Env { xdg }
}

/// Spawn glm on a fresh pty; returns (master fd, glm pid).
fn spawn_glm(env: &Env, args: &[&str]) -> (RawFd, libc::pid_t) {
    unsafe {
        let mut master: RawFd = -1;
        let mut slave: RawFd = -1;
        assert_eq!(
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null_mut()
            ),
            0
        );
        let mut cmd = std::process::Command::new(common::glm_bin());
        cmd.args(args)
            .env("XDG_CONFIG_HOME", &env.xdg)
            .env("CLAUDE_CONFIG_DIR", env.xdg.join("claude"))
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", env.xdg.join("bin").display()),
            )
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("GLM_API_KEY");
        let pid = libc::fork();
        assert!(pid >= 0, "fork failed");
        if pid == 0 {
            libc::setsid();
            libc::ioctl(slave, libc::TIOCSCTTY, 0);
            libc::dup2(slave, libc::STDIN_FILENO);
            libc::dup2(slave, libc::STDOUT_FILENO);
            libc::dup2(slave, libc::STDERR_FILENO);
            if slave > 2 {
                libc::close(slave);
            }
            libc::close(master);
            let err = cmd.exec();
            eprintln!("exec failed: {err}");
            libc::exit(126);
        }
        libc::close(slave);
        let flags = libc::fcntl(master, libc::F_GETFL, 0);
        libc::fcntl(master, libc::F_SETFL, flags | libc::O_NONBLOCK);
        (master, pid)
    }
}

/// Read from `fd` for up to `secs`, returning everything received.
fn drain_fd(fd: RawFd, secs: u64) -> Vec<u8> {
    let mut out = Vec::new();
    let start = Instant::now();
    unsafe {
        while start.elapsed() < Duration::from_secs(secs) {
            let mut fds = [libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            }];
            let ready = libc::poll(fds.as_mut_ptr(), 1, 300);
            if ready <= 0 {
                continue;
            }
            let mut buf = [0u8; 8192];
            let n = libc::read(fd, buf.as_mut_ptr().cast(), buf.len());
            if n <= 0 {
                break;
            }
            out.extend_from_slice(&buf[..n as usize]);
        }
    }
    out
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// The registry row of the single live session, read from disk.
fn read_entry(env: &Env) -> (u32, u32) {
    let raw = std::fs::read_to_string(env.xdg.join("glm").join("sessions.json")).expect("registry");
    let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
    (
        v[0]["pid"].as_u64().unwrap() as u32,
        v[0]["child_pid"].as_u64().unwrap() as u32,
    )
}

#[test]
fn attach_and_kill_flow() {
    let env = setup("multi");
    let (master, glm_pid) = spawn_glm(&env, &["--resume", "x"]);

    // Let the relay start and register itself.
    std::thread::sleep(Duration::from_millis(800));
    let (entry_pid, _) = read_entry(&env);
    assert_eq!(entry_pid, glm_pid as u32, "registry records the relay pid");

    // Attach through the unix socket like `glm attach` does: winsize
    // preamble first, then keystrokes.
    let sock = env.xdg.join("glm").join(format!("relay-{entry_pid}.sock"));
    let mut client = UnixStream::connect(&sock).expect("connect to relay socket");
    let mut greet = [0u8; 8];
    greet[0..2].copy_from_slice(&24u16.to_le_bytes());
    greet[2..4].copy_from_slice(&80u16.to_le_bytes());
    client.write_all(&greet).unwrap();
    // The relay replays what it already sent to the (still live) terminal.
    let replay = drain_fd(client.as_raw_fd(), 2);
    assert!(
        contains(&replay, b"no-newline") || replay.is_empty(),
        "replay must not contain foreign bytes"
    );
    client.write_all(b"ping\n").unwrap();
    let reply = drain_fd(client.as_raw_fd(), 5);
    assert!(
        contains(&reply, b"got:ping"),
        "attach input must reach the agent; got {reply:?}"
    );
    drop(client);

    // Simulate the terminal closing: our master disappears; the relay must
    // keep the agent alive and mark the session backgrounded.
    unsafe {
        libc::close(master);
        libc::kill(glm_pid, libc::SIGHUP);
    }
    // The transition is fast but not instantaneous; poll for it.
    let mut out;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let (o, _e, c) = common::run_glm(&["sessions"], &env.xdg, &[]);
        out = o;
        if (c == 0 && out.contains("bg")) || Instant::now() > deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    assert!(out.contains("bg"), "session must be backgrounded: {out}");

    // `glm kill` with a 3-char pid prefix resolves the session (exact pids
    // are 6-7 digits; this is the `glm kill 186` workflow).
    let prefix = entry_pid.to_string()[..3].to_string();
    let (out, _err, code) = common::run_glm(&["kill", &prefix], &env.xdg, &[]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("killed session"), "{out}");
    assert!(out.contains(&entry_pid.to_string()), "{out}");

    let mut status: libc::c_int = 0;
    unsafe {
        libc::waitpid(glm_pid, &mut status, 0);
    }
    let exited = libc::WIFEXITED(status) || libc::WIFSIGNALED(status);
    assert!(exited, "relay must exit after its agent is killed");

    // Registry is clean again.
    let (out, _err, _code) = common::run_glm(&["sessions"], &env.xdg, &[]);
    assert!(out.contains("no glm sessions"), "{out}");
}

#[test]
fn ctrl_c_backgrounds_instead_of_killing() {
    let env = setup("multi-ctrlc");
    let (master, glm_pid) = spawn_glm(&env, &["--resume", "x"]);
    std::thread::sleep(Duration::from_millis(800));

    // The user presses Ctrl-C: under keep-alive this must hand the terminal
    // back and keep the agent running.
    unsafe {
        libc::write(master, b"\x03".as_ptr().cast(), 1);
    }
    let out = drain_fd(master, 3);
    assert!(
        contains(&out, b"session backgrounded"),
        "expected background message, got {out:?}"
    );
    // Terminal modes must be handed back clean: mouse tracking off, alt
    // screen left — or the shell gets flooded with mouse-event garbage.
    assert!(
        contains(&out, b"\x1b[?1003l"),
        "mouse tracking must be disabled: {out:?}"
    );
    assert!(
        contains(&out, b"\x1b[?1049l"),
        "alt screen must be exited: {out:?}"
    );
    // The shell must actually get control back: the relay's PARENT process
    // exits right after the message (the orphan child keeps the agent).
    let mut wstatus: libc::c_int = 0;
    let reaped = unsafe { libc::waitpid(glm_pid, &mut wstatus, libc::WNOHANG) };
    assert_eq!(
        reaped, glm_pid,
        "relay parent must exit so the shell resumes"
    );

    // The agent is still alive and the session is marked bg.
    let (sessions, _e, code) = common::run_glm(&["sessions"], &env.xdg, &[]);
    assert_eq!(code, 0);
    assert!(sessions.contains("bg"), "{sessions}");
    assert!(sessions.contains(&glm_pid.to_string()), "{sessions}");

    // Attach and talk to the still-running agent through the socket.
    let (entry_pid, _) = read_entry(&env);
    let sock = env.xdg.join("glm").join(format!("relay-{entry_pid}.sock"));
    let mut client = UnixStream::connect(&sock).expect("connect after Ctrl-C");
    let mut greet = [0u8; 8];
    greet[0..2].copy_from_slice(&24u16.to_le_bytes());
    greet[2..4].copy_from_slice(&80u16.to_le_bytes());
    client.write_all(&greet).unwrap();
    client.write_all(b"pong\n").unwrap();
    let reply = drain_fd(client.as_raw_fd(), 5);
    assert!(
        contains(&reply, b"got:pong"),
        "agent must still respond after Ctrl-C; got {reply:?}"
    );
    drop(client);

    // Clean up: kill everything. The orphaned relay exits on its own once
    // the agent dies; give the registry a moment to reflect it.
    let (out, _e, code) = common::run_glm(&["kill"], &env.xdg, &[]);
    assert_eq!(code, 0, "{out}");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let (out, _e, _c) = common::run_glm(&["sessions"], &env.xdg, &[]);
        if out.contains("no glm sessions") || Instant::now() > deadline {
            assert!(out.contains("no glm sessions"), "{out}");
            break;
        }
        std::thread::sleep(Duration::from_millis(300));
    }
}
