//! Stream settings chosen in the standalone picker, stored as
//! `$XDG_CONFIG_HOME/omabeam/settings.json` (default `~/.config/omabeam`).
//! Only explicit choices are stored; anything else keeps its default.
use super::config::{EncoderMode, LiveConfig};
use anyhow::{Context, Result, ensure};
use omabeam_capture::PixelMode;
use serde::{Deserialize, Serialize};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

pub const VERSION: u32 = 1;
const MAX_BYTES: usize = 4096;
const FILE_NAME: &str = "settings.json";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamPrefs {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fps: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quality: Option<u8>,
    /// `Some(0)` records an explicit "no width cap".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_width: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native_pixels: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub webrtc: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub h264_bitrate: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encoder: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<bool>,
}

/// The file on disk: a format version beside the remembered fields.
#[derive(Serialize, Deserialize)]
struct SettingsFile<P> {
    version: u32,
    #[serde(flatten)]
    prefs: P,
}

impl StreamPrefs {
    /// Remembered settings, or none if the file is missing, unreadable,
    /// oversized, malformed, or from another format version.
    pub fn load() -> Self {
        settings_path()
            .ok()
            .and_then(|path| read(&path))
            .unwrap_or_default()
    }

    /// The remembered FPS, only if `apply` would use it.
    pub fn valid_fps(&self) -> Option<u32> {
        self.fps.filter(|fps| (1..=120).contains(fps))
    }

    /// Record the bitrate with the meaning `LiveConfig::set_fps` gives it: a
    /// value equal to the automatic one for the current FPS is automatic.
    pub fn record_bitrate(&mut self, config: &LiveConfig) {
        let automatic = LiveConfig::default_h264_bitrate(config.fps);
        self.h264_bitrate = (config.h264_bitrate != automatic).then_some(config.h264_bitrate);
    }

    /// Apply valid fields; an invalid field keeps the value already in `config`.
    pub fn apply(&self, config: &mut LiveConfig) {
        if let Some(fps) = self.valid_fps() {
            // Before the bitrate, so an automatic bitrate follows the rate.
            config.set_fps(fps);
        }
        if let Some(bitrate) = self
            .h264_bitrate
            .filter(|bitrate| (100_000..=50_000_000).contains(bitrate))
        {
            config.h264_bitrate = bitrate;
        }
        if let Some(quality) = self.quality.filter(|quality| (1..=95).contains(quality)) {
            config.quality = quality;
        }
        match self.max_width {
            Some(0) => config.max_width = None,
            Some(width @ 1..=32768) => config.max_width = Some(width),
            _ => {}
        }
        if let Some(native) = self.native_pixels {
            config.pixel_mode = if native {
                PixelMode::Native
            } else {
                PixelMode::Logical
            };
        }
        if let Some(webrtc) = self.webrtc {
            config.webrtc = webrtc;
        }
        if let Some(encoder) = self
            .encoder
            .as_deref()
            .and_then(|name| EncoderMode::parse(name).ok())
        {
            config.encoder = encoder;
        }
        if let Some(cursor) = self.cursor {
            config.cursor = cursor;
        }
    }

    /// Replace the file atomically through a private temporary file.
    pub fn save(&self) -> Result<()> {
        let dir = config_dir()?;
        let bytes = serde_json::to_vec_pretty(&SettingsFile {
            version: VERSION,
            prefs: self,
        })?;
        ensure!(bytes.len() <= MAX_BYTES, "settings file is too large");
        std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos());
        let tmp = dir.join(format!(".{FILE_NAME}.{}.{nanos}.tmp", std::process::id()));
        let path = dir.join(FILE_NAME);
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .with_context(|| format!("create {}", tmp.display()))?;
        let written = file
            .write_all(&bytes)
            .and_then(|()| file.sync_all())
            .and_then(|()| std::fs::rename(&tmp, &path));
        if written.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        written.with_context(|| format!("write {}", path.display()))
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
    Ok(config_dir()?.join(FILE_NAME))
}

