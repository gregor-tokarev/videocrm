//! The app state over a database file that another program locks: the real
//! `Workspace` keeps responding, edits wait in the write queue and land once
//! the lock ends, also across a quit.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Utc;
use gpui_kit::component::WindowExt as _;
use gpui_kit::test::TestWindowExt;
use gpui_kit::{
    AnyWindowHandle, App, AppContext as _, Bounds, ElementId, Entity, Focusable as _, TestAppContext, Window,
    WindowBounds, WindowOptions, px, size,
};
use rushes::ai_edit::{LogEvent, PipelineService, RunMarker, TimelineEntry};
use rushes::publish::{Retired, YoutubeEvent, YoutubeService};
use rushes::route::VideoTab;
use rushes::shell::{self, Workspace};
use rushes::state::{AppEvent, AppState, QUIT_SAVE_TIMEOUT, SAVE_RETRY_DELAY, Severity};
use rushes::video_page::SAVE_DELAY;
use rushes_core::settings::{AppSettings, ThemeMode};
use rushes_core::templates::TemplateKind;
use rushes_core::{
    BlockId, DocumentKind, NewVideo, PipelineRun, PropertyDef, PropertyId, PropertyKind, PropertyValue, RunId,
    RunStatus, SelectOption, Store, TagColor, UploadId, UploadJob, UploadStatus, VideoId,
};
use rusqlite::Connection;

/// A bound on any UI-thread call while the database is locked, generous for a
/// debug build on a busy machine, yet below the 5 s the services' connection
/// waits for a lock. Whether the UI waits is what [`AppState::lock_waits`]
/// counts: the UI connection gives up on a lock after 100 ms, and only the
/// first change made while nothing is queued tries it.
const SANITY: Duration = Duration::from_secs(3);

struct Fixture {
    window: AnyWindowHandle,
    workspace: Entity<Workspace>,
    state: Entity<AppState>,
    database: PathBuf,
    _dir: tempfile::TempDir,
}

