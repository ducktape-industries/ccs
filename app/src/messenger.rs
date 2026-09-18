//! Session-based inbox/queue viewer. Local first; remote is an explicit connection.
use futures::{SinkExt, StreamExt};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use crate::{backend, remote_store};
use ccs::messenger::{self, Kind, Labels, Message, Request};
use gpui_kit::base::TestSupportExt;
use gpui_kit::component::{
    button::*,
    input::{Input, InputState, Textarea, TextareaState},
    *,
};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use serde::Deserialize;

#[derive(Clone)]
enum Connection {
    Local(PathBuf),
    Remote { url: String, token: String },
}
impl Connection {
    fn local() -> Self {
        Self::Local(messenger::directory().unwrap_or_else(|_| PathBuf::from(".ccs/messenger")))
    }
    fn title(&self) -> String {
        match self {
            Self::Local(_) => "Local CCS".into(),
            Self::Remote { url, .. } => format!("Remote · {url}"),
        }
    }
    fn watch(&self, stop: &AtomicBool, changed: impl FnMut()) -> anyhow::Result<()> {
        match self {
            Self::Local(dir) => messenger::watch(dir, stop, changed),
            Self::Remote { url, token } => ccs::messenger_http::watch(url, token, stop, changed),
        }
    }
    fn call(&self, request: &Request) -> anyhow::Result<serde_json::Value> {
        match self {
            Self::Local(dir) => messenger::call(dir, request),
            Self::Remote { url, token } => ccs::messenger_http::call(url, token, request),
        }
    }
}

enum StreamEvent {
    Changed,
    Disconnected(String),
}
struct Subscription(Arc<AtomicBool>);
impl Drop for Subscription {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

#[derive(Clone, Deserialize)]
struct SessionRow {
    name: String,
    provider: String,
    labels: Labels,
    #[serde(default)]
    last_activity: u64,
}
#[derive(Default, Deserialize)]
struct MessagePage {
    messages: Vec<Message>,
    next_offset: Option<usize>,
}
struct Snapshot {
    sessions: Vec<SessionRow>,
    session: Option<String>,
    page: MessagePage,
}

fn recent_messages(messages: Vec<Message>, count: usize) -> (Vec<Message>, bool) {
    let unique: BTreeMap<_, _> = messages.into_iter().map(|m| (m.id.clone(), m)).collect();
    let mut messages: Vec<_> = unique.into_values().collect();
    messages.sort_by_key(|m| std::cmp::Reverse(m.sequence));
    let more = messages.len() > count;
    messages.truncate(count);
    (messages, more)
}

fn visible_sessions<'a>(
    sessions: &'a [SessionRow],
    query: &str,
    show_workers: bool,
    recent_first: bool,
) -> Vec<&'a SessionRow> {
    let query = query.trim().to_lowercase();
    let mut rows: Vec<_> = sessions
        .iter()
        .filter(|row| {
            (show_workers || row.labels.get("role").is_none_or(|role| role != "worker"))
                && (query.is_empty()
                    || row.name.to_lowercase().contains(&query)
                    || row.provider.to_lowercase().contains(&query)
                    || row.labels.values().any(|value| value.to_lowercase().contains(&query)))
        })
        .collect();
    if recent_first {
        rows.sort_by(|a, b| {
            b.last_activity.cmp(&a.last_activity).then_with(|| a.name.cmp(&b.name))
        });
    } else {
        rows.sort_by(|a, b| a.name.cmp(&b.name));
    }
    rows
}

fn load(
    connection: &Connection,
    selected: Option<String>,
    kind: Option<Kind>,
    offset: usize,
    pinned: Option<Message>,
) -> Result<Snapshot, String> {
    let load = || -> anyhow::Result<Snapshot> {
        let sessions: Vec<SessionRow> =
            serde_json::from_value(connection.call(&Request::Sessions { labels: Labels::new() })?)?;
        let session = selected.filter(|name| sessions.iter().any(|s| &s.name == name));
        let count = offset + 20;
        let mut merged = Vec::new();
        let mut cursor = 0;
        while cursor <= count {
            let request = match &session {
                Some(name) => Request::History {
                    session: name.clone(),
                    kind,
                    limit: (count + 1 - cursor).min(100),
                    offset: cursor,
                },
                None => Request::RoomHistory {
                    kind,
                    limit: (count + 1 - cursor).min(100),
                    offset: cursor,
                },
            };
            let page: MessagePage = serde_json::from_value(connection.call(&request)?)?;
            merged.extend(page.messages);
            let Some(next) = page.next_offset else { break };
            if next <= cursor {
                break;
            }
            cursor = next;
        }
        let (mut messages, more) = recent_messages(merged, count);
        let next_offset = more.then_some(offset + 20);
        if let Some(pinned) = pinned {
            let current: Message = serde_json::from_value(
                connection.call(&Request::Message { session: pinned.to, id: pinned.id })?,
            )?;
            if let Some(existing) = messages.iter_mut().find(|m| m.id == current.id) {
                *existing = current;
            } else {
                messages.push(current);
            }
        }
        // Fetch ancestors for replies whose original fell outside the recent window.
        let mut index = 0;
        while index < messages.len() && messages.len() < count + 100 {
            if let Some(parent) = messages[index].reply_to.clone()
                && !messages.iter().any(|m| m.id == parent)
            {
                let value = connection
                    .call(&Request::Message { session: messages[index].to.clone(), id: parent })?;
                messages.push(serde_json::from_value(value)?);
            }
            index += 1;
        }
        messages.sort_by_key(|m| m.sequence);
        let page = MessagePage { messages, next_offset };
        Ok(Snapshot { sessions, session, page })
    };
    load().map_err(|e| format!("{e:#}"))
}

pub struct Messenger {
    connection: Connection,
    sessions: Vec<SessionRow>,
    session: Option<String>,
    messages: Vec<Message>,
    selected: Option<String>,
    kind: Option<Kind>,
    offset: usize,
    next_offset: Option<usize>,
    scroll: ScrollHandle,
    new_messages: usize,
    thread_cache: Vec<Message>,
    scroll_anchor: Option<String>,
    revision: u64,
    subscription: Option<Subscription>,
    stream_epoch: u64,
    streaming: bool,
    refresh_pending: bool,
    stream_error: String,
    connected: bool,
    loading: bool,
    writing: bool,
    remote_form: bool,
    remote_saved: bool,
    new_remote: bool,
    unlocking: bool,
    save_password: Option<String>,
    reset_password: bool,
    reset_answer: bool,
    reset_compose: bool,
    error: String,
    note: String,
    url: Entity<InputState>,
    token: Entity<InputState>,
    password: Entity<InputState>,
    answer: Entity<TextareaState>,
    compose: Entity<TextareaState>,
    participant_query: Entity<InputState>,
    show_workers: bool,
    recent_first: bool,
    participant_width: f32,
    thread_width: f32,
}

