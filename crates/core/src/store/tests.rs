use chrono::{Duration as TimeDelta, NaiveDate, TimeZone};
use serde::Deserialize;
use serde_json::json;

use super::*;
use crate::model::{
    BlockKind, FileStamp, Privacy, PublishChannel, PublishSettings, SelectOption, SortDirection, TagColor, YoutubeLink,
    YoutubeState,
};
use crate::settings::{ThemeMode, YoutubeBackend};

fn store() -> Store {
    Store::open_in_memory().unwrap()
}

fn insert(store: &mut Store, title: &str, position: f64) -> Video {
    let video = Video { position, ..Video::new(title) };
    store.insert_video(&video).unwrap();
    video
}

fn run(video_id: VideoId, status: RunStatus, age_minutes: i64) -> PipelineRun {
    let at = Utc::now() - TimeDelta::minutes(age_minutes);
    PipelineRun {
        id: RunId::new(),
        video_id,
        status,
        brief: "Cut the fillers".into(),
        footage_dir: "/media/video/footage".into(),
        session_id: None,
        harness: None,
        summary: None,
        output: None,
        error: None,
        cost_usd: None,
        created_at: at,
        updated_at: at,
    }
}

fn upload(video_id: VideoId, status: UploadStatus, age_minutes: i64) -> UploadJob {
    let at = Utc::now() - TimeDelta::minutes(age_minutes);
    UploadJob {
        id: UploadId::new(),
        video_id,
        file: "/media/video/footage/edit/final.mp4".into(),
        file_stamp: None,
        status,
        session_url: None,
        channel: None,
        created_at: at,
        updated_at: at,
    }
}

fn select(options: &[&str]) -> PropertyKind {
    PropertyKind::Select {
        options: options
            .iter()
            .map(|&id| SelectOption { id: id.into(), name: id.to_uppercase(), color: TagColor::Blue })
            .collect(),
    }
}

fn words(count: usize) -> Vec<Block> {
    vec![Block::new(BlockKind::Heading2, "Not counted"), Block::paragraph(vec!["word"; count].join(" "))]
}

fn script_of(store: &Store, video_id: VideoId) -> Document {
    store.list_documents(video_id).unwrap().into_iter().find(|d| d.kind == DocumentKind::Script).unwrap()
}

#[test]
fn store_is_send() {
    fn assert_send<T: Send>() {}
    assert_send::<Store>();
}

#[test]
fn fresh_database_is_migrated_and_has_default_views() {
    let store = store();
    assert_eq!(store.schema_version().unwrap(), MIGRATIONS.len() as i64);
    let foreign_keys: bool = store.conn.pragma_query_value(None, "foreign_keys", |row| row.get(0)).unwrap();
    assert!(foreign_keys);

    let views = store.list_views().unwrap();
    let names: Vec<_> = views.iter().map(|v| (v.name.as_str(), v.kind)).collect();
    assert_eq!(names, [("All videos", ViewKind::Table), ("Board", ViewKind::Board)]);
    let table = &views[0];
    assert!(table.filter.hide_archived);
    assert_eq!(table.columns[0].column, Column::Builtin(BuiltinField::Title));
    assert_eq!(table.columns.len(), BuiltinField::ALL.len());
    assert!(table.columns.iter().all(|c| c.width > 0.0));
    assert!(table.sorts.is_empty());
}

#[test]
fn file_database_uses_wal_and_persists_without_reseeding() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nested/data/rushes.db");
    let video = {
        let mut store = Store::open(&path).unwrap();
        let mode: String = store.conn.pragma_query_value(None, "journal_mode", |row| row.get(0)).unwrap();
        assert_eq!(mode, "wal");
        let board = store.list_views().unwrap().remove(1);
        store.delete_view(board.id).unwrap();
        insert(&mut store, "Persisted", 0.0)
    };

    let store = Store::open(&path).unwrap();
    assert_eq!(store.get_video(video.id).unwrap(), Some(video));
    assert_eq!(store.list_views().unwrap().len(), 1, "default views are only seeded on creation");
    assert_eq!(store.schema_version().unwrap(), MIGRATIONS.len() as i64);
}

#[test]
fn concurrent_first_opens_migrate_once() {
    for _ in 0..5 {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rushes.db");
        let barrier = std::sync::Barrier::new(4);
        std::thread::scope(|scope| {
            let opens: Vec<_> = (0..4)
                .map(|_| {
                    scope.spawn(|| {
                        barrier.wait();
                        Store::open(&path).map(|store| store.list_views().unwrap().len())
                    })
                })
                .collect();
            for open in opens {
                assert_eq!(open.join().unwrap().unwrap(), 2, "every open succeeds and views are seeded once");
            }
        });
        assert_eq!(Store::open(&path).unwrap().schema_version().unwrap(), MIGRATIONS.len() as i64);
    }
}