impl Fixture {
    /// A workspace over a database file holding `titles` as videos.
    fn open(cx: &mut TestAppContext, titles: &[&str]) -> Self {
        cx.update(|cx| {
            gpui_kit::init(cx);
            shell::init(cx);
            cx.set_reduce_motion(true);
        });
        let dir = tempfile::tempdir().expect("temp data folder");
        let database = dir.path().join("rushes.db");
        let mut store = Store::open(&database).expect("database file");
        let settings = AppSettings {
            media_root: Some(dir.path().join("media")),
            onboarding_completed: true,
            ..AppSettings::default()
        };
        store.save_settings(&settings).expect("save settings");
        for title in titles {
            let new = NewVideo { title: title.to_string(), template: TemplateKind::Blank, ..NewVideo::default() };
            store.create_video(new).expect("create video");
        }
        let state = AppState::new(store).expect("load state");
        let (window, workspace, state) = cx.update(|cx| {
            let state = cx.new(|_| state);
            AppState::set_global(state.clone(), cx);
            YoutubeService::init(state.clone(), cx);
            PipelineService::init(state.clone(), cx);
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::new(Default::default(), size(px(1440.), px(900.))))),
                ..Default::default()
            };
            let handle = state.clone();
            let (window, workspace) =
                gpui_kit::open_window(options, cx, |window, cx| cx.new(|cx| Workspace::new(handle, window, cx)))
                    .expect("open test window");
            (window, workspace, state)
        });
        let fixture = Self { window, workspace, state, database, _dir: dir };
        fixture.step(cx, |window, _| window.activate_window());
        let workspace = fixture.workspace.clone();
        fixture.step(cx, move |window, cx| workspace.read(cx).focus_handle(cx).focus(window, cx));
        fixture
    }

    fn step(&self, cx: &mut TestAppContext, interact: impl FnOnce(&mut Window, &mut App)) {
        cx.update_window(self.window, |_, window, cx| interact(window, cx)).expect("window is open");
        cx.run_until_parked();
        cx.update_window(self.window, |_, window, cx| window.render_frame(cx)).expect("window is open");
    }

    fn wait(&self, cx: &mut TestAppContext, duration: Duration) {
        cx.executor().advance_clock(duration);
        self.step(cx, |_, _| {});
    }

    fn click(&self, cx: &mut TestAppContext, id: impl Into<ElementId>) {
        let id = id.into();
        self.step(cx, move |window, cx| window.click(id, cx));
    }

    /// Runs `f` on the UI thread without running the background executor, and
    /// says how long it took.
    fn timed<T>(&self, cx: &mut TestAppContext, f: impl FnOnce(&mut Window, &mut App) -> T) -> (T, Duration) {
        let started = Instant::now();
        let value = cx.update_window(self.window, |_, window, cx| f(window, cx)).expect("window is open");
        (value, started.elapsed())
    }

    fn video_id(&self, cx: &mut TestAppContext, title: &str) -> VideoId {
        cx.read(|cx| {
            self.state.read(cx).rows().iter().find(|row| row.video.title == title).expect("video exists").video.id
        })
    }

    fn has(&self, cx: &mut TestAppContext, id: &'static str) -> bool {
        cx.update_window(self.window, |_, window, _| window.try_find(id).is_some()).expect("window is open")
    }

    /// How often the UI thread has waited for the lock so far.
    fn lock_waits(&self, cx: &mut TestAppContext) -> usize {
        cx.read(|cx| self.state.read(cx).lock_waits())
    }

    fn notifications(&self, cx: &mut TestAppContext) -> usize {
        cx.update_window(self.window, |_, window, cx| window.notifications(cx).len()).expect("window is open")
    }

    /// Collects the errors the app reports from now on.
    fn errors(&self, cx: &mut TestAppContext) -> Rc<RefCell<Vec<String>>> {
        let errors = Rc::new(RefCell::new(Vec::new()));
        let sink = errors.clone();
        cx.update(|cx| {
            cx.subscribe(&self.state, move |_, event: &AppEvent, _| match event {
                AppEvent::Error(message) => sink.borrow_mut().push(message.to_string()),
                AppEvent::Problem(problem) => {
                    sink.borrow_mut().push(format!("{}: {}", problem.title(), problem.message()))
                }
                _ => {}
            })
            .detach()
        });
        errors
    }

    fn page_video(&self, cx: &mut TestAppContext) -> Option<VideoId> {
        cx.read(|cx| self.workspace.read(cx).video_page().map(|page| page.read(cx).video_id()))
    }

    fn script_blocks(&self, cx: &mut TestAppContext) -> Vec<BlockId> {
        cx.read(|cx| {
            let page = self.workspace.read(cx).video_page().expect("video page").read(cx);
            let editor = page.script().expect("script tab").read(cx).editor().cloned().expect("script editor");
            editor.read(cx).document(cx).blocks.iter().map(|block| block.id).collect()
        })
    }
}

/// Another program holding the database's write lock, as a backup or sync
/// tool does: `BEGIN EXCLUSIVE` until released.
struct Locker(Option<Connection>);

impl Locker {
    fn hold(database: &Path) -> Self {
        let connection = Connection::open(database).expect("second connection");
        connection.execute_batch("BEGIN EXCLUSIVE").expect("lock the database");
        Self(Some(connection))
    }

    fn release(&mut self) {
        if let Some(connection) = self.0.take() {
            connection.execute_batch("COMMIT").expect("release the lock");
        }
    }
}

impl Drop for Locker {
    fn drop(&mut self) {
        self.release();
    }
}

/// A fresh connection's view of the database, as another program sees it.
fn stored<T>(database: &Path, read: impl FnOnce(&Store) -> T) -> T {
    read(&Store::open(database).expect("open the database"))
}

fn stored_property(database: &Path, video: VideoId, id: PropertyId) -> Option<PropertyValue> {
    stored(database, |store| store.get_video(video).expect("read video").expect("video").properties.get(&id).cloned())
}

fn stored_script(database: &Path, video: VideoId) -> String {
    stored(database, |store| {
        let documents = store.list_documents(video).expect("list documents");
        let script = documents.into_iter().find(|document| document.kind == DocumentKind::Script).expect("script");
        script.blocks[0].text.clone()
    })
}

