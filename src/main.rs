use std::env;

use anyhow::{bail, Result};
use clap::Parser;

mod catalog;
mod config;
mod doctor;
mod keys;
mod paths;
mod quota;
mod registry;
mod relay;
mod run;
mod self_update;
mod statusline;
mod usage;
mod util;

/// First-position words that are OUR subcommands, never passed to claude.
/// `glm -- word` forwards any word (including these) to claude.
const SUBCOMMAND_WORDS: [&str; 11] = [
    "init",
    "config",
    "key",
    "models",
    "usage",
    "statusline",
    "doctor",
    "self-update",
    "sessions",
    "attach",
    "kill",
];

/// Exposed for run.rs unit tests that share the reserved-word list.
#[doc(hidden)]
pub fn main_sub_words() -> &'static [&'static str] {
    &SUBCOMMAND_WORDS
}

fn main() {
    // Unix CLI convention: dying silently on a closed pipe (`glm models |
    // head`) beats Rust's default of panicking on EPIPE in every println.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let argv: Vec<String> = env::args().skip(1).collect();
    let code = match dispatch(argv) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("glm: {e:#}");
            1
        }
    };
    std::process::exit(code);
}

/// Top-level routing on the raw argv: reserved first words are ours; anything
/// else (including a leading `--`) belongs to claude.
fn dispatch(argv: Vec<String>) -> Result<i32> {
    // `glm -- <anything>`: everything after `--` goes to claude verbatim.
    if argv.first().map(|a| a.as_str()) == Some("--") {
        return cmd_claude(&argv[1..]);
    }
    // `glm` with no arguments: run claude bare (interactive).
    if argv.is_empty() {
        return cmd_claude(&[]);
    }
    let first = argv[0].as_str();
    // `glm help` / `glm version` are ours; `--help` and `--version` belong to
    // claude, like every other leading-dash flag.
    if first == "help" {
        print_help();
        return Ok(0);
    }
    if first == "version" {
        println!("glm {}", self_update::VERSION);
        return Ok(0);
    }
    // claude flags or a plain prompt: pass through untouched.
    if first.starts_with('-') || !SUBCOMMAND_WORDS.contains(&first) {
        return cmd_claude(&argv);
    }
    cmd_sub(&argv)
}

fn print_help() {
    println!(
        "glm {} — run Claude Code against Z.ai / Zhipu GLM models\n\
         \n\
         USAGE:\n\
         \x20 glm [claude args...]      run claude with GLM models\n\
         \x20 glm -- [claude args...]    force claude, even for reserved words\n\
         \x20 glm help | version        this help / glm's own version\n\
         \n\
         COMMANDS:\n\
         \x20 init         first-run setup (config + key + doctor)\n\
         \x20 config       get/set config values (`config path` prints the file)\n\
         \x20 key          store the API key (`key path` prints the file)\n\
         \x20 models       list bundled models [--refresh] [--json]\n\
         \x20 usage        usage report [--range 7d|30d] [--json]\n\
         \x20 statusline   print the Claude Code status line (reads stdin)\n\
         \x20 doctor       environment checks\n\
         \x20 self-update  update from GitHub releases [--check]\n\
         \x20 sessions     list glm sessions (live / backgrounded)\n\
         \x20 attach [tgt]  reconnect to a backgrounded session (Ctrl-Q detaches)\n\
         \x20 kill [tgt]    terminate agents (default: all); tgt = pid or\n\
         \x20               unique 3+ char prefix of pid / session id\n\
         \n\
         Run `glm <command> --help` for options.",
        self_update::VERSION,
    );
}

/// Hand the rest of argv to the clap subcommand parser.
fn cmd_sub(argv: &[String]) -> Result<i32> {
    let mut clap_argv = vec![env::args().next().unwrap_or_else(|| "glm".into())];
    clap_argv.extend(argv.iter().cloned());
    let cli = Cli::parse_from(&clap_argv);
    run_sub(&cli)
}

/// clap model for the built-in subcommands.
#[derive(Parser)]
#[command(
    name = "glm",
    version = self_update::VERSION,
    about = "Run Claude Code against Z.ai / Zhipu GLM models",
    disable_help_subcommand = true,
    subcommand_required = true
)]
struct Cli {
    #[command(subcommand)]
    sub: Sub,
}

