//! Rendering a resolved cut.
//!
//! Every span's picture is encoded on its own to an H.264 clip of an exact number of
//! frames; the clips are joined without re-encoding. The sound is cut from the take's
//! audio decoded once from the start, by sample index, exactly the samples of the
//! span's frames, and joined into one track that is loudness-normalised and encoded
//! once. Picture and sound therefore cannot drift apart at the joins (joining AAC clips
//! does: each carries encoder priming), and no span inherits ffmpeg's AAC seek offset.
//! Clips are cached by what they contain, so a revised cut re-encodes only the spans
//! that changed. Overlays replace the picture of the frames they cover (an image, or
//! another take's picture) and leave the sound alone.

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufWriter, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Instant, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::audio;
use crate::error::{EditError, Result};
use crate::plan::{Framing, Overlay, ResolvedCut, Span, clock};
use crate::project::Take;
use crate::rational::Rational;
use crate::session::Session;
use crate::subtitles::{self, Style};
use crate::tool;
use crate::transcribe::write_atomic;

pub const SAMPLE_RATE: u64 = 48_000;
/// Fade at every cut, so no cut clicks.
const FADE_SECONDS: f64 = 0.010;
/// Most a take's sound is raised or lowered to match the main take's speech.
const MAX_MATCH_DB: f64 = 20.0;
/// Speech a take needs in the cut for its level to be measured and matched.
const MIN_MATCH_SPEECH: f64 = 2.0;
pub const LOUDNESS_TARGET: f64 = -14.0;
const TRUE_PEAK: f64 = -1.5;
/// Gentle speech compression before normalising (2.5:1 above -20 dBFS): speech is peaky,
/// and without it -14 LUFS cannot be reached under the peak ceiling.
const COMPRESSOR: &str = "acompressor=threshold=0.1:ratio=2.5:attack=8:release=150:knee=4";
/// Highest true peak a render may have (YouTube and broadcast guidance: -1 dBTP).
pub const MAX_TRUE_PEAK: f64 = -1.0;
/// Peak ceiling after normalising (-2.5 dBFS), applied at 192 kHz so peaks between
/// samples are caught too; AAC encoding adds up to about a decibel.
const LIMIT: f64 = 0.75;

/// HDR (PQ/HLG) to SDR Rec.709.
const TONEMAP: &str = "zscale=t=linear:npl=100,format=gbrpf32le,zscale=p=bt709,tonemap=tonemap=hable:desat=0,zscale=t=bt709:m=bt709:r=tv,format=yuv420p";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Quality {
    /// 720p at up to 30 fps, fast to make: for checking the cut.
    Preview,
    /// Source size and frame rate, high quality.
    Final,
}

impl Quality {
    pub fn file_name(self) -> &'static str {
        match self {
            Self::Preview => "preview.mp4",
            Self::Final => "final.mp4",
        }
    }
}

/// Colour grade presets (the same names video-use uses).
pub const GRADES: [(&str, &str); 4] = [
    ("none", ""),
    ("subtle", "eq=contrast=1.03:saturation=0.98"),
    ("neutral_punch", "eq=contrast=1.06:brightness=0.0:saturation=1.0,curves=master='0/0 0.25/0.23 0.75/0.77 1/1'"),
    (
        "warm_cinematic",
        "eq=contrast=1.12:brightness=-0.02:saturation=0.88,colorbalance=rs=0.02:gs=0.0:bs=-0.03:rm=0.04:gm=0.0:bm=-0.02:rh=0.08:gh=0.02:bh=-0.05,curves=master='0/0 0.25/0.22 0.75/0.78 1/1'",
    ),
];

#[derive(Clone, Debug, PartialEq)]
pub struct RenderOptions {
    pub quality: Quality,
    pub burn_subtitles: bool,
    pub subtitle_style: Style,
    /// One of [`GRADES`].
    pub grade: String,
    pub normalize_loudness: bool,
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self {
            quality: Quality::Preview,
            burn_subtitles: false,
            subtitle_style: Style::Sentence,
            grade: "none".to_owned(),
            normalize_loudness: true,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Format {
    pub width: u32,
    pub height: u32,
    pub rate: Rational,
}

/// A span placed on the output timeline.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Placed {
    pub span: Span,
    pub out_start: f64,
    pub frames: u64,
    pub samples: u64,
    pub fade_in: bool,
    pub fade_out: bool,
}

impl Placed {
    pub fn out_end(&self) -> f64 {
        self.out_start + self.samples as f64 / SAMPLE_RATE as f64
    }
}

/// A run of output frames and where their picture comes from: the span's own take, or
/// an overlay's.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Picture {
    pub take: String,
    /// Take time of the first frame (0 for an image).
    pub start: f64,
    pub frames: u64,
    pub framing: Framing,
    /// Output time of the first frame.
    pub out_start: f64,
    /// The overlay shown (its index in the plan), if any.
    pub overlay: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Loudness {
    pub integrated: f64,
    pub true_peak: f64,
    pub range: f64,
}

/// What a render made; kept as `edit/.cache/render/last-<quality>.json` for checks.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RenderRecord {
    pub output: PathBuf,
    pub quality: Quality,
    pub format: Format,
    pub placed: Vec<Placed>,
    pub frames: u64,
    pub duration: f64,
    pub measured_before: Option<Loudness>,
    pub seconds_taken: f64,
    pub clips_encoded: usize,
    pub clips_reused: usize,
    pub subtitles: Option<PathBuf>,
    /// File names of the cached clips this render joined.
    #[serde(default)]
    pub clips: Vec<String>,
    /// Where the picture of each run of frames comes from.
    #[serde(default)]
    pub pictures: Vec<Picture>,
    /// Gain in dB that brought each take's speech to the main take's level, where it
    /// differed by a decibel or more.
    #[serde(default)]
    pub gains: BTreeMap<String, f64>,
}

