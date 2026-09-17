use std::{
    fs,
    io::{BufRead, BufReader, Write},
    net::{TcpListener, TcpStream},
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
fn subscriptions_send_initial_event_and_changes_but_not_reads() {
    let dir = std::env::temp_dir().join(format!("ccs-stream-{}", std::process::id()));
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
        assert!(server.child.try_wait().unwrap().is_none());
        if TcpStream::connect(addr).is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let token = fs::read_to_string(server.dir.join("http.token")).unwrap();
    let mut stream = TcpStream::connect(addr).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    write!(
        stream,
        "GET /events HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {}\r\n\r\n",
        token.trim()
    )
    .unwrap();
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    assert!(line.starts_with("HTTP/1.1 200"), "stream subscription unavailable: {line}");
    while line != "\r\n" {
        line.clear();
        reader.read_line(&mut line).unwrap();
    }
    line.clear();
    reader.read_line(&mut line).unwrap();
    assert!(line.starts_with("data: "), "initial revision event: {line}");
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    };
    let (events, updates) = mpsc::channel();
    let mut workers = Vec::new();
    for remote in [false, true] {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let events = events.clone();
        let dir = server.dir.clone();
        let token = token.clone();
        let worker = std::thread::spawn(move || {
            let changed = || {
                events.send(remote).unwrap();
            };
            let result = if remote {
                ccs::messenger_http::watch(
                    &format!("http://{addr}/rpc"),
                    token.trim(),
                    &flag,
                    changed,
                )
            } else {
                ccs::messenger::watch(&dir, &flag, changed)
            };
            if let Err(error) = &result {
                eprintln!("watch remote={remote}: {error:#}");
            }
            result
        });
        workers.push((stop, worker));
    }
    let first = updates.recv_timeout(Duration::from_secs(3)).unwrap();
    assert_ne!(
        first,
        updates.recv_timeout(Duration::from_secs(3)).unwrap(),
        "both subscriptions send initial event"
    );
    ccs::messenger::call(
        &server.dir,
        &ccs::messenger::Request::Sessions { labels: Default::default() },
    )
    .unwrap();
    assert!(
        updates.recv_timeout(Duration::from_millis(1200)).is_err(),
        "reads/heartbeats must not invalidate"
    );
    let registration = ccs::messenger::Request::Register {
        session: ccs::messenger::Registration {
            name: "synthetic-session".into(),
            labels: Default::default(),
            endpoint: ccs::adapters::Session::Claude(ccs::adapters::ClaudeCode {
                socket: server.dir.join("server.sock").to_str().unwrap().into(),
                config: server.dir.clone(),
                bypass: false,
            }),
        },
    };
    ccs::messenger::call(&server.dir, &registration).unwrap();
    let first = updates.recv_timeout(Duration::from_secs(3)).unwrap();
    assert_ne!(
        first,
        updates.recv_timeout(Duration::from_secs(3)).unwrap(),
        "both transports see local mutations"
    );
    ccs::messenger::call(
        &server.dir,
        &ccs::messenger::Request::Label {
            name: "synthetic-session".into(),
            labels: [("test".into(), "value".into())].into(),
        },
    )
    .unwrap();
    let first = updates.recv_timeout(Duration::from_secs(3)).unwrap();
    assert_ne!(first, updates.recv_timeout(Duration::from_secs(3)).unwrap());
    // A healthy stream must outlive the regular RPC client's ten-second timeout.
    assert!(updates.recv_timeout(Duration::from_secs(11)).is_err());
    ccs::messenger::call(
        &server.dir,
        &ccs::messenger::Request::Remove { name: "synthetic-session".into() },
    )
    .unwrap();
    let first = updates.recv_timeout(Duration::from_secs(3)).unwrap();
    assert_ne!(
        first,
        updates.recv_timeout(Duration::from_secs(3)).unwrap(),
        "streams remain live after ten seconds"
    );
    let started = std::time::Instant::now();
    for (stop, _) in &workers {
        stop.store(true, Ordering::SeqCst);
    }
    for (_, worker) in workers {
        worker.join().unwrap().unwrap();
    }
    assert!(started.elapsed() < Duration::from_secs(4), "cancellation must close streams promptly");
    let stop = AtomicBool::new(false);
    assert!(
        ccs::messenger_http::watch(&format!("http://{addr}"), "wrong-token", &stop, || panic!(
            "unauthorized stream event"
        ))
        .is_err()
    );
    // Fill the separate subscription capacity; ordinary RPC must still work.
    let mut subscriptions = Vec::new();
    // Earlier server-side watchers discover cancellation on their next heartbeat.
    std::thread::sleep(Duration::from_millis(2200));
    drop(reader);
    for _ in 0..31 {
        let mut stream =
            std::os::unix::net::UnixStream::connect(server.dir.join("server.sock")).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        stream.write_all(b"{\"op\":\"subscribe\"}\n").unwrap();
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        assert!(line.contains("revision"), "subscription rejected: {line}");
        subscriptions.push(reader);
    }
    assert!(
        ccs::messenger::call(
            &server.dir,
            &ccs::messenger::Request::Sessions { labels: Default::default() }
        )
        .unwrap()
        .as_array()
        .unwrap()
        .is_empty()
    );
}

#[test]
fn remote_watch_cancels_even_when_peer_stops_heartbeats() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    };
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (release, hold) = mpsc::channel::<()>();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        loop {
            line.clear();
            reader.read_line(&mut line).unwrap();
            if line == "\r\n" {
                break;
            }
        }
        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {\"revision\":0}\n\n").unwrap();
        let _ = hold.recv_timeout(Duration::from_secs(6));
    });
    let stop = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&stop);
    let (ready, initial) = mpsc::channel();
    let watcher = std::thread::spawn(move || {
        ccs::messenger_http::watch(&format!("http://{addr}"), "synthetic-token", &flag, || {
            ready.send(()).unwrap();
        })
    });
    initial.recv_timeout(Duration::from_secs(3)).unwrap();
    let started = std::time::Instant::now();
    stop.store(true, Ordering::SeqCst);
    watcher.join().unwrap().unwrap();
    assert!(started.elapsed() < Duration::from_secs(4));
    let _ = release.send(());
    server.join().unwrap();
}