#[derive(clap::Subcommand)]
enum Sub {
    /// First-run setup: config, key prompt, doctor
    Init,
    /// Read/write config values
    Config {
        #[command(subcommand)]
        cmd: ConfigCmd,
    },
    /// Manage the API key file
    Key {
        #[command(subcommand)]
        cmd: KeyCmd,
    },
    /// List models from the bundled catalog (optionally refreshed)
    Models {
        /// Fetch live ids from the API and show which are missing locally
        #[arg(long)]
        refresh: bool,
        /// JSON output
        #[arg(long)]
        json: bool,
    },
    /// Usage report across the quota and credit endpoints
    Usage {
        /// Report window
        #[arg(long, value_enum, default_value_t = RangeArg::D7)]
        range: RangeArg,
        /// JSON output
        #[arg(long)]
        json: bool,
    },
    /// Emit the Claude Code statusline (reads stdin JSON)
    Statusline,
    /// Environment checks
    Doctor,
    /// Self-update from GitHub releases
    SelfUpdate {
        /// Only report the latest version
        #[arg(long)]
        check: bool,
    },
    /// List glm sessions (live and backgrounded)
    Sessions,
    /// Reconnect to a backgrounded session (most recent by default)
    Attach {
        /// glm/agent pid or session-id prefix (3+ chars, unique)
        target: Option<String>,
    },
    /// Terminate tracked agent processes (all by default)
    Kill {
        /// glm/agent pid or session-id prefix (3+ chars, unique)
        target: Option<String>,
    },
    /// Print the config.toml path
    ConfigPath,
    /// Print the key path
    KeyPath,
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum RangeArg {
    #[value(name = "7d")]
    D7,
    #[value(name = "30d")]
    D30,
}

impl RangeArg {
    fn days(self) -> u64 {
        match self {
            RangeArg::D7 => 7,
            RangeArg::D30 => 30,
        }
    }
}

#[derive(clap::Subcommand)]
enum ConfigCmd {
    /// Print a config value
    Get { key: String },
    /// Set a config value
    Set { key: String, value: String },
    /// Print the config file path
    Path,
}

#[derive(clap::Subcommand)]
enum KeyCmd {
    /// Store the API key (prompts without echo)
    Set {
        #[arg(long)]
        stdin: bool,
    },
    /// Print the key file path
    Path,
}

/// Load config.toml (defaults when missing) and resolve everything.
fn load_config() -> Result<config::ResolvedConfig> {
    let file = config::load_file()?;
    config::resolve(Some(&file), &std::collections::BTreeMap::new())
}

/// Whether the user already passed a --settings flag for claude.
fn user_passed_settings(args: &[String]) -> bool {
    args.iter()
        .take_while(|a| a.as_str() != "--")
        .any(|a| a == "--settings" || a.starts_with("--settings="))
}

/// Run claude with our env, settings.json, and argv.
fn cmd_claude(args: &[String]) -> Result<i32> {
    let cfg = load_config()?;
    let key = keys::resolve_key(None)?;
    if !paths::config_file().exists() {
        eprintln!("glm: no config.toml yet — defaults in effect; `glm init` creates one");
    }
    // 1. env mutations
    let plan = run::env_plan(&cfg, &key.value);
    run::apply_env_plan(&plan);
    // 2. settings.json (regenerated on every run)
    let settings_path = paths::settings_file();
    run::write_settings(&paths::current_exe(), &cfg, &settings_path)?;
    // 3. the glm personal slash commands (idempotent; foreign files with the
    // same names are never touched)
    match run::install_personal_commands(&paths::claude_commands_dir()) {
        Ok(actions) => {
            for (name, action) in &actions {
                if action.starts_with("kept") {
                    eprintln!("glm: note: your own /{name} command was left untouched");
                }
            }
        }
        Err(e) => eprintln!("glm: warning: could not install slash commands: {e:#}"),
    }
    // 3b. surface backgrounded sessions so they are not forgotten
    if relay::is_interactive(args) {
        let bg: Vec<_> = registry::load(registry::pid_alive)
            .into_iter()
            .filter(|e| e.backgrounded)
            .collect();
        match bg.len() {
            0 => {}
            1 => eprintln!("glm: 1 backgrounded session - `glm attach` to resume it"),
            n => eprintln!("glm: {n} backgrounded sessions - `glm attach` / `glm sessions`"),
        }
    }
    // 4. claude args
    let mut final_args = args.to_vec();
    if !user_passed_settings(args) {
        final_args.push("--settings".to_string());
        final_args.push(settings_path.display().to_string());
    }
    // 5. warn once when the model is not in the catalog
    if !catalog::is_known_model(&cfg.model) {
        eprintln!(
            "glm: warning: model {:?} is not in the bundled catalog; running claude anyway",
            catalog::api_model_id(&cfg.model)
        );
    }
    // 6. run claude (interactive pty relay, else exec; signals and exit
    // codes pass through)
    match run::start_claude(&final_args) {
        Ok(()) => unreachable!("start_claude never returns Ok"),
        Err(e) => {
            let missing = e
                .chain()
                .any(|c| c.to_string().contains("claude is not installed"));
            eprintln!("glm: {e:#}");
            if missing {
                return Ok(127);
            }
            Err(e)
        }
    }
}

fn run_sub(cli: &Cli) -> Result<i32> {
    match &cli.sub {
        Sub::Init => cmd_init(),
        Sub::Config { cmd } => cmd_config(cmd),
        Sub::Key { cmd } => cmd_key(cmd),
        Sub::Models { refresh, json } => cmd_models(*refresh, *json),
        Sub::Usage { range, json } => cmd_usage(*range, *json),
        Sub::Statusline => cmd_statusline(),
        Sub::Doctor => cmd_doctor(),
        Sub::SelfUpdate { check } => cmd_self_update(*check),
        Sub::Sessions => cmd_sessions(),
        Sub::Attach { target } => cmd_attach(target.as_deref()),
        Sub::Kill { target } => cmd_kill(target.as_deref()),
        Sub::ConfigPath => {
            println!("{}", paths::config_file().display());
            Ok(0)
        }
        Sub::KeyPath => {
            println!("{}", paths::key_file().display());
            Ok(0)
        }
    }
}

fn cmd_init() -> Result<i32> {
    let created = config::ensure_config_file()?;
    if created {
        println!("wrote default config to {}", paths::config_file().display());
    }
    // prompt for a key if none resolves
    if keys::resolve_key(None).is_err() {
        println!();
        println!("{}", keys::missing_key_hint());
        let value = util::prompt_secret("Paste your Z.ai API key (input hidden): ")?;
        if value.trim().is_empty() {
            bail!("no key provided; you can add one later with `glm key set`");
        }
        keys::store_key(&value)?;
        println!("stored API key in {}", paths::key_file().display());
    }
    cmd_doctor()
}

fn valid_config_keys() -> Vec<&'static str> {
    let mut v = config::TOP_KEYS.to_vec();
    v.extend(["statusline.cache_ttl_secs", "statusline.timeout_secs"]);
    v
}

