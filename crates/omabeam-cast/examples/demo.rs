//! Explicit receiver test: animated synthetic frames, never capture.
use anyhow::{Context, Result, bail};
use omabeam_cast::{Helper, VideoConfig};
use openh264::{
    OpenH264API, Timestamp,
    encoder::{
        BitRate, Encoder, EncoderConfig, FrameRate, IntraFramePeriod, Profile, RateControlMode,
    },
    formats::{RgbSliceU8, YUVBuffer},
};
use std::{
    path::Path,
    time::{Duration, Instant},
};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if !(4..=5).contains(&args.len()) {
        bail!(
            "Usage: demo HELPER RECEIVER_IP:PORT DEVELOPER_CERTIFICATE|--production [360p|720p|1080p]"
        );
    }
    let certificate = (args[3] != "--production").then(|| Path::new(&args[3]));
    let mut helper = Helper::spawn(Path::new(&args[1]), certificate)?;
    let mut config = VideoConfig {
        width: 640,
        height: 360,
        fps: 30,
        bitrate: 1_000_000,
    };
    match args.get(4).map(String::as_str).unwrap_or("360p") {
        "360p" => {}
        "720p" => {
            config.width = 1280;
            config.height = 720;
            config.bitrate = 4_000_000;
        }
        "1080p" => {
            config.width = 1920;
            config.height = 1080;
            config.bitrate = 6_000_000;
        }
        _ => bail!("Unknown test profile"),
    }
    helper.connect(args[2].parse()?, &config)?;
    let until = Instant::now() + Duration::from_secs(45);
    loop {
        let event = helper.event(Duration::from_secs(1))?;
        if let Some(event) = event {
            eprintln!("{event}");
            if event["event"] == "error" {
                bail!("Cast negotiation: {event}");
            }
            if event["event"] == "negotiated" {
                break;
            }
        }
        if Instant::now() > until {
            bail!("Cast negotiation timed out");
        }
    }
    let create = |rate: u32| -> Result<Encoder> {
        Ok(Encoder::with_api_config(
            OpenH264API::from_source(),
            EncoderConfig::new()
                .profile(Profile::Baseline)
                .bitrate(BitRate::from_bps(rate))
                .max_frame_rate(FrameRate::from_hz(config.fps as f32))
                .rate_control_mode(RateControlMode::Bitrate)
                .skip_frames(false)
                .intra_frame_period(IntraFramePeriod::from_num_frames(60)),
        )?)
    };
    let mut encoder = create(config.bitrate)?;
    let mut pixels = vec![0u8; (config.width * config.height * 3) as usize];
    let mut yuv = YUVBuffer::new(config.width as usize, config.height as usize);
    let origin = Instant::now();
    let mut force = true;
    let mut accepted = 0;
    let mut released = 0;
    for sequence in 1..=150 {
        // One outstanding frame at a time, keeping encoder work bounded by
        // feedback. The production capture loop will use the same contract.
        let capture = Instant::now();
        for y in 0..config.height as usize {
            for x in 0..config.width as usize {
                let i = (y * config.width as usize + x) * 3;
                pixels[i] = (x + sequence as usize * 3) as u8;
                pixels[i + 1] = (y * 2) as u8;
                pixels[i + 2] = if x / 20 == sequence as usize % 16 {
                    255
                } else {
                    30
                };
            }
        }
        yuv.read_rgb8(RgbSliceU8::new(
            &pixels,
            (config.width as usize, config.height as usize),
        ));
        if force {
            encoder.force_intra_frame();
        }
        let pts = capture.duration_since(origin).as_micros() as u64;
        let encoded = encoder
            .encode_at(&yuv, Timestamp::from_millis(pts / 1000))?
            .to_vec();
        helper.frame(
            sequence,
            pts,
            capture.elapsed().as_micros() as u64,
            &encoded,
        )?;
        force = false;
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let event = helper.event(Duration::from_millis(100))?;
            if let Some(event) = event {
                match event["event"].as_str() {
                    Some("error") => bail!("Cast failed: {event}"),
                    Some("feedback") => {
                        released = event["released"].as_u64().unwrap_or(0);
                        let rate =
                            event["bitrate"].as_u64().unwrap_or(config.bitrate.into()) as u32;
                        if rate != config.bitrate {
                            config.bitrate = rate;
                            encoder = create(rate)?;
                            force = true;
                        }
                    }
                    Some("frame") if event["sequence"].as_u64() == Some(sequence) => {
                        force |= event["keyframe"].as_bool().unwrap_or(true);
                        accepted += u64::from(event["accepted"].as_bool().unwrap_or(false));
                        break;
                    }
                    Some("keyframe") => force = true,
                    Some("state") if event["state"] == "ended" => bail!("Cast ended early"),
                    _ => {}
                }
            }
            if Instant::now() > deadline {
                bail!("Cast frame acknowledgement timed out");
            }
        }
        let next = origin + Duration::from_micros(sequence * 1_000_000 / config.fps as u64);
        std::thread::sleep(next.saturating_duration_since(Instant::now()));
    }
    // Allow the final scheduled frames to reach the software decoder before
    // STOP destroys its playback pipeline. This is a test drain, not latency.
    let drain_until = Instant::now() + Duration::from_millis(500);
    while Instant::now() < drain_until {
        if let Some(event) = helper.event(Duration::from_millis(20))? {
            if event["event"] == "error" {
                bail!("Cast failed while draining: {event}");
            }
            if event["event"] == "feedback" {
                released = event["released"].as_u64().unwrap_or(released);
            }
        }
    }
    helper.stop()?;
    loop {
        let event = helper
            .event(Duration::from_secs(2))?
            .context("Cast stop timed out")?;
        if event["event"] == "state" && event["state"] == "ended" {
            break;
        }
    }
    println!("accepted={accepted} released={released}; receiver decode must be checked separately");
    anyhow::ensure!(
        accepted >= 100 && released > 0,
        "Insufficient transport progress"
    );
    Ok(())
}
