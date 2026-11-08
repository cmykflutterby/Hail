use crate::emulator::{
    is_short_name, parse_templates, CustomEmulators, EmulatorSession, Emulators,
    Kind as EmulatorKind, ARG_FIELDS,
};
use crate::event::EventKind;
use crate::log::{LogClock, LogName};
use crate::remote::{
    parse_apis, parse_rpcs, Api, ApiAction, RawApi, RawApiCall, RawRpc, RawRpcCall, Rpc, RpcAction,
};
use crate::template::Template;
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

pub const MIN_INTERVAL_MS: u64 = 100;
pub const MAX_INTERVAL_MS: u64 = 3_600_000;
pub const MIN_SETTLE_MS: u64 = 50;
pub const MAX_SETTLE_MS: u64 = 60_000;
pub const DEFAULT_SETTLE_MS: u64 = 500;
pub const MIN_STOP_TIMEOUT_MS: u64 = 100;
pub const MAX_STOP_TIMEOUT_MS: u64 = 60_000;
pub const DEFAULT_STOP_TIMEOUT_MS: u64 = 5_000;
pub const MIN_WAIT_TIMEOUT_MS: u64 = 100;
pub const MAX_WAIT_TIMEOUT_MS: u64 = 3_600_000;
pub const DEFAULT_WAIT_TIMEOUT_MS: u64 = 30_000;

/// What is watched.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PollingType {
    UsbVolumes,
}

/// How changes are detected.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Mode {
    /// Rescan on a fixed timer.
    Interval { interval_ms: u64 },
    /// Sleep until the OS reports a hardware/mount change, then rescan after `settle_ms`.
    OsEvents { settle_ms: u64 },
}

impl Mode {
    pub fn name(&self) -> &'static str {
        match self {
            Mode::Interval { .. } => "interval",
            Mode::OsEvents { .. } => "os-events",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Polling {
    pub kind: PollingType,
    pub mode: Mode,
}

impl Polling {
    pub fn describe(&self) -> String {
        match self.mode {
            Mode::Interval { interval_ms } => {
                format!("usb-volumes, interval every {interval_ms} ms")
            }
            Mode::OsEvents { settle_ms } => {
                format!("usb-volumes, OS hardware events (settle {settle_ms} ms)")
            }
        }
    }
}

/// What a failed `emulator.launch`, `process.run`, `api.call` or `rpc.call` does to its
/// chain. The failure is always logged.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum OnError {
    /// Run the next action.
    #[default]
    Continue,
    /// Skip the rest of this chain.
    Stop,
    /// Stop hail with exit code 1.
    Fail,
}

impl OnError {
    pub fn name(self) -> &'static str {
        match self {
            OnError::Continue => "continue",
            OnError::Stop => "stop",
            OnError::Fail => "fail",
        }
    }
}

/// Which emulator `emulator.launch` starts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LaunchTarget {
    /// An `[emulators]` entry (canonical name).
    Named(String),
    /// Defined by the action itself (`emulator.custom = true`).
    Custom {
        name: String,
        /// Must render to a full path; one template, or `emulator.path.<os>.<arch>`.
        path: crate::platform::PerPlatform<Template>,
        /// `["{image}"]` unless set.
        args: Vec<Template>,
    },
}

impl LaunchTarget {
    pub fn name(&self) -> &str {
        match self {
            LaunchTarget::Named(name) | LaunchTarget::Custom { name, .. } => name,
        }
    }
}

/// `emulator.launch`: starts an `[emulators]` or action-defined emulator and tracks it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LaunchAction {
    pub emulator: LaunchTarget,
    /// Must render to a full path; arguments using `{image}` are left out without it.
    pub image: Option<Template>,
    pub savefile: Option<Template>,
    /// Not started again while one with the same name is running.
    pub single: bool,
    /// Overrides `[service] stop_on_remove` (usb.inserted chains only).
    pub stop_on_remove: Option<bool>,
    pub on_error: OnError,
}

