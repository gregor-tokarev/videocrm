//! Cut plans: the agent says which phrases to keep and in which order; this module
//! turns that into exact source spans. All timing arithmetic lives here, so the model
//! never adds up durations or types a cut time for speech.
//!
//! Inside a kept segment, fillers are cut out and pauses longer than the limits are
//! tightened; every cut edge is padded and moved to the quietest moment nearby, never
//! into a word.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

use crate::error::{EditError, Result};
use crate::session::Session;
use crate::shots;
use crate::transcript::{TakeView, Word};

/// Pauses up to this long stay as spoken: cutting a short pause saves little and shows
/// as a jump cut.
pub const DEFAULT_MAX_PAUSE: f64 = 1.0;
/// What a longer pause inside a sentence is cut down to.
pub const DEFAULT_TIGHT_PAUSE: f64 = 0.45;
/// What a longer pause after a full stop is cut down to.
pub const DEFAULT_SENTENCE_PAUSE: f64 = 0.7;
/// Silence kept before the first word of a span (less when the previous word is closer).
const PAD_BEFORE: f64 = 0.10;
/// Silence kept after the last word of a span.
const PAD_AFTER: f64 = 0.14;
/// How far a cut may move to find a quieter moment.
const SNAP: f64 = 0.04;
/// Spans shorter than this are dropped.
const MIN_SPAN: f64 = 0.04;
/// A pause this long inside a segment is worth a word in the receipt.
const LONG_PAUSE: f64 = 3.0;
/// A silent time segment this long may hold preparation around the demo.
const LONG_TIME_SEGMENT: f64 = 20.0;
/// A still stretch inside a long time segment worth pointing out.
const STILL_SHOT: f64 = 3.0;
/// Longest an image may be held as a segment of its own.
const MAX_STILL: f64 = 60.0;
/// An overlay comes in this much before its first word and stays this much after its last.
const OVERLAY_LEAD: f64 = 0.1;
const OVERLAY_TAIL: f64 = 0.3;
/// Overlays shorter than this flash by.
const SHORT_OVERLAY: f64 = 1.5;

fn default_true() -> bool {
    true
}

/// What the agent sends.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CutPlan {
    pub segments: Vec<SegmentSpec>,
    /// Cut fillers (um, uh, э-э-э) inside kept segments.
    #[serde(default = "default_true")]
    pub remove_fillers: bool,
    /// Pauses longer than this (seconds) are tightened; shorter ones stay.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_pause: Option<f64>,
    /// What a tightened pause inside a sentence becomes, seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tight_pause: Option<f64>,
    /// What a tightened pause after a sentence ends becomes, seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sentence_pause: Option<f64>,
    /// Wanted output length, seconds: the receipt says how far off the cut is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_min_seconds: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_max_seconds: Option<f64>,
    /// Output aspect ratio (`16:9`, `9:16`, `1:1`, `4:5`); default the source's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aspect: Option<String>,
    /// Hide jump cuts: at each cut inside a speech segment the picture alternates
    /// between the full frame and a slight zoom-in. Segments can turn it on or off.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub punch_in: bool,
    /// Pictures shown over the sound of the cut (a screenshot, a chart, B-roll).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub overlays: Vec<OverlaySpec>,
}

/// How much a punched-in span is zoomed.
const PUNCH_IN: f64 = 1.12;

/// How a segment's picture fills an output of another shape.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Framing {
    /// `true`: fill the frame, cropping the sides; `false`: fit the whole picture over
    /// a blurred copy of itself.
    pub fill: bool,
    /// Where the crop is centred, 0 (left/top) to 1 (right/bottom).
    pub focus_x: f64,
    pub focus_y: f64,
    /// Zoom on top of filling the frame (1 = none), for punch-ins.
    #[serde(default = "one")]
    pub zoom: f64,
}

fn one() -> f64 {
    1.0
}

impl Default for Framing {
    fn default() -> Self {
        Self { fill: true, focus_x: 0.5, focus_y: 0.5, zoom: 1.0 }
    }
}

/// One segment of the output: phrases `from`..=`to` of one take, or a time range of a
/// take (for stretches without speech, such as a silent screen demo).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SegmentSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    /// The first words to keep in `from` (drops what comes before them).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub starts_with: Option<String>,
    /// The last words to keep in `to` (drops what comes after them).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ends_with: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub take: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end: Option<f64>,
    /// Keep the pauses inside this segment as they are (fillers are still cut).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub keep_pauses: bool,
    /// When the output has another shape than the take: `fill` (crop) or `fit`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub framing: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focus_x: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focus_y: Option<f64>,
    /// Zoom into the picture (1 = none, up to 3), around `focus_x` / `focus_y`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub zoom: Option<f64>,
    /// Punch-ins for this segment, over the plan's `punch_in`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub punch_in: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// A picture shown instead of the cut's own while its sound goes on: an image, or the
