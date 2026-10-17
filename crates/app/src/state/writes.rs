//! Writes to the database, queued and retried in the background.
//!
//! Every change the creator makes goes through [`AppState`]. While nothing is
//! queued it is written at once on the UI thread's connection, which waits at
//! most [`UI_BUSY_TIMEOUT`] for another program's lock. Otherwise (the database
//! is busy, or earlier changes still wait) the change joins the queue: the
//! cached rows show it at once, and a background task writes the queue in order
//! on its own connection, retrying every [`SAVE_RETRY_DELAY`] until it lands.
//! The queue keeps only the newest value of a video, document, view or the
//! settings. Whatever is still queued when the app quits is written before it
//! exits, waiting at most [`QUIT_SAVE_TIMEOUT`] for a lock; what can't be
//! written by then is kept in a file next to the database and written at the
//! next start.
//!
//! The background services' records (AI edit runs and their logs, uploads,
//! their own settings) take the same way: the services keep them in memory,
//! and the queue writes them in order, the newest state of each run and
//! upload and every log entry.

use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use gpui_kit::{AppContext as _, Context, Subscription, Task};
use rushes_core::document::{DEFAULT_WPM, runtime_secs_for_words, spoken_word_count};
use rushes_core::settings::AppSettings;
use rushes_core::store::{forget_property, reconcile_property_value};
use rushes_core::{
    DatabaseView, Document, DocumentId, DocumentKind, PipelineRun, PropertyDef, PropertyId, RunEvent, RunId, Store,
    StoreError, UploadId, UploadJob, Video, VideoId, VideoRow, ViewId,
};
use rusqlite::ErrorCode;
use serde::{Deserialize, Serialize};

use super::{AppEvent, AppState, Problem, Severity, SharedStore};

/// How long the UI thread waits for another program's lock before it hands a
/// write to the background writer. Reads don't wait for writers (WAL).
pub const UI_BUSY_TIMEOUT: Duration = Duration::from_millis(100);
/// How long one background attempt waits for a lock before it counts as failed.
pub const WRITE_BUSY_TIMEOUT: Duration = Duration::from_secs(1);
/// How long a failed write waits before the next attempt.
pub const SAVE_RETRY_DELAY: Duration = Duration::from_secs(2);
/// How long quitting waits in all for queued writes to land.
pub const QUIT_SAVE_TIMEOUT: Duration = Duration::from_secs(5);
/// Next to the database: writes that couldn't land before the app quit.
const UNSAVED_FILE: &str = "unsaved-changes.json";

/// A change to the database.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) enum Write {
    Video(Video),
    CreateVideo {
        video: Video,
        documents: Vec<Document>,
    },
    DeleteVideos(Vec<VideoId>),
    InsertProperty(PropertyDef),
    UpdateProperty(PropertyDef),
    DeleteProperty(PropertyId),
    SaveView(DatabaseView),
    DeleteView(ViewId),
    Settings(AppSettings),
    Document(Document),
    SeedSample,
    /// A new AI edit run.
    InsertRun(PipelineRun),
    /// An AI edit run's state, as the pipeline service keeps it.
    Run(PipelineRun),
    /// An entry of an AI edit run's log, numbered by the pipeline service.
    RunEvent {
        video_id: VideoId,
        event: RunEvent,
    },
    InsertUpload(UploadJob),
    /// An upload's state, as the YouTube service keeps it.
    Upload(UploadJob),
    /// A value of the settings table other than the app settings.
    Setting {
        key: String,
        value: serde_json::Value,
    },
}

/// What a write replaces as a whole, so a later write of the same key makes an
/// earlier one unnecessary.
#[derive(Clone, PartialEq)]
enum Key {
    Video(VideoId),
    Document(DocumentId),
    View(ViewId),
    Settings,
    Run(RunId),
    /// Log entries are never replaced, and don't keep other writes apart.
    RunEvent(RunId, u32),
    Upload(UploadId),
    Setting(String),
}

impl Write {
    /// `None` for writes whose effect depends on what was written before them
    /// (creating, deleting, property definitions): later writes never move
    /// in front of them.
    fn key(&self) -> Option<Key> {
        match self {
            Write::Video(video) => Some(Key::Video(video.id)),
            Write::Document(document) => Some(Key::Document(document.id)),
            Write::SaveView(view) => Some(Key::View(view.id)),
            Write::Settings(_) => Some(Key::Settings),
            Write::Run(run) => Some(Key::Run(run.id)),
            Write::RunEvent { event, .. } => Some(Key::RunEvent(event.run_id, event.seq)),
            Write::Upload(job) => Some(Key::Upload(job.id)),
            Write::Setting { key, .. } => Some(Key::Setting(key.clone())),
            _ => None,
        }
    }

    fn apply(&self, store: &mut Store) -> Result<(), StoreError> {
        match self {
            Write::Video(video) => store.update_video(&mut video.clone()),
            Write::CreateVideo { video, documents } => store.insert_video_with_documents(video, documents),
            Write::DeleteVideos(ids) => ids.iter().try_for_each(|id| store.delete_video(*id)),
            Write::InsertProperty(property) => store.insert_property(property),
            Write::UpdateProperty(property) => store.update_property(property),
            Write::DeleteProperty(id) => store.delete_property(*id),
            Write::SaveView(view) => store.save_view(view),
            Write::DeleteView(id) => store.delete_view(*id),
            Write::Settings(settings) => store.save_settings(settings),
            Write::Document(document) => store.save_document(&mut document.clone()),
            Write::SeedSample => store.seed_sample_workspace().map(drop),
            Write::InsertRun(run) => store.insert_run(run),
            Write::Run(run) => store.replace_run(run),
            Write::RunEvent { event, .. } => store.insert_run_event(event),
            Write::InsertUpload(job) => store.insert_upload(job),
            Write::Upload(job) => store.replace_upload(job),
            Write::Setting { key, value } => store.set_setting(key, value),
        }
    }

    /// The records the write changes: its video, and its run or upload.
    fn targets(&self) -> Vec<Target> {
        match self {
            Write::DeleteVideos(ids) => ids.iter().copied().map(Target::Video).collect(),
            Write::InsertRun(run) | Write::Run(run) => vec![Target::Video(run.video_id), Target::Run(run.id)],
            Write::RunEvent { video_id, event } => vec![Target::Video(*video_id), Target::Run(event.run_id)],
            Write::InsertUpload(job) | Write::Upload(job) => vec![Target::Video(job.video_id), Target::Upload(job.id)],
            write => write.video_id().map(Target::Video).into_iter().collect(),
        }
    }

