//! Emulator integration. `[emulator] type` selects the handler that carries out the
//! `emulator.*` actions; each supported emulator implements [`Emulator`].
use crate::event::{utc_timestamp, Event};
use crate::log::EventLog;
use crate::media::{CustomPath, DiskConfigStatus, DiskImage, MountMode, TrackedDisk};
use crate::template::Template;
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

mod launch;
mod snow;

#[cfg(windows)]
pub use launch::close_windows;
pub use launch::{
    environment_block, merge_environment, parse_environment_block, quote_windows_arg, spawn,
    windows_command_line, CloseRequest, EmulatorSession, LaunchError, LaunchRequest, LogindSession,
    Process, Started,
};
pub use snow::{Snow, SnowMount};

/// Placeholders allowed in `[emulators]` `args`.
pub const ARG_FIELDS: [&str; 2] = ["image", "savefile"];

/// Emulators hail can drive (`[emulator] type`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Kind {
    Snow,
}

impl Kind {
    pub const ALL: [Kind; 1] = [Kind::Snow];

    pub fn name(self) -> &'static str {
        match self {
            Kind::Snow => "snow",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.name() == name)
    }

    /// The handler a `hail.cfg` / `[emulators]` short name refers to, ignoring case.
    pub fn from_short_name(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|kind| kind.name().eq_ignore_ascii_case(name))
    }

    /// Creates the handler that performs this emulator's actions.
    pub fn handler(self) -> Box<dyn Emulator> {
        match self {
            Kind::Snow => Box::new(Snow::default()),
        }
    }
}

/// Action handler for one emulator. Implementations own their process/connection state
/// and write `[debug]` log entries describing each call when [`Emulator::debug_logging`].
pub trait Emulator {
    fn kind(&self) -> Kind;
    fn is_running(&self) -> bool;
    /// True while the handler is a stub (every call is logged) or in debug builds.
    fn debug_logging(&self) -> bool;
    /// `emulator.start`: starts the emulator; a no-op if it is already running.
    fn start(&mut self, log: &mut EventLog, trigger: &Event) -> Result<()>;
    /// `emulator.stop`: stops the emulator; a no-op if it is not running.
    fn stop(&mut self, log: &mut EventLog, trigger: &Event) -> Result<()>;
    /// Mounts `image` from `disk` attached as `mode` asks (used by `emulator.automount`);
    /// `savefile` is the hail.cfg savefile for guest writes, when set.
    fn mount(
        &mut self,
        log: &mut EventLog,
        disk: &TrackedDisk,
        image: &DiskImage,
        mode: MountMode,
        savefile: Option<&Path>,
    ) -> Result<()>;
}

/// True for `[emulators]` / `hail.cfg` short names: 1-32 ASCII letters, digits, `-`, `_`.
pub fn is_short_name(name: &str) -> bool {
    (1..=32).contains(&name.len())
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
}

/// One `[emulators]` entry: a short name, the emulator executable (unset when blank)
/// and the arguments it is started with.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EmulatorEntry {
    pub name: String,
    pub path: Option<PathBuf>,
    /// Argument templates using [`ARG_FIELDS`]; `["{image}"]` unless `args` is set.
    pub args: Vec<Template>,
}

impl EmulatorEntry {
    /// True when an argument passes `{savefile}` to the emulator.
    pub fn uses_savefile(&self) -> bool {
        self.args.iter().any(|arg| arg.uses("savefile"))
    }

    /// The rendered arguments. Arguments using `{savefile}` are left out when no
    /// savefile is set.
    pub fn command_args(&self, image: &Path, savefile: Option<&Path>) -> Vec<String> {
        let mut fields = BTreeMap::new();
        fields.insert("image", image.display().to_string());
        if let Some(savefile) = savefile {
            fields.insert("savefile", savefile.display().to_string());
        }
        render_args(&self.args, &fields)
    }
}

/// Renders argument templates; an argument with a placeholder that has no value in
/// `fields` (such as `{savefile}` without a savefile) is left out entirely.
pub fn render_args(args: &[Template], fields: &BTreeMap<&'static str, String>) -> Vec<String> {
    args.iter()
        .filter(|arg| !arg.misses_any(fields))
        .map(|arg| arg.render(fields))
        .collect()
}

/// Parses an array of templates allowing `allowed` placeholders; `label` (e.g.
/// `events."usb.inserted"[0].args`) prefixes error messages.
pub fn parse_templates(
    label: &str,
    value: &toml::Value,
    allowed: &[&str],
) -> Result<Vec<Template>> {
    let items = value
        .as_array()
        .with_context(|| format!("{label} must be an array of strings"))?;
    items
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let text = item
                .as_str()
                .with_context(|| format!("{label}[{index}] must be a string"))?;
            Template::parse(text, allowed).with_context(|| format!("{label}[{index}]"))
        })
        .collect()
}

