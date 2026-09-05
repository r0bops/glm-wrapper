use std::env;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// os name used by the installer/self-update (`linux`, `darwin`, ...)
pub fn os_name() -> &'static str {
    match std::env::consts::OS {
        "linux" => "linux",
        "macos" => "darwin",
        other => other,
    }
}

/// arch name used by the installer/self-update (`x86_64`, `aarch64`, ...)
pub fn arch_name() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        other => other,
    }
}

/// `~/.config/glm` (respects XDG_CONFIG_HOME).
pub fn config_dir() -> std::path::PathBuf {
    if let Some(xdg) = env::var_os("XDG_CONFIG_HOME") {
        return Path::new(&xdg).join("glm");
    }
    dirs::config_dir()
        .map(|d| d.join("glm"))
        .unwrap_or_else(|| std::path::PathBuf::from(".glm"))
}

pub fn config_file() -> std::path::PathBuf {
    config_dir().join("config.toml")
}

pub fn key_file() -> std::path::PathBuf {
    config_dir().join("key")
}

pub fn quota_cache_file() -> std::path::PathBuf {
    config_dir().join("quota-cache.json")
}

pub fn settings_file() -> std::path::PathBuf {
    config_dir().join("settings.json")
}

/// Registry of live/recent interactive sessions.
pub fn sessions_file() -> std::path::PathBuf {
    config_dir().join("sessions.json")
}

/// Unix socket a backgrounded relay listens on for `glm attach`.
pub fn relay_socket(pid: u32) -> std::path::PathBuf {
    config_dir().join(format!("relay-{pid}.sock"))
}

/// Claude Code's config dir (`CLAUDE_CONFIG_DIR` or `~/.claude`).
pub fn claude_dir() -> std::path::PathBuf {
    if let Some(dir) = env::var_os("CLAUDE_CONFIG_DIR") {
        return PathBuf::from(dir);
    }
    dirs::home_dir()
        .map(|h| h.join(".claude"))
        .unwrap_or_else(|| PathBuf::from(".claude"))
}

/// Directory of personal custom slash commands (`/name` -> `<name>.md`).
pub fn claude_commands_dir() -> std::path::PathBuf {
    claude_dir().join("commands")
}

/// Canonical path used for the statusline hook command and the env key.
pub fn current_exe() -> std::path::PathBuf {
    static EXE: OnceLock<std::path::PathBuf> = OnceLock::new();
    EXE.get_or_init(|| {
        env::current_exe()
            .and_then(|p| p.canonicalize())
            .unwrap_or_else(|_| env::current_exe().unwrap_or_else(|_| PathBuf::from("glm")))
    })
    .clone()
}
