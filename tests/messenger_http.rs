use std::{
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    os::unix::fs::PermissionsExt,
    process::{Child, Command, Stdio},
    time::Duration,
};

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
fn authenticated_http_shares_local_store_and_bounds_input() {
    let dir = std::env::temp_dir().join(format!("ccs-http-test-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let child = Command::new(env!("CARGO_BIN_EXE_ccs"))
        .args(["server", "--http", &addr.to_string()])
        .env("CCS_SERVER_DIR", &dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut server = Server { child, dir };
    for _ in 0..200 {
        assert!(server.child.try_wait().unwrap().is_none(), "HTTP server exited before listening");
        if server.dir.join("server.sock").exists() && TcpStream::connect(addr).is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let token_path = server.dir.join("http.token");
    let token = fs::read_to_string(&token_path).expect("HTTP must create private token");
    assert_eq!(fs::metadata(token_path).unwrap().permissions().mode() & 0o777, 0o600);
    assert!(token.trim().len() >= 32);
    let request = |headers: &str, body: &str| {
        let mut stream = TcpStream::connect(addr).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        write!(stream, "POST /rpc HTTP/1.1\r\nHost: localhost\r\n{headers}\r\n{body}").unwrap();
        let mut response = String::new();
        if let Err(error) = stream.read_to_string(&mut response) {
            assert_eq!(error.kind(), std::io::ErrorKind::ConnectionReset);
            assert!(!response.is_empty(), "server reset without responding");
        }
        response
    };
    assert!(request("Content-Length: 999999999\r\n", "").starts_with("HTTP/1.1 401"));
    let auth = format!("Authorization: Bearer {}\r\n", token.trim());
    assert!(
        request(&format!("{auth}Content-Length: 999999999\r\n"), "").starts_with("HTTP/1.1 413")
    );
    let body = r#"{"op":"sessions","labels":{}}"#;
    let response = request(&format!("{auth}Content-Length: {}\r\n", body.len()), body);
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(response.contains(r#"{"ok":[]}"#));
    let registration: ccs::messenger::Request = serde_json::from_value(serde_json::json!({"op":"register","session":{"name":"fake","labels":{},"endpoint":{"provider":"claude","socket":server.dir.join("server.sock"),"config":server.dir,"bypass":false}}})).unwrap();
    ccs::messenger::call(&server.dir, &registration).unwrap();
    let response = request(&format!("{auth}Content-Length: {}\r\n", body.len()), body);
    assert!(response.contains("fake"), "HTTP should see locally registered sessions: {response}");
    let sessions = ccs::messenger::Request::Sessions { labels: Default::default() };
    let url = format!("http://{addr}");
    assert_eq!(
        ccs::messenger_http::call(&url, token.trim(), &sessions).unwrap()[0]["name"],
        "fake"
    );
    let mut second = serde_json::to_value(&registration).unwrap();
    second["session"]["name"] = "sender".into();
    ccs::messenger::call(&server.dir, &serde_json::from_value(second).unwrap()).unwrap();
    ccs::messenger::call(
        &server.dir,
        &ccs::messenger::Request::Send {
            id: "test-inbox".into(),
            from: "sender".into(),
            to: Some("fake".into()),
            labels: Default::default(),
            kind: ccs::messenger::Kind::Inbox,
            body: "isolated test".into(),
        },
    )
    .unwrap();
    let ack = ccs::messenger::Request::Ack { session: "fake".into(), id: "test-inbox".into() };
    assert_eq!(ccs::messenger_http::call(&url, token.trim(), &ack).unwrap()["status"], "read");
    let reply = ccs::messenger::Request::Reply {
        session: "fake".into(),
        id: "test-inbox".into(),
        body: "test reply".into(),
    };
    ccs::messenger_http::call(&format!("{url}/rpc"), token.trim(), &reply).unwrap();
    let message =
        ccs::messenger::Request::Message { session: "sender".into(), id: "test-inbox".into() };
    assert_eq!(ccs::messenger::call(&server.dir, &message).unwrap()["reply"], "test reply");
    let history = ccs::messenger::Request::History {
        session: "fake".into(),
        kind: Some(ccs::messenger::Kind::Inbox),
        limit: 20,
        offset: 0,
    };
    assert_eq!(
        ccs::messenger_http::call(&url, token.trim(), &history).unwrap()["messages"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let room = ccs::messenger::Request::RoomHistory { kind: None, limit: 20, offset: 0 };
    assert!(
        ccs::messenger_http::call(&url, token.trim(), &room).unwrap()["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|message| message["id"] == "test-inbox")
    );
    let post = ccs::messenger::Request::Post {
        id: "from-app".into(),
        to: "missing".into(),
        body: "hello".into(),
    };
    assert!(
        ccs::messenger_http::call(&url, token.trim(), &post)
            .unwrap_err()
            .to_string()
            .contains("recipient must match")
    );
    // A rejection may race with the client's request body still arriving.
    for _ in 0..20 {
        let error = ccs::messenger_http::call(&url, "wrong", &sessions).unwrap_err();
        assert!(error.to_string().contains("unauthorized"), "unexpected auth error: {error:#}");
    }
    assert!(
        request(&format!("{auth}Content-Length: 1\r\nContent-Length: 1\r\n"), "")
            .starts_with("HTTP/1.1 400")
    );
    assert!(
        request(&format!("{auth}Transfer-Encoding: chunked\r\n"), "").starts_with("HTTP/1.1 400")
    );
    assert!(request(&format!("X-Large: {}\r\n", "a".repeat(8200)), "").starts_with("HTTP/1.1 431"));
    let body = r#"{"op":"remove","name":"fake"}"#;
    assert!(
        request(&format!("{auth}Content-Length: {}\r\n", body.len()), body)
            .starts_with("HTTP/1.1 403")
    );
}

#[test]
fn client_rejects_unsafe_urls_redirects_and_oversized_responses() {
    let request = ccs::messenger::Request::Sessions { labels: Default::default() };
    for url in [
        "file:///tmp/socket",
        "ftp://localhost",
        "http://user:password@localhost",
        "http://localhost/#fragment",
        "http://localhost/?query",
    ] {
        assert!(ccs::messenger_http::call(url, "token", &request).is_err(), "accepted {url}");
    }
    for response in [
        "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:1/stolen\r\nContent-Length: 0\r\n\r\n"
            .to_owned(),
        format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
            1024 * 1024 + 10,
            format_args!(r#"{{"ok":"{}"}}"#, "x".repeat(1024 * 1024 + 1))
        ),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let worker = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
            let mut head = Vec::new();
            while !head.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                stream.read_exact(&mut byte).unwrap();
                head.push(byte[0]);
            }
            let _ = stream.write_all(response.as_bytes());
        });
        let error = ccs::messenger_http::call(&url, "token", &request).unwrap_err();
        assert!(!error.to_string().contains("Connection refused"), "followed redirect: {error}");
        worker.join().unwrap();
    }
}

#[test]
fn insecure_existing_token_stops_http_startup() {
    for (suffix, contents, mode) in
        [("empty", "", 0o600), ("public", "secret", 0o644), ("spaces", "bad token", 0o600)]
    {
        let dir =
            std::env::temp_dir().join(format!("ccs-http-token-{}-{suffix}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("http.token");
        fs::write(&path, contents).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_ccs"))
            .args(["server", "--http", "127.0.0.1:0"])
            .env("CCS_SERVER_DIR", &dir)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert_eq!(fs::read_to_string(path).unwrap(), contents, "must not silently rotate token");
        fs::remove_dir_all(dir).unwrap();
    }
}
