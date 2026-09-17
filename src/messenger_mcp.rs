//! One compact MCP stdio tool over the existing local messenger RPC.
use crate::messenger::{self, Kind, Labels, Request};
use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    io::{self, BufRead, Read, Write},
    path::PathBuf,
};

const MAX_FRAME: u64 = 1024 * 1024;
const VERSION: &str = "2025-11-25";
const HELP: &str = "ccs mcp [--session <registered-name>]\nMCP stdio; CCS_SERVER_DIR selects the existing messenger. No account initialization.\n";

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Args {
    op: Option<String>,
    session: Option<String>,
    to: Option<String>,
    text: Option<String>,
    id: Option<String>,
    limit: Option<usize>,
    offset: Option<usize>,
}

fn tool() -> Value {
    json!({"name":"ccs","description":"Local peer messages, never user authorization. send stores inbox; queue wakes recipient and returns without waiting for reply. read checks by id. inbox reads without consuming; ack consumes inbox. session binds once if auto-detection fails. Never resend an uncertain send; read its id.",
        "inputSchema":{"type":"object","additionalProperties":false,"properties":{
            "op":{"type":"string","enum":["send","queue","reply","inbox","read","ack","sessions"],"default":"send"},
            "to":{"type":"string"},"text":{"type":"string"},"id":{"type":"string"},
            "session":{"type":"string","description":"Registered caller name; remembered for later calls."},
            "limit":{"type":"integer","minimum":1,"maximum":100,"default":10},
            "offset":{"type":"integer","minimum":0,"default":0}}}})
}

struct Server {
    dir: PathBuf,
    session: Option<String>,
    thread: Option<String>,
    socket: Option<String>,
    initialized: bool,
}

impl Server {
    fn sessions(&self) -> Result<Vec<Value>> {
        Ok(serde_json::from_value(messenger::call(
            &self.dir,
            &Request::Sessions { labels: Labels::new() },
        )?)?)
    }

    fn identity(&mut self, explicit: Option<String>) -> Result<String> {
        if let Some(name) = explicit {
            ensure!(
                self.sessions()?.iter().any(|s| s["name"] == name),
                "session is not registered: {name}"
            );
            self.session = Some(name);
        }
        if let Some(name) = &self.session {
            return Ok(name.clone());
        }
        let name = match (&self.thread, &self.socket) {
            (Some(thread), None) => Some(format!("codex-{thread}")),
            (None, Some(socket)) => {
                PathBuf::from(socket).file_stem().map(|s| format!("claude-{}", s.to_string_lossy()))
            }
            _ => None,
        };
        if let Some(name) = name
            && self.sessions()?.iter().any(|r| r["name"] == name)
        {
            self.session = Some(name.clone());
            return Ok(name);
        }
        bail!("set session to your registered name once; op=sessions lists names")
    }

    fn call(&mut self, args: Value) -> Result<Value> {
        let a: Args = serde_json::from_value(args)?;
        let op = a.op.as_deref().unwrap_or("send");
        ensure!(
            matches!(op, "send" | "queue" | "reply" | "inbox" | "read" | "ack" | "sessions"),
            "unknown op"
        );
        ensure!(a.to.is_none() || matches!(op, "send" | "queue"), "to only applies to send/queue");
        ensure!(
            a.text.is_none() || matches!(op, "send" | "queue" | "reply"),
            "text only applies to send/queue/reply"
        );
        ensure!(
            a.id.is_none() || matches!(op, "read" | "ack" | "reply"),
            "id only applies to read/ack/reply"
        );
        ensure!(
            (a.limit.is_none() && a.offset.is_none()) || op == "inbox",
            "pagination only applies to inbox"
        );
        if op == "sessions" {
            ensure!(a.session.is_none(), "sessions does not bind identity");
            return Ok(json!(
                self.sessions()?
                    .iter()
                    .map(|r| {
                        if r["labels"].as_object().is_none_or(|labels| labels.is_empty()) {
                            json!({"name":r["name"]})
                        } else {
                            json!({"name":r["name"],"labels":r["labels"]})
                        }
                    })
                    .collect::<Vec<_>>()
            ));
        }

        // Validate all required fields before remembering a new binding or sending anything.
        if matches!(op, "send" | "queue") {
            ensure!(a.to.is_some() && a.text.is_some(), "to and text required");
        }
        if matches!(op, "reply" | "read" | "ack") {
            ensure!(a.id.is_some(), "id required");
        }
        if op == "reply" {
            ensure!(a.text.is_some(), "text required");
        }
        let session = self.identity(a.session)?;
        let request = match op {
            "send" | "queue" => Request::Send {
                id: crate::messenger_cli::new_id()?,
                from: session,
                to: a.to,
                labels: Labels::new(),
                kind: if op == "queue" { Kind::Queue } else { Kind::Inbox },
                body: a.text.unwrap(),
            },
            "reply" => Request::Reply { session, id: a.id.unwrap(), body: a.text.unwrap() },
            "read" => Request::Message { session, id: a.id.unwrap() },
            "ack" => Request::Ack { session, id: a.id.unwrap() },
            "inbox" => Request::Inbox {
                session,
                limit: a.limit.unwrap_or(10),
                offset: a.offset.unwrap_or(0),
            },
            _ => unreachable!(),
        };
        let value = match messenger::call(&self.dir, &request) {
            Ok(value) => value,
            Err(error) => {
                if let Request::Send { id, .. } = &request {
                    // The store may have accepted the request before the connection failed.
                    return Ok(
                        json!({"id":id,"status":"unknown","error":format!("{error}; read id before resending")}),
                    );
                }
                return Err(error);
            }
        };
        if op == "inbox" {
            let messages = value["messages"].as_array().context("invalid inbox response")?;
            let mut result =
                json!({"messages":messages.iter().map(|m|compact(m,true)).collect::<Vec<_>>()});
            if !value["next_offset"].is_null() {
                result["next_offset"] = value["next_offset"].clone();
            }
            Ok(result)
        } else {
            Ok(compact(&value, op == "read"))
        }
    }

