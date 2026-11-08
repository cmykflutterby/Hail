//! Host platform names and per-platform settings (`path.<os>.<arch> = "..."`).
use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::sync::OnceLock;

pub const OSES: [&str; 2] = ["windows", "linux"];
pub const ARCHES: [&str; 2] = ["x64", "arm64"];

/// `windows` or `linux` (another target's own name elsewhere).
pub fn host_os() -> &'static str {
    std::env::consts::OS
}

/// The machine's own architecture, `x64` or `arm64` (another target's own name
/// elsewhere). On Windows this is the native machine even when this build runs
/// under emulation (an x64 hail on Windows on Arm reports `arm64`).
pub fn host_arch() -> &'static str {
    static ARCH: OnceLock<&'static str> = OnceLock::new();
    ARCH.get_or_init(|| {
        native_arch().unwrap_or(match std::env::consts::ARCH {
            "x86_64" => "x64",
            "aarch64" => "arm64",
            other => other,
        })
    })
}

/// Whether this host can run x64 programs although it is not x64: Windows on Arm
/// with x64 emulation (Windows 11). Linux has no standard x64 emulation.
pub fn host_runs_x64() -> bool {
    static RUNS: OnceLock<bool> = OnceLock::new();
    *RUNS.get_or_init(|| host_arch() == "arm64" && x64_emulation())
}

/// `<os>.<arch>` of this host, e.g. `windows.x64`.
pub fn host() -> String {
    format!("{}.{}", host_os(), host_arch())
}

#[cfg(windows)]
fn native_arch() -> Option<&'static str> {
    use windows_sys::Win32::System::SystemInformation::{
        IMAGE_FILE_MACHINE_AMD64, IMAGE_FILE_MACHINE_ARM64,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, IsWow64Process2};
    let (mut process, mut native) = (0, 0);
    // SAFETY: both out-pointers are valid locals; the pseudo handle needs no closing.
    if unsafe { IsWow64Process2(GetCurrentProcess(), &mut process, &mut native) } == 0 {
        return None;
    }
    match native {
        IMAGE_FILE_MACHINE_AMD64 => Some("x64"),
        IMAGE_FILE_MACHINE_ARM64 => Some("arm64"),
        _ => None,
    }
}

#[cfg(not(windows))]
fn native_arch() -> Option<&'static str> {
    None
}

#[cfg(windows)]
fn x64_emulation() -> bool {
    use windows_sys::Win32::System::SystemInformation::{
        IsWow64GuestMachineSupported, IMAGE_FILE_MACHINE_AMD64,
    };
    let mut supported = 0;
    // SAFETY: `supported` is a valid local out-pointer.
    let result = unsafe { IsWow64GuestMachineSupported(IMAGE_FILE_MACHINE_AMD64, &mut supported) };
    result >= 0 && supported != 0
}

#[cfg(not(windows))]
fn x64_emulation() -> bool {
    false
}

/// The keys a per-platform setting is looked up by on `os`/`arch`, best first:
/// `<os>.<arch>`, `<os>` (every architecture), then `<os>.x64` when an Arm host
/// runs x64 programs under emulation (`runs_x64`).
pub fn candidates(os: &str, arch: &str, runs_x64: bool) -> Vec<String> {
    let mut keys = vec![format!("{os}.{arch}"), os.to_string()];
    if runs_x64 && arch != "x64" {
        keys.push(format!("{os}.x64"));
    }
    keys
}

/// Whether `path` is a full path on `os`, which need not be this host: a drive
/// (`C:\`, `C:/`) or UNC path (`\\server\share`) on Windows, `/...` on Linux.
pub fn is_absolute_on(os: &str, path: &str) -> bool {
    if os == "windows" {
        let bytes = path.as_bytes();
        path.starts_with("\\\\")
            || path.starts_with("//")
            || (bytes.len() >= 3
                && bytes[0].is_ascii_alphabetic()
                && bytes[1] == b':'
                && matches!(bytes[2], b'\\' | b'/'))
    } else {
        path.starts_with('/')
    }
}

/// A setting given once for every host, or per platform.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PerPlatform<T> {
    Any(T),
    /// Keyed `<os>.<arch>`, or `<os>` for every architecture of that OS.
    Platforms(BTreeMap<String, T>),
}

impl<T> PerPlatform<T> {
    /// Parses a string (every host) or a table `{ <os> = "...", <os> = { <arch> = "..." } }`.
    /// `parse_one(label, os, text)` gets `os = None` for the every-host form.
    pub fn parse(
        label: &str,
        value: &toml::Value,
        mut parse_one: impl FnMut(&str, Option<&str>, &str) -> Result<T>,
    ) -> Result<Self> {
        let table = match value {
            toml::Value::String(text) => return Ok(Self::Any(parse_one(label, None, text)?)),
            toml::Value::Table(table) => table,
            _ => bail!(
                "{label} must be a path, or paths per platform ({label}.<os>.<arch> = \"...\")"
            ),
        };
        let mut entries = BTreeMap::new();
        for (os, value) in table {
            if !OSES.contains(&os.as_str()) {
                bail!("{label}: unknown OS {os:?} (expected {})", OSES.join(", "));
            }
            let os_label = format!("{label}.{os}");
            match value {
                toml::Value::String(text) => {
                    entries.insert(os.clone(), parse_one(&os_label, Some(os), text)?);
                }
                toml::Value::Table(arches) => {
                    for (arch, value) in arches {
                        if !ARCHES.contains(&arch.as_str()) {
                            bail!(
                                "{os_label}: unknown architecture {arch:?} (expected {})",
                                ARCHES.join(", ")
                            );
                        }
                        let entry_label = format!("{os_label}.{arch}");
                        let text = value
                            .as_str()
                            .with_context(|| format!("{entry_label} must be a string"))?;
                        entries.insert(
                            format!("{os}.{arch}"),
                            parse_one(&entry_label, Some(os), text)?,
                        );
                    }
                }
                _ => bail!("{os_label} must be a path, or paths per architecture ({os_label}.<arch> = \"...\")"),
            }
        }
        if entries.is_empty() {
            bail!("{label} lists no platforms");
        }
        Ok(Self::Platforms(entries))
    }

    /// The entry for this host (see [`candidates`]).
    pub fn for_host(&self) -> std::result::Result<&T, String> {
        self.for_platform(host_os(), host_arch(), host_runs_x64())
    }

    /// The entry for `os`/`arch`: the first of [`candidates`] that is defined.
    pub fn for_platform(
        &self,
        os: &str,
        arch: &str,
        runs_x64: bool,
    ) -> std::result::Result<&T, String> {
        let entries = match self {
            Self::Any(value) => return Ok(value),
            Self::Platforms(entries) => entries,
        };
        let keys = candidates(os, arch, runs_x64);
        keys.iter().find_map(|key| entries.get(key)).ok_or_else(|| {
            format!(
                "no entry for this host ({os}.{arch}; looked for {}); defined for {}",
                keys.join(", "),
                entries.keys().cloned().collect::<Vec<_>>().join(", ")
            )
        })
    }

    /// `describe_one(value)`, or `<platform>=<value>` for each platform.
    pub fn describe(&self, describe_one: impl Fn(&T) -> String) -> String {
        match self {
            Self::Any(value) => describe_one(value),
            Self::Platforms(entries) => entries
                .iter()
                .map(|(platform, value)| format!("{platform}={}", describe_one(value)))
                .collect::<Vec<_>>()
                .join(" "),
        }
    }
}
