//! Starting and stopping the programs hail launches (`[emulators]` executables,
//! `emulator.launch`, `process.run`), by default in the logged-in user's desktop session
//! (`[service] emulator_session`). A service has no desktop of its own: on Windows it runs
//! in session 0, on Linux as root outside any graphical login, so a program started
//! there directly would have no visible window.
//!
//! Stopping is graceful first: `WM_CLOSE` to the process's top-level windows on Windows
//! (through the hidden `hail --close-windows <pid>` helper when the process is in another
//! session), `SIGTERM` on Linux; then [`Process::kill`].
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Where `[emulators]` executables are started (`[service] emulator_session`).
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum EmulatorSession {
    /// The logged-in user's desktop session. When hail itself already runs in a user's
    /// session (started from a desktop or a per-user autostart), that session is used.
    #[default]
    User,
    /// hail's own session and account, even when that is a service without a desktop.
    Service,
}

impl EmulatorSession {
    pub fn name(self) -> &'static str {
        match self {
            EmulatorSession::User => "user",
            EmulatorSession::Service => "service",
        }
    }
}

/// An emulator process to start.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LaunchRequest {
    pub executable: PathBuf,
    pub args: Vec<String>,
    /// Added to (or replacing) the started process's environment.
    pub env: Vec<(String, String)>,
    pub directory: Option<PathBuf>,
}

/// A started process.
#[derive(Debug)]
pub struct Started {
    pub pid: u32,
    /// Which session/account it runs in, for the log.
    pub session: String,
    /// Handle used to watch and stop it.
    pub process: Process,
}

/// What asking a process to close did.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CloseRequest {
    /// A close request was delivered (e.g. "SIGTERM sent"); wait for it to exit.
    Sent(String),
    /// It cannot be asked (e.g. no window to close); terminate it instead.
    Unsupported(String),
}

/// A handle to a started process. Dropping it neither stops nor waits for the process.
pub struct Process {
    inner: Inner,
}

enum Inner {
    Child(std::process::Child),
    /// Started in another session through `CreateProcessAsUserW`.
    #[cfg(windows)]
    Session {
        pid: u32,
        handle: platform::OwnedHandle,
        session: u32,
    },
}

impl std::fmt::Debug for Process {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "Process(pid {})", self.pid())
    }
}

impl Process {
    pub fn pid(&self) -> u32 {
        match &self.inner {
            Inner::Child(child) => child.id(),
            #[cfg(windows)]
            Inner::Session { pid, .. } => *pid,
        }
    }

    /// `None` while running; once exited, its exit code (`None` if killed by a signal).
    pub fn try_exit(&mut self) -> Option<Option<i32>> {
        match &mut self.inner {
            Inner::Child(child) => match child.try_wait() {
                Ok(Some(status)) => Some(status.code()),
                Ok(None) => None,
                Err(_) => Some(None),
            },
            #[cfg(windows)]
            Inner::Session { handle, .. } => platform::try_exit(handle),
        }
    }

    /// Asks the process to close itself: `WM_CLOSE` to its windows on Windows, `SIGTERM`
    /// on Linux.
    pub fn request_close(&mut self) -> CloseRequest {
        match &mut self.inner {
            Inner::Child(child) => platform::close_child(child),
            #[cfg(windows)]
            Inner::Session { pid, session, .. } => platform::close_in_session(*pid, *session),
        }
    }

