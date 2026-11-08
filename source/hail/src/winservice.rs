//! Native Windows service support: Service Control Manager (SCM) dispatch, status
//! reporting, and installation with recovery actions.
//!
//! The SCM restarts the service when it stops with a non-zero exit code (reported as a
//! service-specific error), so `hail.restart` (75) and errors (1) restart it, while
//! `hail.quit` (0) and a stop request leave it stopped.
use crate::service::EXIT_FAILURE;
use anyhow::{bail, Context, Result};
use std::ffi::c_void;
use std::path::Path;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicPtr, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use windows_sys::core::PWSTR;
use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_CALL_NOT_IMPLEMENTED, ERROR_FAILED_SERVICE_CONTROLLER_CONNECT,
    ERROR_SERVICE_DOES_NOT_EXIST, ERROR_SERVICE_EXISTS, ERROR_SERVICE_NOT_ACTIVE,
    ERROR_SERVICE_SPECIFIC_ERROR, NO_ERROR,
};
use windows_sys::Win32::Storage::FileSystem::DELETE;
use windows_sys::Win32::System::Services::{
    ChangeServiceConfig2W, CloseServiceHandle, ControlService, CreateServiceW, DeleteService,
    OpenSCManagerW, OpenServiceW, QueryServiceStatus, RegisterServiceCtrlHandlerExW,
    SetServiceStatus, StartServiceCtrlDispatcherW, StartServiceW, SC_ACTION, SC_ACTION_RESTART,
    SC_HANDLE, SC_MANAGER_CONNECT, SC_MANAGER_CREATE_SERVICE, SERVICE_ACCEPT_SHUTDOWN,
    SERVICE_ACCEPT_STOP, SERVICE_ALL_ACCESS, SERVICE_AUTO_START, SERVICE_CONFIG_DESCRIPTION,
    SERVICE_CONFIG_FAILURE_ACTIONS, SERVICE_CONFIG_FAILURE_ACTIONS_FLAG,
    SERVICE_CONTROL_INTERROGATE, SERVICE_CONTROL_SHUTDOWN, SERVICE_CONTROL_STOP,
    SERVICE_DESCRIPTIONW, SERVICE_ERROR_NORMAL, SERVICE_FAILURE_ACTIONSW,
    SERVICE_FAILURE_ACTIONS_FLAG, SERVICE_QUERY_STATUS, SERVICE_RUNNING, SERVICE_START_PENDING,
    SERVICE_STATUS, SERVICE_STATUS_HANDLE, SERVICE_STOP, SERVICE_STOPPED, SERVICE_STOP_PENDING,
    SERVICE_TABLE_ENTRYW, SERVICE_WIN32_OWN_PROCESS,
};

/// Name the service is registered under (`sc.exe query HAIL`).
pub const SERVICE_NAME: &str = "HAIL";
pub const DISPLAY_NAME: &str = "HAIL - Hardware Abstraction\\Integration Layer Service";
const DESCRIPTION: &str =
    "Watches OS Events and runs HAIL configurable actions for supported emulator instances.";
/// Restart delays after the 1st, 2nd and later failures; the count resets after a day.
const RESTART_DELAYS_MS: [u32; 3] = [2_000, 2_000, 10_000];
const FAILURE_RESET_SECONDS: u32 = 86_400;

type Body = Box<dyn FnOnce(Arc<AtomicBool>) -> i32 + Send>;

static BODY: Mutex<Option<Body>> = Mutex::new(None);
static STOP: OnceLock<Arc<AtomicBool>> = OnceLock::new();
static STATUS_HANDLE: AtomicPtr<c_void> = AtomicPtr::new(null_mut());
static CHECKPOINT: AtomicU32 = AtomicU32::new(0);
static EXIT_CODE: AtomicI32 = AtomicI32::new(EXIT_FAILURE);

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

fn last_error(what: &str) -> anyhow::Error {
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) {
        anyhow::anyhow!(
            "{what}: access denied; run this from an elevated prompt (Run as administrator)"
        )
    } else {
        anyhow::Error::new(error).context(what.to_string())
    }
}

/// Closes an SCM handle when dropped.
struct ScHandle(SC_HANDLE);

impl Drop for ScHandle {
    fn drop(&mut self) {
        // SAFETY: the handle came from OpenSCManagerW/OpenServiceW/CreateServiceW.
        unsafe { CloseServiceHandle(self.0) };
    }
}

fn open_manager(access: u32) -> Result<ScHandle> {
    // SAFETY: null machine/database names select the local active database.
    let handle = unsafe { OpenSCManagerW(null(), null(), access) };
    if handle.is_null() {
        return Err(last_error("opening the Windows service manager"));
    }
    Ok(ScHandle(handle))
}