    /// What the later writes wait for while this one fails: a write that
    /// depends on order holds back the later writes of what it creates or
    /// deletes, so none of them lands before it (or is refused as gone);
    /// `None` holds back every later write. Other writes hold nothing back.
    fn holds(&self) -> Option<Vec<Target>> {
        match self {
            _ if self.key().is_some() => Some(Vec::new()),
            Write::CreateVideo { .. } | Write::DeleteVideos(_) => Some(self.targets()),
            Write::InsertRun(run) => Some(vec![Target::Run(run.id)]),
            Write::InsertUpload(job) => Some(vec![Target::Upload(job.id)]),
            _ => None,
        }
    }

    /// The video the write belongs to.
    fn video_id(&self) -> Option<VideoId> {
        match self {
            Write::Video(video) | Write::CreateVideo { video, .. } => Some(video.id),
            Write::Document(document) => Some(document.video_id),
            Write::InsertRun(run) | Write::Run(run) => Some(run.video_id),
            Write::RunEvent { video_id, .. } => Some(*video_id),
            Write::InsertUpload(job) | Write::Upload(job) => Some(job.video_id),
            _ => None,
        }
    }

    /// Whether the cached rows, properties or views follow the stored ones
    /// after it. The services reload the rows themselves after their records
    /// change, as the rows only summarize them.
    fn changes_data(&self) -> bool {
        !matches!(self, Write::Settings(_) | Write::Setting { .. }) && self.record().is_none()
    }

    /// The service record the write saves, if it is one.
    fn record(&self) -> Option<Record> {
        match self {
            Write::InsertRun(_) | Write::Run(_) | Write::RunEvent { .. } => Some(Record::Edit),
            Write::InsertUpload(_) | Write::Upload(_) => Some(Record::Upload),
            _ => None,
        }
    }

    /// Whether the creator hears that the database refused it. A run or an
    /// upload that is gone was deleted with its video.
    fn reports_refusal(&self, error: &StoreError) -> bool {
        self.record().is_none() || !matches!(error, StoreError::NotFound { .. })
    }

    /// What failed, as the title of a problem.
    fn failure_title(&self) -> &'static str {
        match self {
            Write::CreateVideo { .. } => "Couldn’t create the video",
            Write::Settings(_) => "Couldn’t save the settings",
            Write::InsertRun(_) | Write::Run(_) | Write::RunEvent { .. } => "Couldn’t save the AI edit",
            Write::InsertUpload(_) | Write::Upload(_) => "Couldn’t save the upload",
            _ => "Couldn’t save your changes",
        }
    }

    /// Shows the write in the cached data as the database will have it.
    /// Returns whether anything changed.
    fn apply_cached(&self, cached: &mut Cached<'_>) -> bool {
        match self {
            Write::Video(video) => match cached.rows.iter_mut().find(|row| row.video.id == video.id) {
                Some(row) if row.video != *video => {
                    row.video = video.clone();
                    cached.sort_rows();
                    true
                }
                _ => false,
            },
            Write::CreateVideo { video, documents } => {
                if cached.rows.iter().any(|row| row.video.id == video.id) {
                    return false;
                }
                let script = documents.iter().filter(|document| document.kind == DocumentKind::Script);
                let script_words = script
                    .min_by_key(|document| (document.position, document.id))
                    .map_or(0, |document| spoken_word_count(&document.blocks));
                cached.rows.push(VideoRow {
                    video: video.clone(),
                    script_words,
                    script_seconds: runtime_secs_for_words(script_words, DEFAULT_WPM),
                    latest_run: None,
                    has_finished_edit: false,
                    latest_upload: None,
                });
                cached.sort_rows();
                true
            }
            Write::DeleteVideos(ids) => {
                let before = cached.rows.len();
                cached.rows.retain(|row| !ids.contains(&row.video.id));
                cached.rows.len() != before
            }
            Write::InsertProperty(property) => {
                if cached.properties.iter().any(|known| known.id == property.id) {
                    return false;
                }
                cached.properties.push(property.clone());
                cached.properties.sort_by_key(|property| (property.position, property.id));
                true
            }
            Write::UpdateProperty(property) => {
                let Some(known) = cached.properties.iter_mut().find(|known| known.id == property.id) else {
                    return false;
                };
                let old = std::mem::replace(known, property.clone());
                cached.properties.sort_by_key(|property| (property.position, property.id));
                for row in cached.rows.iter_mut() {
                    reconcile_property_value(&old.kind, property, &mut row.video.properties);
                }
                true
            }
            Write::DeleteProperty(id) => {
                cached.properties.retain(|property| property.id != *id);
                for row in cached.rows.iter_mut() {
                    row.video.properties.remove(id);
                }
                for view in cached.views.iter_mut() {
                    forget_property(view, *id);
                }
                true
            }
            Write::SaveView(view) => {
                match cached.views.iter_mut().find(|known| known.id == view.id) {
                    Some(known) if known == view => return false,
                    Some(known) => *known = view.clone(),
                    None => cached.views.push(view.clone()),
                }
                cached.views.sort_by_key(|view| (view.position, view.id));
                true
            }
            Write::DeleteView(id) => {
                let before = cached.views.len();
                cached.views.retain(|view| view.id != *id);
                cached.views.len() != before
            }
            Write::Document(document) if document.kind == DocumentKind::Script => {
                let Some(row) = cached.rows.iter_mut().find(|row| row.video.id == document.video_id) else {
                    return false;
                };
                let script_words = spoken_word_count(&document.blocks);
                if row.script_words == script_words {
                    return false;
                }
                row.script_words = script_words;
                row.script_seconds = runtime_secs_for_words(script_words, DEFAULT_WPM);
                true
            }
            Write::Settings(_)
            | Write::Document(_)
            | Write::SeedSample
            | Write::InsertRun(_)
            | Write::Run(_)
            | Write::RunEvent { .. }
            | Write::InsertUpload(_)
            | Write::Upload(_)
            | Write::Setting { .. } => false,
        }
    }
}

/// The kinds of service records, for what a failure is about.
#[derive(Clone, Copy, PartialEq)]
enum Record {
    Edit,
    Upload,
}

/// A record in the database, for keeping the writes of it in order.
#[derive(Clone, Copy, PartialEq)]
enum Target {
    Video(VideoId),
    Run(RunId),
    Upload(UploadId),
}

/// The data [`AppState`] caches, as a write changes it.
pub(super) struct Cached<'a> {
    pub rows: &'a mut Vec<VideoRow>,
    pub properties: &'a mut Vec<PropertyDef>,
    pub views: &'a mut Vec<DatabaseView>,
}

impl Cached<'_> {
    /// The order [`Store::list_video_rows`] returns.
    fn sort_rows(&mut self) {
        self.rows.sort_by(|a, b| {
            a.video.position.total_cmp(&b.video.position).then(a.video.created_at.cmp(&b.video.created_at))
        });
    }
}

/// Writes not in the database yet, oldest first.
#[derive(Default)]
pub(crate) struct Queue {
    entries: Vec<Entry>,
    revision: u64,
}

