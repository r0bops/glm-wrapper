# glm — Claude Code on Z.ai GLM, from the CLI

`glm` runs [Claude Code](https://docs.anthropic.com/en/docs/claude-code) against
Z.ai / Zhipu GLM models with the right environment, settings, and guardrails —
the GLM-coding-plan experience as a plain CLI. No fork of Claude Code, no
patching: a small Rust binary that prepares the world and hands over.

```text
$ glm
GLM|glm-5.3[1m]  plan 8% 357/4000  |  5h 1%  |  ctx 0%
```

## How it works

For every interactive run, glm:

1. loads `~/.config/glm/config.toml` (defaults when missing) and resolves the
   active profile, models, and knobs,
2. exports the auth and model environment (`ANTHROPIC_BASE_URL`,
   `ANTHROPIC_AUTH_TOKEN`, `ANTHROPIC_MODEL`, per-slot models, max output
   tokens, auto-compact window, effort, thinking budget) and strips
   `ANTHROPIC_API_KEY` (Z.ai requires Bearer auth; the two are mutually
   exclusive),
3. regenerates `~/.config/glm/settings.json`, which curates Claude Code to
   GLM: a GLM-only `/model` picker (first-party Anthropic models like Fable
   are hidden — those ids don't exist on the GLM endpoint), the glm statusline,
   claude.ai cloud connectors disabled (unreachable through GLM, and their
   absence kills the "API key takes precedence" warning banner), and the
   Workflow tool kept available at any effort level — with an `effort` pin of
   `max` that means max effort + workflows, strictly more than the picker's
   `ultracode` entry (which locks the session to xhigh),
4. ships a suite of slash commands as a session-only plugin
   (`~/.config/glm/plugin/`, passed via `--plugin-dir`): they exist in glm
   sessions only and never show up in a plain `claude` run. Plugin commands
   are namespaced, so type `/glm:` to see them:
   - `/glm:workflow <task>` — run a task through the Workflow tool
     (multi-agent fan-out; scripts use `$.agent.spawn` and `$.ui.*`);
   - `/glm:quota` — live GLM plan/5-hour quota, summarized in-session
     (the built-in `/usage` tracks Anthropic subscriptions, not GLM);
   - `/glm:usage` — same data as `/glm:quota`, under the muscle-memory name;
   - `/glm:glm-models` — the bundled catalog with documented effort levels,
     marking your active model and configuration;
   - `/glm:research <question>` — deep-research via a Workflow fan-out: one
     agent per sub-question searching the web in parallel, synthesized into
     a cited report;
   - `/glm:goals` — track project goals across sessions in a versionable
     `.claude/goals.md` (`add` / `doing <n>` / `done <n>` / list);
   - `/glm:kill` — terminate every tracked glm agent process (this session
     included).
   The built-in `/workflows` (browse runs) and `/workflow-authoring`
   (script reference) round out the workflow surface,
5. runs claude on a private pty and relays bytes, rewriting exactly one thing:
   claude's hardcoded exit hint `claude --resume <id>` becomes
   `glm --resume <id>` — following the original verbatim would bypass glm and
   resume against your Anthropic login.

Non-interactive runs (`-p`/`--print`) exec claude directly with the same
environment. If the relay cannot start, glm falls back to a plain exec; set
`GLM_RELAY=0` to force that path at any time.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/r0bops/glm-wrapper/main/install.sh | sh
```

or build from source:

```sh
cargo install --path .
```

Claude Code itself must be installed (`npm install -g @anthropic-ai/claude-code`).

## Quick start

```sh
glm init          # write default config, prompt for the API key, run doctor
glm               # interactive session on GLM
glm -p "hi"       # print mode (plain exec)
```

## Commands

| Command | Purpose |
| --- | --- |
| `glm [claude args…]` | run claude with GLM models (`glm -- word` forces claude) |
| `glm init` | first-run setup: config, key prompt, doctor |
| `glm config get\|set <key> <value>` | read/write config (`config path` prints the file) |
| `glm key set [--stdin]` / `glm key path` | store the API key |
| `glm models [--refresh] [--json]` | bundled catalog; `--refresh` diffs live ids |
| `glm usage [--range 7d\|30d] [--json]` | quota/credit report |
| `glm statusline` | the Claude Code statusline (reads stdin) |
| `glm doctor` | environment checks |
| `glm sessions` | list glm sessions (live and backgrounded) |
| `glm attach [tgt]` | reconnect to a backgrounded session (Ctrl-Q detaches) |
| `glm kill [tgt]` | terminate tracked agent processes (default: all) |
| `glm self-update [--check]` | update from GitHub releases |
| `glm help` / `glm version` | help / version |

## Configuration (`~/.config/glm/config.toml`)

| Key | Default | Meaning |
| --- | --- | --- |
| `profile` | `zai-coding-plan` | `zai`, `zai-coding-plan`, `bigmodel`, `bigmodel-coding-plan` |
| `model` | `glm-5.3[1m]` | main-loop model. `[1m]` configures the 1M context and auto-compaction windows. GLM receives the bare model ID |
| `small_model` | `glm-4.7-flash` | side calls (titles/summaries). Kept off the thinking-always 5.3 tier on purpose |
| `base_url` | profile default | Anthropic-compatible API base |
| `usage_quota_url` | profile default | quota monitor endpoint |
| `api_timeout_ms` | `3000000` | `API_TIMEOUT_MS` |
| `effort` | *(unset)* | pin reasoning effort: `low\|medium\|high\|xhigh\|max` → `CLAUDE_CODE_EFFORT_LEVEL`. Unset keeps Claude Code's picker in charge |
| `thinking_budget` | *(unset)* | thinking-token budget → `MAX_THINKING_TOKENS` (ZCode parity: 32000; `0` disables where the model allows) |
| `[statusline]` | `cache_ttl_secs = 90`, `timeout_secs = 5` | statusline caching |

Every key also works as `GLM_<KEY>` in the environment (e.g. `GLM_MODEL`,
`GLM_EFFORT`), which beats the file.

### Model and effort notes

- `glm models` marks each model's documented effort levels (GLM-5.2+ accept
  `low/high/max`; older tiers have no effort parameter). Picking an
  undocumented value in Claude Code's picker degrades gracefully — Claude Code
  clamps per model and retries without effort — but `glm doctor` warns when
  your configured effort isn't documented for your model.
- The `/model` picker shows only GLM rows from the bundled catalog with
  context/effort/vision descriptions; the built-in Anthropic lineup is hidden.
- `glm-5.3` and `glm-5.3-flash` have thinking permanently on (docs.z.ai), so
  the wrapper keeps them off the side-call slot by default; `glm doctor`
  flags a `small_model` that always thinks.

## Multiple sessions and keep-alive

Every interactive `glm` session is registered in
`~/.config/glm/sessions.json` (pid, Claude session id, cwd, state).

- **Close the terminal or press Ctrl-C twice within two seconds** to send the
  session to the background. Claude Code receives the first Ctrl-C. The
  second returns control to your shell while the session keeps running and
  buffering output. Backgrounded sessions show as `bg` in `glm sessions`.
  Reconnect with `glm attach` to replay recent output and stream new output.
  Press **Ctrl-Q** to detach again. Set `GLM_KEEP_ALIVE=0` to disable
  backgrounding.
- **`glm kill`** terminates all tracked agent process groups. Use
  `glm kill <tgt>` to stop one session or `/glm:kill` to stop all of them.
- A target is a pid, or a **prefix**: the first 3+ characters of a pid or
  session id, as long as they match exactly one session. `glm kill 186`
  works when session 1861 is the only `186…`; if three digits are ambiguous
  the error lists the candidates so you can add the fourth.
- Bare `glm` reminds you when backgrounded sessions exist, and `glm attach`
  with no arguments resumes the **last-backgrounded** one (not the
  last-started).

## Development

```sh
cargo test        # unit + integration (HTTP stub, real-pty relay tests)
cargo clippy --all-targets
```

The pty relay tests spawn a real pseudo-terminal; they run wherever Rust's
libc bindings do (Unix).

## License

MIT — see [LICENSE](LICENSE). This project talks to the GLM API but is not
affiliated with Z.ai / Zhipu; Claude Code is a product of Anthropic.
