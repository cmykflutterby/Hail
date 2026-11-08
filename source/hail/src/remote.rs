//! `[apis]` with `api.call` (HTTP/HTTPS requests) and `[rpc]` with `rpc.call`
//! (JSON-RPC 2.0 over HTTP, TCP or a local named pipe / Unix socket).
use crate::config::OnError;
use crate::event::utc_timestamp;
use crate::log::EventLog;
use crate::template::Template;
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

pub const DEFAULT_API_TIMEOUT_MS: u64 = 10_000;
pub const DEFAULT_RPC_TIMEOUT_MS: u64 = 5_000;
pub const MIN_CALL_TIMEOUT_MS: u64 = 100;
pub const MAX_CALL_TIMEOUT_MS: u64 = 300_000;
/// Longest response body / JSON-RPC response line read.
pub const MAX_RESPONSE_BYTES: u64 = 1 << 20;
/// Characters of request/response bodies written to the log.
const LOG_EXCERPT: usize = 1000;
/// HTTP methods `api.call` accepts.
pub const API_METHODS: [&str; 5] = ["GET", "POST", "PUT", "PATCH", "DELETE"];

/// A JSON value from TOML whose strings are templates.
#[derive(Clone, Debug, PartialEq)]
pub enum JsonTemplate {
    Literal(Value),
    Text(Template),
    Array(Vec<JsonTemplate>),
    Object(Vec<(String, JsonTemplate)>),
}

impl JsonTemplate {
    pub fn parse(label: &str, value: &toml::Value, allowed: &[&str]) -> Result<Self> {
        Ok(match value {
            toml::Value::String(text) => JsonTemplate::Text(
                Template::parse(text, allowed).with_context(|| label.to_string())?,
            ),
            toml::Value::Integer(number) => JsonTemplate::Literal(Value::from(*number)),
            toml::Value::Float(number) => JsonTemplate::Literal(
                serde_json::Number::from_f64(*number)
                    .map(Value::Number)
                    .with_context(|| format!("{label}: {number} is not a JSON number"))?,
            ),
            toml::Value::Boolean(value) => JsonTemplate::Literal(Value::Bool(*value)),
            toml::Value::Datetime(value) => JsonTemplate::Literal(Value::String(value.to_string())),
            toml::Value::Array(items) => JsonTemplate::Array(
                items
                    .iter()
                    .enumerate()
                    .map(|(index, item)| Self::parse(&format!("{label}[{index}]"), item, allowed))
                    .collect::<Result<_>>()?,
            ),
            toml::Value::Table(table) => JsonTemplate::Object(
                table
                    .iter()
                    .map(|(key, item)| {
                        Ok((
                            key.clone(),
                            Self::parse(&format!("{label}.{key}"), item, allowed)?,
                        ))
                    })
                    .collect::<Result<_>>()?,
            ),
        })
    }

    /// The JSON value with placeholders filled in (string values are escaped by the
    /// JSON encoder, so field contents cannot change the document's structure).
    pub fn render(&self, fields: &BTreeMap<&'static str, String>) -> Value {
        match self {
            JsonTemplate::Literal(value) => value.clone(),
            JsonTemplate::Text(template) => Value::String(template.render(fields)),
            JsonTemplate::Array(items) => {
                Value::Array(items.iter().map(|item| item.render(fields)).collect())
            }
            JsonTemplate::Object(entries) => Value::Object(
                entries
                    .iter()
                    .map(|(key, item)| (key.clone(), item.render(fields)))
                    .collect(),
            ),
        }
    }

    fn is_structured(&self) -> bool {
        matches!(self, JsonTemplate::Array(_) | JsonTemplate::Object(_))
    }
}

/// Percent-encodes everything except RFC 3986 unreserved characters.
pub fn percent_encode(value: &str) -> String {
    value
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
                (byte as char).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}

/// An HTTP(S) endpoint: base URL, fixed headers and optional private CA certificates.
#[derive(Clone)]
pub struct HttpTarget {
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub ca_file: Option<PathBuf>,
    roots: Option<Vec<ureq::tls::Certificate<'static>>>,
}

