//! Interactive-session pty relay.
//!
//! Non-interactive runs exec claude directly. Interactive ones (tty on
//! stdin+stdout, not print mode) run claude on a private pty with glm
//! relaying the bytes, for one reason: claude's exit banner
//! "Resume this session with:\nclaude --resume <id>" is a compiled-in
//! literal naming the wrong binary — followed verbatim it would bypass glm
//! entirely (wrong auth, base URL, and model). The relay rewrites that one
//! token to `glm`; every other byte passes through untouched.
//!
//! Keep-alive: when the terminal goes away (SIGHUP / stdin EOF), the relay
//! does NOT die by default — claude lives in its own session and keeps
//! running; output is buffered and the relay listens on a unix socket, so
//! `glm attach` reconnects (replaying the tail, then streaming live).
//! `GLM_KEEP_ALIVE=0` restores the old behavior (SIGHUP forwards, relay
//! exits with the terminal). Every interactive session is registered in
//! ~/.config/glm/sessions.json for `glm sessions` / `glm kill`.

/// The heading claude emits before the hardcoded program name.
const HEADING: &[u8] = b"Resume this session with:";
/// Longest sequence we can still be mid-match on: heading + EOL + "claude".
const HOLD: usize = HEADING.len() + 2 + b"claude".len() - 1;

/// Byte-stream rewriter turning claude's `claude --resume <id>` exit hint
/// into `glm --resume <id>`. Streaming: everything is emitted immediately
/// except a suffix that could still grow into a hint (at most `HOLD` bytes,
/// and never at idle — a normal frame is passed through in full). Rewrites
/// at most once per stream. The EOL inside the hint is matched flexibly
/// (`\n` or `\r\n`): claude prints the hint in raw mode, but restores cooked
/// mode (ONLCR) on some paths, which the pty line discipline translates.
#[derive(Default)]
pub struct HintRewriter {
    buf: Vec<u8>,
    done: bool,
}

impl HintRewriter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Push output bytes; returns the bytes that may be emitted now.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<u8> {
        if self.done {
            return chunk.to_vec();
        }
        self.buf.extend_from_slice(chunk);
        if let Some((claude_at, matched_to)) = find_hint(&self.buf) {
            let mut out = Vec::with_capacity(self.buf.len());
            out.extend_from_slice(&self.buf[..claude_at]);
            out.extend_from_slice(b"glm");
            self.done = true;
            out.extend_from_slice(&self.buf[matched_to..]);
            self.buf.clear();
            return out;
        }
        let hold = hold_back_len(&self.buf);
        let split = self.buf.len() - hold;
        let out = self.buf[..split].to_vec();
        self.buf.drain(..split);
        out
    }

    /// The session ended: flush anything still held back.
    pub fn finish(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.buf)
    }
}

/// Find `HEADING`, a flexible EOL, and the literal `claude`; returns
/// (offset of "claude", offset just past it) so the caller can swap the
/// token in place. Absent any of the three, returns None.
fn find_hint(hay: &[u8]) -> Option<(usize, usize)> {
    let pos = find(hay, HEADING)?;
    let mut p = pos + HEADING.len();
    if hay.get(p) == Some(&b'\r') {
        p += 1;
    }
    if hay.get(p) == Some(&b'\n') {
        p += 1;
    }
    let claude = b"claude";
    if hay.len() - p < claude.len() || &hay[p..p + claude.len()] != claude {
        return None;
    }
    Some((p, p + claude.len()))
}

/// How many trailing bytes of `buf` must be held back because they could be
/// the start of a hint arriving in pieces. A partial match is a suffix that
/// is a prefix of `HEADING + EOL + "claude"`; anything else is emitted at
/// once so idle output never stalls.
fn hold_back_len(buf: &[u8]) -> usize {
    let max = HOLD.min(buf.len());
    (1..=max)
        .rev()
        .find(|&l| partial_match(&buf[buf.len() - l..]))
        .unwrap_or(0)
}

fn partial_match(suffix: &[u8]) -> bool {
    if HEADING.starts_with(suffix) {
        return true;
    }
    if suffix.len() <= HEADING.len() || !suffix.starts_with(HEADING) {
        return false;
    }
    let rest = &suffix[HEADING.len()..];
    let after_eol = if rest == b"\r" {
        return true; // mid-EOL
    } else if rest.starts_with(b"\r\n") {
        &rest[2..]
    } else if rest.starts_with(b"\n") {
        &rest[1..]
    } else {
        return false;
    };
    after_eol.len() <= b"claude".len() && b"claude".starts_with(after_eol)
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.len() > hay.len() {
        return None;
    }
    (0..=hay.len() - needle.len()).find(|&i| &hay[i..i + needle.len()] == needle)
}

/// Extract the Claude Code session id from the exit banner that follows the
/// rewritten hint: the token after "--resume ". Returns None until a
/// plausible id (hex/dashes, >= 8 chars) is complete in `tail`.
pub fn session_id_from_tail(tail: &[u8]) -> Option<String> {
    let pos = find(tail, b"--resume ")?;
    let rest = &tail[pos + b"--resume ".len()..];
    let end = rest
        .iter()
        .position(|b| !(b.is_ascii_hexdigit() || *b == b'-'))
        .unwrap_or(rest.len());
    let id = &rest[..end];
    if id.len() >= 8 {
        Some(String::from_utf8_lossy(id).into_owned())
    } else {
        None
    }
}

