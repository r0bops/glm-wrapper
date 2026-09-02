use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// A single quota/limit entry from the envelope data.limits[].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Limit {
    #[serde(rename = "type")]
    pub kind: String,
    pub unit: Option<u64>,
    pub number: Option<u64>,
    pub usage: Option<f64>,
    #[serde(rename = "currentValue")]
    pub current_value: Option<f64>,
    pub remaining: Option<f64>,
    pub percentage: Option<f64>,
    #[serde(rename = "nextResetTime")]
    pub next_reset_time: Option<i64>,
    #[serde(rename = "usageDetails")]
    pub usage_details: Vec<UsageDetail>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct UsageDetail {
    #[serde(rename = "modelCode")]
    pub model_code: Option<String>,
    #[serde(rename = "displayName")]
    pub display_name: Option<String>,
    pub usage: Option<f64>,
}

/// Envelope: {code, msg, success, data}.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct QuotaEnvelope {
    pub code: Option<i64>,
    pub msg: Option<String>,
    pub success: Option<bool>,
    pub data: Option<QuotaData>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct QuotaData {
    pub limits: Vec<Limit>,
}

impl Limit {
    /// Display label per the verified rule.
    pub fn label(&self) -> String {
        match self.kind.as_str() {
            "TOKENS_LIMIT" | "CREDIT_LIMIT" if matches!(self.unit, Some(3) | Some(5)) => {
                "5h".into()
            }
            "TOKENS_LIMIT" if self.unit == Some(6) => "wk".into(),
            "TIME_LIMIT" => "plan".into(),
            other => other.to_lowercase(),
        }
    }

    /// Percentage used, rounded, when the API reports it.
    pub fn pct_rounded(&self) -> Option<u64> {
        self.percentage.map(|p| p.round() as u64)
    }

    /// `currentValue/usage` when both are numeric.
    pub fn ratio(&self) -> Option<String> {
        let cur = self.current_value?;
        let used = self.usage?;
        Some(format!("{:.0}/{used:.0}", cur))
    }

    #[cfg(test)]
    pub fn has_numeric_remaining(&self) -> bool {
        self.remaining.is_some()
    }
}

/// Pick the primary limit: TIME_LIMIT, else first entry with numeric
/// remaining, else the first entry.
#[cfg(test)]
pub fn primary_index(limits: &[Limit]) -> Option<usize> {
    limits
        .iter()
        .position(|l| l.kind == "TIME_LIMIT")
        .or_else(|| limits.iter().position(|l| l.has_numeric_remaining()))
        .or(if limits.is_empty() { None } else { Some(0) })
}

/// Compact one-line summary for the statusline.
#[cfg(test)]
pub fn segment_text(limit: &Limit) -> String {
    let label = limit.label();
    match limit.pct_rounded() {
        Some(p) => format!("{label} {p}%"),
        None => format!("{label} usage n/a"),
    }
}

/// What the statusline renderer needs: label, pct, ratio.
pub fn statusline_segment(limit: &Limit) -> (String, Option<u64>, Option<String>) {
    (limit.label(), limit.pct_rounded(), limit.ratio())
}

/// Fields the lenient parser keeps as numbers when present. Anything else in
/// the payload (wrong-typed scalars, unknown shapes) is dropped, never fatal.
const NUMERIC_LIMIT_KEYS: [&str; 7] = [
    "unit",
    "number",
    "usage",
    "currentValue",
    "remaining",
    "percentage",
    "nextResetTime",
];

fn scrub_limits_array(arr: &mut [serde_json::Value]) {
    for item in arr.iter_mut() {
        let Some(map) = item.as_object_mut() else {
            continue;
        };
        for (k, v) in map.clone().iter() {
            match k.as_str() {
                "type" if !v.is_string() => {
                    map.remove(k);
                }
                "usageDetails" if !v.is_array() => {
                    map.remove(k);
                }
                other if NUMERIC_LIMIT_KEYS.contains(&other) && !v.is_number() => {
                    map.remove(k);
                }
                _ => {}
            }
        }
    }
}

