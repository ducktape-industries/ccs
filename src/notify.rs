//! Opt-in session notices: Claude Code's peer inbox and Codex CLI's queue.
//! Each provider receives its own usage events; switches are scoped to the
//! credential home they change. Codex recipients retain their thread id,
//! home and executable so a watcher can queue from outside the session.
//!
//! Codex notices need a CLI with `queue --thread --message`. Registration
//! checks the command before saving; failed deliveries retain the recipient
//! for future events and are reported without failing the account switch.
//!
//! A session subscribes with `ccs notify` from inside itself, naming the
//! kinds of notice it wants (see `watch::KINDS`): Claude Code exports its
//! inbox path as `CLAUDE_CODE_MESSAGING_SOCKET`, and that is what gets
//! remembered. Every session also publishes `sessions/<pid>.<hash>.key`
//! under its configuration directory, holding the token its inbox expects.
//!
//! The wire format is Claude Code's own peer messaging: one auth line, one
//! user-message line, done. It is undocumented, so a mismatch after an upgrade
//! costs only the notice, never the switch.
//!
//! A session that bypasses permission prompts holds a notice for review unless
//! the sender is in its own process tree or attests the same permission mode.
//! A subscriber that runs that way says so with `--bypass`, and every notice
//! to it carries the attestation; then `ccs watch` can run anywhere.

use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::fsx::write_atomic;
use crate::lock;
use crate::model::Provider;
use crate::watch::KINDS;

const SOCKET_ENV: &str = "CLAUDE_CODE_MESSAGING_SOCKET";
const FILE: &str = "notify.json";
const TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Debug, Default, Serialize, Deserialize)]
struct Subscription {
    kinds: Vec<String>,
    /// The permission-mode class the session runs in, when it said: "bypass"
    /// or "prompting". Attested on every notice so the inbox accepts it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mode: Option<String>,
    /// A missing directory uses the broadcaster's key directory for legacy registrations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    config: Option<PathBuf>,
}

type Subscribers = BTreeMap<String, Subscription>;

pub use crate::adapters::Session;
use crate::adapters::{ClaudeCode, Codex};

impl Session {
    pub fn detect(
        config: &Path,
        home: &Path,
        binary: &str,
        provider: Option<Provider>,
        bypass: bool,
    ) -> Result<Self> {
        let socket = std::env::var(SOCKET_ENV).ok().filter(|s| !s.is_empty());
        let thread = std::env::var("CODEX_THREAD_ID").ok().filter(|s| !s.is_empty());
        let provider = match provider {
            Some(provider) => provider,
            None => calling_provider(socket.as_deref(), thread.as_deref())?,
        };
        match provider {
            Provider::Claude => ClaudeCode::detect(config, bypass).map(Self::Claude),
            Provider::Codex => Codex::detect(home, binary, bypass).map(Self::Codex),
        }
    }
}

fn calling_provider(socket: Option<&str>, thread: Option<&str>) -> Result<Provider> {
    match (socket, thread) {
        (Some(_), None) => Ok(Provider::Claude),
        (None, Some(_)) => Ok(Provider::Codex),
        (Some(_), Some(_)) => bail!(
            "both clients' session variables are set; use --claude or --codex without a terminal"
        ),
        (None, None) => bail!("run `ccs notify` from inside a Claude Code or Codex session"),
    }
}

pub(crate) fn executable(binary: &str) -> Result<PathBuf> {
    if Path::new(binary).components().count() > 1 {
        return fs::canonicalize(binary).with_context(|| format!("resolving {binary}"));
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .map(|dir| dir.join(binary))
        .find(|p| p.is_file())
        .with_context(|| format!("{binary} is not on PATH; set CCS_CODEX_BINARY"))
        .and_then(|p| fs::canonicalize(p).map_err(Into::into))
}

pub fn subscribe(root: &Path, session: Session, on: bool, kinds: &[String]) -> Result<()> {
    if let Some(bad) = kinds.iter().find(|k| !KINDS.contains(&k.as_str())) {
        bail!("unknown notice kind {bad:?}; the kinds are {}", KINDS.join(", "));
    }
    let kinds = if kinds.is_empty() { KINDS.map(String::from).to_vec() } else { kinds.to_vec() };
    if let Session::Claude(ClaudeCode { socket, config, bypass }) = session {
        return subscribe_claude(root, &socket, &config, bypass, on, kinds);
    }
    if let Session::Codex(Codex { thread, home, binary }) = session {
        return subscribe_codex(root, &thread, &home, &binary, on, kinds);
    }
    bail!("usage notices are not supported for adapter {}", session.provider())
}

fn subscribe_claude(
    root: &Path,
    sock: &str,
    config: &Path,
    bypass: bool,
    on: bool,
    kinds: Vec<String>,
) -> Result<()> {
    let missing_inbox = on && !Path::new(sock).exists();
    if missing_inbox {
        bail!("{sock} does not exist; is this session's inbox up?");
    }
    let _guard = lock::acquire(root)?;
    let mut subs = subscribers(root);
    subs.remove(sock);
    if on {
        let mode = bypass.then(|| "bypass".to_string());
        println!("will notify {sock} of {}", kinds.join(", "));
        subs.insert(
            sock.to_string(),
            Subscription { kinds, mode, config: Some(config.to_path_buf()) },
        );
    } else {
        println!("will no longer notify {sock}");
    }
    save(root, &subs)
}

const CODEX_FILE: &str = "notify-codex.json";

#[derive(Serialize, Deserialize)]
struct CodexSubscription {
    thread: String,
    home: PathBuf,
    binary: PathBuf,
    kinds: Vec<String>,
}

fn codex_subscribers(root: &Path) -> Result<Vec<CodexSubscription>> {
    let raw = match fs::read(root.join(CODEX_FILE)) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).context("reading Codex subscriptions"),
    };
    serde_json::from_slice(&raw).context("parsing Codex subscriptions")
}

