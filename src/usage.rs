use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::config::ResolvedConfig;
use crate::quota::{self, Limit};

/// Number of seconds in a day.
const DAY: u64 = 86_400;

/// A user-visible row: name, usage, unit label.
#[derive(Debug, Clone)]
pub struct Row {
    pub name: String,
    pub value: String,
}

/// Output of one endpoint: section title, rows, or an error.
#[derive(Debug, Clone)]
pub enum Section {
    Ok { title: String, rows: Vec<Row> },
    Err { title: String, message: String },
}

/// Generic envelope for the usage endpoints (fields vary wildly; only the
/// fields we render are modeled).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct UsageEnvelope {
    pub code: Option<i64>,
    pub success: Option<bool>,
    pub data: Option<serde_json::Value>,
}

/// Read rows from the common shapes: data.items[], data.usageDetails[],
/// data.list[], data.activityList[], data.details[], or the envelope itself.
pub fn rows_from_data(data: &serde_json::Value) -> Vec<serde_json::Value> {
    for key in [
        "items",
        "usageDetails",
        "list",
        "activityList",
        "details",
        "records",
        "limitList",
    ] {
        if let Some(arr) = data.get(key).and_then(|v| v.as_array()) {
            return arr.clone();
        }
    }
    if let Some(arr) = data.as_array() {
        return arr.clone();
    }
    // model-usage returns the list under data.usageDetails.modelUsage or similar
    if let Some(details) = data.get("modelUsage").and_then(|v| v.as_array()) {
        return details.clone();
    }
    Vec::new()
}

fn fmt_num(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{:.0}", v)
    } else {
        format!("{v:.2}")
    }
}

fn token_cell(item: &serde_json::Value) -> Option<String> {
    for k in ["inputTokens", "outputTokens", "totalTokens", "usage"] {
        if let Some(n) = item.get(k).and_then(|v| v.as_f64()) {
            return Some(fmt_num(n));
        }
    }
    None
}

fn name_of(item: &serde_json::Value) -> Option<String> {
    for k in [
        "displayName",
        "modelCode",
        "toolName",
        "name",
        "time",
        "date",
    ] {
        if let Some(s) = item.get(k).and_then(|v| v.as_str()) {
            return Some(s.to_string());
        }
    }
    None
}

fn count_of(item: &serde_json::Value) -> Option<String> {
    for k in ["count", "usageCount", "callCount"] {
        if let Some(n) = item.get(k).and_then(|v| v.as_f64()) {
            return Some(fmt_num(n));
        }
    }
    None
}

/// Render one endpoint's body into a section.
pub fn section_from_body(title: &str, body: &str) -> Section {
    let parsed: Result<UsageEnvelope, _> = serde_json::from_str(body);
    let Ok(env) = parsed else {
        return Section::Err {
            title: title.to_string(),
            message: "invalid JSON response".into(),
        };
    };
    let Some(data) = env.data else {
        return Section::Err {
            title: title.to_string(),
            message: "no data in response".into(),
        };
    };
    let items = rows_from_data(&data);
    if items.is_empty() {
        return Section::Ok {
            title: title.to_string(),
            rows: vec![],
        };
    }
    let mut rows: Vec<Row> = Vec::new();
    for item in &items {
        let name = name_of(item).unwrap_or_else(|| "<unknown>".to_string());
        let mut parts: Vec<String> = Vec::new();
        if let Some(t) = token_cell(item) {
            parts.push(t);
        }
        if let Some(c) = count_of(item) {
            parts.push(c);
        }
        rows.push(Row {
            name,
            value: parts.join("  "),
        });
    }
    Section::Ok {
        title: title.to_string(),
        rows,
    }
}

