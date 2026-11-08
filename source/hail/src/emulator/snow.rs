//! Snow (https://github.com/twvd/snow) handler.
use super::{describe_mount, Emulator, Kind};
use crate::event::Event;
use crate::log::EventLog;
use crate::media::{DiskImage, ImageKind, MountMode, TrackedDisk};
use anyhow::Result;
use serde_json::{json, Value};
use std::fmt;
use std::path::Path;

/// Not yet connected to a Snow instance. While this is true every call is logged;
/// afterwards the same entries are written by debug builds only.
pub const IS_STUB: bool = true;

const STUB_NOTE: &str = " (stub: not yet connected to Snow)";

/// One `snow_core::emulator::comm::EmulatorCommand` HAIL sends to Snow: the variant
/// name and its parameters by name (`"next-free"` where Snow picks the drive or SCSI ID,
/// `"same"` for the one the previous command used).
#[derive(Clone, Debug, PartialEq)]
pub struct SnowRpc {
    pub method: &'static str,
    pub params: Value,
}

impl SnowRpc {
    pub fn new(method: &'static str, params: Value) -> Self {
        Self { method, params }
    }
}

impl fmt::Display for SnowRpc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.method, self.params)
    }
}

/// The Snow commands (`snow_core::emulator::comm::EmulatorCommand`) a mount resolves to.
#[derive(Clone, Debug, PartialEq)]
pub struct SnowMount {
    pub commands: Vec<String>,
    /// The same commands as the calls HAIL sends (one per entry of `commands`).
    pub rpc: Vec<SnowRpc>,
    /// How the mode was applied, including flags Snow cannot honour for this media.
    pub notes: Vec<String>,
}

impl SnowMount {
    /// Resolves `mode` (and hail.cfg `savefile`) for `image`. Snow inserts floppies with
    /// writeback off; hard disks are written in place unless branched to a copy; CD-ROM
    /// media is always read-only.
    pub fn plan(image: &DiskImage, mode: MountMode, savefile: Option<&Path>) -> Self {
        let path = image.path.display();
        let mut notes = Vec::new();
        let commands = match image.kind {
            ImageKind::Floppy => {
                let mut commands = vec![format!(
                    "InsertFloppy(drive=next free, \"{path}\", write_protect={})",
                    mode.readonly
                )];
                match (mode.readonly, mode.writeback) {
                    (_, Some(true)) => {
                        commands.push("SetFloppyWriteback(drive, true)".to_string());
                        notes.push(
                            "guest writes are saved to the image file (if its format supports writeback)"
                                .to_string(),
                        );
                        if let Some(savefile) = savefile {
                            notes.push(format!(
                                "savefile {} not used: Snow floppy writeback saves into the image file",
                                savefile.display()
                            ));
                        }
                    }
                    (true, _) => notes.push("guest sees a write-protected floppy".to_string()),
                    (false, _) => notes.push(
                        "writeback off (Snow default): guest writes stay in memory".to_string(),
                    ),
                }
                commands
            }
            ImageKind::HardDisk => match savefile.filter(|_| mode.writeback == Some(true)) {
                Some(savefile) if savefile.is_file() => {
                    notes.push(format!(
                        "resuming from savefile {}: guest writes continue there; the image is not modified",
                        savefile.display()
                    ));
                    vec![format!(
                        "ScsiAttachHdd(id=next free, \"{}\")",
                        savefile.display()
                    )]
                }
                Some(savefile) => {
                    notes.push(format!(
                        "first use: the image is copied to savefile {} and guest writes go there; the image is not modified",
                        savefile.display()
                    ));
                    vec![
                        format!("ScsiAttachHdd(id=next free, \"{path}\")"),
                        format!("ScsiBranchHdd(id, \"{}\")", savefile.display()),
                    ]
                }
                None => Self::hard_disk(&path.to_string(), mode, &mut notes),
            },
            ImageKind::CdRom => {
                if mode.writeback == Some(true) {
                    notes.push("writeback ignored: CD-ROM media is read-only".to_string());
                }
                if let Some(savefile) = savefile {
                    notes.push(format!(
                        "savefile {} ignored: CD-ROM media is read-only",
                        savefile.display()
                    ));
                }
                notes.push("CD-ROM media is always read-only".to_string());
                vec![
                    "ScsiAttachCdrom(id=next free)".to_string(),
                    format!("ScsiLoadMedia(id, \"{path}\")"),
                ]
            }
        };
        let rpc = Self::rpc_calls(image, mode, savefile);
        Self {
            commands,
            notes,
            rpc,
        }
    }

