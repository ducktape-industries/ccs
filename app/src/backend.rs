//! Background account, usage, routing and preference services for the GPUI app.

#[cfg(not(test))]
use std::sync::{Arc, Mutex};

use ccs::cmd::Cached;
use ccs::model::Health;
use ccs::render;
use futures::Stream;
use jiff::Timestamp;

/// One limit as the view draws it.
#[derive(Clone, Debug, PartialEq)]
pub struct Limit {
    pub column: String,
    pub percent: f64,
    /// The CLI's countdown, or nothing when the limit names no reset.
    pub resets_in: String,
    /// `ok`, `warn` or `spent`: the CLI's thresholds.
    pub health: String,
}

/// One account as the view draws it.
#[derive(Clone, Debug, PartialEq)]
pub struct Account {
    pub provider: String,
    pub slug: String,
    pub email: String,
    pub plan: String,
    pub active: bool,
    /// Any limit with nothing left.
    pub spent: bool,
    /// The five-hour window, or -1 when the account reports none.
    pub session_percent: f64,
    /// When the watcher last read it, RFC 3339, or empty: the key a row is
    /// rebuilt on, since every reading writes a new one.
    pub polled_at: String,
    /// The same, as a clock time for a person.
    pub polled: String,
    /// Why there is no reading, when there is none.
    pub note: String,
    pub limits: Vec<Limit>,
}

/// What went wrong, for the window to say. A switch refused because the
/// account is spent says so, and names the account, so the window can ask
/// and come back with `force`.
#[derive(Clone, Debug)]
pub struct Failure {
    pub message: String,
    pub slug: String,
    pub spent: bool,
}

impl Failure {
    fn new(message: impl Into<String>) -> Self {
        Self { message: message.into(), slug: String::new(), spent: false }
    }

    fn spent(slug: &str, email: &str) -> Self {
        Self {
            message: format!("{email} has nothing left on one of its limits"),
            slug: slug.to_string(),
            spent: true,
        }
    }
}

impl From<anyhow::Error> for Failure {
    fn from(error: anyhow::Error) -> Self {
        Self::new(error.chain().map(|c| c.to_string()).collect::<Vec<_>>().join(": "))
    }
}

/// One turn of the watcher, as the window takes it.
#[derive(Clone, Debug)]
pub struct Poll {
    pub accounts: Vec<Account>,
    /// The notices raised this turn, one line each.
    pub notices: Vec<String>,
    /// The accounts rotated onto, one line each.
    pub rotated: Vec<String>,
}

/// What the app remembers between launches.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Prefs {
    #[serde(default)]
    pub gateway_on: bool,
    #[serde(default = "default_port")]
    pub gateway_port: String,
    #[serde(default)]
    pub rotation_on: bool,
    #[serde(default)]
    pub pool: Vec<String>,
    #[serde(default = "yes")]
    pub notifications_on: bool,
    #[serde(default)]
    pub launch_at_login: bool,
}

fn default_port() -> String {
    "4141".to_string()
}

fn yes() -> bool {
    true
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            gateway_on: false,
            gateway_port: default_port(),
            rotation_on: false,
            pool: Vec::new(),
            notifications_on: true,
            launch_at_login: false,
        }
    }
}

// ── the process ─────────────────────────────────────────────────────────────

/// Everything a command borrows, opened on first use and shared with every
/// thread that asks. A failure to open is not kept: what stopped it — a
/// directory that could not be made, say — may be gone by the next ask.
#[cfg(not(test))]
static ENV: Mutex<Option<Arc<ccs::env::Env>>> = Mutex::new(None);

/// The whole-stash operations — a read of every account, a switch, a poll —
/// happen one at a time. See the module note for what this does not cover.
#[cfg(not(test))]
static STASH: Mutex<()> = Mutex::new(());

#[cfg(not(test))]
fn env() -> Result<Arc<ccs::env::Env>, Failure> {
    let mut held = ENV.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(env) = held.as_ref() {
        return Ok(Arc::clone(env));
    }
    let env = Arc::new(ccs::env::Env::open()?);
    *held = Some(Arc::clone(&env));
    Ok(env)
}

