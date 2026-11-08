use anyhow::{bail, Context, Result};
use hail::{
    discover_usb_volumes, utc_timestamp, ChangeSource, Config, Event, EventLog, Exit, LogName,
    Mode, OsEventSource, Service, DEFAULT_CONFIG_NAME, EXIT_FAILURE, EXIT_QUIT,
};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::SystemTime;

const USAGE: &str = "\
HAIL - Hardware Abstraction\\Integration Layer Service

Usage:
  hail [--config <file>] [--once | --watch-config]
  hail [--config <file>] --check-config
  hail [--config <file>] [--watch-config] --install   (Windows, elevated)
  hail --uninstall                      (Windows, elevated)
  hail --help | --version

Runs until killed, detecting changes as configured by [polling] mode:
  interval    rescan on a fixed timer
  os-events   sleep until the OS reports a hardware/mount change

Options:
  --config <file>   Startup configuration (default: hail.toml beside the executable)
  --once            Scan once, run triggered action chains, then exit
  --check-config    Validate the configuration and exit
  --install         Validate the configuration, register hail as the automatic-start
                    Windows service HAIL (restarted by Windows after exit codes 75
                    and 1), and start it
  --uninstall       Stop and remove the HAIL Windows service
  --watch-config    Windows: check the configuration file every 2 seconds while running;
                    a changed file that passes validation replaces the running
                    configuration, otherwise the running configuration is kept. With
                    --install the service is registered to run with it.
  --service         Used by the Windows service manager only; not for interactive use

Without --watch-config (and always on Linux) a changed configuration is loaded when
hail (or its service) is restarted.

Programs hail started (emulator.launch, process.run, automount) are stopped when it
exits, including on Ctrl+C, closing the console, SIGINT/SIGTERM/SIGHUP or a service stop.

Exit codes:
  0    normal exit, a hail.quit action, or a stop (Ctrl+C, signal, service stop)
  1    error (arguments, configuration, failed action, lost OS event subscription)
  75   a hail.restart action: the service manager should start hail again
";

#[derive(Clone, Copy, PartialEq)]
enum Command {
    Run,
    Check,
    Service,
    Install,
    Uninstall,
}

struct Options {
    config: Option<PathBuf>,
    once: bool,
    watch_config: bool,
    command: Command,
}

fn parse_options() -> Result<Option<Options>> {
    let mut options = Options {
        config: None,
        once: false,
        watch_config: false,
        command: Command::Run,
    };
    let mut arguments = std::env::args_os().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.to_str() {
            Some("--help" | "-h") => {
                print!("{USAGE}");
                return Ok(None);
            }
            Some("--version") => {
                println!("hail {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            Some("--config") if options.config.is_none() => {
                options.config = Some(
                    arguments
                        .next()
                        .context("--config requires a file path")?
                        .into(),
                );
            }
            Some("--once") => options.once = true,
            Some("--watch-config") => options.watch_config = true,
            Some("--check-config") => options.command = Command::Check,
            Some("--service") => options.command = Command::Service,
            Some("--install") => options.command = Command::Install,
            Some("--uninstall") => options.command = Command::Uninstall,
            _ => bail!(
                "invalid argument {}; use --help",
                argument.to_string_lossy()
            ),
        }
    }
    if options.once && options.command != Command::Run {
        bail!("--once cannot be combined with --check-config, --service, --install or --uninstall");
    }
    if options.watch_config {
        if !cfg!(windows) {
            bail!("--watch-config is Windows-only; on Linux restart hail to apply configuration changes");
        }
        if options.once || matches!(options.command, Command::Check | Command::Uninstall) {
            bail!("--watch-config cannot be combined with --once, --check-config or --uninstall");
        }
    }
    if options.command == Command::Uninstall && options.config.is_some() {
        bail!("--uninstall does not take --config");
    }
    Ok(Some(options))
}

fn report(events: &[Event]) {
    for event in events {
        println!(
            "hail: {} {}",
            event.kind.name(),
            event
                .fields
                .iter()
                .filter(|(name, _)| !matches!(**name, "event" | "timestamp"))
                .map(|(name, value)| format!("{name}={value:?}"))
                .collect::<Vec<_>>()
                .join(" ")
        );
    }
}

fn finish(exit: Exit) -> i32 {
    let cause = match exit {
        Exit::Stop => "stop requested".to_string(),
        _ => format!("{} requested", exit.action()),
    };
    println!("hail: {cause}, exiting with code {}", exit.code());
    exit.code()
}