#[test]
fn a_second_connection_reads_during_another_programs_lock_and_fails_fast_on_writes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rushes.db");
    let mut store = Store::open(&path).unwrap();
    let mut video = insert(&mut store, "Locked out", 0.0);
    assert_eq!(store.path(), Some(path.clone()));
    let other = store.reopen().unwrap().expect("a file database opens again");
    other.set_busy_timeout(Duration::from_millis(50)).unwrap();

    let locker = Connection::open(&path).unwrap();
    locker.execute_batch("BEGIN EXCLUSIVE").unwrap();
    assert_eq!(other.list_video_rows().unwrap().len(), 1, "WAL readers don't wait for the lock");
    let started = Instant::now();
    let mut other = other;
    let error = other.update_video(&mut video).expect_err("the lock is held");
    assert!(error.is_busy(), "{error}");
    assert!(started.elapsed() < Duration::from_secs(1), "gave up after its busy timeout");
    assert!(!not_found("video", video.id).is_busy());

    locker.execute_batch("COMMIT").unwrap();
    other.update_video(&mut video).unwrap();
}

#[test]
fn in_memory_databases_have_no_second_connection() {
    let store = store();
    assert_eq!(store.path(), None);
    assert!(store.reopen().unwrap().is_none());
}

#[test]
fn videos_built_elsewhere_are_inserted_with_their_documents() {
    let mut store = store();
    let (video, documents) = NewVideo { title: "Built".into(), ..NewVideo::default() }.build(4.0);
    store.insert_video_with_documents(&video, &documents).unwrap();
    assert_eq!(store.get_video(video.id).unwrap(), Some(video.clone()));
    assert_eq!(store.list_documents(video.id).unwrap(), documents);
    assert!(store.insert_video_with_documents(&video, &documents).is_err(), "inserted once");
    assert_eq!(store.list_documents(video.id).unwrap().len(), 2);
}

#[test]
fn cached_values_follow_property_changes_like_stored_ones() {
    let option = |id: &str| SelectOption { id: id.into(), name: id.into(), color: TagColor::Gray };
    let old = PropertyKind::MultiSelect { options: vec![option("yt"), option("tt")] };
    let property = PropertyDef {
        id: PropertyId::new(),
        name: "Platforms".into(),
        kind: PropertyKind::MultiSelect { options: vec![option("yt")] },
        position: 0,
    };
    let mut values = BTreeMap::from([(property.id, PropertyValue::MultiSelect(vec!["yt".into(), "tt".into()]))]);
    reconcile_property_value(&old, &property, &mut values);
    assert_eq!(values.get(&property.id), Some(&PropertyValue::MultiSelect(vec!["yt".into()])));
    let text = PropertyDef { kind: PropertyKind::Text, ..property.clone() };
    reconcile_property_value(&property.kind, &text, &mut values);
    assert!(values.is_empty(), "a new kind drops the value");
}

#[cfg(unix)]
#[test]
fn new_database_files_are_private() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rushes.db");
    let mut store = Store::open(&path).unwrap();
    insert(&mut store, "Secret plans", 0.0);
    for file in ["rushes.db", "rushes.db-wal", "rushes.db-shm"] {
        let mode = std::fs::metadata(dir.path().join(file)).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "{file}");
    }
}

#[test]
fn rejects_databases_from_newer_versions() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rushes.db");
    drop(Store::open(&path).unwrap());
    Connection::open(&path).unwrap().pragma_update(None, "user_version", 99).unwrap();

    match Store::open(&path) {
        Err(StoreError::UnsupportedSchema { found: 99, supported }) if supported == MIGRATIONS.len() as i64 => {}
        other => panic!("expected UnsupportedSchema, got {:?}", other.map(|_| ())),
    }
}

#[test]
fn uploads_stored_before_file_stamps_still_load() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rushes.db");
    let job = upload(VideoId::new(), UploadStatus::Cancelled, 1);
    {
        // A database of the first release, with one upload in it.
        let mut conn = Connection::open(&path).unwrap();
        let tx = conn.transaction().unwrap();
        migration_1(&tx).unwrap();
        tx.pragma_update(None, "user_version", 1).unwrap();
        tx.commit().unwrap();
        let mut store = Store { conn };
        let video = Video { id: job.video_id, ..Video::new("Old upload") };
        store.insert_video(&video).unwrap();
        store
            .conn
            .execute(
                "INSERT INTO uploads (id, video_id, file, status, session_url, created_at, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    job.id,
                    job.video_id,
                    SqlPath(&job.file),
                    Json(&job.status),
                    job.session_url,
                    job.created_at,
                    job.updated_at
                ],
            )
            .unwrap();
    }
    let store = Store::open(&path).unwrap();
    assert_eq!(store.schema_version().unwrap(), MIGRATIONS.len() as i64);
    assert_eq!(store.list_uploads(None).unwrap(), vec![job]);
}