impl std::fmt::Debug for HttpTarget {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HttpTarget")
            .field("url", &self.url)
            .field(
                "headers",
                &self
                    .headers
                    .iter()
                    .map(|(name, _)| name)
                    .collect::<Vec<_>>(),
            )
            .field("ca_file", &self.ca_file)
            .finish()
    }
}

fn check_header_name(label: &str, name: &str) -> Result<()> {
    ureq::http::HeaderName::from_bytes(name.as_bytes())
        .map(|_| ())
        .with_context(|| format!("{label}: invalid header name {name:?}"))
}

impl HttpTarget {
    fn parse(
        label: &str,
        url: Option<String>,
        headers: Option<BTreeMap<String, String>>,
        ca_file: Option<String>,
    ) -> Result<Self> {
        let url = url
            .with_context(|| format!("{label}.url is required"))?
            .trim()
            .to_string();
        let lower = url.to_ascii_lowercase();
        if !(lower.starts_with("http://") || lower.starts_with("https://")) {
            bail!("{label}.url must start with http:// or https://, got {url:?}");
        }
        let uri = ureq::http::Uri::try_from(url.as_str())
            .with_context(|| format!("{label}.url {url:?} is not a valid URL"))?;
        if uri.host().is_none_or(str::is_empty) {
            bail!("{label}.url {url:?} has no host");
        }
        let headers: Vec<(String, String)> = headers.unwrap_or_default().into_iter().collect();
        for (name, value) in &headers {
            check_header_name(&format!("{label}.headers"), name)?;
            ureq::http::HeaderValue::from_str(value)
                .with_context(|| format!("{label}.headers.{name}: invalid header value"))?;
        }
        let (ca_file, roots) = match ca_file {
            None => (None, None),
            Some(path) => {
                let path = PathBuf::from(path.trim());
                if !path.is_absolute() {
                    bail!(
                        "{label}.ca_file must be a full path, got {:?}",
                        path.display()
                    );
                }
                let pem = std::fs::read(&path)
                    .with_context(|| format!("{label}.ca_file: reading {}", path.display()))?;
                let mut roots = Vec::new();
                for item in ureq::tls::parse_pem(&pem) {
                    if let ureq::tls::PemItem::Certificate(certificate) =
                        item.with_context(|| format!("{label}.ca_file {}", path.display()))?
                    {
                        roots.push(certificate);
                    }
                }
                if roots.is_empty() {
                    bail!(
                        "{label}.ca_file {} contains no PEM certificates",
                        path.display()
                    );
                }
                (Some(path), Some(roots))
            }
        };
        Ok(Self {
            url,
            headers,
            ca_file,
            roots,
        })
    }

    fn agent(&self, timeout: Duration) -> ureq::Agent {
        let mut tls = ureq::tls::TlsConfig::builder();
        if let Some(roots) = &self.roots {
            tls = tls.root_certs(ureq::tls::RootCerts::new_with_certs(roots));
        }
        ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(timeout))
            .tls_config(tls.build())
            .build()
            .into()
    }
}

struct HttpReply {
    status: u16,
    reason: String,
    body: String,
}

/// Sends one request; any HTTP status is a reply, only transport problems are errors.
fn http_send(
    target: &HttpTarget,
    timeout: Duration,
    method: &str,
    url: &str,
    headers: &[(String, String)],
    body: Option<(&str, Vec<u8>)>,
) -> std::result::Result<HttpReply, String> {
    let mut builder = ureq::http::Request::builder().method(method).uri(url);
    for (name, value) in target.headers.iter().chain(headers) {
        builder = builder.header(name.as_str(), value.as_str());
    }
    let agent = target.agent(timeout);
    let response = match body {
        Some((content_type, bytes)) => builder
            .header("content-type", content_type)
            .body(bytes)
            .map_err(|error| error.to_string())
            .and_then(|request| agent.run(request).map_err(|error| error.to_string())),
        None => builder
            .body(())
            .map_err(|error| error.to_string())
            .and_then(|request| agent.run(request).map_err(|error| error.to_string())),
    };
    let mut response = response?;
    let status = response.status();
    let bytes = response
        .body_mut()
        .with_config()
        .limit(MAX_RESPONSE_BYTES)
        .read_to_vec()
        .map_err(|error| format!("reading the response: {error}"))?;
    Ok(HttpReply {
        status: status.as_u16(),
        reason: status.canonical_reason().unwrap_or("").to_string(),
        body: String::from_utf8_lossy(&bytes).into_owned(),
    })
}

