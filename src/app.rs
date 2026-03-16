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
    player::{PlaybackSnapshot, Player},
    storage::{
        Config, LibraryCache, ResumeEntry, ResumeStore, SortMode, Storage, UiState, canonical_key,
        duration_from_entry,
    },
    ui::{self, HitTarget},
};

const TICK_RATE: Duration = Duration::from_millis(250);
const RESUME_SAVE_INTERVAL: Duration = Duration::from_secs(60);
const IDLE_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const SEEK_STEP: Duration = Duration::from_secs(10);
const VOLUME_STEP: u8 = 5;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InputMode {
    Normal,
    AddDirectory,
    Search,
}

#[derive(Clone, Debug)]
pub enum LibraryRow {
    GroupHeader { title: String, count: usize },
    Item { item_index: usize },
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
    status: String,
    last_interaction_at: Instant,
    last_resume_save_at: Instant,
    resume_store: ResumeStore,
    library_cache: LibraryCache,
}

impl App {
    pub fn load() -> Result<Self> {
        let storage = Storage::new()?;
        let config = storage.load_config()?;
        let ui_state = storage.load_ui_state()?;
        let resume_store = storage.load_resume_store()?;
        let mut library_cache = storage.load_library_cache()?;
        let library_items = scan_library(&dirs_from_config(&config), &mut library_cache);
        storage.save_library_cache(&library_cache)?;
        let mut list_state = ListState::default();
        let player = Player::new(config.default_volume)?;

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
            status: "Press 'a' to add a library directory.".to_owned(),
            last_interaction_at: Instant::now(),
            last_resume_save_at: Instant::now(),
            resume_store,
            library_cache,
        };

        let selected = initial_selection(
            &app.library_items,
            app.sorted_filtered_indices(),
            app.ui_state.last_selected.as_deref(),
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
        self.player.snapshot()
    }

    pub fn current_duration(&self) -> Option<Duration> {
        self.player.duration().or_else(|| {
            self.selected_item()
                .and_then(|item| self.resume_duration_for(item.path.as_path()))
        })
    }

    pub fn status_line(&self) -> String {
        if self.idle_paused {
            format!("{} Auto-resume is armed.", self.status)
        } else {
            self.status.clone()
        }
    }

