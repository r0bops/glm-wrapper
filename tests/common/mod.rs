//! Shared helpers: a minimal single-threaded HTTP stub on std::net, plus
//! helpers to run the built `glm` binary with an isolated XDG_CONFIG_HOME.
//! Each test binary compiles this module standalone, so helpers unused by
//! one binary are fine.
#![allow(dead_code)]
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Tiny HTTP/1.1 server: responds to every request with the first queued
/// (status, body) pair, optionally counting requests and recording the
/// Authorization headers it saw.
pub struct StubServer {
    pub addr: String,
    pub requests: Arc<Mutex<Vec<String>>>,
    shutdown: Arc<Mutex<bool>>,
}

impl StubServer {
    pub fn start(responses: Vec<(u16, String)>) -> StubServer {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind stub listener");
        let addr = listener.local_addr().unwrap().to_string();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let shutdown = Arc::new(Mutex::new(false));
        let (reqs, stop) = (requests.clone(), shutdown.clone());
        let responses = Arc::new(Mutex::new(responses));
        std::thread::spawn(move || {
            listener
                .set_nonblocking(true)
                .expect("nonblocking listener");
            let mut conns: Vec<TcpStream> = Vec::new();
            loop {
                if *stop.lock().unwrap() {
                    return;
                }
                match listener.accept() {
                    Ok((stream, _)) => {
                        stream.set_nonblocking(true).ok();
                        conns.push(stream);
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(_) => return,
                }
                let mut i = 0;
                while i < conns.len() {
                    let mut buf = [0u8; 8192];
                    match conns[i].read(&mut buf) {
                        Ok(0) => {
                            conns.remove(i);
                            continue;
                        }
                        Ok(n) => {
                            let head = String::from_utf8_lossy(&buf[..n]);
                            let req_line = head.lines().next().unwrap_or("").to_string();
                            let auth = head
                                .lines()
                                .find(|l| l.to_ascii_lowercase().starts_with("authorization:"))
                                .map(|l| l.to_string())
                                .unwrap_or_default();
                            reqs.lock().unwrap().push(format!("{req_line} | {auth}"));
                            let (status, body) = {
                                let mut resp = responses.lock().unwrap();
                                if resp.len() > 1 {
                                    resp.remove(0)
                                } else {
                                    resp[0].clone()
                                }
                            };
                            let reason = if status == 200 { "OK" } else { "ERR" };
                            let resp = format!(
                                "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                                body.len(),
                                body
                            );
                            conns[i].write_all(resp.as_bytes()).ok();
                            conns[i].flush().ok();
                            // close this connection
                            conns.remove(i);
                            continue;
                        }
                        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                        Err(_) => {
                            conns.remove(i);
                            continue;
                        }
                    }
                    i += 1;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        });
        StubServer {
            addr,
            requests,
            shutdown,
        }
    }

    /// A stub that sleeps (or never responds) so client timeouts fire.
    pub fn start_silent() -> StubServer {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind stub listener");
        let addr = listener.local_addr().unwrap().to_string();
        let shutdown = Arc::new(Mutex::new(false));
        let stop = shutdown.clone();
        std::thread::spawn(move || {
            listener.set_nonblocking(true).ok();
            let mut conns: Vec<TcpStream> = Vec::new();
            loop {
                if *stop.lock().unwrap() {
                    return;
                }
                match listener.accept() {
                    Ok((stream, _)) => {
                        stream.set_nonblocking(true).ok();
                        conns.push(stream);
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(_) => return,
                }
                // never write: hold connections open
                conns.retain(|mut c| {
                    let mut buf = [0u8; 1024];
                    match c.read(&mut buf) {
                        Ok(0) | Err(_) => false,
                        Ok(_) => true,
                    }
                });
                std::thread::sleep(Duration::from_millis(5));
            }
        });
        StubServer {
            addr,
            requests: Arc::new(Mutex::new(Vec::new())),
            shutdown,
        }
    }

    pub fn auth_headers(&self) -> Vec<String> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter_map(|r| r.split(" | ").nth(1).map(|a| a.trim().to_string()))
            .collect()
    }

    pub fn request_lines(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for StubServer {
    fn drop(&mut self) {
        *self.shutdown.lock().unwrap() = true;
    }
}

/// Binary path for integration tests (target/debug/glm).
pub fn glm_bin() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("target");
    p.push(if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    });
    p.push("glm");
    assert!(p.exists(), "glm binary missing at {}", p.display());
    p
}

/// Run glm with an isolated config dir and env; returns (stdout, stderr, code).
pub fn run_glm(
    args: &[&str],
    xdg: &std::path::Path,
    envs: &[(&str, &str)],
) -> (String, String, i32) {
    let out = run_glm_cmd(args, xdg, envs);
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code().unwrap_or(-1),
    )
}

pub fn run_glm_cmd(args: &[&str], xdg: &std::path::Path, envs: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(glm_bin());
    cmd.args(args)
        .env("XDG_CONFIG_HOME", xdg)
        // Keep the installed slash commands out of the developer's ~/.claude.
        .env("CLAUDE_CONFIG_DIR", xdg.join("claude"))
        // An empty PATH dir: tests that expect "claude missing" must not
        // depend on where the host machine installed it.
        .env("PATH", empty_path_dir())
        .env_remove("GLM_API_KEY")
        .env_remove("ZAI_API_KEY")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("GLM_PROFILE")
        .env_remove("GLM_MODEL")
        .env_remove("GLM_BASE_URL")
        .env_remove("GLM_USAGE_QUOTA_URL")
        .env_remove("BIGMODEL_USAGE_QUOTA_URL");
    for (k, v) in envs {
        cmd.env(k, v);
    }
    cmd.output().expect("spawn glm")
}

/// A scratch XDG_CONFIG_HOME per test.
pub fn scratch_xdg(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("glm-it-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch config dir");
    dir
}

pub const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

pub fn fixture(name: &str) -> String {
    std::fs::read_to_string(format!("{FIXTURES}/{name}")).expect("fixture exists")
}

/// A directory guaranteed to contain no executables, used as PATH.
pub fn empty_path_dir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join("glm-it-empty-path");
    std::fs::create_dir_all(&dir).expect("create empty PATH dir");
    dir
}