impl RenderRecord {
    pub fn summary(&self) -> String {
        let mut out = format!(
            "Rendered {} ({}x{} at {} fps, {} frames, {}) in {:.0} s: {} clips encoded, {} reused.",
            self.output.display(),
            self.format.width,
            self.format.height,
            fps_label(self.format.rate),
            self.frames,
            clock(self.duration),
            self.seconds_taken,
            self.clips_encoded,
            self.clips_reused
        );
        if let Some(loudness) = &self.measured_before {
            out.push_str(&format!(
                " Sound lightly compressed ({:.1} LUFS, peak {:.1} dBTP) and normalised to {LOUDNESS_TARGET} LUFS.",
                loudness.integrated, loudness.true_peak
            ));
        }
        if !self.gains.is_empty() {
            let gains: Vec<String> = self.gains.iter().map(|(take, gain)| format!("{take} {gain:+.1} dB")).collect();
            out.push_str(&format!(" Speech levels matched to the main take: {}.", gains.join(", ")));
        }
        if let Some(subtitles) = &self.subtitles {
            out.push_str(&format!(" Captions: {}.", subtitles.display()));
        }
        out
    }
}

pub fn fps_label(rate: Rational) -> String {
    let value = rate.as_f64();
    if rate.den == 1 { rate.num.to_string() } else { format!("{value:.3}") }
}

pub fn record_path(session: &Session, quality: Quality) -> Result<PathBuf> {
    session.project.edit_path(&format!(
        ".cache/render/last-{}.json",
        match quality {
            Quality::Preview => "preview",
            Quality::Final => "final",
        }
    ))
}

pub fn last_record(session: &Session, quality: Quality) -> Result<Option<RenderRecord>> {
    let path = record_path(session, quality)?;
    if !path.is_file() {
        return Ok(None);
    }
    Ok(Some(serde_json::from_slice(&fs::read(path)?)?))
}

/// The output size and rate: those of the footage the cut shows longest (not an insert
/// from another video), 720p and at most 30 fps for previews.
pub fn output_format(session: &Session, cut: &ResolvedCut, quality: Quality) -> Result<Format> {
    let mut shown: Vec<(&str, f64)> = Vec::new();
    for span in &cut.spans {
        let take = session.take(&span.take)?;
        if take.media.video.is_some() && !take.still() {
            match shown.iter_mut().find(|(id, _)| *id == span.take) {
                Some(entry) => entry.1 += span.duration(),
                None => shown.push((&span.take, span.duration())),
            }
        }
    }
    let main = shown.iter().fold(None, |best: Option<&(&str, f64)>, entry| match best {
        Some(best) if best.1 >= entry.1 => Some(best),
        _ => Some(entry),
    });
    let video = match main {
        Some((take, _)) => session.take(take)?.media.video.clone(),
        // Only images on screen (sound with a picture held over it): 30 fps.
        None => cut
            .spans
            .iter()
            .map(|span| &span.take)
            .chain(cut.overlays.iter().map(|overlay| &overlay.take))
            .find_map(|take| session.take(take).ok()?.media.video.clone())
            .map(|video| crate::probe::VideoInfo { rate: Rational::new(30, 1), r_rate: Rational::new(30, 1), ..video }),
    };
    let video = video.ok_or_else(|| EditError::invalid("none of the takes in the cut has a picture"))?;
    let rate = video.rate.standard();
    let (mut width, mut height) = (video.width, video.height);
    // Another shape: as tall (or wide) as the source's long side allows, up to 1080x1920.
    if let Some(aspect) = &cut.plan.aspect {
        let ratio = crate::plan::aspect_ratio(aspect)?;
        let long = width.max(height).min(1920) as f64;
        (width, height) = if ratio < 1.0 {
            ((long * ratio).round() as u32, long as u32)
        } else {
            (long as u32, (long / ratio).round() as u32)
        };
    }
    let max_height = match quality {
        Quality::Preview => 720,
        Quality::Final => 2160,
    };
    let short_side = width.min(height);
    if short_side > max_height {
        let scale = f64::from(max_height) / f64::from(short_side);
        width = even((f64::from(width) * scale).round() as u32);
        height = even((f64::from(height) * scale).round() as u32);
    }
    let rate = match quality {
        Quality::Preview if rate.as_f64() > 31.0 => Rational::new(rate.num, rate.den * 2).reduced(),
        _ => rate,
    };
    Ok(Format { width: even(width), height: even(height), rate })
}

fn even(value: u32) -> u32 {
    value.max(2) & !1
}

