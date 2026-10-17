//! The editing tools as the agent sees them: names, descriptions, input schemas and
//! handlers returning text (and images).

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::check;
use crate::error::{EditError, Result};
use crate::import;
use crate::look;
use crate::plan::{self, CutPlan, ResolvedCut, clock};
use crate::project::Project;
use crate::render::{self, GRADES, Quality, RenderOptions};
use crate::session::Session;
use crate::shots;
use crate::subtitles::Style;
use crate::transcribe::{self, Options};
use crate::transcript::TakeView;

/// Phrases per `read_transcript` page.
const PAGE: usize = 250;
/// A pause this long gets a note on what the picture does.
const NOTE_PAUSE: f64 = 3.0;

pub enum Content {
    Text(String),
    Png(Vec<u8>),
}

pub struct ToolResult {
    pub content: Vec<Content>,
    pub is_error: bool,
}

impl ToolResult {
    fn text(text: impl Into<String>) -> Self {
        Self { content: vec![Content::Text(text.into())], is_error: false }
    }

    fn error(text: impl Into<String>) -> Self {
        Self { content: vec![Content::Text(text.into())], is_error: true }
    }

    pub fn to_json(&self) -> Value {
        let content: Vec<Value> = self
            .content
            .iter()
            .map(|item| match item {
                Content::Text(text) => json!({"type": "text", "text": text}),
                Content::Png(bytes) => json!({
                    "type": "image",
                    "data": base64::engine::general_purpose::STANDARD.encode(bytes),
                    "mimeType": "image/png",
                }),
            })
            .collect();
        json!({"content": content, "isError": self.is_error})
    }
}

/// The tools of one footage folder.
pub struct Tools {
    footage_dir: PathBuf,
    session: Option<Session>,
}

impl Tools {
    pub fn new(footage_dir: &Path) -> Self {
        Self { footage_dir: footage_dir.to_owned(), session: None }
    }

    fn session(&mut self) -> Result<&mut Session> {
        if self.session.is_none() {
            self.session = Some(Session::new(Project::open(&self.footage_dir)?));
        }
        Ok(self.session.as_mut().expect("opened"))
    }

    /// Runs tool `name`; failures come back as error results the agent can read.
    pub fn call(&mut self, name: &str, arguments: Value, progress: &(dyn Fn(&str) + Sync)) -> ToolResult {
        let result = match name {
            "inventory" => self.inventory(),
            "add_footage" => self.add_footage(arguments, progress),
            "transcribe" => self.transcribe(arguments, progress),
            "read_transcript" => self.read_transcript(arguments),
            "phrase_words" => self.phrase_words(arguments),
            "silences" => self.silences(arguments),
            "look" => return self.look(arguments).unwrap_or_else(|error| ToolResult::error(error.to_string())),
            "shots" => return self.shots(arguments).unwrap_or_else(|error| ToolResult::error(error.to_string())),
            "plan_cut" => self.plan_cut(arguments),
            "render" => self.render(arguments, progress),
            "check" => self.check(arguments),
            _ => Err(EditError::invalid(format!("unknown tool {name}"))),
        };
        match result {
            Ok(text) => ToolResult::text(text),
            Err(error) => ToolResult::error(error.to_string()),
        }
    }