/// picture of another take from `start`. It covers the output from the first kept word
/// of `from` (or `starts_with`) to the last of `to` (or `ends_with`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OverlaySpec {
    /// The take whose picture is shown.
    pub take: String,
    /// Where a video take's picture starts, take seconds (default 0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<f64>,
    pub from: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub starts_with: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ends_with: Option<String>,
    /// `fit` (default: the whole picture) or `fill` (cropped to the frame).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub framing: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focus_x: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focus_y: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub zoom: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// An overlay placed on the output timeline.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Overlay {
    /// Position in the plan's `overlays` (`O1` is 0).
    pub index: usize,
    pub take: String,
    /// Take time of the first frame shown (0 for an image).
    pub source_start: f64,
    pub out_start: f64,
    pub out_end: f64,
    pub framing: Framing,
    pub label: Option<String>,
    /// The take and what it is, for the receipt.
    pub source: String,
    /// The words it covers.
    pub text: String,
}

/// A stretch of a take that goes into the output, in output order.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Span {
    pub take: String,
    pub start: f64,
    pub end: f64,
    /// Index of the segment it belongs to.
    pub segment: usize,
    #[serde(default)]
    pub framing: Framing,
}

impl Span {
    pub fn duration(&self) -> f64 {
        self.end - self.start
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SegmentInfo {
    pub label: Option<String>,
    pub take: String,
    /// `A4-A9`, or the time range for a time segment.
    pub source: String,
    pub out_start: f64,
    pub out_end: f64,
    pub text: String,
}

/// A plan turned into spans, with what the receipt reports.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ResolvedCut {
    pub plan: CutPlan,
    pub spans: Vec<Span>,
    pub segments: Vec<SegmentInfo>,
    #[serde(default)]
    pub overlays: Vec<Overlay>,
    pub duration: f64,
    pub fillers_removed: usize,
    pub pauses_tightened: usize,
    pub seconds_tightened: f64,
    pub warnings: Vec<String>,
}

/// Which words of a take a segment keeps.
struct Selection {
    take: String,
    /// Kept spoken words, as indices into the take's words.
    kept: Vec<usize>,
    /// The first and last word index of the selected range (fillers included).
    range: (usize, usize),
}

pub fn resolve(session: &mut Session, plan: &CutPlan) -> Result<ResolvedCut> {
    if plan.segments.is_empty() {
        return Err(EditError::invalid("the plan has no segments"));
    }
    if let Some(aspect) = &plan.aspect {
        aspect_ratio(aspect)?;
    }
    let max_pause = plan.max_pause.unwrap_or(DEFAULT_MAX_PAUSE).clamp(0.05, 10.0);
    let tight_pause = plan.tight_pause.unwrap_or(DEFAULT_TIGHT_PAUSE).clamp(0.05, 5.0).min(max_pause);
    let sentence_pause =
        plan.sentence_pause.unwrap_or(DEFAULT_SENTENCE_PAUSE).clamp(0.05, 5.0).min(max_pause.max(0.05));

    // Load everything first; resolving then only reads.
    for (index, segment) in plan.segments.iter().enumerate() {
        let take = segment_take(index, segment)?;
        if segment.from.is_some() {
            session.load_view(&take)?;
        }
        session.load_envelope(&take)?;
        if session.take(&take)?.media.video.is_some() {
            session.load_activity(&take)?;
        }
    }
    for (index, overlay) in plan.overlays.iter().enumerate() {
        let take = take_of(&overlay.from).ok_or_else(|| {
            EditError::invalid(format!("O{}: from “{}” is not a phrase id like A12", index + 1, overlay.from))
        })?;
        session.load_view(&take)?;
        session.take(&overlay.take)?;
    }

    let mut spans: Vec<Span> = Vec::new();
    let mut infos = Vec::new();
    let mut warnings = Vec::new();
    let mut selections: Vec<Option<Selection>> = Vec::new();
    let (mut fillers_removed, mut pauses_tightened, mut seconds_tightened) = (0, 0, 0.0);

    for (index, segment) in plan.segments.iter().enumerate() {
        let name = format!("S{}", index + 1);
        let take = segment_take(index, segment)?;
        let envelope = session.envelope(&take).expect("loaded");
        let duration = session.take(&take)?.media.duration;
        let first_span = spans.len();
        if segment.from.is_some() {
            let view = session.view(&take).expect("loaded");
            let selection = select(view, segment, plan.remove_fillers, &name)?;
            let words = &view.transcript.words;
            let kept = &selection.kept;
            fillers_removed +=
                (selection.range.0..=selection.range.1).filter(|i| !words[*i].event && !kept.contains(i)).count();

            let first = &words[kept[0]];
            let previous_end = previous_word(words, kept[0]).map_or(0.0, |w| w.end);
            let pad = PAD_BEFORE.min((first.start - previous_end).max(0.0) * 0.8);
            let mut start = snap(envelope, first.start - pad, previous_end, first.start);

            for pair in kept.windows(2) {
                let (a, b) = (&words[pair[0]], &words[pair[1]]);
                let removed: Vec<&Word> = words[pair[0] + 1..pair[1]].iter().filter(|w| !w.event).collect();
                let gap = (b.start - a.end).max(0.0);
                let limit = if a.ends_sentence() { sentence_pause } else { tight_pause };
                if removed.is_empty() && (segment.keep_pauses || gap <= max_pause) {
                    continue;
                }
                let after = removed.first().map_or(gap, |w| (w.start - a.end).max(0.0));
                let before = removed.last().map_or(gap, |w| (b.start - w.end).max(0.0));
                let natural = if removed.is_empty() { gap } else { after + before };
                let target = if segment.keep_pauses { natural } else { natural.min(limit) };
                let (keep_after, keep_before) = if removed.is_empty() {
                    (target * 0.55, target * 0.45)
                } else {
                    let keep_after = after.min(target * 0.55);
                    let keep_before = before.min(target - keep_after);
                    (after.min(target - keep_before), keep_before)
                };
                let cut_from = snap(envelope, a.end + keep_after, a.end, a.end + after);
                let cut_to = snap(envelope, b.start - keep_before, b.start - before, b.start);
                if cut_to - cut_from < 0.02 {
                    continue;
                }
                if removed.is_empty() {
                    pauses_tightened += 1;
                }
                seconds_tightened += cut_to - cut_from;
                if gap >= LONG_PAUSE
                    && let Some(activity) = session.activity(&take)
                {
                    let summary = activity.summary(a.end, b.start);
                    if summary.eventful() {
                        warnings.push(format!(
                            "{name} cuts a {gap:.1} s pause ({:.2}-{:.2}, {}) down to {target:.2} s: if the viewer should see what happens on screen there, end the segment before it and keep the stretch as a time segment",
                            a.end,
                            b.start,
                            summary.label()
                        ));
                    }
                }
                spans.push(Span {
                    take: take.clone(),
                    start,
                    end: cut_from,
                    segment: index,
                    framing: Framing::default(),
                });
                start = cut_to;
            }

            let last = &words[*kept.last().expect("non-empty")];
            let next_start = next_word(words, *kept.last().expect("non-empty")).map_or(duration, |w| w.start);
            let pad = PAD_AFTER.min((next_start - last.end).max(0.0) * 0.8);
            let end = snap(envelope, last.end + pad, last.end, next_start);
            spans.push(Span { take: take.clone(), start, end, segment: index, framing: Framing::default() });

            for word in kept.iter().map(|i| &words[*i]).filter(|w| w.is_cut_off()) {
                warnings
                    .push(format!("{name} keeps a cut-off word “{}” at {:.2}: a false start?", word.text, word.start));
            }
            let source = match (segment.from.as_deref(), segment.to.as_deref()) {
                (Some(from), Some(to)) if !from.eq_ignore_ascii_case(to) => format!("{}-{}", from.trim(), to.trim()),
                (Some(from), _) => from.trim().to_owned(),
                _ => String::new(),
            };
            infos.push(SegmentInfo {
                label: segment.label.clone(),
                take: take.clone(),
                source,
                out_start: 0.0,
                out_end: 0.0,
                text: view.text(selection.range.0..selection.range.1 + 1),
            });
            selections.push(Some(selection));
        } else {
            let (Some(from), Some(to)) = (segment.start, segment.end) else {
                return Err(EditError::invalid(format!("{name}: a time segment needs take, start and end")));
            };
            // An image is held for as long as the segment asks.
            let still = session.take(&take)?.still();
            let duration = if still { MAX_STILL } else { duration };
            if !(from >= 0.0 && to > from && from < duration) {
                return Err(EditError::invalid(if still {
                    format!(
                        "{name}: {take} is an image: hold it with start 0 and end the seconds to show it (up to {MAX_STILL:.0})"
                    )
                } else {
                    format!("{name}: {from:.2}-{to:.2} is not a range inside take {take} (0-{duration:.2})")
                }));
            }
            let to = to.min(duration);
            let start = snap(envelope, from, (from - SNAP).max(0.0), from + SNAP);
            let end = snap(envelope, to, to - SNAP, (to + SNAP).min(duration));
            let mut text = String::new();
            if let Some(view) = session.view(&take) {
                for word in view.transcript.words.iter().filter(|w| !w.event) {
                    if word.start < start && word.end > start + 0.02 || word.start < end - 0.02 && word.end > end {
                        warnings.push(format!(
                            "{name} cuts through the word “{}” ({:.2}-{:.2}); use phrase ids or move the edge",
                            word.text, word.start, word.end
                        ));
                    }
                    if word.start >= start && word.end <= end {
                        if !text.is_empty() {
                            text.push(' ');
                        }
                        text.push_str(&word.text);
                    }
                }
            }
            let source = session.take(&take)?;
            if end - start > LONG_TIME_SEGMENT
                && end - start <= shots::MAX_SPAN
                && text.is_empty()
                && source.media.video.is_some()
                && !still
            {
                let still: Vec<String> = shots::shots(&source.path, start, end)?
                    .iter()
                    .filter(|shot| !shot.moving() && shot.end - shot.start >= STILL_SHOT)
                    .map(|shot| format!("{:.2}-{:.2}", shot.start, shot.end))
                    .collect();
                if !still.is_empty() {
                    warnings.push(format!(
                        "{name} keeps {:.0} s without speech, and the screen mostly stays in {}: if that is preparation (opening pages, a small player, switching windows), keep only the shots where the demo itself plays",
                        end - start,
                        still.join(", ")
                    ));
                }
            }
            spans.push(Span { take: take.clone(), start, end, segment: index, framing: Framing::default() });
            infos.push(SegmentInfo {
                label: segment.label.clone(),
                take: take.clone(),
                source: if still {
                    format!("{take} (image) for {:.2} s", end - start)
                } else {
                    format!("{take} {start:.2}-{end:.2}")
                },
                out_start: 0.0,
                out_end: 0.0,
                text: if text.is_empty() { "(no speech)".to_owned() } else { text },
            });
            selections.push(None);
        }
        if spans.len() == first_span {
            return Err(EditError::invalid(format!("{name} keeps nothing")));
        }
    }

    for span in &mut spans {
        span.framing = framing(&plan.segments[span.segment])?;
    }
    let mut spans = tidy(spans);
    punch_in(&mut spans, &plan.segments, plan.punch_in);
    let mut out = 0.0;
    let mut segment_times: HashMap<usize, (f64, f64)> = HashMap::new();
    for span in &spans {
        let entry = segment_times.entry(span.segment).or_insert((out, out));
        out += span.duration();
        entry.1 = out;
    }
    for (index, info) in infos.iter_mut().enumerate() {
        if let Some((start, end)) = segment_times.get(&index) {
            info.out_start = *start;
            info.out_end = *end;
        }
    }

    warnings.extend(overlaps(&spans));
    warnings.extend(duplicates(session, &selections));
    warnings.extend(mid_sentence_joins(session, &selections));
    for (index, info) in infos.iter().enumerate() {
        if info.out_end - info.out_start < 0.6 {
            warnings.push(format!("S{} is only {:.2} s long", index + 1, info.out_end - info.out_start));
        }
    }
    let duration = out;
    let overlays = overlays(session, plan, &spans, duration, &mut warnings)?;
    match (plan.target_min_seconds, plan.target_max_seconds) {
        (Some(min), _) if duration < min - 0.5 => warnings.push(format!(
            "the cut is {} long, {:.1} s shorter than the {} minimum",
            clock(duration),
            min - duration,
            clock(min)
        )),
        (_, Some(max)) if duration > max + 0.5 => warnings.push(format!(
            "the cut is {} long, {:.1} s over the {} maximum",
            clock(duration),
            duration - max,
            clock(max)
        )),
        _ => {}
    }

    Ok(ResolvedCut {
        plan: plan.clone(),
        spans,
        segments: infos,
        overlays,
        duration,
        fillers_removed,
        pauses_tightened,
        seconds_tightened,
        warnings,
    })
}

fn framing(segment: &SegmentSpec) -> Result<Framing> {
    let fill = match segment.framing.as_deref().map(str::trim) {
        None | Some("") | Some("fill") => true,
        Some("fit") => false,
        Some(other) => return Err(EditError::invalid(format!("framing “{other}”: use fill or fit"))),
    };
    Ok(Framing {
        fill,
        focus_x: segment.focus_x.unwrap_or(0.5).clamp(0.0, 1.0),
        focus_y: segment.focus_y.unwrap_or(0.5).clamp(0.0, 1.0),
        zoom: segment.zoom.unwrap_or(1.0).clamp(1.0, 3.0),
    })
}

/// Places the plan's overlays on the output timeline of `spans`, in output order.
fn overlays(
    session: &Session,
    plan: &CutPlan,
    spans: &[Span],
    duration: f64,
    warnings: &mut Vec<String>,
) -> Result<Vec<Overlay>> {
    let mut starts = Vec::with_capacity(spans.len());
    let mut out = 0.0;
    for span in spans {
        starts.push(out);
        out += span.duration();
    }
    // Where a moment of a take plays in the output, if the cut keeps it.
    let out_time = |take: &str, time: f64| {
        spans
            .iter()
            .zip(&starts)
            .find(|(span, _)| span.take == take && time >= span.start - 0.001 && time <= span.end + 0.001)
            .map(|(span, start)| start + (time - span.start).clamp(0.0, span.duration()))
    };
    let mut placed = Vec::new();
    for (index, spec) in plan.overlays.iter().enumerate() {
        let name = format!("O{}", index + 1);
        let anchor = take_of(&spec.from).expect("checked while loading");
        if let Some(to) = &spec.to
            && take_of(to).as_deref() != Some(anchor.as_str())
        {
            return Err(EditError::invalid(format!(
                "{name}: from {} and to {to} must be phrases of the same take",
                spec.from
            )));
        }
        let view = session.view(&anchor).expect("loaded");
        let words = SegmentSpec {
            from: Some(spec.from.clone()),
            to: spec.to.clone(),
            starts_with: spec.starts_with.clone(),
            ends_with: spec.ends_with.clone(),
            ..SegmentSpec::default()
        };
        let selection = select(view, &words, true, &name)?;
        let all = &view.transcript.words;
        let (first, last) = (&all[selection.kept[0]], &all[*selection.kept.last().expect("non-empty")]);
        let not_kept = |word: &Word| {
            EditError::invalid(format!(
                "{name} is anchored on “{}” at {:.2} in take {anchor}, which the cut does not keep",
                word.text, word.start
            ))
        };
        let start = out_time(&anchor, first.start).ok_or_else(|| not_kept(first))?;
        let end = out_time(&anchor, last.end).ok_or_else(|| not_kept(last))?;
        let (out_start, out_end) = ((start - OVERLAY_LEAD).max(0.0), (end + OVERLAY_TAIL).min(duration));
        if out_end - out_start < 0.2 {
            return Err(EditError::invalid(format!("{name}: its last word comes before its first in the cut")));
        }

        let source = session.take(&spec.take)?;
        if source.media.video.is_none() {
            return Err(EditError::invalid(format!("{name}: take {} has no picture", source.id)));
        }
        let source_start = if source.still() { 0.0 } else { spec.start.unwrap_or(0.0) };
        if !source.still() {
            if !(0.0..source.media.duration).contains(&source_start) {
                return Err(EditError::invalid(format!(
                    "{name}: start {source_start:.2} is not inside take {} (0-{:.2})",
                    source.id, source.media.duration
                )));
            }
            if source_start + (out_end - out_start) > source.media.duration + 0.05 {
                warnings.push(format!(
                    "{name} shows {} from {source_start:.2} for {:.1} s, but the take ends at {:.2}: its last frame holds",
                    source.id,
                    out_end - out_start,
                    source.media.duration
                ));
            }
        }
        let fill = match spec.framing.as_deref().map(str::trim) {
            None | Some("") | Some("fit") => false,
            Some("fill") => true,
            Some(other) => return Err(EditError::invalid(format!("{name}: framing “{other}”: use fit or fill"))),
        };
        if out_end - out_start < SHORT_OVERLAY {
            warnings.push(format!("{name} is on screen for only {:.1} s", out_end - out_start));
        }
        placed.push(Overlay {
            index,
            take: source.id.clone(),
            source_start,
            out_start,
            out_end,
            framing: Framing {
                fill,
                focus_x: spec.focus_x.unwrap_or(0.5).clamp(0.0, 1.0),
                focus_y: spec.focus_y.unwrap_or(0.5).clamp(0.0, 1.0),
                zoom: spec.zoom.unwrap_or(1.0).clamp(1.0, 3.0),
            },
            label: spec.label.clone(),
            source: if source.still() {
                format!("{} ({}, image)", source.id, source.file)
            } else {
                format!("{} ({}) from {source_start:.2}", source.id, source.file)
            },
            text: view.text(selection.range.0..selection.range.1 + 1),
        });
    }
    placed.sort_by(|a, b| a.out_start.total_cmp(&b.out_start));
    // Overlays on neighbouring words meet: the earlier one ends where the next begins.
    for index in 1..placed.len() {
        let (previous, next) = (&placed[index - 1], &placed[index]);
        if next.out_start < previous.out_end {
            if previous.out_end - next.out_start > OVERLAY_LEAD + OVERLAY_TAIL + 0.05 {
                return Err(EditError::invalid(format!(
                    "O{} and O{} cover the same words: one picture at a time",
                    previous.index + 1,
                    next.index + 1
                )));
            }
            placed[index - 1].out_end = placed[index].out_start;
        }
    }
    Ok(placed)
}

/// Width over height of an aspect like `9:16`.
pub fn aspect_ratio(aspect: &str) -> Result<f64> {
    let parsed = aspect
        .split_once(':')
        .and_then(|(w, h)| Some((w.trim().parse::<f64>().ok()?, h.trim().parse::<f64>().ok()?)))
        .filter(|(w, h)| *w > 0.0 && *h > 0.0);
    match parsed {
        Some((w, h)) if (0.2..=5.0).contains(&(w / h)) => Ok(w / h),
        _ => Err(EditError::invalid(format!("aspect “{aspect}”: use a ratio like 16:9, 9:16, 1:1 or 4:5"))),
    }
}

fn segment_take(index: usize, segment: &SegmentSpec) -> Result<String> {
    let name = format!("S{}", index + 1);
    match (&segment.from, &segment.take) {
        (Some(from), _) => {
            let take = take_of(from)
                .ok_or_else(|| EditError::invalid(format!("{name}: “{from}” is not a phrase id like A12")))?;
            if let Some(to) = &segment.to
                && take_of(to).as_deref() != Some(take.as_str())
            {
                return Err(EditError::invalid(format!(
                    "{name}: from {from} and to {to} must be phrases of the same take"
                )));
            }
            Ok(take)
        }
        (None, Some(take)) => Ok(take.trim().to_ascii_uppercase()),
        (None, None) => Err(EditError::invalid(format!(
            "{name}: give either from/to phrase ids, or take with start and end seconds"
        ))),
    }
}

/// `A12` -> `A`.
fn take_of(id: &str) -> Option<String> {
    let id = id.trim();
    let letters: String = id.chars().take_while(char::is_ascii_alphabetic).collect();
    let digits = &id[letters.len()..];
    (!letters.is_empty() && !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()))
        .then(|| letters.to_ascii_uppercase())
}

