use std::{
    collections::BTreeMap,
    env, fs,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Config {
    pub library_dirs: Vec<String>,
    pub default_volume: u8,
    #[serde(default = "default_idle_timeout")]
    pub idle_timeout: String,
    #[serde(default = "default_memory_on_move")]
    pub memory_on_move: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            library_dirs: Vec::new(),
            default_volume: 70,
            idle_timeout: default_idle_timeout(),
            memory_on_move: default_memory_on_move(),
        }
    }
}

fn default_idle_timeout() -> String {
    "30m".to_owned()
}

fn default_memory_on_move() -> bool {
    true
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
pub enum SortMode {
    #[default]
    Alphabetical,
    Date,
    LastPlayed,
}

impl SortMode {
    pub fn next(self) -> Self {
        match self {
            Self::Alphabetical => Self::Date,
            Self::Date => Self::LastPlayed,
            Self::LastPlayed => Self::Alphabetical,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Alphabetical => "alpha",
            Self::Date => "date",
            Self::LastPlayed => "last-played",
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct UiState {
    #[serde(default)]
    pub last_selected: Option<String>,
    #[serde(default)]
    pub sort_mode: SortMode,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ResumeStore {
    pub positions: BTreeMap<String, ResumeEntry>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct BookmarkStore {
    #[serde(default)]
    pub files: BTreeMap<String, Vec<BookmarkEntry>>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct LibraryCache {
    #[serde(default)]
    pub entries: BTreeMap<String, CachedLibraryEntry>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CachedLibraryEntry {
    pub title: String,
    pub metadata_title: Option<String>,
    pub extension: String,
    pub parent_label: String,
    pub modified_epoch_seconds: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ResumeEntry {
    pub position_seconds: f64,
    pub updated_at_epoch_seconds: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct BookmarkEntry {
    pub id: String,
    pub position_seconds: f64,
    pub label: String,
    pub created_at_epoch_seconds: u64,
}

pub struct Storage {
    config_path: PathBuf,
    state_path: PathBuf,
    resume_path: PathBuf,
    bookmarks_path: PathBuf,
    cache_path: PathBuf,
}

impl Storage {
    pub fn new() -> Result<Self> {
        let config_root = xdg_dir("XDG_CONFIG_HOME", ".config")?.join("booktui");
        let state_root = xdg_dir("XDG_STATE_HOME", ".local/state")?.join("booktui");

        fs::create_dir_all(&config_root)?;
        fs::create_dir_all(&state_root)?;

        Ok(Self {
            config_path: config_root.join("config.toml"),
            state_path: state_root.join("state.toml"),
            resume_path: state_root.join("resume.toml"),
            bookmarks_path: state_root.join("bookmarks.toml"),
            cache_path: state_root.join("library-cache.toml"),
        })
    }

    pub fn load_config(&self) -> Result<Config> {
        load_or_default(&self.config_path)
    }

    pub fn save_config(&self, config: &Config) -> Result<()> {
        save_toml(&self.config_path, config)
    }

    pub fn load_ui_state(&self) -> Result<UiState> {
        load_or_default(&self.state_path)
    }

    pub fn save_ui_state(&self, state: &UiState) -> Result<()> {
        save_toml(&self.state_path, state)
    }

    pub fn load_resume_store(&self) -> Result<ResumeStore> {
        load_or_default(&self.resume_path)
    }

    pub fn save_resume_store(&self, resume: &ResumeStore) -> Result<()> {
        save_toml(&self.resume_path, resume)
    }

    pub fn load_bookmark_store(&self) -> Result<BookmarkStore> {
        load_or_default(&self.bookmarks_path)
    }

    pub fn save_bookmark_store(&self, bookmarks: &BookmarkStore) -> Result<()> {
        save_toml(&self.bookmarks_path, bookmarks)
    }

    pub fn load_library_cache(&self) -> Result<LibraryCache> {
        load_or_default(&self.cache_path)
    }

    pub fn save_library_cache(&self, cache: &LibraryCache) -> Result<()> {
        save_toml(&self.cache_path, cache)
    }
}

pub fn canonical_key(path: &Path) -> String {
    path.canonicalize()
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

pub fn media_key(path: &Path) -> String {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    let size = fs::metadata(path)
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    format!("{name}::{size}")
}

pub fn duration_from_entry(entry: &ResumeEntry) -> Duration {
    Duration::from_secs_f64(entry.position_seconds.max(0.0))
}

fn xdg_dir(var_name: &str, fallback_suffix: &str) -> Result<PathBuf> {
    if let Ok(value) = env::var(var_name) {
        return Ok(PathBuf::from(value));
    }

    let home = env::var("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(fallback_suffix))
}

fn load_or_default<T>(path: &Path) -> Result<T>
where
    T: Default + for<'de> Deserialize<'de>,
{
    if !path.exists() {
        return Ok(T::default());
    }

    let contents =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    toml::from_str(&contents).with_context(|| format!("failed to parse {}", path.display()))
}

fn save_toml<T>(path: &Path, value: &T) -> Result<()>
where
    T: Serialize,
{
    let serialized = toml::to_string_pretty(value)?;
    fs::write(path, serialized).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}
