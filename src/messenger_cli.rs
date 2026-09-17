//! Messenger CLI. No account-store initialization or provider-specific calling convention.
use crate::adapters::Session;
use crate::messenger::{self, Kind, Labels, Registration, Request};
use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;
use std::io::Read;
use std::path::PathBuf;
use std::time::{Duration, Instant};

pub const HELP: &str = "\
ccs server [--http <ip:port>]           run messenger; optional authenticated HTTP
ccs session register <name> [--adapter <name>|--claude|--codex] [--label key=value] [--bypass]
ccs session label <name> --label key=value  set labels (empty value removes)
ccs session remove <name>                explicitly release a registered name
ccs sessions [--label key=value]         list registered names; labels are ANDed
ccs inbox send <name> --message <text>    store an asynchronous message
ccs inbox [--session <name>] [--limit 20] [--offset 0]             list pending messages, without consuming
ccs inbox ack <id>                       mark an inbox message read
ccs queue <name> --message <text> [--timeout <seconds>]  send and wait for reply
ccs reply <id> --message <text>          answer a request or inbox message
ccs message <id>                         inspect message, delivery status and reply

For sends, --label key=value (repeatable) can replace the recipient name.
Use --session <name> or CCS_SESSION for sender/reader/replier identity.
Without either, identity is codex-<CODEX_THREAD_ID> or claude-<socket stem>;
register that name, or export CCS_SESSION when using a custom name.
All results are JSON. Queue timeout defaults to 300 seconds; it does not cancel.
CCS_SERVER_DIR selects storage/socket; default ~/.ccs/messenger.
";

