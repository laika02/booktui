# booktui

`booktui` is a Linux-first terminal audiobook player built with Rust, `ratatui`, `crossterm`, and `rodio`.

It is designed around local audiobook playback in the terminal: track directories inside the app, browse files quickly, resume reliably, and control playback with both keyboard and mouse without leaving the terminal.

## Highlights

- Add and untrack library directories from inside the TUI
- File-based audiobook library for `mp3` and `m4b`
- Native audio playback with pause, resume, seek, speed, and volume control
- Resume persistence with periodic checkpoints and normal-shutdown saves
- Idle auto-pause plus an explicit sleep timer
- Manual bookmarks with timeline markers
- `m4b` chapter detection with chapter ticks and chapter navigation
- Filtered browsing, grouped library display, and multiple sort modes
- Small-window-friendly layout and mouse support for timeline and volume

## Runtime Requirements

- Linux
- A working audio output device supported by `rodio` / `cpal`
- `ffprobe` in `PATH` if you want `m4b` chapter detection and reliable scanned duration metadata

For development from source:

- Rust toolchain

## Supported Formats

- `mp3`
- `m4b`

## Running

Run from source with cached dependencies:

```bash
cargo run --offline
```

Run from source with network access if dependencies are not cached:

```bash
cargo run
```

Run a prebuilt release binary:

```bash
./booktui
```

If you publish GitHub releases, attaching the built Linux binary lets users run `booktui` without installing Rust.

## Building A Release Binary

```bash
cargo build --release
cp target/release/booktui ./booktui
chmod +x ./booktui
```

## Controls

### Library And Playback

- `a`: add a library directory
- `d`: untrack the selected file's library directory
- `r`: rescan tracked directories
- `/`: filter the library
- `s`: cycle sort mode
- `Up` / `Down` or `j` / `k`: move selection
- `PageUp` / `PageDown`: move faster through the list
- `Enter`: play selected file
- `Space`: pause or resume playback
- `q`: quit

### Seek, Speed, Volume

- `Left` / `Right` or `h` / `l`: seek backward or forward
- `e`: open the "seek by" prompt
- `u`: undo the most recent committed seek
- `o` / `p`: slower or faster playback speed
- `-` / `=`: lower or raise volume

Seek input supports:

- `1m`: absolute seek to one minute
- `-30s`: relative backward seek
- `+1h2m3s`: relative forward seek

### Sleep Timer

- `t`: open the sleep timer prompt

Sleep timer input uses the same duration format as seek input, such as `15m`, `1h`, or `1h30m`.

The sleep timer only counts down while playback is actively running. It freezes while paused or stopped.

### Bookmarks

- `b`: add a bookmark at the current playback position
- `B`: open bookmarks for the current file

Bookmark popup:

- `Enter`: jump to bookmark
- `d`: delete bookmark
- `Up` / `Down` or `j` / `k`: move through bookmarks
- `Esc`: close

If the bookmark label is left empty, `booktui` uses the current timestamp as the label.

### Chapters

- `c`: jump to the previous chapter
- `v`: jump to the next chapter
- `C`: open the chapter picker for the current file

For `m4b` files, chapter ticks are shown on the timeline when chapter metadata can be read through `ffprobe`.

Chapter popup:

- `Enter`: jump to chapter
- `Up` / `Down` or `j` / `k`: move through chapters
- `Esc`: close

### Mouse

- Left click or drag on the timeline to seek
- Left click or drag on the volume bar to change volume

Timeline dragging is preview-based and commits on mouse release. Drag lock keeps timeline and volume drags from switching accidentally if the pointer drifts vertically.

### Text Input Popups

- `Tab`: complete directory paths
- `Left` / `Right`: move cursor
- `Home` / `End`: jump to start or end
- `Backspace` / `Delete`: delete characters
- `Ctrl-A` / `Ctrl-E`: move to start or end
- `Ctrl-U`: clear to the start of the line
- `Ctrl-W`: delete the previous word
- `Enter`: confirm
- `Esc`: cancel

## Configuration

Configuration lives at:

- `~/.config/booktui/config.toml`

Example:

```toml
library_dirs = ["/home/you/Audiobooks"]
default_volume = 70
idle_timeout = "30m"
memory_on_move = true
```

### Config Fields

- `library_dirs`: directories scanned for supported audio files
- `default_volume`: startup volume, from `0` to `100`
- `idle_timeout`: pause after this much time without keyboard interaction
- `memory_on_move`: if `true`, resume and similar state follow a file by `filename + size`; if `false`, state is strict path-based

`idle_timeout` uses the same duration format as seek and sleep timer input, for example `30m` or `1h`.

## State And Persistence

State is stored under XDG state paths when available:

- `~/.local/state/booktui/state.toml`: UI state such as selected item and sort mode
- `~/.local/state/booktui/resume.toml`: resume positions and last-played timestamps
- `~/.local/state/booktui/bookmarks.toml`: per-file bookmarks
- `~/.local/state/booktui/library-cache.toml`: cached metadata used to speed rescans

### Resume Behavior

- Resume checkpoints are saved every minute during active playback
- Resume is also saved on normal shutdown and on several playback transitions
- Unexpected termination is best-effort, with roughly one minute of worst-case progress loss

### Memory On Move

When `memory_on_move = true`:

- resume positions
- last-played ordering
- selected-item memory
- bookmarks

follow files using `filename + size` instead of full path.

This makes moves inside the library friendlier, but it can merge state for files that share the same filename and byte size. If you want strict path identity instead, set `memory_on_move = false`.

## Library Behavior

- The library is file-based, not folder-as-book based
- Files are grouped by parent directory in the library view
- Sort modes are alphabetical, file date, and last played
- Embedded title metadata is used when available, with filename fallback
- Metadata scanning is cached for faster subsequent rescans

## Operational Notes

- Idle auto-pause triggers only from lack of keyboard interaction
- Any keypress counts as interaction for the idle timer
- The sleep timer is separate from idle auto-pause
- Broken or unsupported files should fail with an in-app error instead of crashing the TUI
- `m4b` chapter detection and scanned duration metadata are best-effort and depend on what `ffprobe` can read from the file

## Project Status

`booktui` is already usable as a practical local audiobook player on Linux. The current codebase covers playback, persistence, library management, bookmarks, chapters, and terminal-first controls, and is in a reasonable state for packaging and release distribution.