#[test]
fn video_round_trips_every_field() {
    let mut store = store();
    let property = PropertyId::new();
    let video = Video {
        icon: Some("🦀".into()),
        status: VideoStatus::Editing,
        format: VideoFormat::Short,
        publish_date: NaiveDate::from_ymd_opt(2026, 11, 2),
        tags: vec!["rust".into(), "tutorial".into()],
        hook: "Ownership in 60 seconds".into(),
        folder: Some("/media/rust-ownership-0192f3a4".into()),
        publish: PublishSettings {
            title: "Rust Ownership".into(),
            description: "Chapters\n0:00 Intro".into(),
            tags: vec!["rust".into()],
            category_id: "28".into(),
            privacy: Privacy::Private,
            publish_at: Some(Utc.with_ymd_and_hms(2026, 11, 2, 16, 0, 0).unwrap()),
            made_for_kids: false,
            file: Some("/media/final.mp4".into()),
            thumbnail: Some("/media/thumb.png".into()),
            channel: Some(PublishChannel { id: "UC123".into(), name: "Rust in 60".into() }),
        },
        youtube: Some(YoutubeLink {
            video_id: "abc123".into(),
            url: "https://youtu.be/abc123".into(),
            channel_id: "UC123".into(),
            privacy: Privacy::Unlisted,
            uploaded_at: Utc::now(),
            thumbnail: None,
        }),
        properties: BTreeMap::from([
            (property, PropertyValue::Number(4.5)),
            (PropertyId::new(), PropertyValue::MultiSelect(vec!["a".into(), "b".into()])),
            (PropertyId::new(), PropertyValue::Date(NaiveDate::from_ymd_opt(2026, 1, 31))),
        ]),
        position: 3.25,
        ..Video::new("Rust ownership")
    };
    store.insert_video(&video).unwrap();
    assert_eq!(store.get_video(video.id).unwrap(), Some(video.clone()));
    assert!(store.insert_video(&video).is_err(), "duplicate ids are rejected");

    let mut edited = video.clone();
    edited.title = "Rust ownership, explained".into();
    edited.youtube = None;
    edited.properties.remove(&property);
    store.update_video(&mut edited).unwrap();
    assert!(edited.updated_at > video.updated_at);
    assert_eq!(edited.created_at, video.created_at);
    assert_eq!(store.get_video(video.id).unwrap(), Some(edited));
}

#[test]
fn update_and_delete_of_missing_videos() {
    let mut store = store();
    let mut ghost = Video::new("Ghost");
    assert!(matches!(store.update_video(&mut ghost), Err(StoreError::NotFound { entity: "video", .. })));
    store.delete_video(ghost.id).unwrap();
    assert_eq!(store.get_video(ghost.id).unwrap(), None);
}

#[test]
fn videos_are_listed_by_position() {
    let mut store = store();
    assert_eq!(store.next_position().unwrap(), 0.0);
    let c = insert(&mut store, "C", 2.0);
    let a = insert(&mut store, "A", 0.0);
    let b = insert(&mut store, "B", 1.5);
    let ids: Vec<_> = store.list_videos().unwrap().into_iter().map(|v| v.id).collect();
    assert_eq!(ids, [a.id, b.id, c.id]);
    assert_eq!(store.next_position().unwrap(), 3.0);
}

#[test]
fn create_video_uses_template_and_appends() {
    let mut store = store();
    insert(&mut store, "Existing", 4.0);
    let (video, documents) = store
        .create_video(NewVideo {
            title: "Stop using print()".into(),
            format: VideoFormat::Short,
            hook: "Your debugger is one line away.".into(),
            publish_date: NaiveDate::from_ymd_opt(2026, 10, 20),
            template: TemplateKind::Short,
        })
        .unwrap();

    assert_eq!(video.position, 5.0);
    assert_eq!(video.icon.as_deref(), Some("⚡"));
    assert_eq!(video.status, VideoStatus::Idea);
    assert_eq!(store.get_video(video.id).unwrap(), Some(video.clone()));

    let kinds: Vec<_> = documents.iter().map(|d| (d.kind, d.title.as_str(), d.position)).collect();
    assert_eq!(kinds, [(DocumentKind::Script, "Script", 0), (DocumentKind::Notes, "Notes", 1)]);
    assert!(documents[0].blocks.iter().any(|b| b.text == "Your debugger is one line away."));
    assert_eq!(shape_of(&documents[1].blocks), [(BlockKind::Paragraph, String::new())]);
    assert_eq!(store.list_documents(video.id).unwrap(), documents);

    let (blank, _) = store.create_video(NewVideo { title: "Blank".into(), ..NewVideo::default() }).unwrap();
    assert_eq!(blank.icon, None);
    assert_eq!(blank.position, 6.0);
}

fn shape_of(blocks: &[Block]) -> Vec<(BlockKind, String)> {
    blocks.iter().map(|b| (b.kind, b.text.clone())).collect()
}

#[test]
fn video_rows_derive_script_stats_and_latest_statuses() {
    let mut store = store();
    let (busy, _) = store.create_video(NewVideo { title: "Busy".into(), ..NewVideo::default() }).unwrap();
    let idle = insert(&mut store, "Idle", 10.0);

    let mut script = script_of(&store, busy.id);
    script.blocks = words(300);
    store.save_document(&mut script).unwrap();
    // Only the first Script document counts; notes never do.
    let mut notes = store.list_documents(busy.id).unwrap().remove(1);
    notes.blocks = words(50);
    store.save_document(&mut notes).unwrap();
    let mut second_script = store.create_document(busy.id, DocumentKind::Script, "Alt script").unwrap();
    second_script.blocks = words(999);
    store.save_document(&mut second_script).unwrap();

    store.insert_run(&run(busy.id, RunStatus::Failed, 30)).unwrap();
    store.insert_run(&run(busy.id, RunStatus::AwaitingApproval, 5)).unwrap();
    store.insert_upload(&upload(busy.id, UploadStatus::Failed { error: "quota".into() }, 20)).unwrap();
    store.insert_upload(&upload(busy.id, UploadStatus::Uploading { sent: 10, total: 100 }, 1)).unwrap();

    let rows = store.list_video_rows().unwrap();
    assert_eq!(rows.len(), 2);
    let busy_row = &rows[0];
    assert_eq!(busy_row.video, busy);
    assert_eq!(busy_row.script_words, 300);
    assert_eq!(busy_row.script_seconds, 120);
    assert_eq!(busy_row.latest_run, Some(RunStatus::AwaitingApproval));
    assert!(!busy_row.has_finished_edit);
    assert_eq!(busy_row.latest_upload, Some(UploadStatus::Uploading { sent: 10, total: 100 }));

    let idle_row = &rows[1];
    assert_eq!(idle_row.video, idle);
    assert_eq!((idle_row.script_words, idle_row.script_seconds), (0, 0));
    assert_eq!(idle_row.latest_run, None);
    assert!(!idle_row.has_finished_edit);
    assert_eq!(idle_row.latest_upload, None);
    assert_eq!((idle_row.edit_status(), idle_row.youtube_state()), (None, None));
}

