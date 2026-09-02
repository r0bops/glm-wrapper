use std::collections::BTreeMap;
use std::env;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::catalog::{resolve_profile, Profile};

/// Default values, also used when writing a fresh config.toml.
pub const DEFAULT_MODEL: &str = "glm-5.3[1m]";
pub const DEFAULT_SMALL_MODEL: &str = "glm-5.3-flash";
pub const DEFAULT_API_TIMEOUT_MS: u64 = 3_000_000;
pub const DEFAULT_CACHE_TTL_SECS: u64 = 90;
pub const DEFAULT_STATUSLINE_TIMEOUT_SECS: u64 = 5;
pub const USAGE_TIMEOUT_SECS: u64 = 15;

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
    pub statusline: Option<StatuslineFile>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StatuslineFile {
    pub cache_ttl_secs: Option<u64>,
    pub timeout_secs: Option<u64>,
}

pub const TOP_KEYS: [&str; 6] = [
    "profile",
    "model",
    "small_model",
    "base_url",
    "usage_quota_url",
    "api_timeout_ms",
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
    }
}
