//! SQLite persistence.
//!
//! Queryable scalars (status, position, timestamps, foreign keys) are real
//! columns; nested values (tags, publish settings, YouTube link, property
//! values, view configuration, document blocks, upload status, settings) are
//! JSON text. Child rows (documents, runs, run events, uploads) are deleted
//! with their video through `ON DELETE CASCADE`.
//!
//! Writes take `&mut self`, reads `&self`. `Store` is `Send`; the app keeps it
//! behind a mutex and calls it from background tasks. A file database can be
//! opened by several connections at once ([`Store::reopen`]): WAL lets one
//! read while another writes or waits for another program's lock.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use chrono::{DateTime, NaiveDate, Utc};
use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSqlOutput, ValueRef};
use rusqlite::{Connection, ErrorCode, OptionalExtension, Row, ToSql, Transaction, TransactionBehavior, params};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::document::{self, DEFAULT_WPM};
use crate::model::{
    Block, BlockId, BuiltinField, Column, ColumnLayout, DatabaseView, Document, DocumentId, DocumentKind, FilterSpec,
    PipelineRun, PropertyDef, PropertyId, PropertyKind, PropertyValue, RunId, RunStatus, SortSpec, UploadId, UploadJob,
    UploadStatus, Video, VideoFormat, VideoId, VideoRow, VideoStatus, ViewId, ViewKind,
};
use crate::sample;
use crate::settings::{AppSettings, EditHarness};
use crate::templates::{TemplateKind, script_template};

/// Settings key holding [`AppSettings`].
const APP_SETTINGS_KEY: &str = "app";
/// How long a file database waits for another connection's lock.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("could not create database folder {path}: {source}")]
    CreateDir { path: PathBuf, source: std::io::Error },
    #[error("could not create database file {path}: {source}")]
    CreateFile { path: PathBuf, source: std::io::Error },
    #[error("database schema version {found} is newer than this version of Rushes supports ({supported})")]
    UnsupportedSchema { found: i64, supported: i64 },
    #[error("{entity} {id} not found")]
    NotFound { entity: &'static str, id: String },
}

impl StoreError {
    /// Whether another connection holds the lock the operation needed, so it
    /// may succeed when tried again later.
    pub fn is_busy(&self) -> bool {
        matches!(
            self,
            StoreError::Sqlite(error)
                if matches!(error.sqlite_error_code(), Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked))
        )
    }
}

pub type StoreResult<T> = Result<T, StoreError>;

fn not_found(entity: &'static str, id: impl ToString) -> StoreError {
    StoreError::NotFound { entity, id: id.to_string() }
}

/// Input of [`Store::create_video`] (the New-video wizard).
#[derive(Clone, Debug, PartialEq, Default)]
pub struct NewVideo {
    pub title: String,
    pub format: VideoFormat,
    pub hook: String,
    pub publish_date: Option<NaiveDate>,
    pub template: TemplateKind,
}

impl NewVideo {
    /// The video at `position` with a Script document from the chosen template
    /// and an empty Notes document, not stored yet. The template's icon becomes
    /// the video icon.
    pub fn build(self, position: f64) -> (Video, Vec<Document>) {
        let video = Video {
            icon: self.template.icon().map(str::to_string),
            format: self.format,
            hook: self.hook,
            publish_date: self.publish_date,
            position,
            ..Video::new(self.title)
        };
        let now = video.created_at;
        let documents = vec![
            Document {
                id: DocumentId::new(),
                video_id: video.id,
                kind: DocumentKind::Script,
                title: "Script".to_string(),
                blocks: script_template(self.template, &video),
                position: 0,
                updated_at: now,
            },
            Document {
                id: DocumentId::new(),
                video_id: video.id,
                kind: DocumentKind::Notes,
                title: "Notes".to_string(),
                blocks: vec![Block::paragraph("")],
                position: 1,
                updated_at: now,
            },
        ];
        (video, documents)
    }
}

/// A copy of `original` and its `documents` placed right below it, before the
/// video at `next_position` if there is one: title gets " (copy)", documents
/// get fresh document and block ids. Things tied to the original's footage and
/// YouTube upload are not copied (media folder, YouTube link, upload file,
/// schedule), and a published or scheduled original yields an `Idea`.
pub fn copy_of(original: &Video, documents: Vec<Document>, next_position: Option<f64>) -> (Video, Vec<Document>) {
    let now = Utc::now();
    let mut copy = Video {
        id: VideoId::new(),
        title: format!("{} (copy)", original.title),
        folder: None,
        youtube: None,
        position: next_position.map_or(original.position + 1.0, |next| (original.position + next) / 2.0),
        created_at: now,
        updated_at: now,
        ..original.clone()
    };
    copy.publish.file = None;
    copy.publish.publish_at = None;
    if matches!(copy.status, VideoStatus::Published | VideoStatus::Scheduled) {
        copy.status = VideoStatus::Idea;
    }
    let documents = documents
        .into_iter()
        .map(|document| {
            let blocks = document.blocks.into_iter().map(|block| Block { id: BlockId::new(), ..block }).collect();
            Document { id: DocumentId::new(), video_id: copy.id, blocks, updated_at: now, ..document }
        })
        .collect();
    (copy, documents)
}

/// An empty document of a video at `position`, not stored yet.
pub fn blank_document(video_id: VideoId, kind: DocumentKind, title: &str, position: i32) -> Document {
    Document {
        id: DocumentId::new(),
        video_id,
        kind,
        title: title.to_string(),
        blocks: vec![Block::paragraph("")],
        position,
        updated_at: Utc::now(),
    }
}

/// Keeps a video's value of `property` consistent with its changed definition,
/// as [`Store::update_property`] does for stored videos: a new kind drops the
/// value, removed options drop the values that pointed at them.
pub fn reconcile_property_value(
    old: &PropertyKind,
    property: &PropertyDef,
    values: &mut BTreeMap<PropertyId, PropertyValue>,
) {
    if std::mem::discriminant(old) != std::mem::discriminant(&property.kind) {
        values.remove(&property.id);
    } else if options_removed(old, &property.kind) {
        prune_select_value(property, values);
    }
}