struct Entry {
    revision: u64,
    write: Write,
    /// The last attempt failed, or the database was busy before it got its
    /// turn. It stays queued and is retried.
    failing: bool,
}

impl Queue {
    /// Adds `write` after the queued ones. A queued write of the same key takes
    /// its value instead, unless a write that depends on order came after it.
    fn push(&mut self, write: Write) {
        self.revision += 1;
        let revision = self.revision;
        if let Some(key) = write.key() {
            for entry in self.entries.iter_mut().rev() {
                match entry.write.key() {
                    Some(queued) if queued == key => {
                        entry.write = write;
                        entry.revision = revision;
                        return;
                    }
                    Some(_) => {}
                    None => break,
                }
            }
        }
        self.entries.push(Entry { revision, write, failing: false });
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn len(&self) -> usize {
        self.entries.len()
    }

    fn writes(&self) -> Vec<(u64, Write)> {
        self.entries.iter().map(|entry| (entry.revision, entry.write.clone())).collect()
    }

    /// Removes the entry of `revision`; a newer value that took its place stays.
    fn remove(&mut self, revision: u64) {
        self.entries.retain(|entry| entry.revision != revision);
    }

    fn mark_failing(&mut self, revision: Option<u64>) {
        for entry in &mut self.entries {
            if revision.is_none_or(|revision| entry.revision == revision) {
                entry.failing = true;
            }
        }
    }

    /// Drops the failing writes of videos that are gone: deleted meanwhile,
    /// their documents went with them.
    fn drop_failing(&mut self, exists: impl Fn(VideoId) -> bool) {
        self.entries.retain(|entry| !entry.failing || entry.write.video_id().is_none_or(&exists));
    }

    /// The newest queued snapshot of a document.
    pub(crate) fn document(&self, id: DocumentId) -> Option<Document> {
        self.entries.iter().rev().find_map(|entry| match &entry.write {
            Write::Document(document) if document.id == id => Some(document.clone()),
            Write::CreateVideo { documents, .. } => documents.iter().find(|document| document.id == id).cloned(),
            _ => None,
        })
    }

    pub(crate) fn is_document_failing(&self, id: DocumentId) -> bool {
        self.entries
            .iter()
            .any(|entry| entry.failing && matches!(&entry.write, Write::Document(document) if document.id == id))
    }

    fn is_video_failing(&self, id: VideoId) -> bool {
        self.entries.iter().any(|entry| entry.failing && entry.write.video_id() == Some(id))
    }

    /// Replays the queued writes on data read from the database.
    fn overlay(&self, cached: &mut Cached<'_>) {
        for entry in &self.entries {
            entry.write.apply_cached(cached);
        }
    }

    /// Replays the queued writes on the stored documents of a video.
    fn overlay_documents(&self, video_id: VideoId, documents: &mut Vec<Document>) {
        let put = |documents: &mut Vec<Document>, document: &Document| match documents
            .iter_mut()
            .find(|known| known.id == document.id)
        {
            Some(known) => *known = document.clone(),
            None => documents.push(document.clone()),
        };
        for entry in &self.entries {
            match &entry.write {
                Write::Document(document) if document.video_id == video_id => put(documents, document),
                Write::CreateVideo { video, documents: created } if video.id == video_id => {
                    for document in created {
                        put(documents, document);
                    }
                }
                Write::DeleteVideos(ids) if ids.contains(&video_id) => documents.clear(),
                _ => {}
            }
        }
        documents.sort_by_key(|document| (document.position, document.id));
    }

    /// Replays the queued writes on runs read from the database, newest first.
    pub(super) fn overlay_runs(&self, runs: &mut Vec<PipelineRun>) {
        for entry in &self.entries {
            match &entry.write {
                Write::InsertRun(run) | Write::Run(run) => match runs.iter_mut().find(|known| known.id == run.id) {
                    Some(known) => *known = run.clone(),
                    None if matches!(entry.write, Write::InsertRun(_)) => runs.push(run.clone()),
                    None => {}
                },
                Write::DeleteVideos(ids) => runs.retain(|run| !ids.contains(&run.video_id)),
                _ => {}
            }
        }
        runs.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(b.id.cmp(&a.id)));
    }

    /// Adds the queued entries of a run's log to the ones read from the
    /// database, in order; an entry written meanwhile is not repeated.
    pub(super) fn overlay_run_events(&self, run_id: RunId, events: &mut Vec<RunEvent>) {
        for entry in &self.entries {
            if let Write::RunEvent { event, .. } = &entry.write
                && event.run_id == run_id
                && !events.iter().any(|known| known.seq == event.seq)
            {
                events.push(event.clone());
            }
        }
        events.sort_by_key(|event| event.seq);
    }

    /// Replays the queued writes on uploads read from the database, newest first.
    pub(super) fn overlay_uploads(&self, jobs: &mut Vec<UploadJob>) {
        for entry in &self.entries {
            match &entry.write {
                Write::InsertUpload(job) | Write::Upload(job) => match jobs.iter_mut().find(|known| known.id == job.id)
                {
                    Some(known) => *known = job.clone(),
                    None if matches!(entry.write, Write::InsertUpload(_)) => jobs.push(job.clone()),
                    None => {}
                },
                Write::DeleteVideos(ids) => jobs.retain(|job| !ids.contains(&job.video_id)),
                _ => {}
            }
        }
        jobs.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(b.id.cmp(&a.id)));
    }

    /// The newest queued app settings.
    pub(super) fn settings(&self) -> Option<&AppSettings> {
        self.entries.iter().rev().find_map(|entry| match &entry.write {
            Write::Settings(settings) => Some(settings),
            _ => None,
        })
    }

    /// The newest queued value of a settings key other than the app settings.
    pub(super) fn setting(&self, key: &str) -> Option<&serde_json::Value> {
        self.entries.iter().rev().find_map(|entry| match &entry.write {
            Write::Setting { key: queued, value } if queued == key => Some(value),
            _ => None,
        })
    }

    /// What the failing writes are, for a sentence: "Couldn’t save …".
    fn failing_subject(&self) -> Subject {
        let mut records = self.entries.iter().filter(|entry| entry.failing).map(|entry| entry.write.record());
        match records.next() {
            Some(Some(first)) if records.all(|record| record == Some(first)) => {
                return match first {
                    Record::Edit => Subject::Edit,
                    Record::Upload => Subject::Upload,
                };
            }
            _ => {}
        }
        let mut kinds = self.entries.iter().filter(|entry| entry.failing).map(|entry| match &entry.write {
            Write::Document(document) => Some(document.kind),
            _ => None,
        });
        let Some(first) = kinds.next() else { return Subject::Changes };
        match (first, kinds.all(|kind| kind == first)) {
            (Some(DocumentKind::Script), true) => Subject::Script,
            (Some(DocumentKind::Notes), true) => Subject::Notes,
            (Some(_), _)
                if self.entries.iter().all(|entry| !entry.failing || matches!(entry.write, Write::Document(_))) =>
            {
                Subject::Writing
            }
            _ => Subject::Changes,
        }
    }
}