fn select(view: &TakeView, segment: &SegmentSpec, remove_fillers: bool, name: &str) -> Result<Selection> {
    let from = view.phrase_index(segment.from.as_deref().unwrap_or_default())?;
    let to = match &segment.to {
        Some(to) => view.phrase_index(to)?,
        None => from,
    };
    if to < from {
        return Err(EditError::invalid(format!(
            "{name}: {} comes after {}",
            view.phrases[from].id, view.phrases[to].id
        )));
    }
    let words = &view.transcript.words;
    let mut first = view.phrases[from].words.start;
    let mut last = view.phrases[to].words.end - 1;
    if let Some(needle) = segment.starts_with.as_deref().filter(|needle| !needle.trim().is_empty()) {
        first = find_words(view, from, needle, false).ok_or_else(|| {
            EditError::invalid(format!(
                "{name}: “{needle}” is not in {}. {}",
                view.phrases[from].id,
                view.word_lines(from)
            ))
        })?;
    }
    if let Some(needle) = segment.ends_with.as_deref().filter(|needle| !needle.trim().is_empty()) {
        last = find_words(view, to, needle, true).ok_or_else(|| {
            EditError::invalid(format!("{name}: “{needle}” is not in {}. {}", view.phrases[to].id, view.word_lines(to)))
        })?;
    }
    if last < first {
        return Err(EditError::invalid(format!("{name}: ends_with comes before starts_with")));
    }
    let kept: Vec<usize> =
        (first..=last).filter(|i| !words[*i].event && !(remove_fillers && words[*i].is_filler())).collect();
    if kept.is_empty() {
        return Err(EditError::invalid(format!("{name} keeps no words")));
    }
    Ok(Selection { take: view.take.clone(), kept, range: (first, last) })
}

