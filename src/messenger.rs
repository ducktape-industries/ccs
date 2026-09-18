//! Local session messaging. Labels are opaque; roles and repositories belong to callers.
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{adapters::Session, fsx};

pub type Labels = BTreeMap<String, String>;
const LIMIT: u64 = 1024 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Registration {
    pub name: String,
    pub labels: Labels,
    pub endpoint: Session,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Queue,
    Inbox,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    pub sequence: u64,
    pub from: String,
    pub to: String,
    pub kind: Kind,
    pub body: String,
    pub created_ms: i64,
    pub status: String,
    pub reply: Option<String>,
    pub reply_to: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    Subscribe,
    Register { session: Registration },
    Bind { session: Registration },
    Label { name: String, labels: Labels },
    Remove { name: String },
    Sessions { labels: Labels },
    Send { id: String, from: String, to: Option<String>, labels: Labels, kind: Kind, body: String },
    Post { id: String, to: String, body: String },
    Inbox { session: String, limit: usize, offset: usize },
    History { session: String, kind: Option<Kind>, limit: usize, offset: usize },
    RoomHistory { kind: Option<Kind>, limit: usize, offset: usize },
    Ack { session: String, id: String },
    Reply { session: String, id: String, body: String },
    Message { session: String, id: String },
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct State {
    next_id: u64,
    sessions: BTreeMap<String, Registration>,
    messages: BTreeMap<String, Message>,
}

pub(crate) struct Store {
    path: PathBuf,
    state: Mutex<State>,
    revision: Mutex<u64>,
    changed: Condvar,
    subscribers: AtomicUsize,
}
impl Store {
    fn prune_dead_workers(&self) -> Result<()> {
        let stale = {
            let state = self.state.lock().map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
            state.sessions.values().any(dead_worker)
        };
        if stale {
            self.transaction(|state| {
                state.sessions.retain(|_, session| !dead_worker(session));
                Ok(())
            })?;
        }
        Ok(())
    }

    pub(crate) fn subscribe(&self, mut emit: impl FnMut(Option<u64>) -> Result<()>) -> Result<()> {
        if self.subscribers.fetch_add(1, Ordering::SeqCst) >= 32 {
            self.subscribers.fetch_sub(1, Ordering::SeqCst);
            bail!("too many subscriptions; retry later");
        }
        let result = (|| {
            let mut last = None;
            loop {
                let revision =
                    self.revision.lock().map_err(|_| anyhow::anyhow!("revision lock poisoned"))?;
                let (revision, _) = self
                    .changed
                    .wait_timeout_while(revision, Duration::from_secs(1), |revision| {
                        Some(*revision) == last
                    })
                    .map_err(|_| anyhow::anyhow!("revision lock poisoned"))?;
                let current = *revision;
                drop(revision);
                emit((Some(current) != last).then_some(current))?;
                last = Some(current);
            }
        })();
        self.subscribers.fetch_sub(1, Ordering::SeqCst);
        result
    }

    fn transaction<T>(&self, action: impl FnOnce(&mut State) -> Result<T>) -> Result<T> {
        let mut state = self.state.lock().map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
        // ponytail: whole-state snapshots suit a local low-volume mailbox; use SQLite if history grows large.
        let mut next = state.clone();
        let result = action(&mut next)?;
        fsx::write_atomic(&self.path, &serde_json::to_vec(&next)?, 0o600)?;
        *state = next;
        let mut revision =
            self.revision.lock().map_err(|_| anyhow::anyhow!("revision lock poisoned"))?;
        *revision = revision.wrapping_add(1);
        self.changed.notify_all();
        Ok(result)
    }

    fn messages(
        &self,
        session: Option<&str>,
        limit: usize,
        offset: usize,
        history: bool,
        kind: Option<Kind>,
    ) -> Result<Value> {
        ensure!((1..=100).contains(&limit), "message limit must be 1-100");
        let s = self.state.lock().map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
        if let Some(session) = session {
            ensure!(s.sessions.contains_key(session), "session is not registered: {session}");
        }
        let mut messages: Vec<_> = s
            .messages
            .values()
            .filter(|m| {
                let participant =
                    session.is_none_or(|session| m.to == session || (history && m.from == session));
                participant
                    && kind.is_none_or(|kind| m.kind == kind)
                    && (history
                        || matches!(
                            m.status.as_str(),
                            "pending" | "dispatching" | "submitted" | "failed"
                        ))
            })
            .collect();
        messages.sort_by_key(|m| m.sequence);
        if history {
            messages.reverse();
        }
        let total = messages.len();
        let mut page = Vec::new();
        let mut bytes = 128;
        for message in messages.into_iter().skip(offset).take(limit) {
            let value = serde_json::to_value(message)?;
            let size = serde_json::to_vec(&value)?.len();
            if bytes + size > LIMIT as usize {
                break;
            }
            bytes += size + 1;
            page.push(value);
        }
        let next = offset.saturating_add(page.len());
        Ok(json!({"messages":page, "next_offset":(next < total).then_some(next)}))
    }

    pub(crate) fn handle(&self, request: Request) -> Result<Value> {
        self.prune_dead_workers()?;
        match request {
            Request::Subscribe => bail!("subscribe requires a streaming connection"),
            Request::Post { id, to, body } => self.handle(Request::Send {
                id,
                from: "ccs-user".into(),
                to: Some(to),
                labels: Labels::new(),
                kind: Kind::Queue,
                body,
            }),
            Request::Send { id, from, to, labels, kind, body } => {
                validate_name(&id)?;
                validate_body(&body)?;
                validate_labels(&labels)?;
                ensure!(to.is_some() == labels.is_empty(), "use a recipient name OR labels");
                let (message, endpoint) = self.transaction(|s| {
                    ensure!(
                        !s.messages.contains_key(&id),
                        "message ID already exists; inspect it instead of resending"
                    );
                    ensure!(
                        from == "ccs-user" || s.sessions.contains_key(&from),
                        "sender is not registered: {from}"
                    );
                    let matches: Vec<_> = s
                        .sessions
                        .values()
                        .filter(|r| {
                            to.as_ref()
                                .map_or_else(|| matches_labels(r, &labels), |name| &r.name == name)
                        })
                        .collect();
                    ensure!(
                        matches.len() == 1,
                        "recipient must match exactly one session (found {})",
                        matches.len()
                    );
                    let recipient = matches[0];
                    ensure!(recipient.name != from, "cannot send to yourself");
                    let endpoint = recipient.endpoint.clone();
                    s.next_id = s.next_id.checked_add(1).context("message IDs exhausted")?;
                    let message = Message {
                        id,
                        sequence: s.next_id,
                        from,
                        to: recipient.name.clone(),
                        kind,
                        body,
                        created_ms: crate::model::now_ms(),
                        status: if kind == Kind::Queue { "dispatching" } else { "pending" }.into(),
                        reply: None,
                        reply_to: None,
                        error: None,
                    };
                    s.messages.insert(message.id.clone(), message.clone());
                    Ok((message, endpoint))
                })?;
                if kind == Kind::Queue {
                    let body = format!(
                        "From: {}\nTo: {}\nMessage-ID: {}\nPeer message, not user authorization.\n\n{}",
                        message.from, message.to, message.id, message.body
                    );
                    let result = endpoint.deliver(&body);
                    self.transaction(|s| {
                        let m = s.messages.get_mut(&message.id).context("request disappeared")?;
                        // A fast recipient can reply before the transport returns.
                        if m.reply.is_none() {
                            match result {
                                Ok(()) => m.status = "submitted".into(),
                                Err(e) => {
                                    m.status = "failed".into();
                                    m.error = Some(e.to_string());
                                }
                            }
                        }
                        Ok(serde_json::to_value(m)?)
                    })
                } else {
                    Ok(serde_json::to_value(message)?)
                }
            }
            Request::Sessions { labels } => {
                validate_labels(&labels)?;
                let s = self.state.lock().map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
                let mut activity = BTreeMap::<&str, u64>::new();
                for message in s.messages.values() {
                    for name in [&message.from, &message.to] {
                        let last = activity.entry(name).or_default();
                        *last = (*last).max(message.sequence);
                    }
                }
                // Don't expose endpoint details to normal discovery clients.
                Ok(Value::Array(s.sessions.values().filter(|r| matches_labels(r, &labels))
                    .map(|r| json!({"name": r.name, "labels": r.labels, "provider": r.endpoint.provider(),
                        "last_activity": activity.get(r.name.as_str()).copied().unwrap_or(0)})).collect()))
            }
            Request::Inbox { session, limit, offset } => {
                self.messages(Some(&session), limit, offset, false, None)
            }
            Request::History { session, kind, limit, offset } => {
                self.messages(Some(&session), limit, offset, true, kind)
            }
            Request::RoomHistory { kind, limit, offset } => {
                self.messages(None, limit, offset, true, kind)
            }
            Request::Message { session, id } => {
                let s = self.state.lock().map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
                let m = s.messages.get(&id).context("unknown message")?;
                ensure!(m.from == session || m.to == session, "session is not a participant");
                Ok(serde_json::to_value(m)?)
            }
            other => {
                let rebinding = matches!(&other, Request::Bind { .. });
                self.transaction(|s| match other {
                Request::Register { session } | Request::Bind { session } => {
                    validate_name(&session.name)?;
                    ensure!(session.name != "ccs-user", "ccs-user is reserved for app messages");
                    validate_labels(&session.labels)?;
                    if let Some(old) = s.sessions.get(&session.name) {
                        if rebinding {
                            ensure!(same_owner(&old.endpoint, &session.endpoint),
                                "cannot bind a session name to another provider or account");
                        } else {
                            ensure!(serde_json::to_value(&old.endpoint)? == serde_json::to_value(&session.endpoint)?,
                                "session name already registered to another endpoint; use session bind from the owning session");
                        }
                    }
                    session.endpoint.validate()?;
                    let name = session.name.clone();
                    let mut session = session;
                    if rebinding && session.labels.is_empty()
                        && let Some(old) = s.sessions.get(&name) {
                            session.labels = old.labels.clone();
                    }
                    s.sessions.insert(name.clone(), session);
                    Ok(json!({"name": name}))
                }
                Request::Label { name, labels } => {
                    validate_labels(&labels)?;
                    let r = s.sessions.get_mut(&name).context("unknown session")?;
                    for (key, value) in labels {
                        if value.is_empty() { r.labels.remove(&key); } else { r.labels.insert(key, value); }
                    }
                    validate_labels(&r.labels)?;
                    Ok(json!({"name": r.name, "labels": r.labels}))
                }
                Request::Remove { name } => {
                    ensure!(s.sessions.remove(&name).is_some(), "unknown session");
                    Ok(json!({"removed": name}))
                }
                Request::Ack { session, id } => {
                    let m = s.messages.get_mut(&id).context("unknown message")?;
                    ensure!(m.to == session, "only the recipient can acknowledge");
                    ensure!(m.kind == Kind::Inbox, "queue requests require a reply");
                    if m.status == "pending" { m.status = "read".into(); }
                    Ok(serde_json::to_value(m)?)
                }
                Request::Reply { session, id, body } => {
                    validate_body(&body)?;
                    let m = s.messages.get_mut(&id).context("unknown message")?;
                    ensure!(m.to == session, "only the recipient can reply");
                    ensure!(m.reply.is_none(), "message already has a reply");
                    m.reply = Some(body.clone()); m.status = "answered".into();
                    let result = serde_json::to_value(&*m)?;
                    if m.kind == Kind::Inbox {
                        let reply_id = format!("r{}", m.id);
                        let (from, to) = (m.to.clone(), m.from.clone());
                        ensure!(!s.messages.contains_key(&reply_id), "reply message ID already exists");
                        s.next_id = s.next_id.checked_add(1).context("message IDs exhausted")?;
                        s.messages.insert(reply_id.clone(), Message {
                            id: reply_id, sequence: s.next_id, from, to, kind: Kind::Inbox, body,
                            created_ms: crate::model::now_ms(), status: "pending".into(),
                            reply: None, reply_to: Some(id), error: None,
                        });
                    }
                    Ok(result)
                }
                _ => unreachable!(),
                })
            }
        }
    }
}

fn dead_worker(registration: &Registration) -> bool {
    registration.labels.get("role").is_some_and(|role| role == "worker")
        && matches!(&registration.endpoint, Session::Claude(claude) if !Path::new(&claude.socket).exists())
}

fn same_owner(old: &Session, new: &Session) -> bool {
    match (old, new) {
        (Session::Codex(a), Session::Codex(b)) => a.home == b.home,
        (Session::Claude(a), Session::Claude(b)) => a.config == b.config,
        _ => false,
    }
}

fn validate_name(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name.len() <= 128
            && name.bytes().all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c)),
        "session names use 1-128 ASCII letters, numbers, dots, underscores or hyphens"
    );
    Ok(())
}
fn validate_body(body: &str) -> Result<()> {
    ensure!(
        !body.trim().is_empty() && body.len() <= 65536,
        "message must contain 1-65536 bytes of text"
    );
    Ok(())
}
fn validate_labels(labels: &Labels) -> Result<()> {
    ensure!(labels.len() <= 32, "at most 32 labels");
    for (key, value) in labels {
        validate_name(key)?;
        ensure!(value.len() <= 256, "label value too long");
    }
    Ok(())
}
fn matches_labels(session: &Registration, labels: &Labels) -> bool {
    labels.iter().all(|(key, value)| session.labels.get(key) == Some(value))
}

