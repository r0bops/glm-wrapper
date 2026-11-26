# Evaluation brief — `glm` wrapper

**Purpose:** a self-contained briefing for reviewers/agents evaluating this
project. Everything claimed below is either runnable or points at code/tests;
"verified live" means exercised against the real Claude Code 2.1.259 binary
and/or the real Z.ai API from this machine.

## What this is

`glm` is a thin Rust wrapper that runs the **stock Claude Code CLI** against
**Z.ai GLM models**. It never patches or forks Claude Code: it prepares the
environment (auth, base URL, model slots, effort, thinking budget), writes a
curated `settings.json`, runs claude on a **private pty** for interactive
sessions (so exactly one compiled-in string — the `claude --resume` exit
hint — can be rewritten to `glm --resume`), and adds session
management (background/attach/kill) plus a suite of GLM-specific slash
commands. Design rule: everything else passes through byte-for-byte, and any
relay setup failure falls back to plain exec.

## Where to look

| Area | Files |
| --- | --- |
| CLI surface, subcommands | `src/main.rs` |
| Env plan, settings.json, slash commands | `src/run.rs` |
| pty relay: hint rewriter, keep-alive, attach, TUI modes | `src/relay.rs` |
| Session registry (`~/.config/glm/sessions.json`) | `src/registry.rs` |
| Config resolution (effort, thinking_budget, …) | `src/config.rs` |
| Model catalog + effort/thinking metadata | `src/catalog.rs`, `models.json` |
| Statusline (quota %, reset hints, ctx %) | `src/statusline.rs`, `src/quota.rs` |
| Doctor checks | `src/doctor.rs` |
| Usage report (404-tolerant credit sections) | `src/usage.rs` |

## Feature inventory (this cycle)

1. **Reasoning effort**: `effort` config key / `GLM_EFFORT` env →
   `CLAUDE_CODE_EFFORT_LEVEL` (values low|medium|high|xhigh|max; unset =
   Claude Code default). Live-verified: pinned `max` shows "with max effort"
   in the session header.
2. **Thinking budget**: `thinking_budget` / `GLM_THINKING_BUDGET` →
   `MAX_THINKING_TOKENS` (ZCode parity: 32000).
3. **GLM-only /model picker** uses `replaceBuiltInOptions: true` and the
   bundled GLM catalog. Claude Code receives bare model IDs, and a configured
   `[1m]` suffix sets the context and auto-compaction windows. The earlier
   live picker test confirmed the GLM-only lineup.
4. **claude.ai connectors disabled** (`disableClaudeAiConnectors: true`) —
   suppresses the "ANTHROPIC_API_KEY takes precedence" banner.
5. **Workflow tool kept on** (`enableWorkflows: true`): max effort +
   workflows, strictly more than the picker's session-only "ultracode"
   (xhigh + workflows; persists nothing — live-verified).
6. **Side-call discipline**: `small_model` default is non-thinking
   `glm-4.7-flash` (glm-5.3 generation cannot disable thinking).
7. **Doctor guardrails**: thinking-always small_model, effort undocumented
   for model, thinking_budget=0 on thinking-always model, plus the pre-existing
   checks for Claude Code version, key, endpoint, catalog membership, and
   writability of settings and plugin files.
8. **`glm models`** shows documented effort levels per model (GLM-5.2+:
   low/high/max) in text and JSON.
9. **Statusline**: live plan/5h quota with **reset-time hints**
   (`plan 9% 361/4000 ·9/11 | 5h 1% ·22:56`), ctx % against the true window.
   Field-by-field verified against the live API. If a refresh fails, the
   statusline can reuse cached figures for the same endpoint.
10. **`glm usage`**: per-section report; credit endpoints 404 on coding-plan
    accounts → quiet note, not error spam.
11. **Multi-session**: every interactive session registered in
    `~/.config/glm/sessions.json`; `glm sessions` lists them.
12. **Keep-alive** backgrounds a session on terminal close (SIGHUP/EOF/EIO)
    or on a second Ctrl-C within two seconds. Claude Code receives the first
    press. The relay returns control to the shell and keeps the agent,
    a 256 KB output tail, and the attach socket alive. Set
    `GLM_KEEP_ALIVE=0` to disable backgrounding.
13. **`glm attach [tgt]`**: reconnects (tail replay + live stream, winsize
    forwarded, **Ctrl-Q** detaches, real exit code on agent exit). No arg =
    most-recently-*backgrounded* (not last-started). Bare `glm` hints when
    backgrounded sessions exist. TUI modes (mouse/paste/focus/alt-screen)
    are disabled on hand-back and re-enabled on attach — otherwise the shell
    is flooded with SGR mouse-event garbage (a real bug we hit and fixed).
