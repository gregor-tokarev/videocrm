//! Domain model shared by every layer of the application.
//!
//! These types are plain data. Persistence lives in `store`, presentation lives
//! in the `rushes` app crate. Ids are UUID v7 so they sort by creation time.

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::settings::EditHarness;

macro_rules! id_type {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub Uuid);

        impl $name {
            pub fn new() -> Self {
                Self(Uuid::now_v7())
            }

            /// Last 8 hex characters, used in folder names and compact UI. These
            /// are random bits; the leading ones of a UUID v7 are a timestamp
            /// that ids created within about a minute share.
            pub fn short(&self) -> String {
                let simple = self.0.simple().to_string();
                simple[simple.len() - 8..].to_string()
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }

        impl std::str::FromStr for $name {
            type Err = uuid::Error;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Uuid::parse_str(s).map(Self)
            }
        }
    };
}

id_type!(
    /// A row in the videos database.
    VideoId
);
id_type!(
    /// A rich document attached to a video (script, notes, ...).
    DocumentId
);
id_type!(
    /// A block inside a document. Stable across edits so the editor can keep
    /// per-block state keyed by it.
    BlockId
);
id_type!(
    /// A user-defined column of the videos database.
    PropertyId
);
id_type!(
    /// A saved view (table or board) of the videos database.
    ViewId
);
id_type!(
    /// One AI editing pipeline run.
    RunId
);
id_type!(
    /// One YouTube upload attempt.
    UploadId
);

/// Semantic color names. The app maps them to theme colors; the core never
/// stores concrete colors.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TagColor {
    #[default]
    Gray,
    Brown,
    Orange,
    Yellow,
    Green,
    Blue,
    Purple,
    Pink,
    Red,
}

impl TagColor {
    pub const ALL: [TagColor; 9] = [
        TagColor::Gray,
        TagColor::Brown,
        TagColor::Orange,
        TagColor::Yellow,
        TagColor::Green,
        TagColor::Blue,
        TagColor::Purple,
        TagColor::Pink,
        TagColor::Red,
    ];

    pub fn label(self) -> &'static str {
        match self {
            TagColor::Gray => "Gray",
            TagColor::Brown => "Brown",
            TagColor::Orange => "Orange",
            TagColor::Yellow => "Yellow",
            TagColor::Green => "Green",
            TagColor::Blue => "Blue",
            TagColor::Purple => "Purple",
            TagColor::Pink => "Pink",
            TagColor::Red => "Red",
        }
    }
}

/// Production stage of a video. Fixed set so boards and filters are reliable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum VideoStatus {
    #[default]
    Idea,
    Scripting,
    Filming,
    Editing,
    Ready,
    Scheduled,
    Published,
    Archived,
}

impl VideoStatus {
    pub const ALL: [VideoStatus; 8] = [
        VideoStatus::Idea,
        VideoStatus::Scripting,
        VideoStatus::Filming,
        VideoStatus::Editing,
        VideoStatus::Ready,
        VideoStatus::Scheduled,
        VideoStatus::Published,
        VideoStatus::Archived,
    ];

