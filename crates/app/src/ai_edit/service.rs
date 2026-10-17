//! [`PipelineService`]: starts and steers AI edit runs and keeps their state.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant, SystemTime};

use chrono::{DateTime, Utc};
use futures::StreamExt as _;
use futures::channel::mpsc::{UnboundedSender, unbounded};
use gpui_kit::{App, AppContext as _, Context, Entity, Global, SharedString, Subscription, Task};
use rushes_core::input::expand_user_path;
use rushes_core::media::{self, MediaKind};
use rushes_core::settings::{EditHarness, PipelineSettings, data_dir, read_elevenlabs_key};
use rushes_core::{PipelineRun, RunEvent, RunId, RunStatus, StoreError, VideoId, VideoStatus};
use rushes_pipeline::{
    AgentConfig, AgentEvent, AgentProcess, Doctor, Harness, Phase, PhaseOutcome, PipelineError, RunContext, ToolServer,
    default_skill_dir, doctor, doctor_rushes, keep_final_cut, run_phase, set_aside_final_cut, stop_leftover_agent,
};
use serde::{Deserialize, Serialize};

use super::brief::BriefDraft;
use super::footage::FootageImports;
use super::timeline::{LogEvent, MarkerKind, PhaseKind, PhaseStarted, RunMarker, TimelineEntry, agent_time};
use crate::state::{AppEvent, AppState};

/// Error of runs a previous session left running.
pub const INTERRUPTED: &str = "Interrupted when Rushes closed";

/// Starts the error of a render phase that wrote its cut and then reported an error:
/// the cut is kept, but the agent may not have checked it.
pub const RENDERED_WITH_ERROR: &str = "Rendered, but the agent stopped with an error.";

/// How long startup waits for an agent a crashed session left running to stop after
/// `SIGTERM`, before it is killed.
const LEFTOVER_GRACE: Duration = Duration::from_secs(2);

/// Separates the agent's error message from its stderr tail in [`PipelineRun::error`].
const STDERR_SEPARATOR: &str = "\n\nAgent output:\n";

/// Settings key of the brief forms kept across restarts.
const DRAFTS_KEY: &str = "ai_edit.brief_drafts";

/// How long quitting waits for agents to stop, beyond their kill grace period.
const QUIT_MARGIN: Duration = Duration::from_secs(1);

/// Why a run could not be started or continued.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunError {
    VideoNotFound,
    RunNotFound,
    /// Another run of the same video is still working.
    AlreadyRunning,
    /// The media folder could not be created.
    NoMediaFolder,
    /// The footage folder has no video or audio files.
    NoFootage(PathBuf),
    /// No video-use skill folder is configured and there is no home directory.
    NoSkillFolder,
    /// The run is not in a state that allows the request.
    NotAvailable(RunStatus),
    /// Feedback without a message.
    EmptyFeedback,
    /// A cut left at `edit/final.mp4` could not be moved out of the render's way.
    CutNotKept,
    Store(String),
    Spawn(String),
}

impl fmt::Display for RunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RunError::VideoNotFound => write!(f, "This video no longer exists."),
            RunError::RunNotFound => write!(f, "This edit no longer exists."),
            RunError::AlreadyRunning => {
                write!(f, "An AI edit is already running for this video. Wait for it to finish or cancel it.")
            }
            RunError::NoMediaFolder => {
                write!(f, "Couldn’t create the video’s media folder. Check the media folder in Settings.")
            }
            RunError::NoFootage(dir) => {
                write!(f, "Add footage first: video-use edits the raw takes in {}.", dir.display())
            }
            RunError::NoSkillFolder => write!(f, "Choose the video-use skill folder in Settings."),
            RunError::NotAvailable(status) => {
                write!(f, "This isn’t possible while the edit is {}.", status.label().to_lowercase())
            }
            RunError::EmptyFeedback => write!(f, "Write what should change first."),
            RunError::CutNotKept => write!(
                f,
                "Couldn’t move the earlier cut in edit/final.mp4 out of the way, so a render would overwrite it. \
                 Check that the footage folder can be written to, then try again."
            ),
            // The library errors themselves are logged where they occur.
            RunError::Store(_) => {
                write!(f, "Couldn’t save the edit. Try again; if it keeps failing, restart Rushes.")
            }
            RunError::Spawn(_) => write!(f, "Couldn’t start the agent. Try again."),
        }
    }
}

impl std::error::Error for RunError {}

impl From<StoreError> for RunError {
    fn from(error: StoreError) -> Self {
        log_line!("rushes: saving an AI edit failed: {error}");
        RunError::Store(error.to_string())
    }
}

/// The phase a running agent works on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LivePhase {
    pub kind: PhaseKind,
    pub started_at: DateTime<Utc>,
    /// Cancel was requested; the agent is being stopped.
    pub cancelling: bool,
}

/// Why a run's last phase did not finish while the run still offers what an earlier
/// phase made; see [`PipelineService::setback`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Setback<'a> {
    /// The creator cancelled it.
    Stopped,
    /// It failed, or Rushes closed while it ran ([`INTERRUPTED`]): the run's error.
    Failed(&'a str),
}

struct LiveRun {
    video_id: VideoId,
    phase: LivePhase,
    cancel: Arc<AtomicBool>,
    /// Signalled by the agent thread once the agent and its children are gone.
    stopped: mpsc::Receiver<()>,
}

enum AgentMessage {
    Event(AgentEvent),
    /// The phase ended; `kept` is the cut a render phase wrote, kept in `edit/cuts/`
    /// (or left at `edit/final.mp4` when it could not be moved), whatever the result.
    Finished {
        result: Result<PhaseOutcome, PipelineError>,
        kept: Option<PathBuf>,
    },
}

/// App-wide pipeline runner: starts phases on background threads, persists runs
/// and events, survives navigation, stops agents on quit.
///
/// Agent phases run for minutes, so each gets its own thread rather than a slot of
/// the background executor; its events reach the foreground through a channel,
/// where they are appended to the run's timeline. The service keeps the runs and
/// the loaded logs; [`AppState`] saves them through its write queue, so another
/// program's lock on the database never holds up the UI. Observe the service to
/// follow runs, the timeline and the requirement check.
pub struct PipelineService {
    state: Entity<AppState>,
    /// Every run, newest first.
    runs: Vec<PipelineRun>,
    live: HashMap<RunId, LiveRun>,
    /// Event logs loaded so far, by run.
    timelines: HashMap<RunId, Vec<TimelineEntry>>,
    /// How long the agent worked on runs whose log is not loaded, read once.
    agent_times: RefCell<HashMap<RunId, Duration>>,
    /// The cuts the logs not loaded say were kept, read once.
    logged_cuts: RefCell<HashMap<RunId, Vec<PathBuf>>>,
    doctor: Option<Doctor>,
    doctor_task: Option<Task<()>>,
    /// The settings the last requirement check ran with.
    checked: Option<PipelineSettings>,
    agent_env: Vec<(String, String)>,
    /// A run another view asked to show; taken by the panel of its video.
    revealed: Option<RunId>,
    /// Brief forms by video, as their panels left them; the ones that differ from a
    /// fresh form are also saved in the settings, so they survive a restart.
    drafts: HashMap<VideoId, BriefDraft>,
    /// Copies into footage folders, whichever card started them.
    footage_imports: Entity<FootageImports>,
    _subscriptions: Vec<Subscription>,
}