    pub fn resume_label(&self, path: &Path) -> String {
        self.resume_store
            .positions
            .get(&canonical_key(path))
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
            InputMode::Normal => "",
        }
    }

    pub fn input_help(&self) -> &'static str {
        match self.input_mode {
            InputMode::AddDirectory => "Type a path. Tab completes. Enter saves. Esc cancels.",
            InputMode::Search => "Type to filter. Enter keeps it. Esc clears and exits.",
            InputMode::Normal => "",
        }
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
        }
    }

    fn handle_normal_mode(&mut self, key: KeyEvent) -> Result<bool> {
        match key.code {
            KeyCode::Char('q') => return Ok(true),
            KeyCode::Char('a') => self.begin_input(InputMode::AddDirectory, String::new()),
            KeyCode::Char('/') => self.begin_input(InputMode::Search, self.filter_query.clone()),
            KeyCode::Char('r') => self.rescan_library()?,
            KeyCode::Char('d') => self.remove_selected_root()?,
            KeyCode::Char('s') => self.cycle_sort_mode()?,
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
                self.player.seek_relative(SEEK_STEP, false)?;
                self.persist_current_resume()?;
            }
            KeyCode::Right | KeyCode::Char('l') => {
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
                self.status = "Directory entry canceled.".to_owned();
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
                self.status = "Library filter cleared.".to_owned();
            }
            KeyCode::Enter => {
                self.apply_filter_from_input();
                self.input_mode = InputMode::Normal;
                self.status = if self.filter_query.is_empty() {
                    "Library filter cleared.".to_owned()
                } else {
                    format!("Filtering library by '{}'.", self.filter_query)
                };
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
            self.status = "Playback resumed after idle timeout.".to_owned();
        }
        Ok(())
    }

    fn play_selected(&mut self) -> Result<()> {
        let Some(item) = self.selected_item().cloned() else {
            self.status = "No audiobook selected.".to_owned();
            return Ok(());
        };

        let start_at = self
            .resume_store
            .positions
            .get(&canonical_key(item.path.as_path()))
            .map(duration_from_entry)
            .unwrap_or(Duration::ZERO);

        self.player.load(item.path.as_path(), start_at)?;
        self.last_resume_save_at = Instant::now();
        self.status = format!("Playing {}", item.title);
        Ok(())
    }

    fn refresh_playback_state(&mut self) -> Result<()> {
        if let Some(path) = self.player.tick()? {
            self.save_resume_for_path(path.as_path(), Duration::ZERO)?;
            self.status = format!("Finished {}", path.display());
            self.idle_paused = false;
        }
        Ok(())
    }

    fn handle_idle_timeout(&mut self) -> Result<()> {
        if self.player.is_playing()
            && !self.player.is_paused()
            && self.last_interaction_at.elapsed() >= IDLE_TIMEOUT
        {
            self.player.pause()?;
            self.persist_current_resume()?;
            self.idle_paused = true;
            self.status = "Paused after 30 minutes without input.".to_owned();
        }
        Ok(())
    }

    fn begin_input(&mut self, mode: InputMode, initial: String) {
        self.input_mode = mode;
        self.input_buffer = initial;
        self.input_cursor = self.input_buffer.len();
        self.status.clear();
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
            self.status = "Directory path is empty.".to_owned();
            return Ok(());
        }

        let path = expand_tilde(input);
        if !path.is_dir() {
            self.status = format!("Not a directory: {}", path.display());
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
            self.status = "Directory already exists in the library.".to_owned();
            return Ok(());
        }

        self.config.library_dirs.push(canonical_str);
        self.config.library_dirs.sort();
        self.storage.save_config(&self.config)?;
        self.rescan_library()?;
        self.status = format!("Added {}", canonical.display());
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
                self.status = format!("Cannot read {}", search_dir.display());
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
            self.status = "No matching directories.".to_owned();
            return Ok(());
        }

        let new_path = if matches.len() == 1 {
            matches[0].clone()
        } else {
            let common = longest_common_prefix(&matches);
            if common.as_os_str().is_empty() {
                self.status = format!("{} directory matches.", matches.len());
                return Ok(());
            }
            common
        };

        self.input_buffer = display_input_path(&new_path);
        self.input_cursor = self.input_buffer.len();
        self.status = if matches.len() == 1 {
            format!("Completed {}", self.input_buffer)
        } else {
            format!("{} matches", matches.len())
        };
        Ok(())
    }

    fn apply_filter_from_input(&mut self) {
        let selected = self
            .selected_item()
            .map(|item| canonical_key(item.path.as_path()));
        self.filter_query = self.input_buffer.trim().to_owned();
        self.refresh_selection_after_structure_change(selected.as_deref());
    }

    fn rescan_library(&mut self) -> Result<()> {
        let selected = self
            .selected_item()
            .map(|item| canonical_key(item.path.as_path()));
        self.library_items = scan_library(&dirs_from_config(&self.config), &mut self.library_cache);
        self.storage.save_library_cache(&self.library_cache)?;
        self.refresh_selection_after_structure_change(selected.as_deref());
        self.persist_ui_state()?;
        self.status = format!(
            "Scanned {} audiobook files across {} roots.",
            self.library_items.len(),
            self.config.library_dirs.len()
        );
        Ok(())
    }

    fn remove_selected_root(&mut self) -> Result<()> {
        let Some(root) = self.selected_root().map(str::to_owned) else {
            self.status = "Select a file to remove its library root.".to_owned();
            return Ok(());
        };

        self.config.library_dirs.retain(|entry| entry != &root);
        self.storage.save_config(&self.config)?;
        self.rescan_library()?;
        self.status = format!("Removed root {}", root);
        Ok(())
    }

    fn cycle_sort_mode(&mut self) -> Result<()> {
        let selected = self
            .selected_item()
            .map(|item| canonical_key(item.path.as_path()));
        self.ui_state.sort_mode = self.ui_state.sort_mode.next();
        self.storage.save_ui_state(&self.ui_state)?;
        self.refresh_selection_after_structure_change(selected.as_deref());
        self.status = format!("Sort mode: {}", self.ui_state.sort_mode.label());
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

    fn handle_mouse_event(&mut self, area: Rect, column: u16, row: u16) -> Result<()> {
        match ui::hit_test(area, column, row) {
            Some(HitTarget::Timeline { area }) => {
                let Some(duration) = self.player.duration() else {
                    return Ok(());
                };
                let ratio = ui::ratio_from_gauge_click(area, column);
                let target = Duration::from_secs_f64(duration.as_secs_f64() * ratio);
                self.player.seek_to(target)?;
                self.persist_current_resume()?;
                self.status = format!("Seeked to {}", ui::format_duration(target));
            }
            Some(HitTarget::Volume { area }) => {
                let ratio = ui::ratio_from_gauge_click(area, column);
                let volume = (ratio * 100.0).round() as u8;
                self.set_volume(volume)?;
            }
            None => {}
        }
        Ok(())
    }

    fn persist_ui_state(&mut self) -> Result<()> {
        self.ui_state.last_selected = self
            .selected_item()
            .map(|item| canonical_key(item.path.as_path()));
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
        let key = canonical_key(path);
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
        self.resume_store
            .positions
            .get(&canonical_key(path))
            .map(duration_from_entry)
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
                    .resume_store
                    .positions
                    .get(&canonical_key(item.path.as_path()))
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
            self.sorted_filtered_indices()
                .into_iter()
                .find(|index| canonical_key(self.library_items[*index].path.as_path()) == key)
        });
        self.list_state.select(
            self.selectable_row_index_for_item(target_item)
                .or_else(|| self.selectable_row_indices().first().copied()),
        );
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
) -> Option<usize> {
    selected_path
        .and_then(|path| {
            indices
                .iter()
                .copied()
                .find(|index| canonical_key(items[*index].path.as_path()) == path)
        })
        .or_else(|| indices.first().copied())
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
    if let Some(stripped) = input.strip_prefix("~/") {
        if let Some(home) = home_dir() {
            return home.join(stripped);
        }
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
