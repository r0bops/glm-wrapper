//! Session registry: a small JSON file at ~/.config/glm/sessions.json
//! tracking every interactive glm session (one entry per running relay).
//! Written by the relay on start/exit and read by `glm sessions` / `glm kill`
//! / `glm attach`. Entries whose glm and claude pids are both gone are
//! dropped on load, so a crash never leaves ghosts behind.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionEntry {
    /// glm's own pid (the relay process).
    pub pid: u32,
    /// claude's pid: session leader of the child process group. Killing this
    /// group kills the agent but not the relay.
    pub child_pid: u32,
    /// Claude Code session id, captured from the exit banner when seen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sid: Option<String>,
    /// Working directory the session was started in.
    pub cwd: String,
    /// Epoch ms when the relay started.
    pub started_ms: i64,
    /// True once the terminal went away and the relay is keeping the agent
    /// alive in the background (reconnect with `glm attach`).
    pub backgrounded: bool,
    /// When the session was backgrounded: "last" for `glm attach` means
    /// last-backgrounded, not last-started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backgrounded_at_ms: Option<i64>,
}

/// Load entries, dropping stale ones (neither pid alive). `alive` decides
/// whether a pid belongs to a live process.
pub fn load(alive: impl Fn(u32) -> bool) -> Vec<SessionEntry> {
    let path = crate::paths::sessions_file();
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    let Ok(entries) = serde_json::from_str::<Vec<SessionEntry>>(&raw) else {
        return Vec::new();
    };
    entries
        .into_iter()
        .filter(|e| alive(e.pid) || alive(e.child_pid))
        .collect()
}

/// Merge `entries` into the registry and persist atomically. Rows with the
/// same pid are replaced; other rows are preserved.
pub fn save(entries: &[SessionEntry]) -> anyhow::Result<()> {
    let path = crate::paths::sessions_file();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let existing: Vec<SessionEntry> = {
        match std::fs::read_to_string(&path) {
            Ok(raw) => serde_json::from_str(&raw).unwrap_or_default(),
            Err(_) => Vec::new(),
        }
    };
    let mut merged: Vec<SessionEntry> = existing
        .into_iter()
        .filter(|e| !entries.iter().any(|n| n.pid == e.pid))
        .collect();
    merged.extend_from_slice(entries);
    merged.sort_by_key(|e| e.started_ms);
    crate::util::atomic_write(&path, &serde_json::to_string_pretty(&merged)?)
        .map_err(|e| anyhow::anyhow!("{e:#}"))
}

/// The attach target: the most recently backgrounded session. `target`
/// pins a specific glm/claude pid when given.
pub fn pick_attach_target(entries: &[SessionEntry], target: Option<u32>) -> Option<&SessionEntry> {
    match target {
        Some(p) => entries.iter().find(|e| e.pid == p || e.child_pid == p),
        None => entries
            .iter()
            .filter(|e| e.backgrounded)
            .max_by_key(|e| e.backgrounded_at_ms.unwrap_or(e.started_ms)),
    }
}

