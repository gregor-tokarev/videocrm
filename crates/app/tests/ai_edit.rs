//! AI edit runs end to end: `PipelineService` drives `scripts/fake-agent` on generated
//! footage, with the store, the video and the run timeline checked after each phase.
//! The UI integration tests at the end drive the same runs through the real panel,
//! pipelines page and settings in headless windows.

#![cfg(unix)]

use std::cell::{Cell, RefCell};
use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::time::{Duration, Instant};

use chrono::Utc;
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{
    AnyView, AnyWindowHandle, App, AppContext as _, Bounds, Context, ElementId, Entity, ExternalPaths, FileDropEvent,
    InteractiveElement as _, IntoElement, ParentElement as _, Pixels, PlatformInput, Render, ScrollDelta, ScrollHandle,
    StatefulInteractiveElement as _, Styled as _, TestAppContext, Window, WindowBounds, WindowOptions, canvas, div,
    point, px, size,
};
use rushes::ai_edit::{
    AiEditPanel, AiSettings, INTERRUPTED, LogEvent, PhaseKind, PhaseStarted, PipelineService, PipelinesPage,
    RENDERED_WITH_ERROR, RunError, RunMarker, TimelineEntry, delete_run_notice, split_failure,
};
use rushes::route::{Route, VideoTab};
use rushes::state::{AppEvent, AppState, SAVE_RETRY_DELAY};
use rushes::ui::zoom::apply_zoom;
use rushes_core::settings::{EditHarness, Zoom};
use rushes_core::{Block, DocumentKind, NewVideo, PipelineRun, RunId, RunStatus, Store, VideoId, VideoStatus};
use rushes_pipeline::{AgentEvent, AgentProcess};
use tempfile::TempDir;

const TIMEOUT: Duration = Duration::from_secs(90);

/// The fake agent needs python3 and renders with ffmpeg.
fn tools_available() -> bool {
    let ok = ["python3", "ffmpeg", "ffprobe"].iter().all(|tool| Command::new(tool).arg("-version").output().is_ok());
    if !ok {
        eprintln!("skipping: python3, ffmpeg and ffprobe are required");
    }
    ok
}

struct Fixture {
    root: TempDir,
    state: Entity<AppState>,
    service: Entity<PipelineService>,
    video: VideoId,
    footage: PathBuf,
    pid_file: PathBuf,
}

impl Fixture {
    /// A video with one 3 s take, the fake agent and a fake video-use skill.
    fn new(cx: &mut TestAppContext, env: &[(&str, &str)]) -> Self {
        Self::with_store(cx, Store::open_in_memory().expect("store"), env, true)
    }

    fn with_store(cx: &mut TestAppContext, store: Store, env: &[(&str, &str)], footage: bool) -> Self {
        // Agents run on real threads and wake the foreground from there.
        cx.executor().allow_parking();
        cx.update(gpui_kit::init);

        let root = tempfile::tempdir().expect("tempdir");
        let base = fs::canonicalize(root.path()).expect("canonical tempdir");
        let skill = base.join("video-use");
        fs::create_dir_all(skill.join("helpers")).unwrap();
        fs::write(skill.join("SKILL.md"), "# video-use\n").unwrap();
        fs::write(skill.join(".env"), "ELEVENLABS_API_KEY=test-key\n").unwrap();
        let pid_file = base.join("pids");

        let state = cx.new(|_| AppState::new(store).expect("app state"));
        let video = cx.update(|cx| {
            AppState::set_global(state.clone(), cx);
            state.update(cx, |state, cx| {
                state.update_settings(cx, |settings| {
                    settings.media_root = Some(base.join("media"));
                    settings.pipeline.agent_command = fake_agent().display().to_string();
                    settings.pipeline.skill_dir = Some(skill.clone());
                });
                state
                    .create_video(NewVideo { title: "Desk setup 2026".into(), ..NewVideo::default() }, cx)
                    .expect("video created")
            })
        });
        let footage_dir = cx.read(|cx| {
            state.read(cx).video(video).and_then(|video| video.folder.clone()).expect("media folder").join("footage")
        });
        if footage {
            make_clip(&footage_dir.join("take1.mp4"));
        }

        let mut agent_env: Vec<(String, String)> =
            env.iter().map(|(key, value)| ((*key).to_owned(), (*value).to_owned())).collect();
        agent_env.push(("FAKE_AGENT_PID_FILE".into(), pid_file.display().to_string()));
        let service = cx.update(|cx| {
            PipelineService::init(state.clone(), cx);
            let service = PipelineService::global(cx);
            service.update(cx, |service, cx| service.set_agent_env(agent_env, cx));
            service
        });
        Self { root, state, service, video, footage: footage_dir, pid_file }
    }

    fn start(&self, cx: &mut TestAppContext, auto_approve: bool) -> RunId {
        cx.update(|cx| {
            self.service
                .update(cx, |service, cx| service.start_run(self.video, "Cut it tight.".into(), auto_approve, cx))
        })
        .expect("run started")
    }

    fn run(&self, cx: &mut TestAppContext, id: RunId) -> PipelineRun {
        cx.read(|cx| self.service.read(cx).run(id).cloned().expect("run"))
    }

    fn stored_run(&self, cx: &mut TestAppContext, id: RunId) -> PipelineRun {
        cx.read(|cx| self.state.read(cx).store().lock().unwrap().get_run(id).unwrap().expect("stored run"))
    }

    fn stored_events(&self, cx: &mut TestAppContext, id: RunId) -> Vec<TimelineEntry> {
        let events = cx.read(|cx| self.state.read(cx).store().lock().unwrap().list_run_events(id).unwrap());
        events.into_iter().map(TimelineEntry::from).collect()
    }

    fn wait_for_status(&self, cx: &mut TestAppContext, id: RunId, status: RunStatus) -> PipelineRun {
        let service = self.service.clone();
        wait_until(cx, &format!("run status {status:?}"), move |cx| {
            let run = service.read(cx).run(id);
            run.is_some_and(|run| run.status == status) && service.read(cx).live_phase(id).is_none()
        });
        let run = self.run(cx, id);
        assert_eq!(self.stored_run(cx, id), run, "the cached run is the stored run");
        run
    }

    fn video_status(&self, cx: &mut TestAppContext) -> VideoStatus {
        cx.read(|cx| self.state.read(cx).video(self.video).expect("video").status)
    }

    fn upload_file(&self, cx: &mut TestAppContext) -> Option<PathBuf> {
        cx.read(|cx| self.state.read(cx).video(self.video).expect("video").publish.file.clone())
    }

    fn set_harness(&self, cx: &mut TestAppContext, harness: EditHarness) {
        cx.update(|cx| {
            self.state.update(cx, |state, cx| state.update_settings(cx, |settings| settings.pipeline.harness = harness))
        });
    }

    /// Where the agent renders.
    fn final_cut(&self) -> PathBuf {
        self.footage.join("edit").join("final.mp4")
    }

    /// Where the `n`th render of the video is kept.
    fn cut(&self, n: u32) -> PathBuf {
        self.footage.join("edit").join("cuts").join(format!("final-{n}.mp4"))
    }
}