/// Run blocking work on a thread of its own and await its answer, so the
/// executor's thread stays free for everything else the window does. A
/// worker that dies without answering is a failure, not the window's end.
#[cfg(not(test))]
async fn offload<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, Failure> + Send + 'static,
) -> Result<T, Failure> {
    let (tx, rx) = futures::channel::oneshot::channel();
    std::thread::spawn(move || {
        let _ = tx.send(work());
    });
    rx.await.unwrap_or_else(|_| Err(Failure::new("the work gave up without an answer")))
}

// ── readings ────────────────────────────────────────────────────────────────

/// The accounts as the watcher last wrote them down. Never polls: opening
/// the window twenty times costs the limits nothing.
pub async fn load() -> Result<Vec<Account>, Failure> {
    #[cfg(test)]
    return fixture::load();
    #[cfg(not(test))]
    offload(read_accounts).await
}

pub async fn refresh() -> Result<Vec<Account>, Failure> {
    #[cfg(test)]
    return fixture::load();
    #[cfg(not(test))]
    offload(|| {
        let env = env()?;
        let _stash = STASH.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let readings = ccs::cmd::refresh_readings(&env.ctx())?;
        Ok(accounts_of(&readings, Timestamp::now()))
    })
    .await
}

#[cfg(not(test))]
fn read_accounts() -> Result<Vec<Account>, Failure> {
    let env = env()?;
    let _stash = STASH.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let readings = ccs::cmd::readings(&env.ctx())?;
    Ok(accounts_of(&readings, Timestamp::now()))
}

/// Switch to `slug`. A spent account is refused unless `force`, with a
/// failure that says so: off a terminal the CLI's guard cannot ask, so the
/// window asks instead and comes back with `force`. What comes back is the
/// cache read again.
pub async fn switch(slug: String, force: bool) -> Result<Vec<Account>, Failure> {
    #[cfg(test)]
    return fixture::switch(&slug, force);
    #[cfg(not(test))]
    offload(move || {
        let env = env()?;
        let _stash = STASH.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let ctx = env.ctx();
        let before = accounts_of(&ccs::cmd::readings(&ctx)?, Timestamp::now());
        if let Some(account) = before.iter().find(|a| a.slug == slug && a.spent && !force) {
            return Err(Failure::spent(&account.slug, &account.email));
        }
        ccs::cmd::switch(&ctx, &slug, force)?;
        Ok(accounts_of(&ccs::cmd::readings(&ctx)?, Timestamp::now()))
    })
    .await
}

// ── the watcher ─────────────────────────────────────────────────────────────

/// What the watcher reads before each turn: the pool to rotate among, and
/// whether to show what it found. Set by the handlers; the thread is never
/// restarted for a change, so no turn is wasted and no turn acts on a pool
/// that was just changed.
#[cfg(not(test))]
static WATCHING: Mutex<(Vec<String>, bool)> = Mutex::new((Vec::new(), true));

/// Tell the watcher what to rotate among and whether to notify. Immediate,
/// and always taken.
pub fn set_watch(pool: Vec<String>, notify: bool) -> bool {
    #[cfg(test)]
    {
        let _ = (pool, notify);
        true
    }
    #[cfg(not(test))]
    {
        *WATCHING.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = (pool, notify);
        true
    }
}

/// How often the watcher polls; the CLI's default.
#[cfg(not(test))]
const INTERVAL: std::time::Duration = std::time::Duration::from_secs(300);

