# AI edit benchmark: Rushes' tools vs video-use

How the editing harness ([editing-harness.md](editing-harness.md)) is compared with the
[video-use](https://github.com/browser-use/video-use) skill. Both run inside the same
Claude Code, model and machine, through the real pipeline (`Phase::Auto`, the creator's
default brief); only the harness differs. Footage, transcripts and annotations stay out of
the repository (the recording is private); the tooling is in `scripts/bench/`.

## Material

One real recording by a creator: 33 minutes of Russian talking head (webcam) with
screen-share stretches, recorded in Meld Studio (1080p, variable 60 fps, AAC). It is what
an editor gets: retakes, false starts, fillers, 17 minutes of pauses, silent screen demos
next to silent window-hunting, a spoken instruction to the editor ("that was wrong, redo
this take, remove what I said about the file size") and a curse after the outro.

A hand-checked annotation (`gold/full.json`, 167 units) marks what must go (false starts,
abandoned takes, fumbling, the wrong claim, the instruction, the outtake), what must stay
(core and supporting content, the silent demos) and retakes where exactly one attempt
should stay.

## Tasks

| Task | Source | Brief |
| --- | --- | --- |
| excerpt-longform | first 9:20 | long-form video, natural length, remove fillers, false starts, retakes and dead air |
| full-longform | all 33:04 | the same |
| full-short | all 33:04 | YouTube Short, vertical, 45–60 s, burnt-in subtitles |

## Measurements (`scripts/bench/evaluate.py`)

Everything is measured from the output file, independently of what a harness reports:

- **Mapping**: where every piece of the output comes from in the source, from the audio
  (envelope search, Viterbi over candidate offsets, sample-level refinement) and, for
  stretches without speech, from frame matching.
- **A/V sync**: at up to 48 points, the source time of the sound against the source time of
  the picture (short frame runs matched against the source), with drift per join and per
  minute. This catches the known video-use renderer bug (audio slips ~25-35 ms per join).
- **Editorial**: share of annotated drop units removed (per category), core/normal content
  kept, silent demos kept, retakes kept exactly once, duplicated footage.
- **Craft**: joins, clipped words, cut edges inside words, clicks at joins, fillers and
  false-start tokens left, words per minute, dead air.
- **Delivery**: loudness (LUFS, true peak), validity, duration.
- **Cost**: wall time, agent turns, the agent's reported cost at API prices.

`scripts/bench/judge.py` adds a blind editorial comparison: a fresh model reads what the
viewer hears in each edit (the transcript inside each measured segment) next to the source
transcript and the brief, scores both and picks one; every pair is judged twice with the
order swapped.

## Running

```sh
cargo build --release -p rushes-edit -p rushes-pipeline --example bench_run
python3 scripts/bench/run_edits.py --bench <bench dir> --tasks excerpt-longform --runs 3 --parallel 2
python3 scripts/bench/evaluate.py --source … --reference … --gold … --output … --edl … --run-meta … --out result.json
python3 scripts/bench/compare.py <bench dir>/runs/*/*/*/result.json
python3 scripts/bench/judge.py --bench <bench dir> --task excerpt-longform
```

## Results (October 2026)

Claude Code 2.1.300 with Opus 5.5 for every run, video-use at `b877063` (its latest code),
one 12-core Linux machine with two runs at a time. Means over runs; the full table and
every run's measurements are in the benchmark folder's `results/`.

| | 9:20 clip, Rushes | video-use | 33 min, Rushes | video-use |
| --- | --- | --- | --- | --- |
| runs | 3 | 3 | 3 | 2 |
| core lines kept whole | 100% | 100% | 99% | 79% |
| silent demo time kept | 75% | 98% | 80% | 64% |
| annotated junk removed | 100% | 92% | 98% | 93% |
| preparation before the demo kept | 1% | 61% | 0% | 0% |
| words clipped at a cut | 1.3 | 3.3 | 13.0 | 28.5 |
| cut edges inside a word | 1.3 | 4.7 | 15.7 | 37.0 |
| fillers left | 0% | 0% | 2% | 16% |
| footage used twice (s) | 0 | 0 | 0 | 8.9 |
| dead air of 1 s or more, demos excluded (s) | 0.6 | 14.4 | 1.9 | 2.5 |
| worst sound-to-picture offset | 14 ms | 1.0 s | 31 ms | 6.7 s |
| sync drift per cut | −0.1 ms | −15.7 ms | 0.0 ms | −32.4 ms |
| model cost per edit (API prices) | $0.29 | $1.67 | $0.71 | $3.23 |
| wall time per edit | 6 min | 40 min | 16 min | 101 min |
| agent turns | 16 | 46 | 22 | 77 |

Shorts (vertical, 45-60 s, 2 runs each): both picked the same story; Rushes $0.34 and
3 min per Short against $1.60 and 15 min, with no clipped words (video-use 1.0 per Short).
Sync is not measured on the Shorts: they are crops of a nearly still talking head, the
picture matcher found 0-5 usable points per Short for every harness, and the numbers it
gave from those (up to seconds, either way) do not hold up. Checked directly, the sound
of every Rushes Short sits exactly where its edit list says.

Blind judge: Rushes' cut was preferred in 14 of 14 comparisons against video-use (6 on
the clip, 4 on the Shorts, 4 on the full recording; every pair judged in both orders).
Mean scores out of 10: 8.0 vs 7.2 (clip), 7.9 vs 6.2 (Shorts), 8.4 vs 4.5 (full). The
reasons it gave were mostly video-use's broken sentences (words cut out of the middle of
a line) and, on the full recording, lost content.

