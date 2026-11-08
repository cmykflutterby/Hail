//! Read-only inspection of inserted volumes: disk images and the optional `hail.cfg`.
use crate::discovery::Disk;
use crate::template::Template;
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Per-disk configuration file, looked up (case-insensitively) in the volume root.
pub const DISK_CONFIG_NAME: &str = "hail.cfg";
const MAX_DISK_CONFIG_BYTES: u64 = 64 * 1024;
const MAX_CUE_SHEET_BYTES: u64 = 64 * 1024;
/// Snow's CD-ROM image formats (`.cue` sheets reference BINARY/WAVE track files).
pub const CD_ROM_EXTENSIONS: [&str; 3] = ["iso", "toast", "cue"];
/// `.img`/`.dsk`/`.image`/`.raw` files up to this size are floppies; larger ones are
/// hard disks (Snow likewise tries its floppy loader before attaching a hard disk).
pub const MAX_FLOPPY_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImageKind {
    Floppy,
    HardDisk,
    CdRom,
}

impl ImageKind {
    pub fn name(self) -> &'static str {
        match self {
            ImageKind::Floppy => "floppy",
            ImageKind::HardDisk => "hard-disk",
            ImageKind::CdRom => "cd-rom",
        }
    }

    /// Classifies by Snow's file extensions; `None` for files Snow cannot mount.
    pub fn classify(path: &Path, size: u64) -> Option<Self> {
        let extension = path.extension()?.to_str()?.to_ascii_lowercase();
        match extension.as_str() {
            extension if CD_ROM_EXTENSIONS.contains(&extension) => Some(ImageKind::CdRom),
            "hda" => Some(ImageKind::HardDisk),
            "img" | "dsk" | "image" | "raw" if size > MAX_FLOPPY_BYTES => Some(ImageKind::HardDisk),
            "img" | "dsk" | "image" | "raw" | "moof" | "a2r" | "dc42" | "dart" | "pfi" | "pri"
            | "woz" | "2mg" | "2img" | "po" => Some(ImageKind::Floppy),
            _ => None,
        }
    }
}

/// A track data file referenced by a cue sheet.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CueFile {
    pub path: PathBuf,
    /// `BINARY` or `WAVE`.
    pub format: String,
    pub size: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CueTrack {
    pub number: u8,
    /// `AUDIO`, `MODE1/2352`, `MODE1/2048` or `MODE2/2352`.
    pub mode: String,
}

/// Parsed `.cue` sheet whose track files all exist beside it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CueSheet {
    pub files: Vec<CueFile>,
    pub tracks: Vec<CueTrack>,
}

fn cue_word(text: &str) -> Option<(&str, &str)> {
    let text = text.trim_start();
    if let Some(quoted) = text.strip_prefix('"') {
        let end = quoted.find('"')?;
        return Some((&quoted[..end], &quoted[end + 1..]));
    }
    let end = text.find(char::is_whitespace).unwrap_or(text.len());
    (end > 0).then(|| (&text[..end], &text[end..]))
}

impl CueSheet {
    /// Parses the commands Snow uses. Referenced files must exist and stay on the disk.
    pub fn parse(directory: &Path, text: &str) -> Result<Self> {
        let mut files = Vec::new();
        let mut tracks = Vec::new();
        for (index, line) in text.lines().enumerate() {
            let line_number = index + 1;
            let Some((command, rest)) = cue_word(line) else {
                continue;
            };
            match command.to_ascii_uppercase().as_str() {
                "FILE" => {
                    let (name, rest) = cue_word(rest)
                        .with_context(|| format!("line {line_number}: FILE needs a path"))?;
                    let (format, _) = cue_word(rest)
                        .with_context(|| format!("line {line_number}: FILE needs a type"))?;
                    let format = format.to_ascii_uppercase();
                    if !matches!(format.as_str(), "BINARY" | "WAVE") {
                        bail!("line {line_number}: unsupported FILE type {format}");
                    }
                    let path = directory.join(
                        relative_disk_path("image", name)
                            .with_context(|| format!("line {line_number}: FILE"))?,
                    );
                    let metadata = fs::metadata(&path).with_context(|| {
                        format!("line {line_number}: track file {}", path.display())
                    })?;
                    if !metadata.is_file() {
                        bail!("line {line_number}: {} is not a file", path.display());
                    }
                    files.push(CueFile {
                        path,
                        format,
                        size: metadata.len(),
                    });
                }
                "TRACK" => {
                    let (number, rest) = cue_word(rest)
                        .with_context(|| format!("line {line_number}: TRACK needs a number"))?;
                    let number = number
                        .parse()
                        .with_context(|| format!("line {line_number}: TRACK number"))?;
                    let (mode, _) = cue_word(rest)
                        .with_context(|| format!("line {line_number}: TRACK needs a mode"))?;
                    let mode = mode.to_ascii_uppercase();
                    if !matches!(
                        mode.as_str(),
                        "AUDIO" | "MODE1/2352" | "MODE1/2048" | "MODE2/2352"
                    ) {
                        bail!("line {line_number}: unsupported TRACK mode {mode}");
                    }
                    if files.is_empty() {
                        bail!("line {line_number}: TRACK before any FILE");
                    }
                    tracks.push(CueTrack { number, mode });
                }
                _ => {}
            }
        }
        if tracks.is_empty() {
            bail!("cue sheet has no tracks");
        }
        Ok(Self { files, tracks })
    }

