//! Pipeline phases: what to ask the agent, and what came out of it.
//!
//! An edit never cuts before the creator approved its plain-English strategy, so it
//! runs as a [`Phase::Plan`] followed by [`Phase::Approve`] (and any number of
//! [`Phase::Feedback`] rounds), each resuming the same agent session. [`Phase::Auto`]
//! does everything in one go for creators who pre-approve.
//!
//! The agent edits with one of two [`Harness`]es: Rushes' own editing tools, served
//! to it over MCP next to its shell, or the video-use skill (a prompt plus Python
//! helpers it runs through its shell).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::SystemTime;

use crate::agent::{AgentConfig, Invocation, run_agent};
use crate::error::PipelineError;
use crate::event::AgentEvent;
use crate::media::{FootageFile, list_footage};

const SKILL_FILE: &str = "SKILL.md";
const EDIT_DIR: &str = "edit";
const FINAL_CUT: &str = "final.mp4";
const PREVIEW: &str = "preview.mp4";
/// Where [`keep_final_cut`] moves finished renders, inside the edit folder.
const CUTS_DIR: &str = "cuts";
/// Where [`set_aside_final_cut`] moves renders nobody can use, inside the edit folder.
const LEFTOVERS_DIR: &str = "leftovers";

const HEADLESS_NOTE: &str = "You are running headless inside the Rushes app: nobody can \
answer questions mid-run. Where something is ambiguous, make the most sensible choice and \
say which choice you made.\n\n";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Inventory, transcribe and propose a strategy, then stop without rendering.
    Plan { brief: String },
    /// The creator approved the strategy: render, self-evaluate, summarise.
    Approve { notes: Option<String> },
    /// Revise the edit with the creator's feedback and re-render.
    Feedback { message: String },
    /// Plan and render in one pass; the creator pre-approved the strategy.
    Auto { brief: String },
}

/// What the agent edits with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Harness {
    /// Rushes' editing tools (`rushes-edit`), served over MCP by `tools`. The agent
    /// cuts and renders only through them, and has its shell, file and web tools to
    /// find and fetch material.
    Rushes { tools: ToolServer },
    /// The video-use skill: the folder holding its `SKILL.md` and `helpers/`.
    VideoUse { skill_dir: PathBuf },
}

/// A program that serves Rushes' editing tools over MCP on stdio when started with
/// `args` followed by `--footage <dir>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolServer {
    pub program: PathBuf,
    pub args: Vec<String>,
}

/// Where a phase runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunContext {
    /// Raw footage; the agent's working directory. Outputs go to `<footage_dir>/edit/`.
    pub footage_dir: PathBuf,
    pub harness: Harness,
    /// Agent session to resume (the previous phase's [`PhaseOutcome::session_id`]).
    pub session_id: Option<String>,
}

/// Built-in tools the agent keeps with Rushes' tools: the shell and the web, to find,
/// fetch and make material; files; a todo list.
const RUSHES_BUILTIN_TOOLS: [&str; 9] =
    ["Bash", "Read", "Write", "Edit", "Glob", "Grep", "TodoWrite", "WebFetch", "WebSearch"];
/// How long one tool call may take (a final render of a long video takes minutes).
const TOOL_TIMEOUT_MS: &str = "3600000";

#[derive(Clone, Debug, PartialEq)]
pub struct PhaseOutcome {
    /// Session to resume in the next phase.
    pub session_id: Option<String>,
    /// The agent's final message: the strategy after a plan, a summary otherwise.
    pub summary_text: String,
    pub cost_usd: Option<f64>,
    /// The agent finished normally but reported an error (e.g. it hit the turn limit).
    pub is_error: bool,
    /// `<footage_dir>/edit/final.mp4`, if it exists after the phase.
    pub final_cut: Option<PathBuf>,
    /// `<footage_dir>/edit/preview.mp4`, if it exists after the phase.
    pub preview: Option<PathBuf>,
    /// `final.mp4` was created or rewritten during this phase.
    pub rendered: bool,
}