fn subscribe_codex(
    root: &Path,
    thread: &str,
    home: &Path,
    binary: &Path,
    on: bool,
    kinds: Vec<String>,
) -> Result<()> {
    if on {
        let help = Command::new(binary)
            .args(["queue", "--help"])
            .env("CODEX_HOME", home)
            .output()
            .context("checking Codex queue support")?;
        let output = String::from_utf8_lossy(&help.stdout);
        let supports_queue =
            help.status.success() && output.contains("--thread") && output.contains("--message");
        if !supports_queue {
            bail!(
                "this Codex does not support `codex queue --thread --message`; update Codex to receive notices; `ccs status --codex --cached` still works"
            );
        }
    }
    let binary = match on {
        true => executable(binary.to_str().context("Codex binary path is not UTF-8")?)?,
        false => binary.to_path_buf(),
    };
    let _guard = lock::acquire(root)?;
    let mut subs = codex_subscribers(root)?;
    subs.retain(|sub| sub.thread != thread || sub.home != home);
    if on {
        println!("will queue Codex notices for {thread}: {}", kinds.join(", "));
        subs.push(CodexSubscription {
            thread: thread.to_string(),
            home: home.to_path_buf(),
            binary: binary.to_path_buf(),
            kinds,
        });
    } else {
        println!("will no longer notify Codex session {thread}");
    }
    write_atomic(&root.join(CODEX_FILE), &serde_json::to_vec_pretty(&subs)?, 0o600)
}

/// Push a `kind` notice reading `text` to every subscriber to that kind still
/// reachable. Returns how many took it; a subscriber whose inbox is gone is
/// forgotten.
pub fn broadcast(config: &Path, root: &Path, provider: Provider, kind: &str, text: &str) -> usize {
    match provider {
        Provider::Claude => broadcast_claude(config, root, kind, text),
        Provider::Codex => broadcast_codex(config, root, kind, text),
    }
}

fn broadcast_codex(home: &Path, root: &Path, kind: &str, text: &str) -> usize {
    let subs = match codex_subscribers(root) {
        Ok(subs) => subs,
        Err(error) => {
            eprintln!("ccs: {error:#}");
            return 0;
        }
    };
    let home = fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf());
    let mut told = 0;
    for sub in subs {
        let another_home_switched = kind == "switch" && sub.home != home;
        let wants_notice = sub.kinds.iter().any(|wanted| wanted == kind) && !another_home_switched;
        if !wants_notice {
            continue;
        }
        let body = format!("ccs {kind}: {text}");
        let result = Command::new(&sub.binary)
            .args(["queue", "--thread", &sub.thread, "--message", &body])
            .env("CODEX_HOME", &sub.home)
            .current_dir(&sub.home)
            .output();
        match result {
            Ok(output) if output.status.success() => told += 1,
            Ok(output) => eprintln!(
                "ccs: could not queue notice for {} ({}); subscription retained",
                sub.thread, output.status
            ),
            Err(error) => eprintln!(
                "ccs: could not queue notice for {}: {error}; subscription retained",
                sub.thread
            ),
        }
    }
    told
}

