use std::collections::BTreeMap;
use std::env;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::catalog::{resolve_profile, Profile};

/// Default values, also used when writing a fresh config.toml.
pub const DEFAULT_MODEL: &str = "glm-5.3[1m]";
/// Non-thinking tier for Claude Code's side calls (titles, summaries,
/// classifiers): the glm-5.3 generation has thinking always on, so every
/// side call through it would pay thinking tokens (docs.z.ai; AGENTS.md).
pub const DEFAULT_SMALL_MODEL: &str = "glm-4.7-flash";
pub const DEFAULT_API_TIMEOUT_MS: u64 = 3_000_000;
pub const DEFAULT_CACHE_TTL_SECS: u64 = 90;
pub const DEFAULT_STATUSLINE_TIMEOUT_SECS: u64 = 5;
pub const USAGE_TIMEOUT_SECS: u64 = 15;

/// Reasoning-effort levels Claude Code accepts (forwarded as
/// CLAUDE_CODE_EFFORT_LEVEL). Unset means "let Claude Code decide", which
/// keeps the in-picker ←/→ adjustment working.
pub const EFFORT_VALUES: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

/// Normalize and validate an effort value from config or env.
pub fn normalize_effort(value: &str) -> Result<String> {
    let v = value.trim().to_lowercase();
    if EFFORT_VALUES.contains(&v.as_str()) {
        Ok(v)
    } else {
        anyhow::bail!(
            "invalid effort {value:?}; valid values: {}",
            EFFORT_VALUES.join(", ")
        )
    }
}

/// Raw file contents. `deny_unknown_fields` is not used so that `set`/`unset`
/// can detect every unknown key and so that the file round-trips; unknown keys
/// are validated explicitly.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ConfigFile {
    pub profile: Option<String>,
    pub model: Option<String>,
    pub small_model: Option<String>,
    pub base_url: Option<String>,
    pub usage_quota_url: Option<String>,
    pub api_timeout_ms: Option<u64>,
    pub effort: Option<String>,
    pub thinking_budget: Option<u64>,
    pub statusline: Option<StatuslineFile>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StatuslineFile {
    pub cache_ttl_secs: Option<u64>,
    pub timeout_secs: Option<u64>,
}

pub const TOP_KEYS: [&str; 8] = [
    "profile",
    "model",
    "small_model",
    "base_url",
    "usage_quota_url",
    "api_timeout_ms",
    "effort",
    "thinking_budget",
];
pub const STATUSLINE_KEYS: [&str; 2] = ["cache_ttl_secs", "timeout_secs"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Env,
    File,
    Default,
}

