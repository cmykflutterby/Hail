pub mod config;
pub mod discovery;
pub mod emulator;
pub mod event;
pub mod log;
pub mod media;
pub mod platform;
pub mod process;
pub mod remote;
pub mod service;
pub mod template;
pub mod watch;
#[cfg(windows)]
pub mod winservice;

pub use config::{
    Action, Config, LaunchAction, LaunchTarget, Mode, OnError, Polling, PollingType, ProcessAction,
};
pub use discovery::{changes, discover_usb_volumes, Disk, DiskChange};
#[cfg(windows)]
pub use emulator::close_windows;
pub use emulator::{
    display_command, environment_block, is_short_name, launch_external, merge_environment,
    parse_args, parse_environment_block, quote_windows_arg, render_args, spawn,
    windows_command_line, CloseRequest, CustomEmulators, Emulator, EmulatorEntry, EmulatorSession,
    Emulators, Kind as EmulatorKind, LaunchError, LaunchRequest, LogindSession, Process, Snow,
    SnowMount, Started, ARG_FIELDS,
};
pub use event::{days_from_civil, unix_seconds, utc_timestamp, CivilTime, Event, EventKind};
pub use log::{EventLog, LogClock, LogName, LOG_NAME_TOKENS};
pub use media::{
    scan_volume, CueSheet, CustomEmulator, CustomPath, DiskConfig, DiskConfigStatus, DiskImage,
    DiskScan, ImageKind, MountMode, MountPlan, Select, TrackedDisk,
};
pub use platform::PerPlatform;
pub use process::{ProcessKind, ProcessTable, TrackedProcess};
pub use remote::{
    percent_encode, rpc_request, rpc_response, Api, ApiAction, ApiBody, JsonTemplate, Rpc,
    RpcAction, RpcTransport,
};
pub use service::{
    ConfigReload, Exit, Service, CONFIG_CHECK_INTERVAL, EXIT_FAILURE, EXIT_QUIT, EXIT_RESTART,
};
pub use template::Template;
pub use watch::{ChangeSource, OsEventSource, RescanSchedule};

pub const DEFAULT_CONFIG_NAME: &str = "hail.toml";

#[cfg(not(any(target_os = "windows", target_os = "linux")))]
compile_error!("hail supports Windows and Linux only");