fn broadcast_claude(config: &Path, root: &Path, kind: &str, text: &str) -> usize {
    let config = fs::canonicalize(config).unwrap_or_else(|_| config.to_path_buf());
    let config = config.as_path();
    let subs = subscribers(root);
    let wanted: Vec<(String, Option<String>, PathBuf)> = subs
        .iter()
        .filter(|(_, s)| s.kinds.iter().any(|k| k == kind))
        .filter(|(_, s)| {
            let same_home = s.config.as_deref().is_none_or(|home| home == config);
            kind != "switch" || same_home
        })
        .map(|(sock, s)| {
            (sock.clone(), s.mode.clone(), s.config.clone().unwrap_or_else(|| config.to_path_buf()))
        })
        .collect();
    if wanted.is_empty() {
        return 0;
    }
    let mut told = 0;
    for (sock, mode, config) in wanted {
        let sessions = config.join("sessions");
        let attest = mode.as_deref().map(|m| format!(" from-mode=\"{m}\"")).unwrap_or_default();
        let body = format!(
            "<cross-session-message from-name=\"ccs\"{attest}>\nccs {kind}: {text}\n</cross-session-message>"
        );
        if send(&sessions, &sock, &body, mode.as_deref()).is_some() {
            told += 1;
        } else if let Ok(_guard) = lock::acquire(root) {
            let mut current = subscribers(root);
            current.remove(&sock);
            let _ = save(root, &current);
        }
    }
    told
}