#[test]
fn a_video_gone_from_youtube_has_no_youtube_state() {
    let mut store = store();
    let video = insert(&mut store, "Deleted in YouTube Studio", 0.0);
    store.insert_upload(&upload(video.id, UploadStatus::Done { video_id: "yt1".into() }, 2)).unwrap();
    // The app drops the link once YouTube no longer has the video; "Uploaded" would be wrong.
    let row = store.list_video_rows().unwrap().remove(0);
    assert_eq!(row.youtube_state(), None);
    assert!(row.left_youtube(), "the page says it was there");

    let never = insert(&mut store, "Never uploaded", 1.0);
    store.insert_upload(&upload(never.id, UploadStatus::Cancelled, 1)).unwrap();
    let rows = store.list_video_rows().unwrap();
    assert!(!rows.iter().find(|row| row.video.id == never.id).unwrap().left_youtube());
}

#[test]
fn a_failed_retry_does_not_hide_a_finished_edit_or_a_live_video() {
    let mut store = store();
    let mut video = insert(&mut store, "Retried", 0.0);
    store.insert_run(&run(video.id, RunStatus::Succeeded, 30)).unwrap();
    store.insert_run(&run(video.id, RunStatus::Failed, 5)).unwrap();
    store.insert_upload(&upload(video.id, UploadStatus::Done { video_id: "yt1".into() }, 20)).unwrap();
    store.insert_upload(&upload(video.id, UploadStatus::Cancelled, 2)).unwrap();

    // Not linked to YouTube (e.g. the link was removed): the attempt is all there is.
    let row = store.list_video_rows().unwrap().remove(0);
    assert_eq!(row.latest_run, Some(RunStatus::Failed));
    assert!(row.has_finished_edit);
    assert_eq!(row.edit_status(), Some(RunStatus::Succeeded));
    assert_eq!(row.newer_edit_attempt(), Some(RunStatus::Failed));
    assert_eq!(row.youtube_state(), Some(YoutubeState::Upload(&UploadStatus::Cancelled)));
    assert_eq!(row.newer_upload_attempt(), None);

    let link = YoutubeLink {
        video_id: "yt1".into(),
        url: "https://www.youtube.com/watch?v=yt1".into(),
        channel_id: "UC1".into(),
        privacy: Privacy::Public,
        uploaded_at: Utc::now(),
        thumbnail: None,
    };
    video.youtube = Some(link.clone());
    store.update_video(&mut video).unwrap();
    let row = store.list_video_rows().unwrap().remove(0);
    assert_eq!(row.youtube_state(), Some(YoutubeState::Live(&link)));
    assert_eq!(row.newer_upload_attempt(), Some(&UploadStatus::Cancelled));

    // A running edit is the state to show, even after a finished one.
    store.insert_run(&run(video.id, RunStatus::Rendering, 1)).unwrap();
    let row = store.list_video_rows().unwrap().remove(0);
    assert_eq!(row.edit_status(), Some(RunStatus::Rendering));
    assert_eq!(row.newer_edit_attempt(), None);
}

#[test]
fn deleting_a_video_cascades_to_children() {
    let mut store = store();
    let (video, documents) = store.create_video(NewVideo { title: "Doomed".into(), ..NewVideo::default() }).unwrap();
    let keep = insert(&mut store, "Keep", 9.0);
    let doomed_run = run(video.id, RunStatus::Rendering, 1);
    store.insert_run(&doomed_run).unwrap();
    store.append_run_event(doomed_run.id, &json!({"type": "assistant"})).unwrap();
    let doomed_upload = upload(video.id, UploadStatus::Pending, 1);
    store.insert_upload(&doomed_upload).unwrap();
    let kept_run = run(keep.id, RunStatus::Queued, 1);
    store.insert_run(&kept_run).unwrap();

    store.delete_video(video.id).unwrap();
    assert_eq!(store.get_video(video.id).unwrap(), None);
    assert!(store.list_documents(video.id).unwrap().is_empty());
    assert_eq!(store.get_document(documents[0].id).unwrap(), None);
    assert_eq!(store.get_run(doomed_run.id).unwrap(), None);
    assert!(store.list_run_events(doomed_run.id).unwrap().is_empty());
    assert_eq!(store.get_upload(doomed_upload.id).unwrap(), None);
    assert_eq!(store.list_runs(None).unwrap(), vec![kept_run]);
}