/// Runs one phase to completion, streaming agent events to `on_event`.
///
/// Blocks until the agent exits. Setting `cancel` stops the agent and every
/// process it started, and returns [`PipelineError::Cancelled`].
pub fn run_phase(
    config: &AgentConfig,
    ctx: &RunContext,
    phase: Phase,
    on_event: &mut dyn FnMut(&AgentEvent),
    cancel: &AtomicBool,
) -> Result<PhaseOutcome, PipelineError> {
    if !ctx.footage_dir.is_dir() {
        return Err(PipelineError::FootageNotFound(ctx.footage_dir.clone()));
    }
    let program = config.resolve_command().ok_or_else(|| PipelineError::AgentNotFound(config.command.clone()))?;
    let footage = match phase {
        Phase::Plan { .. } | Phase::Auto { .. } => list_footage(&ctx.footage_dir)?,
        Phase::Approve { .. } | Phase::Feedback { .. } => Vec::new(),
    };

    // A blank id (e.g. an unset database column) means a fresh session.
    let resume = ctx.session_id.clone().filter(|session_id| !session_id.trim().is_empty());

    let edit_dir = ctx.footage_dir.join(EDIT_DIR);
    let final_cut = edit_dir.join(FINAL_CUT);
    let before = file_stamp(&final_cut);
    let invocation = match &ctx.harness {
        Harness::VideoUse { skill_dir } => {
            let skill_md = skill_dir.join(SKILL_FILE);
            if !skill_md.is_file() {
                return Err(PipelineError::SkillNotFound(skill_md));
            }
            // The skill is usually a symlink; `--add-dir` and the agent work with real paths.
            let skill_dir = fs::canonicalize(skill_dir)?;
            Invocation {
                cwd: ctx.footage_dir.clone(),
                prompt: build_prompt(&phase, &skill_dir.join(SKILL_FILE), &footage),
                resume: resume.clone(),
                add_dirs: vec![skill_dir],
                ..Invocation::default()
            }
        }
        Harness::Rushes { tools } => {
            let footage_dir = std::path::absolute(&ctx.footage_dir)?;
            let mut args = tools.args.clone();
            args.extend(["--footage".to_owned(), footage_dir.to_string_lossy().into_owned()]);
            let server = serde_json::json!({"type": "stdio", "command": tools.program, "args": args});
            let mcp_config = serde_json::json!({"mcpServers": {rushes_edit::SERVER_NAME: server}}).to_string();
            let builtin: Vec<String> = RUSHES_BUILTIN_TOOLS.map(str::to_owned).to_vec();
            let mut allowed = vec![format!("mcp__{}", rushes_edit::SERVER_NAME)];
            allowed.extend(builtin.iter().cloned());
            Invocation {
                cwd: ctx.footage_dir.clone(),
                prompt: build_rushes_prompt(&phase, &footage),
                resume: resume.clone(),
                mcp_config: Some(mcp_config),
                tools: Some(builtin),
                allowed_tools: Some(allowed),
                system_prompt: Some(rushes_edit::INSTRUCTIONS.to_owned()),
                env: vec![("MCP_TOOL_TIMEOUT".to_owned(), TOOL_TIMEOUT_MS.to_owned())],
                ..Invocation::default()
            }
        }
    };
    let (status, run) = run_agent(config, &program, &invocation, on_event, cancel)?;

    let session_id = run.session_id.or(resume);
    let failed = |message: String, no_result: bool| PipelineError::AgentFailed {
        message,
        stderr_tail: run.stderr_tail.clone(),
        session_id: session_id.clone(),
        no_result,
    };
    let Some(AgentEvent::Result { is_error, result_text, cost_usd, .. }) = run.result else {
        return Err(failed(format!("the agent stopped without reporting a result ({status})"), true));
    };
    if !status.success() {
        let message = if is_error && !result_text.is_empty() {
            result_text
        } else {
            format!("the agent stopped unexpectedly ({status})")
        };
        return Err(failed(message, false));
    }

    let summary_text = if result_text.trim().is_empty() { run.last_text.unwrap_or_default() } else { result_text };
    let after = file_stamp(&final_cut);
    let preview = edit_dir.join(PREVIEW);
    Ok(PhaseOutcome {
        session_id,
        summary_text,
        cost_usd,
        is_error,
        rendered: after.is_some() && after != before,
        final_cut: final_cut.is_file().then_some(final_cut),
        preview: preview.is_file().then_some(preview),
    })
}