fn fake_agent() -> PathBuf {
    fs::canonicalize(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/fake-agent")).expect("fake agent")
}

/// Writes an executable script at `path`, from a child process.
///
/// A file this process writes is open for writing until it is closed, and a
/// process another test thread forks meanwhile inherits that descriptor and
/// keeps it until it starts its own program. Running the script before then
/// fails with "Text file busy" (ETXTBSY), which a loaded machine turns from a
/// microsecond window into an occasional failure. Written by a child, the
/// script is never open in this process, so no fork of ours can hold it.
fn write_executable(path: &Path, script: &str) {
    let written = Command::new("/bin/sh")
        .args(["-c", r#"printf '%s' "$1" > "$2""#, "sh"])
        .arg(script)
        .arg(path)
        .status()
        .expect("run /bin/sh");
    assert!(written.success(), "couldn't write {}", path.display());
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("make it executable");
}

/// An agent that reports success without doing anything; with `LAZY_IS_ERROR=1` it
/// reports an error result (like a turn limit) but still exits normally.
fn lazy_agent(dir: &Path) -> PathBuf {
    let path = dir.join("lazy-agent");
    write_executable(
        &path,
        r#"#!/bin/sh
echo '{"type":"system","subtype":"init","session_id":"lazy-session","cwd":"/","tools":[],"model":"lazy"}'
if [ "$LAZY_IS_ERROR" = 1 ]; then
  echo '{"type":"result","subtype":"error_max_turns","is_error":true,"result":"Reached the turn limit.","session_id":"lazy-session","total_cost_usd":0.01}'
else
  echo '{"type":"result","subtype":"success","is_error":false,"result":"All done.","session_id":"lazy-session","total_cost_usd":0.01}'
fi
"#,
    );
    path
}

/// An agent that renders edit/final.mp4 (a copy of the first take) and then reports
/// an error, as Claude Code does when it hits its turn limit during the self-check.
/// With `RENDER_THEN_EXIT=1` it also exits with an error status.
fn render_then_error_agent(dir: &Path) -> PathBuf {
    let path = dir.join("render-then-error-agent");
    write_executable(
        &path,
        r#"#!/bin/sh
echo '{"type":"system","subtype":"init","session_id":"rte-session","cwd":"/","tools":[],"model":"rte"}'
mkdir -p edit && cp take1.mp4 edit/final.mp4
echo '{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Rendered edit/final.mp4; running the self-eval now."}]}}'
echo '{"type":"result","subtype":"error_max_turns","is_error":true,"session_id":"rte-session","total_cost_usd":0.9}'
[ "$RENDER_THEN_EXIT" = 1 ] && exit 1
exit 0
"#,
    );
    path
}

/// An agent that starts a render, writes the first 40 kB of the take to
/// edit/final.mp4 and then works silently, as Claude does while ffmpeg renders.
fn partial_render_agent(dir: &Path) -> PathBuf {
    let path = dir.join("partial-render-agent");
    write_executable(
        &path,
        r#"#!/bin/sh
case "$*" in *--version*) echo "2.1.284 (Claude Code)"; exit 0;; esac
echo '{"type":"system","subtype":"init","session_id":"partial-session","cwd":"/","tools":[],"model":"partial"}'
mkdir -p edit && head -c 40000 take1.mp4 > edit/final.mp4
echo '{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Rendering edit/final.mp4."}]}}'
exec sleep 60
"#,
    );
    path
}

/// An agent with a real run's log shape: a few dozen tool calls, then a final summary
/// far taller than the activity box, then the result. It renders nothing.
fn chatty_agent(dir: &Path) -> PathBuf {
    let path = dir.join("chatty-agent");
    write_executable(
        &path,
        r#"#!/usr/bin/env python3
import json, sys
def out(event):
    event.setdefault("session_id", "chatty-session")
    print(json.dumps(event), flush=True)
def assistant(block):
    out({"type": "assistant", "message": {"role": "assistant", "content": [block]}})
out({"type": "system", "subtype": "init", "cwd": "/", "tools": ["Bash"], "model": "chatty"})
for ix in range(30):
    assistant({"type": "tool_use", "id": f"t{ix}", "name": "Bash", "input": {"command": f"ffprobe take{ix}.mp4"}})
    out({"type": "user", "message": {"role": "user", "content": [
        {"type": "tool_result", "tool_use_id": f"t{ix}", "content": "ok", "is_error": False}]}})
point = "**Audio sync:** the sound drifted until it was 0.1s behind, so I rebuilt it from the `edit/` segments. "
summary = "The final cut is rendered: **`edit/final.mp4`**, 14.6s.\n\n" + "\n\n".join(
    f"**Section {n}:**\n" + "\n".join(f"- {point * 3}" for _ in range(4)) for n in range(4))
assistant({"type": "text", "text": summary})
out({"type": "result", "subtype": "success", "is_error": False, "result": summary, "total_cost_usd": 0.5})
"#,
    );
    path
}

/// An agent that sends its next tool call each time the test creates the next numbered
/// file (`0`, `1`, ...) in `gate`, then ends after `steps` of them.
fn stepping_agent(dir: &Path, gate: &Path, steps: usize) -> PathBuf {
    let path = dir.join("stepping-agent");
    let script = r#"#!/usr/bin/env python3
import json, os, time
def out(event):
    event.setdefault("session_id", "stepping-session")
    print(json.dumps(event), flush=True)
out({"type": "system", "subtype": "init", "cwd": "/", "tools": ["Bash"], "model": "stepping"})
for ix in range(STEPS):
    while not os.path.exists(os.path.join(GATE, str(ix))):
        time.sleep(0.01)
    out({"type": "assistant", "message": {"role": "assistant", "content": [
        {"type": "tool_use", "id": f"t{ix}", "name": "Bash", "input": {"command": f"ffprobe take{ix}.mp4"}}]}})
out({"type": "result", "subtype": "success", "is_error": False, "result": "A strategy.", "total_cost_usd": 0.1})
"#;
    let script =
        script.replace("STEPS", &steps.to_string()).replace("GATE", &format!("{:?}", gate.display().to_string()));
    write_executable(&path, &script);
    path
}

/// A 3 second 320x240 test pattern with a sine tone.
fn make_clip(path: &Path) {
    let status = Command::new("ffmpeg")
        .args(["-loglevel", "error", "-y"])
        .args(["-f", "lavfi", "-i", "testsrc=duration=3:size=320x240:rate=25"])
        .args(["-f", "lavfi", "-i", "sine=frequency=440:duration=3"])
        .args(["-c:v", "libx264", "-preset", "ultrafast", "-pix_fmt", "yuv420p"])
        .args(["-c:a", "aac", "-shortest"])
        .arg(path)
        .status()
        .expect("run ffmpeg");
    assert!(status.success(), "ffmpeg failed for {}", path.display());
}

/// Drives the foreground until `done` holds, letting the agent threads work in real time.
fn wait_until(cx: &mut TestAppContext, what: &str, mut done: impl FnMut(&App) -> bool) {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        cx.run_until_parked();
        if cx.read(|cx| done(cx)) {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn phases(events: &[TimelineEntry]) -> Vec<(PhaseKind, Option<String>)> {
    events
        .iter()
        .filter_map(|entry| match &entry.event {
            LogEvent::Phase(started) => Some((started.phase, started.text.clone())),
            LogEvent::Agent(_) | LogEvent::Marker(_) => None,
        })
        .collect()
}

/// The markers in a run's log, with whether each phase was a retry.
fn markers(events: &[TimelineEntry]) -> Vec<String> {
    events
        .iter()
        .filter_map(|entry| match &entry.event {
            LogEvent::Phase(started) if started.retry => Some(format!("retry {:?}", started.phase)),
            LogEvent::Phase(started) => Some(format!("{:?}", started.phase)),
            LogEvent::Marker(RunMarker { kind, .. }) => Some(format!("{kind:?}")),
            LogEvent::Agent(_) => None,
        })
        .collect()
}

fn tool_names(events: &[TimelineEntry]) -> Vec<String> {
    events
        .iter()
        .filter_map(|entry| match &entry.event {
            LogEvent::Agent(AgentEvent::ToolUse { name, .. }) => Some(name.clone()),
            _ => None,
        })
        .collect()
}

/// Whether `pid` is gone (or a zombie waiting to be reaped).
fn process_gone(pid: u32) -> bool {
    match fs::read_to_string(format!("/proc/{pid}/stat")) {
        Err(_) => true,
        Ok(stat) => stat.rsplit_once(')').is_some_and(|(_, rest)| rest.trim_start().starts_with('Z')),
    }
}

fn recorded_pids(file: &Path) -> Vec<u32> {
    fs::read_to_string(file).unwrap_or_default().lines().filter_map(|line| line.trim().parse().ok()).collect()
}

#[gpui_kit::test]
fn plan_revise_approve_renders_and_prepares_the_upload(cx: &mut TestAppContext) {
    if !tools_available() {
        return;
    }
    let fx = Fixture::new(cx, &[]);
    assert_eq!(fx.video_status(cx), VideoStatus::Idea);

    let id = fx.start(cx, false);
    assert_eq!(fx.run(cx, id).status, RunStatus::Planning);
    assert_eq!(fx.video_status(cx), VideoStatus::Editing, "starting an edit moves the video to Editing");

    let planned = fx.wait_for_status(cx, id, RunStatus::AwaitingApproval);
    let strategy = planned.summary.clone().expect("strategy");
    assert!(strategy.contains("**Strategy**"), "{strategy}");
    let session = planned.session_id.clone().expect("session id");
    assert!((planned.cost_usd.expect("cost") - 0.0123).abs() < 1e-9);
    assert_eq!(planned.output, None);
    assert_eq!(planned.brief, "Cut it tight.");
    assert_eq!(planned.footage_dir, fx.footage);

    let events = fx.stored_events(cx, id);
    assert_eq!(phases(&events), [(PhaseKind::Plan, None)]);
    assert!(matches!(events[1].event, LogEvent::Agent(AgentEvent::Spawned { .. })), "{:?}", events[1]);
    assert!(matches!(events[2].event, LogEvent::Agent(AgentEvent::Init { .. })), "{:?}", events[2]);
    assert_eq!(tool_names(&events), ["Skill", "Bash", "Bash", "Bash", "Write"]);
    assert!(matches!(events.last().unwrap().event, LogEvent::Agent(AgentEvent::Result { is_error: false, .. })));
    assert_eq!(cx.read(|cx| fx.service.read(cx).timeline(id).to_vec()), events, "live timeline matches the store");
    assert!(!fx.final_cut().exists(), "planning renders nothing");

    let empty = cx.update(|cx| fx.service.update(cx, |service, cx| service.send_feedback(id, "  ".into(), cx)));
    assert_eq!(empty, Err(RunError::EmptyFeedback));

    // Feedback on the strategy asks for a revised strategy, in the same session.
    cx.update(|cx| fx.service.update(cx, |service, cx| service.send_feedback(id, "Add an end card.".into(), cx)))
        .expect("feedback sent");
    assert_eq!(fx.run(cx, id).status, RunStatus::Planning);
    let revised = fx.wait_for_status(cx, id, RunStatus::AwaitingApproval);
    assert_eq!(revised.session_id.as_deref(), Some(session.as_str()));
    assert!(!fx.final_cut().exists(), "a revised strategy renders nothing");

    cx.update(|cx| fx.service.update(cx, |service, cx| service.approve(id, Some("  Keep the intro.  ".into()), cx)))
        .expect("approved");
    assert_eq!(fx.run(cx, id).status, RunStatus::Rendering);
    let done = fx.wait_for_status(cx, id, RunStatus::Succeeded);
    assert_eq!(done.output, Some(fx.cut(1)), "the render is kept as the edit's own file");
    assert!(fx.cut(1).is_file());
    assert!(!fx.final_cut().exists(), "nothing is left where the next render writes");
    assert_eq!(done.session_id.as_deref(), Some(session.as_str()), "phases resume one session");
    assert!((done.cost_usd.unwrap() - 3.0 * 0.0123).abs() < 1e-9, "cost adds up over phases");
    // The summary names the file the cut is kept in, not the shared path it was rendered to.
    let summary = done.summary.unwrap();
    assert!(summary.contains("Rendered edit/cuts/final-1.mp4") && !summary.contains("edit/final.mp4"), "{summary}");
    assert_eq!(fx.upload_file(cx), Some(fx.cut(1)), "the final cut becomes the upload file");
    assert_eq!(fx.video_status(cx), VideoStatus::Editing);

    let events = fx.stored_events(cx, id);
    assert_eq!(
        phases(&events),
        [
            (PhaseKind::Plan, None),
            (PhaseKind::Revise, Some("Add an end card.".into())),
            (PhaseKind::Approve, Some("Keep the intro.".into())),
        ]
    );
    let seqs: Vec<u32> = events.iter().map(|entry| entry.seq).collect();
    assert_eq!(seqs, (1..=events.len() as u32).collect::<Vec<_>>());
    let row = cx.read(|cx| fx.state.read(cx).row(fx.video).cloned().unwrap());
    assert_eq!(row.latest_run, Some(RunStatus::Succeeded), "rows reload with the run state");
}

#[gpui_kit::test]
fn an_edit_keeps_the_tools_it_started_with(cx: &mut TestAppContext) {
    if !tools_available() {
        return;
    }
    let argv_dir = tempfile::tempdir().unwrap();
    let argv_file = argv_dir.path().join("argv.json");
    let fx = Fixture::new(cx, &[("FAKE_AGENT_ARGV_FILE", argv_file.to_str().unwrap())]);
    let argv = || -> Vec<String> { serde_json::from_slice(&fs::read(&argv_file).unwrap()).unwrap() };
    let uses_skill = |argv: &[String]| {
        argv.iter().any(|arg| arg.starts_with("--add-dir=")) && !argv.iter().any(|arg| arg.starts_with("--mcp-config"))
    };

    fx.set_harness(cx, EditHarness::VideoUse);
    let id = fx.start(cx, false);
    let planned = fx.wait_for_status(cx, id, RunStatus::AwaitingApproval);
    assert_eq!(planned.harness, Some(EditHarness::VideoUse));
    assert!(uses_skill(&argv()), "{:?}", argv());

    // The setting changes mid-edit: this edit resumes its session with the skill.
    fx.set_harness(cx, EditHarness::Rushes);
    cx.update(|cx| fx.service.update(cx, |service, cx| service.approve(id, None, cx))).expect("approved");
    let done = fx.wait_for_status(cx, id, RunStatus::Succeeded);
    assert_eq!(done.harness, Some(EditHarness::VideoUse));
    assert!(uses_skill(&argv()), "{:?}", argv());

    // A new edit takes the setting.
    let next = fx.start(cx, false);
    let planned = fx.wait_for_status(cx, next, RunStatus::AwaitingApproval);
    assert_eq!(planned.harness, Some(EditHarness::Rushes));
    assert!(argv().iter().any(|arg| arg.starts_with("--mcp-config=")), "{:?}", argv());
}

#[gpui_kit::test]
fn auto_approve_renders_and_feedback_renders_again(cx: &mut TestAppContext) {
    if !tools_available() {
        return;
    }
    let fx = Fixture::new(cx, &[]);
    let id = fx.start(cx, true);
    assert_eq!(fx.run(cx, id).status, RunStatus::Rendering);
    let first = fx.wait_for_status(cx, id, RunStatus::Succeeded);
    assert_eq!(first.output, Some(fx.cut(1)));
    assert_eq!(phases(&fx.stored_events(cx, id)), [(PhaseKind::Auto, None)]);

    // An upload file chosen by hand is kept.
    let chosen = fx.root.path().join("my-cut.mp4");
    cx.update(|cx| {
        fx.state.update(cx, |state, cx| {
            let mut video = state.video(fx.video).unwrap().clone();
            video.publish.file = Some(chosen.clone());
            state.update_video(video, cx);
        })
    });
    let rendered_at = fs::metadata(fx.cut(1)).unwrap().modified().unwrap();
    std::thread::sleep(Duration::from_millis(20));

    cx.update(|cx| fx.service.update(cx, |service, cx| service.send_feedback(id, "Shorter outro.".into(), cx)))
        .expect("feedback sent");
    assert_eq!(fx.run(cx, id).status, RunStatus::Rendering);
    let again = fx.wait_for_status(cx, id, RunStatus::Succeeded);
    assert_eq!(again.output, Some(fx.cut(2)), "the new render is a new file");
    assert_eq!(fs::metadata(fx.cut(1)).unwrap().modified().unwrap(), rendered_at, "the first render is kept");
    assert!(again.summary.unwrap().contains("rendered to edit/cuts/final-2.mp4 with your feedback applied"));
    assert_eq!(fx.upload_file(cx), Some(chosen));
    assert_eq!(
        phases(&fx.stored_events(cx, id)),
        [(PhaseKind::Auto, None), (PhaseKind::Feedback, Some("Shorter outro.".into()))]
    );
}

#[gpui_kit::test]
fn a_new_edit_keeps_the_cut_of_the_earlier_one(cx: &mut TestAppContext) {
    if !tools_available() {
        return;
    }
    let fx = Fixture::new(cx, &[]);
    let first = fx.start(cx, true);
    fx.wait_for_status(cx, first, RunStatus::Succeeded);
    let first_cut = fs::read(fx.cut(1)).unwrap();

    // A second edit of the same footage, cut differently.
    make_clip(&fx.footage.join("take2.mp4"));
    let second = fx.start(cx, true);
    let done = fx.wait_for_status(cx, second, RunStatus::Succeeded);
    assert_eq!(done.output, Some(fx.cut(2)));
    assert_eq!(fx.run(cx, first).output, Some(fx.cut(1)), "the earlier edit still has its cut");
    assert_eq!(fs::read(fx.cut(1)).unwrap(), first_cut, "and the file is untouched");
    assert_ne!(fs::read(fx.cut(2)).unwrap(), first_cut);
    assert_eq!(fx.upload_file(cx), Some(fx.cut(1)), "the upload keeps the cut it was given");
}

#[gpui_kit::test]
fn a_cut_left_at_the_shared_path_is_kept_before_the_next_render(cx: &mut TestAppContext) {
    if !tools_available() {
        return;
    }
    // A store as an earlier version left it: the edit's output and the upload file
    // are the shared edit/final.mp4.
    let fx = Fixture::new(cx, &[]);
    fs::create_dir_all(fx.final_cut().parent().unwrap()).unwrap();
    fs::copy(fx.footage.join("take1.mp4"), fx.final_cut()).unwrap();
    let old_cut = fs::read(fx.final_cut()).unwrap();
    let now = Utc::now();
    let old = PipelineRun {
        id: RunId::new(),
        video_id: fx.video,
        status: RunStatus::Succeeded,
        brief: String::new(),
        footage_dir: fx.footage.clone(),
        session_id: Some("old".into()),
        harness: None,
        summary: Some("Rendered.".into()),
        output: Some(fx.final_cut()),
        error: None,
        cost_usd: None,
        created_at: now,
        updated_at: now,
    };
    cx.read(|cx| fx.state.read(cx).store().lock().unwrap().insert_run(&old).unwrap());
    cx.update(|cx| {
        fx.state.update(cx, |state, cx| {
            let mut video = state.video(fx.video).unwrap().clone();
            video.publish.file = Some(fx.final_cut());
            state.update_video(video, cx);
        });
        PipelineService::init(fx.state.clone(), cx);
    });
    let service = cx.read(PipelineService::global);
    let id = cx
        .update(|cx| service.update(cx, |service, cx| service.start_run(fx.video, "Again.".into(), true, cx)))
        .expect("run started");
    assert_eq!(cx.read(|cx| service.read(cx).run(old.id).cloned().unwrap().output), Some(fx.cut(1)));
    assert_eq!(fx.upload_file(cx), Some(fx.cut(1)), "the upload form follows the kept cut");
    assert_eq!(fs::read(fx.cut(1)).unwrap(), old_cut);

    wait_until(cx, "the new render", |cx| {
        service.read(cx).run(id).is_some_and(|run| run.status == RunStatus::Succeeded)
    });
    assert_eq!(cx.read(|cx| service.read(cx).run(id).cloned().unwrap().output), Some(fx.cut(2)));
    assert_eq!(fs::read(fx.cut(1)).unwrap(), old_cut, "the render did not overwrite it");
}

#[gpui_kit::test]
fn cancel_stops_the_agent_and_its_children(cx: &mut TestAppContext) {
    if !tools_available() {
        return;
    }
    let fx = Fixture::new(cx, &[("FAKE_AGENT_DELAY_MS", "1500")]);
    let id = fx.start(cx, false);
    let pid_file = fx.pid_file.clone();
    // The agent and the `sleep` it waits in between events.
    wait_until(cx, "agent and child pids", |_| recorded_pids(&pid_file).len() >= 2);
    let pids = recorded_pids(&fx.pid_file);
    assert!(pids.iter().any(|pid| !process_gone(*pid)), "the agent is running");

    cx.update(|cx| fx.service.update(cx, |service, cx| service.cancel(id, cx)));
    let live = cx.read(|cx| fx.service.read(cx).live_phase(id)).expect("still stopping");
    assert!(live.cancelling);
    let cancelled = fx.wait_for_status(cx, id, RunStatus::Cancelled);
    assert_eq!(cancelled.error, None);
    assert_eq!(markers(&fx.stored_events(cx, id)), ["Plan", "Cancelled"], "the log says where it stopped");

    let deadline = Instant::now() + Duration::from_secs(10);
    while !pids.iter().all(|pid| process_gone(*pid)) {
        assert!(Instant::now() < deadline, "agent processes still running: {pids:?}");
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!fx.footage.join("edit").join("project.md").exists(), "the plan never finished");
}

#[gpui_kit::test]
fn a_second_edit_of_the_same_video_is_refused_while_one_runs(cx: &mut TestAppContext) {
    if !tools_available() {
        return;
    }
    let fx = Fixture::new(cx, &[("FAKE_AGENT_DELAY_MS", "500")]);
    let id = fx.start(cx, false);
    let second =
        cx.update(|cx| fx.service.update(cx, |service, cx| service.start_run(fx.video, "Again".into(), false, cx)));
    assert_eq!(second, Err(RunError::AlreadyRunning));
    assert!(second.unwrap_err().to_string().contains("already running"));
    assert_eq!(cx.read(|cx| fx.service.read(cx).video_runs(fx.video).count()), 1);
    assert_eq!(cx.read(|cx| fx.service.read(cx).live_run_of(fx.video)), Some(id));

    cx.update(|cx| fx.service.update(cx, |service, cx| service.cancel(id, cx)));
    fx.wait_for_status(cx, id, RunStatus::Cancelled);
}

#[gpui_kit::test]
fn a_video_without_footage_cannot_be_edited(cx: &mut TestAppContext) {
    let fx = Fixture::with_store(cx, Store::open_in_memory().unwrap(), &[], false);
    let result =
        cx.update(|cx| fx.service.update(cx, |service, cx| service.start_run(fx.video, String::new(), false, cx)));
    assert_eq!(result, Err(RunError::NoFootage(fx.footage.clone())));
    assert!(cx.read(|cx| fx.service.read(cx).runs().is_empty()));
    assert_eq!(fx.video_status(cx), VideoStatus::Idea);
}

#[gpui_kit::test]
fn a_failing_agent_fails_the_run_with_its_stderr(cx: &mut TestAppContext) {
    if !tools_available() {
        return;
    }
    let fx = Fixture::new(cx, &[("FAKE_AGENT_MODE", "fail")]);
    let id = fx.start(cx, false);
    let failed = fx.wait_for_status(cx, id, RunStatus::Failed);
    let error = failed.error.expect("error");
    let (message, stderr) = split_failure(&error);
    assert!(message.contains("simulated failure"), "{message}");
    assert!(stderr.expect("stderr tail").contains("ELEVENLABS_API_KEY rejected (401)"), "{error}");
    assert!(failed.session_id.is_some(), "the session can be resumed");

    cx.update(|cx| fx.service.update(cx, |service, cx| service.retry(id, cx))).expect("retried");
    assert_eq!(fx.run(cx, id).status, RunStatus::Planning);
    assert_eq!(fx.run(cx, id).error, None);
    fx.wait_for_status(cx, id, RunStatus::Failed);
    assert_eq!(markers(&fx.stored_events(cx, id)), ["Plan", "retry Plan"]);
}

#[gpui_kit::test]
fn a_failed_edit_marks_the_step_it_stopped_at(cx: &mut TestAppContext) {
    if !tools_available() {
        return;
    }
    let fx = Fixture::new(cx, &[("FAKE_AGENT_MODE", "fail")]);
    let video = fx.video;
    let window = open(cx, move |window, cx| AiEditPanel::new(video, window, cx));
    let id = fx.start(cx, false);
    fx.wait_for_status(cx, id, RunStatus::Failed);
    // Not only the card below: the stepper itself shows the step failed.
    wait_for_window(cx, window, "the failed step", |window, _| shows(window, "run-stopped-step"));
}

#[gpui_kit::test]
fn an_agent_command_that_is_not_claude_code_is_named_as_the_cause(cx: &mut TestAppContext) {
    if !tools_available() {
        return;
    }
    let fx = Fixture::new(cx, &[]);
    let silent = fx.root.path().join("not-claude");
    write_executable(&silent, "#!/bin/sh\necho 'true (GNU coreutils) 9.7'\n");
    cx.update(|cx| {
        fx.state.update(cx, |state, cx| {
            state.update_settings(cx, |settings| settings.pipeline.agent_command = silent.display().to_string())
        })
    });
    let service = fx.service.clone();
    wait_until(cx, "the requirement check", |cx| !service.read(cx).is_checking());
    let doctor = cx.read(|cx| service.read(cx).doctor().cloned()).expect("doctor");
    assert!(!doctor.agent.ok && doctor.agent.detail.contains("is not Claude Code"), "{:?}", doctor.agent);

    let id = fx.start(cx, false);
    let failed = fx.wait_for_status(cx, id, RunStatus::Failed);
    let error = failed.error.expect("error");
    assert!(error.starts_with("The agent stopped without reporting a result"), "{error}");
    assert!(error.ends_with("Check the agent command in Settings > AI editing."), "{error}");
}

#[gpui_kit::test]
fn startup_marks_interrupted_runs_failed_and_they_can_continue(cx: &mut TestAppContext) {
    if !tools_available() {
        return;
    }
    // A store as a previous session left it: one video, runs in every state.
    let mut store = Store::open_in_memory().unwrap();
    let video = rushes_core::Video::new("Old session");
    store.insert_video(&video).unwrap();
    let run = |status: RunStatus, session: Option<&str>| PipelineRun {
        id: RunId::new(),
        video_id: video.id,
        status,
        brief: String::new(),
        footage_dir: PathBuf::from("/nonexistent"),
        session_id: session.map(str::to_owned),
        harness: None,
        summary: None,
        output: None,
        error: None,
        cost_usd: None,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };
    let runs = [
        run(RunStatus::Queued, None),
        run(RunStatus::Planning, Some("s-plan")),
        run(RunStatus::Rendering, Some("s-render")),
        run(RunStatus::AwaitingApproval, Some("s-wait")),
        run(RunStatus::Succeeded, Some("s-done")),
    ];
    for run in &runs {
        store.insert_run(run).unwrap();
    }

    let fx = Fixture::with_store(cx, store, &[], true);
    let status = |cx: &mut TestAppContext, id| fx.stored_run(cx, id);
    for run in &runs[..3] {
        let stored = status(cx, run.id);
        assert_eq!((stored.status, stored.error.as_deref()), (RunStatus::Failed, Some(INTERRUPTED)));
        assert_eq!(fx.run(cx, run.id).status, RunStatus::Failed);
        assert_eq!(markers(&fx.stored_events(cx, run.id)), ["Interrupted"]);
    }
    assert_eq!(status(cx, runs[3].id).status, RunStatus::AwaitingApproval);
    assert_eq!(status(cx, runs[4].id).status, RunStatus::Succeeded);

    // The interrupted render continues in its session, on the fixture's footage.
    let interrupted = runs[2].id;
    let mut moved = fx.stored_run(cx, interrupted);
    moved.footage_dir = fx.footage.clone();
    cx.read(|cx| fx.state.read(cx).store().lock().unwrap().update_run(&mut moved).unwrap());
    cx.update(|cx| {
        PipelineService::init(fx.state.clone(), cx);
        let service = PipelineService::global(cx);
        service.update(cx, |service, cx| {
            service.send_feedback(interrupted, "Finish the render.".into(), cx).expect("continued")
        });
    });
    let service = cx.read(PipelineService::global);
    wait_until(cx, "continued render", |cx| {
        service.read(cx).run(interrupted).is_some_and(|run| run.status == RunStatus::Succeeded)
    });
    let continued = cx.read(|cx| service.read(cx).run(interrupted).cloned().unwrap());
    assert_eq!(continued.session_id.as_deref(), Some("s-render"));
    assert_eq!(continued.output, Some(fx.cut(1)));
}

#[gpui_kit::test]
fn video_rows_show_interrupted_runs_as_failed_from_the_start(cx: &mut TestAppContext) {
    let mut store = Store::open_in_memory().unwrap();
    let video = rushes_core::Video::new("Quit while planning");
    store.insert_video(&video).unwrap();
    let now = Utc::now();
    let run = PipelineRun {
        id: RunId::new(),
        video_id: video.id,
        status: RunStatus::Planning,
        brief: String::new(),
        footage_dir: PathBuf::from("/nonexistent"),
        session_id: Some("s".into()),
        harness: None,
        summary: None,
        output: None,
        error: None,
        cost_usd: None,
        created_at: now,
        updated_at: now,
    };
    store.insert_run(&run).unwrap();
    // The rows are read before the service starts, as at app startup.
    let state = cx.new(|_| AppState::new(store).expect("app state"));
    assert_eq!(cx.read(|cx| state.read(cx).row(video.id).unwrap().latest_run), Some(RunStatus::Planning));

    cx.update(|cx| {
        AppState::set_global(state.clone(), cx);
        PipelineService::init(state.clone(), cx);
    });
    let row = cx.read(|cx| state.read(cx).row(video.id).cloned().unwrap());
    assert_eq!(row.latest_run, Some(RunStatus::Failed), "no edit shows as running after a restart");
}

#[gpui_kit::test]
fn the_requirement_check_passes_with_the_fake_agent(cx: &mut TestAppContext) {
    let fx = Fixture::with_store(cx, Store::open_in_memory().unwrap(), &[], false);
    let service = fx.service.clone();
    wait_until(cx, "requirement check", |cx| !service.read(cx).is_checking());
    let doctor = cx.read(|cx| service.read(cx).doctor().cloned()).expect("doctor result");
    assert!(doctor.agent.ok, "{:?}", doctor.agent);
    // Rushes' own tools by default: no skill and no Python to check.
    assert_eq!((doctor.skill.as_ref(), doctor.python.as_ref()), (None, None), "{doctor:?}");
    assert!(doctor.elevenlabs.ok, "{:?}", doctor.elevenlabs);
    if tools_available() {
        assert!(doctor.ready(), "{doctor:?}");
    }

    // Pointing the settings at a missing agent re-runs the check.
    cx.update(|cx| {
        fx.state.update(cx, |state, cx| {
            state.update_settings(cx, |settings| settings.pipeline.agent_command = "/nonexistent/claude".into())
        })
    });
    assert!(cx.read(|cx| service.read(cx).is_checking()));
    wait_until(cx, "second check", |cx| !service.read(cx).is_checking());
    let doctor = cx.read(|cx| service.read(cx).doctor().cloned()).unwrap();
    assert!(!doctor.agent.ok && !doctor.ready());
}

#[gpui_kit::test]
fn an_edit_runs_on_while_another_program_locks_the_database(cx: &mut TestAppContext) {
    if !tools_available() {
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let database = dir.path().join("rushes.db");
    let fx = Fixture::with_store(cx, Store::open(&database).expect("database file"), &[], true);
    let stored = |database: &Path, id: RunId| {
        let store = Store::open(database).expect("open the database");
        (store.get_run(id).expect("read run"), store.list_run_events(id).expect("read log"))
    };

    // Another program holds the write lock, as a backup or sync tool does.
    let lock = rusqlite::Connection::open(&database).expect("second connection");
    lock.execute_batch("BEGIN EXCLUSIVE").expect("lock the database");
    let started = Instant::now();
    let id = fx.start(cx, false);
    assert!(started.elapsed() < Duration::from_secs(1), "starting took {:?}", started.elapsed());
    let service = fx.service.clone();
    wait_until(cx, "the strategy", move |cx| {
        service.read(cx).run(id).is_some_and(|run| run.status == RunStatus::AwaitingApproval)
            && service.read(cx).live_phase(id).is_none()
    });
    let run = fx.run(cx, id);
    let timeline = cx.read(|cx| fx.service.read(cx).timeline(id).to_vec());
    assert!(run.summary.is_some() && run.session_id.is_some(), "{run:?}");
    assert_eq!(tool_names(&timeline), ["Skill", "Bash", "Bash", "Bash", "Write"]);
    assert_eq!(stored(&database, id), (None, Vec::new()), "nothing is written during the lock");

    // The lock ends: the background writer's next attempt writes the run and its log.
    lock.execute_batch("COMMIT").expect("release the lock");
    let deadline = Instant::now() + TIMEOUT;
    while cx.read(|cx| fx.state.read(cx).has_unsaved_changes()) {
        assert!(Instant::now() < deadline, "timed out waiting for the saved edit");
        cx.executor().advance_clock(SAVE_RETRY_DELAY);
        cx.run_until_parked();
    }
    let (stored_run, log) = stored(&database, id);
    assert_eq!(stored_run, Some(run));
    assert_eq!(log.into_iter().map(TimelineEntry::from).collect::<Vec<_>>(), timeline);
}

#[gpui_kit::test]
fn an_edit_that_failed_while_planning_plans_again_with_feedback(cx: &mut TestAppContext) {
    if !tools_available() {
        return;
    }
    let fx = Fixture::new(cx, &[("FAKE_AGENT_MODE", "fail")]);
    let id = fx.start(cx, false);
    fx.wait_for_status(cx, id, RunStatus::Failed);

    // The agent works again; the creator continues the failed plan with feedback.
    let pid_file = fx.pid_file.display().to_string();
    cx.update(|cx| {
        fx.service.update(cx, |service, cx| {
            service.set_agent_env(vec![("FAKE_AGENT_PID_FILE".into(), pid_file)], cx);
            service.send_feedback(id, "Use the second take for the intro.".into(), cx)
        })
    })
    .expect("continued");
    let planned = fx.wait_for_status(cx, id, RunStatus::AwaitingApproval);
    assert!(planned.summary.expect("strategy").contains("**Strategy**"));
    assert!(!fx.final_cut().exists(), "nothing is rendered before the strategy is approved");
    assert_eq!(
        phases(&fx.stored_events(cx, id)),
        [(PhaseKind::Plan, None), (PhaseKind::Revise, Some("Use the second take for the intro.".into()))]
    );
}

#[gpui_kit::test]
fn a_render_that_leaves_an_old_final_cut_fails(cx: &mut TestAppContext) {
    if !tools_available() {
        return;
    }
    let fx = Fixture::new(cx, &[]);
    // A final cut from an earlier edit of the same footage.
    fs::create_dir_all(fx.final_cut().parent().unwrap()).unwrap();
    fs::copy(fx.footage.join("take1.mp4"), fx.final_cut()).unwrap();
    let lazy = lazy_agent(fx.root.path());
    cx.update(|cx| {
        fx.state.update(cx, |state, cx| {
            state.update_settings(cx, |settings| settings.pipeline.agent_command = lazy.display().to_string())
        })
    });

    let id = fx.start(cx, true);
    let failed = fx.wait_for_status(cx, id, RunStatus::Failed);
    assert!(failed.error.as_deref().is_some_and(|error| error.contains("without rendering")), "{failed:?}");
    assert_eq!(failed.output, None);
    assert_eq!(fx.upload_file(cx), None, "the old cut is not offered for upload");
    // Nobody referred to the old file: a render must not overwrite it, and it is no
    // AI edit's cut either, so it is set aside where no list of cuts offers it.
    let set_aside = fx.footage.join("edit/leftovers/final-1.mp4");
    assert_eq!(fs::read(set_aside).unwrap(), fs::read(fx.footage.join("take1.mp4")).unwrap());
    assert!(!fx.final_cut().exists());
    assert_eq!(rushes_pipeline::kept_cuts(&fx.footage), Vec::<PathBuf>::new());
}

#[gpui_kit::test]
fn a_cancelled_render_is_set_aside_and_never_offered_as_a_cut(cx: &mut TestAppContext) {
    if !tools_available() {
        return;
    }
    let fx = Fixture::new(cx, &[]);
    let id = fx.start(cx, true);
    fx.wait_for_status(cx, id, RunStatus::Succeeded);
    let rendered = |cx: &mut TestAppContext| cx.read(|cx| fx.service.read(cx).rendered_cuts(fx.video, cx));
    assert_eq!(rendered(cx), [fx.cut(1)]);

    // A change whose render is cancelled halfway through writing edit/final.mp4.
    let partial = partial_render_agent(fx.root.path());
    let set_agent = |cx: &mut TestAppContext, agent: &Path| {
        let agent = agent.display().to_string();
        cx.update(|cx| {
            fx.state
                .update(cx, |state, cx| state.update_settings(cx, |settings| settings.pipeline.agent_command = agent))
        });
    };
    set_agent(cx, &partial);
    cx.update(|cx| fx.service.update(cx, |service, cx| service.send_feedback(id, "A punchier ending.".into(), cx)))
        .expect("feedback sent");
    let final_cut = fx.final_cut();
    wait_until(cx, "half a render", |_| fs::metadata(&final_cut).is_ok_and(|meta| meta.len() == 40_000));
    cx.update(|cx| fx.service.update(cx, |service, cx| service.cancel(id, cx)));
    let stopped = fx.wait_for_status(cx, id, RunStatus::Succeeded);
    assert_eq!(stopped.output, Some(fx.cut(1)), "the cut from before is still the edit's");

    // Set aside at once: not at the shared path, not among the cuts.
    let leftover = fx.footage.join("edit/leftovers/final-1.mp4");
    assert_eq!(fs::metadata(&leftover).map(|meta| meta.len()).ok(), Some(40_000));
    assert!(!fx.final_cut().exists());
    assert_eq!(rushes_pipeline::kept_cuts(&fx.footage), [fx.cut(1)]);
    assert_eq!(rendered(cx), [fx.cut(1)]);

    // The next render is kept under the next number; the earlier render of the
    // edit stays on offer, and the log says which render each file is.
    set_agent(cx, &fake_agent());
    cx.update(|cx| fx.service.update(cx, |service, cx| service.send_feedback(id, "A punchier ending.".into(), cx)))
        .expect("feedback sent");
    let done = fx.wait_for_status(cx, id, RunStatus::Succeeded);
    assert_eq!(done.output, Some(fx.cut(2)));
    assert_eq!(rushes_pipeline::kept_cuts(&fx.footage), [fx.cut(2), fx.cut(1)]);
    assert_eq!(rendered(cx), [fx.cut(1), fx.cut(2)]);
    assert_eq!(markers(&fx.stored_events(cx, id)), ["Auto", "CutKept", "Feedback", "Cancelled", "Feedback", "CutKept"]);
    let kept: Vec<PathBuf> = fx
        .stored_events(cx, id)
        .into_iter()
        .filter_map(|entry| match entry.event {
            LogEvent::Marker(RunMarker { cut, .. }) => cut,
            _ => None,
        })
        .collect();
    assert_eq!(kept, [fx.cut(1), fx.cut(2)]);

    // A restart reads which cuts are the edit's from the log.
    cx.update(|cx| PipelineService::init(fx.state.clone(), cx));
    let service = cx.read(PipelineService::global);
    assert_eq!(cx.read(|cx| service.read(cx).rendered_cuts(fx.video, cx)), [fx.cut(1), fx.cut(2)]);
}

#[gpui_kit::test]
fn an_error_result_keeps_the_proposed_strategy(cx: &mut TestAppContext) {
    if !tools_available() {
        return;
    }
    let fx = Fixture::new(cx, &[]);
    let id = fx.start(cx, false);
    let strategy = fx.wait_for_status(cx, id, RunStatus::AwaitingApproval).summary.expect("strategy");

    let lazy = lazy_agent(fx.root.path());
    cx.update(|cx| {
        fx.state.update(cx, |state, cx| {
            state.update_settings(cx, |settings| settings.pipeline.agent_command = lazy.display().to_string())
        });
        fx.service.update(cx, |service, cx| service.set_agent_env(vec![("LAZY_IS_ERROR".into(), "1".into())], cx));
        fx.service.update(cx, |service, cx| service.send_feedback(id, "Shorter.".into(), cx))
    })
    .expect("feedback sent");
    // The revision failed; the strategy from before awaits approval again.
    let failed = fx.wait_for_status(cx, id, RunStatus::AwaitingApproval);
    assert_eq!(failed.error.as_deref(), Some("Reached the turn limit."));
    assert_eq!(failed.summary, Some(strategy), "the error does not replace the strategy");
}

#[gpui_kit::test]
fn a_cut_rendered_before_an_agent_error_is_kept(cx: &mut TestAppContext) {
    if !tools_available() {
        return;
    }
    let fx = Fixture::new(cx, &[]);
    let agent = render_then_error_agent(fx.root.path());
    cx.update(|cx| {
        fx.state.update(cx, |state, cx| {
            state.update_settings(cx, |settings| settings.pipeline.agent_command = agent.display().to_string())
        })
    });

    let id = fx.start(cx, true);
    let done = fx.wait_for_status(cx, id, RunStatus::Succeeded);
    assert_eq!(done.output, Some(fx.cut(1)), "the render is kept like any other");
    let error = done.error.expect("the agent's error is kept with it");
    assert!(error.starts_with(RENDERED_WITH_ERROR) && error.ends_with("It reached its turn limit."), "{error}");
    assert_eq!(done.summary, None, "no summary describes this cut");
    assert_eq!(fx.upload_file(cx), Some(fx.cut(1)));
    assert!(!fx.final_cut().exists(), "the next render can't overwrite it");

    // The same when the agent also exits with an error status.
    let pid_file = fx.pid_file.display().to_string();
    cx.update(|cx| {
        fx.service.update(cx, |service, cx| {
            service.set_agent_env(
                vec![("RENDER_THEN_EXIT".into(), "1".into()), ("FAKE_AGENT_PID_FILE".into(), pid_file)],
                cx,
            );
            service.send_feedback(id, "Again.".into(), cx)
        })
    })
    .expect("feedback sent");
    let again = fx.wait_for_status(cx, id, RunStatus::Succeeded);
    assert_eq!(again.output, Some(fx.cut(2)));
    assert!(again.error.is_some_and(|error| error.starts_with(RENDERED_WITH_ERROR)));
    // Both are the edit's cuts, offered for upload.
    let rendered = cx.read(|cx| fx.service.read(cx).rendered_cuts(fx.video, cx));
    assert_eq!(rendered, [fx.cut(1), fx.cut(2)]);
    let row = cx.read(|cx| fx.state.read(cx).row(fx.video).cloned().unwrap());
    assert_eq!(row.edit_status(), Some(RunStatus::Succeeded), "the edit reads as done");
}

#[gpui_kit::test]
fn a_change_that_fails_leaves_the_finished_edit_done(cx: &mut TestAppContext) {
    if !tools_available() {
        return;
    }
    let fx = Fixture::new(cx, &[]);
    let video = fx.video;
    let window = open(cx, move |window, cx| AiEditPanel::new(video, window, cx));
    let id = fx.start(cx, true);
    fx.wait_for_status(cx, id, RunStatus::Succeeded);
    wait_for_window(cx, window, "the output card", |window, _| shows(window, "run-use-for-upload"));

    // Iterating fails: the cut from before is still the edit's result.
    let pid_file = fx.pid_file.display().to_string();
    let agent_env = |fail: bool| {
        let mut env = vec![("FAKE_AGENT_PID_FILE".to_owned(), pid_file.clone())];
        if fail {
            env.push(("FAKE_AGENT_MODE".to_owned(), "fail".to_owned()));
        }
        env
    };
    cx.update(|cx| {
        fx.service.update(cx, |service, cx| {
            service.set_agent_env(agent_env(true), cx);
            service.send_feedback(id, "Make it shorter.".into(), cx)
        })
    })
    .expect("feedback sent");
    let run = fx.wait_for_status(cx, id, RunStatus::Succeeded);
    assert_eq!(run.output, Some(fx.cut(1)));
    assert!(run.error.as_deref().is_some_and(|error| error.contains("simulated failure")), "{run:?}");
    let row = cx.read(|cx| fx.state.read(cx).row(video).cloned().unwrap());
    assert_eq!((row.latest_run, row.edit_status()), (Some(RunStatus::Succeeded), Some(RunStatus::Succeeded)));
    wait_for_window(cx, window, "the cut and the failure", |window, _| {
        shows(window, "run-use-for-upload") && shows(window, "run-setback") && !shows(window, "run-stopped-step")
    });

    // Retry sends the same change again.
    cx.update(|cx| fx.service.update(cx, |service, cx| service.set_agent_env(agent_env(false), cx)));
    click(cx, window, "run-setback-retry");
    assert_eq!(fx.run(cx, id).status, RunStatus::Rendering);
    let done = fx.wait_for_status(cx, id, RunStatus::Succeeded);
    assert_eq!((done.output, done.error), (Some(fx.cut(2)), None));
    assert_eq!(markers(&fx.stored_events(cx, id)), ["Auto", "CutKept", "Feedback", "retry Feedback", "CutKept"]);
    wait_for_window(cx, window, "the new cut alone", |window, _| {
        shows(window, "run-use-for-upload") && !shows(window, "run-setback")
    });
}

#[gpui_kit::test]
fn startup_stops_an_agent_a_crashed_session_left_running(cx: &mut TestAppContext) {
    if !cfg!(target_os = "linux") {
        return;
    }
    use std::os::unix::process::CommandExt as _;
    // Agent process groups a crashed session left: one is the run's, the other has
    // its id but started at another time.
    let spawn = || Command::new("sleep").arg("60").process_group(0).spawn().expect("spawn sleep");
    let (leftover, stranger) = (spawn(), spawn());
    let (leftover_pid, stranger_pid) = (leftover.id(), stranger.id());
    let reap = |mut child: std::process::Child| {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || _ = tx.send(child.wait()));
        rx
    };
    let (leftover_exit, stranger_exit) = (reap(leftover), reap(stranger));

    let mut store = Store::open_in_memory().unwrap();
    let video = rushes_core::Video::new("Crashed while rendering");
    store.insert_video(&video).unwrap();
    let run = |status: RunStatus, output: Option<PathBuf>| PipelineRun {
        id: RunId::new(),
        video_id: video.id,
        status,
        brief: String::new(),
        footage_dir: PathBuf::from("/nonexistent"),
        session_id: Some("s".into()),
        harness: None,
        summary: None,
        output,
        error: None,
        cost_usd: None,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };
    let iterating = run(RunStatus::Rendering, Some(PathBuf::from("/nonexistent/edit/cuts/final-1.mp4")));
    let planning = run(RunStatus::Planning, None);
    let stranger_process = AgentProcess {
        start_ticks: AgentProcess::of(stranger_pid).start_ticks.map(|ticks| ticks + 1),
        ..AgentProcess::of(stranger_pid)
    };
    for (run, phase, process) in [
        (&iterating, PhaseKind::Feedback, AgentProcess::of(leftover_pid)),
        (&planning, PhaseKind::Plan, stranger_process),
    ] {
        store.insert_run(run).unwrap();
        let started = LogEvent::Phase(PhaseStarted { phase, text: None, retry: false });
        store.append_run_event(run.id, &started.to_payload()).unwrap();
        store.append_run_event(run.id, &LogEvent::Agent(AgentEvent::Spawned { process }).to_payload()).unwrap();
    }

    let fx = Fixture::with_store(cx, store, &[], false);
    let status = leftover_exit.recv_timeout(Duration::from_secs(5)).expect("the leftover agent was stopped").unwrap();
    assert!(!status.success());
    assert!(stranger_exit.try_recv().is_err(), "a process that only shares the id is left alone");
    _ = Command::new("kill").arg(stranger_pid.to_string()).status();

    let iterated = fx.stored_run(cx, iterating.id);
    assert_eq!(
        (iterated.status, iterated.error.as_deref()),
        (RunStatus::Succeeded, Some(INTERRUPTED)),
        "the cut stays"
    );
    let planned = fx.stored_run(cx, planning.id);
    assert_eq!((planned.status, planned.error.as_deref()), (RunStatus::Failed, Some(INTERRUPTED)));
}

#[gpui_kit::test]
fn deleting_a_video_whose_edit_runs_says_it_stops_the_agent(cx: &mut TestAppContext) {
    if !tools_available() {
        return;
    }
    let fx = Fixture::new(cx, &[("FAKE_AGENT_DELAY_MS", "1500")]);
    let other = cx.update(|cx| {
        fx.state
            .update(cx, |state, cx| state.create_video(NewVideo { title: "Other".into(), ..NewVideo::default() }, cx))
    });
    let other = other.expect("video created");
    assert_eq!(cx.read(|cx| delete_run_notice(&[fx.video], cx)), None);

    let id = fx.start(cx, false);
    let notice = cx.read(|cx| delete_run_notice(&[fx.video], cx));
    assert!(notice.is_some_and(|notice| notice.contains("stops the agent")), "{notice:?}");
    assert!(
        cx.read(|cx| delete_run_notice(&[fx.video, other], cx))
            .is_some_and(|notice| notice.starts_with("Some of them"))
    );
    assert_eq!(cx.read(|cx| delete_run_notice(&[other], cx)), None);

    cx.update(|cx| fx.service.update(cx, |service, cx| service.cancel(id, cx)));
    fx.wait_for_status(cx, id, RunStatus::Cancelled);
    assert_eq!(cx.read(|cx| delete_run_notice(&[fx.video], cx)), None, "a stopped edit has nothing to stop");
}

#[gpui_kit::test]
fn deleting_a_video_stops_its_edit_quietly(cx: &mut TestAppContext) {
    if !tools_available() {
        return;
    }
    let fx = Fixture::new(cx, &[("FAKE_AGENT_DELAY_MS", "400")]);
    let errors = Rc::new(RefCell::new(Vec::new()));
    let _errors = cx.update(|cx| {
        let errors = errors.clone();
        cx.subscribe(&fx.state, move |_, event: &AppEvent, _| {
            if let AppEvent::Error(message) = event {
                errors.borrow_mut().push(message.to_string());
            }
        })
    });
    let id = fx.start(cx, false);
    let pid_file = fx.pid_file.clone();
    wait_until(cx, "the agent", |_| !recorded_pids(&pid_file).is_empty());

    cx.update(|cx| fx.state.update(cx, |state, cx| state.delete_videos(&[fx.video], cx)));
    assert!(cx.read(|cx| fx.service.read(cx).runs().is_empty()), "the store deleted the run with its video");
    let service = fx.service.clone();
    wait_until(cx, "the agent to stop", |cx| service.read(cx).live_phase(id).is_none());
    let pids = recorded_pids(&fx.pid_file);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !pids.iter().all(|pid| process_gone(*pid)) {
        assert!(Instant::now() < deadline, "agent processes still running: {pids:?}");
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(*errors.borrow(), Vec::<String>::new());
}

// UI integration tests

/// Opens `build`'s view in a headless window tall enough for the whole panel.
fn open<V: Render>(
    cx: &mut TestAppContext,
    build: impl FnOnce(&mut Window, &mut Context<V>) -> V + 'static,
) -> AnyWindowHandle {
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::new(Default::default(), size(px(1280.), px(2400.))))),
        ..WindowOptions::default()
    };
    let (handle, _) = cx
        .update(|cx| gpui_kit::open_window(options, cx, |window, cx| cx.new(|cx| build(window, cx))))
        .expect("open test window");
    handle
}

/// Shows one view that a test can close or replace, as navigating does.
struct Host {
    view: Option<AnyView>,
}

impl Render for Host {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().size_full().children(self.view.clone())
    }
}

fn open_host(
    cx: &mut TestAppContext,
    build: impl FnOnce(&mut Window, &mut App) -> AnyView + 'static,
) -> (AnyWindowHandle, Entity<Host>) {
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::new(Default::default(), size(px(1280.), px(2400.))))),
        ..WindowOptions::default()
    };
    cx.update(|cx| {
        gpui_kit::open_window(options, cx, |window, cx| {
            let view = build(window, cx);
            cx.new(|_| Host { view: Some(view) })
        })
    })
    .expect("open test window")
}

