use std::env;
use std::path::Path;

use anyhow::{bail, Context, Result};

use crate::config::ResolvedConfig;

/// Split argv into (subcommand-word, claude-args) for the case where claude
/// itself runs. `--` forwards everything after it to claude.
#[cfg(test)]
pub fn split_passthrough(args: &[String]) -> (Option<String>, Vec<String>) {
    let mut claude: Vec<String> = Vec::new();
    let mut rest: Vec<String> = Vec::new();
    for (i, a) in args.iter().enumerate() {
        if a == "--" {
            claude.extend_from_slice(&args[i + 1..]);
            rest.extend_from_slice(&args[..i]);
            return (None, claude);
        }
        rest.push(a.clone());
    }
    if rest.is_empty() {
        return (None, claude);
    }
    if crate::main_sub_words().contains(&rest[0].as_str()) {
        (Some(rest[0].clone()), rest[1..].to_vec())
    } else {
        (None, rest)
    }
}

/// Where `--settings` was given by the user (value = path, or None when the
/// flag has no attached value yet).
#[cfg(test)]
pub fn find_settings_arg(args: &[String]) -> Option<Option<String>> {
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if let Some(v) = a.strip_prefix("--settings=") {
            return Some(Some(v.to_string()));
        }
        if a == "--settings" {
            return Some(args.get(i + 1).cloned());
        }
        if a == "--" {
            return None;
        }
        i += 1;
    }
    None
}

#[derive(Debug, Clone)]
pub struct EnvPlan {
    /// variables to set
    pub set: Vec<(String, String)>,
    /// variables to remove from the child env
    pub unset: Vec<String>,
}

/// Compute the environment mutations for running claude with the resolved
/// configuration. Pure: no process state, easy to test.
pub fn env_plan(cfg: &ResolvedConfig, key: &str) -> EnvPlan {
    let mut set = vec![
        ("ANTHROPIC_AUTH_TOKEN".to_string(), key.to_string()),
        ("ANTHROPIC_BASE_URL".to_string(), cfg.base_url.clone()),
        ("ANTHROPIC_MODEL".to_string(), cfg.model.clone()),
        (
            "ANTHROPIC_DEFAULT_OPUS_MODEL".to_string(),
            cfg.model.clone(),
        ),
        (
            "ANTHROPIC_DEFAULT_SONNET_MODEL".to_string(),
            cfg.model.clone(),
        ),
        (
            "ANTHROPIC_SMALL_FAST_MODEL".to_string(),
            cfg.small_model.clone(),
        ),
        (
            "ANTHROPIC_DEFAULT_HAIKU_MODEL".to_string(),
            cfg.small_model.clone(),
        ),
        ("API_TIMEOUT_MS".to_string(), cfg.api_timeout_ms.to_string()),
    ];
    let spec = crate::catalog::parse_model_spec(&cfg.model);
    if let Some(entry) = crate::catalog::find_model(&crate::catalog::catalog(), &spec.base_id) {
        if entry.max_output_tokens > 0 {
            set.push((
                "CLAUDE_CODE_MAX_OUTPUT_TOKENS".to_string(),
                entry.max_output_tokens.to_string(),
            ));
        }
    }
    if spec.million {
        set.push((
            "CLAUDE_CODE_AUTO_COMPACT_WINDOW".to_string(),
            "1000000".to_string(),
        ));
    }
    EnvPlan {
        set,
        unset: vec!["ANTHROPIC_API_KEY".to_string()],
    }
}

/// The statusline-only settings.json pointing back at this binary.
pub fn settings_body(glm_exe: &Path) -> serde_json::Value {
    serde_json::json!({
        "statusLine": {
            "type": "command",
            "command": format!("{} statusline", glm_exe.display()),
        }
    })
}

/// Write settings.json atomically.
pub fn write_settings(glm_exe: &Path, settings_path: &Path) -> Result<()> {
    let body = settings_body(glm_exe);
    crate::util::atomic_write(
        settings_path,
        &serde_json::to_string_pretty(&body).context("serialize settings.json")?,
    )
}

/// Inject env mutations into the current process env (used before exec).
pub fn apply_env_plan(plan: &EnvPlan) {
    for (k, v) in &plan.set {
        env::set_var(k, v);
    }
    for k in &plan.unset {
        env::remove_var(k);
    }
}

/// exec claude. Returns Err containing the hint when claude cannot be found.
pub fn exec_claude(args: &[String]) -> Result<()> {
    let err = execvp("claude", args).unwrap_err();
    if err.kind() == std::io::ErrorKind::NotFound {
        bail!(
            "claude is not installed. Install Claude Code first:\n  npm install -g @anthropic-ai/claude-code"
        );
    }
    Err(anyhow::anyhow!("failed to exec claude: {err}"))
}

#[cfg(unix)]
fn execvp(program: &str, args: &[String]) -> std::io::Result<()> {
    use std::os::unix::process::CommandExt;
    let mut cmd = std::process::Command::new(program);
    cmd.args(args);
    let _ = cmd.exec(); // exec only returns on failure
    Err(std::io::Error::last_os_error())
}