/// A command line for the log: each part quoted.
pub fn display_command(executable: &Path, args: &[String]) -> String {
    std::iter::once(executable.display().to_string())
        .chain(args.iter().cloned())
        .map(|part| format!("\"{part}\""))
        .collect::<Vec<_>>()
        .join(" ")
}

/// `[emulators]`: short names (matched ignoring case) for emulator executables.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Emulators {
    entries: Vec<EmulatorEntry>,
}

/// Parses an `args` array of [`ARG_FIELDS`] templates (`["{image}"]` when unset);
/// `label` (e.g. `emulators.SNES`) prefixes error messages.
pub fn parse_args(label: &str, value: Option<&toml::Value>) -> Result<Vec<Template>> {
    let Some(value) = value else {
        return Ok(vec![Template::parse("{image}", &ARG_FIELDS)?]);
    };
    let args = parse_templates(&format!("{label}.args"), value, &ARG_FIELDS)?;
    if !args.iter().any(|arg| arg.uses("image")) {
        bail!("{label}.args must pass the image: add an argument with {{image}}");
    }
    Ok(args)
}

/// `[service] custom_emulators`: which hail.cfg `emulator.custom = true` executables
/// may be started.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum CustomEmulators {
    /// No custom emulators.
    None,
    /// Only executables already listed in `[emulators]` (the disk may change the args).
    #[default]
    Defined,
    /// Only executables stored on the inserted disk itself.
    Disk,
    /// Any executable on the host or the disk.
    Any,
}

impl CustomEmulators {
    pub const ALL: [CustomEmulators; 4] = [Self::None, Self::Defined, Self::Disk, Self::Any];

    pub fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Defined => "defined",
            Self::Disk => "disk",
            Self::Any => "any",
        }
    }

    /// Decides whether a custom emulator `executable` from the disk mounted at
    /// `disk_root` may start. Paths are compared after resolving links and `..`, so a
    /// link on the disk cannot point elsewhere. `Ok` explains why it is permitted,
    /// `Err` why it is refused.
    pub fn check(
        self,
        executable: &Path,
        disk_root: &Path,
        emulators: &Emulators,
    ) -> std::result::Result<String, String> {
        let setting = format!("custom_emulators = \"{}\"", self.name());
        let resolve = |path: &Path| {
            path.canonicalize()
                .map_err(|error| format!("{setting}, cannot resolve {}: {error}", path.display()))
        };
        match self {
            Self::None => Err(format!("{setting} allows no hail.cfg custom emulators")),
            Self::Any => Ok(setting),
            Self::Defined => {
                let resolved = resolve(executable)?;
                emulators
                    .entries()
                    .iter()
                    .find(|entry| {
                        entry
                            .path
                            .as_deref()
                            .and_then(|path| path.canonicalize().ok())
                            .is_some_and(|path| path == resolved)
                    })
                    .map(|entry| {
                        format!("{setting}: same executable as [emulators] {}", entry.name)
                    })
                    .ok_or_else(|| {
                        format!(
                            "{setting} and {} is not an [emulators] executable",
                            executable.display()
                        )
                    })
            }
            Self::Disk => {
                let resolved = resolve(executable)?;
                let root = resolve(disk_root)?;
                if resolved.starts_with(&root) {
                    Ok(format!("{setting}: executable is on the inserted disk"))
                } else {
                    Err(format!(
                        "{setting} and {} is not on the inserted disk {}",
                        executable.display(),
                        disk_root.display()
                    ))
                }
            }
        }
    }
}

impl Emulators {
    /// Validates `[emulators]` values: a path string (blank leaves the entry unset), or a
    /// table `{ path = "...", args = ["...", "{image}"] }`.
    pub fn parse<'a>(pairs: impl IntoIterator<Item = (&'a str, &'a toml::Value)>) -> Result<Self> {
        let mut entries: Vec<EmulatorEntry> = Vec::new();
        for (name, value) in pairs {
            if !is_short_name(name) {
                bail!("emulators: invalid short name {name:?} (1-32 letters, digits, - or _)");
            }
            if let Some(other) = entries
                .iter()
                .find(|entry| entry.name.eq_ignore_ascii_case(name))
            {
                bail!(
                    "emulators: {name:?} and {:?} are the same short name",
                    other.name
                );
            }
            let (path, args) = match value {
                toml::Value::String(path) => (path.as_str(), None),
                toml::Value::Table(table) => {
                    if let Some(key) = table.keys().find(|key| !["path", "args"].contains(&key.as_str())) {
                        bail!("emulators.{name}: unknown key {key:?} (expected path, args)");
                    }
                    let path = table
                        .get("path")
                        .and_then(toml::Value::as_str)
                        .with_context(|| format!("emulators.{name}.path: expected a string"))?;
                    (path, table.get("args"))
                }
                _ => bail!(
                    "emulators.{name}: expected a string path or a table {{ path = \"...\", args = [...] }}"
                ),
            };
            let path = path.trim();
            let path = if path.is_empty() {
                None
            } else if Path::new(path).is_absolute() {
                Some(PathBuf::from(path))
            } else {
                bail!("emulators.{name} must be the full path to the emulator executable, got {path:?}");
            };
            entries.push(EmulatorEntry {
                name: name.to_string(),
                path,
                args: parse_args(&format!("emulators.{name}"), args)?,
            });
        }
        entries.sort_by_key(|entry| entry.name.to_ascii_lowercase());
        Ok(Self { entries })
    }

    pub fn get(&self, name: &str) -> Option<&EmulatorEntry> {
        self.entries
            .iter()
            .find(|entry| entry.name.eq_ignore_ascii_case(name))
    }

    pub fn entries(&self) -> &[EmulatorEntry] {
        &self.entries
    }

    /// Entries with a path set.
    pub fn configured(&self) -> usize {
        self.entries
            .iter()
            .filter(|entry| entry.path.is_some())
            .count()
    }
}