/// Index of the first (or, with `last`, the final) word of `needle` in a phrase.
fn find_words(view: &TakeView, phrase: usize, needle: &str, last: bool) -> Option<usize> {
    let needle: Vec<String> = needle.split_whitespace().map(token).filter(|t| !t.is_empty()).collect();
    if needle.is_empty() {
        return None;
    }
    let words: Vec<(usize, String)> = view.phrases[phrase]
        .words
        .clone()
        .filter(|i| !view.transcript.words[*i].event)
        .map(|i| (i, token(&view.transcript.words[i].text)))
        .filter(|(_, t)| !t.is_empty())
        .collect();
    let starts: Vec<usize> = (0..words.len().saturating_sub(needle.len() - 1))
        .filter(|s| needle.iter().enumerate().all(|(k, t)| words[s + k].1 == *t))
        .collect();
    if last { starts.last().map(|s| words[s + needle.len() - 1].0) } else { starts.first().map(|s| words[*s].0) }
}

fn token(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .map(|c| if c == 'ё' { 'е' } else { c })
        .collect()
}

fn previous_word(words: &[Word], index: usize) -> Option<&Word> {
    words[..index].iter().rev().find(|w| !w.event)
}

fn next_word(words: &[Word], index: usize) -> Option<&Word> {
    words[index + 1..].iter().find(|w| !w.event)
}