#[test]
fn duplicate_copies_content_but_not_publishing_state() {
    let mut store = store();
    let (mut original, documents) = store
        .create_video(NewVideo {
            title: "Mini PC review".into(),
            template: TemplateKind::Review,
            ..NewVideo::default()
        })
        .unwrap();
    original.status = VideoStatus::Published;
    original.tags = vec!["review".into()];
    original.folder = Some("/media/mini-pc".into());
    original.publish.description = "Description".into();
    original.publish.file = Some("/media/mini-pc/footage/edit/final.mp4".into());
    original.publish.publish_at = Some(Utc::now());
    original.youtube = Some(YoutubeLink {
        video_id: "x".into(),
        url: "https://youtu.be/x".into(),
        channel_id: "UC".into(),
        privacy: Privacy::Public,
        uploaded_at: Utc::now(),
        thumbnail: None,
    });
    store.update_video(&mut original).unwrap();
    let after = insert(&mut store, "After", original.position + 1.0);
    store.insert_run(&run(original.id, RunStatus::Succeeded, 1)).unwrap();
    store.insert_upload(&upload(original.id, UploadStatus::Done { video_id: "x".into() }, 1)).unwrap();

    let copy = store.duplicate_video(original.id).unwrap();
    assert_ne!(copy.id, original.id);
    assert_eq!(copy.title, "Mini PC review (copy)");
    assert_eq!(copy.status, VideoStatus::Idea);
    assert_eq!(copy.tags, original.tags);
    assert_eq!(copy.publish.description, "Description");
    assert_eq!((copy.folder.clone(), copy.youtube.clone()), (None, None));
    assert_eq!((copy.publish.file.clone(), copy.publish.publish_at), (None, None));
    assert!(copy.position > original.position && copy.position < after.position);
    assert_eq!(store.get_video(copy.id).unwrap(), Some(copy.clone()));

    let order: Vec<_> = store.list_videos().unwrap().into_iter().map(|v| v.id).collect();
    assert_eq!(order, [original.id, copy.id, after.id]);

    let copied = store.list_documents(copy.id).unwrap();
    assert_eq!(copied.len(), documents.len());
    for (copy_doc, original_doc) in copied.iter().zip(&documents) {
        assert_ne!(copy_doc.id, original_doc.id);
        assert_eq!(
            (copy_doc.kind, &copy_doc.title, copy_doc.position),
            (original_doc.kind, &original_doc.title, original_doc.position)
        );
        assert_eq!(shape_of(&copy_doc.blocks), shape_of(&original_doc.blocks));
        assert!(copy_doc.blocks.iter().zip(&original_doc.blocks).all(|(a, b)| a.id != b.id));
    }
    assert!(store.list_runs(Some(copy.id)).unwrap().is_empty());
    assert!(store.list_uploads(Some(copy.id)).unwrap().is_empty());
    assert_eq!(store.get_video(original.id).unwrap(), Some(original.clone()));

    // Duplicating the last video appends after it.
    let last_copy = store.duplicate_video(after.id).unwrap();
    assert_eq!(last_copy.position, after.position + 1.0);
    assert!(matches!(store.duplicate_video(VideoId::new()), Err(StoreError::NotFound { .. })));
}

#[test]
fn properties_crud_keeps_values_consistent() {
    let mut store = store();
    let priority =
        PropertyDef { id: PropertyId::new(), name: "Priority".into(), kind: select(&["high", "low"]), position: 1 };
    let notes = PropertyDef { id: PropertyId::new(), name: "Notes".into(), kind: PropertyKind::Text, position: 0 };
    store.insert_property(&priority).unwrap();
    store.insert_property(&notes).unwrap();
    assert_eq!(store.list_properties().unwrap(), vec![notes.clone(), priority.clone()]);

    let mut high = Video::new("High");
    high.properties.insert(priority.id, PropertyValue::Select(Some("high".into())));
    high.properties.insert(notes.id, PropertyValue::Text("keep me".into()));
    let mut low = Video::new("Low");
    low.properties.insert(priority.id, PropertyValue::Select(Some("low".into())));
    store.insert_video(&high).unwrap();
    store.insert_video(&low).unwrap();

    // Renaming keeps values; removing the "high" option drops values that used it.
    let renamed = PropertyDef { name: "Urgency".into(), kind: select(&["low", "medium"]), ..priority.clone() };
    store.update_property(&renamed).unwrap();
    assert_eq!(store.list_properties().unwrap()[1], renamed);
    let high_after = store.get_video(high.id).unwrap().unwrap();
    assert_eq!(high_after.properties.get(&priority.id), None);
    assert_eq!(high_after.properties.get(&notes.id), Some(&PropertyValue::Text("keep me".into())));
    assert_eq!(
        store.get_video(low.id).unwrap().unwrap().properties.get(&priority.id),
        Some(&PropertyValue::Select(Some("low".into())))
    );

    // Changing the kind clears the values.
    store.update_property(&PropertyDef { kind: PropertyKind::Number, ..renamed.clone() }).unwrap();
    assert!(store.get_video(low.id).unwrap().unwrap().properties.is_empty());

    let missing = PropertyDef { id: PropertyId::new(), ..notes.clone() };
    assert!(matches!(store.update_property(&missing), Err(StoreError::NotFound { entity: "property", .. })));
}