/// The watcher: one poll every five minutes, on a thread of its own for the
/// life of the process, each turn handed to the window as it happens.
/// Manual refreshes share its stash lock. A first turn is not spent
/// when the cache is younger than the interval: the window shows that
/// reading and the thread waits out the rest. Notices are shown as
/// notifications here, when asked, since a handler cannot walk a list.
pub fn watch(high: f64) -> impl Stream<Item = Result<Poll, Failure>> + Send + 'static {
    #[cfg(test)]
    {
        let _ = high;
        fixture::watch()
    }
    #[cfg(not(test))]
    {
        use futures::SinkExt;
        let (mut tx, rx) = futures::channel::mpsc::channel::<Result<Poll, Failure>>(4);
        std::thread::spawn(move || {
            let mut before = ccs::watch::Snapshot::new();
            // What the cache already holds decides how long the first wait is.
            let mut wait = read_accounts()
                .ok()
                .and_then(|accounts| youngest(&accounts))
                .map_or(std::time::Duration::ZERO, |age| INTERVAL.saturating_sub(age));
            loop {
                for _ in 0..wait.as_secs() {
                    std::thread::sleep(std::time::Duration::from_secs(1));
                    if tx.is_closed() {
                        return;
                    }
                }
                let (pool, notify) =
                    WATCHING.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone();
                let turn = turn(&mut before, high, &pool);
                if let (true, Ok(turn)) = (notify, &turn) {
                    for line in turn.notices.iter().chain(&turn.rotated) {
                        crate::platform::notify("ccs", line);
                    }
                }
                if futures::executor::block_on(tx.send(turn)).is_err() {
                    return;
                }
                wait = INTERVAL;
            }
        });
        rx
    }
}

/// One turn: poll, level, rotate, and read the cache back.
#[cfg(not(test))]
fn turn(before: &mut ccs::watch::Snapshot, high: f64, pool: &[String]) -> Result<Poll, Failure> {
    let env = env()?;
    let _stash = STASH.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let ctx = env.ctx();
    let turn = ccs::cmd::poll(&ctx, before, high, pool);
    if let Some(why) = turn.failed {
        return Err(Failure::new(why));
    }
    let readings = ccs::cmd::readings(&ctx)?;
    Ok(Poll {
        accounts: accounts_of(&readings, Timestamp::now()),
        notices: turn
            .notices
            .iter()
            .map(|(event, _)| format!("{}: {}", event.kind, event.text))
            .collect(),
        rotated: turn
            .rotations
            .iter()
            .map(|r| match r {
                Ok(switched) => format!("switched to {}", switched.target.account.email),
                Err(why) => format!("rotate failed: {why}"),
            })
            .collect(),
    })
}

/// How old the newest reading is.
#[cfg(not(test))]
fn youngest(accounts: &[Account]) -> Option<std::time::Duration> {
    let now = Timestamp::now();
    accounts
        .iter()
        .filter_map(|a| a.polled_at.parse::<Timestamp>().ok())
        .map(|at| (now.as_second() - at.as_second()).max(0) as u64)
        .min()
        .map(std::time::Duration::from_secs)
}

// ── the menu bar item ───────────────────────────────────────────────────────

/// Menu bar clicks forwarded to the GPUI window.
#[cfg(all(target_os = "macos", not(test)))]
pub fn tray_clicks() -> futures::stream::BoxStream<'static, ()> {
    use futures::StreamExt;
    let (tx, rx) = futures::channel::mpsc::unbounded::<()>();
    tray_icon::TrayIconEvent::set_event_handler(Some(move |event: tray_icon::TrayIconEvent| {
        if let tray_icon::TrayIconEvent::Click {
            button: tray_icon::MouseButton::Left,
            button_state: tray_icon::MouseButtonState::Up,
            ..
        } = event
        {
            let _ = tx.unbounded_send(());
        }
    }));
    rx.boxed()
}

// ── the gateway ─────────────────────────────────────────────────────────────

/// The gateway that is up, if one is.
#[cfg(not(test))]
static GATEWAY: Mutex<Option<ccs::serve::Listening>> = Mutex::new(None);