/// Parse the quota/limit body leniently: envelope shape, or a bare `limits`
/// array. Wrong-typed fields are dropped, never fatal.
pub fn parse_envelope(body: &str) -> Result<QuotaEnvelope> {
    let mut v: serde_json::Value =
        serde_json::from_str(body).context("invalid JSON in quota response")?;
    // Accept both the documented envelope and a bare limits array.
    let mut wrapper = serde_json::Map::new();
    if v.is_array() {
        wrapper.insert(
            "data".into(),
            serde_json::Value::Object(serde_json::Map::new()),
        );
        if let Some(obj) = wrapper.get_mut("data").and_then(|d| d.as_object_mut()) {
            obj.insert("limits".into(), v);
        }
        v = serde_json::Value::Object(wrapper);
    }
    if let Some(map) = v.as_object_mut() {
        for (k, val) in map.clone().iter() {
            match k.as_str() {
                "code" if !val.is_number() => {
                    map.remove(k);
                }
                "success" => {
                    if !(val.is_boolean() || (val.is_number() && val.as_f64() == Some(1.0))) {
                        map.remove(k);
                    }
                    // normalize the accepted 1.0 into a real boolean
                    if val.is_number() && val.as_f64() == Some(1.0) {
                        if let Some(v) = map.get_mut("success") {
                            *v = serde_json::Value::Bool(true);
                        }
                    }
                }
                "data" if val.is_object() => {
                    if let Some(data) = val.as_object() {
                        if let Some(limits) = data.get("limits") {
                            if let Some(arr) = limits.as_array() {
                                let mut arr = arr.clone();
                                scrub_limits_array(&mut arr);
                                if let Some(d) = map.get_mut("data").and_then(|d| d.as_object_mut())
                                {
                                    d.insert("limits".into(), serde_json::Value::Array(arr));
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }
    let env: QuotaEnvelope = serde_json::from_value(v)?;
    Ok(env)
}

/// Fetch quota/limit with an explicit timeout and Bearer auth.
pub fn fetch_quota(url: &str, key: &str, timeout: Duration) -> Result<QuotaEnvelope> {
    let agent = ureq::AgentBuilder::new().timeout(timeout).build();
    let resp = agent
        .get(url)
        .set("Authorization", &format!("Bearer {key}"))
        .call()
        .with_context(|| format!("quota request to {url} failed"))?;
    let text = resp.into_string().context("reading quota response")?;
    parse_envelope(&text)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuotaCache {
    pub fetched_at_ms: i64,
    pub url: String,
    pub envelope: QuotaEnvelope,
}

impl QuotaCache {
    pub fn is_fresh(&self, ttl: Duration, now_ms: i64) -> bool {
        let age_ms = now_ms.saturating_sub(self.fetched_at_ms);
        age_ms <= ttl.as_millis() as i64
    }
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Read the cache if younger than ttl; corrupt or stale returns None.
pub fn load_cache(path: &Path, ttl: Duration) -> Option<QuotaCache> {
    let raw = std::fs::read_to_string(path).ok()?;
    let cache: QuotaCache = serde_json::from_str(&raw).ok()?;
    cache.is_fresh(ttl, now_ms()).then_some(cache)
}

/// Atomically rewrite the cache.
pub fn store_cache(path: &Path, cache: &QuotaCache) -> Result<()> {
    let body = serde_json::to_string(cache).context("serializing quota cache")?;
    crate::util::atomic_write(path, &body)
}

/// Cache lookup + fetch combined for the statusline: (envelope, from_cache).
pub fn quota_for_statusline(
    cache_file: &Path,
    url: &str,
    key: &str,
    cache_ttl: Duration,
    fetch_timeout: Duration,
) -> Result<(QuotaEnvelope, bool)> {
    if let Some(cache) = load_cache(cache_file, cache_ttl) {
        return Ok((cache.envelope, true));
    }
    let envelope = fetch_quota(url, key, fetch_timeout)?;
    let cache = QuotaCache {
        fetched_at_ms: now_ms(),
        url: url.to_string(),
        envelope: envelope.clone(),
    };
    let _ = store_cache(cache_file, &cache); // cache write failures are non-fatal
    Ok((envelope, false))
}

pub fn cache_path() -> PathBuf {
    crate::paths::quota_cache_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limit(kind: &str, unit: Option<u64>) -> Limit {
        Limit {
            kind: kind.into(),
            unit,
            ..Default::default()
        }
    }

    fn with(
        mut l: Limit,
        pct: Option<f64>,
        cur: Option<f64>,
        usage: Option<f64>,
        remaining: Option<f64>,
    ) -> Limit {
        l.percentage = pct;
        l.current_value = cur;
        l.usage = usage;
        l.remaining = remaining;
        l
    }

    #[test]
    fn labels_follow_the_verified_rule() {
        assert_eq!(limit("TOKENS_LIMIT", Some(3)).label(), "5h");
        assert_eq!(limit("TOKENS_LIMIT", Some(5)).label(), "5h");
        assert_eq!(limit("CREDIT_LIMIT", Some(5)).label(), "5h");
        assert_eq!(limit("TOKENS_LIMIT", Some(6)).label(), "wk");
        assert_eq!(limit("TIME_LIMIT", None).label(), "plan");
        assert_eq!(limit("TOKENS_LIMIT", Some(1)).label(), "tokens_limit");
        assert_eq!(limit("CREDIT_LIMIT", None).label(), "credit_limit");
        assert_eq!(limit("MYSTERY_LIMIT", None).label(), "mystery_limit");
    }

    #[test]
    fn primary_pick_order() {
        let five = with(
            limit("TOKENS_LIMIT", Some(5)),
            Some(40.0),
            None,
            Some(5.0),
            Some(95.0),
        );
        let wk = with(
            limit("TOKENS_LIMIT", Some(6)),
            Some(10.0),
            None,
            Some(6.0),
            Some(54.0),
        );
        let plan = with(
            limit("TIME_LIMIT", None),
            Some(80.0),
            None,
            Some(80.0),
            Some(20.0),
        );
        let no_rem = with(
            limit("TOKENS_LIMIT", Some(5)),
            None,
            Some(1.0),
            Some(2.0),
            None,
        );
        assert_eq!(primary_index(&[five.clone(), wk.clone(), plan]), Some(2));
        assert_eq!(primary_index(&[five.clone(), wk]), Some(0));
        assert_eq!(primary_index(&[no_rem.clone(), five.clone()]), Some(1));
        assert_eq!(primary_index(&[no_rem]), Some(0));
        assert_eq!(primary_index(&[]), None);
    }

    #[test]
    fn envelope_parses_leniently() {
        let body = r#"{"code":0,"msg":"","success":true,"data":{"limits":[]}}"#;
        let e = parse_envelope(body).unwrap();
        assert_eq!(e.success, Some(true));
        assert_eq!(e.data.unwrap().limits.len(), 0);
    }

    #[test]
    fn malformed_body_is_error() {
        assert!(parse_envelope("not json at all").is_err());
    }

    #[test]
    fn wrong_typed_fields_are_dropped_not_fatal() {
        let body = r#"{"code":"x","success":1,"data":{"limits":[{"type":7,"unit":"five","percentage":"12","usageDetails":{}}]}}"#;
        let e = parse_envelope(body).unwrap();
        assert_eq!(e.code, None);
        assert_eq!(e.success, Some(true)); // success=1 accepted and normalized
        let limits = e.data.unwrap().limits;
        assert_eq!(limits.len(), 1);
        assert_eq!(limits[0].kind, "");
        assert_eq!(limits[0].unit, None);
        assert_eq!(limits[0].percentage, None);
        assert!(limits[0].usage_details.is_empty());
    }

    #[test]
    fn cache_freshness_logic() {
        let cache = QuotaCache {
            fetched_at_ms: 1_000_000,
            url: "u".into(),
            envelope: QuotaEnvelope::default(),
        };
        assert!(cache.is_fresh(Duration::from_secs(90), 1_000_000 + 89_000));
        assert!(!cache.is_fresh(Duration::from_secs(90), 1_000_000 + 91_000));
    }

    #[test]
    fn bare_limits_array_accepted() {
        let body = r#"[{"type":"TIME_LIMIT","percentage":12.5}]"#;
        let e = parse_envelope(body).unwrap();
        let limits = e.data.unwrap().limits;
        assert_eq!(limits.len(), 1);
        assert_eq!(limits[0].label(), "plan");
        assert_eq!(limits[0].pct_rounded(), Some(13));
    }

    #[test]
    fn segment_ratio_and_text() {
        let l = with(
            limit("TOKENS_LIMIT", Some(5)),
            Some(43.0),
            Some(4300.0),
            Some(10000.0),
            Some(5700.0),
        );
        let (label, pct, ratio) = statusline_segment(&l);
        assert_eq!(label, "5h");
        assert_eq!(pct, Some(43));
        assert_eq!(ratio, Some("4300/10000".to_string()));
        assert_eq!(segment_text(&l), "5h 43%");
        let empty = Limit::default();
        assert_eq!(empty.label(), "");
        assert_eq!(empty.pct_rounded(), None);
        assert_eq!(empty.ratio(), None);
    }
}