    fn inventory(&mut self) -> Result<String> {
        // Re-read the folder: takes may have been added.
        self.session = None;
        let session = self.session()?;
        let project = &session.project;
        let mut out = format!("Footage folder: {}\nTakes:\n", project.footage_dir.display());
        for take in &project.takes {
            let media = &take.media;
            if let Some(video) = media.video.as_ref().filter(|video| video.still) {
                let _ = writeln!(
                    out,
                    "- {}: {} (image, {}x{}) — show it as an overlay or hold it in a time segment",
                    take.id, take.file, video.width, video.height
                );
                continue;
            }
            let mut facts = vec![clock(media.duration)];
            match &media.video {
                Some(video) => {
                    facts.push(format!("{}x{}", video.width, video.height));
                    facts.push(format!(
                        "{} fps{}",
                        render::fps_label(video.rate.standard()),
                        if video.variable_rate() { " (variable)" } else { "" }
                    ));
                    if video.hdr {
                        facts.push("HDR".into());
                    }
                }
                None => facts.push("audio only".into()),
            }
            match media.audio.len() {
                0 => facts.push("no audio".into()),
                1 => {}
                n => facts.push(format!("{n} audio tracks")),
            }
            let transcribed = if !take.has_sound() {
                "nothing to transcribe"
            } else if transcribe::cached(project, take)?.is_some() {
                "transcribed"
            } else {
                "not transcribed"
            };
            let _ = writeln!(out, "- {}: {} ({}) — {transcribed}", take.id, take.file, facts.join(", "));
        }
        let edit = project.edit_dir();
        let existing: Vec<&str> = ["project.md", "cut.json", "preview.mp4", "final.mp4"]
            .into_iter()
            .filter(|name| edit.join(name).is_file())
            .collect();
        if !existing.is_empty() {
            let _ = writeln!(out, "In edit/: {}", existing.join(", "));
        }
        Ok(out)
    }