### Demos and the preparation around them

The recording's main demo is a montage of game mods playing full screen (40 s). Before
it, the creator spends 30 s opening the post with the video in a small player, and after
it goes back to the page and the recorder's window. The annotation counts the full-screen
part as the demo and the rest as junk. The first Rushes build kept the whole stretch as
one silent segment (73 s); it now has a `shots` tool (see
[editing-harness.md](editing-harness.md#shots)), and every Rushes run on both long-form
tasks starts the demo where the video goes full screen (472.3-472.4 s) and cuts the
preparation. video-use kept 38-94% of the preparation on the clip, kinocut 73-99%. Four of
the six Rushes runs also cut the montage's tail (keeping the first 28 s, the part the
speaker refers to), which is where the lower "silent demo time kept" comes from.

What drives the difference:

- **Sync.** video-use joins per-segment AAC files, and every join adds the encoder's
  priming: the sound slips 15-36 ms per cut, so a 200-cut edit ends 6.7 s out of sync.
  Rushes cuts picture and sound on one clock and stays within a frame.
- **Timing done in code.** The agent picks phrases; the tool cuts at word boundaries and
  quiet moments. video-use's agent writes cut times itself (in scripts it writes for each
  edit), which is where the clipped words, mid-word cuts and kept fillers come from.
- **Fewer turns.** The tools return what the agent needs (packed phrases with retake and
  false-start hints, receipts with warnings, measured checks), so it does not spend turns
  on scripts, ffmpeg commands and contact sheets of its own.

Where Rushes was weaker: less of the demo montage kept (its tail, by choice; see above),
one full-recording run that kept 12 s of the passage the creator retracted, and a few more
cut-off words left inside kept lines on the full recording (2.0 against 0.5 per edit),
though it removed more of the annotated false starts (96% against 91%). Some clipped words
are the evaluator's: at one join the "Не" of a retake is in the output (checked against
the source), but the evaluator placed the join after it because the abandoned take three
seconds earlier starts with the same "не"; the judge then read the line without it.

Fixed during the benchmark, then re-measured: a 22 ms offset from the recorder's audio
timestamp gap (sound decoded without filling it), picture up to a frame late at each cut,
true peaks over −1 dBTP, and the preparation kept around silent demos (the `shots` tool).
Every Rushes run in the tables above is from the build with all of these fixes.

Caveats: one recording, one language, one model; few runs per task. The judge is a Claude
model reading transcripts, not a person watching the videos. The size of the editorial
margins may change on other material; the sync, cost and time differences come from how
the two harnesses are built.

### Changes that need material from outside the footage

A creator's round of changes after an edit of the 9:20 clip: put in one line from their older
video right after they mention it, show a benchmark screenshot while they say the benchmarks
stopped improving, and no zoom on screen content. Two versions, one run each, after the
agent got its shell and the web (`add_footage`, overlays, per-segment zoom):

