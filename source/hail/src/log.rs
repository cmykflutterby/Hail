use crate::event::{unix_seconds, utc_timestamp, CivilTime};
use crate::template::Template;
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

/// Time tokens allowed in `[service] log_file`.
pub const LOG_NAME_TOKENS: [&str; 9] = [
    "year",
    "month",
    "day",
    "hour",
    "minute",
    "date",
    "week",
    "week_year",
    "weekday",
];

const WEEKDAYS: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];

/// Clock used to render the time tokens of a log file name.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum LogClock {
    #[default]
    Local,
    Utc,
}

impl LogClock {
    pub fn name(self) -> &'static str {
        match self {
            LogClock::Local => "local",
            LogClock::Utc => "utc",
        }
    }

    /// Calendar fields of `time` on this clock.
    pub fn civil(self, time: SystemTime) -> CivilTime {
        let seconds = unix_seconds(time);
        let offset = match self {
            LogClock::Local => local_offset_seconds(seconds),
            LogClock::Utc => 0,
        };
        CivilTime::from_unix(seconds + offset)
    }
}

/// Offset of the host's local time zone from UTC at `unix` seconds.
#[cfg(windows)]
fn local_offset_seconds(unix: i64) -> i64 {
    use windows_sys::Win32::Foundation::{FILETIME, SYSTEMTIME};
    use windows_sys::Win32::System::Time::{
        FileTimeToSystemTime, SystemTimeToFileTime, SystemTimeToTzSpecificLocalTime,
    };
    let ticks = (unix + 11_644_473_600) * 10_000_000;
    let utc_file_time = FILETIME {
        dwLowDateTime: ticks as u32,
        dwHighDateTime: (ticks >> 32) as u32,
    };
    // SAFETY: plain-data structs fully written by the calls below before use.
    let mut utc: SYSTEMTIME = unsafe { std::mem::zeroed() };
    let mut local: SYSTEMTIME = unsafe { std::mem::zeroed() };
    let mut local_file_time = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    // SAFETY: every pointer refers to a live local; a null time zone means the current one.
    let ok = unsafe {
        FileTimeToSystemTime(&utc_file_time, &mut utc) != 0
            && SystemTimeToTzSpecificLocalTime(std::ptr::null(), &utc, &mut local) != 0
            && SystemTimeToFileTime(&local, &mut local_file_time) != 0
    };
    if !ok {
        return 0;
    }
    let local_ticks = (i64::from(local_file_time.dwHighDateTime) << 32)
        | i64::from(local_file_time.dwLowDateTime);
    (local_ticks - ticks) / 10_000_000
}

#[cfg(target_os = "linux")]
fn local_offset_seconds(unix: i64) -> i64 {
    let time = unix as libc::time_t;
    // SAFETY: `tm` is plain data written by localtime_r; both pointers are valid locals.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&time, &mut tm) }.is_null() {
        return 0;
    }
    // c_long is i32 on 32-bit Linux.
    #[allow(clippy::useless_conversion)]
    let offset = i64::from(tm.tm_gmtoff);
    offset
}

#[cfg(not(any(windows, target_os = "linux")))]
fn local_offset_seconds(_unix: i64) -> i64 {
    0
}

/// `[service] log_file`: a plain file name, optionally with `{year}`-style time tokens.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LogName {
    pattern: String,
    template: Template,
    clock: LogClock,
}

impl LogName {
    pub fn parse(pattern: &str, clock: LogClock) -> Result<Self> {
        if pattern.contains(['/', '\\', ':']) {
            bail!("must be a plain file name (no folders), got {pattern:?}");
        }
        let template = Template::parse(pattern, &LOG_NAME_TOKENS)?;
        let name = Self {
            pattern: pattern.to_string(),
            template,
            clock,
        };
        let sample = name.render(SystemTime::UNIX_EPOCH);
        if sample.is_empty() || sample == "." || sample == ".." {
            bail!("must be a plain file name, got {pattern:?}");
        }
        Ok(name)
    }

    pub fn pattern(&self) -> &str {
        &self.pattern
    }

    pub fn clock(&self) -> LogClock {
        self.clock
    }

    /// True when the name contains time tokens, so a new file starts as time moves on.
    pub fn is_timed(&self) -> bool {
        self.render(SystemTime::UNIX_EPOCH)
            != self.render(
                SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(400 * 86_400 + 3_661),
            )
    }