pub fn format_section(s: &Section) -> String {
    match s {
        Section::Err { title, message } => format!("== {title} ==\nerror: {message}"),
        Section::Ok { title, rows } => {
            let mut out = format!("== {title} ==");
            if rows.is_empty() {
                out.push_str("\n(no usage in range)");
            } else {
                let w = rows
                    .iter()
                    .map(|r| r.name.chars().count())
                    .max()
                    .unwrap_or(0);
                for r in rows {
                    out.push('\n');
                    out.push_str(&r.name);
                    if w > r.name.chars().count() {
                        out.push_str(&" ".repeat(w - r.name.chars().count()));
                    }
                    if !r.value.is_empty() {
                        out.push_str("  ");
                        out.push_str(&r.value);
                    }
                }
            }
            out
        }
    }
}

pub fn format_sections(sections: &[Section]) -> String {
    sections
        .iter()
        .map(format_section)
        .collect::<Vec<_>>()
        .join("\n")
}

/// GET a usage endpoint, returning its body (empty on error is NOT how we
/// report: errors are per-section strings).
pub fn get_usage(url: &str, key: &str, timeout: Duration) -> Result<String> {
    let agent = ureq::AgentBuilder::new().timeout(timeout).build();
    let resp = agent
        .get(url)
        .set("Authorization", &format!("Bearer {key}"))
        .call()
        .with_context(|| format!("request to {url} failed"))?;
    resp.into_string().context("reading response body")
}

fn fmt_ts(secs: u64) -> String {
    // local time "yyyy-MM-dd HH:mm:ss", matching what Z.ai's own usage script sends
    use chrono::{Local, TimeZone};
    match Local.timestamp_opt(secs as i64, 0) {
        chrono::LocalResult::Single(t) | chrono::LocalResult::Ambiguous(t, _) => {
            t.format("%Y-%m-%d %H:%M:%S").to_string()
        }
        chrono::LocalResult::None => String::from("1970-01-01 00:00:00"),
    }
}

/// Cover `range_days` ending now: startTime/endTime in local time.
pub fn range_params(range_days: u64) -> (String, String) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let end = fmt_ts(now);
    let start = fmt_ts(now.saturating_sub(range_days * DAY));
    (start, end)
}

fn quota_section(cfg: &ResolvedConfig, key: &str, timeout: Duration) -> Section {
    let url = cfg.usage_quota_url.clone();
    match quota::fetch_quota(&url, key, timeout) {
        Ok(env) => {
            let mut rows: Vec<Row> = Vec::new();
            for l in env
                .data
                .as_ref()
                .map(|d| d.limits.as_slice())
                .unwrap_or(&[])
            {
                rows.push(Row {
                    name: l.label(),
                    value: describe_limit(l),
                });
            }
            Section::Ok {
                title: "quota/limit".into(),
                rows,
            }
        }
        Err(e) => Section::Err {
            title: "quota/limit".into(),
            message: format!("{e:#}"),
        },
    }
}

fn describe_limit(l: &Limit) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(p) = l.pct_rounded() {
        parts.push(format!("{p}% used"));
    }
    if let Some(r) = l.ratio() {
        parts.push(r);
    }
    if let Some(rem) = l.remaining {
        parts.push(format!("{rem:.0} remaining"));
    }
    if parts.is_empty() {
        "no data".into()
    } else {
        parts.join(", ")
    }
}

fn fetch_section(title: &str, url: &str, key: &str, timeout: Duration) -> Section {
    match get_usage(url, key, timeout) {
        Ok(body) => section_from_body(title, &body),
        Err(e) => Section::Err {
            title: title.into(),
            message: format!("{e:#}"),
        },
    }
}