    /// Terminates the process immediately.
    pub fn kill(&mut self) -> std::io::Result<()> {
        match &mut self.inner {
            Inner::Child(child) => child.kill(),
            #[cfg(windows)]
            Inner::Session { handle, .. } => platform::terminate(handle),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LaunchError {
    /// `emulator_session = "user"`, hail runs as a service and nobody is logged in.
    NoUser(String),
    Failed(String),
}

/// Starts `request` as `session` asks. The process runs independently of hail.
pub fn spawn(request: &LaunchRequest, session: EmulatorSession) -> Result<Started, LaunchError> {
    platform::spawn(request, session)
}

/// Posts `WM_CLOSE` to the visible top-level windows of process `pid` on this session's
/// desktop; returns how many windows were asked to close.
#[cfg(windows)]
pub fn close_windows(pid: u32) -> usize {
    platform::close_windows(pid)
}

/// Starts `request` in hail's own session/account without console I/O.
fn spawn_here(
    request: &LaunchRequest,
    command: &mut Command,
    session: String,
) -> Result<Started, LaunchError> {
    command
        .args(&request.args)
        .envs(request.env.iter().map(|(name, value)| (name, value)))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(directory) = &request.directory {
        command.current_dir(directory);
    }
    let child = command
        .spawn()
        .map_err(|error| LaunchError::Failed(error.to_string()))?;
    Ok(Started {
        pid: child.id(),
        session,
        process: Process {
            inner: Inner::Child(child),
        },
    })
}

/// Quotes one argument so the Microsoft C runtime (and `CommandLineToArgvW`) reads it
/// back unchanged.
pub fn quote_windows_arg(argument: &str) -> String {
    if !argument.is_empty() && !argument.contains([' ', '\t', '\n', '\u{b}', '"']) {
        return argument.to_string();
    }
    let mut quoted = String::from("\"");
    let mut backslashes = 0;
    for character in argument.chars() {
        match character {
            '\\' => backslashes += 1,
            '"' => {
                quoted.push_str(&"\\".repeat(backslashes * 2 + 1));
                quoted.push('"');
                backslashes = 0;
            }
            other => {
                quoted.push_str(&"\\".repeat(backslashes));
                quoted.push(other);
                backslashes = 0;
            }
        }
    }
    quoted.push_str(&"\\".repeat(backslashes * 2));
    quoted.push('"');
    quoted
}

/// A Windows command line: the quoted program path followed by the quoted arguments.
pub fn windows_command_line(executable: &Path, args: &[String]) -> String {
    // argv[0] is parsed without escapes; paths cannot contain '"'.
    let mut line = format!("\"{}\"", executable.display());
    for argument in args {
        line.push(' ');
        line.push_str(&quote_windows_arg(argument));
    }
    line
}

/// Splits a Windows environment block (`NAME=value\0...\0\0`, UTF-16) into pairs.
/// Entries without a name (such as `=C:=C:\`) are kept as-is with the name `=C:`.
pub fn parse_environment_block(block: &[u16]) -> Vec<(String, String)> {
    block
        .split(|unit| *unit == 0)
        .take_while(|entry| !entry.is_empty())
        .map(|entry| {
            let entry = String::from_utf16_lossy(entry);
            let split = entry
                .char_indices()
                .skip(1)
                .find(|(_, character)| *character == '=')
                .map_or(entry.len(), |(index, _)| index);
            let (name, value) = entry.split_at(split);
            (name.to_string(), value.get(1..).unwrap_or("").to_string())
        })
        .collect()
}

/// `base` with `extra` added; an existing name (matched ignoring case, as Windows does)
/// takes the new value in place.
pub fn merge_environment(
    mut base: Vec<(String, String)>,
    extra: &[(String, String)],
) -> Vec<(String, String)> {
    for (name, value) in extra {
        match base
            .iter_mut()
            .find(|(existing, _)| existing.eq_ignore_ascii_case(name))
        {
            Some(entry) => entry.1 = value.clone(),
            None => base.push((name.clone(), value.clone())),
        }
    }
    base
}

/// Encodes pairs as a UTF-16 environment block for `CREATE_UNICODE_ENVIRONMENT`.
pub fn environment_block(variables: &[(String, String)]) -> Vec<u16> {
    let mut block = Vec::new();
    for (name, value) in variables {
        block.extend(format!("{name}={value}").encode_utf16());
        block.push(0);
    }
    if block.is_empty() {
        block.push(0);
    }
    block.push(0);
    block
}

/// One systemd-logind session (`/run/systemd/sessions/<id>`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LogindSession {
    pub id: String,
    pub uid: u32,
    pub user: String,
    pub active: bool,
    /// `TYPE`: x11, wayland, mir, tty, unspecified.
    pub kind: String,
    /// `CLASS`: user, greeter, lock-screen, background...
    pub class: String,
    pub display: Option<String>,
    pub seat: Option<String>,
}

impl LogindSession {
    /// Parses a logind session state file; `None` without a numeric `UID`.
    pub fn parse(id: &str, text: &str) -> Option<Self> {
        let value = |key: &str| {
            text.lines().find_map(|line| {
                line.strip_prefix(key)
                    .and_then(|rest| rest.strip_prefix('='))
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string)
            })
        };
        Some(Self {
            id: id.to_string(),
            uid: value("UID")?.parse().ok()?,
            user: value("USER").unwrap_or_default(),
            active: value("ACTIVE").as_deref() == Some("1")
                || value("STATE").as_deref() == Some("active"),
            kind: value("TYPE").unwrap_or_default(),
            class: value("CLASS").unwrap_or_else(|| "user".to_string()),
            display: value("DISPLAY"),
            seat: value("SEAT"),
        })
    }

    pub fn is_graphical(&self) -> bool {
        matches!(self.kind.as_str(), "x11" | "wayland" | "mir")
    }

    /// The session an emulator should open in: an active graphical session of a real
    /// user (not root, not a login greeter), preferring one on a local seat.
    pub fn pick(sessions: &[LogindSession]) -> Option<&LogindSession> {
        let mut candidates: Vec<&LogindSession> = sessions
            .iter()
            .filter(|session| {
                session.active
                    && session.is_graphical()
                    && session.uid != 0
                    && session.class == "user"
            })
            .collect();
        candidates.sort_by_key(|session| (session.seat.is_none(), session.id.clone()));
        candidates.first().copied()
    }
}

#[cfg(windows)]
mod platform {
    use super::{
        environment_block, merge_environment, parse_environment_block, spawn_here,
        windows_command_line, CloseRequest, EmulatorSession, Inner, LaunchError, LaunchRequest,
        Process, Started,
    };
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;
    use std::process::Command;
    use std::ptr::{null, null_mut};
    use std::time::{Duration, Instant};
    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, BOOL, HANDLE, HWND, LPARAM, WAIT_OBJECT_0,
    };
    use windows_sys::Win32::System::Environment::{
        CreateEnvironmentBlock, DestroyEnvironmentBlock,
    };
    use windows_sys::Win32::System::RemoteDesktop::{
        ProcessIdToSessionId, WTSActive, WTSEnumerateSessionsW, WTSFreeMemory,
        WTSGetActiveConsoleSessionId, WTSQuerySessionInformationW, WTSQueryUserToken, WTSUserName,
        WTS_CURRENT_SERVER_HANDLE, WTS_SESSION_INFOW,
    };
    use windows_sys::Win32::System::Threading::{
        CreateProcessAsUserW, GetCurrentProcessId, GetExitCodeProcess, TerminateProcess,
        WaitForSingleObject, CREATE_NEW_CONSOLE, CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT,
        PROCESS_INFORMATION, STARTUPINFOW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindow, GetWindowThreadProcessId, IsWindowVisible, PostMessageW, GW_OWNER,
        WM_CLOSE,
    };

