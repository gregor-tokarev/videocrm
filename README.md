<p align="center">
  <img src="assets/icon.png" width="96" alt="Rushes icon">
</p>

<h1 align="center">Rushes</h1>

<p align="center">A desktop production home for YouTube creators: plan videos, write scripts, edit with AI and upload.</p>

<p align="center">
  <a href="docs/youtube.md#connecting-a-real-youtube-account">Connect YouTube</a>
  ·
  <a href="docs/pipeline.md#requirements">Set up AI editing</a>
  ·
  <a href="docs/ARCHITECTURE.md">Architecture</a>
</p>

- **Plan** every video in a Notion-style database, as a table or a board.
- **Write** scripts in rich documents with B-roll and on-screen cues, word count and runtime.
  Mark the ideas you'd rather say in your own words as talking points instead of scripting
  every line.
- **Edit with AI**: hand your raw takes and script to the built-in AI editor. It drops
  retakes, false starts, fillers and dead air, keeps the screen demos viewers need to see, and
  shows you its plan before it cuts. Ask for changes in plain words: it can bring in a clip
  from your older video or a screenshot and show it over what you say. Long-form videos or
  vertical Shorts with captions.
- **Publish** with AI help: have the agent write the title, description and tags from your
  script and what you actually said on camera, and have Codex draw a few thumbnails to
  pick from.
- **Upload** to your YouTube channels from the video's page, with resumable uploads. Connect
  several accounts and pick, per video, the channel it goes to.

Everything stays on your computer: one local database and a folder per video.

## Download

Get the latest build from the repository's Releases page:

- **macOS** (Apple silicon and Intel): open `Rushes-….dmg` and drag Rushes to
  Applications. The app isn't notarized yet, so the first time, right-click it, choose
  **Open**, and confirm.
- **Linux** (x86_64): make `Rushes-….AppImage` executable and run it. Or unpack
  `rushes-…-linux-x86_64.tar.gz` into `~/.local` to get Rushes in your app menu:
  `tar -xzf rushes-…-linux-x86_64.tar.gz -C ~/.local --strip-components=1`.

Rushes keeps itself up to date: new versions download in the background and start the
next time you open it, or right away with **Restart to update**.

Rushes used to be called VideoCRM. A VideoCRM install updates itself to Rushes and keeps
your videos, scripts, settings and YouTube sign-ins; footage stays in its folder. The
`VideoCRM-…` files on the Releases page are only for that update.

AI editing also needs [Claude Code](https://claude.com/claude-code), ffmpeg and an
[ElevenLabs](https://elevenlabs.io) API key for transcription; the app checks for them and
says what's missing. Thumbnails need the [Codex CLI](https://github.com/openai/codex),
signed in.

## Build from source

Install Rust. On Linux, also install a C/C++ toolchain, CMake, `pkg-config`, and the
development libraries for Fontconfig, FreeType, X11, Wayland and xkbcommon.

```sh
cargo run --release
```