    fn add_footage(&mut self, arguments: Value, progress: &(dyn Fn(&str) + Sync)) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            path: Option<String>,
            url: Option<String>,
            name: Option<String>,
        }
        let args: Args = parse_args(arguments)?;
        let footage_dir = std::path::absolute(&self.footage_dir)?;
        // The takes there get their ids first, so the new file cannot take one of theirs.
        self.session = None;
        self.session()?;
        let file = match (&args.path, &args.url) {
            (Some(path), None) => {
                let path = Path::new(path.trim());
                let path = if path.is_absolute() { path.to_owned() } else { footage_dir.join(path) };
                import::copy_in(&footage_dir, &path, args.name.as_deref())?
            }
            (None, Some(url)) => import::download(&footage_dir, url, args.name.as_deref(), progress)?,
            _ => return Err(EditError::invalid("give either path (a file) or url (a link)")),
        };
        // Re-read the folder: the file is a take now.
        self.session = None;
        let session = self.session()?;
        let take = session.project.takes.iter().find(|take| take.file == file).ok_or_else(|| {
            EditError::invalid(format!("{file} is in the footage folder but could not be read as a take"))
        })?;
        let next = if take.still() {
            "Show it with an overlay, or hold it as a time segment."
        } else if take.has_sound() {
            "Transcribe it (transcribe with takes) to use its speech by phrase; its picture can also be an overlay."
        } else {
            "It has no sound: use it as an overlay or in time segments."
        };
        Ok(format!(
            "Added take {}: {} ({}). {next}\n",
            take.id,
            take.file,
            match &take.media.video {
                Some(video) if video.still => format!("image, {}x{}", video.width, video.height),
                Some(video) => format!("{}, {}x{}", clock(take.media.duration), video.width, video.height),
                None => format!("{}, audio only", clock(take.media.duration)),
            }
        ))
    }

    fn transcribe(&mut self, arguments: Value, progress: &(dyn Fn(&str) + Sync)) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            language: Option<String>,
            speakers: Option<u32>,
            takes: Option<Vec<String>>,
        }
        let args: Args = parse_args(arguments)?;
        let session = self.session()?;
        if let Some(takes) = &args.takes {
            for take in takes {
                session.take(take)?;
            }
        }
        let options = Options { language: args.language, num_speakers: args.speakers };
        let report = transcribe::transcribe_all(&session.project, args.takes.as_deref(), &options, progress);
        session.reset_views();
        let mut out = String::new();
        let mut failed = 0;
        for (take, result) in report {
            match result {
                Ok(fresh) => {
                    let view = session.load_view(&take)?;
                    let words = view.transcript.speech().count();
                    let _ = writeln!(
                        out,
                        "{take}: {} ({words} words, {} phrases, language {})",
                        if fresh { "transcribed" } else { "already transcribed" },
                        view.phrases.len(),
                        view.transcript.language.as_deref().unwrap_or("unknown")
                    );
                }
                Err(error) => {
                    failed += 1;
                    let _ = writeln!(out, "{take}: failed: {error}");
                }
            }
        }
        for take in session.project.takes.iter().filter(|take| !take.has_sound()) {
            let what = if take.still() { "an image" } else { "no sound" };
            let _ = writeln!(out, "{}: {what}, nothing to transcribe", take.id);
        }
        if failed > 0 {
            return Err(EditError::Transcription(out));
        }
        Ok(out)
    }

    fn read_transcript(&mut self, arguments: Value) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            take: Option<String>,
            from: Option<String>,
            count: Option<usize>,
        }
        let args: Args = parse_args(arguments)?;
        let session = self.session()?;
        let take = match (&args.take, &args.from) {
            (Some(take), _) => session.take(take)?.id.clone(),
            (None, Some(from)) => {
                from.chars().take_while(char::is_ascii_alphabetic).collect::<String>().to_ascii_uppercase()
            }
            (None, None) => first_with_sound(session)?,
        };
        let take = session.take(&take)?.clone();
        session.load_view(&take.id)?;
        if take.media.video.is_some() {
            session.load_activity(&take.id)?;
        }
        let view = session.view(&take.id).expect("loaded");
        let first = match &args.from {
            Some(from) => view.phrase_index(from)?,
            None => 0,
        };
        let count = args.count.unwrap_or(PAGE).clamp(1, 600);
        let last = (first + count).min(view.phrases.len());
        let mut out = String::new();
        if first == 0 {
            let words = view.transcript.speech().count();
            let fillers = view.transcript.words.iter().filter(|w| w.is_filler()).count();
            let _ = writeln!(
                out,
                "Take {} — {} ({}), {} words, {} fillers, {} phrases, language {}.",
                take.id,
                take.file,
                clock(take.media.duration),
                words,
                fillers,
                view.phrases.len(),
                view.transcript.language.as_deref().unwrap_or("unknown")
            );
            out.push_str(
                "id  start-end (s)  +pause before [sound tags] (picture during long pauses)  words  {hints}\n",
            );
        }
        for index in first..last {
            let note = pause_note(session, view, index);
            out.push_str(&view.line(index, note.as_deref()));
            out.push('\n');
        }
        if last < view.phrases.len() {
            let _ = writeln!(
                out,
                "… {} more phrases: read_transcript from={}",
                view.phrases.len() - last,
                view.phrases[last].id
            );
        } else {
            let tail = take.media.duration - view.phrases.last().map_or(0.0, |p| p.end);
            if tail >= NOTE_PAUSE {
                let _ = writeln!(out, "(then {tail:.1} s without speech to the end of the take)");
            }
        }
        Ok(out)
    }

    fn phrase_words(&mut self, arguments: Value) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            phrases: Vec<String>,
        }
        let args: Args = parse_args(arguments)?;
        let session = self.session()?;
        let mut out = String::new();
        for id in args.phrases.iter().take(20) {
            let take: String = id.chars().take_while(char::is_ascii_alphabetic).collect();
            let view = session.load_view(&take)?;
            let index = view.phrase_index(id)?;
            out.push_str(&view.word_lines(index));
        }
        Ok(out)
    }

    fn silences(&mut self, arguments: Value) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            take: Option<String>,
            min_seconds: Option<f64>,
        }
        let args: Args = parse_args(arguments)?;
        let min = args.min_seconds.unwrap_or(NOTE_PAUSE).max(0.5);
        let session = self.session()?;
        let takes: Vec<String> = match &args.take {
            Some(take) => vec![session.take(take)?.id.clone()],
            None => session.project.takes.iter().filter(|take| take.has_sound()).map(|take| take.id.clone()).collect(),
        };
        let mut out = String::new();
        for take in takes {
            let duration = session.take(&take)?.media.duration;
            let has_video = session.take(&take)?.media.video.is_some();
            session.load_view(&take)?;
            if has_video {
                session.load_activity(&take)?;
            }
            let view = session.view(&take).expect("loaded");
            let mut gaps: Vec<(f64, f64, String, String)> = Vec::new();
            let mut previous_end = 0.0;
            let mut previous_id = "start".to_owned();
            for phrase in &view.phrases {
                gaps.push((previous_end, phrase.start, previous_id.clone(), phrase.id.clone()));
                previous_end = phrase.end;
                previous_id = phrase.id.clone();
            }
            gaps.push((previous_end, duration, previous_id, "end".to_owned()));
            let _ = writeln!(out, "Take {take}: pauses of {min:.1} s or more");
            let mut listed = 0;
            for (start, end, before, after) in gaps.into_iter().filter(|(start, end, _, _)| end - start >= min) {
                listed += 1;
                let picture = session
                    .activity(&take)
                    .map(|activity| activity.summary(start, end).label())
                    .unwrap_or_else(|| "no picture".into());
                let _ = writeln!(
                    out,
                    "- {start:.2}-{end:.2} ({:.1} s) after {before}, before {after}: {picture}",
                    end - start
                );
            }
            if listed == 0 {
                out.push_str("- none\n");
            } else {
                out.push_str("Run shots on the ones that might show something, to keep only the payoff.\n");
            }
        }
        Ok(out)
    }

    fn look(&mut self, arguments: Value) -> Result<ToolResult> {
        #[derive(Deserialize)]
        struct Args {
            take: Option<String>,
            output: Option<String>,
            start: f64,
            end: f64,
            frames: Option<usize>,
        }
        let args: Args = parse_args(arguments)?;
        let session = self.session()?;
        let scratch = session.project.edit_path(".cache/look/x")?.with_file_name("");
        let (file, has_audio, what) = match (&args.output, &args.take) {
            (Some(output), _) => {
                let quality = quality(output)?;
                let file = session.project.edit_dir().join(quality.file_name());
                if !file.is_file() {
                    return Err(EditError::invalid(format!("{} is not rendered yet", quality.file_name())));
                }
                (file, true, quality.file_name().to_owned())
            }
            (None, take) => {
                let take = match take {
                    Some(take) => session.take(take)?,
                    None => &session.project.takes[0],
                };
                if take.media.video.is_none() {
                    return Err(EditError::invalid(format!("take {} has no picture", take.id)));
                }
                if take.still() {
                    let png = look::image(&take.path, &scratch)?;
                    let text = format!("take {} ({}), an image:", take.id, take.file);
                    return Ok(ToolResult { content: vec![Content::Text(text), Content::Png(png)], is_error: false });
                }
                (take.path.clone(), !take.media.audio.is_empty(), format!("take {}", take.id))
            }
        };
        let sheet = look::sheet(&file, args.start, args.end, args.frames.unwrap_or(8), has_audio, &scratch)?;
        let times: Vec<String> = sheet.times.iter().map(|t| format!("{t:.2}")).collect();
        let text = format!(
            "{what}, {:.2}-{:.2} s: frames at {} (left to right, top to bottom; waveform of the whole stretch below).",
            args.start,
            args.end,
            times.join(", ")
        );
        Ok(ToolResult { content: vec![Content::Text(text), Content::Png(sheet.png)], is_error: false })
    }

    fn shots(&mut self, arguments: Value) -> Result<ToolResult> {
        #[derive(Deserialize)]
        struct Args {
            take: Option<String>,
            start: f64,
            end: f64,
        }
        let args: Args = parse_args(arguments)?;
        let session = self.session()?;
        let take = match &args.take {
            Some(take) => session.take(take)?.clone(),
            None => session.project.takes[0].clone(),
        };
        if take.media.video.is_none() || take.still() {
            return Err(EditError::invalid(format!("take {} has no moving picture", take.id)));
        }
        let end = args.end.min(take.media.duration);
        let found = shots::shots(&take.path, args.start, end)?;
        let scratch = session.project.edit_path(".cache/look/x")?.with_file_name("");
        let times = shots::sheet_times(&found);
        let png = look::sheet_at(&take.path, args.start, end, &times, &scratch)?;
        let text = format!(
            "Take {}, {:.2}-{:.2} s: {} shots. Frames below: the middle of each shot (time labelled).\n{}\
             Keep only the shots that show the thing itself (most of the screen moving: the video or result playing); \
             opening pages, a small player, scrolling, loading and switching windows are preparation to cut.\n",
            take.id,
            args.start,
            end,
            found.len(),
            shots::describe(&found)
        );
        Ok(ToolResult { content: vec![Content::Text(text), Content::Png(png)], is_error: false })
    }

    fn plan_cut(&mut self, arguments: Value) -> Result<String> {
        let plan: CutPlan =
            serde_json::from_value(arguments).map_err(|error| EditError::invalid(format!("plan: {error}")))?;
        let session = self.session()?;
        let cut = plan::resolve(session, &plan)?;
        let footage: f64 = session.project.takes.iter().map(|take| take.media.duration).sum();
        let receipt = cut.receipt(footage);
        transcribe::write_atomic(&session.project.edit_path("cut.json")?, &serde_json::to_vec_pretty(&cut)?)?;
        transcribe::write_atomic(&session.project.edit_path("plan.md")?, receipt.as_bytes())?;
        Ok(format!("{receipt}Saved to edit/cut.json. Next: render a preview and check it.\n"))
    }

    fn current_cut(&mut self) -> Result<ResolvedCut> {
        let session = self.session()?;
        let path = session.project.edit_dir().join("cut.json");
        if !path.is_file() {
            return Err(EditError::invalid("no cut planned yet: call plan_cut first"));
        }
        Ok(serde_json::from_slice(&fs::read(path)?)?)
    }

    fn render(&mut self, arguments: Value, progress: &(dyn Fn(&str) + Sync)) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            quality: String,
            #[serde(default)]
            burn_subtitles: bool,
            subtitle_style: Option<String>,
            grade: Option<String>,
        }
        let args: Args = parse_args(arguments)?;
        let quality = quality(&args.quality)?;
        let style = match args.subtitle_style.as_deref().map(str::trim) {
            None | Some("") | Some("sentence") => Style::Sentence,
            Some("punchy") => Style::Punchy,
            Some(other) => return Err(EditError::invalid(format!("subtitle_style “{other}”: use sentence or punchy"))),
        };
        let cut = self.current_cut()?;
        let options = RenderOptions {
            quality,
            burn_subtitles: args.burn_subtitles,
            subtitle_style: style,
            grade: args.grade.unwrap_or_else(|| "none".into()),
            normalize_loudness: true,
        };
        let session = self.session()?;
        let record = render::render(session, &cut, &options, progress)?;
        let report = check::check(session, quality, &labels(&cut))?;
        Ok(format!("{}\n\nChecks:\n{report}", record.summary()))
    }

    fn check(&mut self, arguments: Value) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            output: Option<String>,
        }
        let args: Args = parse_args(arguments)?;
        let quality = quality(args.output.as_deref().unwrap_or("final"))?;
        let cut = self.current_cut()?;
        let session = self.session()?;
        check::check(session, quality, &labels(&cut))
    }
}

