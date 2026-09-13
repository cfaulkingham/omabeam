//! Picker stream settings under `$XDG_CONFIG_HOME/omabeam`.
//! Preferred GPU encoder names are not secrets; the file is still user-owned.
use super::config::{EncoderMode, LiveConfig};
use anyhow::{Context, Result, ensure};
use omabeam_capture::PixelMode;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

const MAX_BYTES: usize = 4096;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SavedSettings {
    #[serde(default)]
    pub fps: u32,
    #[serde(default)]
    pub quality: u8,
    #[serde(default)]
    pub max_width: Option<u32>,
    #[serde(default)]
    pub native_pixels: bool,
    #[serde(default)]
    pub webrtc: bool,
    #[serde(default)]
    pub h264_bitrate: u32,
    #[serde(default)]
    pub encoder: String,
    #[serde(default)]
    pub cursor: bool,
    #[serde(default)]
    pub preferred_encoder: Option<String>,
}

impl SavedSettings {
    fn from_config(config: &LiveConfig, preferred_encoder: Option<String>) -> Self {
        Self {
            fps: config.fps,
            quality: config.quality,
            max_width: config.max_width,
            native_pixels: config.pixel_mode == PixelMode::Native,
            webrtc: config.webrtc,
            h264_bitrate: config.h264_bitrate,
            encoder: config.encoder.as_str().into(),
            cursor: config.cursor,
            preferred_encoder,
        }
    }

    fn apply(&self, config: &mut LiveConfig) {
        if self.fps != 0 {
            config.fps = self.fps;
        }
        if self.quality != 0 {
            config.quality = self.quality;
        }
        config.max_width = self.max_width;
        config.pixel_mode = if self.native_pixels {
            PixelMode::Native
        } else {
            PixelMode::Logical
        };
        if self.fps != 0 || self.quality != 0 || self.h264_bitrate != 0 || !self.encoder.is_empty()
        {
            config.webrtc = self.webrtc;
        }
        if self.h264_bitrate != 0 {
            config.h264_bitrate = self.h264_bitrate;
        }
        if let Ok(encoder) = EncoderMode::parse(&self.encoder) {
            config.encoder = encoder;
        }
        config.cursor = self.cursor;
        let _ = config.validate();
    }
}

fn config_dir() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|value| !value.is_empty())
                .map(|home| PathBuf::from(home).join(".config"))
        })
        .context("HOME or XDG_CONFIG_HOME is required for saved settings")?;
    ensure!(base.is_absolute(), "config directory must be absolute");
    Ok(base.join("omabeam"))
}

fn settings_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("settings.json"))
}

pub fn load() -> Option<SavedSettings> {
    load_from(&settings_path().ok()?).ok()
}

fn load_from(path: &std::path::Path) -> Result<SavedSettings> {
    if path.as_os_str().is_empty() {
        anyhow::bail!("missing settings path");
    }
    let bytes = std::fs::read(path)?;
    ensure!(bytes.len() <= MAX_BYTES, "settings file is too large");
    Ok(serde_json::from_slice(&bytes)?)
}

pub fn load_config() -> LiveConfig {
    let mut config = LiveConfig::default();
    if let Some(saved) = load() {
        saved.apply(&mut config);
    }
    config
}

pub fn preferred_encoder() -> Option<String> {
    load()?
        .preferred_encoder
        .filter(|name| !name.is_empty() && name.len() <= 160 && !name.contains("software"))
}

pub fn save_config(config: &LiveConfig) -> Result<()> {
    let preferred = load().and_then(|saved| saved.preferred_encoder);
    save(&SavedSettings::from_config(config, preferred))
}

pub fn save_preferred_encoder(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty() && name.len() <= 160 && !name.contains("software"),
        "not a hardware encoder"
    );
    let mut saved = load().unwrap_or_default();
    saved.preferred_encoder = Some(name.into());
    save(&saved)
}

fn save(settings: &SavedSettings) -> Result<()> {
    let dir = config_dir()?;
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("settings.json");
    let bytes = serde_json::to_vec_pretty(settings)?;
    ensure!(bytes.len() <= MAX_BYTES, "settings file is too large");
    let tmp = dir.join(".settings.json.tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn round_trips_stream_settings_and_preferred_encoder() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", dir.path());
        }
        let mut config = LiveConfig::default();
        config.fps = 30;
        config.quality = 72;
        config.max_width = Some(1280);
        config.pixel_mode = PixelMode::Native;
        config.cursor = true;
        config.encoder = EncoderMode::Hardware;
        config.h264_bitrate = 8_000_000;
        save_config(&config).unwrap();
        save_preferred_encoder("VA-API (/dev/dri/renderD128)").unwrap();
        let loaded = load_config();
        assert_eq!(loaded.fps, 30);
        assert_eq!(loaded.quality, 72);
        assert_eq!(loaded.max_width, Some(1280));
        assert_eq!(loaded.pixel_mode, PixelMode::Native);
        assert!(loaded.cursor);
        assert_eq!(loaded.encoder, EncoderMode::Hardware);
        assert_eq!(loaded.h264_bitrate, 8_000_000);
        assert_eq!(
            preferred_encoder().as_deref(),
            Some("VA-API (/dev/dri/renderD128)")
        );
        unsafe {
            std::env::remove_var("XDG_CONFIG_HOME");
        }
    }
}