#[cfg(not(unix))]
fn execvp(_program: &str, _args: &[String]) -> std::io::Result<()> {
    unimplemented!("non-unix exec not supported")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with(model: &str, small: &str) -> ResolvedConfig {
        ResolvedConfig {
            profile: crate::catalog::find_profile("zai").unwrap(),
            profile_source: crate::config::Source::Default,
            model: model.to_string(),
            small_model: small.to_string(),
            base_url: "https://api.z.ai/api/anthropic".into(),
            usage_quota_url: "https://api.z.ai/api/monitor/usage/quota/limit".into(),
            api_timeout_ms: 3_000_000,
            cache_ttl_secs: 90,
            statusline_timeout_secs: 5,
        }
    }

    #[test]
    fn settings_detection_variants() {
        assert_eq!(find_settings_arg(&[]), None);
        assert_eq!(
            find_settings_arg(&["--settings".into(), "/tmp/s.json".into()]),
            Some(Some("/tmp/s.json".into()))
        );
        assert_eq!(
            find_settings_arg(&["--settings=/tmp/s.json".into()]),
            Some(Some("/tmp/s.json".into()))
        );
        assert_eq!(
            find_settings_arg(&["--dangerously-skip-permissions".into()]),
            None
        );
        assert_eq!(
            find_settings_arg(&["--".into(), "--settings".into(), "x".into()]),
            None
        );
        assert_eq!(
            find_settings_arg(&["-p".into(), "--settings".into()]),
            Some(None)
        );
    }

    #[test]
    fn subcommand_reservation_and_dashdash() {
        let (sub, args) = split_passthrough(&["init".into()]);
        assert_eq!(sub.as_deref(), Some("init"));
        assert!(args.is_empty());

        let (sub, args) = split_passthrough(&["--".into(), "init".into()]);
        assert_eq!(sub, None);
        assert_eq!(args, vec!["init".to_string()]);

        let (sub, args) = split_passthrough(&["-p".into(), "how do I work?".into()]);
        assert_eq!(sub, None);
        assert_eq!(args, vec!["-p".to_string(), "how do I work?".to_string()]);
    }

    #[test]
    fn million_suffix_sets_compact_window_and_strips_model() {
        let cfg = cfg_with("glm-5.3[1m]", "glm-5.3-flash");
        let plan = env_plan(&cfg, "sk-test");
        let get = |k: &str| {
            plan.set
                .iter()
                .find(|(n, _)| n == k)
                .map(|(_, v)| v.clone())
        };
        assert_eq!(get("ANTHROPIC_AUTH_TOKEN").as_deref(), Some("sk-test"));
        assert_eq!(get("ANTHROPIC_MODEL").as_deref(), Some("glm-5.3[1m]"));
        assert_eq!(
            get("CLAUDE_CODE_AUTO_COMPACT_WINDOW").as_deref(),
            Some("1000000")
        );
        assert_eq!(
            get("CLAUDE_CODE_MAX_OUTPUT_TOKENS").as_deref(),
            Some("128000")
        );
        assert!(plan.unset.contains(&"ANTHROPIC_API_KEY".to_string()));
    }

    #[test]
    fn no_suffix_no_compact_window() {
        let cfg = cfg_with("glm-4.6", "glm-4.6-flash");
        let plan = env_plan(&cfg, "sk-test");
        assert!(!plan
            .set
            .iter()
            .any(|(n, _)| n == "CLAUDE_CODE_AUTO_COMPACT_WINDOW"));
        assert_eq!(
            plan.set
                .iter()
                .find(|(n, _)| n == "CLAUDE_CODE_MAX_OUTPUT_TOKENS")
                .map(|(_, v)| v.as_str()),
            Some("131072")
        );
        assert_eq!(
            plan.set
                .iter()
                .find(|(n, _)| n == "ANTHROPIC_DEFAULT_SONNET_MODEL")
                .map(|(_, v)| v.as_str()),
            Some("glm-4.6")
        );
        assert_eq!(
            plan.set
                .iter()
                .find(|(n, _)| n == "ANTHROPIC_DEFAULT_HAIKU_MODEL")
                .map(|(_, v)| v.as_str()),
            Some("glm-4.6-flash")
        );
    }

    #[test]
    fn unknown_model_gets_env_but_no_limits() {
        let cfg = cfg_with("glm-9.9", "glm-5.3-flash");
        let plan = env_plan(&cfg, "sk-test");
        assert_eq!(
            plan.set
                .iter()
                .find(|(n, _)| n == "ANTHROPIC_MODEL")
                .map(|(_, v)| v.as_str()),
            Some("glm-9.9")
        );
        assert!(!plan
            .set
            .iter()
            .any(|(n, _)| n == "CLAUDE_CODE_MAX_OUTPUT_TOKENS"));
    }

    #[test]
    fn settings_body_contains_absolute_glm() {
        let body = settings_body(Path::new("/usr/local/bin/glm"));
        let command = body["statusLine"]["command"].as_str().unwrap();
        assert_eq!(command, "/usr/local/bin/glm statusline");
    }

    #[test]
    fn reserved_words_cover_all_subcommands() {
        for w in [
            "init",
            "config",
            "key",
            "models",
            "usage",
            "statusline",
            "doctor",
            "self-update",
        ] {
            assert!(crate::main_sub_words().contains(&w), "{w}");
        }
        assert!(!crate::main_sub_words().contains(&"-p"));
        assert!(!crate::main_sub_words().contains(&"resume"));
        assert!(!crate::main_sub_words().contains(&"--continue"));
    }
}
