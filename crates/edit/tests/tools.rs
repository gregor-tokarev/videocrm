//! The tools end to end on generated footage: a transcript in the cache (no API call),
//! a cut plan by phrase ids, renders, and the checks the agent reads.

use std::fs;
use std::path::Path;
use std::process::Command;

use rushes_edit::tools::{Content, Tools};
use serde_json::{Value, json};

fn ffmpeg_available() -> bool {
    ["ffmpeg", "ffprobe"]
        .iter()
        .all(|tool| Command::new(tool).arg("-version").output().is_ok_and(|out| out.status.success()))
}

/// 12 s of test pattern with pink noise (a signal whose position can be measured).
fn make_take(path: &Path) {
    let status = Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-f", "lavfi", "-i", "testsrc2=size=320x240:rate=30", "-f", "lavfi"])
        .args([
            "-i",
            "anoisesrc=color=pink:seed=42:amplitude=0.3",
            "-t",
            "12",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
        ])
        .args(["-c:a", "aac", "-shortest"])
        .arg(path)
        .status()
        .expect("run ffmpeg");
    assert!(status.success());
}

/// Words every 0.4 s from 0.1 to 4 s (phrase A1), a 2 s pause, then words from 6 to 11 s
/// with a filler at 7.2 s (phrase A2).
fn write_transcript(edit: &Path) {
    let mut words = Vec::new();
    let mut add = |text: &str, start: f64, end: f64| {
        words.push(json!({"text": text, "start": start, "end": end, "type": "word", "speaker_id": "s0"}));
    };
    let mut t = 0.1;
    let mut n = 0;
    while t < 3.9 {
        n += 1;
        add(&format!("one{n}"), t, t + 0.3);
        t += 0.4;
    }
    t = 6.0;
    while t < 11.0 {
        n += 1;
        if (t - 7.2).abs() < 0.01 {
            add("um,", t, t + 0.3);
        } else if (t - 10.8).abs() < 0.01 {
            add("end.", t, t + 0.3);
        } else {
            add(&format!("two{n}"), t, t + 0.3);
        }
        t += 0.4;
    }
    fs::create_dir_all(edit.join("transcripts")).unwrap();
    fs::write(edit.join("transcripts/take.json"), json!({"language_code": "eng", "words": words}).to_string()).unwrap();
}

fn text(result: &rushes_edit::tools::ToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|item| match item {
            Content::Text(text) => Some(text.as_str()),
            Content::Png(_) => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn call(tools: &mut Tools, name: &str, args: Value) -> String {
    let result = tools.call(name, args, &|_| {});
    let out = text(&result);
    assert!(!result.is_error, "{name} failed: {out}");
    out
}

#[test]
fn a_plan_renders_frame_and_sample_exact() {
    if !ffmpeg_available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let footage = dir.path();
    make_take(&footage.join("take.mp4"));
    write_transcript(&footage.join("edit"));
    let mut tools = Tools::new(footage);

    let inventory = call(&mut tools, "inventory", json!({}));
    assert!(inventory.contains("A: take.mp4 (0:12.00, 320x240, 30 fps)"), "{inventory}");
    let transcript = call(&mut tools, "read_transcript", json!({}));
    assert!(transcript.contains("A1 ") && transcript.contains("A2 "), "{transcript}");
    assert!(transcript.contains("~um,~"), "fillers are marked: {transcript}");

    // The second phrase first, then a time segment of the pause, then the first phrase.
    let receipt = call(
        &mut tools,
        "plan_cut",
        json!({"segments": [
            {"from": "A2", "label": "second"},
            {"take": "A", "start": 4.3, "end": 5.6, "label": "pause"},
            {"from": "A1", "ends_with": "one9", "label": "first"},
        ]}),
    );
    assert!(receipt.contains("Cut: 3 segments"), "{receipt}");
    assert!(receipt.contains("1 fillers cut"), "{receipt}");

    for quality in ["preview", "final"] {
        let report = call(&mut tools, "render", json!({"quality": quality}));
        assert!(report.contains("No technical problems found."), "{report}");
        assert!(report.contains("median offset 0.0 ms"), "audio lands where the plan puts it: {report}");
    }
    let edl: Value = serde_json::from_slice(&fs::read(footage.join("edit/edl.json")).unwrap()).unwrap();
    let ranges = edl["ranges"].as_array().unwrap();
    assert!(ranges.len() >= 4, "the filler splits the second phrase: {edl}");
    assert!(ranges[0]["start"].as_f64().unwrap() > 5.8, "starts with the second phrase: {edl}");
    assert!(footage.join("edit/final.srt").is_file());

    // A time segment that cuts through a word is flagged; an unknown phrase is an error the agent reads.
    let receipt = call(&mut tools, "plan_cut", json!({"segments": [{"take": "A", "start": 0.2, "end": 2.0}]}));
    assert!(receipt.contains("cuts through the word"), "{receipt}");
    let error = tools.call("plan_cut", json!({"segments": [{"from": "A9"}]}), &|_| {});
    assert!(error.is_error && text(&error).contains("no phrase “A9”"), "{}", text(&error));
}

#[test]
fn vertical_output_crops_and_fits() {
    if !ffmpeg_available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let footage = dir.path();
    make_take(&footage.join("take.mp4"));
    write_transcript(&footage.join("edit"));
    let mut tools = Tools::new(footage);
    call(
        &mut tools,
        "plan_cut",
        json!({"aspect": "9:16", "punch_in": true, "segments": [
            {"from": "A1"},
            {"take": "A", "start": 4.3, "end": 5.6, "framing": "fit"},
            {"from": "A2", "focus_x": 0.3},
        ]}),
    );
    let report =
        call(&mut tools, "render", json!({"quality": "preview", "burn_subtitles": true, "subtitle_style": "punchy"}));
    assert!(report.contains("No technical problems found."), "{report}");
    let probe = Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "v:0", "-show_entries", "stream=width,height", "-of", "csv=p=0"])
        .arg(footage.join("edit/preview.mp4"))
        .output()
        .unwrap();
    // As tall as the source is wide (its long side), in a 9:16 shape.
    assert_eq!(String::from_utf8_lossy(&probe.stdout).trim(), "180,320");
}

