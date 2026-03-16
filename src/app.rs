use std::{
    cmp::Reverse,
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use ratatui::{Terminal, backend::CrosstermBackend, layout::Rect, widgets::ListState};

use crate::{
    library::{LibraryItem, scan_library},
    player::{Chapter, PlaybackSnapshot, Player},
    storage::{
        BookmarkEntry, BookmarkStore, Config, LibraryCache, ResumeEntry, ResumeStore, SortMode,
        Storage, UiState, canonical_key, duration_from_entry, media_key,
    },
    ui::{self, HitTarget},
};

const TICK_RATE: Duration = Duration::from_millis(250);
const RESUME_SAVE_INTERVAL: Duration = Duration::from_secs(60);
const SEEK_STEP: Duration = Duration::from_secs(10);
const VOLUME_STEP: u8 = 5;
const TOAST_DURATION: Duration = Duration::from_secs(4);
const SPEED_PRESETS: [f32; 5] = [0.25, 0.5, 1.0, 2.0, 4.0];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InputMode {
    Normal,
    AddDirectory,
    Search,
    Seek,
    Sleep,
    BookmarkLabel,
    BookmarkList,
    ChapterList,
}

#[derive(Clone, Debug)]
pub enum LibraryRow {
    GroupHeader { title: String, count: usize },
    Item { item_index: usize },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DragLock {
    Timeline { area: Rect },
    Volume { area: Rect },
}

#[derive(Clone, Copy)]
pub enum ToastLevel {
    Info,
    Success,
    Warning,
    Error,
}

struct Toast {
    message: String,
    level: ToastLevel,
    expires_at: Instant,
}

#[derive(Clone, Debug)]
pub struct BookmarkView {
    pub id: String,
    pub label: String,
    pub position: Duration,
}

pub struct App {
    storage: Storage,
    config: Config,
    ui_state: UiState,
    pub player: Player,
    pub library_items: Vec<LibraryItem>,
    pub list_state: ListState,
    pub input_mode: InputMode,
    pub input_buffer: String,
    pub input_cursor: usize,
    pub filter_query: String,
    pub idle_paused: bool,
    pub bookmark_list_state: ListState,
    pub chapter_list_state: ListState,
    toast: Option<Toast>,
    last_interaction_at: Instant,
    last_resume_save_at: Instant,
    resume_store: ResumeStore,
    bookmark_store: BookmarkStore,
    library_cache: LibraryCache,
    drag_lock: Option<DragLock>,
    seek_undo_stack: Vec<Duration>,
    preview_seek_position: Option<Duration>,
    idle_timeout: Duration,
    sleep_timer_remaining: Option<Duration>,
    last_sleep_tick_at: Instant,
}

impl App {
    pub fn load() -> Result<Self> {
        let storage = Storage::new()?;
        let config = storage.load_config()?;
        let ui_state = storage.load_ui_state()?;
        let resume_store = storage.load_resume_store()?;
        let bookmark_store = storage.load_bookmark_store()?;
        let mut library_cache = storage.load_library_cache()?;
        let library_items = scan_library(&dirs_from_config(&config), &mut library_cache);
        storage.save_library_cache(&library_cache)?;
        let mut list_state = ListState::default();
        let player = Player::new(config.default_volume)?;
        let idle_timeout = parse_duration_expr(&config.idle_timeout)
            .unwrap_or_else(|| Duration::from_secs(30 * 60));

        let mut app = Self {
            player,
            storage,
            config,
            ui_state,
            library_items,
            list_state: ListState::default(),
            input_mode: InputMode::Normal,
            input_buffer: String::new(),
            input_cursor: 0,
            filter_query: String::new(),
            idle_paused: false,
            bookmark_list_state: ListState::default(),
            chapter_list_state: ListState::default(),
            toast: Some(Toast {
                message: "Press 'a' to add a library directory.".to_owned(),
                level: ToastLevel::Info,
                expires_at: Instant::now() + TOAST_DURATION,
            }),
            last_interaction_at: Instant::now(),
            last_resume_save_at: Instant::now(),
            resume_store,
            bookmark_store,
            library_cache,
            drag_lock: None,
            seek_undo_stack: Vec::new(),
            preview_seek_position: None,
            idle_timeout,
            sleep_timer_remaining: None,
            last_sleep_tick_at: Instant::now(),
        };

        app.migrate_state_keys_if_needed()?;

        let selected = initial_selection(
            &app.library_items,
            app.sorted_filtered_indices(),
            app.ui_state.last_selected.as_deref(),
            app.config.memory_on_move,
        );
        list_state.select(app.selectable_row_index_for_item(selected));
        app.list_state = list_state;
        Ok(app)
    }

    pub fn run(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    ) -> Result<()> {
        loop {
            self.refresh_playback_state()?;
            self.handle_idle_timeout()?;
            self.handle_sleep_timer()?;
            self.clear_expired_toast();

            terminal.draw(|frame| ui::render(frame, self))?;

            if event::poll(TICK_RATE)? {
                match event::read()? {
                    Event::Key(key) if key.kind == KeyEventKind::Press => {
                        self.on_keypress()?;
                        if self.handle_key_event(key)? {
                            break;
                        }
                    }
                    Event::Mouse(mouse)
                        if matches!(
                            mouse.kind,
                            MouseEventKind::Down(MouseButton::Left)
                                | MouseEventKind::Drag(MouseButton::Left)
                        ) =>
                    {
                        self.last_interaction_at = Instant::now();
                        let area = terminal.size()?;
                        self.handle_mouse_event(area, mouse.column, mouse.row)?;
                    }
                    Event::Mouse(mouse)
                        if matches!(mouse.kind, MouseEventKind::Up(MouseButton::Left)) =>
                    {
                        self.handle_mouse_up();
                    }
                    _ => {}
                }
            }

            self.save_resume_checkpoint_if_needed()?;
        }

        Ok(())
    }

    pub fn shutdown(&mut self) -> Result<()> {
        self.persist_current_resume()?;
        self.persist_ui_state()?;
        self.player.stop()?;
        Ok(())
    }

    pub fn visible_rows(&self) -> Vec<LibraryRow> {
        let indices = self.sorted_filtered_indices();
        let mut rows = Vec::new();
        let mut current_group = String::new();
        let mut current_count = 0usize;
        let mut group_start = 0usize;

        for item_index in indices {
            let item = &self.library_items[item_index];
            if item.parent_label != current_group {
                current_group = item.parent_label.clone();
                current_count = 0;
                group_start = rows.len();
                rows.push(LibraryRow::GroupHeader {
                    title: current_group.clone(),
                    count: 0,
                });
            }
            current_count += 1;
            if let Some(LibraryRow::GroupHeader { count, .. }) = rows.get_mut(group_start) {
                *count = current_count;
            }
            rows.push(LibraryRow::Item { item_index });
        }

        rows
    }

    pub fn selected_item(&self) -> Option<&LibraryItem> {
        let row_index = self.list_state.selected()?;
        match self.visible_rows().get(row_index)? {
            LibraryRow::Item { item_index } => self.library_items.get(*item_index),
            LibraryRow::GroupHeader { .. } => None,
        }
    }

    pub fn playback_snapshot(&self) -> Option<PlaybackSnapshot> {
        let mut snapshot = self.player.snapshot()?;
        if let Some(preview) = self.preview_seek_position {
            snapshot.position = preview;
        }
        Some(snapshot)
    }

    pub fn current_duration(&self) -> Option<Duration> {
        self.player.duration().or_else(|| {
            self.selected_item()
                .and_then(|item| self.resume_duration_for(item.path.as_path()))
        })
    }

    pub fn status_line(&self) -> String {
        if self.idle_paused {
            return "Paused by idle timer. Any keypress resumes playback.".to_owned();
        }

        if let Some(remaining) = self.sleep_remaining() {
            return format!("Sleep timer: {}", ui::format_duration(remaining));
        }

        self.toast
            .as_ref()
            .map(|toast| toast.message.clone())
            .unwrap_or_else(|| "Ready.".to_owned())
    }

    pub fn status_level(&self) -> ToastLevel {
        if self.idle_paused {
            return ToastLevel::Warning;
        }
        self.toast
            .as_ref()
            .map(|toast| toast.level)
            .unwrap_or(ToastLevel::Info)
    }

    pub fn resume_label(&self, path: &Path) -> String {
        self.resume_entry_for(path)
            .map(duration_from_entry)
            .map(ui::format_duration)
            .unwrap_or_else(|| "Start".to_owned())
    }

    pub fn library_title(&self) -> String {
        let base = format!(
            "Library [{} roots | sort: {}]",
            self.config.library_dirs.len(),
            self.ui_state.sort_mode.label()
        );
        if self.filter_query.is_empty() {
            base
        } else {
            format!("{base} filter: {}", self.filter_query)
        }
    }

    pub fn input_title(&self) -> &'static str {
        match self.input_mode {
            InputMode::AddDirectory => "Add Directory",
            InputMode::Search => "Filter Library",
            InputMode::Seek => "Seek By",
            InputMode::Sleep => "Sleep Timer",
            InputMode::BookmarkLabel => "Add Bookmark",
            InputMode::BookmarkList => "Bookmarks",
            InputMode::ChapterList => "Chapters",
            InputMode::Normal => "",
        }
    }

    pub fn input_help(&self) -> &'static str {
        match self.input_mode {
            InputMode::AddDirectory => "Type a path. Tab completes. Enter saves. Esc cancels.",
            InputMode::Search => "Type to filter. Enter keeps it. Esc clears and exits.",
            InputMode::Seek => "Examples: 1m, -1m, +30s, +1h2m3s. Enter applies.",
            InputMode::Sleep => "Examples: 15m, 1h, 1h30m. Enter arms timer. Esc cancels.",
            InputMode::BookmarkLabel => {
                "Type a bookmark label. Leave empty to use the current timestamp."
            }
            InputMode::BookmarkList => "Enter jumps. d deletes. Esc closes.",
            InputMode::ChapterList => "Enter jumps. Esc closes.",
            InputMode::Normal => "",
        }
    }

    pub fn current_file_bookmarks(&self) -> Vec<BookmarkView> {
        let Some(path) = self.current_bookmark_path() else {
            return Vec::new();
        };
        let Some(entries) = self.bookmark_store.files.get(&self.state_key(path)) else {
            return Vec::new();
        };

        let mut bookmarks: Vec<BookmarkView> = entries
            .iter()
            .map(|entry| BookmarkView {
                id: entry.id.clone(),
                label: entry.label.clone(),
                position: Duration::from_secs_f64(entry.position_seconds.max(0.0)),
            })
            .collect();
        bookmarks.sort_by_key(|entry| entry.position);
        bookmarks
    }

    pub fn selected_bookmark(&self) -> Option<BookmarkView> {
        let index = self.bookmark_list_state.selected()?;
        self.current_file_bookmarks().get(index).cloned()
    }

    pub fn current_chapters(&self) -> &[Chapter] {
        self.player.chapters()
    }

    pub fn selected_chapter(&self) -> Option<Chapter> {
        let index = self.chapter_list_state.selected()?;
        self.player.chapters().get(index).cloned()
    }

    pub fn selected_root(&self) -> Option<&str> {
        let item = self.selected_item()?;
        self.library_root_for_path(item.path.as_path())
    }

    pub fn library_root_for_path(&self, path: &Path) -> Option<&str> {
        self.config
            .library_dirs
            .iter()
            .find(|root| path.starts_with(root.as_str()))
            .map(String::as_str)
    }

    pub fn save_resume_checkpoint_if_needed(&mut self) -> Result<()> {
        if !self.player.is_playing() || self.player.is_paused() {
            return Ok(());
        }

        if self.last_resume_save_at.elapsed() >= RESUME_SAVE_INTERVAL {
            self.persist_current_resume()?;
        }

        Ok(())
    }

    fn handle_key_event(&mut self, key: KeyEvent) -> Result<bool> {
        match self.input_mode {
            InputMode::Normal => self.handle_normal_mode(key),
            InputMode::AddDirectory => self.handle_add_directory_mode(key),
            InputMode::Search => self.handle_search_mode(key),
            InputMode::Seek => self.handle_seek_mode(key),
            InputMode::Sleep => self.handle_sleep_mode(key),
            InputMode::BookmarkLabel => self.handle_bookmark_label_mode(key),
            InputMode::BookmarkList => self.handle_bookmark_list_mode(key),
            InputMode::ChapterList => self.handle_chapter_list_mode(key),
        }
    }

    fn handle_normal_mode(&mut self, key: KeyEvent) -> Result<bool> {
        match key.code {
            KeyCode::Char('q') => return Ok(true),
            KeyCode::Char('a') => self.begin_input(InputMode::AddDirectory, String::new()),
            KeyCode::Char('/') => self.begin_input(InputMode::Search, self.filter_query.clone()),
            KeyCode::Char('e') => self.begin_input(InputMode::Seek, String::new()),
            KeyCode::Char('t') => self.begin_input(InputMode::Sleep, String::new()),
            KeyCode::Char('b') => self.begin_add_bookmark(),
            KeyCode::Char('B') => self.open_bookmark_list(),
            KeyCode::Char('c') => self.seek_chapter(false)?,
            KeyCode::Char('v') => self.seek_chapter(true)?,
            KeyCode::Char('C') => self.open_chapter_list(),
            KeyCode::Char('u') => self.undo_last_seek()?,
            KeyCode::Char('r') => self.rescan_library()?,
            KeyCode::Char('d') => self.remove_selected_root()?,
            KeyCode::Char('s') => self.cycle_sort_mode()?,
            KeyCode::Char('o') => self.adjust_speed(false),
            KeyCode::Char('p') => self.adjust_speed(true),
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::PageUp => self.move_selection(-10),
            KeyCode::PageDown => self.move_selection(10),
            KeyCode::Enter => self.play_selected()?,
            KeyCode::Char(' ') => {
                self.player.play_pause()?;
                self.persist_current_resume()?;
            }
            KeyCode::Left | KeyCode::Char('h') => {
                self.capture_seek_undo();
                self.player.seek_relative(SEEK_STEP, false)?;
                self.persist_current_resume()?;
            }
            KeyCode::Right | KeyCode::Char('l') => {
                self.capture_seek_undo();
                self.player.seek_relative(SEEK_STEP, true)?;
                self.persist_current_resume()?;
            }
            KeyCode::Char('-') => self.adjust_volume(false)?,
            KeyCode::Char('=') | KeyCode::Char('+') => self.adjust_volume(true)?,
            _ => {}
        }

        Ok(false)
    }

    fn handle_add_directory_mode(&mut self, key: KeyEvent) -> Result<bool> {
        if self.handle_shared_input_key(key)? {
            return Ok(false);
        }

        match key.code {
            KeyCode::Esc => {
                self.input_mode = InputMode::Normal;
                self.set_toast("Directory entry canceled.", ToastLevel::Info);
            }
            KeyCode::Enter => {
                self.commit_directory()?;
                self.input_mode = InputMode::Normal;
            }
            KeyCode::Tab => self.complete_directory_input()?,
            _ => {}
        }

        Ok(false)
    }

    fn handle_search_mode(&mut self, key: KeyEvent) -> Result<bool> {
        if self.handle_shared_input_key(key)? {
            self.apply_filter_from_input();
            return Ok(false);
        }

        match key.code {
            KeyCode::Esc => {
                self.filter_query.clear();
                self.input_buffer.clear();
                self.input_cursor = 0;
                self.input_mode = InputMode::Normal;
                self.refresh_selection_after_structure_change(None);
                self.set_toast("Library filter cleared.", ToastLevel::Info);
            }
            KeyCode::Enter => {
                self.apply_filter_from_input();
                self.input_mode = InputMode::Normal;
                if self.filter_query.is_empty() {
                    self.set_toast("Library filter cleared.", ToastLevel::Info);
                } else {
                    self.set_toast(
                        &format!("Filtering library by '{}'.", self.filter_query),
                        ToastLevel::Info,
                    );
                }
            }
            _ => {}
        }

        Ok(false)
    }

    fn handle_seek_mode(&mut self, key: KeyEvent) -> Result<bool> {
        if self.handle_shared_input_key(key)? {
            return Ok(false);
        }

        match key.code {
            KeyCode::Esc => {
                self.input_mode = InputMode::Normal;
                self.set_toast("Seek canceled.", ToastLevel::Info);
            }
            KeyCode::Enter => {
                self.apply_seek_input()?;
                self.input_mode = InputMode::Normal;
            }
            _ => {}
        }

        Ok(false)
    }

    fn handle_sleep_mode(&mut self, key: KeyEvent) -> Result<bool> {
        if self.handle_shared_input_key(key)? {
            return Ok(false);
        }

        match key.code {
            KeyCode::Esc => {
                self.input_mode = InputMode::Normal;
                self.set_toast("Sleep timer canceled.", ToastLevel::Info);
            }
            KeyCode::Enter => {
                self.apply_sleep_input();
                self.input_mode = InputMode::Normal;
            }
            _ => {}
        }

        Ok(false)
    }

    fn handle_bookmark_label_mode(&mut self, key: KeyEvent) -> Result<bool> {
        if self.handle_shared_input_key(key)? {
            return Ok(false);
        }

        match key.code {
            KeyCode::Esc => {
                self.input_mode = InputMode::Normal;
                self.set_toast("Bookmark creation canceled.", ToastLevel::Info);
            }
            KeyCode::Enter => {
                self.commit_bookmark()?;
                self.input_mode = InputMode::Normal;
            }
            _ => {}
        }

        Ok(false)
    }

    fn handle_bookmark_list_mode(&mut self, key: KeyEvent) -> Result<bool> {
        match key.code {
            KeyCode::Esc => {
                self.input_mode = InputMode::Normal;
                self.set_toast("Closed bookmarks.", ToastLevel::Info);
            }
            KeyCode::Up | KeyCode::Char('k') => self.move_bookmark_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_bookmark_selection(1),
            KeyCode::Enter => {
                self.jump_to_selected_bookmark()?;
                self.input_mode = InputMode::Normal;
            }
            KeyCode::Char('d') => self.delete_selected_bookmark()?,
            _ => {}
        }

        Ok(false)
    }

    fn handle_chapter_list_mode(&mut self, key: KeyEvent) -> Result<bool> {
        match key.code {
            KeyCode::Esc => {
                self.input_mode = InputMode::Normal;
                self.set_toast("Closed chapters.", ToastLevel::Info);
            }
            KeyCode::Up | KeyCode::Char('k') => self.move_chapter_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_chapter_selection(1),
            KeyCode::Enter => {
                self.jump_to_selected_chapter()?;
                self.input_mode = InputMode::Normal;
            }
            _ => {}
        }

        Ok(false)
    }

    fn on_keypress(&mut self) -> Result<()> {
        self.last_interaction_at = Instant::now();
        if self.idle_paused {
            self.player.resume()?;
            self.idle_paused = false;
            self.set_toast("Playback resumed after idle timeout.", ToastLevel::Info);
        }
        Ok(())
    }

    fn play_selected(&mut self) -> Result<()> {
        let Some(item) = self.selected_item().cloned() else {
            self.set_toast("No audiobook selected.", ToastLevel::Warning);
            return Ok(());
        };

        let start_at = self
            .resume_entry_for(item.path.as_path())
            .map(duration_from_entry)
            .unwrap_or(Duration::ZERO);

        match self.player.load(item.path.as_path(), start_at) {
            Ok(()) => {
                self.last_resume_save_at = Instant::now();
                self.set_toast(&format!("Playing {}", item.title), ToastLevel::Success);
            }
            Err(error) => {
                self.set_toast(
                    &format!("Failed to play {}: {}", item.title, error),
                    ToastLevel::Error,
                );
            }
        }
        Ok(())
    }

    fn refresh_playback_state(&mut self) -> Result<()> {
        if let Some(path) = self.player.tick()? {
            self.save_resume_for_path(path.as_path(), Duration::ZERO)?;
            self.set_toast(&format!("Finished {}", path.display()), ToastLevel::Info);
            self.idle_paused = false;
        }
        Ok(())
    }

    fn handle_idle_timeout(&mut self) -> Result<()> {
        if self.player.is_playing()
            && !self.player.is_paused()
            && self.last_interaction_at.elapsed() >= self.idle_timeout
        {
            self.player.pause()?;
            self.persist_current_resume()?;
            self.idle_paused = true;
            self.set_toast("Paused by idle timeout.", ToastLevel::Warning);
        }
        Ok(())
    }

    fn handle_sleep_timer(&mut self) -> Result<()> {
        let now = Instant::now();
        let elapsed = now.saturating_duration_since(self.last_sleep_tick_at);
        self.last_sleep_tick_at = now;

        if !self.player.is_playing() || self.player.is_paused() {
            return Ok(());
        }

        if let Some(remaining) = self.sleep_timer_remaining {
            if elapsed >= remaining {
                self.player.pause()?;
                self.persist_current_resume()?;
                self.sleep_timer_remaining = None;
                self.set_toast("Paused by sleep timer.", ToastLevel::Warning);
            } else {
                self.sleep_timer_remaining = Some(remaining - elapsed);
            }
        }
        Ok(())
    }

    fn begin_input(&mut self, mode: InputMode, initial: String) {
        self.input_mode = mode;
        self.input_buffer = initial;
        self.input_cursor = self.input_buffer.len();
        self.toast = None;
    }

    fn begin_add_bookmark(&mut self) {
        if self.player.current_file().is_none() {
            self.set_toast(
                "Start playback before adding a bookmark.",
                ToastLevel::Warning,
            );
            return;
        }
        self.begin_input(InputMode::BookmarkLabel, String::new());
    }

    fn open_bookmark_list(&mut self) {
        if self.player.current_file().is_none() {
            self.set_toast(
                "Start playback before opening bookmarks.",
                ToastLevel::Warning,
            );
            return;
        }

        let bookmarks = self.current_file_bookmarks();
        if bookmarks.is_empty() {
            self.set_toast("No bookmarks for the current file.", ToastLevel::Info);
            return;
        }

        self.bookmark_list_state.select(Some(0));
        self.input_mode = InputMode::BookmarkList;
        self.toast = None;
    }

    fn open_chapter_list(&mut self) {
        if self.player.current_file().is_none() {
            self.set_toast(
                "Start playback before opening chapters.",
                ToastLevel::Warning,
            );
            return;
        }

        if self.player.chapters().is_empty() {
            self.set_toast("No chapters for the current file.", ToastLevel::Info);
            return;
        }

        self.chapter_list_state.select(Some(0));
        self.input_mode = InputMode::ChapterList;
        self.toast = None;
    }

    fn handle_shared_input_key(&mut self, key: KeyEvent) -> Result<bool> {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('a') => {
                    self.input_cursor = 0;
                    return Ok(true);
                }
                KeyCode::Char('e') => {
                    self.input_cursor = self.input_buffer.len();
                    return Ok(true);
                }
                KeyCode::Char('u') => {
                    self.input_buffer.drain(..self.input_cursor);
                    self.input_cursor = 0;
                    return Ok(true);
                }
                KeyCode::Char('w') => {
                    self.delete_prev_word();
                    return Ok(true);
                }
                _ => {}
            }
        }

        match key.code {
            KeyCode::Left => {
                self.input_cursor = self.input_cursor.saturating_sub(1);
                Ok(true)
            }
            KeyCode::Right => {
                self.input_cursor = self
                    .input_cursor
                    .saturating_add(1)
                    .min(self.input_buffer.len());
                Ok(true)
            }
            KeyCode::Home => {
                self.input_cursor = 0;
                Ok(true)
            }
            KeyCode::End => {
                self.input_cursor = self.input_buffer.len();
                Ok(true)
            }
            KeyCode::Backspace => {
                self.delete_prev_char();
                Ok(true)
            }
            KeyCode::Delete => {
                self.delete_next_char();
                Ok(true)
            }
            KeyCode::Char(ch) => {
                self.input_buffer.insert(self.input_cursor, ch);
                self.input_cursor += 1;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    fn commit_directory(&mut self) -> Result<()> {
        let input = self.input_buffer.trim();
        if input.is_empty() {
            self.set_toast("Directory path is empty.", ToastLevel::Warning);
            return Ok(());
        }

        let path = expand_tilde(input);
        if !path.is_dir() {
            self.set_toast(
                &format!("Not a directory: {}", path.display()),
                ToastLevel::Warning,
            );
            return Ok(());
        }

        let canonical = path
            .canonicalize()
            .with_context(|| format!("failed to canonicalize {}", path.display()))?;
        let canonical_str = canonical.to_string_lossy().to_string();

        if self
            .config
            .library_dirs
            .iter()
            .any(|entry| entry == &canonical_str)
        {
            self.set_toast(
                "Directory already exists in the library.",
                ToastLevel::Warning,
            );
            return Ok(());
        }

        self.config.library_dirs.push(canonical_str);
        self.config.library_dirs.sort();
        self.storage.save_config(&self.config)?;
        self.rescan_library()?;
        self.set_toast(
            &format!("Added {}", canonical.display()),
            ToastLevel::Success,
        );
        self.input_buffer.clear();
        self.input_cursor = 0;
        Ok(())
    }

    fn complete_directory_input(&mut self) -> Result<()> {
        let input = self.input_buffer.trim();
        let expanded = expand_tilde(if input.is_empty() { "~/" } else { input });

        let (search_dir, prefix) = if expanded.as_os_str().is_empty() || expanded.is_dir() {
            (expanded.clone(), String::new())
        } else {
            let parent = expanded
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("."));
            let prefix = expanded
                .file_name()
                .and_then(OsStr::to_str)
                .unwrap_or_default()
                .to_owned();
            (parent, prefix)
        };

        let entries = match fs::read_dir(&search_dir) {
            Ok(entries) => entries,
            Err(_) => {
                self.set_toast(
                    &format!("Cannot read {}", search_dir.display()),
                    ToastLevel::Warning,
                );
                return Ok(());
            }
        };

        let mut matches = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir()
                && path
                    .file_name()
                    .and_then(OsStr::to_str)
                    .is_some_and(|name| name.starts_with(&prefix))
            {
                matches.push(path);
            }
        }
        matches.sort();

        if matches.is_empty() {
            self.set_toast("No matching directories.", ToastLevel::Info);
            return Ok(());
        }

        let new_path = if matches.len() == 1 {
            matches[0].clone()
        } else {
            let common = longest_common_prefix(&matches);
            if common.as_os_str().is_empty() {
                self.set_toast(
                    &format!("{} directory matches.", matches.len()),
                    ToastLevel::Info,
                );
                return Ok(());
            }
            common
        };

        self.input_buffer = display_input_path(&new_path);
        self.input_cursor = self.input_buffer.len();
        if matches.len() == 1 {
            self.set_toast(
                &format!("Completed {}", self.input_buffer),
                ToastLevel::Info,
            );
        } else {
            self.set_toast(&format!("{} matches", matches.len()), ToastLevel::Info);
        }
        Ok(())
    }

    fn apply_filter_from_input(&mut self) {
        let selected = self
            .selected_item()
            .map(|item| self.state_key(item.path.as_path()));
        self.filter_query = self.input_buffer.trim().to_owned();
        self.refresh_selection_after_structure_change(selected.as_deref());
    }

    fn rescan_library(&mut self) -> Result<()> {
        let selected = self
            .selected_item()
            .map(|item| self.state_key(item.path.as_path()));
        self.library_items = scan_library(&dirs_from_config(&self.config), &mut self.library_cache);
        self.storage.save_library_cache(&self.library_cache)?;
        self.refresh_selection_after_structure_change(selected.as_deref());
        self.persist_ui_state()?;
        self.set_toast(
            &format!(
                "Scanned {} audiobook files across {} roots.",
                self.library_items.len(),
                self.config.library_dirs.len()
            ),
            ToastLevel::Success,
        );
        Ok(())
    }

    fn remove_selected_root(&mut self) -> Result<()> {
        let Some(root) = self.selected_root().map(str::to_owned) else {
            self.set_toast(
                "Select a file to untrack its library directory.",
                ToastLevel::Warning,
            );
            return Ok(());
        };

        self.config.library_dirs.retain(|entry| entry != &root);
        self.storage.save_config(&self.config)?;
        self.rescan_library()?;
        self.set_toast(
            &format!("Untracked directory {}", root),
            ToastLevel::Success,
        );
        Ok(())
    }

    fn cycle_sort_mode(&mut self) -> Result<()> {
        let selected = self
            .selected_item()
            .map(|item| self.state_key(item.path.as_path()));
        self.ui_state.sort_mode = self.ui_state.sort_mode.next();
        self.storage.save_ui_state(&self.ui_state)?;
        self.refresh_selection_after_structure_change(selected.as_deref());
        self.set_toast(
            &format!("Sort mode: {}", self.ui_state.sort_mode.label()),
            ToastLevel::Info,
        );
        Ok(())
    }

    fn move_selection(&mut self, delta: isize) {
        let selectable = self.selectable_row_indices();
        if selectable.is_empty() {
            self.list_state.select(None);
            return;
        }

        let current_row = self.list_state.selected().unwrap_or(selectable[0]);
        let current_pos = selectable
            .iter()
            .position(|row| *row == current_row)
            .unwrap_or(0) as isize;
        let next_pos = (current_pos + delta).clamp(0, (selectable.len() - 1) as isize) as usize;
        self.list_state.select(Some(selectable[next_pos]));
        let _ = self.persist_ui_state();
    }

    fn adjust_volume(&mut self, increase: bool) -> Result<()> {
        let current = self.player.volume();
        let next = if increase {
            current.saturating_add(VOLUME_STEP)
        } else {
            current.saturating_sub(VOLUME_STEP)
        }
        .min(100);
        self.player.restart_with_volume(next)?;
        self.config.default_volume = next;
        self.storage.save_config(&self.config)?;
        Ok(())
    }

    fn set_volume(&mut self, value: u8) -> Result<()> {
        let next = value.min(100);
        self.player.restart_with_volume(next)?;
        self.config.default_volume = next;
        self.storage.save_config(&self.config)?;
        Ok(())
    }

    fn apply_seek_input(&mut self) -> Result<()> {
        let Some(parsed) = parse_seek_spec(self.input_buffer.trim()) else {
            self.set_toast(
                "Invalid seek. Use forms like 1m, -30s, +1h2m3s.",
                ToastLevel::Error,
            );
            return Ok(());
        };

        let current = self.player.current_position();
        let target = match parsed {
            SeekSpec::Absolute(duration) => duration,
            SeekSpec::RelativeForward(duration) => current.saturating_add(duration),
            SeekSpec::RelativeBackward(duration) => current.saturating_sub(duration),
        };

        self.capture_seek_undo();
        self.player.seek_to(target)?;
        self.persist_current_resume()?;
        self.set_toast(
            &format!("Seeked to {}", ui::format_duration(target)),
            ToastLevel::Success,
        );
        Ok(())
    }

    fn apply_sleep_input(&mut self) {
        let Some(duration) = parse_duration_expr(self.input_buffer.trim()) else {
            self.set_toast(
                "Invalid sleep timer. Use forms like 15m or 1h30m.",
                ToastLevel::Error,
            );
            return;
        };
        if duration.is_zero() {
            self.set_toast(
                "Sleep timer must be greater than zero.",
                ToastLevel::Warning,
            );
            return;
        }
        self.sleep_timer_remaining = Some(duration);
        self.last_sleep_tick_at = Instant::now();
        self.set_toast(
            &format!("Sleep timer set for {}", ui::format_duration(duration)),
            ToastLevel::Success,
        );
    }

    fn commit_bookmark(&mut self) -> Result<()> {
        let Some(path) = self.player.current_file().map(Path::to_path_buf) else {
            self.set_toast("No active file for bookmark.", ToastLevel::Warning);
            return Ok(());
        };

        let position = self.player.current_position();
        let key = self.state_key(path.as_path());
        let created_at = unix_epoch_now();
        let label = if self.input_buffer.trim().is_empty() {
            ui::format_duration(position)
        } else {
            self.input_buffer.trim().to_owned()
        };

        let entries = self.bookmark_store.files.entry(key).or_default();
        entries.push(BookmarkEntry {
            id: format!("bk-{created_at}-{}", entries.len()),
            position_seconds: position.as_secs_f64(),
            label: label.clone(),
            created_at_epoch_seconds: created_at,
        });
        entries.sort_by(|left, right| left.position_seconds.total_cmp(&right.position_seconds));
        self.storage.save_bookmark_store(&self.bookmark_store)?;
        self.input_buffer.clear();
        self.input_cursor = 0;
        self.set_toast(&format!("Saved bookmark '{}'.", label), ToastLevel::Success);
        Ok(())
    }

    fn handle_mouse_event(&mut self, area: Rect, column: u16, row: u16) -> Result<()> {
        let lock = self.drag_lock;
        let target = lock.or_else(|| match ui::hit_test(area, column, row) {
            Some(HitTarget::Timeline { area }) => Some(DragLock::Timeline { area }),
            Some(HitTarget::Volume { area }) => Some(DragLock::Volume { area }),
            None => None,
        });

        match target {
            Some(DragLock::Timeline { area }) => {
                let Some(duration) = self.player.duration() else {
                    return Ok(());
                };
                if self.drag_lock.is_none() {
                    self.capture_seek_undo();
                }
                self.drag_lock = Some(DragLock::Timeline { area });
                let ratio = ui::ratio_from_gauge_click(area, column);
                let target = Duration::from_secs_f64(duration.as_secs_f64() * ratio);
                self.preview_seek_position = Some(target);
                self.toast = None;
            }
            Some(DragLock::Volume { area }) => {
                self.drag_lock = Some(DragLock::Volume { area });
                let ratio = ui::ratio_from_gauge_click(area, column);
                let volume = (ratio * 100.0).round() as u8;
                self.set_volume(volume)?;
            }
            None => {}
        }
        Ok(())
    }

    fn handle_mouse_up(&mut self) {
        if let Some(target) = self.preview_seek_position.take()
            && self.player.seek_to(target).is_ok()
        {
            let _ = self.persist_current_resume();
            self.set_toast(
                &format!("Seeked to {}", ui::format_duration(target)),
                ToastLevel::Success,
            );
        }
        self.drag_lock = None;
    }

    fn capture_seek_undo(&mut self) {
        if self.player.current_file().is_some() {
            self.seek_undo_stack.push(self.player.current_position());
            if self.seek_undo_stack.len() > 3 {
                let overflow = self.seek_undo_stack.len() - 3;
                self.seek_undo_stack.drain(..overflow);
            }
        }
    }

    fn undo_last_seek(&mut self) -> Result<()> {
        let Some(target) = self.seek_undo_stack.pop() else {
            self.set_toast("No seek to undo.", ToastLevel::Warning);
            return Ok(());
        };
        self.player.seek_to(target)?;
        self.persist_current_resume()?;
        self.set_toast(
            &format!("Undid seek to {}", ui::format_duration(target)),
            ToastLevel::Success,
        );
        Ok(())
    }

    fn move_bookmark_selection(&mut self, delta: isize) {
        let bookmarks = self.current_file_bookmarks();
        if bookmarks.is_empty() {
            self.bookmark_list_state.select(None);
            return;
        }

        let current = self.bookmark_list_state.selected().unwrap_or(0) as isize;
        let next = (current + delta).clamp(0, (bookmarks.len() - 1) as isize) as usize;
        self.bookmark_list_state.select(Some(next));
    }

    fn move_chapter_selection(&mut self, delta: isize) {
        let chapters = self.player.chapters();
        if chapters.is_empty() {
            self.chapter_list_state.select(None);
            return;
        }

        let current = self.chapter_list_state.selected().unwrap_or(0) as isize;
        let next = (current + delta).clamp(0, (chapters.len() - 1) as isize) as usize;
        self.chapter_list_state.select(Some(next));
    }

    fn jump_to_selected_bookmark(&mut self) -> Result<()> {
        let Some(bookmark) = self.selected_bookmark() else {
            self.set_toast("No bookmark selected.", ToastLevel::Warning);
            return Ok(());
        };

        self.capture_seek_undo();
        self.player.seek_to(bookmark.position)?;
        self.persist_current_resume()?;
        self.set_toast(
            &format!("Jumped to bookmark '{}'.", bookmark.label),
            ToastLevel::Success,
        );
        Ok(())
    }

    fn delete_selected_bookmark(&mut self) -> Result<()> {
        let Some(path) = self.current_bookmark_path().map(Path::to_path_buf) else {
            self.set_toast("No active file for bookmarks.", ToastLevel::Warning);
            return Ok(());
        };
        let Some(bookmark) = self.selected_bookmark() else {
            self.set_toast("No bookmark selected.", ToastLevel::Warning);
            return Ok(());
        };

        let key = self.state_key(path.as_path());
        if let Some(entries) = self.bookmark_store.files.get_mut(&key) {
            entries.retain(|entry| entry.id != bookmark.id);
            if entries.is_empty() {
                self.bookmark_store.files.remove(&key);
            }
            self.storage.save_bookmark_store(&self.bookmark_store)?;
        }

        let remaining = self.current_file_bookmarks().len();
        if remaining == 0 {
            self.bookmark_list_state.select(None);
            self.input_mode = InputMode::Normal;
        } else {
            let selected = self
                .bookmark_list_state
                .selected()
                .unwrap_or(0)
                .min(remaining - 1);
            self.bookmark_list_state.select(Some(selected));
        }
        self.set_toast(
            &format!("Deleted bookmark '{}'.", bookmark.label),
            ToastLevel::Success,
        );
        Ok(())
    }

    fn seek_chapter(&mut self, forward: bool) -> Result<()> {
        if self.player.chapters().is_empty() {
            self.set_toast("No chapters for the current file.", ToastLevel::Info);
            return Ok(());
        }

        let current = self.player.current_position();
        let target = if forward {
            self.player
                .chapters()
                .iter()
                .find(|chapter| chapter.position > current.saturating_add(Duration::from_secs(1)))
                .cloned()
        } else {
            self.player
                .chapters()
                .iter()
                .rev()
                .find(|chapter| chapter.position.saturating_add(Duration::from_secs(1)) < current)
                .cloned()
        };

        let Some(chapter) = target else {
            self.set_toast("No more chapters in that direction.", ToastLevel::Info);
            return Ok(());
        };

        self.capture_seek_undo();
        self.player.seek_to(chapter.position)?;
        self.persist_current_resume()?;
        let label = chapter.title.as_deref().unwrap_or("chapter");
        self.set_toast(
            &format!(
                "Jumped to {} at {}.",
                label,
                ui::format_duration(chapter.position)
            ),
            ToastLevel::Success,
        );
        Ok(())
    }

    fn jump_to_selected_chapter(&mut self) -> Result<()> {
        let Some(chapter) = self.selected_chapter() else {
            self.set_toast("No chapter selected.", ToastLevel::Warning);
            return Ok(());
        };

        self.capture_seek_undo();
        self.player.seek_to(chapter.position)?;
        self.persist_current_resume()?;
        let label = chapter.title.as_deref().unwrap_or("chapter");
        self.set_toast(
            &format!(
                "Jumped to {} at {}.",
                label,
                ui::format_duration(chapter.position)
            ),
            ToastLevel::Success,
        );
        Ok(())
    }

    fn adjust_speed(&mut self, increase: bool) {
        let current = self.player.speed();
        let current_index = SPEED_PRESETS
            .iter()
            .position(|preset| (*preset - current).abs() < f32::EPSILON)
            .unwrap_or(2);
        let next_index = if increase {
            (current_index + 1).min(SPEED_PRESETS.len() - 1)
        } else {
            current_index.saturating_sub(1)
        };
        let next = SPEED_PRESETS[next_index];
        self.player.set_speed(next);
        self.set_toast(
            &format!("Speed set to {}x", format_speed(next)),
            ToastLevel::Info,
        );
    }

    fn set_toast(&mut self, message: &str, level: ToastLevel) {
        self.toast = Some(Toast {
            message: message.to_owned(),
            level,
            expires_at: Instant::now() + TOAST_DURATION,
        });
    }

    fn clear_expired_toast(&mut self) {
        if self
            .toast
            .as_ref()
            .is_some_and(|toast| Instant::now() >= toast.expires_at)
        {
            self.toast = None;
        }
    }

    fn sleep_remaining(&self) -> Option<Duration> {
        self.sleep_timer_remaining
            .filter(|remaining| !remaining.is_zero())
    }

    fn persist_ui_state(&mut self) -> Result<()> {
        self.ui_state.last_selected = self
            .selected_item()
            .map(|item| self.state_key(item.path.as_path()));
        self.storage.save_ui_state(&self.ui_state)
    }

    fn persist_current_resume(&mut self) -> Result<()> {
        let Some(path) = self.player.current_file().map(Path::to_path_buf) else {
            return Ok(());
        };
        let position = self.player.current_position();
        self.save_resume_for_path(path.as_path(), position)?;
        self.last_resume_save_at = Instant::now();
        Ok(())
    }

    fn save_resume_for_path(&mut self, path: &Path, position: Duration) -> Result<()> {
        let key = self.state_key(path);
        if self.config.memory_on_move {
            self.resume_store.positions.remove(&canonical_key(path));
        } else {
            self.resume_store.positions.remove(&media_key(path));
        }
        self.resume_store.positions.insert(
            key,
            ResumeEntry {
                position_seconds: position.as_secs_f64(),
                updated_at_epoch_seconds: unix_epoch_now(),
            },
        );
        self.storage.save_resume_store(&self.resume_store)
    }

    fn resume_duration_for(&self, path: &Path) -> Option<Duration> {
        self.resume_entry_for(path).map(duration_from_entry)
    }

    fn sorted_filtered_indices(&self) -> Vec<usize> {
        let mut indices: Vec<usize> = self
            .library_items
            .iter()
            .enumerate()
            .filter(|(_, item)| self.matches_filter(item))
            .map(|(index, _)| index)
            .collect();

        match self.ui_state.sort_mode {
            SortMode::Alphabetical => indices.sort_by(|left, right| {
                let a = &self.library_items[*left];
                let b = &self.library_items[*right];
                a.parent_label
                    .cmp(&b.parent_label)
                    .then_with(|| {
                        a.title
                            .to_ascii_lowercase()
                            .cmp(&b.title.to_ascii_lowercase())
                    })
                    .then_with(|| a.path.cmp(&b.path))
            }),
            SortMode::Date => indices.sort_by_key(|index| {
                let item = &self.library_items[*index];
                (
                    item.parent_label.clone(),
                    Reverse(item.modified_epoch_seconds),
                    item.title.to_ascii_lowercase(),
                )
            }),
            SortMode::LastPlayed => indices.sort_by_key(|index| {
                let item = &self.library_items[*index];
                let last = self
                    .resume_entry_for(item.path.as_path())
                    .map(|entry| entry.updated_at_epoch_seconds)
                    .unwrap_or(0);
                (
                    item.parent_label.clone(),
                    Reverse(last),
                    item.title.to_ascii_lowercase(),
                )
            }),
        }

        indices
    }

    fn matches_filter(&self, item: &LibraryItem) -> bool {
        if self.filter_query.is_empty() {
            return true;
        }

        let query = self.filter_query.to_ascii_lowercase();
        item.title.to_ascii_lowercase().contains(&query)
            || item
                .metadata_title
                .as_ref()
                .is_some_and(|title| title.to_ascii_lowercase().contains(&query))
            || item.parent_label.to_ascii_lowercase().contains(&query)
            || item
                .path
                .to_string_lossy()
                .to_ascii_lowercase()
                .contains(&query)
    }

    fn selectable_row_indices(&self) -> Vec<usize> {
        self.visible_rows()
            .iter()
            .enumerate()
            .filter_map(|(row, entry)| matches!(entry, LibraryRow::Item { .. }).then_some(row))
            .collect()
    }

    fn selectable_row_index_for_item(&self, item_index: Option<usize>) -> Option<usize> {
        let target = item_index?;
        self.visible_rows()
            .iter()
            .enumerate()
            .find_map(|(row, entry)| match entry {
                LibraryRow::Item { item_index } if *item_index == target => Some(row),
                _ => None,
            })
            .or_else(|| self.selectable_row_indices().first().copied())
    }

    fn refresh_selection_after_structure_change(&mut self, selected_key: Option<&str>) {
        let target_item = selected_key.and_then(|key| {
            self.sorted_filtered_indices().into_iter().find(|index| {
                self.state_key_matches(self.library_items[*index].path.as_path(), key)
            })
        });
        self.list_state.select(
            self.selectable_row_index_for_item(target_item)
                .or_else(|| self.selectable_row_indices().first().copied()),
        );
    }

    fn resume_entry_for(&self, path: &Path) -> Option<&ResumeEntry> {
        if self.config.memory_on_move {
            self.resume_store
                .positions
                .get(&media_key(path))
                .or_else(|| self.resume_store.positions.get(&canonical_key(path)))
        } else {
            self.resume_store.positions.get(&canonical_key(path))
        }
    }

    fn state_key(&self, path: &Path) -> String {
        if self.config.memory_on_move {
            media_key(path)
        } else {
            canonical_key(path)
        }
    }

    fn state_key_matches(&self, path: &Path, saved_key: &str) -> bool {
        state_key_matches(path, saved_key, self.config.memory_on_move)
    }

    fn current_bookmark_path(&self) -> Option<&Path> {
        self.player
            .current_file()
            .or_else(|| self.selected_item().map(|item| item.path.as_path()))
    }

    fn migrate_state_keys_if_needed(&mut self) -> Result<()> {
        if self.config.memory_on_move {
            return Ok(());
        }

        let mut resume_changed = false;
        let mut bookmark_changed = false;
        for item in &self.library_items {
            let media = media_key(item.path.as_path());
            let canonical = canonical_key(item.path.as_path());
            if let Some(entry) = self.resume_store.positions.remove(&media) {
                if !self.resume_store.positions.contains_key(&canonical) {
                    self.resume_store.positions.insert(canonical.clone(), entry);
                }
                resume_changed = true;
            }
            if let Some(entries) = self.bookmark_store.files.remove(&media) {
                self.bookmark_store
                    .files
                    .entry(canonical)
                    .or_default()
                    .extend(entries);
                bookmark_changed = true;
            }
        }

        if resume_changed {
            self.storage.save_resume_store(&self.resume_store)?;
        }
        if bookmark_changed {
            for entries in self.bookmark_store.files.values_mut() {
                entries.sort_by(|left, right| {
                    left.position_seconds.total_cmp(&right.position_seconds)
                });
            }
            self.storage.save_bookmark_store(&self.bookmark_store)?;
        }

        if let Some(saved) = self.ui_state.last_selected.clone() {
            for item in &self.library_items {
                if media_key(item.path.as_path()) == saved {
                    self.ui_state.last_selected = Some(canonical_key(item.path.as_path()));
                    self.storage.save_ui_state(&self.ui_state)?;
                    break;
                }
            }
        }

        Ok(())
    }

    fn delete_prev_char(&mut self) {
        if self.input_cursor == 0 {
            return;
        }
        self.input_cursor -= 1;
        self.input_buffer.remove(self.input_cursor);
    }

    fn delete_next_char(&mut self) {
        if self.input_cursor >= self.input_buffer.len() {
            return;
        }
        self.input_buffer.remove(self.input_cursor);
    }

    fn delete_prev_word(&mut self) {
        if self.input_cursor == 0 {
            return;
        }
        let bytes = self.input_buffer.as_bytes();
        let mut start = self.input_cursor;
        while start > 0 && bytes[start - 1].is_ascii_whitespace() {
            start -= 1;
        }
        while start > 0 && !bytes[start - 1].is_ascii_whitespace() {
            start -= 1;
        }
        self.input_buffer.drain(start..self.input_cursor);
        self.input_cursor = start;
    }
}

