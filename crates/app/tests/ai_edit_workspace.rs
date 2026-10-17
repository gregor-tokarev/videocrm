//! AI edit views inside the real `Workspace`: the AI editing settings at the window
//! sizes the app supports, and runs opened from the Pipelines page.

use std::fs;
use std::path::Path;

use chrono::Utc;
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{
    AnyWindowHandle, AppContext as _, Bounds, ElementId, Entity, Focusable as _, Pixels, ScrollDelta, Size,
    TestAppContext, WindowBounds, WindowOptions, point, px, size,
};
use rushes::ai_edit::PipelineService;
use rushes::publish::YoutubeService;
use rushes::route::{Route, SettingsSection, VideoTab};
use rushes::shell::{self, Workspace};
use rushes::state::AppState;
use rushes::ui::CollectionTable;
use rushes_core::settings::{AppSettings, Zoom};
use rushes_core::templates::TemplateKind;
use rushes_core::{NewVideo, PipelineRun, RunId, RunStatus, Store, VideoId, media};

struct Fixture {
    window: AnyWindowHandle,
    state: Entity<AppState>,
    _media: tempfile::TempDir,
}

impl Fixture {
    /// A workspace in a window of `window_size` over a store `prepare` fills, given
    /// the media folder.
    fn open(cx: &mut TestAppContext, window_size: Size<Pixels>, prepare: impl FnOnce(&mut Store, &Path)) -> Self {
        cx.update(|cx| {
            gpui_kit::init(cx);
            shell::init(cx);
            cx.set_reduce_motion(true);
        });
        let media = tempfile::tempdir().expect("temp media folder");
        let mut store = Store::open_in_memory().expect("in-memory store");
        let settings = AppSettings {
            media_root: Some(media.path().to_path_buf()),
            onboarding_completed: true,
            ..AppSettings::default()
        };
        store.save_settings(&settings).expect("save settings");
        prepare(&mut store, media.path());
        let state = AppState::new(store).expect("load state");
        let (window, workspace, state) = cx.update(|cx| {
            let state = cx.new(|_| state);
            AppState::set_global(state.clone(), cx);
            YoutubeService::init(state.clone(), cx);
            PipelineService::init(state.clone(), cx);
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::new(Default::default(), window_size))),
                ..Default::default()
            };
            let handle = state.clone();
            let (window, workspace) =
                gpui_kit::open_window(options, cx, |window, cx| cx.new(|cx| Workspace::new(handle, window, cx)))
                    .expect("open test window");
            (window, workspace, state)
        });
        cx.update_window(window, |_, window, cx| workspace.read(cx).focus_handle(cx).focus(window, cx)).unwrap();
        let fixture = Self { window, state, _media: media };
        fixture.settle(cx);
        fixture
    }

    fn settle(&self, cx: &mut TestAppContext) {
        cx.run_until_parked();
        cx.update_window(self.window, |_, window, cx| window.render_frame(cx)).expect("window is open");
    }

    fn navigate(&self, cx: &mut TestAppContext, route: Route) {
        cx.update(|cx| self.state.update(cx, |state, cx| state.navigate(route, cx)));
        self.settle(cx);
    }

    fn click(&self, cx: &mut TestAppContext, id: impl Into<ElementId>) {
        let id = id.into();
        cx.update_window(self.window, |_, window, cx| window.click(id, cx)).expect("window is open");
        self.settle(cx);
    }

    fn bounds(&self, cx: &mut TestAppContext, id: &'static str) -> Bounds<Pixels> {
        cx.update_window(self.window, |_, window, _| window.find(id).bounds()).expect("window is open")
    }

    fn shows(&self, cx: &mut TestAppContext, id: impl Into<ElementId>) -> bool {
        let id = id.into();
        cx.update_window(self.window, |_, window, _| window.try_find(id).is_some_and(|element| element.visible()))
            .expect("window is open")
    }
}

/// With a custom model chosen, the model name and the Model menu, Target length and
/// the Colour grade menu end on the settings group's trailing edge, inside the window.
fn assert_ai_settings_fit(cx: &mut TestAppContext, window_size: Size<Pixels>) {
    let app = Fixture::open(cx, window_size, |_, _| {});
    cx.update(|cx| {
        app.state.update(cx, |state, cx| {
            state.update_settings(cx, |settings| settings.pipeline.model = Some("claude-sonnet-5-5".into()))
        })
    });
    app.navigate(cx, Route::Settings { section: SettingsSection::AiEditing });

    let [custom, model] = ["ai-settings-custom-model", "ai-settings-model"].map(|id| app.bounds(cx, id));
    // Down to the New edits group, which a small window lays out only once in view.
    for _ in 0..10 {
        if app.shows(cx, "ai-settings-target-length") {
            break;
        }
        cx.update_window(app.window, |_, window, cx| {
            window.scroll("settings-page", ScrollDelta::Lines(point(0., -5.)), cx)
        })
        .unwrap();
        app.settle(cx);
    }
    let [target, grade] = ["ai-settings-target-length", "ai-settings-grade"].map(|id| app.bounds(cx, id));
    let edge = grade.right();
    assert!(edge <= window_size.width - px(8.), "{edge:?} inside a {window_size:?} window");
    for (name, control) in [("model name", custom), ("Model menu", model), ("Target length", target)] {
        assert_eq!(control.right(), edge, "the {name} ends on the group's trailing edge: {control:?}");
        assert!(control.size.width >= px(120.), "the {name} keeps a usable width: {control:?}");
    }
    assert!(model.bottom() <= custom.top(), "the model name sits under the menu");
}

