//! Process tracking (`emulator.launch`, `process.run`, stops, stop on removal) and
//! remote calls (`api.call`, `rpc.call`) against local mock servers.
use crate::{expect_invalid, wait_for, TestContext};
use anyhow::{anyhow, ensure, Context, Result};
use hail::{
    percent_encode, rpc_request, rpc_response, Action, Config, Disk, EventKind, EventLog,
    LaunchTarget, OnError, Service,
};
use serde_json::{json, Value};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

/// File stem under which this binary acts as a fake process for `process.run` and
/// `emulator.launch`; its first argument selects what it does.
pub const FAKE_PROCESS: &str = "fake-process";

const CA_PEM: &str = include_str!("../fixtures/tls/ca.pem");
const SERVER_PEM: &str = include_str!("../fixtures/tls/server.pem");
const SERVER_KEY: &str = include_str!("../fixtures/tls/server.key");
const RECEIVE: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------------------
// Fake process

/// `window [--ignore-close]`: shows a window and exits 0 when it is closed (or, with
/// `--ignore-close`, ignores close requests / SIGTERM). `sleep`: runs without a window.
/// `exit <code>`: exits at once. Each writes `fake-<pid>.txt` (arguments and `HAIL_*`
/// variables) into its working directory once ready. All end after 60 s at the latest.
pub fn fake_process() -> i32 {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut record = String::new();
    for arg in &args {
        record.push_str(&format!("arg={arg}\n"));
    }
    let mut variables: Vec<(String, String)> = std::env::vars()
        .filter(|(name, _)| {
            name.starts_with("HAIL_")
                && !matches!(
                    name.as_str(),
                    "HAIL_TEST_ROOT" | "HAIL_APP" | "HAIL_DEFAULT_CONFIG"
                )
        })
        .collect();
    variables.sort();
    for (name, value) in variables {
        record.push_str(&format!("{name}={value}\n"));
    }
    let ready = move || {
        let name = format!("fake-{}.txt", std::process::id());
        let partial = format!("{name}.partial");
        if fs::write(&partial, &record).is_ok() {
            let _ = fs::rename(partial, name);
        }
    };
    match args.first().map(String::as_str) {
        Some("window") => fake_window(args.iter().any(|arg| arg == "--ignore-close"), ready),
        Some("sleep") => {
            ready();
            thread::sleep(Duration::from_secs(60));
            0
        }
        Some("exit") => {
            ready();
            args.get(1).and_then(|code| code.parse().ok()).unwrap_or(0)
        }
        _ => 2,
    }
}

#[cfg(windows)]
fn fake_window(ignore_close: bool, ready: impl FnOnce()) -> i32 {
    use std::sync::atomic::{AtomicBool, Ordering};
    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW,
        PostQuitMessage, RegisterClassW, SetTimer, ShowWindow, TranslateMessage, MSG,
        SW_SHOWNOACTIVATE, WM_CLOSE, WM_DESTROY, WM_TIMER, WNDCLASSW, WS_EX_NOACTIVATE,
        WS_EX_TOOLWINDOW, WS_POPUP,
    };
    static IGNORE_CLOSE: AtomicBool = AtomicBool::new(false);
    unsafe extern "system" fn procedure(
        window: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        match message {
            WM_CLOSE => {
                if !IGNORE_CLOSE.load(Ordering::SeqCst) {
                    unsafe { DestroyWindow(window) };
                }
                0
            }
            // Safety net: give up after 60 s.
            WM_TIMER => {
                unsafe { DestroyWindow(window) };
                0
            }
            WM_DESTROY => {
                unsafe { PostQuitMessage(0) };
                0
            }
            _ => unsafe { DefWindowProcW(window, message, wparam, lparam) },
        }
    }
    IGNORE_CLOSE.store(ignore_close, Ordering::SeqCst);
    let class: Vec<u16> = "HailFakeWindow\0".encode_utf16().collect();
    unsafe {
        let instance = GetModuleHandleW(std::ptr::null());
        let mut window_class: WNDCLASSW = std::mem::zeroed();
        window_class.lpfnWndProc = Some(procedure);
        window_class.hInstance = instance;
        window_class.lpszClassName = class.as_ptr();
        if RegisterClassW(&window_class) == 0 {
            return 3;
        }
        // Visible but off-screen and never activated, so tests do not disturb the desktop.
        let window = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            class.as_ptr(),
            class.as_ptr(),
            WS_POPUP,
            -32000,
            -32000,
            16,
            16,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            instance,
            std::ptr::null(),
        );
        if window.is_null() {
            return 4;
        }
        ShowWindow(window, SW_SHOWNOACTIVATE);
        SetTimer(window, 1, 60_000, None);
        ready();
        let mut message: MSG = std::mem::zeroed();
        while GetMessageW(&mut message, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
    0
}

#[cfg(not(windows))]
fn fake_window(ignore_close: bool, ready: impl FnOnce()) -> i32 {
    if ignore_close {
        unsafe { libc::signal(libc::SIGTERM, libc::SIG_IGN) };
    }
    ready();
    thread::sleep(Duration::from_secs(60));
    0
}

// ---------------------------------------------------------------------------------------
// Mock HTTP(S) server

#[derive(Clone, Debug)]
struct HttpRequest {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
    body: String,
}

impl HttpRequest {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

struct HttpReply {
    status: u16,
    body: String,
    delay: Duration,
}

fn reply(status: u16, body: &str) -> HttpReply {
    HttpReply {
        status,
        body: body.to_string(),
        delay: Duration::ZERO,
    }
}

fn read_http_request(stream: impl Read) -> Result<HttpRequest> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let method = parts.next().context("no request line")?.to_string();
    let target = parts.next().context("no request target")?.to_string();
    let mut headers = Vec::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line)?;
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        let (name, value) = line.split_once(':').context("malformed header")?;
        headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
    }
    let header = |name: &str| {
        headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
    };
    let mut body = Vec::new();
    if let Some(length) = header("content-length") {
        body.resize(length.parse()?, 0);
        reader.read_exact(&mut body)?;
    } else if header("transfer-encoding").is_some_and(|value| value.contains("chunked")) {
        loop {
            let mut size = String::new();
            reader.read_line(&mut size)?;
            let size = usize::from_str_radix(size.trim(), 16)?;
            let mut chunk = vec![0; size + 2];
            reader.read_exact(&mut chunk)?;
            if size == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..size]);
        }
    }
    Ok(HttpRequest {
        method,
        target,
        headers,
        body: String::from_utf8(body)?,
    })
}

fn serve_http(
    mut stream: impl Read + Write,
    respond: &dyn Fn(&HttpRequest) -> HttpReply,
    requests: &mpsc::Sender<HttpRequest>,
) {
    let Ok(request) = read_http_request(&mut stream) else {
        return;
    };
    let answer = respond(&request);
    let _ = requests.send(request);
    thread::sleep(answer.delay);
    let reason = match answer.status {
        200 => "OK",
        201 => "Created",
        404 => "Not Found",
        500 => "Internal Server Error",
        _ => "Status",
    };
    let _ = stream.write_all(
        format!(
            "HTTP/1.1 {} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            answer.status,
            answer.body.len(),
            answer.body
        )
        .as_bytes(),
    );
    let _ = stream.flush();
}