/// `seconds` of a solid `colour` picture at 25 fps with pink noise of `amplitude`.
fn make_clip(path: &Path, colour: &str, seconds: f64, amplitude: f64) {
    let status = Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-f", "lavfi", "-i", &format!("color=c={colour}:size=320x240:rate=25"), "-f"])
        .args(["lavfi", "-i", &format!("anoisesrc=color=pink:seed=7:amplitude={amplitude}")])
        .args(["-t", &seconds.to_string()])
        .args(["-c:v", "libx264", "-preset", "ultrafast", "-c:a", "aac", "-shortest"])
        .arg(path)
        .status()
        .expect("run ffmpeg");
    assert!(status.success());
}

fn make_image(path: &Path, colour: &str) {
    let status = Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-f", "lavfi", "-i", &format!("color=c={colour}:size=640x360"), "-frames:v", "1"])
        .arg(path)
        .status()
        .expect("run ffmpeg");
    assert!(status.success());
}

/// Mean colour (r, g, b) of the middle of the frame at `seconds`.
fn colour_at(file: &Path, seconds: f64) -> (u8, u8, u8) {
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-ss", &format!("{seconds:.3}"), "-i"])
        .arg(file)
        .args(["-frames:v", "1", "-vf", "crop=iw/4:ih/4,scale=1:1", "-f", "rawvideo", "-pix_fmt", "rgb24", "-"])
        .output()
        .expect("run ffmpeg");
    assert_eq!(out.stdout.len(), 3, "{}", String::from_utf8_lossy(&out.stderr));
    (out.stdout[0], out.stdout[1], out.stdout[2])
}

/// Integrated loudness (LUFS) of `file` between `from` and `to` seconds.
fn loudness(file: &Path, (from, to): (f64, f64)) -> f64 {
    let out = Command::new("ffmpeg")
        .args(["-nostdin", "-v", "info", "-ss", &format!("{from:.3}"), "-to", &format!("{to:.3}"), "-i"])
        .arg(file)
        .args(["-map", "0:a", "-af", "ebur128", "-f", "null", "-"])
        .output()
        .expect("run ffmpeg");
    let text = String::from_utf8_lossy(&out.stderr);
    let summary = text.rsplit("Summary:").next().unwrap();
    summary
        .lines()
        .find_map(|line| line.trim().strip_prefix("I:"))
        .and_then(|rest| rest.split_whitespace().next()?.parse().ok())
        .expect("integrated loudness")
}