#[test]
fn multi_select_values_are_pruned_per_option() {
    let mut store = store();
    let options = |ids: &[&str]| match select(ids) {
        PropertyKind::Select { options } => PropertyKind::MultiSelect { options },
        _ => unreachable!(),
    };
    let topics =
        PropertyDef { id: PropertyId::new(), name: "Topics".into(), kind: options(&["a", "b", "c"]), position: 0 };
    store.insert_property(&topics).unwrap();
    let mut video = Video::new("Tagged");
    video.properties.insert(topics.id, PropertyValue::MultiSelect(vec!["a".into(), "b".into()]));
    store.insert_video(&video).unwrap();
    let mut only_b = Video::new("Only b");
    only_b.properties.insert(topics.id, PropertyValue::MultiSelect(vec!["b".into()]));
    store.insert_video(&only_b).unwrap();

    store.update_property(&PropertyDef { kind: options(&["a", "c"]), ..topics.clone() }).unwrap();
    assert_eq!(
        store.get_video(video.id).unwrap().unwrap().properties.get(&topics.id),
        Some(&PropertyValue::MultiSelect(vec!["a".into()]))
    );
    assert!(store.get_video(only_b.id).unwrap().unwrap().properties.is_empty());
}

#[test]
fn non_finite_numbers_are_rejected_instead_of_breaking_listings() {
    let mut store = store();
    let score = PropertyId::new();
    // `"nan".parse::<f64>()` and `"inf"` succeed, so a number cell can produce these.
    for number in [f64::NAN, f64::INFINITY] {
        let mut video = Video::new("Scored");
        video.properties.insert(score, PropertyValue::Number(number));
        assert!(store.insert_video(&video).is_err());
    }
    let mut video = insert(&mut store, "Scored", 0.0);
    video.properties.insert(score, PropertyValue::Number(f64::NEG_INFINITY));
    assert!(store.update_video(&mut video).is_err());
    assert_eq!(store.list_videos().unwrap().len(), 1);
    assert!(store.get_video(video.id).unwrap().unwrap().properties.is_empty());

    let mut view = store.list_views().unwrap().remove(0);
    view.columns[0].width = f32::NAN;
    assert!(store.save_view(&view).is_err());
    assert_eq!(store.list_views().unwrap().len(), 2);
}

#[test]
fn deleting_a_property_removes_values_and_view_columns() {
    let mut store = store();
    let sponsor = PropertyDef { id: PropertyId::new(), name: "Sponsor".into(), kind: PropertyKind::Text, position: 0 };
    let other = PropertyDef { id: PropertyId::new(), name: "Other".into(), kind: PropertyKind::Checkbox, position: 1 };
    store.insert_property(&sponsor).unwrap();
    store.insert_property(&other).unwrap();
    let mut video = Video::new("Sponsored");
    video.properties.insert(sponsor.id, PropertyValue::Text("CloudNest".into()));
    video.properties.insert(other.id, PropertyValue::Checkbox(true));
    store.insert_video(&video).unwrap();

    let mut view = store.list_views().unwrap().remove(0);
    for id in [sponsor.id, other.id] {
        view.columns.push(ColumnLayout { column: Column::Custom(id), width: 120.0, visible: true });
    }
    view.sorts.push(SortSpec { column: Column::Custom(sponsor.id), direction: SortDirection::Ascending });
    store.save_view(&view).unwrap();

    store.delete_property(sponsor.id).unwrap();
    assert_eq!(store.list_properties().unwrap(), vec![other.clone()]);
    let stored = store.get_video(video.id).unwrap().unwrap();
    assert_eq!(stored.properties, BTreeMap::from([(other.id, PropertyValue::Checkbox(true))]));
    assert_eq!(stored.updated_at, video.updated_at);
    let view = store.list_views().unwrap().remove(0);
    assert!(view.columns.iter().all(|c| c.column != Column::Custom(sponsor.id)));
    assert!(view.columns.iter().any(|c| c.column == Column::Custom(other.id)));
    assert!(view.sorts.is_empty());
}

#[test]
fn documents_crud() {
    let mut store = store();
    let video = insert(&mut store, "Docs", 0.0);
    let first = store.create_document(video.id, DocumentKind::Script, "Script").unwrap();
    let second = store.create_document(video.id, DocumentKind::Custom, "Shot list").unwrap();
    assert_eq!((first.position, second.position), (0, 1));
    assert_eq!(shape_of(&first.blocks), [(BlockKind::Paragraph, String::new())]);
    assert_eq!(store.get_document(second.id).unwrap(), Some(second.clone()));

    let mut edited = first.clone();
    edited.blocks = vec![
        Block { checked: true, ..Block::new(BlockKind::Todo, "Record") },
        Block { indent: 2, ..Block::new(BlockKind::Bulleted, "**nested**") },
    ];
    edited.title = "Final script".into();
    edited.position = 5;
    store.save_document(&mut edited).unwrap();
    assert!(edited.updated_at > first.updated_at);
    assert_eq!(store.get_document(first.id).unwrap(), Some(edited.clone()));
    assert_eq!(store.list_documents(video.id).unwrap(), vec![second.clone(), edited]);

    store.delete_document(second.id).unwrap();
    assert_eq!(store.get_document(second.id).unwrap(), None);

    let mut orphan = Document { id: DocumentId::new(), video_id: VideoId::new(), ..second };
    assert!(store.save_document(&mut orphan).is_err(), "documents need an existing video");
}