/// Places the spans on a frame grid; audio gets exactly the samples of its frames.
pub fn place(spans: &[Span], rate: Rational) -> Vec<Placed> {
    let frames_at = |seconds: f64| (seconds * rate.num as f64 / rate.den as f64).round() as u64;
    let samples_at = |frames: u64| {
        ((frames as u128 * SAMPLE_RATE as u128 * rate.den as u128 + rate.num as u128 / 2) / rate.num as u128) as u64
    };
    let mut placed = Vec::new();
    let (mut exact, mut frames_done) = (0.0, 0u64);
    for span in spans {
        exact += span.duration();
        let frames_end = frames_at(exact);
        let frames = frames_end.saturating_sub(frames_done);
        if frames == 0 {
            continue;
        }
        let samples = samples_at(frames_end) - samples_at(frames_done);
        placed.push(Placed {
            span: span.clone(),
            out_start: samples_at(frames_done) as f64 / SAMPLE_RATE as f64,
            frames,
            samples,
            fade_in: true,
            fade_out: true,
        });
        frames_done = frames_end;
    }
    // No fade where a span continues the previous one in the source.
    for index in 1..placed.len() {
        let (previous, current) = (&placed[index - 1].span, &placed[index].span);
        if previous.take == current.take && (current.start - previous.end).abs() < 0.002 {
            placed[index - 1].fade_out = false;
            placed[index].fade_in = false;
        }
    }
    placed
}

/// The pictures of the placed spans, with the overlays cut in on the same frame grid.
pub fn pictures(placed: &[Placed], overlays: &[Overlay], rate: Rational) -> Vec<Picture> {
    let frame_time = |frame: u64| frame as f64 * rate.den as f64 / rate.num as f64;
    let frame_at = |seconds: f64| (seconds.max(0.0) * rate.num as f64 / rate.den as f64).round() as u64;
    let total: u64 = placed.iter().map(|p| p.frames).sum();
    let covers: Vec<(u64, u64, &Overlay)> = overlays
        .iter()
        .map(|overlay| (frame_at(overlay.out_start).min(total), frame_at(overlay.out_end).min(total), overlay))
        .filter(|(first, end, _)| end > first)
        .collect();
    let mut out: Vec<Picture> = Vec::new();
    let mut first = 0u64;
    for item in placed {
        let end = first + item.frames;
        let mut cursor = first;
        while cursor < end {
            let (stop, picture) = match covers.iter().find(|(from, to, _)| *from <= cursor && cursor < *to) {
                Some((from, to, overlay)) => {
                    let stop = (*to).min(end);
                    let picture = Picture {
                        take: overlay.take.clone(),
                        start: overlay.source_start + frame_time(cursor - from),
                        frames: stop - cursor,
                        framing: overlay.framing,
                        out_start: frame_time(cursor),
                        overlay: Some(overlay.index),
                    };
                    (stop, picture)
                }
                None => {
                    let next = covers.iter().map(|(from, _, _)| *from).filter(|from| *from > cursor).min();
                    let stop = next.unwrap_or(end).min(end);
                    let picture = Picture {
                        take: item.span.take.clone(),
                        start: item.span.start + frame_time(cursor - first),
                        frames: stop - cursor,
                        framing: item.span.framing,
                        out_start: frame_time(cursor),
                        overlay: None,
                    };
                    (stop, picture)
                }
            };
            // An overlay running on across a join of the spans under it stays one clip.
            match out.last_mut() {
                Some(last) if picture.overlay.is_some() && last.overlay == picture.overlay => {
                    last.frames += picture.frames;
                }
                _ => out.push(picture),
            }
            cursor = stop;
        }
        first = end;
    }
    out
}

struct Encoder {
    final_args: Vec<String>,
    preview_args: Vec<String>,
}

fn encoder(ffmpeg: &Path) -> &'static Encoder {
    static ENCODER: OnceLock<Encoder> = OnceLock::new();
    ENCODER.get_or_init(|| {
        let list = tool::run(ffmpeg, ["-hide_banner", "-encoders"])
            .map(|out| String::from_utf8_lossy(&out).into_owned())
            .unwrap_or_default();
        let strings = |args: &[&str]| args.iter().map(|arg| (*arg).to_owned()).collect();
        if list.contains(" libx264 ") {
            Encoder {
                final_args: strings(&["-c:v", "libx264", "-preset", "veryfast", "-crf", "18", "-profile:v", "high"]),
                preview_args: strings(&["-c:v", "libx264", "-preset", "ultrafast", "-crf", "28"]),
            }
        } else if list.contains(" h264_videotoolbox ") {
            Encoder {
                final_args: strings(&["-c:v", "h264_videotoolbox", "-q:v", "65"]),
                preview_args: strings(&["-c:v", "h264_videotoolbox", "-q:v", "45"]),
            }
        } else {
            Encoder {
                final_args: strings(&["-c:v", "h264", "-b:v", "12M"]),
                preview_args: strings(&["-c:v", "h264", "-b:v", "3M"]),
            }
        }
    })
}

struct Job<'a> {
    picture: &'a Picture,
    take: Take,
    video_filter: String,
    clip: PathBuf,
    rate: Rational,
}