    pub fn label(self) -> &'static str {
        match self {
            VideoStatus::Idea => "Idea",
            VideoStatus::Scripting => "Scripting",
            VideoStatus::Filming => "Filming",
            VideoStatus::Editing => "Editing",
            VideoStatus::Ready => "Ready",
            VideoStatus::Scheduled => "Scheduled",
            VideoStatus::Published => "Published",
            VideoStatus::Archived => "Archived",
        }
    }

    pub fn color(self) -> TagColor {
        match self {
            VideoStatus::Idea => TagColor::Gray,
            VideoStatus::Scripting => TagColor::Blue,
            VideoStatus::Filming => TagColor::Purple,
            VideoStatus::Editing => TagColor::Orange,
            VideoStatus::Ready => TagColor::Yellow,
            VideoStatus::Scheduled => TagColor::Pink,
            VideoStatus::Published => TagColor::Green,
            VideoStatus::Archived => TagColor::Brown,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum VideoFormat {
    #[default]
    LongForm,
    Short,
    Livestream,
}

impl VideoFormat {
    pub const ALL: [VideoFormat; 3] = [VideoFormat::LongForm, VideoFormat::Short, VideoFormat::Livestream];

    pub fn label(self) -> &'static str {
        match self {
            VideoFormat::LongForm => "Long-form",
            VideoFormat::Short => "Short",
            VideoFormat::Livestream => "Livestream",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Privacy {
    #[default]
    Private,
    Unlisted,
    Public,
}

impl Privacy {
    pub const ALL: [Privacy; 3] = [Privacy::Private, Privacy::Unlisted, Privacy::Public];

    pub fn label(self) -> &'static str {
        match self {
            Privacy::Private => "Private",
            Privacy::Unlisted => "Unlisted",
            Privacy::Public => "Public",
        }
    }

    /// Value of `status.privacyStatus` in the YouTube Data API.
    pub fn api_value(self) -> &'static str {
        match self {
            Privacy::Private => "private",
            Privacy::Unlisted => "unlisted",
            Privacy::Public => "public",
        }
    }
}

/// YouTube metadata the user prepares before uploading. Lives on the video so
/// it can be drafted long before the upload happens.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
pub struct PublishSettings {
    /// Empty means "use the video title".
    pub title: String,
    pub description: String,
    pub tags: Vec<String>,
    /// YouTube video category id, e.g. "22" (People & Blogs), "27" (Education),
    /// "28" (Science & Technology).
    pub category_id: String,
    pub privacy: Privacy,
    /// Scheduled publish time. YouTube requires `privacy == Private` while scheduled.
    pub publish_at: Option<DateTime<Utc>>,
    pub made_for_kids: bool,
    /// File that will be uploaded. Usually the AI edit output.
    pub file: Option<PathBuf>,
    pub thumbnail: Option<PathBuf>,
    /// The YouTube account the video uploads to; `None` uploads to the default account.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<PublishChannel>,
}

/// The YouTube account a video uploads to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishChannel {
    /// The account's key: its channel id, or its email while it has no channel.
    pub id: String,
    /// The channel's name when it was chosen, shown while the account isn't connected.
    pub name: String,
}

/// Where a video lives on YouTube once uploaded.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct YoutubeLink {
    pub video_id: String,
    pub url: String,
    pub channel_id: String,
    pub privacy: Privacy,
    pub uploaded_at: DateTime<Utc>,
    /// The image YouTube's custom thumbnail of the video was last set from; `None`
    /// until Rushes sets one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thumbnail: Option<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Video {
    pub id: VideoId,
    pub title: String,
    /// Single emoji shown next to the title, Notion style.
    pub icon: Option<String>,
    pub status: VideoStatus,
    pub format: VideoFormat,
    pub publish_date: Option<NaiveDate>,
    pub tags: Vec<String>,
    /// One-line premise / hook used by templates and the AI edit brief.
    pub hook: String,
    /// Media folder for footage and edit outputs. Created lazily.
    pub folder: Option<PathBuf>,
    pub publish: PublishSettings,
    pub youtube: Option<YoutubeLink>,
    /// Values of user-defined properties. Missing key means empty.
    pub properties: BTreeMap<PropertyId, PropertyValue>,
    /// Manual ordering inside the database (ascending).
    pub position: f64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Video {
    pub fn new(title: impl Into<String>) -> Self {
        let now = Utc::now();
        Self {
            id: VideoId::new(),
            title: title.into(),
            icon: None,
            status: VideoStatus::default(),
            format: VideoFormat::default(),
            publish_date: None,
            tags: Vec::new(),
            hook: String::new(),
            folder: None,
            publish: PublishSettings::default(),
            youtube: None,
            properties: BTreeMap::new(),
            position: 0.0,
            created_at: now,
            updated_at: now,
        }
    }

    /// Title used for the YouTube upload.
    pub fn upload_title(&self) -> &str {
        if self.publish.title.trim().is_empty() { &self.title } else { &self.publish.title }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SelectOption {
    /// Stable id so renaming an option keeps existing values.
    pub id: String,
    pub name: String,
    pub color: TagColor,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PropertyKind {
    Text,
    Number,
    Select { options: Vec<SelectOption> },
    MultiSelect { options: Vec<SelectOption> },
    Date,
    Checkbox,
    Url,
}

impl PropertyKind {
    pub fn label(&self) -> &'static str {
        match self {
            PropertyKind::Text => "Text",
            PropertyKind::Number => "Number",
            PropertyKind::Select { .. } => "Select",
            PropertyKind::MultiSelect { .. } => "Multi-select",
            PropertyKind::Date => "Date",
            PropertyKind::Checkbox => "Checkbox",
            PropertyKind::Url => "URL",
        }
    }

    pub fn options(&self) -> &[SelectOption] {
        match self {
            PropertyKind::Select { options } | PropertyKind::MultiSelect { options } => options,
            _ => &[],
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PropertyDef {
    pub id: PropertyId,
    pub name: String,
    pub kind: PropertyKind,
    /// Ordering among custom properties (ascending).
    pub position: i32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum PropertyValue {
    Text(String),
    Number(f64),
    /// Option id.
    Select(Option<String>),
    /// Option ids.
    MultiSelect(Vec<String>),
    Date(Option<NaiveDate>),
    Checkbox(bool),
    Url(String),
}

/// A column of the videos database: either a built-in field or a custom property.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", content = "id", rename_all = "snake_case")]
pub enum Column {
    Builtin(BuiltinField),
    Custom(PropertyId),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuiltinField {
    Title,
    Status,
    Format,
    PublishDate,
    Tags,
    /// Derived: spoken words / estimated runtime of the script.
    Script,
    /// Derived: latest AI edit pipeline state.
    Edit,
    /// Derived: YouTube upload state.
    Youtube,
    /// The YouTube account the video uploads to (`PublishSettings::channel`).
    Channel,
    CreatedAt,
    UpdatedAt,
}

impl BuiltinField {
    pub const ALL: [BuiltinField; 11] = [
        BuiltinField::Title,
        BuiltinField::Status,
        BuiltinField::Format,
        BuiltinField::PublishDate,
        BuiltinField::Tags,
        BuiltinField::Script,
        BuiltinField::Edit,
        BuiltinField::Youtube,
        BuiltinField::Channel,
        BuiltinField::CreatedAt,
        BuiltinField::UpdatedAt,
    ];

    pub fn label(self) -> &'static str {
        match self {
            BuiltinField::Title => "Title",
            BuiltinField::Status => "Status",
            BuiltinField::Format => "Format",
            BuiltinField::PublishDate => "Publish date",
            BuiltinField::Tags => "Tags",
            BuiltinField::Script => "Script",
            BuiltinField::Edit => "AI edit",
            BuiltinField::Youtube => "YouTube",
            BuiltinField::Channel => "Channel",
            BuiltinField::CreatedAt => "Created",
            BuiltinField::UpdatedAt => "Updated",
        }
    }

    /// Derived and timestamp columns cannot be edited in place.
    pub fn is_editable(self) -> bool {
        matches!(
            self,
            BuiltinField::Title
                | BuiltinField::Status
                | BuiltinField::Format
                | BuiltinField::PublishDate
                | BuiltinField::Tags
                | BuiltinField::Channel
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ViewKind {
    #[default]
    Table,
    /// Kanban grouped by status.
    Board,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SortDirection {
    Ascending,
    Descending,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SortSpec {
    pub column: Column,
    pub direction: SortDirection,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
pub struct FilterSpec {
    /// Free-text match against title, hook and tags (case-insensitive).
    pub search: String,
    /// Empty means all statuses.
    pub statuses: Vec<VideoStatus>,
    /// Empty means all formats.
    pub formats: Vec<VideoFormat>,
    /// Hide archived videos unless explicitly filtered for.
    pub hide_archived: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ColumnLayout {
    pub column: Column,
    pub width: f32,
    pub visible: bool,
}

/// A saved, named way of looking at the videos database.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DatabaseView {
    pub id: ViewId,
    pub name: String,
    pub kind: ViewKind,
    /// Column order, width and visibility. Columns missing from this list are
    /// appended hidden-by-default (custom) or visible (builtin) by the UI.
    pub columns: Vec<ColumnLayout>,
    /// Applied in order; empty means manual `position` order.
    pub sorts: Vec<SortSpec>,
    pub filter: FilterSpec,
    pub position: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DocumentKind {
    Script,
    Notes,
    Custom,
}

/// Kind of a document block. Script-specific kinds (`TalkingPoint`, `BRoll`,
/// `OnScreen`) are shown in the script but not counted as spoken words: a talking
/// point is said in the creator's own words, the others are production cues.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum BlockKind {
    #[default]
    Paragraph,
    Heading1,
    Heading2,
    Heading3,
    Bulleted,
    Numbered,
    Todo,
    Quote,
    Callout,
    Code,
    Divider,
    /// An idea the creator talks through in their own words while recording,
    /// instead of reading it.
    TalkingPoint,
    /// Visual cue for the editor: footage to cut away to.
    BRoll,
    /// Text overlay shown on screen.
    OnScreen,
}

impl BlockKind {
    pub const ALL: [BlockKind; 14] = [
        BlockKind::Paragraph,
        BlockKind::Heading1,
        BlockKind::Heading2,
        BlockKind::Heading3,
        BlockKind::Bulleted,
        BlockKind::Numbered,
        BlockKind::Todo,
        BlockKind::Quote,
        BlockKind::Callout,
        BlockKind::Code,
        BlockKind::Divider,
        BlockKind::TalkingPoint,
        BlockKind::BRoll,
        BlockKind::OnScreen,
    ];

    pub fn label(self) -> &'static str {
        match self {
            BlockKind::Paragraph => "Text",
            BlockKind::Heading1 => "Heading 1",
            BlockKind::Heading2 => "Heading 2",
            BlockKind::Heading3 => "Heading 3",
            BlockKind::Bulleted => "Bulleted list",
            BlockKind::Numbered => "Numbered list",
            BlockKind::Todo => "To-do",
            BlockKind::Quote => "Quote",
            BlockKind::Callout => "Callout",
            BlockKind::Code => "Code",
            BlockKind::Divider => "Divider",
            BlockKind::TalkingPoint => "Talking point",
            BlockKind::BRoll => "B-roll cue",
            BlockKind::OnScreen => "On-screen text",
        }
    }

    /// Whether the block's text is read aloud in the video, word for word.
    pub fn is_spoken(self) -> bool {
        matches!(self, BlockKind::Paragraph | BlockKind::Bulleted | BlockKind::Numbered | BlockKind::Quote)
    }

    pub fn is_heading(self) -> bool {
        matches!(self, BlockKind::Heading1 | BlockKind::Heading2 | BlockKind::Heading3)
    }

    pub fn has_text(self) -> bool {
        !matches!(self, BlockKind::Divider)
    }
}

/// One block of a document. `text` holds inline Markdown (`**bold**`,
/// `*italic*`, `` `code` ``, `[link](url)`); block structure is never encoded in it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Block {
    pub id: BlockId,
    pub kind: BlockKind,
    pub text: String,
    /// Only meaningful for `Todo`.
    #[serde(default)]
    pub checked: bool,
    /// Nesting level for list items (0 = top level).
    #[serde(default)]
    pub indent: u8,
    /// Template guidance shown in place of the text while the block is empty
    /// ("Lead with the end result…"). It is never part of the script: not
    /// spoken, not exported and not copied as teleprompter text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placeholder: Option<String>,
}

impl Block {
    pub fn new(kind: BlockKind, text: impl Into<String>) -> Self {
        Self { id: BlockId::new(), kind, text: text.into(), checked: false, indent: 0, placeholder: None }
    }

    pub fn paragraph(text: impl Into<String>) -> Self {
        Self::new(BlockKind::Paragraph, text)
    }

    /// An empty block that shows `guidance` until the creator writes in it.
    pub fn guided(kind: BlockKind, guidance: impl Into<String>) -> Self {
        Self { placeholder: Some(guidance.into()), ..Self::new(kind, "") }
    }

    /// The template guidance to show: set, and the block still has no text.
    pub fn guidance(&self) -> Option<&str> {
        self.placeholder.as_deref().filter(|_| self.text.trim().is_empty())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Document {
    pub id: DocumentId,
    pub video_id: VideoId,
    pub kind: DocumentKind,
    pub title: String,
    pub blocks: Vec<Block>,
    pub position: i32,
    pub updated_at: DateTime<Utc>,
}

/// Lifecycle of an AI editing pipeline run (video-use driven by a coding agent).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Queued,
    /// Agent is inventorying, transcribing and drafting the edit strategy.
    Planning,
    /// Strategy proposed; waiting for the user to approve or give feedback.
    AwaitingApproval,
    /// Agent is cutting and rendering.
    Rendering,
    Succeeded,
    Failed,
    Cancelled,
}

impl RunStatus {
    pub fn label(self) -> &'static str {
        match self {
            RunStatus::Queued => "Queued",
            RunStatus::Planning => "Planning",
            RunStatus::AwaitingApproval => "Needs approval",
            RunStatus::Rendering => "Rendering",
            RunStatus::Succeeded => "Done",
            RunStatus::Failed => "Failed",
            RunStatus::Cancelled => "Cancelled",
        }
    }

    pub fn is_active(self) -> bool {
        matches!(self, RunStatus::Queued | RunStatus::Planning | RunStatus::Rendering)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PipelineRun {
    pub id: RunId,
    pub video_id: VideoId,
    pub status: RunStatus,
    /// Instructions given to the agent (generated from the script, editable).
    pub brief: String,
    /// Directory holding the source footage; outputs go to `<footage_dir>/edit/`.
    pub footage_dir: PathBuf,
    /// Agent session id so follow-ups resume the same conversation.
    pub session_id: Option<String>,
    /// What the agent edits with. Fixed when the run starts, so follow-ups resume the
    /// session with the tools it began with; `None` for runs from before it was kept.
    #[serde(default)]
    pub harness: Option<EditHarness>,
    /// Latest strategy / summary text from the agent (Markdown).
    pub summary: Option<String>,
    pub output: Option<PathBuf>,
    pub error: Option<String>,
    pub cost_usd: Option<f64>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum UploadStatus {
    Pending,
    Uploading {
        sent: u64,
        total: u64,
    },
    /// Bytes delivered; YouTube is processing.
    Processing {
        video_id: String,
    },
    Done {
        video_id: String,
    },
    Failed {
        error: String,
    },
    Cancelled,
}

impl UploadStatus {
    pub fn label(&self) -> &'static str {
        match self {
            UploadStatus::Pending => "Pending",
            UploadStatus::Uploading { .. } => "Uploading",
            UploadStatus::Processing { .. } => "Processing",
            UploadStatus::Done { .. } => "Uploaded",
            UploadStatus::Failed { .. } => "Failed",
            UploadStatus::Cancelled => "Cancelled",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UploadJob {
    pub id: UploadId,
    pub video_id: VideoId,
    pub file: PathBuf,
    /// `file` as it was when the upload started. A resume must send the same
    /// bytes; `None` for jobs stored before stamps were recorded.
    pub file_stamp: Option<FileStamp>,
    pub status: UploadStatus,
    /// Resumable session URL returned by YouTube; lets an interrupted upload continue.
    pub session_url: Option<String>,
    /// Key of the YouTube account the upload goes to (see [`PublishChannel::id`]);
    /// `None` for jobs stored before uploads recorded it.
    pub channel: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// One version of a file: replacing or re-exporting it changes the stamp.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileStamp {
    pub size: u64,
    pub modified: Option<DateTime<Utc>>,
}

/// A video plus the derived values the database views display.
#[derive(Clone, Debug, PartialEq)]
pub struct VideoRow {
    pub video: Video,
    pub script_words: usize,
    /// Estimated spoken runtime of the script in seconds.
    pub script_seconds: u32,
    /// Status of the most recent AI edit run.
    pub latest_run: Option<RunStatus>,
    /// Whether any AI edit run of the video succeeded, i.e. it has a finished cut.
    pub has_finished_edit: bool,
    /// Status of the most recent upload attempt.
    pub latest_upload: Option<UploadStatus>,
}

/// Where a video stands on YouTube, as the database shows it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum YoutubeState<'a> {
    /// The video is on YouTube.
    Live(&'a YoutubeLink),
    /// Not on YouTube: the latest upload attempt.
    Upload(&'a UploadStatus),
}

impl VideoRow {
    /// The AI edit state to show: the latest run, except that a failed or
    /// cancelled retry does not hide an earlier edit that finished.
    pub fn edit_status(&self) -> Option<RunStatus> {
        match self.latest_run {
            Some(RunStatus::Failed | RunStatus::Cancelled) if self.has_finished_edit => Some(RunStatus::Succeeded),
            latest => latest,
        }
    }

    /// The latest run when [`Self::edit_status`] shows an earlier, finished one instead.
    pub fn newer_edit_attempt(&self) -> Option<RunStatus> {
        self.latest_run.filter(|latest| Some(*latest) != self.edit_status())
    }

    /// The video's YouTube link when it is on YouTube, otherwise the latest
    /// upload attempt; `None` when it was never uploaded.
    pub fn youtube_state(&self) -> Option<YoutubeState<'_>> {
        match (&self.video.youtube, &self.latest_upload) {
            (Some(link), _) => Some(YoutubeState::Live(link)),
            // The upload reached YouTube, but the app dropped the link since because
            // YouTube no longer has the video: it isn't on YouTube.
            (None, Some(UploadStatus::Processing { .. } | UploadStatus::Done { .. })) => None,
            (None, Some(upload)) => Some(YoutubeState::Upload(upload)),
            (None, None) => None,
        }
    }

    /// Whether the video reached YouTube and is gone from there since (it was
    /// deleted in YouTube Studio, say): its latest upload got there, but the
    /// app dropped the link because YouTube no longer has the video.
    pub fn left_youtube(&self) -> bool {
        self.video.youtube.is_none()
            && matches!(self.latest_upload, Some(UploadStatus::Processing { .. } | UploadStatus::Done { .. }))
    }

    /// The latest upload attempt when [`Self::youtube_state`] shows the video
    /// already on YouTube and that attempt has not ended there (it is still
    /// running, failed or was cancelled).
    pub fn newer_upload_attempt(&self) -> Option<&UploadStatus> {
        self.video.youtube.as_ref()?;
        self.latest_upload.as_ref().filter(|upload| !matches!(upload, UploadStatus::Done { .. }))
    }
}
