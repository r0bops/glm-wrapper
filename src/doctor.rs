use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;

use crate::config::{validate_keys, ConfigFile};
use crate::keys::KeySource;

/// Outcomes for a doctor check line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Pass,
    Warn,
    Fail,
}

#[derive(Debug, Clone)]
pub struct Check {
    pub level: Level,
    pub name: String,
    pub detail: String,
    pub fix: String,
}

impl Check {
    fn pass(name: &str, detail: &str) -> Check {
        Check {
            level: Level::Pass,
            name: name.to_string(),
            detail: detail.to_string(),
            fix: String::new(),
        }
    }
    fn warn(name: &str, detail: &str, fix: &str) -> Check {
        Check {
            level: Level::Warn,
            name: name.to_string(),
            detail: detail.to_string(),
            fix: fix.to_string(),
        }
    }
    fn fail(name: &str, detail: &str, fix: &str) -> Check {
        Check {
            level: Level::Fail,
            name: name.to_string(),
            detail: detail.to_string(),
            fix: fix.to_string(),
        }
    }
}

/// Everything doctor needs from the environment, so the checks are testable.
pub type FetchModels<'a> = Box<dyn Fn(&str, &str, Duration) -> Result<ModelProbe> + 'a>;

/// Result of probing the model-list route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelProbe {
    pub ids: Vec<String>,
    /// Which Authorization form the endpoint accepted.
    pub auth: AuthMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMode {
    Bearer,
    RawKey,
}

impl AuthMode {
    pub fn describe(self) -> &'static str {
        match self {
            AuthMode::Bearer => "Authorization: Bearer <key> accepted",
            AuthMode::RawKey => "Bearer rejected (401); raw key accepted",
        }
    }
}

pub struct DoctorContext<'a> {
    pub config_file: &'a Path,
    pub settings_path: &'a Path,
    pub resolved_key: Option<(String, KeySource)>,
    pub cfg: &'a ConfigFile,
    pub base_url: String,
    /// Resolved effective model ids (env/file/default merged).
    pub model: String,
    pub small_model: String,
    pub profile_source: crate::config::Source,
    /// Model-list check callback: `(url, key, timeout)` -> Ok(ids). Allows
    /// tests to stub the HTTP call.
    pub fetch_models: FetchModels<'a>,
    /// Set when the config file could not even be parsed (reported as a
    /// failing config check instead of aborting doctor).
    pub extra_fail: Option<String>,
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let cand = dir.join(name);
        if cand.is_file() {
            return Some(cand);
        }
    }
    None
}

/// Parse the leading dotted version of a `claude --version` line into
/// (major, minor, patch).
pub fn parse_version(line: &str) -> Option<(u64, u64, u64)> {
    let first = line.split_whitespace().next()?;
    let mut parts = first.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or("0").parse().ok()?;
    let patch = parts.next().unwrap_or("0").parse().ok()?;
    Some((major, minor, patch))
}

/// Version requirement: >= 2.0.14.
pub fn version_satisfies(v: (u64, u64, u64)) -> bool {
    v.0 > 2 || (v.0 == 2 && (v.1 > 0 || (v.1 == 0 && v.2 >= 14)))
}

/// `claude --version` check; Ok(true) when found and new enough.
fn check_claude_version() -> Result<bool, String> {
    let bin = find_on_path("claude").ok_or_else(|| "not on PATH".to_string())?;
    let out = std::process::Command::new(&bin)
        .arg("--version")
        .output()
        .map_err(|e| format!("failed to run {}: {e}", bin.display()))?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let v = parse_version(&stdout).ok_or_else(|| format!("unparsable version {stdout:?}"))?;
    Ok(version_satisfies(v))
}