/// Renders `cut` to `edit/preview.mp4` or `edit/final.mp4`.
pub fn render(
    session: &mut Session,
    cut: &ResolvedCut,
    options: &RenderOptions,
    on_progress: &(dyn Fn(&str) + Sync),
) -> Result<RenderRecord> {
    let started = Instant::now();
    let ffmpeg = tool::ffmpeg()?;
    let format = output_format(session, cut, options.quality)?;
    let placed = place(&cut.spans, format.rate);
    if placed.is_empty() {
        return Err(EditError::invalid("the cut is empty"));
    }
    let grade = GRADES
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(options.grade.trim()))
        .map(|(_, filter)| *filter)
        .ok_or_else(|| {
            let names: Vec<&str> = GRADES.iter().map(|(name, _)| *name).collect();
            EditError::invalid(format!("unknown grade “{}”; use one of {}", options.grade, names.join(", ")))
        })?;
    let cache = session.project.edit_path(".cache/render/x")?.with_file_name("");
    let output = session.project.edit_path(options.quality.file_name())?;

    // Captions in output time: always written next to the render, burnt in on request.
    for take in placed.iter().map(|p| p.span.take.clone()).collect::<std::collections::BTreeSet<_>>() {
        let _ = session.load_view(&take);
    }
    let words = subtitles::output_words(session, &placed);
    let cues = subtitles::cues(&words, options.subtitle_style);
    let srt_path = output.with_extension("srt");
    let subtitles = if cues.is_empty() {
        None
    } else {
        write_atomic(&srt_path, subtitles::srt(&cues).as_bytes())?;
        Some(srt_path)
    };
    let ass = if options.burn_subtitles && !cues.is_empty() {
        let text = subtitles::ass(&cues, options.subtitle_style, format.width, format.height);
        let name = format!("subs-{}.ass", short_hash(text.as_bytes()));
        write_atomic(&cache.join(&name), text.as_bytes())?;
        Some(name)
    } else {
        None
    };

    let encoder = encoder(&ffmpeg);
    let encoder_args = match options.quality {
        Quality::Preview => &encoder.preview_args,
        Quality::Final => &encoder.final_args,
    };
    let pictures = pictures(&placed, &cut.overlays, format.rate);
    let mut jobs = Vec::new();
    for picture in &pictures {
        let take = session.take(&picture.take)?.clone();
        let video_filter = video_filter(&take, &format, grade, ass.as_deref(), picture);
        let stamp = fs::metadata(&take.path)
            .and_then(|meta| meta.modified())
            .map(|time| time.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs())
            .unwrap_or(0);
        let key = short_hash(
            format!(
                "{}|{}|{:.6}|{}|{}|{}|{}",
                take.path.display(),
                stamp,
                picture.start,
                picture.frames,
                format.rate,
                video_filter,
                encoder_args.join(" ")
            )
            .as_bytes(),
        );
        let clip = cache.join(format!("{key}.mp4"));
        jobs.push(Job { picture, take, video_filter, clip, rate: format.rate });
    }

    let todo: Vec<&Job> = jobs.iter().filter(|job| !job.clip.is_file()).collect();
    let reused = jobs.len() - todo.len();
    let workers = std::thread::available_parallelism().map_or(4, |n| n.get()).div_ceil(3).clamp(1, 6);
    let threads = (std::thread::available_parallelism().map_or(4, |n| n.get()) / workers).max(1);
    let next = AtomicUsize::new(0);
    let done = AtomicUsize::new(0);
    let failed = AtomicBool::new(false);
    let error: Mutex<Option<EditError>> = Mutex::new(None);
    std::thread::scope(|scope| {
        for _ in 0..workers.min(todo.len().max(1)) {
            scope.spawn(|| {
                loop {
                    if failed.load(Ordering::Relaxed) {
                        break;
                    }
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    let Some(job) = todo.get(index) else { break };
                    if let Err(err) = encode_clip(&ffmpeg, job, encoder_args, threads, &cache) {
                        failed.store(true, Ordering::Relaxed);
                        error.lock().unwrap().get_or_insert(err);
                        break;
                    }
                    let finished = done.fetch_add(1, Ordering::Relaxed) + 1;
                    if finished.is_multiple_of(10) || finished == todo.len() {
                        on_progress(&format!("encoded {finished} of {} clips", todo.len()));
                    }
                }
            });
        }
    });
    if let Some(error) = error.into_inner().unwrap() {
        return Err(error);
    }

    // Join: video clips by stream copy; sound cut from the takes by sample index.
    let list = cache.join(format!("concat-{}.txt", short_hash(output.as_os_str().as_encoded_bytes())));
    let mut text = String::from("ffconcat version 1.0\n");
    for job in &jobs {
        text.push_str(&format!("file '{}'\n", job.clip.file_name().expect("file name").to_string_lossy()));
    }
    fs::write(&list, text)?;
    on_progress("cutting the sound");
    let wav = cache.join(format!("audio-{}.wav", short_hash(output.as_os_str().as_encoded_bytes())));
    let gains = speech_gains(session, &placed)?;
    assemble_audio(session, &placed, &gains, &wav)?;

    let total_samples: u64 = placed.iter().map(|p| p.samples).sum();
    let measured = if options.normalize_loudness { measure_loudness(&ffmpeg, &wav).ok() } else { None };
    // Loudness to target, a limiter for the peaks the gain pushes up (oversampled for the
    // final, to catch peaks between samples), then exactly the samples of the picture.
    let limiter_rate = match options.quality {
        Quality::Preview => SAMPLE_RATE,
        Quality::Final => 4 * SAMPLE_RATE,
    };
    let audio_filter = match &measured {
        Some(loudness) => format!(
            "{COMPRESSOR},loudnorm=I={LOUDNESS_TARGET}:TP={TRUE_PEAK}:LRA=11:measured_I={:.2}:measured_TP={:.2}:measured_LRA={:.2}:measured_thresh={:.2}:offset={:.2}:linear=true,aresample={limiter_rate},alimiter=limit={LIMIT}:attack=5:release=50:level=false:latency=1,aresample={SAMPLE_RATE},apad,atrim=end_sample={total_samples}",
            loudness.0.integrated, loudness.0.true_peak, loudness.0.range, loudness.1, loudness.2
        ),
        None => format!("aresample={SAMPLE_RATE},apad,atrim=end_sample={total_samples}"),
    };
    on_progress("joining clips");
    let tmp = output.with_extension("tmp.mp4");
    mux(&ffmpeg, &list, &wav, &audio_filter, &tmp)?;
    // AAC encoding can push peaks back up past the limiter: measure the result and,
    // when it is over, encode the sound again a little quieter.
    if let Some(peak) = true_peak(&ffmpeg, &tmp)
        && peak > MAX_TRUE_PEAK
    {
        let quieter = format!("{audio_filter},volume={:.2}dB", MAX_TRUE_PEAK - 0.3 - peak);
        mux(&ffmpeg, &list, &wav, &quieter, &tmp)?;
    }
    fs::rename(&tmp, &output)?;
    let _ = fs::remove_file(&wav);

    let frames = placed.iter().map(|p| p.frames).sum();
    let duration = placed.last().map_or(0.0, Placed::out_end);
    let record = RenderRecord {
        output: output.clone(),
        quality: options.quality,
        format,
        placed: placed.clone(),
        frames,
        duration,
        measured_before: measured.map(|m| m.0),
        seconds_taken: started.elapsed().as_secs_f64(),
        clips_encoded: todo.len(),
        clips_reused: reused,
        subtitles,
        clips: jobs
            .iter()
            .filter_map(|job| job.clip.file_name().map(|name| name.to_string_lossy().into_owned()))
            .collect(),
        pictures: pictures.clone(),
        gains,
    };
    write_atomic(&record_path(session, options.quality)?, &serde_json::to_vec_pretty(&record)?)?;
    write_edl(session, cut, &record)?;
    prune_clips(session, &cache);
    Ok(record)
}