/// `process.run`: starts a program, optionally waiting for it to finish.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessAction {
    /// Name it is tracked and stopped by; the command's file stem when unset.
    pub name: Option<String>,
    /// Must render to a full path.
    pub command: Template,
    pub args: Vec<Template>,
    /// Where it runs; `[service] emulator_session` when unset.
    pub session: Option<EmulatorSession>,
    /// Wait up to `timeout` for it to exit; a non-zero exit code or timeout fails.
    pub wait: bool,
    pub timeout: Duration,
    pub single: bool,
    pub stop_on_remove: Option<bool>,
    pub on_error: OnError,
}

impl ProcessAction {
    /// The tracking name for the rendered `command`.
    pub fn name_for(&self, command: &Path) -> String {
        self.name.clone().unwrap_or_else(|| {
            command.file_stem().map_or("process".to_string(), |stem| {
                stem.to_string_lossy().into_owned()
            })
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    LogWrite {
        message: Template,
    },
    /// Applies the inserted disk's hail.cfg automount policy (usb.inserted only).
    EmulatorAutomount,
    /// Starts the configured emulator (no-op if already running).
    EmulatorStart,
    /// Without a name: stops the `[emulator]` handler (no-op if not running). With a
    /// name (or `*`): stops tracked emulators started by hail with that name.
    EmulatorStop {
        emulator: Option<String>,
    },
    EmulatorLaunch(LaunchAction),
    ProcessRun(ProcessAction),
    /// Stops tracked `process.run` processes with this name, or all with `*`.
    ProcessStop {
        name: String,
    },
    ApiCall(ApiAction),
    RpcCall(RpcAction),
    /// Stops the chain and exits hail with `EXIT_QUIT`.
    Quit,
    /// Stops the chain and exits hail with `EXIT_RESTART` so a service manager restarts it.
    Restart,
}

#[derive(Clone, Debug)]
pub struct Config {
    /// `[service] log_file` as written (empty when logging is disabled and it is unset).
    pub log_file: String,
    /// The log file name to use; `None` when `[service] logging = false`.
    pub log: Option<LogName>,
    pub polling: Polling,
    /// [emulator] type; required when any mulator.* action is configured.
    pub emulator: Option<EmulatorKind>,
    /// [emulators]: short names for emulator executables, referenced by hail.cfg `emulator`.
    pub emulators: Emulators,
    /// `[service] emulator_session`: where `[emulators]` executables are started.
    pub emulator_session: EmulatorSession,
    /// `[service] custom_emulators`: which hail.cfg custom emulators may start.
    pub custom_emulators: CustomEmulators,
    /// `[service] stop_on_remove`: stop what a disk's usb.inserted chain started when
    /// the disk is removed.
    pub stop_on_remove: bool,
    /// `[service] stop_timeout_ms`: how long a stopped process may take to close before
    /// it is killed.
    pub stop_timeout: Duration,
    /// `[apis.<name>]` HTTP endpoints for `api.call`.
    pub apis: BTreeMap<String, Api>,
    /// `[rpc.<name>]` JSON-RPC endpoints for `rpc.call`.
    pub rpc: BTreeMap<String, Rpc>,
    /// Ordered action chain per event; events without a chain are observed but ignored.
    pub chains: BTreeMap<EventKind, Vec<Action>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    service: RawService,
    polling: RawPolling,
    emulator: Option<RawEmulator>,
    #[serde(default)]
    emulators: BTreeMap<String, toml::Value>,
    #[serde(default)]
    apis: BTreeMap<String, RawApi>,
    #[serde(default)]
    rpc: BTreeMap<String, RawRpc>,
    #[serde(default)]
    events: BTreeMap<String, Vec<RawAction>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawService {
    logging: Option<bool>,
    log_file: Option<String>,
    log_time: Option<LogClock>,
    emulator_session: Option<EmulatorSession>,
    custom_emulators: Option<CustomEmulators>,
    stop_on_remove: Option<bool>,
    stop_timeout_ms: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawEmulator {
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPolling {
    #[serde(rename = "type")]
    kind: RawPollingType,
    mode: RawMode,
    interval_ms: Option<u64>,
    settle_ms: Option<u64>,
}

#[derive(Deserialize)]
enum RawPollingType {
    #[serde(rename = "usb-volumes")]
    UsbVolumes,
}

#[derive(Deserialize)]
enum RawMode {
    #[serde(rename = "interval")]
    Interval,
    #[serde(rename = "os-events")]
    OsEvents,
}

#[derive(Deserialize)]
#[serde(tag = "action")]
enum RawAction {
    #[serde(rename = "log.write")]
    LogWrite(RawLogWrite),
    #[serde(rename = "emulator.automount")]
    EmulatorAutomount(RawNoParameters),
    #[serde(rename = "emulator.start")]
    EmulatorStart(RawNoParameters),
    #[serde(rename = "emulator.stop")]
    EmulatorStop(RawEmulatorStop),
    #[serde(rename = "emulator.launch")]
    EmulatorLaunch(RawEmulatorLaunch),
    #[serde(rename = "process.run", alias = "process_run")]
    ProcessRun(RawProcessRun),
    #[serde(rename = "process.stop", alias = "process_stop")]
    ProcessStop(RawProcessStop),
    #[serde(rename = "api.call")]
    ApiCall(RawApiCall),
    #[serde(rename = "rpc.call")]
    RpcCall(RawRpcCall),
    #[serde(rename = "hail.quit")]
    Quit(RawNoParameters),
    #[serde(rename = "hail.restart")]
    Restart(RawNoParameters),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawNoParameters {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLogWrite {
    message: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawEmulatorStop {
    emulator: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawEmulatorLaunch {
    emulator: toml::Value,
    image: Option<String>,
    savefile: Option<String>,
    single: Option<bool>,
    stop_on_remove: Option<bool>,
    on_error: Option<OnError>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProcessRun {
    command: String,
    args: Option<toml::Value>,
    name: Option<String>,
    session: Option<EmulatorSession>,
    wait: Option<bool>,
    timeout_ms: Option<u64>,
    single: Option<bool>,
    stop_on_remove: Option<bool>,
    on_error: Option<OnError>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProcessStop {
    name: String,
}

/// Checks a template that must produce a full path: when it has no placeholders it is
/// checked now, otherwise when it runs.
fn path_template(label: &str, text: &str, allowed: &[&str]) -> Result<Template> {
    platform_path_template(label, None, text, allowed)
}

/// Like [`path_template`], for a path used on `os` (`None`: this host).
fn platform_path_template(
    label: &str,
    os: Option<&str>,
    text: &str,
    allowed: &[&str],
) -> Result<Template> {
    let text = text.trim();
    if text.is_empty() {
        bail!("{label} must not be empty");
    }
    let template = Template::parse(text, allowed).with_context(|| label.to_string())?;
    let absolute = match os {
        Some(os) => crate::platform::is_absolute_on(os, text),
        None => Path::new(text).is_absolute(),
    };
    if !template.has_fields() && !absolute {
        bail!("{label} must be a full path (placeholders such as {{mount_point}} may build one), got {text:?}");
    }
    Ok(template)
}

fn check_tracking_name(label: &str, name: &str) -> Result<()> {
    if !is_short_name(name) {
        bail!("{label}: invalid name {name:?} (1-32 letters, digits, - or _)");
    }
    Ok(())
}

/// `emulator` of `emulator.launch`: an `[emulators]` short name, or a table like
/// hail.cfg's `{ custom, name, path, args }`.
fn parse_launch_target(
    label: &str,
    value: &toml::Value,
    allowed: &[&str],
    emulators: &Emulators,
) -> Result<LaunchTarget> {
    let named = |name: &str| -> Result<LaunchTarget> {
        check_tracking_name(label, name)?;
        if let Some(kind) = EmulatorKind::from_short_name(name) {
            bail!(
                "{label}: {} is driven by the [emulator] handler; use emulator.start",
                kind.name()
            );
        }
        let entry = emulators
            .get(name)
            .with_context(|| format!("{label}: emulator {name:?} is not listed in [emulators]"))?;
        Ok(LaunchTarget::Named(entry.name.clone()))
    };
    let table = match value {
        toml::Value::String(name) => return named(name),
        toml::Value::Table(table) => table,
        _ => bail!("{label}: emulator must be a short name or a table (emulator.name = \"...\")"),
    };
    if let Some(key) = table
        .keys()
        .find(|key| !["custom", "name", "path", "args"].contains(&key.as_str()))
    {
        bail!("{label}: emulator: unknown key {key:?} (expected custom, name, path, args)");
    }
    let custom = match table.get("custom") {
        None => false,
        Some(value) => value
            .as_bool()
            .with_context(|| format!("{label}: emulator.custom must be true or false"))?,
    };
    let name = table
        .get("name")
        .with_context(|| format!("{label}: emulator.name is required"))?
        .as_str()
        .with_context(|| format!("{label}: emulator.name must be a string"))?;
    if !custom {
        if let Some(key) = ["path", "args"]
            .iter()
            .find(|key| table.contains_key(**key))
        {
            bail!("{label}: emulator.{key} needs emulator.custom = true");
        }
        return named(name);
    }
    check_tracking_name(label, name)?;
    let path = table
        .get("path")
        .with_context(|| format!("{label}: emulator.custom = true needs emulator.path"))?;
    let path = crate::platform::PerPlatform::parse(
        &format!("{label}: emulator.path"),
        path,
        |label, os, text| platform_path_template(label, os, text, allowed),
    )?;
    let arg_fields: Vec<&str> = allowed.iter().copied().chain(ARG_FIELDS).collect();
    let args = match table.get("args") {
        None => vec![Template::parse("{image}", &ARG_FIELDS)?],
        Some(args) => parse_templates(&format!("{label}: emulator.args"), args, &arg_fields)?,
    };
    Ok(LaunchTarget::Custom {
        name: name.to_string(),
        path,
        args,
    })
}

fn wait_timeout(label: &str, wait: bool, timeout_ms: Option<u64>) -> Result<Duration> {
    if timeout_ms.is_some() && !wait {
        bail!("{label}: timeout_ms applies only with wait = true");
    }
    let timeout_ms = timeout_ms.unwrap_or(DEFAULT_WAIT_TIMEOUT_MS);
    if !(MIN_WAIT_TIMEOUT_MS..=MAX_WAIT_TIMEOUT_MS).contains(&timeout_ms) {
        bail!("{label}: timeout_ms must be {MIN_WAIT_TIMEOUT_MS}..={MAX_WAIT_TIMEOUT_MS}, got {timeout_ms}");
    }
    Ok(Duration::from_millis(timeout_ms))
}

/// Context for parsing one chain's actions.
struct ChainContext<'a> {
    name: &'a str,
    kind: EventKind,
    emulators: &'a Emulators,
    apis: &'a BTreeMap<String, Api>,
    rpc: &'a BTreeMap<String, Rpc>,
}

impl ChainContext<'_> {
    fn stop_on_remove(&self, label: &str, value: Option<bool>) -> Result<Option<bool>> {
        if value.is_some() && self.kind != EventKind::UsbInserted {
            bail!("{label}: stop_on_remove applies only to usb.inserted chains");
        }
        Ok(value)
    }

    fn parse(&self, index: usize, action: RawAction) -> Result<Action> {
        let name = self.name;
        let kind = self.kind;
        let label = format!("events.{name:?}[{index}]");
        let allowed = kind.fields();
        Ok(match action {
            RawAction::LogWrite(log) => Template::parse(&log.message, allowed)
                .map(|message| Action::LogWrite { message })
                .with_context(|| format!("{label} log.write message"))?,
            RawAction::EmulatorAutomount(_) if kind != EventKind::UsbInserted => {
                bail!("{label}: emulator.automount is only valid for usb.inserted")
            }
            RawAction::EmulatorAutomount(_) => Action::EmulatorAutomount,
            RawAction::EmulatorStart(_) => Action::EmulatorStart,
            RawAction::EmulatorStop(stop) => {
                if let Some(emulator) = &stop.emulator {
                    if emulator != "*" {
                        check_tracking_name(&format!("{label} emulator.stop"), emulator)?;
                    }
                }
                Action::EmulatorStop {
                    emulator: stop.emulator,
                }
            }
            RawAction::EmulatorLaunch(launch) => {
                let label = format!("{label} emulator.launch");
                let template = |key: &str, text: Option<String>| {
                    text.map(|text| path_template(&format!("{label}: {key}"), &text, allowed))
                        .transpose()
                };
                Action::EmulatorLaunch(LaunchAction {
                    emulator: parse_launch_target(
                        &label,
                        &launch.emulator,
                        allowed,
                        self.emulators,
                    )?,
                    image: template("image", launch.image)?,
                    savefile: template("savefile", launch.savefile)?,
                    single: launch.single.unwrap_or(true),
                    stop_on_remove: self.stop_on_remove(&label, launch.stop_on_remove)?,
                    on_error: launch.on_error.unwrap_or_default(),
                })
            }
            RawAction::ProcessRun(run) => {
                let label = format!("{label} process.run");
                if let Some(name) = &run.name {
                    check_tracking_name(&label, name)?;
                }
                let wait = run.wait.unwrap_or(false);
                if wait && (run.single.is_some() || run.stop_on_remove.is_some()) {
                    bail!("{label}: single and stop_on_remove apply only without wait = true");
                }
                Action::ProcessRun(ProcessAction {
                    name: run.name,
                    command: path_template(&format!("{label}: command"), &run.command, allowed)?,
                    args: run
                        .args
                        .map(|args| parse_templates(&format!("{label}: args"), &args, allowed))
                        .transpose()?
                        .unwrap_or_default(),
                    session: run.session,
                    wait,
                    timeout: wait_timeout(&label, wait, run.timeout_ms)?,
                    single: run.single.unwrap_or(false),
                    stop_on_remove: self.stop_on_remove(&label, run.stop_on_remove)?,
                    on_error: run.on_error.unwrap_or_default(),
                })
            }
            RawAction::ProcessStop(stop) => {
                if stop.name != "*" {
                    check_tracking_name(&format!("{label} process.stop"), &stop.name)?;
                }
                Action::ProcessStop { name: stop.name }
            }
            RawAction::ApiCall(call) => Action::ApiCall(ApiAction::parse(
                &format!("{label} api.call"),
                call,
                allowed,
                self.apis,
            )?),
            RawAction::RpcCall(call) => Action::RpcCall(RpcAction::parse(
                &format!("{label} rpc.call"),
                call,
                allowed,
                self.rpc,
            )?),
            RawAction::Quit(_) => Action::Quit,
            RawAction::Restart(_) => Action::Restart,
        })
    }
}

impl Action {
    /// The `action = "..."` name.
    pub fn name(&self) -> &'static str {
        match self {
            Action::LogWrite { .. } => "log.write",
            Action::EmulatorAutomount => "emulator.automount",
            Action::EmulatorStart => "emulator.start",
            Action::EmulatorStop { .. } => "emulator.stop",
            Action::EmulatorLaunch(_) => "emulator.launch",
            Action::ProcessRun(_) => "process.run",
            Action::ProcessStop { .. } => "process.stop",
            Action::ApiCall(_) => "api.call",
            Action::RpcCall(_) => "rpc.call",
            Action::Quit => "hail.quit",
            Action::Restart => "hail.restart",
        }
    }

    /// What a failure of this action does to its chain.
    pub fn on_error(&self) -> OnError {
        match self {
            Action::EmulatorLaunch(action) => action.on_error,
            Action::ProcessRun(action) => action.on_error,
            Action::ApiCall(action) => action.on_error,
            Action::RpcCall(action) => action.on_error,
            _ => OnError::Continue,
        }
    }

    /// The action's name when it needs an emulator handler.
    pub fn emulator_name(&self) -> Option<&'static str> {
        match self {
            Action::EmulatorAutomount => Some("emulator.automount"),
            Action::EmulatorStart => Some("emulator.start"),
            Action::EmulatorStop { emulator: None } => Some("emulator.stop"),
            Action::EmulatorStop {
                emulator: Some(name),
            } if EmulatorKind::from_short_name(name).is_some() => Some("emulator.stop"),
            _ => None,
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading configuration {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("invalid configuration {}", path.display()))
    }

    pub fn parse(text: &str) -> Result<Self> {
        let raw: RawConfig = toml::from_str(text).context("parsing TOML")?;
        let logging = raw.service.logging.unwrap_or(true);
        let log_file = match raw.service.log_file {
            Some(log_file) => log_file,
            None if logging => bail!("service.log_file is required unless logging = false"),
            None => String::new(),
        };
        let log_time = raw.service.log_time.unwrap_or_default();
        // Validated even when logging is off, so re-enabling it cannot fail later.
        let log_name = if log_file.is_empty() && !logging {
            None
        } else {
            Some(LogName::parse(&log_file, log_time).context("service.log_file")?)
        };
        let log = log_name.filter(|_| logging);
        let kind = match raw.polling.kind {
            RawPollingType::UsbVolumes => PollingType::UsbVolumes,
        };
        let mode = match (
            raw.polling.mode,
            raw.polling.interval_ms,
            raw.polling.settle_ms,
        ) {
            (RawMode::Interval, _, Some(_)) => {
                bail!("polling.settle_ms applies only to mode = \"os-events\"")
            }
            (RawMode::Interval, None, None) => {
                bail!("mode = \"interval\" requires polling.interval_ms")
            }
            (RawMode::Interval, Some(interval_ms), None) => {
                if !(MIN_INTERVAL_MS..=MAX_INTERVAL_MS).contains(&interval_ms) {
                    bail!(
                        "polling.interval_ms must be {MIN_INTERVAL_MS}..={MAX_INTERVAL_MS}, got {interval_ms}"
                    );
                }
                Mode::Interval { interval_ms }
            }
            (RawMode::OsEvents, Some(_), _) => {
                bail!("polling.interval_ms applies only to mode = \"interval\"")
            }
            (RawMode::OsEvents, None, settle_ms) => {
                let settle_ms = settle_ms.unwrap_or(DEFAULT_SETTLE_MS);
                if !(MIN_SETTLE_MS..=MAX_SETTLE_MS).contains(&settle_ms) {
                    bail!(
                        "polling.settle_ms must be {MIN_SETTLE_MS}..={MAX_SETTLE_MS}, got {settle_ms}"
                    );
                }
                Mode::OsEvents { settle_ms }
            }
        };
        let polling = Polling { kind, mode };
        let emulator = raw
            .emulator
            .map(|emulator| {
                EmulatorKind::from_name(&emulator.kind).with_context(|| {
                    format!(
                        "unknown emulator.type {:?}; supported: {}",
                        emulator.kind,
                        EmulatorKind::ALL.map(EmulatorKind::name).join(", ")
                    )
                })
            })
            .transpose()?;
        let emulators = Emulators::parse(
            raw.emulators
                .iter()
                .map(|(name, value)| (name.as_str(), value)),
        )?;
        let apis = parse_apis(raw.apis)?;
        let rpc = parse_rpcs(raw.rpc)?;
        let stop_timeout_ms = raw
            .service
            .stop_timeout_ms
            .unwrap_or(DEFAULT_STOP_TIMEOUT_MS);
        if !(MIN_STOP_TIMEOUT_MS..=MAX_STOP_TIMEOUT_MS).contains(&stop_timeout_ms) {
            bail!("service.stop_timeout_ms must be {MIN_STOP_TIMEOUT_MS}..={MAX_STOP_TIMEOUT_MS}, got {stop_timeout_ms}");
        }
        let mut chains = BTreeMap::new();
        for (name, raw_actions) in raw.events {
            let kind = EventKind::from_name(&name).with_context(|| {
                format!(
                    "unknown event {name:?}; supported: {}",
                    EventKind::ALL.map(EventKind::name).join(", ")
                )
            })?;
            let context = ChainContext {
                name: &name,
                kind,
                emulators: &emulators,
                apis: &apis,
                rpc: &rpc,
            };
            let actions = raw_actions
                .into_iter()
                .enumerate()
                .map(|(index, action)| context.parse(index, action))
                .collect::<Result<Vec<_>>>()?;
            if emulator.is_none() {
                if let Some(action) = actions.iter().find_map(Action::emulator_name) {
                    bail!("events.{name:?}: {action} requires an [emulator] section with type = \"snow\"");
                }
            }
            chains.insert(kind, actions);
        }
        Ok(Self {
            log_file,
            log,
            polling,
            emulator,
            emulators,
            emulator_session: raw.service.emulator_session.unwrap_or_default(),
            custom_emulators: raw.service.custom_emulators.unwrap_or_default(),
            stop_on_remove: raw.service.stop_on_remove.unwrap_or(true),
            stop_timeout: Duration::from_millis(stop_timeout_ms),
            apis,
            rpc,
            chains,
        })
    }

    pub fn chain(&self, kind: EventKind) -> &[Action] {
        self.chains.get(&kind).map_or(&[], Vec::as_slice)
    }
}