fn priority_property() -> PropertyDef {
    let option = |id: &str, name: &str, color| SelectOption { id: id.into(), name: name.into(), color };
    PropertyDef {
        id: PropertyId::new(),
        name: "Priority".into(),
        kind: PropertyKind::Select {
            options: vec![option("high", "High", TagColor::Red), option("low", "Low", TagColor::Gray)],
        },
        position: 0,
    }
}

fn block_part(id: BlockId, name: &'static str) -> ElementId {
    ElementId::NamedChild(Arc::new(ElementId::Uuid(id.0)), name.into())
}

fn set_priority(state: &Entity<AppState>, video: VideoId, priority: PropertyId, option: &str, cx: &mut App) {
    state.update(cx, |state, cx| {
        let mut changed = state.video(video).cloned().expect("video");
        changed.properties.insert(priority, PropertyValue::Select(Some(option.into())));
        state.update_video(changed, cx);
    });
}

#[gpui_kit::test]
fn edits_during_another_programs_lock_wait_for_nothing_and_land_after_it(cx: &mut TestAppContext) {
    let app = Fixture::open(cx, &["Desk setup tour", "Studio tour"]);
    let (desk, studio) = (app.video_id(cx, "Desk setup tour"), app.video_id(cx, "Studio tour"));
    let property = priority_property();
    let priority = property.id;
    let state = app.state.clone();
    app.step(cx, move |_, cx| {
        state.update(cx, |state, cx| state.insert_property(property, cx));
        set_priority(&state, desk, priority, "high", cx);
        state.update(cx, |state, cx| state.open_video(desk, VideoTab::Script, cx));
    });
    let first = app.script_blocks(cx)[0];
    app.click(cx, block_part(first, "text"));
    let errors = app.errors(cx);

    let mut locker = Locker::hold(&app.database);

    // Picking a priority shows at once. It tried the database once, for at
    // most the UI connection's 100 ms; everything after it joins the queue.
    let state = app.state.clone();
    let ((), took) = app.timed(cx, move |_, cx| set_priority(&state, desk, priority, "low", cx));
    assert!(took < SANITY, "a property change took {took:?}");
    assert_eq!(app.lock_waits(cx), 1);
    let shown = cx.read(|cx| app.state.read(cx).video(desk).and_then(|video| video.properties.get(&priority).cloned()));
    assert_eq!(shown, Some(PropertyValue::Select(Some("low".into()))));

    // Typing goes to the queue after the save delay, without waiting either.
    for key in ["Locked", " ", "in"] {
        let ((), took) = app.timed(cx, |window, cx| window.input(key, cx));
        assert!(took < SANITY, "typing {key:?} took {took:?}");
    }
    cx.executor().advance_clock(SAVE_DELAY);
    let page = cx.read(|cx| app.workspace.read(cx).video_page().cloned().expect("video page"));
    let ((), took) = app.timed(cx, move |_, cx| {
        let script = page.read(cx).script().cloned().expect("script");
        script.update(cx, |script, cx| script.flush(cx));
    });
    assert!(took < SANITY, "handing the script to the queue took {took:?}");

    // Another page opens at once: reading doesn't wait for the lock.
    let state = app.state.clone();
    let ((), took) =
        app.timed(cx, move |_, cx| state.update(cx, |state, cx| state.open_video(studio, VideoTab::Notes, cx)));
    assert!(took < SANITY, "opening a page took {took:?}");
    assert_eq!(app.page_video(cx), Some(studio));
    let notes = cx.read(|cx| {
        let page = app.workspace.read(cx).video_page().expect("video page").read(cx);
        page.notes().and_then(|notes| notes.read(cx).editor().cloned()).is_some()
    });
    assert!(notes, "the notes are shown");
    assert_eq!(app.lock_waits(cx), 1, "typing, saving the script and reading waited for nothing");

    // A reload (as a finished pipeline step does) keeps the queued change.
    let state = app.state.clone();
    app.step(cx, move |_, cx| state.update(cx, |state, cx| state.reload(cx)));
    let shown = cx.read(|cx| app.state.read(cx).video(desk).and_then(|video| video.properties.get(&priority).cloned()));
    assert_eq!(shown, Some(PropertyValue::Select(Some("low".into()))), "the queued change still shows");

    // The background writer gave up on this attempt: the page says so, once.
    let state = app.state.clone();
    app.step(cx, move |_, cx| state.update(cx, |state, cx| state.open_video(desk, VideoTab::Script, cx)));
    assert!(cx.read(|cx| app.state.read(cx).is_save_failing(desk)));
    assert!(app.has(cx, "not-saved"), "the page shows its edits aren't saved");
    assert_eq!(app.notifications(cx), 1);
    assert_eq!(
        *errors.borrow(),
        ["Couldn’t save your changes: another program is using the database. Rushes keeps trying and saves them \
          when the database is free."]
    );
    assert_eq!(stored_property(&app.database, desk, priority), Some(PropertyValue::Select(Some("high".into()))));
    app.wait(cx, SAVE_RETRY_DELAY);
    assert_eq!(errors.borrow().len(), 1, "retries stay quiet");

    // The lock ends: the next retry writes both, in the background.
    locker.release();
    app.wait(cx, SAVE_RETRY_DELAY);
    assert_eq!(errors.borrow().len(), 1, "nothing else went wrong");
    assert!(!cx.read(|cx| app.state.read(cx).has_unsaved_changes()));
    assert!(!app.has(cx, "not-saved"));
    assert_eq!(stored_property(&app.database, desk, priority), Some(PropertyValue::Select(Some("low".into()))));
    assert_eq!(stored_script(&app.database, desk), "Locked in");
    assert_eq!(app.lock_waits(cx), 1, "the retries ran in the background");
}

