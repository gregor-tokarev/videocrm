//! Contact sheets: a few frames of a stretch of a take or of the render, with the
//! waveform underneath, so the agent can see what is on screen and where the sound is.

use std::fs;
use std::path::Path;

use crate::error::{EditError, Result};
use crate::tool;

const COLUMNS: usize = 4;
const TILE_WIDTH: u32 = 320;
const TILE_HEIGHT: u32 = 180;
const WAVE_HEIGHT: u32 = 100;
pub const MAX_FRAMES: usize = 16;

pub struct Sheet {
    pub png: Vec<u8>,
    /// Time of each frame, in the file's own seconds.
    pub times: Vec<f64>,
}

/// `frames` evenly spaced frames of `file` between `start` and `end` (seconds).
pub fn sheet(file: &Path, start: f64, end: f64, frames: usize, has_audio: bool, scratch: &Path) -> Result<Sheet> {
    if end.partial_cmp(&start) != Some(std::cmp::Ordering::Greater) {
        return Err(EditError::invalid(format!("look: end {end:.2} must come after start {start:.2}")));
    }
    let frames = frames.clamp(1, MAX_FRAMES);
    let span = end - start;
    let rows = frames.div_ceil(COLUMNS);
    let columns = frames.min(COLUMNS);
    let times: Vec<f64> = (0..frames).map(|i| start + span * i as f64 / frames as f64).collect();
    let width = TILE_WIDTH * columns as u32;
    let label = |with_text: bool| {
        if with_text {
            format!(
                ",drawtext=text='%{{pts\\:hms\\:{start:.3}}}':x=6:y=6:fontsize=18:fontcolor=white:box=1:boxcolor=black@0.6:boxborderw=4"
            )
        } else {
            String::new()
        }
    };
    let png = scratch.join(format!("look-{}.png", std::process::id()));
    let attempt = |with_text: bool| -> Result<()> {
        let tiles = format!(
            "[0:v]fps={frames}/{span:.6}:start_time=0,scale={TILE_WIDTH}:{TILE_HEIGHT}:force_original_aspect_ratio=decrease,pad={TILE_WIDTH}:{TILE_HEIGHT}:(ow-iw)/2:(oh-ih)/2{},tile={columns}x{rows}[t]",
            label(with_text)
        );
        let graph = if has_audio {
            format!(
                "{tiles};[0:a:0]aformat=channel_layouts=mono,showwavespic=s={width}x{WAVE_HEIGHT}:colors=0x9fd3ff:scale=sqrt[w];[t][w]vstack[out]"
            )
        } else {
            tiles.replace("[t]", "[out]")
        };
        tool::run(
            &tool::ffmpeg()?,
            [
                "-nostdin".as_ref(),
                "-y".as_ref(),
                "-v".as_ref(),
                "error".as_ref(),
                "-ss".as_ref(),
                tool::secs(start).as_ref(),
                "-t".as_ref(),
                tool::secs(span).as_ref(),
                "-i".as_ref(),
                file.as_os_str(),
                "-filter_complex".as_ref(),
                graph.as_ref(),
                "-map".as_ref(),
                "[out]".as_ref(),
                "-frames:v".as_ref(),
                "1".as_ref(),
                png.as_os_str(),
            ],
        )?;
        Ok(())
    };
    // drawtext needs a font; without one, the sheet comes unlabelled.
    if attempt(true).is_err() {
        attempt(false)?;
    }
    let bytes = fs::read(&png)?;
    let _ = fs::remove_file(&png);
    Ok(Sheet { png: bytes, times })
}

/// One frame of `file` at each of `times` (seconds, inside `start`..`end`, at most 16), as
/// a labelled sheet without a waveform: the frames `shots` describes.
pub fn sheet_at(file: &Path, start: f64, end: f64, times: &[f64], scratch: &Path) -> Result<Vec<u8>> {
    const RATE: f64 = 5.0;
    let times: Vec<f64> = times.iter().copied().filter(|t| *t >= start && *t <= end).take(MAX_FRAMES).collect();
    if times.is_empty() {
        return Err(EditError::invalid("no frames to show"));
    }
    let picks: Vec<String> = times.iter().map(|t| format!("eq(n\\,{})", ((t - start) * RATE).floor() as u64)).collect();
    let columns = times.len().min(COLUMNS);
    let rows = times.len().div_ceil(COLUMNS);
    let png = scratch.join(format!("shots-{}.png", std::process::id()));
    let attempt = |with_text: bool| -> Result<()> {
        let label = if with_text {
            format!(
                ",drawtext=text='%{{pts\\:hms\\:{start:.3}}}':x=6:y=6:fontsize=18:fontcolor=white:box=1:boxcolor=black@0.6:boxborderw=4"
            )
        } else {
            String::new()
        };
        let filter = format!(
            "fps={RATE},select='{}',scale={TILE_WIDTH}:{TILE_HEIGHT}:force_original_aspect_ratio=decrease,pad={TILE_WIDTH}:{TILE_HEIGHT}:(ow-iw)/2:(oh-ih)/2{label},tile={columns}x{rows}",
            picks.join("+")
        );
        tool::run(
            &tool::ffmpeg()?,
            [
                "-nostdin".as_ref(),
                "-y".as_ref(),
                "-v".as_ref(),
                "error".as_ref(),
                "-ss".as_ref(),
                tool::secs(start).as_ref(),
                "-t".as_ref(),
                tool::secs(end - start).as_ref(),
                "-i".as_ref(),
                file.as_os_str(),
                "-map".as_ref(),
                "0:v:0".as_ref(),
                "-vf".as_ref(),
                filter.as_ref(),
                "-frames:v".as_ref(),
                "1".as_ref(),
                png.as_os_str(),
            ],
        )?;
        Ok(())
    };
    if attempt(true).is_err() {
        attempt(false)?;
    }
    let bytes = fs::read(&png)?;
    let _ = fs::remove_file(&png);
    Ok(bytes)
}

/// An image, scaled to fit 960x540, as PNG.
pub fn image(file: &Path, scratch: &Path) -> Result<Vec<u8>> {
    let png = scratch.join(format!("image-{}.png", std::process::id()));
    tool::run(
        &tool::ffmpeg()?,
        [
            "-nostdin".as_ref(),
            "-y".as_ref(),
            "-v".as_ref(),
            "error".as_ref(),
            "-i".as_ref(),
            file.as_os_str(),
            "-vf".as_ref(),
            "scale=960:540:force_original_aspect_ratio=decrease".as_ref(),
            "-frames:v".as_ref(),
            "1".as_ref(),
            png.as_os_str(),
        ],
    )?;
    let bytes = fs::read(&png)?;
    let _ = fs::remove_file(&png);
    Ok(bytes)
}