/// Serves `connections` HTTP (or HTTPS with the fixture certificate) connections on
/// 127.0.0.1; returns the base URL and the received requests.
fn http_server(
    tls: bool,
    connections: usize,
    respond: impl Fn(&HttpRequest) -> HttpReply + Send + 'static,
) -> Result<(String, mpsc::Receiver<HttpRequest>)> {
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let address = listener.local_addr()?;
    let tls_config = if tls {
        let certificates = CertificateDer::pem_slice_iter(SERVER_PEM.as_bytes())
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|error| anyhow!("server certificate: {error:?}"))?;
        let key = PrivateKeyDer::from_pem_slice(SERVER_KEY.as_bytes())
            .map_err(|error| anyhow!("server key: {error:?}"))?;
        Some(Arc::new(
            rustls::ServerConfig::builder_with_provider(Arc::new(
                rustls::crypto::ring::default_provider(),
            ))
            .with_safe_default_protocol_versions()?
            .with_no_client_auth()
            .with_single_cert(certificates, key)?,
        ))
    } else {
        None
    };
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        for stream in listener.incoming().take(connections) {
            let Ok(stream) = stream else { continue };
            let _ = stream.set_read_timeout(Some(RECEIVE));
            match &tls_config {
                None => serve_http(stream, &respond, &sender),
                Some(config) => {
                    let Ok(connection) = rustls::ServerConnection::new(config.clone()) else {
                        continue;
                    };
                    let mut stream = rustls::StreamOwned::new(connection, stream);
                    serve_http(&mut stream, &respond, &sender);
                    stream.conn.send_close_notify();
                    let _ = stream.flush();
                }
            }
        }
    });
    let scheme = if tls { "https" } else { "http" };
    Ok((format!("{scheme}://{address}"), receiver))
}

// ---------------------------------------------------------------------------------------
// Mock JSON-RPC listeners: one request line in, one response line out.

type RpcResponder = Arc<dyn Fn(&Value) -> Option<String> + Send + Sync>;

fn serve_rpc(stream: impl Read + Write, respond: &RpcResponder, requests: &mpsc::Sender<Value>) {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() {
        return;
    }
    let Ok(request) = serde_json::from_str::<Value>(&line) else {
        return;
    };
    let answer = respond(&request);
    let _ = requests.send(request);
    if let Some(answer) = answer {
        let stream = reader.get_mut();
        let _ = stream.write_all(format!("{answer}\n").as_bytes());
        let _ = stream.flush();
    }
}

fn tcp_rpc_server(
    connections: usize,
    respond: RpcResponder,
) -> Result<(String, mpsc::Receiver<Value>)> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let address = listener.local_addr()?.to_string();
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        for stream in listener.incoming().take(connections) {
            let Ok(stream) = stream else { continue };
            let _ = stream.set_read_timeout(Some(RECEIVE));
            serve_rpc(stream, &respond, &sender);
        }
    });
    Ok((address, receiver))
}

/// A named pipe `\\.\pipe\<name>`; the next instance is created as soon as a client
/// connects, so sequential clients never find the pipe missing.
#[cfg(windows)]
fn local_rpc_server(
    _root: &Path,
    name: &str,
    connections: usize,
    respond: RpcResponder,
) -> Result<(String, mpsc::Receiver<Value>)> {
    use std::os::windows::io::FromRawHandle;
    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, ERROR_PIPE_CONNECTED, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Storage::FileSystem::{FlushFileBuffers, PIPE_ACCESS_DUPLEX};
    use windows_sys::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE,
        PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
    };
    let path = format!(r"\\.\pipe\{name}");
    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    // Handles travel between threads as integers.
    let create = move || -> Option<isize> {
        let handle = unsafe {
            CreateNamedPipeW(
                wide.as_ptr(),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                PIPE_UNLIMITED_INSTANCES,
                65536,
                65536,
                0,
                std::ptr::null(),
            )
        };
        (handle != INVALID_HANDLE_VALUE).then_some(handle as isize)
    };
    let first = create().with_context(|| format!("creating {path}"))?;
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut next = Some(first);
        for index in 0..connections {
            let Some(handle) = next.take() else { return };
            let raw = handle as windows_sys::Win32::Foundation::HANDLE;
            let connected = unsafe { ConnectNamedPipe(raw, std::ptr::null_mut()) } != 0
                || unsafe { GetLastError() } == ERROR_PIPE_CONNECTED;
            if index + 1 < connections {
                next = create();
            }
            if !connected {
                unsafe { CloseHandle(raw) };
                continue;
            }
            let pipe = unsafe { fs::File::from_raw_handle(raw as _) };
            serve_rpc(&pipe, &respond, &sender);
            // Wait until the client has read the response before closing.
            unsafe { FlushFileBuffers(raw) };
            drop(pipe);
        }
    });
    Ok((path, receiver))
}

#[cfg(unix)]
fn local_rpc_server(
    root: &Path,
    name: &str,
    connections: usize,
    respond: RpcResponder,
) -> Result<(String, mpsc::Receiver<Value>)> {
    let path = root.join(format!("{name}.sock"));
    let listener = std::os::unix::net::UnixListener::bind(&path)?;
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        for stream in listener.incoming().take(connections) {
            let Ok(stream) = stream else { continue };
            let _ = stream.set_read_timeout(Some(RECEIVE));
            serve_rpc(stream, &respond, &sender);
        }
    });
    Ok((path.display().to_string(), receiver))
}

// ---------------------------------------------------------------------------------------
// Helpers

fn base_config(extra: &str) -> String {
    format!(
        "[service]\nlog_file = \"test.log.txt\"\nstop_timeout_ms = 500\n[polling]\ntype = \"usb-volumes\"\nmode = \"interval\"\ninterval_ms = 250\n{extra}"
    )
}

fn open_service(root: &Path, name: &str, text: &str) -> Result<Service> {
    let config = Config::parse(text).with_context(|| format!("{name} configuration"))?;
    let directory = root.join(name);
    fs::create_dir_all(&directory)?;
    let log = EventLog::open(&directory, &directory, &config.log_file, &[])?;
    Ok(Service::new(config, log))
}

fn log_text(service: &Service) -> Result<String> {
    Ok(fs::read_to_string(&service.log.path)?)
}

fn expect_lines(text: &str, needles: &[String], what: &str) -> Result<()> {
    for needle in needles {
        ensure!(
            text.contains(needle.as_str()),
            "{what} lacks {needle:?}:\n{text}"
        );
    }
    Ok(())
}

fn pid_of(service: &Service, name: &str) -> Result<u32> {
    service
        .processes()
        .iter()
        .find(|process| process.name == name)
        .map(|process| process.pid)
        .with_context(|| format!("{name} is not tracked: {:?}", service.processes()))
}

fn record(folder: &Path, pid: u32) -> Result<String> {
    wait_for(&folder.join(format!("fake-{pid}.txt")))
}

/// The polling.failed event with `error` = `message`.
fn fail_polling(service: &mut Service, message: &str) -> Result<()> {
    service.observe(Err(anyhow!("{message}"))).map(|_| ())
}

/// How a graceful close request is described on this platform.
fn graceful() -> &'static str {
    if cfg!(windows) {
        "WM_CLOSE posted to 1 window(s)"
    } else {
        "SIGTERM sent"
    }
}

/// How the exit of a process that honoured the close request is described.
fn graceful_exit() -> &'static str {
    if cfg!(windows) {
        "(exit code 0)"
    } else {
        "(no exit code (signal))"
    }
}

