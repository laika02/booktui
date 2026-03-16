# Changelog

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