/// Deletes cached clips neither the last preview nor the last final render uses: a
/// long edit's clips take as much space as the video itself.
fn prune_clips(session: &Session, cache: &Path) {
    let mut keep = std::collections::HashSet::new();
    for quality in [Quality::Preview, Quality::Final] {
        if let Ok(Some(record)) = last_record(session, quality) {
            keep.extend(record.clips);
        }
    }
    let Ok(entries) = fs::read_dir(cache) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let clip = name.ends_with(".mp4") || name.ends_with(".pcm");
        if clip && !keep.contains(&name) {
            let _ = fs::remove_file(entry.path());
        }
    }
}

fn video_filter(take: &Take, format: &Format, grade: &str, ass: Option<&str>, picture: &Picture) -> String {
    let mut parts = Vec::new();
    if take.media.video.as_ref().is_some_and(|video| video.hdr) {
        parts.push(TONEMAP.to_owned());
    }
    if take.still() {
        // An image looped at the output rate.
        parts.push(format!("fps=fps={},setpts=PTS-STARTPTS", format.rate));
    } else {
        // The clip is decoded from a little before the span (see `seek_lead`), so the
        // frame nearest the span's start can be picked even when it comes just before it.
        parts.push(format!(
            "fps=fps={}:start_time={:.6}:round=near,setpts=PTS-STARTPTS",
            format.rate,
            seek_lead(picture.start)
        ));
    }
    let (w, h) = (format.width, format.height);
    let same_shape = take.media.video.as_ref().is_none_or(|video| {
        (f64::from(video.width) / f64::from(video.height) - f64::from(w) / f64::from(h)).abs() < 0.01
    });
    let framing = picture.framing;
    let zoom = framing.zoom.clamp(1.0, 3.0);
    parts.push(if same_shape && zoom == 1.0 {
        format!("scale={w}:{h}:force_original_aspect_ratio=decrease:flags=bicubic,pad={w}:{h}:(ow-iw)/2:(oh-ih)/2,setsar=1")
    } else if framing.fill || same_shape {
        // Cover the frame (zoomed in for a punch-in), then crop around the focus.
        let (zw, zh) = (even((f64::from(w) * zoom).round() as u32), even((f64::from(h) * zoom).round() as u32));
        format!(
            "scale={zw}:{zh}:force_original_aspect_ratio=increase:flags=bicubic,crop={w}:{h}:(iw-{w})*{fx:.3}:(ih-{h})*{fy:.3},setsar=1",
            fx = framing.focus_x,
            fy = framing.focus_y
        )
    } else {
        // The whole picture (zoomed: the part around the focus) over a darkened, blurred
        // copy that fills the frame.
        let detail = if zoom > 1.0 {
            format!(
                "crop=iw/{zoom:.4}:ih/{zoom:.4}:(iw-ow)*{fx:.3}:(ih-oh)*{fy:.3},",
                fx = framing.focus_x,
                fy = framing.focus_y
            )
        } else {
            String::new()
        };
        format!(
            "{detail}split[bg][fg];[bg]scale={w}:{h}:force_original_aspect_ratio=increase,crop={w}:{h},boxblur=20:2,eq=brightness=-0.12[bg];\
             [fg]scale={w}:{h}:force_original_aspect_ratio=decrease:flags=bicubic[fg];[bg][fg]overlay=(W-w)/2:(H-h)/2,setsar=1"
        )
    });
    // Enough frames even when the span runs to the very end of the take.
    parts.push("tpad=stop_mode=clone:stop_duration=1".to_owned());
    parts.push("format=yuv420p".to_owned());
    if !grade.is_empty() {
        parts.push(grade.to_owned());
    }
    if let Some(ass) = ass {
        // Captions are timed on the output: shift this clip to its place, burn, shift back.
        parts.push(format!(
            "setpts=PTS-STARTPTS+{:.6}/TB,subtitles=filename={ass},setpts=PTS-STARTPTS",
            picture.out_start
        ));
    }
    parts.join(",")
}

