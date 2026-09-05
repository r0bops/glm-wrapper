use anyhow::Result;
use serde::{Deserialize, Serialize};

pub const MODELS_JSON: &str = include_str!("../models.json");

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogFile {
    #[serde(rename = "schemaVersion")]
    pub schema_version: String,
    pub providers: Vec<Provider>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Provider {
    pub id: String,
    pub name: String,
    #[serde(rename = "defaultKind")]
    pub default_kind: String,
    pub endpoints: Endpoints,
    pub models: Vec<ModelEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Endpoints {
    #[serde(rename = "baseURL")]
    pub base_url: String,
    pub paths: Paths,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Paths {
    pub anthropic: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelEntry {
    pub id: String,
    #[serde(default)]
    pub kinds: Vec<String>,
    #[serde(default, rename = "contextWindow")]
    pub context_window: u64,
    #[serde(default, rename = "maxOutputTokens")]
    pub max_output_tokens: u64,
    #[serde(default)]
    pub modalities: Vec<String>,
    #[serde(default)]
    pub reasoning: bool,
    /// Present only for entries added by the weekly catalog sync.
    #[serde(default)]
    pub source: Option<String>,
}

pub const DEFAULT_PROFILE: &str = "zai-coding-plan";

/// A configured provider profile: base URL for the Anthropic-compatible API and
/// the host serving the usage/quota monitor endpoints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Profile {
    pub id: &'static str,
    pub base_url: &'static str,
    pub quota_host: &'static str,
}

pub const PROFILES: [Profile; 4] = [
    Profile {
        id: "zai",
        base_url: "https://api.z.ai/api/anthropic",
        quota_host: "https://api.z.ai",
    },
    Profile {
        id: "zai-coding-plan",
        base_url: "https://api.z.ai/api/anthropic",
        quota_host: "https://api.z.ai",
    },
    Profile {
        id: "bigmodel",
        base_url: "https://open.bigmodel.cn/api/anthropic",
        quota_host: "https://open.bigmodel.cn",
    },
    Profile {
        id: "bigmodel-coding-plan",
        base_url: "https://open.bigmodel.cn/api/anthropic",
        quota_host: "https://open.bigmodel.cn",
    },
];

pub fn profile_names() -> Vec<&'static str> {
    PROFILES.iter().map(|p| p.id).collect()
}

pub fn find_profile(id: &str) -> Option<Profile> {
    PROFILES.iter().copied().find(|p| p.id == id)
}

pub fn catalog() -> CatalogFile {
    serde_json::from_str(MODELS_JSON).expect("models.json is embedded and must parse")
}

/// Find a model entry by id across all providers.
pub fn find_model<'a>(catalog: &'a CatalogFile, id: &str) -> Option<&'a ModelEntry> {
    catalog
        .providers
        .iter()
        .flat_map(|p| p.models.iter())
        .find(|m| m.id == id)
}

/// A model id possibly carrying the `[1m]` suffix (requests a 1M context window).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelSpec {
    /// Id without the `[1m]` suffix, for catalog lookup and the API.
    pub base_id: String,
    /// True when the original id ended in `[1m]`.
    pub million: bool,
}

/// Split a model id like `glm-5.3[1m]` into its base id and the suffix flag.
pub fn parse_model_spec(id: &str) -> ModelSpec {
    let trimmed = id.trim();
    if let Some(base) = trimmed.strip_suffix("[1m]") {
        ModelSpec {
            base_id: base.trim().to_string(),
            million: true,
        }
    } else {
        ModelSpec {
            base_id: trimmed.to_string(),
            million: false,
        }
    }
}

/// The id actually sent to the API (suffix stripped).
pub fn api_model_id(id: &str) -> String {
    parse_model_spec(id).base_id
}

/// Render the configured id as-is: `[1m]` only when the user asked for it.
/// Claude Code only gets the 1M window (and auto-compact) when the suffix is
/// present, so the display must not claim a window the session isn't using.
pub fn display_model_id(id: &str) -> String {
    let spec = parse_model_spec(id);
    if spec.million {
        format!("{}[1m]", spec.base_id)
    } else {
        spec.base_id
    }
}

fn ctx_width(model: &ModelSpec, entry: Option<&ModelEntry>) -> Option<u64> {
    match entry {
        Some(m) if m.context_window > 0 => Some(m.context_window),
        Some(_) => None,
        None if model.million => Some(1_000_000),
        None => None,
    }
}

/// Returns the effective context percentage and window size for `used` tokens of
/// `model`, or None when the model (or its window) is unknown.
pub fn context_pct(model: &ModelSpec, used: Option<u64>) -> Option<(u64, u64)> {
    let used = used?;
    let window = ctx_width(model, find_model(&catalog(), &model.base_id))?;
    Some(((used * 100) / window, window))
}