/// Whether the interactive relay applies: a terminal on both ends, not
/// print mode (print mode is never interactive, so there is no hint), and
/// not disabled via `GLM_RELAY=0` — a kill-switch in case the byte relay
/// ever misbehaves in an unusual terminal.
pub fn eligible(args: &[String]) -> bool {
    if is_print_mode(args) || relay_disabled_by_env() {
        return false;
    }
    #[cfg(unix)]
    unsafe {
        libc::isatty(libc::STDIN_FILENO) == 1 && libc::isatty(libc::STDOUT_FILENO) == 1
    }
    #[cfg(not(unix))]
    {
        false
    }
}

fn relay_disabled_by_env() -> bool {
    flag_env("GLM_RELAY")
}

/// Keep-alive default on: a closed terminal backgrounds the session instead
/// of killing it. `GLM_KEEP_ALIVE=0` restores SIGHUP pass-through.
pub fn keep_alive_enabled() -> bool {
    !flag_env("GLM_KEEP_ALIVE")
}

fn flag_env(name: &str) -> bool {
    match std::env::var(name) {
        Ok(v) => matches!(
            v.trim().to_lowercase().as_str(),
            "0" | "false" | "no" | "off"
        ),
        Err(_) => false,
    }
}

fn is_print_mode(args: &[String]) -> bool {
    args.iter().any(|a| a == "-p" || a == "--print")
}

/// Whether this invocation drives the interactive relay (no print mode).
pub fn is_interactive(args: &[String]) -> bool {
    !is_print_mode(args)
}

#[cfg(unix)]
mod imp {
    use super::{keep_alive_enabled, HintRewriter};
    use crate::registry::{self, SessionEntry};
    use anyhow::{bail, Result};
    use std::ffi::CString;
    use std::io::{Read, Write};
    use std::os::unix::io::AsRawFd;
    use std::os::unix::net::{UnixListener, UnixStream};

    /// Signals the relay forwards to the child (which lives in its own
    /// session, so terminal teardown signals never reach it on their own).
    const FORWARDED: [libc::c_int; 6] = [
        libc::SIGINT,
        libc::SIGTERM,
        libc::SIGHUP,
        libc::SIGQUIT,
        libc::SIGTSTP,
        libc::SIGCONT,
    ];

    /// Terminal modes the claude TUI enables (mouse tracking, focus
    /// reporting, bracketed paste, alternate screen). When the relay hands
    /// the terminal back to the shell these must be switched OFF, or the
    /// emulator keeps sending mouse-event garbage into the shell input.
    pub(super) const TUI_MODES_DISABLE: &[u8] =
        b"\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1004l\x1b[?2004l\x1b[?1049l\x1b[?25h";
    /// And back ON when a terminal re-attaches to the still-running TUI.
    pub(super) const TUI_MODES_ENABLE: &[u8] =
        b"\x1b[?1049h\x1b[?25l\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1006h\x1b[?1004h\x1b[?2004h";

    /// Output retained while no attach client is connected, so `glm attach`
    /// can replay what the dead terminal missed.
    const TAIL_CAP: usize = 256 * 1024;
    /// Live bytes queued for the attach client that did not fit the socket
    /// buffer yet (flushed on POLLOUT).
    const OUTBOX_CAP: usize = 1024 * 1024;

