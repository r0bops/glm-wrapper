use std::time::{Duration, Instant};

use anyhow::Result;

use crate::catalog::{display_model_id, ModelSpec};
use crate::config::ResolvedConfig;
use crate::quota::{self, Limit, QuotaEnvelope};

/// ANSI color codes used by the renderer.
pub const GREEN: &str = "\x1b[32m";
pub const YELLOW: &str = "\x1b[33m";
pub const RED: &str = "\x1b[31m";
pub const DIM: &str = "\x1b[2m";
pub const RESET: &str = "\x1b[0m";

/// The statusline JSON claude feeds us on stdin.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(default)]
pub struct StatuslineInput {
    pub model: Option<ModelInfo>,
    pub context_window: Option<ContextWindow>,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(default)]
pub struct ModelInfo {
    pub id: Option<String>,
    #[serde(rename = "display_name")]
    pub display_name: Option<String>,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(default)]
pub struct ContextWindow {
    #[serde(rename = "used_percentage")]
    pub used_percentage: Option<f64>,
    #[serde(rename = "total_input_tokens")]
    pub total_input_tokens: Option<u64>,
}

// Raw POSIX declarations so we can poll stdin with a deadline without pulling
// in a libc crate (not on the allowed dependency list). F_GETFL/F_SETFL have
// the same values on Linux and macOS; only O_NONBLOCK differs.
#[cfg(unix)]
mod ffi {
    use std::os::raw::{c_int, c_void};

    extern "C" {
        pub fn fcntl(fd: c_int, cmd: c_int, ...) -> c_int;
        pub fn read(fd: c_int, buf: *mut c_void, count: usize) -> isize;
    }

    pub const F_GETFL: c_int = 3;
    pub const F_SETFL: c_int = 4;
    #[cfg(target_os = "linux")]
    pub const O_NONBLOCK: c_int = 0o4000;
    #[cfg(target_os = "macos")]
    pub const O_NONBLOCK: c_int = 0x4;
}

/// Read stdin with a deadline. Returns None on timeout, EOF with no data, or
/// any IO error — the statusline never fails, it just renders without input.
/// Also returns None immediately when stdin is a terminal (interactive use).
pub fn read_stdin_with_deadline(timeout: Duration) -> Option<String> {
    #[cfg(unix)]
    {
        use std::io::IsTerminal;
        use std::os::fd::AsRawFd;
        let stdin = std::io::stdin();
        if stdin.is_terminal() {
            return None;
        }
        let fd = stdin.as_raw_fd();
        let flags = unsafe { ffi::fcntl(fd, ffi::F_GETFL) };
        if flags >= 0 {
            unsafe { ffi::fcntl(fd, ffi::F_SETFL, flags | ffi::O_NONBLOCK) };
        }
        let deadline = Instant::now() + timeout;
        let mut buf: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let n = unsafe { ffi::read(fd, chunk.as_mut_ptr().cast(), chunk.len()) };
            if n > 0 {
                buf.extend_from_slice(&chunk[..n as usize]);
                continue;
            }
            if n == 0 {
                break; // EOF
            }
            // EAGAIN: sleep a slice of the remaining time
            let remaining = deadline - now;
            std::thread::sleep(remaining.min(Duration::from_millis(10)));
        }
        if buf.is_empty() {
            None
        } else {
            Some(String::from_utf8_lossy(&buf).into_owned())
        }
    }
    #[cfg(not(unix))]
    {
        let _ = timeout;
        None
    }
}

/// Parse the input; junk that isn't JSON renders the fallback. Wrong-typed
/// fields are dropped, never fatal.
pub fn parse_input(raw: &str) -> Option<StatuslineInput> {
    if raw.trim().is_empty() {
        return None;
    }
    let mut v: serde_json::Value = serde_json::from_str(raw).ok()?;
    if let Some(map) = v.as_object_mut() {
        for k in ["model", "context_window"] {
            if let Some(val) = map.get(k) {
                if !val.is_object() {
                    map.remove(k);
                }
            }
        }
        for key in ["model", "context_window"] {
            let Some(obj) = map.get_mut(key).and_then(|m| m.as_object_mut()) else {
                continue;
            };
            let mut drop: Vec<String> = Vec::new();
            for (k, val) in obj.iter() {
                let string_ok = matches!(k.as_str(), "id" | "display_name");
                let f64_ok = matches!(k.as_str(), "used_percentage");
                let u64_ok = matches!(k.as_str(), "total_input_tokens");
                let ok = (string_ok && val.is_string())
                    || (f64_ok && val.is_number())
                    || (u64_ok && val.is_number());
                if !ok {
                    drop.push(k.clone());
                }
            }
            for k in drop {
                obj.remove(&k);
            }
        }
    }
    serde_json::from_value(v).ok()
}