#[derive(Clone, Copy)]
enum Subject {
    Script,
    Notes,
    Writing,
    Edit,
    Upload,
    Changes,
}

impl Subject {
    fn name(self) -> &'static str {
        match self {
            Subject::Script => "the script",
            Subject::Notes => "the notes",
            Subject::Writing => "your writing",
            Subject::Edit => "the AI edit’s progress",
            Subject::Upload => "the upload’s progress",
            Subject::Changes => "your changes",
        }
    }
}

/// A handle on the queued writes, readable without a context.
#[derive(Clone)]
pub(crate) struct QueuedWrites(Arc<Mutex<Queue>>);

impl QueuedWrites {
    pub(crate) fn read(&self) -> MutexGuard<'_, Queue> {
        lock(&self.0)
    }
}

/// The outcome of one pass over the queue.
#[derive(Default)]
struct Pass {
    written: usize,
    /// Writes the database can never accept, removed from the queue.
    refused: Vec<(Write, StoreError)>,
    /// Why writes stay queued; a busy database wins over other reasons.
    failed: Option<StoreError>,
}

/// Writes the queued entries in order. The store is locked first and the queue
/// read under it, so of two passes the later one writes the later values. An
/// entry leaves the queue once written, unless a newer value took its place
/// meanwhile. A busy database ends the pass: the writes after it would wait
/// just as long, and wait their turn so they land in order. Another failure
/// holds back what [`Write::holds`] names; those writes wait for the retry.
fn write_queue(store: &SharedStore, queue: &Mutex<Queue>) -> Pass {
    let mut store = lock(store);
    let writes = lock(queue).writes();
    let mut pass = Pass::default();
    let mut held = Vec::new();
    for (revision, write) in writes {
        if write.targets().iter().any(|target| held.contains(target)) {
            lock(queue).mark_failing(Some(revision));
            continue;
        }
        match write.apply(&mut store) {
            Ok(()) => {
                lock(queue).remove(revision);
                pass.written += 1;
            }
            Err(error) if is_refused(&error) => {
                lock(queue).remove(revision);
                pass.refused.push((write, error));
            }
            Err(error) => {
                let holds = write.holds().filter(|_| !error.is_busy());
                lock(queue).mark_failing(if holds.is_none() { None } else { Some(revision) });
                if error.is_busy() || pass.failed.is_none() {
                    pass.failed = Some(error);
                }
                match holds {
                    Some(targets) => held.extend(targets),
                    None => break,
                }
            }
        }
    }
    pass
}