    fn read(path: &Path) -> Result<Self> {
        let text = read_limited(path, MAX_CUE_SHEET_BYTES)?;
        Self::parse(path.parent().unwrap_or(Path::new("")), &text)
    }
}

fn read_limited(path: &Path, limit: u64) -> Result<String> {
    let mut text = String::new();
    File::open(path)?
        .take(limit + 1)
        .read_to_string(&mut text)?;
    if text.len() as u64 > limit {
        bail!("larger than {limit} bytes");
    }
    Ok(text)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiskImage {
    pub path: PathBuf,
    pub kind: ImageKind,
    pub size: u64,
    /// Last modification time, if the filesystem reports one.
    pub modified: Option<SystemTime>,
    /// Creation time, if the filesystem reports one (some Linux mounts do not).
    pub created: Option<SystemTime>,
    /// Present for `.cue` CD-ROM images.
    pub cue: Option<CueSheet>,
}

impl DiskImage {
    /// Reads file metadata and, for `.cue` sheets, the sheet itself; never writes.
    pub fn inspect(path: &Path) -> Result<Self> {
        let metadata =
            fs::metadata(path).with_context(|| format!("disk image {}", path.display()))?;
        if !metadata.is_file() {
            bail!("disk image {} is not a file", path.display());
        }
        let kind = ImageKind::classify(path, metadata.len())
            .with_context(|| format!("{} is not a supported disk image type", path.display()))?;
        let is_cue = path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("cue"));
        let cue = if is_cue {
            Some(CueSheet::read(path).with_context(|| format!("cue sheet {}", path.display()))?)
        } else {
            None
        };
        Ok(Self {
            path: path.to_path_buf(),
            kind,
            size: metadata.len(),
            modified: metadata.modified().ok(),
            created: metadata.created().ok(),
            cue,
        })
    }

    /// Bytes of image data: the file itself plus, for `.cue` sheets, every track file.
    pub fn data_size(&self) -> u64 {
        self.size
            + self
                .cue
                .as_ref()
                .map_or(0, |cue| cue.files.iter().map(|file| file.size).sum())
    }

    /// Creation time, falling back to the modification time where creation is not recorded.
    pub fn created_or_modified(&self) -> Option<SystemTime> {
        self.created.or(self.modified)
    }
}

/// Which images to mount when hail.cfg does not name one.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Select {
    None,
    First,
    All,
    LastModified,
    OldestCreated,
    Largest,
    Smallest,
}

