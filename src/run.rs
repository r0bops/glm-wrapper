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
    // Pin the reasoning effort only when configured; unset keeps Claude
    // Code's picker default (and its per-session ←/→ adjustment) in charge.
    if let Some(effort) = &cfg.effort {
        set.push(("CLAUDE_CODE_EFFORT_LEVEL".to_string(), effort.clone()));
    }
    // Thinking-token budget (ZCode parity: 32000). 0 disables thinking on
    // models that support switching it off.
    if let Some(budget) = cfg.thinking_budget {
        set.push(("MAX_THINKING_TOKENS".to_string(), budget.to_string()));
    }
    EnvPlan {
        set,
        unset: vec!["ANTHROPIC_API_KEY".to_string()],
    }
}

/// The statusline + model-picker settings.json pointing back at this binary.
pub fn settings_body(glm_exe: &Path, cfg: &ResolvedConfig) -> serde_json::Value {
    serde_json::json!({
        "statusLine": {
            "type": "command",
            "command": format!("{} statusline", glm_exe.display()),
        },
        "modelPicker": {
            // Default + our rows only: hides the built-in first-party lineup
            // (whose claude-* ids the GLM endpoint cannot serve), along with
            // gateway-discovered models and ANTHROPIC_CUSTOM_MODEL_OPTION.
            "replaceBuiltInOptions": true,
            "options": picker_options(cfg),
        },
        // claude.ai cloud connectors need a claude.ai login and are never
        // reachable through the GLM endpoint; disabling them here also stops
        // the "ANTHROPIC_API_KEY takes precedence" warning banner. Explicitly
        // configured MCP servers (.mcp.json etc.) are not affected.
        "disableClaudeAiConnectors": true,
        // Keep the Workflow tool available ("ultracode" substance) at any
        // effort level: its availability is entitlement + this switch, not
        // the effort level, and unset would mean "default by plan". With an
        // effort=max pin this yields max + workflows — strictly more than
        // the picker's ultracode entry, which locks the session to xhigh.
        "enableWorkflows": true,
    })
}

/// One glm-installed personal slash command. The marker comment sits at the
/// END of each body: frontmatter must be the first thing in the file or
/// Claude Code ignores the description.
#[derive(Debug, Clone)]
pub struct PersonalCommand {
    /// Slash-command name (file name without .md).
    pub name: &'static str,
    pub body: String,
}

fn marker(name: &str) -> String {
    format!("<!-- glm-managed: /{name} command -->\n")
}