fn received<T>(receiver: &mpsc::Receiver<T>, what: &str) -> Result<T> {
    receiver
        .recv_timeout(RECEIVE)
        .with_context(|| format!("{what} was not received"))
}

// ---------------------------------------------------------------------------------------
// Tests

pub fn process_tracking(context: &TestContext) -> Result<()> {
    let root = context.root.join("processes");
    fs::create_dir(&root)?;
    let exe = format!("{FAKE_PROCESS}{}", std::env::consts::EXE_SUFFIX);
    let bin = root.join("bin");
    fs::create_dir(&bin)?;
    let fake = bin.join(&exe);
    fs::copy(std::env::current_exe()?, &fake)?;
    let fake_toml = fake.display().to_string();
    let with_emulators = |events: &str| {
        base_config(&format!(
            "[emulators]\nWin = {{ path = '{fake_toml}', args = [\"window\", \"{{image}}\"] }}\nBlank = \"\"\n{events}"
        ))
    };
    let started = |body: &str| with_emulators(&format!("[[events.\"service.started\"]]\n{body}"));
    let launch = |body: &str| started(&format!("action = \"emulator.launch\"\n{body}"));
    let process_run = |body: &str| started(&format!("action = \"process.run\"\n{body}"));

    // Parsing.
    for (text, needle) in [
        (
            launch("emulator = \"snow\"\n"),
            "snow is driven by the [emulator] handler; use emulator.start",
        ),
        (
            launch("emulator = \"Nope\"\n"),
            "emulator \"Nope\" is not listed in [emulators]",
        ),
        (
            launch("emulator = { name = \"Win\", path = 'x' }\n"),
            "emulator.path needs emulator.custom = true",
        ),
        (
            launch("emulator = { name = \"Win\", args = [] }\n"),
            "emulator.args needs emulator.custom = true",
        ),
        (
            launch("emulator = { custom = true, name = \"X\" }\n"),
            "emulator.custom = true needs emulator.path",
        ),
        (
            launch("emulator = { custom = true, name = \"X\", path = \"relative.exe\" }\n"),
            "emulator.path must be a full path",
        ),
        (
            launch("emulator = { custom = true, name = \"X\", path = '{fake_toml}', cmd = 1 }\n"),
            "unknown key \"cmd\"",
        ),
        (
            launch("emulator = { custom = true, path = 'x' }\n"),
            "emulator.name is required",
        ),
        (
            launch("emulator = { custom = true, name = \"bad name\", path = 'x' }\n"),
            "invalid name \"bad name\"",
        ),
        (
            launch("emulator = 5\n"),
            "emulator must be a short name or a table",
        ),
        (
            launch("emulator = \"Win\"\nimage = \"relative.rom\"\n"),
            "image must be a full path",
        ),
        (
            launch("emulator = \"Win\"\nstop_on_remove = true\n"),
            "stop_on_remove applies only to usb.inserted chains",
        ),
        (
            launch("emulator = \"Win\"\nimagex = \"x\"\n"),
            "unknown field `imagex`",
        ),
        (
            launch("emulator = \"Win\"\non_error = \"explode\"\n"),
            "unknown variant `explode`",
        ),
        (
            process_run("command = \"tool.exe\"\n"),
            "command must be a full path",
        ),
        (
            process_run(&format!("command = '{fake_toml}'\ntimeout_ms = 500\n")),
            "timeout_ms applies only with wait = true",
        ),
        (
            process_run(&format!(
                "command = '{fake_toml}'\nwait = true\nsingle = true\n"
            )),
            "single and stop_on_remove apply only without wait = true",
        ),
        (
            process_run(&format!(
                "command = '{fake_toml}'\nwait = true\ntimeout_ms = 5\n"
            )),
            "timeout_ms must be 100..=3600000",
        ),
        (
            process_run(&format!("command = '{fake_toml}'\nname = \"bad name\"\n")),
            "invalid name \"bad name\"",
        ),
        (
            process_run(&format!("command = '{fake_toml}'\nargs = \"x\"\n")),
            "args must be an array of strings",
        ),
        (
            process_run(&format!(
                "command = '{fake_toml}'\nargs = [\"{{image}}\"]\n"
            )),
            "image",
        ),
        (
            started("action = \"process.stop\"\nname = \"bad name\"\n"),
            "invalid name \"bad name\"",
        ),
        (
            started("action = \"emulator.stop\"\nemulator = \"bad name\"\n"),
            "invalid name \"bad name\"",
        ),
        (
            started("action = \"emulator.stop\"\nemulator = \"snow\"\n"),
            "emulator.stop requires an [emulator] section",
        ),
        (
            with_emulators("").replace("stop_timeout_ms = 500", "stop_timeout_ms = 50"),
            "service.stop_timeout_ms must be 100..=60000",
        ),
    ] {
        expect_invalid(&text, needle)?;
    }
    let parsed = Config::parse(&format!(
        "{}{}",
        launch("emulator = \"win\"\n"),
        format_args!(
            "[[events.\"service.started\"]]\naction = \"emulator.launch\"\nemulator = {{ custom = true, name = \"Own\", path = '{fake_toml}' }}\nsingle = false\non_error = \"fail\"\n[[events.\"service.started\"]]\naction = \"process.run\"\ncommand = '{fake_toml}'\n[[events.\"service.started\"]]\naction = \"process.run\"\ncommand = '{fake_toml}'\nwait = true\n[[events.\"service.started\"]]\naction = \"emulator.stop\"\nemulator = \"*\"\n[[events.\"service.started\"]]\naction = \"process.stop\"\nname = \"*\"\n"
        )
    ))?;
    ensure!(
        parsed.stop_timeout == Duration::from_millis(500) && parsed.stop_on_remove,
        "stop_timeout_ms / stop_on_remove defaults wrong"
    );
    let chain = parsed.chain(EventKind::ServiceStarted);
    let ok = matches!(
        chain,
        [
            Action::EmulatorLaunch(named),
            Action::EmulatorLaunch(custom),
            Action::ProcessRun(detached),
            Action::ProcessRun(waited),
            Action::EmulatorStop { emulator: Some(all) },
            Action::ProcessStop { name: every },
        ] if named.emulator == LaunchTarget::Named("Win".into())
            && named.single
            && named.on_error == OnError::Continue
            && named.image.is_none()
            && matches!(&custom.emulator, LaunchTarget::Custom { name, args, .. } if name == "Own" && args.len() == 1 && args[0].uses("image"))
            && !custom.single
            && custom.on_error == OnError::Fail
            && !detached.wait && !detached.single && detached.name.is_none()
            && detached.name_for(&fake) == FAKE_PROCESS
            && waited.wait && waited.timeout == Duration::from_secs(30)
            && all == "*" && every == "*"
    );
    ensure!(
        ok,
        "emulator.launch / process.run / stops parsed wrongly: {chain:#?}"
    );
    ensure!(
        Config::parse(&started("action = \"emulator.stop\"\nemulator = \"Win\"\n")).is_ok(),
        "emulator.stop with an [emulators] name must not need [emulator]"
    );
    // process_run / process_stop are accepted spellings of process.run / process.stop.
    let aliased = Config::parse(&format!(
        "{}[[events.\"service.started\"]]\naction = \"process_run\"\ncommand = '{fake_toml}'\nwait = true\n[[events.\"service.started\"]]\naction = \"process_stop\"\nname = \"*\"\n",
        launch("emulator = \"win\"\n")
    ))?;
    let chain = aliased.chain(EventKind::ServiceStarted);
    ensure!(
        matches!(
            chain,
            [
                Action::EmulatorLaunch(_),
                Action::ProcessRun(run),
                Action::ProcessStop { name },
            ] if run.wait && name == "*"
        ),
        "process_run / process_stop aliases parsed wrongly: {chain:#?}"
    );

    // Launch, single, graceful close, forced kill, terminate without a window.
    let mut service = open_service(
        &root,
        "stops",
        &with_emulators(&format!(
            "[[events.\"service.started\"]]\naction = \"emulator.launch\"\nemulator = \"Win\"\n\
             [[events.\"service.started\"]]\naction = \"emulator.launch\"\nemulator = {{ custom = true, name = \"Stubborn\", path = '{fake_toml}', args = [\"window\", \"--ignore-close\"] }}\n\
             [[events.\"service.started\"]]\naction = \"process.run\"\ncommand = '{fake_toml}'\nargs = [\"sleep\"]\nname = \"sleeper\"\n\
             [[events.\"polling.failed\"]]\naction = \"emulator.stop\"\nemulator = \"win\"\n\
             [[events.\"polling.failed\"]]\naction = \"emulator.stop\"\nemulator = \"Stubborn\"\n\
             [[events.\"polling.failed\"]]\naction = \"process.stop\"\nname = \"sleeper\"\n\
             [[events.\"polling.failed\"]]\naction = \"process.stop\"\nname = \"nothing\"\n"
        )),
    )?;
    service.start(&root.join("hail.toml"))?;
    let win = pid_of(&service, "Win")?;
    let stubborn = pid_of(&service, "Stubborn")?;
    let win_record = record(&bin, win)?;
    record(&bin, stubborn)?;
    expect_lines(
        &win_record,
        &[
            "arg=window\n".into(),
            "HAIL_EVENT=service.started\n".into(),
            "HAIL_IMAGE=\n".into(),
            "HAIL_SAVEFILE=\n".into(),
            format!("HAIL_VERSION={}\n", env!("CARGO_PKG_VERSION")),
        ],
        "emulator.launch arguments/environment",
    )?;
    ensure!(
        !win_record.contains("{image}") && win_record.matches("arg=").count() == 1,
        "{{image}} must be left out without an image:\n{win_record}"
    );
    service.start(&root.join("hail.toml"))?;
    let sleepers: Vec<u32> = service
        .processes()
        .iter()
        .filter(|process| process.name == "sleeper")
        .map(|process| process.pid)
        .collect();
    ensure!(
        sleepers.len() == 2 && service.processes().len() == 4,
        "single must stop a second emulator.launch but not process.run: {:?}",
        service.processes()
    );
    for pid in &sleepers {
        record(&bin, *pid)?;
    }
    fail_polling(&mut service, "stop them")?;
    let text = log_text(&service)?;
    let sleeper_close = if cfg!(windows) {
        "no visible window to close; terminated; exited after"
    } else {
        "SIGTERM sent; exited after"
    };
    expect_lines(
        &text,
        &[
            format!("[emulator] emulator.launch Win started (pid {win})"),
            format!("  command: \"{}\" \"window\"\n", fake.display()),
            "  trigger: service.started".into(),
            format!("[emulator] emulator.launch Stubborn started (pid {stubborn})"),
            format!("  command: \"{}\" \"window\" \"--ignore-close\"\n", fake.display()),
            "[process] process.run sleeper started (pid ".into(),
            format!("[emulator] emulator.launch Win: already running (pid {win}), not started again"),
            format!("[emulator] emulator.launch Stubborn: already running (pid {stubborn}), not started again"),
            format!("[emulator] stopped Win (pid {win}) (emulator.stop in the polling.failed chain): {}; exited after", graceful()),
            format!("[emulator] stopped Stubborn (pid {stubborn}) (emulator.stop in the polling.failed chain): {}; still running after 500 ms, killed", graceful()),
            format!("[process] stopped sleeper (pid {}) (process.stop in the polling.failed chain): {sleeper_close}", sleepers[0]),
            format!("[process] stopped sleeper (pid {}) (process.stop in the polling.failed chain): {sleeper_close}", sleepers[1]),
            "[process] process.stop nothing: not running".into(),
        ],
        "stop log",
    )?;
    let win_stop = text
        .lines()
        .find(|line| line.contains(&format!("stopped Win (pid {win})")))
        .unwrap_or_default();
    ensure!(
        win_stop.ends_with(graceful_exit()),
        "Win must exit on its own after the close request: {win_stop}"
    );
    ensure!(
        service.processes().is_empty(),
        "stopped processes must be forgotten: {:?}",
        service.processes()
    );
    drop(service);

    // process.run: wait, exit codes, timeout, on_error, reaping of detached processes,
    // and an [emulators] entry without a path.
    let mut service = open_service(
        &root,
        "waits",
        &with_emulators(&format!(
            "[[events.\"service.started\"]]\naction = \"process.run\"\ncommand = '{fake_toml}'\nargs = [\"exit\", \"0\"]\nname = \"zero\"\nwait = true\n\
             [[events.\"service.started\"]]\naction = \"process.run\"\ncommand = '{fake_toml}'\nargs = [\"exit\", \"3\"]\nname = \"three\"\nwait = true\n\
             [[events.\"service.started\"]]\naction = \"process.run\"\ncommand = '{fake_toml}'\nargs = [\"sleep\"]\nname = \"slow\"\nwait = true\ntimeout_ms = 300\n\
             [[events.\"service.started\"]]\naction = \"emulator.launch\"\nemulator = \"Blank\"\n\
             [[events.\"service.started\"]]\naction = \"process.run\"\ncommand = '{fake_toml}'\nargs = [\"exit\", \"0\"]\nname = \"quick\"\n\
             [[events.\"service.started\"]]\naction = \"process.run\"\ncommand = '{fake_toml}'\nargs = [\"exit\", \"4\"]\nname = \"halt\"\nwait = true\non_error = \"stop\"\n\
             [[events.\"service.started\"]]\naction = \"log.write\"\nmessage = \"unreachable marker\"\n"
        )),
    )?;
    service.start(&root.join("hail.toml"))?;
    let deadline = Instant::now() + RECEIVE;
    while !service.processes().is_empty() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(50));
        service.reap_processes()?;
    }
    let text = log_text(&service)?;
    expect_lines(
        &text,
        &[
            "[process] process.run zero started (pid ".into(),
            "[process] zero (pid ".into(),
            "[error] process.run: three (pid ".into(),
            ") failed (exit code 3)".into(),
            "[error] process.run: slow (pid ".into(),
            ") still running after 300 ms; killed".into(),
            "[error] emulator.launch: no executable set for Blank in [emulators]".into(),
            "[process] process.run quick started (pid ".into(),
            "[error] process.run: halt (pid ".into(),
            ") failed (exit code 4)".into(),
            "[chain] service.started chain stopped: process.run failed (on_error = \"stop\")"
                .into(),
        ],
        "process.run log",
    )?;
    ensure!(
        text.contains(") finished after ") && text.contains(" ms (exit code 0)"),
        "a waited process that exits 0 must be logged as finished:\n{text}"
    );
    ensure!(
        !text.contains("unreachable marker"),
        "on_error = \"stop\" must end the chain:\n{text}"
    );
    let quick = text.lines().find(|line| {
        line.contains("[process] quick (pid ") && line.ends_with("exited (exit code 0)")
    });
    ensure!(
        quick.is_some() && service.processes().is_empty(),
        "a detached process that exits must be reaped and logged:\n{text}"
    );
    drop(service);

    let mut service = open_service(
        &root,
        "fails",
        &started(&format!(
            "action = \"process.run\"\ncommand = '{fake_toml}'\nargs = [\"exit\", \"5\"]\nname = \"five\"\nwait = true\non_error = \"fail\"\n"
        )),
    )?;
    let error = service
        .start(&root.join("hail.toml"))
        .err()
        .context("on_error = \"fail\" must fail the chain")?;
    let message = format!("{error:#}");
    ensure!(
        message.contains(
            "service.started chain: process.run failed (on_error = \"fail\"): five (pid "
        ) && message.contains("failed (exit code 5)"),
        "on_error = \"fail\" error wrong: {message}"
    );
    drop(service);

    // Started from usb.inserted: stopped on removal unless stop_on_remove = false.
    let volume = root.join("stick");
    fs::create_dir_all(volume.join("Emulator"))?;
    let disk_fake = volume.join("Emulator").join(&exe);
    fs::copy(std::env::current_exe()?, &disk_fake)?;
    crate::sized(&volume.join("game.rom"), 1024)?;
    let separator = std::path::MAIN_SEPARATOR;
    let disk = Disk {
        device: "dev-stick".into(),
        label: "stick".into(),
        mount_point: volume.clone(),
    };
    let mut service = open_service(
        &root,
        "removal",
        &with_emulators(&format!(
            "[[events.\"usb.inserted\"]]\naction = \"emulator.launch\"\nemulator = {{ custom = true, name = \"Disky\", path = '{{mount_point}}{separator}Emulator{separator}{exe}', args = [\"window\", \"{{image}}\", \"--label={{label}}\"] }}\nimage = '{{mount_point}}{separator}game.rom'\n\
             [[events.\"usb.inserted\"]]\naction = \"process.run\"\ncommand = '{fake_toml}'\nargs = [\"sleep\", \"{{label}}\"]\nname = \"keeper\"\nstop_on_remove = false\n\
             [[events.\"usb.removed\"]]\naction = \"log.write\"\nmessage = \"removed {{mount_point}}\"\n\
             [[events.\"polling.failed\"]]\naction = \"process.stop\"\nname = \"*\"\n"
        )),
    )?;
    service.observe(Ok(vec![disk.clone()]))?;
    let disky = pid_of(&service, "Disky")?;
    let keeper = pid_of(&service, "keeper")?;
    let image = volume.join("game.rom");
    let disky_record = record(&volume.join("Emulator"), disky)?;
    expect_lines(
        &disky_record,
        &[
            format!("arg=window\narg={}\narg=--label=stick\n", image.display()),
            format!("HAIL_IMAGE={}\n", image.display()),
            format!("HAIL_MOUNT_POINT={}\n", volume.display()),
            "HAIL_LABEL=stick\n".into(),
            "HAIL_EVENT=usb.inserted\n".into(),
        ],
        "disk emulator arguments/environment",
    )?;
    ensure!(
        record(&bin, keeper)?.contains("arg=sleep\narg=stick\n"),
        "process.run arguments wrong"
    );
    ensure!(
        service
            .processes()
            .iter()
            .all(|process| process.disk.as_deref() == Some(volume.as_path())),
        "processes from usb.inserted must remember their disk: {:?}",
        service.processes()
    );
    service.observe(Ok(Vec::new()))?;
    let text = log_text(&service)?;
    let stop_line = format!(
        "[emulator] stopped Disky (pid {disky}) (usb.removed {}): {}; exited after",
        volume.display(),
        graceful()
    );
    let removed_line = format!("removed {}", volume.display());
    ensure!(
        text.find(&stop_line)
            .zip(text.find(&removed_line))
            .is_some_and(|(stop, removed)| stop < removed),
        "Disky must be stopped before the usb.removed chain runs:\n{text}"
    );
    ensure!(
        service.processes().len() == 1 && pid_of(&service, "keeper")? == keeper,
        "stop_on_remove = false must keep the process: {:?}",
        service.processes()
    );
    fail_polling(&mut service, "cleanup")?;
    expect_lines(
        &log_text(&service)?,
        &[format!(
            "[process] stopped keeper (pid {keeper}) (process.stop in the polling.failed chain): "
        )],
        "process.stop *",
    )?;
    drop(service);

    // emulator.automount launches are tracked and stopped on removal too.
    fs::write(
        volume.join("hail.cfg"),
        "automount = true\nimage = \"game.rom\"\nemulator = \"Auto\"\n",
    )?;
    let mut service = open_service(
        &root,
        "automount",
        &base_config(&format!(
            "[emulator]\ntype = \"snow\"\n[emulators]\nAuto = {{ path = '{fake_toml}', args = [\"window\", \"{{image}}\"] }}\n\
             [[events.\"usb.inserted\"]]\naction = \"emulator.automount\"\n"
        )),
    )?;
    service.observe(Ok(vec![disk]))?;
    let auto = pid_of(&service, "Auto")?;
    record(&bin, auto)?;
    service.observe(Ok(Vec::new()))?;
    expect_lines(
        &log_text(&service)?,
        &[format!(
            "[emulator] stopped Auto (pid {auto}) (usb.removed {}): {}; exited after",
            volume.display(),
            graceful()
        )],
        "automount stop on removal",
    )?;
    ensure!(service.processes().is_empty(), "Auto must be forgotten");
    drop(service);

    // emulator.path per platform in an action, and Service::shutdown (run when hail exits).
    let (os, arch) = (hail::platform::host_os(), hail::platform::host_arch());
    let (other_os, other_path) = if cfg!(windows) {
        ("linux", "/opt/emu/emu")
    } else {
        ("windows", "C:\\\\Emu\\\\emu.exe")
    };
    let multi = format!(
        "{{ custom = true, name = \"Multi\", path = {{ {os} = {{ {arch} = '{fake_toml}' }}, {other_os} = \"{other_path}\" }}, args = [\"window\"] }}"
    );
    for (path, needle) in [
        ("{ macos = '/x' }", "emulator.path: unknown OS \"macos\""),
        (
            "{ windows = { x86 = 'C:\\x.exe' } }",
            "emulator.path.windows: unknown architecture \"x86\"",
        ),
        (
            "{ windows = { x64 = 'relative.exe' } }",
            "emulator.path.windows.x64 must be a full path",
        ),
        (
            "{ linux = 'C:\\x' }",
            "emulator.path.linux must be a full path",
        ),
        ("{ linux = 5 }", "emulator.path.linux must be a path"),
        ("{}", "emulator.path lists no platforms"),
    ] {
        expect_invalid(
            &launch(&format!(
                "emulator = {{ custom = true, name = \"X\", path = {path} }}\n"
            )),
            needle,
        )?;
    }
    let mut service = open_service(
        &root,
        "platforms",
        &with_emulators(&format!(
            "[[events.\"service.started\"]]\naction = \"emulator.launch\"\nemulator = {multi}\n\
             [[events.\"service.started\"]]\naction = \"emulator.launch\"\nemulator = {{ custom = true, name = \"Elsewhere\", path = {{ {other_os} = {{ arm64 = \"{other_path}\" }} }} }}\n\
             [[events.\"service.started\"]]\naction = \"process.run\"\ncommand = '{fake_toml}'\nargs = [\"sleep\"]\nname = \"lingering\"\n"
        )),
    )?;
    service.start(&root.join("hail.toml"))?;
    let multi_pid = pid_of(&service, "Multi")?;
    let lingering = pid_of(&service, "lingering")?;
    record(&bin, multi_pid)?;
    record(&bin, lingering)?;
    let stopped = service.shutdown("hail.quit")?;
    let text = log_text(&service)?;
    expect_lines(
        &text,
        &[
            format!("[emulator] emulator.launch Multi started (pid {multi_pid})"),
            format!(
                "[error] emulator.launch: Elsewhere: emulator.path: no entry for this host ({os}.{arch}; looked for {}); defined for {other_os}.arm64",
                hail::platform::candidates(os, arch, hail::platform::host_runs_x64()).join(", ")
            ),
            format!(
                "[emulator] stopped Multi (pid {multi_pid}) (hail exiting: hail.quit): {}; exited after",
                graceful()
            ),
            format!("[process] stopped lingering (pid {lingering}) (hail exiting: hail.quit): "),
        ],
        "per-platform launch and shutdown",
    )?;
    ensure!(
        stopped == 2 && service.processes().is_empty() && service.shutdown("again")? == 0,
        "shutdown must stop and forget every started program ({stopped}): {:?}",
        service.processes()
    );
    println!("PASS process tracking: emulator.launch ([emulators] and custom), process.run (detached, wait, timeout), single, emulator.stop/process.stop (close, kill, terminate), on_error continue/stop/fail, reaping, stop on usb.removed (incl. automount launches), per-platform emulator.path, stop on exit (shutdown), parse errors");
    Ok(())
}