fn lookup_config_value(cfg: &config::ConfigFile, key: &str) -> Option<String> {
    match key {
        "profile" => cfg.profile.clone(),
        "model" => cfg.model.clone(),
        "small_model" => cfg.small_model.clone(),
        "base_url" => cfg.base_url.clone(),
        "usage_quota_url" => cfg.usage_quota_url.clone(),
        "api_timeout_ms" => cfg.api_timeout_ms.map(|v| v.to_string()),
        "effort" => cfg.effort.clone(),
        "thinking_budget" => cfg.thinking_budget.map(|v| v.to_string()),
        "statusline.cache_ttl_secs" => cfg
            .statusline
            .as_ref()
            .and_then(|s| s.cache_ttl_secs)
            .map(|v| v.to_string()),
        "statusline.timeout_secs" => cfg
            .statusline
            .as_ref()
            .and_then(|s| s.timeout_secs)
            .map(|v| v.to_string()),
        _ => None,
    }
}

fn set_config_value(cfg: &mut config::ConfigFile, key: &str, value: &str) -> bool {
    match key {
        "profile" => cfg.profile = Some(value.to_string()),
        "model" => cfg.model = Some(value.to_string()),
        "small_model" => cfg.small_model = Some(value.to_string()),
        "base_url" => cfg.base_url = Some(value.to_string()),
        "usage_quota_url" => cfg.usage_quota_url = Some(value.to_string()),
        "api_timeout_ms" => match value.parse() {
            Ok(v) => cfg.api_timeout_ms = Some(v),
            Err(_) => return false,
        },
        "effort" => match config::normalize_effort(value) {
            Ok(v) => cfg.effort = Some(v),
            Err(_) => return false,
        },
        "thinking_budget" => match value.parse() {
            Ok(v) => cfg.thinking_budget = Some(v),
            Err(_) => return false,
        },
        "statusline.cache_ttl_secs" => match value.parse() {
            Ok(v) => {
                cfg.statusline
                    .get_or_insert_with(Default::default)
                    .cache_ttl_secs = Some(v)
            }
            Err(_) => return false,
        },
        "statusline.timeout_secs" => match value.parse() {
            Ok(v) => {
                cfg.statusline
                    .get_or_insert_with(Default::default)
                    .timeout_secs = Some(v)
            }
            Err(_) => return false,
        },
        _ => return false,
    }
    true
}