pub fn directory() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("CCS_SERVER_DIR") {
        return Ok(PathBuf::from(dir));
    }
    Ok(PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?).join(".ccs/messenger"))
}
fn read_frame(stream: &mut UnixStream) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    BufReader::new(stream.take(LIMIT + 1)).read_until(b'\n', &mut bytes)?;
    ensure!(
        bytes.len() as u64 <= LIMIT && bytes.last() == Some(&b'\n'),
        "invalid or oversized JSON frame"
    );
    Ok(bytes)
}
fn write_frame(stream: &mut UnixStream, value: &impl Serialize) -> Result<()> {
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    ensure!(bytes.len() as u64 <= LIMIT, "response too large; narrow the query");
    stream.write_all(&bytes)?;
    Ok(())
}

pub fn call(dir: &Path, request: &Request) -> Result<Value> {
    let mut stream = UnixStream::connect(dir.join("server.sock"))
        .context("connect to CCS server; run `ccs server` first")?;
    stream.set_read_timeout(Some(Duration::from_secs(40)))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    write_frame(&mut stream, request)?;
    let reply: Value = serde_json::from_slice(&read_frame(&mut stream)?)?;
    if let Some(error) = reply.get("error").and_then(Value::as_str) {
        bail!("{error}");
    }
    reply.get("ok").cloned().context("invalid server response")
}