/// `preferred`, moved to the quietest moment within [`SNAP`] of it, staying inside
/// `[low, high]`.
fn snap(envelope: &crate::audio::Envelope, preferred: f64, low: f64, high: f64) -> f64 {
    let (low, high) = (low.min(high), high.max(low));
    let preferred = preferred.clamp(low, high);
    envelope.quietest((preferred - SNAP).max(low), (preferred + SNAP).min(high), preferred)
}

/// Alternates the segment's framing and a zoom-in at every jump cut in speech segments
/// with punch-ins (`default`, or the segment's own choice): a cut between two spans of
/// the same take where the picture would jump. A new take or a time segment starts at
/// the segment's framing again.
fn punch_in(spans: &mut [Span], segments: &[SegmentSpec], default: bool) {
    let mut zoomed = false;
    for index in 0..spans.len() {
        let on = segments
            .get(spans[index].segment)
            .is_some_and(|spec| spec.from.is_some() && spec.punch_in.unwrap_or(default));
        let jump = index > 0 && {
            let previous = &spans[index - 1];
            let previous_speech = segments.get(previous.segment).is_some_and(|spec| spec.from.is_some());
            previous_speech && previous.take == spans[index].take && (spans[index].start - previous.end).abs() > 0.005
        };
        zoomed = on && jump && !zoomed;
        if zoomed {
            spans[index].framing.zoom = (spans[index].framing.zoom * PUNCH_IN).min(3.0);
        }
    }
}

