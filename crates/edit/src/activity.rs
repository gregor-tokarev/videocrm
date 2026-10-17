//! How much the picture changes over time, from a take's keyframes: tells a silent
//! screen demo (the viewer watches something) from dead air (nothing happens).

use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::project::{Project, Take};
use crate::tool;

const WIDTH: usize = 64;
const HEIGHT: usize = 36;
/// Mean absolute difference (0-255) between keyframes above which the scene changed.
const SCENE_CHANGE: f32 = 22.0;

/// Keyframe times and their difference to the previous keyframe.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Activity {
    pub times: Vec<f64>,
    pub change: Vec<f32>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Summary {
    pub keyframes: usize,
    pub mean_change: f32,
    pub scene_changes: usize,
}

impl Summary {
    pub fn label(&self) -> String {
        let motion = if self.keyframes == 0 {
            "picture unknown"
        } else if self.mean_change < 1.5 {
            "picture still"
        } else if self.mean_change < 10.0 {
            "picture changes a little"
        } else {
            "picture changes a lot"
        };
        match self.scene_changes {
            0 => motion.to_owned(),
            1 => format!("{motion}, 1 scene change"),
            n => format!("{motion}, {n} scene changes"),
        }
    }

    /// Something worth seeing may happen: a lot of motion or a scene change. (A person
    /// on camera only changes the picture a little.)
    pub fn eventful(&self) -> bool {
        self.mean_change >= 10.0 || self.scene_changes > 0
    }
}

impl Activity {
    /// What happens in the picture between `from` and `to` (seconds).
    pub fn summary(&self, from: f64, to: f64) -> Summary {
        let inside: Vec<f32> = self
            .times
            .iter()
            .zip(&self.change)
            .filter(|(time, _)| **time > from && **time <= to)
            .map(|(_, change)| *change)
            .collect();
        let mean = if inside.is_empty() { 0.0 } else { inside.iter().sum::<f32>() / inside.len() as f32 };
        Summary {
            keyframes: inside.len(),
            mean_change: mean,
            scene_changes: inside.iter().filter(|change| **change >= SCENE_CHANGE).count(),
        }
    }
}

fn cache_path(project: &Project, take: &Take) -> Result<PathBuf> {
    project.edit_path(&format!(".cache/activity/{}.json", take.stem()))
}

/// The keyframe activity of `take`, computed once and cached.
pub fn activity(project: &Project, take: &Take) -> Result<Activity> {
    if take.media.video.is_none() || take.still() {
        return Ok(Activity::default());
    }
    let path = cache_path(project, take)?;
    let fresh = match (fs::metadata(&path), fs::metadata(&take.path)) {
        (Ok(cache), Ok(source)) => cache.modified()? >= source.modified()?,
        _ => false,
    };
    if fresh && let Ok(activity) = serde_json::from_slice(&fs::read(&path)?) {
        return Ok(activity);
    }
    let (frames, stderr) = tool::run_with_stderr(
        &tool::ffmpeg()?,
        [
            "-nostdin".as_ref(),
            "-v".as_ref(),
            "info".as_ref(),
            "-skip_frame".as_ref(),
            "nokey".as_ref(),
            "-i".as_ref(),
            take.path.as_os_str(),
            "-map".as_ref(),
            "0:v:0".as_ref(),
            "-vf".as_ref(),
            format!("scale={WIDTH}:{HEIGHT}:flags=area,format=gray,showinfo").as_ref(),
            "-fps_mode".as_ref(),
            "passthrough".as_ref(),
            "-f".as_ref(),
            "rawvideo".as_ref(),
            "-".as_ref(),
        ],
    )?;
    let times: Vec<f64> = stderr
        .lines()
        .filter(|line| line.contains("Parsed_showinfo"))
        .filter_map(|line| line.split("pts_time:").nth(1)?.split_whitespace().next()?.parse().ok())
        .collect();
    let size = WIDTH * HEIGHT;
    let mut activity = Activity::default();
    let mut previous: Option<&[u8]> = None;
    for (frame, time) in frames.chunks_exact(size).zip(times) {
        let change = previous.map_or(0.0, |previous| {
            frame.iter().zip(previous).map(|(a, b)| f32::from(a.abs_diff(*b))).sum::<f32>() / size as f32
        });
        activity.times.push(time);
        activity.change.push(change);
        previous = Some(frame);
    }
    fs::write(&path, serde_json::to_vec(&activity)?)?;
    Ok(activity)
}