#[test]
fn views_upsert_and_delete() {
    let mut store = store();
    let mut view = DatabaseView {
        id: ViewId::new(),
        name: "Shorts".into(),
        kind: ViewKind::Table,
        columns: vec![ColumnLayout { column: Column::Builtin(BuiltinField::Title), width: 200.0, visible: true }],
        sorts: vec![SortSpec {
            column: Column::Builtin(BuiltinField::PublishDate),
            direction: SortDirection::Descending,
        }],
        filter: FilterSpec {
            search: "rust".into(),
            statuses: vec![VideoStatus::Idea, VideoStatus::Scripting],
            formats: vec![VideoFormat::Short],
            hide_archived: true,
        },
        position: 2,
    };
    store.save_view(&view).unwrap();
    assert_eq!(store.list_views().unwrap()[2], view);

    view.name = "Rust shorts".into();
    view.position = -1;
    store.save_view(&view).unwrap();
    let views = store.list_views().unwrap();
    assert_eq!(views.len(), 3);
    assert_eq!(views[0], view);

    store.delete_view(view.id).unwrap();
    assert_eq!(store.list_views().unwrap().len(), 2);
}

#[test]
fn runs_crud_and_event_log() {
    let mut store = store();
    let a = insert(&mut store, "A", 0.0);
    let b = insert(&mut store, "B", 1.0);
    let old = run(a.id, RunStatus::Planning, 30);
    let mut newer = run(b.id, RunStatus::Queued, 10);
    let newest = run(a.id, RunStatus::Queued, 1);
    for r in [&old, &newer, &newest] {
        store.insert_run(r).unwrap();
    }
    assert_eq!(store.list_runs(None).unwrap(), vec![newest.clone(), newer.clone(), old.clone()]);
    assert_eq!(store.list_runs(Some(a.id)).unwrap(), vec![newest.clone(), old.clone()]);

    let before = newer.updated_at;
    newer.status = RunStatus::Succeeded;
    newer.session_id = Some("session-1".into());
    newer.summary = Some("Cut 3 takes into 8:12".into());
    newer.output = Some("/media/b/footage/edit/final.mp4".into());
    newer.error = None;
    newer.cost_usd = Some(0.42);
    newer.harness = Some(EditHarness::VideoUse);
    store.update_run(&mut newer).unwrap();
    assert!(newer.updated_at > before);
    assert_eq!(store.get_run(newer.id).unwrap(), Some(newer.clone()));
    assert!(matches!(store.update_run(&mut run(a.id, RunStatus::Failed, 0)), Err(StoreError::NotFound { .. })));

    let payloads =
        [json!({"type": "system", "session_id": "s"}), json!({"type": "assistant", "text": "Plan"}), json!(42)];
    for (index, payload) in payloads.iter().enumerate() {
        let event = store.append_run_event(old.id, payload).unwrap();
        assert_eq!((event.run_id, event.seq, &event.payload), (old.id, index as u32 + 1, payload));
    }
    assert_eq!(store.append_run_event(newest.id, &json!("other run")).unwrap().seq, 1);
    let events = store.list_run_events(old.id).unwrap();
    assert_eq!(events.iter().map(|e| e.seq).collect::<Vec<_>>(), [1, 2, 3]);
    assert_eq!(events.iter().map(|e| e.payload.clone()).collect::<Vec<_>>(), payloads);
    assert!(events.windows(2).all(|pair| pair[0].at <= pair[1].at));
    assert!(store.append_run_event(RunId::new(), &json!({})).is_err(), "events need an existing run");
}

#[test]
fn runs_and_events_written_later_keep_what_the_caller_kept() {
    let mut store = store();
    let video = insert(&mut store, "A", 0.0);
    let mut kept = run(video.id, RunStatus::Planning, 30);
    store.insert_run(&kept).unwrap();
    kept.status = RunStatus::AwaitingApproval;
    kept.updated_at = Utc::now() - TimeDelta::seconds(5);
    store.replace_run(&kept).unwrap();
    assert_eq!(store.get_run(kept.id).unwrap(), Some(kept.clone()), "updated_at as the caller set it");
    assert!(matches!(store.replace_run(&run(video.id, RunStatus::Failed, 0)), Err(StoreError::NotFound { .. })));

    let event = |seq: u32, payload| RunEvent { run_id: kept.id, seq, at: Utc::now(), payload };
    let (first, second) = (event(1, json!("plan")), event(2, json!("done")));
    store.insert_run_event(&first).unwrap();
    store.insert_run_event(&second).unwrap();
    assert_eq!(store.list_run_events(kept.id).unwrap(), vec![first.clone(), second]);
    assert!(store.insert_run_event(&first).is_err(), "a number is used once");
    assert_eq!(store.append_run_event(kept.id, &json!("next")).unwrap().seq, 3);

    let mut job = upload(video.id, UploadStatus::Pending, 5);
    store.insert_upload(&job).unwrap();
    job.status = UploadStatus::Uploading { sent: 1, total: 2 };
    job.updated_at = Utc::now() - TimeDelta::seconds(5);
    store.replace_upload(&job).unwrap();
    assert_eq!(store.get_upload(job.id).unwrap(), Some(job));
}

