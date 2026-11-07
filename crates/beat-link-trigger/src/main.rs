//! Command-line entry point. Accepts the same options as the Java version
//! (`--offline`, `--show`, `--config`, `--reset`) plus headless and conversion
//! modes that are handy for servers and scripting.

use beat_link_trigger::app::{App, AppOptions};
use beat_link_trigger::config::{self, Settings};
use beat_link_trigger::{logs, VERSION};
use clap::Parser;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

#[derive(Parser, Debug)]
#[command(name = "beat-link-trigger", version = VERSION, about = "Trigger events and automate shows in response to CDJ activity.")]
struct Cli {
    /// Start in offline mode.
    #[arg(short = 'o', long)]
    offline: bool,

    /// Open an additional show after startup (may be repeated).
    #[arg(short = 's', long = "show", value_name = "FILE")]
    shows: Vec<PathBuf>,

    /// Write the saved configuration to a file and forget it.
    #[arg(long, value_name = "FILE")]
    reset: Option<PathBuf>,

    /// Use the specified configuration file (.bltj, or a .blt from the Java version).
    #[arg(short = 'c', long, value_name = "FILE")]
    config: Option<PathBuf>,

    /// Run without a graphical user interface.
    #[arg(long)]
    headless: bool,

    /// Device number to claim on the DJ Link network (0 = automatic).
    #[arg(long, default_value_t = 0)]
    device_number: u8,

    /// Add this many simulated players playing the sample tracks.
    #[arg(long, value_name = "N", default_value_t = 0)]
    simulate: u8,

    /// Exit after this many seconds (headless mode); useful for testing.
    #[arg(long, value_name = "SECONDS")]
    run_for: Option<u64>,

    /// Convert a Java .blt trigger file or .bls show to the new JSON format and exit.
    #[arg(long, value_name = "FILE")]
    convert: Option<PathBuf>,

    /// Where to write the converted file (defaults to the input with a new extension).
    #[arg(long, value_name = "FILE", requires = "convert")]
    output: Option<PathBuf>,

    /// Compile every expression in the configuration, report problems, and exit.
    #[arg(long)]
    check: bool,

    /// Write the expression report HTML to a file and exit.
    #[arg(long, value_name = "FILE")]
    report: Option<PathBuf>,

    /// Logging level (error, warn, info, debug, trace).
    #[arg(long, default_value = "info")]
    log_level: log::LevelFilter,
}

fn convert(input: &PathBuf, output: Option<&PathBuf>) -> Result<PathBuf, String> {
    let ext = input.extension().and_then(|e| e.to_str()).unwrap_or("");
    match ext {
        "blt" => {
            let s = Settings::load(input).map_err(|e| e.to_string())?;
            let out = output.cloned().unwrap_or_else(|| input.with_extension(config::TRIGGER_FILE_EXTENSION));
            s.save(&out).map_err(|e| e.to_string())?;
            Ok(out)
        }
        "bls" => {
            let show = beat_link_trigger::shows::Show::load(input).map_err(|e| e.to_string())?;
            let out = output.cloned().unwrap_or_else(|| input.with_extension(beat_link_trigger::shows::SHOW_FILE_EXTENSION));
            show.save(&out).map_err(|e| e.to_string())?;
            Ok(out)
        }
        _ => Err(format!("Don't know how to convert {:?}; expected a .blt or .bls file", input)),
    }
}

