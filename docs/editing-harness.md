# Editing harness (`rushes-edit`)

Rushes' own AI editing tools. The coding agent (Claude Code, driven by `rushes-pipeline`)
cuts a footage folder only through these tools: it reads the takes as numbered phrases,
decides what to keep by phrase id, and the tools do the arithmetic, the cutting, the
rendering and the measuring. Its shell and web tools are for getting material (a clip of an
older video, a screenshot, a chart it makes), which it brings in with `add_footage`. The crate is synchronous and UI-free; ffmpeg and ffprobe do
the media work, ElevenLabs Scribe transcribes.

It replaces the [video-use](https://github.com/browser-use/video-use) skill as the default
harness (`EditHarness::Rushes`); video-use stays selectable in Settings › AI editing.

## How the agent gets the tools

The app's own binary serves them over MCP on stdio:

```text
rushes edit-tools --footage <dir>        # crates/app/src/main.rs, before any window opens
rushes-edit mcp --footage <dir>          # the same server from the crate's dev binary
rushes-edit call <tool> '<json>' --footage <dir>   # one tool call, for debugging
```

`rushes-pipeline` starts Claude Code with:

```text
--mcp-config={"mcpServers":{"rushes":{"type":"stdio","command":<rushes>,"args":["edit-tools","--footage",<dir>]}}}
--strict-mcp-config                         only this server, not the user's others
--tools=Bash,Read,Write,Edit,Glob,Grep,TodoWrite,WebFetch,WebSearch
--allowedTools=mcp__rushes,Bash,Read,Write,Edit,Glob,Grep,TodoWrite,WebFetch,WebSearch
--append-system-prompt=<crates/edit/src/instructions.md>[\n\n<creator's standing prompt>]
MCP_TOOL_TIMEOUT=3600000                    a final render may take minutes
```

The MCP server inherits the agent's environment, which carries `ELEVENLABS_API_KEY` (from
Settings, the environment, or a video-use install's `.env`).

The instructions keep the output in the tools' hands: the agent never cuts, joins or
renders with ffmpeg itself (only `render` writes `edit/final.mp4`, in sync and checked), never
changes the source takes, and keeps scratch files in `edit/work/`.

## Takes

Takes are the video, audio and image files directly in the footage folder (hidden files and
sub-folders skipped). A file gets an id (`A`, `B`, … `AA`) when first seen, in file-name order,
and keeps it: `edit/takes.json` maps ids to files, so a file added later never renames the
takes an existing plan refers to (it gets the next free id, and an id whose file is gone is
not reused). A folder edited before ids were kept takes them from the last render's
`edit/edl.json`. Images (`png`, `jpg`, `webp`, `bmp`) are takes without length or sound: they
are shown as overlays or held as time segments, and the app does not list them as footage.

## Tools

| Tool | What it does |
| --- | --- |
| `inventory` | Takes and their ids: length, size, rate, audio tracks, transcribed or not; images with their size. |
| `add_footage` | Copies a file (a path, e.g. `edit/downloads/clip.mp4`) or downloads a link into the footage folder: a direct link as it is, any other page with `yt-dlp` (up to 1080p) when it is installed. Files are put together in the hidden `.rushes-import/` and get a free name (`name (2).ext`) only when complete. Returns the new take's id. |
| `transcribe` | Scribe v2 for every take with sound without a current transcript (or only `takes`): verbatim, word times, speakers, sound tags. Parallel, cached in `edit/transcripts/<stem>.json` (+ `.rushes.json` with the source's size and time). |
| `read_transcript` | Phrases (`A12`: words without a 0.5 s pause), with the pause before each, sound tags, what the picture does in pauses of 3 s or more, fillers marked `~…~`, and hints: `{false start}` (cut-off word), `{said again in A14}` (the words come back within 120 s: a retake). Pages of 250 phrases. |
| `phrase_words` | The words of a phrase with times, for `starts_with` / `ends_with`. |
| `silences` | Pauses of at least N seconds with a picture-activity label from keyframe differences (still / changes a little / a lot / scene changes). |
| `shots` | The shots inside a stretch of a take (up to 10 min), each labelled by how much of the screen moves, with a contact sheet of them. Tells the payoff of a silent screen demo (a video playing full screen) from the preparation around it (opening a page, a small player, switching windows). |
| `look` | Contact sheet PNG (up to 16 frames + waveform) of a take or of the render; an image take as itself. |
| `plan_cut` | Sets the cut (`edit/cut.json`, `edit/plan.md`) and returns the receipt: output timeline, warnings. |
| `render` | Preview (720p, ≤30 fps, ultrafast) or final (source size and rate) to `edit/preview.mp4` / `edit/final.mp4`, `.srt` beside it, `edit/edl.json` (spans and overlays); then runs `check`. |
| `check` | Frame count and audio length against the plan, loudness, silences inside speech segments, audio placement at the joins. |

## Cut plans

The agent never types times for speech. A plan is a list of segments in output order:

```json
{
  "segments": [
    {"from": "A4", "to": "A9", "label": "hook"},
    {"from": "A12", "to": "A12", "starts_with": "but it turned out", "label": "turn"},
    {"take": "A", "start": 472.4, "end": 512.4, "label": "demo (silent)", "framing": "fit"},
    {"from": "C14", "to": "C15", "label": "quote from the 2025 video"},
    {"take": "D", "start": 0, "end": 3, "label": "title card (an image, held 3 s)"},
    {"from": "A30", "to": "A41", "label": "screen share", "punch_in": false, "zoom": 1.0}
  ],
  "overlays": [
    {"take": "B", "from": "A6", "to": "A7", "framing": "fit", "label": "MMLU chart"}
  ],
  "remove_fillers": true, "max_pause": 1.0, "tight_pause": 0.45, "sentence_pause": 0.7,
  "target_min_seconds": 480, "target_max_seconds": 600, "aspect": "9:16", "punch_in": true
}
```

`plan::resolve` turns it into spans:

- A speech segment keeps the spoken words of its phrases (fillers dropped when
  `remove_fillers`). Between two kept words, a pause longer than `max_pause` (1.0 s) is
  cut down to `tight_pause` (0.45 s), or `sentence_pause` (0.7 s) after a full stop; a pause
  holding a removed filler is always cut. Shorter pauses stay: trimming them saves little
  and shows as a jump cut. 55 % of a tightened pause stays after the first word, 45 %
  before the next.
- Edges are padded (0.10 s before the first word, 0.14 s after the last, never into a
  neighbouring word) and every cut moves to the quietest 5 ms window within 40 ms
  (`audio::Envelope::quietest` on the 16 kHz analysis copy).
- Time segments are kept as they are (edges snapped the same way). A time segment of an
  image holds it from 0 to `end` seconds (60 at most), silent.
- Segments may come from any take: a line from another video is a speech segment of its take.
- Framing: `zoom` (1-3, around `focus_x`/`focus_y`) per segment; `punch_in` on the plan
  alternates the segment's framing with a 12 % zoom-in at each jump cut inside speech
  segments, and a segment's own `punch_in` turns that off (a screen share) or on.
- Overlays show another picture while the cut's sound goes on: an image, or a video take
  from `start`. Each is anchored on words (`from`/`to` phrase ids, `starts_with`/`ends_with`)
  that must be in the cut; it covers the output from 0.1 s before the first of them to 0.3 s
  after the last. `fit` (default) shows the whole picture over a blurred copy of itself
  (zoomed: the part around the focus), `fill` crops it to the frame. Overlays on neighbouring
  words meet; overlays on the same words are an error.
- The receipt warns about: retakes kept twice (shared 4-grams between segments, or a kept
  phrase whose `said again` phrase is kept too), cut-off words kept, segments ending
  mid-sentence that the next one does not continue, long pauses squeezed where the picture
  changes a lot (a demo?), silent time segments of 20 s or more where the screen mostly stays
  for 3 s or more (preparation around a demo?), reused footage, very short segments, length
  against the target, overlays shorter than 1.5 s, a video overlay running past the end of
  its take (its last frame holds).

### Shots

`shots` decodes the stretch at 5 fps, 96x54 grey, and measures for each frame the share of
pixels that changed and the mean change. The share, averaged over 2 s, labels each moment:
most of the screen moving (15 % or more: a full-screen video, action) or the screen mostly
staying (a page, a cursor, a small player). Hard cuts (mean change 30+) split the calm
parts, as each is another page or window; inside a moving part they are the video's own
cuts. Then:

- a moving shot that calms down without a cut (a fade, a still clip) and cuts away within
  4 s keeps going to that cut, so a video ends where the screen leaves it;
- shots under 2.5 s join the longer neighbour, and alike neighbours without a cut between
  them become one;
- every boundary moves to the biggest picture change within 1 s of it (going full screen,
  switching windows), when there is a clear one (mean change 8+).

The agent sees the shot list and one frame per shot (more along long shots, 16 at most) and
keeps only the shots where the thing itself plays, taking a time segment's start and end
from the list.

## Rendering

`render::place` puts the spans on the output frame grid: span *i* gets
`round(cum_end·fps) − round(cum_start·fps)` frames and exactly the audio samples of those
frames, so sound and picture end every span together.

- Clock: everything (transcription audio, render audio, picture) is on the container's
  clock, the one players use. Audio is decoded with timestamp gaps filled
  (`aresample=async=1:min_hard_comp=0.001:first_pts=0`): Meld Studio, for one, leaves a
  22 ms gap after the first audio frame, and a plain decode closes it, which puts all later
  sound 22 ms early against the picture. Cached audio and our transcripts carry the clock
  version (`audio::CLOCK_VERSION`) and are made again when it changes.
- Output format: the size and rate of the video take the cut shows longest (an insert from
  another video is scaled to it), or of the first image at 30 fps when only images are on
  screen.
- Picture: the placed spans are split where overlays start and end (on the same frame grid),
  so each run of frames has one source: the span's take, or the overlay's (an image is looped
  at the output rate with `-loop 1 -framerate`). One ffmpeg per run, H.264 clips joined with
  the concat demuxer without re-encoding. Each clip is decoded from up to 0.25 s before the span and the `fps` filter
  starts at the span with `round=near`, so the frame nearest each cut is used even when it
  comes just before it (otherwise the picture runs up to a frame late). Then scale/pad, or
  crop for another aspect or a punch-in, optional grade and burnt-in captions shifted to
  output time. Clips are cached in `edit/.cache/render/` by what they contain, so a revised
  cut re-encodes only what changed; clips no render uses any more are deleted. Up to six
  encodes run at once.
- Sound: each take is decoded once to `edit/.cache/audio/<stem>.<clock>.48k.pcm`; spans are
  cut from it by sample index, with 10 ms fades at cuts, into one 32-bit float WAV. Each
  take's speech is brought to the level of the take with the most speech in the cut
  (measured on its kept words; up to ±20 dB; takes with under 2 s of kept words, such as a
  silent demo, stay as they are), so a line from another recording plays as loud as the
  rest; the render report lists the gains. Seeking AAC with
  ffmpeg instead shifts the sound by the encoder delay, and joining per-span AAC files
  drifts by the priming of every file (the video-use bug: about 20 ms per cut, 0.84 s late
  after 42 cuts in the benchmark).
- Mix: light speech compression (2.5:1 above −20 dBFS), two-pass `loudnorm` to −14 LUFS
  (true peak −1.5), `alimiter` at −2.5 dBFS (latency-compensated; at 192 kHz for finals, so
  peaks between samples are caught), trimmed to the exact sample count, AAC 192k,
  `+faststart`. The encoded file's true peak is measured; over −1 dBTP (AAC adds up to about
  a decibel) the sound is encoded again that much quieter.
- HDR (PQ/HLG) is tone-mapped to Rec.709; rotation is applied by ffmpeg; variable frame
  rates are read as the nearest broadcast rate (`Rational::standard`).

## Testing

```sh
cargo test -p rushes-edit
cargo clippy -p rushes-edit --all-targets
```

Unit tests cover filler and false-start detection, phrase splitting, retake hints, cut
times, frame/sample placement, overlay pictures, take ids, imports, captions, the MCP
protocol and the sync measurement. `tests/tools.rs` renders generated footage end to end,
with an image and a second clip brought in, an overlay, a held image and an insert. The
benchmark against video-use (real footage, agent runs, an independent evaluator) is in
[benchmarks.md](benchmarks.md).