#[gpui_kit::test]
fn ai_edits_and_uploads_change_during_a_lock_without_waiting_and_land_after_it(cx: &mut TestAppContext) {
    let app = Fixture::open(cx, &["Desk setup tour"]);
    let desk = app.video_id(cx, "Desk setup tour");
    let now = Utc::now();
    let run = PipelineRun {
        id: RunId::new(),
        video_id: desk,
        status: RunStatus::AwaitingApproval,
        brief: "Cut it tight.".into(),
        footage_dir: app._dir.path().join("footage"),
        session_id: Some("session".into()),
        harness: None,
        summary: Some("Keep the second take.".into()),
        output: None,
        error: None,
        cost_usd: None,
        created_at: now,
        updated_at: now,
    };
    // Stopped with a session that can take the rest of the video's file.
    let job = UploadJob {
        id: UploadId::new(),
        video_id: desk,
        file: app._dir.path().join("take.mp4"),
        file_stamp: None,
        status: UploadStatus::Failed { error: "Interrupted — you can resume".into() },
        session_url: Some("http://127.0.0.1:9/upload/session".into()),
        channel: None,
        created_at: now,
        updated_at: now,
    };
    let (run_id, job_id) = (run.id, job.id);
    {
        let mut store = Store::open(&app.database).expect("open the database");
        store.insert_run(&run).expect("insert run");
        store.insert_upload(&job).expect("insert upload");
    }
    let (state, file) = (app.state.clone(), job.file.clone());
    app.step(cx, move |_, cx| {
        state.update(cx, |state, cx| {
            let mut video = state.video(desk).cloned().expect("video");
            video.publish.file = Some(file);
            state.update_video(video, cx);
        })
    });
    let (state, dir) = (app.state.clone(), app._dir.path().to_owned());
    let (pipeline, youtube) = cx.update(|cx| {
        PipelineService::init(state.clone(), cx);
        (PipelineService::global(cx), YoutubeService::init_in(state, dir, cx))
    });
    let errors = app.errors(cx);
    let retired = Rc::new(RefCell::new(0));
    let count = retired.clone();
    cx.update(|cx| {
        cx.subscribe(&youtube, move |_, event: &YoutubeEvent, _| {
            if *event == YoutubeEvent::UploadsChanged {
                *count.borrow_mut() += 1;
            }
        })
        .detach()
    });

    let mut locker = Locker::hold(&app.database);

    // Discarding the strategy logs it and changes the run; so does bringing it
    // back. Neither waits for the lock.
    let service = pipeline.clone();
    let ((), took) = app.timed(cx, move |_, cx| service.update(cx, |service, cx| service.cancel(run_id, cx)));
    assert!(took < SANITY, "discarding took {took:?}");
    let service = pipeline.clone();
    let (restored, took) = app.timed(cx, move |_, cx| service.update(cx, |service, cx| service.restore(run_id, cx)));
    assert!(took < SANITY, "restoring took {took:?}");
    restored.expect("restored");
    // Another file is chosen for the video: the stopped upload's status
    // changes, the way its progress does.
    let state = app.state.clone();
    let other = app._dir.path().join("final.mp4");
    let ((), took) = app.timed(cx, move |_, cx| {
        state.update(cx, |state, cx| {
            let mut video = state.video(desk).cloned().expect("video");
            video.publish.file = Some(other);
            state.update_video(video, cx);
        })
    });
    assert!(took < SANITY, "an upload status took {took:?}");
    app.step(cx, |_, _| {});
    // Retired once: the reload it causes doesn't retire it again.
    assert_eq!(*retired.borrow(), 1);
    assert_eq!(app.lock_waits(cx), 1, "only the first change tried the database");

    // Shown at once, stored once the lock ends.
    let shown_run = cx.read(|cx| pipeline.read(cx).run(run_id).cloned().expect("run"));
    assert_eq!(shown_run.status, RunStatus::AwaitingApproval);
    let timeline = cx.read(|cx| pipeline.read(cx).timeline(run_id).to_vec());
    let markers: Vec<RunMarker> = timeline
        .iter()
        .filter_map(|entry| match &entry.event {
            LogEvent::Marker(marker) => Some(marker.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(markers.len(), 2, "discarded, then restored: {timeline:?}");
    let shown_job = cx.read(|cx| youtube.read(cx).job(job_id).cloned().expect("job"));
    assert_eq!(shown_job.session_url, None);
    assert_eq!(cx.read(|cx| youtube.read(cx).retired(job_id)), Some(Retired::OtherFile));
    assert_eq!(stored(&app.database, |store| store.get_run(run_id).expect("read run")), Some(run));
    assert!(stored(&app.database, |store| store.list_run_events(run_id).expect("read log")).is_empty());
    assert_eq!(stored(&app.database, |store| store.get_upload(job_id).expect("read upload")), Some(job));

    locker.release();
    app.wait(cx, SAVE_RETRY_DELAY);
    assert!(!cx.read(|cx| app.state.read(cx).has_unsaved_changes()));
    assert_eq!(stored(&app.database, |store| store.get_run(run_id).expect("read run")), Some(shown_run));
    let log = stored(&app.database, |store| store.list_run_events(run_id).expect("read log"));
    assert_eq!(log.into_iter().map(TimelineEntry::from).collect::<Vec<_>>(), timeline);
    assert_eq!(stored(&app.database, |store| store.get_upload(job_id).expect("read upload")), Some(shown_job));
    assert_eq!(
        *errors.borrow(),
        ["Couldn’t save your changes: another program is using the database. Rushes keeps trying and saves them \
          when the database is free."]
    );
}

#[gpui_kit::test]
fn quitting_waits_for_a_short_lock_and_writes_everything(cx: &mut TestAppContext) {
    let app = Fixture::open(cx, &["Desk setup tour"]);
    let desk = app.video_id(cx, "Desk setup tour");
    let state = app.state.clone();
    app.step(cx, move |_, cx| state.update(cx, |state, cx| state.open_video(desk, VideoTab::Script, cx)));
    let first = app.script_blocks(cx)[0];
    app.click(cx, block_part(first, "text"));

    let locker = Locker::hold(&app.database);
    let state = app.state.clone();
    app.step(cx, move |_, cx| {
        state.update(cx, |state, cx| {
            let mut video = state.video(desk).cloned().expect("video");
            video.hook = "Cheap gear, real sound".into();
            state.update_video(video, cx);
        })
    });
    // Typed right before quitting: still waiting for the save delay.
    app.step(cx, |window, cx| window.input("Last words", cx));

    // The lock ends a second into the quit.
    let release = std::thread::spawn(move || {
        let mut locker = locker;
        std::thread::sleep(Duration::from_secs(1));
        locker.release();
    });
    let started = Instant::now();
    cx.update(|cx| cx.shutdown());
    let took = started.elapsed();
    release.join().expect("released");
    assert!(took < QUIT_SAVE_TIMEOUT, "quitting took {took:?}");

    let hook = stored(&app.database, |store| store.get_video(desk).expect("read video").expect("video").hook);
    assert_eq!(hook, "Cheap gear, real sound");
    assert_eq!(stored_script(&app.database, desk), "Last words");
    assert!(!app.database.with_file_name("unsaved-changes.json").exists());
}

#[gpui_kit::test]
fn changes_that_cant_be_written_before_quitting_are_written_at_the_next_start(cx: &mut TestAppContext) {
    let app = Fixture::open(cx, &["Desk setup tour"]);
    let desk = app.video_id(cx, "Desk setup tour");
    let property = priority_property();
    let priority = property.id;
    let state = app.state.clone();
    app.step(cx, move |_, cx| state.update(cx, |state, cx| state.insert_property(property, cx)));

    let mut locker = Locker::hold(&app.database);
    let state = app.state.clone();
    app.step(cx, move |_, cx| set_priority(&state, desk, priority, "low", cx));

    // The lock outlasts the quit's wait: the change is kept next to the database.
    let started = Instant::now();
    cx.update(|cx| cx.shutdown());
    let took = started.elapsed();
    assert!(took < QUIT_SAVE_TIMEOUT + Duration::from_secs(3), "quitting took {took:?}");
    let unsaved = app.database.with_file_name("unsaved-changes.json");
    assert!(unsaved.exists(), "the change is kept");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&unsaved).expect("unsaved file").permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "private like the database");
    }
    locker.release();
    assert_eq!(stored_property(&app.database, desk, priority), None);

    // The next start writes it and says so.
    let mut next = AppState::new(Store::open(&app.database).expect("open again")).expect("load state");
    let problems = next.take_startup_problems();
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert_eq!(problems[0].title(), "Changes from last time restored");
    assert!(problems[0].message().contains("they’re saved now"), "{problems:?}");
    assert_eq!(problems[0].severity(), Severity::Success, "nothing was lost: no error");
    assert!(!unsaved.exists(), "written changes aren't replayed again");
    assert_eq!(stored_property(&app.database, desk, priority), Some(PropertyValue::Select(Some("low".into()))));
    let shown = next.video(desk).and_then(|video| video.properties.get(&priority).cloned());
    assert_eq!(shown, Some(PropertyValue::Select(Some("low".into()))));
}

