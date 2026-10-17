//! End-to-end phases against `scripts/fake-agent` on generated footage.

#![cfg(unix)]

mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::AtomicBool;

use rushes_pipeline::{
    AgentConfig, AgentEvent, Harness, Phase, PhaseOutcome, PipelineError, RunContext, ToolServer, list_footage,
    probe_media, run_phase,
};
use tempfile::TempDir;

/// The fake agent needs python3 and renders with ffmpeg.
fn tools_available() -> bool {
    let ok = ["python3", "ffmpeg", "ffprobe"].iter().all(|tool| Command::new(tool).arg("--version").output().is_ok());
    if !ok {
        eprintln!("skipping: python3, ffmpeg and ffprobe are required");
    }
    ok
}

fn fake_agent(env: &[(&str, &str)]) -> AgentConfig {
    AgentConfig {
        command: PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scripts/fake-agent"),
        model: Some("fake-model".to_owned()),
        env: env.iter().map(|(key, value)| ((*key).to_owned(), (*value).to_owned())).collect(),
        ..AgentConfig::default()
    }
}

struct Workspace {
    _root: TempDir,
    footage: PathBuf,
    /// A symlink to the real skill folder, like `~/.claude/skills/video-use`.
    skill_link: PathBuf,
    skill_real: PathBuf,
    argv_file: PathBuf,
}

impl Workspace {
    fn new(takes: usize) -> Self {
        let root = tempfile::tempdir().expect("tempdir");
        let base = fs::canonicalize(root.path()).expect("canonical tempdir");
        let footage = base.join("footage");
        let skill_real = base.join("video-use-checkout");
        let skill_link = base.join("video-use");
        fs::create_dir_all(&footage).unwrap();
        fs::create_dir_all(skill_real.join("helpers")).unwrap();
        fs::write(skill_real.join("SKILL.md"), "# video-use\n").unwrap();
        std::os::unix::fs::symlink(&skill_real, &skill_link).unwrap();
        for take in 1..=takes {
            make_clip(&footage.join(format!("take{take}.mp4")));
        }
        Self { argv_file: base.join("argv.json"), _root: root, footage, skill_link, skill_real }
    }

    fn context(&self, session_id: Option<String>) -> RunContext {
        RunContext {
            footage_dir: self.footage.clone(),
            harness: Harness::VideoUse { skill_dir: self.skill_link.clone() },
            session_id,
        }
    }