fn cmd_config(cmd: &ConfigCmd) -> Result<i32> {
    match cmd {
        ConfigCmd::Get { key } => {
            let cfg = config::load_file()?;
            match lookup_config_value(&cfg, key) {
                Some(v) => {
                    println!("{v}");
                    Ok(0)
                }
                None => {
                    if valid_config_keys().contains(&key.as_str()) {
                        // valid key that is not set in this config: nothing
                        // to print (the effective value may come from env)
                        Ok(0)
                    } else {
                        unknown_config_key(key)
                    }
                }
            }
        }
        ConfigCmd::Set { key, value } => {
            let mut cfg = config::load_file()?;
            if key == "model" && !catalog::is_known_model(value) {
                eprintln!("glm: warning: model {value:?} is not in the bundled catalog");
            }
            if !set_config_value(&mut cfg, key, value) {
                if !valid_config_keys().contains(&key.as_str()) {
                    return unknown_config_key(key);
                }
                if key == "effort" {
                    bail!(
                        "invalid value for effort: {value:?}; valid values: {}",
                        config::EFFORT_VALUES.join(", ")
                    );
                }
                bail!("invalid value for {key}: {value:?}");
            }
            config::write_config(&cfg)?;
            println!("{key} = {value}");
            Ok(0)
        }
        ConfigCmd::Path => {
            println!("{}", paths::config_file().display());
            Ok(0)
        }
    }
}

fn unknown_config_key(key: &str) -> Result<i32> {
    bail!(
        "unknown config key {key:?}; valid keys: {}",
        valid_config_keys().join(", ")
    )
}

fn cmd_key(cmd: &KeyCmd) -> Result<i32> {
    match cmd {
        KeyCmd::Set { stdin } => {
            let value = if *stdin {
                let mut line = String::new();
                std::io::stdin().read_line(&mut line)?;
                line.trim().to_string()
            } else {
                util::prompt_secret("Paste your Z.ai API key (input hidden): ")?
            };
            if value.trim().is_empty() {
                bail!("empty API key");
            }
            keys::store_key(&value)?;
            println!("stored API key in {}", paths::key_file().display());
            Ok(0)
        }
        KeyCmd::Path => {
            println!("{}", paths::key_file().display());
            Ok(0)
        }
    }
}

fn cmd_models(refresh: bool, json: bool) -> Result<i32> {
    let cfg = load_config()?;
    let entries = catalog::catalog();
    let provider = entries
        .providers
        .iter()
        .find(|p| p.id == cfg.profile.id)
        .ok_or_else(|| anyhow::anyhow!("profile {} not in catalog", cfg.profile.id))?;
    if json {
        let models: Vec<serde_json::Value> = provider
            .models
            .iter()
            .map(|m| {
                serde_json::json!({
                    "id": m.id,
                    "contextWindow": m.context_window,
                    "maxOutputTokens": m.max_output_tokens,
                    "modalities": m.modalities,
                    "reasoning": m.reasoning,
                    "million": catalog::display_model_id(&m.id).ends_with("[1m]"),
                    "effortLevels": catalog::effort_levels(&m.id),
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&models)?);
        return Ok(0);
    }
    println!(
        "provider {} ({} models)",
        provider.id,
        provider.models.len()
    );
    for m in &provider.models {
        let win = m.context_window;
        let win_txt = if win >= 1_000_000 {
            "1M".to_string()
        } else {
            format!("{win}")
        };
        let one_m = if catalog::display_model_id(&m.id).ends_with("[1m]") {
            "  [1m]"
        } else {
            ""
        };
        // Documented reasoning-effort values; "-" when the model has no
        // effort parameter (server default applies).
        let effort = match catalog::effort_levels(&m.id) {
            Some(levels) => levels.join("/"),
            None => "-".to_string(),
        };
        println!(
            "{:<22} ctx {:<8} max-out {:<8} {:<12} {}{}",
            m.id,
            win_txt,
            m.max_output_tokens,
            effort,
            m.modalities.join("+"),
            one_m
        );
    }
    if refresh {
        let key = match keys::resolve_key(None) {
            Ok(k) => k.value,
            Err(_) => {
                eprintln!("glm: models --refresh needs an API key; showing bundled catalog");
                return Ok(0);
            }
        };
        // Host comes from the (overridable) quota URL so GLM_USAGE_QUOTA_URL
        // redirects this probe too; falls back to the profile host.
        let host =
            url_origin(&cfg.usage_quota_url).unwrap_or_else(|| cfg.profile.quota_host.to_string());
        let url = format!("{host}/api/anthropic/v1/models");
        match doctor::fetch_model_ids(&url, &key, std::time::Duration::from_secs(15)) {
            Ok(probe) => {
                let missing: Vec<&String> = probe
                    .ids
                    .iter()
                    .filter(|id| !catalog::is_known_model(id))
                    .collect();
                if missing.is_empty() {
                    println!("live models: none new beyond the bundle");
                } else {
                    println!("live only:");
                    for id in missing {
                        println!("  {id}");
                    }
                }
            }
            Err(e) => eprintln!("glm: live refresh failed ({e}); showing bundled catalog"),
        }
    }
    Ok(0)
}

fn cmd_usage(range: RangeArg, json: bool) -> Result<i32> {
    let cfg = load_config()?;
    let key = keys::resolve_key(None)?;
    let sections = usage::collect(
        &cfg,
        &key.value,
        range.days(),
        std::time::Duration::from_secs(config::USAGE_TIMEOUT_SECS),
    );
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&usage::json_from_sections(&sections))?
        );
    } else {
        println!("{}", usage::format_sections(&sections));
    }
    Ok(0)
}