#[test]
fn outside_material_comes_in_as_takes_and_shows_over_the_speech() {
    if !ffmpeg_available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let footage = dir.path().join("footage");
    let outside = dir.path().join("downloads");
    fs::create_dir_all(&footage).unwrap();
    fs::create_dir_all(&outside).unwrap();
    make_take(&footage.join("take.mp4"));
    write_transcript(&footage.join("edit"));
    make_image(&outside.join("chart.png"), "red");
    // Recorded much quieter than the take it goes into.
    make_clip(&outside.join("quote.mp4"), "blue", 4.0, 0.05);
    let mut tools = Tools::new(&footage);

    let added = call(&mut tools, "add_footage", json!({"path": outside.join("chart.png")}));
    assert!(added.contains("Added take B: chart.png (image, 640x360)"), "{added}");
    // It sorts before take.mp4, and still gets the next id: A stays A.
    let added = call(&mut tools, "add_footage", json!({"path": outside.join("quote.mp4"), "name": "0 quote"}));
    assert!(added.contains("Added take C: 0 quote.mp4"), "{added}");
    let inventory = call(&mut tools, "inventory", json!({}));
    assert!(inventory.contains("A: take.mp4") && inventory.contains("B: chart.png (image"), "{inventory}");

    // The quote's words, as if transcribed.
    let words: Vec<Value> = (0..8)
        .map(|n| {
            let start = 0.5 + n as f64 * 0.45;
            json!({"text": format!("quote{n}"), "start": start, "end": start + 0.3, "type": "word"})
        })
        .collect();
    fs::write(
        footage.join("edit/transcripts/0 quote.json"),
        json!({"language_code": "eng", "words": words}).to_string(),
    )
    .unwrap();

    let receipt = call(
        &mut tools,
        "plan_cut",
        json!({"punch_in": true, "segments": [
            {"from": "A1", "label": "intro"},
            {"from": "C1", "label": "quote"},
            {"take": "B", "start": 0, "end": 1.5, "label": "chart held"},
            {"from": "A2", "label": "point", "punch_in": false, "zoom": 1.2},
        ], "overlays": [
            {"take": "B", "from": "A2", "starts_with": "two16", "ends_with": "two19", "label": "chart"},
        ]}),
    );
    assert!(receipt.contains("O1 ") && receipt.contains("B (chart.png, image)"), "{receipt}");
    assert!(receipt.contains("B (image) for 1.50 s"), "{receipt}");
    let report = call(&mut tools, "render", json!({"quality": "preview"}));
    assert!(report.contains("No technical problems found."), "{report}");

    let record: Value =
        serde_json::from_slice(&fs::read(footage.join("edit/.cache/render/last-preview.json")).unwrap()).unwrap();
    let pictures = record["pictures"].as_array().unwrap();
    // The output has the main take's rate (30 fps), not the quote's.
    assert_eq!(record["format"]["rate"], json!({"num": 30, "den": 1}), "{}", record["format"]);
    let middle =
        |picture: &Value| picture["out_start"].as_f64().unwrap() + picture["frames"].as_f64().unwrap() / 30.0 / 2.0;
    let overlay = pictures.iter().find(|p| p["overlay"] == 0).expect("the overlay has its frames");
    let held = pictures.iter().find(|p| p["take"] == "B" && p["overlay"].is_null()).expect("the held image");
    let quote = pictures.iter().find(|p| p["take"] == "C").expect("the quote");
    let preview = footage.join("edit/preview.mp4");
    for (picture, wanted) in [(overlay, "red"), (held, "red"), (quote, "blue")] {
        let (r, g, b) = colour_at(&preview, middle(picture));
        let ok = match wanted {
            "red" => r > 150 && g < 90 && b < 90,
            _ => b > 150 && r < 90 && g < 90,
        };
        assert!(ok, "{wanted} expected at {:.2}, got {:?}: {picture}", middle(picture), (r, g, b));
    }
    // The overlay covers about the words two16..two19 (8.0-9.5 s in the take).
    let seconds = overlay["frames"].as_f64().unwrap() / 30.0;
    assert!((seconds - 1.9).abs() < 0.2, "{seconds}");
    let edl: Value = serde_json::from_slice(&fs::read(footage.join("edit/edl.json")).unwrap()).unwrap();
    assert_eq!(edl["overlays"][0]["source"], "B", "{edl}");

    // The quote is raised to the level of the take around it.
    assert!(record["gains"]["C"].as_f64().is_some_and(|gain| gain > 6.0), "{}", record["gains"]);
    assert!(report.contains("Speech levels matched to the main take: C +"), "{report}");
    let placed = record["placed"].as_array().unwrap();
    let stretch = |take: &str| {
        let item = placed.iter().find(|p| p["span"]["take"] == take).unwrap();
        let start = item["out_start"].as_f64().unwrap();
        (start + 0.5, start + item["samples"].as_f64().unwrap() / 48_000.0 - 0.5)
    };
    let (quote, main) = (loudness(&preview, stretch("C")), loudness(&preview, stretch("A")));
    assert!((quote - main).abs() < 4.0, "quote {quote:.1} LUFS, main {main:.1} LUFS");

    // An overlay must sit on words the cut keeps.
    let error = tools.call(
        "plan_cut",
        json!({"segments": [{"from": "A2"}], "overlays": [{"take": "B", "from": "A1"}]}),
        &|_| {},
    );
    assert!(error.is_error && text(&error).contains("which the cut does not keep"), "{}", text(&error));
}
