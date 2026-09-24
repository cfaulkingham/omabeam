fn main() {
    if let Err(error) = run() {
        eprintln!("{error:#}");
        std::process::exit(1);
    }
}

fn run() -> anyhow::Result<()> {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let fps_explicit = raw
        .iter()
        .take_while(|arg| *arg != "--")
        .any(|arg| arg == "--fps");
    let (mut config, args) = omabeam::live::LiveConfig::parse_args(&raw)?;
    if matches!(args.first().map(String::as_str), Some("--help" | "-h")) {
        omabeam::app::print_help();
        return Ok(());
    }
    if args.first().is_some_and(|arg| arg == "--hypr") {
        anyhow::ensure!(
            args.len() == 2,
            "--hypr needs exactly one command; see --help"
        );
        if args[1] == "reload" {
            omabeam::hypr::reload()?;
            println!("ok");
        } else {
            println!("{}", omabeam::hypr::query(&args[1])?);
        }
        return Ok(());
    }
    if args.first().is_some_and(|arg| arg == "--check-encoders") {
        let sizes = encoder_check_sizes(&args[1..])?;
        println!(
            "{}",
            serde_json::to_string_pretty(&omabeam::live::probe_encoder(&config, &sizes)?)?
        );
        return Ok(());
    }
    if args.first().is_some_and(|arg| arg == "--cast-devices") {
        anyhow::ensure!(args.len() == 1, "unexpected Cast discovery argument");
        println!(
            "{}",
            serde_json::to_string_pretty(&omabeam::live::cast::discover(
                std::time::Duration::from_secs(5)
            )?)?
        );
        return Ok(());
    }
    if args.first().is_some_and(|arg| arg == "--cast-demo") {
        anyhow::ensure!(args.len() == 2, "--cast-demo needs one receiver ID");
        if !fps_explicit {
            config.set_fps(30);
        }
        return omabeam::live::cast::run_receiver_demo(&args[1], config);
    }
    if args.first().is_some_and(|arg| arg == "--cast") {
        anyhow::ensure!(
            args.len() >= 4,
            "--cast needs a receiver ID and source, e.g. --cast ID -- output DP-1"
        );
        if !fps_explicit {
            config.set_fps(30);
        }
        let source = omabeam::live::LiveSource::from_cli_args(&args[2..])?;
        return omabeam::live::cast::run_source(&args[1], source, config);
    }
    if args.first().is_some_and(|arg| arg == "--cast-test") {
        anyhow::ensure!(
            args.len() == 3,
            "--cast-test needs a receiver IP:port and developer certificate"
        );
        if !fps_explicit {
            config.set_fps(30);
        }
        return omabeam::live::cast::run_demo(
            args[1].parse()?,
            std::path::Path::new(&args[2]),
            config,
        );
    }
    if args.first().is_some_and(|arg| arg == "--demo") {
        anyhow::ensure!(args.len() == 1, "unexpected command argument");
        if !std::env::args().any(|arg| arg == "--bind") {
            config.bind = "127.0.0.1".parse()?;
        }
        return omabeam::live::run_demo(config);
    }
    if args.first().is_some_and(|arg| arg == "--stop") {
        anyhow::ensure!(args.len() == 1, "unexpected command argument");
        if omabeam::live::stop_and_cleanup()? {
            println!("stopped");
        } else {
            println!("not live");
        }
        return Ok(());
    }
    if args.first().is_some_and(|arg| arg == "--hide") {
        anyhow::ensure!(args.len() == 1, "unexpected command argument");
        omabeam::hypr::hide_picker();
        return Ok(());
    }
    if args.first().is_some_and(|arg| arg == "--status") {
        anyhow::ensure!(args.len() == 1, "unexpected command argument");
        match omabeam::live::latest_status_report() {
            Ok(Some(status)) => {
                println!("{}", serde_json::to_string(&status).unwrap_or_default());
            }
            Ok(None) => {
                std::process::exit(1);
            }
            Err(_) => {
                std::process::exit(2);
            }
        }
        return Ok(());
    }
    if args.first().is_some_and(|arg| arg == "--share-qr") {
        anyhow::ensure!(args.len() == 1, "unexpected command argument");
        println!("{}", serde_json::to_string(&omabeam::qr::current_share()?)?);
        return Ok(());
    }
    if args.first().is_some_and(|arg| arg == "--send-link") {
        anyhow::ensure!(args.len() == 1, "unexpected command argument");
        let url = omabeam::localsend::resolve_link(omabeam::live::current_status().as_ref())?;
        omabeam::app::open_send_link(url);
        return Ok(());
    }
    if args.first().is_some_and(|arg| arg == "--live") {
        let live_args = args[1..].to_vec();
        let source = omabeam::live::LiveSource::from_cli_args(&live_args)?;
        return omabeam::live::run_headless(source, config);
    }
    let mut options = omabeam::app::Options::from_args(&args, config)?;
    options.fps_explicit = fps_explicit;
    if !options.picker && !options.demo {
        // The standalone picker starts from remembered stream settings;
        // command-line flags still override them for this run.
        let prefs = omabeam::live::prefs::StreamPrefs::load();
        let mut base = omabeam::live::LiveConfig::default();
        prefs.apply(&mut base);
        options.live_config = omabeam::live::LiveConfig::parse_args_from(base, &raw)?.0;
        options.stream_prefs = prefs;
        if let Some(status) = omabeam::live::current_status() {
            if status.stats.state != "ended"
                && let Some(cast) = &status.stats.cast
            {
                omabeam::app::desktop_notify(&format!(
                    "Casting to {}. Use the OmaBeam bar icon to stop.",
                    cast.receiver_name
                ));
                return Ok(());
            }
            if status.stats.state == "ended" {
                eprintln!(
                    "{}",
                    status
                        .stats
                        .error
                        .as_deref()
                        .unwrap_or("The previous share ended.")
                );
                omabeam::live::stop_and_cleanup()?;
            } else {
                let copied = omabeam::live::copy_text(&status.url);
                if omabeam::localsend::spawn_window(&status.url).is_ok() {
                    return Ok(());
                }
                let message = if copied {
                    "A live share is already running. Its URL was copied; use the OmaBeam bar icon to send it, manage it, or stop it."
                } else {
                    "A live share is already running. Use the OmaBeam bar icon to send its URL, manage it, or stop it."
                };
                omabeam::app::desktop_notify(message);
                return Ok(());
            }
        }
    }
    if !options.picker && !options.demo && omabeam::hypr::activate_existing_picker() {
        return Ok(());
    }
    omabeam::app::open(options);
    Ok(())
}