    /// How long the `--close-windows` helper in another session may take.
    const HELPER_TIMEOUT: Duration = Duration::from_secs(5);

    /// A process handle, closed on drop. Kept as an integer so it can move between threads.
    pub struct OwnedHandle(isize);

    impl OwnedHandle {
        fn raw(&self) -> HANDLE {
            self.0 as HANDLE
        }
    }

    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.raw()) };
        }
    }

    pub fn try_exit(handle: &OwnedHandle) -> Option<Option<i32>> {
        if unsafe { WaitForSingleObject(handle.raw(), 0) } != WAIT_OBJECT_0 {
            return None;
        }
        let mut code = 0u32;
        Some((unsafe { GetExitCodeProcess(handle.raw(), &mut code) } != 0).then_some(code as i32))
    }

    pub fn terminate(handle: &OwnedHandle) -> std::io::Result<()> {
        if unsafe { TerminateProcess(handle.raw(), 1) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    struct WindowSearch {
        pid: u32,
        posted: usize,
    }

    unsafe extern "system" fn visit_window(window: HWND, parameter: LPARAM) -> BOOL {
        let search = unsafe { &mut *(parameter as *mut WindowSearch) };
        let mut pid = 0;
        unsafe { GetWindowThreadProcessId(window, &mut pid) };
        if pid == search.pid
            && unsafe { IsWindowVisible(window) } != 0
            && unsafe { GetWindow(window, GW_OWNER) }.is_null()
            && unsafe { PostMessageW(window, WM_CLOSE, 0, 0) } != 0
        {
            search.posted += 1;
        }
        1
    }

    pub fn close_windows(pid: u32) -> usize {
        let mut search = WindowSearch { pid, posted: 0 };
        unsafe {
            EnumWindows(
                Some(visit_window),
                &mut search as *mut WindowSearch as LPARAM,
            )
        };
        search.posted
    }

    fn close_result(posted: usize) -> CloseRequest {
        if posted == 0 {
            CloseRequest::Unsupported("no visible window to close".to_string())
        } else {
            CloseRequest::Sent(format!("WM_CLOSE posted to {posted} window(s)"))
        }
    }

    pub fn close_child(child: &mut std::process::Child) -> CloseRequest {
        close_result(close_windows(child.id()))
    }

    /// Windows of another session are only reachable from that session's desktop, so a
    /// hidden `hail --close-windows <pid>` is started there; its exit code is the count.
    pub fn close_in_session(pid: u32, session: u32) -> CloseRequest {
        if current_session() == Some(session) {
            return close_result(close_windows(pid));
        }
        let executable = match std::env::current_exe() {
            Ok(executable) => executable,
            Err(error) => return CloseRequest::Unsupported(format!("cannot locate hail: {error}")),
        };
        let request = LaunchRequest {
            executable,
            args: vec!["--close-windows".to_string(), pid.to_string()],
            env: Vec::new(),
            directory: None,
        };
        let mut token: HANDLE = null_mut();
        if unsafe { WTSQueryUserToken(session, &mut token) } == 0 {
            return CloseRequest::Unsupported(format!(
                "no token for session {session}: {}",
                std::io::Error::last_os_error()
            ));
        }
        let helper = spawn_in_session(&request, token, session, CREATE_NO_WINDOW);
        unsafe { CloseHandle(token) };
        let mut helper = match helper {
            Ok(started) => started.process,
            Err(error) => {
                return CloseRequest::Unsupported(format!(
                    "close helper did not start in session {session}: {error:?}"
                ))
            }
        };
        let deadline = Instant::now() + HELPER_TIMEOUT;
        loop {
            match helper.try_exit() {
                Some(Some(count)) if count >= 0 => return close_result(count as usize),
                Some(_) => {
                    return CloseRequest::Unsupported(format!(
                        "close helper failed in session {session}"
                    ))
                }
                None if Instant::now() >= deadline => {
                    let _ = helper.kill();
                    return CloseRequest::Unsupported(format!(
                        "close helper timed out in session {session}"
                    ));
                }
                None => std::thread::sleep(Duration::from_millis(20)),
            }
        }
    }

    fn wide(text: &std::ffi::OsStr) -> Vec<u16> {
        text.encode_wide().chain(std::iter::once(0)).collect()
    }

    fn current_session() -> Option<u32> {
        let mut session = 0;
        (unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session) } != 0)
            .then_some(session)
    }

    /// Sessions that may have a desktop user: the console first, then active remote ones.
    fn candidate_sessions() -> Vec<u32> {
        let mut sessions = Vec::new();
        let console = unsafe { WTSGetActiveConsoleSessionId() };
        if console != u32::MAX && console != 0 {
            sessions.push(console);
        }
        let mut info: *mut WTS_SESSION_INFOW = null_mut();
        let mut count = 0;
        if unsafe { WTSEnumerateSessionsW(WTS_CURRENT_SERVER_HANDLE, 0, 1, &mut info, &mut count) }
            != 0
        {
            let entries = unsafe { std::slice::from_raw_parts(info, count as usize) };
            for entry in entries {
                if entry.State == WTSActive
                    && entry.SessionId != 0
                    && !sessions.contains(&entry.SessionId)
                {
                    sessions.push(entry.SessionId);
                }
            }
            unsafe { WTSFreeMemory(info.cast()) };
        }
        sessions
    }

    fn user_name(session: u32) -> String {
        let mut buffer = null_mut();
        let mut bytes = 0;
        if unsafe {
            WTSQuerySessionInformationW(
                WTS_CURRENT_SERVER_HANDLE,
                session,
                WTSUserName,
                &mut buffer,
                &mut bytes,
            )
        } == 0
            || buffer.is_null()
        {
            return "unknown user".to_string();
        }
        let units = unsafe { std::slice::from_raw_parts(buffer, bytes as usize / 2) };
        let name = String::from_utf16_lossy(units)
            .trim_end_matches('\0')
            .to_string();
        unsafe { WTSFreeMemory(buffer.cast()) };
        name
    }

    /// The environment Windows builds for the user's token, with `extra` merged in.
    fn user_environment(token: HANDLE, extra: &[(String, String)]) -> Vec<u16> {
        let mut block: *mut c_void = null_mut();
        let mut variables = Vec::new();
        if unsafe { CreateEnvironmentBlock(&mut block, token, 0) } != 0 && !block.is_null() {
            let start = block as *const u16;
            let mut length = 0;
            // Ends at the first empty entry (two consecutive NULs).
            while unsafe { *start.add(length) != 0 || *start.add(length + 1) != 0 } {
                length += 1;
            }
            let units = unsafe { std::slice::from_raw_parts(start, length + 2) };
            variables = parse_environment_block(units);
            unsafe { DestroyEnvironmentBlock(block) };
        }
        environment_block(&merge_environment(variables, extra))
    }

    fn spawn_in_session(
        request: &LaunchRequest,
        token: HANDLE,
        session: u32,
        console: u32,
    ) -> Result<Started, LaunchError> {
        let application = wide(request.executable.as_os_str());
        let mut command_line: Vec<u16> = windows_command_line(&request.executable, &request.args)
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let environment = user_environment(token, &request.env);
        let directory = request
            .directory
            .as_ref()
            .map(|path| wide(path.as_os_str()));
        let mut desktop: Vec<u16> = "winsta0\\default\0".encode_utf16().collect();
        let mut startup: STARTUPINFOW = unsafe { std::mem::zeroed() };
        startup.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        startup.lpDesktop = desktop.as_mut_ptr();
        let mut process: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
        let created = unsafe {
            CreateProcessAsUserW(
                token,
                application.as_ptr(),
                command_line.as_mut_ptr(),
                null(),
                null(),
                0,
                CREATE_UNICODE_ENVIRONMENT | console,
                environment.as_ptr().cast(),
                directory
                    .as_ref()
                    .map_or(null(), |directory| directory.as_ptr()),
                &startup,
                &mut process,
            )
        };
        if created == 0 {
            let error = std::io::Error::from_raw_os_error(unsafe { GetLastError() } as i32);
            return Err(LaunchError::Failed(error.to_string()));
        }
        unsafe { CloseHandle(process.hThread) };
        Ok(Started {
            pid: process.dwProcessId,
            session: format!("user session {session} ({})", user_name(session)),
            process: Process {
                inner: Inner::Session {
                    pid: process.dwProcessId,
                    handle: OwnedHandle(process.hProcess as isize),
                    session,
                },
            },
        })
    }

    pub fn spawn(request: &LaunchRequest, mode: EmulatorSession) -> Result<Started, LaunchError> {
        let here = current_session();
        if mode == EmulatorSession::Service || here != Some(0) {
            let session = match (here, mode) {
                (Some(0), _) => {
                    "service session 0 (no desktop: emulator_session = \"service\")".to_string()
                }
                (Some(session), _) => format!("hail's session {session}"),
                (None, _) => "hail's session".to_string(),
            };
            return spawn_here(request, &mut Command::new(&request.executable), session);
        }
        let mut last_error = None;
        for session in candidate_sessions() {
            let mut token: HANDLE = null_mut();
            if unsafe { WTSQueryUserToken(session, &mut token) } == 0 {
                last_error = Some(std::io::Error::last_os_error());
                continue;
            }
            let result = spawn_in_session(request, token, session, CREATE_NEW_CONSOLE);
            unsafe { CloseHandle(token) };
            return result;
        }
        Err(LaunchError::NoUser(match last_error {
            Some(error) => format!("no desktop session token: {error}"),
            None => "no active desktop session".to_string(),
        }))
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use super::{
        spawn_here, CloseRequest, EmulatorSession, LaunchError, LaunchRequest, LogindSession,
        Started,
    };
    use std::ffi::{CStr, CString};
    use std::os::unix::process::CommandExt;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    const SESSIONS: &str = "/run/systemd/sessions";
    const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

    pub fn close_child(child: &mut std::process::Child) -> CloseRequest {
        let Ok(pid) = libc::pid_t::try_from(child.id()) else {
            return CloseRequest::Unsupported("invalid pid".to_string());
        };
        if unsafe { libc::kill(pid, libc::SIGTERM) } == 0 {
            CloseRequest::Sent("SIGTERM sent".to_string())
        } else {
            CloseRequest::Unsupported(format!(
                "SIGTERM failed: {}",
                std::io::Error::last_os_error()
            ))
        }
    }

    struct Account {
        name: String,
        home: PathBuf,
        shell: String,
        gid: libc::gid_t,
    }

    fn account(uid: libc::uid_t) -> Option<Account> {
        let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
        let mut buffer = vec![0 as libc::c_char; 16 * 1024];
        let mut result = std::ptr::null_mut();
        let status = unsafe {
            libc::getpwuid_r(
                uid,
                &mut entry,
                buffer.as_mut_ptr(),
                buffer.len(),
                &mut result,
            )
        };
        if status != 0 || result.is_null() {
            return None;
        }
        let text = |pointer: *const libc::c_char| {
            if pointer.is_null() {
                String::new()
            } else {
                unsafe { CStr::from_ptr(pointer) }
                    .to_string_lossy()
                    .into_owned()
            }
        };
        Some(Account {
            name: text(entry.pw_name),
            home: PathBuf::from(text(entry.pw_dir)),
            shell: text(entry.pw_shell),
            gid: entry.pw_gid,
        })
    }

    fn groups(name: &str, gid: libc::gid_t) -> Vec<libc::gid_t> {
        let Ok(name) = CString::new(name) else {
            return vec![gid];
        };
        let mut count: libc::c_int = 32;
        for _ in 0..4 {
            let mut groups = vec![0 as libc::gid_t; count as usize];
            let mut found = count;
            if unsafe { libc::getgrouplist(name.as_ptr(), gid, groups.as_mut_ptr(), &mut found) }
                >= 0
            {
                groups.truncate(found as usize);
                return groups;
            }
            count = found.max(count * 2);
        }
        vec![gid]
    }

    fn sessions() -> Vec<LogindSession> {
        let Ok(entries) = std::fs::read_dir(SESSIONS) else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter_map(|entry| {
                let id = entry.file_name().to_string_lossy().into_owned();
                let text = std::fs::read_to_string(entry.path()).ok()?;
                LogindSession::parse(&id, &text)
            })
            .collect()
    }

    fn wayland_display(runtime: &Path) -> Option<String> {
        let mut names: Vec<String> = std::fs::read_dir(runtime)
            .ok()?
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with("wayland-") && !name.ends_with(".lock"))
            .collect();
        names.sort();
        names.into_iter().next()
    }

    pub fn spawn(request: &LaunchRequest, mode: EmulatorSession) -> Result<Started, LaunchError> {
        let euid = unsafe { libc::geteuid() };
        if mode == EmulatorSession::Service || euid != 0 {
            return spawn_here(
                request,
                &mut Command::new(&request.executable),
                format!("hail's account (uid {euid})"),
            );
        }
        let sessions = sessions();
        let session = LogindSession::pick(&sessions).ok_or_else(|| {
            LaunchError::NoUser(format!("no active graphical login session in {SESSIONS}"))
        })?;
        let account = account(session.uid)
            .ok_or_else(|| LaunchError::Failed(format!("no account for uid {}", session.uid)))?;
        let groups = groups(&account.name, account.gid);
        let runtime = PathBuf::from(format!("/run/user/{}", session.uid));
        let mut command = Command::new(&request.executable);
        command.env_clear();
        let inherited = |name: &str| std::env::var(name).ok();
        let mut env = vec![
            (
                "PATH".to_string(),
                inherited("PATH").unwrap_or_else(|| DEFAULT_PATH.to_string()),
            ),
            ("HOME".to_string(), account.home.display().to_string()),
            ("USER".to_string(), account.name.clone()),
            ("LOGNAME".to_string(), account.name.clone()),
            ("SHELL".to_string(), account.shell.clone()),
            ("XDG_RUNTIME_DIR".to_string(), runtime.display().to_string()),
            ("XDG_SESSION_TYPE".to_string(), session.kind.clone()),
            (
                "DISPLAY".to_string(),
                session.display.clone().unwrap_or_else(|| ":0".to_string()),
            ),
        ];
        if let Some(lang) = inherited("LANG") {
            env.push(("LANG".to_string(), lang));
        }
        if let Some(socket) = wayland_display(&runtime) {
            env.push(("WAYLAND_DISPLAY".to_string(), socket));
        }
        if runtime.join("bus").exists() {
            env.push((
                "DBUS_SESSION_BUS_ADDRESS".to_string(),
                format!("unix:path={}", runtime.join("bus").display()),
            ));
        }
        if let Some(xauthority) = [
            account.home.join(".Xauthority"),
            runtime.join("gdm").join("Xauthority"),
        ]
        .into_iter()
        .find(|path| path.is_file())
        {
            env.push(("XAUTHORITY".to_string(), xauthority.display().to_string()));
        }
        command.envs(env);
        let (uid, gid) = (session.uid, account.gid);
        // Runs in the child between fork and exec: only async-signal-safe calls.
        unsafe {
            command.pre_exec(move || {
                if libc::setgroups(groups.len(), groups.as_ptr()) != 0
                    || libc::setgid(gid) != 0
                    || libc::setuid(uid) != 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let description = format!(
            "user {} (uid {uid}), logind session {} ({})",
            account.name, session.id, session.kind
        );
        spawn_here(request, &mut command, description)
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
mod platform {
    use super::{spawn_here, CloseRequest, EmulatorSession, LaunchError, LaunchRequest, Started};
    use std::process::Command;

    pub fn close_child(_child: &mut std::process::Child) -> CloseRequest {
        CloseRequest::Unsupported("not supported on this platform".to_string())
    }

    pub fn spawn(request: &LaunchRequest, _mode: EmulatorSession) -> Result<Started, LaunchError> {
        spawn_here(
            request,
            &mut Command::new(&request.executable),
            "hail's session".to_string(),
        )
    }
}