    /// Run claude on a private pty, relaying bytes and rewriting the resume
    /// hint. Never returns Ok: on success it exits the process with
    /// claude's status. Err means setup failed before claude started — the
    /// caller should fall back to plain exec.
    pub fn run(program: &str, args: &[String]) -> Result<()> {
        let program_c = CString::new(program)?;
        let mut args_c: Vec<CString> = Vec::with_capacity(args.len() + 1);
        args_c.push(program_c.clone());
        for a in args {
            args_c.push(CString::new(a.as_str())?);
        }
        let keep_alive = keep_alive_enabled();
        let glm_pid = std::process::id();

        unsafe {
            let mut master: libc::c_int = -1;
            let mut slave: libc::c_int = -1;
            if libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null_mut(),
            ) != 0
            {
                bail!("openpty failed: {}", std::io::Error::last_os_error());
            }
            let mut outer: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(libc::STDIN_FILENO, &mut outer) != 0 {
                bail!("stdin is not a terminal");
            }
            apply_winsz(master, current_winsz(libc::STDIN_FILENO));

            // CLOEXEC pipe so the parent learns whether the child's exec
            // worked: EOF = exec succeeded, 4 bytes = errno.
            let mut execp: [libc::c_int; 2] = [-1; 2];
            if libc::pipe2(execp.as_mut_ptr(), libc::O_CLOEXEC) != 0 {
                bail!("pipe2 failed: {}", std::io::Error::last_os_error());
            }
            // Self-pipe for signal handlers (SIGWINCH + forwarded signals).
            let mut sigp: [libc::c_int; 2] = [-1; 2];
            if libc::pipe2(sigp.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) != 0 {
                bail!("pipe2 failed: {}", std::io::Error::last_os_error());
            }

            let pid = libc::fork();
            if pid < 0 {
                bail!("fork failed: {}", std::io::Error::last_os_error());
            }
            if pid == 0 {
                libc::close(execp[0]);
                libc::close(sigp[0]);
                libc::close(sigp[1]);
                libc::setsid();
                libc::ioctl(slave, libc::TIOCSCTTY, 0);
                libc::dup2(slave, libc::STDIN_FILENO);
                libc::dup2(slave, libc::STDOUT_FILENO);
                libc::dup2(slave, libc::STDERR_FILENO);
                if slave > 2 {
                    libc::close(slave);
                }
                libc::close(master);
                let mut argv: Vec<*const libc::c_char> = args_c
                    .iter()
                    .map(|c| c.as_ptr())
                    .chain(std::iter::once(std::ptr::null()))
                    .collect();
                libc::execvp(program_c.as_ptr(), argv.as_mut_ptr());
                // exec failed: tell the parent which errno, then die.
                let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(1);
                let bytes = errno.to_le_bytes();
                let _ = libc::write(execp[1], bytes.as_ptr().cast(), bytes.len());
                libc::exit(127);
            }

            libc::close(execp[1]);
            let mut errbuf = [0u8; 4];
            let n = libc::read(execp[0], errbuf.as_mut_ptr().cast(), errbuf.len());
            libc::close(execp[0]);
            libc::close(slave);
            if n == 4 {
                libc::close(master);
                let errno = i32::from_le_bytes(errbuf);
                return Err(exec_error(errno));
            }

            // Register this session before anything can go missing.
            let entry = SessionEntry {
                pid: glm_pid,
                child_pid: pid as u32,
                sid: None,
                cwd: std::env::current_dir()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default(),
                started_ms: now_ms(),
                backgrounded: false,
                backgrounded_at_ms: None,
            };
            if let Err(e) = registry::save(&[entry]) {
                eprintln!("glm: warning: session registry: {e:#}");
            }

            // Attach socket for `glm attach` after the terminal goes away.
            let sock_path = crate::paths::relay_socket(glm_pid);
            let _ = std::fs::remove_file(&sock_path);
            let listener = match UnixListener::bind(&sock_path) {
                Ok(l) => {
                    let _ = l.set_nonblocking(true);
                    Some(l)
                }
                Err(_) => None,
            };

            install_signal_handlers(sigp[1]);

            // Non-blocking master: output is poll-driven, and keystroke
            // writes that would block (child busy streaming) wait in
            // `pending` instead of stalling the whole relay.
            let flags = libc::fcntl(master, libc::F_GETFL, 0);
            if flags < 0 || libc::fcntl(master, libc::F_SETFL, flags | libc::O_NONBLOCK) != 0 {
                bail!("fcntl master: {}", std::io::Error::last_os_error());
            }

            // Raw the outer terminal for the duration of the relay.
            let mut raw = outer;
            libc::cfmakeraw(&mut raw);
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSADRAIN, &raw);