/// Removes a deleted property's columns and sorts from `view`, as
/// [`Store::delete_property`] does for stored views. Returns whether it changed.
pub fn forget_property(view: &mut DatabaseView, id: PropertyId) -> bool {
    let before = (view.columns.len(), view.sorts.len());
    view.columns.retain(|layout| layout.column != Column::Custom(id));
    view.sorts.retain(|sort| sort.column != Column::Custom(id));
    (view.columns.len(), view.sorts.len()) != before
}

fn options_removed(old: &PropertyKind, new: &PropertyKind) -> bool {
    old.options().iter().any(|option| !new.options().iter().any(|o| o.id == option.id))
}

/// Drops a select / multi-select value whose option no longer exists.
fn prune_select_value(property: &PropertyDef, values: &mut BTreeMap<PropertyId, PropertyValue>) {
    let exists = |option: &String| property.kind.options().iter().any(|o| &o.id == option);
    let keep = match values.get_mut(&property.id) {
        Some(PropertyValue::Select(Some(option))) => exists(option),
        Some(PropertyValue::MultiSelect(options)) => {
            options.retain(exists);
            !options.is_empty()
        }
        _ => true,
    };
    if !keep {
        values.remove(&property.id);
    }
}

/// One entry of a pipeline run's append-only event log (agent messages, tool
/// calls, status changes) as produced by the pipeline.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RunEvent {
    pub run_id: RunId,
    /// 1-based, increasing per run.
    pub seq: u32,
    pub at: DateTime<Utc>,
    pub payload: serde_json::Value,
}

#[derive(Debug)]
pub struct Store {
    conn: Connection,
}