fn config_path(options: &Options) -> Result<PathBuf> {
    if let Some(path) = &options.config {
        return Ok(path.clone());
    }
    let executable = std::env::current_exe().context("locating hail executable")?;
    Ok(executable
        .parent()
        .context("executable has no parent directory")?
        .join(DEFAULT_CONFIG_NAME))
}

fn describe(config: &Config) -> String {
    format!(
        "{}, log {}, emulator {}, {} event chain(s), {} [emulators] path(s) set, {} api(s), {} rpc endpoint(s)",
        config.polling.describe(),
        config
            .log
            .as_ref()
            .map_or("disabled".to_string(), LogName::describe),
        config.emulator.map_or("none", |kind| kind.name()),
        config.chains.len(),
        config.emulators.configured(),
        config.apis.len(),
        config.rpc.len()
    )
}

fn subscribe() -> Result<Box<dyn ChangeSource>> {
    let source = OsEventSource::new().context("subscribing to OS hardware events")?;
    Ok(Box::new(source))
}

/// Runs the service until an exit action, a stop request (`stop`), or an error.
fn serve(options: &Options, stop: Option<Arc<AtomicBool>>) -> Result<i32> {
    let config_path = config_path(options)?;
    let executable = std::env::current_exe().context("locating hail executable")?;
    let binary_directory = executable
        .parent()
        .context("executable has no parent directory")?;
    let config = Config::load(&config_path)?;

    // Subscribe before the first scan so no change between the two is missed.
    let mut source = match config.polling.mode {
        Mode::OsEvents { .. } if !options.once => Some(subscribe()?),
        _ => None,
    };
    let first = discover_usb_volumes();
    let usb_roots: Vec<PathBuf> = first
        .as_ref()
        .map(|disks| disks.iter().map(|disk| disk.mount_point.clone()).collect())
        .unwrap_or_default();
    let log = EventLog::open_optional(
        binary_directory,
        &std::env::temp_dir(),
        config.log.clone(),
        &usb_roots,
    )?;
    match log.name() {
        Some(name) if name.is_timed() => println!(
            "hail: log {} (pattern {}, new file per period, {} time)",
            log.path.display(),
            name.pattern(),
            name.clock().name()
        ),
        Some(_) => println!("hail: log {}", log.path.display()),
        None => println!("hail: log disabled"),
    }
    println!("hail: detection {}", config.polling.describe());
    let mut service = Service::new(config, log);
    let stop = stop.unwrap_or_else(|| {
        let stop = Arc::new(AtomicBool::new(false));
        signals::install(stop.clone());
        stop
    });
    service.set_stop_handle(stop);
    let result = run_service(&mut service, options, &config_path, first, &mut source);
    if let Err(error) = &result {
        // Service mode has no console; keep fatal errors in the log as well.
        let _ = service.log.write_line(&format!(
            "{} [error] {error:#}",
            utc_timestamp(SystemTime::now())
        ));
    }
    // Programs HAIL started do not outlive it, whatever the reason it exits.
    let reason = result.as_ref().map_or("error", |(_, reason)| *reason);
    match service.shutdown(reason) {
        Ok(0) => {}
        Ok(count) => println!("hail: stopped {count} program(s) started by hail"),
        Err(error) => eprintln!("hail: stopping programs started by hail: {error:#}"),
    }
    signals::finished();
    result.map(|(code, _)| code)
}

/// Console mode: Ctrl+C, Ctrl+Break, closing the console window, logoff and shutdown
/// (Windows), SIGINT, SIGTERM and SIGHUP (Linux) become a stop request, so the programs
/// hail started are stopped before it exits. A second Ctrl+C / signal ends hail at once.
mod signals {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, OnceLock};

    static STOP: OnceLock<Arc<AtomicBool>> = OnceLock::new();
    static DONE: AtomicBool = AtomicBool::new(false);

    /// `false` when a stop had already been requested.
    fn request_stop() -> bool {
        STOP.get()
            .is_none_or(|stop| !stop.swap(true, Ordering::SeqCst))
    }

    pub fn install(stop: Arc<AtomicBool>) {
        if STOP.set(stop).is_ok() {
            platform_install();
        }
    }

    /// Shutdown is complete: a waiting console-close handler may return.
    pub fn finished() {
        DONE.store(true, Ordering::SeqCst);
    }

    #[cfg(windows)]
    fn platform_install() {
        use std::time::{Duration, Instant};
        use windows_sys::Win32::Foundation::{BOOL, FALSE, TRUE};
        use windows_sys::Win32::System::Console::{
            SetConsoleCtrlHandler, CTRL_BREAK_EVENT, CTRL_C_EVENT,
        };
        // Windows ends the process once a close/logoff/shutdown handler returns, and
        // after about 5 s in any case.
        const CLOSE_WAIT: Duration = Duration::from_millis(4500);

        unsafe extern "system" fn handler(ctrl: u32) -> BOOL {
            let first = request_stop();
            if ctrl == CTRL_C_EVENT || ctrl == CTRL_BREAK_EVENT {
                // A repeated Ctrl+C falls through to the default handler, which exits.
                return if first { TRUE } else { FALSE };
            }
            let deadline = Instant::now() + CLOSE_WAIT;
            while !DONE.load(Ordering::SeqCst) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(50));
            }
            TRUE
        }

        // SAFETY: registers a handler with the signature the API expects.
        unsafe {
            SetConsoleCtrlHandler(Some(handler), TRUE);
        }
    }

    #[cfg(not(windows))]
    fn platform_install() {
        extern "C" fn handler(signal: libc::c_int) {
            if !request_stop() {
                // SAFETY: _exit is async-signal-safe.
                unsafe { libc::_exit(128 + signal) };
            }
        }
        for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
            // SAFETY: the handler only touches atomics and calls _exit.
            unsafe {
                libc::signal(signal, handler as *const () as libc::sighandler_t);
            }
        }
    }
}