fn main() {
    let cli = Cli::parse();
    logs::init(cli.log_level, logs::log_file_path());
    log::info!("Beat Link Trigger {VERSION} starting.");

    if let Some(input) = &cli.convert {
        match convert(input, cli.output.as_ref()) {
            Ok(out) => {
                println!("Converted {} to {}", input.display(), out.display());
                return;
            }
            Err(e) => {
                eprintln!("Conversion failed: {e}");
                std::process::exit(1);
            }
        }
    }

    let prefs = config::default_preferences_path();
    if let Some(reset) = &cli.reset {
        match std::fs::copy(&prefs, reset) {
            Ok(_) => {
                let _ = std::fs::remove_file(&prefs);
                println!("Wrote saved configuration to {} and reset it.", reset.display());
            }
            Err(e) => println!("--reset: unable to save configuration from {}: {e}", prefs.display()),
        }
    }

    let config_path = cli.config.clone().unwrap_or(prefs.clone());
    let mut settings = if config_path.exists() {
        match Settings::load(&config_path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("Unable to read configuration {}: {e}", config_path.display());
                std::process::exit(1);
            }
        }
    } else {
        if cli.config.is_some() {
            eprintln!("--config: file {} does not exist", config_path.display());
            std::process::exit(1);
        }
        Settings::default()
    };
    for s in &cli.shows {
        if !settings.shows.contains(s) {
            settings.shows.push(s.clone());
        }
    }

    let app = App::new(settings.clone(), AppOptions { device_number: cli.device_number, ..Default::default() });
    if config_path.extension().map(|e| e == "bltj").unwrap_or(false) {
        *app.settings_path.write() = Some(config_path.clone());
    }

    if cli.check {
        let mut problems = 0;
        for (kind, err) in app.triggers.global_errors.read().iter() {
            println!("Global {kind} expression: {err}");
            problems += 1;
        }
        for v in app.triggers.views() {
            for (kind, err) in &v.errors {
                println!("Trigger {} ({}) {kind} expression: {err}", v.index + 1, v.config.comment);
                problems += 1;
            }
        }
        println!("{} trigger(s) checked, {problems} problem(s).", app.triggers.len());
        app.shutdown();
        std::process::exit(if problems > 0 { 2 } else { 0 });
    }

    if let Some(report) = &cli.report {
        if let Err(e) = std::fs::write(report, app.expression_report()) {
            eprintln!("Unable to write report: {e}");
            std::process::exit(1);
        }
        println!("Wrote expression report to {}", report.display());
        app.shutdown();
        return;
    }

    for _ in 0..cli.simulate {
        if let Some(n) = app.simulator.add_player() {
            let track = usize::from(n - 1) % beat_link_trigger::simulator::sample_tracks().len();
            app.simulator.update(n, |p| {
                p.track = Some(track);
                p.playing = true;
            });
        }
    }

    if !cli.offline {
        app.go_online();
    }

    #[cfg(feature = "gui")]
    let headless = cli.headless;
    #[cfg(not(feature = "gui"))]
    let headless = true;

    if headless {
        run_headless(&app, cli.run_for);
    } else {
        #[cfg(feature = "gui")]
        if let Err(e) = beat_link_trigger::gui::run(app.clone(), cli.simulate > 0, cli.simulate > 0) {
            eprintln!("Unable to start the user interface ({e}); running headless. Use --headless to skip the GUI.");
            run_headless(&app, cli.run_for);
        }
    }

    log::info!("Shutting down.");
    if let Err(e) = app.save(None) {
        log::warn!("Unable to save configuration: {e}");
    }
    app.shutdown();
}

fn run_headless(app: &Arc<App>, run_for: Option<u64>) {
    static STOP: AtomicBool = AtomicBool::new(false);
    let _ = ctrlc_handler(&STOP);
    let started = std::time::Instant::now();
    let mut last_report = std::time::Instant::now() - Duration::from_secs(60);
    while !STOP.load(Ordering::SeqCst) {
        if let Some(secs) = run_for {
            if started.elapsed() >= Duration::from_secs(secs) {
                break;
            }
        }
        if last_report.elapsed() >= Duration::from_secs(10) {
            last_report = std::time::Instant::now();
            let tripped: Vec<String> = app.triggers.views().iter().filter(|v| v.tripped).map(|v| (v.index + 1).to_string()).collect();
            log::info!(
                "{:?}; devices: {}; tripped triggers: [{}]",
                app.online_state(),
                app.devices().iter().map(|d| d.number.to_string()).collect::<Vec<_>>().join(","),
                tripped.join(",")
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Install a SIGINT/SIGTERM handler that sets `flag` (Unix only; elsewhere Ctrl-C
/// simply terminates the process).
fn ctrlc_handler(flag: &'static AtomicBool) -> Result<(), ()> {
    #[cfg(unix)]
    {
        use std::sync::OnceLock;
        static FLAG: OnceLock<&'static AtomicBool> = OnceLock::new();
        let _ = FLAG.set(flag);
        extern "C" fn handle(_: i32) {
            if let Some(f) = FLAG.get() {
                f.store(true, Ordering::SeqCst);
            }
        }
        // SAFETY: installing a signal handler that only stores to an atomic flag.
        unsafe {
            libc_signal(2, handle);
            libc_signal(15, handle);
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = flag;
        Err(())
    }
}

#[cfg(unix)]
unsafe fn libc_signal(sig: i32, handler: extern "C" fn(i32)) {
    extern "C" {
        fn signal(signum: i32, handler: extern "C" fn(i32)) -> usize;
    }
    signal(sig, handler);
}