fn command_body(name: &str) -> String {
    let raw = match name {
        // Hand the task to the Workflow tool (kept enabled in settings.json).
        "workflow" => concat!(
            "---\n",
            "description: Author and run a multi-agent Workflow for a task\n",
            "argument-hint: [task description]\n",
            "---\n",
            "\n",
            "## Workflow task\n",
            "\n",
            "$ARGUMENTS\n",
            "\n",
            "Run this task through the Workflow tool (multi-agent fan-out). This\n",
            "command is the user's explicit opt-in for the Workflow tool this turn.\n",
            "\n",
            "1. If the task is ambiguous, ask one clarifying question before writing\n",
            "   anything.\n",
            "2. Load the Workflow tool's script reference first (its guidance requires\n",
            "   it before authoring), then author a workflow script with the script\n",
            "   API: `$.agent.spawn` for budgeted agent fan-out, `$.ui.status` /\n",
            "   `$.ui.log` for progress, `$.ui.ask` for decisions.\n",
            "3. State the plan in one short paragraph — agents, budget, expected\n",
            "   output — then invoke the Workflow tool with the script.\n",
            "4. Summarize the results when the workflow completes.\n",
            "\n",
            "Keep the fan-out budget modest unless the task clearly needs more; all\n",
            "agents run on the configured GLM models and share its quota.\n",
        ),
        // Live plan quota; builtin /usage tracks Anthropic subscriptions,
        // which is meaningless on GLM API billing.
        "quota" => concat!(
            "---\n",
            "description: Show current GLM plan quota and usage\n",
            "allowed-tools: Bash(glm usage:*)\n",
            "---\n",
            "\n",
            "Live GLM quota snapshot:\n",
            "\n",
            "!`glm usage --json`\n",
            "\n",
            "Summarize in 2-3 lines: plan-window usage, 5-hour usage, and anything\n",
            "approaching a limit. If the command failed, say the quota API is\n",
            "unreachable and suggest running `glm doctor` outside the session.\n",
        ),
        // Shadows the builtin /usage (Anthropic subscription limits) with the
        // GLM quota: same data as /quota, under the muscle-memory name.
        "usage" => concat!(
            "---\n",
            "description: Show current GLM plan quota and usage\n",
            "allowed-tools: Bash(glm usage:*)\n",
            "---\n",
            "\n",
            "Live GLM quota snapshot:\n",
            "\n",
            "!`glm usage --json`\n",
            "\n",
            "Summarize in 2-3 lines: plan-window usage, 5-hour usage, and anything\n",
            "approaching a limit. If the command failed, say the quota API is\n",
            "unreachable and suggest running `glm doctor` outside the session.\n",
        ),
        // Catalog + active configuration; builtin /model is the switcher,
        // this gives the in-session overview with effort documentation.
        "glm-models" => concat!(
            "---\n",
            "description: List GLM models, effort levels, and the active configuration\n",
            "allowed-tools: Bash(glm models:*), Bash(glm config:*)\n",
            "---\n",
            "\n",
            "Active configuration:\n",
            "\n",
            "!`glm config get model`\n",
            "!`glm config get effort`\n",
            "\n",
            "Bundled GLM catalog:\n",
            "\n",
            "!`glm models --json`\n",
            "\n",
            "Present a compact table (model, context window, documented effort\n",
            "levels, vision) and mark the active model. Switching happens via the\n",
            "/model picker or `glm config set model <id>` outside the session. If\n",
            "the effort line is empty, mention Claude Code defaults to high while\n",
            "GLM documents max for its 5.2+ models (`glm config set effort max`).\n",
        ),
        // Deep research as a Workflow fan-out: independent sub-questions
        // searched in parallel by agents, synthesized into a cited report.
        "research" => concat!(
            "---\n",
            "description: Deep-research a question with a multi-agent workflow and cited report\n",
            "argument-hint: [research question]\n",
            "allowed-tools: WebSearch, WebFetch, Workflow\n",
            "---\n",
            "\n",
            "## Research question\n",
            "\n",
            "$ARGUMENTS\n",
            "\n",
            "Run this as a deep-research workflow:\n",
            "\n",
            "1. Break the question into 3-5 independent sub-questions (definitions,\n",
            "   state of the art, opposing views, numbers/dates, recent news).\n",
            "2. Load the Workflow tool's script reference, then author a script that\n",
            "   fans out one research agent per sub-question. Each agent uses\n",
            "   WebSearch/WebFetch and returns findings with source URLs and dates.\n",
            "3. Synthesize the agents' findings into a report: key findings first,\n",
            "   then contradictions or uncertainty between sources, then a full\n",
            "   source list with URLs.\n",
            "\n",
            "Keep the fan-out budget modest (3-5 agents) unless the question truly\n",
            "needs more; all agents share the GLM quota. If the question is simple\n",
            "enough for a single pass, say so and answer directly without a\n",
            "workflow.\n",
        ),
        // Kill every tracked glm agent process. The bash injection shows the
        // operator what is running before the kill happens.
        "kill" => concat!(
            "---\n",
            "description: Kill all tracked glm agent processes (this session included)\n",
            "allowed-tools: Bash(glm kill:*), Bash(glm sessions:*)\n",
            "---\n",
            "\n",
            "Currently tracked sessions:\n",
            "\n",
            "!`glm sessions`\n",
            "\n",
            "Now terminate them all with `glm kill`. This includes THIS session:\n",
            "after running it, tell the user every agent process was terminated\n",
            "and do not attempt any further tool calls.\n",
        ),
        // Persistent project goals, tracked in a versionable file so context
        // survives across sessions.
        "goals" => concat!(
            "---\n",
            "description: Track project goals across sessions (.claude/goals.md)\n",
            "argument-hint: [add <goal> | done <n> | doing <n> | list]\n",
            "---\n",
            "\n",
            "Maintain the project goals file at `.claude/goals.md` (create it if\n",
            "missing) with one numbered entry per goal: the goal, a status of\n",
            "`todo` / `doing` / `done`, and the last-updated date.\n",
            "\n",
            "- No arguments: read the file and summarize the goals, their statuses,\n",
            "  and the single most useful next step.\n",
            "- `add <text>`: append a new goal (status `todo`, today's date).\n",
            "- `doing <n>` / `done <n>`: update that goal's status.\n",
            "- Anything else: treat it as goal updates or reflections and apply\n",
            "  them to the file.\n",
            "\n",
            "Finish by printing the updated list compactly. Keep entries short —\n",
            "this file is context, not documentation.\n",
        ),
        other => unreachable!("unknown personal command {other}"),
    };
    format!("{raw}\n{}", marker(name))
}

