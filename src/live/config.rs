use anyhow::{Context, Result, bail, ensure};
use std::{
    net::{IpAddr, Ipv4Addr},
    time::Duration,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveConfig {
    pub fps: u32,
    pub quality: u8,
    pub max_width: Option<u32>,
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
            cursor: false,
            bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: super::LIVE_PORT,
        }
    }
}

impl LiveConfig {
    pub fn validate(&self) -> Result<()> {
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
            Duration::from_secs_f64(1.0 / f64::from(self.fps))
        }
    }

    /// Strip streaming options from the command. `--` preserves literal targets.
    pub fn parse_args(args: &[String]) -> Result<(Self, Vec<String>)> {
        let mut config = Self::default();
        let mut rest = Vec::new();
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--" => {
                    rest.extend(args.cloned());
                    break;
                }
                "--cursor" => config.cursor = true,
                "--fps" | "--quality" | "--width" | "--bind" | "--port" => {
                    let value = args
                        .next()
                        .with_context(|| format!("{arg} needs a value"))?;
                    match arg.as_str() {
                        "--fps" => config.fps = value.parse().context("invalid FPS")?,
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
                                | "--stop"
                                | "--send-link"
                                | "--hide"
                                | "--hypr"
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
        ];
        if let Some(width) = self.max_width {
            args.extend(["--width".into(), width.to_string()]);
        }
        if self.cursor {
            args.push("--cursor".into());
        }
        args
    }
}