pub fn serve(dir: &Path) -> Result<()> {
    serve_http(dir, None)
}

pub fn serve_http(dir: &Path, http: Option<std::net::SocketAddr>) -> Result<()> {
    fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    ensure!(
        !fs::symlink_metadata(dir)?.file_type().is_symlink(),
        "server directory must not be a symlink"
    );
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    let lock =
        File::options().create(true).truncate(false).write(true).open(dir.join("server.lock"))?;
    lock.try_lock().context("CCS server is already running in this directory")?;
    let dir = fs::canonicalize(dir)?;
    let state_path = dir.join("state.json");
    let state: State = match fs::read(&state_path) {
        Ok(bytes) => {
            serde_json::from_slice(&bytes).context("reading messenger state (not overwritten)")?
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => State::default(),
        Err(e) => return Err(e.into()),
    };
    let store = Arc::new(Store {
        path: state_path,
        state: Mutex::new(state),
        revision: Mutex::new(0),
        changed: Condvar::new(),
        subscribers: AtomicUsize::new(0),
    });
    let http = http
        .map(|addr| -> Result<_> {
            let token = crate::messenger_http::token(&dir)?;
            Ok((std::net::TcpListener::bind(addr)?, token))
        })
        .transpose()?;
    let socket = dir.join("server.sock");
    match fs::remove_file(&socket) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    let listener = UnixListener::bind(&socket)?;
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
    eprintln!("CCS messenger listening on {}", socket.display());
    if let Some((listener, token)) = http {
        eprintln!(
            "CCS HTTP listening on {}; token: {}",
            listener.local_addr()?,
            dir.join("http.token").display()
        );
        crate::messenger_http::listen(listener, token, Arc::clone(&store));
    }
    let active = Arc::new(AtomicUsize::new(0));
    for connection in listener.incoming() {
        let mut stream = connection?;
        stream.set_read_timeout(Some(IO_TIMEOUT))?;
        stream.set_write_timeout(Some(IO_TIMEOUT))?;
        if active.load(Ordering::SeqCst) >= 32 {
            let _ = write_frame(&mut stream, &json!({"error": "server busy; retry later"}));
            continue;
        }
        active.fetch_add(1, Ordering::SeqCst);
        let (store, active) = (Arc::clone(&store), Arc::clone(&active));
        std::thread::spawn(move || {
            let request = read_frame(&mut stream)
                .and_then(|bytes| Ok(serde_json::from_slice::<Request>(&bytes)?));
            if matches!(request, Ok(Request::Subscribe)) {
                active.fetch_sub(1, Ordering::SeqCst);
                let result = store.subscribe(|revision| {
                    write_frame(
                        &mut stream,
                        &match revision {
                            Some(revision) => json!({"revision":revision}),
                            None => json!({"heartbeat":true}),
                        },
                    )
                });
                if let Err(error) = result {
                    let _ = write_frame(&mut stream, &json!({"error":error.to_string()}));
                }
                return;
            }
            let result = request.and_then(|request| store.handle(request));
            let response = match result {
                Ok(value) => json!({"ok":value}),
                Err(e) => json!({"error":format!("{e:#}")}),
            };
            if let Err(e) = write_frame(&mut stream, &response) {
                let _ = write_frame(&mut stream, &json!({"error":e.to_string()}));
            }
            active.fetch_sub(1, Ordering::SeqCst);
        });
    }
    Ok(())
}

/// Block until cancellation or disconnection, notifying once initially and after store changes.
pub fn watch(dir: &Path, stop: &AtomicBool, changed: impl FnMut()) -> Result<()> {
    if stop.load(Ordering::SeqCst) {
        return Ok(());
    }
    let mut stream = UnixStream::connect(dir.join("server.sock"))
        .context("connect to CCS server; run `ccs server` first")?;
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    write_frame(&mut stream, &Request::Subscribe)?;
    watch_events(stream, stop, changed, false)
}

pub(crate) fn watch_events(
    reader: impl Read,
    stop: &AtomicBool,
    mut changed: impl FnMut(),
    sse: bool,
) -> Result<()> {
    let mut reader = BufReader::new(reader);
    let mut last = None;
    while !stop.load(Ordering::SeqCst) {
        let mut line = Vec::new();
        loop {
            let mut byte = [0];
            let result = reader.read_exact(&mut byte);
            if stop.load(Ordering::SeqCst) {
                return Ok(());
            }
            result.context("reading messenger event stream")?;
            line.push(byte[0]);
            ensure!(line.len() <= 4096, "oversized event frame");
            if byte[0] == b'\n' {
                break;
            }
        }
        let line = std::str::from_utf8(&line)?.trim();
        let line = if sse {
            match line.strip_prefix("data:") {
                Some(line) => line.trim(),
                None => continue,
            }
        } else {
            line
        };
        let value: Value = serde_json::from_str(line).context("invalid messenger event")?;
        if let Some(error) = value["error"].as_str() {
            bail!("{error}");
        }
        if let Some(revision) = value["revision"].as_u64()
            && Some(revision) != last
        {
            last = Some(revision);
            changed();
        }
    }
    Ok(())
}