/// The model name to display: display_name from claude, else the model id
/// (with [1m] marking for 1M-window models).
pub fn display_model_name(input: &StatuslineInput) -> Option<String> {
    if let Some(m) = &input.model {
        if let Some(n) = &m.display_name {
            if !n.trim().is_empty() {
                return Some(n.clone());
            }
        }
        if let Some(id) = &m.id {
            if !id.trim().is_empty() {
                return Some(display_model_id(id));
            }
        }
    }
    None
}

/// Compute the ctx percentage: prefer claude's precomputed used_percentage,
/// else tokens/window via the catalog.
pub fn context_used_pct(input: &StatuslineInput, cfg_model: &str) -> Option<(u64, u64)> {
    let cw = input.context_window.as_ref()?;
    if let Some(p) = cw.used_percentage {
        return Some((p.round() as u64, cw.total_input_tokens.unwrap_or(0)));
    }
    let spec = ModelSpec {
        base_id: crate::catalog::api_model_id(cfg_model),
        million: false,
    };
    crate::catalog::context_pct(&spec, cw.total_input_tokens)
}

fn color_for_pct(p: u64) -> &'static str {
    if p >= 90 {
        RED
    } else if p >= 70 {
        YELLOW
    } else {
        GREEN
    }
}

/// The rendered one-line payload: plain (no escapes) plus colored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Render {
    pub plain: String,
    pub colored: String,
}

/// Render the full statusline. `envelope` is the fetched quota (its data may
/// be empty on failure), `cfg_model` the configured model for ctx fallback.
pub fn render(input: &StatuslineInput, envelope: &QuotaEnvelope, cfg_model: &str) -> Render {
    let model = display_model_name(input).unwrap_or_else(|| display_model_id(cfg_model));
    let limits: &[Limit] = envelope
        .data
        .as_ref()
        .map(|d| d.limits.as_slice())
        .unwrap_or(&[]);

    let mut segments: Vec<String> = Vec::new();
    let mut colored_segments: Vec<String> = Vec::new();
    let mut have_quota = false;
    for l in limits {
        let (label, pct, ratio) = quota::statusline_segment(l);
        let mut piece = label.clone();
        let mut colored = label.clone();
        if let Some(p) = pct {
            have_quota = true;
            piece.push_str(&format!(" {p}%"));
            colored.push_str(&format!(" {}{p}%{}", color_for_pct(p), RESET));
        }
        if let Some(r) = &ratio {
            piece.push(' ');
            piece.push_str(r);
            colored.push_str(&format!(" {DIM}{r}{RESET}"));
        }
        segments.push(piece);
        colored_segments.push(colored);
    }
    let segment_line = segments.join(" | ");
    let colored_line = colored_segments.join(" | ");

    let ctx_txt = match context_used_pct(input, cfg_model) {
        Some((p, _)) => format!(" | ctx {p}%"),
        None => String::new(),
    };
    let ctx_colored = match context_used_pct(input, cfg_model) {
        Some((p, _)) => format!(" | ctx {}{p}%{}", color_for_pct(p), RESET),
        None => String::new(),
    };

    let (plain, colored) = if limits.is_empty() {
        // Nothing from the quota API: dim "usage n/a".
        (
            format!("GLM|{model} usage n/a{ctx_txt}"),
            format!("GLM|{model} {DIM}usage n/a{RESET}{ctx_colored}"),
        )
    } else {
        (
            format!("GLM|{model} {segment_line}{ctx_txt}"),
            format!("GLM|{model} {colored_line}{ctx_colored}"),
        )
    };
    let _ = have_quota;
    Render { plain, colored }
}

/// Command entry: read stdin, load/fetch quota, print one line, exit 0.
/// Never writes to stderr; failures render as the fallback line.
/// Last-resort line when even config loading failed.
pub fn fallback_line() -> String {
    format!("GLM|\u{2014} {DIM}usage n/a{RESET}")
}