- the files already in `edit/downloads/` (as an earlier session left them): the agent added
  both, transcribed the old video, picked the one sentence that makes the point (7 s), put
  the screenshot over the two lines and turned punch-ins off on the screen content and the
  insert. 3.5 min, $0.84.
- nothing prepared: it downloaded the old video by its YouTube link with `add_footage`, found
  the same sentence, looked up published MMLU scores on the web, drew a chart of them, added
  it and showed it over the passage. 5.4 min, $1.07.

The first version showed the old video's line 8 dB quieter than the speech around it (that
recording is 15 dB quieter at the source); takes' speech levels are matched since. The
plain edits before the changes measured as before or better (more of the demo kept: 97%
against 75%); they cost $0.42 and $0.49 against $0.29, since the shell and web tools add to
every turn's prompt.

## Two more harnesses: kinocut and RankCut (October 2026)

The same tasks, footage, evaluator and judge, 2 runs per task each:

- **kinocut** 1.16.1 ([KyaniteLabs/kinocut](https://github.com/KyaniteLabs/kinocut), Apache-2.0):
  a guardrailed ffmpeg MCP server (203 tools) with an agent skill. Claude Code got its MCP
  server, its `kino` CLI and its SKILL.md, with the prompt the pipeline gives video-use
  (`scripts/bench/run_kinocut.py`). It transcribes with local Whisper.
- **RankCut** (Shah et al., IUI '26): the model ranks transcript sentences, a deterministic
  selector assembles them. No code was released; `scripts/bench/run_rankcut.py`
  reimplements the paper's three stages, redundancy filter and selector with the same
  model as the others, Scribe v2 transcripts and the rushes-edit renderer, so only the
  editing idea differs.

Full recording (33 min), means over runs:

| | Rushes | video-use | kinocut | RankCut |
| --- | --- | --- | --- | --- |
| core lines kept whole | 99% | 79% | 92% | 100% |
| annotated junk removed | 98% | 93% | 85% | 84% |
| false starts removed | 96% | 91% | 65% | 56% |
| silent demo time kept | 80% | 64% | 100% | 0% |
| words clipped at a cut | 13.0 | 28.5 | 50.0 | 8.0 |
| fillers left | 2% | 16% | 45% | 80% |
| dead air of 1 s or more (s) | 1.9 | 2.5 | 71.0 | 44.3 |
| worst sound-to-picture offset | 31 ms | 6.7 s | 6.6 s | 11 ms |
| loudness (LUFS) / true peak (dBTP) | −14.4 / −1.9 | −15.3 / −1.8 | −18.0 / +0.3 | −14.4 / −1.7 |
| model cost per edit | $0.71 | $3.23 | ≈$3.34 | $0.55 |
| wall time per edit | 16 min | 101 min | capped at 180 min | 15 min |

Blind judge, Rushes against each (every pair in both orders): 12 of 12 against kinocut,
12 of 12 against RankCut, on top of 14 of 14 against video-use. Mean scores out of 10 on
the full recording: Rushes 7.8-8.4, RankCut 5.0, kinocut 4.8, video-use 4.5.

- **kinocut** is thorough but slow and expensive: 102-106 min and about $3.40 on the 9-min
  clip, about an hour per Short, and both full-recording runs hit the 3-hour cap (each had
  rendered a complete edit and was stopped during its own checks; their cost is estimated
  from tokens). Whisper leaves fillers out of its transcript, so the agent cannot cut what
  it cannot see; joining with kinocut's merge drifted the sound in some runs; loudness was
  left at −15 to −20 LUFS with peaks over 0 dBTP on the full recording. Its `kino doctor` runs
  `npx hyperframes --version`, which installs HyperFrames' agent skills globally on first
  run (the runner now sets `HYPERFRAMES_SKIP_SKILLS=1`).
- **RankCut** is the cheapest and fastest and never loses a core line, but it cuts whole
  sentences: fillers and false starts inside a kept sentence stay, over a third of the retakes come
  out twice, the silent demos (no words to rank) are gone, and pauses between merged
  sentences stay. The idea fits highlight reels to a target length better than clean-ups.