/// Run the full battery. Each check collects its own result.
pub fn run(ctx: &DoctorContext) -> Vec<Check> {
    let mut checks = Vec::new();

    // 1. claude on PATH with version >= 2.0.14
    match find_on_path("claude") {
        Some(path) => match check_claude_version() {
            Ok(true) => checks.push(Check::pass(
                "claude",
                &format!("found at {}", path.display()),
            )),
            Ok(false) => checks.push(Check::fail(
                "claude",
                "claude is too old (need >= 2.0.14)",
                "npm install -g @anthropic-ai/claude-code",
            )),
            Err(msg) => checks.push(Check::fail(
                "claude",
                &format!("version check failed: {msg}"),
                "npm install -g @anthropic-ai/claude-code",
            )),
        },
        None => checks.push(Check::fail(
            "claude",
            "claude not found on PATH",
            "npm install -g @anthropic-ai/claude-code",
        )),
    }

    // 2. key resolution
    match &ctx.resolved_key {
        Some((_, src)) => checks.push(Check::pass("API key", &format!("resolved from {src}"))),
        None => checks.push(Check::fail(
            "API key",
            "no API key found",
            "set GLM_API_KEY, ZAI_API_KEY, or run `glm key set`",
        )),
    }

    // 3. ANTHROPIC_API_KEY in the parent env is a warning
    if std::env::var_os("ANTHROPIC_API_KEY").is_some() {
        checks.push(Check::warn(
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_API_KEY is set in the environment",
            "unset it, or glm strips it before exec'ing claude",
        ));
    } else {
        checks.push(Check::pass(
            "ANTHROPIC_API_KEY",
            "not set in the environment",
        ));
    }

    // 4. base URL reachable via /v1/models
    let key = ctx
        .resolved_key
        .as_ref()
        .map(|(k, _)| k.clone())
        .unwrap_or_default();
    let model_url = format!("{}/v1/models", ctx.base_url.trim_end_matches('/'));
    match (ctx.fetch_models)(&model_url, &key, Duration::from_secs(5)) {
        Ok(probe) => checks.push(Check::pass(
            "API endpoint",
            &format!(
                "{model_url} reachable ({}, {} model ids)",
                probe.auth.describe(),
                probe.ids.len()
            ),
        )),
        Err(e) => checks.push(Check::fail(
            "API endpoint",
            &format!("{model_url} not reachable: {e}"),
            "check GLM_BASE_URL / config.toml base_url and network",
        )),
    }

    // 5. model and small_model in the catalog
    if crate::catalog::is_known_model(&ctx.model) {
        checks.push(Check::pass(
            "model",
            &format!("{} is in the catalog", ctx.model),
        ));
    } else {
        checks.push(Check::warn(
            "model",
            &format!("{} is not in the bundled catalog", ctx.model),
            "glm models --refresh to see live ids",
        ));
    }
    if crate::catalog::is_known_model(&ctx.small_model) {
        // glm-5.3 generation has thinking permanently on: fine as a main
        // model, wasteful for the side calls Claude Code routes through
        // ANTHROPIC_SMALL_FAST_MODEL.
        if crate::catalog::thinking_always_on(&ctx.small_model) {
            checks.push(Check::warn(
                "small_model",
                &format!(
                    "{} has thinking always on; every side call pays thinking tokens",
                    ctx.small_model
                ),
                "set small_model = \"glm-4.7-flash\" in config.toml",
            ));
        } else {
            checks.push(Check::pass(
                "small_model",
                &format!("{} is in the catalog", ctx.small_model),
            ));
        }
    } else {
        checks.push(Check::warn(
            "small_model",
            &format!("{} is not in the bundled catalog", ctx.small_model),
            "update small_model in config.toml",
        ));
    }

    // 5b. effort compatibility with the selected model (docs.z.ai: GLM-5.2+
    // document low|high|max; other tiers have no effort parameter).
    if let Some(effort) = &ctx.cfg.effort {
        match crate::catalog::effort_levels(&ctx.model) {
            Some(levels) if !levels.contains(&effort.as_str()) => {
                checks.push(Check::warn(
                    "effort",
                    &format!(
                        "effort {effort:?} is not documented for {} (documented: {})",
                        ctx.model,
                        levels.join("/")
                    ),
                    &format!("glm config set effort {}", levels.last().unwrap_or(&"max")),
                ));
            }
            Some(_) => checks.push(Check::pass(
                "effort",
                &format!("{effort} is documented for {}", ctx.model),
            )),
            None => checks.push(Check::warn(
                "effort",
                &format!(
                    "{ctx_model} has no documented effort levels; pinned effort {effort:?} will be ignored or clamped",
                    ctx_model = ctx.model
                ),
                "remove effort from config.toml to use the model default",
            )),
        }
    }

    // 5c. thinking_budget = 0 disables thinking, which the glm-5.3
    // generation cannot do (thinking.type only supports "enabled").
    if ctx.cfg.thinking_budget == Some(0) && crate::catalog::thinking_always_on(&ctx.model) {
        checks.push(Check::warn(
            "thinking_budget",
            &format!(
                "budget 0 disables thinking, but {} cannot switch thinking off",
                ctx.model
            ),
            "raise thinking_budget or remove it from config.toml",
        ));
    }

    // 6. config.toml parses and has no unknown keys
    if let Some(err) = &ctx.extra_fail {
        checks.push(Check::fail(
            "config.toml",
            err,
            "edit config.toml or run `glm init` to reset",
        ));
    } else if ctx.config_file.exists() {
        match validate_keys(ctx.cfg) {
            Ok(()) => checks.push(Check::pass(
                "config.toml",
                &format!(
                    "parses cleanly (profile from {})",
                    ctx.profile_source.describe()
                ),
            )),
            Err(e) => checks.push(Check::fail(
                "config.toml",
                &format!("{e}"),
                "edit config.toml or run `glm init`",
            )),
        }
    } else {
        checks.push(Check::warn(
            "config.toml",
            "no config file (defaults in effect)",
            "run `glm init`",
        ));
    }

    // 7. settings.json path writable
    let writable = ctx
        .settings_path
        .parent()
        .map(|p| p.exists() || std::fs::create_dir_all(p).is_ok())
        .unwrap_or(false);
    if writable {
        checks.push(Check::pass(
            "settings.json",
            &format!("{} is writable", ctx.settings_path.display()),
        ));
    } else {
        checks.push(Check::fail(
            "settings.json",
            &format!("cannot write {}", ctx.settings_path.display()),
            "check permissions on ~/.config/glm",
        ));
    }

    checks
}