    fn argv(&self) -> Vec<String> {
        serde_json::from_str(&fs::read_to_string(&self.argv_file).expect("argv recorded")).expect("argv json")
    }
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

fn run(config: &AgentConfig, ctx: &RunContext, phase: Phase) -> (Result<PhaseOutcome, PipelineError>, Vec<AgentEvent>) {
    let mut events = Vec::new();
    let result = run_phase(config, ctx, phase, &mut |event| events.push(event.clone()), &AtomicBool::new(false));
    (result, events)
}

fn tool_summaries(events: &[AgentEvent], tool: &str) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::ToolUse { name, summary, .. } if name == tool => Some(summary.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn footage_is_listed_and_probed() {
    if !tools_available() {
        return;
    }
    let ws = Workspace::new(2);
    fs::write(ws.footage.join("notes.txt"), "not media").unwrap();
    fs::write(ws.footage.join(".hidden.mp4"), "").unwrap();
    fs::create_dir(ws.footage.join("edit")).unwrap();
    // A take on an unplugged drive must not break the listing.
    std::os::unix::fs::symlink("/nonexistent/take0.mp4", ws.footage.join("take0.mp4")).unwrap();

    let footage = list_footage(&ws.footage).expect("list");
    let names: Vec<_> = footage.iter().map(|file| file.name.as_str()).collect();
    assert_eq!(names, ["take1.mp4", "take2.mp4"]);
    let media = footage[0].media.as_ref().expect("probed");
    assert!((media.duration_secs - 3.0).abs() < 0.2, "{media:?}");
    assert_eq!((media.width, media.height, media.has_audio), (Some(320), Some(240), true));
    assert_eq!(probe_media(&ws.footage.join("notes.txt")), None);
}

#[test]
fn rushes_tools_replace_the_skill() {
    if !tools_available() {
        return;
    }
    let ws = Workspace::new(1);
    let argv_file = ws.argv_file.to_str().unwrap();
    let config = fake_agent(&[("FAKE_AGENT_ARGV_FILE", argv_file)]);
    let tools = ToolServer { program: PathBuf::from("/opt/rushes/rushes"), args: vec!["edit-tools".to_owned()] };
    let ctx = RunContext { harness: Harness::Rushes { tools }, ..ws.context(None) };
    let (plan, _) = run(&config, &ctx, Phase::Plan { brief: "Budget studio tutorial".to_owned() });
    plan.expect("plan succeeds");

    let argv = ws.argv();
    assert!(argv[1].starts_with("Rushes edit request (planning phase)."));
    assert!(argv[1].contains("rushes tools") && !argv[1].contains("SKILL.md"), "{}", argv[1]);
    let builtin = "Bash,Read,Write,Edit,Glob,Grep,TodoWrite,WebFetch,WebSearch";
    assert!(argv.iter().any(|arg| *arg == format!("--tools={builtin}")), "{argv:?}");
    assert!(argv.iter().any(|arg| *arg == format!("--allowedTools=mcp__rushes,{builtin}")), "{argv:?}");
    assert!(argv.iter().any(|arg| arg == "--strict-mcp-config"), "{argv:?}");
    assert!(!argv.iter().any(|arg| arg.starts_with("--add-dir")), "{argv:?}");
    let config = argv.iter().find_map(|arg| arg.strip_prefix("--mcp-config=")).expect("mcp config");
    let config: serde_json::Value = serde_json::from_str(config).unwrap();
    let server = &config["mcpServers"]["rushes"];
    assert_eq!(server["command"], "/opt/rushes/rushes");
    let footage = std::path::absolute(&ws.footage).unwrap().display().to_string();
    assert_eq!(server["args"], serde_json::json!(["edit-tools", "--footage", footage]));
    let system = argv.iter().find_map(|arg| arg.strip_prefix("--append-system-prompt=")).expect("instructions");
    assert!(system.starts_with("# Editing with the Rushes tools"), "{system}");
}

#[test]
fn both_harnesses_list_the_same_takes() {
    assert_eq!(rushes_pipeline::FOOTAGE_EXTENSIONS, rushes_edit::FOOTAGE_EXTENSIONS);
}

#[test]
fn plan_then_approve_then_feedback() {
    if !tools_available() {
        return;
    }
    let ws = Workspace::new(2);
    let argv_file = ws.argv_file.to_str().unwrap();
    let config = fake_agent(&[("FAKE_AGENT_ARGV_FILE", argv_file)]);

    // Plan: strategy only, nothing rendered.
    let (plan, events) = run(&config, &ws.context(None), Phase::Plan { brief: "Budget studio tutorial".to_owned() });
    let plan = plan.expect("plan succeeds");
    let session_id = plan.session_id.clone().expect("session id");
    assert!(plan.summary_text.contains("**Strategy**"), "{}", plan.summary_text);
    assert_eq!((plan.final_cut.as_ref(), plan.rendered, plan.is_error), (None, false, false));
    assert_eq!(plan.cost_usd, Some(0.0123));
    assert!(ws.footage.join("edit/project.md").is_file());

    assert!(matches!(&events[0], AgentEvent::Spawned { process } if process.pgid > 0), "{:?}", events[0]);
    assert!(matches!(
        &events[1],
        AgentEvent::Init { session_id: id, model, cwd, .. }
            if *id == session_id && model == "fake-model" && *cwd == ws.footage
    ));
    assert!(matches!(events.last(), Some(AgentEvent::Result { is_error: false, .. })));
    assert_eq!(tool_summaries(&events, "Skill"), ["video-use"]);
    let skill_dir = ws.skill_real.display().to_string();
    assert!(
        tool_summaries(&events, "Bash")
            .iter()
            .any(|command| *command == format!("python {skill_dir}/helpers/transcribe_batch.py ."))
    );

    let argv = ws.argv();
    assert_eq!(argv[0], "-p");
    assert!(argv[1].starts_with("Rushes edit request (planning phase)."));
    assert!(argv[1].contains("- take1.mp4 (0:03, 320x240)"));
    assert!(argv[1].contains(&format!("{skill_dir}/SKILL.md")), "symlink resolved");
    for expected in [
        "--output-format",
        "stream-json",
        "--verbose",
        "acceptEdits",
        "--allowedTools=Bash,Read,Write,Edit,Glob,Grep,Skill,Agent,TodoWrite",
        &format!("--add-dir={skill_dir}"),
    ] {
        assert!(argv.iter().any(|arg| arg == expected), "missing {expected} in {argv:?}");
    }
    assert!(!argv.iter().any(|arg| arg.starts_with("--resume")));

    // Approve: resumes the session and renders.
    let (approve, events) = run(
        &config,
        &ws.context(Some(session_id.clone())),
        Phase::Approve { notes: Some("Keep it tight.".to_owned()) },
    );
    let approve = approve.expect("approve succeeds");
    assert_eq!(approve.session_id.as_deref(), Some(session_id.as_str()));
    assert!(matches!(&events[1], AgentEvent::Init { session_id: id, .. } if *id == session_id));
    let argv = ws.argv();
    assert!(argv.contains(&format!("--resume={session_id}")), "{argv:?}");
    assert!(argv[1].contains("Keep it tight."));

    let final_cut = approve.final_cut.clone().expect("final cut");
    assert_eq!(final_cut, ws.footage.join("edit/final.mp4"));
    assert_eq!(approve.preview, Some(ws.footage.join("edit/preview.mp4")));
    assert!(approve.rendered);
    assert!(approve.summary_text.contains("Rendered edit/final.mp4"));
    let media = probe_media(&final_cut).expect("final cut is a valid video");
    assert!((media.duration_secs - 6.0).abs() < 0.3, "two 3 s takes: {media:?}");
    assert_eq!((media.width, media.height, media.has_audio), (Some(640), Some(360), true));
    assert!(probe_media(approve.preview.as_ref().unwrap()).is_some());

    // Feedback: same session, re-rendered.
    let (feedback, _) =
        run(&config, &ws.context(approve.session_id.clone()), Phase::Feedback { message: "Shorter intro".to_owned() });
    let feedback = feedback.expect("feedback succeeds");
    assert_eq!(feedback.session_id.as_deref(), Some(session_id.as_str()));
    assert!(feedback.rendered);
    assert!(feedback.summary_text.contains("feedback applied"));
}

#[test]
fn auto_renders_in_one_pass() {
    if !tools_available() {
        return;
    }
    // No footage: the fake agent falls back to a synthetic clip.
    let ws = Workspace::new(0);
    let config = fake_agent(&[("FAKE_AGENT_ARGV_FILE", ws.argv_file.to_str().unwrap())]);
    // A blank stored session id starts a fresh session.
    let (outcome, events) = run(&config, &ws.context(Some(" ".to_owned())), Phase::Auto { brief: String::new() });
    let outcome = outcome.expect("auto succeeds");
    let argv = ws.argv();
    assert!(argv[1].contains("No media files were found"));
    assert!(!argv.iter().any(|arg| arg.starts_with("--resume")), "{argv:?}");
    assert!(outcome.session_id.is_some_and(|id| !id.trim().is_empty()));
    assert!(outcome.rendered);
    let media = probe_media(outcome.final_cut.as_ref().expect("final cut")).expect("valid video");
    assert!((media.duration_secs - 3.0).abs() < 0.3, "{media:?}");
    assert_eq!(tool_summaries(&events, "Skill"), ["video-use"]);
}

#[test]
fn agent_failure_reports_stderr() {
    if !tools_available() {
        return;
    }
    let ws = Workspace::new(1);
    let config = fake_agent(&[("FAKE_AGENT_MODE", "fail")]);
    let (result, events) = run(&config, &ws.context(None), Phase::Plan { brief: String::new() });
    let Err(PipelineError::AgentFailed { message, stderr_tail, session_id, no_result }) = result else {
        panic!("expected AgentFailed, got {result:?}");
    };
    assert_eq!(message, "API Error: simulated failure");
    assert!(!no_result, "the agent reported its error");
    assert_eq!(stderr_tail, ["fake-agent: simulated failure", "Error: ELEVENLABS_API_KEY rejected (401)"]);
    assert!(session_id.is_some(), "the session can still be resumed");
    assert!(events.contains(&AgentEvent::Stderr { line: "fake-agent: simulated failure".to_owned() }));
}

/// A command that is not Claude Code (`/bin/true`) exits without a word.
#[test]
fn an_agent_that_reports_nothing_fails_without_a_result() {
    let ws = Workspace::new(0);
    let silent = ws.skill_real.with_file_name("silent-agent");
    support::write_executable(&silent, "#!/bin/sh\nexit 0\n");
    let config = AgentConfig { command: silent, ..AgentConfig::default() };
    let (result, _) = run(&config, &ws.context(None), Phase::Plan { brief: String::new() });
    let Err(PipelineError::AgentFailed { message, no_result, .. }) = result else {
        panic!("expected AgentFailed, got {result:?}");
    };
    assert!(no_result);
    assert!(message.starts_with("the agent stopped without reporting a result"), "{message}");
}

#[test]
fn missing_prerequisites_are_reported_before_spawning() {
    let ws = Workspace::new(0);
    let config = fake_agent(&[]);
    let phase = || Phase::Plan { brief: String::new() };

    let missing_agent = AgentConfig { command: PathBuf::from("no-such-agent-cli"), ..config.clone() };
    let (result, events) = run(&missing_agent, &ws.context(None), phase());
    assert!(matches!(result, Err(PipelineError::AgentNotFound(command)) if command == Path::new("no-such-agent-cli")));
    assert!(events.is_empty());

    let ctx = RunContext { harness: Harness::VideoUse { skill_dir: ws.footage.clone() }, ..ws.context(None) };
    let (result, _) = run(&config, &ctx, phase());
    assert!(matches!(result, Err(PipelineError::SkillNotFound(path)) if path == ws.footage.join("SKILL.md")));

    let ctx = RunContext { footage_dir: ws.footage.join("nope"), ..ws.context(None) };
    let (result, _) = run(&config, &ctx, phase());
    assert!(matches!(result, Err(PipelineError::FootageNotFound(_))));
}

#[cfg(target_os = "linux")]
mod cancel {
    use std::sync::atomic::Ordering;
    use std::thread;
    use std::time::{Duration, Instant};

    use super::*;

    /// Pids the fake agent recorded: itself, then each `sleep` child.
    fn recorded_pids(path: &Path) -> Vec<i32> {
        fs::read_to_string(path).unwrap_or_default().lines().filter_map(|line| line.trim().parse().ok()).collect()
    }

    /// Gone, or a zombie waiting for its new parent to reap it.
    fn is_dead(pid: i32) -> bool {
        match fs::read_to_string(format!("/proc/{pid}/stat")) {
            Err(_) => true,
            Ok(stat) => stat
                .rsplit_once(')')
                .and_then(|(_, rest)| rest.split_whitespace().next())
                .is_some_and(|state| state == "Z" || state == "X"),
        }
    }

    fn wait_until(timeout: Duration, mut condition: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if condition() {
                return true;
            }
            thread::sleep(Duration::from_millis(50));
        }
        condition()
    }

    /// Starts a slow fake agent, cancels once it is sleeping in a child process and
    /// checks that the agent and its child are gone.
    fn assert_cancel_kills_tree(extra_env: &[(&str, &str)], grace: Duration) {
        let ws = Workspace::new(1);
        let pid_file = ws.footage.parent().unwrap().join("pids");
        let mut env = vec![("FAKE_AGENT_DELAY_MS", "30000"), ("FAKE_AGENT_PID_FILE", pid_file.to_str().unwrap())];
        env.extend_from_slice(extra_env);
        let config = AgentConfig { kill_grace: grace, ..fake_agent(&env) };
        let cancel = AtomicBool::new(false);
        let mut events = Vec::new();

        let (result, cancelled_at, pids) = thread::scope(|scope| {
            let watcher = scope.spawn(|| {
                let started = wait_until(Duration::from_secs(20), || recorded_pids(&pid_file).len() >= 2);
                assert!(started, "fake agent never started its child");
                cancel.store(true, Ordering::Relaxed);
                Instant::now()
            });
            let result = run_phase(
                &config,
                &ws.context(None),
                Phase::Plan { brief: String::new() },
                &mut |event| events.push(event.clone()),
                &cancel,
            );
            let cancelled_at = watcher.join().expect("watcher");
            (result, cancelled_at, recorded_pids(&pid_file))
        });

        assert!(matches!(result, Err(PipelineError::Cancelled)), "{result:?}");
        assert!(
            cancelled_at.elapsed() < grace + Duration::from_secs(3),
            "cancellation took {:?}",
            cancelled_at.elapsed()
        );
        assert!(matches!(events.get(1), Some(AgentEvent::Init { .. })), "{events:?}");
        for pid in pids {
            assert!(wait_until(Duration::from_secs(5), || is_dead(pid)), "pid {pid} survived");
        }
    }

    #[test]
    fn cancel_terminates_the_whole_process_group() {
        if !tools_available() {
            return;
        }
        assert_cancel_kills_tree(&[], Duration::from_secs(5));
    }

    #[test]
    fn cancel_falls_back_to_sigkill() {
        if !tools_available() {
            return;
        }
        assert_cancel_kills_tree(&[("FAKE_AGENT_IGNORE_TERM", "1")], Duration::from_millis(500));
    }

    /// A process the agent leaves behind (like a backgrounded ffmpeg) is stopped when
    /// the agent exits, instead of running on and holding the output pipes open.
    #[test]
    fn leftover_processes_are_terminated_after_the_agent_exits() {
        if !tools_available() {
            return;
        }
        let ws = Workspace::new(1);
        let pid_file = ws.footage.parent().unwrap().join("pids");
        let config =
            fake_agent(&[("FAKE_AGENT_LEAVE_CHILD", "1"), ("FAKE_AGENT_PID_FILE", pid_file.to_str().unwrap())]);
        let started = Instant::now();
        let (result, _) = run(&config, &ws.context(None), Phase::Plan { brief: String::new() });
        result.expect("plan succeeds");
        assert!(started.elapsed() < Duration::from_millis(1500), "waited for the leftover: {:?}", started.elapsed());
        let pids = recorded_pids(&pid_file);
        assert_eq!(pids.len(), 2, "agent and its leftover child");
        for pid in pids {
            assert!(wait_until(Duration::from_secs(5), || is_dead(pid)), "pid {pid} survived");
        }
    }

    /// The agent and what it started die with the app, even when the app is killed
    /// outright. The app here is this test binary, run again as a child process that
    /// starts a phase with an agent waiting in a child of its own, like ffmpeg.
    #[test]
    fn the_agent_dies_with_a_killed_app() {
        const APP_DIR: &str = "RUSHES_TEST_KILLED_APP_DIR";
        if let Some(dir) = std::env::var_os(APP_DIR) {
            let dir = PathBuf::from(dir);
            let ctx = RunContext {
                footage_dir: dir.join("footage"),
                harness: Harness::VideoUse { skill_dir: dir.join("video-use") },
                session_id: None,
            };
            let config = AgentConfig {
                command: dir.join("waiting-agent"),
                env: vec![("PID_FILE".to_owned(), dir.join("pids").display().to_string())],
                ..AgentConfig::default()
            };
            let (result, _) = run(&config, &ctx, Phase::Plan { brief: String::new() });
            panic!("the app should have been killed while the agent ran: {result:?}");
        }

        let ws = Workspace::new(0);
        let dir = ws.footage.parent().unwrap().to_owned();
        let agent = dir.join("waiting-agent");
        support::write_executable(
            &agent,
            "#!/bin/sh\necho $$ >> \"$PID_FILE\"\nsleep 60 &\necho $! >> \"$PID_FILE\"\n\
             echo '{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s\",\"cwd\":\"/\",\"tools\":[],\"model\":\"m\"}'\n\
             wait\n",
        );
        let mut app = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "cancel::the_agent_dies_with_a_killed_app", "--nocapture"])
            .env(APP_DIR, &dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("start the app");
        let pid_file = dir.join("pids");
        let started = wait_until(Duration::from_secs(20), || recorded_pids(&pid_file).len() >= 2);
        assert!(started, "the agent never started its child");

        app.kill().expect("kill -9 the app");
        app.wait().unwrap();
        for pid in recorded_pids(&pid_file) {
            assert!(wait_until(Duration::from_secs(5), || is_dead(pid)), "pid {pid} outlived the app");
        }
    }

    /// A panicking event callback must not leave the agent running unsupervised.
    #[test]
    fn panicking_callback_still_stops_the_agent() {
        if !tools_available() {
            return;
        }
        let ws = Workspace::new(1);
        let pid_file = ws.footage.parent().unwrap().join("pids");
        let config = AgentConfig {
            kill_grace: Duration::from_secs(2),
            ..fake_agent(&[("FAKE_AGENT_DELAY_MS", "30000"), ("FAKE_AGENT_PID_FILE", pid_file.to_str().unwrap())])
        };
        let outcome = std::panic::catch_unwind(|| {
            run_phase(
                &config,
                &ws.context(None),
                Phase::Plan { brief: String::new() },
                // Once the agent speaks: it has recorded its pid by then.
                &mut |event| {
                    if !matches!(event, AgentEvent::Spawned { .. }) {
                        panic!("event sink gone")
                    }
                },
                &AtomicBool::new(false),
            )
        });
        assert!(outcome.is_err(), "the callback panicked");
        let pids = recorded_pids(&pid_file);
        assert!(!pids.is_empty());
        for pid in pids {
            assert!(wait_until(Duration::from_secs(5), || is_dead(pid)), "pid {pid} survived");
        }
    }
}