/// Errors that trying again can't fix: what the write changes is gone, or a
/// value the database can't hold. Anything else (a lock, a full disk, a
/// read-only folder, a video row missing for now) may pass later.
fn is_refused(error: &StoreError) -> bool {
    match error {
        StoreError::NotFound { .. } => true,
        StoreError::Sqlite(rusqlite::Error::ToSqlConversionFailure(_)) => true,
        StoreError::Sqlite(rusqlite::Error::SqliteFailure(error, _)) => {
            error.code == ErrorCode::ConstraintViolation
                && error.extended_code != rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY
        }
        _ => false,
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // A panic while holding the lock leaves the data usable: SQLite
    // transactions roll back on their own, and the queue is only edited whole.
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The background writer and its queue, owned by [`AppState`].
pub(super) struct Writes {
    /// The background writer's connection.
    store: SharedStore,
    queue: Arc<Mutex<Queue>>,
    /// The pass in flight, or the wait before a retry.
    worker: Option<Task<()>>,
    /// Whether the creator has been told that saving fails, so retries stay quiet.
    reported: bool,
    /// Set once the app quits: later writes are written at once, until then.
    quit_deadline: Option<Instant>,
    /// Where writes that couldn't land before quitting are kept; `None` for
    /// an in-memory database.
    unsaved_file: Option<PathBuf>,
    /// Whether that file holds writes, to be removed once they landed.
    unsaved_kept: bool,
    /// How many writes quitting kept last, so each quit hook doesn't log them again.
    logged_kept: usize,
    _quit: Option<Subscription>,
}

impl Writes {
    /// The writer on `store` for the database at `database`. Writes kept when
    /// the app last quit are queued again and tried once; the problem says so.
    pub(super) fn new(store: SharedStore, database: Option<&Path>) -> (Self, Option<Problem>) {
        if let Err(error) = lock(&store).set_busy_timeout(WRITE_BUSY_TIMEOUT) {
            log_line!("rushes: can't set the writer's busy timeout: {error}");
        }
        let unsaved_file = database.and_then(Path::parent).map(|folder| folder.join(UNSAVED_FILE));
        let queue = Mutex::new(Queue::default());
        let mut reported = false;
        let problem = unsaved_file.as_deref().and_then(read_unsaved).map(|kept| match kept {
            Ok(writes) => {
                let mut queued = lock(&queue);
                writes.into_iter().for_each(|write| queued.push(write));
                drop(queued);
                // Before the window opens; the background writer retries the rest.
                let pass = write_queue(&store, &queue);
                log_refused(&pass);
                let saved = lock(&queue).is_empty();
                // The notice says they wait for the database to be free, so
                // the retries stay quiet until a pass succeeds.
                reported = !saved && pass.failed.as_ref().is_some_and(StoreError::is_busy);
                recovered_problem(saved)
            }
            Err(problem) => problem,
        });
        let unsaved_kept = unsaved_file.as_deref().is_some_and(Path::exists);
        let mut writes = Self {
            store,
            queue: Arc::new(queue),
            worker: None,
            reported,
            quit_deadline: None,
            unsaved_file,
            unsaved_kept,
            logged_kept: 0,
            _quit: None,
        };
        if writes.is_empty() {
            // Written: the file must not be replayed over later edits.
            writes.keep_unsaved();
        }
        (writes, problem)
    }

    pub(super) fn queued(&self) -> QueuedWrites {
        QueuedWrites(self.queue.clone())
    }

    /// Whether a write can go straight to the database: nothing waits before it.
    fn is_idle(&self) -> bool {
        self.worker.is_none() && self.quit_deadline.is_none() && lock(&self.queue).is_empty()
    }

    pub(super) fn overlay(&self, cached: &mut Cached<'_>) {
        lock(&self.queue).overlay(cached);
    }

    pub(super) fn overlay_documents(&self, video_id: VideoId, documents: &mut Vec<Document>) {
        lock(&self.queue).overlay_documents(video_id, documents);
    }

    pub(super) fn is_video_failing(&self, id: VideoId) -> bool {
        lock(&self.queue).is_video_failing(id)
    }

    pub(super) fn is_empty(&self) -> bool {
        lock(&self.queue).is_empty()
    }

    /// Keeps what is still queued in the unsaved file, or removes the file
    /// once nothing is left.
    fn keep_unsaved(&mut self) {
        let writes: Vec<Write> = lock(&self.queue).entries.iter().map(|entry| entry.write.clone()).collect();
        let changes = match writes.len() {
            1 => "1 change".to_string(),
            count => format!("{count} changes"),
        };
        let log = std::mem::replace(&mut self.logged_kept, writes.len()) != writes.len();
        let Some(file) = &self.unsaved_file else {
            if log && !writes.is_empty() {
                log_line!("rushes: {changes} couldn’t be saved before quitting");
            }
            return;
        };
        if writes.is_empty() {
            if self.unsaved_kept {
                match fs::remove_file(file) {
                    Ok(()) => self.unsaved_kept = false,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => self.unsaved_kept = false,
                    Err(error) => log_line!("rushes: can't remove {}: {error}", file.display()),
                }
            }
            return;
        }
        match write_private_json(file, &writes) {
            Ok(()) => {
                self.unsaved_kept = true;
                if log {
                    log_line!(
                        "rushes: {changes} couldn’t be saved before quitting; kept in {} for the next start",
                        file.display()
                    );
                }
            }
            Err(error) => log_line!("rushes: {changes} are lost, can't write {}: {error}", file.display()),
        }
    }
}

impl AppState {
    /// Writes `write` now when nothing is queued, else queues it for the
    /// background writer. Returns `false` when the database refused it, which
    /// is reported.
    pub(super) fn save(&mut self, write: Write, cx: &mut Context<Self>) -> bool {
        if self.writes.is_idle() {
            let result = write.apply(&mut self.lock());
            match result {
                Ok(()) => {
                    if write.changes_data() {
                        self.reload(cx);
                    }
                    return true;
                }
                Err(error) if is_refused(&error) => {
                    if write.reports_refusal(&error) {
                        self.report_problem(Problem::store(write.failure_title(), &error), cx);
                    } else {
                        log_line!("rushes: {}: {error}", write.failure_title());
                    }
                    return false;
                }
                // Busy or failing: the background writer retries it.
                Err(error) => {
                    if error.is_busy() {
                        self.lock_waits += 1;
                    }
                }
            }
        }
        self.queue(write, cx);
        true
    }

    /// Queues `write` for the background writer and shows it in the cached
    /// data at once.
    pub(super) fn queue(&mut self, write: Write, cx: &mut Context<Self>) {
        let changed = write.apply_cached(&mut self.cached());
        lock(&self.writes.queue).push(write);
        if self.writes.quit_deadline.is_some() {
            self.write_before_quit();
        } else {
            self.start_writes(cx);
        }
        if changed {
            cx.emit(AppEvent::DataChanged);
        }
        cx.notify();
    }

    /// Starts a background pass over the queue unless one runs or waits.
    pub(super) fn start_writes(&mut self, cx: &mut Context<Self>) {
        if self.writes._quit.is_none() {
            // Quitting must not lose a queued write, even one whose page is gone.
            self.writes._quit = Some(cx.on_app_quit(|this: &mut Self, _| {
                this.write_before_quit();
                async {}
            }));
        }
        if self.writes.worker.is_some() || self.writes.quit_deadline.is_some() || self.writes.is_empty() {
            return;
        }
        let (store, queue) = (self.writes.store.clone(), self.writes.queue.clone());
        self.writes.worker = Some(cx.spawn(async move |this, cx| {
            let pass = cx.background_spawn(async move { write_queue(&store, &queue) }).await;
            _ = this.update(cx, |this, cx| {
                this.writes.worker = None;
                this.finish_writes(pass, cx);
            });
        }));
    }

    fn finish_writes(&mut self, pass: Pass, cx: &mut Context<Self>) {
        for (write, error) in &pass.refused {
            if write.reports_refusal(error) {
                self.report_problem(Problem::store(write.failure_title(), error), cx);
            } else {
                log_line!("rushes: {}: {error}", write.failure_title());
            }
        }
        let known: Vec<VideoId> = self.rows.iter().map(|row| row.video.id).collect();
        lock(&self.writes.queue).drop_failing(|id| known.contains(&id));
        if pass.written > 0 || !pass.refused.is_empty() {
            // Derived values (script stats) follow the stored data, and a
            // refused write leaves the cached rows.
            self.reload(cx);
        }
        match pass.failed {
            Some(error) if !self.writes.is_empty() => {
                self.report_failing(&error, cx);
                self.writes.worker = Some(cx.spawn(async move |this, cx| {
                    cx.background_executor().timer(SAVE_RETRY_DELAY).await;
                    _ = this.update(cx, |this, cx| {
                        this.writes.worker = None;
                        this.start_writes(cx);
                    });
                }));
            }
            _ => {
                self.writes.reported = false;
                if self.writes.is_empty() {
                    self.writes.keep_unsaved();
                }
                // Writes queued while the pass ran.
                self.start_writes(cx);
            }
        }
        cx.notify();
    }

    /// Tells the creator once that saving fails; pages show it until a retry succeeds.
    fn report_failing(&mut self, error: &StoreError, cx: &mut Context<Self>) {
        if std::mem::replace(&mut self.writes.reported, true) {
            return;
        }
        log_line!("rushes: couldn’t save, retrying: {error}");
        let subject = lock(&self.writes.queue).failing_subject();
        let (what, it) = (subject.name(), if matches!(subject, Subject::Changes) { "them" } else { "it" });
        let code = match error {
            StoreError::Sqlite(error) => error.sqlite_error_code(),
            _ => None,
        };
        let message = if error.is_busy() {
            format!(
                "Couldn’t save {what}: another program is using the database. Rushes keeps trying and saves {it} when the database is free."
            )
        } else if code == Some(ErrorCode::DiskFull) {
            format!(
                "Couldn’t save {what}: the disk is full. Free up some space; Rushes keeps trying and saves {it} then."
            )
        } else if matches!(code, Some(ErrorCode::ReadOnly | ErrorCode::PermissionDenied | ErrorCode::CannotOpen)) {
            format!(
                "Couldn’t save {what}: Rushes can’t write to its data folder. Check that the folder is yours and not read-only; Rushes keeps trying."
            )
        } else if matches!(subject, Subject::Changes) {
            format!("Couldn’t save {what}. Rushes keeps trying; keep the app open until they’re saved.")
        } else {
            format!("Couldn’t save {what}. Rushes keeps trying; keep the app open until it shows as saved.")
        };
        self.report_error(message, cx);
    }

    /// Writes everything queued before the app exits, on the calling thread,
    /// waiting at most [`QUIT_SAVE_TIMEOUT`] in all for a lock. What can't be
    /// written by then is kept for the next start. Later writes during the
    /// quit come here too.
    pub(super) fn write_before_quit(&mut self) {
        let deadline = *self.writes.quit_deadline.get_or_insert_with(|| Instant::now() + QUIT_SAVE_TIMEOUT);
        // A pass in flight finishes first: it holds the writer's connection.
        self.writes.worker = None;
        let mut failure = None;
        while !self.writes.is_empty() {
            // Taking the connection waits for a pass in flight, within the deadline.
            let store = lock(&self.writes.store);
            if let Err(error) = store.set_busy_timeout(deadline.saturating_duration_since(Instant::now())) {
                log_line!("rushes: can't set the writer's busy timeout: {error}");
            }
            drop(store);
            let pass = write_queue(&self.writes.store, &self.writes.queue);
            log_refused(&pass);
            match pass.failed {
                Some(error) if error.is_busy() && Instant::now() < deadline => {}
                Some(error) => {
                    failure = Some(error);
                    break;
                }
                None if pass.written == 0 && pass.refused.is_empty() => break,
                None => {}
            }
        }
        if let Err(error) = lock(&self.writes.store).set_busy_timeout(WRITE_BUSY_TIMEOUT) {
            log_line!("rushes: can't set the writer's busy timeout: {error}");
        }
        if let Some(error) = failure
            && lock(&self.writes.queue).len() != self.writes.logged_kept
        {
            log_line!("rushes: couldn’t save before quitting: {error}");
        }
        self.writes.keep_unsaved();
    }
}

fn log_refused(pass: &Pass) {
    for (write, error) in &pass.refused {
        log_line!("rushes: {}: {error}", write.failure_title());
    }
}

/// The notice that changes kept at the last quit were restored: nothing is
/// lost, so it is no error.
fn recovered_problem(saved: bool) -> Problem {
    let (message, severity) = if saved {
        (
            "Rushes couldn’t save some changes before it last quit. It kept them, and they’re saved now.",
            Severity::Success,
        )
    } else {
        (
            "Rushes couldn’t save some changes before it last quit. It kept them and saves them as soon as the database is free.",
            Severity::Info,
        )
    };
    Problem::new("Changes from last time restored", message).with_severity(severity)
}

/// The writes kept at the last quit; `None` when there are none.
fn read_unsaved(file: &Path) -> Option<Result<Vec<Write>, Problem>> {
    let lost = || {
        Problem::new(
            "Some changes from last time couldn’t be restored",
            format!(
                "Rushes kept changes it couldn’t save before it last quit, but can’t read them. They’re in {} in the data folder.",
                UNSAVED_FILE.replace(".json", ".unreadable.json")
            ),
        )
    };
    let bytes = match fs::read(file) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return None,
        Err(error) => {
            log_line!("rushes: can't read {}: {error}", file.display());
            return Some(Err(lost()));
        }
    };
    match serde_json::from_slice(&bytes) {
        Ok(writes) => Some(Ok(writes)),
        Err(error) => {
            log_line!("rushes: can't read the unsaved changes in {}: {error}", file.display());
            // Set aside, so it isn't read again at every start.
            if let Err(error) = fs::rename(file, file.with_extension("unreadable.json")) {
                log_line!("rushes: can't set aside {}: {error}", file.display());
            }
            Some(Err(lost()))
        }
    }
}

/// Writes `value` as JSON to `file`, readable by its owner only (the settings
/// hold the OAuth client secret), replacing it whole.
fn write_private_json(file: &Path, value: &impl Serialize) -> io::Result<()> {
    let json = serde_json::to_vec(value).map_err(io::Error::other)?;
    let partial = file.with_extension("json.partial");
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut out = options.open(&partial)?;
    out.write_all(&json)?;
    out.sync_all()?;
    fs::rename(&partial, file)
}

#[cfg(test)]
mod tests {
    use rushes_core::store::blank_document;
    use rushes_core::{Block, PropertyKind, PropertyValue, SelectOption, TagColor};

    use super::*;

    fn row(video: Video) -> VideoRow {
        VideoRow {
            video,
            script_words: 0,
            script_seconds: 0,
            latest_run: None,
            has_finished_edit: false,
            latest_upload: None,
        }
    }

    fn select(options: &[&str]) -> PropertyDef {
        let options = options
            .iter()
            .map(|id| SelectOption { id: id.to_string(), name: id.to_string(), color: TagColor::Gray })
            .collect();
        PropertyDef {
            id: PropertyId::new(),
            name: "Priority".into(),
            kind: PropertyKind::Select { options },
            position: 0,
        }
    }

    fn titles(queue: &Queue) -> Vec<String> {
        queue
            .entries
            .iter()
            .map(|entry| match &entry.write {
                Write::Video(video) => video.title.clone(),
                Write::DeleteProperty(_) => "delete property".into(),
                Write::Document(document) => document.blocks[0].text.clone(),
                _ => "other".into(),
            })
            .collect()
    }

    #[test]
    fn later_values_replace_queued_ones_unless_an_ordered_write_came_between() {
        let video = Video::new("First");
        let mut document = blank_document(video.id, DocumentKind::Script, "Script", 0);
        let mut queue = Queue::default();
        queue.push(Write::Video(video.clone()));
        queue.push(Write::Document(document.clone()));
        queue.push(Write::Video(Video { title: "Second".into(), ..video.clone() }));
        document.blocks = vec![Block::paragraph("Hook")];
        queue.push(Write::Document(document.clone()));
        assert_eq!(titles(&queue), ["Second", "Hook"], "one entry per video and document, in place");

        // A property deleted after the edit must not be undone by it.
        queue.push(Write::DeleteProperty(PropertyId::new()));
        queue.push(Write::Video(Video { title: "Third".into(), ..video }));
        assert_eq!(titles(&queue), ["Second", "Hook", "delete property", "Third"]);
    }

    #[test]
    fn queued_property_changes_show_in_the_cached_data_as_stored() {
        let property = select(&["high", "low"]);
        let mut video = Video::new("Desk setup tour");
        video.properties.insert(property.id, PropertyValue::Select(Some("low".into())));
        let (mut rows, mut properties, mut views) = (vec![row(video.clone())], vec![property.clone()], Vec::new());
        let mut queue = Queue::default();
        // The option is removed: the value pointing at it goes, as in the database.
        queue.push(Write::UpdateProperty(PropertyDef { kind: select(&["high"]).kind, ..property.clone() }));
        queue.overlay(&mut Cached { rows: &mut rows, properties: &mut properties, views: &mut views });
        assert_eq!(rows[0].video.properties.get(&property.id), None);
        assert_eq!(properties[0].kind.options().len(), 1);

        // Replayed on fresh data, as after a reload.
        let mut rows = vec![row(video)];
        let mut properties = vec![property.clone()];
        queue.overlay(&mut Cached { rows: &mut rows, properties: &mut properties, views: &mut views });
        assert_eq!(rows[0].video.properties.get(&property.id), None);
    }

    #[test]
    fn queued_documents_and_videos_show_before_they_are_written() {
        let (video, documents) = rushes_core::NewVideo { title: "New".into(), ..Default::default() }.build(3.0);
        let mut queue = Queue::default();
        queue.push(Write::CreateVideo { video: video.clone(), documents: documents.clone() });
        let mut script = documents[0].clone();
        script.blocks = vec![Block::paragraph("one two three")];
        queue.push(Write::Document(script.clone()));

        let (mut rows, mut properties, mut views) = (Vec::new(), Vec::new(), Vec::new());
        queue.overlay(&mut Cached { rows: &mut rows, properties: &mut properties, views: &mut views });
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].script_words, 3);
        let mut stored = Vec::new();
        queue.overlay_documents(video.id, &mut stored);
        assert_eq!(stored, [script.clone(), documents[1].clone()]);
        assert_eq!(queue.document(script.id), Some(script));

        queue.push(Write::DeleteVideos(vec![video.id]));
        queue.overlay(&mut Cached { rows: &mut rows, properties: &mut properties, views: &mut views });
        assert!(rows.is_empty());
    }

    #[test]
    fn a_busy_database_keeps_the_queue_in_order_until_it_is_free() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rushes.db");
        let mut setup = Store::open(&path).unwrap();
        let (video, documents) = rushes_core::NewVideo { title: "Desk".into(), ..Default::default() }.build(0.0);
        setup.insert_video_with_documents(&video, &documents).unwrap();
        let writer = Store::open(&path).unwrap();
        writer.set_busy_timeout(Duration::from_millis(50)).unwrap();
        let writer: SharedStore = Arc::new(Mutex::new(writer));

        let queue = Mutex::new(Queue::default());
        let mut script = documents[0].clone();
        script.blocks = vec![Block::paragraph("Locked in")];
        lock(&queue).push(Write::Video(Video { title: "Desk setup".into(), ..video.clone() }));
        lock(&queue).push(Write::Document(script));

        let locker = rusqlite::Connection::open(&path).unwrap();
        locker.execute_batch("BEGIN EXCLUSIVE").unwrap();
        let pass = write_queue(&writer, &queue);
        assert!(pass.failed.as_ref().is_some_and(StoreError::is_busy));
        assert_eq!(pass.written, 0);
        assert!(lock(&queue).entries.iter().all(|entry| entry.failing), "both wait for a retry");
        assert_eq!(lock(&queue).failing_subject().name(), "your changes");

        locker.execute_batch("COMMIT").unwrap();
        let pass = write_queue(&writer, &queue);
        assert!(pass.failed.is_none());
        assert_eq!(pass.written, 2);
        assert!(lock(&queue).is_empty());
        assert_eq!(setup.get_video(video.id).unwrap().unwrap().title, "Desk setup");
        assert_eq!(setup.list_documents(video.id).unwrap()[0].blocks[0].text, "Locked in");
    }

    #[test]
    fn service_records_queue_in_order_and_show_in_what_is_read() {
        let video = Video::new("Desk setup tour");
        let now = chrono::Utc::now();
        let mut run = pipeline_run(video.id);
        let event = |seq: u32| Write::RunEvent {
            video_id: video.id,
            event: RunEvent { run_id: run.id, seq, at: now, payload: serde_json::json!(seq) },
        };
        let mut queue = Queue::default();
        queue.push(Write::InsertRun(run.clone()));
        queue.push(event(1));
        queue.push(Write::Run(run.clone()));
        queue.push(event(2));
        run.status = rushes_core::RunStatus::AwaitingApproval;
        queue.push(Write::Run(run.clone()));
        queue.push(Write::Video(video.clone()));
        queue.push(Write::Video(Video { title: "Desk setup".into(), ..video.clone() }));
        // Log entries stay, each once; a run's newer state and a video's newer
        // value take the place of the queued ones past them.
        assert_eq!(queue.len(), 5);
        assert!(matches!(&queue.entries[2].write, Write::Run(queued) if queued.status == run.status));
        assert!(matches!(&queue.entries[4].write, Write::Video(queued) if queued.title == "Desk setup"));

        let mut runs = Vec::new();
        queue.overlay_runs(&mut runs);
        assert_eq!(runs, vec![run.clone()]);
        // The first entry was written meanwhile: it is read, and not repeated.
        let mut events = vec![RunEvent { run_id: run.id, seq: 1, at: now, payload: serde_json::json!(1) }];
        queue.overlay_run_events(run.id, &mut events);
        assert_eq!(events.iter().map(|event| event.seq).collect::<Vec<_>>(), [1, 2]);

        queue.mark_failing(None);
        assert_eq!(queue.failing_subject().name(), "your changes");
        queue.entries.retain(|entry| entry.write.record().is_some());
        assert_eq!(queue.failing_subject().name(), "the AI edit’s progress");

        queue.push(Write::DeleteVideos(vec![video.id]));
        let mut runs = vec![run.clone()];
        queue.overlay_runs(&mut runs);
        assert!(runs.is_empty(), "deleted with the video");
        // A run gone with its video is refused without a word.
        let gone = StoreError::NotFound { entity: "pipeline run", id: run.id.to_string() };
        assert!(!Write::Run(run).reports_refusal(&gone));
        assert!(Write::Video(video).reports_refusal(&gone));
    }

    /// The database at `path` refuses every new row of `table` with an error
    /// other than a lock, as a full disk does, until the trigger is dropped.
    fn no_room(path: &Path, table: &str) -> rusqlite::Connection {
        let disk = rusqlite::Connection::open(path).unwrap();
        disk.execute_batch(&format!(
            "CREATE TRIGGER no_room BEFORE INSERT ON {table} BEGIN SELECT abs(-9223372036854775807 - 1); END;"
        ))
        .unwrap();
        disk
    }

    fn pipeline_run(video_id: VideoId) -> PipelineRun {
        let now = chrono::Utc::now();
        PipelineRun {
            id: RunId::new(),
            video_id,
            status: rushes_core::RunStatus::Planning,
            brief: "Cut it tight.".into(),
            footage_dir: "/media/desk/footage".into(),
            session_id: None,
            harness: None,
            summary: None,
            output: None,
            error: None,
            cost_usd: None,
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn a_failing_create_holds_back_the_later_writes_of_its_video_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rushes.db");
        let mut setup = Store::open(&path).unwrap();
        let other = Video::new("Studio tour");
        setup.insert_video(&other).unwrap();
        let writer: SharedStore = Arc::new(Mutex::new(Store::open(&path).unwrap()));
        let disk = no_room(&path, "videos");

        let (video, documents) =
            rushes_core::NewVideo { title: "Full disk test".into(), ..Default::default() }.build(1.0);
        let mut script = documents[0].clone();
        script.blocks = vec![Block::paragraph("Typed on a full disk")];
        let queue = Mutex::new(Queue::default());
        lock(&queue).push(Write::CreateVideo { video: video.clone(), documents });
        lock(&queue).push(Write::Video(Video { hook: "Hook typed on a full disk".into(), ..video.clone() }));
        lock(&queue).push(Write::Document(script));
        lock(&queue).push(Write::Video(Video { title: "Studio tour, part 2".into(), ..other.clone() }));

        let pass = write_queue(&writer, &queue);
        assert!(pass.refused.is_empty(), "nothing is taken for gone: {:?}", pass.refused);
        assert!(pass.failed.is_some());
        assert_eq!(pass.written, 1, "another video's change lands");
        assert_eq!(setup.get_video(other.id).unwrap().unwrap().title, "Studio tour, part 2");
        assert_eq!(lock(&queue).len(), 3);
        assert!(lock(&queue).entries.iter().all(|entry| entry.failing), "the page shows them as not saved");
        assert!(lock(&queue).is_video_failing(video.id));

        // The disk has room again: the video lands as the creator left it.
        disk.execute_batch("DROP TRIGGER no_room").unwrap();
        let pass = write_queue(&writer, &queue);
        assert!(pass.failed.is_none() && pass.refused.is_empty());
        assert_eq!(pass.written, 3);
        assert_eq!(setup.get_video(video.id).unwrap().unwrap().hook, "Hook typed on a full disk");
        assert_eq!(setup.list_documents(video.id).unwrap()[0].blocks[0].text, "Typed on a full disk");
    }

    #[test]
    fn a_failing_run_or_upload_insert_holds_back_its_later_states() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rushes.db");
        let mut setup = Store::open(&path).unwrap();
        let video = Video::new("Desk setup tour");
        setup.insert_video(&video).unwrap();
        let writer: SharedStore = Arc::new(Mutex::new(Store::open(&path).unwrap()));
        let disk = no_room(&path, "pipeline_runs");
        disk.execute_batch(
            "CREATE TRIGGER no_room_uploads BEFORE INSERT ON uploads BEGIN SELECT abs(-9223372036854775807 - 1); END;",
        )
        .unwrap();

        let mut run = pipeline_run(video.id);
        let now = chrono::Utc::now();
        let mut job = UploadJob {
            id: UploadId::new(),
            video_id: video.id,
            file: "/media/desk/footage/edit/final.mp4".into(),
            file_stamp: None,
            status: rushes_core::UploadStatus::Pending,
            session_url: None,
            channel: None,
            created_at: now,
            updated_at: now,
        };
        let queue = Mutex::new(Queue::default());
        lock(&queue).push(Write::InsertRun(run.clone()));
        lock(&queue).push(Write::InsertUpload(job.clone()));
        run.status = rushes_core::RunStatus::AwaitingApproval;
        lock(&queue).push(Write::Run(run.clone()));
        job.status = rushes_core::UploadStatus::Done { video_id: "7iSYEubHWqU".into() };
        lock(&queue).push(Write::Upload(job.clone()));
        lock(&queue).push(Write::Video(Video { title: "Desk setup".into(), ..video.clone() }));

        let pass = write_queue(&writer, &queue);
        assert!(pass.refused.is_empty(), "nothing is taken for gone: {:?}", pass.refused);
        assert_eq!(pass.written, 1, "the video's own change lands");
        assert_eq!(lock(&queue).len(), 4);

        disk.execute_batch("DROP TRIGGER no_room; DROP TRIGGER no_room_uploads").unwrap();
        let pass = write_queue(&writer, &queue);
        assert!(pass.failed.is_none() && pass.refused.is_empty());
        assert_eq!(pass.written, 4);
        assert_eq!(setup.get_run(run.id).unwrap(), Some(run));
        assert_eq!(setup.get_upload(job.id).unwrap(), Some(job));
    }

    #[test]
    fn a_failing_property_definition_holds_back_every_later_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rushes.db");
        let mut setup = Store::open(&path).unwrap();
        let property = select(&["high", "low"]);
        let mut video = Video::new("Desk setup tour");
        setup.insert_video(&video).unwrap();
        let writer: SharedStore = Arc::new(Mutex::new(Store::open(&path).unwrap()));
        let disk = no_room(&path, "property_defs");

        let queue = Mutex::new(Queue::default());
        lock(&queue).push(Write::InsertProperty(property.clone()));
        video.properties.insert(property.id, PropertyValue::Select(Some("high".into())));
        lock(&queue).push(Write::Video(video.clone()));
        let pass = write_queue(&writer, &queue);
        assert_eq!(pass.written, 0);
        assert_eq!(lock(&queue).len(), 2);
        assert!(lock(&queue).entries.iter().all(|entry| entry.failing));

        disk.execute_batch("DROP TRIGGER no_room").unwrap();
        assert_eq!(write_queue(&writer, &queue).written, 2);
        assert_eq!(setup.list_properties().unwrap(), [property]);
        assert_eq!(setup.get_video(video.id).unwrap().unwrap().properties, video.properties);
    }

    #[test]
    fn writes_that_can_never_succeed_leave_the_queue() {
        let store: SharedStore = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
        let queue = Mutex::new(Queue::default());
        lock(&queue).push(Write::Video(Video::new("Deleted elsewhere")));
        let pass = write_queue(&store, &queue);
        assert_eq!(pass.refused.len(), 1);
        assert!(lock(&queue).is_empty());
    }

    #[test]
    fn restored_changes_read_as_a_notice_not_an_error() {
        assert_eq!(recovered_problem(true).severity(), Severity::Success);
        assert_eq!(recovered_problem(false).severity(), Severity::Info);
        let gone = StoreError::NotFound { entity: "video", id: "1".into() };
        assert_eq!(Problem::store("Couldn’t save your changes", &gone).severity(), Severity::Error);
    }

    #[test]
    fn unsaved_writes_round_trip_through_their_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(UNSAVED_FILE);
        let video = Video::new("Kept");
        let event = RunEvent { run_id: RunId::new(), seq: 1, at: chrono::Utc::now(), payload: serde_json::json!({}) };
        let writes = vec![
            Write::Video(video.clone()),
            Write::Settings(AppSettings::default()),
            Write::SeedSample,
            Write::RunEvent { video_id: video.id, event },
            Write::Setting { key: "ai_edit.brief_drafts".into(), value: serde_json::json!([]) },
        ];
        write_private_json(&file, &writes).unwrap();
        assert_eq!(read_unsaved(&file).unwrap().unwrap(), writes);

        fs::write(&file, "not json").unwrap();
        assert!(read_unsaved(&file).unwrap().is_err());
        assert!(!file.exists(), "set aside, not read at every start");
        assert!(dir.path().join("unsaved-changes.unreadable.json").exists());
        assert!(read_unsaved(&file).is_none());
    }
}