    fn rpc_calls(image: &DiskImage, mode: MountMode, savefile: Option<&Path>) -> Vec<SnowRpc> {
        let path = image.path.display().to_string();
        let attach_hdd =
            |path: &str| SnowRpc::new("ScsiAttachHdd", json!({"id": "next-free", "path": path}));
        let branch_hdd =
            |path: &str| SnowRpc::new("ScsiBranchHdd", json!({"id": "same", "path": path}));
        match image.kind {
            ImageKind::Floppy => {
                let mut calls = vec![SnowRpc::new(
                    "InsertFloppy",
                    json!({"drive": "next-free", "path": path, "write_protect": mode.readonly}),
                )];
                if mode.writeback == Some(true) {
                    calls.push(SnowRpc::new(
                        "SetFloppyWriteback",
                        json!({"drive": "same", "enabled": true}),
                    ));
                }
                calls
            }
            ImageKind::HardDisk => match savefile.filter(|_| mode.writeback == Some(true)) {
                Some(savefile) if savefile.is_file() => {
                    vec![attach_hdd(&savefile.display().to_string())]
                }
                Some(savefile) => vec![
                    attach_hdd(&path),
                    branch_hdd(&savefile.display().to_string()),
                ],
                None if mode.readonly || mode.writeback == Some(false) => {
                    vec![attach_hdd(&path), branch_hdd("<host temporary copy>")]
                }
                None => vec![attach_hdd(&path)],
            },
            ImageKind::CdRom => vec![
                SnowRpc::new("ScsiAttachCdrom", json!({"id": "next-free"})),
                SnowRpc::new("ScsiLoadMedia", json!({"id": "same", "path": path})),
            ],
        }
    }

    fn hard_disk(path: &str, mode: MountMode, notes: &mut Vec<String>) -> Vec<String> {
        let mut commands = vec![format!("ScsiAttachHdd(id=next free, \"{path}\")")];
        if mode.readonly || mode.writeback == Some(false) {
            commands.push("ScsiBranchHdd(id, <host temporary copy>)".to_string());
            if mode.readonly {
                notes.push(
                    "Snow has no write-protected hard disks: readonly is applied as nowriteback"
                        .to_string(),
                );
            }
            notes.push(
                "guest writes go to a host-side copy; the image on the disk is not modified"
                    .to_string(),
            );
        } else {
            notes.push("guest writes are made in place to the image file".to_string());
        }
        commands
    }
}

#[derive(Debug, Default)]
pub struct Snow {
    running: bool,
}

impl Snow {
    fn note(&self) -> &'static str {
        if IS_STUB {
            STUB_NOTE
        } else {
            ""
        }
    }

    fn header(&self, call: &str, trigger: &Event) -> String {
        format!(
            "emulator.{call}{}\n  emulator: snow running={}\n  trigger: {}",
            self.note(),
            self.running,
            trigger.kind.name()
        )
    }

    /// The log line for a call to Snow.
    fn rpc_line(call: &SnowRpc) -> String {
        let sent = if IS_STUB { " (not sent: stub)" } else { "" };
        format!("\n  rpc: {call}{sent}")
    }
}

impl Emulator for Snow {
    fn kind(&self) -> Kind {
        Kind::Snow
    }

    fn is_running(&self) -> bool {
        self.running
    }

    fn debug_logging(&self) -> bool {
        IS_STUB || cfg!(debug_assertions)
    }

    fn start(&mut self, log: &mut EventLog, trigger: &Event) -> Result<()> {
        let message = if self.running {
            format!(
                "{}\n  result: already running, ignored",
                self.header("start", trigger)
            )
        } else {
            let message = format!(
                "{}{}\n  result: started",
                self.header("start", trigger),
                Self::rpc_line(&SnowRpc::new("Run", json!({})))
            );
            self.running = true;
            message
        };
        if self.debug_logging() {
            log.debug(&message)?;
        }
        Ok(())
    }

    fn stop(&mut self, log: &mut EventLog, trigger: &Event) -> Result<()> {
        let message = if self.running {
            let message = format!(
                "{}{}\n  result: stopped",
                self.header("stop", trigger),
                Self::rpc_line(&SnowRpc::new("Quit", json!({})))
            );
            self.running = false;
            message
        } else {
            format!(
                "{}\n  result: not running, ignored",
                self.header("stop", trigger)
            )
        };
        if self.debug_logging() {
            log.debug(&message)?;
        }
        Ok(())
    }

    fn mount(
        &mut self,
        log: &mut EventLog,
        disk: &TrackedDisk,
        image: &DiskImage,
        mode: MountMode,
        savefile: Option<&Path>,
    ) -> Result<()> {
        if !self.debug_logging() {
            return Ok(());
        }
        let plan = SnowMount::plan(image, mode, savefile);
        let lines = |label: &str, items: &[String]| -> String {
            items
                .iter()
                .map(|item| format!("\n  {label}: {item}"))
                .collect()
        };
        let rpc: String = plan.rpc.iter().map(Self::rpc_line).collect();
        log.debug(&format!(
            "emulator.mount{}\n  emulator: snow running={}\n{}{}{}{}",
            self.note(),
            self.running,
            describe_mount(disk, image, mode),
            lines("snow", &plan.commands),
            rpc,
            lines("note", &plan.notes),
        ))
    }
}