/// Closes the hosted view, as navigating away does.
fn close(cx: &mut TestAppContext, host: &Entity<Host>) {
    cx.update(|cx| {
        host.update(cx, |host, cx| {
            host.view = None;
            cx.notify();
        })
    });
    cx.run_until_parked();
}

/// Hosts a new view, as navigating to a page does.
fn show(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    host: &Entity<Host>,
    build: impl FnOnce(&mut Window, &mut App) -> AnyView,
) {
    cx.update_window(window, |_, window, cx| {
        let view = build(window, cx);
        host.update(cx, |host, cx| {
            host.view = Some(view);
            cx.notify();
        });
    })
    .expect("window");
    cx.run_until_parked();
}

/// Like [`wait_until`], re-rendering the window and asking it each time.
fn wait_for_window(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    what: &str,
    mut done: impl FnMut(&mut Window, &mut App) -> bool,
) {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        cx.run_until_parked();
        let ready = cx
            .update_window(window, |_, window, cx| {
                window.render_frame(cx);
                done(window, cx)
            })
            .expect("window");
        if ready {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn click(cx: &mut TestAppContext, window: AnyWindowHandle, id: impl Into<ElementId>) {
    let id = id.into();
    cx.update_window(window, |_, window, cx| window.click(id, cx)).expect("window");
}

fn shows(window: &Window, id: &str) -> bool {
    window.try_find(ElementId::Name(id.to_owned().into())).is_some_and(|element| element.visible())
}

fn latest_run(cx: &mut TestAppContext, fx: &Fixture) -> Option<RunId> {
    cx.read(|cx| fx.service.read(cx).video_runs(fx.video).next().map(|run| run.id))
}

fn route(cx: &mut TestAppContext, fx: &Fixture) -> Route {
    cx.read(|cx| fx.state.read(cx).route().clone())
}

#[gpui_kit::test]
fn the_panel_takes_an_edit_from_brief_to_publish(cx: &mut TestAppContext) {
    if !tools_available() {
        return;
    }
    let fx = Fixture::new(cx, &[]);
    let video = fx.video;
    let window = open(cx, move |window, cx| AiEditPanel::new(video, window, cx));
    let service = fx.service.clone();
    wait_until(cx, "requirement check", |cx| service.read(cx).doctor().is_some_and(|doctor| doctor.ready()));
    wait_for_window(cx, window, "the composer", |window, _| shows(window, "ai-edit-start"));
    assert!(latest_run(cx, &fx).is_none());

    click(cx, window, "brief-subtitles");
    click(cx, window, "ai-edit-start");
    let id = latest_run(cx, &fx).expect("the panel started a run");
    let planned = fx.wait_for_status(cx, id, RunStatus::AwaitingApproval);
    assert!(planned.brief.contains("- Burn subtitles into the render."), "{}", planned.brief);
    assert!(planned.brief.starts_with("Edit “Desk setup 2026”"), "{}", planned.brief);

    wait_for_window(cx, window, "the strategy card", |window, _| shows(window, "run-approve"));
    let composing = cx.update_window(window, |_, window, _| shows(window, "ai-edit-start")).unwrap();
    assert!(!composing, "the brief form closes once the edit starts");
    click(cx, window, "run-approve");
    fx.wait_for_status(cx, id, RunStatus::Succeeded);

    wait_for_window(cx, window, "the output card", |window, _| shows(window, "run-use-for-upload"));
    click(cx, window, "run-use-for-upload");
    assert_eq!(route(cx, &fx), Route::Video { id: video, tab: VideoTab::Publish });
    assert_eq!(fx.upload_file(cx), Some(fx.cut(1)));
}

#[gpui_kit::test]
fn the_edit_cannot_start_without_footage(cx: &mut TestAppContext) {
    let fx = Fixture::with_store(cx, Store::open_in_memory().unwrap(), &[], false);
    let video = fx.video;
    let window = open(cx, move |window, cx| AiEditPanel::new(video, window, cx));
    let service = fx.service.clone();
    wait_until(cx, "requirement check", |cx| service.read(cx).doctor().is_some());
    wait_for_window(cx, window, "the composer", |window, _| shows(window, "ai-edit-start"));
    click(cx, window, "ai-edit-start");
    cx.run_until_parked();
    assert!(latest_run(cx, &fx).is_none(), "Start AI edit is disabled without footage");
}

#[gpui_kit::test]
fn requested_changes_revise_the_strategy(cx: &mut TestAppContext) {
    if !tools_available() {
        return;
    }
    let fx = Fixture::new(cx, &[]);
    let id = fx.start(cx, false);
    let video = fx.video;
    let window = open(cx, move |window, cx| AiEditPanel::new(video, window, cx));
    fx.wait_for_status(cx, id, RunStatus::AwaitingApproval);
    wait_for_window(cx, window, "the strategy card", |window, _| shows(window, "run-request-changes"));

    click(cx, window, "run-request-changes");
    cx.update_window(window, |_, window, cx| window.input("Add an end card.", cx)).unwrap();
    click(cx, window, "run-send-feedback");
    assert_eq!(fx.run(cx, id).status, RunStatus::Planning);
    fx.wait_for_status(cx, id, RunStatus::AwaitingApproval);
    assert_eq!(
        phases(&fx.stored_events(cx, id)),
        [(PhaseKind::Plan, None), (PhaseKind::Revise, Some("Add an end card.".into()))]
    );
}

#[gpui_kit::test]
fn a_discarded_strategy_can_be_restored(cx: &mut TestAppContext) {
    if !tools_available() {
        return;
    }
    let fx = Fixture::new(cx, &[]);
    let id = fx.start(cx, false);
    let video = fx.video;
    let window = open(cx, move |window, cx| AiEditPanel::new(video, window, cx));
    let strategy = fx.wait_for_status(cx, id, RunStatus::AwaitingApproval).summary;
    wait_for_window(cx, window, "the strategy card", |window, _| shows(window, "run-discard"));

    click(cx, window, "run-discard");
    assert_eq!(fx.run(cx, id).status, RunStatus::Cancelled);
    wait_for_window(cx, window, "the discarded card", |window, _| shows(window, "run-restore"));
    assert!(!cx.update_window(window, |_, window, _| shows(window, "run-continue")).unwrap());

    click(cx, window, "run-restore");
    let restored = fx.run(cx, id);
    assert_eq!((restored.status, restored.summary), (RunStatus::AwaitingApproval, strategy));
    wait_for_window(cx, window, "the strategy card", |window, _| shows(window, "run-approve"));
    assert_eq!(markers(&fx.stored_events(cx, id)), ["Plan", "Discarded", "Restored"]);
    assert!(latest_run(cx, &fx) == Some(id) && fx.stored_run(cx, id).status == RunStatus::AwaitingApproval);

    // An edit cancelled while the agent worked has no strategy to bring back.
    let restore = cx.update(|cx| fx.service.update(cx, |service, cx| service.restore(id, cx)));
    assert_eq!(restore, Err(RunError::NotAvailable(RunStatus::AwaitingApproval)));
}

#[gpui_kit::test]
fn a_cancelled_revision_offers_the_strategy_again(cx: &mut TestAppContext) {
    if !tools_available() {
        return;
    }
    let fx = Fixture::new(cx, &[]);
    let id = fx.start(cx, false);
    let strategy = fx.wait_for_status(cx, id, RunStatus::AwaitingApproval).summary;
    let video = fx.video;
    let window = open(cx, move |window, cx| AiEditPanel::new(video, window, cx));
    wait_for_window(cx, window, "the strategy card", |window, _| shows(window, "run-request-changes"));

    // A slow revision, cancelled while the agent works.
    let pid_file = fx.pid_file.display().to_string();
    cx.update(|cx| {
        fx.service.update(cx, |service, cx| {
            let env =
                vec![("FAKE_AGENT_DELAY_MS".into(), "1500".into()), ("FAKE_AGENT_PID_FILE".into(), pid_file.clone())];
            service.set_agent_env(env, cx)
        })
    });
    click(cx, window, "run-request-changes");
    cx.update_window(window, |_, window, cx| window.input("Shorter please.", cx)).unwrap();
    click(cx, window, "run-send-feedback");
    assert_eq!(fx.run(cx, id).status, RunStatus::Planning);
    wait_for_window(cx, window, "the live status", |window, _| shows(window, "run-cancel"));
    click(cx, window, "run-cancel");

    let run = fx.wait_for_status(cx, id, RunStatus::AwaitingApproval);
    assert_eq!((run.summary, run.error), (strategy, None), "the strategy from before awaits approval");
    assert_eq!(markers(&fx.stored_events(cx, id)), ["Plan", "Revise", "Cancelled"]);
    wait_for_window(cx, window, "the strategy and the notice", |window, _| {
        shows(window, "run-approve") && shows(window, "run-setback")
    });

    // Approving renders it without planning again.
    cx.update(|cx| {
        fx.service.update(cx, |service, cx| service.set_agent_env(vec![("FAKE_AGENT_PID_FILE".into(), pid_file)], cx))
    });
    click(cx, window, "run-approve");
    fx.wait_for_status(cx, id, RunStatus::Succeeded);
    assert_eq!(markers(&fx.stored_events(cx, id)), ["Plan", "Revise", "Cancelled", "Approve", "CutKept"]);
}

#[gpui_kit::test]
fn a_running_edit_can_be_cancelled_and_retried(cx: &mut TestAppContext) {
    if !tools_available() {
        return;
    }
    let fx = Fixture::new(cx, &[("FAKE_AGENT_DELAY_MS", "1500")]);
    let id = fx.start(cx, false);
    let video = fx.video;
    let window = open(cx, move |window, cx| AiEditPanel::new(video, window, cx));
    wait_for_window(cx, window, "the live status", |window, _| shows(window, "run-cancel"));

    click(cx, window, "run-cancel");
    fx.wait_for_status(cx, id, RunStatus::Cancelled);
    wait_for_window(cx, window, "the cancelled card", |window, _| shows(window, "run-retry"));
    assert!(cx.update_window(window, |_, window, _| shows(window, "run-continue")).unwrap(), "a session to resume");
    let stopped = cx.update_window(window, |_, window, _| shows(window, "run-stopped-step")).unwrap();
    assert!(stopped, "the stepper marks where the edit stopped");

    click(cx, window, "run-retry");
    assert_eq!(fx.run(cx, id).status, RunStatus::Planning);
    cx.update(|cx| fx.service.update(cx, |service, cx| service.cancel(id, cx)));
    fx.wait_for_status(cx, id, RunStatus::Cancelled);
}

#[gpui_kit::test]
fn the_agent_time_leaves_out_the_wait_for_approval(cx: &mut TestAppContext) {
    if !tools_available() {
        return;
    }
    let fx = Fixture::new(cx, &[]);
    let id = fx.start(cx, false);
    fx.wait_for_status(cx, id, RunStatus::AwaitingApproval);
    // The creator reads the strategy for a while.
    std::thread::sleep(Duration::from_millis(1500));
    cx.update(|cx| fx.service.update(cx, |service, cx| service.approve(id, None, cx))).expect("approved");
    let done = fx.wait_for_status(cx, id, RunStatus::Succeeded);

    let reported: u64 = fx
        .stored_events(cx, id)
        .iter()
        .filter_map(|entry| match &entry.event {
            LogEvent::Agent(AgentEvent::Result { duration_ms, .. }) => *duration_ms,
            _ => None,
        })
        .sum();
    let wall_clock = (done.updated_at - done.created_at).to_std().unwrap();
    let agent_time = cx.read(|cx| fx.service.read(cx).agent_time(id, cx));
    assert_eq!(agent_time, Duration::from_millis(reported), "the phases as the agent reported them");
    assert!(agent_time + Duration::from_millis(1400) < wall_clock, "{agent_time:?} of {wall_clock:?}");

    // The same from the stored log after a restart.
    cx.update(|cx| PipelineService::init(fx.state.clone(), cx));
    let restarted = cx.read(|cx| PipelineService::global(cx).read(cx).agent_time(id, cx));
    assert_eq!(restarted, agent_time);
}

#[gpui_kit::test]
fn the_empty_pipelines_page_leads_to_the_videos(cx: &mut TestAppContext) {
    let fx = Fixture::new(cx, &[]);
    let window = open(cx, PipelinesPage::new);
    cx.update(|cx| fx.state.update(cx, |state, cx| state.navigate(Route::Pipelines, cx)));
    // The same empty state as Videos and Uploads, with the next step.
    click(cx, window, "pipelines-go-to-videos");
    assert_eq!(route(cx, &fx), Route::default());
}

#[gpui_kit::test]
fn a_pipelines_row_opens_its_run(cx: &mut TestAppContext) {
    if !tools_available() {
        return;
    }
    let fx = Fixture::new(cx, &[]);
    let id = fx.start(cx, true);
    fx.wait_for_status(cx, id, RunStatus::Succeeded);
    let window = open(cx, PipelinesPage::new);
    let row = format!("pipeline-run-{id}");
    wait_for_window(cx, window, "the run row", |window, _| shows(window, &row));

    // The keyboard reaches the rows like buttons: Tab (the root's `focus_next`), then Enter.
    cx.update_window(window, |_, window, cx| {
        window.focus_next(cx);
        window.press("enter", cx);
    })
    .unwrap();
    assert_eq!(route(cx, &fx), Route::Video { id: fx.video, tab: VideoTab::Footage });
    cx.update(|cx| fx.service.update(cx, |service, _| service.take_revealed(fx.video)));

    cx.update(|cx| fx.state.update(cx, |state, cx| state.navigate(Route::Pipelines, cx)));
    click(cx, window, ElementId::Name(row.into()));
    assert_eq!(route(cx, &fx), Route::Video { id: fx.video, tab: VideoTab::Footage });
    let revealed = cx.update(|cx| fx.service.update(cx, |service, _| service.take_revealed(fx.video)));
    assert_eq!(revealed, Some(id), "the video's panel selects the clicked run");
}

#[gpui_kit::test]
fn settings_save_toggles_at_once_and_text_after_a_pause(cx: &mut TestAppContext) {
    let fx = Fixture::with_store(cx, Store::open_in_memory().unwrap(), &[], false);
    let window = open(cx, AiSettings::new);
    let pipeline = |cx: &mut TestAppContext| cx.read(|cx| fx.state.read(cx).settings().pipeline.clone());
    assert!(!pipeline(cx).auto_approve);

    click(cx, window, "ai-settings-auto-approve");
    assert!(pipeline(cx).auto_approve);
    click(cx, window, "ai-settings-subtitles");
    assert!(pipeline(cx).default_preferences.subtitles);

    let before = pipeline(cx).agent_command;
    click(cx, window, "ai-settings-agent-command");
    cx.update_window(window, |_, window, cx| {
        window.press("secondary-a", cx);
        window.input("/opt/claude/bin/claude", cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(pipeline(cx).agent_command, before, "typing is saved after a pause");
    cx.executor().advance_clock(Duration::from_secs(1));
    cx.run_until_parked();
    assert_eq!(pipeline(cx).agent_command, "/opt/claude/bin/claude");
    let service = fx.service.clone();
    wait_until(cx, "the new check", |cx| !service.read(cx).is_checking());
    let doctor = cx.read(|cx| service.read(cx).doctor().cloned()).expect("doctor");
    assert!(!doctor.agent.ok, "the new agent command was checked: {:?}", doctor.agent);
}

#[gpui_kit::test]
fn a_skill_folder_typed_with_a_tilde_is_found_in_the_home_folder(cx: &mut TestAppContext) {
    let fx = Fixture::with_store(cx, Store::open_in_memory().unwrap(), &[], false);
    let window = open(cx, AiSettings::new);
    let pipeline = |cx: &mut TestAppContext| cx.read(|cx| fx.state.read(cx).settings().pipeline.clone());
    let saved = pipeline(cx).skill_dir;
    let type_folder = |cx: &mut TestAppContext, text: &str| {
        click(cx, window, "ai-settings-skill-dir");
        cx.update_window(window, |_, window, cx| {
            window.press("secondary-a", cx);
            window.input(text, cx);
        })
        .unwrap();
        cx.executor().advance_clock(Duration::from_secs(1));
        cx.run_until_parked();
    };

    // A relative path would resolve against wherever the app was started from.
    type_folder(cx, "video-use");
    assert_eq!(pipeline(cx).skill_dir, saved, "the unusable path is not saved");
    wait_for_window(cx, window, "the reason", |window, _| shows(window, "ai-settings-skill-dir-error"));

    type_folder(cx, "~/video_editing/video-use");
    let home = std::env::home_dir().expect("home folder");
    assert_eq!(pipeline(cx).skill_dir, Some(home.join("video_editing/video-use")));
}

#[gpui_kit::test]
fn takes_dropped_on_the_footage_card_are_copied_in(cx: &mut TestAppContext) {
    let fx = Fixture::with_store(cx, Store::open_in_memory().unwrap(), &[], false);
    let card = fx.root.path().join("sd-card");
    fs::create_dir_all(&card).unwrap();
    let take = card.join("take9.mp4");
    fs::write(&take, vec![0u8; 3 * 1024 * 1024]).unwrap();
    let notes = card.join("notes.txt");
    fs::write(&notes, "not footage").unwrap();
    let video = fx.video;
    let window = open(cx, move |window, cx| AiEditPanel::new(video, window, cx));
    wait_for_window(cx, window, "the footage card", |window, _| shows(window, "footage-drop"));

    cx.update_window(window, |_, window, cx| {
        let position = window.find("footage-drop").bounds().center();
        let paths = ExternalPaths(vec![take.clone(), notes.clone()].into());
        window.dispatch_event(PlatformInput::FileDrop(FileDropEvent::Entered { position, paths }), cx);
        window.dispatch_event(PlatformInput::FileDrop(FileDropEvent::Submit { position }), cx);
    })
    .unwrap();
    let copied = fx.footage.join("take9.mp4");
    wait_until(cx, "the copied take", |_| fs::metadata(&copied).is_ok_and(|meta| meta.len() == 3 * 1024 * 1024));
    assert!(take.is_file(), "the source is copied, not moved");
    assert!(!fx.footage.join("notes.txt").exists(), "only media files are imported");
}

#[gpui_kit::test]
fn text_typed_right_before_leaving_the_settings_is_saved(cx: &mut TestAppContext) {
    let fx = Fixture::with_store(cx, Store::open_in_memory().unwrap(), &[], false);
    let (window, settings) = open_host(cx, |window, cx| cx.new(|cx| AiSettings::new(window, cx)).into());
    click(cx, window, "ai-settings-agent-command");
    cx.update_window(window, |_, window, cx| {
        window.press("secondary-a", cx);
        window.input("/opt/claude/bin/claude", cx);
    })
    .unwrap();
    cx.run_until_parked();

    close(cx, &settings);
    let pipeline = cx.read(|cx| fx.state.read(cx).settings().pipeline.clone());
    assert_eq!(pipeline.agent_command, "/opt/claude/bin/claude");
}

#[gpui_kit::test]
fn the_brief_form_survives_leaving_the_video(cx: &mut TestAppContext) {
    if !tools_available() {
        return;
    }
    let fx = Fixture::new(cx, &[]);
    let video = fx.video;
    let panel =
        move |window: &mut Window, cx: &mut App| -> AnyView { cx.new(|cx| AiEditPanel::new(video, window, cx)).into() };
    let (window, page) = open_host(cx, panel);
    let service = fx.service.clone();
    wait_until(cx, "requirement check", |cx| service.read(cx).doctor().is_some_and(|doctor| doctor.ready()));
    wait_for_window(cx, window, "the composer", |window, _| shows(window, "ai-edit-start"));
    click(cx, window, "brief-subtitles");
    click(cx, window, "brief-auto-approve");

    // Away to another page and back.
    close(cx, &page);
    show(cx, window, &page, panel);
    wait_for_window(cx, window, "the composer", |window, _| shows(window, "ai-edit-start"));
    click(cx, window, "ai-edit-start");
    let id = latest_run(cx, &fx).expect("the panel started a run");
    let run = fx.run(cx, id);
    assert!(run.brief.contains("- Burn subtitles into the render."), "{}", run.brief);
    assert_eq!(run.status, RunStatus::Rendering, "the strategy review stays skipped");
    fx.wait_for_status(cx, id, RunStatus::Succeeded);
}

#[gpui_kit::test]
fn a_preference_changes_only_its_own_line_of_the_brief(cx: &mut TestAppContext) {
    let fx = Fixture::with_store(cx, Store::open_in_memory().unwrap(), &[], false);
    let video = fx.video;
    let (window, host) = open_host(cx, move |window, cx| cx.new(|cx| AiEditPanel::new(video, window, cx)).into());
    let panel: Entity<AiEditPanel> = cx.read(|cx| host.read(cx).view.clone().unwrap().downcast().unwrap());
    let brief = |cx: &mut TestAppContext| cx.read(|cx| panel.read(cx).brief_text(cx));
    wait_for_window(cx, window, "the composer", |window, _| shows(window, "brief"));
    let sponsor = "- Mention the sponsor at the end.";

    // The natural place for an instruction: one more bullet at the end of the list,
    // on the line the brief ends with.
    click(cx, window, "brief");
    cx.update_window(window, |_, window, cx| {
        window.press("ctrl-end", cx);
        window.input(sponsor, cx);
    })
    .unwrap();
    let typed = brief(cx);
    assert!(typed.ends_with(&format!("- No colour grade: keep the original look.\n{sponsor}")), "{typed}");

    click(cx, window, "brief-remove-fillers");
    click(cx, window, "brief-subtitles");
    let toggled = brief(cx);
    assert!(toggled.contains(sponsor), "the creator's line stays: {toggled}");
    assert!(toggled.contains("- Keep filler words and natural pauses.\n- Burn subtitles into the render.\n"));
    assert_eq!(toggled.lines().count(), typed.lines().count(), "one line for one line");

    // The brief's undo takes the changes back one at a time, the creator's typing last.
    click(cx, window, "brief");
    cx.update_window(window, |_, window, cx| window.press("ctrl-z", cx)).unwrap();
    assert!(brief(cx).contains("- No subtitles.") && brief(cx).contains(sponsor), "{}", brief(cx));
    cx.update_window(window, |_, window, cx| window.press("ctrl-z", cx)).unwrap();
    assert!(brief(cx).contains("- Remove filler words") && brief(cx).contains(sponsor), "{}", brief(cx));
    let preferences = cx.read(|cx| panel.read(cx).preferences().clone());
    assert!(preferences.remove_fillers && !preferences.subtitles, "the checkboxes follow the brief: {preferences:?}");
    cx.update_window(window, |_, window, cx| window.press("ctrl-z", cx)).unwrap();
    assert!(!brief(cx).contains(sponsor), "{}", brief(cx));
}

#[gpui_kit::test]
fn an_edited_brief_survives_a_restart(cx: &mut TestAppContext) {
    let fx = Fixture::with_store(cx, Store::open_in_memory().unwrap(), &[], false);
    let video = fx.video;
    let panel =
        move |window: &mut Window, cx: &mut App| -> AnyView { cx.new(|cx| AiEditPanel::new(video, window, cx)).into() };
    let (window, host) = open_host(cx, panel);
    let brief = |cx: &mut TestAppContext| {
        let panel: Entity<AiEditPanel> = cx.read(|cx| host.read(cx).view.clone().unwrap().downcast().unwrap());
        cx.read(|cx| panel.read(cx).brief_text(cx))
    };
    wait_for_window(cx, window, "the composer", |window, _| shows(window, "brief"));
    click(cx, window, "brief");
    cx.update_window(window, |_, window, cx| {
        window.press("ctrl-end", cx);
        window.input(" Keep it under ten minutes.", cx);
    })
    .unwrap();
    click(cx, window, "brief-subtitles");
    let edited = brief(cx);
    // Saved once typing pauses, before the app could be quit or crash.
    cx.executor().advance_clock(Duration::from_secs(1));
    cx.run_until_parked();

    // Rushes starts again on the same database.
    close(cx, &host);
    cx.update(|cx| PipelineService::init(fx.state.clone(), cx));
    show(cx, window, &host, panel);
    wait_for_window(cx, window, "the composer", |window, _| shows(window, "brief"));
    assert_eq!(brief(cx), edited);
    assert!(edited.contains("Keep it under ten minutes.") && edited.contains("- Burn subtitles into the render."));

    // It is still the creator's: a script change only notes that it is out of date.
    cx.update(|cx| {
        fx.state.update(cx, |state, cx| {
            let mut script = state.ensure_document(video, DocumentKind::Script, cx).expect("script");
            script.blocks = vec![Block::paragraph("A new line.")];
            state.save_document(script, cx);
        })
    });
    wait_for_window(cx, window, "the notice", |window, _| shows(window, "brief-stale"));
    assert_eq!(brief(cx), edited);
}

#[gpui_kit::test]
fn a_brief_written_while_another_program_locks_the_database_survives_a_quit(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().expect("tempdir");
    let database = dir.path().join("rushes.db");
    let fx = Fixture::with_store(cx, Store::open(&database).expect("database file"), &[], false);
    let errors = Rc::new(RefCell::new(Vec::new()));
    let sink = errors.clone();
    cx.update(|cx| {
        cx.subscribe(&fx.state, move |_, event: &AppEvent, _| {
            if let AppEvent::Error(message) = event {
                sink.borrow_mut().push(message.to_string());
            }
        })
        .detach()
    });
    let video = fx.video;
    let (window, _host) = open_host(cx, move |window, cx| cx.new(|cx| AiEditPanel::new(video, window, cx)).into());
    wait_for_window(cx, window, "the composer", |window, _| shows(window, "brief"));

    // Another program holds the write lock, as a backup or sync tool does.
    let lock = rusqlite::Connection::open(&database).expect("second connection");
    lock.execute_batch("BEGIN EXCLUSIVE").expect("lock the database");
    click(cx, window, "brief");
    cx.update_window(window, |_, window, cx| {
        window.press("ctrl-end", cx);
        window.input(" Keep the bloopers.", cx);
    })
    .unwrap();
    // The pause after typing saves the brief: it joins the write queue, as every
    // other change does, instead of waiting for the lock and giving up.
    cx.executor().advance_clock(Duration::from_secs(1));
    cx.run_until_parked();
    let drafts = |state: &AppState| state.read_setting::<serde_json::Value>("ai_edit.brief_drafts");
    let queued = cx.read(|cx| drafts(fx.state.read(cx))).expect("read drafts").expect("drafts");
    assert!(queued.to_string().contains("Keep the bloopers."), "{queued}");
    assert!(cx.read(|cx| fx.state.read(cx).has_unsaved_changes()), "queued until the lock ends");
    assert!(!errors.borrow().iter().any(|error| error.contains("brief")), "{:?}", errors.borrow());

    // Quitting during the lock keeps it for the next start.
    cx.update(|cx| cx.shutdown());
    let unsaved = fs::read_to_string(dir.path().join("unsaved-changes.json")).expect("kept changes");
    assert!(unsaved.contains("ai_edit.brief_drafts") && unsaved.contains("Keep the bloopers."), "{unsaved}");
    lock.execute_batch("COMMIT").expect("release the lock");

    let next = AppState::new(Store::open(&database).expect("open again")).expect("load state");
    let restored = drafts(&next).expect("read drafts").expect("drafts");
    assert!(restored.to_string().contains("Keep the bloopers."), "{restored}");
}

#[gpui_kit::test]
fn the_brief_follows_the_video_until_the_creator_edits_it(cx: &mut TestAppContext) {
    if !tools_available() {
        return;
    }
    let fx = Fixture::new(cx, &[]);
    let video = fx.video;
    let write_script = |cx: &mut TestAppContext, text: &str| {
        cx.update(|cx| {
            fx.state.update(cx, |state, cx| {
                let mut script = state.ensure_document(video, DocumentKind::Script, cx).expect("script");
                script.blocks = vec![Block::paragraph(text)];
                state.save_document(script, cx);
            })
        });
        cx.run_until_parked();
    };
    let start = |cx: &mut TestAppContext, window: AnyWindowHandle| -> String {
        wait_for_window(cx, window, "the composer", |window, _| shows(window, "ai-edit-start"));
        click(cx, window, "ai-edit-start");
        let id = latest_run(cx, &fx).expect("the panel started a run");
        let brief = fx.run(cx, id).brief;
        fx.wait_for_status(cx, id, RunStatus::AwaitingApproval);
        click(cx, window, "ai-edit-new");
        brief
    };
    write_script(cx, "The template's opening.");
    let panel =
        move |window: &mut Window, cx: &mut App| -> AnyView { cx.new(|cx| AiEditPanel::new(video, window, cx)).into() };
    let (window, page) = open_host(cx, panel);
    let service = fx.service.clone();
    wait_until(cx, "requirement check", |cx| service.read(cx).doctor().is_some_and(|doctor| doctor.ready()));

    // The script changes while the tab is open, the hook while the page is closed.
    write_script(cx, "Creators who film with one camera.");
    close(cx, &page);
    cx.update(|cx| {
        fx.state.update(cx, |state, cx| {
            let mut changed = state.video(video).unwrap().clone();
            changed.hook = "One camera is enough.".into();
            state.update_video(changed, cx);
        })
    });
    show(cx, window, &page, panel);
    let brief = start(cx, window);
    assert!(brief.contains("Creators who film with one camera.") && !brief.contains("template's"), "{brief}");
    assert!(brief.contains("Hook: One camera is enough."), "{brief}");

    // Once the creator edits it, the brief keeps their words and says it is out of date.
    wait_for_window(cx, window, "the composer", |window, _| shows(window, "brief"));
    click(cx, window, "brief");
    cx.update_window(window, |_, window, cx| window.input("Keep my words. ", cx)).unwrap();
    write_script(cx, "A newer line.");
    wait_for_window(cx, window, "the notice", |window, _| shows(window, "brief-stale"));
    let brief = start(cx, window);
    assert!(brief.contains("Keep my words.") && !brief.contains("A newer line."), "{brief}");

    // Reset drafts it from the video again.
    wait_for_window(cx, window, "the composer", |window, _| shows(window, "brief-reset"));
    click(cx, window, "brief-reset");
    assert!(!cx.update_window(window, |_, window, _| shows(window, "brief-stale")).unwrap());
    let brief = start(cx, window);
    assert!(brief.contains("A newer line.") && !brief.contains("Keep my words."), "{brief}");
}

#[gpui_kit::test]
fn a_long_activity_log_lays_out_only_the_rows_in_view(cx: &mut TestAppContext) {
    let fx = Fixture::with_store(cx, Store::open_in_memory().unwrap(), &[], false);
    let now = Utc::now();
    let run = PipelineRun {
        id: RunId::new(),
        video_id: fx.video,
        status: RunStatus::Succeeded,
        brief: String::new(),
        footage_dir: fx.footage.clone(),
        session_id: Some("s".into()),
        harness: None,
        summary: Some("Rendered.".into()),
        output: None,
        error: None,
        cost_usd: None,
        created_at: now,
        updated_at: now,
    };
    cx.read(|cx| {
        let store = fx.state.read(cx).store();
        let mut store = store.lock().unwrap();
        store.insert_run(&run).unwrap();
        for ix in 0..1000 {
            let event = if ix % 2 == 0 {
                AgentEvent::ToolUse {
                    id: format!("t{ix}"),
                    name: "Bash".into(),
                    summary: format!("ffmpeg -i take{ix}.mp4"),
                }
            } else {
                AgentEvent::ToolResult { tool_use_id: format!("t{}", ix - 1), is_error: false, summary: "ok".into() }
            };
            store.append_run_event(run.id, &LogEvent::Agent(event).to_payload()).unwrap();
        }
    });
    // A fresh service reads the run the store now holds.
    cx.update(|cx| PipelineService::init(fx.state.clone(), cx));
    let video = fx.video;
    let window = open(cx, move |window, cx| AiEditPanel::new(video, window, cx));
    let (first, last) = (format!("timeline-1-{}", run.id), format!("timeline-999-{}", run.id));
    wait_for_window(cx, window, "the first activity rows", |window, _| shows(window, &first));
    assert!(!cx.update_window(window, |_, window, _| shows(window, &last)).unwrap(), "rows far below are not laid out");
}

/// The panel inside a page that scrolls, as on the video page.
struct ScrollPage {
    panel: Entity<AiEditPanel>,
    scroll: ScrollHandle,
}

impl Render for ScrollPage {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().id("page").size_full().overflow_y_scroll().track_scroll(&self.scroll).child(self.panel.clone())
    }
}

#[gpui_kit::test]
fn the_wheel_scrolls_back_through_a_log_that_followed_a_live_edit(cx: &mut TestAppContext) {
    if !tools_available() {
        return;
    }
    let fx = Fixture::new(cx, &[]);
    let chatty = chatty_agent(fx.root.path());
    cx.update(|cx| {
        fx.state.update(cx, |state, cx| {
            state.update_settings(cx, |settings| settings.pipeline.agent_command = chatty.display().to_string())
        })
    });
    let video = fx.video;
    let scroll = ScrollHandle::new();
    let page_scroll = scroll.clone();
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::new(Default::default(), size(px(1280.), px(900.))))),
        ..WindowOptions::default()
    };
    let (window, _) = cx
        .update(|cx| {
            gpui_kit::open_window(options, cx, move |window, cx| {
                let panel = cx.new(|cx| AiEditPanel::new(video, window, cx));
                cx.new(|_| ScrollPage { panel, scroll: page_scroll })
            })
        })
        .expect("open test window");
    // The panel watches the edit from its start, so its log follows the newest rows.
    let id = fx.start(cx, true);
    fx.wait_for_status(cx, id, RunStatus::Failed);
    let events = fx.stored_events(cx, id);
    let last = format!("timeline-{}-{id}", events.last().expect("events").seq);
    let summary_seq = events
        .iter()
        .rev()
        .find(|entry| matches!(entry.event, LogEvent::Agent(AgentEvent::AssistantText { .. })))
        .expect("the summary")
        .seq;
    let wheel = |cx: &mut TestAppContext, target: &str, lines: f32| {
        let target = target.to_owned();
        cx.update_window(window, |_, window, cx| window.scroll(target, ScrollDelta::Lines(point(0., lines)), cx))
            .unwrap();
        cx.run_until_parked();
    };
    let visible = |cx: &mut TestAppContext, id: &str| {
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            shows(window, id)
        })
        .unwrap()
    };

    // Down the page to the activity box.
    wheel(cx, "footage-drop", -100.);
    wait_for_window(cx, window, "the end of the log", |window, _| shows(window, &last));
    let page_end = scroll.offset();
    assert!(page_end.y < px(0.), "the page scrolled");

    // The final summary shows its first lines; expanded, it is taller than the box.
    let toggle = format!("message-toggle-{summary_seq}-{id}");
    assert!(visible(cx, &toggle), "a long message offers Show more");
    click(cx, window, ElementId::Name(toggle.into()));
    let summary_row = ElementId::Name(format!("timeline-{summary_seq}-{id}").into());
    wait_for_window(cx, window, "the whole summary", |window, _| {
        window.find(summary_row.clone()).bounds().size.height > window.find("run-activity").bounds().size.height
    });

    // Back to the end, where the log follows its newest rows again, then up.
    wheel(cx, "run-activity", -500.);
    assert!(visible(cx, &last), "the wheel reaches the end of the log");
    wheel(cx, "run-activity", 5.);
    assert!(!visible(cx, &last), "wheeling up leaves the end of the log");
    assert!(!visible(cx, &last), "and stays there");
    assert_eq!(scroll.offset(), page_end, "the log takes the wheel, not the page");

    // At the start of the log, the wheel moves the page.
    wheel(cx, "run-activity", 2000.);
    assert_eq!(scroll.offset(), page_end, "the log scrolls to its start first");
    wheel(cx, "run-activity", 3.);
    assert!(scroll.offset().y > page_end.y, "then the page scrolls up");
}