/// A brief form as the settings keep it.
#[derive(Serialize, Deserialize)]
struct StoredDraft {
    video_id: VideoId,
    #[serde(flatten)]
    draft: BriefDraft,
}

struct GlobalPipelineService(Entity<PipelineService>);

impl Global for GlobalPipelineService {}

impl PipelineService {
    /// Creates the service and registers it globally. Call once at startup, after
    /// `AppState::set_global` and before anything shows run state.
    ///
    /// Runs a previous session left working are marked failed ([`INTERRUPTED`]), and
    /// the video rows (whose latest run they may be) are reloaded; they can be
    /// continued when the agent session is known.
    pub fn init(state: Entity<AppState>, cx: &mut App) {
        let service = cx.new(|cx| {
            let subscriptions = vec![
                cx.subscribe(&state, |this: &mut Self, _, event: &AppEvent, cx| match event {
                    AppEvent::SettingsChanged => this.on_settings_changed(cx),
                    AppEvent::DataChanged => this.forget_deleted_videos(cx),
                    _ => {}
                }),
                cx.on_app_quit(|this: &mut Self, _| {
                    this.stop_all();
                    async {}
                }),
            ];
            let mut service = PipelineService {
                state,
                runs: Vec::new(),
                live: HashMap::new(),
                timelines: HashMap::new(),
                agent_times: RefCell::default(),
                logged_cuts: RefCell::default(),
                doctor: None,
                doctor_task: None,
                checked: None,
                agent_env: Vec::new(),
                revealed: None,
                drafts: HashMap::new(),
                footage_imports: cx.new(|_| FootageImports::new()),
                _subscriptions: subscriptions,
            };
            match service.recover_interrupted(cx) {
                Ok(true) => service.state.update(cx, |state, cx| state.reload(cx)),
                Ok(false) => {}
                Err(error) => service.report(error, cx),
            }
            service.reload_runs(cx);
            service.load_drafts(cx);
            service.refresh_doctor(cx);
            service
        });
        cx.set_global(GlobalPipelineService(service));
    }

    pub fn global(cx: &App) -> Entity<PipelineService> {
        cx.global::<GlobalPipelineService>().0.clone()
    }

    /// The service, once [`Self::init`] ran.
    pub fn try_global(cx: &App) -> Option<Entity<PipelineService>> {
        cx.try_global::<GlobalPipelineService>().map(|global| global.0.clone())
    }

    pub fn state(&self) -> &Entity<AppState> {
        &self.state
    }

    // Runs

    /// Every run, newest first.
    pub fn runs(&self) -> &[PipelineRun] {
        &self.runs
    }

    /// Runs of one video, newest first.
    pub fn video_runs(&self, video_id: VideoId) -> impl Iterator<Item = &PipelineRun> {
        self.runs.iter().filter(move |run| run.video_id == video_id)
    }

    pub fn run(&self, id: RunId) -> Option<&PipelineRun> {
        self.runs.iter().find(|run| run.id == id)
    }

    /// The phase the agent of `id` is working on, while it runs.
    pub fn live_phase(&self, id: RunId) -> Option<LivePhase> {
        self.live.get(&id).map(|live| live.phase)
    }

    /// How many runs have their agent working now.
    pub fn live_run_count(&self) -> usize {
        self.live.len()
    }

    /// The run of `video_id` whose agent is working, if any.
    pub fn live_run_of(&self, video_id: VideoId) -> Option<RunId> {
        self.live.iter().find(|(_, live)| live.video_id == video_id).map(|(id, _)| *id)
    }

    /// The loaded event log of a run; see [`Self::load_timeline`].
    pub fn timeline(&self, id: RunId) -> &[TimelineEntry] {
        self.timelines.get(&id).map_or(&[], Vec::as_slice)
    }

    /// Reads a run's event log from the store unless it is loaded already. Live runs
    /// keep their log up to date as events arrive.
    pub fn load_timeline(&mut self, id: RunId, cx: &mut Context<Self>) {
        if self.timelines.contains_key(&id) {
            return;
        }
        let events = self.state.read(cx).read_run_events(id);
        match events {
            Ok(events) => {
                self.timelines.insert(id, events.into_iter().map(TimelineEntry::from).collect());
                cx.notify();
            }
            Err(error) => self.report(error, cx),
        }
    }

    /// How long the agent worked on the run, without the time it waited for the
    /// creator or Rushes was closed; see [`agent_time`]. A running phase counts up to now.
    pub fn agent_time(&self, id: RunId, cx: &App) -> Duration {
        if let Some(timeline) = self.timelines.get(&id) {
            return agent_time(timeline, self.live.contains_key(&id).then(Utc::now));
        }
        if let Some(time) = self.agent_times.borrow().get(&id) {
            return *time;
        }
        let time = match self.state.read(cx).read_run_events(id) {
            Ok(events) => agent_time(&events.into_iter().map(TimelineEntry::from).collect::<Vec<_>>(), None),
            Err(error) => {
                log_line!("rushes: couldn't read the log of run {id}: {error}");
                Duration::ZERO
            }
        };
        self.agent_times.borrow_mut().insert(id, time);
        time
    }

    /// The cuts the AI edits of `video_id` rendered, earlier ones included: every
    /// run's output and the cuts its log says were kept. Nothing else in
    /// `edit/cuts/` is an AI edit's cut.
    pub fn rendered_cuts(&self, video_id: VideoId, cx: &App) -> Vec<PathBuf> {
        let mut cuts = Vec::new();
        for run in self.video_runs(video_id) {
            cuts.extend(run.output.clone());
            if let Some(timeline) = self.timelines.get(&run.id) {
                cuts.extend(kept_cuts_in(timeline));
                continue;
            }
            let mut logged = self.logged_cuts.borrow_mut();
            let kept = logged.entry(run.id).or_insert_with(|| match self.state.read(cx).read_run_events(run.id) {
                Ok(events) => {
                    let entries: Vec<TimelineEntry> = events.into_iter().map(TimelineEntry::from).collect();
                    kept_cuts_in(&entries).collect()
                }
                Err(error) => {
                    log_line!("rushes: couldn't read the log of run {}: {error}", run.id);
                    Vec::new()
                }
            });
            cuts.extend(kept.iter().cloned());
        }
        cuts.sort();
        cuts.dedup();
        cuts
    }

    /// Asks the AI edit panel of the run's video to select it.
    pub fn reveal_run(&mut self, id: RunId, cx: &mut Context<Self>) {
        self.revealed = Some(id);
        cx.notify();
    }

    /// The run to select in `video_id`'s panel, if one was revealed; clears the request.
    pub fn take_revealed(&mut self, video_id: VideoId) -> Option<RunId> {
        let id = self.revealed.filter(|id| self.run(*id).is_some_and(|run| run.video_id == video_id))?;
        self.revealed = None;
        Some(id)
    }