/// The gateway, up or down. Up, the listener and the desk that answers it
/// run on threads of this process; `pool` is what a limited request falls
/// over to. Changing the port or the pool takes it down and up again. The
/// line that comes back is what the window shows under the switch.
pub async fn gateway(on: bool, port: String, pool: Vec<String>) -> Result<String, Failure> {
    #[cfg(test)]
    {
        let _ = pool;
        Ok(fixture::gateway(on, &port))
    }
    #[cfg(not(test))]
    offload(move || {
        let mut running = GATEWAY.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(up) = running.take() {
            up.stop();
        }
        if !on {
            return Ok("off".to_string());
        }
        let port: u16 =
            port.trim().parse().map_err(|_| Failure::new(format!("{port:?} is not a port")))?;
        let env = env()?;
        let keys = ccs::cmd::gateway_keys(&env.ctx())?;
        let listener = std::net::TcpListener::bind(("127.0.0.1", port))
            .map_err(|e| Failure::new(format!("listening on 127.0.0.1:{port}: {e}")))?;
        let (asks, inbox) = std::sync::mpsc::channel();
        let up = ccs::serve::listen(listener, keys, asks);
        let desk_env = Arc::clone(&env);
        std::thread::spawn(move || ccs::cmd::desk(&desk_env.ctx(), &pool, inbox));
        *running = Some(up);
        Ok(format!("serving http://127.0.0.1:{port} as the accounts in use"))
    })
    .await
}

/// Take the gateway down, for a quit that should not leave a port held.
/// Immediate: it flips a flag and knocks once.
pub fn shutdown() -> bool {
    #[cfg(not(test))]
    if let Some(up) = GATEWAY.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).take() {
        up.stop();
    }
    true
}

// ── preferences ─────────────────────────────────────────────────────────────

#[cfg(not(test))]
const PREFS_FILE: &str = "app.json";

/// Where the preferences live: beside the stash, which is where the app's
/// state belongs.
#[cfg(not(test))]
fn prefs_path() -> Result<std::path::PathBuf, Failure> {
    Ok(env()?.ctx().stash.root().join(PREFS_FILE))
}

/// Read the preferences, or the defaults. Missing and broken read the same:
/// a file this program cannot read is not one it should reason from.
pub fn read_prefs(path: &std::path::Path) -> Prefs {
    std::fs::read(path).ok().and_then(|raw| serde_json::from_slice(&raw).ok()).unwrap_or_default()
}

pub fn write_prefs(path: &std::path::Path, prefs: &Prefs) -> Result<(), Failure> {
    let body = serde_json::to_vec_pretty(prefs).map_err(|e| Failure::new(e.to_string()))?;
    ccs::fsx::write_atomic(path, &body, 0o600)?;
    Ok(())
}

/// The preferences as the program starts: each state field asks for its
/// own at initialization.
pub fn prefs() -> Prefs {
    #[cfg(test)]
    return Prefs::default();
    #[cfg(not(test))]
    prefs_path().map(|path| read_prefs(&path)).unwrap_or_default()
}

/// Write the preferences down. Every toggle calls this; the answer is
/// whether it took, for the window to say when it did not.
pub fn save_prefs(
    gateway_on: bool,
    gateway_port: String,
    rotation_on: bool,
    pool: Vec<String>,
    notifications_on: bool,
    launch_at_login: bool,
) -> bool {
    let prefs =
        Prefs { gateway_on, gateway_port, rotation_on, pool, notifications_on, launch_at_login };
    #[cfg(test)]
    {
        let _ = prefs;
        true
    }
    #[cfg(not(test))]
    prefs_path().and_then(|path| write_prefs(&path, &prefs)).is_ok()
}

/// Start at login or stop; what comes back is what is now set.
pub async fn launch_at_login(on: bool) -> Result<bool, Failure> {
    #[cfg(test)]
    return Ok(on);
    #[cfg(not(test))]
    offload(move || crate::platform::launch_at_login(on).map_err(Failure::from)).await
}

// ── the cache in the view's terms ───────────────────────────────────────────