pub fn is_known_model(id: &str) -> bool {
    find_model(&catalog(), &api_model_id(id)).is_some()
}

/// Models whose thinking is always on and cannot be disabled (GLM-5.3
/// generation, per docs.z.ai). Every side call through them pays thinking
/// tokens, which makes them a poor ANTHROPIC_SMALL_FAST_MODEL choice.
pub fn thinking_always_on(id: &str) -> bool {
    matches!(api_model_id(id).as_str(), "glm-5.3" | "glm-5.3-flash")
}

/// Documented reasoning-effort levels (docs.z.ai): GLM-5.2 and newer accept
/// `low|high|max` (default max); older GLM models have no effort parameter
/// at all. None means "send no effort value" — the server default applies.
pub fn effort_levels(id: &str) -> Option<&'static [&'static str]> {
    let base = api_model_id(id);
    let rest = base.strip_prefix("glm-")?;
    // Leading [digits.dot]* is the version: 5.3, 5.1-highspeed -> 5.1,
    // 5v-turbo -> 5, 4-flash-250414 -> 4.
    let ver: String = rest
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    let mut parts = ver.split('.');
    let major: u32 = parts.next()?.parse().ok()?;
    let minor: u32 = parts.next().unwrap_or("0").parse().ok()?;
    if (major, minor) >= (5, 2) {
        Some(&["low", "high", "max"])
    } else {
        None
    }
}

/// Resolve the profile, erroring when unknown and listing the valid ids.
pub fn resolve_profile(id: &str) -> Result<Profile> {
    find_profile(id).ok_or_else(|| {
        anyhow::anyhow!(
            "unknown profile {id:?}; valid profiles: {}",
            profile_names().join(", ")
        )
    })
}

