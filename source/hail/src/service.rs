use crate::config::{Action, Config, LaunchAction, LaunchTarget, Mode, OnError, ProcessAction};
use crate::discovery::{changes, Disk, DiskChange};
use crate::emulator::{
    custom_unavailable, display_command, launch_external, refuse_custom, render_args, spawn,
    Emulator, Kind, LaunchError, LaunchRequest, Started,
};
use crate::event::{utc_timestamp, Event, EventKind};
use crate::log::EventLog;
use crate::media::{scan_volume, MountPlan, TrackedDisk};
use crate::process::{ProcessKind, ProcessTable, TrackedProcess};
use crate::remote::{api_call, rpc_call};
use crate::watch::{ChangeSource, RescanSchedule};
use crate::VERSION;
use anyhow::{anyhow, bail, Context, Result};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

/// An action's own failure (logged; handled by its `on_error`), as opposed to a fatal
/// error such as an unwritable log.
type ActionOutcome = std::result::Result<(), String>;

/// How often a watched configuration file is checked for changes.
pub const CONFIG_CHECK_INTERVAL: Duration = Duration::from_secs(2);
/// Longest wait between checks of the stop handle.
const STOP_POLL: Duration = Duration::from_millis(250);

/// Exit code of `hail.quit`.
pub const EXIT_QUIT: i32 = 0;
/// Exit code for errors (invalid arguments/configuration, failed actions, lost OS subscription).
pub const EXIT_FAILURE: i32 = 1;
/// Exit code of `hail.restart` (EX_TEMPFAIL): asks the service manager to start hail again.
pub const EXIT_RESTART: i32 = 75;

/// Why the service stopped running: an action chain's request, or the stop handle.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Exit {
    Quit,
    Restart,
    /// The stop handle was set (e.g. by the Windows service manager).
    Stop,
}

impl Exit {
    pub fn code(self) -> i32 {
        match self {
            Exit::Quit | Exit::Stop => EXIT_QUIT,
            Exit::Restart => EXIT_RESTART,
        }
    }

    pub fn action(self) -> &'static str {
        match self {
            Exit::Quit => "hail.quit",
            Exit::Restart => "hail.restart",
            Exit::Stop => "stop request",
        }
    }
}

pub struct Service {
    pub config: Config,
    pub log: EventLog,
    previous: Vec<Disk>,
    disks: Vec<TrackedDisk>,
    emulator: Option<Box<dyn Emulator>>,
    processes: ProcessTable,
    /// Last JSON-RPC request id used.
    rpc_id: u64,
    exit: Option<Exit>,
    stop: Option<Arc<AtomicBool>>,
    watch: Option<ConfigWatch>,
}

/// Result of checking a watched configuration file.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConfigReload {
    /// No watch, file unchanged, missing or unreadable (retried at the next check).
    Unchanged,
    /// The file changed and passed validation; it is now the running configuration.
    Reloaded,
    /// The file changed but is invalid; the running configuration is kept.
    Rejected(String),
}

struct ConfigWatch {
    path: PathBuf,
    interval: Duration,
    next_check: Instant,
    stamp: Option<(Option<SystemTime>, u64)>,
    text: String,
}

fn file_stamp(path: &Path) -> Option<(Option<SystemTime>, u64)> {
    std::fs::metadata(path)
        .ok()
        .map(|metadata| (metadata.modified().ok(), metadata.len()))
}

fn disk_fields(disk: &Disk) -> Vec<(&'static str, String)> {
    vec![
        ("device", disk.device.clone()),
        ("label", disk.label.clone()),
        ("mount_point", disk.mount_point.display().to_string()),
    ]
}

/// How often `process.run` with `wait = true` checks whether the process finished.
const WAIT_POLL: Duration = Duration::from_millis(25);