impl Select {
    pub const ALL: [Select; 7] = [
        Select::None,
        Select::First,
        Select::All,
        Select::LastModified,
        Select::OldestCreated,
        Select::Largest,
        Select::Smallest,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Select::None => "none",
            Select::First => "first",
            Select::All => "all",
            Select::LastModified => "last-modified",
            Select::OldestCreated => "oldest-created",
            Select::Largest => "largest",
            Select::Smallest => "smallest",
        }
    }

    /// Picks the images to mount from `images` (sorted by name). Ties go to the first
    /// image by name; images without the needed timestamp are chosen last.
    pub fn pick(self, images: &[DiskImage]) -> Vec<DiskImage> {
        use std::cmp::Reverse;
        let chosen = match self {
            Select::None => None,
            Select::All => return images.to_vec(),
            Select::First => images.first(),
            Select::LastModified => images.iter().min_by_key(|image| Reverse(image.modified)),
            Select::OldestCreated => images.iter().min_by_key(|image| {
                let time = image.created_or_modified();
                (time.is_none(), time)
            }),
            Select::Largest => images.iter().min_by_key(|image| Reverse(image.data_size())),
            Select::Smallest => images.iter().min_by_key(|image| image.data_size()),
        };
        chosen.cloned().into_iter().collect()
    }
}

/// How the emulator should attach the images mounted from a disk (`hail.cfg` `mode`).
/// Flags: `readonly` (guest sees write-protected media), `writeback` (guest writes are
/// saved to the image file), `nowriteback` (guest writes are kept out of the image file).
/// Without flags the emulator's defaults apply.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MountMode {
    pub readonly: bool,
    /// `Some(true)` = writeback, `Some(false)` = nowriteback, `None` = emulator default.
    pub writeback: Option<bool>,
}

impl MountMode {
    pub const FLAGS: [&'static str; 3] = ["readonly", "writeback", "nowriteback"];

    /// Parses comma-separated flags, e.g. `"readonly,nowriteback"`. Spaces around
    /// flags are ignored; an empty string means no flags.
    pub fn parse(text: &str) -> Result<Self> {
        let mut mode = Self::default();
        let mut seen = Vec::new();
        if text.trim().is_empty() {
            return Ok(mode);
        }
        for flag in text.split(',').map(str::trim) {
            if flag.is_empty() {
                bail!("mode {text:?} has an empty flag");
            }
            if seen.contains(&flag) {
                bail!("mode flag {flag:?} is repeated");
            }
            seen.push(flag);
            match flag {
                "readonly" => mode.readonly = true,
                "writeback" | "nowriteback" => {
                    if mode.writeback.is_some() {
                        bail!("mode cannot combine writeback and nowriteback");
                    }
                    mode.writeback = Some(flag == "writeback");
                }
                _ => bail!(
                    "unknown mode flag {flag:?}; supported: {}",
                    Self::FLAGS.join(", ")
                ),
            }
        }
        if mode.readonly && mode.writeback == Some(true) {
            bail!("mode cannot combine readonly and writeback");
        }
        Ok(mode)
    }

    /// Canonical flag list, or `default` when no flags are set.
    pub fn describe(&self) -> String {
        let mut flags = Vec::new();
        if self.readonly {
            flags.push("readonly");
        }
        match self.writeback {
            Some(true) => flags.push("writeback"),
            Some(false) => flags.push("nowriteback"),
            None => {}
        }
        if flags.is_empty() {
            "default".to_string()
        } else {
            flags.join(",")
        }
    }
}

/// Where a hail.cfg custom emulator (`emulator.path`) lives.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CustomPath {
    /// A full path on the host.
    Host(PathBuf),
    /// Relative to the root of the disk the hail.cfg is on.
    Disk(PathBuf),
}

impl CustomPath {
    /// The executable for a disk mounted at `root`.
    pub fn resolve(&self, root: &Path) -> PathBuf {
        match self {
            CustomPath::Host(path) => path.clone(),
            CustomPath::Disk(path) => root.join(path),
        }
    }

    pub fn describe(&self) -> String {
        match self {
            CustomPath::Host(path) => format!("host:{}", path.display()),
            CustomPath::Disk(path) => format!("disk:{}", path.display()),
        }
    }
}

/// A hail.cfg `emulator.custom = true` emulator, not taken from `[emulators]`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CustomEmulator {
    /// One path, or `emulator.path.<os>.<arch>` entries (a disk carrying builds for
    /// several hosts).
    pub path: crate::platform::PerPlatform<CustomPath>,
    /// `emulator.args` templates (`["{image}"]` when unset).
    pub args: Vec<Template>,
}