#[gpui_kit::test]
fn a_short_activity_log_takes_only_the_room_it_needs(cx: &mut TestAppContext) {
    let fx = Fixture::with_store(cx, Store::open_in_memory().unwrap(), &[], false);
    let now = Utc::now();
    let run = PipelineRun {
        id: RunId::new(),
        video_id: fx.video,
        status: RunStatus::Failed,
        brief: String::new(),
        footage_dir: fx.footage.clone(),
        session_id: None,
        harness: None,
        summary: None,
        output: None,
        error: Some("The agent stopped.".into()),
        cost_usd: None,
        created_at: now,
        updated_at: now,
    };
    cx.read(|cx| {
        let store = fx.state.read(cx).store();
        let mut store = store.lock().unwrap();
        store.insert_run(&run).unwrap();
        let events = [
            LogEvent::Phase(PhaseStarted { phase: PhaseKind::Plan, text: None, retry: false }),
            LogEvent::Agent(AgentEvent::AssistantText { text: "Looking at the footage.".into() }),
            LogEvent::Agent(AgentEvent::ToolUse { id: "t1".into(), name: "Bash".into(), summary: "ls".into() }),
        ];
        for event in events {
            store.append_run_event(run.id, &event.to_payload()).unwrap();
        }
    });
    cx.update(|cx| PipelineService::init(fx.state.clone(), cx));
    let video = fx.video;
    let window = open(cx, move |window, cx| AiEditPanel::new(video, window, cx));
    let last = format!("timeline-3-{}", run.id);
    wait_for_window(cx, window, "the log", |window, _| shows(window, &last));
    wait_for_window(cx, window, "the box to fit the log", |window, _| {
        let row = window.find(ElementId::Name(last.clone().into())).bounds();
        let log = window.find("run-activity").bounds();
        log.size.height < px(200.) && row.bottom() <= log.bottom()
    });
}

