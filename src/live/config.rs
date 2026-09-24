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
    pub(crate) fn parse(value: &str) -> Result<Self> {
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
        Self::parse_args_from(Self::default(), args)
    }

    /// Like `parse_args`, but options missing from the command keep their
    /// values from `base`, such as the picker's remembered settings.
    pub fn parse_args_from(base: Self, args: &[String]) -> Result<(Self, Vec<String>)> {
        let mut config = base;
        let mut fps = None;
        let mut bitrate = None;
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
                            bitrate = Some(value.parse().context("invalid H.264 bitrate")?)
                        }
                        "--fps" => fps = Some(value.parse().context("invalid FPS")?),
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
                                | "--cast"
                                | "--cast-devices"
                                | "--cast-demo"
                                | "--cast-test"
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
        let extend = rest.first().is_some_and(|s| s == "--live")
            && rest.get(1).is_some_and(|s| s == "extend");
        // An automatic bitrate follows the new rate; an explicit one wins last.
        if let Some(fps) = fps.or(extend.then_some(60)) {
            config.set_fps(fps);
        }
        if let Some(bitrate) = bitrate {
            config.h264_bitrate = bitrate;
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

#[cfg(test)]
mod tests {
    use super::*;

    const EXTEND: [&str; 6] = ["--live", "extend", "1920", "1080", "1", "right"];

    fn parse_from(base: &LiveConfig, items: &[&str]) -> LiveConfig {
        let args: Vec<String> = items.iter().map(|item| item.to_string()).collect();
        LiveConfig::parse_args_from(base.clone(), &args).unwrap().0
    }

    /// A remembered picker configuration at 30 FPS.
    fn remembered(h264_bitrate: u32) -> LiveConfig {
        LiveConfig {
            fps: 30,
            quality: 72,
            max_width: Some(1280),
            pixel_mode: PixelMode::Native,
            webrtc: false,
            h264_bitrate,
            encoder: EncoderMode::Software,
            cursor: true,
            ..LiveConfig::default()
        }
    }

    #[test]
    fn flags_override_the_base_and_everything_else_keeps_it() {
        let base = remembered(LiveConfig::default_h264_bitrate(30));
        assert_eq!(parse_from(&base, &[]), base);
        let args: Vec<String> = ["--quality", "90", "--webrtc", "--encoder", "auto"]
            .iter()
            .map(|item| item.to_string())
            .collect();
        let (config, rest) = LiveConfig::parse_args_from(base.clone(), &args).unwrap();
        assert!(rest.is_empty());
        assert_eq!(
            config,
            LiveConfig {
                quality: 90,
                webrtc: true,
                encoder: EncoderMode::Auto,
                ..base
            }
        );
    }

    #[test]
    fn fps_flag_moves_an_automatic_bitrate_and_keeps_a_custom_one() {
        let config = parse_from(
            &remembered(LiveConfig::default_h264_bitrate(30)),
            &["--fps", "60"],
        );
        assert_eq!(
            (config.fps, config.h264_bitrate),
            (60, LiveConfig::default_h264_bitrate(60))
        );
        let config = parse_from(&remembered(6_000_000), &["--fps", "60"]);
        assert_eq!((config.fps, config.h264_bitrate), (60, 6_000_000));
    }

    #[test]
    fn bitrate_flag_wins_whatever_the_flag_order() {
        let mut extend = vec!["--h264-bitrate", "3000000"];
        extend.extend(EXTEND);
        for base in [
            remembered(LiveConfig::default_h264_bitrate(30)),
            remembered(6_000_000),
        ] {
            for (input, fps) in [
                (vec!["--fps", "60", "--h264-bitrate", "3000000"], 60),
                (vec!["--h264-bitrate", "3000000", "--fps", "60"], 60),
                (vec!["--h264-bitrate", "3000000"], 30),
                (extend.clone(), 60),
            ] {
                let config = parse_from(&base, &input);
                assert_eq!(
                    (config.fps, config.h264_bitrate),
                    (fps, 3_000_000),
                    "{input:?}"
                );
            }
        }
    }

    #[test]
    fn custom_base_bitrate_survives_without_rate_flags() {
        let base = remembered(6_000_000);
        for input in [vec![], vec!["--cursor", "--quality", "55"], EXTEND.to_vec()] {
            assert_eq!(
                parse_from(&base, &input).h264_bitrate,
                6_000_000,
                "{input:?}"
            );
        }
    }

    #[test]
    fn extended_desktop_defaults_to_sixty_fps_over_the_base_rate() {
        let base = remembered(LiveConfig::default_h264_bitrate(30));
        let config = parse_from(&base, &EXTEND);
        assert_eq!(
            (config.fps, config.h264_bitrate),
            (60, LiveConfig::default_h264_bitrate(60))
        );
        let mut input = vec!["--fps", "24"];
        input.extend(EXTEND);
        let config = parse_from(&base, &input);
        assert_eq!(
            (config.fps, config.h264_bitrate),
            (24, LiveConfig::default_h264_bitrate(24))
        );
        let config = parse_from(&remembered(6_000_000), &EXTEND);
        assert_eq!((config.fps, config.h264_bitrate), (60, 6_000_000));
    }
}