    /// The footage copies of every video.
    pub(crate) fn footage_imports(&self) -> &Entity<FootageImports> {
        &self.footage_imports
    }

    /// The brief form a panel of `video_id` left behind, in this session or an earlier one.
    pub(crate) fn draft(&self, video_id: VideoId) -> Option<&BriefDraft> {
        self.drafts.get(&video_id)
    }

    /// Keeps the brief form of a video's panel, so neither navigating away nor
    /// quitting loses what the creator wrote.
    pub(crate) fn keep_draft(&mut self, video_id: VideoId, draft: BriefDraft, cx: &mut Context<Self>) {
        if self.drafts.get(&video_id) != Some(&draft) {
            self.drafts.insert(video_id, draft);
            self.save_drafts(cx);
        }
    }

    fn load_drafts(&mut self, cx: &mut Context<Self>) {
        let stored = self.state.read(cx).read_setting::<Vec<StoredDraft>>(DRAFTS_KEY);
        match stored {
            Ok(stored) => {
                let state = self.state.read(cx);
                self.drafts = stored
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|stored| state.video(stored.video_id).is_some())
                    .map(|stored| (stored.video_id, stored.draft))
                    .collect();
            }
            Err(error) => log_line!("rushes: couldn't read the saved edit briefs: {error}"),
        }
    }

    /// Saves the forms a fresh panel would not show the same: an edited brief, or
    /// preferences other than the defaults.
    fn save_drafts(&mut self, cx: &mut Context<Self>) {
        let settings = &self.state.read(cx).settings().pipeline;
        let mut stored: Vec<StoredDraft> = self
            .drafts
            .iter()
            .filter(|(_, draft)| {
                draft.edited
                    || draft.preferences != settings.default_preferences
                    || draft.auto_approve != settings.auto_approve
            })
            .map(|(video_id, draft)| StoredDraft { video_id: *video_id, draft: draft.clone() })
            .collect();
        stored.sort_by_key(|stored| stored.video_id);
        // A refusal is reported by the state; the forms stay until Rushes closes.
        self.state.update(cx, |state, cx| state.save_setting(DRAFTS_KEY, &stored, cx));
    }

    /// Extra environment for the agent and every tool it runs, such as a complete
    /// `PATH` when the app was started from a desktop launcher.
    pub fn set_agent_env(&mut self, env: Vec<(String, String)>, cx: &mut Context<Self>) {
        self.agent_env = env;
        self.refresh_doctor(cx);
    }

    /// Starts an AI edit of `video_id`: plans (or, with `auto_approve`, plans and
    /// renders in one pass) from the footage in the video's media folder.
    pub fn start_run(
        &mut self,
        video_id: VideoId,
        brief: String,
        auto_approve: bool,
        cx: &mut Context<Self>,
    ) -> Result<RunId, RunError> {
        if self.live_run_of(video_id).is_some() {
            return Err(RunError::AlreadyRunning);
        }
        if self.state.read(cx).video(video_id).is_none() {
            return Err(RunError::VideoNotFound);
        }
        let folder = self
            .state
            .update(cx, |state, cx| state.ensure_video_folder(video_id, cx))
            .ok_or(RunError::NoMediaFolder)?;
        let footage_dir = folder.join("footage");
        if !has_footage(&footage_dir) {
            return Err(RunError::NoFootage(footage_dir));
        }

        let now = Utc::now();
        let run = PipelineRun {
            id: RunId::new(),
            video_id,
            status: RunStatus::Queued,
            brief,
            footage_dir,
            session_id: None,
            harness: Some(self.state.read(cx).settings().pipeline.harness),
            summary: None,
            output: None,
            error: None,
            cost_usd: None,
            created_at: now,
            updated_at: now,
        };
        if !self.state.update(cx, |state, cx| state.insert_run(run.clone(), cx)) {
            return Err(RunError::Store("the database refused the run".to_owned()));
        }
        let id = run.id;
        self.runs.insert(0, run);
        self.timelines.insert(id, Vec::new());
        self.mark_editing(video_id, cx);

        let phase = if auto_approve { PhaseKind::Auto } else { PhaseKind::Plan };
        if let Err(error) = self.launch(id, PhaseStarted { phase, text: None, retry: false }, cx) {
            if self.run(id).is_some_and(|run| run.status == RunStatus::Queued) {
                self.settle_unfinished(id, Some(phase), Some(error.to_string()), cx);
            }
            return Err(error);
        }
        Ok(id)
    }

    /// Renders the strategy a run proposed, with the creator's optional notes.
    pub fn approve(&mut self, id: RunId, notes: Option<String>, cx: &mut Context<Self>) -> Result<(), RunError> {
        let run = self.run(id).ok_or(RunError::RunNotFound)?;
        if run.status != RunStatus::AwaitingApproval {
            return Err(RunError::NotAvailable(run.status));
        }
        let notes = notes.map(|notes| notes.trim().to_owned()).filter(|notes| !notes.is_empty());
        self.continue_run(id, PhaseStarted { phase: PhaseKind::Approve, text: notes, retry: false }, cx)
    }

    /// Sends the creator's feedback. While the strategy awaits approval this asks for a
    /// revised strategy; after a render (or a failure) it changes the edit and renders
    /// again.
    pub fn send_feedback(&mut self, id: RunId, message: String, cx: &mut Context<Self>) -> Result<(), RunError> {
        let run = self.run(id).ok_or(RunError::RunNotFound)?;
        if message.trim().is_empty() {
            return Err(RunError::EmptyFeedback);
        }
        let phase = match run.status {
            RunStatus::AwaitingApproval => PhaseKind::Revise,
            RunStatus::Succeeded => PhaseKind::Feedback,
            // Nothing is rendered before the creator approved a strategy, so an edit
            // that stopped while planning goes on planning.
            RunStatus::Failed | RunStatus::Cancelled => match self.last_phase(id, cx) {
                Some(phase) if !phase.renders() => PhaseKind::Revise,
                _ => PhaseKind::Feedback,
            },
            status => return Err(RunError::NotAvailable(status)),
        };
        self.continue_run(id, PhaseStarted { phase, text: Some(message.trim().to_owned()), retry: false }, cx)
    }

    /// Runs the phase that failed or was cancelled again, resuming the agent session
    /// when there is one.
    pub fn retry(&mut self, id: RunId, cx: &mut Context<Self>) -> Result<(), RunError> {
        let status = self.run(id).ok_or(RunError::RunNotFound)?.status;
        self.load_timeline(id, cx);
        if !matches!(status, RunStatus::Failed | RunStatus::Cancelled) && self.setback(id).is_none() {
            return Err(RunError::NotAvailable(status));
        }
        let (phase, text) =
            self.last_started(id, cx).map_or((PhaseKind::Plan, None), |started| (started.phase, started.text));
        self.continue_run(id, PhaseStarted { phase, text, retry: true }, cx)
    }

    /// The marker of the phase the run started last, from its event log.
    fn last_started(&mut self, id: RunId, cx: &mut Context<Self>) -> Option<PhaseStarted> {
        self.load_timeline(id, cx);
        self.timeline(id).iter().rev().find_map(|entry| match &entry.event {
            LogEvent::Phase(started) => Some(started.clone()),
            LogEvent::Agent(_) | LogEvent::Marker(_) => None,
        })
    }

    /// Whether the run's strategy was discarded and can be brought back: nothing
    /// happened to the run since. Reads the loaded event log.
    pub fn can_restore(&self, id: RunId) -> bool {
        let discarded = self.timeline(id).last().is_some_and(|entry| {
            matches!(&entry.event, LogEvent::Marker(RunMarker { kind: MarkerKind::Discarded, .. }))
        });
        discarded && self.run(id).is_some_and(|run| run.status == RunStatus::Cancelled && run.summary.is_some())
    }

    /// What stopped the run's last phase, when the run still offers what an earlier
    /// phase made: a strategy awaiting approval after a revision failed or was
    /// cancelled, a rendered cut after a change to it did not finish. Reads the loaded
    /// event log.
    pub fn setback(&self, id: RunId) -> Option<Setback<'_>> {
        let run = self.run(id)?;
        if !matches!(run.status, RunStatus::AwaitingApproval | RunStatus::Succeeded) {
            return None;
        }
        if let Some(error) = run.error.as_deref() {
            return Some(Setback::Failed(error));
        }
        let stopped = self
            .timeline(id)
            .iter()
            .rev()
            .take_while(|entry| !matches!(entry.event, LogEvent::Phase(_)))
            .any(|entry| matches!(&entry.event, LogEvent::Marker(RunMarker { kind: MarkerKind::Cancelled, .. })));
        stopped.then_some(Setback::Stopped)
    }

    /// Brings a discarded strategy back for approval.
    pub fn restore(&mut self, id: RunId, cx: &mut Context<Self>) -> Result<(), RunError> {
        let status = self.run(id).ok_or(RunError::RunNotFound)?.status;
        self.load_timeline(id, cx);
        if !self.can_restore(id) {
            return Err(RunError::NotAvailable(status));
        }
        self.append_event(id, LogEvent::Marker(RunMarker::new(MarkerKind::Restored)), cx);
        self.update_run(id, cx, |run| run.status = RunStatus::AwaitingApproval);
        self.after_status_change(cx);
        Ok(())
    }

    fn last_phase(&mut self, id: RunId, cx: &mut Context<Self>) -> Option<PhaseKind> {
        self.last_started(id, cx).map(|started| started.phase)
    }

    /// Stops the agent of a working run, or discards a strategy awaiting approval (it
    /// can be restored; see [`Self::restore`]).
    pub fn cancel(&mut self, id: RunId, cx: &mut Context<Self>) {
        if let Some(live) = self.live.get_mut(&id) {
            live.cancel.store(true, Ordering::SeqCst);
            live.phase.cancelling = true;
            cx.notify();
        } else if self.run(id).is_some_and(|run| run.status == RunStatus::AwaitingApproval) {
            self.load_timeline(id, cx);
            self.append_event(id, LogEvent::Marker(RunMarker::new(MarkerKind::Discarded)), cx);
            self.update_run(id, cx, |run| {
                run.status = RunStatus::Cancelled;
                run.error = None;
            });
            self.after_status_change(cx);
        }
    }

    fn continue_run(&mut self, id: RunId, started: PhaseStarted, cx: &mut Context<Self>) -> Result<(), RunError> {
        let video_id = self.run(id).ok_or(RunError::RunNotFound)?.video_id;
        if self.live_run_of(video_id).is_some() {
            return Err(RunError::AlreadyRunning);
        }
        self.launch(id, started, cx)
    }

    /// Records the phase, sets the run working and starts the agent thread.
    fn launch(&mut self, id: RunId, started: PhaseStarted, cx: &mut Context<Self>) -> Result<(), RunError> {
        // A working run keeps its log in memory, where its events are appended.
        self.load_timeline(id, cx);
        let (phase, text) = (started.phase, started.text.clone());
        let run = self.run(id).ok_or(RunError::RunNotFound)?.clone();
        let mut settings = self.state.read(cx).settings().pipeline.clone();
        // A run keeps editing with what it started with: its agent session knows those
        // tools. A run from before that was kept takes the setting now and keeps it.
        match run.harness {
            Some(harness) => settings.harness = harness,
            None => self.update_run(id, cx, |run| run.harness = Some(settings.harness)),
        }
        let harness = match settings.harness {
            EditHarness::Rushes => Harness::Rushes { tools: tool_server() },
            EditHarness::VideoUse => Harness::VideoUse {
                skill_dir: skill_dir(&settings).or_else(default_skill_dir).ok_or(RunError::NoSkillFolder)?,
            },
        };
        let config = self.agent_config(&settings);
        let context = RunContext { footage_dir: run.footage_dir.clone(), harness, session_id: run.session_id.clone() };
        let pipeline_phase = match phase {
            PhaseKind::Plan => Phase::Plan { brief: run.brief.clone() },
            PhaseKind::Auto => Phase::Auto { brief: run.brief.clone() },
            PhaseKind::Revise => Phase::Plan { brief: revision_brief(&run.brief, text.as_deref().unwrap_or_default()) },
            PhaseKind::Approve => Phase::Approve { notes: text.clone() },
            PhaseKind::Feedback => Phase::Feedback { message: text.clone().unwrap_or_default() },
        };

        if phase.renders() {
            self.keep_shared_cut(run.video_id, &run.footage_dir, cx)?;
        }
        self.append_event(id, LogEvent::Phase(started), cx);
        let status = if phase.renders() { RunStatus::Rendering } else { RunStatus::Planning };
        self.update_run(id, cx, |run| {
            run.status = status;
            run.error = None;
        });

        let (events, mut receiver) = unbounded();
        let (stopped_tx, stopped) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let agent_cancel = cancel.clone();
        let renders = phase.renders();
        let spawned = std::thread::Builder::new().name(format!("ai-edit-{}", id.short())).spawn(move || {
            let shared_cut = context.footage_dir.join("edit").join("final.mp4");
            let before = file_stamp(&shared_cut);
            let result = run_phase(
                &config,
                &context,
                pipeline_phase,
                &mut |event| send(&events, AgentMessage::Event(event.clone())),
                &agent_cancel,
            );
            // A render phase can write its cut and still end with an error (a turn
            // limit, an API error during the self-check): that cut is kept too. A
            // cancelled render may have left half a file, which is set aside where no
            // list of cuts offers it.
            let rendered = match &result {
                Ok(outcome) => outcome.rendered,
                Err(PipelineError::AgentFailed { .. }) => {
                    file_stamp(&shared_cut).is_some_and(|after| Some(after) != before)
                }
                Err(_) => false,
            };
            let kept =
                (renders && rendered).then(|| keep_final_cut(&context.footage_dir).unwrap_or(shared_cut.clone()));
            if renders && kept.is_none() && shared_cut.is_file() {
                match set_aside_final_cut(&context.footage_dir) {
                    Ok(path) => log_line!("rushes: an unfinished render was set aside in {}", path.display()),
                    // The next render tries again before it starts.
                    Err(error) => log_line!("rushes: couldn't set aside the unfinished render: {error}"),
                }
            }
            send(&events, AgentMessage::Finished { result, kept });
            // The quit handler may already have stopped waiting.
            _ = stopped_tx.send(());
        });
        if let Err(error) = spawned {
            log_line!("rushes: couldn't start the agent thread: {error}");
            let error = RunError::Spawn(error.to_string());
            self.settle_unfinished(id, Some(phase), Some(error.to_string()), cx);
            return Err(error);
        }

        let phase = LivePhase { kind: phase, started_at: Utc::now(), cancelling: false };
        self.live.insert(id, LiveRun { video_id: run.video_id, phase, cancel, stopped });
        cx.spawn(async move |this, cx| {
            while let Some(message) = receiver.next().await {
                let handled = this.update(cx, |service, cx| match message {
                    AgentMessage::Event(event) => service.on_agent_event(id, event, cx),
                    AgentMessage::Finished { result, kept } => service.on_phase_finished(id, result, kept, cx),
                });
                if handled.is_err() {
                    break;
                }
            }
        })
        .detach();
        self.after_status_change(cx);
        Ok(())
    }

    fn on_agent_event(&mut self, id: RunId, event: AgentEvent, cx: &mut Context<Self>) {
        if self.run(id).is_none() {
            // The video was deleted, and the run with it; its agent is being stopped.
            return;
        }
        match &event {
            AgentEvent::Init { session_id, .. } if !session_id.is_empty() => {
                let session_id = session_id.clone();
                self.update_run(id, cx, |run| run.session_id = Some(session_id));
            }
            AgentEvent::Result { cost_usd: Some(cost), .. } => {
                let cost = *cost;
                self.update_run(id, cx, |run| run.cost_usd = Some(run.cost_usd.unwrap_or(0.0) + cost));
            }
            _ => {}
        }
        self.append_event(id, LogEvent::Agent(event), cx);
        cx.notify();
    }

    fn on_phase_finished(
        &mut self,
        id: RunId,
        result: Result<PhaseOutcome, PipelineError>,
        kept: Option<PathBuf>,
        cx: &mut Context<Self>,
    ) {
        let Some(live) = self.live.remove(&id) else { return };
        let phase = live.phase.kind;
        let outcome = match result {
            Ok(outcome) => outcome,
            Err(PipelineError::Cancelled) => {
                self.append_event(id, LogEvent::Marker(RunMarker::new(MarkerKind::Cancelled)), cx);
                return self.settle_unfinished(id, Some(phase), None, cx);
            }
            Err(PipelineError::AgentFailed { message, stderr_tail, session_id, no_result }) => {
                if let Some(session_id) = session_id {
                    self.update_run(id, cx, |run| run.session_id = Some(session_id));
                }
                let mut message = agent_error(&message);
                if no_result {
                    // Most likely not Claude Code, or one that can't start.
                    message.push_str(" Check the agent command in Settings > AI editing.");
                }
                return match kept {
                    Some(cut) => {
                        let error = failure_text(&format!("{RENDERED_WITH_ERROR} {message}"), &stderr_tail);
                        self.finish_render(id, cut, None, Some(error), cx)
                    }
                    None => self.settle_unfinished(id, Some(phase), Some(failure_text(&message, &stderr_tail)), cx),
                };
            }
            Err(error) => {
                log_line!("rushes: AI edit phase failed: {error}");
                return self.settle_unfinished(id, Some(phase), Some(pipeline_problem(&error)), cx);
            }
        };

        let text = Some(outcome.summary_text.trim().to_owned()).filter(|text| !text.is_empty());
        if outcome.session_id.is_some() {
            self.update_run(id, cx, |run| run.session_id = outcome.session_id.clone());
        }
        // An error message must not replace the strategy or the last render's summary.
        let error = outcome
            .is_error
            .then(|| text.as_deref().map_or_else(|| "The agent reported an error.".to_owned(), agent_error));
        match (phase.renders(), kept) {
            (false, _) => match error {
                Some(error) => self.settle_unfinished(id, Some(phase), Some(error), cx),
                None => {
                    self.update_run(id, cx, |run| {
                        run.status = RunStatus::AwaitingApproval;
                        if text.is_some() {
                            run.summary = text;
                        }
                    });
                    self.after_status_change(cx);
                }
            },
            (true, Some(cut)) => match error {
                Some(error) => self.finish_render(id, cut, None, Some(format!("{RENDERED_WITH_ERROR} {error}")), cx),
                None => {
                    let footage_dir = self.run(id).map(|run| run.footage_dir.clone()).unwrap_or_default();
                    let summary = text.map(|text| point_at_kept_cut(&text, &footage_dir, &cut));
                    self.finish_render(id, cut, summary, None, cx)
                }
            },
            // A final cut left over from an earlier render or edit is not this phase's result.
            (true, None) => {
                let error = error.unwrap_or_else(|| "The agent finished without rendering edit/final.mp4.".to_owned());
                self.settle_unfinished(id, Some(phase), Some(error), cx)
            }
        }
    }

    /// Settles a render phase that produced `cut`: the edit is done, with the agent's
    /// summary, or with `error` when the agent stopped with one after rendering.
    fn finish_render(
        &mut self,
        id: RunId,
        cut: PathBuf,
        summary: Option<String>,
        error: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.live.remove(&id);
        // The log says which file each render is, so an earlier one stays on offer.
        self.append_event(id, LogEvent::Marker(RunMarker::cut_kept(cut.clone())), cx);
        self.update_run(id, cx, |run| {
            run.status = RunStatus::Succeeded;
            run.output = Some(cut.clone());
            // Without a summary of this render, the earlier one (or the strategy) would
            // describe the wrong cut.
            run.summary = summary;
            run.error = error;
        });
        if let Some(video_id) = self.run(id).map(|run| run.video_id) {
            self.state.update(cx, |state, cx| {
                let Some(mut video) = state.video(video_id).cloned() else { return };
                if video.publish.file.is_none() {
                    video.publish.file = Some(cut);
                    state.update_video(video, cx);
                }
            });
        }
        self.after_status_change(cx);
    }

    /// Settles a phase that did not finish: `error` says why, `None` when it was
    /// cancelled. What an earlier phase made stays on offer (see [`unfinished_status`]).
    fn settle_unfinished(
        &mut self,
        id: RunId,
        phase: Option<PhaseKind>,
        error: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.live.remove(&id);
        self.update_run(id, cx, |run| {
            run.status = unfinished_status(phase, run, error.is_some());
            run.error = error;
        });
        self.after_status_change(cx);
    }

    /// A file left at the shared `edit/final.mp4` is moved before a render can
    /// overwrite it. A cut a run or the upload form refers to (an earlier version kept
    /// cuts there, keeping one may have failed) moves to `edit/cuts/`, and they follow
    /// it. Anything else (an unfinished render, a file made outside Rushes) is set
    /// aside in `edit/leftovers/`, where no list of cuts offers it.
    fn keep_shared_cut(
        &mut self,
        video_id: VideoId,
        footage_dir: &Path,
        cx: &mut Context<Self>,
    ) -> Result<(), RunError> {
        let Some(shared) = media::edit_outputs(footage_dir).final_cut else { return Ok(()) };
        let owner = self.video_runs(video_id).find(|run| run.output.as_ref() == Some(&shared)).map(|run| run.id);
        let uploads =
            self.state.read(cx).video(video_id).is_some_and(|video| video.publish.file.as_ref() == Some(&shared));
        if owner.is_none() && !uploads {
            let set_aside = set_aside_final_cut(footage_dir).map_err(|error| {
                log_line!("rushes: couldn't set aside {}: {error}", shared.display());
                RunError::CutNotKept
            })?;
            log_line!("rushes: {} was set aside in {}", shared.display(), set_aside.display());
            return Ok(());
        }
        let kept = keep_final_cut(footage_dir).map_err(|error| {
            log_line!("rushes: couldn't keep the earlier cut {}: {error}", shared.display());
            RunError::CutNotKept
        })?;
        if let Some(owner) = owner {
            self.update_run(owner, cx, |run| run.output = Some(kept.clone()));
        }
        if uploads {
            self.state.update(cx, |state, cx| {
                let Some(mut video) = state.video(video_id).cloned() else { return };
                video.publish.file = Some(kept);
                state.update_video(video, cx);
            });
        }
        Ok(())
    }

    fn after_status_change(&mut self, cx: &mut Context<Self>) {
        self.state.update(cx, |state, cx| state.reload(cx));
        cx.notify();
    }

    /// Moves a video that is not that far yet to Editing.
    fn mark_editing(&mut self, video_id: VideoId, cx: &mut Context<Self>) {
        self.state.update(cx, |state, cx| {
            let Some(mut video) = state.video(video_id).cloned() else { return };
            if video.status < VideoStatus::Editing {
                video.status = VideoStatus::Editing;
                state.update_video(video, cx);
            }
        });
    }

    /// Applies `change` to the cached run and saves it.
    fn update_run(&mut self, id: RunId, cx: &mut Context<Self>, change: impl FnOnce(&mut PipelineRun)) {
        let Some(run) = self.runs.iter_mut().find(|run| run.id == id) else { return };
        change(run);
        run.updated_at = Utc::now();
        let run = run.clone();
        self.state.update(cx, |state, cx| state.save_run(run, cx));
    }

    /// Adds `event` to the run's log, numbered after the last entry.
    fn append_event(&mut self, id: RunId, event: LogEvent, cx: &mut Context<Self>) {
        let Some(video_id) = self.run(id).map(|run| run.video_id) else {
            // Deleted with its video; the store has nothing to append to.
            return;
        };
        self.agent_times.borrow_mut().remove(&id);
        self.logged_cuts.borrow_mut().remove(&id);
        self.load_timeline(id, cx);
        // Without its log (it couldn't be read, which was reported) the entry has no number.
        let Some(timeline) = self.timelines.get_mut(&id) else { return };
        let stored = RunEvent {
            run_id: id,
            seq: timeline.last().map_or(1, |entry| entry.seq + 1),
            at: Utc::now(),
            payload: event.to_payload(),
        };
        timeline.push(TimelineEntry { seq: stored.seq, at: stored.at, event });
        self.state.update(cx, |state, cx| state.save_run_event(video_id, stored, cx));
    }

    fn reload_runs(&mut self, cx: &mut Context<Self>) {
        let runs = self.state.read(cx).read_runs();
        match runs {
            Ok(runs) => self.runs = runs,
            Err(error) => self.report(error, cx),
        }
    }

    /// Settles the runs a previous session left working, after stopping an agent it
    /// left running (it crashed or was killed), and brings runs an earlier version
    /// saved in their current shape. Returns whether any run changed.
    ///
    /// An agent normally dies with the app; one that ignored that is stopped here
    /// before the window opens, so a retry can't race with it. That waits at most
    /// [`LEFTOVER_GRACE`], and only after a crash.
    fn recover_interrupted(&mut self, cx: &mut Context<Self>) -> Result<bool, StoreError> {
        let mut recovered = false;
        for mut run in self.state.read(cx).read_runs()? {
            let unfinished = run.status.is_active();
            // Saved by an earlier version, which failed a finished edit when a change
            // to it failed and dropped a strategy whose revision failed.
            let outdated = matches!(run.status, RunStatus::Failed | RunStatus::Cancelled)
                && (run.output.is_some() || run.summary.is_some());
            if !unfinished && !outdated {
                continue;
            }
            let events: Vec<TimelineEntry> =
                self.state.read(cx).read_run_events(run.id)?.into_iter().map(TimelineEntry::from).collect();
            let last_phase = events.iter().rev().find_map(|entry| match &entry.event {
                LogEvent::Phase(started) => Some(started.phase),
                _ => None,
            });
            if unfinished {
                if let Some(process) = leftover_agent(&events) {
                    stop_leftover_agent(&process, LEFTOVER_GRACE);
                }
                run.status = unfinished_status(last_phase, &run, true);
                run.error = Some(INTERRUPTED.to_owned());
                run.updated_at = Utc::now();
                let marker = LogEvent::Marker(RunMarker::new(MarkerKind::Interrupted));
                let marker = RunEvent {
                    run_id: run.id,
                    seq: events.last().map_or(1, |entry| entry.seq + 1),
                    at: Utc::now(),
                    payload: marker.to_payload(),
                };
                let video_id = run.video_id;
                self.state.update(cx, |state, cx| {
                    state.save_run(run, cx);
                    state.save_run_event(video_id, marker, cx);
                });
                recovered = true;
                continue;
            }
            // A discarded strategy stays discarded.
            let discarded = events.last().is_some_and(|entry| {
                matches!(&entry.event, LogEvent::Marker(RunMarker { kind: MarkerKind::Discarded, .. }))
            });
            let status = unfinished_status(last_phase, &run, run.status == RunStatus::Failed);
            if !discarded && status != run.status {
                run.status = status;
                run.updated_at = Utc::now();
                self.state.update(cx, |state, cx| state.save_run(run, cx));
                recovered = true;
            }
        }
        Ok(recovered)
    }

    /// Drops the runs of deleted videos (the store deletes them with their video) and
    /// stops their agents.
    fn forget_deleted_videos(&mut self, cx: &mut Context<Self>) {
        let state = self.state.read(cx);
        let drafts = self.drafts.len();
        self.drafts.retain(|video_id, _| state.video(*video_id).is_some());
        let deleted: Vec<RunId> =
            self.runs.iter().filter(|run| state.video(run.video_id).is_none()).map(|run| run.id).collect();
        if self.drafts.len() != drafts {
            self.save_drafts(cx);
        }
        if deleted.is_empty() {
            return;
        }
        for id in &deleted {
            if let Some(live) = self.live.get_mut(id) {
                live.cancel.store(true, Ordering::SeqCst);
                live.phase.cancelling = true;
            }
            self.timelines.remove(id);
        }
        self.runs.retain(|run| !deleted.contains(&run.id));
        cx.notify();
    }

    /// Cancels every working agent and waits until their process groups are gone, so
    /// no agent keeps running (and spending) after the app quits.
    fn stop_all(&mut self) {
        for live in self.live.values() {
            live.cancel.store(true, Ordering::SeqCst);
        }
        let deadline = Instant::now() + AgentConfig::default().kill_grace + QUIT_MARGIN;
        for live in self.live.values() {
            _ = live.stopped.recv_timeout(deadline.saturating_duration_since(Instant::now()));
        }
    }

    // Requirements

    /// The last requirement check, once one has finished.
    pub fn doctor(&self) -> Option<&Doctor> {
        self.doctor.as_ref()
    }

    pub fn is_checking(&self) -> bool {
        self.doctor_task.is_some()
    }

    /// Checks the agent, ffmpeg, the ElevenLabs key and, for video-use, the skill and
    /// Python again on a background thread.
    pub fn refresh_doctor(&mut self, cx: &mut Context<Self>) {
        let settings = self.state.read(cx).settings().pipeline.clone();
        let config = self.agent_config(&settings);
        let skill_dir = skill_dir(&settings);
        let harness = settings.harness;
        self.checked = Some(settings);
        self.doctor_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    match harness {
                        EditHarness::Rushes => doctor_rushes(&config),
                        EditHarness::VideoUse => doctor(&config, skill_dir.as_deref()),
                    }
                })
                .await;
            _ = this.update(cx, |this, cx| {
                this.doctor = Some(result);
                this.doctor_task = None;
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn on_settings_changed(&mut self, cx: &mut Context<Self>) {
        let settings = &self.state.read(cx).settings().pipeline;
        let changed = self.checked.as_ref().is_none_or(|checked| {
            checked.agent_command != settings.agent_command
                || checked.skill_dir != settings.skill_dir
                || checked.harness != settings.harness
        });
        if changed {
            self.refresh_doctor(cx);
        }
    }

    fn agent_config(&self, settings: &PipelineSettings) -> AgentConfig {
        agent_config(settings, self.agent_env_with_key(settings))
    }

    /// The agent's extra environment plus the ElevenLabs key the edit transcribes with:
    /// the one saved in Rushes, else (for Rushes' tools) the one a video-use install
    /// keeps in its `.env`. A key the environment already has is left alone.
    fn agent_env_with_key(&self, settings: &PipelineSettings) -> Vec<(String, String)> {
        let mut env = self.agent_env.clone();
        let has = env.iter().any(|(name, value)| name == ELEVENLABS_KEY && !value.trim().is_empty())
            || std::env::var(ELEVENLABS_KEY).is_ok_and(|value| !value.trim().is_empty());
        if has {
            return env;
        }
        let key = read_elevenlabs_key(&data_dir()).or_else(|| match settings.harness {
            EditHarness::Rushes => {
                skill_dir(settings).or_else(default_skill_dir).and_then(|dir| dotenv_key(&dir.join(".env")))
            }
            EditHarness::VideoUse => None,
        });
        if let Some(key) = key {
            env.push((ELEVENLABS_KEY.to_owned(), key));
        }
        env
    }

    fn report(&self, error: impl fmt::Display, cx: &mut Context<Self>) {
        let message = SharedString::from(error.to_string());
        self.state.update(cx, |_, cx| cx.emit(AppEvent::Error(message)));
    }
}

/// How the settings launch the agent, with extra environment `env`.
pub(crate) fn agent_config(settings: &PipelineSettings, env: Vec<(String, String)>) -> AgentConfig {
    AgentConfig {
        command: command_path(&settings.agent_command, &AgentConfig::default().command),
        model: settings.model.as_deref().map(str::trim).filter(|model| !model.is_empty()).map(str::to_owned),
        env,
        ..AgentConfig::default()
    }
}

/// The program a command setting names: `default` when empty, `~` expanded.
pub(crate) fn command_path(command: &str, default: &Path) -> PathBuf {
    match command.trim() {
        "" => default.to_owned(),
        // Saved before the settings expanded `~` themselves.
        path if path.starts_with('~') => expand_user_path(path).unwrap_or_else(|_| PathBuf::from(path)),
        command => PathBuf::from(command),
    }
}

fn send(events: &UnboundedSender<AgentMessage>, message: AgentMessage) {
    // A closed channel means the app is going away; the agent is stopped by the
    // cancel flag, not by its events.
    _ = events.unbounded_send(message);
}

/// Modification time and size of a file, to tell whether a phase rewrote it.
fn file_stamp(path: &Path) -> Option<(SystemTime, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

/// The cuts a run's log says were kept.
fn kept_cuts_in(entries: &[TimelineEntry]) -> impl Iterator<Item = PathBuf> + '_ {
    entries.iter().filter_map(|entry| match &entry.event {
        LogEvent::Marker(RunMarker { kind: MarkerKind::CutKept, cut }) => cut.clone(),
        _ => None,
    })
}

/// The status of a run whose last phase did not finish (`failed`, or was cancelled):
/// an edit that has a cut stays done, a strategy whose revision did not finish awaits
/// approval again; only a run with neither failed or stopped.
fn unfinished_status(phase: Option<PhaseKind>, run: &PipelineRun, failed: bool) -> RunStatus {
    if run.output.is_some() {
        RunStatus::Succeeded
    } else if phase == Some(PhaseKind::Revise) && run.summary.is_some() {
        RunStatus::AwaitingApproval
    } else if failed {
        RunStatus::Failed
    } else {
        RunStatus::Cancelled
    }
}

/// The agent process group started by the run's last phase, from its event log.
fn leftover_agent(events: &[TimelineEntry]) -> Option<AgentProcess> {
    events.iter().rev().take_while(|entry| !matches!(entry.event, LogEvent::Phase(_))).find_map(|entry| {
        match &entry.event {
            LogEvent::Agent(AgentEvent::Spawned { process }) => Some(process.clone()),
            _ => None,
        }
    })
}

/// The agent's summary names the `edit/final.mp4` it rendered to, but the cut is kept
/// at `kept`: the summary names the file that exists.
fn point_at_kept_cut(summary: &str, footage_dir: &Path, kept: &Path) -> String {
    match kept.strip_prefix(footage_dir) {
        Ok(relative) if relative != Path::new("edit/final.mp4") => {
            summary.replace("edit/final.mp4", &relative.to_string_lossy())
        }
        _ => summary.to_owned(),
    }
}

/// The error an agent reported, as a sentence: Claude Code reports some only by the
/// result's subtype.
fn agent_error(text: &str) -> String {
    match text {
        "error_max_turns" => "It reached its turn limit.".to_owned(),
        "error_during_execution" => "It hit an error while it worked.".to_owned(),
        text => sentence(text),
    }
}

/// What the creator reads when a phase could not run; the library error is logged.
fn pipeline_problem(error: &PipelineError) -> String {
    match error {
        PipelineError::AgentNotFound(command) => format!(
            "Couldn’t find the agent command “{}”. Check the agent command in Settings > AI editing.",
            command.display()
        ),
        PipelineError::SkillNotFound(_) => {
            "The video-use skill folder has no SKILL.md. Choose the skill folder in Settings > AI editing.".to_owned()
        }
        PipelineError::FootageNotFound(dir) => {
            format!("The footage folder {} is missing. Add footage, then try again.", dir.display())
        }
        PipelineError::Spawn { .. } => {
            "Couldn’t start the agent. Check the agent command in Settings > AI editing.".to_owned()
        }
        PipelineError::Cancelled | PipelineError::AgentFailed { .. } | PipelineError::Io(_) => {
            "Something went wrong while the agent worked. Try again.".to_owned()
        }
    }
}

const ELEVENLABS_KEY: &str = "ELEVENLABS_API_KEY";

/// The program that serves Rushes' editing tools to the agent: this app's own binary,
/// started as `rushes edit-tools --footage <dir>`.
fn tool_server() -> ToolServer {
    let program = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("rushes"));
    ToolServer { program, args: vec![super::EDIT_TOOLS_COMMAND.to_owned()] }
}