/// The full suite of glm-installed personal commands.
pub fn personal_commands() -> [PersonalCommand; 7] {
    [
        PersonalCommand {
            name: "workflow",
            body: command_body("workflow"),
        },
        PersonalCommand {
            name: "quota",
            body: command_body("quota"),
        },
        // Shadows the builtin /usage so the muscle-memory name shows GLM data.
        PersonalCommand {
            name: "usage",
            body: command_body("usage"),
        },
        PersonalCommand {
            name: "glm-models",
            body: command_body("glm-models"),
        },
        PersonalCommand {
            name: "research",
            body: command_body("research"),
        },
        PersonalCommand {
            name: "goals",
            body: command_body("goals"),
        },
        PersonalCommand {
            name: "kill",
            body: command_body("kill"),
        },
    ]
}

/// Install the personal slash commands into `dir`. Idempotent: updates
/// files that carry our marker, never touches a foreign file. Returns
/// (name, action) per command.
pub fn install_personal_commands(dir: &Path) -> Result<Vec<(&'static str, &'static str)>> {
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let mut out = Vec::new();
    for cmd in personal_commands() {
        let path = dir.join(format!("{}.md", cmd.name));
        let action = if path.exists() {
            let existing = std::fs::read_to_string(&path)
                .with_context(|| format!("read {}", path.display()))?;
            let marker = marker(cmd.name);
            if existing.contains(&marker) {
                if existing == cmd.body {
                    "unchanged"
                } else {
                    "updated"
                }
            } else {
                // A user's own command with this name: never stomp it.
                "kept user file"
            }
        } else {
            "created"
        };
        if action != "unchanged" && action != "kept user file" {
            crate::util::atomic_write(&path, &cmd.body)
                .with_context(|| format!("write {}", path.display()))?;
        }
        out.push((cmd.name, action));
    }
    Ok(out)
}

/// `13_1072 -> "131K"`, `1_000_000 -> "1M"`; small counts pass through.
fn fmt_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{}M", n / 1_000_000)
    } else if n >= 1_000 {
        format!("{}K", n / 1_000)
    } else {
        format!("{n}")
    }
}

/// Picker row id: 1M-window models carry the `[1m]` suffix so Claude Code
/// engages the 1M window and auto-compact, matching the configured default.
fn picker_row_id(base_id: &str, context_window: u64) -> String {
    if context_window >= 1_000_000 {
        format!("{base_id}[1m]")
    } else {
        base_id.to_string()
    }
}

