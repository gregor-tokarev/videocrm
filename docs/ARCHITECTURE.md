# Rushes architecture

Desktop app (Rust, [GPUI Kit](https://gpui-kit.com) 0.7.1) for planning, scripting,
AI-editing and uploading YouTube videos. Everything is local: one SQLite database plus
a media folder per video.

## Workspace layout

```text
crates/
  core/       rushes-core      domain model, SQLite store, documents, templates, media folders
  youtube/    rushes-youtube   Google OAuth (loopback + PKCE), YouTube Data API v3 client, resumable upload
  edit/       rushes-edit      Rushes' editing tools (transcribe, cut plans, render, checks), served to the agent over MCP
  pipeline/   rushes-pipeline  AI edit pipelines: drives a coding agent (Claude Code) with rushes-edit or the video-use skill;
                               writes YouTube details with it and draws thumbnails with Codex
  update/     rushes-update    self-update: newest GitHub release, verified download, install in place, relaunch
  app/        rushes           GPUI Kit desktop app (the only crate that depends on gpui-kit)
emulators/    local API emulation for dev/tests: vercel-labs/emulate (Google OAuth) + custom YouTube emulator
scripts/      dev helpers (run emulators, fake agent for pipeline tests, Xvfb screenshots)
docs/         technical docs (README.md stays product-only)
```

Dependency direction: `app -> core, youtube, pipeline, edit, update` and `pipeline -> edit`.
`youtube`, `pipeline`, `edit` and `update` do NOT depend on `core`; they expose their own small
types and the app maps between them. This keeps them independently testable and lets them be
built in parallel.

## Threading model

`core`, `youtube`, `pipeline` and `edit` are synchronous, blocking libraries (std threads, `ureq`
for HTTP, `std::process` for child processes). They never require an async runtime.
The app runs them on GPUI's background executor (`cx.background_spawn`) and streams
progress back to the UI through channels (`smol::channel` / `futures::channel::mpsc`),
consumed by a foreground task (`cx.spawn`) that updates entities and calls `cx.notify()`.
Long operations take a cancellation flag (`Arc<AtomicBool>`) and a progress callback.

### Database access

Another program (a second copy of Rushes, a backup or sync tool, `sqlite3`) may hold the
database's write lock for seconds. The UI thread never waits for it:

- The database is in WAL mode and the app opens three connections to it: the UI
  thread's, the background writer's and a spare one for code outside the write queue
  (`AppState::store()`, used by tests; it waits up to 5 s for a lock, so never on the
  UI thread). WAL readers never wait for a writer, so reading rows, documents, runs and
  uploads on the UI thread stays instant during a lock.
- Every change goes through `AppState` (`state/writes.rs`): videos, properties, views,
  settings, documents, new, copied and deleted videos. While nothing is queued a change
  is written at once on the UI connection, which gives up on a lock after 100 ms.
  Otherwise it joins one ordered write queue: the cached rows show it at once (reloads
  replay the queue over what they read), and a background task writes the queue in order
  on the writer's connection, retrying every 2 s until it lands. The queue keeps only the
  newest value per video, document, view and the settings; writes whose effect depends on
  order (creating, deleting, property definitions) are never overtaken. A busy database
  ends a pass. Another failure (a full disk) holds back what depends on the failing
  write: the later writes of a video it creates or deletes, of a run or upload it
  inserts, or every later write after a property definition.
- A write that keeps failing is never dropped: the video page shows "Not saved —
  retrying" and the creator is told once. Only writes that can never succeed (the video
  was deleted) leave the queue, with a notification.
- Quitting (Ctrl+Q, closing the window, SIGTERM/SIGINT/SIGHUP) writes the queue on the
  UI thread, waiting at most 5 s in all for a lock. What still can't be written is kept
  in `unsaved-changes.json` next to the database (mode 0600) and written at the next
  start, before the settings are read (kept settings that still can't be written are
  used as queued), which says so.
- The background services' records take the same queue (`state/records.rs`): AI
  edit runs, every entry of their logs (numbered by the pipeline service), upload jobs
  and their progress, kept brief forms. The services keep them in memory, which is
  what the UI shows; the queue keeps the newest state per run and upload and every
  log entry, in order after the run's insert. A running edit or upload therefore
  never makes the UI wait for another program's lock. The rows' latest run and
  upload status follow once those writes land.

## Data locations

- Database and settings: `dirs::data_dir()/rushes/` (`rushes.db`, `youtube-accounts/`).
  Overridable with env `RUSHES_DATA_DIR` (used by tests and screenshots).
  The running app holds an exclusive lock on `rushes.lock` there; a second copy on the
  same data dir shows "Rushes is already open" and quits without opening the database.
  `unsaved-changes.json` appears there only when the app quit while another program
  kept the database locked; the next start writes it to the database and removes it.
- Media root (default `dirs::video_dir()/Rushes`, fallback `~/Videos/Rushes`):
  one folder per video `<slug>-<short id>/` containing `footage/` (raw takes).
  AI edits write all outputs to `footage/edit/` (`final.mp4`, `project.md`, `cut.json`, ...;
  caches under `edit/.cache/`). Thumbnails Codex draws go to `thumbnails/`
  (`thumbnail-<n>.jpg`) beside `footage/`.
- ElevenLabs key for AI edits: `elevenlabs-key` in the data folder, mode 0600, never in the
  database.
- OAuth credentials: one file per signed-in YouTube account in `youtube-accounts/`, file
  mode 0600. Never logged. A `credentials.json` left by a release that kept one account
  moves there at the next start.
- From VideoCRM (the app's name before the rename): the first start moves
  `dirs::data_dir()/videocrm/` to `rushes/`, while `rushes/` doesn't exist yet and
  `RUSHES_DATA_DIR` isn't set (`shell::adopt_legacy_data`). It holds VideoCRM's lock
  (`videocrm.lock`) while it renames `videocrm.db` (`-wal` and `-shm` first) to
  `rushes.db`, then the folder; a running VideoCRM gets "Quit VideoCRM to open Rushes".
  Media isn't moved, since videos keep absolute folder paths: a creator who never chose a
  media root gets VideoCRM's default, `dirs::video_dir()/VideoCRM`, pinned in settings
  when it exists.
- Updates: `.rushes-update/` appears beside the installation (the app bundle, the
  AppImage, or the `~/.local` prefix) while an update is prepared, and goes once it's in
  place. See [updates.md](updates.md).

## core (`rushes-core`)

- `model.rs` — the shared contract (already written; extend, do not break).
- `store.rs` — `Store` over `rusqlite` (bundled). Schema migrations via `PRAGMA user_version`.
  Videos, property defs, documents (blocks as JSON), database views, pipeline runs
  (+ append-only run events), uploads, key/value settings (JSON).
  `Store::list_video_rows()` returns `VideoRow`s with derived script stats and the
  latest run/upload status. First open seeds a default "All videos" table view and a
  "Board" view.
- `document.rs` — document logic: spoken word count, estimated runtime (150 wpm default),
  outline (headings), Markdown export/import (round-trippable for all `BlockKind`s;
  talking points / B-roll / on-screen cues use a `> [!talkingpoint]` / `> [!broll]` /
  `> [!onscreen]` callout syntax), teleprompter text (spoken blocks plus talking points
  marked `[Talking point]`, inline markdown stripped). Talking points are ideas the creator
  says in their own words, so they count toward neither the word count nor the runtime.
- `templates.rs` — script templates (Tutorial, Review, Vlog, Short, Blank) producing
  `Vec<Block>` personalised with title/hook/format.
- `media.rs` — media root, per-video folder creation, footage listing (video/audio
  extensions, size, mtime), finding edit outputs (`footage/edit/final.mp4`, `preview.mp4`).
- `settings.rs` — typed `AppSettings` stored in the settings table: media root,
  YouTube backend (`Google { client_id, client_secret }` | `Emulator { google_url, youtube_url }`),
  pipeline config (agent command, editing harness: Rushes' tools or video-use, video-use
  skill dir, extra instructions, auto-approve, Codex command),
  `onboarding_completed`, theme, zoom, week start, updates, and whether video pages open
  with their properties folded (`properties_collapsed`).

## youtube (`rushes-youtube`)

- `YoutubeConfig` — endpoints + OAuth client. `YoutubeConfig::google(client_id, secret)`
  points at real Google; `YoutubeConfig::emulator(google_url, youtube_url)` points at the
  local emulators. Env overrides: `RUSHES_GOOGLE_URL`, `RUSHES_YOUTUBE_URL`.
- OAuth: installed-app flow. `begin_authorization(&config) -> PendingAuthorization`
  binds `127.0.0.1:0`, builds the consent URL (scopes: `openid email profile
  https://www.googleapis.com/auth/youtube.upload https://www.googleapis.com/auth/youtube.readonly`,
  `access_type=offline`, `prompt=consent`, PKCE S256, random `state`). The app opens the URL
  in the browser; `PendingAuthorization::wait(timeout, cancel)` serves the redirect, shows a
  small "you can close this tab" page, validates `state`, exchanges the code.
- `Credentials` (access/refresh token, expiry, scope) + `CredentialStore` (JSON file, 0600).
- `YoutubeClient` — refreshes the access token when expired or on 401 and reports
  refreshed credentials through a callback so the app can persist them.
  `userinfo()`, `my_channel()`, `upload_video(path, &VideoMetadata, progress, cancel)`
  (resumable protocol, chunked, resumable after interruption via the session URL),
  `resume_upload(session_url, path, ...)`, `set_thumbnail(video_id, path)`,
  `get_video(id)`, `list_my_uploads(max)`, `revoke()`.

## edit (`rushes-edit`)

Rushes' own editing harness: tools the agent edits with (inventory, add_footage, transcribe,
read_transcript, phrase_words, silences, shots, look, plan_cut, render, check), an MCP server
for them, and the agent's instructions. The agent picks phrases by id; the crate does all
timing, renders frame- and sample-exact, and measures the result. The app binary serves it
as `rushes edit-tools --footage <dir>`. See [editing-harness.md](editing-harness.md).

## pipeline (`rushes-pipeline`)

Drives a headless coding agent (`claude -p --output-format stream-json --verbose`) that
edits with Rushes' tools (`Harness::Rushes`, over MCP, next to its shell) or with
[video-use](https://github.com/browser-use/video-use) (`Harness::VideoUse`); a run keeps the
one it started with (`PipelineRun::harness`). An edit
proposes a strategy and waits for approval, so a run has two phases:

1. **Plan** — agent inventories footage, transcribes, proposes a strategy, stops.
   Run status `Planning -> AwaitingApproval`; the strategy text is the run summary.
2. **Render** — user approves (optionally with notes) or sends feedback; the app resumes
   the same agent session (`--resume <session_id>`). Status `Rendering -> Succeeded`
   once `footage/edit/final.mp4` exists. Further feedback resumes again.

`auto_approve` collapses both phases into one prompt. `doctor_rushes()` reports whether the
agent CLI, ffmpeg/ffprobe and the ElevenLabs key are available; `doctor()` checks the same
plus the video-use skill and Python.
`scripts/fake-agent` imitates the agent's stream-json output for tests.

The same agent writes a video's YouTube title, description and tags in one turn without
tools (`write_details`, JSON checked against a schema), from the script and the edit's
transcript; Codex (`codex exec --json`) draws thumbnails with its image generation
(`generate_thumbnails`), which ffmpeg turns into 1280 × 720 JPGs. `scripts/fake-codex`
stands in for Codex in tests.

## app (`rushes`)

GPUI Kit app. Main pieces:

- `AppState` entity: owns the database connections (see "Database access"), the write
  queue, cached rows/properties/views, settings; all mutations go
  through it and emit events.
- Shell: title bar, sidebar (Videos views, AI edits, Uploads, recent pages, account,
  settings), routed main area.
- Zoom (`ui/zoom.rs`, `shell/zoom.rs`): Ctrl/⌘ with `=`/`+`, `-` and `0` (app-wide
  actions, so they work in dialogs and onboarding) or Settings › General step the
  `zoom` setting through 75–250%. The workspace sets the kit theme's base font, which
  GPUI Kit makes the window's `rem`, to 16 px times the zoom, so rem-sized UI scales.
  What the kit sizes in pixels is scaled by hand: title bar, sidebar width, table rows
  and column widths (views store widths at 100%), notifications, calendars. The kit
  hangs a notification's icon in pixels, so notifications come from `ui::notification`,
  which hangs it in rems. Tab bars (`zoom::zoomed_tabs`) step up a kit size when their
  label outgrows the frame; past the largest frame (about 175%, 155% for segmented tabs)
  the bar is drawn at the largest rem that frame holds, so its labels stop growing
  instead of being cut. The kit's `DatePicker` can't be widened, so dates are picked in
  `ui::date_field` (kit calendar and time field in a popover). Above 150% a 1440 × 900
  screen holds less than the window's designed minimum: pages wrap and the sidebar
  scrolls.
- `database/` — Notion-like videos database: table view (inline cell editing, column
  menu, add property, sort/filter/search) and board view (kanban by status, drag cards).
- `editor/` — block-based rich document editor (slash menu, markdown shortcuts,
  keyboard navigation between blocks, drag to reorder, inline formatting, live stats).
- Video page: icon + title, properties panel, tabs Script / Notes / Footage & AI edit / Publish.
- Onboarding (first run) and New-video wizard (templates).
- Publish: the video's YouTube account (one of several connected accounts, or the
  default one), metadata form, upload with progress, resume. The thumbnail goes up
  when the upload finishes, and at once when it's chosen for a video already on
  YouTube (`publish/thumbnail.rs`); `YoutubeLink.thumbnail` records the image YouTube
  has, so the Thumbnail card can say whether the chosen one is there. `PublishAssistant`
  has the agent write the title, description and tags (with Undo) and Codex draw
  thumbnails to choose from; both keep running when the tab is left.
- AI edit: footage list, edit brief, run timeline (agent messages/tool calls), approve /
  feedback, output preview and "use for upload".
- `update/` — `UpdateService`: automatic checks, background install, Settings › About ›
  Updates and the title bar's "Restart to update" ([updates.md](updates.md)).

## update (`rushes-update`)

`Updater::check` asks GitHub for the newest release and says whether this copy installs it
(`Delivery::Install`) or the creator downloads it (`Delivery::Manual`); `Updater::install`
downloads the release file for the detected `Installation` (macOS app, AppImage, `~/.local`
prefix), checks its size and SHA-256, and puts it in place; `relaunch_after_exit` opens the
new version once the app quits. Details in [updates.md](updates.md).

## Testing

- `cargo test -p rushes-core` — store, documents, templates.
- `cargo test -p rushes-youtube` — unit tests + integration tests against the emulators
  (`emulators/`; skipped with a message when Node is unavailable).
- `cargo test -p rushes-pipeline` — with `scripts/fake-agent` and `scripts/fake-codex`.
- `cargo test -p rushes-update` — against a local stand-in for GitHub.
- `cargo test -p rushes` — GPUI UI integration tests (`#[gpui_kit::test]`).
- Visual checks: `scripts/screenshot.sh` runs the app under Xvfb and captures PNGs.