/// A log excerpt: at most [`LOG_EXCERPT`] characters, continuation lines indented.
fn excerpt(text: &str) -> String {
    let text = text.trim_end();
    let mut short: String = text.chars().take(LOG_EXCERPT).collect();
    if short.len() < text.len() {
        short.push_str(&format!(" ... ({} bytes)", text.len()));
    }
    short.replace("\r\n", "\n").replace('\n', "\n    | ")
}

fn timeout_ms(label: &str, value: Option<u64>, default: u64) -> Result<Duration> {
    let value = value.unwrap_or(default);
    if !(MIN_CALL_TIMEOUT_MS..=MAX_CALL_TIMEOUT_MS).contains(&value) {
        bail!(
            "{label}.timeout_ms must be {MIN_CALL_TIMEOUT_MS}..={MAX_CALL_TIMEOUT_MS}, got {value}"
        );
    }
    Ok(Duration::from_millis(value))
}

fn check_name(section: &str, name: &str) -> Result<()> {
    if !crate::emulator::is_short_name(name) {
        bail!("{section}: invalid name {name:?} (1-32 letters, digits, - or _)");
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawApi {
    url: Option<String>,
    timeout_ms: Option<u64>,
    headers: Option<BTreeMap<String, String>>,
    ca_file: Option<String>,
}

/// One `[apis.<name>]` endpoint.
#[derive(Clone, Debug)]
pub struct Api {
    pub name: String,
    pub http: HttpTarget,
    pub timeout: Duration,
}

pub(crate) fn parse_apis(raw: BTreeMap<String, RawApi>) -> Result<BTreeMap<String, Api>> {
    raw.into_iter()
        .map(|(name, raw)| {
            check_name("apis", &name)?;
            let label = format!("apis.{name}");
            let api = Api {
                http: HttpTarget::parse(&label, raw.url, raw.headers, raw.ca_file)?,
                timeout: timeout_ms(&label, raw.timeout_ms, DEFAULT_API_TIMEOUT_MS)?,
                name: name.clone(),
            };
            Ok((name, api))
        })
        .collect()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawApiCall {
    api: String,
    method: Option<String>,
    path: Option<String>,
    json: Option<toml::Value>,
    body: Option<String>,
    content_type: Option<String>,
    headers: Option<BTreeMap<String, String>>,
    on_error: Option<OnError>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ApiBody {
    None,
    Json(JsonTemplate),
    Text {
        body: Template,
        content_type: String,
    },
}

/// An `api.call` action.
#[derive(Clone, Debug, PartialEq)]
pub struct ApiAction {
    pub api: String,
    pub method: String,
    /// Appended to the API url; placeholder values are percent-encoded.
    pub path: Template,
    pub body: ApiBody,
    pub headers: Vec<(String, Template)>,
    pub on_error: OnError,
}

impl ApiAction {
    pub(crate) fn parse(
        label: &str,
        raw: RawApiCall,
        allowed: &[&str],
        apis: &BTreeMap<String, Api>,
    ) -> Result<Self> {
        if !apis.contains_key(&raw.api) {
            bail!(
                "{label}: api.call uses unknown api {:?}; defined in [apis]: {}",
                raw.api,
                if apis.is_empty() {
                    "none".to_string()
                } else {
                    apis.keys().cloned().collect::<Vec<_>>().join(", ")
                }
            );
        }
        let body = match (raw.json, raw.body, raw.content_type) {
            (Some(_), Some(_), _) => bail!("{label}: api.call takes json or body, not both"),
            (Some(_), None, Some(_)) => {
                bail!("{label}: content_type applies to body (json is sent as application/json)")
            }
            (Some(json), None, None) => ApiBody::Json(JsonTemplate::parse(
                &format!("{label}.json"),
                &json,
                allowed,
            )?),
            (None, Some(body), content_type) => ApiBody::Text {
                body: Template::parse(&body, allowed).with_context(|| format!("{label}.body"))?,
                content_type: content_type
                    .unwrap_or_else(|| "text/plain; charset=utf-8".to_string()),
            },
            (None, None, Some(_)) => bail!("{label}: content_type needs body"),
            (None, None, None) => ApiBody::None,
        };
        let method = match raw.method {
            Some(method) => method.trim().to_ascii_uppercase(),
            None if body == ApiBody::None => "GET".to_string(),
            None => "POST".to_string(),
        };
        if !API_METHODS.contains(&method.as_str()) {
            bail!(
                "{label}: method {method:?} is not supported; use {}",
                API_METHODS.join(", ")
            );
        }
        if body != ApiBody::None && matches!(method.as_str(), "GET" | "DELETE") {
            bail!("{label}: {method} requests cannot send json or body");
        }
        let path = raw.path.unwrap_or_default();
        if !path.is_empty() && !path.starts_with(['/', '?']) {
            bail!("{label}.path must start with / or ?, got {path:?}");
        }
        let path = Template::parse(&path, allowed).with_context(|| format!("{label}.path"))?;
        let headers = raw
            .headers
            .unwrap_or_default()
            .into_iter()
            .map(|(name, value)| {
                check_header_name(&format!("{label}.headers"), &name)?;
                let value = Template::parse(&value, allowed)
                    .with_context(|| format!("{label}.headers.{name}"))?;
                Ok((name, value))
            })
            .collect::<Result<_>>()?;
        Ok(Self {
            api: raw.api,
            method,
            path,
            body,
            headers,
            on_error: raw.on_error.unwrap_or_default(),
        })
    }
}

/// Runs an `api.call`: logs the request and its outcome. Statuses of 400 and above,
/// timeouts and connection errors are action failures (`Ok(Err(..))`).
pub fn api_call(
    log: &mut EventLog,
    api: &Api,
    action: &ApiAction,
    fields: &BTreeMap<&'static str, String>,
) -> Result<std::result::Result<(), String>> {
    let url = format!(
        "{}{}",
        api.http.url.trim_end_matches('/'),
        action.path.render_with(fields, percent_encode)
    );
    let headers: Vec<(String, String)> = action
        .headers
        .iter()
        .map(|(name, value)| (name.clone(), value.render(fields)))
        .collect();
    let body = match &action.body {
        ApiBody::None => None,
        ApiBody::Json(json) => Some((
            "application/json",
            serde_json::to_string(&json.render(fields)).unwrap_or_default(),
        )),
        ApiBody::Text { body, content_type } => Some((content_type.as_str(), body.render(fields))),
    };
    let request_line = body.as_ref().map_or(String::new(), |(_, text)| {
        format!("\n  request: {}", excerpt(text))
    });
    let start = Instant::now();
    let reply = http_send(
        &api.http,
        api.timeout,
        &action.method,
        &url,
        &headers,
        body.map(|(content_type, text)| (content_type, text.into_bytes())),
    );
    let elapsed = start.elapsed().as_millis();
    let timestamp = utc_timestamp(SystemTime::now());
    let call = format!("api.call {} {} {url}", api.name, action.method);
    match reply {
        Ok(reply) => {
            let response = if reply.body.trim().is_empty() {
                String::new()
            } else {
                format!("\n  response: {}", excerpt(&reply.body))
            };
            let failed = reply.status >= 400;
            log.write_line(&format!(
                "{timestamp} [{}] {call}: {} {} ({elapsed} ms){request_line}{response}",
                if failed { "error" } else { "api" },
                reply.status,
                reply.reason
            ))?;
            Ok(if failed {
                Err(format!("{call}: HTTP {} {}", reply.status, reply.reason))
            } else {
                Ok(())
            })
        }
        Err(error) => {
            log.write_line(&format!(
                "{timestamp} [error] {call}: {error} ({elapsed} ms){request_line}"
            ))?;
            Ok(Err(format!("{call}: {error}")))
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case")]
enum RawTransport {
    Http,
    Tcp,
    Local,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawRpc {
    transport: RawTransport,
    url: Option<String>,
    address: Option<String>,
    path: Option<String>,
    timeout_ms: Option<u64>,
    headers: Option<BTreeMap<String, String>>,
    ca_file: Option<String>,
}

/// How JSON-RPC requests reach the listener.
#[derive(Clone, Debug)]
pub enum RpcTransport {
    /// HTTP POST of the request; the response body is the JSON-RPC response.
    Http(HttpTarget),
    /// `host:port`; one request line, one response line.
    Tcp(String),
    /// Windows named pipe (`\\.\pipe\name`) or Unix socket path; one line each way.
    Local(PathBuf),
}

impl RpcTransport {
    pub fn describe(&self) -> String {
        match self {
            RpcTransport::Http(target) => format!("http {}", target.url),
            RpcTransport::Tcp(address) => format!("tcp {address}"),
            RpcTransport::Local(path) => format!("local {}", path.display()),
        }
    }
}

/// One `[rpc.<name>]` endpoint.
#[derive(Clone, Debug)]
pub struct Rpc {
    pub name: String,
    pub transport: RpcTransport,
    pub timeout: Duration,
}

/// True when `path` can name a local JSON-RPC listener on this platform.
pub fn is_local_endpoint(path: &str) -> bool {
    if cfg!(windows) {
        path.len() > 9 && path[..9].eq_ignore_ascii_case(r"\\.\pipe\")
    } else {
        Path::new(path).is_absolute()
    }
}

pub(crate) fn parse_rpcs(raw: BTreeMap<String, RawRpc>) -> Result<BTreeMap<String, Rpc>> {
    raw.into_iter()
        .map(|(name, raw)| {
            check_name("rpc", &name)?;
            let label = format!("rpc.{name}");
            let only = |allowed: &[&str]| -> Result<()> {
                for (key, set) in [
                    ("url", raw.url.is_some()),
                    ("address", raw.address.is_some()),
                    ("path", raw.path.is_some()),
                    ("headers", raw.headers.is_some()),
                    ("ca_file", raw.ca_file.is_some()),
                ] {
                    if set && !allowed.contains(&key) {
                        bail!("{label}.{key} does not apply to this transport");
                    }
                }
                Ok(())
            };
            let transport = match raw.transport {
                RawTransport::Http => {
                    only(&["url", "headers", "ca_file"])?;
                    RpcTransport::Http(HttpTarget::parse(
                        &label,
                        raw.url,
                        raw.headers,
                        raw.ca_file,
                    )?)
                }
                RawTransport::Tcp => {
                    only(&["address"])?;
                    let address = raw
                        .address
                        .with_context(|| format!("{label}.address is required (host:port)"))?
                        .trim()
                        .to_string();
                    match address.rsplit_once(':') {
                        Some((host, port)) if !host.is_empty() && port.parse::<u16>().is_ok() => {}
                        _ => bail!("{label}.address must be host:port, got {address:?}"),
                    }
                    RpcTransport::Tcp(address)
                }
                RawTransport::Local => {
                    only(&["path"])?;
                    let path = raw
                        .path
                        .with_context(|| format!("{label}.path is required"))?
                        .trim()
                        .to_string();
                    if !is_local_endpoint(&path) {
                        bail!(
                            "{label}.path must be {}, got {path:?}",
                            if cfg!(windows) {
                                r"a named pipe \\.\pipe\<name>"
                            } else {
                                "the full path of a Unix socket"
                            }
                        );
                    }
                    RpcTransport::Local(PathBuf::from(path))
                }
            };
            let rpc = Rpc {
                transport,
                timeout: timeout_ms(&label, raw.timeout_ms, DEFAULT_RPC_TIMEOUT_MS)?,
                name: name.clone(),
            };
            Ok((name, rpc))
        })
        .collect()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawRpcCall {
    rpc: String,
    method: String,
    params: Option<toml::Value>,
    notify: Option<bool>,
    on_error: Option<OnError>,
}

/// An `rpc.call` action.
#[derive(Clone, Debug, PartialEq)]
pub struct RpcAction {
    pub rpc: String,
    pub method: String,
    /// A table (named) or array (positional) of parameters.
    pub params: Option<JsonTemplate>,
    /// Sent as a notification: no `id`, no response expected.
    pub notify: bool,
    pub on_error: OnError,
}

impl RpcAction {
    pub(crate) fn parse(
        label: &str,
        raw: RawRpcCall,
        allowed: &[&str],
        rpcs: &BTreeMap<String, Rpc>,
    ) -> Result<Self> {
        if !rpcs.contains_key(&raw.rpc) {
            bail!(
                "{label}: rpc.call uses unknown rpc {:?}; defined in [rpc]: {}",
                raw.rpc,
                if rpcs.is_empty() {
                    "none".to_string()
                } else {
                    rpcs.keys().cloned().collect::<Vec<_>>().join(", ")
                }
            );
        }
        let method = raw.method.trim().to_string();
        if method.is_empty() {
            bail!("{label}.method must not be empty");
        }
        if method.starts_with("rpc.") {
            bail!("{label}.method: names starting with \"rpc.\" are reserved by JSON-RPC 2.0");
        }
        let params = raw
            .params
            .map(|params| JsonTemplate::parse(&format!("{label}.params"), &params, allowed))
            .transpose()?;
        if params
            .as_ref()
            .is_some_and(|params| !params.is_structured())
        {
            bail!("{label}.params must be a table (named) or an array (positional)");
        }
        Ok(Self {
            rpc: raw.rpc,
            method,
            params,
            notify: raw.notify.unwrap_or(false),
            on_error: raw.on_error.unwrap_or_default(),
        })
    }
}

/// The JSON-RPC 2.0 request object hail sends.
pub fn rpc_request(id: Option<u64>, method: &str, params: Option<Value>) -> Value {
    let mut request = serde_json::Map::new();
    request.insert("jsonrpc".to_string(), Value::from("2.0"));
    if let Some(id) = id {
        request.insert("id".to_string(), Value::from(id));
    }
    request.insert("method".to_string(), Value::from(method));
    if let Some(params) = params {
        request.insert("params".to_string(), params);
    }
    Value::Object(request)
}

/// Checks a JSON-RPC 2.0 response for request `id`: its `result`, or the `error` as text.
pub fn rpc_response(text: &str, id: u64) -> std::result::Result<Value, String> {
    let response: Value = serde_json::from_str(text.trim())
        .map_err(|error| format!("response is not JSON: {error}"))?;
    let object = response
        .as_object()
        .ok_or("response is not a JSON object")?;
    if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err("response lacks \"jsonrpc\": \"2.0\"".to_string());
    }
    if let Some(error) = object.get("error") {
        let code = error.get("code").and_then(Value::as_i64);
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("(no message)");
        let data = error
            .get("data")
            .map_or(String::new(), |data| format!(" data={data}"));
        return Err(format!(
            "error {}: {message}{data}",
            code.map_or("?".to_string(), |code| code.to_string())
        ));
    }
    if object.get("id") != Some(&Value::from(id)) {
        return Err(format!(
            "response id {} does not match request id {id}",
            object.get("id").unwrap_or(&Value::Null)
        ));
    }
    object
        .get("result")
        .cloned()
        .ok_or_else(|| "response has neither result nor error".to_string())
}

/// Writes one request line; unless `notify`, reads one response line.
fn exchange<S: Read + Write>(
    mut stream: S,
    line: &str,
    notify: bool,
) -> std::result::Result<Option<String>, String> {
    stream
        .write_all(format!("{line}\n").as_bytes())
        .and_then(|()| stream.flush())
        .map_err(|error| format!("sending: {error}"))?;
    if notify {
        return Ok(None);
    }
    let mut response = String::new();
    BufReader::new(stream.take(MAX_RESPONSE_BYTES))
        .read_line(&mut response)
        .map_err(|error| format!("reading the response: {error}"))?;
    if response.trim().is_empty() {
        return Err("connection closed without a response".to_string());
    }
    Ok(Some(response))
}

fn tcp_exchange(
    address: &str,
    timeout: Duration,
    line: &str,
    notify: bool,
) -> std::result::Result<Option<String>, String> {
    use std::net::{TcpStream, ToSocketAddrs};
    let addresses = address
        .to_socket_addrs()
        .map_err(|error| format!("resolving {address}: {error}"))?;
    let mut last = format!("{address} did not resolve");
    for socket in addresses {
        match TcpStream::connect_timeout(&socket, timeout) {
            Ok(stream) => {
                let _ = stream.set_read_timeout(Some(timeout));
                let _ = stream.set_write_timeout(Some(timeout));
                return exchange(stream, line, notify);
            }
            Err(error) => last = format!("connecting to {socket}: {error}"),
        }
    }
    Err(last)
}

#[cfg(windows)]
fn local_exchange(
    path: &Path,
    timeout: Duration,
    line: &str,
    notify: bool,
) -> std::result::Result<Option<String>, String> {
    // All pipe instances busy (ERROR_PIPE_BUSY): retry until the deadline.
    const PIPE_BUSY: i32 = 231;
    let deadline = Instant::now() + timeout;
    let pipe = loop {
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
        {
            Ok(pipe) => break pipe,
            Err(error) if error.raw_os_error() == Some(PIPE_BUSY) && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20))
            }
            Err(error) => return Err(format!("opening {}: {error}", path.display())),
        }
    };
    // Pipe reads cannot time out; the exchange runs on a thread that is abandoned
    // (it ends when the listener closes the pipe) if no response arrives in time.
    let (sender, receiver) = std::sync::mpsc::channel();
    let line = line.to_string();
    std::thread::spawn(move || {
        let _ = sender.send(exchange(pipe, &line, notify));
    });
    receiver
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .unwrap_or_else(|_| Err(format!("no response within {} ms", timeout.as_millis())))
}

#[cfg(unix)]
fn local_exchange(
    path: &Path,
    timeout: Duration,
    line: &str,
    notify: bool,
) -> std::result::Result<Option<String>, String> {
    let stream = std::os::unix::net::UnixStream::connect(path)
        .map_err(|error| format!("connecting to {}: {error}", path.display()))?;
    let _ = stream.set_read_timeout(Some(timeout));
    let _ = stream.set_write_timeout(Some(timeout));
    exchange(stream, line, notify)
}

/// Runs an `rpc.call` with request id `id`: logs the request and the result or error.
/// JSON-RPC errors, transport errors, timeouts and malformed responses are action
/// failures (`Ok(Err(..))`).
pub fn rpc_call(
    log: &mut EventLog,
    rpc: &Rpc,
    action: &RpcAction,
    id: u64,
    fields: &BTreeMap<&'static str, String>,
) -> Result<std::result::Result<(), String>> {
    let request = rpc_request(
        (!action.notify).then_some(id),
        &action.method,
        action.params.as_ref().map(|params| params.render(fields)),
    );
    let line = request.to_string();
    let start = Instant::now();
    let reply: std::result::Result<Option<String>, String> = match &rpc.transport {
        RpcTransport::Http(target) => http_send(
            target,
            rpc.timeout,
            "POST",
            &target.url,
            &[],
            Some(("application/json", line.clone().into_bytes())),
        )
        .and_then(|reply| {
            if !(200..300).contains(&reply.status) {
                Err(format!("HTTP {} {}", reply.status, reply.reason))
            } else if action.notify {
                Ok(None)
            } else {
                Ok(Some(reply.body))
            }
        }),
        RpcTransport::Tcp(address) => tcp_exchange(address, rpc.timeout, &line, action.notify),
        RpcTransport::Local(path) => local_exchange(path, rpc.timeout, &line, action.notify),
    };
    let outcome = reply.and_then(|reply| match reply {
        None => Ok("notification sent".to_string()),
        Some(text) => {
            rpc_response(&text, id).map(|result| format!("result {}", excerpt(&result.to_string())))
        }
    });
    let elapsed = start.elapsed().as_millis();
    let call = format!(
        "rpc.call {} {}{} via {}",
        rpc.name,
        action.method,
        if action.notify {
            String::new()
        } else {
            format!(" (id {id})")
        },
        rpc.transport.describe()
    );
    let timestamp = utc_timestamp(SystemTime::now());
    let request_line = format!("\n  request: {}", excerpt(&line));
    match outcome {
        Ok(result) => {
            log.write_line(&format!(
                "{timestamp} [rpc] {call}: {result} ({elapsed} ms){request_line}"
            ))?;
            Ok(Ok(()))
        }
        Err(error) => {
            log.write_line(&format!(
                "{timestamp} [error] {call}: {error} ({elapsed} ms){request_line}"
            ))?;
            Ok(Err(format!("{call}: {error}")))
        }
    }
}