/// (segment index, description, is a time segment) for the checks.
fn labels(cut: &ResolvedCut) -> Vec<(usize, String, bool)> {
    cut.segments
        .iter()
        .enumerate()
        .map(|(index, segment)| {
            let time = cut.plan.segments.get(index).is_some_and(|spec| spec.from.is_none());
            let label =
                segment.label.clone().map(|label| format!("“{label}”")).unwrap_or_else(|| segment.source.clone());
            (index, label, time)
        })
        .collect()
}

fn first_with_sound(session: &Session) -> Result<String> {
    let take = session.project.takes.iter().find(|take| take.has_sound());
    take.map(|take| take.id.clone()).ok_or_else(|| EditError::invalid("no take has sound"))
}

fn pause_note(session: &Session, view: &TakeView, index: usize) -> Option<String> {
    let phrase = &view.phrases[index];
    if phrase.gap_before < NOTE_PAUSE {
        return None;
    }
    let activity = session.activity(&view.take)?;
    Some(activity.summary(phrase.start - phrase.gap_before, phrase.start).label())
}

fn quality(text: &str) -> Result<Quality> {
    match text.trim() {
        "preview" => Ok(Quality::Preview),
        "final" => Ok(Quality::Final),
        other => Err(EditError::invalid(format!("“{other}”: use preview or final"))),
    }
}