/// The statusline must never fail: a broken config falls back to defaults,
/// and any other error still prints a usable line. Exit code is always 0 and
/// nothing goes to stderr (Claude Code would show it in the status bar).
fn cmd_statusline() -> Result<i32> {
    let cfg = load_config().or_else(|_| config::resolve(None, &std::collections::BTreeMap::new()));
    let key = match keys::resolve_key(None) {
        Ok(k) => k.value,
        Err(_) => String::new(),
    };
    let ok = match cfg {
        Ok(cfg) => statusline::run(&cfg, &key).is_ok(),
        Err(_) => false,
    };
    if !ok {
        println!("{}", statusline::fallback_line());
    }
    Ok(0)
}

fn cmd_doctor() -> Result<i32> {
    // A broken config file must still let the other checks run; the parse
    // problem is reported as a FAIL check below.
    let cfg_file = config::load_file();
    let config_error = cfg_file.as_ref().err().map(|e| format!("{e:#}"));
    let cfg_file = cfg_file.unwrap_or_default();
    let resolved = config::resolve(Some(&cfg_file), &std::collections::BTreeMap::new())?;
    let key = keys::resolve_key(None);
    let mut ctx = doctor::DoctorContext {
        config_file: &paths::config_file(),
        settings_path: &paths::settings_file(),
        resolved_key: key.as_ref().ok().map(|k| (k.value.clone(), k.source)),
        cfg: &cfg_file,
        base_url: resolved.base_url.clone(),
        model: resolved.model.clone(),
        small_model: resolved.small_model.clone(),
        profile_source: resolved.profile_source,
        fetch_models: Box::new(doctor::fetch_model_ids),
        extra_fail: None,
    };
    if let Some(err) = config_error {
        ctx.extra_fail = Some(err);
    }
    let checks = doctor::run(&ctx);
    for c in &checks {
        println!("{}", doctor::format_check(c));
    }
    if doctor::any_fail(&checks) {
        bail!("doctor found failures (see above)");
    }
    Ok(0)
}

/// Resolve an optional target against the registry and attach. Exits with
/// the agent's status; Err only means setup failed.
fn cmd_attach(target: Option<&str>) -> Result<i32> {
    let entries = registry::load(registry::pid_alive);
    let pid = registry::resolve_target(&entries, target.unwrap_or(""))?.map(|e| e.pid);
    relay::attach(pid)?;
    Ok(1)
}

fn cmd_sessions() -> Result<i32> {
    let entries = registry::load(registry::pid_alive);
    if entries.is_empty() {
        println!("no glm sessions");
        return Ok(0);
    }
    println!(
        "{:<8} {:<6} {:<22} {:<10} cwd",
        "PID", "STATE", "SESSION", "STARTED"
    );
    for e in entries {
        let state = if e.backgrounded { "bg" } else { "live" };
        let started = fmt_relative(e.started_ms);
        let sid = e
            .sid
            .as_deref()
            .map(|s| s.chars().take(8).collect::<String>());
        println!(
            "{:<8} {:<6} {:<22} {:<10} {}",
            e.pid,
            state,
            sid.as_deref().unwrap_or("-"),
            started,
            e.cwd
        );
    }
    println!("\nattach: glm attach [pid]   kill: glm kill [pid]");
    Ok(0)
}

