//! Rushes' own AI editing tools.
//!
//! A coding agent (Claude Code) edits a footage folder through these tools, served over
//! MCP: it reads the takes as numbered phrases, decides what to keep by phrase id, and
//! the tools do the arithmetic, cutting, rendering and measuring. Synchronous and
//! UI-free; ffmpeg and ffprobe do the media work. See `docs/editing-harness.md`.

mod activity;
mod audio;
mod check;
mod error;
mod import;
mod look;
pub mod mcp;
pub mod plan;
pub mod probe;
pub mod project;
mod rational;
pub mod render;
pub mod session;
mod shots;
mod subtitles;
mod tool;
pub mod tools;
pub mod transcribe;
pub mod transcript;

pub use error::{EditError, Result};
pub use plan::{CutPlan, ResolvedCut, SegmentSpec};
pub use project::{FOOTAGE_EXTENSIONS, IMAGE_EXTENSIONS, Project};
pub use rational::Rational;
pub use session::Session;
pub use tool::find as find_tool;

/// The agent's instructions for editing with these tools.
pub const INSTRUCTIONS: &str = include_str!("instructions.md");

/// Name of the MCP server the agent sees (its tools are `mcp__rushes__<tool>`).
pub const SERVER_NAME: &str = "rushes";
/// Tool names, for allow-lists.
pub const TOOL_NAMES: [&str; 10] = [
    "inventory",
    "transcribe",
    "read_transcript",
    "phrase_words",
    "silences",
    "shots",
    "look",
    "plan_cut",
    "render",
    "check",
];