/// How far before a span its picture is decoded from.
fn seek_lead(start: f64) -> f64 {
    start.clamp(0.0, 0.25)
}

fn encode_clip(ffmpeg: &Path, job: &Job, encoder_args: &[String], threads: usize, cache: &Path) -> Result<()> {
    let picture = job.picture;
    let clip_tmp = job.clip.with_extension("tmp.mp4");
    let mut args: Vec<String> = vec!["-nostdin".into(), "-y".into(), "-v".into(), "error".into()];
    if job.take.still() {
        args.extend(["-loop".into(), "1".into(), "-framerate".into(), job.rate.to_string()]);
    } else {
        args.extend(["-ss".into(), tool::secs(picture.start - seek_lead(picture.start))]);
    }
    args.extend([
        "-i".into(),
        job.take.path.to_string_lossy().into_owned(),
        "-map".into(),
        "0:v:0".into(),
        "-vf".into(),
        job.video_filter.clone(),
        "-frames:v".into(),
        picture.frames.to_string(),
        "-an".into(),
    ]);
    args.extend(encoder_args.iter().cloned());
    args.extend(["-pix_fmt".into(), "yuv420p".into(), "-threads".into(), threads.to_string()]);
    args.push(clip_tmp.to_string_lossy().into_owned());
    // Relative paths (the burnt-in captions file) are inside the cache folder.
    let (status, _, stderr) = run_in(ffmpeg, &args, cache)?;
    if !status {
        return Err(EditError::ToolFailed { program: "ffmpeg".into(), detail: tail(&stderr) });
    }
    fs::rename(&clip_tmp, &job.clip)?;
    Ok(())
}

fn run_in(program: &Path, args: &[String], dir: &Path) -> Result<(bool, Vec<u8>, String)> {
    let output =
        std::process::Command::new(program).args(args).current_dir(dir).stdin(std::process::Stdio::null()).output()?;
    Ok((output.status.success(), output.stdout, String::from_utf8_lossy(&output.stderr).into_owned()))
}

fn tail(text: &str) -> String {
    let text = text.trim();
    let start = text.char_indices().rev().nth(1500).map_or(0, |(index, _)| index);
    text[start..].to_owned()
}

/// Joins the picture clips (stream copy) with the sound, encoded once.
fn mux(ffmpeg: &Path, list: &Path, wav: &Path, audio_filter: &str, out: &Path) -> Result<()> {
    tool::run(
        ffmpeg,
        [
            "-nostdin".as_ref(),
            "-y".as_ref(),
            "-v".as_ref(),
            "error".as_ref(),
            "-f".as_ref(),
            "concat".as_ref(),
            "-safe".as_ref(),
            "0".as_ref(),
            "-i".as_ref(),
            list.as_os_str(),
            "-i".as_ref(),
            wav.as_os_str(),
            "-map".as_ref(),
            "0:v:0".as_ref(),
            "-map".as_ref(),
            "1:a:0".as_ref(),
            "-c:v".as_ref(),
            "copy".as_ref(),
            "-af".as_ref(),
            audio_filter.as_ref(),
            "-c:a".as_ref(),
            "aac".as_ref(),
            "-b:a".as_ref(),
            "192k".as_ref(),
            "-movflags".as_ref(),
            "+faststart".as_ref(),
            out.as_os_str(),
        ],
    )?;
    Ok(())
}

/// True peak of a file's sound in dBTP, as `ebur128` measures it.
pub(crate) fn true_peak(ffmpeg: &Path, file: &Path) -> Option<f64> {
    let (_, stderr) = tool::run_with_stderr(
        ffmpeg,
        [
            "-nostdin".as_ref(),
            "-v".as_ref(),
            "info".as_ref(),
            "-i".as_ref(),
            file.as_os_str(),
            "-map".as_ref(),
            "0:a:0".as_ref(),
            "-af".as_ref(),
            "ebur128=peak=true".as_ref(),
            "-f".as_ref(),
            "null".as_ref(),
            "-".as_ref(),
        ],
    )
    .ok()?;
    let summary = stderr.rsplit("Summary:").next()?;
    summary
        .lines()
        .find_map(|line| line.trim().strip_prefix("Peak:"))
        .and_then(|rest| rest.split_whitespace().next()?.parse().ok())
}

