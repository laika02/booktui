# Changelog

## 0.2.0

- Switched playback to a native `rodio` backend
- Added click and drag support for the timeline and volume controls
- Added seek-by input with combined duration parsing and seek undo
- Added playback speed presets and an explicit sleep timer
- Added manual bookmarks with timeline markers and a bookmark picker
- Added `m4b` chapter ticks, next/previous chapter jumps, and a chapter picker
- Added configurable `memory_on_move` behavior for resume and bookmark state
- Added path completion and stronger text editing for popup inputs
- Improved small-window layout behavior and transport usability
- Hardened playback failure handling for broken media files
- Expanded README documentation and runtime behavior notes

## 0.1.0

- Initial Linux-first release of `booktui`
- Terminal audiobook playback with `ratatui`, `crossterm`, and native `rodio`
- Add library directories from inside the TUI
- Per-file library browsing with grouping by parent directory
- Sort modes for alphabetical, file date, and last played
- Metadata title extraction with filename fallback
- Resume position persistence and restore
- Idle auto-pause after 30 minutes with next-key auto-resume
- Clickable timeline scrubber and volume slider
- Filter/search inside the library
- Cached metadata scans for faster startup and rescans