impl Store {
    /// Opens (creating if needed) the database file and its folder, enables WAL
    /// and foreign keys, and runs pending migrations. A new database file is
    /// readable by its owner only on Unix.
    pub fn open(path: impl AsRef<Path>) -> StoreResult<Store> {
        let path = path.as_ref();
        if let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
            fs::create_dir_all(parent)
                .map_err(|source| StoreError::CreateDir { path: parent.to_path_buf(), source })?;
        }
        create_private_file(path).map_err(|source| StoreError::CreateFile { path: path.to_path_buf(), source })?;
        let conn = Connection::open(path)?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        enable_wal(&conn)?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        Self::init(conn)
    }

    /// A private in-memory database (tests, previews).
    pub fn open_in_memory() -> StoreResult<Store> {
        Self::init(Connection::open_in_memory()?)
    }

    /// The database file, or `None` for an in-memory database.
    pub fn path(&self) -> Option<PathBuf> {
        self.conn.path().filter(|path| !path.is_empty()).map(PathBuf::from)
    }

    /// Another connection to the same database file, set up as [`Store::open`]
    /// does. `None` for an in-memory database, which no other connection sees.
    pub fn reopen(&self) -> StoreResult<Option<Store>> {
        self.path().map(Store::open).transpose()
    }

    /// How long this connection waits for a lock held by another connection
    /// before an operation fails as busy ([`StoreError::is_busy`]). Five seconds
    /// after [`Store::open`].
    pub fn set_busy_timeout(&self, timeout: Duration) -> StoreResult<()> {
        Ok(self.conn.busy_timeout(timeout)?)
    }

    fn init(mut conn: Connection) -> StoreResult<Store> {
        conn.pragma_update(None, "foreign_keys", true)?;
        migrate(&mut conn)?;
        Ok(Store { conn })
    }

    /// Current `PRAGMA user_version`.
    pub fn schema_version(&self) -> StoreResult<i64> {
        Ok(self.conn.pragma_query_value(None, "user_version", |row| row.get(0))?)
    }

    // --- videos -----------------------------------------------------------

    /// All videos in manual order.
    pub fn list_videos(&self) -> StoreResult<Vec<Video>> {
        let sql = format!("SELECT {VIDEO_COLUMNS} FROM videos ORDER BY position, created_at");
        let mut stmt = self.conn.prepare_cached(&sql)?;
        let videos = stmt.query_map([], video_from_row)?.collect::<rusqlite::Result<_>>()?;
        Ok(videos)
    }

    /// All videos in manual order with script stats (from the first Script
    /// document), the latest run / upload status and whether an edit ever
    /// finished, in one query.
    pub fn list_video_rows(&self) -> StoreResult<Vec<VideoRow>> {
        let sql = format!(
            "SELECT {VIDEO_COLUMNS},
                (SELECT d.spoken_words FROM documents d WHERE d.video_id = videos.id AND d.kind = ?1
                 ORDER BY d.position, d.id LIMIT 1) AS script_words,
                (SELECT r.status FROM pipeline_runs r WHERE r.video_id = videos.id
                 ORDER BY r.created_at DESC, r.id DESC LIMIT 1) AS latest_run,
                EXISTS (SELECT 1 FROM pipeline_runs r WHERE r.video_id = videos.id AND r.status = ?2)
                    AS has_finished_edit,
                (SELECT u.status FROM uploads u WHERE u.video_id = videos.id
                 ORDER BY u.created_at DESC, u.id DESC LIMIT 1) AS latest_upload
             FROM videos ORDER BY position, created_at"
        );
        let mut stmt = self.conn.prepare_cached(&sql)?;
        let rows = stmt
            .query_map(params![DocumentKind::Script, RunStatus::Succeeded], |row| {
                let script_words = row.get::<_, Option<i64>>("script_words")?.unwrap_or(0);
                let script_words = usize::try_from(script_words).unwrap_or(0);
                Ok(VideoRow {
                    video: video_from_row(row)?,
                    script_words,
                    script_seconds: document::runtime_secs_for_words(script_words, DEFAULT_WPM),
                    latest_run: row.get("latest_run")?,
                    has_finished_edit: row.get("has_finished_edit")?,
                    latest_upload: row.get::<_, Option<Json<UploadStatus>>>("latest_upload")?.map(Json::into_inner),
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    pub fn get_video(&self, id: VideoId) -> StoreResult<Option<Video>> {
        Ok(query_video(&self.conn, id)?)
    }

    pub fn insert_video(&mut self, video: &Video) -> StoreResult<()> {
        write_video(&self.conn, INSERT_VIDEO, video)?;
        Ok(())
    }

    /// Saves all fields except `created_at` and sets `updated_at` to now (also on `video`).
    pub fn update_video(&mut self, video: &mut Video) -> StoreResult<()> {
        video.updated_at = Utc::now();
        if write_video(&self.conn, UPDATE_VIDEO, video)? == 0 {
            return Err(not_found("video", video.id));
        }
        Ok(())
    }

    /// Deletes the video with its documents, runs and uploads. Its media folder
    /// on disk is left alone. Deleting a missing video is not an error.
    pub fn delete_video(&mut self, id: VideoId) -> StoreResult<()> {
        self.conn.execute("DELETE FROM videos WHERE id = ?1", [id])?;
        Ok(())
    }

    /// Copies a video right below the original, see [`copy_of`]. Runs and
    /// uploads are not copied.
    pub fn duplicate_video(&mut self, id: VideoId) -> StoreResult<Video> {
        let tx = self.conn.transaction()?;
        let original = query_video(&tx, id)?.ok_or_else(|| not_found("video", id))?;
        let next_position: Option<f64> =
            tx.query_row("SELECT MIN(position) FROM videos WHERE position > ?1", [original.position], |row| {
                row.get(0)
            })?;
        let (copy, documents) = copy_of(&original, query_documents(&tx, id)?, next_position);
        write_video(&tx, INSERT_VIDEO, &copy)?;
        for document in &documents {
            upsert_document(&tx, document)?;
        }
        tx.commit()?;
        Ok(copy)
    }

    /// Position that places a new video after all existing ones.
    pub fn next_position(&self) -> StoreResult<f64> {
        Ok(next_video_position(&self.conn)?)
    }

    /// Creates a video at the end of the list with a Script document from the
    /// chosen template and an empty Notes document, see [`NewVideo::build`].
    pub fn create_video(&mut self, new: NewVideo) -> StoreResult<(Video, Vec<Document>)> {
        let tx = self.conn.transaction()?;
        let (video, documents) = new.build(next_video_position(&tx)?);
        write_video(&tx, INSERT_VIDEO, &video)?;
        for document in &documents {
            upsert_document(&tx, document)?;
        }
        tx.commit()?;
        Ok((video, documents))
    }

    /// Inserts a video built elsewhere ([`NewVideo::build`], [`copy_of`]) with
    /// its documents, all or nothing.
    pub fn insert_video_with_documents(&mut self, video: &Video, documents: &[Document]) -> StoreResult<()> {
        let tx = self.conn.transaction()?;
        write_video(&tx, INSERT_VIDEO, video)?;
        for document in documents {
            upsert_document(&tx, document)?;
        }
        tx.commit()?;
        Ok(())
    }

    // --- properties -------------------------------------------------------

    pub fn list_properties(&self) -> StoreResult<Vec<PropertyDef>> {
        let mut stmt =
            self.conn.prepare_cached("SELECT id, name, kind, position FROM property_defs ORDER BY position, id")?;
        let properties = stmt.query_map([], property_from_row)?.collect::<rusqlite::Result<_>>()?;
        Ok(properties)
    }

    pub fn insert_property(&mut self, property: &PropertyDef) -> StoreResult<()> {
        insert_property(&self.conn, property)?;
        Ok(())
    }

    /// Saves the definition and keeps video values consistent with it: changing
    /// the kind clears all values, removing select options drops values that
    /// pointed at them.
    pub fn update_property(&mut self, property: &PropertyDef) -> StoreResult<()> {
        let tx = self.conn.transaction()?;
        let old: PropertyKind = tx
            .query_row("SELECT kind FROM property_defs WHERE id = ?1", [property.id], |row| row.get::<_, Json<_>>(0))
            .optional()?
            .ok_or_else(|| not_found("property", property.id))?
            .into_inner();
        tx.execute(
            "UPDATE property_defs SET name = ?2, kind = ?3, position = ?4 WHERE id = ?1",
            params![property.id, property.name, Json(&property.kind), property.position],
        )?;
        if std::mem::discriminant(&old) != std::mem::discriminant(&property.kind) {
            remove_property_values(&tx, property.id)?;
        } else if options_removed(&old, &property.kind) {
            prune_select_values(&tx, property)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Deletes the property, its values on every video and its columns/sorts in views.
    pub fn delete_property(&mut self, id: PropertyId) -> StoreResult<()> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM property_defs WHERE id = ?1", [id])?;
        remove_property_values(&tx, id)?;
        for mut view in query_views(&tx)? {
            if forget_property(&mut view, id) {
                upsert_view(&tx, &view)?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    // --- documents --------------------------------------------------------

    /// Documents of a video ordered by position.
    pub fn list_documents(&self, video_id: VideoId) -> StoreResult<Vec<Document>> {
        Ok(query_documents(&self.conn, video_id)?)
    }

    pub fn get_document(&self, id: DocumentId) -> StoreResult<Option<Document>> {
        let sql = format!("SELECT {DOCUMENT_COLUMNS} FROM documents WHERE id = ?1");
        Ok(self.conn.prepare_cached(&sql)?.query_row([id], document_from_row).optional()?)
    }

    /// Inserts or replaces the document and sets `updated_at` to now (also on `document`).
    pub fn save_document(&mut self, document: &mut Document) -> StoreResult<()> {
        document.updated_at = Utc::now();
        upsert_document(&self.conn, document)?;
        Ok(())
    }

    pub fn delete_document(&mut self, id: DocumentId) -> StoreResult<()> {
        self.conn.execute("DELETE FROM documents WHERE id = ?1", [id])?;
        Ok(())
    }

    /// Adds an empty document after the video's existing ones.
    pub fn create_document(&mut self, video_id: VideoId, kind: DocumentKind, title: &str) -> StoreResult<Document> {
        let position = self.conn.query_row(
            "SELECT COALESCE(MAX(position) + 1, 0) FROM documents WHERE video_id = ?1",
            [video_id],
            |row| row.get(0),
        )?;
        let document = blank_document(video_id, kind, title, position);
        upsert_document(&self.conn, &document)?;
        Ok(document)
    }

    // --- views ------------------------------------------------------------

    pub fn list_views(&self) -> StoreResult<Vec<DatabaseView>> {
        Ok(query_views(&self.conn)?)
    }

    /// Inserts or replaces the view.
    pub fn save_view(&mut self, view: &DatabaseView) -> StoreResult<()> {
        upsert_view(&self.conn, view)?;
        Ok(())
    }

    pub fn delete_view(&mut self, id: ViewId) -> StoreResult<()> {
        self.conn.execute("DELETE FROM database_views WHERE id = ?1", [id])?;
        Ok(())
    }

    // --- pipeline runs ----------------------------------------------------

    pub fn insert_run(&mut self, run: &PipelineRun) -> StoreResult<()> {
        write_run(&self.conn, INSERT_RUN, run)?;
        Ok(())
    }

    /// Saves all fields except `video_id`/`created_at` and sets `updated_at` to now (also on `run`).
    pub fn update_run(&mut self, run: &mut PipelineRun) -> StoreResult<()> {
        run.updated_at = Utc::now();
        self.replace_run(run)
    }

    /// Saves all fields except `video_id`/`created_at` as `run` has them,
    /// `updated_at` included: for a caller that keeps the run and writes it later.
    pub fn replace_run(&mut self, run: &PipelineRun) -> StoreResult<()> {
        if write_run(&self.conn, UPDATE_RUN, run)? == 0 {
            return Err(not_found("pipeline run", run.id));
        }
        Ok(())
    }

    pub fn get_run(&self, id: RunId) -> StoreResult<Option<PipelineRun>> {
        let sql = format!("SELECT {RUN_COLUMNS} FROM pipeline_runs WHERE id = ?1");
        Ok(self.conn.prepare_cached(&sql)?.query_row([id], run_from_row).optional()?)
    }

    /// Runs of one video, or of all videos, newest first.
    pub fn list_runs(&self, video_id: Option<VideoId>) -> StoreResult<Vec<PipelineRun>> {
        let sql = format!(
            "SELECT {RUN_COLUMNS} FROM pipeline_runs WHERE ?1 IS NULL OR video_id = ?1 \
             ORDER BY created_at DESC, id DESC"
        );
        let mut stmt = self.conn.prepare_cached(&sql)?;
        let runs = stmt.query_map([video_id], run_from_row)?.collect::<rusqlite::Result<_>>()?;
        Ok(runs)
    }

    /// Appends an event with the next sequence number and the current time.
    pub fn append_run_event(&mut self, run_id: RunId, payload: &serde_json::Value) -> StoreResult<RunEvent> {
        let at = Utc::now();
        let seq = self
            .conn
            .prepare_cached(
                "INSERT INTO run_events (run_id, seq, at, payload)
                 VALUES (?1, (SELECT COALESCE(MAX(seq), 0) + 1 FROM run_events WHERE run_id = ?1), ?2, ?3)
                 RETURNING seq",
            )?
            .query_row(params![run_id, at, Json(payload)], |row| row.get(0))?;
        Ok(RunEvent { run_id, seq, at, payload: payload.clone() })
    }

    /// Adds an event numbered by the caller, which keeps the run's log and
    /// writes it later. A number the run already has is refused.
    pub fn insert_run_event(&mut self, event: &RunEvent) -> StoreResult<()> {
        self.conn
            .prepare_cached("INSERT INTO run_events (run_id, seq, at, payload) VALUES (?1, ?2, ?3, ?4)")?
            .execute(params![event.run_id, event.seq, event.at, Json(&event.payload)])?;
        Ok(())
    }

    /// Events of a run in append order.
    pub fn list_run_events(&self, run_id: RunId) -> StoreResult<Vec<RunEvent>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT run_id, seq, at, payload FROM run_events WHERE run_id = ?1 ORDER BY seq")?;
        let events = stmt
            .query_map([run_id], |row| {
                Ok(RunEvent {
                    run_id: row.get(0)?,
                    seq: row.get(1)?,
                    at: row.get(2)?,
                    payload: row.get::<_, Json<_>>(3)?.into_inner(),
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(events)
    }

    // --- uploads ----------------------------------------------------------

    pub fn insert_upload(&mut self, upload: &UploadJob) -> StoreResult<()> {
        write_upload(&self.conn, INSERT_UPLOAD, upload)?;
        Ok(())
    }

    /// Saves all fields except `video_id`/`created_at` and sets `updated_at` to now (also on `upload`).
    pub fn update_upload(&mut self, upload: &mut UploadJob) -> StoreResult<()> {
        upload.updated_at = Utc::now();
        self.replace_upload(upload)
    }

    /// Saves all fields except `video_id`/`created_at` as `upload` has them,
    /// `updated_at` included: for a caller that keeps the job and writes it later.
    pub fn replace_upload(&mut self, upload: &UploadJob) -> StoreResult<()> {
        if write_upload(&self.conn, UPDATE_UPLOAD, upload)? == 0 {
            return Err(not_found("upload", upload.id));
        }
        Ok(())
    }

    pub fn get_upload(&self, id: UploadId) -> StoreResult<Option<UploadJob>> {
        let sql = format!("SELECT {UPLOAD_COLUMNS} FROM uploads WHERE id = ?1");
        Ok(self.conn.prepare_cached(&sql)?.query_row([id], upload_from_row).optional()?)
    }

    /// Uploads of one video, or of all videos, newest first.
    pub fn list_uploads(&self, video_id: Option<VideoId>) -> StoreResult<Vec<UploadJob>> {
        let sql = format!(
            "SELECT {UPLOAD_COLUMNS} FROM uploads WHERE ?1 IS NULL OR video_id = ?1 ORDER BY created_at DESC, id DESC"
        );
        let mut stmt = self.conn.prepare_cached(&sql)?;
        let uploads = stmt.query_map([video_id], upload_from_row)?.collect::<rusqlite::Result<_>>()?;
        Ok(uploads)
    }

    // --- settings ---------------------------------------------------------

    pub fn get_setting<T: DeserializeOwned>(&self, key: &str) -> StoreResult<Option<T>> {
        let value = self
            .conn
            .prepare_cached("SELECT value FROM settings WHERE key = ?1")?
            .query_row([key], |row| row.get::<_, Json<T>>(0))
            .optional()?;
        Ok(value.map(Json::into_inner))
    }

    pub fn set_setting<T: Serialize + ?Sized>(&mut self, key: &str, value: &T) -> StoreResult<()> {
        self.conn.prepare_cached(
            "INSERT INTO settings (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        )?
        .execute(params![key, Json(value)])?;
        Ok(())
    }

    /// Saved settings, or defaults when none were saved yet.
    pub fn load_settings(&self) -> StoreResult<AppSettings> {
        Ok(self.get_setting(APP_SETTINGS_KEY)?.unwrap_or_default())
    }

    pub fn save_settings(&mut self, settings: &AppSettings) -> StoreResult<()> {
        self.set_setting(APP_SETTINGS_KEY, settings)
    }

    // --- sample data ------------------------------------------------------

    /// Adds a sample channel (videos in every stage with scripts, plus "Priority"
    /// and "Sponsor" select properties shown in table views) after the existing
    /// videos. Returns the inserted videos in order.
    pub fn seed_sample_workspace(&mut self) -> StoreResult<Vec<Video>> {
        let workspace = sample::sample_workspace();
        let tx = self.conn.transaction()?;
        let offset = next_video_position(&tx)?;
        let property_offset: i32 =
            tx.query_row("SELECT COALESCE(MAX(position) + 1, 0) FROM property_defs", [], |row| row.get(0))?;

        for property in &workspace.properties {
            insert_property(&tx, &PropertyDef { position: property.position + property_offset, ..property.clone() })?;
        }
        for mut view in query_views(&tx)?.into_iter().filter(|view| view.kind == ViewKind::Table) {
            view.columns.extend(workspace.properties.iter().map(|property| ColumnLayout {
                column: Column::Custom(property.id),
                width: 150.0,
                visible: true,
            }));
            upsert_view(&tx, &view)?;
        }
        let mut videos = Vec::with_capacity(workspace.videos.len());
        for (mut video, documents) in workspace.videos {
            video.position += offset;
            write_video(&tx, INSERT_VIDEO, &video)?;
            for document in &documents {
                upsert_document(&tx, document)?;
            }
            videos.push(video);
        }
        tx.commit()?;
        Ok(videos)
    }
}

/// Creates an empty database file with mode 0600 unless it exists. The
/// database holds the OAuth client secret and resumable upload URLs, and
/// SQLite gives the `-wal` / `-shm` files the database file's mode.
#[cfg(unix)]
fn create_private_file(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    match fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path) {
        Err(err) if err.kind() != std::io::ErrorKind::AlreadyExists => Err(err),
        _ => Ok(()),
    }
}

#[cfg(not(unix))]
fn create_private_file(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Switches to WAL. Converting a new database needs an exclusive lock and
/// SQLite reports `SQLITE_BUSY` without waiting when another connection is
/// converting it at the same time, so this retries until [`BUSY_TIMEOUT`].
fn enable_wal(conn: &Connection) -> rusqlite::Result<()> {
    let deadline = Instant::now() + BUSY_TIMEOUT;
    loop {
        match conn.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get::<_, String>(0)) {
            Err(err) if err.sqlite_error_code() == Some(ErrorCode::DatabaseBusy) && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5));
            }
            result => return result.map(drop),
        }
    }
}

// ---------------------------------------------------------------------------
// Schema
// ---------------------------------------------------------------------------

type Migration = fn(&Transaction<'_>) -> rusqlite::Result<()>;

/// Applied in order; after `MIGRATIONS[i]` the database is at `user_version = i + 1`.
/// Append new migrations, never edit released ones.
const MIGRATIONS: &[Migration] = &[migration_1, migration_2, migration_3, migration_4];

fn migrate(conn: &mut Connection) -> StoreResult<()> {
    while pending_migration(conn)?.is_some() {
        // IMMEDIATE takes the write lock before the version is read again, so
        // two processes opening a new database at once apply each migration once.
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(index) = pending_migration(&tx)? {
            MIGRATIONS[index](&tx)?;
            tx.pragma_update(None, "user_version", index as i64 + 1)?;
        }
        tx.commit()?;
    }
    Ok(())
}

/// Index of the next migration to apply, or `None` when the schema is current.
fn pending_migration(conn: &Connection) -> StoreResult<Option<usize>> {
    let current: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    let applied = usize::try_from(current)
        .ok()
        .filter(|&applied| applied <= MIGRATIONS.len())
        .ok_or(StoreError::UnsupportedSchema { found: current, supported: MIGRATIONS.len() as i64 })?;
    Ok((applied < MIGRATIONS.len()).then_some(applied))
}

/// Initial schema and the default views.
fn migration_1(tx: &Transaction<'_>) -> rusqlite::Result<()> {
    tx.execute_batch(
        "CREATE TABLE videos (
            id           TEXT PRIMARY KEY NOT NULL,
            title        TEXT NOT NULL,
            icon         TEXT,
            status       TEXT NOT NULL,
            format       TEXT NOT NULL,
            publish_date TEXT,
            tags         TEXT NOT NULL,  -- JSON array of strings
            hook         TEXT NOT NULL,
            folder       TEXT,
            publish      TEXT NOT NULL,  -- JSON PublishSettings
            youtube      TEXT,           -- JSON YoutubeLink
            properties   TEXT NOT NULL,  -- JSON object: property id -> PropertyValue
            position     REAL NOT NULL,
            created_at   TEXT NOT NULL,
            updated_at   TEXT NOT NULL
        );
        CREATE INDEX videos_position ON videos (position);

        CREATE TABLE property_defs (
            id       TEXT PRIMARY KEY NOT NULL,
            name     TEXT NOT NULL,
            kind     TEXT NOT NULL,      -- JSON PropertyKind
            position INTEGER NOT NULL
        );

        CREATE TABLE documents (
            id           TEXT PRIMARY KEY NOT NULL,
            video_id     TEXT NOT NULL REFERENCES videos (id) ON DELETE CASCADE,
            kind         TEXT NOT NULL,
            title        TEXT NOT NULL,
            blocks       TEXT NOT NULL,  -- JSON array of Block
            spoken_words INTEGER NOT NULL,  -- derived from blocks on save
            position     INTEGER NOT NULL,
            updated_at   TEXT NOT NULL
        );
        CREATE INDEX documents_video ON documents (video_id, position);

        CREATE TABLE database_views (
            id       TEXT PRIMARY KEY NOT NULL,
            name     TEXT NOT NULL,
            kind     TEXT NOT NULL,
            config   TEXT NOT NULL,      -- JSON {columns, sorts, filter}
            position INTEGER NOT NULL
        );

        CREATE TABLE pipeline_runs (
            id          TEXT PRIMARY KEY NOT NULL,
            video_id    TEXT NOT NULL REFERENCES videos (id) ON DELETE CASCADE,
            status      TEXT NOT NULL,
            brief       TEXT NOT NULL,
            footage_dir TEXT NOT NULL,
            session_id  TEXT,
            summary     TEXT,
            output      TEXT,
            error       TEXT,
            cost_usd    REAL,
            created_at  TEXT NOT NULL,
            updated_at  TEXT NOT NULL
        );
        CREATE INDEX pipeline_runs_video ON pipeline_runs (video_id, created_at);

        CREATE TABLE run_events (
            run_id  TEXT NOT NULL REFERENCES pipeline_runs (id) ON DELETE CASCADE,
            seq     INTEGER NOT NULL,
            at      TEXT NOT NULL,
            payload TEXT NOT NULL,       -- JSON
            PRIMARY KEY (run_id, seq)
        ) WITHOUT ROWID;

        CREATE TABLE uploads (
            id          TEXT PRIMARY KEY NOT NULL,
            video_id    TEXT NOT NULL REFERENCES videos (id) ON DELETE CASCADE,
            file        TEXT NOT NULL,
            status      TEXT NOT NULL,   -- JSON UploadStatus
            state       TEXT GENERATED ALWAYS AS (json_extract(status, '$.state')) VIRTUAL,  -- status tag, for queries
            session_url TEXT,
            created_at  TEXT NOT NULL,
            updated_at  TEXT NOT NULL
        );
        CREATE INDEX uploads_video ON uploads (video_id, created_at);

        CREATE TABLE settings (
            key   TEXT PRIMARY KEY NOT NULL,
            value TEXT NOT NULL          -- JSON
        );",
    )?;
    for view in default_views() {
        upsert_view(tx, &view)?;
    }
    Ok(())
}

/// Upload jobs remember which version of the file they send (`UploadJob::file_stamp`).
fn migration_2(tx: &Transaction<'_>) -> rusqlite::Result<()> {
    tx.execute_batch("ALTER TABLE uploads ADD COLUMN file_stamp TEXT;  -- JSON FileStamp")
}

/// Upload jobs remember the YouTube account they go to (`UploadJob::channel`).
fn migration_3(tx: &Transaction<'_>) -> rusqlite::Result<()> {
    tx.execute_batch("ALTER TABLE uploads ADD COLUMN channel TEXT;")
}

/// Edit runs remember what the agent edits with (`PipelineRun::harness`).
fn migration_4(tx: &Transaction<'_>) -> rusqlite::Result<()> {
    tx.execute_batch("ALTER TABLE pipeline_runs ADD COLUMN harness TEXT;  -- EditHarness")
}

/// "All videos" (table) and "Board" (kanban by status).
fn default_views() -> [DatabaseView; 2] {
    use BuiltinField::*;
    let layout = |field, width: f32, visible| ColumnLayout { column: Column::Builtin(field), width, visible };
    let table = DatabaseView {
        id: ViewId::new(),
        name: "All videos".to_string(),
        kind: ViewKind::Table,
        columns: vec![
            layout(Title, 320.0, true),
            layout(Status, 140.0, true),
            layout(Format, 120.0, true),
            layout(PublishDate, 140.0, true),
            layout(Tags, 200.0, true),
            layout(Script, 170.0, true),
            layout(Edit, 140.0, true),
            layout(Youtube, 130.0, true),
            layout(Channel, 170.0, true),
            layout(CreatedAt, 150.0, false),
            layout(UpdatedAt, 150.0, false),
        ],
        sorts: Vec::new(),
        filter: FilterSpec { hide_archived: true, ..FilterSpec::default() },
        position: 0,
    };
    // On cards, `visible` selects the fields shown under the title.
    let board = DatabaseView {
        id: ViewId::new(),
        name: "Board".to_string(),
        kind: ViewKind::Board,
        columns: vec![
            layout(Title, 260.0, true),
            layout(Format, 120.0, true),
            layout(PublishDate, 140.0, true),
            layout(Tags, 200.0, true),
            layout(Script, 130.0, true),
            layout(Edit, 140.0, false),
            layout(Youtube, 130.0, false),
            layout(Channel, 170.0, false),
            layout(Status, 140.0, false),
            layout(CreatedAt, 150.0, false),
            layout(UpdatedAt, 150.0, false),
        ],
        sorts: Vec::new(),
        filter: FilterSpec::default(),
        position: 1,
    };
    [table, board]
}

// ---------------------------------------------------------------------------
// Rows
// ---------------------------------------------------------------------------

const VIDEO_COLUMNS: &str = "id, title, icon, status, format, publish_date, tags, hook, folder, publish, youtube, \
                             properties, position, created_at, updated_at";
const INSERT_VIDEO: &str = "INSERT INTO videos (id, title, icon, status, format, publish_date, tags, hook, folder, \
                            publish, youtube, properties, position, created_at, updated_at) \
                            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)";
const UPDATE_VIDEO: &str = "UPDATE videos SET title = ?2, icon = ?3, status = ?4, format = ?5, publish_date = ?6, \
                            tags = ?7, hook = ?8, folder = ?9, publish = ?10, youtube = ?11, properties = ?12, \
                            position = ?13, updated_at = ?15 WHERE id = ?1";

/// Runs `INSERT_VIDEO` or `UPDATE_VIDEO`, which share parameter numbering.
fn write_video(conn: &Connection, sql: &str, video: &Video) -> rusqlite::Result<usize> {
    for value in video.properties.values() {
        if let PropertyValue::Number(number) = value {
            ensure_finite("number property", *number)?;
        }
    }
    conn.prepare_cached(sql)?.execute(params![
        video.id,
        video.title,
        video.icon,
        video.status,
        video.format,
        video.publish_date,
        Json(&video.tags),
        video.hook,
        video.folder.as_deref().map(SqlPath),
        Json(&video.publish),
        video.youtube.as_ref().map(Json),
        Json(&video.properties),
        video.position,
        video.created_at,
        video.updated_at,
    ])
}

fn video_from_row(row: &Row<'_>) -> rusqlite::Result<Video> {
    Ok(Video {
        id: row.get("id")?,
        title: row.get("title")?,
        icon: row.get("icon")?,
        status: row.get("status")?,
        format: row.get("format")?,
        publish_date: row.get("publish_date")?,
        tags: row.get::<_, Json<_>>("tags")?.into_inner(),
        hook: row.get("hook")?,
        folder: row.get::<_, Option<String>>("folder")?.map(PathBuf::from),
        publish: row.get::<_, Json<_>>("publish")?.into_inner(),
        youtube: row.get::<_, Option<Json<_>>>("youtube")?.map(Json::into_inner),
        properties: row.get::<_, Json<_>>("properties")?.into_inner(),
        position: row.get("position")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
    })
}

fn next_video_position(conn: &Connection) -> rusqlite::Result<f64> {
    conn.query_row("SELECT COALESCE(MAX(position) + 1.0, 0.0) FROM videos", [], |row| row.get(0))
}

fn query_video(conn: &Connection, id: VideoId) -> rusqlite::Result<Option<Video>> {
    let sql = format!("SELECT {VIDEO_COLUMNS} FROM videos WHERE id = ?1");
    conn.prepare_cached(&sql)?.query_row([id], video_from_row).optional()
}

fn property_from_row(row: &Row<'_>) -> rusqlite::Result<PropertyDef> {
    Ok(PropertyDef {
        id: row.get("id")?,
        name: row.get("name")?,
        kind: row.get::<_, Json<_>>("kind")?.into_inner(),
        position: row.get("position")?,
    })
}

fn insert_property(conn: &Connection, property: &PropertyDef) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO property_defs (id, name, kind, position) VALUES (?1, ?2, ?3, ?4)",
        params![property.id, property.name, Json(&property.kind), property.position],
    )?;
    Ok(())
}

/// JSON path of a property's value inside `videos.properties`.
fn property_path(id: PropertyId) -> String {
    format!("$.\"{id}\"")
}

fn remove_property_values(conn: &Connection, id: PropertyId) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE videos SET properties = json_remove(properties, ?1) WHERE json_type(properties, ?1) IS NOT NULL",
        [property_path(id)],
    )?;
    Ok(())
}

/// Drops select / multi-select values whose option no longer exists.
fn prune_select_values(conn: &Connection, property: &PropertyDef) -> rusqlite::Result<()> {
    let mut stmt = conn.prepare("SELECT id, properties FROM videos WHERE json_type(properties, ?1) IS NOT NULL")?;
    let videos = stmt
        .query_map([property_path(property.id)], |row| {
            Ok((row.get::<_, VideoId>(0)?, row.get::<_, Json<BTreeMap<PropertyId, PropertyValue>>>(1)?.into_inner()))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (video_id, mut values) in videos {
        prune_select_value(property, &mut values);
        conn.execute("UPDATE videos SET properties = ?2 WHERE id = ?1", params![video_id, Json(&values)])?;
    }
    Ok(())
}

const DOCUMENT_COLUMNS: &str = "id, video_id, kind, title, blocks, position, updated_at";

fn document_from_row(row: &Row<'_>) -> rusqlite::Result<Document> {
    Ok(Document {
        id: row.get("id")?,
        video_id: row.get("video_id")?,
        kind: row.get("kind")?,
        title: row.get("title")?,
        blocks: row.get::<_, Json<_>>("blocks")?.into_inner(),
        position: row.get("position")?,
        updated_at: row.get("updated_at")?,
    })
}

fn query_documents(conn: &Connection, video_id: VideoId) -> rusqlite::Result<Vec<Document>> {
    let sql = format!("SELECT {DOCUMENT_COLUMNS} FROM documents WHERE video_id = ?1 ORDER BY position, id");
    let mut stmt = conn.prepare_cached(&sql)?;
    stmt.query_map([video_id], document_from_row)?.collect()
}

fn upsert_document(conn: &Connection, document: &Document) -> rusqlite::Result<()> {
    let spoken_words = i64::try_from(document::spoken_word_count(&document.blocks)).unwrap_or(i64::MAX);
    conn.prepare_cached(
        "INSERT INTO documents (id, video_id, kind, title, blocks, spoken_words, position, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT (id) DO UPDATE SET video_id = excluded.video_id, kind = excluded.kind, title = excluded.title,
             blocks = excluded.blocks, spoken_words = excluded.spoken_words, position = excluded.position,
             updated_at = excluded.updated_at",
    )?
    .execute(params![
        document.id,
        document.video_id,
        document.kind,
        document.title,
        Json(&document.blocks),
        spoken_words,
        document.position,
        document.updated_at,
    ])?;
    Ok(())
}

/// Serialized `DatabaseView` fields that live in the `config` column.
#[derive(Serialize, serde::Deserialize, Default)]
#[serde(default)]
struct ViewConfig {
    columns: Vec<ColumnLayout>,
    sorts: Vec<SortSpec>,
    filter: FilterSpec,
}

fn query_views(conn: &Connection) -> rusqlite::Result<Vec<DatabaseView>> {
    let mut stmt =
        conn.prepare_cached("SELECT id, name, kind, config, position FROM database_views ORDER BY position, id")?;
    stmt.query_map([], |row| {
        let config: ViewConfig = row.get::<_, Json<_>>("config")?.into_inner();
        Ok(DatabaseView {
            id: row.get("id")?,
            name: row.get("name")?,
            kind: row.get("kind")?,
            columns: config.columns,
            sorts: config.sorts,
            filter: config.filter,
            position: row.get("position")?,
        })
    })?
    .collect()
}

fn upsert_view(conn: &Connection, view: &DatabaseView) -> rusqlite::Result<()> {
    for layout in &view.columns {
        ensure_finite("column width", f64::from(layout.width))?;
    }
    let config = ViewConfig { columns: view.columns.clone(), sorts: view.sorts.clone(), filter: view.filter.clone() };
    conn.prepare_cached(
        "INSERT INTO database_views (id, name, kind, config, position) VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT (id) DO UPDATE SET name = excluded.name, kind = excluded.kind, config = excluded.config,
             position = excluded.position",
    )?
    .execute(params![view.id, view.name, view.kind, Json(&config), view.position])?;
    Ok(())
}

const RUN_COLUMNS: &str = "id, video_id, status, brief, footage_dir, session_id, summary, output, error, cost_usd, \
                           created_at, updated_at, harness";
const INSERT_RUN: &str = "INSERT INTO pipeline_runs (id, video_id, status, brief, footage_dir, session_id, summary, \
                          output, error, cost_usd, created_at, updated_at, harness) \
                          VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)";
const UPDATE_RUN: &str = "UPDATE pipeline_runs SET status = ?3, brief = ?4, footage_dir = ?5, session_id = ?6, \
                          summary = ?7, output = ?8, error = ?9, cost_usd = ?10, updated_at = ?12, harness = ?13 \
                          WHERE id = ?1";

/// Runs `INSERT_RUN` or `UPDATE_RUN`, which share parameter numbering.
fn write_run(conn: &Connection, sql: &str, run: &PipelineRun) -> rusqlite::Result<usize> {
    conn.prepare_cached(sql)?.execute(params![
        run.id,
        run.video_id,
        run.status,
        run.brief,
        SqlPath(&run.footage_dir),
        run.session_id,
        run.summary,
        run.output.as_deref().map(SqlPath),
        run.error,
        run.cost_usd,
        run.created_at,
        run.updated_at,
        run.harness,
    ])
}

fn run_from_row(row: &Row<'_>) -> rusqlite::Result<PipelineRun> {
    Ok(PipelineRun {
        id: row.get("id")?,
        video_id: row.get("video_id")?,
        status: row.get("status")?,
        brief: row.get("brief")?,
        footage_dir: PathBuf::from(row.get::<_, String>("footage_dir")?),
        session_id: row.get("session_id")?,
        summary: row.get("summary")?,
        output: row.get::<_, Option<String>>("output")?.map(PathBuf::from),
        error: row.get("error")?,
        cost_usd: row.get("cost_usd")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
        harness: row.get("harness")?,
    })
}

const UPLOAD_COLUMNS: &str = "id, video_id, file, status, session_url, created_at, updated_at, file_stamp, channel";
const INSERT_UPLOAD: &str = "INSERT INTO uploads \
                             (id, video_id, file, status, session_url, created_at, updated_at, file_stamp, channel) \
                             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)";
const UPDATE_UPLOAD: &str = "UPDATE uploads SET file = ?3, status = ?4, session_url = ?5, updated_at = ?7, \
                             file_stamp = ?8, channel = ?9 WHERE id = ?1";

/// Runs `INSERT_UPLOAD` or `UPDATE_UPLOAD`, which share parameter numbering.
fn write_upload(conn: &Connection, sql: &str, upload: &UploadJob) -> rusqlite::Result<usize> {
    conn.prepare_cached(sql)?.execute(params![
        upload.id,
        upload.video_id,
        SqlPath(&upload.file),
        Json(&upload.status),
        upload.session_url,
        upload.created_at,
        upload.updated_at,
        upload.file_stamp.as_ref().map(Json),
        upload.channel,
    ])
}

fn upload_from_row(row: &Row<'_>) -> rusqlite::Result<UploadJob> {
    Ok(UploadJob {
        id: row.get("id")?,
        video_id: row.get("video_id")?,
        file: PathBuf::from(row.get::<_, String>("file")?),
        file_stamp: row.get::<_, Option<Json<_>>>("file_stamp")?.map(Json::into_inner),
        status: row.get::<_, Json<_>>("status")?.into_inner(),
        session_url: row.get("session_url")?,
        channel: row.get("channel")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
    })
}

// ---------------------------------------------------------------------------
// SQL conversions
// ---------------------------------------------------------------------------

/// A value stored as JSON text.
struct Json<T>(T);

impl<T> Json<T> {
    fn into_inner(self) -> T {
        self.0
    }
}

impl<T: Serialize> ToSql for Json<T> {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        serde_json::to_string(&self.0)
            .map(ToSqlOutput::from)
            .map_err(|err| rusqlite::Error::ToSqlConversionFailure(Box::new(err)))
    }
}

impl<T: DeserializeOwned> FromSql for Json<T> {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        serde_json::from_str(value.as_str()?).map(Json).map_err(FromSqlError::other)
    }
}

/// JSON has no NaN or infinity: serde_json writes them as `null`, which then
/// fails to load and would break every listing of the table.
fn ensure_finite(what: &str, value: f64) -> rusqlite::Result<()> {
    if value.is_finite() {
        Ok(())
    } else {
        Err(rusqlite::Error::ToSqlConversionFailure(format!("{what} must be a finite number, not {value}").into()))
    }
}

/// A path stored as UTF-8 text.
struct SqlPath<'a>(&'a Path);

impl ToSql for SqlPath<'_> {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        let text = self.0.to_str().ok_or_else(|| {
            rusqlite::Error::ToSqlConversionFailure(format!("path is not valid UTF-8: {}", self.0.display()).into())
        })?;
        Ok(ToSqlOutput::from(text))
    }
}

/// Stores ids and unit enums as their serde string (`"0192…"`, `"in_review"`).
macro_rules! sql_as_serde_string {
    ($($ty:ty),* $(,)?) => {$(
        impl ToSql for $ty {
            fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
                match serde_json::to_value(self) {
                    Ok(serde_json::Value::String(text)) => Ok(ToSqlOutput::from(text)),
                    Ok(other) => Err(rusqlite::Error::ToSqlConversionFailure(
                        format!("{} did not serialize to a string: {other}", stringify!($ty)).into(),
                    )),
                    Err(err) => Err(rusqlite::Error::ToSqlConversionFailure(Box::new(err))),
                }
            }
        }

        impl FromSql for $ty {
            fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
                serde_json::from_value(serde_json::Value::String(value.as_str()?.to_owned()))
                    .map_err(FromSqlError::other)
            }
        }
    )*};
}

sql_as_serde_string!(
    VideoId,
    DocumentId,
    PropertyId,
    ViewId,
    RunId,
    UploadId,
    VideoStatus,
    VideoFormat,
    DocumentKind,
    ViewKind,
    RunStatus,
    EditHarness,
);

#[cfg(test)]
mod tests;
