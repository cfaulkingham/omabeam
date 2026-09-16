use anyhow::{Context, Result, bail, ensure};
use omabeam_capture::PixelMode;
use std::{
    net::{IpAddr, Ipv4Addr},
    time::Duration,
};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum EncoderMode {
    #[default]
    Auto,
    Hardware,
    Software,
}
impl EncoderMode {
    pub const ALL: [Self; 3] = [Self::Auto, Self::Hardware, Self::Software];
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Hardware => "hardware",
            Self::Software => "software",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "Auto",
            Self::Hardware => "Hardware",
            Self::Software => "Software",
        }
    }
    fn parse(value: &str) -> Result<Self> {
        Self::ALL
            .into_iter()
            .find(|mode| mode.as_str() == value)
            .context("encoder must be auto, hardware, or software")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveConfig {
    pub fps: u32,
    pub quality: u8,
    pub max_width: Option<u32>,
    pub pixel_mode: PixelMode,
    pub webrtc: bool,
    pub webrtc_port: u16,
    pub h264_bitrate: u32,
    pub encoder: EncoderMode,
    pub cursor: bool,
    pub bind: IpAddr,
    pub port: u16,
}

impl Default for LiveConfig {
    fn default() -> Self {
        Self {
            fps: 15,
            quality: 55,
            max_width: None,
            pixel_mode: PixelMode::Logical,
            webrtc: true,
            webrtc_port: 9848,
            h264_bitrate: 4_000_000,
            encoder: EncoderMode::Auto,
            cursor: false,
            bind: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            port: super::LIVE_PORT,
        }
    }
}

impl LiveConfig {
    /// 4 Mbit/s at 15 FPS, scaled linearly with H.264 FPS, capped at 16 Mbit/s.
    pub fn default_h264_bitrate(fps: u32) -> u32 {
        let fps = fps.min(60).max(1);
        (4_000_000u64 * u64::from(fps) / 15).min(16_000_000) as u32
    }

    /// Change FPS and keep an automatic bitrate in lockstep. An explicit
    /// `--h264-bitrate` or Advanced menu value is left alone.
    pub fn set_fps(&mut self, fps: u32) {
        if self.h264_bitrate == Self::default_h264_bitrate(self.fps) {
            self.h264_bitrate = Self::default_h264_bitrate(fps);
        }
        self.fps = fps;
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            (100_000..=50_000_000).contains(&self.h264_bitrate),
            "H.264 bitrate must be between 100000 and 50000000 bits/s"
        );
        ensure!(
            (1..=120).contains(&self.fps),
            "FPS must be between 1 and 120"
        );
        ensure!(
            (1..=95).contains(&self.quality),
            "JPEG quality must be between 1 and 95"
        );
        ensure!(
            self.max_width.is_none_or(|w| (1..=32768).contains(&w)),
            "maximum width must be between 1 and 32768"
        );
        Ok(())
    }

    pub fn interval(&self, viewers: usize) -> Duration {
        if viewers == 0 {
            Duration::from_secs(1)
        } else {
            // Capture owns pacing. H.264 supports at most 60 FPS; do not
            // capture faster and then impose another deadline in the encoder.
            let fps = if self.webrtc {
                self.fps.min(60)
            } else {
                self.fps
            };
            Duration::from_secs_f64(1.0 / f64::from(fps))
        }
    }

    /// Strip streaming options from the command. `--` preserves literal targets.
    pub fn parse_args(args: &[String]) -> Result<(Self, Vec<String>)> {
        let mut config = Self::default();
        let mut fps_explicit = false;
        let mut bitrate_explicit = false;
        let mut rest = Vec::new();
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--" => {
                    rest.extend(args.cloned());
                    break;
                }
                "--cursor" => config.cursor = true,
                "--native-pixels" => config.pixel_mode = PixelMode::Native,
                "--webrtc" => config.webrtc = true,
                "--jpeg" => config.webrtc = false,
                "--fps" | "--quality" | "--width" | "--bind" | "--port" | "--webrtc-port"
                | "--h264-bitrate" | "--encoder" => {
                    let value = args
                        .next()
                        .with_context(|| format!("{arg} needs a value"))?;
                    match arg.as_str() {
                        "--encoder" => config.encoder = EncoderMode::parse(value)?,
                        "--webrtc-port" => {
                            config.webrtc_port = value.parse().context("invalid WebRTC UDP port")?
                        }
                        "--h264-bitrate" => {
                            config.h264_bitrate = value.parse().context("invalid H.264 bitrate")?;
                            bitrate_explicit = true;
                        }
                        "--fps" => {
                            config.fps = value.parse().context("invalid FPS")?;
                            fps_explicit = true;
                        }
                        "--quality" => {
                            config.quality = value.parse().context("invalid JPEG quality")?
                        }
                        "--width" => {
                            config.max_width = Some(value.parse().context("invalid maximum width")?)
                        }
                        "--bind" => {
                            config.bind = value
                                .parse()
                                .context("bind must be an IPv4 or IPv6 address")?
                        }
                        "--port" => {
                            config.port =
                                value.parse().context("port must be between 0 and 65535")?
                        }
                        _ => unreachable!(),
                    }
                }
                other
                    if other.starts_with('-')
                        && !matches!(
                            other,
                            "--live"
                                | "--demo"
                                | "--demo-picker"
                                | "--picker"
                                | "--allow-token"
                                | "--status"
                                | "--share-qr"
                                | "--stop"
                                | "--send-link"
                                | "--hide"
                                | "--hypr"
                                | "--check-encoders"
                                | "--help"
                                | "-h"
                        ) =>
                {
                    // Negative coordinates are source arguments, not switches.
                    if other.parse::<i32>().is_ok() {
                        rest.push(arg.clone());
                    } else {
                        bail!("unknown option {other}");
                    }
                }
                _ => rest.push(arg.clone()),
            }
        }
        if !fps_explicit
            && rest.first().is_some_and(|s| s == "--live")
            && rest.get(1).is_some_and(|s| s == "extend")
        {
            config.fps = 60;
        }
        if !bitrate_explicit {
            config.h264_bitrate = Self::default_h264_bitrate(config.fps);
        }
        config.validate()?;
        Ok((config, rest))
    }

    pub fn to_cli_args(&self) -> Vec<String> {
        let mut args = vec![
            "--fps".into(),
            self.fps.to_string(),
            "--quality".into(),
            self.quality.to_string(),
            "--bind".into(),
            self.bind.to_string(),
            "--port".into(),
            self.port.to_string(),
            "--webrtc-port".into(),
            self.webrtc_port.to_string(),
            "--h264-bitrate".into(),
            self.h264_bitrate.to_string(),
            "--encoder".into(),
            self.encoder.as_str().into(),
        ];
        if let Some(width) = self.max_width {
            args.extend(["--width".into(), width.to_string()]);
        }
        if self.cursor {
            args.push("--cursor".into());
        }
        if self.webrtc {
            args.push("--webrtc".into());
        } else {
            args.push("--jpeg".into());
        }
        if self.pixel_mode == PixelMode::Native {
            args.push("--native-pixels".into());
        }
        args
    }
}
