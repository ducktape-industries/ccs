//! Opt-in HTTP access to an existing session messenger, authenticated by a private token.
use crate::messenger::{Request, Store};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    fs::{self, File, OpenOptions},
    io::{BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

const LIMIT: usize = 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) fn token(dir: &Path) -> Result<String> {
    let path = dir.join("http.token");
    match OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path) {
        Ok(mut file) => {
            let mut bytes = [0u8; 32];
            File::open("/dev/urandom")?.read_exact(&mut bytes)?;
            let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
            file.write_all(token.as_bytes())?;
            file.sync_all()?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e.into()),
    }
    let metadata = fs::symlink_metadata(&path)?;
    ensure!(
        metadata.is_file() && metadata.permissions().mode() & 0o777 == 0o600,
        "http.token must be a regular private file (mode 0600)"
    );
    let mut bytes = Vec::new();
    File::open(path)?.take(4097).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 4096, "http.token is too large");
    let token = String::from_utf8(bytes)?.trim().to_owned();
    validate_token(&token)?;
    Ok(token)
}
fn validate_token(token: &str) -> Result<()> {
    ensure!(
        !token.is_empty() && token.len() <= 4096 && token.bytes().all(|b| b.is_ascii_graphic()),
        "token must contain 1-4096 printable ASCII characters without spaces"
    );
    Ok(())
}

