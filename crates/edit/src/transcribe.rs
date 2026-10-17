//! Transcription with ElevenLabs Scribe: verbatim words (fillers kept), word-level
//! times, speakers and sound tags. Cached per take in `edit/transcripts/`.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::audio;
use crate::error::{EditError, Result};
use crate::project::{Project, Take};
use crate::transcript::Transcript;

pub const API_KEY_VAR: &str = "ELEVENLABS_API_KEY";
/// Overrides the API origin (tests point it at a local server).
pub const API_URL_VAR: &str = "RUSHES_ELEVENLABS_URL";
const DEFAULT_API_URL: &str = "https://api.elevenlabs.io";
pub const MODEL: &str = "scribe_v2";
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(60 * 60);
/// Takes transcribed at once.
const PARALLEL: usize = 4;
/// Quieter than this (peak, dBFS) is silence: not worth paying to transcribe.
const SILENT_PEAK_DB: f64 = -60.0;

#[derive(Clone, Debug, Default)]
pub struct Options {
    /// ISO 639 code; `None` lets Scribe detect the language.
    pub language: Option<String>,
    pub num_speakers: Option<u32>,
}

/// What the cache knows about a transcript: it is stale when the source changed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Meta {
    model: String,
    source_size: u64,
    source_modified: u64,
    audio_track: usize,
    /// How the uploaded audio was decoded ([`audio::CLOCK_VERSION`]): word times are on
    /// that clock, and the renderer cuts on the current one.
    #[serde(default)]
    clock: String,
}

pub fn transcript_path(project: &Project, take: &Take) -> PathBuf {
    project.edit_dir().join("transcripts").join(format!("{}.json", take.stem()))
}

fn meta_path(project: &Project, take: &Take) -> PathBuf {
    project.edit_dir().join("transcripts").join(format!("{}.rushes.json", take.stem()))
}

/// The cached transcript of `take`, if there is a current one.
pub fn cached(project: &Project, take: &Take) -> Result<Option<Transcript>> {
    let path = transcript_path(project, take);
    if !path.is_file() {
        return Ok(None);
    }
    // A transcript without our note (written by video-use, or by hand) is trusted;
    // ours must match the source it was made from.
    if let Ok(bytes) = fs::read(meta_path(project, take)) {
        let meta: Meta = serde_json::from_slice(&bytes)?;
        let (size, modified) = source_stamp(&take.path)?;
        if meta.source_size != size || meta.source_modified != modified || meta.clock != audio::CLOCK_VERSION {
            return Ok(None);
        }
    }
    Ok(Some(Transcript::from_scribe(&fs::read(path)?)?))
}

/// Outcome per take: `Ok(true)` when it was transcribed now, `Ok(false)` when cached.
pub type Report = Vec<(String, Result<bool>)>;

/// Transcribes the takes with sound (all of them, or the ones in `only`) that have no
/// current transcript, a few at a time.
pub fn transcribe_all(
    project: &Project,
    only: Option<&[String]>,
    options: &Options,
    on_progress: &(dyn Fn(&str) + Sync),
) -> Report {
    let takes: Vec<&Take> = project
        .takes
        .iter()
        .filter(|take| take.has_sound())
        .filter(|take| only.is_none_or(|only| only.iter().any(|id| id.trim().eq_ignore_ascii_case(&take.id))))
        .collect();
    let workers = PARALLEL.min(takes.len());
    let queue = Mutex::new(takes);
    let results = Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                loop {
                    let Some(take) = queue.lock().unwrap().pop() else { break };
                    let result = transcribe(project, take, options, on_progress);
                    results.lock().unwrap().push((take.id.clone(), result));
                }
            });
        }
    });
    let mut results = results.into_inner().unwrap();
    results.sort_by(|a, b| a.0.len().cmp(&b.0.len()).then(a.0.cmp(&b.0)));
    results
}