            let code = relay_loop(pid, master, sigp[0], glm_pid, keep_alive, listener, outer);

            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSADRAIN, &outer);
            libc::close(master);
            registry::remove(glm_pid);
            let _ = std::fs::remove_file(&sock_path);
            libc::exit(code);
        }
    }

    /// Read/write between the outer terminal and the child's pty until the
    /// child exits; returns the exit code for `libc::exit`. The master fd
    /// must be non-blocking so keystroke writes never stall output reads
    /// (pending input waits in `pending` for POLLOUT).
    ///
    /// Output routing: the attach client when one is connected, else the
    /// (possibly dead) terminal; every post-rewrite byte also lands in the
    /// tail buffer for `glm attach` replay.
    unsafe fn relay_loop(
        pid: libc::pid_t,
        master: libc::c_int,
        sigfd: libc::c_int,
        glm_pid: u32,
        keep_alive: bool,
        listener: Option<UnixListener>,
        outer: libc::termios,
    ) -> libc::c_int {
        let mut rewriter = HintRewriter::new();
        let mut buf = [0u8; 8192];
        let mut stdin_open = true;
        let mut status: libc::c_int = 0;
        let mut reaped = false;
        // Keystrokes read from the terminal that the child has not taken
        // yet (its input buffer fills while it is streaming a response).
        let mut pending: Vec<u8> = Vec::new();
        // Replay buffer + attach-client state.
        let mut tail: Vec<u8> = Vec::new();
        let mut client: Option<UnixStream> = None;
        let mut client_out: Vec<u8> = Vec::new();
        let mut client_greeted = false;
        let mut greet_buf: Vec<u8> = Vec::new();
        let mut backgrounded = false;
        // Set by terminal loss (SIGHUP, EOF/EIO) or Ctrl-C under keep-alive.
        let mut bg_requested = false;
        // Set once Ctrl-C (or terminal loss) backgrounded the session: the
        // terminal is restored to the shell and gets no more agent bytes.
        let mut foreground_gone = false;
        // True when the background trigger still had a live terminal
        // (Ctrl-C) — in that case the parent must exit so the shell can
        // resume; terminal-loss triggers continue in place.
        let mut terminal_was_alive = false;
        let mut sid_done = false;
        let listener_fd = listener.as_ref().map(|l| l.as_raw_fd());

        loop {
            let mut fds: Vec<libc::pollfd> = vec![
                libc::pollfd {
                    fd: if stdin_open { libc::STDIN_FILENO } else { -1 },
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: master,
                    events: libc::POLLIN | if pending.is_empty() { 0 } else { libc::POLLOUT },
                    revents: 0,
                },
                libc::pollfd {
                    fd: sigfd,
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            if let Some(fd) = listener_fd {
                fds.push(libc::pollfd {
                    fd,
                    events: libc::POLLIN,
                    revents: 0,
                });
            }
            if let Some(c) = &client {
                fds.push(libc::pollfd {
                    fd: c.as_raw_fd(),
                    events: libc::POLLIN
                        | if client_out.is_empty() {
                            0
                        } else {
                            libc::POLLOUT
                        },
                    revents: 0,
                });
            }
            let ready = libc::poll(fds.as_mut_ptr(), fds.len() as _, -1);
            if ready < 0 {
                if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                break;
            }

            // Signals: SIGWINCH refreshes the child's size; the rest are
            // forwarded — except SIGHUP under keep-alive, which means the
            // terminal went away: ignore it and keep the agent alive.
            if fds[2].revents & libc::POLLIN != 0 {
                let n = libc::read(sigfd, buf.as_mut_ptr().cast(), buf.len());
                let got = if n > 0 { n as usize } else { 0 };
                for &b in &buf[..got] {
                    let sig = b as libc::c_int;
                    if sig == libc::SIGWINCH {
                        apply_winsz(master, current_winsz(libc::STDIN_FILENO));
                    } else if sig == libc::SIGHUP && keep_alive {
                        // The terminal hung up: background, keep alive.
                        bg_requested = true;
                    } else {
                        libc::killpg(pid, sig);
                    }
                }
            }

            // Terminal input (or nothing, once the terminal is gone). A
            // closed master surfaces as POLLERR/POLLHUP, not POLLIN.
            if fds[0].revents & (libc::POLLIN | libc::POLLERR | libc::POLLHUP) != 0 {
                let n = libc::read(libc::STDIN_FILENO, buf.as_mut_ptr().cast(), buf.len());
                if n <= 0 {
                    stdin_open = false;
                    if keep_alive {
                        bg_requested = true;
                    }
                } else if keep_alive && buf[..n as usize].contains(&0x03) {
                    // Ctrl-C: background instead of killing the agent.
                    bg_requested = true;
                    terminal_was_alive = true;
                } else {
                    pending.extend_from_slice(&buf[..n as usize]);
                }
            }
            // One background transition: restore the terminal, tell the
            // user, and register the session as attachable.
            if bg_requested && !backgrounded && keep_alive {
                backgrounded = true;
                stdin_open = false;
                foreground_gone = true;
                libc::tcsetattr(libc::STDIN_FILENO, libc::TCSADRAIN, &outer);
                // Hand a clean terminal to the shell: mouse/paste/focus
                // modes off, out of the alt screen, and any queued
                // mouse-event bytes flushed before the shell reads them.
                let _ = write_all_fd(libc::STDOUT_FILENO, TUI_MODES_DISABLE);
                let _ = libc::tcflush(libc::STDIN_FILENO, libc::TCIFLUSH);
                let msg = format!(
                    "glm: session backgrounded (pid {glm_pid}) - `glm attach` to resume, `glm kill` to stop\n"
                );
                let _ = write_all_fd(libc::STDOUT_FILENO, msg.as_bytes());
                mark_backgrounded(glm_pid);

                // When the terminal is still alive (Ctrl-C), the shell is
                // blocked waiting for us: fork so the PARENT exits and the
                // shell gets its prompt back, while the orphan child keeps
                // the agent, the tail buffer, and the attach socket.
                if terminal_was_alive {
                    let cpid = libc::fork();
                    if cpid < 0 {
                        // Fork failed: keep running in place (old behavior).
                    } else if cpid > 0 {
                        // Parent: registry row survives via child_pid.
                        libc::exit(0);
                    }
                    // Child (orphan): detach stdio from the terminal.
                    let null_fd = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR);
                    if null_fd >= 0 {
                        libc::dup2(null_fd, libc::STDIN_FILENO);
                        libc::dup2(null_fd, libc::STDOUT_FILENO);
                        libc::dup2(null_fd, libc::STDERR_FILENO);
                        if null_fd > 2 {
                            libc::close(null_fd);
                        }
                    }
                }
            }

            // New attach client.
            if let Some(listener) = &listener {
                let lfd = listener_fd.unwrap_or(-1);
                if lfd != -1 && fds[3].revents & libc::POLLIN != 0 {
                    if let Ok((stream, _)) = listener.accept() {
                        let _ = stream.set_nonblocking(true);
                        let mut stream = stream;
                        // Replay the retained tail, then go live. A newer
                        // client takes over from an older one.
                        let _ = stream.write_all(&tail);
                        client = Some(stream);
                        client_out.clear();
                        client_greeted = false;
                    }
                }
            }

            // Attached-terminal input, flushed into the pty like keystrokes.
            // The first bytes from a client are its winsize (rows/cols u16
            // LE + 4 pad) so the child TUI redraws at the right size.
            if let Some(c) = &client {
                let idx = if listener.is_some() { 4 } else { 3 };
                if idx < fds.len() && fds[idx].revents & libc::POLLIN != 0 {
                    let n = libc::read(c.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len());
                    if n <= 0 {
                        client = None; // detached
                        client_out.clear();
                        client_greeted = false;
                        greet_buf.clear();
                    } else if !client_greeted {
                        // Accumulate the 8-byte winsize preamble; stream
                        // writes can split or coalesce it with keystrokes.
                        greet_buf.extend_from_slice(&buf[..n as usize]);
                        if greet_buf.len() >= 8 {
                            client_greeted = true;
                            let ws = libc::winsize {
                                ws_row: u16::from_le_bytes([greet_buf[0], greet_buf[1]]),
                                ws_col: u16::from_le_bytes([greet_buf[2], greet_buf[3]]),
                                ws_xpixel: 0,
                                ws_ypixel: 0,
                            };
                            if ws.ws_row > 0 && ws.ws_col > 0 {
                                apply_winsz(master, ws);
                            }
                            pending.extend_from_slice(&greet_buf[8..]);
                            greet_buf.clear();
                        }
                    } else {
                        pending.extend_from_slice(&buf[..n as usize]);
                    }
                }
            }

            // Flush pending keystrokes into the pty as its buffer frees up.
            while !pending.is_empty() {
                let n = libc::write(master, pending.as_ptr().cast(), pending.len());
                if n < 0 {
                    let errno = std::io::Error::last_os_error();
                    if errno.raw_os_error() == Some(libc::EINTR) {
                        continue;
                    }
                    // EAGAIN: try again after POLLOUT. Anything else (child
                    // gone, EIO): drop the pending input and stop asking.
                    if errno.raw_os_error() != Some(libc::EAGAIN) {
                        pending.clear();
                        stdin_open = false;
                    }
                    break;
                }
                pending.drain(..n as usize);
            }

            // Flush queued bytes for the attach client.
            if let Some(c) = client.as_mut() {
                while !client_out.is_empty() {
                    let n = c.write(&client_out);
                    match n {
                        Ok(0) => break,
                        Ok(n) => {
                            client_out.drain(..n);
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                        Err(_) => {
                            client = None;
                            client_out.clear();
                            break;
                        }
                    }
                }
            }

            if fds[1].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
                let n = libc::read(master, buf.as_mut_ptr().cast(), buf.len());
                if n <= 0 {
                    if n < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EAGAIN)
                    {
                        // Spurious read race with the POLLOUT pass.
                    } else {
                        break;
                    }
                } else {
                    let chunk = rewriter.push(&buf[..n as usize]);
                    if !sid_done {
                        if let Some(sid) = capture_session_id(&tail, &chunk) {
                            sid_done = true;
                            update_sid(glm_pid, &sid);
                        }
                    }
                    push_tail(&mut tail, &chunk);
                    route_output(
                        &chunk,
                        &mut client,
                        &mut client_out,
                        &mut stdin_open,
                        foreground_gone,
                    );
                }
            }
        }

        let tail_chunk = rewriter.finish();
        push_tail(&mut tail, &tail_chunk);
        route_output(
            &tail_chunk,
            &mut client,
            &mut client_out,
            &mut stdin_open,
            foreground_gone,
        );

        // Reap the child; if it is still alive (e.g. our stdout died), give
        // it a moment to exit, then take it down.
        for attempt in 0.. {
            if libc::waitpid(
                pid,
                &mut status,
                if attempt == 0 { 0 } else { libc::WNOHANG },
            ) >= 0
            {
                reaped = true;
                break;
            }
            if std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
                break;
            }
        }
        if !reaped {
            libc::killpg(pid, libc::SIGTERM);
            let _ = libc::waitpid(pid, &mut status, 0);
        }

        // The agent exited: tell an attached terminal the exact code, then
        // let it fall through to EOF.
        let code = if libc::WIFEXITED(status) {
            libc::WEXITSTATUS(status)
        } else if libc::WIFSIGNALED(status) {
            128 + libc::WTERMSIG(status)
        } else {
            1
        };
        if let Some(c) = client.as_mut() {
            let _ = c.write_all(format!("EXIT:{code}\n").as_bytes());
        }
        code
    }

    /// Append to the replay buffer, capping its size (drop from the front).
    fn push_tail(tail: &mut Vec<u8>, chunk: &[u8]) {
        tail.extend_from_slice(chunk);
        if tail.len() > TAIL_CAP {
            let drop = tail.len() - TAIL_CAP;
            tail.drain(..drop);
        }
    }

    /// Send output to the attach client when present; otherwise to the
    /// (possibly dead) terminal. Losing the terminal is fine — the tail
    /// buffer keeps the bytes for replay.
    fn route_output(
        chunk: &[u8],
        client: &mut Option<UnixStream>,
        client_out: &mut Vec<u8>,
        stdin_open: &mut bool,
        foreground_gone: bool,
    ) {
        if chunk.is_empty() {
            return;
        }
        if let Some(c) = client {
            if client_out.is_empty() {
                match c.write(chunk) {
                    Ok(n) if n == chunk.len() => return,
                    Ok(n) => client_out.extend_from_slice(&chunk[n..]),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        client_out.extend_from_slice(chunk)
                    }
                    Err(_) => {
                        *client = None;
                        client_out.clear();
                    }
                }
            } else if client_out.len() + chunk.len() <= OUTBOX_CAP {
                client_out.extend_from_slice(chunk);
            }
            return;
        }
        if foreground_gone {
            // The user detached with Ctrl-C: the terminal belongs to the
            // shell now; the tail buffer keeps the bytes for replay.
            return;
        }
        // Raw fd write: std's stdout is line-buffered, which would hold TUI
        // redraws until a newline appears.
        if write_all_fd(libc::STDOUT_FILENO, chunk).is_err() {
            // Outer terminal gone: stop feeding it input; under keep-alive
            // the agent stays alive in the background.
            *stdin_open = false;
        }
    }

    /// Capture the Claude Code session id from the exit banner once the
    /// "--resume <id>" token has fully arrived after the rewritten hint.
    fn capture_session_id(tail: &[u8], chunk: &[u8]) -> Option<String> {
        let start = tail.len().saturating_sub(2048);
        let mut window = Vec::with_capacity(tail.len() - start + chunk.len());
        window.extend_from_slice(&tail[start..]);
        window.extend_from_slice(chunk);
        super::session_id_from_tail(&window)
    }

    fn mark_backgrounded(glm_pid: u32) {
        let mut entries = registry::load(|_| true);
        if let Some(e) = entries.iter_mut().find(|e| e.pid == glm_pid) {
            e.backgrounded = true;
            e.backgrounded_at_ms = Some(now_ms());
        }
        if let Err(e) = registry::save(&entries) {
            eprintln!("glm: warning: session registry: {e:#}");
        }
    }

    fn update_sid(glm_pid: u32, sid: &str) {
        let mut entries = registry::load(|_| true);
        if let Some(e) = entries.iter_mut().find(|e| e.pid == glm_pid) {
            e.sid = Some(sid.to_string());
        }
        if let Err(e) = registry::save(&entries) {
            eprintln!("glm: warning: session registry: {e:#}");
        }
    }

    fn now_ms() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }

    unsafe fn install_signal_handlers(write_fd: libc::c_int) {
        extern "C" fn handler(
            sig: libc::c_int,
            _info: *mut libc::siginfo_t,
            _ctx: *mut libc::c_void,
        ) {
            // async-signal-safe: a single write into a non-blocking pipe
            let b = sig as u8;
            unsafe {
                let _ = libc::write(
                    SIGNAL_FD.load(std::sync::atomic::Ordering::Relaxed) as libc::c_int,
                    b as *const libc::c_void,
                    1,
                );
            }
        }
        static SIGNAL_FD: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);
        SIGNAL_FD.store(write_fd, std::sync::atomic::Ordering::Relaxed);

        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction =
            handler as extern "C" fn(libc::c_int, *mut libc::siginfo_t, *mut libc::c_void) as usize;
        sa.sa_flags = libc::SA_SIGINFO;
        libc::sigemptyset(&mut sa.sa_mask);
        for sig in [libc::SIGWINCH].into_iter().chain(FORWARDED) {
            libc::sigaction(sig, &sa, std::ptr::null_mut());
        }
    }

    fn exec_error(errno: i32) -> anyhow::Error {
        let err = std::io::Error::from_raw_os_error(errno);
        if err.kind() == std::io::ErrorKind::NotFound {
            anyhow::anyhow!(
                "claude is not installed. Install Claude Code first:\n  npm install -g @anthropic-ai/claude-code"
            )
        } else {
            anyhow::anyhow!("failed to exec claude: {err}")
        }
    }

    fn current_winsz(fd: libc::c_int) -> libc::winsize {
        unsafe {
            let mut ws: libc::winsize = std::mem::zeroed();
            if libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) != 0 || ws.ws_row == 0 || ws.ws_col == 0 {
                return libc::winsize {
                    ws_row: 24,
                    ws_col: 80,
                    ws_xpixel: 0,
                    ws_ypixel: 0,
                };
            }
            ws
        }
    }

    fn apply_winsz(fd: libc::c_int, ws: libc::winsize) {
        unsafe {
            libc::ioctl(fd, libc::TIOCSWINSZ, &ws);
        }
    }

    fn write_all_fd(fd: libc::c_int, mut data: &[u8]) -> std::io::Result<()> {
        while !data.is_empty() {
            let n = unsafe { libc::write(fd, data.as_ptr().cast(), data.len()) };
            if n < 0 {
                let errno = std::io::Error::last_os_error();
                if errno.raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                return Err(errno);
            }
            data = &data[n as usize..];
        }
        Ok(())
    }

    /// `glm attach [pid]`: reconnect to a backgrounded session. Picks the
    /// most recent backgrounded one when no pid is given. Never returns Ok:
    /// exits with the agent's status (or 1 when nothing to attach to).
    pub fn attach(target: Option<u32>) -> Result<()> {
        // SAFETY: raw pty/ioctl handling; exits the process when done.
        unsafe { attach_inner(target) }
    }

    unsafe fn attach_inner(target: Option<u32>) -> Result<()> {
        let entries = registry::load(|_| true);
        let candidate = registry::pick_attach_target(&entries, target);
        let Some(entry) = candidate else {
            eprintln!("glm: no backgrounded session to attach to; see `glm sessions`");
            libc::exit(1);
        };
        let Ok(stream) = UnixStream::connect(crate::paths::relay_socket(entry.pid)) else {
            eprintln!(
                "glm: session {} is not accepting attaches; is its glm still running?",
                entry.pid
            );
            libc::exit(1);
        };
        let _ = stream.set_nonblocking(true);

        // Raw mode on our own terminal for the duration of the attach.
        let mut outer: libc::termios = std::mem::zeroed();
        unsafe {
            if libc::tcgetattr(libc::STDIN_FILENO, &mut outer) != 0 {
                bail!("stdin is not a terminal");
            }
            let mut raw = outer;
            libc::cfmakeraw(&mut raw);
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSADRAIN, &raw);
        }

        // The still-running TUI expects its modes to be active; it will not
        // re-send them for a re-attach.
        let _ = write_all_fd(libc::STDOUT_FILENO, TUI_MODES_ENABLE);

        let mut sock = stream;
        let mut term = std::io::stdout();
        // Preamble: the child TUI must match this terminal's size.
        let ws = current_winsz(libc::STDIN_FILENO);
        let mut greet = [0u8; 8];
        greet[0..2].copy_from_slice(&ws.ws_row.to_le_bytes());
        greet[2..4].copy_from_slice(&ws.ws_col.to_le_bytes());
        let _ = sock.write_all(&greet);
        let mut buf = [0u8; 8192];
        let mut exit_code: Option<i32> = None;
        let mut stdin_open = true;
        let mut out = Vec::new();
        loop {
            let mut fds = [
                libc::pollfd {
                    fd: if stdin_open { libc::STDIN_FILENO } else { -1 },
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: sock.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            let ready = libc::poll(fds.as_mut_ptr(), 2, -1);
            if ready < 0 {
                if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                break;
            }
            if fds[0].revents & libc::POLLIN != 0 {
                let n = libc::read(libc::STDIN_FILENO, buf.as_mut_ptr().cast(), buf.len());
                if n <= 0 {
                    stdin_open = false;
                } else {
                    // Ctrl-Q (0x11): detach, leave the agent running.
                    if buf[..n as usize].contains(&0x11) {
                        break;
                    }
                    if sock.write_all(&buf[..n as usize]).is_err() {
                        break;
                    }
                }
            }
            if fds[1].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
                let n = sock.read(&mut buf);
                match n {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        out.extend_from_slice(&buf[..n]);
                        // Surface a trailing EXIT line to the exit path.
                        while let Some(p) = super::find(&out, b"EXIT:") {
                            if let Some(nl) = super::find(&out[p..], b"\n") {
                                let line = String::from_utf8_lossy(&out[p..p + nl]).into_owned();
                                exit_code = line
                                    .split(':')
                                    .nth(1)
                                    .and_then(|c| c.trim().parse::<i32>().ok());
                                out.drain(..p + nl + 1);
                            } else {
                                break;
                            }
                        }
                        if term.write_all(&out).is_err() {
                            break;
                        }
                        out.clear();
                    }
                }
            }
        }

        // Hand a clean terminal back: TUI modes off, queued bytes dropped.
        let _ = write_all_fd(libc::STDOUT_FILENO, TUI_MODES_DISABLE);
        unsafe {
            let _ = libc::tcflush(libc::STDIN_FILENO, libc::TCIFLUSH);
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSADRAIN, &outer);
        }
        let _ = term.flush();
        match exit_code {
            Some(c) => libc::exit(c),
            None => {
                println!("glm: detached (agent still running; `glm attach` to reconnect)");
                libc::exit(0)
            }
        }
    }
}