/// Moves the final cut a phase rendered (`<footage_dir>/edit/final.mp4`) to
/// `<footage_dir>/edit/cuts/final-<n>.mp4`, numbered after the cuts already kept, and
/// returns the new path. Each render keeps its own file this way: the next render
/// writes a new `final.mp4` instead of overwriting one a run or an upload refers to.
pub fn keep_final_cut(footage_dir: &Path) -> io::Result<PathBuf> {
    move_final_cut(footage_dir, CUTS_DIR)
}

/// Moves a `<footage_dir>/edit/final.mp4` that is no finished render (a cancelled or
/// interrupted render may have written half of it, or something else left it there)
/// to `<footage_dir>/edit/leftovers/final-<n>.mp4` and returns the new path, so the
/// next render does not overwrite it and [`kept_cuts`] never offers it.
pub fn set_aside_final_cut(footage_dir: &Path) -> io::Result<PathBuf> {
    move_final_cut(footage_dir, LEFTOVERS_DIR)
}

/// Moves `edit/final.mp4` into `edit/<dir_name>/`, numbered after the files there.
fn move_final_cut(footage_dir: &Path, dir_name: &str) -> io::Result<PathBuf> {
    let edit_dir = footage_dir.join(EDIT_DIR);
    let dir = edit_dir.join(dir_name);
    fs::create_dir_all(&dir)?;
    let next = cut_numbers(&dir).max().unwrap_or(0) + 1;
    let dest = (next..).map(|n| dir.join(cut_name(n))).find(|path| !path.exists()).unwrap_or_default();
    fs::rename(edit_dir.join(FINAL_CUT), &dest)?;
    Ok(dest)
}

/// The cuts [`keep_final_cut`] kept in `<footage_dir>/edit/cuts/`, newest first.
pub fn kept_cuts(footage_dir: &Path) -> Vec<PathBuf> {
    let cuts_dir = footage_dir.join(EDIT_DIR).join(CUTS_DIR);
    let mut numbers: Vec<u32> = cut_numbers(&cuts_dir).collect();
    numbers.sort_unstable_by(|a, b| b.cmp(a));
    numbers.into_iter().map(|n| cuts_dir.join(cut_name(n))).filter(|path| path.is_file()).collect()
}

fn cut_name(n: u32) -> String {
    format!("final-{n}.mp4")
}

/// Numbers of the `final-<n>.mp4` files in `dir`.
fn cut_numbers(dir: &Path) -> impl Iterator<Item = u32> {
    fs::read_dir(dir).into_iter().flatten().filter_map(|entry| {
        let name = entry.ok()?.file_name().into_string().ok()?;
        name.strip_prefix("final-")?.strip_suffix(".mp4")?.parse().ok()
    })
}