/// What starting `image` with an `[emulators]` entry runs: its executable with the
/// rendered `args`, in the executable's folder, with `HAIL_IMAGE` and `HAIL_SAVEFILE`
/// (empty without a savefile) in its environment.
pub fn launch_request(
    entry: &EmulatorEntry,
    executable: &Path,
    image: &Path,
    savefile: Option<&Path>,
) -> LaunchRequest {
    LaunchRequest {
        executable: executable.to_path_buf(),
        args: entry.command_args(image, savefile),
        env: vec![
            ("HAIL_IMAGE".to_string(), image.display().to_string()),
            (
                "HAIL_SAVEFILE".to_string(),
                savefile.map_or(String::new(), |path| path.display().to_string()),
            ),
        ],
        directory: executable.parent().map(Path::to_path_buf),
    }
}

/// Opens `image` with the `[emulators]` executable for a hail.cfg `emulator` other than
/// the `[emulator]` handler, or with a hail.cfg custom emulator (`custom` holds why
/// `[service] custom_emulators` permitted it), in the session `session` selects. The
/// emulator runs independently of hail (it keeps running if hail stops). The launch, or
/// why it did not happen, is written to the log; a failed start is not an action
/// failure. Returns the started process so it can be tracked.
#[allow(clippy::too_many_arguments)]
pub fn launch_external(
    log: &mut EventLog,
    disk: &TrackedDisk,
    entry: &EmulatorEntry,
    executable: &Path,
    image: &Path,
    savefile: Option<&Path>,
    mode: MountMode,
    session: EmulatorSession,
    custom: Option<&str>,
) -> Result<Option<Started>> {
    let request = launch_request(entry, executable, image, savefile);
    let args_from = if custom.is_some() {
        "hail.cfg emulator.args"
    } else {
        "[emulators] args"
    };
    let origin = custom.map_or(String::new(), |reason| {
        format!("  emulator: custom, defined by hail.cfg (permitted by {reason})\n")
    });
    let size = std::fs::metadata(image).map_or("unavailable".to_string(), |metadata| {
        format!("{} bytes", metadata.len())
    });
    let saves_through_args = savefile.is_some() && entry.uses_savefile();
    let mode_line = if mode == MountMode::default() {
        "default".to_string()
    } else if mode.writeback == Some(true) && saves_through_args {
        format!(
            "{} (applied: {} saves to the savefile)",
            mode.describe(),
            entry.name
        )
    } else {
        format!(
            "{} (not applied: {} only receives its {args_from})",
            mode.describe(),
            entry.name
        )
    };
    let savefile_line = savefile.map_or(String::new(), |savefile| {
        let state = if savefile.is_file() { "exists" } else { "new" };
        let passed = if saves_through_args {
            "passed in args and HAIL_SAVEFILE".to_string()
        } else {
            format!("not in {} {args_from}; set in HAIL_SAVEFILE", entry.name)
        };
        let folder = match savefile.parent() {
            Some(parent) if !parent.is_dir() => {
                format!(", folder {} does not exist", parent.display())
            }
            _ => String::new(),
        };
        format!(
            "\n  savefile: {} ({state}, {passed}{folder})",
            savefile.display()
        )
    });
    let command = display_command(executable, &request.args);
    let details = |session: &str| {
        format!(
            "{origin}  command: {command}\n  session: {session}\n  disk: device={} label={} mount_point={} inserted_at={}\n  image: path={} size={size}{savefile_line}\n  mode: {mode_line}\n{}",
            disk.disk.device,
            disk.disk.label,
            disk.disk.mount_point.display(),
            disk.inserted_at,
            image.display(),
            describe_hail_cfg(disk)
        )
    };
    let timestamp = utc_timestamp(SystemTime::now());
    match launch::spawn(&request, session) {
        Ok(started) => {
            log.write_line(&format!(
                "{timestamp} [emulator] emulator.launch {} started (pid {})\n{}",
                entry.name,
                started.pid,
                details(&started.session)
            ))?;
            Ok(Some(started))
        }
        Err(LaunchError::NoUser(reason)) => {
            log.write_line(&format!(
                "{timestamp} [emulator] emulator.launch {} not started: no user is logged in ({reason})\n{}",
                entry.name,
                details("none")
            ))?;
            Ok(None)
        }
        Err(LaunchError::Failed(error)) => {
            log.write_line(&format!(
                "{timestamp} [error] emulator.launch {} failed to start {}: {error}\n{}",
                entry.name,
                executable.display(),
                details(session.name())
            ))?;
            Ok(None)
        }
    }
}