    /// The file name for an entry written at `time`.
    pub fn render(&self, time: SystemTime) -> String {
        let civil = self.clock.civil(time);
        let mut fields = BTreeMap::new();
        fields.insert("year", format!("{:04}", civil.year));
        fields.insert("month", format!("{:02}", civil.month));
        fields.insert("day", format!("{:02}", civil.day));
        fields.insert("hour", format!("{:02}", civil.hour));
        fields.insert("minute", format!("{:02}", civil.minute));
        fields.insert(
            "date",
            format!("{:04}-{:02}-{:02}", civil.year, civil.month, civil.day),
        );
        fields.insert("week", format!("{:02}", civil.iso_week));
        fields.insert("week_year", format!("{:04}", civil.iso_week_year));
        fields.insert("weekday", WEEKDAYS[civil.weekday as usize - 1].to_string());
        self.template.render(&fields)
    }

    pub fn describe(&self) -> String {
        if self.is_timed() {
            format!(
                "{} (new file per period, {} time)",
                self.pattern,
                self.clock.name()
            )
        } else {
            self.pattern.clone()
        }
    }
}

/// Host-side text log. It is never placed on a detected USB volume.
///
/// A disabled log (`[service] logging = false`) accepts and discards every entry.
/// With time tokens in the file name, each write goes to the file for the current
/// period, so a new file is started when an entry crosses a time boundary.
pub struct EventLog {
    /// Current log file; empty when logging is disabled.
    pub path: PathBuf,
    file: Option<File>,
    name: Option<LogName>,
    current_name: String,
    binary_directory: PathBuf,
    temporary_directory: PathBuf,
    excluded_roots: Vec<PathBuf>,
}

fn normalized(path: &Path) -> Vec<String> {
    path.components()
        .filter(|component| !matches!(component, Component::CurDir))
        .map(|component| {
            let text = component.as_os_str().to_string_lossy();
            let text = text.trim_end_matches(['\\', '/']);
            if cfg!(windows) {
                text.to_ascii_lowercase()
            } else {
                text.to_string()
            }
        })
        .collect()
}

pub fn is_within(path: &Path, root: &Path) -> bool {
    let root = normalized(root);
    !root.is_empty() && normalized(path).starts_with(&root)
}

/// Opens `file_name` beside the executable, falling back to `temporary_directory`
/// when that location is unwritable or lies on one of `excluded_roots`.
fn open_file(
    binary_directory: &Path,
    temporary_directory: &Path,
    file_name: &str,
    excluded_roots: &[PathBuf],
) -> Result<(PathBuf, File)> {
    let primary = binary_directory.join(file_name);
    let reason = match excluded_roots
        .iter()
        .find(|root| is_within(binary_directory, root))
    {
        Some(root) => format!("it is on detected USB volume {}", root.display()),
        None => match OpenOptions::new().create(true).append(true).open(&primary) {
            Ok(file) => return Ok((primary, file)),
            Err(error) => error.to_string(),
        },
    };
    let fallback = temporary_directory.join(file_name);
    if let Some(root) = excluded_roots
        .iter()
        .find(|root| is_within(temporary_directory, root))
    {
        bail!(
            "no safe log location: {} ({reason}) and temporary directory is on USB volume {}",
            primary.display(),
            root.display()
        );
    }
    eprintln!(
        "hail: cannot log beside executable at {}: {reason}. Using {}.",
        primary.display(),
        fallback.display()
    );
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&fallback)
        .with_context(|| {
            format!(
                "opening fallback log {} after {}: {reason}",
                fallback.display(),
                primary.display()
            )
        })?;
    Ok((fallback, file))
}

impl EventLog {
    /// A log that discards every entry.
    pub fn disabled() -> Self {
        Self {
            path: PathBuf::new(),
            file: None,
            name: None,
            current_name: String::new(),
            binary_directory: PathBuf::new(),
            temporary_directory: PathBuf::new(),
            excluded_roots: Vec::new(),
        }
    }

    /// Opens `file_name` (which may contain time tokens, rendered in local time)
    /// beside the executable, or in `temporary_directory` (see [`EventLog::open_named`]).
    pub fn open(
        binary_directory: &Path,
        temporary_directory: &Path,
        file_name: &str,
        excluded_roots: &[PathBuf],
    ) -> Result<Self> {
        let name = LogName::parse(file_name, LogClock::Local)
            .with_context(|| format!("log file name {file_name:?}"))?;
        Self::open_named(binary_directory, temporary_directory, name, excluded_roots)
    }

