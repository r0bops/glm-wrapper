//! End-to-end tests over the compiled binary: reserved-word dispatch,
//! statusline rendering against a stub quota server, cache reuse.

mod common;
use common::*;

fn write_key(xdg: &std::path::Path, key: &str) {
    std::fs::create_dir_all(xdg.join("glm")).unwrap();
    std::fs::write(xdg.join("glm").join("key"), format!("{key}\n")).unwrap();
}

fn write_cfg(xdg: &std::path::Path, body: &str) {
    std::fs::create_dir_all(xdg.join("glm")).unwrap();
    std::fs::write(xdg.join("glm").join("config.toml"), body).unwrap();
}

#[test]
fn reserved_words_never_reach_claude() {
    let xdg = scratch_xdg("reserved");
    // `glm init` with a valid key and no tty should run doctor, not claude.
    write_key(&xdg, "sk-x");
    let (stdout, _stderr, code) = run_glm(&["init"], &xdg, &[]);
    // doctor ran (claude missing on PATH => fail => exit 1, but no exec)
    assert!(stdout.contains("PASS  API key"), "stdout: {stdout}");
    assert_eq!(code, 1, "doctor fails without claude on PATH");
    // `glm statusline` is ours too
    let (stdout, _, _) = run_glm(&["statusline"], &xdg, &[]);
    assert!(stdout.contains("GLM|"), "stdout: {stdout}");
}

#[test]
fn models_shows_bundle_and_json() {
    let xdg = scratch_xdg("models");
    let (stdout, _, code) = run_glm(&["models"], &xdg, &[]);
    assert_eq!(code, 0);
    assert!(stdout.contains("glm-5.3"), "{stdout}");
    assert!(stdout.contains("glm-4.5-air"), "{stdout}");

    let (stdout, _, code) = run_glm(&["models", "--json"], &xdg, &[]);
    assert_eq!(code, 0);
    let v: serde_json::Value = serde_json::from_str(&stdout).expect("models --json is JSON");
    let arr = v.as_array().unwrap();
    assert!(arr.len() >= 20);
    assert_eq!(arr[0]["id"], "glm-5.3");
}

#[test]
fn models_refresh_falls_back_on_failure() {
    let xdg = scratch_xdg("models-refresh");
    write_key(&xdg, "sk-x");
    // Point the probe at a closed local port: the refresh must fail
    // gracefully to a one-line stderr and exit 0, without any network.
    let (stdout, stderr, code) = run_glm(
        &["models", "--refresh"],
        &xdg,
        &[(
            "GLM_USAGE_QUOTA_URL",
            "http://127.0.0.1:1/api/monitor/usage/quota/limit",
        )],
    );
    assert_eq!(code, 0, "refresh failure is not fatal");
    assert!(stdout.contains("glm-5.3"), "bundle still listed: {stdout}");
    assert!(
        stderr.contains("live refresh failed") || stderr.is_empty(),
        "stderr: {stderr}"
    );
}

#[test]
fn statusline_renders_from_stub_with_fixture_input() {
    let srv = StubServer::start(vec![(200, fixture("quota-limit.json"))]);
    let xdg = scratch_xdg("statusline-stub");
    write_key(&xdg, "sk-x");
    write_cfg(
        &xdg,
        &format!(
            "profile = \"zai\"\nmodel = \"glm-5.3[1m]\"\nsmall_model = \"glm-5.3-flash\"\nusage_quota_url = \"http://{}/quota\"\n",
            srv.addr
        ),
    );
    // feed the claude statusline JSON on stdin
    let mut child = std::process::Command::new(glm_bin());
    let mut out = child
        .arg("statusline")
        .env("XDG_CONFIG_HOME", &xdg)
        .env_remove("GLM_API_KEY")
        .env_remove("ZAI_API_KEY")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = out.stdin.take().unwrap();
    use std::io::Write;
    stdin
        .write_all(fixture("statusline-input.json").as_bytes())
        .unwrap();
    drop(stdin);
    let output = out.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0));
    assert!(
        stderr.is_empty(),
        "statusline must not write stderr: {stderr}"
    );
    // display name from input wins; segments from the stub quota (numbers are
    // wrapped in ANSI colors, so only assert on the labels)
    assert!(stdout.contains("GLM|GLM-5.3 "), "stdout: {stdout}");
    assert!(stdout.contains("5h"), "stdout: {stdout}");
    assert!(stdout.contains("wk"), "stdout: {stdout}");
    assert!(stdout.contains("plan"), "stdout: {stdout}");
    assert!(stdout.contains("ctx"), "stdout: {stdout}");
    // cache file written
    let cache =
        std::fs::read_to_string(xdg.join("glm").join("quota-cache.json")).expect("cache written");
    assert!(cache.contains("quota"), "cache: {cache}");
}