#[gpui_kit::test]
fn deleting_the_open_video_during_a_lock_lands_where_the_creator_came_from(cx: &mut TestAppContext) {
    let app = Fixture::open(cx, &["Desk setup tour"]);
    let desk = app.video_id(cx, "Desk setup tour");
    let came_from = cx.read(|cx| app.state.read(cx).route().clone());
    // Opened from All videos, then another tab: both are in the history.
    let state = app.state.clone();
    app.step(cx, move |_, cx| {
        state.update(cx, |state, cx| {
            state.open_video(desk, VideoTab::Script, cx);
            state.open_video(desk, VideoTab::Notes, cx);
        })
    });

    let mut locker = Locker::hold(&app.database);
    let state = app.state.clone();
    app.step(cx, move |_, cx| state.update(cx, |state, cx| state.delete_videos(&[desk], cx)));
    let route = |cx: &mut TestAppContext| cx.read(|cx| app.state.read(cx).route().clone());
    assert_eq!(route(cx), came_from, "the delete waits in the queue, the page doesn't");
    assert!(!app.has(cx, "video-missing"));
    assert!(cx.read(|cx| app.state.read(cx).has_unsaved_changes()));
    assert!(!cx.read(|cx| app.state.read(cx).can_go_forward()));
    assert!(!cx.read(|cx| app.state.read(cx).can_go_back()), "no tab of the deleted video is left behind");

    locker.release();
    app.wait(cx, SAVE_RETRY_DELAY);
    assert!(!cx.read(|cx| app.state.read(cx).has_unsaved_changes()));
    assert_eq!(stored(&app.database, |store| store.get_video(desk).expect("read video")), None);
    assert_eq!(route(cx), came_from);
    assert!(!app.has(cx, "video-missing"));
    assert!(!cx.read(|cx| app.state.read(cx).can_go_forward()));
}