impl Source {
    pub fn describe(self) -> &'static str {
        match self {
            Source::Env => "environment",
            Source::File => "config.toml",
            Source::Default => "built-in default",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ResolvedConfig {
    pub profile: Profile,
    pub profile_source: Source,
    pub model: String,
    pub small_model: String,
    pub base_url: String,
    pub usage_quota_url: String,
    pub api_timeout_ms: u64,
    /// Reasoning effort to pin for the session, or None to leave Claude
    /// Code's own default (and in-picker adjustment) in charge.
    pub effort: Option<String>,
    /// Thinking-token budget (ZCode parity: 32000), forwarded as
    /// MAX_THINKING_TOKENS. None keeps Claude Code's default; 0 disables
    /// thinking entirely (only valid on models that can switch it off).
    pub thinking_budget: Option<u64>,
    pub cache_ttl_secs: u64,
    pub statusline_timeout_secs: u64,
}

pub fn usage_quota_default(profile: Profile) -> String {
    format!("{}/api/monitor/usage/quota/limit", profile.quota_host)
}

/// Load config.toml. A missing file yields defaults; an unparsable file is an
/// error (the fix is `glm init` or editing the file).
pub fn load_file() -> Result<ConfigFile> {
    let path = crate::paths::config_file();
    if !path.exists() {
        return Ok(ConfigFile::default());
    }
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read config {}", path.display()))?;
    let parsed: ConfigFile = toml::from_str(&raw)
        .with_context(|| format!("failed to parse config {}", path.display()))?;
    validate_keys(&parsed)?;
    Ok(parsed)
}

/// Report unknown top-level and `statusline` keys with the valid names.
pub fn validate_keys(cfg: &ConfigFile) -> Result<()> {
    let _ = cfg;
    // Re-parse into a table to see raw keys (serde drops nothing, but the
    // table view is the authoritative list of what the file contained).
    let path = crate::paths::config_file();
    if path.exists() {
        let raw = std::fs::read_to_string(&path)?;
        let table: toml::Table = toml::from_str(&raw)?;
        // `statusline` is a section, not a top-level scalar key.
        let mut top_valid = TOP_KEYS.to_vec();
        top_valid.push("statusline");
        if let Some(msg) = crate::catalog::unknown_keys_error(&table, "", &top_valid) {
            anyhow::bail!("{msg}");
        }
        if let Some(st) = table.get("statusline").and_then(|v| v.as_table()) {
            if let Some(msg) =
                crate::catalog::unknown_keys_error(st, "statusline", &STATUSLINE_KEYS)
            {
                anyhow::bail!("{msg}");
            }
        }
        if let Some(v) = table.get("statusline") {
            if !v.is_table() {
                anyhow::bail!("config key \"statusline\" must be a [statusline] table");
            }
        }
    }
    Ok(())
}

/// env value helpers: `GLM_<NAME>` wins over bare `<NAME>`.
fn env_any(names: &[&str]) -> Option<String> {
    for n in names {
        if let Some(v) = env::var(n).ok().filter(|v| !v.trim().is_empty()) {
            return Some(v);
        }
    }
    None
}

fn env_u64(names: &[&str]) -> Option<u64> {
    env_any(names).and_then(|v| v.trim().parse().ok())
}

/// Merge env (`GLM_<NAME>` then bare `<NAME>`), file, and profile defaults.
/// `profile` honors only `GLM_PROFILE`; a bare `PROFILE` is a common shell
/// variable and must never be treated as a glm setting.
pub fn resolve(
    file: Option<&ConfigFile>,
    overrides: &BTreeMap<String, String>,
) -> Result<ResolvedConfig> {
    // profile
    let profile_id = overrides
        .get("profile")
        .cloned()
        .or_else(|| env_any(&["GLM_PROFILE"]))
        .or_else(|| file.and_then(|f| f.profile.clone()))
        .unwrap_or_else(|| crate::catalog::DEFAULT_PROFILE.to_string());
    let profile = resolve_profile(&profile_id)?;
    let profile_source = if overrides.contains_key("profile") || env::var("GLM_PROFILE").is_ok() {
        Source::Env
    } else if file.and_then(|f| f.profile.as_ref()).is_some() {
        Source::File
    } else {
        Source::Default
    };

    let get_str = |name: &str, bare: Option<&str>| -> Option<String> {
        let mut names = vec![format!("GLM_{}", name.to_uppercase())];
        if let Some(b) = bare {
            names.push(b.to_string());
        }
        let names: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
        env_any(&names)
    };

    let model = overrides
        .get("model")
        .cloned()
        .or_else(|| get_str("model", None))
        .or_else(|| file.and_then(|f| f.model.clone()))
        .unwrap_or_else(|| DEFAULT_MODEL.to_string());

    let small_model = overrides
        .get("small_model")
        .cloned()
        .or_else(|| get_str("small_model", None))
        .or_else(|| file.and_then(|f| f.small_model.clone()))
        .unwrap_or_else(|| DEFAULT_SMALL_MODEL.to_string());

    let base_url = overrides
        .get("base_url")
        .cloned()
        .or_else(|| get_str("base_url", None))
        .or_else(|| file.and_then(|f| f.base_url.clone()))
        .unwrap_or_else(|| profile.base_url.to_string());

    let usage_quota_url = overrides
        .get("usage_quota_url")
        .cloned()
        .or_else(|| get_str("usage_quota_url", Some("BIGMODEL_USAGE_QUOTA_URL")))
        .or_else(|| file.and_then(|f| f.usage_quota_url.clone()))
        .unwrap_or_else(|| usage_quota_default(profile));

    let api_timeout_ms = overrides
        .get("api_timeout_ms")
        .and_then(|v| v.parse().ok())
        .or_else(|| env_u64(&["GLM_API_TIMEOUT_MS"]))
        .or_else(|| file.and_then(|f| f.api_timeout_ms))
        .unwrap_or(DEFAULT_API_TIMEOUT_MS);

    let effort = match overrides
        .get("effort")
        .cloned()
        .or_else(|| get_str("effort", None))
        .or_else(|| file.and_then(|f| f.effort.clone()))
    {
        Some(raw) => Some(normalize_effort(&raw)?),
        None => None,
    };

    let thinking_budget = overrides
        .get("thinking_budget")
        .and_then(|v| v.parse().ok())
        .or_else(|| env_u64(&["GLM_THINKING_BUDGET"]))
        .or_else(|| file.and_then(|f| f.thinking_budget));

    let cache_ttl_secs = overrides
        .get("cache_ttl_secs")
        .and_then(|v| v.parse().ok())
        .or_else(|| env_u64(&["GLM_CACHE_TTL_SECS"]))
        .or_else(|| {
            file.and_then(|f| f.statusline.as_ref())
                .and_then(|s| s.cache_ttl_secs)
        })
        .unwrap_or(DEFAULT_CACHE_TTL_SECS);

    let statusline_timeout_secs = overrides
        .get("statusline_timeout_secs")
        .and_then(|v| v.parse().ok())
        .or_else(|| env_u64(&["GLM_STATUSLINE_TIMEOUT_SECS"]))
        .or_else(|| {
            file.and_then(|f| f.statusline.as_ref())
                .and_then(|s| s.timeout_secs)
        })
        .unwrap_or(DEFAULT_STATUSLINE_TIMEOUT_SECS);

    Ok(ResolvedConfig {
        profile,
        profile_source,
        model,
        small_model,
        base_url,
        usage_quota_url,
        api_timeout_ms,
        effort,
        thinking_budget,
        cache_ttl_secs,
        statusline_timeout_secs,
    })
}

/// The default config.toml body written by `glm init`.
pub fn default_config_toml() -> String {
    format!(
        "# glm configuration - https://github.com/r0bops/glm-wrapper\n\
         profile      = {:?}\n\
         model        = {:?}\n\
         small_model  = {:?}\n\
         # effort = \"max\"  # low|medium|high|xhigh|max; unset = Claude Code default\n\
         # thinking_budget = 32000  # tokens; forwarded as MAX_THINKING_TOKENS\n\
         \n\
         [statusline]\n\
         cache_ttl_secs = {}\n\
         timeout_secs   = {}\n",
        crate::catalog::DEFAULT_PROFILE,
        DEFAULT_MODEL,
        DEFAULT_SMALL_MODEL,
        DEFAULT_CACHE_TTL_SECS,
        DEFAULT_STATUSLINE_TIMEOUT_SECS,
    )
}

/// Write config.toml with defaults if absent. Returns whether it was created.
pub fn ensure_config_file() -> Result<bool> {
    let path = crate::paths::config_file();
    if path.exists() {
        return Ok(false);
    }
    write_config_toml(&path, &default_config_toml())?;
    Ok(true)
}

/// Serialize a ConfigFile back to config.toml (used by `config set`).
pub fn write_config(cfg: &ConfigFile) -> Result<()> {
    write_config_toml(&crate::paths::config_file(), &cfg_to_toml(cfg)?)
}

pub fn write_config_toml(path: &std::path::Path, body: &str) -> Result<()> {
    let dir = crate::paths::config_dir();
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create config dir {}", dir.display()))?;
    crate::util::atomic_write(path, body)?;
    Ok(())
}

/// Lossless round-trip through toml::Value so unknown keys the struct doesn't
/// model survive an edit.
pub fn cfg_to_toml(cfg: &ConfigFile) -> Result<String> {
    let path = crate::paths::config_file();
    let mut value: toml::Value = toml::Value::Table(toml::Table::new());
    if path.exists() {
        let raw = std::fs::read_to_string(&path)?;
        value = toml::from_str(&raw)?;
    }
    let table = value.as_table_mut().expect("top level must be a table");
    if let Some(v) = &cfg.profile {
        table.insert("profile".into(), toml::Value::String(v.clone()));
    } else {
        table.remove("profile");
    }
    if let Some(v) = &cfg.model {
        table.insert("model".into(), toml::Value::String(v.clone()));
    } else {
        table.remove("model");
    }
    if let Some(v) = &cfg.small_model {
        table.insert("small_model".into(), toml::Value::String(v.clone()));
    } else {
        table.remove("small_model");
    }
    if let Some(v) = &cfg.base_url {
        table.insert("base_url".into(), toml::Value::String(v.clone()));
    } else {
        table.remove("base_url");
    }
    if let Some(v) = &cfg.usage_quota_url {
        table.insert("usage_quota_url".into(), toml::Value::String(v.clone()));
    } else {
        table.remove("usage_quota_url");
    }
    if let Some(v) = &cfg.api_timeout_ms {
        table.insert("api_timeout_ms".into(), toml::Value::Integer(*v as i64));
    } else {
        table.remove("api_timeout_ms");
    }
    if let Some(v) = &cfg.effort {
        table.insert("effort".into(), toml::Value::String(v.clone()));
    } else {
        table.remove("effort");
    }
    if let Some(v) = cfg.thinking_budget {
        table.insert("thinking_budget".into(), toml::Value::Integer(v as i64));
    } else {
        table.remove("thinking_budget");
    }
    match &cfg.statusline {
        Some(st) => {
            let mut st_t = toml::Table::new();
            if let Some(v) = st.cache_ttl_secs {
                st_t.insert("cache_ttl_secs".into(), toml::Value::Integer(v as i64));
            }
            if let Some(v) = st.timeout_secs {
                st_t.insert("timeout_secs".into(), toml::Value::Integer(v as i64));
            }
            table.insert("statusline".into(), toml::Value::Table(st_t));
        }
        None => {
            table.remove("statusline");
        }
    }
    Ok(toml::to_string_pretty(&value)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Guard so parallel tests don't fight over process env.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn file(model: Option<&str>, profile: Option<&str>) -> ConfigFile {
        ConfigFile {
            profile: profile.map(String::from),
            model: model.map(String::from),
            small_model: None,
            base_url: None,
            usage_quota_url: None,
            api_timeout_ms: Some(111),
            effort: None,
            thinking_budget: None,
            statusline: Some(StatuslineFile {
                cache_ttl_secs: Some(22),
                timeout_secs: Some(33),
            }),
        }
    }

    fn no_env<F: FnOnce() -> R, R>(f: F) -> R {
        let _g = ENV_LOCK.lock().unwrap();
        for k in [
            "GLM_PROFILE",
            "GLM_MODEL",
            "GLM_SMALL_MODEL",
            "GLM_BASE_URL",
            "GLM_USAGE_QUOTA_URL",
            "GLM_API_TIMEOUT_MS",
            "GLM_EFFORT",
            "GLM_THINKING_BUDGET",
            "GLM_CACHE_TTL_SECS",
            "GLM_STATUSLINE_TIMEOUT_SECS",
            "BIGMODEL_USAGE_QUOTA_URL",
        ] {
            env::remove_var(k);
        }
        f()
    }

    #[test]
    fn file_beats_defaults() {
        no_env(|| {
            let cfg = resolve(
                Some(&file(Some("glm-4.6"), Some("bigmodel"))),
                &BTreeMap::new(),
            )
            .unwrap();
            assert_eq!(cfg.model, "glm-4.6");
            assert_eq!(cfg.profile.id, "bigmodel");
            assert_eq!(cfg.base_url, "https://open.bigmodel.cn/api/anthropic");
            assert_eq!(cfg.api_timeout_ms, 111);
            assert_eq!(cfg.cache_ttl_secs, 22);
            assert_eq!(cfg.statusline_timeout_secs, 33);
        });
    }

    #[test]
    fn env_beats_file() {
        no_env(|| {
            env::set_var("GLM_MODEL", "glm-5.1");
            env::set_var("GLM_CACHE_TTL_SECS", "77");
            let cfg = resolve(Some(&file(Some("glm-4.6"), None)), &BTreeMap::new()).unwrap();
            assert_eq!(cfg.model, "glm-5.1");
            assert_eq!(cfg.cache_ttl_secs, 77);
        });
    }

    #[test]
    fn overrides_beat_env() {
        no_env(|| {
            env::set_var("GLM_MODEL", "glm-5.1");
            let mut o = BTreeMap::new();
            o.insert("model".to_string(), "glm-5".to_string());
            let cfg = resolve(Some(&file(None, None)), &o).unwrap();
            assert_eq!(cfg.model, "glm-5");
        });
    }

    #[test]
    fn bare_env_used_when_glm_prefix_missing() {
        no_env(|| {
            env::set_var("BIGMODEL_USAGE_QUOTA_URL", "https://example.test/q");
            let cfg = resolve(None, &BTreeMap::new()).unwrap();
            assert_eq!(cfg.usage_quota_url, "https://example.test/q");
            // GLM_ wins over bare
            env::set_var("GLM_USAGE_QUOTA_URL", "https://glm.test/q");
            let cfg = resolve(None, &BTreeMap::new()).unwrap();
            assert_eq!(cfg.usage_quota_url, "https://glm.test/q");
        });
    }

    #[test]
    fn defaults_apply_with_no_file_or_env() {
        no_env(|| {
            let cfg = resolve(None, &BTreeMap::new()).unwrap();
            assert_eq!(cfg.profile.id, crate::catalog::DEFAULT_PROFILE);
            assert_eq!(cfg.model, DEFAULT_MODEL);
            assert_eq!(cfg.small_model, DEFAULT_SMALL_MODEL);
            assert_eq!(cfg.base_url, "https://api.z.ai/api/anthropic");
            assert_eq!(cfg.api_timeout_ms, DEFAULT_API_TIMEOUT_MS);
            assert_eq!(cfg.statusline_timeout_secs, DEFAULT_STATUSLINE_TIMEOUT_SECS);
            assert_eq!(cfg.profile_source, Source::Default);
        });
    }

    #[test]
    fn profile_env_beats_file() {
        no_env(|| {
            env::set_var("GLM_PROFILE", "zai");
            let cfg = resolve(Some(&file(None, Some("bigmodel"))), &BTreeMap::new()).unwrap();
            assert_eq!(cfg.profile.id, "zai");
            assert_eq!(cfg.profile_source, Source::Env);
        });
    }

    #[test]
    fn default_config_toml_round_trips() {
        let body = default_config_toml();
        let parsed: ConfigFile = toml::from_str(&body).unwrap();
        assert_eq!(
            parsed.profile.as_deref(),
            Some(crate::catalog::DEFAULT_PROFILE)
        );
        assert_eq!(
            parsed.statusline.as_ref().unwrap().cache_ttl_secs,
            Some(DEFAULT_CACHE_TTL_SECS)
        );
        // The effort line is a comment: documented but unset by default.
        assert_eq!(parsed.effort, None);
    }

    #[test]
    fn effort_resolves_from_file_env_and_overrides() {
        no_env(|| {
            // unset everywhere -> None
            let cfg = resolve(None, &BTreeMap::new()).unwrap();
            assert_eq!(cfg.effort, None);
            // file value
            let mut f = file(None, None);
            f.effort = Some("high".into());
            let cfg = resolve(Some(&f), &BTreeMap::new()).unwrap();
            assert_eq!(cfg.effort.as_deref(), Some("high"));
            // env beats file, and is normalized
            env::set_var("GLM_EFFORT", "  MAX ");
            let cfg = resolve(Some(&f), &BTreeMap::new()).unwrap();
            assert_eq!(cfg.effort.as_deref(), Some("max"));
            // overrides beat env
            let mut o = BTreeMap::new();
            o.insert("effort".to_string(), "low".to_string());
            let cfg = resolve(Some(&f), &o).unwrap();
            assert_eq!(cfg.effort.as_deref(), Some("low"));
        });
    }

    #[test]
    fn thinking_budget_resolves_from_file_env_and_overrides() {
        no_env(|| {
            let cfg = resolve(None, &BTreeMap::new()).unwrap();
            assert_eq!(cfg.thinking_budget, None);
            let mut f = file(None, None);
            f.thinking_budget = Some(32000);
            let cfg = resolve(Some(&f), &BTreeMap::new()).unwrap();
            assert_eq!(cfg.thinking_budget, Some(32000));
            env::set_var("GLM_THINKING_BUDGET", "0");
            let cfg = resolve(Some(&f), &BTreeMap::new()).unwrap();
            assert_eq!(cfg.thinking_budget, Some(0));
            let mut o = BTreeMap::new();
            o.insert("thinking_budget".to_string(), "16384".to_string());
            let cfg = resolve(Some(&f), &o).unwrap();
            assert_eq!(cfg.thinking_budget, Some(16384));
        });
    }

    #[test]
    fn thinking_budget_survives_cfg_to_toml() {
        // Seeded from the on-disk config; effort_survives_cfg_to_toml already
        // covers XDG hermeticity, so only round-trip through the struct here.
        let cfg = ConfigFile {
            thinking_budget: Some(4096),
            ..ConfigFile::default()
        };
        let body = cfg_to_toml(&cfg).unwrap();
        assert!(body.contains("thinking_budget"), "{body}");
        let parsed: ConfigFile = toml::from_str(&body).unwrap();
        assert_eq!(parsed.thinking_budget, Some(4096));
    }

    #[test]
    fn effort_rejects_unknown_values() {
        no_env(|| {
            assert_eq!(normalize_effort("xhigh").unwrap(), "xhigh");
            assert_eq!(normalize_effort("Low").unwrap(), "low");
            let err = normalize_effort("ultra").unwrap_err().to_string();
            assert!(err.contains("low") && err.contains("max"), "{err}");
            let mut f = file(None, None);
            f.effort = Some("bogus".into());
            let err = resolve(Some(&f), &BTreeMap::new()).unwrap_err().to_string();
            assert!(err.contains("bogus"), "{err}");
        });
    }

    #[test]
    fn effort_survives_cfg_to_toml() {
        // cfg_to_toml seeds from the on-disk config; point XDG at an empty
        // temp dir so the test never touches the developer's real file.
        let _g = ENV_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("glm-cfg-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // SAFETY: single-threaded vs other config tests via ENV_LOCK.
        let old = env::var_os("XDG_CONFIG_HOME");
        env::set_var("XDG_CONFIG_HOME", &dir);
        let result = std::panic::catch_unwind(|| {
            let cfg = ConfigFile {
                effort: Some("xhigh".into()),
                ..ConfigFile::default()
            };
            let body = cfg_to_toml(&cfg).unwrap();
            let parsed: ConfigFile = toml::from_str(&body).unwrap();
            assert_eq!(parsed.effort.as_deref(), Some("xhigh"));
            // a None effort must not leave a stale key behind
            let body = cfg_to_toml(&ConfigFile::default()).unwrap();
            assert!(!body.contains("effort"), "{body}");
        });
        match old {
            Some(v) => env::set_var("XDG_CONFIG_HOME", v),
            None => env::remove_var("XDG_CONFIG_HOME"),
        }
        std::fs::remove_dir_all(&dir).ok();
        assert!(result.is_ok());
    }
}
