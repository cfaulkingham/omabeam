fn main() {
    if let Err(error) = run() {
        eprintln!("{error:#}");
        std::process::exit(1);
    }
}

fn run() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let fps_explicit = args
        .iter()
        .take_while(|arg| *arg != "--")
        .any(|arg| arg == "--fps");
    let (mut config, args) = omabeam::live::LiveConfig::parse_args(&args)?;
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
        anyhow::ensure!(args.len() == 1, "unexpected command argument");
        println!(
            "{}",
            serde_json::to_string_pretty(&omabeam::live::probe_encoder(&config)?)?
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
        if let Some(status) = omabeam::live::current_status() {
            if status.stats.state != "ended"
                && let Some(cast) = &status.stats.cast
            {
                let _ = std::process::Command::new("/usr/bin/notify-send")
                    .args([
                        "OmaBeam",
                        &format!(
                            "Casting to {}. Use the OmaBeam bar icon to stop.",
                            cast.receiver_name
                        ),
                    ])
                    .status();
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
                let _ = std::process::Command::new("/usr/bin/notify-send")
                    .args(["OmaBeam", message])
                    .status();
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
