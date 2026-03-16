# booktui

`booktui` is a Linux-first terminal audiobook player built with Rust, `ratatui`, `crossterm`, and `rodio`.

It focuses on practical local playback: add directories inside the TUI, browse files, resume where you left off, seek and change volume from the keyboard or mouse, and keep the UI usable even in smaller terminal windows.

## Features

- Add local library directories from inside the TUI
- Browse audiobook files directly in the terminal
- Native audio playback with seek, pause, resume, and volume control
- Resume playback position when reopening a file
- Auto-save playback position every minute and on normal shutdown
- Auto-pause after 30 minutes without keyboard interaction
- Clickable timeline scrubber and volume slider
- Library filter/search
- Grouping by parent directory
- Sort modes: alphabetical, file date, last played
- Metadata-based titles with filename fallback
- Cached metadata scans for faster startup and rescans

## Supported Formats

- `mp3`
- `m4b`

## Requirements

- Linux
- Rust toolchain
- Working audio output device supported by `rodio` / `cpal`

## Quick Start

Run with cached dependencies:

```bash
cargo run --offline
```

If dependencies are not cached locally yet:

```bash
cargo run
```

## Controls

- `a`: add library directory
- `/`: filter library
- `s`: cycle sort mode
- `d`: remove the selected file's library root
- `r`: rescan library
- `Up` / `Down` or `j` / `k`: move selection
- `PageUp` / `PageDown`: move faster through the list
- `Enter`: play selected file
- `Space`: pause/resume
- `Left` / `Right` or `h` / `l`: seek backward/forward
- `-` / `=`: volume down/up
- `q`: quit

Mouse:
- Left click or drag on the timeline to seek
- Left click or drag on the volume bar to adjust volume

Text input popups:
- `Tab`: complete directory paths
- `Left` / `Right`: move cursor
- `Home` / `End`: jump to start/end
- `Backspace` / `Delete`: delete characters
- `Ctrl-A` / `Ctrl-E`: move to start/end
- `Ctrl-U`: clear to start of line
- `Ctrl-W`: delete previous word
- `Enter`: confirm
- `Esc`: cancel

## Data Files

State is stored under XDG paths when available:

- config: `~/.config/booktui/config.toml`
- state: `~/.local/state/booktui/state.toml`
- resume data: `~/.local/state/booktui/resume.toml`
- library metadata cache: `~/.local/state/booktui/library-cache.toml`

## Notes

- Resume-on-crash is best-effort between checkpoints, so the worst-case loss is roughly one minute.
- Titles come from embedded metadata when available; otherwise the filename is used.
- The library is file-based rather than folder-as-book based.

## Project Status

`booktui` is currently aimed at local playback and terminal-first UX on Linux. The codebase is already structured around persistence, sorting, grouping, cached scans, and a native audio backend, which makes it a reasonable base for further polish or packaging work.