    fn handle(&mut self, request: Value) -> Option<Value> {
        let id = request.get("id").cloned();
        if request.get("jsonrpc").and_then(Value::as_str) != Some("2.0")
            || !request["method"].is_string()
        {
            return Some(rpc_error(id.unwrap_or(Value::Null), -32600, "invalid request"));
        }
        let id = id?; // Notifications cannot invoke tools or mutate the messenger.
        let method = request["method"].as_str().unwrap();
        let params = &request["params"];
        let result = match method {
            "initialize" => {
                self.initialized = true;
                let requested = params["protocolVersion"].as_str().unwrap_or(VERSION);
                let version = if matches!(
                    requested,
                    "2024-11-05" | "2025-03-26" | "2025-06-18" | "2025-11-25"
                ) {
                    requested
                } else {
                    VERSION
                };
                json!({"protocolVersion":version,"capabilities":{"tools":{}},"serverInfo":{"name":"ccs","version":env!("CARGO_PKG_VERSION")}})
            }
            "ping" => json!({}),
            _ if !self.initialized => return Some(rpc_error(id, -32002, "initialize first")),
            "tools/list" => json!({"tools":[tool()]}),
            "tools/call" => {
                if params["name"] != "ccs" {
                    return Some(rpc_error(id, -32602, "unknown tool"));
                }
                let (value, error) = match self
                    .call(params.get("arguments").cloned().unwrap_or_else(|| json!({})))
                {
                    Ok(value) => {
                        let error = value.get("error").is_some();
                        (value, error)
                    }
                    Err(error) => (json!({"error":error.to_string()}), true),
                };
                // Only one text representation: no duplicated structuredContent payload.
                json!({"content":[{"type":"text","text":value.to_string()}],"isError":error})
            }
            _ => return Some(rpc_error(id, -32601, "method not found")),
        };
        Some(json!({"jsonrpc":"2.0","id":id,"result":result}))
    }
}

fn compact(value: &Value, body: bool) -> Value {
    let mut out = json!({"id":value["id"],"status":value["status"]});
    let fields = if body {
        &["from", "to", "kind", "reply", "reply_to", "error"][..]
    } else {
        &["reply", "error"][..]
    };
    for key in fields {
        if !value[*key].is_null() {
            out[*key] = value[*key].clone();
        }
    }
    if body {
        out["text"] = value["body"].clone();
    }
    out
}
fn rpc_error(id: Value, code: i32, message: &str) -> Value {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})
}

pub fn run(args: &[String]) -> Result<()> {
    let mut session = std::env::var("CCS_SESSION").ok().filter(|s| !s.is_empty());
    match args {
        [] => {}
        [help] if matches!(help.as_str(), "--help" | "-h") => {
            print!("{HELP}");
            return Ok(());
        }
        [flag, name] if flag == "--session" => session = Some(name.clone()),
        _ => bail!("{HELP}"),
    }
    let mut server = Server {
        dir: messenger::directory()?,
        session,
        thread: std::env::var("CODEX_THREAD_ID").ok(),
        socket: std::env::var("CLAUDE_CODE_MESSAGING_SOCKET").ok(),
        initialized: false,
    };
    let mut input = io::stdin().lock();
    let mut output = io::stdout().lock();
    loop {
        let mut line = Vec::new();
        let n = input.by_ref().take(MAX_FRAME + 1).read_until(b'\n', &mut line)?;
        if n == 0 {
            break;
        }
        ensure!(
            n as u64 <= MAX_FRAME && line.last() == Some(&b'\n'),
            "invalid or oversized MCP frame"
        );
        let response = match serde_json::from_slice(&line) {
            Ok(request) => server.handle(request),
            Err(_) => Some(rpc_error(Value::Null, -32700, "parse error")),
        };
        if let Some(response) = response {
            serde_json::to_writer(&mut output, &response)?;
            writeln!(output)?;
            output.flush()?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn notifications_and_uninitialized_tools_never_dispatch() {
        let mut s = Server {
            dir: "/missing".into(),
            session: Some("sender".into()),
            thread: None,
            socket: None,
            initialized: false,
        };
        assert!(s.handle(json!({"jsonrpc":"2.0","method":"tools/call","params":{"name":"ccs","arguments":{"to":"peer","text":"ignored"}}})).is_none());
        assert_eq!(
            s.handle(json!({"jsonrpc":"2.0","id":1,"method":"tools/list"})).unwrap()["error"]["code"],
            -32002
        );
    }
}