/// Resolve a user-supplied target (exact pid, or a prefix of a pid /
/// session id) to one session. A prefix of at least 3 characters must
/// match exactly one entry — if several match, the error lists them so the
/// user can add another digit; exact pids always win over prefixes.
pub fn resolve_target<'e>(
    entries: &'e [SessionEntry],
    target: &str,
) -> anyhow::Result<Option<&'e SessionEntry>> {
    let t = target.trim();
    if t.is_empty() {
        return Ok(None);
    }
    // Exact numeric pids take priority and are exempt from the length rule.
    if let Ok(num) = t.parse::<u32>() {
        let exact: Vec<&SessionEntry> = entries
            .iter()
            .filter(|e| e.pid == num || e.child_pid == num)
            .collect();
        if exact.len() == 1 {
            return Ok(Some(exact[0]));
        }
    }
    if t.chars().count() < 3 {
        anyhow::bail!("give at least 3 characters (got {t:?})");
    }
    let tl = t.to_lowercase();
    let matches: Vec<&SessionEntry> = entries
        .iter()
        .filter(|e| {
            e.pid.to_string().starts_with(t)
                || e.child_pid.to_string().starts_with(t)
                || e.sid
                    .as_deref()
                    .is_some_and(|s| s.to_lowercase().starts_with(&tl))
        })
        .collect();
    match matches.len() {
        1 => Ok(Some(matches[0])),
        0 => anyhow::bail!("no session matches {t:?}; see `glm sessions`"),
        _ => {
            let list = matches
                .iter()
                .map(|e| {
                    format!(
                        "  glm pid {} (agent {}) {}",
                        e.pid,
                        e.child_pid,
                        e.sid.as_deref().unwrap_or("-")
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            anyhow::bail!(
                "prefix {t:?} matches {} sessions - add another digit:\n{list}",
                matches.len()
            )
        }
    }
}

/// Remove the entry for `pid` (best effort; used on clean exit).
pub fn remove(pid: u32) {
    let path = crate::paths::sessions_file();
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return;
    };
    let Ok(entries) = serde_json::from_str::<Vec<SessionEntry>>(&raw) else {
        return;
    };
    let kept: Vec<SessionEntry> = entries.into_iter().filter(|e| e.pid != pid).collect();
    if let Ok(body) = serde_json::to_string_pretty(&kept) {
        let _ = crate::util::atomic_write(&path, &body);
    }
}

/// Whether `pid` belongs to a live process (signal 0 probe). Zombies count
/// as alive here: a relay that just exited is reaped by the shell promptly.
pub fn pid_alive(pid: u32) -> bool {
    #[cfg(unix)]
    unsafe {
        let r = libc::kill(pid as i32, 0);
        r == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(not(unix))]
    {
        pid != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn live(pid: u32) -> bool {
        pid == 100 || pid == 200 // test double
    }

    #[test]
    fn attach_target_prefers_last_backgrounded() {
        // Started first but backgrounded last: it must win over a
        // more-recently-started session.
        let older = SessionEntry {
            pid: 100,
            child_pid: 101,
            sid: None,
            cwd: "/tmp".into(),
            started_ms: 1,
            backgrounded: true,
            backgrounded_at_ms: Some(900),
        };
        let newer = SessionEntry {
            pid: 200,
            child_pid: 201,
            sid: None,
            cwd: "/tmp".into(),
            started_ms: 500,
            backgrounded: true,
            backgrounded_at_ms: Some(100),
        };
        let live = SessionEntry {
            pid: 300,
            child_pid: 301,
            sid: None,
            cwd: "/tmp".into(),
            started_ms: 600,
            backgrounded: false,
            backgrounded_at_ms: None,
        };
        let all = vec![older, newer, live.clone()];
        let pick = pick_attach_target(&all, None).unwrap();
        assert_eq!(pick.pid, 100, "last-backgrounded wins, not last-started");
        // explicit pid overrides recency
        assert_eq!(pick_attach_target(&all, Some(300)).unwrap().pid, 300);
        assert!(pick_attach_target(&all, Some(999)).is_none());
        // foreground-only entries are never auto-picked
        let fg_only = vec![live];
        assert!(pick_attach_target(&fg_only, None).is_none());
    }

    fn entry_with(pid: u32, child: u32, sid: Option<&str>) -> SessionEntry {
        SessionEntry {
            pid,
            child_pid: child,
            sid: sid.map(String::from),
            cwd: "/tmp".into(),
            started_ms: 1,
            backgrounded: false,
            backgrounded_at_ms: None,
        }
    }

    #[test]
    fn resolve_target_prefix_rules() {
        let entries = vec![
            entry_with(1861, 1862, Some("E268bf1d-879b-4749-b13e-f0ffba7e4c10")),
            entry_with(1865, 1866, Some("abc12345-0000-0000-0000-000000000000")),
        ];
        // exact pids (glm and agent) win at any length
        assert_eq!(resolve_target(&entries, "1861").unwrap().unwrap().pid, 1861);
        assert_eq!(resolve_target(&entries, "1866").unwrap().unwrap().pid, 1865);
        // 3 digits matching both sessions -> ambiguous; 4th digit resolves
        let err = resolve_target(&entries, "186").unwrap_err().to_string();
        assert!(err.contains("add another digit"), "{err}");
        assert!(err.contains("1861") && err.contains("1865"), "{err}");
        assert_eq!(resolve_target(&entries, "1861").unwrap().unwrap().pid, 1861);
        // 3-char prefix unique to one session -> resolves
        assert_eq!(resolve_target(&entries, "1866").unwrap().unwrap().pid, 1865);
        // session-id prefix, case-insensitive, unique at 3 chars
        assert_eq!(resolve_target(&entries, "e26").unwrap().unwrap().pid, 1861);
        assert_eq!(resolve_target(&entries, "E26").unwrap().unwrap().pid, 1861);
        assert_eq!(resolve_target(&entries, "abc").unwrap().unwrap().pid, 1865);
        // too short / no match
        assert!(resolve_target(&entries, "18").is_err());
        assert!(resolve_target(&entries, "999").is_err());
        // empty target -> no resolution (caller picks the default)
        assert!(resolve_target(&entries, "").unwrap().is_none());
    }

    #[test]
    fn save_merge_load_and_stale_drop() {
        let dir = std::env::temp_dir().join(format!("glm-reg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        // SAFETY: tests are single-threaded over this path.
        std::env::set_var("XDG_CONFIG_HOME", &dir);

        let e1 = SessionEntry {
            pid: 100,
            child_pid: 101,
            sid: None,
            cwd: "/tmp".into(),
            started_ms: 1,
            backgrounded: false,
            backgrounded_at_ms: None,
        };
        save(std::slice::from_ref(&e1)).unwrap();
        let e2 = SessionEntry {
            pid: 200,
            child_pid: 201,
            sid: Some("abc".into()),
            cwd: "/tmp".into(),
            started_ms: 2,
            backgrounded: true,
            backgrounded_at_ms: Some(42),
        };
        save(std::slice::from_ref(&e2)).unwrap(); // merge, not replace
        let all = load(live);
        assert_eq!(all.len(), 2);

        // a pid that is not alive is dropped on load
        let e3 = SessionEntry {
            pid: 300,
            child_pid: 301,
            sid: None,
            cwd: "/tmp".into(),
            started_ms: 3,
            backgrounded: false,
            backgrounded_at_ms: None,
        };
        save(&[e3]).unwrap();
        let all = load(live);
        assert_eq!(all.len(), 2);
        assert!(all.contains(&e1) && all.contains(&e2));

        std::env::remove_var("XDG_CONFIG_HOME");
        std::fs::remove_dir_all(&dir).ok();
    }
}