#[cfg(unix)]
pub use imp::{attach, run};

#[cfg(not(unix))]
pub fn run(_program: &str, _args: &[String]) -> anyhow::Result<()> {
    anyhow::bail!("pty relay not supported on this platform")
}

#[cfg(not(unix))]
pub fn attach(_target: Option<u32>) -> anyhow::Result<()> {
    anyhow::bail!("attach not supported on this platform")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn emit(r: &mut HintRewriter, chunks: &[&[u8]]) -> String {
        let mut out = Vec::new();
        for c in chunks {
            out.extend(r.push(c));
        }
        out.extend(r.finish());
        String::from_utf8(out).unwrap()
    }

    const HINT_LF: &str = "\nResume this session with:\nclaude --resume abc-123\n";
    const HINT_CRLF: &str = "\r\nResume this session with:\r\nclaude --resume abc-123\r\n";

    #[test]
    fn rewrites_hint_in_one_chunk() {
        for hint in [HINT_LF, HINT_CRLF] {
            let mut r = HintRewriter::new();
            let expected = hint.replacen("claude", "glm", 1);
            assert_eq!(emit(&mut r, &[hint.as_bytes()]), expected);
        }
    }

    #[test]
    fn rewrites_hint_split_at_every_boundary() {
        for hint in [HINT_LF, HINT_CRLF] {
            let expected = hint.replacen("claude", "glm", 1);
            let bytes = hint.as_bytes();
            for split in 1..bytes.len() {
                let mut r = HintRewriter::new();
                let out = emit(&mut r, &[&bytes[..split], &bytes[split..]]);
                assert_eq!(out, expected, "split at {split} of {hint:?}");
            }
        }
    }

    #[test]
    fn passes_other_output_through() {
        let mut r = HintRewriter::new();
        let out = emit(
            &mut r,
            &[b"hello world\n", b"normal TUI bytes \x1b[2m dim \x1b[0m\n"],
        );
        assert_eq!(out, "hello world\nnormal TUI bytes \x1b[2m dim \x1b[0m\n");
    }

    #[test]
    fn rewrites_only_once() {
        // A session that legitimately echoes the same text (e.g. a Read of
        // these very sources) is only touched on the first occurrence.
        let mut r = HintRewriter::new();
        let out = emit(&mut r, &[HINT_LF.as_bytes(), HINT_LF.as_bytes()]);
        assert_eq!(out.matches("glm --resume").count(), 1);
        assert!(out.contains("claude --resume"));
    }

    #[test]
    fn heading_without_claude_passes_through() {
        let mut r = HintRewriter::new();
        let out = emit(
            &mut r,
            &[b"Resume this session with:\nother-tool --resume x\n"],
        );
        assert_eq!(out, "Resume this session with:\nother-tool --resume x\n");
    }

    #[test]
    fn partial_prefix_at_stream_end_is_flushed() {
        // Ends with a prefix of the heading that never completes.
        let mut r = HintRewriter::new();
        let out = emit(&mut r, &[b"some output\nResume this se"]);
        assert_eq!(out, "some output\nResume this se");
    }

    #[test]
    fn idle_frames_emit_fully_and_promptly() {
        // Normal output must never be held back: a TUI frame with no hint
        // in sight passes through in the same push, with nothing retained.
        let mut r = HintRewriter::new();
        let frame = b"\x1b[2K\x1b[1Aspinner frame three-dots\x1b[0m";
        assert_eq!(r.push(frame), frame.to_vec());
        let frame2 = b"\x1b[2K\x1b[1Aloading...";
        assert_eq!(r.push(frame2), frame2.to_vec());
        assert!(r.finish().is_empty());
    }

    #[test]
    fn hold_back_is_bounded_by_partial_matches() {
        // A chunk ending mid-heading holds only the partial match, not a
        // fixed window; the completed hint still rewrites.
        let mut r = HintRewriter::new();
        let first = r.push(b"ctx \x1b[0m Resume this session with:");
        assert_eq!(first, b"ctx \x1b[0m ".to_vec());
        let second = r.push(b"\nclaude --resume id-9\n");
        assert_eq!(
            second,
            b"Resume this session with:\nglm --resume id-9\n".to_vec()
        );
        assert!(r.finish().is_empty());
    }

    #[test]
    fn held_heading_that_never_completes_flushes_on_next_push() {
        let mut r = HintRewriter::new();
        assert_eq!(r.push(b"ok\nResume this session with:"), b"ok\n".to_vec());
        // The heading turns out to be ordinary text (followed by other
        // content): it must still be emitted, unchanged.
        assert_eq!(
            r.push(b" and other stuff\n"),
            b"Resume this session with: and other stuff\n".to_vec()
        );
    }

    #[test]
    fn session_id_captured_from_banner_tail() {
        let banner = b"\x1b[2m\nResume this session with:\nglm --resume e268bf1d-879b-4749-b13e-f0ffba7e4c10\n";
        assert_eq!(
            session_id_from_tail(banner),
            Some("e268bf1d-879b-4749-b13e-f0ffba7e4c10".to_string())
        );
        // incomplete id: no capture yet
        assert_eq!(session_id_from_tail(b"glm --resume e268bf"), None);
        assert_eq!(session_id_from_tail(b"no banner here"), None);
    }

    #[test]
    fn print_mode_is_never_relayed() {
        let mk = |xs: &[&str]| xs.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(is_print_mode(&mk(&["-p", "hi"])));
        assert!(is_print_mode(&mk(&["--print", "hi"])));
        assert!(!is_print_mode(&mk(&["--resume", "abc"])));
        assert!(!is_print_mode(&mk(&["--paste"])));
    }

    #[test]
    fn keep_alive_and_relay_env_flags() {
        for v in ["0", "false", "no", "OFF", " false "] {
            std::env::set_var("GLM_KEEP_ALIVE", v);
            assert!(!keep_alive_enabled(), "GLM_KEEP_ALIVE={v}");
        }
        for v in ["1", "true", "yes", ""] {
            std::env::set_var("GLM_KEEP_ALIVE", v);
            assert!(keep_alive_enabled(), "GLM_KEEP_ALIVE={v:?}");
        }
        std::env::remove_var("GLM_KEEP_ALIVE");
        assert!(keep_alive_enabled());
        // GLM_RELAY kill switch stays independent.
        std::env::set_var("GLM_RELAY", "0");
        assert!(relay_disabled_by_env());
        std::env::remove_var("GLM_RELAY");
        assert!(!relay_disabled_by_env());
    }
}

#[cfg(all(unix, test))]
mod tui_mode_tests {
    use super::imp::{TUI_MODES_DISABLE, TUI_MODES_ENABLE};

    #[test]
    fn disable_covers_mouse_paste_focus_altscreen() {
        let d = String::from_utf8_lossy(TUI_MODES_DISABLE);
        for mode in [
            "?1000l", "?1002l", "?1003l", "?1006l", "?1004l", "?2004l", "?1049l", "?25h",
        ] {
            assert!(d.contains(mode), "disable missing {mode}: {d}");
        }
    }

    #[test]
    fn enable_covers_mouse_paste_focus_altscreen() {
        let e = String::from_utf8_lossy(TUI_MODES_ENABLE);
        for mode in [
            "?1049h", "?25l", "?1000h", "?1002h", "?1003h", "?1006h", "?1004h", "?2004h",
        ] {
            assert!(e.contains(mode), "enable missing {mode}: {e}");
        }
    }
}