/// "5s ago" / "3m ago" / "2h ago" style, compact for the table.
fn fmt_relative(ms: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let s = ((now - ms).max(0) / 1000) as u64;
    match s {
        0..=59 => format!("{s}s ago"),
        60..=3599 => format!("{}m ago", s / 60),
        3600..=86399 => format!("{}h ago", s / 3600),
        _ => format!("{}d ago", s / 86400),
    }
}

/// Terminate tracked sessions. Default: all of them; a target (exact pid
/// or unique 3+ char prefix of pid/session-id) kills one.
fn cmd_kill(target: Option<&str>) -> Result<i32> {
    let all = registry::load(registry::pid_alive);
    let targets: Vec<_> = match target {
        // resolve_target errors on ambiguity/non-match with a candidate list
        Some(t) => vec![registry::resolve_target(&all, t)?.expect("non-empty target resolves")],
        None => all.iter().collect(),
    };
    if targets.is_empty() {
        println!("no glm sessions to kill");
        return Ok(0);
    }
    for e in targets {
        // The child is a session leader: SIGTERM to it takes claude (and
        // any agents it spawned) down; the relay then exits and cleans up
        // its own registry row.
        unsafe {
            libc::kill(e.child_pid as i32, libc::SIGTERM);
        }
        println!("killed session {} ({})", e.pid, e.cwd);
    }
    Ok(0)
}

fn cmd_self_update(check: bool) -> Result<i32> {
    let release = self_update::fetch_release(std::time::Duration::from_secs(20))?;
    let latest = release.tag_name.clone();
    let ordering = self_update::version_cmp(&latest, self_update::VERSION);
    if check {
        if ordering == std::cmp::Ordering::Greater {
            println!(
                "update available: {} (local {})",
                latest,
                self_update::VERSION
            );
        } else {
            println!("up to date: {} (local {})", latest, self_update::VERSION);
        }
        return Ok(0);
    }
    if ordering != std::cmp::Ordering::Greater {
        println!(
            "already up to date (local {}, latest {})",
            self_update::VERSION,
            latest
        );
        return Ok(0);
    }
    let current = std::env::current_exe()?.canonicalize()?;
    let stem = self_update::asset_stem();
    let (tar, sums) = self_update::pick_assets(&release, &stem)?;
    let sums_body = self_update::download_public(&sums.url)?;
    let sums_text = String::from_utf8_lossy(&sums_body).into_owned();
    let want = self_update::expected_sha256(&sums_text, &format!("{stem}.tar.gz"))
        .ok_or_else(|| anyhow::anyhow!("SHA256SUMS has no entry for {stem}.tar.gz"))?;
    let data = self_update::download_public(&tar.url)?;
    if !self_update::verify_sha256(&data, &want) {
        bail!("sha256 mismatch for {stem}.tar.gz; aborting update");
    }
    let bin = self_update::extract_glm_binary(&data)
        .ok_or_else(|| anyhow::anyhow!("tarball contains no glm binary"))?;
    self_update::install_bytes(&current, &bin)?;
    println!("updated {} -> {latest}", current.display());
    Ok(0)
}

/// `scheme://host[:port]` of a URL, or None if it has no scheme.
fn url_origin(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    let host = rest.split('/').next()?;
    if host.is_empty() {
        return None;
    }
    Some(format!("{scheme}://{host}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn help_and_version_work() {
        assert!(SUBCOMMAND_WORDS.contains(&"doctor"));
    }

    #[test]
    fn effort_config_round_trip() {
        let mut cfg = config::ConfigFile::default();
        assert!(set_config_value(&mut cfg, "effort", "  MAX "));
        assert_eq!(cfg.effort.as_deref(), Some("max"));
        assert_eq!(lookup_config_value(&cfg, "effort").as_deref(), Some("max"));
        assert!(!set_config_value(&mut cfg, "effort", "ultra"));
        // clearing: not a set-able value, but an unset key reads as None
        cfg.effort = None;
        assert_eq!(lookup_config_value(&cfg, "effort"), None);
    }
}