fn parse_args<T: for<'de> Deserialize<'de>>(arguments: Value) -> Result<T> {
    let arguments = if arguments.is_null() { json!({}) } else { arguments };
    serde_json::from_value(arguments).map_err(|error| EditError::invalid(format!("arguments: {error}")))
}

/// The tool list for MCP `tools/list`.
pub fn definitions() -> Value {
    let grades: Vec<&str> = GRADES.iter().map(|(name, _)| *name).collect();
    json!([
        {
            "name": "inventory",
            "description": "List the takes in the footage folder (id, file, length, picture, audio tracks, transcribed or not; images too) and what edit/ already holds. Call this first.",
            "inputSchema": {"type": "object", "properties": {}, "additionalProperties": false}
        },
        {
            "name": "add_footage",
            "description": "Bring a file into the edit: copy a video, audio file or image from a path (absolute, or relative to the footage folder, e.g. edit/downloads/clip.mp4), or download a link (a direct link to a file as it is; YouTube and other pages with yt-dlp, up to 1080p). It becomes a take with the next free id (existing ids never change) and the creator sees it with their footage. Optional name for the file.",
            "inputSchema": {"type": "object", "properties": {
                "path": {"type": "string"},
                "url": {"type": "string"},
                "name": {"type": "string", "description": "File name to give it in the footage folder (its extension stays)."}
            }, "additionalProperties": false}
        },
        {
            "name": "transcribe",
            "description": "Transcribe the takes with sound that have no transcript yet (or only `takes`): verbatim words (fillers kept), word times, speakers and sound tags. Cached per take, so calling again is free. Give language (ISO code such as en, ru) only when auto-detection might fail.",
            "inputSchema": {"type": "object", "properties": {
                "takes": {"type": "array", "items": {"type": "string"}, "description": "Only these take ids (default: all)."},
                "language": {"type": "string"},
                "speakers": {"type": "integer", "minimum": 1}
            }, "additionalProperties": false}
        },
        {
            "name": "read_transcript",
            "description": "Read a take as numbered phrases (a phrase is a run of words without a 0.5 s pause). Each line: id, start-end seconds, +pause before it with sound tags and, for pauses of 3 s or more, what the picture does meanwhile, then the words. ~word~ is a filler (cut automatically). {false start} marks a cut-off word; {said again in A9} marks words the speaker repeats later, usually a retake: keep the best complete attempt, normally the last. Long takes come in pages; continue with `from`.",
            "inputSchema": {"type": "object", "properties": {
                "take": {"type": "string", "description": "Take id (A, B, ...); default the first take."},
                "from": {"type": "string", "description": "Phrase id to start from, e.g. A251."},
                "count": {"type": "integer", "minimum": 1, "maximum": 600}
            }, "additionalProperties": false}
        },
        {
            "name": "phrase_words",
            "description": "The words of up to 20 phrases with their times, to pick starts_with / ends_with exactly.",
            "inputSchema": {"type": "object", "properties": {
                "phrases": {"type": "array", "items": {"type": "string"}, "minItems": 1}
            }, "required": ["phrases"], "additionalProperties": false}
        },
        {
            "name": "silences",
            "description": "List the pauses without speech of at least min_seconds (default 3) in a take, with what the picture does during each: still, changes a little, changes a lot, scene changes. A silent stretch where the picture changes may be a screen demo the viewer needs to see; look at it before deciding.",
            "inputSchema": {"type": "object", "properties": {
                "take": {"type": "string"},
                "min_seconds": {"type": "number", "minimum": 0.5}
            }, "additionalProperties": false}
        },
        {
            "name": "shots",
            "description": "Split a stretch of a take into shots: where the picture cuts and how much of the screen moves in each part (still; a small part moves, such as a cursor, a small player or a page; most of the screen moves, such as full-screen video). With a frame of each shot. Use it on every silent stretch you consider keeping, to keep only the shots that show the thing and cut the preparation around them (opening links, waiting, small previews, switching windows).",
            "inputSchema": {"type": "object", "properties": {
                "take": {"type": "string"},
                "start": {"type": "number", "minimum": 0},
                "end": {"type": "number", "minimum": 0}
            }, "required": ["start", "end"], "additionalProperties": false}
        },
        {
            "name": "look",
            "description": "See a stretch of a take (take + start/end in take seconds) or of the render (output = preview or final, start/end in output seconds) as a contact sheet: up to 16 frames with timestamps, the waveform underneath. An image take comes back as the image. Use it at decision points (what is on screen during a silence, whether a demo matters, how a join or an overlay looks), not to scan everything.",
            "inputSchema": {"type": "object", "properties": {
                "take": {"type": "string"},
                "output": {"type": "string", "enum": ["preview", "final"]},
                "start": {"type": "number", "minimum": 0},
                "end": {"type": "number", "minimum": 0},
                "frames": {"type": "integer", "minimum": 1, "maximum": 16}
            }, "required": ["start", "end"], "additionalProperties": false}
        },
        {
            "name": "plan_cut",
            "description": "Set the cut (replaces the previous plan). The output plays the segments in order. A speech segment keeps phrases `from`..`to` of one take, both included; `starts_with` / `ends_with` trim it to the exact first / last words to keep, written as in the transcript. Segments may come from any take (an insert from another video is a speech segment of its take). A time segment keeps `take` from `start` to `end` seconds as it is: use it for stretches without speech, such as a silent screen demo, never to trim speech; for an image take it holds the image from 0 to `end` seconds, silent. Inside speech segments fillers are cut (remove_fillers, default true) and pauses longer than max_pause (default 1.0 s) are cut down to tight_pause (0.45 s) inside a sentence or sentence_pause (0.7 s) after a full stop; shorter pauses stay, since cutting them shows as a jump cut and saves little. keep_pauses keeps a segment's pauses. Overlays show another picture (an image, or a take's picture from `start`) while the sound goes on, from the first kept word of `from` (or `starts_with`) to the last of `to` (or `ends_with`); those words must be in the cut. Do not compute times for speech: the tool cuts at word boundaries, pads and moves every cut to the quietest moment nearby. Returns the output timeline with warnings (retakes kept twice, sentences left unfinished, demos squeezed, length against target). Fix the warnings that matter and plan again, then render.",
            "inputSchema": {"type": "object", "properties": {
                "segments": {"type": "array", "minItems": 1, "items": {"type": "object", "properties": {
                    "from": {"type": "string", "description": "First phrase id, e.g. A12."},
                    "to": {"type": "string", "description": "Last phrase id (same take); default = from."},
                    "starts_with": {"type": "string", "description": "First words to keep in `from`."},
                    "ends_with": {"type": "string", "description": "Last words to keep in `to`."},
                    "take": {"type": "string", "description": "Time segment: take id."},
                    "start": {"type": "number", "description": "Time segment: start, take seconds."},
                    "end": {"type": "number", "description": "Time segment: end, take seconds."},
                    "keep_pauses": {"type": "boolean"},
                    "framing": {"type": "string", "enum": ["fill", "fit"], "description": "When `aspect` differs from the take: fill crops to the frame (talking head), fit shows the whole picture over a blurred copy (screen content). Default fill."},
                    "focus_x": {"type": "number", "minimum": 0, "maximum": 1, "description": "Fill: horizontal centre of the crop, 0 left to 1 right (default 0.5)."},
                    "focus_y": {"type": "number", "minimum": 0, "maximum": 1},
                    "zoom": {"type": "number", "minimum": 1, "maximum": 3, "description": "Zoom into the picture around focus_x / focus_y (1 = none)."},
                    "punch_in": {"type": "boolean", "description": "Punch-ins for this segment, over the plan's punch_in (false for a screen share, slides or anything that must not be cropped)."},
                    "label": {"type": "string", "description": "Short name of the beat, shown in the timeline."}
                }, "additionalProperties": false}},
                "overlays": {"type": "array", "items": {"type": "object", "properties": {
                    "take": {"type": "string", "description": "Take whose picture is shown: an image, or a video."},
                    "start": {"type": "number", "minimum": 0, "description": "Video take: where its picture starts, take seconds (default 0)."},
                    "from": {"type": "string", "description": "Phrase the overlay starts on, e.g. A120."},
                    "to": {"type": "string", "description": "Phrase it ends on (same take); default = from."},
                    "starts_with": {"type": "string", "description": "First words it covers in `from`."},
                    "ends_with": {"type": "string", "description": "Last words it covers in `to`."},
                    "framing": {"type": "string", "enum": ["fit", "fill"], "description": "fit (default) shows the whole picture over a blurred copy of itself: screenshots, pages, charts. fill crops it to the frame: B-roll footage."},
                    "focus_x": {"type": "number", "minimum": 0, "maximum": 1},
                    "focus_y": {"type": "number", "minimum": 0, "maximum": 1},
                    "zoom": {"type": "number", "minimum": 1, "maximum": 3, "description": "Zoom into the overlay picture (a detail of a screenshot), around focus_x / focus_y."},
                    "label": {"type": "string"}
                }, "required": ["take", "from"], "additionalProperties": false}},
                "aspect": {"type": "string", "description": "Output shape when it should differ from the footage: 9:16 for Shorts/Reels, 1:1, 4:5."},
                "punch_in": {"type": "boolean", "description": "Hide jump cuts in a single-camera talking head: at each cut inside a speech segment the picture alternates between full frame and a slight zoom-in. Segments can turn it off (or on) with their own punch_in."},
                "remove_fillers": {"type": "boolean"},
                "max_pause": {"type": "number", "minimum": 0.05, "maximum": 10},
                "tight_pause": {"type": "number", "minimum": 0.05, "maximum": 5},
                "sentence_pause": {"type": "number", "minimum": 0.05, "maximum": 5},
                "target_min_seconds": {"type": "number", "minimum": 0},
                "target_max_seconds": {"type": "number", "minimum": 0}
            }, "required": ["segments"], "additionalProperties": false}
        },
        {
            "name": "render",
            "description": "Render the current plan. quality preview: 720p, at most 30 fps, fast, for checking (edit/preview.mp4). quality final: source size and frame rate (edit/final.mp4). Clips are cached, so after a small change only what changed is encoded. Sound is normalised to -14 LUFS. Captions are always written next to the video as .srt; burn_subtitles also burns them in. Returns the render report and the checks.",
            "inputSchema": {"type": "object", "properties": {
                "quality": {"type": "string", "enum": ["preview", "final"]},
                "burn_subtitles": {"type": "boolean"},
                "subtitle_style": {"type": "string", "enum": ["sentence", "punchy"]},
                "grade": {"type": "string", "enum": grades}
            }, "required": ["quality"], "additionalProperties": false}
        },
        {
            "name": "check",
            "description": "Measure a render again: frame count and audio length against the plan, loudness, silences inside speech segments, and where the sound sits at the joins. render already runs this.",
            "inputSchema": {"type": "object", "properties": {
                "output": {"type": "string", "enum": ["preview", "final"]}
            }, "additionalProperties": false}
        }
    ])
}