/// Gain in dB per take that brings its speech to the level of the take with the most
/// speech in the cut, so an insert from another recording plays as loud as the rest.
/// Measured on the kept words (from the analysis envelope); takes without at least
/// [`MIN_MATCH_SPEECH`] of kept words (a silent demo, music) stay as they are.
fn speech_gains(session: &mut Session, placed: &[Placed]) -> Result<BTreeMap<String, f64>> {
    let takes: std::collections::BTreeSet<String> = placed.iter().map(|p| p.span.take.clone()).collect();
    let takes: Vec<String> = takes.into_iter().filter(|take| session.view(take).is_some()).collect();
    for take in &takes {
        session.load_envelope(take)?;
    }
    let hop = audio::Envelope::hop_seconds();
    let mut levels: Vec<(String, f64, f64)> = Vec::new();
    for take in takes {
        let (view, envelope) = (session.view(&take).expect("loaded"), session.envelope(&take).expect("loaded"));
        let (mut power, mut windows) = (0.0, 0usize);
        for item in placed.iter().filter(|p| p.span.take == take) {
            for word in view.transcript.words.iter().filter(|w| !w.event && !w.is_filler()) {
                let (from, to) = (word.start.max(item.span.start), word.end.min(item.span.end));
                if to <= from {
                    continue;
                }
                let last = ((to / hop) as usize).min(envelope.db.len());
                for db in &envelope.db[((from / hop) as usize).min(last)..last] {
                    power += 10f64.powf(f64::from(*db) / 10.0);
                    windows += 1;
                }
            }
        }
        let seconds = windows as f64 * hop;
        if seconds >= MIN_MATCH_SPEECH && power > 0.0 {
            levels.push((take, seconds, 10.0 * (power / windows as f64).log10()));
        }
    }
    let Some((_, _, reference)) = levels.iter().max_by(|a, b| a.1.total_cmp(&b.1)).cloned() else {
        return Ok(BTreeMap::new());
    };
    Ok(levels
        .into_iter()
        .map(|(take, _, level)| (take, (reference - level).clamp(-MAX_MATCH_DB, MAX_MATCH_DB)))
        .filter(|(_, gain)| gain.abs() >= 1.0)
        .collect())
}

/// Writes the sound of the placed spans as one 48 kHz stereo 32-bit float WAV: each
/// span's samples cut from its take's decoded audio, with its take's gain (in dB, see
/// [`speech_gains`]; float keeps the peaks a gain raises for the limiter) and a short
/// fade at every cut.
fn assemble_audio(session: &Session, placed: &[Placed], gains: &BTreeMap<String, f64>, wav: &Path) -> Result<()> {
    use std::io::{Read as _, Seek as _, SeekFrom};
    let total: u64 = placed.iter().map(|p| p.samples * 8).sum();
    let data_len =
        u32::try_from(total).map_err(|_| EditError::invalid("the cut is too long for one WAV file (over 3 hours)"))?;
    let mut out = BufWriter::new(fs::File::create(wav)?);
    out.write_all(b"RIFF")?;
    out.write_all(&(36 + data_len).to_le_bytes())?;
    out.write_all(b"WAVEfmt ")?;
    out.write_all(&16u32.to_le_bytes())?;
    out.write_all(&3u16.to_le_bytes())?; // IEEE float
    out.write_all(&2u16.to_le_bytes())?; // stereo
    out.write_all(&(SAMPLE_RATE as u32).to_le_bytes())?;
    out.write_all(&(SAMPLE_RATE as u32 * 8).to_le_bytes())?;
    out.write_all(&8u16.to_le_bytes())?;
    out.write_all(&32u16.to_le_bytes())?;
    out.write_all(b"data")?;
    out.write_all(&data_len.to_le_bytes())?;
    let mut sources: std::collections::HashMap<String, Option<fs::File>> = std::collections::HashMap::new();
    let fade = (FADE_SECONDS * SAMPLE_RATE as f64) as usize;
    for item in placed {
        if !sources.contains_key(&item.span.take) {
            let take = session.take(&item.span.take)?;
            let file = if take.media.audio.is_empty() {
                None
            } else {
                Some(fs::File::open(audio::render_audio(&session.project, take)?)?)
            };
            sources.insert(item.span.take.clone(), file);
        }
        let count = item.samples as usize;
        let mut bytes = vec![0u8; count * 4];
        if let Some(Some(file)) = sources.get_mut(&item.span.take) {
            let first = (item.span.start * SAMPLE_RATE as f64).round() as u64;
            file.seek(SeekFrom::Start(first * 4))?;
            let mut filled = 0;
            // Past the end of the take: silence.
            while filled < bytes.len() {
                let n = file.read(&mut bytes[filled..])?;
                if n == 0 {
                    break;
                }
                filled += n;
            }
        }
        let level = gains.get(&item.span.take).map_or(1.0, |db| 10f32.powf(*db as f32 / 20.0)) / 32768.0;
        let mut samples: Vec<f32> =
            bytes.as_chunks::<2>().0.iter().map(|pair| f32::from(i16::from_le_bytes(*pair)) * level).collect();
        let ramp = fade.min(count / 4);
        for frame in 0..ramp {
            let gain = (frame as f32 + 0.5) / ramp as f32;
            if item.fade_in {
                for channel in 0..2 {
                    samples[frame * 2 + channel] *= gain;
                }
            }
            if item.fade_out {
                for channel in 0..2 {
                    samples[(count - 1 - frame) * 2 + channel] *= gain;
                }
            }
        }
        for sample in samples {
            out.write_all(&sample.to_le_bytes())?;
        }
    }
    out.flush()?;
    Ok(())
}