14. **`glm kill [tgt]`** sends SIGTERM to the selected agent process group,
    or to all tracked groups when called without a target. `/glm:kill`
    stops all tracked sessions.
15. **Target resolution**: exact pid, else unique **3+ char prefix** of glm
    pid / agent pid / session id (case-insensitive); ambiguous prefixes
    error with the candidate list.
16. **Session plugin** loads through `--plugin-dir` on `glm` launches and
    provides `/glm:workflow`, `/glm:quota`, `/glm:usage`, `/glm:glm-models`,
    `/glm:research`, `/glm:goals`, and `/glm:kill`. The migration removes
    older global commands carrying glm's ownership marker.
17. **SIGPIPE** reset (`glm models | head` exits cleanly).
18. **README.md** documents all of the above.

## Claims → evidence

| Claim | How to check |
| --- | --- |
| Hint rewrite works, exit codes pass, output unbuffered | `cargo test --test resume_hint_it` (real pty, timing assertions) |
| Background/attach/kill flow, double Ctrl-C shell hand-back, TUI-mode cleanup, prefix kill | `cargo test --test multi_session_it` (real ptys with a Claude Code stub) |
| Rewriter edge cases (split at every byte boundary, LF+CRLF, once-only) | unit tests in `src/relay.rs` |
| Config precedence (env > file > default), effort/budget validation | unit tests in `src/config.rs` |
| Doctor warnings | unit + `cargo test --test doctor_it` |
| Statusline rendering incl. reset hints | unit tests in `src/statusline.rs` |
| Plugin install and legacy command cleanup | unit tests `personal_commands_lifecycle`, `install_plugin_writes_manifest_and_commands`, and `legacy_commands_only_marked_files_removed` |
| Picker is GLM-only | unit test `settings_picker_is_glm_only` |
| Whole suite | `cargo test` → 118 passing (98 unit + 20 integration) |
| Live API wiring | `glm doctor` (Bearer probe), `glm usage`, `glm models` against real endpoint |
| Live TUI behavior (picker rows, /effort screen, command menu, effort header) | pty-driven experiments on Claude Code 2.1.259 |

CI runs `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
and `cargo test` on Linux and macOS.

## Known limitations / open questions (for reviewers)

1. **Version-pinned knowledge.** All Claude Code internals were derived from
   2.1.259 (native ELF). A Claude Code upgrade can silently change any of:
   `modelPicker`, `disableClaudeAiConnectors`, `enableWorkflows`,
   `CLAUDE_CODE_EFFORT_LEVEL`, the compiled `claude --resume` banner. The
   relay degrades gracefully (rewriter just never matches) but the curated
   settings could be ignored by future versions.
2. **The /effort scale cannot be filtered** to GLM's documented levels —
   proven live (even an empty capability whitelist changed the screen). This
   is a recorded dead end, not an oversight. Enforcement relies on Claude's
   per-model clamp/retry plus GLM server defaults.
3. **Attach is single-client** (a newer attach takes over). TUI repaint on
   attach relies on the winsize preamble triggering a claude redraw.
4. **Registry edge cases**: session lookup checks PID liveness and uses
   process command lines when `/proc` is available. PID reuse can still
   produce a false match.
5. **Windows is unsupported** for the interactive relay (print mode execs
   normally).
6. **`/cost` dollar figures** use Anthropic pricing. Z.ai's console reports
   GLM costs, and `/glm:quota` shows plan usage.
7. **Security posture**: key stored 0600 in `~/.config/glm/key`; no secrets
   in settings.json or the registry; attach socket lives in the user's
   config dir (per-user trust domain). No formal security review has been
   done — that would be a useful evaluation angle.
8. **Not done (deliberately)**: `glm config unset` (edit config.toml by hand
   for now), multi-client attach, resume-by-prefix (`glm <sid-prefix>`,
   Claude Code owns `--resume`), packaging beyond install.sh + GitHub
   releases.

## Suggested evaluation angles

- **Correctness pass**: run the suite; then adversarially review
  `src/relay.rs` (fork/poll/signal paths) — it is the riskiest code by far.
- **Live behavioral pass**: `glm init` in a scratch XDG dir, drive the TUI
  under a pty, then test double Ctrl-C →
  `glm sessions` → `glm attach` → `glm kill` end-to-end.
- **Upgrade-resilience pass**: what breaks when Claude Code bumps a minor
  version? Which settings keys degrade loudly vs silently?
- **Docs pass**: README claims vs actual behavior table.