/// Reject unknown keys inside a parsed config value (deny_unknown_fields in
/// serde reports only the first; this reports them all).
pub fn unknown_keys_error(table: &toml::Table, path: &str, valid: &[&str]) -> Option<String> {
    let mut unknown: Vec<&String> = table
        .keys()
        .filter(|k| !valid.contains(&k.as_str()))
        .collect();
    unknown.sort();
    if unknown.is_empty() {
        return None;
    }
    let where_ = if path.is_empty() {
        String::new()
    } else {
        format!(" under {path}")
    };
    Some(format!(
        "unknown config key{} {}{} (valid: {})",
        if unknown.len() == 1 { "" } else { "s" },
        unknown
            .iter()
            .map(|k| format!("{k:?}"))
            .collect::<Vec<_>>()
            .join(", "),
        where_,
        valid.join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_catalog() -> CatalogFile {
        CatalogFile {
            schema_version: "zcode.model-providers.v1".into(),
            providers: vec![Provider {
                id: "zai".into(),
                name: "Z.ai".into(),
                default_kind: "anthropic".into(),
                endpoints: Endpoints {
                    base_url: "https://api.z.ai".into(),
                    paths: Paths {
                        anthropic: "/api/anthropic/v1/messages".into(),
                    },
                },
                models: vec![
                    ModelEntry {
                        id: "glm-4.6".into(),
                        kinds: vec!["anthropic".into()],
                        context_window: 200_000,
                        max_output_tokens: 131_072,
                        modalities: vec!["text".into()],
                        reasoning: true,
                        source: None,
                    },
                    ModelEntry {
                        id: "glm-5.3".into(),
                        kinds: vec!["anthropic".into()],
                        context_window: 1_000_000,
                        max_output_tokens: 128_000,
                        modalities: vec!["text".into()],
                        reasoning: true,
                        source: None,
                    },
                ],
            }],
        }
    }

    #[test]
    fn parses_bundled_models_json() {
        let c = catalog();
        assert_eq!(c.schema_version, "zcode.model-providers.v1");
        assert_eq!(c.providers.len(), 4);
        assert_eq!(c.providers[0].id, "zai");
        assert_eq!(c.providers[1].id, "zai-coding-plan");
        assert_eq!(c.providers[2].id, "bigmodel");
        assert_eq!(c.providers[3].id, "bigmodel-coding-plan");
        for p in &c.providers {
            assert_eq!(p.models.len(), 20, "{}", p.id);
            assert_eq!(
                p.endpoints.base_url,
                if p.id.starts_with("bigmodel") {
                    "https://open.bigmodel.cn"
                } else {
                    "https://api.z.ai"
                }
            );
        }
    }

    #[test]
    fn bundled_entries_have_expected_shapes() {
        let c = catalog();
        let m = find_model(&c, "glm-4.6v-flash").unwrap();
        assert_eq!(m.context_window, 131_072);
        assert_eq!(m.max_output_tokens, 32_768);
        assert!(m.modalities.contains(&"image".to_string()));
        assert!(!m.reasoning);
        let m = find_model(&c, "glm-5.3-flash").unwrap();
        assert_eq!(m.context_window, 1_000_000);
        assert_eq!(m.max_output_tokens, 128_000);
        assert!(m.modalities.contains(&"image".to_string()));
        let m = find_model(&c, "glm-4.1v-thinking-flash").unwrap();
        assert_eq!(m.context_window, 65_536);
        let m = find_model(&c, "glm-4.5").unwrap();
        assert_eq!(m.max_output_tokens, 98_304);
        assert!(m.reasoning);
        let m = find_model(&c, "glm-4-flash-250414").unwrap();
        assert_eq!(m.max_output_tokens, 16_384);
        assert!(!m.reasoning);
    }

    #[test]
    fn thinking_always_on_flags_5_3_generation() {
        assert!(thinking_always_on("glm-5.3"));
        assert!(thinking_always_on("glm-5.3-flash"));
        assert!(!thinking_always_on("glm-4.7-flash"));
        assert!(!thinking_always_on("glm-5.1"));
        assert!(!thinking_always_on("glm-zzz"));
    }

    #[test]
    fn effort_levels_follow_docs_availability() {
        // GLM-5.2+: low/high/max
        assert_eq!(effort_levels("glm-5.3"), Some(&["low", "high", "max"][..]));
        assert_eq!(
            effort_levels("glm-5.3-flash"),
            Some(&["low", "high", "max"][..])
        );
        // older tiers: no effort parameter
        assert_eq!(effort_levels("glm-5.1"), None);
        assert_eq!(effort_levels("glm-5"), None);
        assert_eq!(effort_levels("glm-5v-turbo"), None);
        assert_eq!(effort_levels("glm-4.7"), None);
        assert_eq!(effort_levels("glm-4-flash-250414"), None);
        assert_eq!(effort_levels("codegeex-4"), None);
    }

    #[test]
    fn million_suffix_stripped_for_lookup() {
        let spec = parse_model_spec("glm-5.3[1m]");
        assert_eq!(spec.base_id, "glm-5.3");
        assert!(spec.million);
        assert_eq!(api_model_id("glm-5.3[1m]"), "glm-5.3");
        assert_eq!(api_model_id("glm-4.6"), "glm-4.6");
        assert!(is_known_model("glm-5.3[1m]"));
        assert!(!is_known_model("glm-9.9"));
    }

    #[test]
    fn unknown_profile_lists_valid_ones() {
        let err = resolve_profile("nope").unwrap_err().to_string();
        assert!(err.contains("zai") && err.contains("bigmodel"));
        assert_eq!(
            resolve_profile("bigmodel").unwrap().quota_host,
            "https://open.bigmodel.cn"
        );
    }

    #[test]
    fn context_pct_uses_catalog_window_and_million_flag() {
        let m4 = parse_model_spec("glm-4.6");
        let m53 = parse_model_spec("glm-5.3");
        let m1m = parse_model_spec("glm-5.3[1m]");
        // inject catalog with only glm-4.6 + glm-5.3
        // (catalog() is the real embedded one; these models exist there too)
        assert_eq!(context_pct(&m4, Some(50_000)), Some((25, 200_000)));
        assert_eq!(context_pct(&m53, Some(1_000_000)), Some((100, 1_000_000)));
        assert_eq!(context_pct(&m1m, Some(1_000_000)), Some((100, 1_000_000)));
        assert_eq!(context_pct(&m4, None), None);
        assert_eq!(context_pct(&parse_model_spec("glm-zzz"), Some(10)), None);
    }

    #[test]
    fn display_model_id_keeps_suffix_only_when_configured() {
        // known 1M-window model without suffix stays bare: Claude Code is not
        // using the 1M window unless the suffix was configured
        assert_eq!(display_model_id("glm-5.3"), "glm-5.3");
        assert_eq!(display_model_id("glm-5.3[1m]"), "glm-5.3[1m]");
        // smaller-window model stays bare
        assert_eq!(display_model_id("glm-4.6"), "glm-4.6");
        // unknown model with suffix keeps the suffix
        assert_eq!(display_model_id("glm-zzz[1m]"), "glm-zzz[1m]");
        // unknown model without suffix stays bare
        assert_eq!(display_model_id("glm-zzz"), "glm-zzz");
    }

    #[test]
    fn test_catalog_lookup() {
        let c = test_catalog();
        assert!(find_model(&c, "glm-4.6").is_some());
        assert!(find_model(&c, "glm-5.3").is_some());
        assert!(find_model(&c, "glm-nope").is_none());
        let spec = parse_model_spec("glm-4.6[1m]");
        assert!(find_model(&c, &spec.base_id).is_some());
    }
}