/// First loudnorm pass: (loudness, threshold, target offset).
fn measure_loudness(ffmpeg: &Path, wav: &Path) -> Result<(Loudness, f64, f64)> {
    let (_, stderr) = tool::run_with_stderr(
        ffmpeg,
        [
            "-nostdin".as_ref(),
            "-v".as_ref(),
            "info".as_ref(),
            "-i".as_ref(),
            wav.as_os_str(),
            "-af".as_ref(),
            format!("{COMPRESSOR},loudnorm=I={LOUDNESS_TARGET}:TP={TRUE_PEAK}:LRA=11:print_format=json").as_ref(),
            "-f".as_ref(),
            "null".as_ref(),
            "-".as_ref(),
        ],
    )?;
    let json = stderr.rfind('{').and_then(|start| stderr[start..].find('}').map(|end| &stderr[start..=start + end]));
    let value: serde_json::Value =
        serde_json::from_str(json.ok_or_else(|| EditError::invalid("loudnorm printed no measurement"))?)?;
    let number = |key: &str| value[key].as_str().and_then(|text| text.parse::<f64>().ok()).filter(|v| v.is_finite());
    let missing = || EditError::invalid("loudnorm measurement incomplete (silent audio?)");
    Ok((
        Loudness {
            integrated: number("input_i").ok_or_else(missing)?,
            true_peak: number("input_tp").ok_or_else(missing)?,
            range: number("input_lra").ok_or_else(missing)?,
        },
        number("input_thresh").ok_or_else(missing)?,
        number("target_offset").ok_or_else(missing)?,
    ))
}

/// `edit/edl.json` in video-use's format: one range per rendered span.
fn write_edl(session: &Session, cut: &ResolvedCut, record: &RenderRecord) -> Result<()> {
    let mut sources = serde_json::Map::new();
    let mut ranges = Vec::new();
    for item in &record.placed {
        let take = session.take(&item.span.take)?;
        sources.insert(take.id.clone(), take.path.to_string_lossy().into_owned().into());
        let info = cut.segments.get(item.span.segment);
        ranges.push(serde_json::json!({
            "source": take.id,
            "start": item.span.start,
            "end": item.span.start + item.samples as f64 / SAMPLE_RATE as f64,
            "beat": info.and_then(|info| info.label.clone()).unwrap_or_else(|| format!("S{}", item.span.segment + 1)),
            "segment": item.span.segment + 1,
            "out_start": item.out_start,
        }));
    }
    let mut overlays = Vec::new();
    for overlay in &cut.overlays {
        let take = session.take(&overlay.take)?;
        sources.insert(take.id.clone(), take.path.to_string_lossy().into_owned().into());
        overlays.push(serde_json::json!({
            "source": take.id,
            "start": overlay.source_start,
            "out_start": overlay.out_start,
            "out_end": overlay.out_end,
            "label": overlay.label,
        }));
    }
    let edl = serde_json::json!({
        "version": 1,
        "sources": sources,
        "ranges": ranges,
        "overlays": overlays,
        "rendered": record.output,
        "total_duration_s": record.duration,
    });
    write_atomic(&session.project.edit_path("edl.json")?, &serde_json::to_vec_pretty(&edl)?)
}

fn short_hash(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().take(10).map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(start: f64, end: f64) -> Span {
        Span { take: "A".into(), start, end, segment: 0, framing: Default::default() }
    }

    #[test]
    fn spans_land_on_whole_frames_with_matching_samples() {
        let placed =
            place(&[span(0.0, 1.01), span(5.0, 5.333), span(9.0, 9.004), span(20.0, 21.0)], Rational::new(30000, 1001));
        // 1.01 s -> 30 frames; then 1.343 s total -> 40 frames; the 4 ms sliver adds no frame.
        assert_eq!(placed.iter().map(|p| p.frames).collect::<Vec<_>>(), [30, 10, 30]);
        let total_frames: u64 = placed.iter().map(|p| p.frames).sum();
        let total_samples: u64 = placed.iter().map(|p| p.samples).sum();
        assert_eq!(total_samples, (total_frames as u128 * 48_000 * 1001 / 30_000) as u64);
        for pair in placed.windows(2) {
            assert!((pair[0].out_end() - pair[1].out_start).abs() < 1e-9);
        }
    }

    #[test]
    fn overlays_replace_the_picture_of_their_frames_across_spans() {
        let rate = Rational::new(10, 1);
        let placed = place(&[span(0.0, 2.0), span(5.0, 7.0)], rate);
        let overlay = Overlay {
            index: 0,
            take: "B".into(),
            source_start: 30.0,
            out_start: 1.5,
            out_end: 2.6,
            framing: Framing::default(),
            label: None,
            source: String::new(),
            text: String::new(),
        };
        let pictures = pictures(&placed, &[overlay], rate);
        let summary: Vec<(&str, f64, u64)> =
            pictures.iter().map(|p| (p.take.as_str(), (p.start * 100.0).round() / 100.0, p.frames)).collect();
        // A 0-1.5, then B from 30 s over the join for 11 frames, then A again from 5.6.
        assert_eq!(summary, [("A", 0.0, 15), ("B", 30.0, 11), ("A", 5.6, 14)]);
        assert_eq!(pictures.iter().map(|p| p.frames).sum::<u64>(), 40);
        assert!((pictures[2].out_start - 2.6).abs() < 1e-9);
    }

    #[test]
    fn spans_that_continue_in_the_source_join_without_fades() {
        let placed = place(&[span(0.0, 1.0), span(1.0, 2.0), span(3.0, 4.0)], Rational::new(25, 1));
        assert_eq!(
            placed.iter().map(|p| (p.fade_in, p.fade_out)).collect::<Vec<_>>(),
            [(true, false), (false, true), (true, true)]
        );
    }
}