/// Merges spans of a segment that continue each other and drops slivers.
fn tidy(spans: Vec<Span>) -> Vec<Span> {
    let mut out: Vec<Span> = Vec::new();
    for span in spans {
        if span.duration() < MIN_SPAN {
            continue;
        }
        if let Some(last) = out.last_mut()
            && last.take == span.take
            && last.segment == span.segment
            && (span.start - last.end).abs() < 0.005
        {
            last.end = span.end;
            continue;
        }
        out.push(span);
    }
    out
}

fn overlaps(spans: &[Span]) -> Vec<String> {
    let mut warnings = Vec::new();
    for (i, a) in spans.iter().enumerate() {
        for b in &spans[i + 1..] {
            if a.take == b.take && a.segment != b.segment && a.start < b.end - 0.05 && b.start < a.end - 0.05 {
                warnings.push(format!(
                    "S{} reuses footage S{} already uses ({} {:.2}-{:.2})",
                    b.segment + 1,
                    a.segment + 1,
                    a.take,
                    a.start.max(b.start),
                    a.end.min(b.end)
                ));
            }
        }
    }
    warnings.dedup();
    warnings
}

/// Segments that say the same thing (a retake kept twice), and phrases kept together
/// with the phrase that says them again.
fn duplicates(session: &Session, selections: &[Option<Selection>]) -> Vec<String> {
    let mut warnings = Vec::new();
    let grams: Vec<HashSet<Vec<String>>> = selections
        .iter()
        .map(|selection| {
            let Some(selection) = selection else { return HashSet::new() };
            let view = session.view(&selection.take).expect("loaded");
            let tokens: Vec<String> = selection
                .kept
                .iter()
                .map(|i| token(&view.transcript.words[*i].text))
                .filter(|t| !t.is_empty())
                .collect();
            let n = if tokens.len() < 8 { 3 } else { 4 };
            tokens.windows(n).map(<[String]>::to_vec).collect()
        })
        .collect();
    for i in 0..grams.len() {
        for j in i + 1..grams.len() {
            let shared = grams[i].intersection(&grams[j]).count();
            let smaller = grams[i].len().min(grams[j].len());
            if shared >= 1 && smaller > 0 && shared * 2 >= smaller {
                warnings.push(format!("S{} and S{} say mostly the same words: a retake kept twice?", i + 1, j + 1));
            }
        }
    }
    // Kept phrases whose retake is kept too.
    let mut kept_words: HashMap<&str, HashSet<usize>> = HashMap::new();
    for selection in selections.iter().flatten() {
        kept_words.entry(selection.take.as_str()).or_default().extend(selection.kept.iter().copied());
    }
    for (take, kept) in &kept_words {
        let Some(view) = session.view(take) else { continue };
        let phrase_kept = |index: usize| {
            let words: Vec<usize> = view.phrases[index]
                .words
                .clone()
                .filter(|i| !view.transcript.words[*i].event && !view.transcript.words[*i].is_filler())
                .collect();
            !words.is_empty() && words.iter().filter(|i| kept.contains(i)).count() * 2 > words.len()
        };
        for (index, later) in view.said_again.iter().enumerate() {
            if !phrase_kept(index) {
                continue;
            }
            for later in later.iter().filter(|later| phrase_kept(**later)) {
                warnings.push(format!(
                    "{} and {} are both kept, but {} says {}'s words again (a retake?)",
                    view.phrases[index].id, view.phrases[*later].id, view.phrases[*later].id, view.phrases[index].id
                ));
            }
        }
    }
    warnings
}