impl Messenger {
    pub fn footer_line(&self) -> String {
        let state = if self.connected { "Connected" } else { "Offline" };
        let current = self.session.as_deref().unwrap_or("All conversations");
        format!(
            "{} · {state} · {} participants · {current}",
            self.connection.title(),
            self.sessions.len()
        )
    }

    pub fn footer_note(&self) -> String {
        if self.loading {
            "Updating conversations…".into()
        } else if !self.error.is_empty() {
            "Messenger needs attention".into()
        } else {
            self.note.clone()
        }
    }

    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self {
            connection: Connection::local(),
            sessions: vec![],
            session: None,
            messages: vec![],
            selected: None,
            kind: None,
            offset: 0,
            next_offset: None,
            scroll: ScrollHandle::new(),
            new_messages: 0,
            thread_cache: vec![],
            scroll_anchor: None,
            revision: 0,
            subscription: None,
            stream_epoch: 0,
            streaming: false,
            refresh_pending: false,
            stream_error: String::new(),
            connected: false,
            loading: false,
            writing: false,
            remote_form: false,
            remote_saved: backend::remote_path().is_ok_and(|path| path.exists()),
            new_remote: false,
            unlocking: false,
            save_password: None,
            reset_password: false,
            reset_answer: false,
            reset_compose: false,
            error: String::new(),
            note: String::new(),
            url: cx.new(|cx| InputState::new(window, cx).placeholder("http://server:4142")),
            token: cx.new(|cx| {
                InputState::new(window, cx).placeholder("Server access token").masked(true)
            }),
            password: cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("Encryption password (8+ characters)")
                    .masked(true)
            }),
            answer: cx.new(|cx| {
                TextareaState::new(window, cx).placeholder("Write a reply…").auto_grow(2, 5)
            }),
            compose: cx.new(|cx| {
                TextareaState::new(window, cx)
                    .placeholder("Message this participant…")
                    .auto_grow(2, 5)
            }),
            participant_query: cx
                .new(|cx| InputState::new(window, cx).placeholder("Find participant")),
            show_workers: false,
            recent_first: true,
            participant_width: 200.,
            thread_width: 360.,
        }
    }

    pub fn open_remote(&mut self, cx: &mut Context<Self>) {
        self.remote_form = true;
        self.refresh(cx);
        cx.notify();
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        if self.subscription.is_none() {
            self.start_watch(cx);
        }
        if self.loading || self.writing {
            return;
        }
        self.start_load(None, cx);
    }

    fn start_watch(&mut self, cx: &mut Context<Self>) {
        self.subscription.take();
        self.stream_epoch += 1;
        let epoch = self.stream_epoch;
        self.streaming = false;
        self.refresh_pending = false;
        self.stream_error.clear();
        let stop = Arc::new(AtomicBool::new(false));
        self.subscription = Some(Subscription(Arc::clone(&stop)));
        let connection = self.connection.clone();
        let (mut tx, mut rx) = futures::channel::mpsc::channel(1);
        std::thread::spawn(move || {
            while !stop.load(Ordering::Acquire) {
                let result = connection.watch(&stop, || {
                    if futures::executor::block_on(tx.send(StreamEvent::Changed)).is_err() {
                        stop.store(true, Ordering::Release);
                    }
                });
                if stop.load(Ordering::Acquire) {
                    break;
                }
                let error = result.err().map_or_else(|| "Stream closed".into(), |e| e.to_string());
                if futures::executor::block_on(tx.send(StreamEvent::Disconnected(error))).is_err() {
                    break;
                }
                for _ in 0..20 {
                    if stop.load(Ordering::Acquire) {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        });
        cx.spawn(async move |this, cx| {
            while let Some(event) = rx.next().await {
                if this
                    .update(cx, |this, cx| {
                        if this.stream_epoch != epoch {
                            return;
                        }
                        match event {
                            StreamEvent::Changed => {
                                this.streaming = true;
                                this.stream_error.clear();
                                if this.loading || this.writing {
                                    this.refresh_pending = true;
                                } else {
                                    this.start_load(None, cx);
                                }
                            }
                            StreamEvent::Disconnected(error) => {
                                this.streaming = false;
                                this.stream_error = error;
                            }
                        }
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }

    fn start_load(&mut self, candidate: Option<Connection>, cx: &mut Context<Self>) {
        self.revision += 1;
        let revision = self.revision;
        let connection = candidate.clone().unwrap_or_else(|| self.connection.clone());
        let session = if candidate.is_some() { None } else { self.session.clone() };
        let kind = if candidate.is_some() { None } else { self.kind };
        let offset = if candidate.is_some() { 0 } else { self.offset };
        let pinned = if candidate.is_some() {
            None
        } else {
            self.messages
                .iter()
                .chain(&self.thread_cache)
                .find(|m| Some(&m.id) == self.selected.as_ref())
                .cloned()
        };
        self.loading = true;
        self.error.clear();
        let task = cx
            .background_executor()
            .spawn(async move { load(&connection, session, kind, offset, pinned) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                let current = this.revision == revision;
                let switching = current && candidate.is_some() && result.is_ok();
                let save = if switching {
                    match (candidate.as_ref(), this.save_password.take()) {
                        (Some(Connection::Remote { url, token }), Some(password)) => Some((
                            remote_store::Remote { url: url.clone(), token: token.clone() },
                            password,
                        )),
                        _ => None,
                    }
                } else {
                    if current && candidate.is_some() {
                        this.save_password = None;
                    }
                    None
                };
                this.finish_load(revision, candidate, result);
                if switching {
                    this.start_watch(cx);
                }
                if let Some((remote, password)) = save {
                    let task = cx.background_executor().spawn(async move {
                        let path = backend::remote_path().map_err(|e| e.message)?;
                        remote_store::save(&path, &password, &remote).map_err(|e| e.to_string())
                    });
                    cx.spawn(async move |this, cx| {
                        let result = task.await;
                        let _ = this.update(cx, |this, cx| {
                            match result {
                                Ok(()) => {
                                    this.remote_saved = true;
                                    this.new_remote = false;
                                    this.note = "Remote connection saved securely.".into();
                                }
                                Err(error) => {
                                    this.error = format!("Connected, but could not save: {error}")
                                }
                            }
                            cx.notify();
                        });
                    })
                    .detach();
                }
                if current && this.refresh_pending && !this.writing {
                    this.refresh_pending = false;
                    this.start_load(None, cx);
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn finish_load(
        &mut self,
        revision: u64,
        candidate: Option<Connection>,
        result: Result<Snapshot, String>,
    ) {
        if revision != self.revision {
            return;
        }
        self.loading = false;
        match result {
            Ok(snapshot) => {
                if let Some(connection) = candidate {
                    self.connection = connection;
                    self.clear_selection();
                    self.kind = None;
                    self.remote_form = false;
                }
                if self.session != snapshot.session {
                    self.messages.clear();
                    self.thread_cache.clear();
                    self.scroll_anchor = None;
                    self.reset_answer = true;
                    self.selected = None;
                    self.offset = 0;
                    self.new_messages = 0;
                }
                self.sessions = snapshot.sessions;
                self.session = snapshot.session;
                let at_bottom = self.scroll.max_offset().y + self.scroll.offset().y <= px(24.);
                let latest = self.messages.iter().map(|m| m.sequence).max().unwrap_or(0);
                let added = snapshot.page.messages.iter().filter(|m| m.sequence > latest).count();
                // Keep loaded history while the room is open. A moving recent window must
                // not remove the messages a reader is looking at.
                let top = self.scroll.top_item();
                let old_roots = thread_roots(&self.messages);
                let roots: Vec<_> =
                    self.messages.iter().filter(|m| old_roots.get(&m.id) == Some(&m.id)).collect();
                let anchor = roots
                    .get(top.saturating_sub(usize::from(self.next_offset.is_some())))
                    .map(|m| m.id.clone());
                let prepending = snapshot
                    .page
                    .messages
                    .first()
                    .zip(self.messages.first())
                    .is_some_and(|(new, old)| new.sequence < old.sequence);
                if self.messages.is_empty() || (at_bottom && !prepending) {
                    self.scroll.scroll_to_bottom();
                    self.new_messages = 0;
                } else {
                    self.scroll_anchor = anchor;
                    self.new_messages += added;
                }
                let mut merged: BTreeMap<_, _> =
                    self.messages.drain(..).map(|m| (m.id.clone(), m)).collect();
                for message in snapshot.page.messages {
                    merged.insert(message.id.clone(), message);
                }
                self.messages = merged.into_values().collect();
                self.messages.sort_by_key(|m| m.sequence);
                if let Some(id) = &self.selected {
                    let mut all = self.thread_cache.clone();
                    for message in &self.messages {
                        if let Some(old) = all.iter_mut().find(|m| m.id == message.id) {
                            *old = message.clone();
                        } else {
                            all.push(message.clone());
                        }
                    }
                    let roots = thread_roots(&all);
                    let root = roots.get(id);
                    self.thread_cache =
                        all.iter().filter(|m| roots.get(&m.id) == root).cloned().collect();
                }
                self.next_offset = snapshot.page.next_offset;
                if !self
                    .messages
                    .iter()
                    .chain(&self.thread_cache)
                    .any(|m| Some(&m.id) == self.selected.as_ref())
                {
                    self.selected = None;
                }
                self.connected = true;
            }
            Err(error) => {
                if candidate.is_none() {
                    self.connected = false;
                }
                self.error = error;
            }
        }
    }

    fn clear_selection(&mut self) {
        self.reset_answer = true;
        self.session = None;
        self.messages.clear();
        self.selected = None;
        self.offset = 0;
        self.next_offset = None;
        self.new_messages = 0;
        self.thread_cache.clear();
        self.scroll_anchor = None;
        self.note.clear();
    }

    fn choose_session(
        &mut self,
        name: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.clear_selection();
        self.session = name;
        self.answer.update(cx, |input, cx| input.set_value("", window, cx));
        self.start_load(None, cx);
    }

    fn connect_remote(&mut self, cx: &mut Context<Self>) {
        let url = self.url.read(cx).value().trim().to_string();
        let token = self.token.read(cx).value().trim().to_string();
        let password = self.password.read(cx).value().to_string();
        if url.is_empty() || token.is_empty() || password.len() < 8 {
            self.error = "Enter the HTTP address, access token, and an encryption password of at least 8 characters.".into();
            cx.notify();
            return;
        }
        self.save_password = Some(password);
        self.reset_password = true;
        self.start_load(Some(Connection::Remote { url, token }), cx);
    }

    fn unlock_remote(&mut self, cx: &mut Context<Self>) {
        let password = self.password.read(cx).value().to_string();
        if password.is_empty() || self.unlocking {
            return;
        }
        let path = match backend::remote_path() {
            Ok(path) => path,
            Err(error) => {
                self.error = error.message;
                cx.notify();
                return;
            }
        };
        self.unlocking = true;
        self.error.clear();
        let task = cx.background_executor().spawn(async move {
            remote_store::open(&path, &password).map_err(|e| format!("{e:#}"))
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.unlocking = false;
                match result {
                    Ok(remote) => {
                        this.reset_password = true;
                        this.start_load(
                            Some(Connection::Remote { url: remote.url, token: remote.token }),
                            cx,
                        );
                    }
                    Err(error) => this.error = error,
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn act(&mut self, reply: bool, cx: &mut Context<Self>) {
        if self.writing || self.loading || !self.connected {
            return;
        }
        let Some(id) = self.selected.clone() else {
            return;
        };
        let Some(message) = self.messages.iter().chain(&self.thread_cache).find(|m| m.id == id)
        else {
            return;
        };
        let session = message.to.clone();
        if (reply && message.reply.is_some()) || (!reply && message.kind != Kind::Inbox) {
            return;
        }
        let body = self.answer.read(cx).value().to_string();
        if reply && body.trim().is_empty() {
            self.error = "Write a reply first.".into();
            cx.notify();
            return;
        }
        let request =
            if reply { Request::Reply { session, id, body } } else { Request::Ack { session, id } };
        let connection = self.connection.clone();
        let revision = self.revision;
        self.writing = true;
        self.error.clear();
        let task = cx
            .background_executor()
            .spawn(async move { connection.call(&request).map_err(|e| format!("{e:#}")) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.revision != revision {
                    return;
                }
                this.writing = false;
                match result {
                    Ok(_) => {
                        this.note = if reply { "Reply sent." } else { "Marked as read." }.into();
                        this.refresh_pending = false;
                        this.reset_answer = reply;
                        this.refresh(cx);
                    }
                    Err(error) => {
                        this.error =
                            format!("{error}. Refresh to check the result before retrying.");
                        this.refresh_pending = false;
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn post(&mut self, cx: &mut Context<Self>) {
        if self.writing || self.loading || !self.connected {
            return;
        }
        let Some(to) = self.session.clone() else {
            return;
        };
        let body = self.compose.read(cx).value().trim().to_string();
        if body.is_empty() {
            self.error = "Write a message first.".into();
            cx.notify();
            return;
        }
        let mut bytes = [0u8; 16];
        if ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut bytes).is_err() {
            self.error = "Could not create a message ID.".into();
            cx.notify();
            return;
        }
        let id = format!("u{}", bytes.iter().map(|byte| format!("{byte:02x}")).collect::<String>());
        let connection = self.connection.clone();
        let revision = self.revision;
        self.writing = true;
        self.error.clear();
        let task = cx.background_executor().spawn(async move {
            connection.call(&Request::Post { id, to, body }).map_err(|e| format!("{e:#}"))
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.revision != revision {
                    return;
                }
                this.writing = false;
                match result {
                    Ok(_) => {
                        this.reset_compose = true;
                        this.note = "Message sent.".into();
                        this.refresh(cx);
                    }
                    Err(error) => {
                        this.error = format!("{error}. Check the conversation before retrying.")
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn connection_header(&self, cx: &mut Context<Self>) -> Div {
        let remote = matches!(self.connection, Connection::Remote { .. });
        let state = if self.loading {
            "Updating…"
        } else if self.streaming {
            "● Live"
        } else if self.connected {
            "Connected"
        } else {
            "Offline"
        };
        let mut header =
            div().v_flex().gap_2().pb_3().border_b_1().border_color(rgb(0xe5e7eb)).child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(div().font_semibold().child(self.connection.title()))
                    .child(
                        div()
                            .text_xs()
                            .text_color(if self.streaming { rgb(0x067647) } else { rgb(0x737373) })
                            .child(state),
                    )
                    .child(div().flex_1())
                    .when(remote, |row| {
                        row.child(
                            Button::new("back-local-ccs")
                                .small()
                                .ghost()
                                .label("Back to local")
                                .disabled(self.writing)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.connection = Connection::local();
                                    this.start_watch(cx);
                                    this.clear_selection();
                                    this.sessions.clear();
                                    this.kind = None;
                                    this.connected = false;
                                    this.answer
                                        .update(cx, |input, cx| input.set_value("", window, cx));
                                    this.start_load(None, cx);
                                })),
                        )
                    })
                    .child(
                        Button::new("view-remote-ccs")
                            .small()
                            .ghost()
                            .label(if remote { "Change server" } else { "View remote CCS →" })
                            .disabled(self.loading || self.writing)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.remote_form = !this.remote_form;
                                cx.notify();
                            })),
                    ),
            );
        if !self.stream_error.is_empty() {
            header = header
                .child(crate::muted(format!("Reconnecting… {}", self.stream_error)).text_xs());
        }
        if self.remote_form {
            let mut form = div()
                .id("remote-ccs-form")
                .test_support()
                .v_flex()
                .gap_2()
                .p_4()
                .rounded_md()
                .bg(rgb(0xf6f8fc))
                .child(div().font_semibold().child(if self.remote_saved && !self.new_remote {
                    "Unlock saved Remote CCS"
                } else {
                    "Connect to Remote CCS"
                }))
                .child(
                    crate::muted(if self.remote_saved && !self.new_remote {
                        "Enter the password used to encrypt this connection."
                    } else {
                        "The address and access token will be encrypted on this device."
                    })
                    .text_xs(),
                );
            if self.remote_saved && !self.new_remote {
                form = form.child(Input::new(&self.password).id("remote-password")).child(
                    div()
                        .flex()
                        .gap_2()
                        .child(
                            Button::new("unlock-remote-ccs")
                                .label(if self.unlocking {
                                    "Unlocking…"
                                } else {
                                    "Unlock & connect"
                                })
                                .disabled(self.unlocking || self.loading || self.writing)
                                .on_click(cx.listener(|this, _, _, cx| this.unlock_remote(cx))),
                        )
                        .child(
                            Button::new("different-remote-ccs")
                                .ghost()
                                .label("Use another server")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.new_remote = true;
                                    cx.notify();
                                })),
                        ),
                );
            } else {
                form = form
                    .child(crate::muted("Server URL").text_xs())
                    .child(
                        Input::new(&self.url)
                            .id("remote-url")
                            .disabled(self.loading || self.writing),
                    )
                    .child(crate::muted("Access token").text_xs())
                    .child(
                        Input::new(&self.token)
                            .id("remote-token")
                            .disabled(self.loading || self.writing),
                    )
                    .child(crate::muted("Encryption password").text_xs())
                    .child(
                        Input::new(&self.password)
                            .id("remote-password")
                            .disabled(self.loading || self.writing),
                    )
                    .child(
                        Button::new("connect-remote-ccs")
                            .label("Connect & save encrypted")
                            .disabled(self.loading || self.writing)
                            .on_click(cx.listener(|this, _, _, cx| this.connect_remote(cx))),
                    )
                    .child(crate::muted("8+ characters · needed again after restart").text_xs());
            }
            header = header.child(form);
        }
        header
    }
}

fn status_text(message: &Message) -> &'static str {
    match message.status.as_str() {
        "pending" => "Unread",
        "dispatching" => "Delivery unconfirmed",
        "submitted" => "Awaiting reply",
        "failed" => "Delivery failed",
        "answered" => "Answered",
        "read" => "Read",
        _ => "Unknown",
    }
}
fn status_color(message: &Message) -> Rgba {
    match message.status.as_str() {
        "failed" => rgb(0xb42318),
        "answered" | "read" => rgb(0x067647),
        _ => rgb(0x946200),
    }
}
fn thread_roots(messages: &[Message]) -> HashMap<String, String> {
    let by_id: HashMap<_, _> = messages.iter().map(|m| (m.id.as_str(), m)).collect();
    let mut roots: HashMap<String, String> = HashMap::with_capacity(messages.len());
    for message in messages {
        let mut current = message.id.as_str();
        for _ in 0..messages.len() {
            if let Some(root) = roots.get(current) {
                current = root.as_str();
                break;
            }
            match by_id.get(current).and_then(|m| m.reply_to.as_deref()) {
                Some(parent) if by_id.contains_key(parent) => current = parent,
                _ => break,
            }
        }
        roots.insert(message.id.clone(), current.to_string());
    }
    roots
}

fn message_time(message: &Message) -> String {
    jiff::Timestamp::from_millisecond(message.created_ms)
        .map(|t| t.to_zoned(jiff::tz::TimeZone::system()).strftime("%H:%M").to_string())
        .unwrap_or_default()
}

fn avatar(name: &str) -> Div {
    div()
        .size(px(30.))
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .rounded_md()
        .bg(rgb(0xe9edf7))
        .text_color(rgb(0x425a8b))
        .text_xs()
        .font_semibold()
        .child(
            name.trim_start_matches("ducktape-").chars().take(2).collect::<String>().to_uppercase(),
        )
}

impl Render for Messenger {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.reset_answer {
            self.answer.update(cx, |input, cx| input.set_value("", window, cx));
            self.reset_answer = false;
        }
        if self.reset_compose {
            self.compose.update(cx, |input, cx| input.set_value("", window, cx));
            self.reset_compose = false;
        }
        if self.reset_password {
            self.password.update(cx, |input, cx| input.set_value("", window, cx));
            self.reset_password = false;
        }
        let height = (window.viewport_size().height - px(160.)).max(px(340.));
        let narrow = window.viewport_size().width < px(820.);
        let mut content = div().v_flex().h(height).gap_3().child(self.connection_header(cx));
        if !self.error.is_empty() {
            content = content.child(
                div()
                    .id("messenger-error")
                    .p_2()
                    .rounded_md()
                    .bg(rgb(0xfff4ed))
                    .text_color(rgb(0x9a3412))
                    .child(self.error.clone()),
            );
        }
        if self.sessions.is_empty() {
            return content.child(div().id("messenger-empty").v_flex().gap_2().py_6()
                .child(div().font_semibold().child(if self.connected { "No participants yet" } else { "Local CCS is offline" }))
                .child(crate::muted("Registered sessions appear here. Connect your local CCS or view a remote server.")));
        }
        let mut all = self.messages.clone();
        let mut seen: HashSet<_> = all.iter().map(|m| m.id.clone()).collect();
        for message in &self.thread_cache {
            if seen.insert(message.id.clone()) {
                all.push(message.clone());
            }
        }
        all.sort_by_key(|m| m.sequence);
        let query = self.participant_query.read(cx).value().to_string();
        let visible =
            visible_sessions(&self.sessions, &query, self.show_workers, self.recent_first);
        let worker_count = self
            .sessions
            .iter()
            .filter(|s| s.labels.get("role").is_some_and(|role| role == "worker"))
            .count();
        let roots = thread_roots(&all);
        let mut replies_by_root = HashMap::<&str, usize>::new();
        for message in &all {
            if let Some(root) = roots.get(&message.id)
                && root != &message.id
            {
                *replies_by_root.entry(root).or_default() += 1;
            }
        }
        let selected =
            self.selected.as_ref().and_then(|id| all.iter().find(|m| &m.id == id)).cloned();
        let mut room = div().flex_1().min_w_0().v_flex().gap_2();
        room =
            room.child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(div().font_semibold().child(
                        self.session.clone().unwrap_or_else(|| "# All conversations".into()),
                    ))
                    .child(
                        Button::new("room-all")
                            .small()
                            .ghost()
                            .label("All participants")
                            .disabled(self.writing)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.choose_session(None, window, cx)
                            })),
                    ),
            );
        if narrow {
            room = room.child(
                div()
                    .flex()
                    .gap_1()
                    .items_center()
                    .child(Input::new(&self.participant_query).id("participant-filter-narrow"))
                    .child(
                        Button::new("participants-sort-narrow")
                            .small()
                            .ghost()
                            .label(if self.recent_first { "Recent ↓" } else { "Name ↑" })
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.recent_first = !this.recent_first;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("participants-workers-narrow")
                            .small()
                            .ghost()
                            .label(format!("Workers {worker_count}"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.show_workers = !this.show_workers;
                                cx.notify();
                            })),
                    ),
            );
            let mut people = div().id("participant-strip").flex().gap_1().overflow_x_scroll();
            for session in &visible {
                let name = session.name.clone();
                people = people.child(
                    Button::new(SharedString::from(format!("session-{name}")))
                        .small()
                        .ghost()
                        .label(name.clone())
                        .selected(self.session.as_ref() == Some(&name))
                        .disabled(self.writing)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.choose_session(Some(name.clone()), window, cx)
                        })),
                );
            }
            room = room.child(people);
        }
        if let Some(id) = self.scroll_anchor.take()
            && let Some(index) = self
                .messages
                .iter()
                .filter(|m| roots.get(&m.id) == Some(&m.id))
                .position(|m| m.id == id)
        {
            let index = index + usize::from(self.next_offset.is_some());
            if index != self.scroll.top_item() {
                self.scroll.scroll_to_top_of_item(index);
            }
        }
        let mut timeline = div()
            .id("message-list")
            .test_support()
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .v_flex()
            .gap_1();
        if self.next_offset.is_some() {
            timeline = timeline.child(
                Button::new("messages-next")
                    .small()
                    .ghost()
                    .label("Load earlier messages")
                    .disabled(self.loading || self.writing)
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(offset) = this.next_offset {
                            this.offset = offset;
                            this.start_load(None, cx);
                        }
                    })),
            );
        }
        let selected_root = selected.as_ref().and_then(|m| roots.get(&m.id));
        for message in self.messages.iter().filter(|m| roots.get(&m.id) == Some(&m.id)) {
            let id = message.id.clone();
            let descendants = replies_by_root.get(id.as_str()).copied().unwrap_or(0);
            let replies = descendants + usize::from(message.reply.is_some() && descendants == 0);
            let preview: String = message.body.chars().take(420).collect();
            let preview =
                if preview.len() < message.body.len() { format!("{preview}…") } else { preview };
            timeline = timeline.child(
                div()
                    .id(SharedString::from(format!("message-{id}")))
                    .test_support()
                    .flex()
                    .gap_3()
                    .p_3()
                    .rounded_md()
                    .cursor_pointer()
                    .when(selected_root == Some(&id), |d| d.bg(rgb(0xf0f4fc)))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if this.writing {
                            return;
                        }
                        if this.selected.as_ref() != Some(&id) {
                            this.answer.update(cx, |input, cx| input.set_value("", window, cx));
                        }
                        this.selected = Some(id.clone());
                        cx.notify();
                    }))
                    .child(avatar(&message.from))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .v_flex()
                            .gap_1()
                            .child(
                                div()
                                    .flex()
                                    .gap_2()
                                    .items_center()
                                    .child(div().font_semibold().child(message.from.clone()))
                                    .child(
                                        crate::muted(format!(
                                            "→ {} · {}",
                                            message.to,
                                            message_time(message)
                                        ))
                                        .text_xs(),
                                    ),
                            )
                            .child(div().child(preview))
                            .child(
                                div()
                                    .flex()
                                    .gap_2()
                                    .items_center()
                                    .child(
                                        crate::muted(if message.kind == Kind::Queue {
                                            "Queue"
                                        } else {
                                            "Inbox"
                                        })
                                        .text_xs(),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(status_color(message))
                                            .child(status_text(message)),
                                    )
                                    .child(div().text_xs().text_color(rgb(0x4169a5)).child(
                                        if replies > 0 {
                                            if replies == 1 {
                                                "1 reply →".into()
                                            } else {
                                                format!("{replies} replies →")
                                            }
                                        } else {
                                            "Open thread →".into()
                                        },
                                    )),
                            ),
                    ),
            );
        }
        if self.messages.is_empty() {
            timeline = timeline.child(div().p_6().child(crate::muted("No conversations yet.")));
        }
        room = room.child(timeline);
        if self.new_messages > 0 {
            room = room.child(
                Button::new("new-messages")
                    .small()
                    .label(format!("New messages ↓ · {}", self.new_messages))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.scroll.scroll_to_bottom();
                        this.new_messages = 0;
                        cx.notify();
                    })),
            );
        }
        if let Some(target) = &self.session {
            room = room.child(
                div()
                    .id("new-message")
                    .v_flex()
                    .gap_2()
                    .p_2()
                    .rounded_md()
                    .bg(rgb(0xf6f8fc))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(crate::muted(format!("You → {target}")).text_xs())
                            .child(
                                Button::new("send-new-message")
                                    .small()
                                    .label("Send message")
                                    .disabled(self.loading || self.writing || !self.connected)
                                    .on_click(cx.listener(|this, _, _, cx| this.post(cx))),
                            ),
                    )
                    .child(Textarea::new(&self.compose).disabled(self.writing)),
            );
        } else {
            room = room.child(
                crate::muted("Choose a participant to send a message, or open a thread to reply.")
                    .text_xs(),
            );
        }
        let mut layout = div().flex().flex_1().min_h_0().gap_4();
        if !narrow || selected.is_none() {
            layout = layout.child(room);
        }
        if let Some(message) = selected {
            let root = roots.get(&message.id);
            let mut thread = div()
                .id("message-detail")
                .v_flex()
                .gap_3()
                .min_h_0()
                .when(!narrow, |d| {
                    d.w(px(self.thread_width))
                        .flex_shrink_0()
                        .pl_4()
                        .border_l_1()
                        .border_color(rgb(0xe5e7eb))
                })
                .when(narrow, |d| d.flex_1())
                .child(
                    div()
                        .flex()
                        .justify_between()
                        .items_center()
                        .child(div().font_semibold().child("Thread"))
                        .child(
                            div()
                                .flex()
                                .gap_1()
                                .child(
                                    Button::new("thread-narrower")
                                        .small()
                                        .ghost()
                                        .label("−")
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.thread_width = (this.thread_width - 40.).max(280.);
                                            cx.notify();
                                        })),
                                )
                                .child(
                                    Button::new("thread-wider")
                                        .small()
                                        .ghost()
                                        .label("+")
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.thread_width = (this.thread_width + 40.).min(560.);
                                            cx.notify();
                                        })),
                                ),
                        )
                        .child(
                            Button::new("close-thread")
                                .small()
                                .ghost()
                                .label("Close")
                                .disabled(self.writing)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.selected = None;
                                    this.thread_cache.clear();
                                    cx.notify();
                                })),
                        ),
                );
            let mut posts =
                div().id("thread-posts").flex_1().min_h_0().overflow_y_scroll().v_flex().gap_4();
            for post in all.iter().filter(|m| roots.get(&m.id) == root) {
                let id = post.id.clone();
                let mut entry = div()
                    .v_flex()
                    .gap_2()
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .items_center()
                            .child(avatar(&post.from))
                            .child(div().font_semibold().child(post.from.clone()))
                            .child(crate::muted(message_time(post)).text_xs()),
                    )
                    .child(crate::muted(format!("To {}", post.to)).text_xs())
                    .child(
                        div()
                            .id(if root == Some(&post.id) {
                                SharedString::from("message-body")
                            } else {
                                SharedString::from(format!("thread-body-{}", post.id))
                            })
                            .test_support()
                            .child(post.body.clone()),
                    );
                if let Some(error) = &post.error {
                    entry = entry.child(div().text_color(rgb(0xb42318)).child(error.clone()));
                }
                if let Some(reply) = &post.reply
                    && !all.iter().any(|m| m.reply_to.as_ref() == Some(&post.id))
                {
                    entry = entry.child(
                        div()
                            .pl_3()
                            .border_l_2()
                            .border_color(rgb(0xdce6f6))
                            .v_flex()
                            .gap_1()
                            .child(div().font_semibold().child(post.to.clone()))
                            .child(reply.clone()),
                    );
                }
                if post.reply.is_none() && post.id != message.id {
                    entry = entry.child(
                        Button::new(SharedString::from(format!("reply-to-{}", post.id)))
                            .small()
                            .ghost()
                            .label("Reply here")
                            .disabled(self.writing)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.selected = Some(id.clone());
                                this.answer.update(cx, |input, cx| input.set_value("", window, cx));
                                cx.notify();
                            })),
                    );
                }
                posts = posts.child(entry);
            }
            thread = thread.child(posts);
            if message.kind == Kind::Inbox && message.status == "pending" {
                thread = thread.child(
                    Button::new("message-ack")
                        .small()
                        .ghost()
                        .label("Mark as read")
                        .disabled(self.loading || self.writing || !self.connected)
                        .on_click(cx.listener(|this, _, _, cx| this.act(false, cx))),
                );
            }
            if message.reply.is_none() {
                thread = thread
                    .child(
                        crate::muted(format!("Reply as {} → {}", message.to, message.from))
                            .text_xs(),
                    )
                    .child(
                        div()
                            .id("message-answer")
                            .child(Textarea::new(&self.answer).disabled(self.writing)),
                    )
                    .child(
                        Button::new("message-reply")
                            .label(if self.writing { "Sending…" } else { "Send reply" })
                            .disabled(self.loading || self.writing || !self.connected)
                            .on_click(cx.listener(|this, _, _, cx| this.act(true, cx))),
                    );
            }
            if !self.note.is_empty() {
                thread = thread.child(crate::muted(self.note.clone()).text_xs());
            }
            layout = layout.child(thread);
        } else if !narrow {
            let people = div()
                .w(px(self.participant_width))
                .flex_shrink_0()
                .v_flex()
                .gap_1()
                .pl_4()
                .border_l_1()
                .border_color(rgb(0xe5e7eb))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .child(
                            crate::muted(format!("PARTICIPANTS · {}", self.sessions.len()))
                                .text_xs(),
                        )
                        .child(
                            div()
                                .flex()
                                .gap_1()
                                .child(
                                    Button::new("participants-narrower")
                                        .small()
                                        .ghost()
                                        .label("−")
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.participant_width =
                                                (this.participant_width - 40.).max(160.);
                                            cx.notify();
                                        })),
                                )
                                .child(
                                    Button::new("participants-wider")
                                        .small()
                                        .ghost()
                                        .label("+")
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.participant_width =
                                                (this.participant_width + 40.).min(400.);
                                            cx.notify();
                                        })),
                                ),
                        ),
                )
                .child(Input::new(&self.participant_query).id("participant-filter"))
                .child(
                    div()
                        .flex()
                        .gap_1()
                        .child(
                            Button::new("participants-sort")
                                .small()
                                .ghost()
                                .label(if self.recent_first { "Recent ↓" } else { "Name ↑" })
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.recent_first = !this.recent_first;
                                    cx.notify();
                                })),
                        )
                        .child(
                            Button::new("participants-workers")
                                .small()
                                .ghost()
                                .label(if self.show_workers {
                                    format!("Workers {worker_count} ✓")
                                } else {
                                    format!("Workers {worker_count}")
                                })
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.show_workers = !this.show_workers;
                                    cx.notify();
                                })),
                        ),
                )
                .child(crate::muted(format!("{} shown", visible.len())).text_xs());
            let mut rows = div()
                .id("participant-list")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .v_flex()
                .gap_1();
            for session in &visible {
                let name = session.name.clone();
                rows = rows.child(
                    div()
                        .flex()
                        .items_center()
                        .child(
                            Button::new(SharedString::from(format!("session-{name}")))
                                .small()
                                .ghost()
                                .label(name.clone())
                                .selected(self.session.as_ref() == Some(&name))
                                .disabled(self.writing)
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.choose_session(Some(name.clone()), window, cx)
                                })),
                        )
                        .child(crate::muted(session.provider.clone()).text_xs()),
                );
            }
            layout = layout.child(people.child(rows));
        }
        content.child(layout)
    }
}