/// The value of `ELEVENLABS_API_KEY=` in a `.env` file (quotes stripped), if set.
fn dotenv_key(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let value = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.starts_with('#'))
        .filter_map(|line| line.split_once('='))
        .find(|(name, _)| name.trim() == ELEVENLABS_KEY)?
        .1
        .trim()
        .trim_matches('"')
        .trim_matches('\'')
        .to_owned();
    (!value.is_empty()).then_some(value)
}

/// The configured skill folder, `~` expanded (settings saved before the field expanded
/// it may hold one).
fn skill_dir(settings: &PipelineSettings) -> Option<PathBuf> {
    let dir = settings.skill_dir.clone()?;
    match dir.to_str() {
        Some(text) if text.starts_with('~') => Some(expand_user_path(text).unwrap_or(dir)),
        _ => Some(dir),
    }
}

/// Whether `dir` holds at least one video or audio take.
fn has_footage(dir: &Path) -> bool {
    media::list_footage(dir)
        .is_ok_and(|files| files.iter().any(|file| matches!(file.kind, MediaKind::Video | MediaKind::Audio)))
}

/// Plan prompt for reworking a proposed strategy, or for planning on after an
/// interruption: the creator's feedback on top of the original brief.
fn revision_brief(brief: &str, feedback: &str) -> String {
    format!(
        "You were planning this edit in this session; any strategy you proposed so far is also in \
         ./edit/project.md. The creator asks for these changes:\n\n{}\n\nRevise the strategy \
         accordingly. The original brief was:\n\n{}",
        feedback.trim(),
        brief.trim()
    )
}