#[gpui_kit::test]
fn deleting_a_video_from_the_table_during_a_lock_takes_it_out_of_forward(cx: &mut TestAppContext) {
    let app = Fixture::open(cx, &["Desk setup tour", "Studio tour"]);
    let desk = app.video_id(cx, "Desk setup tour");
    let table = cx.read(|cx| app.state.read(cx).route().clone());
    // Opened, then back to the table: Forward leads to the video.
    let state = app.state.clone();
    app.step(cx, move |_, cx| {
        state.update(cx, |state, cx| {
            state.open_video(desk, VideoTab::Script, cx);
            state.go_back(cx);
        })
    });
    assert!(cx.read(|cx| app.state.read(cx).can_go_forward()));

    let mut locker = Locker::hold(&app.database);
    let state = app.state.clone();
    app.step(cx, move |_, cx| state.update(cx, |state, cx| state.delete_videos(&[desk], cx)));
    assert!(cx.read(|cx| app.state.read(cx).has_unsaved_changes()), "the delete waits in the queue");
    assert!(!cx.read(|cx| app.state.read(cx).can_go_forward()), "Forward no longer leads to the deleted video");
    app.step(cx, |window, cx| window.press("alt-right", cx));
    let route = |cx: &mut TestAppContext| cx.read(|cx| app.state.read(cx).route().clone());
    assert_eq!(route(cx), table);
    assert!(!app.has(cx, "video-missing"));

    locker.release();
    app.wait(cx, SAVE_RETRY_DELAY);
    assert!(!cx.read(|cx| app.state.read(cx).has_unsaved_changes()));
    assert_eq!(stored(&app.database, |store| store.get_video(desk).expect("read video")), None);
    assert_eq!(route(cx), table);
    assert!(!cx.read(|cx| app.state.read(cx).can_go_forward()));
}