/// A check of what a frame drew.
type FrameCheck = dyn Fn(&Window, &App);

/// The AI edit panel, with a check that runs after each frame it draws.
struct CheckedPanel {
    panel: Entity<AiEditPanel>,
    check: Rc<FrameCheck>,
}

impl Render for CheckedPanel {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let check = self.check.clone();
        div().child(self.panel.clone()).child(canvas(|_, _, _| {}, move |_, _, window, cx| check(window, cx)))
    }
}

#[gpui_kit::test]
fn rows_that_come_in_leave_the_rows_above_in_place(cx: &mut TestAppContext) {
    if !tools_available() {
        return;
    }
    const STEPS: usize = 6;
    let fx = Fixture::new(cx, &[]);
    let gate = fx.root.path().join("gate");
    fs::create_dir_all(&gate).unwrap();
    let agent = stepping_agent(fx.root.path(), &gate, STEPS);
    cx.update(|cx| {
        fx.state.update(cx, |state, cx| {
            state.update_settings(cx, |settings| settings.pipeline.agent_command = agent.display().to_string())
        })
    });

    // In every frame that shows the box, the first row stays where it first was and the
    // box is no taller than the rows in it.
    let checked_frames = Rc::new(Cell::new(0));
    let check = {
        let (service, video, checked_frames) = (fx.service.clone(), fx.video, checked_frames.clone());
        let first_row_offset = Cell::new(None);
        move |window: &Window, cx: &App| {
            let Some(id) = service.read(cx).video_runs(video).next().map(|run| run.id) else { return };
            if !shows(window, "run-activity") {
                return;
            }
            let log = window.find("run-activity").bounds();
            let first = window.try_find(ElementId::Name(format!("timeline-1-{id}").into()));
            let first = first.filter(|first| first.visible()).expect("the first row stays in view");
            let offset = first.bounds().top() - log.top();
            let expected = first_row_offset.get().unwrap_or(offset);
            first_row_offset.set(Some(expected));
            assert!((offset - expected).abs() < px(0.5), "the first row moved from {expected:?} to {offset:?}");
            let rows_bottom = (1..=STEPS as u32 + 10)
                .filter_map(|seq| window.try_find(ElementId::Name(format!("timeline-{seq}-{id}").into())))
                .map(|row| row.bounds().bottom())
                .fold(log.top(), Pixels::max);
            // Below the last row, only the log's padding (0.5rem).
            assert!(log.bottom() <= rows_bottom + px(8.5), "the box is taller than its rows");
            checked_frames.set(checked_frames.get() + 1);
        }
    };
    let video = fx.video;
    let window = open(cx, move |window, cx| CheckedPanel {
        panel: cx.new(|cx| AiEditPanel::new(video, window, cx)),
        check: Rc::new(check),
    });
    let id = fx.start(cx, false);
    let timeline_len = |cx: &mut TestAppContext| cx.read(|cx| fx.service.read(cx).timeline(id).len());
    let render = |cx: &mut TestAppContext| cx.update_window(window, |_, window, cx| window.render_frame(cx)).unwrap();

    render(cx);
    render(cx);
    for step in 0..STEPS {
        let len = timeline_len(cx);
        fs::write(gate.join(step.to_string()), "").unwrap();
        wait_until(cx, "the next row", |cx| fx.service.read(cx).timeline(id).len() > len);
        render(cx);
    }
    fx.wait_for_status(cx, id, RunStatus::AwaitingApproval);
    assert!(checked_frames.get() > STEPS, "the box showed in {} frames", checked_frames.get());
}

