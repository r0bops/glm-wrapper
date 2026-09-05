//! Interactive relay tests, driven through a real pty (openpty + fork, no
//! external tools — util-linux `script` is unavailable in some sandboxes).

#![cfg(unix)]

mod common;

use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// A stub claude: emits a TUI-style frame WITHOUT a trailing newline, then
/// (after `delay_secs`) claude's exit banner — dim-styled, exactly like
/// `printResumeHint` writes it — and exits with `code`.
fn stub_claude(bin_dir: &Path, delay_secs: &str, code: u8) {
    let bin = bin_dir.join("claude");
    let mut f = std::fs::File::create(&bin).unwrap();
    write!(
        f,
        "#!/bin/sh\n\
         printf 'no-newline-frame\\033[0K'\n\
         sleep {delay_secs}\n\
         printf '\\033[2m\\nResume this session with:\\nclaude --resume stub-1234\\n\\033[0m\\n'\n\
         exit {code}\n"
    )
    .unwrap();
    drop(f);
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
}

struct Env {
    xdg: PathBuf,
}

/// Isolated config (with a stored key) plus a PATH holding the stub first,
/// then the system dirs (so `sleep` resolves for the stub).
fn setup(name: &str, delay_secs: &str, code: u8) -> Env {
    let xdg = common::scratch_xdg(name);
    let glm_dir = xdg.join("glm");
    std::fs::create_dir_all(&glm_dir).unwrap();
    std::fs::write(glm_dir.join("key"), "sk-it-test").unwrap();
    let bin_dir = xdg.join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    stub_claude(&bin_dir, delay_secs, code);
    Env { xdg }
}

fn stub_path(env: &Env) -> String {
    format!("{}:/usr/bin:/bin", env.xdg.join("bin").display())
}

/// Run glm on a fresh pty; returns (output, exit code). Timestamps of the
/// frame and hint markers are checked by the streaming test separately.
fn run_under_pty(env: &Env, args: &[&str]) -> (Vec<u8>, i32) {
    let marks = run_under_pty_typed(env, args, &[]);
    (marks.data, marks.exit_code)
}

/// A stub claude that reads one line of stdin and echoes it back — an
/// interactive session that waits for keystrokes before exiting.
fn stub_claude_interactive(bin_dir: &Path) {
    let bin = bin_dir.join("claude");
    let mut f = std::fs::File::create(&bin).unwrap();
    write!(
        f,
        "#!/bin/sh\n\
         IFS= read -r line\n\
         printf 'got:%s\\n' \"$line\"\n\
         exit 0\n"
    )
    .unwrap();
    drop(f);
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
}

struct Marks {
    data: Vec<u8>,
    exit_code: i32,
    frame_at: Option<Instant>,
    hint_at: Option<Instant>,
}

fn run_under_pty_typed(env: &Env, args: &[&str], input: &[u8]) -> Marks {
    use std::os::unix::io::RawFd;

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
            .env("PATH", stub_path(env))
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("GLM_API_KEY")
            .env_remove("ZAI_API_KEY")
            .env_remove("GLM_PROFILE")
            .env_remove("GLM_MODEL");

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

        // Feed simulated keystrokes: the pty slave's line discipline buffers
        // input until the child reads it, so writing right away is fine.
        let mut to_write = input;
        while !to_write.is_empty() {
            let n = libc::write(master, to_write.as_ptr().cast(), to_write.len());
            if n < 0 {
                if std::io::Error::last_os_error().raw_os_error() == Some(libc::EAGAIN) {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                }
                panic!("writing pty input failed");
            }
            to_write = &to_write[n as usize..];
        }

        let start = Instant::now();
        let mut marks = Marks {
            data: Vec::new(),
            exit_code: -1,
            frame_at: None,
            hint_at: None,
        };
        let mut buf = [0u8; 8192];
        loop {
            if start.elapsed() > Duration::from_secs(15) {
                panic!("pty session did not finish in time");
            }
            let mut fds = [libc::pollfd {
                fd: master,
                events: libc::POLLIN,
                revents: 0,
            }];
            let ready = libc::poll(fds.as_mut_ptr(), 1, 500);
            if ready <= 0 {
                // Check whether the child is gone even with no data.
                let mut st: libc::c_int = 0;
                if libc::waitpid(pid, &mut st, libc::WNOHANG) == pid {
                    marks.exit_code = wait_code(st);
                    break;
                }
                continue;
            }
            let n = libc::read(master, buf.as_mut_ptr().cast(), buf.len());
            if n <= 0 {
                let mut st: libc::c_int = 0;
                if libc::waitpid(pid, &mut st, libc::WNOHANG) == pid {
                    marks.exit_code = wait_code(st);
                    break;
                }
                continue;
            }
            marks.data.extend_from_slice(&buf[..n as usize]);
            if marks.frame_at.is_none() && contains(&marks.data, b"no-newline-frame") {
                marks.frame_at = Some(Instant::now());
            }
            if marks.hint_at.is_none() && contains(&marks.data, b"glm --resume stub-1234") {
                marks.hint_at = Some(Instant::now());
            }
        }
        // Reap now if the loop exited without capturing the status.
        if marks.exit_code < 0 {
            let mut st: libc::c_int = 0;
            libc::waitpid(pid, &mut st, 0);
            marks.exit_code = wait_code(st);
        }
        libc::close(master);
        marks
    }
}