    pub fn open_named(
        binary_directory: &Path,
        temporary_directory: &Path,
        name: LogName,
        excluded_roots: &[PathBuf],
    ) -> Result<Self> {
        Self::open_at(
            binary_directory,
            temporary_directory,
            name,
            excluded_roots,
            SystemTime::now(),
        )
    }

    /// Opens the log file for the period containing `now`. The file is placed beside
    /// the executable, or in `temporary_directory` when that location is unwritable
    /// or lies on one of `excluded_roots`; later periods are placed the same way.
    pub fn open_at(
        binary_directory: &Path,
        temporary_directory: &Path,
        name: LogName,
        excluded_roots: &[PathBuf],
        now: SystemTime,
    ) -> Result<Self> {
        let current_name = name.render(now);
        let (path, file) = open_file(
            binary_directory,
            temporary_directory,
            &current_name,
            excluded_roots,
        )?;
        Ok(Self {
            path,
            file: Some(file),
            name: Some(name),
            current_name,
            binary_directory: binary_directory.to_path_buf(),
            temporary_directory: temporary_directory.to_path_buf(),
            excluded_roots: excluded_roots.to_vec(),
        })
    }

    /// Like [`EventLog::open_named`], but `None` gives a disabled log that can be
    /// enabled later with [`EventLog::set_name`].
    pub fn open_optional(
        binary_directory: &Path,
        temporary_directory: &Path,
        name: Option<LogName>,
        excluded_roots: &[PathBuf],
    ) -> Result<Self> {
        match name {
            Some(name) => {
                Self::open_named(binary_directory, temporary_directory, name, excluded_roots)
            }
            None => Ok(Self {
                binary_directory: binary_directory.to_path_buf(),
                temporary_directory: temporary_directory.to_path_buf(),
                excluded_roots: excluded_roots.to_vec(),
                ..Self::disabled()
            }),
        }
    }

    /// Switches to another file name/clock, or disables (`None`) the log. The new file
    /// is opened before anything changes, so on error the current log stays in use.
    pub fn set_name(&mut self, name: Option<LogName>) -> Result<()> {
        if name == self.name {
            return Ok(());
        }
        match name {
            None => {
                self.file = None;
                self.name = None;
                self.current_name.clear();
                self.path = PathBuf::new();
            }
            Some(name) => {
                let current_name = name.render(SystemTime::now());
                let (path, file) = open_file(
                    &self.binary_directory,
                    &self.temporary_directory,
                    &current_name,
                    &self.excluded_roots,
                )?;
                self.path = path;
                self.file = Some(file);
                self.name = Some(name);
                self.current_name = current_name;
            }
        }
        Ok(())
    }

    pub fn is_enabled(&self) -> bool {
        self.name.is_some()
    }

    pub fn name(&self) -> Option<&LogName> {
        self.name.as_ref()
    }

    /// Writes a timestamped `[debug]` entry.
    pub fn debug(&mut self, message: &str) -> Result<()> {
        let now = SystemTime::now();
        self.write_line_at(&format!("{} [debug] {message}", utc_timestamp(now)), now)
    }

    pub fn write_line(&mut self, message: &str) -> Result<()> {
        self.write_line_at(message, SystemTime::now())
    }

    /// Appends `message` to the file for the period containing `now`, starting a new
    /// file when the rendered file name has changed since the previous entry.
    pub fn write_line_at(&mut self, message: &str, now: SystemTime) -> Result<()> {
        let Some(name) = &self.name else {
            return Ok(());
        };
        let wanted = name.render(now);
        if wanted != self.current_name || self.file.is_none() {
            // Drop the old handle first so a failed open never writes to a stale period.
            self.file = None;
            let (path, file) = open_file(
                &self.binary_directory,
                &self.temporary_directory,
                &wanted,
                &self.excluded_roots,
            )?;
            self.path = path;
            self.file = Some(file);
            self.current_name = wanted;
        }
        let Some(file) = self.file.as_mut() else {
            return Ok(());
        };
        writeln!(file, "{message}")
            .with_context(|| format!("writing log {}", self.path.display()))?;
        file.sync_data()
            .with_context(|| format!("syncing log {}", self.path.display()))
    }
}
