# AI edit pipelines (`rushes-pipeline`)

A pipeline turns a folder of raw takes into `edit/final.mp4` with a headless coding agent
(Claude Code in print mode) that edits with one of two harnesses (`RunContext::harness`):

- `Harness::Rushes { tools }`: Rushes' own editing tools ([editing-harness.md](editing-harness.md)),
  served over MCP by `tools` (the app's binary: `rushes edit-tools --footage <dir>`). The
  agent cuts and renders only through them; its shell and web tools are for finding,
  fetching and making material. The default.
- `Harness::VideoUse { skill_dir }`: the [video-use](https://github.com/browser-use/video-use)
  skill, a prompt plus Python helpers the agent runs through its shell.

The crate also has the agent write a video's YouTube details and Codex draw its thumbnails
(see [YouTube details](#youtube-details) and [Thumbnails](#thumbnails)). It is synchronous
and UI-free: the app calls it on a background thread with an event callback and a
cancellation flag.

## Phases

A run keeps the harness it started with (`PipelineRun::harness`, set from Settings when the
run is created): its follow-up phases resume an agent session that knows those tools, so
changing the setting affects only new edits. Runs stored before the harness was recorded
take the setting at their next phase and keep it from then on.

Both harnesses have the same rule: never touch the cut before the creator approved the
plain-English strategy. A headless agent cannot wait for a reply, so the
conversation is split into phases. Each phase is one agent process; follow-up phases
resume the same agent session (`--resume <session_id>`), so transcripts and context carry
over.

| Phase | Agent is asked to | Run status in the app |
| --- | --- | --- |
| `Plan { brief }` | inventory, transcribe, propose a strategy, append it to `edit/project.md`, then **stop without rendering**. The final message is the strategy only. | `Planning -> AwaitingApproval` |
| `Approve { notes }` | render `edit/preview.mp4` and `edit/final.mp4`, self-evaluate (max 3 passes), summarise | `Rendering -> Succeeded` |
| `Feedback { message }` | apply the feedback (to the strategy if nothing was rendered yet), re-render, summarise | `Rendering -> Succeeded` |
| `Auto { brief }` | everything in one pass; the creator pre-approved the strategy | `Rendering -> Succeeded` |

A phase that fails, is cancelled or is interrupted (Rushes closed) leaves what an
earlier phase made on offer: a revised strategy that does not finish leaves the
strategy before it awaiting approval, and a change to a rendered edit leaves the run
`Succeeded` with its cut. The failure is kept as the run's error and shown next to the
strategy or the cut, with a retry; only a run with neither ends `Failed` or
`Cancelled`. The app's "Agent time" is the sum of the phases (the agent's reported
`duration_ms`, or a phase's first to last event), so waiting for approval does not count.

Each harness has its own prompts (`build_rushes_prompt`, `build_prompt`) with the same
phase markers. `Plan` and `Auto` prompts (for video-use, with the absolute `SKILL.md` path) list the footage with
duration and frame size (when `ffprobe` is available; the takes are the files with a
`FOOTAGE_EXTENSIONS` extension, the same list the app's footage card shows, which a
test keeps in agreement), include the creator's brief and
tell the agent that it runs headless: it must make sensible choices instead of asking,
and put open questions at the end of the strategy. All outputs go to `<footage>/edit/`.
`Approve`/`Feedback` without a session id still work: they point the agent at
`edit/project.md`, where the strategy was recorded.

```rust
use std::sync::atomic::AtomicBool;
use rushes_pipeline::{AgentConfig, Harness, Phase, RunContext, ToolServer, run_phase};

let config = AgentConfig::default();
let cancel = AtomicBool::new(false);
let tools = ToolServer { program: std::env::current_exe()?, args: vec!["edit-tools".into()] };
let mut ctx = RunContext { footage_dir, harness: Harness::Rushes { tools }, session_id: None };

let plan = run_phase(&config, &ctx, Phase::Plan { brief }, &mut |event| { /* timeline */ }, &cancel)?;
// show plan.summary_text (the strategy); wait for the creator
ctx.session_id = plan.session_id;
let render = run_phase(&config, &ctx, Phase::Approve { notes: None }, &mut |_| {}, &cancel)?;
// render.final_cut == Some(<footage>/edit/final.mp4)
```

`PhaseOutcome` carries the session id for the next phase, the agent's final message
(`summary_text`), `cost_usd`, `is_error` (agent finished but reported an error, e.g. turn
limit), `final_cut` / `preview` when those files exist after the phase, and `rendered`
(final cut created or rewritten during this phase; a failed re-render leaves the old one).

### Kept cuts

Every render writes the same `edit/final.mp4`, so a later render (an iteration, or a new
edit of the same footage) would overwrite a cut that an earlier edit or the upload form
refers to. `keep_final_cut(footage_dir)` moves the fresh `edit/final.mp4` to
`edit/cuts/final-<n>.mp4` (numbers only count up) and returns that path; `kept_cuts`
lists them, newest first. The app keeps the cut of every render this way and stores
the kept path as the run's output, also when the agent wrote `edit/final.mp4` and then
ended with an error (the run is done, with "Rendered, but the agent stopped with an
error" as its error). The run's log gets a `cut_kept` marker naming the file (the
activity log shows "Cut kept as final-2.mp4"), so an earlier render of the edit stays
known after the next one replaces the output. The agent's summary is saved naming the
kept file instead of `edit/final.mp4`.

A render that does not finish (cancelled, failed or interrupted without a new cut) may
leave half a file at `edit/final.mp4`. `set_aside_final_cut(footage_dir)` moves it to
`edit/leftovers/final-<n>.mp4`, where `kept_cuts` doesn't look: the app does so when
the phase ends, and before a render starts it sets aside anything at the shared path
that no run and no upload form refers to. A cut that one does refer to (an earlier
version kept cuts there, keeping one may have failed) moves into `edit/cuts/` and they
follow it. Either way nothing is overwritten; when the move fails the render does not
start. The Publish tab offers as "AI edit" only the kept cuts that are a run's output,
named in a run's log, or the file the upload form already has. `edit/preview.mp4`,
`edit/project.md` and the transcripts stay shared: they are the skill's working state
for the footage.

## Agent invocation

```text
# Rushes' tools
claude -p <prompt> --output-format stream-json --verbose [--resume=<id>] [--model <m>]
       --permission-mode acceptEdits --tools=Bash,Read,Write,Edit,Glob,Grep,TodoWrite,WebFetch,WebSearch
       --allowedTools=mcp__rushes,Bash,Read,Write,Edit,Glob,Grep,TodoWrite,WebFetch,WebSearch
       --mcp-config=<rushes server JSON> --strict-mcp-config
       --append-system-prompt=<rushes-edit instructions>[\n\n<text>] [--max-turns <n>] [extra args]
       (environment: MCP_TOOL_TIMEOUT=3600000)

# video-use
claude -p <prompt> --output-format stream-json --verbose [--resume=<id>] [--model <m>]
       --permission-mode acceptEdits
       --allowedTools=Bash,Read,Write,Edit,Glob,Grep,Skill,Agent,TodoWrite
       --add-dir=<skill dir> [--append-system-prompt=<text>] [--max-turns <n>] [extra args]
```

- Working directory is the footage folder; stdin is closed; stdout is parsed line by line;
  stderr is streamed as `Stderr` events and its last 20 lines are kept for errors.
- Nobody can answer a permission prompt in print mode, so anything not allowed is denied
  (reported as a `PermissionDenied` event). With Rushes' tools the agent has the `rushes` MCP
  server, the shell, the file tools and the web (`--tools` leaves out the rest,
  `--strict-mcp-config` ignores the creator's other MCP servers). `acceptEdits` plus the
  allowed tools above is what video-use needs: shell for ffmpeg and its Python helpers, file tools, the skill
  loader, sub-agents for animations. `bypassPermissions` is never used implicitly; set
  `permission_mode` explicitly if you want it.
- `--add-dir` gets the symlink-resolved skill folder so the agent may read the helpers.
- The prompt directly follows `-p`, and variadic flags use the `--flag=value` form, so no
  option can swallow the prompt. `--resume=<id>` uses it too, so an id can never be read
  as an option. A blank `RunContext::session_id` starts a fresh session.
- The agent runs in its own process group. Cancelling sends `SIGTERM` to the group (the
  agent and every ffmpeg/python child), waits `kill_grace` (default 5 s), then `SIGKILL`s
  whatever is left. The same happens to processes the agent leaves running when it exits,
  and when `run_phase` unwinds (a panicking event callback). On non-Unix platforms only
  the agent process is killed.
- On Linux the group also dies with the app, even when the app is killed: its leader is
  a small `/bin/sh` script that starts the agent, gets `SIGTERM` when the app's thread
  running the phase ends (`PR_SET_PDEATHSIG`, set before it runs) and passes it on to
  the whole group, and waits for the agent so the group keeps its leader while the
  agent lives. `run_phase` reports the group first, as a `Spawned` event
  (`AgentProcess`: leader pid, boot id, start time); the app stores it with the run's
  events. When an earlier session crashed while an agent worked, startup calls
  `stop_leftover_agent`, which stops the group (`SIGTERM`, `SIGKILL` after a grace
  period) only while its leader is still that process (same boot, same start time),
  before it marks the run interrupted.
- A prompt may come on stdin instead of after `-p` (`Invocation::prompt_on_stdin`), with
  no limit on its length: the details prompt carries transcripts. The group leader hands
  its own stdin to the agent through descriptor 3, since `sh` gives a background command
  `/dev/null`; a thread writes the prompt and closes the pipe.

### Events

Each stdout line becomes zero or more `AgentEvent`s (`serde`, tagged by `"type"`, so the
app can persist them as run events):

| Event | From |
| --- | --- |
| `Spawned { process }` | this crate, when the agent starts (before anything it prints) |
| `Init { session_id, model, cwd, tools }` | `system/init` |
| `AssistantText { text }`, `Thinking { text }` | assistant content blocks (redacted thinking is skipped) |
| `ToolUse { id, name, summary }` | `tool_use` blocks; summary = Bash command's first line, file path, skill name, sub-agent description, ... |
| `ToolResult { tool_use_id, is_error, summary }` | `tool_result` blocks; first 300 characters |
| `PermissionDenied { tool_name, message }` | `system/permission_denied` |
| `Result { is_error, result_text, session_id, cost_usd, duration_ms, num_turns }` | `result` (`result`, or `errors` for failed resumes) |
| `Stderr { line }` | stderr |
| `Unknown { kind, excerpt }` | anything else: new event types, new block types, non-JSON lines |

Bookkeeping lines (`rate_limit_event`, `tool_progress` heartbeats of long tool calls,
`system/thinking_tokens`, injected skill text) yield nothing. Calls of Rushes' tools
(`mcp__rushes__<tool>`) get short summaries (`3 segments`, `take A 390.0-522.0 s`, `final`). Parsing never fails a run.

### Errors

`PipelineError`: `AgentNotFound`, `SkillNotFound`, `FootageNotFound` (checked before
spawning), `Spawn`, `Cancelled`, `AgentFailed { message, stderr_tail, session_id, no_result }`
(non-zero exit or no result event; the session id allows a retry with `--resume`;
`no_result` when the agent never reported a result, which usually means the command is not
Claude Code or could not start), `Io`.

## YouTube details

`write_details(&config, cwd, &DetailsRequest, on_event, cancel)` has the edit agent write
a video's title, description and tags in one headless turn and returns `VideoDetails`:

```text
claude -p --output-format stream-json --verbose [--model <m>] --permission-mode acceptEdits
       --tools= --json-schema=<DETAILS_SCHEMA> --no-session-persistence --strict-mcp-config
       < prompt
```

- No tools and none of the creator's MCP servers: everything is in the prompt, on stdin.
  The answer is the `result` text, JSON matching `DETAILS_SCHEMA`; `parse_details` also
  takes JSON wrapped in prose or a code fence, and anything else is an `AgentFailed`.
- The prompt (`details_prompt`) has YouTube's limits and the house rules (language of the
  script, say only what the video says, no Markdown, no chapter list of its own, keep the
  current description's chapter list, links and standing text, no `<` or `>`), the
  creator's instructions first, what the form holds now, the script as Markdown (first
  60,000 characters), the transcript and the edit notes.
- `read_transcript(footage_dir)` is video-use's `edit/takes_packed.md`, or else the `text`
  of each `edit/transcripts/*.json` under its take's name (both harnesses keep Scribe's
  JSON there; the Rushes tools' `.rushes.json` cache records have no `text` and are skipped) (first 150,000 characters);
  every take is in it, retakes included, which the prompt says. `has_transcript` only
  checks. `read_edit_notes` is the end of `edit/project.md` (12,000 characters): the
  strategy and what made the cut.
- The app checks the answer against YouTube's limits again (`publish::rules::fit_details`).

## Thumbnails

`generate_thumbnails(&CodexConfig, &ThumbnailRequest, on_event, cancel)` has Codex draw
`count` (1–4) thumbnails with its image generation and puts them in `request.dir` as
`thumbnail-<n>.jpg`, numbered after the ones there (`list_thumbnails` lists them, newest
first):

```text
codex exec --json --skip-git-repo-check --sandbox=workspace-write --cd=<thumbnails dir>
           [--model=<m>] --config=model_reasoning_effort="medium" [--image=<frame>]... < prompt
```

- Codex works in the thumbnails folder and may write only there. The prompt
  (`thumbnail_prompt`) names the video, the rules of a thumbnail that works (16:9, one
  subject, at most four words of text), the creator's ideas, the start of the script, and
  the draft paths: Codex copies each image its tool drew to the next one,
  `.drafts/draft-<n>.png`. Reasoning effort is medium: drawing needs little, and the
  creator's own default may be the slowest.
- With `frames_from`, three frames of that video (at a quarter, half and three quarters,
  ffmpeg) are attached with `--image`, so its people and set stay recognisable.
- Afterwards ffmpeg scales and crops each draft to 1280 × 720 and saves it as a JPG
  (`-q:v 3`, well under YouTube's 2 MB); without ffmpeg the draft is kept as it is. The
  `.drafts` folder goes either way. Cancelling stops Codex's process group and keeps
  nothing.
- Codex's `--json` lines become the same `AgentEvent`s (`parse_codex_line`): the thread
  is `Init`, messages `AssistantText`, shell commands `ToolUse`/`ToolResult`,
  `turn.completed`/`turn.failed` the `Result`, `error` lines `Stderr`. Drawing fewer than
  asked returns what was drawn with the reason (`ThumbnailOutcome::problem`); drawing
  none is an `AgentFailed`.

## Requirements

`doctor_rushes(&config)` (Rushes' tools) checks the agent, ffmpeg, ffprobe and an
ElevenLabs key in the agent's environment; `Doctor::skill` and `Doctor::python` are `None`.
`doctor(&config, skill_dir)` (video-use) checks everything below. Each check has a
plain-text detail for people (version and path, or what is missing); `Doctor::ready()` is
true when all present checks pass. Tools are looked up on the agent's `PATH` (`config.env`
first). The app passes the ElevenLabs key saved in Settings (`<data dir>/elevenlabs-key`,
mode 0600) in the agent's environment; with Rushes' tools and no saved key it uses the one
in video-use's `.env`.

- **Agent**: Claude Code CLI; `--version` must print `Claude Code` (any other program
  answers `--version` too, but cannot run an edit). Login is not checked.
- **Skill**: `SKILL.md` in the configured folder, default `~/.claude/skills/video-use`
  (symlinks resolved; the resolved folder is returned as `Doctor::skill_dir`).
- **ffmpeg** and **ffprobe** on `PATH`.
- **ElevenLabs key** for transcription, looked up the way the skill's helpers do: an
  `ELEVENLABS_API_KEY=` line in `<skill>/.env` wins (an empty one fails the check even
  when the environment has the key; `export` lines are not understood), otherwise
  `ELEVENLABS_API_KEY` in the environment. The value is never read out, logged or
  returned, and `AgentConfig`'s `Debug` output shows only the names of `env` entries.
- **Python**: the skill's `.venv`, else `uv`, else `python3` (dependencies unverified).

An app started from a desktop launcher (Finder, a `.desktop` file) often gets a minimal
`PATH` without `~/.local/bin` or `/opt/homebrew/bin`. The Rushes app appends the usual
install folders that exist (Homebrew, `/usr/local/bin`, `~/.local/bin`, `~/.claude/local`,
`~/.npm-global/bin`, `~/.cargo/bin`, `~/bin`) to its own `PATH` at startup
(`shell::extend_tool_path`), and the agent's shell inherits it. Tools elsewhere need
`command` set to the agent's full path, or a complete `PATH` in `AgentConfig::env`.

## Configuration

`AgentConfig` fields: `command` (default `claude`, a name on `PATH` or a path), `model`,
`permission_mode` (default `acceptEdits`), `allowed_tools`, `append_system_prompt` (the
creator's standing instructions), `max_turns`, `extra_args`, `env` (also passed to every
tool the agent runs), `kill_grace`. The app maps its pipeline settings (agent command,
skill folder, extra instructions, auto-approve) onto `AgentConfig`, `RunContext` and the
choice between `Plan` and `Auto`.

## Testing

```sh
cargo test -p rushes-pipeline
cargo clippy -p rushes-pipeline --all-targets
```

- `tests/fixtures/*.jsonl` are real Claude Code 2.1 `stream-json` captures (plain reply,
  Bash + Read tool calls, resume, permission denial, Skill load, unknown model, unknown
  session), sanitised. `tests/fixtures.rs` checks that every line parses to a known event.
  To refresh them, run `claude -p "<prompt>" --output-format stream-json --verbose
  --model haiku` in a scratch folder and strip personal paths.
- `scripts/fake-agent` (python3) accepts the same flags and prints the same kind of
  stream: a plan prompt writes `edit/project.md` and replies with a strategy; render,
  revision and single-pass prompts build `edit/final.mp4` and `edit/preview.mp4` with
  ffmpeg from the footage in the working directory (or a 3 s test pattern), `--resume`
  keeps the session id. Point `AgentConfig::command` at it. Environment switches:
  - `FAKE_AGENT_MODE=fail`: stderr lines, an error result, exit 1;
  - `FAKE_AGENT_DELAY_MS=<ms>`: wait between events in a child `sleep` process;
  - `FAKE_AGENT_IGNORE_TERM=1`: ignore `SIGTERM` (tests the `SIGKILL` fallback);
  - `FAKE_AGENT_LEAVE_CHILD=1`: leave a `sleep` running after the agent exits;
  - `FAKE_AGENT_PID_FILE=<path>`, `FAKE_AGENT_ARGV_FILE=<path>`: record pids / argv.
- `tests/pipeline.rs` runs Plan -> Approve -> Feedback and Auto on generated footage
  (ffmpeg `testsrc` + `sine`), checks session continuity and that `final.mp4` is a valid
  video, the failure path, that cancelling kills the whole process tree, and that the
  agent and its child die when the app is killed with `SIGKILL` (Linux; the test binary
  runs itself as the app).
- `tests/doctor.rs` runs `doctor()` against a fake `PATH` of stub scripts.
- `scripts/fake-agent` answers a details prompt (`--json-schema`, prompt on stdin) with a
  title, description and tags made from it, keeping the current description's links;
  `FAKE_AGENT_DETAILS=<text>` replaces the answer, `FAKE_AGENT_PROMPT_FILE=<path>` records
  the prompt.
- `scripts/fake-codex` (python3) reads the prompt from stdin, saves a test pattern PNG at
  every draft path it names and prints Codex-shaped `--json` events. Switches:
  `FAKE_CODEX_MODE=fail` (a usage limit), `FAKE_CODEX_DRAW=<n>` (draw only the first n,
  then fail), `FAKE_CODEX_DELAY_MS=<ms>`, `FAKE_CODEX_ARGV_FILE`, `FAKE_CODEX_PROMPT_FILE`.
- `tests/publish.rs` writes details from a prompt larger than an argument may be, refuses
  answers that aren't details, and draws thumbnails with frames of a clip, checking they
  are 1280 × 720 JPGs numbered after the existing ones; also a partial run and
  cancelling.

The tests never call a model. A real video-use run costs model and ElevenLabs credits;
try it on short footage (`scripts/make-test-footage.sh <dir>`).