pub fn api_and_rpc(context: &TestContext) -> Result<()> {
    let root = context.root.join("calls");
    fs::create_dir(&root)?;

    // Building blocks.
    ensure!(
        percent_encode("a b/ü&~-._") == "a%20b%2F%C3%BC%26~-._",
        "percent_encode wrong: {}",
        percent_encode("a b/ü&~-._")
    );
    ensure!(
        rpc_request(Some(7), "emulator.mount", Some(json!({"image": "x"})))
            == json!({"jsonrpc": "2.0", "id": 7, "method": "emulator.mount", "params": {"image": "x"}})
            && rpc_request(None, "notify", None) == json!({"jsonrpc": "2.0", "method": "notify"}),
        "rpc_request wrong"
    );
    let response_error = |text: &str| rpc_response(text, 7).err().unwrap_or_default();
    ensure!(
        rpc_response(r#"{"jsonrpc":"2.0","id":7,"result":{"ok":true}}"#, 7)
            == Ok(json!({"ok": true}))
            && response_error(
                r#"{"jsonrpc":"2.0","id":7,"error":{"code":-32601,"message":"Method not found","data":"x"}}"#
            ) == "error -32601: Method not found data=\"x\""
            && response_error(r#"{"jsonrpc":"2.0","id":8,"result":1}"#)
                == "response id 8 does not match request id 7"
            && response_error("nope").starts_with("response is not JSON")
            && response_error(r#"{"id":7,"result":1}"#) == "response lacks \"jsonrpc\": \"2.0\""
            && response_error(r#"{"jsonrpc":"2.0","id":7}"#)
                == "response has neither result nor error"
            && response_error("[1]") == "response is not a JSON object",
        "rpc_response checks wrong"
    );

    // Parsing.
    let empty_pem = root.join("empty.pem");
    fs::write(&empty_pem, "not a certificate\n")?;
    let api = |section: &str, body: &str| {
        base_config(&format!(
            "[apis.web]\nurl = \"http://127.0.0.1:1/\"\n{section}[[events.\"polling.failed\"]]\naction = \"api.call\"\n{body}"
        ))
    };
    let rpc = |section: &str, body: &str| {
        base_config(&format!(
            "[rpc.r]\ntransport = \"tcp\"\naddress = \"127.0.0.1:1\"\n{section}[[events.\"polling.failed\"]]\naction = \"rpc.call\"\n{body}"
        ))
    };
    for (text, needle) in [
        (
            base_config("[apis.web]\nurl = \"ftp://x\"\n"),
            "apis.web.url must start with http:// or https://".to_string(),
        ),
        (
            base_config("[apis.web]\ntimeout_ms = 500\n"),
            "apis.web.url is required".into(),
        ),
        (
            base_config("[apis.web]\nurl = \"http://x\"\ntimeout_ms = 5\n"),
            "apis.web.timeout_ms must be 100..=300000".into(),
        ),
        (
            base_config("[apis.web]\nurl = \"https://x\"\nca_file = \"ca.pem\"\n"),
            "apis.web.ca_file must be a full path".into(),
        ),
        (
            base_config(&format!(
                "[apis.web]\nurl = \"https://x\"\nca_file = '{}'\n",
                empty_pem.display()
            )),
            "contains no PEM certificates".into(),
        ),
        (
            base_config("[apis.web]\nurl = \"http://x\"\nheaders = { \"bad header\" = \"1\" }\n"),
            "invalid header name".into(),
        ),
        (
            base_config("[apis.web]\nurl = \"http://x\"\nproxy = \"y\"\n"),
            "unknown field `proxy`".into(),
        ),
        (
            base_config("[apis.\"bad name\"]\nurl = \"http://x\"\n"),
            "invalid name \"bad name\"".into(),
        ),
        (
            api("", "api = \"nope\"\n"),
            "api.call uses unknown api \"nope\"; defined in [apis]: web".into(),
        ),
        (
            api("", "api = \"web\"\njson = { a = 1 }\nbody = \"x\"\n"),
            "api.call takes json or body, not both".into(),
        ),
        (
            api(
                "",
                "api = \"web\"\njson = { a = 1 }\ncontent_type = \"text/plain\"\n",
            ),
            "content_type applies to body".into(),
        ),
        (
            api("", "api = \"web\"\ncontent_type = \"text/plain\"\n"),
            "content_type needs body".into(),
        ),
        (
            api("", "api = \"web\"\nmethod = \"GET\"\njson = { a = 1 }\n"),
            "GET requests cannot send json or body".into(),
        ),
        (
            api("", "api = \"web\"\nmethod = \"TRACE\"\n"),
            "method \"TRACE\" is not supported".into(),
        ),
        (
            api("", "api = \"web\"\npath = \"status\"\n"),
            "path must start with / or ?".into(),
        ),
        (
            api("", "api = \"web\"\npath = \"/{image}\"\n"),
            "image".into(),
        ),
        (
            rpc("", "rpc = \"nope\"\nmethod = \"m\"\n"),
            "rpc.call uses unknown rpc \"nope\"".into(),
        ),
        (
            rpc("", "rpc = \"r\"\nmethod = \"rpc.discover\"\n"),
            "reserved by JSON-RPC 2.0".into(),
        ),
        (
            rpc("", "rpc = \"r\"\nmethod = \" \"\n"),
            "method must not be empty".into(),
        ),
        (
            rpc("", "rpc = \"r\"\nmethod = \"m\"\nparams = 5\n"),
            "params must be a table (named) or an array (positional)".into(),
        ),
        (
            base_config("[rpc.r]\ntransport = \"tcp\"\naddress = \"nohost\"\n"),
            "rpc.r.address must be host:port".into(),
        ),
        (
            base_config("[rpc.r]\ntransport = \"tcp\"\n"),
            "rpc.r.address is required".into(),
        ),
        (
            base_config(
                "[rpc.r]\ntransport = \"tcp\"\naddress = \"127.0.0.1:1\"\nurl = \"http://x\"\n",
            ),
            "rpc.r.url does not apply to this transport".into(),
        ),
        (
            base_config("[rpc.r]\ntransport = \"http\"\nurl = \"http://x\"\naddress = \"a:1\"\n"),
            "rpc.r.address does not apply to this transport".into(),
        ),
        (
            base_config("[rpc.r]\ntransport = \"local\"\npath = \"relative\"\n"),
            "rpc.r.path must be".into(),
        ),
        (
            base_config("[rpc.r]\ntransport = \"carrier-pigeon\"\n"),
            "unknown variant `carrier-pigeon`".into(),
        ),
    ] {
        expect_invalid(&text, &needle)?;
    }

    // api.call over HTTP.
    let (url, requests) = http_server(false, 4, |request| {
        if request.target.ends_with("/slow") {
            HttpReply {
                delay: Duration::from_millis(1500),
                ..reply(200, "late")
            }
        } else if request.method == "PUT" {
            reply(500, "oops")
        } else if request.method == "POST" {
            reply(201, "")
        } else {
            reply(200, r#"{"ok":true}"#)
        }
    })?;
    let closed = TcpListener::bind("127.0.0.1:0")?.local_addr()?;
    let mut service = open_service(
        &root,
        "api",
        &base_config(&format!(
            "[apis.web]\nurl = \"{url}/base/\"\nheaders = {{ X-Token = \"secret\" }}\n\
             [apis.slow]\nurl = \"{url}\"\ntimeout_ms = 200\n\
             [apis.down]\nurl = \"http://{closed}\"\ntimeout_ms = 2000\n\
             [[events.\"polling.failed\"]]\naction = \"api.call\"\napi = \"web\"\npath = \"/status?error={{error}}\"\nheaders = {{ X-Event = \"{{event}}\" }}\n\
             [[events.\"polling.failed\"]]\naction = \"api.call\"\napi = \"web\"\njson = {{ error = \"{{error}}\", count = 3, nested = {{ ok = true, list = [\"{{event}}\", 1.5] }} }}\n\
             [[events.\"polling.failed\"]]\naction = \"api.call\"\napi = \"web\"\nmethod = \"put\"\npath = \"/raw\"\nbody = \"raw {{error}}\"\ncontent_type = \"text/x-hail\"\n\
             [[events.\"polling.failed\"]]\naction = \"api.call\"\napi = \"slow\"\npath = \"/slow\"\n\
             [[events.\"polling.failed\"]]\naction = \"api.call\"\napi = \"down\"\non_error = \"stop\"\n\
             [[events.\"polling.failed\"]]\naction = \"log.write\"\nmessage = \"not reached\"\n"
        )),
    )?;
    fail_polling(&mut service, "disk a/b & c\"")?;
    let get = received(&requests, "GET request")?;
    ensure!(
        get.method == "GET"
            && get.target == "/base/status?error=disk%20a%2Fb%20%26%20c%22"
            && get.header("x-token") == Some("secret")
            && get.header("x-event") == Some("polling.failed")
            && get.body.is_empty(),
        "api.call GET sent wrongly: {get:?}"
    );
    let post = received(&requests, "POST request")?;
    let posted: Value = serde_json::from_str(&post.body)?;
    ensure!(
        post.method == "POST"
            && post.target == "/base"
            && post.header("content-type") == Some("application/json")
            && posted
                == json!({"error": "disk a/b & c\"", "count": 3, "nested": {"ok": true, "list": ["polling.failed", 1.5]}}),
        "api.call json sent wrongly: {post:?}"
    );
    let put = received(&requests, "PUT request")?;
    ensure!(
        put.method == "PUT"
            && put.target == "/base/raw"
            && put.header("content-type") == Some("text/x-hail")
            && put.body == "raw disk a/b & c\"",
        "api.call body sent wrongly: {put:?}"
    );
    received(&requests, "slow request")?;
    let text = log_text(&service)?;
    expect_lines(
        &text,
        &[
            format!("[api] api.call web GET {url}/base/status?error=disk%20a%2Fb%20%26%20c%22: 200 OK ("),
            "  response: {\"ok\":true}".into(),
            format!("[api] api.call web POST {url}/base: 201 Created ("),
            "  request: {\"count\":3,".into(),
            format!("[error] api.call web PUT {url}/base/raw: 500 Internal Server Error ("),
            "  request: raw disk a/b & c\"".into(),
            "  response: oops".into(),
            format!("[error] api.call slow GET {url}/slow: "),
            format!("[error] api.call down GET http://{closed}: "),
            "[chain] polling.failed chain stopped: api.call failed (on_error = \"stop\")".into(),
        ],
        "api.call log",
    )?;
    ensure!(
        !text.contains("not reached"),
        "on_error = \"stop\" must end the chain:\n{text}"
    );
    let slow_ms = logged_ms(&text, &format!("[error] api.call slow GET {url}/slow: "));
    ensure!(
        slow_ms.is_some_and(|ms| ms < 1000),
        "api timeout_ms = 200 was not applied ({slow_ms:?} ms):\n{text}"
    );
    drop(service);

    // HTTPS: trusted through ca_file, refused without it.
    let ca = root.join("ca.pem");
    fs::write(&ca, CA_PEM)?;
    let (secure_url, secure_requests) = http_server(true, 2, |_| reply(200, "secure"))?;
    let mut service = open_service(
        &root,
        "https",
        &base_config(&format!(
            "[apis.secure]\nurl = \"{secure_url}\"\nca_file = '{}'\n\
             [apis.untrusted]\nurl = \"{secure_url}\"\n\
             [[events.\"polling.failed\"]]\naction = \"api.call\"\napi = \"secure\"\npath = \"/tls\"\n\
             [[events.\"polling.failed\"]]\naction = \"api.call\"\napi = \"untrusted\"\npath = \"/tls\"\n",
            ca.display()
        )),
    )?;
    fail_polling(&mut service, "tls")?;
    let tls_request = received(&secure_requests, "HTTPS request")?;
    ensure!(
        tls_request.target == "/tls"
            && secure_requests
                .recv_timeout(Duration::from_millis(500))
                .is_err(),
        "only the ca_file request may reach the HTTPS server: {tls_request:?}"
    );
    expect_lines(
        &log_text(&service)?,
        &[
            format!("[api] api.call secure GET {secure_url}/tls: 200 OK ("),
            "  response: secure".into(),
            format!("[error] api.call untrusted GET {secure_url}/tls: "),
        ],
        "HTTPS log",
    )?;
    drop(service);

    // rpc.call over TCP, HTTP and a named pipe / Unix socket.
    let responder: RpcResponder = Arc::new(|request: &Value| {
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        let method = request.get("method").and_then(Value::as_str).unwrap_or("");
        match method {
            "emulator.status" | "ping" | "local.ping" => Some(
                json!({"jsonrpc": "2.0", "id": id, "result": {"method": method, "running": true}})
                    .to_string(),
            ),
            "fail" => Some(
                json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "Method not found"}})
                    .to_string(),
            ),
            "wrongid" => Some(
                json!({"jsonrpc": "2.0", "id": id.as_u64().unwrap_or(0) + 100, "result": 1})
                    .to_string(),
            ),
            "garbage" => Some("not json".to_string()),
            "sleepy" => {
                thread::sleep(Duration::from_millis(1000));
                Some(json!({"jsonrpc": "2.0", "id": id, "result": "late"}).to_string())
            }
            _ => None,
        }
    });
    let (tcp, tcp_requests) = tcp_rpc_server(6, responder.clone())?;
    let http_responder = responder.clone();
    let (rpc_url, rpc_http_requests) = http_server(false, 1, move |request| {
        let body: Value = serde_json::from_str(&request.body).unwrap_or(Value::Null);
        reply(200, &http_responder(&body).unwrap_or_default())
    })?;
    let pipe_name = format!("hail-tests-{}", std::process::id());
    let (local, local_requests) = local_rpc_server(&root, &pipe_name, 2, responder)?;
    let closed = TcpListener::bind("127.0.0.1:0")?.local_addr()?;
    let mut service = open_service(
        &root,
        "rpc",
        &base_config(&format!(
            "[rpc.tcp]\ntransport = \"tcp\"\naddress = \"{tcp}\"\n\
             [rpc.web]\ntransport = \"http\"\nurl = \"{rpc_url}/rpc\"\n\
             [rpc.local]\ntransport = \"local\"\npath = '{local}'\n\
             [rpc.slow]\ntransport = \"local\"\npath = '{local}'\ntimeout_ms = 200\n\
             [rpc.down]\ntransport = \"tcp\"\naddress = \"{closed}\"\ntimeout_ms = 1000\n\
             [[events.\"polling.failed\"]]\naction = \"rpc.call\"\nrpc = \"tcp\"\nmethod = \"emulator.status\"\nparams = {{ error = \"{{error}}\", n = 1 }}\n\
             [[events.\"polling.failed\"]]\naction = \"rpc.call\"\nrpc = \"tcp\"\nmethod = \"notify.event\"\nnotify = true\nparams = [\"{{event}}\"]\n\
             [[events.\"polling.failed\"]]\naction = \"rpc.call\"\nrpc = \"tcp\"\nmethod = \"fail\"\n\
             [[events.\"polling.failed\"]]\naction = \"rpc.call\"\nrpc = \"tcp\"\nmethod = \"wrongid\"\n\
             [[events.\"polling.failed\"]]\naction = \"rpc.call\"\nrpc = \"tcp\"\nmethod = \"garbage\"\n\
             [[events.\"polling.failed\"]]\naction = \"rpc.call\"\nrpc = \"tcp\"\nmethod = \"silent\"\n\
             [[events.\"polling.failed\"]]\naction = \"rpc.call\"\nrpc = \"web\"\nmethod = \"ping\"\n\
             [[events.\"polling.failed\"]]\naction = \"rpc.call\"\nrpc = \"local\"\nmethod = \"local.ping\"\nparams = [1, \"{{error}}\"]\n\
             [[events.\"polling.failed\"]]\naction = \"rpc.call\"\nrpc = \"slow\"\nmethod = \"sleepy\"\n\
             [[events.\"polling.failed\"]]\naction = \"rpc.call\"\nrpc = \"down\"\nmethod = \"m\"\non_error = \"fail\"\n\
             [[events.\"polling.failed\"]]\naction = \"log.write\"\nmessage = \"not reached\"\n"
        )),
    )?;
    let error = fail_polling(&mut service, "rpc \"quoted\"")
        .err()
        .context("on_error = \"fail\" must fail the polling.failed chain")?;
    ensure!(
        format!("{error:#}").contains(&format!(
            "polling.failed chain: rpc.call failed (on_error = \"fail\"): rpc.call down m (id 10) via tcp {closed}: "
        )),
        "rpc on_error = \"fail\" error wrong: {error:#}"
    );
    let first = received(&tcp_requests, "rpc request")?;
    ensure!(
        first
            == json!({"jsonrpc": "2.0", "id": 1, "method": "emulator.status", "params": {"error": "rpc \"quoted\"", "n": 1}}),
        "rpc.call request wrong: {first}"
    );
    let notification = received(&tcp_requests, "rpc notification")?;
    ensure!(
        notification
            == json!({"jsonrpc": "2.0", "method": "notify.event", "params": ["polling.failed"]}),
        "rpc notification wrong: {notification}"
    );
    for method in ["fail", "wrongid", "garbage", "silent"] {
        let request = received(&tcp_requests, method)?;
        ensure!(
            request["method"] == method,
            "unexpected rpc request {request}"
        );
    }
    let over_http = received(&rpc_http_requests, "rpc over http")?;
    ensure!(
        over_http.method == "POST"
            && over_http.target == "/rpc"
            && over_http.header("content-type") == Some("application/json")
            && serde_json::from_str::<Value>(&over_http.body)?
                == json!({"jsonrpc": "2.0", "id": 7, "method": "ping"}),
        "rpc over http sent wrongly: {over_http:?}"
    );
    let over_local = received(&local_requests, "rpc over local")?;
    ensure!(
        over_local
            == json!({"jsonrpc": "2.0", "id": 8, "method": "local.ping", "params": [1, "rpc \"quoted\""]}),
        "rpc over local sent wrongly: {over_local}"
    );
    received(&local_requests, "slow rpc over local")?;
    let text = log_text(&service)?;
    let via_tcp = format!("via tcp {tcp}");
    expect_lines(
        &text,
        &[
            format!("[rpc] rpc.call tcp emulator.status (id 1) {via_tcp}: result {{\"method\":\"emulator.status\",\"running\":true}} ("),
            "  request: {\"id\":1,\"jsonrpc\":\"2.0\",\"method\":\"emulator.status\",\"params\":{\"error\":\"rpc \\\"quoted\\\"\",\"n\":1}}".into(),
            format!("[rpc] rpc.call tcp notify.event {via_tcp}: notification sent ("),
            format!("[error] rpc.call tcp fail (id 3) {via_tcp}: error -32601: Method not found ("),
            format!("[error] rpc.call tcp wrongid (id 4) {via_tcp}: response id 104 does not match request id 4 ("),
            format!("[error] rpc.call tcp garbage (id 5) {via_tcp}: response is not JSON: "),
            format!("[error] rpc.call tcp silent (id 6) {via_tcp}: connection closed without a response ("),
            format!("[rpc] rpc.call web ping (id 7) via http {rpc_url}/rpc: result {{\"method\":\"ping\",\"running\":true}} ("),
            format!("[rpc] rpc.call local local.ping (id 8) via local {local}: result {{\"method\":\"local.ping\",\"running\":true}} ("),
            format!("[error] rpc.call slow sleepy (id 9) via local {local}: "),
            format!("[error] rpc.call down m (id 10) via tcp {closed}: "),
        ],
        "rpc.call log",
    )?;
    ensure!(
        !text.contains("not reached"),
        "on_error = \"fail\" must end the chain:\n{text}"
    );
    println!("PASS api.call (GET/POST json/PUT body, headers, percent-encoding, HTTP errors, timeout, refused, HTTPS with ca_file) and rpc.call (JSON-RPC 2.0 over TCP/HTTP/{}, results, errors, id checks, notifications, timeout), on_error stop/fail, parse errors", if cfg!(windows) { "named pipe" } else { "Unix socket" });
    Ok(())
}

/// The `(<n> ms)` duration logged on the line containing `needle`.
fn logged_ms(text: &str, needle: &str) -> Option<u128> {
    let line = text.lines().find(|line| line.contains(needle))?;
    let (_, tail) = line.rsplit_once(" (")?;
    tail.strip_suffix(" ms)")?.parse().ok()
}