fn report(state: u32, exit_code: i32, wait_hint_ms: u32) {
    let handle: SERVICE_STATUS_HANDLE = STATUS_HANDLE.load(Ordering::SeqCst);
    if handle.is_null() {
        return;
    }
    let pending = state == SERVICE_START_PENDING || state == SERVICE_STOP_PENDING;
    let status = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: state,
        dwControlsAccepted: if state == SERVICE_RUNNING {
            SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN
        } else {
            0
        },
        dwWin32ExitCode: if exit_code == 0 {
            NO_ERROR
        } else {
            ERROR_SERVICE_SPECIFIC_ERROR
        },
        dwServiceSpecificExitCode: exit_code as u32,
        dwCheckPoint: if pending {
            CHECKPOINT.fetch_add(1, Ordering::SeqCst) + 1
        } else {
            0
        },
        dwWaitHint: wait_hint_ms,
    };
    // SAFETY: `handle` came from RegisterServiceCtrlHandlerExW; `status` is a valid local.
    unsafe { SetServiceStatus(handle, &status) };
}

unsafe extern "system" fn control_handler(
    control: u32,
    _event_type: u32,
    _event_data: *mut c_void,
    _context: *mut c_void,
) -> u32 {
    match control {
        SERVICE_CONTROL_STOP | SERVICE_CONTROL_SHUTDOWN => {
            if let Some(stop) = STOP.get() {
                stop.store(true, Ordering::SeqCst);
            }
            // Room for stopping the programs hail started (stop_timeout_ms, then kill).
            report(SERVICE_STOP_PENDING, 0, 30_000);
            NO_ERROR
        }
        SERVICE_CONTROL_INTERROGATE => NO_ERROR,
        _ => ERROR_CALL_NOT_IMPLEMENTED,
    }
}

unsafe extern "system" fn service_main(_argc: u32, _argv: *mut PWSTR) {
    let name = wide(SERVICE_NAME);
    // SAFETY: `name` is NUL-terminated and outlives the call; the handler is 'static.
    let handle =
        unsafe { RegisterServiceCtrlHandlerExW(name.as_ptr(), Some(control_handler), null()) };
    if handle.is_null() {
        EXIT_CODE.store(EXIT_FAILURE, Ordering::SeqCst);
        return;
    }
    STATUS_HANDLE.store(handle, Ordering::SeqCst);
    report(SERVICE_START_PENDING, 0, 5_000);
    let stop = STOP.get_or_init(Default::default).clone();
    let body = BODY.lock().ok().and_then(|mut body| body.take());
    report(SERVICE_RUNNING, 0, 0);
    // A panic must not unwind across this extern "system" boundary.
    let code = body.map_or(EXIT_FAILURE, |body| {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| body(stop)))
            .unwrap_or(EXIT_FAILURE)
    });
    EXIT_CODE.store(code, Ordering::SeqCst);
    report(SERVICE_STOPPED, code, 0);
}

/// Runs `body` as the HAIL Windows service. Blocks until the service has stopped and
/// returns `body`'s exit code. `body` receives the stop flag set by Stop/Shutdown requests.
/// Fails when the process was not started by the service manager.
pub fn run(body: impl FnOnce(Arc<AtomicBool>) -> i32 + Send + 'static) -> Result<i32> {
    STOP.get_or_init(Default::default);
    *BODY
        .lock()
        .map_err(|_| anyhow::anyhow!("service state poisoned"))? = Some(Box::new(body));
    let mut name = wide(SERVICE_NAME);
    let table = [
        SERVICE_TABLE_ENTRYW {
            lpServiceName: name.as_mut_ptr(),
            lpServiceProc: Some(service_main),
        },
        SERVICE_TABLE_ENTRYW {
            lpServiceName: null_mut(),
            lpServiceProc: None,
        },
    ];
    // SAFETY: `table` is terminated by a null entry and outlives the blocking call.
    if unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) } == 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(ERROR_FAILED_SERVICE_CONTROLLER_CONNECT as i32) {
            bail!(
                "--service is only for the Windows service manager; install the service with \
                 hail --install, or run hail without --service"
            );
        }
        return Err(error).context("connecting to the Windows service manager");
    }
    Ok(EXIT_CODE.load(Ordering::SeqCst))
}

/// The command line the service manager runs.
pub fn service_command(executable: &Path, config: Option<&Path>, watch_config: bool) -> String {
    let mut command = format!("\"{}\" --service", executable.display());
    if watch_config {
        command.push_str(" --watch-config");
    }
    if let Some(config) = config {
        command.push_str(&format!(" --config \"{}\"", config.display()));
    }
    command
}