/// Parses hail.cfg `emulator`: a short name, or a table
/// `{ custom, name, path, args }`. Returns the name and, for `custom = true`, the
/// emulator the disk defines.
fn parse_disk_emulator(value: &toml::Value) -> Result<(String, Option<CustomEmulator>)> {
    let check_name = |name: &str| {
        if !crate::emulator::is_short_name(name) {
            bail!("emulator {name:?} is not a valid [emulators] short name");
        }
        Ok(name.to_string())
    };
    let table = match value {
        toml::Value::String(name) => return Ok((check_name(name)?, None)),
        toml::Value::Table(table) => table,
        _ => bail!("emulator must be a short name or a table (emulator.name = \"...\")"),
    };
    if let Some(key) = table
        .keys()
        .find(|key| !["custom", "name", "path", "args"].contains(&key.as_str()))
    {
        bail!("emulator: unknown key {key:?} (expected custom, name, path, args)");
    }
    let custom = match table.get("custom") {
        None => false,
        Some(value) => value
            .as_bool()
            .context("emulator.custom must be true or false")?,
    };
    let name = table
        .get("name")
        .context("emulator.name is required")?
        .as_str()
        .context("emulator.name must be a string")?;
    let name = check_name(name)?;
    if !custom {
        if let Some(key) = ["path", "args"]
            .iter()
            .find(|key| table.contains_key(**key))
        {
            bail!("emulator.{key} needs emulator.custom = true");
        }
        return Ok((name, None));
    }
    let path = table
        .get("path")
        .context("emulator.custom = true needs emulator.path")?;
    let path = crate::platform::PerPlatform::parse("emulator.path", path, custom_path)?;
    let args = crate::emulator::parse_args("emulator", table.get("args"))?;
    Ok((name, Some(CustomEmulator { path, args })))
}

/// One hail.cfg `emulator.path`. `os` is the platform the entry is for (`None`: every
/// host): a Windows path in a `windows` entry is a host path even on Linux.
fn custom_path(label: &str, os: Option<&str>, path: &str) -> Result<CustomPath> {
    let path = path.trim();
    let absolute = match os {
        Some(os) => crate::platform::is_absolute_on(os, path),
        None => Path::new(path).is_absolute(),
    };
    if path.is_empty() {
        bail!("emulator.custom = true needs {label} (a full host path or a path on the disk)")
    } else if absolute {
        Ok(CustomPath::Host(PathBuf::from(path)))
    } else if path.starts_with(['/', '\\']) {
        bail!("{label} {path:?} must be a full host path or relative to the disk root")
    } else {
        Ok(CustomPath::Disk(relative_disk_path(label, path)?))
    }
}

/// Parsed `hail.cfg`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiskConfig {
    pub path: PathBuf,
    /// Original file contents, kept for diagnostics.
    pub text: String,
    pub automount: bool,
    /// `[emulators]` short name the image is meant for; `None` means the `[emulator]` handler.
    /// For a custom emulator, the name it is logged under.
    pub emulator: Option<String>,
    /// Set by `emulator.custom = true`: the disk defines the emulator itself.
    pub custom: Option<CustomEmulator>,
    /// Image relative to the volume root; when set, only this image is mounted.
    pub image: Option<PathBuf>,
    pub select: Select,
    /// How every image mounted from this disk is attached.
    pub mode: MountMode,
    /// File relative to the volume root where the emulator persists guest writes
    /// (e.g. an SNES `.srm`); only with `mode` writeback. It need not exist yet.
    pub savefile: Option<PathBuf>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDiskConfig {
    #[serde(default)]
    automount: bool,
    emulator: Option<toml::Value>,
    image: Option<String>,
    select: Option<Select>,
    mode: Option<String>,
    savefile: Option<String>,
}

/// Validates a hail.cfg path (`key` = image or savefile) relative to the volume root.
fn relative_disk_path(key: &str, text: &str) -> Result<PathBuf> {
    let mut path = PathBuf::new();
    for part in text.split(['/', '\\']) {
        match part {
            "" | "." => {}
            ".." => bail!("{key} {text:?} must stay on the disk (no \"..\")"),
            part if part.contains(':') => bail!("{key} {text:?} must be relative to the disk root"),
            part => path.push(part),
        }
    }
    if path.as_os_str().is_empty() {
        bail!("{key} must name a file");
    }
    Ok(path)
}

