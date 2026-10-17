//! What a source file holds, via ffprobe.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{EditError, Result};
use crate::rational::Rational;
use crate::tool;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MediaInfo {
    pub duration: f64,
    pub video: Option<VideoInfo>,
    pub audio: Vec<AudioTrack>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VideoInfo {
    pub codec: String,
    /// Size as displayed: rotation applied.
    pub width: u32,
    pub height: u32,
    /// Average frame rate; `r_rate` is the container's nominal rate.
    pub rate: Rational,
    pub r_rate: Rational,
    /// Rotation from the display matrix, in degrees.
    pub rotation: i32,
    /// PQ or HLG transfer: needs tone mapping for SDR output.
    pub hdr: bool,
    /// A still image: one picture, shown for as long as the plan asks.
    #[serde(default)]
    pub still: bool,
}

impl VideoInfo {
    /// Frames come at irregular intervals (screen recorders, phones).
    pub fn variable_rate(&self) -> bool {
        let (avg, nominal) = (self.rate.as_f64(), self.r_rate.as_f64());
        avg > 0.0 && nominal > 0.0 && (avg - nominal).abs() / nominal > 0.001
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AudioTrack {
    /// Position among the file's audio streams (`0:a:<index>`).
    pub index: usize,
    pub codec: String,
    pub channels: u32,
    pub sample_rate: u32,
}

pub fn probe(path: &Path) -> Result<MediaInfo> {
    let ffprobe = tool::ffprobe()?;
    let json = tool::run(
        &ffprobe,
        [
            "-v".as_ref(),
            "error".as_ref(),
            "-print_format".as_ref(),
            "json".as_ref(),
            "-show_format".as_ref(),
            "-show_streams".as_ref(),
            path.as_os_str(),
        ],
    )?;
    parse(&json).ok_or_else(|| EditError::invalid(format!("ffprobe could not read {}", path.display())))
}

/// What a still image holds: its size, no length and no sound.
pub fn probe_image(path: &Path) -> Result<MediaInfo> {
    let ffprobe = tool::ffprobe()?;
    let json = tool::run(
        &ffprobe,
        [
            "-v".as_ref(),
            "error".as_ref(),
            "-print_format".as_ref(),
            "json".as_ref(),
            "-show_streams".as_ref(),
            path.as_os_str(),
        ],
    )?;
    parse_image(&json).ok_or_else(|| EditError::invalid(format!("ffprobe could not read the image {}", path.display())))
}

fn parse_image(json: &[u8]) -> Option<MediaInfo> {
    let probe: Probe = serde_json::from_slice(json).ok()?;
    let stream = probe.streams.into_iter().find(|stream| stream.codec_type.as_deref() == Some("video"))?;
    let (width, height) = (stream.width?, stream.height?);
    let video = VideoInfo {
        codec: stream.codec_name.unwrap_or_default(),
        width,
        height,
        rate: Rational::new(1, 1),
        r_rate: Rational::new(1, 1),
        rotation: 0,
        hdr: false,
        still: true,
    };
    Some(MediaInfo { duration: 0.0, video: Some(video), audio: Vec::new() })
}

#[derive(Deserialize)]
struct Probe {
    #[serde(default)]
    streams: Vec<Stream>,
    format: Option<Format>,
}

#[derive(Deserialize)]
struct Format {
    duration: Option<String>,
}

#[derive(Deserialize)]
struct Stream {
    codec_type: Option<String>,
    codec_name: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    avg_frame_rate: Option<String>,
    r_frame_rate: Option<String>,
    color_transfer: Option<String>,
    channels: Option<u32>,
    sample_rate: Option<String>,
    duration: Option<String>,
    #[serde(default)]
    side_data_list: Vec<SideData>,
    #[serde(default)]
    disposition: Disposition,
}

#[derive(Deserialize, Default)]
struct Disposition {
    #[serde(default)]
    attached_pic: u8,
}

#[derive(Deserialize)]
struct SideData {
    rotation: Option<f64>,
}

fn parse(json: &[u8]) -> Option<MediaInfo> {
    let probe: Probe = serde_json::from_slice(json).ok()?;
    let mut duration = probe.format.and_then(|format| format.duration).and_then(|d| d.parse::<f64>().ok());
    let mut video = None;
    let mut audio = Vec::new();
    for stream in probe.streams {
        match stream.codec_type.as_deref() {
            // Cover art in audio files is a "video" stream of one picture.
            Some("video") if video.is_none() && stream.disposition.attached_pic == 0 => {
                let (Some(width), Some(height)) = (stream.width, stream.height) else { continue };
                let rotation =
                    stream.side_data_list.iter().find_map(|data| data.rotation).unwrap_or(0.0).round() as i32;
                let quarter = rotation.rem_euclid(180) == 90;
                let r_rate = stream.r_frame_rate.as_deref().and_then(Rational::parse).unwrap_or(Rational::new(30, 1));
                let rate = stream
                    .avg_frame_rate
                    .as_deref()
                    .and_then(Rational::parse)
                    .filter(|rate| rate.num > 0)
                    .unwrap_or(r_rate);
                if duration.is_none() {
                    duration = stream.duration.as_deref().and_then(|d| d.parse().ok());
                }
                video = Some(VideoInfo {
                    codec: stream.codec_name.unwrap_or_default(),
                    width: if quarter { height } else { width },
                    height: if quarter { width } else { height },
                    rate,
                    r_rate,
                    rotation,
                    hdr: matches!(stream.color_transfer.as_deref(), Some("smpte2084" | "arib-std-b67")),
                    still: false,
                });
            }
            Some("audio") => {
                if duration.is_none() {
                    duration = stream.duration.as_deref().and_then(|d| d.parse().ok());
                }
                audio.push(AudioTrack {
                    index: audio.len(),
                    codec: stream.codec_name.unwrap_or_default(),
                    channels: stream.channels.unwrap_or(2),
                    sample_rate: stream.sample_rate.and_then(|rate| rate.parse().ok()).unwrap_or(48_000),
                });
            }
            _ => {}
        }
    }
    Some(MediaInfo { duration: duration?, video, audio })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_rotation_rate_and_hdr() {
        let json = br#"{"streams":[
            {"codec_type":"video","codec_name":"hevc","width":1920,"height":1080,"avg_frame_rate":"2900/100",
             "r_frame_rate":"30/1","color_transfer":"arib-std-b67","side_data_list":[{"rotation":-90}]},
            {"codec_type":"audio","codec_name":"aac","channels":2,"sample_rate":"48000"},
            {"codec_type":"audio","codec_name":"pcm_s16le","channels":1,"sample_rate":"44100"}],
            "format":{"duration":"12.5"}}"#;
        let info = parse(json).unwrap();
        let video = info.video.unwrap();
        assert_eq!((video.width, video.height), (1080, 1920));
        assert!(video.hdr);
        assert!(video.variable_rate());
        assert_eq!(info.audio.len(), 2);
        assert_eq!(info.audio[1].index, 1);
        assert_eq!(info.duration, 12.5);
    }

    #[test]
    fn an_image_is_a_still_without_length_or_sound() {
        let json = br#"{"streams":[{"codec_type":"video","codec_name":"png","width":1600,"height":900}]}"#;
        let info = parse_image(json).unwrap();
        let video = info.video.unwrap();
        assert!(video.still);
        assert_eq!((video.width, video.height), (1600, 900));
        assert_eq!(info.duration, 0.0);
        assert!(info.audio.is_empty());
    }

    #[test]
    fn cover_art_is_not_video() {
        let json = br#"{"streams":[
            {"codec_type":"audio","codec_name":"mp3","channels":2,"sample_rate":"44100"},
            {"codec_type":"video","codec_name":"mjpeg","width":600,"height":600,"disposition":{"attached_pic":1}}],
            "format":{"duration":"3"}}"#;
        assert_eq!(parse(json).unwrap().video, None);
    }
}