#[gpui_kit::test]
fn the_activity_log_follows_the_zoom(cx: &mut TestAppContext) {
    let fx = Fixture::with_store(cx, Store::open_in_memory().unwrap(), &[], false);
    let now = Utc::now();
    let run = PipelineRun {
        id: RunId::new(),
        video_id: fx.video,
        status: RunStatus::Failed,
        brief: String::new(),
        footage_dir: fx.footage.clone(),
        session_id: None,
        harness: None,
        summary: None,
        output: None,
        error: Some("The agent stopped.".into()),
        cost_usd: None,
        created_at: now,
        updated_at: now,
    };
    cx.read(|cx| {
        let store = fx.state.read(cx).store();
        let mut store = store.lock().unwrap();
        store.insert_run(&run).unwrap();
        let events = [
            LogEvent::Phase(PhaseStarted { phase: PhaseKind::Plan, text: None, retry: false }),
            LogEvent::Agent(AgentEvent::AssistantText { text: "Looking at the footage.".into() }),
            LogEvent::Agent(AgentEvent::ToolUse { id: "t1".into(), name: "Bash".into(), summary: "ls".into() }),
        ];
        for event in events {
            store.append_run_event(run.id, &event.to_payload()).unwrap();
        }
    });
    cx.update(|cx| PipelineService::init(fx.state.clone(), cx));
    let video = fx.video;
    let window = open(cx, move |window, cx| AiEditPanel::new(video, window, cx));
    let rows: Vec<ElementId> =
        (1..=3).map(|seq| ElementId::Name(format!("timeline-{seq}-{}", run.id).into())).collect();
    let bounds = |window: &mut Window| rows.iter().map(|row| window.find(row.clone()).bounds()).collect::<Vec<_>>();
    wait_for_window(cx, window, "the log", |window, _| shows(window, "run-activity"));
    let before = cx.update_window(window, |_, window, _| bounds(window)[1].size.height).expect("window");

    cx.update(|cx| apply_zoom(Zoom::new(150), cx));
    // The rows grow with the zoom, one under the other, inside the box.
    wait_for_window(cx, window, "the rows laid out at 150%", |window, _| {
        let rows = bounds(window);
        let log = window.find("run-activity").bounds();
        rows[1].size.height >= before * 1.4
            && rows.windows(2).all(|pair| pair[0].bottom() <= pair[1].top() + px(0.5))
            && rows[2].bottom() <= log.bottom()
    });
}