/// Joins where a sentence is left unfinished and the next segment does not continue it.
fn mid_sentence_joins(session: &Session, selections: &[Option<Selection>]) -> Vec<String> {
    let mut warnings = Vec::new();
    for (index, pair) in selections.windows(2).enumerate() {
        let (Some(a), Some(b)) = (&pair[0], &pair[1]) else { continue };
        let view = session.view(&a.take).expect("loaded");
        let words = &view.transcript.words;
        let last = *a.kept.last().expect("non-empty");
        if words[last].ends_sentence() {
            continue;
        }
        let continues = a.take == b.take && {
            let next = (last + 1..words.len()).find(|i| !words[*i].event && !words[*i].is_filler());
            next == b.kept.first().copied()
        };
        if !continues {
            let tail: Vec<&str> = a.kept.iter().rev().take(5).rev().map(|i| words[*i].text.as_str()).collect();
            let other = session.view(&b.take).expect("loaded");
            let head: Vec<&str> = b.kept.iter().take(5).map(|i| other.transcript.words[*i].text.as_str()).collect();
            warnings.push(format!(
                "S{} ends mid-sentence (“…{}”) and S{} goes on with “{}…”",
                index + 1,
                tail.join(" "),
                index + 2,
                head.join(" ")
            ));
        }
    }
    warnings
}

/// `m:ss.cc` (or `h:mm:ss.cc`).
pub fn clock(seconds: f64) -> String {
    let seconds = seconds.max(0.0);
    let whole = seconds.floor() as u64;
    let centis = ((seconds - whole as f64) * 100.0).round().min(99.0) as u64;
    let (h, m, s) = (whole / 3600, whole / 60 % 60, whole % 60);
    if h > 0 { format!("{h}:{m:02}:{s:02}.{centis:02}") } else { format!("{m}:{s:02}.{centis:02}") }
}