pub fn run(args: &[String]) -> Result<()> {
    let command = args.first().context("missing messenger command")?.as_str();
    let mut positionals = Vec::new();
    let mut labels = Labels::new();
    let (mut identity, mut body, mut provider) = (None, None, None);
    let mut timeout = None;
    let (mut limit, mut offset) = (None, None);
    let mut bypass = false;
    let mut http = None;
    let mut i = 1;
    while i < args.len() {
        let arg = &args[i];
        match arg.as_str() {
            "--help" | "-h" => {
                print!("{HELP}");
                return Ok(());
            }
            "--label" | "--session" | "--message" | "--timeout" | "--limit" | "--offset" => {
                i += 1;
                let value = args.get(i).with_context(|| format!("{arg} requires a value"))?.clone();
                match arg.as_str() {
                    "--label" => {
                        let (key, val) = value.split_once('=').context("labels use key=value")?;
                        ensure!(
                            labels.insert(key.into(), val.into()).is_none(),
                            "duplicate label key"
                        );
                    }
                    "--session" => {
                        ensure!(identity.replace(value).is_none(), "duplicate --session");
                    }
                    "--message" => {
                        ensure!(body.replace(value).is_none(), "duplicate --message");
                    }
                    "--limit" => {
                        ensure!(
                            limit
                                .replace(value.parse::<usize>().context("invalid limit")?)
                                .is_none(),
                            "duplicate --limit"
                        );
                    }
                    "--offset" => {
                        ensure!(
                            offset
                                .replace(value.parse::<usize>().context("invalid offset")?)
                                .is_none(),
                            "duplicate --offset"
                        );
                    }
                    _ => {
                        let seconds: u64 = value.parse().context("timeout must be an integer")?;
                        ensure!((1..=86400).contains(&seconds), "timeout must be 1-86400 seconds");
                        ensure!(timeout.replace(seconds).is_none(), "duplicate --timeout");
                    }
                }
            }
            "--http" => {
                ensure!(command == "server", "--http only applies to server");
                i += 1;
                let addr = args
                    .get(i)
                    .context("--http requires ip:port")?
                    .parse::<std::net::SocketAddr>()
                    .context("invalid HTTP listen address")?;
                ensure!(http.replace(addr).is_none(), "duplicate --http");
            }
            "--adapter" => {
                i += 1;
                let name = args.get(i).context("--adapter requires a name")?.clone();
                ensure!(provider.replace(name).is_none(), "choose one adapter");
            }
            "--claude" | "--codex" => {
                ensure!(
                    provider.replace(arg.trim_start_matches("--").into()).is_none(),
                    "choose one adapter"
                );
            }
            "--bypass" => bypass = true,
            _ if arg.starts_with('-') => bail!("unknown messenger option: {arg}"),
            _ => positionals.push(arg.clone()),
        }
        i += 1;
    }
    let subcommand = positionals.first().map(String::as_str).unwrap_or("");
    let registration = command == "session" && subcommand == "register";
    let sending = command == "queue" || (command == "inbox" && subcommand == "send");
    ensure!(
        provider.is_none() && !bypass || registration,
        "provider options only apply to session register"
    );
    ensure!(timeout.is_none() || command == "queue", "--timeout only applies to queue");
    ensure!(
        body.is_none() || sending || command == "reply",
        "--message does not apply to this command"
    );
    ensure!(
        labels.is_empty()
            || sending
            || command == "sessions"
            || (command == "session" && matches!(subcommand, "register" | "label")),
        "--label does not apply to this command"
    );
    ensure!(
        (limit.is_none() && offset.is_none()) || (command == "inbox" && subcommand.is_empty()),
        "pagination only applies to inbox listing"
    );
    let dir = messenger::directory()?;
    let name = || -> Result<String> {
        identity
            .clone()
            .or_else(|| std::env::var("CCS_SESSION").ok())
            .map(Ok)
            .unwrap_or_else(default_name)
    };
    let one = |offset: usize| -> Result<String> {
        ensure!(
            positionals.len() == offset + 1,
            "expected exactly one name or message ID; see ccs session --help"
        );
        Ok(positionals[offset].clone())
    };
    let request = match command {
        "server" => {
            ensure!(positionals.is_empty() && identity.is_none(), "usage: ccs server");
            return messenger::serve_http(&dir, http);
        }
        "sessions" => {
            ensure!(positionals.is_empty(), "usage: ccs sessions [--label key=value]");
            Request::Sessions { labels }
        }
        "session" => match subcommand {
            "register" => Request::Register {
                session: Registration {
                    name: one(1)?,
                    labels,
                    endpoint: endpoint(provider.as_deref(), bypass)?,
                },
            },
            "label" => Request::Label { name: one(1)?, labels },
            "remove" => Request::Remove { name: one(1)? },
            _ => bail!("usage: ccs session register|label|remove <name>"),
        },
        "queue" | "inbox" if sending => {
            let offset = usize::from(command == "inbox");
            ensure!(positionals.len() <= offset + 1, "too many recipients");
            Request::Send {
                id: new_id()?,
                from: name()?,
                to: positionals.get(offset).cloned(),
                labels,
                kind: if command == "queue" { Kind::Queue } else { Kind::Inbox },
                body: body.context("--message is required")?,
            }
        }
        "inbox" => match subcommand {
            "" => Request::Inbox {
                session: name()?,
                limit: limit.unwrap_or(20),
                offset: offset.unwrap_or(0),
            },
            "ack" => Request::Ack { session: name()?, id: one(1)? },
            _ => bail!("usage: ccs inbox [send|ack]"),
        },
        "reply" => Request::Reply {
            session: name()?,
            id: one(0)?,
            body: body.context("--message is required")?,
        },
        "message" => Request::Message { session: name()?, id: one(0)? },
        _ => bail!("unknown messenger command"),
    };
    let started = Instant::now();
    if let Request::Send { id, from, .. } = &request {
        eprintln!("request {id}; inspect with: ccs message {id} --session {from}");
    }
    let mut value = messenger::call(&dir, &request).with_context(|| match &request {
        Request::Send { id, .. } => format!(
            "request {id}: if delivery is uncertain, inspect with ccs message {id} before resending"
        ),
        _ => "messenger request failed".into(),
    })?;
    if command == "queue" {
        let id = value["id"].as_str().context("server returned no message ID")?.to_owned();
        let session = name()?;
        loop {
            if value["reply"].is_string() {
                break;
            }
            if value["status"] == "failed" {
                bail!("request {id} delivery failed: {}", value["error"]);
            }
            if started.elapsed() >= Duration::from_secs(timeout.unwrap_or(300)) {
                bail!(
                    "request {id} timed out waiting for reply; it remains stored; inspect with ccs message {id} --session {session}"
                );
            }
            std::thread::sleep(Duration::from_millis(100));
            value = messenger::call(
                &dir,
                &Request::Message { session: session.clone(), id: id.clone() },
            )
            .with_context(|| format!("request {id} remains stored; unable to check reply"))?;
        }
    }
    print_json(&value)
}
fn print_json(value: &Value) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}
fn default_name() -> Result<String> {
    let thread = std::env::var("CODEX_THREAD_ID").ok();
    let socket = std::env::var("CLAUDE_CODE_MESSAGING_SOCKET").ok();
    match (thread, socket) {
        (Some(t), None) => Ok(format!("codex-{t}")),
        (None, Some(s)) => Ok(format!(
            "claude-{}",
            PathBuf::from(s).file_stem().context("invalid Claude socket")?.to_string_lossy()
        )),
        _ => bail!("set CCS_SESSION or pass --session <registered-name>"),
    }
}
fn endpoint(provider: Option<&str>, bypass: bool) -> Result<Session> {
    let home = PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?);
    let config = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".claude"));
    let codex =
        std::env::var_os("CODEX_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".codex"));
    let binary = std::env::var("CCS_CODEX_BINARY").unwrap_or_else(|_| "codex".into());
    Session::register(provider, &config, &codex, &binary, bypass)
}

fn new_id() -> Result<String> {
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(format!("m{}", bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()))
}