/// The cache's rows in the view's terms, stamped against `now`.
pub fn accounts_of(readings: &[Cached], now: Timestamp) -> Vec<Account> {
    readings
        .iter()
        .map(|reading| {
            let entry = &reading.entry;
            let limits: Vec<Limit> = entry
                .known()
                .iter()
                .map(|limit| Limit {
                    column: limit.column(),
                    percent: limit.percent,
                    resets_in: limit
                        .resets_at
                        .as_deref()
                        .and_then(render::until)
                        .unwrap_or_default(),
                    health: match limit.health() {
                        Health::Ok => "ok",
                        Health::Warn => "warn",
                        Health::Critical => "spent",
                    }
                    .into(),
                })
                .collect();
            Account {
                provider: entry.provider.to_string(),
                slug: entry.slug.clone(),
                email: entry.email.clone(),
                plan: entry.plan.clone(),
                active: entry.active,
                spent: entry.health() == Health::Critical,
                session_percent: entry
                    .known()
                    .iter()
                    .find(|l| l.kind == "session")
                    .map_or(-1.0, |l| l.percent),
                polled_at: reading.polled_at.clone().unwrap_or_default(),
                polled: reading.polled_at.as_deref().map(|at| clock(at, now)).unwrap_or_default(),
                note: entry.usage.as_ref().err().cloned().unwrap_or_default(),
                limits,
            }
        })
        .collect()
}

/// When `rfc3339` was, as a person reads a clock: the time of day today,
/// the date before that. Stable however long it is looked at, which "5m
/// ago" is not.
fn clock(rfc3339: &str, now: Timestamp) -> String {
    clock_in(rfc3339, now, jiff::tz::TimeZone::system())
}

fn clock_in(rfc3339: &str, now: Timestamp, zone: jiff::tz::TimeZone) -> String {
    let Ok(then) = rfc3339.parse::<Timestamp>() else { return String::new() };
    let (then, now) = (then.to_zoned(zone.clone()), now.to_zoned(zone));
    match then.date() == now.date() {
        true => then.strftime("%H:%M").to_string(),
        false => then.strftime("%m-%d %H:%M").to_string(),
    }
}

/// The stash a first-class test sees. Externs are real in those tests, so
/// the real thing is replaced here, under `cfg(test)`, by one that answers
/// the way the stash would: a switch marks one account of the provider
/// active and no other, and refuses a spent one unless forced.
#[cfg(test)]
pub mod fixture {
    use super::{Account, Failure, Limit};
    use std::sync::Mutex;

    static ACCOUNTS: Mutex<Vec<Account>> = Mutex::new(Vec::new());

    fn limit(column: &str, percent: f64, health: &str) -> Limit {
        Limit { column: column.into(), percent, resets_in: "3h04m".into(), health: health.into() }
    }

    fn account(provider: &str, slug: &str, active: bool, session: f64, spent: bool) -> Account {
        // A Codex account reports no five-hour window at all, as the real
        // endpoint does for a Pro plan; its row says nothing of a session.
        let mut limits = Vec::new();
        if session >= 0.0 {
            limits.push(limit("session", session, if spent { "spent" } else { "ok" }));
        }
        limits.push(limit("weekly", 37.0, "ok"));
        Account {
            provider: provider.into(),
            slug: slug.into(),
            email: format!("{slug}@example.com"),
            plan: if provider == "codex" { "codex pro".into() } else { "max20x".into() },
            active,
            spent,
            session_percent: session,
            polled_at: "2026-09-07T00:00:00Z".into(),
            polled: "09:00".into(),
            note: String::new(),
            limits,
        }
    }

    /// Three Claude accounts, one spent, and one Codex account; `hong` in use.
    pub fn reset() -> Vec<Account> {
        let accounts = vec![
            account("claude", "agent", false, 6.0, false),
            account("codex", "codex-frost", true, -1.0, false),
            account("claude", "hong", true, 52.0, false),
            account("claude", "robin", false, 100.0, true),
        ];
        *ACCOUNTS.lock().expect("fixture") = accounts.clone();
        accounts
    }

    pub fn load() -> Result<Vec<Account>, Failure> {
        let held = ACCOUNTS.lock().expect("fixture").clone();
        Ok(if held.is_empty() { reset() } else { held })
    }

