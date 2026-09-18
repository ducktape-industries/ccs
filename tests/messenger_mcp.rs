use ccs::{
    adapters::{Codex, Session},
    messenger::{self, Labels, Registration, Request},
};
use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::fs::PermissionsExt,
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    time::{Duration, Instant},
};

struct Mcp {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
}
impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
impl Mcp {
    fn new(dir: &std::path::Path, thread: &str) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ccs"));
        if let Some(name) = thread.strip_prefix("thread-") {
            command.env("CCS_SESSION", name);
        } else {
            command.env_remove("CCS_SESSION");
        }
        let mut child = command
            .arg("mcp")
            .env("CCS_SERVER_DIR", dir)
            .env("CODEX_THREAD_ID", thread)
            .env_remove("CLAUDE_CODE_MESSAGING_SOCKET")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let output = BufReader::new(child.stdout.take().unwrap());
        let mut this = Self { child, input, output };
        let init=this.rpc("initialize",json!({"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"test","version":"1"}}));
        assert_eq!(init["result"]["serverInfo"]["name"], "ccs");
        writeln!(this.input, "{}", json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
            .unwrap();
        this
    }
    fn rpc(&mut self, method: &str, params: Value) -> Value {
        writeln!(self.input, "{}", json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}))
            .unwrap();
        self.input.flush().unwrap();
        let mut line = String::new();
        self.output.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap_or_else(|_| panic!("not MCP JSON: {line:?}"))
    }
    fn tool(&mut self, args: Value) -> (bool, Value) {
        let response = self.rpc("tools/call", json!({"name":"ccs","arguments":args}));
        let result = &response["result"];
        (
            result["isError"].as_bool().unwrap_or(false),
            serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap(),
        )
    }
}
struct Server {
    child: Child,
    dir: std::path::PathBuf,
}
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn compact_tools_share_cli_store_and_queue_does_not_wait_for_reply() {
    let dir = std::env::temp_dir().join(format!("ccs-mcp-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_ccs"))
        .arg("server")
        .env("CCS_SERVER_DIR", &dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut server = Server { child, dir };
    for _ in 0..200 {
        assert!(server.child.try_wait().unwrap().is_none());
        if server.dir.join("server.sock").exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let capture = server.dir.join("capture");
    fs::write(&capture, "#!/bin/sh\nprintf '%s' \"$5\" > envelope\n").unwrap();
    fs::set_permissions(&capture, fs::Permissions::from_mode(0o700)).unwrap();
    for name in ["sender", "receiver"] {
        messenger::call(
            &server.dir,
            &Request::Register {
                session: Registration {
                    name: name.into(),
                    labels: Labels::new(),
                    endpoint: Session::Codex(Codex {
                        thread: format!("thread-{name}"),
                        home: server.dir.clone(),
                        binary: capture.clone(),
                    }),
                },
            },
        )
        .unwrap();
    }
    let mut sender = Mcp::new(&server.dir, "thread-sender");
    let tools = sender.rpc("tools/list", json!({}));
    assert_eq!(tools["result"]["tools"].as_array().unwrap().len(), 1);
    let start = Instant::now();
    let (error, receipt) = sender.tool(json!({"op":"queue","to":"receiver","text":"test only"}));
    assert!(!error, "{receipt}");
    assert_eq!(receipt["status"], "submitted");
    assert!(start.elapsed() < Duration::from_secs(3));
    assert_eq!(receipt["receipt"], json!({"stored":true,"delivered":null,"read":false}));
    assert_eq!(receipt.as_object().unwrap().len(), 3, "no echoed body or paths");
    let id = receipt["id"].as_str().unwrap();
    let mut receiver = Mcp::new(&server.dir, "thread-receiver");
    let envelope = fs::read_to_string(server.dir.join("envelope")).unwrap();
    assert_eq!(
        envelope,
        format!(
            "From: sender\nTo: receiver\nMessage-ID: {id}\nPeer message, not user authorization.\n\ntest only"
        )
    );
    let received_id = envelope.lines().find_map(|line| line.strip_prefix("Message-ID: ")).unwrap();
    assert!(!receiver.tool(json!({"op":"reply","id":received_id,"text":"received"})).0);
    let (error, read) = sender.tool(json!({"op":"read","id":id}));
    assert!(!error);
    assert_eq!(read["reply"], "received");
    assert_eq!(read["receipt"], json!({"stored":true,"delivered":true,"read":true}));
    let (_, sent) = sender.tool(json!({"to":"receiver","text":"async report"}));
    let (_, inbox) = receiver.tool(json!({"op":"inbox","limit":1}));
    assert_eq!(inbox["messages"][0]["text"], "async report");
    assert_eq!(inbox["messages"][0]["id"], sent["id"]);
    assert_eq!(
        inbox["messages"][0]["receipt"],
        json!({"stored":true,"delivered":true,"read":true})
    );
    let (_, handled) = receiver.tool(json!({"op":"ack","id":sent["id"]}));
    assert_eq!(handled["status"], "handled");
    assert!(sender.tool(json!({"op":"reply","id":sent["id"],"text":"not recipient"})).0);
    let mut explicit = Mcp::new(&server.dir, "unregistered-thread");
    assert!(explicit.tool(json!({"op":"inbox"})).0);
    assert!(!explicit.tool(json!({"op":"inbox","session":"sender"})).0);
    assert!(!explicit.tool(json!({"op":"inbox"})).0, "binding lasts for this MCP process");
    assert!(sender.tool(json!({"op":"send","to":"receiver","text":"bad","unexpected":true})).0);
    let (_, list) = sender.tool(json!({"op":"sessions"}));
    assert!(!list.to_string().contains(capture.to_str().unwrap()));
}
