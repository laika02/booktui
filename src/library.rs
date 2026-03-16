use std::{
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
    time::UNIX_EPOCH,
};

use symphonia::{
    core::{
        formats::FormatOptions,
        io::MediaSourceStream,
        meta::{MetadataOptions, StandardTagKey},
        probe::Hint,
    },
    default::get_probe,
};

use crate::storage::{CachedLibraryEntry, LibraryCache, canonical_key};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LibraryItem {
    pub path: PathBuf,
    pub title: String,
    pub metadata_title: Option<String>,
    pub extension: String,
    pub parent_label: String,
    pub modified_epoch_seconds: u64,
    pub duration: Option<Duration>,
}

pub fn scan_library(directories: &[PathBuf], cache: &mut LibraryCache) -> Vec<LibraryItem> {
    let mut files = Vec::new();
    let mut seen_keys = Vec::new();

    for directory in directories {
        visit_directory(directory, &mut files, cache, &mut seen_keys);
    }

    files.sort_by_cached_key(|item| item.path.clone());
    files.dedup_by(|left, right| left.path == right.path);
    cache
        .entries
        .retain(|key, _| seen_keys.iter().any(|seen| seen == key));
    files
}

fn visit_directory(
    path: &Path,
    files: &mut Vec<LibraryItem>,
    cache: &mut LibraryCache,
    seen_keys: &mut Vec<String>,
) {
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let entry_path = entry.path();

        if entry_path.is_dir() {
            visit_directory(&entry_path, files, cache, seen_keys);
            continue;
        }

        if !is_supported_audio(&entry_path) {
            continue;
        }

        let modified_epoch_seconds = modified_epoch_seconds(&entry_path);
        let cache_key = canonical_key(&entry_path);
        seen_keys.push(cache_key.clone());

        let item = if let Some(cached) = cache.entries.get(&cache_key) {
            if cached.modified_epoch_seconds == modified_epoch_seconds
                && cached.duration_seconds.is_some()
            {
                LibraryItem {
                    path: entry_path,
                    title: cached.title.clone(),
                    metadata_title: cached.metadata_title.clone(),
                    extension: cached.extension.clone(),
                    parent_label: cached.parent_label.clone(),
                    modified_epoch_seconds: cached.modified_epoch_seconds,
                    duration: cached.duration_seconds.map(Duration::from_secs_f64),
                }
            } else {
                rebuild_item(entry_path, modified_epoch_seconds, cache, cache_key)
            }
        } else {
            rebuild_item(entry_path, modified_epoch_seconds, cache, cache_key)
        };

        files.push(item);
    }
}

fn rebuild_item(
    entry_path: PathBuf,
    modified_epoch_seconds: u64,
    cache: &mut LibraryCache,
    cache_key: String,
) -> LibraryItem {
    let (metadata_title, duration) = read_media_info(&entry_path);
    let item = LibraryItem {
        title: metadata_title
            .clone()
            .unwrap_or_else(|| item_title(&entry_path)),
        metadata_title,
        extension: entry_path
            .extension()
            .and_then(OsStr::to_str)
            .unwrap_or_default()
            .to_ascii_lowercase(),
        parent_label: parent_label(&entry_path),
        modified_epoch_seconds,
        path: entry_path,
        duration,
    };

    cache.entries.insert(
        cache_key,
        CachedLibraryEntry {
            title: item.title.clone(),
            metadata_title: item.metadata_title.clone(),
            extension: item.extension.clone(),
            parent_label: item.parent_label.clone(),
            modified_epoch_seconds: item.modified_epoch_seconds,
            duration_seconds: item.duration.map(|duration| duration.as_secs_f64()),
        },
    );

    item
}

fn is_supported_audio(path: &Path) -> bool {
    matches!(
        path.extension().and_then(OsStr::to_str),
        Some(ext) if ext.eq_ignore_ascii_case("mp3") || ext.eq_ignore_ascii_case("m4b")
    )
}

fn item_title(path: &Path) -> String {
    path.file_stem()
        .and_then(OsStr::to_str)
        .unwrap_or("Unknown")
        .to_owned()
}

fn parent_label(path: &Path) -> String {
    path.parent()
        .and_then(Path::file_name)
        .and_then(OsStr::to_str)
        .filter(|name| !name.is_empty())
        .unwrap_or("Library Root")
        .to_owned()
}

fn modified_epoch_seconds(path: &Path) -> u64 {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn read_media_info(path: &Path) -> (Option<String>, Option<Duration>) {
    let source = match fs::File::open(path) {
        Ok(source) => source,
        Err(_) => return (None, None),
    };
    let mss = MediaSourceStream::new(Box::new(source), Default::default());
    let mut hint = Hint::new();
    if let Some(extension) = path.extension().and_then(OsStr::to_str) {
        hint.with_extension(extension);
    }

    let probed = get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .ok();
    let Some(probed) = probed else {
        return (None, None);
    };
    let mut format = probed.format;

    while !format.metadata().is_latest() {
        format.metadata().pop();
    }

    let metadata = format.metadata();
    let title = metadata.current().and_then(|revision| {
        revision
            .tags()
            .iter()
            .find(|tag| matches!(tag.std_key, Some(StandardTagKey::TrackTitle)))
            .map(|tag| tag.value.to_string())
            .or_else(|| {
                revision
                    .tags()
                    .iter()
                    .find(|tag| tag.key.eq_ignore_ascii_case("title"))
                    .map(|tag| tag.value.to_string())
            })
    });

    let duration = probe_duration(path);

    (title, duration)
}

fn probe_duration(path: &Path) -> Option<Duration> {
    let output = Command::new("ffprobe")
        .arg("-v")
        .arg("error")
        .arg("-show_entries")
        .arg("format=duration")
        .arg("-of")
        .arg("default=noprint_wrappers=1:nokey=1")
        .arg(path)
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let text = String::from_utf8_lossy(&output.stdout);
    let seconds = text.trim().parse::<f64>().ok()?;
    Some(Duration::from_secs_f64(seconds.max(0.0)))
}