/// Registers HAIL as an automatic-start service running as LocalSystem, with recovery
/// actions that restart it after a non-zero exit, then starts it.
pub fn install(executable: &Path, config: Option<&Path>, watch_config: bool) -> Result<()> {
    let manager = open_manager(SC_MANAGER_CONNECT | SC_MANAGER_CREATE_SERVICE)?;
    let name = wide(SERVICE_NAME);
    let display = wide(DISPLAY_NAME);
    let command = wide(&service_command(executable, config, watch_config));
    // SAFETY: all strings are NUL-terminated locals; null account means LocalSystem.
    let handle = unsafe {
        CreateServiceW(
            manager.0,
            name.as_ptr(),
            display.as_ptr(),
            SERVICE_ALL_ACCESS,
            SERVICE_WIN32_OWN_PROCESS,
            SERVICE_AUTO_START,
            SERVICE_ERROR_NORMAL,
            command.as_ptr(),
            null(),
            null_mut(),
            null(),
            null(),
            null(),
        )
    };
    if handle.is_null() {
        if std::io::Error::last_os_error().raw_os_error() == Some(ERROR_SERVICE_EXISTS as i32) {
            bail!("service {SERVICE_NAME} is already installed; run hail --uninstall first");
        }
        return Err(last_error("creating the service"));
    }
    let service = ScHandle(handle);

    let mut description = wide(DESCRIPTION);
    let description = SERVICE_DESCRIPTIONW {
        lpDescription: description.as_mut_ptr(),
    };
    let mut actions = RESTART_DELAYS_MS.map(|delay| SC_ACTION {
        Type: SC_ACTION_RESTART,
        Delay: delay,
    });
    let failure_actions = SERVICE_FAILURE_ACTIONSW {
        dwResetPeriod: FAILURE_RESET_SECONDS,
        lpRebootMsg: null_mut(),
        lpCommand: null_mut(),
        cActions: actions.len() as u32,
        lpsaActions: actions.as_mut_ptr(),
    };
    // A clean exit with a non-zero code (75, 1) counts as a failure, not only crashes.
    let failure_flag = SERVICE_FAILURE_ACTIONS_FLAG {
        fFailureActionsOnNonCrashFailures: 1,
    };
    for (level, info, what) in [
        (
            SERVICE_CONFIG_DESCRIPTION,
            &description as *const _ as *const c_void,
            "setting the service description",
        ),
        (
            SERVICE_CONFIG_FAILURE_ACTIONS,
            &failure_actions as *const _ as *const c_void,
            "setting the service recovery actions",
        ),
        (
            SERVICE_CONFIG_FAILURE_ACTIONS_FLAG,
            &failure_flag as *const _ as *const c_void,
            "enabling recovery after non-zero exit codes",
        ),
    ] {
        // SAFETY: `info` points at the live struct matching `level`.
        if unsafe { ChangeServiceConfig2W(service.0, level, info) } == 0 {
            return Err(last_error(what));
        }
    }
    // SAFETY: valid service handle; no start arguments.
    if unsafe { StartServiceW(service.0, 0, null()) } == 0 {
        return Err(last_error(
            "the service was installed but could not be started",
        ));
    }
    Ok(())
}

/// Stops (waiting up to 20 seconds) and removes the HAIL service.
pub fn uninstall() -> Result<()> {
    let manager = open_manager(SC_MANAGER_CONNECT)?;
    let name = wide(SERVICE_NAME);
    // SAFETY: valid manager handle and NUL-terminated name.
    let handle = unsafe {
        OpenServiceW(
            manager.0,
            name.as_ptr(),
            SERVICE_STOP | SERVICE_QUERY_STATUS | DELETE,
        )
    };
    if handle.is_null() {
        if std::io::Error::last_os_error().raw_os_error()
            == Some(ERROR_SERVICE_DOES_NOT_EXIST as i32)
        {
            bail!("service {SERVICE_NAME} is not installed");
        }
        return Err(last_error("opening the service"));
    }
    let service = ScHandle(handle);
    // SAFETY: plain-data struct written by the calls below.
    let mut status: SERVICE_STATUS = unsafe { std::mem::zeroed() };
    // SAFETY: valid service handle and status pointer.
    if unsafe { ControlService(service.0, SERVICE_CONTROL_STOP, &mut status) } == 0 {
        if std::io::Error::last_os_error().raw_os_error() != Some(ERROR_SERVICE_NOT_ACTIVE as i32) {
            return Err(last_error("stopping the service"));
        }
    } else {
        let deadline = Instant::now() + Duration::from_secs(20);
        // SAFETY: valid service handle and status pointer.
        while unsafe { QueryServiceStatus(service.0, &mut status) } != 0
            && status.dwCurrentState != SERVICE_STOPPED
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(250));
        }
    }
    // SAFETY: valid service handle opened with DELETE access.
    if unsafe { DeleteService(service.0) } == 0 {
        return Err(last_error("removing the service"));
    }
    Ok(())
}

/// Path of `hail.exe` as the service manager will run it.
pub fn executable() -> Result<std::path::PathBuf> {
    std::env::current_exe().context("locating hail executable")
}

/// Expands a relative `--config` path so the service (started in System32) finds it.
pub fn absolute(path: &Path) -> Result<std::path::PathBuf> {
    std::path::absolute(path).with_context(|| format!("resolving {}", path.display()))
}