/// Logs that a hail.cfg custom emulator was not started because
/// `[service] custom_emulators` refused it.
pub fn refuse_custom(
    log: &mut EventLog,
    disk: &TrackedDisk,
    name: &str,
    executable: &Path,
    reason: &str,
) -> Result<()> {
    log.write_line(&format!(
        "{} [emulator] emulator.launch {name} refused: {reason}\n  executable: {}\n  disk: device={} label={} mount_point={} inserted_at={}\n{}",
        utc_timestamp(SystemTime::now()),
        executable.display(),
        disk.disk.device,
        disk.disk.label,
        disk.disk.mount_point.display(),
        disk.inserted_at,
        describe_hail_cfg(disk)
    ))
}

/// Logs that a hail.cfg custom emulator was not started because its `emulator.path`
/// has no entry for this host.
pub fn custom_unavailable(
    log: &mut EventLog,
    disk: &TrackedDisk,
    name: &str,
    reason: &str,
) -> Result<()> {
    log.write_line(&format!(
        "{} [emulator] emulator.launch {name} skipped: {reason}\n  disk: device={} label={} mount_point={} inserted_at={}\n{}",
        utc_timestamp(SystemTime::now()),
        disk.disk.device,
        disk.disk.label,
        disk.disk.mount_point.display(),
        disk.inserted_at,
        describe_hail_cfg(disk)
    ))
}

fn time(time: Option<SystemTime>) -> String {
    time.map_or_else(|| "unavailable".to_string(), utc_timestamp)
}

/// Multi-line description of a mount call: disk, image (with CD-ROM cue tracks), the
/// requested mode and hail.cfg.
pub fn describe_mount(disk: &TrackedDisk, image: &DiskImage, mode: MountMode) -> String {
    let tracks = image.cue.as_ref().map_or(String::new(), |cue| {
        let files = cue.files.iter().map(|file| {
            format!(
                "\n    file: {} {} ({} bytes)",
                file.format,
                file.path.display(),
                file.size
            )
        });
        let tracks = cue
            .tracks
            .iter()
            .map(|track| format!("\n    track {:02}: {}", track.number, track.mode));
        files.chain(tracks).collect()
    });
    let hail_cfg = describe_hail_cfg(disk);
    format!(
        "  disk: device={} label={} mount_point={} inserted_at={}\n  image: path={} kind={} size={} bytes data={} bytes modified={} created={}{tracks}\n  mode: {}\n{hail_cfg}",
        disk.disk.device,
        disk.disk.label,
        disk.disk.mount_point.display(),
        disk.inserted_at,
        image.path.display(),
        image.kind.name(),
        image.size,
        image.data_size(),
        time(image.modified),
        time(image.created),
        mode.describe(),
    )
}

/// The disk's hail.cfg status, summary and contents.
fn describe_hail_cfg(disk: &TrackedDisk) -> String {
    match &disk.scan.config {
        DiskConfigStatus::Absent => "  hail.cfg: none".to_string(),
        DiskConfigStatus::Invalid { path, error } => {
            format!("  hail.cfg: {} (invalid: {error})", path.display())
        }
        DiskConfigStatus::Found(config) => format!(
            "  hail.cfg: {} (automount={} emulator={}{} image={} select={} mode={} savefile={})\n{}",
            config.path.display(),
            config.automount,
            config.emulator.as_deref().unwrap_or("-"),
            config.custom.as_ref().map_or(String::new(), |custom| format!(
                " custom={} args={}",
                custom.path.describe(CustomPath::describe),
                custom.args.len()
            )),
            config
                .image
                .as_ref()
                .map_or("-".to_string(), |image| image.display().to_string()),
            config.select.name(),
            config.mode.describe(),
            config
                .savefile
                .as_ref()
                .map_or("-".to_string(), |savefile| savefile.display().to_string()),
            config
                .text
                .lines()
                .map(|line| format!("    | {line}"))
                .collect::<Vec<_>>()
                .join("\n")
        ),
    }
}