impl DiskConfig {
    pub fn parse(path: PathBuf, text: &str) -> Result<Self> {
        let raw: RawDiskConfig = toml::from_str(text).context("parsing TOML")?;
        let image = raw
            .image
            .as_deref()
            .map(|text| relative_disk_path("image", text))
            .transpose()?;
        let select = match (&image, raw.select) {
            (Some(_), Some(_)) => bail!("set either image or select, not both"),
            (_, select) => select.unwrap_or(Select::First),
        };
        let mode = MountMode::parse(raw.mode.as_deref().unwrap_or(""))?;
        let savefile = raw
            .savefile
            .as_deref()
            .map(|text| relative_disk_path("savefile", text))
            .transpose()?;
        if savefile.is_some() {
            if mode.writeback != Some(true) {
                bail!("savefile requires mode = \"writeback\"");
            }
            if image.is_none() && select == Select::All {
                bail!(
                    "savefile cannot be shared by select = \"all\"; name one image or select one"
                );
            }
            let same = |a: &Path, b: &Path| {
                a.to_string_lossy()
                    .eq_ignore_ascii_case(&b.to_string_lossy())
            };
            if let (Some(image), Some(savefile)) = (&image, &savefile) {
                if same(image, savefile) {
                    bail!("savefile must differ from image");
                }
            }
        }
        let (emulator, custom) = match &raw.emulator {
            None => (None, None),
            Some(value) => {
                let (name, custom) = parse_disk_emulator(value)?;
                if image.is_none() {
                    if custom.is_some() {
                        bail!("custom emulator {name:?} needs image = \"...\"");
                    }
                    if crate::emulator::Kind::from_short_name(&name).is_none() {
                        bail!(
                            "emulator {name:?} needs image = \"...\" (select only finds Snow images)"
                        );
                    }
                }
                (Some(name), custom)
            }
        };
        Ok(Self {
            path,
            text: text.to_string(),
            automount: raw.automount,
            emulator,
            custom,
            image,
            select,
            mode,
            savefile,
        })
    }