/// A pipeline message as a sentence: capitalised, ending in a full stop.
pub(crate) fn sentence(message: &str) -> String {
    let message = message.trim();
    let mut chars = message.chars();
    let mut text: String = chars.next().map(|first| first.to_uppercase().chain(chars).collect()).unwrap_or_default();
    if !text.is_empty() && !text.ends_with(['.', '!', '?']) {
        text.push('.');
    }
    text
}

/// The text stored in [`PipelineRun::error`] for a failed agent.
fn failure_text(message: &str, stderr_tail: &[String]) -> String {
    if stderr_tail.is_empty() {
        message.to_owned()
    } else {
        format!("{message}{STDERR_SEPARATOR}{}", stderr_tail.join("\n"))
    }
}

/// Splits a run error into the message and the agent's stderr tail, if any.
pub fn split_failure(error: &str) -> (&str, Option<&str>) {
    match error.split_once(STDERR_SEPARATOR) {
        Some((message, tail)) => (message, Some(tail)),
        None => (error, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failures_keep_the_stderr_tail_apart() {
        let text = failure_text("agent failed: API Error", &["line 1".into(), "line 2".into()]);
        assert_eq!(split_failure(&text), ("agent failed: API Error", Some("line 1\nline 2")));
        assert_eq!(split_failure(&failure_text("cancelled", &[])), ("cancelled", None));
    }

    #[test]
    fn pipeline_messages_read_as_sentences() {
        assert_eq!(
            sentence("the agent stopped without reporting a result (exit status: 0)"),
            "The agent stopped without reporting a result (exit status: 0)."
        );
        assert_eq!(sentence("API Error: overloaded."), "API Error: overloaded.");
        assert_eq!(sentence(""), "");
    }

    #[test]
    fn summaries_name_the_file_the_cut_is_kept_in() {
        let footage = Path::new("/videos/desk/footage");
        let kept = footage.join("edit/cuts/final-2.mp4");
        assert_eq!(
            point_at_kept_cut("The final cut is ./edit/final.mp4 (14.6s).", footage, &kept),
            "The final cut is ./edit/cuts/final-2.mp4 (14.6s)."
        );
        assert_eq!(
            point_at_kept_cut("Open /videos/desk/footage/edit/final.mp4", footage, &kept),
            "Open /videos/desk/footage/edit/cuts/final-2.mp4"
        );
        // A cut that could not be moved is where the agent says.
        let shared = footage.join("edit/final.mp4");
        assert_eq!(point_at_kept_cut("At edit/final.mp4.", footage, &shared), "At edit/final.mp4.");
    }

    #[test]
    fn a_phase_that_did_not_finish_keeps_what_earlier_ones_made() {
        let now = Utc::now();
        let run = PipelineRun {
            id: RunId::new(),
            video_id: VideoId::new(),
            status: RunStatus::Rendering,
            brief: String::new(),
            footage_dir: PathBuf::new(),
            session_id: None,
            harness: None,
            summary: None,
            output: None,
            error: None,
            cost_usd: None,
            created_at: now,
            updated_at: now,
        };
        let strategy = PipelineRun { summary: Some("Strategy".into()), ..run.clone() };
        let rendered = PipelineRun { output: Some("cuts/final-1.mp4".into()), ..strategy.clone() };
        let revise = Some(PhaseKind::Revise);
        assert_eq!(unfinished_status(revise, &strategy, true), RunStatus::AwaitingApproval);
        assert_eq!(unfinished_status(revise, &strategy, false), RunStatus::AwaitingApproval);
        assert_eq!(unfinished_status(Some(PhaseKind::Feedback), &rendered, true), RunStatus::Succeeded);
        assert_eq!(unfinished_status(Some(PhaseKind::Feedback), &rendered, false), RunStatus::Succeeded);
        // A plan from scratch (planning again after a discard) does not bring back the old strategy.
        assert_eq!(unfinished_status(Some(PhaseKind::Plan), &strategy, true), RunStatus::Failed);
        assert_eq!(unfinished_status(Some(PhaseKind::Approve), &strategy, false), RunStatus::Cancelled);
        assert_eq!(unfinished_status(None, &run, true), RunStatus::Failed);
    }

    #[test]
    fn agent_errors_read_as_sentences() {
        assert_eq!(agent_error("error_max_turns"), "It reached its turn limit.");
        assert_eq!(agent_error("API Error: overloaded"), "API Error: overloaded.");
    }

    #[test]
    fn revision_brief_leads_with_the_feedback() {
        let brief = revision_brief("Cut it tight.", "  Keep the intro joke. ");
        assert!(brief.contains("asks for these changes:\n\nKeep the intro joke.\n\nRevise"), "{brief}");
        assert!(brief.ends_with("The original brief was:\n\nCut it tight."), "{brief}");
    }
}