#[test]
fn uploads_crud() {
    let mut store = store();
    let video = insert(&mut store, "Upload me", 0.0);
    let other = insert(&mut store, "Other", 1.0);
    let mut job = upload(video.id, UploadStatus::Pending, 5);
    let other_job = upload(other.id, UploadStatus::Cancelled, 1);
    store.insert_upload(&job).unwrap();
    store.insert_upload(&other_job).unwrap();
    assert_eq!(store.list_uploads(None).unwrap(), vec![other_job.clone(), job.clone()]);

    job.status = UploadStatus::Uploading { sent: 1 << 20, total: 1 << 30 };
    job.session_url = Some("https://upload.example/session/1".into());
    job.file_stamp = Some(FileStamp { size: 1 << 30, modified: Some(Utc::now()) });
    job.channel = Some("UC123".into());
    store.update_upload(&mut job).unwrap();
    assert_eq!(store.get_upload(job.id).unwrap(), Some(job.clone()));
    assert_eq!(store.list_uploads(Some(video.id)).unwrap(), vec![job.clone()]);

    let state: String =
        store.conn.query_row("SELECT state FROM uploads WHERE id = ?1", [job.id], |r| r.get(0)).unwrap();
    assert_eq!(state, "uploading");
    assert!(matches!(
        store.update_upload(&mut upload(video.id, UploadStatus::Pending, 0)),
        Err(StoreError::NotFound { entity: "upload", .. })
    ));
}

#[test]
fn settings_round_trip() {
    let mut store = store();
    assert_eq!(store.load_settings().unwrap(), AppSettings::default());

    let mut settings = AppSettings {
        onboarding_completed: true,
        theme: ThemeMode::Dark,
        media_root: Some("/home/me/Videos/Rushes".into()),
        youtube: YoutubeBackend::Google {
            client_id: "id.apps.googleusercontent.com".into(),
            client_secret: "s".into(),
        },
        ..AppSettings::default()
    };
    settings.pipeline.model = Some("claude-opus-5-5".into());
    settings.pipeline.default_preferences.subtitles = true;
    store.save_settings(&settings).unwrap();
    assert_eq!(store.load_settings().unwrap(), settings);

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Window {
        width: u32,
        maximized: bool,
    }
    assert_eq!(store.get_setting::<Window>("window").unwrap(), None);
    store.set_setting("window", &Window { width: 1280, maximized: false }).unwrap();
    store.set_setting("window", &Window { width: 1440, maximized: true }).unwrap();
    assert_eq!(store.get_setting("window").unwrap(), Some(Window { width: 1440, maximized: true }));
    store.set_setting("recent", &["a", "b"][..]).unwrap();
    assert_eq!(store.get_setting::<Vec<String>>("recent").unwrap(), Some(vec!["a".into(), "b".into()]));
    assert!(store.get_setting::<Window>("recent").is_err(), "type mismatches are reported");
}

#[test]
fn sample_workspace_is_complete() {
    let mut store = store();
    insert(&mut store, "Mine", 0.0);
    let videos = store.seed_sample_workspace().unwrap();
    assert!(videos.len() >= 6);
    assert!(videos.iter().all(|v| v.position >= 1.0));

    let statuses: std::collections::HashSet<_> = videos.iter().map(|v| v.status).collect();
    assert!(statuses.len() >= 6, "{statuses:?}");
    assert!(videos.iter().any(|v| v.format == VideoFormat::Short));
    assert!(videos.iter().any(|v| v.youtube.is_some() && v.status == VideoStatus::Published));

    let properties = store.list_properties().unwrap();
    let names: Vec<_> = properties.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, ["Priority", "Sponsor"]);
    for video in &videos {
        assert!(video.properties.contains_key(&properties[0].id), "{} has a priority", video.title);
        for (id, value) in &video.properties {
            let property = properties.iter().find(|p| p.id == *id).expect("value of a known property");
            let PropertyValue::Select(Some(option)) = value else { panic!("unexpected value {value:?}") };
            assert!(property.kind.options().iter().any(|o| &o.id == option));
        }
        let documents = store.list_documents(video.id).unwrap();
        assert_eq!(documents.iter().map(|d| d.kind).collect::<Vec<_>>(), [DocumentKind::Script, DocumentKind::Notes]);
    }

    let rows = store.list_video_rows().unwrap();
    assert_eq!(rows.len(), videos.len() + 1);
    for row in rows.iter().skip(1) {
        // Videos still on a template have only their hook written; guidance is not script.
        assert!(row.script_words > 5, "{} has a script", row.video.title);
        if row.video.format == VideoFormat::Short {
            assert!(row.script_seconds < 60, "{} fits a Short", row.video.title);
        }
    }

    let table = store.list_views().unwrap().remove(0);
    for property in &properties {
        assert!(table.columns.iter().any(|c| c.column == Column::Custom(property.id) && c.visible));
    }

    // Seeding again appends instead of colliding.
    let again = store.seed_sample_workspace().unwrap();
    assert!(again[0].position > videos.last().unwrap().position);
    assert_eq!(store.list_properties().unwrap().len(), 4);
}