    fn read(path: &Path) -> Result<Self> {
        let text = read_limited(path, MAX_DISK_CONFIG_BYTES)?;
        Self::parse(path.to_path_buf(), &text)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DiskConfigStatus {
    Absent,
    Found(DiskConfig),
    Invalid { path: PathBuf, error: String },
}

impl DiskConfigStatus {
    pub fn name(&self) -> &'static str {
        match self {
            DiskConfigStatus::Absent => "absent",
            DiskConfigStatus::Found(_) => "found",
            DiskConfigStatus::Invalid { .. } => "invalid",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiskScan {
    /// Mountable images in the volume root, sorted case-insensitively by name.
    pub images: Vec<DiskImage>,
    /// Image-like files that cannot be mounted (e.g. a cue sheet with missing tracks).
    pub rejected: Vec<(PathBuf, String)>,
    pub config: DiskConfigStatus,
    /// Set when the volume root could not be listed.
    pub error: Option<String>,
}

/// Lists the volume root without writing anything to the volume.
pub fn scan_volume(root: &Path) -> DiskScan {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) => {
            return DiskScan {
                images: Vec::new(),
                rejected: Vec::new(),
                config: DiskConfigStatus::Absent,
                error: Some(format!("listing {}: {error}", root.display())),
            }
        }
    };
    let mut images = Vec::new();
    let mut rejected = Vec::new();
    let mut config_path = None;
    for entry in entries.flatten() {
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            // Hidden files, including macOS "._name" AppleDouble companions.
            continue;
        }
        if name.eq_ignore_ascii_case(DISK_CONFIG_NAME) {
            config_path = Some(path);
        } else if ImageKind::classify(&path, metadata.len()).is_some() {
            match DiskImage::inspect(&path) {
                Ok(image) => images.push(image),
                Err(error) => rejected.push((path, format!("{error:#}"))),
            }
        }
    }
    images.sort_by_cached_key(|image| {
        image
            .path
            .file_name()
            .map(|name| name.to_string_lossy().to_lowercase())
    });
    let config = match config_path {
        None => DiskConfigStatus::Absent,
        Some(path) => match DiskConfig::read(&path) {
            Ok(config) => DiskConfigStatus::Found(config),
            Err(error) => DiskConfigStatus::Invalid {
                path,
                error: format!("{error:#}"),
            },
        },
    };
    rejected.sort();
    DiskScan {
        images,
        rejected,
        config,
        error: None,
    }
}

/// An inserted, currently available disk held in the running service's memory.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TrackedDisk {
    pub disk: Disk,
    pub inserted_at: String,
    pub scan: DiskScan,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MountPlan {
    /// Images for the `[emulator]` handler.
    Mount(Vec<DiskImage>),
    /// One file to open with the `[emulators]` entry named by hail.cfg `emulator`, or
    /// with the custom emulator the hail.cfg defines.
    External {
        emulator: String,
        image: PathBuf,
        /// Absolute hail.cfg `savefile`, when set.
        savefile: Option<PathBuf>,
        /// For `emulator.custom = true`: the emulator with its executable resolved
        /// (still subject to `[service] custom_emulators`).
        custom: Option<crate::emulator::EmulatorEntry>,
    },
    /// hail.cfg defines a custom emulator with no `emulator.path` for this host.
    NoHostEmulator {
        emulator: String,
        reason: String,
    },
    Skip(String),
}

impl TrackedDisk {
    /// Decides what hail.cfg asks to be mounted.
    pub fn mount_plan(&self) -> MountPlan {
        let skip = |reason: &str| MountPlan::Skip(reason.to_string());
        let config = match &self.scan.config {
            DiskConfigStatus::Absent => return skip("no hail.cfg on disk"),
            DiskConfigStatus::Invalid { error, .. } => {
                return MountPlan::Skip(format!("invalid hail.cfg: {error}"))
            }
            DiskConfigStatus::Found(config) => config,
        };
        if !config.automount {
            return skip("hail.cfg automount = false");
        }
        if let (Some(emulator), Some(image)) = (&config.emulator, &config.image) {
            if config.custom.is_some() || crate::emulator::Kind::from_short_name(emulator).is_none()
            {
                let custom = match config.custom.as_ref().map(|custom| custom.path.for_host()) {
                    Some(Err(reason)) => {
                        return MountPlan::NoHostEmulator {
                            emulator: emulator.clone(),
                            reason: format!("emulator.path: {reason}"),
                        }
                    }
                    Some(Ok(path)) => Some(path),
                    None => None,
                };
                let path = self.disk.mount_point.join(image);
                return match fs::metadata(&path) {
                    Ok(metadata) if metadata.is_file() => {
                        MountPlan::External {
                            emulator: emulator.clone(),
                            image: path,
                            savefile: self.savefile(),
                            custom: config.custom.as_ref().zip(custom).map(
                                |(custom, executable)| crate::emulator::EmulatorEntry {
                                    name: emulator.clone(),
                                    path: Some(executable.resolve(&self.disk.mount_point)),
                                    args: custom.args.clone(),
                                },
                            ),
                        }
                    }
                    Ok(_) => {
                        MountPlan::Skip(format!("hail.cfg image {} is not a file", path.display()))
                    }
                    Err(error) => {
                        MountPlan::Skip(format!("hail.cfg image {}: {error}", path.display()))
                    }
                };
            }
        }
        if let Some(image) = &config.image {
            return match DiskImage::inspect(&self.disk.mount_point.join(image)) {
                Ok(image) => MountPlan::Mount(vec![image]),
                Err(error) => MountPlan::Skip(format!("hail.cfg image: {error:#}")),
            };
        }
        match config.select {
            Select::None => skip("hail.cfg select = \"none\""),
            _ if self.scan.images.is_empty() => skip("no disk images in the volume root"),
            select => MountPlan::Mount(select.pick(&self.scan.images)),
        }
    }

    /// The disk's parsed hail.cfg, if it has a valid one.
    pub fn disk_config(&self) -> Option<&DiskConfig> {
        match &self.scan.config {
            DiskConfigStatus::Found(config) => Some(config),
            _ => None,
        }
    }

    /// Absolute path of the hail.cfg `savefile`, if one is set.
    pub fn savefile(&self) -> Option<PathBuf> {
        self.disk_config()
            .and_then(|config| config.savefile.as_ref())
            .map(|savefile| self.disk.mount_point.join(savefile))
    }

    /// Mount flags from hail.cfg (emulator defaults without a valid hail.cfg).
    pub fn mount_mode(&self) -> MountMode {
        self.disk_config()
            .map_or_else(MountMode::default, |config| config.mode)
    }
}