#[test]
fn statusline_renders_na_without_quota_server() {
    let xdg = scratch_xdg("statusline-na");
    write_key(&xdg, "sk-x");
    write_cfg(
        &xdg,
        "profile = \"zai\"\nmodel = \"glm-5.3\"\nsmall_model = \"glm-5.3-flash\"\nusage_quota_url = \"http://127.0.0.1:1/quota\"\n",
    );
    let mut child = std::process::Command::new(glm_bin());
    let mut out = child
        .arg("statusline")
        .env("XDG_CONFIG_HOME", &xdg)
        .env_remove("GLM_API_KEY")
        .env_remove("ZAI_API_KEY")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    drop(out.stdin.take());
    let output = out.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(0), "statusline never fails");
    assert!(stdout.contains("GLM|"), "stdout: {stdout}");
    assert!(
        stdout.contains("usage n/a") || stdout.contains("glm-5.3"),
        "stdout: {stdout}"
    );
}

#[test]
fn statusline_reuses_fresh_cache_without_network() {
    // Pre-seed a fresh cache; the server would fail the request if contacted
    // (it holds connections open), proving the cache path is taken.
    let srv = StubServer::start_silent();
    let xdg = scratch_xdg("statusline-cache");
    write_key(&xdg, "sk-x");
    write_cfg(
        &xdg,
        &format!(
            "profile = \"zai\"\nmodel = \"glm-4.6\"\nsmall_model = \"glm-4.6-flash\"\nusage_quota_url = \"http://{}/quota\"\nstatusline.cache_ttl_secs = 99999\n",
            srv.addr
        ),
    );
    // note: statusline.cache_ttl_secs top-level is NOT valid; it lives under [statusline]
    std::fs::write(
        xdg.join("glm").join("config.toml"),
        format!(
            "profile = \"zai\"\nmodel = \"glm-4.6\"\nsmall_model = \"glm-4.6-flash\"\nusage_quota_url = \"http://{}/quota\"\n\n[statusline]\ncache_ttl_secs = 99999\n",
            srv.addr
        ),
    )
    .unwrap();
    // seed cache with the 5h+wk+plan envelope
    let cache = serde_json::json!({
        "fetched_at_ms": (std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64),
        "url": format!("http://{}/quota", srv.addr),
        "envelope": serde_json::from_str::<serde_json::Value>(&fixture("quota-limit.json")).unwrap()
    });
    std::fs::write(
        xdg.join("glm").join("quota-cache.json"),
        serde_json::to_string(&cache).unwrap(),
    )
    .unwrap();

    let mut child = std::process::Command::new(glm_bin());
    let mut out = child
        .arg("statusline")
        .env("XDG_CONFIG_HOME", &xdg)
        .env_remove("GLM_API_KEY")
        .env_remove("ZAI_API_KEY")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    drop(out.stdin.take());
    let output = out.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(0));
    assert!(stdout.contains("5h"), "cached quota rendered: {stdout}");
    // no request reached the silent stub
    assert!(
        srv.request_lines().is_empty(),
        "cache must avoid the network"
    );
}

#[test]
fn usage_prints_per_section_errors() {
    let srv = StubServer::start(vec![
        (200, fixture("quota-limit.json")),
        (200, fixture("model-usage.json")),
        (200, fixture("tool-usage.json")),
        (500, "boom".to_string()),
        (200, fixture("credit-detail.json")),
    ]);
    let xdg = scratch_xdg("usage-stub");
    write_key(&xdg, "sk-x");
    write_cfg(
        &xdg,
        &format!(
            "profile = \"zai\"\nmodel = \"glm-5.3\"\nsmall_model = \"glm-5.3-flash\"\nusage_quota_url = \"http://{}/api/monitor/usage/quota/limit\"\nbase_url = \"http://{}/api/anthropic\"\n",
            srv.addr, srv.addr
        ),
    );
    let (stdout, stderr, code) = run_glm(&["usage", "--range", "7d"], &xdg, &[]);
    assert_eq!(
        code, 0,
        "usage exits 0 even with per-section errors: {stderr}"
    );
    assert!(stdout.contains("quota/limit"), "{stdout}");
    assert!(stdout.contains("model-usage"), "{stdout}");
    assert!(stdout.contains("tool-usage"), "{stdout}");
    assert!(stdout.contains("credit activity"), "{stdout}");
    // the failing endpoint shows its error in the section
    assert!(stdout.contains("error: request"), "{stdout}");
    assert!(stdout.contains("credit usage-detail"), "{stdout}");
    // time params were formatted yyyy-MM-dd HH:mm:ss (space -> %20 by ureq)
    let joined = srv.request_lines().join("\n");
    assert!(joined.contains("startTime=20"), "{joined}");
    assert!(
        joined.contains("%20"),
        "space must be URL-encoded: {joined}"
    );
    assert!(joined.contains("endTime=20"), "{joined}");
}