#[gpui_kit::test]
fn ai_settings_fit_the_smallest_window(cx: &mut TestAppContext) {
    assert_ai_settings_fit(cx, size(px(960.), px(600.)));
}

#[gpui_kit::test]
fn ai_settings_end_on_one_edge_in_a_large_window(cx: &mut TestAppContext) {
    assert_ai_settings_fit(cx, size(px(1440.), px(900.)));
}

/// A finished edit of a video whose footage list is long: the run is far below the
/// top of the Footage & AI edit tab.
fn video_with_long_footage(store: &mut Store, media_root: &Path) -> (VideoId, RunId) {
    let new = NewVideo { title: "Fifteen takes".into(), template: TemplateKind::Blank, ..NewVideo::default() };
    let (mut video, _) = store.create_video(new).expect("create video");
    let folder = media::ensure_video_folder(media_root, &mut video).expect("media folder");
    store.update_video(&mut video).expect("save folder");
    let footage = folder.join("footage");
    for take in 1..=15 {
        fs::write(footage.join(format!("take{take:02}.mp4")), b"not really a video").unwrap();
    }
    let now = Utc::now();
    let run = PipelineRun {
        id: RunId::new(),
        video_id: video.id,
        status: RunStatus::Succeeded,
        brief: String::new(),
        footage_dir: footage.clone(),
        session_id: Some("s".into()),
        harness: None,
        summary: Some("Cut the fifteen takes into one.".into()),
        output: Some(footage.join("edit/cuts/final-1.mp4")),
        error: None,
        cost_usd: Some(0.5),
        created_at: now,
        updated_at: now,
    };
    store.insert_run(&run).expect("insert run");
    (video.id, run.id)
}

#[gpui_kit::test]
fn a_pipelines_row_opens_its_video_with_the_run_in_view(cx: &mut TestAppContext) {
    let mut ids = None;
    let app = Fixture::open(cx, size(px(1440.), px(900.)), |store, media_root| {
        ids = Some(video_with_long_footage(store, media_root));
    });
    let (video, run) = ids.unwrap();
    app.navigate(cx, Route::Pipelines);
    app.click(cx, ElementId::Name(format!("pipeline-run-{run}").into()));
    // The footage list is read in the background; the run comes into view after it.
    for _ in 0..3 {
        app.settle(cx);
    }
    assert_eq!(cx.read(|cx| app.state.read(cx).route().clone()), Route::Video { id: video, tab: VideoTab::Footage });
    assert!(app.shows(cx, "ai-edit-history"), "the AI edit section is in view");
    assert!(app.shows(cx, "run-use-for-upload"), "and so is the run's result");
}

#[gpui_kit::test]
fn the_ai_edits_table_scrolls_under_its_header_and_sideways(cx: &mut TestAppContext) {
    let mut runs = Vec::new();
    let window_size = size(px(960.), px(600.));
    let app = Fixture::open(cx, window_size, |store, media_root| {
        for take in 0..30 {
            let new = NewVideo { title: format!("Take {take}"), template: TemplateKind::Blank, ..NewVideo::default() };
            let (mut video, _) = store.create_video(new).expect("create video");
            let folder = media::ensure_video_folder(media_root, &mut video).expect("media folder");
            store.update_video(&mut video).expect("save folder");
            // Newest first: each run a minute after the one before.
            let at = Utc::now() + chrono::Duration::minutes(take);
            let run = PipelineRun {
                id: RunId::new(),
                video_id: video.id,
                status: RunStatus::Succeeded,
                brief: String::new(),
                footage_dir: folder.join("footage"),
                session_id: None,
                harness: None,
                summary: None,
                output: None,
                error: None,
                cost_usd: Some(0.5),
                created_at: at,
                updated_at: at,
            };
            store.insert_run(&run).expect("insert run");
            runs.push(run.id);
        }
    });
    // Zoomed in, the smallest window is too narrow for every column.
    cx.update(|cx| app.state.update(cx, |state, cx| state.update_settings(cx, |s| s.zoom = Zoom::new(150))));
    app.navigate(cx, Route::Pipelines);
    app.settle(cx);
    let newest = ElementId::Name(format!("pipeline-run-{}", runs[29]).into());
    let header = CollectionTable::header_id("pipelines-table");
    let bounds = |cx: &mut TestAppContext, id: &ElementId| {
        let id = id.clone();
        cx.update_window(app.window, |_, window, _| window.find(id).bounds()).expect("window is open")
    };
    let scroll = |cx: &mut TestAppContext, id: &ElementId, delta: ScrollDelta| {
        let id = id.clone();
        cx.update_window(app.window, |_, window, cx| window.scroll(id, delta, cx)).expect("window is open");
        app.settle(cx);
    };
    let (row, head) = (bounds(cx, &newest), bounds(cx, &header));
    assert!(row.right() > window_size.width, "the Cost column is past the window's edge: {row:?}");

    // The wheel scrolls the rows under the header, not sideways.
    scroll(cx, &newest, ScrollDelta::Lines(point(0., -3.)));
    let down = bounds(cx, &newest);
    assert!(down.top() < row.top(), "the rows scrolled: {down:?}");
    assert_eq!(down.left(), row.left(), "and only down");
    assert_eq!(bounds(cx, &header), head, "the header stays put");

    // A sideways swipe brings the last columns in, header and rows together.
    scroll(cx, &header, ScrollDelta::Pixels(point(px(-5000.), px(0.))));
    let across = bounds(cx, &newest);
    assert!(across.right() <= window_size.width, "the Cost column is in view: {across:?}");
    assert_eq!(across.top(), down.top(), "and only sideways");
    assert_eq!(bounds(cx, &header).left(), across.left(), "the header moved with the rows");
}