pub(crate) fn listen(listener: TcpListener, token: String, store: Arc<Store>) {
    std::thread::spawn(move || {
        let active = Arc::new(AtomicUsize::new(0));
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            if active.load(Ordering::SeqCst) >= 32 {
                continue;
            }
            active.fetch_add(1, Ordering::SeqCst);
            let (active, store, token) = (Arc::clone(&active), Arc::clone(&store), token.clone());
            std::thread::spawn(move || {
                let _ = stream.set_write_timeout(Some(TIMEOUT));
                let request = receive(&mut stream, &token);
                if matches!(request, Ok(Request::Subscribe)) {
                    active.fetch_sub(1, Ordering::SeqCst);
                    let mut started = false;
                    let result = store.subscribe(|revision| {
                        if !started {
                            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache, no-store\r\nX-Accel-Buffering: no\r\nConnection: close\r\n\r\n")?;
                            started = true;
                        }
                        match revision {
                            Some(revision) => write!(stream, "data: {{\"revision\":{revision}}}\n\n")?,
                            None => stream.write_all(b": heartbeat\n\n")?,
                        }
                        stream.flush()?;
                        Ok(())
                    });
                    if let Err(error) = result
                        && !started
                    {
                        let _ = respond(&mut stream, 503, json!({"error":error.to_string()}));
                    }
                    return;
                }
                let (status, response) = match request {
                    Ok(request) => match store.handle(request) {
                        Ok(value) => (200, json!({"ok":value})),
                        Err(e) => (200, json!({"error":e.to_string()})),
                    },
                    Err((status, error)) => (status, json!({"error":error})),
                };
                let _ = respond(&mut stream, status, response);
                active.fetch_sub(1, Ordering::SeqCst);
            });
        }
    });
}
fn receive(stream: &mut TcpStream, token: &str) -> std::result::Result<Request, (u16, String)> {
    let mut reader = BufReader::new(stream);
    let deadline = Instant::now() + TIMEOUT;
    let mut head = Vec::new();
    // Bound headers before parsing or allocating the body; authenticate before reading any body.
    loop {
        let mut byte = [0];
        read(&mut reader, &mut byte, deadline)
            .map_err(|_| (400, "incomplete HTTP headers".into()))?;
        head.push(byte[0]);
        if head.ends_with(b"\r\n\r\n") {
            break;
        }
        if head.len() >= 8192 {
            return Err((431, "HTTP headers too large".into()));
        }
    }
    let head = std::str::from_utf8(&head).map_err(|_| (400, "invalid HTTP headers".into()))?;
    let mut lines = head.split("\r\n");
    let first = lines.next().unwrap_or_default();
    let mut authorization = None;
    let mut length = None;
    let mut transfer = false;
    for line in lines.filter(|s| !s.is_empty()) {
        let (name, value) = line.split_once(':').ok_or((400, "invalid HTTP header".into()))?;
        match name.to_ascii_lowercase().as_str() {
            "authorization" => {
                if authorization.replace(value.trim()).is_some() {
                    return Err((400, "duplicate authorization".into()));
                }
            }
            "content-length" => {
                if length.replace(value.trim()).is_some() {
                    return Err((400, "duplicate content-length".into()));
                }
            }
            "transfer-encoding" => transfer = true,
            _ => {}
        }
    }
    let expected = format!("Bearer {token}");
    let presented = authorization.unwrap_or_default().as_bytes();
    let difference = presented
        .iter()
        .zip(expected.bytes())
        .fold(presented.len() ^ expected.len(), |diff, (a, b)| diff | usize::from(*a ^ b));
    if difference != 0 {
        return Err((401, "unauthorized".into()));
    }
    if matches!(first, "GET /events HTTP/1.1" | "GET /events HTTP/1.0") {
        if transfer || length.is_some_and(|length| length != "0") {
            return Err((400, "event subscription must not have a body".into()));
        }
        return Ok(Request::Subscribe);
    }
    if !matches!(first, "POST /rpc HTTP/1.1" | "POST /rpc HTTP/1.0") {
        return Err((404, "use POST /rpc".into()));
    }
    if transfer {
        return Err((400, "transfer-encoding is unsupported; use content-length".into()));
    }
    let length: usize = length
        .ok_or((411, "content-length required".into()))?
        .parse()
        .map_err(|_| (400, "invalid content-length".into()))?;
    if length > LIMIT {
        return Err((413, "request too large".into()));
    }
    let mut body = vec![0; length];
    read(&mut reader, &mut body, deadline).map_err(|_| (400, "incomplete HTTP body".into()))?;
    let request =
        serde_json::from_slice(&body).map_err(|_| (400, "invalid messenger request".into()))?;
    if !matches!(
        request,
        Request::Sessions { .. }
            | Request::History { .. }
            | Request::Inbox { .. }
            | Request::Message { .. }
            | Request::Reply { .. }
            | Request::Ack { .. }
    ) {
        return Err((403, "this operation requires the local Unix socket".into()));
    }
    Ok(request)
}
fn read(
    reader: &mut BufReader<&mut TcpStream>,
    mut bytes: &mut [u8],
    deadline: Instant,
) -> Result<()> {
    while !bytes.is_empty() {
        let remaining =
            deadline.checked_duration_since(Instant::now()).context("HTTP request timed out")?;
        reader.get_ref().set_read_timeout(Some(remaining))?;
        let count = reader.read(bytes)?;
        ensure!(count > 0, "incomplete HTTP request");
        bytes = &mut bytes[count..];
    }
    Ok(())
}
fn respond(stream: &mut TcpStream, status: u16, response: Value) -> Result<()> {
    let mut bytes = serde_json::to_vec(&response)?;
    if bytes.len() > LIMIT {
        bytes = serde_json::to_vec(&json!({"error":"response too large; narrow the query"}))?;
    }
    write!(
        stream,
        "HTTP/1.1 {status} Response\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n\r\n",
        bytes.len()
    )?;
    stream.write_all(&bytes)?;
    Ok(())
}

/// Call a remote server URL (or its `/rpc` endpoint). Redirects never receive the token.
pub fn call(url: &str, token: &str, request: &Request) -> Result<Value> {
    validate_token(token)?;
    let endpoint = endpoint(url, "rpc")?;
    let body = serde_json::to_vec(request)?;
    ensure!(body.len() <= LIMIT, "request too large");
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(TIMEOUT))
        .max_redirects(0)
        .http_status_as_error(false)
        .build()
        .new_agent();
    let mut response = agent
        .post(&endpoint)
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/json")
        .send(&body)?;
    let status = response.status();
    ensure!(!status.is_redirection(), "CCS redirects are not followed");
    let bytes = response.body_mut().with_config().limit(LIMIT as u64).read_to_vec()?;
    let reply: Value = serde_json::from_slice(&bytes).context("invalid CCS response")?;
    if let Some(error) = reply.get("error").and_then(Value::as_str) {
        bail!("{error}");
    }
    ensure!(status.is_success(), "CCS returned HTTP {status}");
    reply.get("ok").cloned().context("invalid CCS response")
}