/// Optional `--check-encoders` sizes, which replace the default 640x360,
/// 1080p, and 4K checks. Width and height are separated by x, X, or ×; a
/// repeated size is checked once.
fn encoder_check_sizes(args: &[String]) -> anyhow::Result<Vec<(u32, u32)>> {
    let mut sizes = Vec::new();
    for size in args {
        let parsed = size
            .split_once(['x', 'X', '×'])
            .and_then(|(width, height)| Some((width.parse().ok()?, height.parse().ok()?)))
            .ok_or_else(|| anyhow::anyhow!("invalid size {size}; use WxH, e.g. 1920x1080"))?;
        if !sizes.contains(&parsed) {
            sizes.push(parsed);
        }
    }
    Ok(sizes)
}

#[cfg(test)]
mod tests {
    use super::encoder_check_sizes;

    fn sizes(args: &[&str]) -> anyhow::Result<Vec<(u32, u32)>> {
        encoder_check_sizes(&args.iter().map(|arg| arg.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn encoder_check_sizes_accept_any_times_sign_and_skip_repeats() {
        assert_eq!(sizes(&[]).unwrap(), []);
        assert_eq!(
            sizes(&[
                "2560x1440",
                "1920X1080",
                "2560×1440",
                "1920x1080",
                "720x1280"
            ])
            .unwrap(),
            [(2560, 1440), (1920, 1080), (720, 1280)]
        );
        for bad in [
            "1920",
            "1920x",
            "x1080",
            "1920x1080x2",
            "-1x2",
            "1920*1080",
            "wxh",
        ] {
            let error = sizes(&[bad]).unwrap_err().to_string();
            assert!(error.contains(bad) && error.contains("WxH"), "{error}");
        }
    }
}