#[gpui_kit::test]
fn a_video_created_while_the_disk_is_full_keeps_its_edits_until_it_lands(cx: &mut TestAppContext) {
    let app = Fixture::open(cx, &[]);
    let errors = app.errors(cx);
    // As a full disk does: a new video fails, and not because of a lock.
    let disk = Connection::open(&app.database).expect("second connection");
    disk.execute_batch(
        "CREATE TRIGGER no_room BEFORE INSERT ON videos BEGIN SELECT abs(-9223372036854775807 - 1); END;",
    )
    .expect("fill the disk");
    let new = NewVideo { title: "Full disk test".into(), template: TemplateKind::Blank, ..NewVideo::default() };
    let video = cx.update(|cx| app.state.update(cx, |state, cx| state.create_video(new, cx))).expect("created");
    let state = app.state.clone();
    app.step(cx, move |_, cx| {
        state.update(cx, |state, cx| {
            let mut changed = state.video(video).cloned().expect("video");
            changed.hook = "Hook typed on a full disk".into();
            state.update_video(changed, cx);
        })
    });
    app.wait(cx, SAVE_RETRY_DELAY);
    app.wait(cx, SAVE_RETRY_DELAY);
    let hook = |cx: &mut TestAppContext| cx.read(|cx| app.state.read(cx).video(video).map(|video| video.hook.clone()));
    assert_eq!(hook(cx).as_deref(), Some("Hook typed on a full disk"), "the edit waits behind the create");
    assert!(cx.read(|cx| app.state.read(cx).is_save_failing(video)));
    assert_eq!(
        *errors.borrow(),
        ["Couldn’t save your changes. Rushes keeps trying; keep the app open until they’re saved."]
    );

    // The disk has room again: the video lands as the creator left it.
    disk.execute_batch("DROP TRIGGER no_room").expect("free the disk");
    app.wait(cx, SAVE_RETRY_DELAY);
    assert!(!cx.read(|cx| app.state.read(cx).has_unsaved_changes()));
    assert_eq!(hook(cx).as_deref(), Some("Hook typed on a full disk"));
    let stored_hook =
        stored(&app.database, |store| store.get_video(video).expect("read video").map(|video| video.hook));
    assert_eq!(stored_hook.as_deref(), Some("Hook typed on a full disk"));
    assert_eq!(errors.borrow().len(), 1, "nothing was taken for deleted");
}