/// What `emulator.launch` and `process.run` start: in the executable's folder, with every
/// field as `HAIL_<FIELD>` (plus `HAIL_IMAGE`/`HAIL_SAVEFILE`, empty when unset).
fn tracked_request(
    executable: PathBuf,
    args: Vec<String>,
    fields: &std::collections::BTreeMap<&'static str, String>,
) -> LaunchRequest {
    let mut env: Vec<(String, String)> = ["image", "savefile"]
        .into_iter()
        .filter(|key| !fields.contains_key(key))
        .map(|key| (format!("HAIL_{}", key.to_ascii_uppercase()), String::new()))
        .collect();
    env.extend(
        fields
            .iter()
            .map(|(key, value)| (format!("HAIL_{}", key.to_ascii_uppercase()), value.clone())),
    );
    LaunchRequest {
        directory: executable.parent().map(Path::to_path_buf),
        executable,
        args,
        env,
    }
}

fn automount(
    emulator: &mut dyn Emulator,
    config: &Config,
    log: &mut EventLog,
    processes: &mut ProcessTable,
    disk: Option<&TrackedDisk>,
) -> Result<()> {
    let disk = disk.context("emulator.automount needs an inserted disk")?;
    let skip = |log: &mut EventLog, reason: String| {
        log.debug(&format!(
            "emulator.automount {}: nothing to mount ({reason})",
            disk.disk.mount_point.display()
        ))
    };
    let mut track = |name: &str, started: Option<Started>| {
        if let Some(started) = started {
            processes.add(
                ProcessKind::Emulator,
                name,
                started,
                Some(&disk.disk.mount_point),
                config.stop_on_remove,
            );
        }
    };
    match disk.mount_plan() {
        // Launches and their skips are always logged: they start or affect a real process.
        MountPlan::External {
            emulator: name,
            image,
            savefile,
            custom: Some(entry),
        } => {
            let executable = entry.path.clone().unwrap_or_default();
            match config.custom_emulators.check(
                &executable,
                &disk.disk.mount_point,
                &config.emulators,
            ) {
                Ok(reason) => launch_external(
                    log,
                    disk,
                    &entry,
                    &executable,
                    &image,
                    savefile.as_deref(),
                    disk.mount_mode(),
                    config.emulator_session,
                    Some(&reason),
                )
                .map(|started| track(&entry.name, started)),
                Err(reason) => refuse_custom(log, disk, &name, &executable, &reason),
            }
        }
        MountPlan::External {
            emulator,
            image,
            savefile,
            custom: None,
        } => match config.emulators.get(&emulator) {
            None => skip(
                log,
                format!("hail.cfg emulator {emulator:?} is not listed in [emulators]"),
            ),
            Some(entry) => match &entry.path {
                None => skip(
                    log,
                    format!("no executable set for {} in [emulators]", entry.name),
                ),
                Some(executable) => launch_external(
                    log,
                    disk,
                    entry,
                    executable,
                    &image,
                    savefile.as_deref(),
                    disk.mount_mode(),
                    config.emulator_session,
                    None,
                )
                .map(|started| track(&entry.name, started)),
            },
        },
        MountPlan::NoHostEmulator { emulator, reason } => {
            custom_unavailable(log, disk, &emulator, &reason)
        }
        MountPlan::Mount(images) => {
            let mode = disk.mount_mode();
            let savefile = disk.savefile();
            for image in &images {
                emulator.mount(log, disk, image, mode, savefile.as_deref())?;
            }
            Ok(())
        }
        MountPlan::Skip(reason) if emulator.debug_logging() => log.debug(&format!(
            "emulator.automount {}: nothing to mount ({reason})",
            disk.disk.mount_point.display()
        )),
        MountPlan::Skip(_) => Ok(()),
    }
}

impl Service {
    pub fn new(config: Config, log: EventLog) -> Self {
        Self {
            emulator: config.emulator.map(|kind| kind.handler()),
            config,
            log,
            previous: Vec::new(),
            disks: Vec::new(),
            processes: ProcessTable::default(),
            rpc_id: 0,
            exit: None,
            stop: None,
            watch: None,
        }
    }

    /// A flag that makes the run loops return [`Exit::Stop`] within about 250 ms once set.
    pub fn stop_handle(&mut self) -> Arc<AtomicBool> {
        self.stop
            .get_or_insert_with(|| Arc::new(AtomicBool::new(false)))
            .clone()
    }

    /// Uses an existing flag (e.g. the Windows service manager's) as the stop handle.
    pub fn set_stop_handle(&mut self, stop: Arc<AtomicBool>) {
        self.stop = Some(stop);
    }

