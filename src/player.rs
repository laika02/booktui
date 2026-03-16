use std::{
    fs::File,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result};
use rodio::{
    Decoder, DeviceSinkBuilder, MixerDeviceSink, Player as RodioPlayer, Source, cpal::BufferSize,
};

#[derive(Debug)]
pub struct PlaybackSnapshot {
    pub position: Duration,
    pub duration: Option<Duration>,
    pub volume: u8,
    pub is_paused: bool,
}

pub struct Player {
    sink: MixerDeviceSink,
    player: RodioPlayer,
    file: Option<PathBuf>,
    duration: Option<Duration>,
    volume: u8,
}

impl Player {
    pub fn new(default_volume: u8) -> Result<Self> {
        let sink = DeviceSinkBuilder::from_default_device()
            .context("failed to open default audio output device")?
            .with_buffer_size(BufferSize::Fixed(4096))
            .with_error_callback(|_| {})
            .open_sink_or_fallback()
            .context("failed to open default audio output device")?;
        let player = RodioPlayer::connect_new(sink.mixer());
        let volume = default_volume.min(100);
        player.set_volume(volume_to_float(volume));

        Ok(Self {
            sink,
            player,
            file: None,
            duration: None,
            volume,
        })
    }

    pub fn snapshot(&self) -> Option<PlaybackSnapshot> {
        Some(PlaybackSnapshot {
            position: self.current_position(),
            duration: self.duration,
            volume: self.volume,
            is_paused: self.is_paused(),
        })
    }

    pub fn load(&mut self, path: &Path, start_at: Duration) -> Result<()> {
        self.stop()?;

        let file =
            File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
        let decoder = Decoder::try_from(file)
            .with_context(|| format!("failed to decode {}", path.display()))?;
        let duration = decoder.total_duration();

        self.player.append(decoder);
        self.player.set_volume(volume_to_float(self.volume));

        if !start_at.is_zero() {
            self.player
                .try_seek(start_at)
                .with_context(|| format!("failed to seek {}", path.display()))?;
        }

        self.file = Some(path.to_path_buf());
        self.duration = duration;
        Ok(())
    }

    pub fn play_pause(&mut self) -> Result<()> {
        if self.is_paused() {
            self.resume()
        } else {
            self.pause()
        }
    }

    pub fn pause(&mut self) -> Result<()> {
        self.player.pause();
        Ok(())
    }

    pub fn resume(&mut self) -> Result<()> {
        self.player.play();
        Ok(())
    }

    pub fn stop(&mut self) -> Result<()> {
        self.player.stop();
        self.file = None;
        self.duration = None;
        Ok(())
    }

    pub fn seek_relative(&mut self, delta: Duration, forward: bool) -> Result<()> {
        let current = self.current_position();
        let mut target = if forward {
            current.saturating_add(delta)
        } else {
            current.saturating_sub(delta)
        };

        if let Some(duration) = self.duration {
            target = target.min(duration);
        }

        self.seek_to(target)
    }

    pub fn restart_with_volume(&mut self, volume: u8) -> Result<()> {
        self.volume = volume.min(100);
        self.player.set_volume(volume_to_float(self.volume));
        Ok(())
    }

    pub fn seek_to(&mut self, target: Duration) -> Result<()> {
        if self.file.is_none() {
            return Ok(());
        }

        let clamped = if let Some(duration) = self.duration {
            target.min(duration)
        } else {
            target
        };

        self.player.try_seek(clamped).context("failed to seek")?;
        Ok(())
    }

    pub fn tick(&mut self) -> Result<Option<PathBuf>> {
        if self.file.is_some() && self.player.empty() {
            let file = self.file.clone();
            self.stop()?;
            return Ok(file);
        }

        Ok(None)
    }

    pub fn is_playing(&self) -> bool {
        self.file.is_some() && !self.player.empty()
    }

    pub fn is_paused(&self) -> bool {
        self.player.is_paused()
    }

    pub fn current_position(&self) -> Duration {
        if self.file.is_none() {
            Duration::ZERO
        } else {
            self.player.get_pos()
        }
    }

    pub fn duration(&self) -> Option<Duration> {
        self.duration
    }

    pub fn current_file(&self) -> Option<&Path> {
        self.file.as_deref()
    }

    pub fn volume(&self) -> u8 {
        self.volume
    }

    #[allow(dead_code)]
    pub fn sink(&self) -> &MixerDeviceSink {
        &self.sink
    }
}

fn volume_to_float(volume: u8) -> f32 {
    (volume.min(100) as f32 / 100.0).clamp(0.0, 1.0)
}