    /// One turn, then the end: the fixture's accounts with `agent` in use
    /// and a later reading on every row, as if the watcher had polled and
    /// rotated onto it.
    pub fn watch() -> impl super::Stream<Item = Result<super::Poll, Failure>> + Send + 'static {
        let mut accounts = reset();
        for account in accounts.iter_mut() {
            account.polled_at = "2026-09-07T00:05:00Z".into();
            account.polled = "09:05".into();
            if account.provider == "claude" {
                account.active = account.slug == "agent";
            }
        }
        *ACCOUNTS.lock().expect("fixture") = accounts.clone();
        let turn = super::Poll {
            accounts,
            notices: vec!["session-high: hong@example.com has crossed 90%".into()],
            rotated: vec!["switched to agent@example.com".into()],
        };
        futures::stream::once(async move { Ok(turn) })
    }

    pub fn gateway(on: bool, port: &str) -> String {
        match on {
            true => format!("serving http://127.0.0.1:{port} as the accounts in use"),
            false => "off".into(),
        }
    }

    pub fn switch(slug: &str, force: bool) -> Result<Vec<Account>, Failure> {
        if ACCOUNTS.lock().expect("fixture").is_empty() {
            reset();
        }
        let mut held = ACCOUNTS.lock().expect("fixture");
        let Some(target) = held.iter().find(|a| a.slug == slug).cloned() else {
            return Err(Failure::new(format!("no stashed account matches {slug:?}")));
        };
        if target.spent && !force {
            return Err(Failure::spent(&target.slug, &target.email));
        }
        for account in held.iter_mut().filter(|a| a.provider == target.provider) {
            account.active = account.slug == slug;
        }
        Ok(held.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ccs::model::{Limit as Reading, Provider};
    use ccs::render::Entry;

    #[test]
    fn preferences_round_trip_and_missing_ones_are_the_defaults() {
        let dir = std::env::temp_dir().join(format!("ccs-app-prefs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("app.json");

        assert_eq!(read_prefs(&path), Prefs::default());
        let prefs = Prefs { gateway_on: true, pool: vec!["a".into()], ..Prefs::default() };
        write_prefs(&path, &prefs).expect("writes");
        assert_eq!(read_prefs(&path), prefs);
        std::fs::write(&path, "not json").expect("write");
        assert_eq!(read_prefs(&path), Prefs::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_older_file_missing_a_field_still_reads() {
        let dir = std::env::temp_dir().join(format!("ccs-app-prefs-old-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("app.json");
        std::fs::write(&path, r#"{"gateway_on": true}"#).expect("write");
        let prefs = read_prefs(&path);
        assert!(prefs.gateway_on);
        assert_eq!(prefs.gateway_port, "4141");
        assert!(prefs.notifications_on);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn cached(slug: &str, active: bool, limits: Result<Vec<Reading>, String>) -> Cached {
        Cached {
            entry: Entry {
                provider: Provider::Claude,
                slug: slug.into(),
                email: format!("{slug}@x.com"),
                plan: "max20x".into(),
                active,
                usage: limits.map(Into::into),
            },
            polled_at: Some("2026-09-07T00:00:00Z".into()),
        }
    }

    fn reading(kind: &str, percent: f64, resets_at: &str) -> Reading {
        Reading {
            kind: kind.into(),
            percent,
            severity: None,
            resets_at: Some(resets_at.into()),
            scope: None,
        }
    }

    #[test]
    fn a_cached_row_becomes_an_account_with_its_limits_named_and_judged() {
        let now: Timestamp = "2026-09-07T00:04:30Z".parse().expect("stamp");
        let rows = vec![cached(
            "h",
            true,
            Ok(vec![
                reading("session", 52.0, "2099-01-01T00:00:00Z"),
                reading("weekly_all", 100.0, "2099-01-01T00:00:00Z"),
            ]),
        )];

        let accounts = accounts_of(&rows, now);

        assert_eq!(accounts.len(), 1);
        let account = &accounts[0];
        assert_eq!(
            (account.provider.as_str(), account.slug.as_str(), account.active),
            ("claude", "h", true)
        );
        assert_eq!(account.session_percent, 52.0);
        assert!(account.spent);
        assert_eq!(account.polled_at, "2026-09-07T00:00:00Z");
        assert!(!account.polled.is_empty());
        assert_eq!(account.limits[0].column, "session");
        assert_eq!(account.limits[0].health, "ok");
        assert_eq!(account.limits[1].column, "weekly");
        assert_eq!(account.limits[1].health, "spent");
        assert!(!account.limits[0].resets_in.is_empty());
    }

    #[test]
    fn dashboard_limits_exclude_model_availability() {
        let now: Timestamp = "2026-09-15T00:00:00Z".parse().unwrap();
        let mut row = cached(
            "a",
            false,
            Ok((0..7)
                .map(|i| reading(&format!("future-{i}"), i as f64, "2026-09-16T00:00:00Z"))
                .collect()),
        );
        row.entry.usage.as_mut().unwrap().model_usage = Some(
            serde_json::from_value(serde_json::json!({
                "future-model": {"available": false}, "unknown-model": {}
            }))
            .unwrap(),
        );
        let account = &accounts_of(&[row], now)[0];
        assert_eq!(account.limits.len(), 7);
    }

    #[test]
    fn an_account_without_a_reading_carries_the_reason_and_no_session() {
        let rows = vec![cached("n", false, Err("not polled yet".into()))];
        let account = &accounts_of(&rows, Timestamp::now())[0];
        assert_eq!(account.note, "not polled yet");
        assert_eq!(account.session_percent, -1.0);
        assert!(account.limits.is_empty());
        assert!(!account.spent);
    }

    /// The clock says the time today and the date otherwise, in the zone
    /// the machine is in; either way the same string however long it is
    /// looked at.
    #[test]
    fn a_reading_is_stamped_with_a_clock_not_an_age() {
        let now: Timestamp = "2026-09-07T12:00:00Z".parse().expect("stamp");
        let seoul = jiff::tz::TimeZone::get("Asia/Seoul").expect("zone");
        assert_eq!(clock_in("2026-09-07T11:30:00Z", now, seoul.clone()), "20:30");
        assert_eq!(clock_in("2026-09-06T16:30:00Z", now, seoul.clone()), "01:30");
        assert_eq!(clock_in("2026-09-06T14:30:00Z", now, seoul.clone()), "09-06 23:30");
        assert_eq!(clock_in("nonsense", now, seoul), "");
        assert!(!clock("2026-09-07T11:30:00Z", now).is_empty());
    }
}

/// Load provider-native IDs without translating aliases or changing the active account.
pub async fn load_models(provider: ccs::model::Provider) -> Result<Vec<String>, Failure> {
    #[cfg(test)]
    {
        let _ = provider;
        Ok(vec![])
    }
    #[cfg(not(test))]
    offload(move || {
        let env = env()?;
        Ok(ccs::cmd::model_ids(&env.ctx(), provider)?)
    })
    .await
}

pub async fn load_routes() -> Result<ccs::routing::Routing, Failure> {
    #[cfg(test)]
    return Ok(ccs::routing::Routing::default());
    #[cfg(not(test))]
    offload(|| Ok(ccs::routing::Routing::read(env()?.ctx().stash.root())?)).await
}

pub async fn save_routes(routes: ccs::routing::Routing) -> Result<(), Failure> {
    #[cfg(test)]
    return routes.validate().map_err(Failure::from);
    #[cfg(not(test))]
    offload(move || {
        let env = env()?;
        let accounts = env.ctx().stash.list()?;
        routes.validate()?;
        for rule in &routes.rules {
            for slug in &rule.accounts {
                if !accounts.iter().any(|a| a.slug == *slug && a.account.provider == rule.provider)
                {
                    return Err(Failure::new(format!("No {} account named {slug}", rule.provider)));
                }
            }
        }
        routes.write(env.ctx().stash.root())?;
        Ok(())
    })
    .await
}