    fn stop_requested(&self) -> bool {
        self.stop
            .as_ref()
            .is_some_and(|stop| stop.load(Ordering::SeqCst))
    }

    /// Watches `path` (the file the running configuration was loaded from): while the
    /// run loops are active it is checked every `interval`, and a changed file replaces
    /// the running configuration only if it passes validation.
    pub fn watch_config(&mut self, path: &Path, interval: Duration) -> Result<()> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading configuration {}", path.display()))?;
        self.watch = Some(ConfigWatch {
            path: path.to_path_buf(),
            interval,
            next_check: Instant::now() + interval,
            stamp: file_stamp(path),
            text,
        });
        Ok(())
    }

    /// Checks the watched configuration file now (see [`Service::watch_config`]).
    pub fn check_config(&mut self) -> Result<ConfigReload> {
        let Some(watch) = self.watch.as_mut() else {
            return Ok(ConfigReload::Unchanged);
        };
        let stamp = file_stamp(&watch.path);
        if stamp.is_none() || stamp == watch.stamp {
            return Ok(ConfigReload::Unchanged);
        }
        // Editors may replace the file in steps; an unreadable file is retried later.
        let Ok(text) = std::fs::read_to_string(&watch.path) else {
            return Ok(ConfigReload::Unchanged);
        };
        watch.stamp = stamp;
        if text == watch.text {
            return Ok(ConfigReload::Unchanged);
        }
        watch.text = text.clone();
        let path = watch.path.clone();
        let timestamp = utc_timestamp(SystemTime::now());
        match Config::parse(&text).and_then(|config| self.apply_config(config)) {
            Ok(()) => {
                let message = format!(
                    "{timestamp} [config] reloaded {} ({}, emulator {}, {} event chain(s))",
                    path.display(),
                    self.config.polling.describe(),
                    self.config.emulator.map_or("none", |kind| kind.name()),
                    self.config.chains.len()
                );
                println!("hail: {message}");
                self.log.write_line(&message)?;
                Ok(ConfigReload::Reloaded)
            }
            Err(error) => {
                let error = format!("{error:#}");
                let message = format!(
                    "{timestamp} [config] rejected changes to {}; keeping the running configuration: {error}",
                    path.display()
                );
                eprintln!("hail: {message}");
                self.log.write_line(&message)?;
                Ok(ConfigReload::Rejected(error))
            }
        }
    }

    /// Replaces the running configuration. The new log is opened first, so a failure
    /// leaves everything unchanged. The emulator handler (and its running state) is kept
    /// unless `[emulator] type` changed.
    fn apply_config(&mut self, config: Config) -> Result<()> {
        self.log
            .set_name(config.log.clone())
            .context("opening the new log file")?;
        if config.emulator != self.config.emulator {
            self.emulator = config.emulator.map(|kind| kind.handler());
        }
        self.config = config;
        Ok(())
    }

    fn poll_config(&mut self) -> Result<()> {
        let now = Instant::now();
        match &mut self.watch {
            Some(watch) if now >= watch.next_check => watch.next_check = now + watch.interval,
            _ => return Ok(()),
        }
        self.check_config().map(|_| ())
    }

    /// Longest the run loops may block before checking the stop handle or the
    /// configuration file; `None` when neither is in use.
    fn tick(&self, now: Instant) -> Option<Duration> {
        let stop = self.stop.as_ref().map(|_| STOP_POLL);
        let watch = self
            .watch
            .as_ref()
            .map(|watch| watch.next_check.saturating_duration_since(now));
        stop.into_iter().chain(watch).min()
    }

    /// Inserted, currently available disks with their scan results, in insertion order.
    pub fn disks(&self) -> &[TrackedDisk] {
        &self.disks
    }

    /// The configured emulator's handler, if [emulator] is set.
    pub fn emulator(&self) -> Option<&dyn Emulator> {
        self.emulator.as_deref()
    }

    /// Set once a chain ran `hail.quit` or `hail.restart`; no further chains run.
    pub fn exit_requested(&self) -> Option<Exit> {
        self.exit
    }

    pub fn start(&mut self, config_path: &Path) -> Result<Event> {
        let event = Event::new(
            EventKind::ServiceStarted,
            utc_timestamp(SystemTime::now()),
            &[
                ("version", VERSION.to_string()),
                ("config", config_path.display().to_string()),
                ("mode", self.config.polling.mode.name().to_string()),
                (
                    "emulator",
                    self.config
                        .emulator
                        .map_or("none", |kind| kind.name())
                        .to_string(),
                ),
            ],
        );
        self.dispatch(&event)?;
        Ok(event)
    }

    /// Runs the event's action chain in configured order. The first failing action
    /// stops the chain with an error; `hail.quit`/`hail.restart` stop it and request an exit.
    pub fn dispatch(&mut self, event: &Event) -> Result<()> {
        self.run_chain(event, None)
    }

    fn run_chain(&mut self, event: &Event, disk: Option<&TrackedDisk>) -> Result<()> {
        if self.exit.is_some() {
            return Ok(());
        }
        self.processes.reap(&mut self.log)?;
        // Cloned: actions need `&mut self`, and a chain may not be changed while it runs.
        let chain = self.config.chain(event.kind).to_vec();
        for action in &chain {
            let outcome: ActionOutcome = match action {
                Action::LogWrite { message } => {
                    self.log.write_line(&message.render(&event.fields))?;
                    Ok(())
                }
                Action::EmulatorAutomount => {
                    let emulator = self
                        .emulator
                        .as_deref_mut()
                        .context("no [emulator] configured")?;
                    automount(
                        emulator,
                        &self.config,
                        &mut self.log,
                        &mut self.processes,
                        disk,
                    )?;
                    Ok(())
                }
                Action::EmulatorStart => {
                    let emulator = self
                        .emulator
                        .as_deref_mut()
                        .context("no [emulator] configured")?;
                    emulator.start(&mut self.log, event)?;
                    Ok(())
                }
                Action::EmulatorStop {
                    emulator: Some(name),
                } if Kind::from_short_name(name).is_none() => {
                    self.stop_tracked(ProcessKind::Emulator, name, event)?
                }
                Action::EmulatorStop { .. } => {
                    let emulator = self
                        .emulator
                        .as_deref_mut()
                        .context("no [emulator] configured")?;
                    emulator.stop(&mut self.log, event)?;
                    Ok(())
                }
                Action::EmulatorLaunch(launch) => self.launch(launch, event, disk)?,
                Action::ProcessRun(run) => self.run_process(run, event, disk)?,
                Action::ProcessStop { name } => {
                    self.stop_tracked(ProcessKind::Process, name, event)?
                }
                Action::ApiCall(call) => {
                    let api = self
                        .config
                        .apis
                        .get(&call.api)
                        .with_context(|| format!("api {:?} is not defined", call.api))?;
                    api_call(&mut self.log, api, call, &event.fields)?
                }
                Action::RpcCall(call) => {
                    let rpc = self
                        .config
                        .rpc
                        .get(&call.rpc)
                        .with_context(|| format!("rpc {:?} is not defined", call.rpc))?;
                    self.rpc_id += 1;
                    rpc_call(&mut self.log, rpc, call, self.rpc_id, &event.fields)?
                }
                Action::Quit => {
                    self.exit = Some(Exit::Quit);
                    break;
                }
                Action::Restart => {
                    self.exit = Some(Exit::Restart);
                    break;
                }
            };
            if let Err(error) = outcome {
                match action.on_error() {
                    OnError::Continue => {}
                    OnError::Stop => {
                        self.log.write_line(&format!(
                            "{} [chain] {} chain stopped: {} failed (on_error = \"stop\")",
                            utc_timestamp(SystemTime::now()),
                            event.kind.name(),
                            action.name()
                        ))?;
                        break;
                    }
                    OnError::Fail => bail!(
                        "{} chain: {} failed (on_error = \"fail\"): {error}",
                        event.kind.name(),
                        action.name()
                    ),
                }
            }
        }
        Ok(())
    }

    /// Processes hail started that are still running (as of the last check).
    pub fn processes(&self) -> &[TrackedProcess] {
        self.processes.entries()
    }

    /// Forgets started processes that have exited, logging their exit codes.
    pub fn reap_processes(&mut self) -> Result<()> {
        self.processes.reap(&mut self.log)
    }

    /// Stops every program HAIL started that is still running, as `emulator.stop "*"`
    /// does. Called whenever HAIL exits (an exit action, a stop request, `--once`, an
    /// error), with `reason` naming the cause in the log. Returns how many were stopped.
    pub fn shutdown(&mut self, reason: &str) -> Result<usize> {
        self.processes.stop(
            &mut self.log,
            |_| true,
            self.config.stop_timeout,
            &format!("hail exiting: {reason}"),
        )
    }

    /// `emulator.stop` with an `[emulators]` name and `process.stop`.
    fn stop_tracked(
        &mut self,
        kind: ProcessKind,
        name: &str,
        event: &Event,
    ) -> Result<ActionOutcome> {
        let stopped = self.processes.stop(
            &mut self.log,
            |entry| entry.matches(kind, name),
            self.config.stop_timeout,
            &format!("{}.stop in the {} chain", kind.name(), event.kind.name()),
        )?;
        if stopped == 0 {
            self.log.write_line(&format!(
                "{} [{}] {}.stop {name}: not running",
                utc_timestamp(SystemTime::now()),
                kind.name(),
                kind.name()
            ))?;
        }
        Ok(Ok(()))
    }

    /// Logs an action failure as an `[error]` line and returns it as the action's outcome.
    fn fail(&mut self, action: &str, message: String) -> Result<ActionOutcome> {
        Ok(Err(self.log_failure(action, message)?))
    }

    fn log_failure(&mut self, action: &str, message: String) -> Result<String> {
        self.log.write_line(&format!(
            "{} [error] {action}: {message}",
            utc_timestamp(SystemTime::now())
        ))?;
        Ok(message)
    }

    /// Starts `request`, logging the command line; `Ok(Err(..))` (already logged) when
    /// it could not start.
    #[allow(clippy::too_many_arguments)]
    fn spawn_logged(
        &mut self,
        action: &str,
        kind: ProcessKind,
        name: &str,
        request: &LaunchRequest,
        session: crate::emulator::EmulatorSession,
        event: &Event,
    ) -> Result<std::result::Result<Started, String>> {
        let command = display_command(&request.executable, &request.args);
        match spawn(request, session) {
            Ok(started) => {
                self.log.write_line(&format!(
                    "{} [{}] {action} {name} started (pid {})\n  command: {command}\n  session: {}\n  trigger: {}",
                    utc_timestamp(SystemTime::now()),
                    kind.name(),
                    started.pid,
                    started.session,
                    event.kind.name()
                ))?;
                Ok(Ok(started))
            }
            Err(LaunchError::NoUser(reason)) => Ok(Err(self.log_failure(
                action,
                format!(
                    "{name} not started: no user is logged in ({reason})\n  command: {command}"
                ),
            )?)),
            Err(LaunchError::Failed(error)) => Ok(Err(self.log_failure(
                action,
                format!("{name} failed to start: {error}\n  command: {command}"),
            )?)),
        }
    }

    /// The disk a process started by this chain belongs to (usb.inserted only).
    fn chain_disk(event: &Event, disk: Option<&TrackedDisk>) -> Option<PathBuf> {
        (event.kind == EventKind::UsbInserted)
            .then(|| disk.map(|disk| disk.disk.mount_point.clone()))
            .flatten()
    }

    fn already_running(&mut self, action: &str, kind: ProcessKind, name: &str) -> Result<bool> {
        let Some(running) = self.processes.find(kind, name) else {
            return Ok(false);
        };
        let message = format!(
            "{} [{}] {action} {name}: already running (pid {}), not started again",
            utc_timestamp(SystemTime::now()),
            kind.name(),
            running.pid
        );
        self.log.write_line(&message)?;
        Ok(true)
    }

    /// `emulator.launch`: an `[emulators]` entry or an emulator defined by the action.
    fn launch(
        &mut self,
        action: &LaunchAction,
        event: &Event,
        disk: Option<&TrackedDisk>,
    ) -> Result<ActionOutcome> {
        const ACTION: &str = "emulator.launch";
        let name = action.emulator.name().to_string();
        if action.single && self.already_running(ACTION, ProcessKind::Emulator, &name)? {
            return Ok(Ok(()));
        }
        let mut fields = event.fields.clone();
        for (key, template) in [("image", &action.image), ("savefile", &action.savefile)] {
            let Some(template) = template else { continue };
            let value = template.render(&event.fields);
            if value.trim().is_empty() {
                continue;
            }
            if !Path::new(&value).is_absolute() {
                return self.fail(
                    ACTION,
                    format!("{name}: {key} {value:?} is not a full path"),
                );
            }
            fields.insert(key, value);
        }
        let (executable, args) = match &action.emulator {
            LaunchTarget::Named(entry_name) => {
                let Some(entry) = self.config.emulators.get(entry_name) else {
                    return self.fail(ACTION, format!("{entry_name} is not listed in [emulators]"));
                };
                let Some(path) = entry.path.clone() else {
                    return self.fail(
                        ACTION,
                        format!("no executable set for {} in [emulators]", entry.name),
                    );
                };
                (path, render_args(&entry.args, &fields))
            }
            LaunchTarget::Custom { path, args, .. } => {
                let path = match path.for_host() {
                    Ok(path) => path,
                    Err(reason) => {
                        return self.fail(ACTION, format!("{name}: emulator.path: {reason}"))
                    }
                };
                let path = PathBuf::from(path.render(&event.fields));
                if !path.is_absolute() {
                    return self.fail(
                        ACTION,
                        format!(
                            "{name}: emulator.path {} is not a full path",
                            path.display()
                        ),
                    );
                }
                (path, render_args(args, &fields))
            }
        };
        let request = tracked_request(executable, args, &fields);
        let session = self.config.emulator_session;
        match self.spawn_logged(
            ACTION,
            ProcessKind::Emulator,
            &name,
            &request,
            session,
            event,
        )? {
            Ok(started) => {
                let disk = Self::chain_disk(event, disk);
                let stop_on_remove = action.stop_on_remove.unwrap_or(self.config.stop_on_remove);
                self.processes.add(
                    ProcessKind::Emulator,
                    &name,
                    started,
                    disk.as_deref(),
                    stop_on_remove,
                );
                Ok(Ok(()))
            }
            Err(error) => Ok(Err(error)),
        }
    }

    /// `process.run`: starts a program and tracks it, or waits for it to finish.
    fn run_process(
        &mut self,
        action: &ProcessAction,
        event: &Event,
        disk: Option<&TrackedDisk>,
    ) -> Result<ActionOutcome> {
        const ACTION: &str = "process.run";
        let executable = PathBuf::from(action.command.render(&event.fields));
        let name = action.name_for(&executable);
        if !executable.is_absolute() {
            return self.fail(
                ACTION,
                format!(
                    "{name}: command {} is not a full path",
                    executable.display()
                ),
            );
        }
        if action.single && self.already_running(ACTION, ProcessKind::Process, &name)? {
            return Ok(Ok(()));
        }
        let args = render_args(&action.args, &event.fields);
        let request = tracked_request(executable, args, &event.fields);
        let session = action.session.unwrap_or(self.config.emulator_session);
        let mut started = match self.spawn_logged(
            ACTION,
            ProcessKind::Process,
            &name,
            &request,
            session,
            event,
        )? {
            Ok(started) => started,
            Err(error) => return Ok(Err(error)),
        };
        if !action.wait {
            let disk = Self::chain_disk(event, disk);
            let stop_on_remove = action.stop_on_remove.unwrap_or(self.config.stop_on_remove);
            self.processes.add(
                ProcessKind::Process,
                &name,
                started,
                disk.as_deref(),
                stop_on_remove,
            );
            return Ok(Ok(()));
        }
        let begin = Instant::now();
        let pid = started.pid;
        let code = loop {
            if let Some(code) = started.process.try_exit() {
                break code;
            }
            if begin.elapsed() >= action.timeout {
                let killed = started.process.kill().map_or_else(
                    |error| format!("kill failed: {error}"),
                    |()| "killed".to_string(),
                );
                return self.fail(
                    ACTION,
                    format!(
                        "{name} (pid {pid}) still running after {} ms; {killed}",
                        action.timeout.as_millis()
                    ),
                );
            }
            std::thread::sleep(WAIT_POLL);
        };
        match code {
            Some(0) => {
                self.log.write_line(&format!(
                    "{} [process] {name} (pid {pid}) finished after {} ms (exit code 0)",
                    utc_timestamp(SystemTime::now()),
                    begin.elapsed().as_millis()
                ))?;
                Ok(Ok(()))
            }
            Some(code) => self.fail(
                ACTION,
                format!("{name} (pid {pid}) failed (exit code {code})"),
            ),
            None => self.fail(ACTION, format!("{name} (pid {pid}) ended by a signal")),
        }
    }

    /// Applies one poll result. Volumes already present on the first poll are reported as
    /// inserted. Inserted volumes are scanned (read-only) and tracked until removed.
    /// Stops early once an exit is requested.
    pub fn observe(&mut self, snapshot: Result<Vec<Disk>>) -> Result<Vec<Event>> {
        let current = match snapshot {
            Ok(current) => current,
            Err(error) => {
                let event = Event::new(
                    EventKind::PollingFailed,
                    utc_timestamp(SystemTime::now()),
                    &[("error", format!("{error:#}"))],
                );
                self.dispatch(&event)?;
                return Ok(vec![event]);
            }
        };
        let mut events = Vec::new();
        for change in changes(&self.previous, &current) {
            if self.exit.is_some() {
                break;
            }
            let timestamp = utc_timestamp(SystemTime::now());
            match change {
                DiskChange::Inserted(disk) => {
                    let tracked = TrackedDisk {
                        scan: scan_volume(&disk.mount_point),
                        inserted_at: timestamp.clone(),
                        disk,
                    };
                    let mut fields = disk_fields(&tracked.disk);
                    fields.push(("hail_cfg", tracked.scan.config.name().to_string()));
                    fields.push(("images", tracked.scan.images.len().to_string()));
                    let event = Event::new(EventKind::UsbInserted, timestamp, &fields);
                    self.disks.push(tracked.clone());
                    self.run_chain(&event, Some(&tracked))?;
                    events.push(event);
                }
                DiskChange::Removed(disk) => {
                    self.disks.retain(|tracked| {
                        tracked.disk.device != disk.device
                            || tracked.disk.mount_point != disk.mount_point
                    });
                    let mount_point = disk.mount_point.clone();
                    self.processes.stop(
                        &mut self.log,
                        |entry| entry.stop_on_remove && entry.disk.as_ref() == Some(&mount_point),
                        self.config.stop_timeout,
                        &format!("usb.removed {}", mount_point.display()),
                    )?;
                    let event = Event::new(EventKind::UsbRemoved, timestamp, &disk_fields(&disk));
                    self.run_chain(&event, None)?;
                    events.push(event);
                }
            }
        }
        self.previous = current;
        Ok(events)
    }

    /// Runs until killed, stopped, a chain requests an exit, or a fatal error, following
    /// `[polling] mode` across configuration reloads. `source` is the OS event subscription:
    /// created with `subscribe` when os-events mode needs one (followed by a rescan, so
    /// nothing is missed), dropped in interval mode.
    pub fn run(
        &mut self,
        source: &mut Option<Box<dyn ChangeSource>>,
        mut subscribe: impl FnMut() -> Result<Box<dyn ChangeSource>>,
        mut scan: impl FnMut() -> Result<Vec<Disk>>,
        mut on_events: impl FnMut(&[Event]),
    ) -> Result<Exit> {
        loop {
            let end = match self.config.polling.mode {
                Mode::Interval { interval_ms } => {
                    *source = None;
                    self.interval_loop(interval_ms, &mut scan, &mut on_events)?
                }
                Mode::OsEvents { settle_ms } => {
                    if source.is_none() {
                        *source = Some(subscribe().context("subscribing to OS hardware events")?);
                        on_events(&self.observe(scan())?);
                    }
                    let source = source.as_deref_mut().context("no OS event subscription")?;
                    self.os_events_loop(settle_ms, source, &mut scan, &mut on_events)?
                }
            };
            if let Some(exit) = end {
                return Ok(exit);
            }
        }
    }

    /// Runs until killed, stopped, a chain requests an exit, or a fatal error: rescans
    /// every `interval_ms`. Fails if a configuration reload changes `[polling] mode`
    /// (use [`Service::run`] to follow such changes).
    pub fn run_interval(
        &mut self,
        mut scan: impl FnMut() -> Result<Vec<Disk>>,
        mut on_events: impl FnMut(&[Event]),
    ) -> Result<Exit> {
        let Mode::Interval { interval_ms } = self.config.polling.mode else {
            anyhow::bail!("run_interval requires mode = \"interval\"");
        };
        self.interval_loop(interval_ms, &mut scan, &mut on_events)?
            .context("[polling] mode changed by a configuration reload")
    }

    /// Runs until killed, stopped, a chain requests an exit, or a fatal error: idles until
    /// `source` reports an OS hardware change, then rescans on the settle schedule. A failure
    /// of the OS subscription itself raises `polling.failed` and stops the service.
    pub fn run_os_events(
        &mut self,
        source: &mut dyn ChangeSource,
        mut scan: impl FnMut() -> Result<Vec<Disk>>,
        mut on_events: impl FnMut(&[Event]),
    ) -> Result<Exit> {
        let Mode::OsEvents { settle_ms } = self.config.polling.mode else {
            anyhow::bail!("run_os_events requires mode = \"os-events\"");
        };
        self.os_events_loop(settle_ms, source, &mut scan, &mut on_events)?
            .context("[polling] mode changed by a configuration reload")
    }

    /// Checks for an exit, a stop request and configuration changes; `Ok(None)` means
    /// keep running in the same mode.
    fn between_waits(&mut self, mode: Mode) -> Result<Option<Option<Exit>>> {
        if let Some(exit) = self.exit {
            return Ok(Some(Some(exit)));
        }
        if self.stop_requested() {
            return Ok(Some(Some(Exit::Stop)));
        }
        self.poll_config()?;
        self.processes.reap(&mut self.log)?;
        if self.config.polling.mode != mode {
            return Ok(Some(None));
        }
        Ok(None)
    }

    /// `Ok(None)`: the polling mode changed.
    fn interval_loop(
        &mut self,
        interval_ms: u64,
        scan: &mut dyn FnMut() -> Result<Vec<Disk>>,
        on_events: &mut dyn FnMut(&[Event]),
    ) -> Result<Option<Exit>> {
        let mode = self.config.polling.mode;
        loop {
            let deadline = Instant::now() + Duration::from_millis(interval_ms);
            loop {
                if let Some(end) = self.between_waits(mode)? {
                    return Ok(end);
                }
                let now = Instant::now();
                if now >= deadline {
                    break;
                }
                let remaining = deadline - now;
                std::thread::sleep(self.tick(now).map_or(remaining, |tick| tick.min(remaining)));
            }
            on_events(&self.observe(scan())?);
        }
    }

    /// `Ok(None)`: the polling mode changed.
    fn os_events_loop(
        &mut self,
        settle_ms: u64,
        source: &mut dyn ChangeSource,
        scan: &mut dyn FnMut() -> Result<Vec<Disk>>,
        on_events: &mut dyn FnMut(&[Event]),
    ) -> Result<Option<Exit>> {
        let mode = self.config.polling.mode;
        let mut schedule = RescanSchedule::new(Duration::from_millis(settle_ms));
        loop {
            if let Some(end) = self.between_waits(mode)? {
                return Ok(end);
            }
            let now = Instant::now();
            let timeout = match (schedule.timeout(now), self.tick(now)) {
                (Some(rescan), Some(tick)) => Some(rescan.min(tick)),
                (rescan, tick) => rescan.or(tick),
            };
            match source.wait(timeout) {
                Ok(true) => schedule.signal(Instant::now()),
                Ok(false) => {}
                Err(error) => {
                    let error = error.context("waiting for OS hardware events");
                    on_events(&self.observe(Err(anyhow!("{error:#}")))?);
                    return Err(error);
                }
            }
            if schedule.take_due(Instant::now()) {
                on_events(&self.observe(scan())?);
            }
        }
    }
}