pub fn run(cfg: &ResolvedConfig, key: &str) -> Result<()> {
    let raw = read_stdin_with_deadline(Duration::from_millis(250));
    let input = raw.as_deref().and_then(parse_input).unwrap_or_default();
    let (envelope, _from_cache) = {
        let quota_url = cfg.usage_quota_url.clone();
        let cache_ttl = Duration::from_secs(cfg.cache_ttl_secs);
        let fetch_timeout = Duration::from_secs(cfg.statusline_timeout_secs);
        quota::quota_for_statusline(
            &quota::cache_path(),
            &quota_url,
            key,
            cache_ttl,
            fetch_timeout,
        )
        .unwrap_or_default()
    };
    let out = render(&input, &envelope, &cfg.model);
    println!("{}", out.colored);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quota::QuotaData;

    fn env_with_limits(limits: Vec<Limit>) -> QuotaEnvelope {
        QuotaEnvelope {
            data: Some(QuotaData { limits }),
            ..Default::default()
        }
    }

    fn l(kind: &str, unit: Option<u64>, pct: f64) -> Limit {
        Limit {
            kind: kind.into(),
            unit,
            percentage: Some(pct),
            ..Default::default()
        }
    }

    #[test]
    fn empty_stdin_renders_fallback() {
        let r = render(
            &StatuslineInput::default(),
            &QuotaEnvelope::default(),
            "glm-5.3",
        );
        assert_eq!(r.plain, "GLM|glm-5.3 usage n/a");
        assert!(r.colored.contains(DIM));
    }

    #[test]
    fn limits_render_colored_segments() {
        let env = env_with_limits(vec![
            l("TOKENS_LIMIT", Some(5), 50.0),
            l("TIME_LIMIT", None, 75.0),
            l("TOKENS_LIMIT", Some(6), 95.0),
        ]);
        let r = render(&StatuslineInput::default(), &env, "glm-5.3");
        assert_eq!(r.plain, "GLM|glm-5.3 5h 50% | plan 75% | wk 95%");
        assert!(r.colored.contains(GREEN));
        assert!(r.colored.contains(RED));
        assert!(r.colored.contains(YELLOW));
        // the plain line carries no escape sequences
        assert!(!r.plain.contains('\x1b'));
    }

    #[test]
    fn model_display_from_input_wins() {
        let input = StatuslineInput {
            model: Some(ModelInfo {
                id: Some("glm-5.3".into()),
                display_name: Some("GLM 5.3".into()),
            }),
            context_window: None,
        };
        let r = render(&input, &QuotaEnvelope::default(), "glm-4.6");
        assert!(r.plain.starts_with("GLM|GLM 5.3 usage n/a"));
    }

    #[test]
    fn uses_configured_model_when_input_has_none() {
        let r = render(
            &StatuslineInput::default(),
            &QuotaEnvelope::default(),
            "glm-4.6",
        );
        assert!(r.plain.starts_with("GLM|glm-4.6"));
        let r = render(
            &StatuslineInput::default(),
            &QuotaEnvelope::default(),
            "glm-5.3[1m]",
        );
        assert!(r.plain.starts_with("GLM|glm-5.3[1m]"));
    }

    #[test]
    fn ctx_percentage_from_input_and_catalog() {
        let input = StatuslineInput {
            model: None,
            context_window: Some(ContextWindow {
                used_percentage: Some(33.3),
                total_input_tokens: Some(66_000),
            }),
        };
        let r = render(&input, &QuotaEnvelope::default(), "glm-4.6");
        assert!(r.plain.ends_with("| ctx 33%"));
        // fallback: tokens / catalog window
        let input = StatuslineInput {
            model: None,
            context_window: Some(ContextWindow {
                used_percentage: None,
                total_input_tokens: Some(50_000),
            }),
        };
        let r = render(&input, &QuotaEnvelope::default(), "glm-4.6");
        assert!(r.plain.ends_with("| ctx 25%"));
    }

    #[test]
    fn ratio_shows_dimmed_current_over_usage() {
        let mut lim = l("TOKENS_LIMIT", Some(5), 43.0);
        lim.current_value = Some(4300.0);
        lim.usage = Some(10000.0);
        let env = env_with_limits(vec![lim]);
        let r = render(&StatuslineInput::default(), &env, "glm-4.6");
        assert_eq!(r.plain, "GLM|glm-4.6 5h 43% 4300/10000");
        assert!(r.colored.contains(DIM));
    }

    #[test]
    fn fixture_parse_handles_junk() {
        assert!(parse_input("").is_none());
        assert!(parse_input("not json").is_none());
        assert!(parse_input(r#"{"model":{"id":"glm-4.6","display_name":"GLM 4.6"}}"#).is_some());
        // a wrong-typed top-level section is ignored, not fatal
        assert!(parse_input(r#"{"model":42}"#).is_some());
        assert!(parse_input(r#"{"context_window":{"used_percentage":"zzz"}}"#).is_some());
    }
}