fn dirs_from_config(config: &Config) -> Vec<PathBuf> {
    config.library_dirs.iter().map(PathBuf::from).collect()
}

fn initial_selection(
    items: &[LibraryItem],
    indices: Vec<usize>,
    selected_path: Option<&str>,
    memory_on_move: bool,
) -> Option<usize> {
    selected_path
        .and_then(|path| {
            indices
                .iter()
                .copied()
                .find(|index| state_key_matches(items[*index].path.as_path(), path, memory_on_move))
        })
        .or_else(|| indices.first().copied())
}

fn state_key_matches(path: &Path, saved_key: &str, memory_on_move: bool) -> bool {
    if memory_on_move {
        media_key(path) == saved_key || canonical_key(path) == saved_key
    } else {
        canonical_key(path) == saved_key
    }
}

fn unix_epoch_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
}

fn expand_tilde(input: &str) -> PathBuf {
    if input == "~" {
        return home_dir().unwrap_or_else(|| PathBuf::from(input));
    }
    if let Some(stripped) = input.strip_prefix("~/")
        && let Some(home) = home_dir()
    {
        return home.join(stripped);
    }
    PathBuf::from(input)
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

fn display_input_path(path: &Path) -> String {
    let Some(home) = home_dir() else {
        return display_path_with_dir_hint(path);
    };

    if path == home {
        return "~/".to_owned();
    }

    if let Ok(stripped) = path.strip_prefix(&home) {
        let suffix = stripped.to_string_lossy();
        return if suffix.is_empty() {
            "~/".to_owned()
        } else if path.is_dir() {
            format!("~/{}", ensure_trailing_slash(&suffix))
        } else {
            format!("~/{}", suffix)
        };
    }
    display_path_with_dir_hint(path)
}

fn display_path_with_dir_hint(path: &Path) -> String {
    let text = path.to_string_lossy().to_string();
    if path.is_dir() {
        ensure_trailing_slash(&text)
    } else {
        text
    }
}

fn longest_common_prefix(paths: &[PathBuf]) -> PathBuf {
    let Some(first) = paths.first() else {
        return PathBuf::new();
    };
    let first_name = first
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or_default()
        .to_owned();
    let mut prefix_len = first_name.len();
    for path in &paths[1..] {
        let name = path.file_name().and_then(OsStr::to_str).unwrap_or_default();
        prefix_len = common_prefix_len(&first_name[..prefix_len], name);
        if prefix_len == 0 {
            break;
        }
    }
    let parent = first
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(PathBuf::new);
    if prefix_len == 0 {
        return PathBuf::new();
    }
    parent.join(&first_name[..prefix_len])
}

fn common_prefix_len(left: &str, right: &str) -> usize {
    left.bytes()
        .zip(right.bytes())
        .take_while(|(a, b)| a == b)
        .count()
}

fn ensure_trailing_slash(text: &str) -> String {
    if text.ends_with('/') {
        text.to_owned()
    } else {
        format!("{text}/")
    }
}

enum SeekSpec {
    Absolute(Duration),
    RelativeForward(Duration),
    RelativeBackward(Duration),
}

fn parse_seek_spec(input: &str) -> Option<SeekSpec> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }

    let (mode, rest) = if let Some(rest) = trimmed.strip_prefix('+') {
        (1i8, rest)
    } else if let Some(rest) = trimmed.strip_prefix('-') {
        (-1i8, rest)
    } else {
        (0i8, trimmed)
    };

    let duration = parse_duration_expr(rest)?;
    if duration.is_zero() {
        return None;
    }

    Some(match mode {
        1 => SeekSpec::RelativeForward(duration),
        -1 => SeekSpec::RelativeBackward(duration),
        _ => SeekSpec::Absolute(duration),
    })
}

fn parse_duration_expr(input: &str) -> Option<Duration> {
    let mut chars = input.chars().peekable();
    let mut total_seconds = 0u64;
    let mut saw_unit = false;

    while chars.peek().is_some() {
        let mut number = String::new();
        while let Some(ch) = chars.peek() {
            if ch.is_ascii_digit() {
                number.push(*ch);
                chars.next();
            } else {
                break;
            }
        }

        if number.is_empty() {
            return None;
        }

        let value: u64 = number.parse().ok()?;
        let unit = chars.next()?;
        let seconds = match unit {
            'h' | 'H' => value.checked_mul(3600)?,
            'm' | 'M' => value.checked_mul(60)?,
            's' | 'S' => value,
            _ => return None,
        };
        total_seconds = total_seconds.checked_add(seconds)?;
        saw_unit = true;
    }

    saw_unit.then(|| Duration::from_secs(total_seconds))
}

fn format_speed(speed: f32) -> String {
    if (speed.fract()).abs() < f32::EPSILON {
        format!("{speed:.0}")
    } else {
        format!("{speed:.2}")
            .trim_end_matches('0')
            .trim_end_matches('.')
            .to_owned()
    }
}