/// All sections for `glm usage`. Individual failures are reported per-section.
pub fn collect(
    cfg: &ResolvedConfig,
    key: &str,
    range_days: u64,
    timeout: Duration,
) -> Vec<Section> {
    // The other monitor endpoints share the quota host. When a custom
    // usage_quota_url is configured we derive the host from it so test
    // environments (and mirrors) route everything consistently.
    let host = usage_host(cfg);
    let (start, end) = range_params(range_days);
    let mut out = vec![quota_section(cfg, key, timeout)];
    let model = format!("{host}/api/monitor/usage/model-usage?startTime={start}&endTime={end}");
    let tool = format!("{host}/api/monitor/usage/tool-usage?startTime={start}&endTime={end}");
    let activity = format!(
        "{host}/api/monitor/usage/credit-usage/activity?type=1&startTime={start}&endTime={end}"
    );
    let detail = format!(
        "{host}/api/monitor/usage/credit-usage/usage-detail?type=1&startTime={start}&endTime={end}"
    );
    out.push(fetch_section("model-usage", &model, key, timeout));
    out.push(fetch_section("tool-usage", &tool, key, timeout));
    out.push(fetch_section("credit activity", &activity, key, timeout));
    out.push(fetch_section("credit usage-detail", &detail, key, timeout));
    out
}

/// The monitor host: derived from usage_quota_url when it differs from the
/// profile default, else the profile's quota host.
fn usage_host(cfg: &ResolvedConfig) -> String {
    let default = crate::config::usage_quota_default(cfg.profile);
    if cfg.usage_quota_url != default {
        cfg.usage_quota_url
            .trim_end_matches("/api/monitor/usage/quota/limit")
            .to_string()
    } else {
        cfg.profile.quota_host.to_string()
    }
}

pub fn json_from_sections(sections: &[Section]) -> serde_json::Value {
    let mut arr = Vec::new();
    for s in sections {
        let obj = match s {
            Section::Ok { title, rows } => serde_json::json!({
                "endpoint": title, "ok": true,
                "rows": rows.iter().map(|r| serde_json::json!({"name": r.name, "value": r.value})).collect::<Vec<_>>(),
            }),
            Section::Err { title, message } => {
                serde_json::json!({ "endpoint": title, "ok": false, "error": message })
            }
        };
        arr.push(obj);
    }
    serde_json::Value::Array(arr)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_params_shape() {
        let (start, end) = range_params(7);
        assert!(start.len() == 19 && end.len() == 19);
        assert!(start < end, "{start} < {end}");
    }

    #[test]
    fn rows_from_common_shapes() {
        for key in ["items", "usageDetails", "list", "activityList", "details"] {
            let data = serde_json::json!({ key: [{"name": "a"}] });
            assert_eq!(rows_from_data(&data).len(), 1, "{key}");
        }
    }

    #[test]
    fn section_error_on_bad_body() {
        match section_from_body("x", "not json") {
            Section::Err { message, .. } => assert!(message.contains("invalid JSON")),
            _ => panic!("expected Err section"),
        }
    }

    #[test]
    fn table_formatting_aligns() {
        let sec = Section::Ok {
            title: "t".into(),
            rows: vec![
                Row {
                    name: "a".into(),
                    value: "1".into(),
                },
                Row {
                    name: "longname".into(),
                    value: "2".into(),
                },
            ],
        };
        let txt = format_section(&sec);
        assert!(txt.contains("a         1"), "{txt}");
        assert!(txt.contains("longname  2"), "{txt}");
    }

    #[test]
    fn fmt_ts_outputs_local_style() {
        // 2026-09-02T04:00:00Z is the epoch used in the fixtures; UTC has no
        // DST so the civil date check is stable everywhere.
        // Shape only: the wall-clock value depends on the local timezone.
        let t = fmt_ts(1_788_321_600);
        assert_eq!(t.len(), 19, "{t}");
        assert_eq!(&t[4..5], "-");
        assert_eq!(&t[10..11], " ");
        assert_eq!(&t[13..14], ":");
        assert!(t.starts_with("2026-09-0"), "{t}");
        // local rendering must round-trip through chrono's own parser
        use chrono::{Local, NaiveDateTime, TimeZone};
        let naive = NaiveDateTime::parse_from_str(&t, "%Y-%m-%d %H:%M:%S").unwrap();
        let back = Local.from_local_datetime(&naive).single().unwrap();
        assert_eq!(back.timestamp(), 1_788_321_600);
    }
}