fn endpoint(url: &str, route: &str) -> Result<String> {
    let uri: ureq::http::Uri = url.parse().context("invalid CCS URL")?;
    ensure!(
        matches!(uri.scheme_str(), Some("http" | "https"))
            && uri.host().is_some()
            && !url.contains('#')
            && uri.query().is_none()
            && !uri.authority().is_some_and(|a| a.as_str().contains('@')),
        "CCS URL must be http:// or https:// without credentials, query, or fragment"
    );
    let url = url.trim_end_matches('/');
    let url = url.strip_suffix("/rpc").unwrap_or(url);
    Ok(format!("{url}/{route}"))
}

/// Subscribe to authenticated revision events; reconnect by calling again after an error.
pub fn watch(url: &str, token: &str, stop: &AtomicBool, changed: impl FnMut()) -> Result<()> {
    if stop.load(Ordering::SeqCst) {
        return Ok(());
    }
    validate_token(token)?;
    let endpoint = endpoint(url, "events")?;
    let config = ureq::Agent::config_builder()
        .timeout_global(None)
        .timeout_connect(Some(Duration::from_secs(3)))
        .timeout_resolve(Some(Duration::from_secs(3)))
        .timeout_send_request(Some(Duration::from_secs(3)))
        .max_redirects(0)
        .http_status_as_error(false)
        .build();
    let agent = ureq::Agent::with_parts(
        config,
        StreamConnector,
        ureq::unversioned::resolver::DefaultResolver::default(),
    );
    let mut response = agent
        .get(endpoint)
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "text/event-stream")
        .call()?;
    ensure!(response.status().is_success(), "CCS event stream returned HTTP {}", response.status());
    ensure!(
        response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.split(';').next() == Some("text/event-stream")),
        "CCS did not return an event stream"
    );
    crate::messenger::watch_events(response.body_mut().as_reader(), stop, changed, true)
}

// ureq's recv_body timeout covers the whole response, so cap each transport read instead.
// This preserves long-lived HTTPS streams while bounding cancellation on a silent peer.
use ureq::unversioned::transport::{
    Buffers, ConnectionDetails, Connector, DefaultConnector, NextTimeout, Transport,
};
#[derive(Debug)]
struct StreamConnector;
impl Connector for StreamConnector {
    type Out = StreamTransport;
    fn connect(
        &self,
        details: &ConnectionDetails,
        chained: Option<()>,
    ) -> std::result::Result<Option<Self::Out>, ureq::Error> {
        DefaultConnector::default()
            .connect(details, chained)
            .map(|transport| transport.map(StreamTransport))
    }
}
#[derive(Debug)]
struct StreamTransport(Box<dyn Transport>);
impl Transport for StreamTransport {
    fn buffers(&mut self) -> &mut dyn Buffers {
        self.0.buffers()
    }
    fn transmit_output(
        &mut self,
        amount: usize,
        timeout: NextTimeout,
    ) -> std::result::Result<(), ureq::Error> {
        self.0.transmit_output(amount, timeout)
    }
    fn await_input(&mut self, mut timeout: NextTimeout) -> std::result::Result<bool, ureq::Error> {
        timeout.after =
            timeout.after.min(ureq::unversioned::transport::time::Duration::from_secs(3));
        self.0.await_input(timeout)
    }
    fn is_open(&mut self) -> bool {
        self.0.is_open()
    }
    fn is_tls(&self) -> bool {
        self.0.is_tls()
    }
}