/// Modification time and size, to tell whether a file was rewritten.
fn file_stamp(path: &Path) -> Option<(SystemTime, u64)> {
    let meta = fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

/// The prompt sent to the agent for `phase`. `skill_md` is the absolute path of the
/// video-use `SKILL.md`; `footage` lists the source files (used by plan and auto).
pub fn build_prompt(phase: &Phase, skill_md: &Path, footage: &[FootageFile]) -> String {
    let skill = skill_md.display();
    match phase {
        Phase::Plan { brief } => format!(
            "Rushes edit request (planning phase).\n\n\
             {intro}{brief}\
             This phase is planning only:\n\
             1. Inventory and transcribe the footage as the skill describes (transcripts are \
             cached for the render phase).\n\
             2. Pre-scan the transcripts for slips, false starts and retakes.\n\
             3. Propose the edit strategy in plain English, 4-8 sentences: shape, take choices, \
             cut direction, grade, subtitles, animations and estimated length. Append it to \
             ./edit/project.md as a new session.\n\
             4. Then STOP. Do not cut or render anything. The creator reviews the strategy in \
             Rushes and approves it or sends feedback in a follow-up message.\n\n\
             {HEADLESS_NOTE}\
             End with the strategy only: your final message must be the strategy in Markdown, \
             without preamble, with any open questions for the creator at the end.",
            intro = intro(skill_md, footage),
            brief = brief_section(brief),
        ),
        Phase::Approve { notes } => format!(
            "Rushes edit: strategy approved (render phase).\n\n\
             The creator approved the edit strategy you proposed (recorded in \
             ./edit/project.md).{notes}\n\n\
             Continue with the video-use skill ({skill}) in the current directory:\n\
             1. Execute the edit. Render a preview to ./edit/preview.mp4, then the final cut \
             to ./edit/final.mp4.\n\
             2. Self-evaluate the rendered output as the skill describes; fix and re-render \
             if needed (at most 3 passes).\n\
             3. Append this session to ./edit/project.md.\n\n\
             {HEADLESS_NOTE}\
             Reply with a short summary of what was produced: final length, structure, \
             notable decisions and anything the creator should check.",
            notes = match notes.as_deref().map(str::trim) {
                Some(notes) if !notes.is_empty() => {
                    format!(" Their notes, to take into account:\n\n{notes}")
                }
                _ => String::new(),
            },
        ),
        Phase::Feedback { message } => format!(
            "Rushes edit: feedback from the creator (revision phase).\n\n\
             The creator reviewed your work and asks for these changes:\n\n\
             {message}\n\n\
             Continue with the video-use skill ({skill}) in the current directory; the \
             decisions so far are in ./edit/project.md:\n\
             1. Apply the feedback. If nothing has been rendered yet, adjust the strategy \
             accordingly and go ahead with it.\n\
             2. Re-render ./edit/preview.mp4 and ./edit/final.mp4, reusing the cached \
             transcripts.\n\
             3. Self-evaluate the new render (at most 3 passes) and append the decisions to \
             ./edit/project.md.\n\n\
             {HEADLESS_NOTE}\
             Reply with a short summary of what changed.",
            message = message.trim(),
        ),
        Phase::Auto { brief } => format!(
            "Rushes edit request (single pass, pre-approved).\n\n\
             {intro}{brief}\
             The creator pre-approved this edit, so do not stop to confirm the strategy:\n\
             1. Inventory and transcribe the footage, decide the strategy and append it to \
             ./edit/project.md.\n\
             2. Execute the edit. Render a preview to ./edit/preview.mp4, then the final cut \
             to ./edit/final.mp4.\n\
             3. Self-evaluate the rendered output as the skill describes; fix and re-render \
             if needed (at most 3 passes).\n\n\
             {HEADLESS_NOTE}\
             Reply with a short summary: the strategy you chose and what was produced.",
            intro = intro(skill_md, footage),
            brief = brief_section(brief),
        ),
    }
}

/// The prompt for `phase` when the agent edits with Rushes' tools (see [`Harness::Rushes`]).
pub fn build_rushes_prompt(phase: &Phase, footage: &[FootageFile]) -> String {
    match phase {
        Phase::Plan { brief } => format!(
            "Rushes edit request (planning phase).\n\n\
             {intro}{brief}\
             This phase is planning only:\n\
             1. inventory, transcribe and read every transcript; check the silences and look at \
             what matters, as your instructions describe.\n\
             2. Set the cut with plan_cut and fix the warnings that matter. The plan is kept for \
             the render phase.\n\
             3. Write the strategy in plain English, 4-8 sentences: structure, which takes, what \
             is cut and why, what is kept on screen without speech, estimated length (from the \
             receipt). Append it to ./edit/project.md as a new session.\n\
             4. Then STOP. Do not render anything. The creator reviews the strategy in Rushes and \
             approves it or sends feedback in a follow-up message.\n\n\
             {HEADLESS_NOTE}\
             End with the strategy only: your final message must be the strategy in Markdown, \
             without preamble, with any open questions for the creator at the end.",
            intro = rushes_intro(footage),
            brief = brief_section(brief),
        ),
        Phase::Approve { notes } => format!(
            "Rushes edit: strategy approved (render phase).\n\n\
             The creator approved the edit strategy you proposed (recorded in \
             ./edit/project.md; the cut is the current plan).{notes}\n\n\
             Continue with the rushes tools:\n\
             1. If the notes ask for changes, plan the cut again.\n\
             2. Render a preview, read its checks, look at the joins that could go wrong, fix and \
             render again where needed.\n\
             3. Render final (./edit/final.mp4) and append this session to ./edit/project.md.\n\n\
             {HEADLESS_NOTE}\
             Reply with a short summary of what was produced: final length, structure, notable \
             decisions and anything the creator should check.",
            notes = match notes.as_deref().map(str::trim) {
                Some(notes) if !notes.is_empty() => {
                    format!(" Their notes, to take into account:\n\n{notes}")
                }
                _ => String::new(),
            },
        ),
        Phase::Feedback { message } => format!(
            "Rushes edit: feedback from the creator (revision phase).\n\n\
             The creator reviewed your work and asks for these changes:\n\n\
             {message}\n\n\
             Continue with the rushes tools; the decisions so far are in ./edit/project.md:\n\
             1. Apply the feedback: plan the cut again.\n\
             2. Render a preview, check it and fix, then render final (./edit/final.mp4).\n\
             3. Append the decisions to ./edit/project.md.\n\n\
             {HEADLESS_NOTE}\
             Reply with a short summary of what changed.",
            message = message.trim(),
        ),
        Phase::Auto { brief } => format!(
            "Rushes edit request (single pass, pre-approved).\n\n\
             {intro}{brief}\
             The creator pre-approved this edit, so do not stop to confirm the strategy:\n\
             1. inventory, transcribe and read every transcript; check the silences and look at \
             what matters.\n\
             2. Set the cut with plan_cut and fix the warnings that matter.\n\
             3. Render a preview, read its checks, look at the joins that could go wrong, fix.\n\
             4. Render final (./edit/final.mp4) and append the session to ./edit/project.md.\n\n\
             {HEADLESS_NOTE}\
             Reply with a short summary: the strategy you chose and what was produced.",
            intro = rushes_intro(footage),
            brief = brief_section(brief),
        ),
    }
}

fn rushes_intro(footage: &[FootageFile]) -> String {
    let files = if footage.is_empty() {
        "No media files were found in the current directory: call inventory, and if there is \
         nothing to edit, say so and stop."
            .to_owned()
    } else {
        let lines: Vec<String> = footage.iter().map(describe).collect();
        format!("Raw footage in the current directory:\n{}", lines.join("\n"))
    };
    format!(
        "Edit the raw footage in the current directory with the rushes tools, as your \
         instructions describe.\n\n\
         {files}\n\n\
         The tools write every output to ./edit/. Never modify the source files.\n\n"
    )
}

fn intro(skill_md: &Path, footage: &[FootageFile]) -> String {
    let files = if footage.is_empty() {
        "No media files were found in the current directory. Check with `ls`, and if there \
         is nothing to edit, say so and stop."
            .to_owned()
    } else {
        let lines: Vec<String> = footage.iter().map(describe).collect();
        format!("Raw footage in the current directory:\n{}", lines.join("\n"))
    };
    format!(
        "Use the video-use skill to edit the raw footage in the current directory. Read the \
         skill first: {}. Its helper scripts are in helpers/ next to it.\n\n\
         {files}\n\n\
         Write every output to ./edit/. Never modify the source files or anything in the \
         skill directory.\n\n",
        skill_md.display(),
    )
}

fn brief_section(brief: &str) -> String {
    match brief.trim() {
        "" => "The creator gave no brief: infer the kind of video from the footage.\n\n".to_owned(),
        brief => format!("The creator's brief:\n\n{brief}\n\n"),
    }
}

/// `- take1.mp4 (1:02, 1920x1080)`
fn describe(file: &FootageFile) -> String {
    let Some(media) = &file.media else {
        return format!("- {}", file.name);
    };
    let mut facts = vec![format_duration(media.duration_secs)];
    match (media.width, media.height) {
        (Some(width), Some(height)) => {
            facts.push(format!("{width}x{height}"));
            if !media.has_audio {
                facts.push("no audio".to_owned());
            }
        }
        _ => facts.push("audio only".to_owned()),
    }
    format!("- {} ({})", file.name, facts.join(", "))
}

/// `m:ss`, or `h:mm:ss` from one hour.
fn format_duration(seconds: f64) -> String {
    let total = seconds.max(0.0).round() as u64;
    let (hours, minutes, seconds) = (total / 3600, total / 60 % 60, total % 60);
    if hours > 0 { format!("{hours}:{minutes:02}:{seconds:02}") } else { format!("{minutes}:{seconds:02}") }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn final_cuts_are_kept_under_new_numbers() {
        let footage = tempfile::tempdir().unwrap();
        let edit = footage.path().join(EDIT_DIR);
        fs::create_dir_all(&edit).unwrap();
        assert_eq!(kept_cuts(footage.path()), Vec::<PathBuf>::new());
        assert!(keep_final_cut(footage.path()).is_err(), "nothing rendered");

        fs::write(edit.join(FINAL_CUT), "first").unwrap();
        let first = keep_final_cut(footage.path()).unwrap();
        assert_eq!(first, edit.join("cuts/final-1.mp4"));
        assert!(!edit.join(FINAL_CUT).exists());

        // Gaps (a deleted cut) are not reused: numbers keep counting up.
        fs::write(edit.join("cuts/final-7.mp4"), "kept by hand").unwrap();
        fs::write(edit.join("cuts/notes.txt"), "").unwrap();
        fs::write(edit.join(FINAL_CUT), "second").unwrap();
        let second = keep_final_cut(footage.path()).unwrap();
        assert_eq!(second, edit.join("cuts/final-8.mp4"));
        assert_eq!(fs::read_to_string(&first).unwrap(), "first");
        assert_eq!(fs::read_to_string(&second).unwrap(), "second");
        assert_eq!(kept_cuts(footage.path()), [second, edit.join("cuts/final-7.mp4"), first]);
    }

    #[test]
    fn unfinished_renders_are_set_aside_where_cuts_are_not_listed() {
        let footage = tempfile::tempdir().unwrap();
        let edit = footage.path().join(EDIT_DIR);
        fs::create_dir_all(&edit).unwrap();
        fs::write(edit.join(FINAL_CUT), "finished").unwrap();
        let cut = keep_final_cut(footage.path()).unwrap();
        assert!(set_aside_final_cut(footage.path()).is_err(), "nothing left at the shared path");

        fs::write(edit.join(FINAL_CUT), "half a render").unwrap();
        let first = set_aside_final_cut(footage.path()).unwrap();
        assert_eq!(first, edit.join("leftovers/final-1.mp4"));
        fs::write(edit.join(FINAL_CUT), "another half").unwrap();
        let second = set_aside_final_cut(footage.path()).unwrap();
        assert_eq!(second, edit.join("leftovers/final-2.mp4"));
        assert_eq!(fs::read_to_string(&first).unwrap(), "half a render");
        assert!(!edit.join(FINAL_CUT).exists());
        assert_eq!(kept_cuts(footage.path()), [cut], "only finished renders are listed");

        // The cuts count on from their own numbers.
        fs::write(edit.join(FINAL_CUT), "finished again").unwrap();
        assert_eq!(keep_final_cut(footage.path()).unwrap(), edit.join("cuts/final-2.mp4"));
    }

    #[test]
    fn durations_read_like_a_player() {
        assert_eq!(format_duration(3.023), "0:03");
        assert_eq!(format_duration(62.5), "1:03");
        assert_eq!(format_duration(3725.0), "1:02:05");
        assert_eq!(format_duration(-1.0), "0:00");
    }
}