/// The exit code and the reason hail exits.
fn run_service(
    service: &mut Service,
    options: &Options,
    config_path: &Path,
    first: Result<Vec<hail::Disk>>,
    source: &mut Option<Box<dyn ChangeSource>>,
) -> Result<(i32, &'static str)> {
    service.start(config_path)?;
    if service.exit_requested().is_none() {
        report(&service.observe(first)?);
    }
    if let Some(exit) = service.exit_requested() {
        return Ok((finish(exit), exit.action()));
    }
    if options.once {
        return Ok((EXIT_QUIT, "--once"));
    }
    if options.watch_config {
        service.watch_config(config_path, hail::CONFIG_CHECK_INTERVAL)?;
    }
    let exit = service.run(source, subscribe, discover_usb_volumes, report)?;
    Ok((finish(exit), exit.action()))
}

fn check(options: &Options) -> Result<()> {
    let config_path = config_path(options)?;
    let config = Config::load(&config_path)?;
    println!(
        "Configuration OK: {} ({})",
        config_path.display(),
        describe(&config)
    );
    Ok(())
}

#[cfg(windows)]
fn windows_command(options: Options) -> Result<i32> {
    use hail::winservice;
    match options.command {
        Command::Service => {
            winservice::run(move |stop| serve(&options, Some(stop)).unwrap_or(EXIT_FAILURE))
        }
        Command::Install => {
            check(&options)?;
            let executable = winservice::executable()?;
            let config = options
                .config
                .as_deref()
                .map(winservice::absolute)
                .transpose()?;
            winservice::install(&executable, config.as_deref(), options.watch_config)?;
            println!(
                "hail: installed and started Windows service {} ({})\nhail: command: {}",
                winservice::SERVICE_NAME,
                winservice::DISPLAY_NAME,
                winservice::service_command(&executable, config.as_deref(), options.watch_config)
            );
            Ok(EXIT_QUIT)
        }
        Command::Uninstall => {
            winservice::uninstall()?;
            println!(
                "hail: stopped and removed Windows service {}",
                winservice::SERVICE_NAME
            );
            Ok(EXIT_QUIT)
        }
        Command::Run | Command::Check => unreachable!(),
    }
}

#[cfg(not(windows))]
fn windows_command(_options: Options) -> Result<i32> {
    bail!(
        "--service, --install and --uninstall are Windows-only; on Linux run hail under \
         systemd (see README.md)"
    )
}

fn run() -> Result<i32> {
    // Internal helper: run in a user's session by the Windows service to close the windows
    // of a process it started there (see emulator::launch); exits with the number closed.
    #[cfg(windows)]
    {
        let arguments: Vec<String> = std::env::args().skip(1).collect();
        if let [flag, pid] = arguments.as_slice() {
            if flag == "--close-windows" {
                let pid: u32 = pid
                    .parse()
                    .context("--close-windows requires a process id")?;
                return Ok(hail::close_windows(pid) as i32);
            }
        }
    }
    let Some(options) = parse_options()? else {
        return Ok(EXIT_QUIT);
    };
    match options.command {
        Command::Run => serve(&options, None),
        Command::Check => {
            check(&options)?;
            Ok(EXIT_QUIT)
        }
        Command::Service | Command::Install | Command::Uninstall => windows_command(options),
    }
}

fn main() {
    let code = run().unwrap_or_else(|error| {
        eprintln!("hail: {error:#}");
        EXIT_FAILURE
    });
    std::process::exit(code);
}