impl ResolvedCut {
    /// What the agent reads after planning.
    pub fn receipt(&self, footage_seconds: f64) -> String {
        let mut out = format!(
            "Cut: {} segments, {} spans, output {} ({:.1} s) from {} of footage.\n",
            self.segments.len(),
            self.spans.len(),
            clock(self.duration),
            self.duration,
            clock(footage_seconds)
        );
        let _ = writeln!(
            out,
            "Inside segments: {} fillers cut, {} long pauses tightened, {:.1} s of pauses and fillers removed.",
            self.fillers_removed, self.pauses_tightened, self.seconds_tightened
        );
        if self.warnings.is_empty() {
            out.push_str("Warnings: none.\n");
        } else {
            let _ = writeln!(out, "Warnings ({}):", self.warnings.len());
            for warning in &self.warnings {
                let _ = writeln!(out, "- {warning}");
            }
        }
        out.push_str("Segments (output time, source, words):\n");
        for (index, segment) in self.segments.iter().enumerate() {
            let label = segment.label.as_deref().map(|label| format!(" [{label}]")).unwrap_or_default();
            let _ = writeln!(
                out,
                "S{:<3} {}-{}  {}{}  “{}”",
                index + 1,
                clock(segment.out_start),
                clock(segment.out_end),
                segment.source,
                label,
                excerpt(&segment.text, 8)
            );
        }
        if !self.overlays.is_empty() {
            out.push_str("Overlays (output time, picture shown, over the words):\n");
            for overlay in &self.overlays {
                let label = overlay.label.as_deref().map(|label| format!(" [{label}]")).unwrap_or_default();
                let _ = writeln!(
                    out,
                    "O{:<3} {}-{}  {}{}  “{}”",
                    overlay.index + 1,
                    clock(overlay.out_start),
                    clock(overlay.out_end),
                    overlay.source,
                    label,
                    excerpt(&overlay.text, 6)
                );
            }
        }
        out
    }
}

/// The first and last `n` words of `text`.
fn excerpt(text: &str, n: usize) -> String {
    let words: Vec<&str> = text.split_whitespace().collect();
    if words.len() <= n * 2 + 2 {
        return words.join(" ");
    }
    format!("{} … {}", words[..n].join(" "), words[words.len() - n..].join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phrase_ids_name_their_take() {
        assert_eq!(take_of("A12").as_deref(), Some("A"));
        assert_eq!(take_of(" ab7 ").as_deref(), Some("AB"));
        assert_eq!(take_of("A"), None);
        assert_eq!(take_of("12"), None);
        assert_eq!(take_of("A1x"), None);
    }

    #[test]
    fn punch_ins_alternate_at_jump_cuts_only() {
        let speech = SegmentSpec { from: Some("A1".into()), ..SegmentSpec::default() };
        let time = SegmentSpec { take: Some("A".into()), start: Some(0.0), end: Some(1.0), ..SegmentSpec::default() };
        let segments = vec![speech.clone(), time, speech];
        let span = |start: f64, end: f64, segment: usize| Span {
            take: "A".into(),
            start,
            end,
            segment,
            framing: Framing::default(),
        };
        let mut spans = vec![
            span(0.0, 1.0, 0),
            span(2.0, 3.0, 0),
            span(4.0, 5.0, 0),
            span(50.0, 60.0, 1),
            span(70.0, 71.0, 2),
            span(72.0, 73.0, 2),
        ];
        punch_in(&mut spans, &segments, true);
        let zooms: Vec<f64> = spans.iter().map(|span| span.framing.zoom).collect();
        assert_eq!(zooms, [1.0, PUNCH_IN, 1.0, 1.0, 1.0, PUNCH_IN]);
    }

    #[test]
    fn segments_turn_punch_ins_off_and_keep_their_own_zoom() {
        let face = SegmentSpec { from: Some("A1".into()), ..SegmentSpec::default() };
        let screen = SegmentSpec { from: Some("A5".into()), punch_in: Some(false), ..SegmentSpec::default() };
        let segments = vec![face, screen];
        let span = |start: f64, end: f64, segment: usize, zoom: f64| Span {
            take: "A".into(),
            start,
            end,
            segment,
            framing: Framing { zoom, ..Framing::default() },
        };
        let mut spans =
            vec![span(0.0, 1.0, 0, 1.0), span(2.0, 3.0, 0, 1.0), span(10.0, 11.0, 1, 1.5), span(12.0, 13.0, 1, 1.5)];
        punch_in(&mut spans, &segments, true);
        let zooms: Vec<f64> = spans.iter().map(|span| span.framing.zoom).collect();
        assert_eq!(zooms, [1.0, PUNCH_IN, 1.5, 1.5]);
    }

    #[test]
    fn aspects_parse_as_ratios() {
        assert!((aspect_ratio("9:16").unwrap() - 0.5625).abs() < 1e-9);
        assert_eq!(aspect_ratio(" 1 : 1 ").unwrap(), 1.0);
        assert!(aspect_ratio("vertical").is_err());
        assert!(aspect_ratio("1:0").is_err());
        assert!(aspect_ratio("100:1").is_err());
    }

    #[test]
    fn clock_reads_like_a_timecode() {
        assert_eq!(clock(0.0), "0:00.00");
        assert_eq!(clock(83.456), "1:23.46");
        assert_eq!(clock(3725.5), "1:02:05.50");
    }

    #[test]
    fn spans_that_continue_each_other_merge() {
        let span = |start: f64, end: f64, segment: usize| Span {
            take: "A".into(),
            start,
            end,
            segment,
            framing: Framing::default(),
        };
        let spans =
            tidy(vec![span(0.0, 1.0, 0), span(1.0, 2.0, 0), span(2.0, 3.0, 1), span(3.0, 3.01, 1), span(4.0, 5.0, 1)]);
        assert_eq!(spans, vec![span(0.0, 2.0, 0), span(2.0, 3.0, 1), span(4.0, 5.0, 1)]);
    }
}
