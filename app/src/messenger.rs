//! Session-based inbox/queue viewer. Local first; remote is an explicit connection.
use futures::{SinkExt, StreamExt};
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

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

fn load(
    connection: &Connection,
    selected: Option<String>,
    kind: Option<Kind>,
    offset: usize,
) -> Result<Snapshot, String> {
    let load = || -> anyhow::Result<Snapshot> {
        let sessions: Vec<SessionRow> =
            serde_json::from_value(connection.call(&Request::Sessions { labels: Labels::new() })?)?;
        let original = selected.clone();
        let session = selected
            .filter(|name| sessions.iter().any(|s| &s.name == name))
            .or_else(|| sessions.first().map(|s| s.name.clone()));
        let offset = if session == original { offset } else { 0 };
        let page = if let Some(name) = &session {
            serde_json::from_value(connection.call(&Request::History {
                session: name.clone(),
                kind,
                limit: 20,
                offset,
            })?)?
        } else {
            MessagePage::default()
        };
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
    previous: Vec<usize>,
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
    reset_answer: bool,
    error: String,
    note: String,
    url: Entity<InputState>,
    token: Entity<InputState>,
    answer: Entity<TextareaState>,
}

impl Messenger {
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
            previous: vec![],
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
            reset_answer: false,
            error: String::new(),
            note: String::new(),
            url: cx.new(|cx| InputState::new(window, cx).placeholder("http://server:4142")),
            token: cx.new(|cx| {
                InputState::new(window, cx).placeholder("Server access token").masked(true)
            }),
            answer: cx.new(|cx| {
                TextareaState::new(window, cx).placeholder("Write a reply…").auto_grow(2, 5)
            }),
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
        self.loading = true;
        self.error.clear();
        let task =
            cx.background_executor().spawn(async move { load(&connection, session, kind, offset) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                let current = this.revision == revision;
                let switching = current && candidate.is_some() && result.is_ok();
                this.finish_load(revision, candidate, result);
                if switching {
                    this.start_watch(cx);
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
                    self.reset_answer = true;
                    self.selected = None;
                    self.offset = 0;
                    self.previous.clear();
                }
                self.sessions = snapshot.sessions;
                self.session = snapshot.session;
                self.messages = snapshot.page.messages;
                self.next_offset = snapshot.page.next_offset;
                if !self.messages.iter().any(|m| Some(&m.id) == self.selected.as_ref()) {
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
        self.previous.clear();
        self.note.clear();
    }

    fn choose_session(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        self.clear_selection();
        self.session = Some(name);
        self.answer.update(cx, |input, cx| input.set_value("", window, cx));
        self.start_load(None, cx);
    }

    fn change_kind(&mut self, kind: Option<Kind>, window: &mut Window, cx: &mut Context<Self>) {
        self.kind = kind;
        self.messages.clear();
        self.selected = None;
        self.offset = 0;
        self.previous.clear();
        self.answer.update(cx, |input, cx| input.set_value("", window, cx));
        self.start_load(None, cx);
    }

    fn connect_remote(&mut self, cx: &mut Context<Self>) {
        let url = self.url.read(cx).value().trim().to_string();
        let token = self.token.read(cx).value().trim().to_string();
        if url.is_empty() || token.is_empty() {
            self.error = "Enter the HTTP address and server access token.".into();
            cx.notify();
            return;
        }
        self.start_load(Some(Connection::Remote { url, token }), cx);
    }

    fn act(&mut self, reply: bool, cx: &mut Context<Self>) {
        if self.writing || self.loading || !self.connected {
            return;
        }
        let Some(session) = self.session.clone() else {
            return;
        };
        let Some(id) = self.selected.clone() else {
            return;
        };
        let Some(message) = self.messages.iter().find(|m| m.id == id) else {
            return;
        };
        if message.to != session
            || (reply && message.reply.is_some())
            || (!reply && message.kind != Kind::Inbox)
        {
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

    fn connection_header(&self, cx: &mut Context<Self>) -> Div {
        let remote = matches!(self.connection, Connection::Remote { .. });
        let mut header = div()
            .v_flex()
            .gap_3()
            .p_4()
            .rounded_lg()
            .bg(rgb(0xf5f7fa))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        div()
                            .flex_1()
                            .v_flex()
                            .gap_1()
                            .child(div().font_semibold().child(self.connection.title()))
                            .child(crate::muted(if self.loading {
                                "Connecting / refreshing…".into()
                            } else if self.connected {
                                format!(
                                    "{} · {} registered sessions",
                                    if self.streaming { "Live" } else { "Connected" },
                                    self.sessions.len()
                                )
                            } else {
                                "Messenger is not connected".into()
                            })),
                    )
                    .child(
                        Button::new("messenger-refresh")
                            .small()
                            .label("Refresh")
                            .disabled(self.loading || self.writing)
                            .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(crate::muted(if remote {
                        "Viewing messages on the remote CCS server."
                    } else {
                        "Your current CCS sessions, inbox and queue."
                    }))
                    .child(
                        Button::new("view-remote-ccs")
                            .small()
                            .ghost()
                            .label(if remote { "Change remote" } else { "View remote CCS →" })
                            .disabled(self.loading || self.writing)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.remote_form = !this.remote_form;
                                cx.notify();
                            })),
                    ),
            );
        if !self.stream_error.is_empty() {
            header = header.child(
                crate::muted(format!("Reconnecting live updates… {}", self.stream_error)).text_xs(),
            );
        }
        if remote {
            header = header.child(
                Button::new("back-local-ccs")
                    .small()
                    .ghost()
                    .label("← Back to local CCS")
                    .disabled(self.writing)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.connection = Connection::local();
                        this.start_watch(cx);
                        this.clear_selection();
                        this.sessions.clear();
                        this.kind = None;
                        this.connected = false;
                        this.answer.update(cx, |input, cx| input.set_value("", window, cx));
                        this.start_load(None, cx);
                    })),
            );
        }
        if self.remote_form {
            header = header.child(
                div()
                    .id("remote-ccs-form")
                    .test_support()
                    .v_flex()
                    .gap_2()
                    .pt_2()
                    .child(crate::muted("Remote CCS · HTTP connection"))
                    .child(
                        Input::new(&self.url)
                            .id("remote-url")
                            .disabled(self.loading || self.writing),
                    )
                    .child(
                        Input::new(&self.token)
                            .id("remote-token")
                            .disabled(self.loading || self.writing),
                    )
                    .child(
                        div().flex().gap_2().child(
                            Button::new("connect-remote-ccs")
                                .label("Connect")
                                .disabled(self.loading || self.writing)
                                .on_click(cx.listener(|this, _, _, cx| this.connect_remote(cx))),
                        ),
                    )
                    .child(
                        crate::muted("The access token stays in memory for this app session.")
                            .text_xs(),
                    ),
            );
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
fn short_body(body: &str) -> String {
    body.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(100).collect()
}

impl Render for Messenger {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.reset_answer {
            self.answer.update(cx, |input, cx| input.set_value("", window, cx));
            self.reset_answer = false;
        }
        let mut content = div().v_flex().gap_4().child(self.connection_header(cx));
        if !self.error.is_empty() {
            content = content.child(
                div()
                    .id("messenger-error")
                    .p_3()
                    .rounded_md()
                    .bg(rgb(0xfff4ed))
                    .text_color(rgb(0x9a3412))
                    .child(self.error.clone()),
            );
        }
        if self.sessions.is_empty() {
            return content.child(div().id("messenger-empty").v_flex().gap_2().py_6()
                .child(div().font_semibold().child(if self.connected { "No registered sessions yet" } else { "Connect to your local messenger" }))
                .child(crate::muted(if self.connected { "Register an existing Claude or Codex session with ccs session register." } else { "Run ccs server on this machine, then refresh. You can also view a remote CCS server above." })));
        }
        let mut sessions = div()
            .w(px(185.))
            .flex_shrink_0()
            .v_flex()
            .gap_2()
            .child(crate::muted("SESSIONS").text_xs());
        for session in &self.sessions {
            let name = session.name.clone();
            sessions = sessions.child(
                div()
                    .v_flex()
                    .gap_1()
                    .p_2()
                    .rounded_md()
                    .when(self.session.as_ref() == Some(&name), |d| d.bg(rgb(0xeff6ff)))
                    .child(
                        Button::new(SharedString::from(format!("session-{name}")))
                            .small()
                            .ghost()
                            .label(name.clone())
                            .selected(self.session.as_ref() == Some(&name))
                            .disabled(self.writing)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.choose_session(name.clone(), window, cx)
                            })),
                    )
                    .child(crate::muted(session.provider.clone()).text_xs())
                    .children(
                        session
                            .labels
                            .iter()
                            .map(|(k, v)| crate::muted(format!("{k}={v}")).text_xs()),
                    ),
            );
        }
        let mut flow = div().flex_1().min_w_0().v_flex().gap_3();
        let mut filters = div().flex().items_center().gap_1();
        for (id, title, kind) in [
            ("flow-all", "All activity", None),
            ("flow-inbox", "Inbox", Some(Kind::Inbox)),
            ("flow-queue", "Queue", Some(Kind::Queue)),
        ] {
            filters = filters.child(
                Button::new(id)
                    .small()
                    .ghost()
                    .label(title)
                    .selected(self.kind == kind)
                    .disabled(self.writing)
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.change_kind(kind, window, cx)),
                    ),
            );
        }
        flow = flow.child(filters).child(
            crate::muted(
                "Incoming and outgoing · newest first · viewing does not mark messages read",
            )
            .text_xs(),
        );
        let mut list =
            div().id("message-list").v_flex().gap_2().max_h(px(250.)).overflow_y_scroll();
        for message in &self.messages {
            let id = message.id.clone();
            let selected = self.selected.as_ref() == Some(&id);
            list = list.child(
                div()
                    .id(SharedString::from(format!("message-{id}")))
                    .test_support()
                    .p_3()
                    .border_1()
                    .rounded_md()
                    .border_color(if selected { rgb(0x3b82f6) } else { rgb(0xe5e7eb) })
                    .when(selected, |d| d.bg(rgb(0xf5f9ff)))
                    .cursor_pointer()
                    .v_flex()
                    .gap_1()
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
                    .child(
                        div()
                            .flex()
                            .justify_between()
                            .gap_2()
                            .child(
                                div()
                                    .font_semibold()
                                    .child(format!("{} → {}", message.from, message.to)),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(status_color(message))
                                    .child(status_text(message)),
                            ),
                    )
                    .child(div().child(short_body(&message.body)))
                    .child(
                        crate::muted(format!(
                            "{} · {}",
                            if message.kind == Kind::Queue { "Queue" } else { "Inbox" },
                            message.id
                        ))
                        .text_xs(),
                    ),
            );
        }
        if self.messages.is_empty() {
            list = list.child(div().py_6().child(crate::muted("No messages in this view.")));
        }
        flow = flow.child(list).child(
            div()
                .flex()
                .gap_2()
                .items_center()
                .child(
                    Button::new("messages-previous")
                        .small()
                        .ghost()
                        .label("Newer")
                        .disabled(self.previous.is_empty() || self.loading || self.writing)
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Some(offset) = this.previous.pop() {
                                this.offset = offset;
                                this.selected = None;
                                this.start_load(None, cx);
                            }
                        })),
                )
                .child(crate::muted(format!("{} messages shown", self.messages.len())).text_xs())
                .child(
                    Button::new("messages-next")
                        .small()
                        .ghost()
                        .label("Older")
                        .disabled(self.next_offset.is_none() || self.loading || self.writing)
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Some(offset) = this.next_offset {
                                this.previous.push(this.offset);
                                this.offset = offset;
                                this.selected = None;
                                this.start_load(None, cx);
                            }
                        })),
                ),
        );
        if let Some(message) = self.messages.iter().find(|m| Some(&m.id) == self.selected.as_ref())
        {
            let mut detail = div()
                .id("message-detail")
                .v_flex()
                .gap_3()
                .p_4()
                .rounded_lg()
                .bg(rgb(0xf8fafc))
                .child(div().font_semibold().child(format!("{} → {}", message.from, message.to)))
                .child(crate::muted(message.id.clone()).text_xs())
                .child(div().id("message-body").test_support().child(message.body.clone()));
            if let Some(id) = &message.reply_to {
                detail = detail.child(crate::muted(format!("In reply to {id}")).text_xs());
            }
            if let Some(error) = &message.error {
                detail = detail.child(div().text_color(rgb(0xb42318)).child(error.clone()));
            }
            if let Some(reply) = &message.reply {
                detail = detail.child(
                    div()
                        .v_flex()
                        .gap_1()
                        .child(div().font_semibold().child("Reply"))
                        .child(reply.clone()),
                );
            }
            if self.session.as_ref() == Some(&message.to) {
                if message.kind == Kind::Inbox && message.status == "pending" {
                    detail = detail.child(
                        Button::new("message-ack")
                            .small()
                            .ghost()
                            .label("Mark as read")
                            .disabled(self.loading || self.writing || !self.connected)
                            .on_click(cx.listener(|this, _, _, cx| this.act(false, cx))),
                    );
                }
                if message.reply.is_none() {
                    detail = detail
                        .child(crate::muted(format!("Reply as {}", message.to)))
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
            }
            flow = flow.child(detail);
        } else {
            flow = flow.child(crate::muted("Select a message to read the full conversation."));
        }
        if !self.note.is_empty() {
            flow = flow.child(div().text_color(rgb(0x067647)).child(self.note.clone()));
        }
        content.child(div().flex().gap_5().child(sessions).child(flow))
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