fn transcribe(project: &Project, take: &Take, options: &Options, on_progress: &(dyn Fn(&str) + Sync)) -> Result<bool> {
    if cached(project, take)?.is_some() {
        return Ok(false);
    }
    if take.media.audio.is_empty() {
        return Err(EditError::invalid(format!("{} ({}) has no audio to transcribe", take.id, take.file)));
    }
    let key = std::env::var(API_KEY_VAR).ok().filter(|key| !key.trim().is_empty()).ok_or_else(|| {
        EditError::Transcription(format!(
            "{API_KEY_VAR} is not set; add an ElevenLabs API key in Rushes' AI edit settings"
        ))
    })?;
    on_progress(&format!("{}: extracting audio", take.id));
    let track = audio::speech_track(take)?;
    let wav = audio::speech_audio(project, take)?;
    let peak = audio::peak_db(&wav)?;
    if peak < SILENT_PEAK_DB {
        return Err(EditError::invalid(format!(
            "{} ({}) is silent on audio track {} (peak {peak:.0} dBFS): nothing to transcribe",
            take.id,
            take.file,
            track + 1
        )));
    }
    let flac = audio::flac_for_upload(&wav)?;
    on_progress(&format!("{}: transcribing", take.id));
    let response = upload(&key, &flac, options);
    let _ = fs::remove_file(&flac);
    let body = response?;
    // Parse before caching: an error body must not be kept as a transcript.
    Transcript::from_scribe(&body)
        .map_err(|error| EditError::Transcription(format!("unreadable response: {error}")))?;
    let path = transcript_path(project, take);
    fs::create_dir_all(path.parent().expect("transcripts dir"))?;
    write_atomic(&path, &body)?;
    let (source_size, source_modified) = source_stamp(&take.path)?;
    let meta = Meta {
        model: MODEL.to_owned(),
        source_size,
        source_modified,
        audio_track: track,
        clock: audio::CLOCK_VERSION.to_owned(),
    };
    write_atomic(&meta_path(project, take), &serde_json::to_vec_pretty(&meta)?)?;
    Ok(true)
}

fn upload(key: &str, audio: &Path, options: &Options) -> Result<Vec<u8>> {
    let origin = std::env::var(API_URL_VAR).unwrap_or_else(|_| DEFAULT_API_URL.to_owned());
    let url = format!("{}/v1/speech-to-text", origin.trim_end_matches('/'));
    let mut fields = vec![
        ("model_id", MODEL.to_owned()),
        ("diarize", "true".to_owned()),
        ("tag_audio_events", "true".to_owned()),
        ("timestamps_granularity", "word".to_owned()),
    ];
    if let Some(language) = options.language.as_deref().filter(|language| !language.trim().is_empty()) {
        fields.push(("language_code", language.trim().to_owned()));
    }
    if let Some(speakers) = options.num_speakers {
        fields.push(("num_speakers", speakers.to_string()));
    }
    let boundary = format!("rushes-{:x}", SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos());
    let mut body = Vec::new();
    for (name, value) in &fields {
        write!(body, "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n")?;
    }
    write!(
        body,
        "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"audio.flac\"\r\nContent-Type: audio/flac\r\n\r\n"
    )?;
    body.extend(fs::read(audio)?);
    write!(body, "\r\n--{boundary}--\r\n")?;

    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(UPLOAD_TIMEOUT))
        .http_status_as_error(false)
        .build()
        .new_agent();
    let mut response = agent
        .post(&url)
        .header("xi-api-key", key)
        .header("content-type", format!("multipart/form-data; boundary={boundary}"))
        .send(&body[..])
        .map_err(|error| EditError::Transcription(error.to_string()))?;
    let status = response.status();
    let bytes = response
        .body_mut()
        .with_config()
        .limit(256 * 1024 * 1024)
        .read_to_vec()
        .map_err(|error| EditError::Transcription(error.to_string()))?;
    if !status.is_success() {
        let text = String::from_utf8_lossy(&bytes);
        let text: String = text.chars().take(400).collect();
        return Err(EditError::Transcription(format!("ElevenLabs answered {status}: {text}")));
    }
    Ok(bytes)
}

fn source_stamp(path: &Path) -> Result<(u64, u64)> {
    let meta = fs::metadata(path)?;
    let modified = meta.modified()?.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    Ok((meta.len(), modified))
}

pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)?;
    Ok(())
}