/// One line of output for a check.
pub fn format_check(c: &Check) -> String {
    let tag = match c.level {
        Level::Pass => "PASS",
        Level::Warn => "WARN",
        Level::Fail => "FAIL",
    };
    let fix = if c.fix.is_empty() {
        String::new()
    } else {
        format!("  ({})", c.fix)
    };
    format!("{tag}  {:<18} {}{}", c.name, c.detail, fix)
}

pub fn any_fail(checks: &[Check]) -> bool {
    checks.iter().any(|c| c.level == Level::Fail)
}

/// The real model-list fetch used outside tests: Bearer first, on 401 retry
/// with the raw key. Returns the live model ids.
pub fn fetch_model_ids(url: &str, key: &str, timeout: Duration) -> Result<ModelProbe> {
    let agent = ureq::AgentBuilder::new().timeout(timeout).build();
    let (body, auth) = match fetch_once(&agent, url, &format!("Bearer {key}")) {
        Ok(body) => (body, AuthMode::Bearer),
        Err(e) if is_unauthorized(&e) => (fetch_once(&agent, url, key)?, AuthMode::RawKey),
        Err(e) => return Err(e),
    };
    let ids =
        extract_model_ids(&body).ok_or_else(|| anyhow::anyhow!("response had no model id list"))?;
    Ok(ModelProbe { ids, auth })
}

fn fetch_once(agent: &ureq::Agent, url: &str, auth: &str) -> Result<String> {
    let resp = agent.get(url).set("Authorization", auth).call()?;
    Ok(resp.into_string()?)
}

fn is_unauthorized(e: &anyhow::Error) -> bool {
    e.downcast_ref::<ureq::Error>()
        .is_some_and(|u| matches!(u, ureq::Error::Status(401, _)))
}

/// Accept a top-level JSON array of {"id"} or an object with a `data` array.
/// Anything else is "unavailable", reported as an Err by the caller.
pub fn extract_model_ids(body: &str) -> Option<Vec<String>> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let arr = v
        .as_array()
        .or_else(|| v.get("data").and_then(|d| d.as_array()))?;
    let mut ids = Vec::new();
    for item in arr {
        if let Some(id) = item.get("id").and_then(|i| i.as_str()) {
            if !id.is_empty() {
                ids.push(id.to_string());
            }
        }
    }
    Some(ids)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_ids_from_array_or_data() {
        assert_eq!(
            extract_model_ids(r#"[{"id":"glm-4.6"}]"#).unwrap(),
            vec!["glm-4.6".to_string()]
        );
        assert_eq!(
            extract_model_ids(r#"{"data":[{"id":"glm-4.6"},{"id":"glm-5.3"}]}"#).unwrap(),
            vec!["glm-4.6".to_string(), "glm-5.3".to_string()]
        );
        assert!(extract_model_ids("{\"foo\":1}").is_none());
        assert!(extract_model_ids("nope").is_none());
    }

    #[test]
    fn version_parse_like_claude_output() {
        assert_eq!(parse_version("2.1.258 (Claude Code)"), Some((2, 1, 258)));
        assert_eq!(parse_version("2.0.14 (Claude Code)"), Some((2, 0, 14)));
        assert_eq!(parse_version("1.9.1"), Some((1, 9, 1)));
        assert_eq!(parse_version("garbage"), None);
    }

    #[test]
    fn version_requirement_is_at_least_2_0_14() {
        assert!(version_satisfies((2, 1, 258)));
        assert!(version_satisfies((2, 0, 14)));
        assert!(version_satisfies((3, 0, 0)));
        assert!(!version_satisfies((2, 0, 13)));
        assert!(!version_satisfies((1, 99, 0)));
    }

    #[test]
    fn format_check_lines() {
        let p = Check::pass("claude", "found");
        assert!(format_check(&p).starts_with("PASS"));
        let f = Check::fail("claude", "gone", "npm i -g");
        assert!(format_check(&f).starts_with("FAIL"));
        assert!(format_check(&f).contains("npm i -g"));
    }

    #[test]
    fn any_fail_detection() {
        assert!(!any_fail(&[
            Check::pass("a", "ok"),
            Check::warn("b", "w", "f")
        ]));
        assert!(any_fail(&[
            Check::pass("a", "ok"),
            Check::fail("b", "f", "fix")
        ]));
    }
}