#[cfg(test)]
mod tests {
    use super::{Connection, MessagePage, Messenger, SessionRow, Snapshot};
    use ccs::messenger::{Kind, Message};
    use gpui_kit::component::Root;
    use gpui_kit::test::TestWindowExt;
    use gpui_kit::{AppContext, TestAppContext};

    fn snapshot(name: &str) -> Snapshot {
        Snapshot {
            sessions: vec![SessionRow {
                name: name.into(),
                provider: "codex".into(),
                labels: [("role".into(), "manager".into())].into(),
                last_activity: 3,
            }],
            session: Some(name.into()),
            page: MessagePage {
                messages: vec![Message {
                    id: "m1".into(),
                    sequence: 1,
                    from: "peer".into(),
                    to: name.into(),
                    kind: Kind::Inbox,
                    body: "Should we use the new API?".into(),
                    created_ms: 1,
                    status: "pending".into(),
                    reply: None,
                    reply_to: None,
                    error: None,
                }],
                next_offset: None,
            },
        }
    }

    #[gpui_kit::test]
    fn local_first_remote_form_is_opt_in_and_message_details_show_reply(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let mut entity = None;
        let handle = cx.add_window(|window, cx| {
            let view = cx.new(|cx| {
                let mut view = Messenger::new(window, cx);
                view.finish_load(0, None, Ok(snapshot("local-session")));
                view
            });
            entity = Some(view.clone());
            Root::new(view, window, cx)
        });
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("remote-ccs-form").is_none());
            window.click("message-m1", cx);
            assert!(window.try_find("message-body").is_some());
            assert!(window.try_find("message-reply").is_some());
            window.click("view-remote-ccs", cx);
            assert!(window.try_find("remote-ccs-form").is_some());
        })
        .unwrap();
        entity.unwrap().update(cx, |view, _| {
            assert!(matches!(view.connection, Connection::Local(_)));
            assert_eq!(view.session.as_deref(), Some("local-session"));
        });
    }

    #[test]
    fn room_history_deduplicates_shared_deliveries_before_pagination() {
        let first = snapshot("local").page.messages.remove(0);
        let mut second = first.clone();
        second.id = "m2".into();
        second.sequence = 2;
        let mut third = first.clone();
        third.id = "m3".into();
        third.sequence = 3;
        let (page, more) = super::recent_messages(
            vec![first.clone(), second.clone(), third.clone(), first, third],
            2,
        );
        assert_eq!(page.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["m3", "m2"]);
        assert!(more);
        let (page, more) = super::recent_messages(vec![second.clone(), second], 2);
        assert_eq!(page.len(), 1);
        assert!(!more, "duplicate sender/recipient history is not another page");
    }

    #[test]
    fn participants_hide_workers_and_sort_by_activity_or_name() {
        let mut rows = snapshot("z-manager").sessions;
        let mut worker = rows[0].clone();
        worker.name = "a-worker".into();
        worker.labels.insert("role".into(), "worker".into());
        worker.last_activity = 10;
        let mut chief = rows[0].clone();
        chief.name = "b-chief".into();
        chief.last_activity = 5;
        rows.extend([worker, chief]);
        assert_eq!(
            super::visible_sessions(&rows, "", false, true)
                .iter()
                .map(|r| r.name.as_str())
                .collect::<Vec<_>>(),
            ["b-chief", "z-manager"]
        );
        assert_eq!(
            super::visible_sessions(&rows, "", true, false)
                .iter()
                .map(|r| r.name.as_str())
                .collect::<Vec<_>>(),
            ["a-worker", "b-chief", "z-manager"]
        );
        assert_eq!(super::visible_sessions(&rows, "worker", true, true).len(), 1);
    }

    #[gpui_kit::test]
    fn participant_controls_reveal_workers_and_resize_sidebar(cx: &mut TestAppContext) {
        use gpui_kit::{px, size};
        cx.update(gpui_kit::init);
        let mut entity = None;
        let handle = cx.add_window(|window, cx| {
            let view = cx.new(|cx| {
                let mut view = Messenger::new(window, cx);
                let mut data = snapshot("manager");
                data.session = None;
                let mut worker = data.sessions[0].clone();
                worker.name = "worker".into();
                worker.labels.insert("role".into(), "worker".into());
                data.sessions.push(worker);
                view.finish_load(0, None, Ok(data));
                view
            });
            entity = Some(view.clone());
            Root::new(view, window, cx)
        });
        cx.simulate_window_resize(handle.into(), size(px(1100.), px(800.)));
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("session-worker").is_none());
            window.click("participants-workers", cx);
            window.render_frame(cx);
            assert!(window.try_find("session-worker").is_some());
            window.click("participants-wider", cx);
            entity.as_ref().unwrap().update(cx, |view, _| assert_eq!(view.participant_width, 240.));
        })
        .unwrap();
    }

    #[test]
    fn reply_chains_share_a_root_without_grouping_unrelated_messages() {
        let mut messages = snapshot("local").page.messages;
        let mut reply = messages[0].clone();
        reply.id = "r1".into();
        reply.reply_to = Some("m1".into());
        reply.sequence = 2;
        let mut followup = reply.clone();
        followup.id = "r2".into();
        followup.reply_to = Some("r1".into());
        followup.sequence = 3;
        let mut unrelated = messages[0].clone();
        unrelated.id = "other".into();
        messages.extend([reply, followup, unrelated]);
        let roots = super::thread_roots(&messages);
        assert_eq!(roots["r2"], "m1");
        assert_eq!(roots["other"], "other");
        assert!(!roots.contains_key("missing"));
    }

    #[gpui_kit::test]
    fn live_updates_keep_thread_and_draft_and_narrow_view_returns_to_room(cx: &mut TestAppContext) {
        use gpui_kit::{px, size};
        cx.update(gpui_kit::init);
        let mut entity = None;
        let handle = cx.add_window(|window, cx| {
            let view = cx.new(|cx| {
                let mut view = Messenger::new(window, cx);
                view.finish_load(0, None, Ok(snapshot("local")));
                view
            });
            entity = Some(view.clone());
            Root::new(view, window, cx)
        });
        cx.simulate_window_resize(handle.into(), size(px(1100.), px(800.)));
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            window.click("message-m1", cx);
            entity.as_ref().unwrap().update(cx, |view, cx| {
                view.answer.update(cx, |input, cx| input.set_value("draft stays", window, cx));
                let mut fresh = snapshot("local");
                let mut another = fresh.page.messages[0].clone();
                another.id = "m2".into();
                another.sequence = 2;
                fresh.page.messages.push(another);
                view.finish_load(0, None, Ok(fresh));
                assert_eq!(view.selected.as_deref(), Some("m1"));
                assert_eq!(view.answer.read(cx).value().as_str(), "draft stays");
            });
            window.render_frame(cx);
            assert!(window.try_find("message-list").is_some());
            assert!(window.try_find("message-body").is_some());
        })
        .unwrap();
        cx.simulate_window_resize(handle.into(), size(px(780.), px(800.)));
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("message-list").is_none());
            assert!(window.try_find("message-body").is_some());
            window.click("close-thread", cx);
            assert!(window.try_find("message-list").is_some());
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn repeated_refresh_preserves_history_thread_and_viewport(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let mut entity = None;
        let handle = cx.add_window(|window, cx| {
            let view = cx.new(|cx| {
                let mut view = Messenger::new(window, cx);
                let mut initial = snapshot("local");
                let template = initial.page.messages[0].clone();
                initial.page.messages = (10..40)
                    .map(|sequence| {
                        let mut m = template.clone();
                        m.id = format!("m{sequence}");
                        m.sequence = sequence;
                        m
                    })
                    .collect();
                initial.page.next_offset = Some(20);
                view.finish_load(0, None, Ok(initial));
                view
            });
            entity = Some(view.clone());
            Root::new(view, window, cx)
        });
        let view = entity.unwrap();
        cx.simulate_window_resize(
            handle.into(),
            gpui_kit::size(gpui_kit::px(1100.), gpui_kit::px(620.)),
        );
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            view.update(cx, |v, _| v.scroll.scroll_to_top_of_item(5));
            window.render_frame(cx);
            view.update(cx, |v, _| {
                let mut offset = v.scroll.offset();
                offset.y -= gpui_kit::px(7.);
                v.scroll.set_offset(offset);
            });
            window.render_frame(cx);
        })
        .unwrap();
        let before = view.read_with(cx, |v, _| v.scroll.logical_scroll_top());
        assert_eq!(before.0, 5);
        for sequence in [40, 41, 9] {
            view.update(cx, |v, cx| {
                let mut fresh = snapshot("local");
                fresh.page.messages[0].id = format!("m{sequence}");
                fresh.page.messages[0].sequence = sequence;
                fresh.page.next_offset = Some(20);
                v.finish_load(0, None, Ok(fresh));
                assert!(v.messages.iter().any(|m| m.id == "m10"));
                cx.notify();
            });
            cx.run_until_parked();
            cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx)).unwrap();
            cx.run_until_parked();
            cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx)).unwrap();
            cx.run_until_parked();
            view.read_with(cx, |v, _| {
                let (index, within) = v.scroll.logical_scroll_top();
                assert_eq!(
                    v.messages[index - 1].id,
                    "m14",
                    "sequence={sequence}, before={before:?}, after={:?}",
                    v.scroll.logical_scroll_top()
                );
                if sequence == 9 {
                    assert_eq!(within, gpui_kit::px(0.));
                } else {
                    assert!((within - before.1).abs() < gpui_kit::px(1.));
                }
            });
        }
        view.update(cx, |v, _| {
            v.selected = Some("m10".into());
            let mut reply = v.messages.iter().find(|m| m.id == "m10").unwrap().clone();
            reply.id = "reply".into();
            reply.reply_to = Some("m10".into());
            v.thread_cache.push(reply);
            for _ in 0..3 {
                v.finish_load(0, None, Ok(snapshot("local")));
                assert!(v.thread_cache.iter().any(|m| m.id == "reply"));
            }
        });
    }

    #[gpui_kit::test]
    fn failed_remote_connect_preserves_local_and_late_results_cannot_cross_servers(
        cx: &mut TestAppContext,
    ) {
        cx.update(gpui_kit::init);
        let mut entity = None;
        let handle = cx.add_window(|window, cx| {
            let view = cx.new(|cx| {
                let mut view = Messenger::new(window, cx);
                view.finish_load(0, None, Ok(snapshot("local")));
                view.answer.update(cx, |input, cx| input.set_value("local draft", window, cx));
                view.revision = 2;
                let remote =
                    Connection::Remote { url: "http://remote:4142".into(), token: "secret".into() };
                view.finish_load(2, Some(remote.clone()), Err("unauthorized".into()));
                assert!(matches!(view.connection, Connection::Local(_)));
                assert_eq!(view.session.as_deref(), Some("local"));
                view.finish_load(2, Some(remote), Ok(snapshot("remote")));
                view.finish_load(1, None, Ok(snapshot("late-local")));
                assert_eq!(view.session.as_deref(), Some("remote"));
                assert_eq!(view.messages[0].to, "remote");
                assert!(view.selected.is_none());
                view
            });
            entity = Some(view.clone());
            Root::new(view, window, cx)
        });
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("back-local-ccs").is_some());
            entity.as_ref().unwrap().update(cx, |view, cx| {
                assert!(view.answer.read(cx).value().is_empty());
            });
        })
        .unwrap();
    }
}