#[gpui_kit::test]
fn settings_kept_at_a_quit_are_the_ones_the_next_start_uses(cx: &mut TestAppContext) {
    let app = Fixture::open(cx, &["Desk setup tour"]);
    let mut locker = Locker::hold(&app.database);
    let state = app.state.clone();
    app.step(cx, move |_, cx| state.update(cx, |state, cx| state.update_settings(cx, |s| s.theme = ThemeMode::Dark)));
    cx.update(|cx| cx.shutdown());
    let unsaved = app.database.with_file_name("unsaved-changes.json");
    assert!(unsaved.exists(), "the settings are kept");
    let stored_theme = || stored(&app.database, |store| store.load_settings().expect("read settings").theme);

    // Still locked at the next start: the kept settings wait in the queue and
    // are the ones in use.
    let next = AppState::new(Store::open(&app.database).expect("open again")).expect("load state");
    assert_eq!(next.settings().theme, ThemeMode::Dark);
    assert!(next.has_unsaved_changes());
    drop(next);
    locker.release();

    // Written at the start once the lock is gone, and used.
    let next = AppState::new(Store::open(&app.database).expect("open again")).expect("load state");
    assert_eq!(next.settings().theme, ThemeMode::Dark);
    assert_eq!(stored_theme(), ThemeMode::Dark);
    assert!(!unsaved.exists());

    // A later change builds on them rather than on what was stored before.
    let next = cx.new(|_| next);
    next.update(cx, |state, cx| state.update_settings(cx, |s| s.pipeline.auto_approve = true));
    let stored_settings = stored(&app.database, |store| store.load_settings().expect("read settings"));
    assert_eq!(stored_settings.theme, ThemeMode::Dark);
    assert!(stored_settings.pipeline.auto_approve);
}

#[gpui_kit::test]
fn changes_restored_while_the_database_is_still_locked_wait_without_an_error(cx: &mut TestAppContext) {
    let app = Fixture::open(cx, &["Desk setup tour"]);
    let desk = app.video_id(cx, "Desk setup tour");
    let mut locker = Locker::hold(&app.database);
    let state = app.state.clone();
    let set_hook = |hook: &'static str| {
        move |state: &mut AppState, cx: &mut gpui_kit::Context<AppState>| {
            let mut video = state.video(desk).cloned().expect("video");
            video.hook = hook.into();
            state.update_video(video, cx);
        }
    };
    app.step(cx, move |_, cx| state.update(cx, set_hook("Kept over a restart")));
    cx.update(|cx| cx.shutdown());
    assert!(app.database.with_file_name("unsaved-changes.json").exists(), "the change is kept");

    // Started again while the lock still holds: the notice says the kept
    // change waits for the database, and the retries add no error to it.
    let mut next = AppState::new(Store::open(&app.database).expect("open again")).expect("load state");
    let problems = next.take_startup_problems();
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert_eq!(problems[0].severity(), Severity::Info);
    let next = cx.new(|_| next);
    let errors = Rc::new(RefCell::new(Vec::new()));
    let sink = errors.clone();
    cx.update(|cx| {
        cx.subscribe(&next, move |_, event: &AppEvent, _| {
            if let AppEvent::Error(message) = event {
                sink.borrow_mut().push(message.to_string());
            }
        })
        .detach();
        AppState::set_global(next.clone(), cx);
    });
    for _ in 0..2 {
        cx.executor().advance_clock(SAVE_RETRY_DELAY);
        cx.run_until_parked();
    }
    assert!(cx.read(|cx| next.read(cx).has_unsaved_changes()), "still locked");
    assert!(errors.borrow().is_empty(), "the notice already told the creator: {:?}", errors.borrow());

    locker.release();
    cx.executor().advance_clock(SAVE_RETRY_DELAY);
    cx.run_until_parked();
    assert!(!cx.read(|cx| next.read(cx).has_unsaved_changes()));
    let hook = stored(&app.database, |store| store.get_video(desk).expect("read video").expect("video").hook);
    assert_eq!(hook, "Kept over a restart");
    assert!(errors.borrow().is_empty());

    // A change that can't be saved later on is reported as usual.
    let _locker = Locker::hold(&app.database);
    next.update(cx, set_hook("Changed during another lock"));
    cx.run_until_parked();
    assert_eq!(errors.borrow().len(), 1, "{:?}", errors.borrow());
}