fn wait_code(st: libc::c_int) -> i32 {
    unsafe { wait_code_impl(st) }
}

unsafe fn wait_code_impl(st: libc::c_int) -> i32 {
    if libc::WIFEXITED(st) {
        libc::WEXITSTATUS(st)
    } else if libc::WIFSIGNALED(st) {
        128 + libc::WTERMSIG(st)
    } else {
        -1
    }
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

#[test]
fn resume_hint_says_glm_and_exit_code_passes_through() {
    let env = setup("relay", "0", 7);
    let (data, code) = run_under_pty(&env, &["--resume", "x"]);
    let text = String::from_utf8_lossy(&data).into_owned();
    assert!(text.contains("glm --resume stub-1234"), "output: {text}");
    assert!(!text.contains("claude --resume"), "output: {text}");
    assert_eq!(code, 7, "exit code must pass through; output: {text}");
}

#[test]
fn output_streams_promptly_without_buffering() {
    // The frame has no trailing newline: it must surface while the stub is
    // still sleeping (line-buffered output would hold it until the banner).
    let env = setup("relay-stream", "1", 0);
    let marks = run_under_pty_typed(&env, &["--resume", "x"], &[]);
    let (frame, hint) = (marks.frame_at, marks.hint_at);
    let (frame, hint) = (
        frame.expect("frame marker observed"),
        hint.expect("hint marker observed"),
    );
    let gap = hint.duration_since(frame);
    assert!(
        gap > Duration::from_millis(500),
        "frame must stream long before the 1s-delayed banner; gap {gap:?}"
    );
    assert!(!contains(&marks.data, b"claude --resume"));
    assert_eq!(marks.exit_code, 0);
}

#[test]
fn keystrokes_flow_through_to_the_child() {
    // The input path: what the user types must reach the child even though
    // glm sits between the terminal and the pty.
    let xdg = common::scratch_xdg("relay-input");
    let glm_dir = xdg.join("glm");
    std::fs::create_dir_all(&glm_dir).unwrap();
    std::fs::write(glm_dir.join("key"), "sk-it-test").unwrap();
    let bin_dir = xdg.join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    stub_claude_interactive(&bin_dir);
    let env = Env { xdg };

    let marks = run_under_pty_typed(&env, &["--resume", "x"], b"ping\n");
    let text = String::from_utf8_lossy(&marks.data).into_owned();
    assert!(text.contains("got:ping"), "output: {text}");
    assert_eq!(marks.exit_code, 0, "output: {text}");
}

#[test]
fn print_mode_skips_the_relay() {
    // -p is not interactive: plain exec, no pty, hint untouched.
    let env = setup("relay-p", "0", 0);
    let out = std::process::Command::new(common::glm_bin())
        .args(["-p", "hi"])
        .env("XDG_CONFIG_HOME", &env.xdg)
        .env("PATH", stub_path(&env))
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("GLM_API_KEY")
        .env_remove("ZAI_API_KEY")
        .output()
        .expect("run glm -p");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(text.contains("Resume this session with:"), "output: {text}");
    assert!(text.contains("claude --resume stub-1234"), "output: {text}");
    assert_eq!(out.status.code(), Some(0));
}