fn read(path: &Path) -> Option<StreamPrefs> {
    let mut bytes = Vec::new();
    File::open(path)
        .ok()?
        .take(MAX_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() > MAX_BYTES {
        return None;
    }
    let file: SettingsFile<StreamPrefs> = serde_json::from_slice(&bytes).ok()?;
    (file.version == VERSION).then_some(file.prefs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::{Mutex, PoisonError};

    /// Tests run on parallel threads; hold this while `XDG_CONFIG_HOME` is changed.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn with_config_home<T>(run: impl FnOnce(&Path) -> T) -> T {
        let _guard = ENV_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        let home = tempfile::TempDir::new().unwrap();
        let previous = std::env::var_os("XDG_CONFIG_HOME");
        unsafe { std::env::set_var("XDG_CONFIG_HOME", home.path()) };
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(home.path())));
        match previous {
            Some(value) => unsafe { std::env::set_var("XDG_CONFIG_HOME", value) },
            None => unsafe { std::env::remove_var("XDG_CONFIG_HOME") },
        }
        match result {
            Ok(value) => value,
            Err(panic) => std::panic::resume_unwind(panic),
        }
    }

    fn file_names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    fn applied(prefs: StreamPrefs) -> LiveConfig {
        let mut config = LiveConfig::default();
        prefs.apply(&mut config);
        config
    }

    #[test]
    fn saved_prefs_round_trip_through_a_private_versioned_file() {
        with_config_home(|home| {
            let prefs = StreamPrefs {
                fps: Some(30),
                max_width: Some(0),
                encoder: Some("hardware".into()),
                cursor: Some(false),
                ..StreamPrefs::default()
            };
            prefs.save().unwrap();
            assert_eq!(StreamPrefs::load(), prefs);
            let dir = home.join("omabeam");
            let path = dir.join("settings.json");
            let text = std::fs::read_to_string(&path).unwrap();
            assert!(text.contains("\"version\": 1"), "{text}");
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&text).unwrap(),
                serde_json::json!({
                    "version": 1,
                    "fps": 30,
                    "max_width": 0,
                    "encoder": "hardware",
                    "cursor": false,
                })
            );
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
            let changed = StreamPrefs {
                quality: Some(90),
                ..prefs
            };
            changed.save().unwrap();
            assert_eq!(StreamPrefs::load(), changed);
            assert_eq!(file_names(&dir), ["settings.json"]);
        });
    }

    #[test]
    fn apply_changes_only_the_remembered_fields() {
        assert_eq!(applied(StreamPrefs::default()), LiveConfig::default());
        assert_eq!(
            applied(StreamPrefs {
                quality: Some(72),
                ..StreamPrefs::default()
            }),
            LiveConfig {
                quality: 72,
                ..LiveConfig::default()
            }
        );
        assert_eq!(
            applied(StreamPrefs {
                fps: Some(30),
                ..StreamPrefs::default()
            }),
            LiveConfig {
                fps: 30,
                h264_bitrate: LiveConfig::default_h264_bitrate(30),
                ..LiveConfig::default()
            }
        );
        assert_eq!(
            applied(StreamPrefs {
                fps: Some(30),
                h264_bitrate: Some(6_000_000),
                ..StreamPrefs::default()
            }),
            LiveConfig {
                fps: 30,
                h264_bitrate: 6_000_000,
                ..LiveConfig::default()
            }
        );
        let mut capped = LiveConfig {
            max_width: Some(1920),
            ..LiveConfig::default()
        };
        StreamPrefs {
            max_width: Some(0),
            ..StreamPrefs::default()
        }
        .apply(&mut capped);
        assert_eq!(capped.max_width, None);
        let config = applied(StreamPrefs {
            max_width: Some(1280),
            ..StreamPrefs::default()
        });
        assert_eq!(config.max_width, Some(1280));
        assert_eq!(
            applied(StreamPrefs {
                fps: Some(60),
                quality: Some(90),
                max_width: Some(1280),
                native_pixels: Some(true),
                webrtc: Some(false),
                h264_bitrate: Some(8_000_000),
                encoder: Some("software".into()),
                cursor: Some(true),
            }),
            LiveConfig {
                fps: 60,
                quality: 90,
                max_width: Some(1280),
                pixel_mode: PixelMode::Native,
                webrtc: false,
                h264_bitrate: 8_000_000,
                encoder: EncoderMode::Software,
                cursor: true,
                ..LiveConfig::default()
            }
        );
    }

    #[test]
    fn record_bitrate_keeps_only_a_custom_bitrate() {
        let mut prefs = StreamPrefs {
            h264_bitrate: Some(8_000_000),
            ..StreamPrefs::default()
        };
        prefs.record_bitrate(&LiveConfig::default());
        assert_eq!(prefs.h264_bitrate, None);
        prefs.record_bitrate(&LiveConfig {
            h264_bitrate: 8_000_000,
            ..LiveConfig::default()
        });
        assert_eq!(prefs.h264_bitrate, Some(8_000_000));
        prefs.record_bitrate(&LiveConfig {
            fps: 60,
            h264_bitrate: LiveConfig::default_h264_bitrate(60),
            ..LiveConfig::default()
        });
        assert_eq!(prefs.h264_bitrate, None);
    }

    #[test]
    fn recorded_bitrate_restores_the_bitrate_the_session_showed() {
        // Advanced bitrate at 15 FPS, then Advanced FPS changes, recorded the
        // way the picker's handlers record them.
        for picked in [4_000_000, 8_000_000, 16_000_000] {
            let mut session = LiveConfig::default();
            let mut prefs = StreamPrefs::default();
            session.h264_bitrate = picked;
            prefs.record_bitrate(&session);
            for fps in [60, 15, 30] {
                session.set_fps(fps);
                prefs.fps = Some(fps);
                prefs.record_bitrate(&session);
                let restored = applied(prefs.clone());
                assert_eq!(
                    (restored.fps, restored.h264_bitrate),
                    (session.fps, session.h264_bitrate),
                    "{picked} bit/s, then {fps} FPS"
                );
                if picked == 4_000_000 && fps == 60 {
                    assert_eq!(prefs.h264_bitrate, None);
                    assert_eq!(session.h264_bitrate, 16_000_000);
                }
                if picked == 8_000_000 && fps == 60 {
                    assert_eq!(prefs.h264_bitrate, Some(8_000_000));
                    assert_eq!(session.h264_bitrate, 8_000_000);
                }
            }
        }
    }

    #[test]
    fn only_an_in_range_fps_counts_as_remembered() {
        for (fps, valid) in [
            (None, None),
            (Some(0), None),
            (Some(1), Some(1)),
            (Some(120), Some(120)),
            (Some(121), None),
        ] {
            let prefs = StreamPrefs {
                fps,
                ..StreamPrefs::default()
            };
            assert_eq!(prefs.valid_fps(), valid, "{fps:?}");
            let expected = valid.unwrap_or(LiveConfig::default().fps);
            assert_eq!(applied(prefs).fps, expected, "{fps:?}");
        }
    }

    #[test]
    fn invalid_fields_are_ignored_one_by_one() {
        with_config_home(|home| {
            let dir = home.join("omabeam");
            std::fs::create_dir_all(&dir).unwrap();
            for (text, expected) in [
                (
                    r#"{"version": 1, "fps": 0, "quality": 0, "max_width": 40000,
                        "h264_bitrate": 1, "encoder": "turbo",
                        "native_pixels": true, "webrtc": false, "cursor": true}"#,
                    LiveConfig {
                        pixel_mode: PixelMode::Native,
                        webrtc: false,
                        cursor: true,
                        ..LiveConfig::default()
                    },
                ),
                (
                    r#"{"version": 1, "fps": 500, "quality": 99, "max_width": 1280,
                        "h264_bitrate": 6000000, "encoder": "software"}"#,
                    LiveConfig {
                        max_width: Some(1280),
                        h264_bitrate: 6_000_000,
                        encoder: EncoderMode::Software,
                        ..LiveConfig::default()
                    },
                ),
                (
                    r#"{"version": 1, "fps": 30, "quality": 72, "h264_bitrate": 1}"#,
                    LiveConfig {
                        fps: 30,
                        quality: 72,
                        h264_bitrate: LiveConfig::default_h264_bitrate(30),
                        ..LiveConfig::default()
                    },
                ),
            ] {
                std::fs::write(dir.join("settings.json"), text).unwrap();
                let config = applied(StreamPrefs::load());
                config.validate().unwrap();
                assert_eq!(config, expected, "{text}");
            }
        });
    }

    #[test]
    fn load_ignores_missing_oversized_invalid_and_unversioned_files() {
        with_config_home(|home| {
            assert_eq!(StreamPrefs::load(), StreamPrefs::default());
            let dir = home.join("omabeam");
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("settings.json");
            // Trailing whitespace keeps the JSON valid, so only the size differs.
            let valid = r#"{"version": 1, "fps": 30}"#;
            let padded = |len: usize| format!("{valid:len$}");
            std::fs::write(&path, padded(MAX_BYTES)).unwrap();
            assert_eq!(StreamPrefs::load().fps, Some(30));
            for text in [
                padded(MAX_BYTES + 1),
                r#"{"version": 1, "fps": 30"#.into(),
                r#"{"version": 2, "fps": 30}"#.into(),
                r#"{"fps":30,"quality":72,"max_width":1280,"native_pixels":true,"webrtc":true,"h264_bitrate":8000000,"encoder":"hardware","cursor":true}"#.into(),
            ] {
                std::fs::write(&path, &text).unwrap();
                assert_eq!(StreamPrefs::load(), StreamPrefs::default(), "{text}");
            }
        });
    }

    #[test]
    fn failed_saves_leave_no_temporary_file() {
        with_config_home(|home| {
            let dir = home.join("omabeam");
            // A directory in place of the file makes the final rename fail.
            std::fs::create_dir_all(dir.join("settings.json")).unwrap();
            let prefs = StreamPrefs {
                fps: Some(30),
                ..StreamPrefs::default()
            };
            assert!(prefs.save().is_err());
            let oversized = StreamPrefs {
                encoder: Some("x".repeat(MAX_BYTES)),
                ..StreamPrefs::default()
            };
            assert!(oversized.save().is_err());
            assert_eq!(file_names(&dir), ["settings.json"]);
            unsafe { std::env::set_var("XDG_CONFIG_HOME", "relative") };
            assert!(prefs.save().is_err());
            assert_eq!(StreamPrefs::load(), StreamPrefs::default());
        });
    }
}