fn subscribers(root: &Path) -> Subscribers {
    fs::read(root.join(FILE)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn save(root: &Path, subs: &Subscribers) -> Result<()> {
    write_atomic(&root.join(FILE), &serde_json::to_vec_pretty(subs)?, 0o600)
}

/// The token a session's inbox expects, from the key it published beside its
/// registration. The key name carries the pid and a hash of the socket path;
/// the pid alone is enough to pick it out.
fn token(sessions: &Path, sock: &str) -> Option<String> {
    let pid = Path::new(sock).file_stem()?.to_str()?;
    let prefix = format!("{pid}.");
    fs::read_dir(sessions).ok()?.flatten().find_map(|e| {
        let name = e.file_name().into_string().ok()?;
        if !name.starts_with(&prefix) || !name.ends_with(".key") {
            return None;
        }
        let key: Value = serde_json::from_slice(&fs::read(e.path()).ok()?).ok()?;
        Some(key.get("peerToken")?.as_str()?.to_owned())
    })
}

pub(crate) fn send(sessions: &Path, sock: &str, body: &str, mode: Option<&str>) -> Option<()> {
    let token = token(sessions, sock)?;
    let mut s = UnixStream::connect(sock).ok()?;
    s.set_write_timeout(Some(TIMEOUT)).ok()?;
    let msg_id = format!("ccs-{}-{}", std::process::id(), crate::model::now_ms());
    let lines = format!(
        "{}\n{}\n",
        json!({"type": "auth", "token": token}),
        json!({
            "type": "user",
            "message": {"role": "user", "content": body},
            "priority": "next",
            "from": "ccs",
            "msg_id": msg_id,
            "from_mode": mode,
        })
    );
    s.write_all(lines.as_bytes()).ok()?;
    // Stay alive until the inbox has looked at the message: it vets the sender
    // by walking its process ancestry in /proc, and a sender that has already
    // exited by then is treated as a stranger and held for review.
    let _ = s.shutdown(Shutdown::Write);
    let _ = s.set_read_timeout(Some(TIMEOUT));
    let _ = s.read(&mut [0; 64]);
    Some(())
}

#[cfg(test)]
mod tests {

    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;

    struct Fixture {
        root: PathBuf,
        home: PathBuf,
        binary: PathBuf,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let root =
                std::env::temp_dir().join(format!("ccs-notify-{}-{name}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            let home = root.join("codex");
            fs::create_dir_all(&home).unwrap();
            let binary =
                Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex-queue.sh");
            Self { root, home, binary }
        }

        fn session(&self) -> Session {
            Session::Codex(Codex {
                thread: "session-uuid".into(),
                home: self.home.clone(),
                binary: self.binary.clone(),
            })
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn only_an_unambiguous_session_environment_selects_a_provider() {
        assert_eq!(calling_provider(Some("socket"), None).unwrap(), Provider::Claude);
        assert_eq!(calling_provider(None, Some("thread")).unwrap(), Provider::Codex);
        assert!(calling_provider(Some("socket"), Some("thread")).is_err());
        assert!(calling_provider(None, None).is_err());
    }

    #[test]
    fn codex_delivery_uses_the_registered_home_and_literal_message() {
        let f = Fixture::new("queue");
        subscribe(&f.root, f.session(), true, &[]).unwrap();
        let body = "usage is 95%; $(touch unwanted) `echo ignored`";
        assert_eq!(broadcast(&f.home, &f.root, Provider::Codex, "session-high", body), 1);
        let queued = fs::read_to_string(f.home.join("queued")).unwrap();
        assert_eq!(
            queued,
            format!("queue\n--thread\nsession-uuid\n--message\nccs session-high: {body}\n")
        );
        assert_eq!(
            fs::metadata(f.root.join(CODEX_FILE)).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn codex_subscriptions_filter_kinds_and_provider_and_can_be_removed() {
        let f = Fixture::new("filter");
        subscribe(&f.root, f.session(), true, &["session-high".into()]).unwrap();
        assert_eq!(broadcast(&f.home, &f.root, Provider::Claude, "session-high", "Claude"), 0);
        assert_eq!(broadcast(&f.home, &f.root, Provider::Codex, "weekly-reset", "weekly"), 0);
        assert!(!f.home.join("queued").exists());
        subscribe(&f.root, f.session(), true, &["weekly-reset".into()]).unwrap();
        assert_eq!(codex_subscribers(&f.root).unwrap().len(), 1);
        assert_eq!(broadcast(&f.home, &f.root, Provider::Codex, "weekly-reset", "weekly"), 1);
        let missing_binary = Session::Codex(Codex {
            thread: "session-uuid".into(),
            home: f.home.clone(),
            binary: f.root.join("missing-codex"),
        });
        subscribe(&f.root, missing_binary, false, &[]).unwrap();
        assert!(codex_subscribers(&f.root).unwrap().is_empty());
    }

    #[test]
    fn a_codex_pin_does_not_hear_a_global_switch_but_hears_its_own() {
        let f = Fixture::new("pin-scope");
        subscribe(&f.root, f.session(), true, &[]).unwrap();
        assert_eq!(broadcast(&f.root, &f.root, Provider::Codex, "switch", "global"), 0);
        assert!(!f.home.join("queued").exists());
        assert_eq!(broadcast(&f.home, &f.root, Provider::Codex, "switch", "pin"), 1);
    }

    #[test]
    fn a_failed_queue_keeps_the_subscription_and_does_not_claim_delivery() {
        let f = Fixture::new("failed-queue");
        subscribe(&f.root, f.session(), true, &[]).unwrap();
        fs::write(f.home.join("reject"), "").unwrap();
        assert_eq!(broadcast(&f.home, &f.root, Provider::Codex, "session-high", "high"), 0);
        assert_eq!(codex_subscribers(&f.root).unwrap().len(), 1);
    }

    #[test]
    fn unsupported_codex_and_unknown_kinds_register_nothing() {
        let f = Fixture::new("unsupported");
        assert!(subscribe(&f.root, f.session(), true, &["typo".into()]).is_err());
        fs::write(f.home.join("reject"), "").unwrap();
        let error = subscribe(&f.root, f.session(), true, &[]).unwrap_err();
        assert!(error.to_string().contains("does not support"), "{error:#}");
        assert!(!f.root.join(CODEX_FILE).exists());
    }

    #[test]
    fn malformed_codex_subscriptions_are_not_silently_overwritten() {
        let f = Fixture::new("malformed");
        fs::write(f.root.join(CODEX_FILE), "broken").unwrap();
        assert!(subscribe(&f.root, f.session(), true, &[]).is_err());
        assert_eq!(fs::read_to_string(f.root.join(CODEX_FILE)).unwrap(), "broken");
    }

    #[test]
    fn legacy_claude_registrations_still_decode() {
        let sub: Subscription =
            serde_json::from_str(r#"{"kinds":["switch"],"mode":"bypass"}"#).unwrap();
        assert_eq!(sub.mode.as_deref(), Some("bypass"));
        assert!(sub.config.is_none());
    }

    #[test]
    fn claude_delivery_reads_the_subscribers_key_and_keeps_its_attestation() {
        let f = Fixture::new("claude");
        let config = f.root.join("claude");
        fs::create_dir_all(config.join("sessions")).unwrap();
        let socket = f.root.join("123.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        fs::write(config.join("sessions/123.hash.key"), r#"{"peerToken":"test-token"}"#).unwrap();
        subscribe(
            &f.root,
            Session::Claude(ClaudeCode {
                socket: socket.to_str().unwrap().into(),
                config: config.clone(),
                bypass: true,
            }),
            true,
            &[],
        )
        .unwrap();
        assert_eq!(broadcast(&f.home, &f.root, Provider::Claude, "switch", "elsewhere"), 0);
        let reader = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut body = String::new();
            stream.read_to_string(&mut body).unwrap();
            body
        });
        assert_eq!(broadcast(&f.home, &f.root, Provider::Claude, "session-high", "high"), 1);
        let lines: Vec<Value> = reader
            .join()
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(lines[0]["token"], "test-token");
        assert_eq!(lines[1]["from_mode"], "bypass");
        assert_eq!(lines[1]["priority"], "next");
    }
}
