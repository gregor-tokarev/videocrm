//! Runs one single-pass AI edit (the pipeline's `Phase::Auto`) for the benchmark in
//! `scripts/bench/` and records what it cost. With `--session` and `--feedback`, runs a
//! round of changes (`Phase::Feedback`) in that agent session instead.
//!
//! cargo run --release -p rushes-pipeline --example bench_run -- \
//!   --harness rushes|video-use --footage <dir> --brief <file> --out <run.json>
//!   [--model <m>] [--tools <rushes-edit binary>] [--skill <video-use dir>] [--label <text>]
//!   [--session <id> --feedback <file>]
//!
//! Writes `<run.json>` (harness, wall time, cost, turns, outcome) and the agent's events
//! next to it (`events.jsonl`).

use std::fs;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use rushes_pipeline::{AgentConfig, AgentEvent, Harness, Phase, RunContext, ToolServer, run_phase};

fn main() {
    let mut harness = String::from("rushes");
    let (mut footage, mut brief, mut out, mut model, mut tools, mut skill, mut label) =
        (None, None, None, None, None, None, String::new());
    let (mut session, mut feedback) = (None, None);
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        let value = args.next().unwrap_or_else(|| panic!("{flag} needs a value"));
        match flag.as_str() {
            "--harness" => harness = value,
            "--footage" => footage = Some(PathBuf::from(value)),
            "--brief" => brief = Some(fs::read_to_string(value).expect("brief file")),
            "--out" => out = Some(PathBuf::from(value)),
            "--model" => model = Some(value),
            "--tools" => tools = Some(PathBuf::from(value)),
            "--skill" => skill = Some(PathBuf::from(value)),
            "--label" => label = value,
            "--session" => session = Some(value),
            "--feedback" => feedback = Some(fs::read_to_string(value).expect("feedback file")),
            other => panic!("unknown flag {other}"),
        }
    }
    let footage = footage.expect("--footage");
    let out = out.expect("--out");
    let harness_value = match harness.as_str() {
        "rushes" => Harness::Rushes {
            tools: ToolServer { program: tools.expect("--tools <rushes-edit binary>"), args: vec!["mcp".to_owned()] },
        },
        "video-use" => Harness::VideoUse { skill_dir: skill.expect("--skill <video-use dir>") },
        other => panic!("unknown harness {other}"),
    };
    let config = AgentConfig { model: model.clone(), ..AgentConfig::default() };
    let ctx = RunContext { footage_dir: footage.clone(), harness: harness_value, session_id: session };
    let phase = match feedback {
        Some(message) => Phase::Feedback { message },
        None => Phase::Auto { brief: brief.unwrap_or_default() },
    };
    let events_path = out.with_file_name("events.jsonl");
    let mut events = fs::File::create(&events_path).expect("events file");
    let cancel = AtomicBool::new(false);
    let started = Instant::now();
    let mut turns = None;
    let mut agent_ms = None;
    let result = run_phase(
        &config,
        &ctx,
        phase,
        &mut |event| {
            if let AgentEvent::Result { num_turns, duration_ms, .. } = event {
                turns = *num_turns;
                agent_ms = *duration_ms;
            }
            let _ = writeln!(events, "{}", serde_json::to_string(event).unwrap_or_default());
        },
        &cancel,
    );
    let wall = started.elapsed().as_secs_f64();
    let record = match &result {
        Ok(outcome) => serde_json::json!({
            "harness": harness, "label": label, "model": model, "footage": footage,
            "wall_seconds": wall, "agent_ms": agent_ms, "num_turns": turns,
            "cost_usd": outcome.cost_usd, "is_error": outcome.is_error, "rendered": outcome.rendered,
            "final_cut": outcome.final_cut, "summary": outcome.summary_text, "session_id": outcome.session_id,
        }),
        Err(error) => serde_json::json!({
            "harness": harness, "label": label, "model": model, "footage": footage,
            "wall_seconds": wall, "agent_ms": agent_ms, "num_turns": turns, "error": error.to_string(),
        }),
    };
    fs::write(&out, serde_json::to_vec_pretty(&record).unwrap()).expect("write run.json");
    println!("{}", serde_json::to_string_pretty(&record).unwrap());
}