/// One /model picker row per model in the active profile's bundled catalog.
fn picker_options(cfg: &ResolvedConfig) -> Vec<serde_json::Value> {
    let cat = crate::catalog::catalog();
    let provider = cat
        .providers
        .iter()
        .find(|p| p.id == cfg.profile.id)
        .or_else(|| cat.providers.first());
    let Some(provider) = provider else {
        return Vec::new();
    };
    let default_display = crate::catalog::display_model_id(&cfg.model);
    provider
        .models
        .iter()
        .map(|m| {
            let display = picker_row_id(&m.id, m.context_window);
            let mut desc = format!(
                "{} context, {} max output",
                fmt_tokens(m.context_window),
                fmt_tokens(m.max_output_tokens)
            );
            if m.modalities.iter().any(|x| x == "image") {
                desc.push_str(", vision");
            }
            if display == default_display {
                desc.push_str(" (configured default)");
            }
            serde_json::json!({ "model": display, "description": desc })
        })
        .collect()
}

/// Write settings.json atomically.
pub fn write_settings(glm_exe: &Path, cfg: &ResolvedConfig, settings_path: &Path) -> Result<()> {
    let body = settings_body(glm_exe, cfg);
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

/// Run claude interactively when possible: the pty relay rewrites claude's
/// hardcoded `claude --resume` exit hint to `glm --resume`. Falls back to
/// plain exec for non-interactive runs or if the relay cannot be set up.
pub fn start_claude(args: &[String]) -> Result<()> {
    #[cfg(unix)]
    {
        if crate::relay::eligible(args) {
            match crate::relay::run("claude", args) {
                Ok(()) => unreachable!("relay::run exits on success"),
                Err(setup) => {
                    // Only claude-not-installed is worth surfacing; any other
                    // setup failure falls back to a plain exec silently.
                    if setup.to_string().contains("claude is not installed") {
                        return Err(setup);
                    }
                    eprintln!("glm: interactive relay unavailable ({setup:#}); exec fallback");
                }
            }
        }
    }
    exec_claude(args)
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
            effort: None,
            thinking_budget: None,
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
        let cfg = cfg_with("glm-5.3[1m]", "glm-5.3-flash");
        let body = settings_body(Path::new("/usr/local/bin/glm"), &cfg);
        let command = body["statusLine"]["command"].as_str().unwrap();
        assert_eq!(command, "/usr/local/bin/glm statusline");
        // claude.ai connectors are unreachable through GLM; off, always.
        assert_eq!(body["disableClaudeAiConnectors"], serde_json::json!(true));
        // Workflow tool ("ultracode" substance) stays available at any pin.
        assert_eq!(body["enableWorkflows"], serde_json::json!(true));
    }

    #[test]
    fn env_plan_pins_configured_effort() {
        let mut cfg = cfg_with("glm-5.3[1m]", "glm-5.3-flash");
        assert!(!env_plan(&cfg, "sk")
            .set
            .iter()
            .any(|(k, _)| k == "CLAUDE_CODE_EFFORT_LEVEL"));
        cfg.effort = Some("max".into());
        assert_eq!(
            env_plan(&cfg, "sk")
                .set
                .iter()
                .find(|(k, _)| k == "CLAUDE_CODE_EFFORT_LEVEL")
                .map(|(_, v)| v.as_str()),
            Some("max")
        );
    }

    #[test]
    fn env_plan_pins_thinking_budget() {
        let mut cfg = cfg_with("glm-5.3[1m]", "glm-4.7-flash");
        assert!(!env_plan(&cfg, "sk")
            .set
            .iter()
            .any(|(k, _)| k == "MAX_THINKING_TOKENS"));
        cfg.thinking_budget = Some(32000);
        assert_eq!(
            env_plan(&cfg, "sk")
                .set
                .iter()
                .find(|(k, _)| k == "MAX_THINKING_TOKENS")
                .map(|(_, v)| v.as_str()),
            Some("32000")
        );
        // 0 is meaningful: disables thinking where the model allows it.
        cfg.thinking_budget = Some(0);
        assert_eq!(
            env_plan(&cfg, "sk")
                .set
                .iter()
                .find(|(k, _)| k == "MAX_THINKING_TOKENS")
                .map(|(_, v)| v.as_str()),
            Some("0")
        );
    }

    #[test]
    fn settings_picker_is_glm_only() {
        let cfg = cfg_with("glm-5.3[1m]", "glm-5.3-flash");
        let body = settings_body(Path::new("/usr/local/bin/glm"), &cfg);
        let picker = &body["modelPicker"];
        assert_eq!(picker["replaceBuiltInOptions"], serde_json::json!(true));
        let rows = picker["options"].as_array().unwrap();
        assert!(!rows.is_empty());
        for row in rows {
            let id = row["model"].as_str().unwrap();
            assert!(id.starts_with("glm-"), "non-GLM picker row: {id}");
            assert!(!row["description"].as_str().unwrap().is_empty());
        }
        let ids: Vec<&str> = rows.iter().map(|r| r["model"].as_str().unwrap()).collect();
        // 1M-native models carry the [1m] suffix; smaller ones stay bare.
        assert!(ids.contains(&"glm-5.3[1m]"), "{ids:?}");
        assert!(ids.contains(&"glm-4.6"), "{ids:?}");
        // The configured default row is annotated.
        let default_row = rows.iter().find(|r| r["model"] == "glm-5.3[1m]").unwrap()["description"]
            .as_str()
            .unwrap();
        assert!(default_row.contains("configured default"), "{default_row}");
    }

    #[test]
    fn fmt_tokens_scales() {
        assert_eq!(fmt_tokens(1_000_000), "1M");
        assert_eq!(fmt_tokens(200_000), "200K");
        assert_eq!(fmt_tokens(131_072), "131K");
        assert_eq!(fmt_tokens(500), "500");
    }

    #[test]
    fn personal_commands_lifecycle() {
        let dir = std::env::temp_dir().join(format!("glm-cmds-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();

        // create (also makes the parent dir); every command lands as .md
        let actions = install_personal_commands(&dir).expect("install");
        let names: Vec<&str> = actions.iter().map(|(n, _)| *n).collect();
        assert_eq!(
            names,
            vec![
                "workflow",
                "quota",
                "usage",
                "glm-models",
                "research",
                "goals",
                "kill"
            ]
        );
        assert!(actions.iter().all(|(_, a)| *a == "created"));
        for cmd in personal_commands() {
            let body = std::fs::read_to_string(dir.join(format!("{}.md", cmd.name))).unwrap();
            // frontmatter first (Claude Code ignores a description that is
            // not at the top of the file), marker at the end.
            assert!(body.starts_with("---\n"), "{}", cmd.name);
            assert!(body.contains(marker(cmd.name).trim()), "{}", cmd.name);
            assert_eq!(body, cmd.body);
        }

        // unchanged on re-run when content matches
        let actions = install_personal_commands(&dir).expect("reinstall");
        assert!(actions.iter().all(|(_, a)| *a == "unchanged"));

        // a foreign file with a managed name is never stomped
        let foreign = dir.join("quota.md");
        std::fs::write(&foreign, "# my own quota command\n").unwrap();
        let actions = install_personal_commands(&dir).expect("reinstall");
        assert!(actions.contains(&("quota", "kept user file")));
        assert_eq!(
            std::fs::read_to_string(&foreign).unwrap(),
            "# my own quota command\n"
        );

        // but our own older versions get updated
        for cmd in personal_commands() {
            let path = dir.join(format!("{}.md", cmd.name));
            std::fs::write(&path, format!("{}\nold\n", marker(cmd.name))).unwrap();
        }
        let actions = install_personal_commands(&dir).expect("reinstall");
        assert!(actions.iter().all(|(_, a)| *a == "updated"));
        for cmd in personal_commands() {
            let body = std::fs::read_to_string(dir.join(format!("{}.md", cmd.name))).unwrap();
            assert_eq!(body, cmd.body);
        }

        std::fs::remove_dir_all(&dir).ok();
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
