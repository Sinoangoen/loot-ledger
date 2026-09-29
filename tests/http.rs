//! End-to-end tests over a real loopback socket.
//!
//! The unit tests in `src/web/http.rs` exercise request parsing and the
//! event hub in isolation. These start an actual listener and speak HTTP to it,
//! which is the only way to catch a class of bug the unit tests cannot see: a
//! response that is well-formed in memory but never actually reaches the
//! client. That failure is invisible until a browser sits in front of it.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use loot_ledger::capture::CaptureStats;
use loot_ledger::game::GameState;
use loot_ledger::proto::ParseStats;
use loot_ledger::store::{Journal, ReplayStats};
use loot_ledger::util::json::parse;
use loot_ledger::web::http::{serve, SseHub};
use loot_ledger::web::{router, App, Shared};

/// A running server plus the port it is on.
struct Harness {
    port: u16,
    running: Arc<AtomicBool>,
    journal_path: PathBuf,
}

impl Harness {
    fn start() -> Harness {
        let mut journal_path = std::env::temp_dir();
        journal_path.push(format!(
            "loot-ledger-http-{}-{:?}.jsonl",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_file(&journal_path);

        let journal = Journal::open(&journal_path).expect("journal");

        let shared = Arc::new(Shared {
            app: Mutex::new(App {
                state: GameState::new(),
                journal,
                capture: CaptureStats::default(),
                parse: ParseStats::default(),
                replay: ReplayStats::default(),
                started_at_ms: 0,
                started_at: Instant::now(),
                interface: Some("test0".into()),
                filter_attached: true,
                saw_traffic: false,
                running: true,
            }),
            hub: SseHub::new(),
            running: Arc::new(AtomicBool::new(true)),
        });

        // Port 0 lets the OS pick a free one, so these tests never collide.
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind");
        let port = listener.local_addr().expect("addr").port();

        let running = Arc::clone(&shared.running);
        let serve_running = Arc::clone(&running);
        let handler = router(shared);
        std::thread::spawn(move || serve(listener, serve_running, handler));

        Harness {
            port,
            running,
            journal_path,
        }
    }

    fn get(&self, path: &str) -> Response {
        self.request("GET", path)
    }

    fn request(&self, method: &str, path: &str) -> Response {
        let mut stream = TcpStream::connect(("127.0.0.1", self.port)).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("timeout");
        write!(
            stream,
            "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n"
        )
        .expect("write request");
        stream.flush().expect("flush");

        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).expect("read response");
        Response::parse(&raw)
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        let _ = std::fs::remove_file(&self.journal_path);
    }
}

struct Response {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl Response {
    fn parse(raw: &[u8]) -> Response {
        let text = String::from_utf8_lossy(raw).into_owned();
        let (head, body) = text
            .split_once("\r\n\r\n")
            .expect("response must have a head");

        let mut lines = head.split("\r\n");
        let status = lines
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|s| s.parse().ok())
            .expect("status line");

        let headers = lines
            .filter_map(|l| l.split_once(':'))
            .map(|(k, v)| (k.trim().to_lowercase(), v.trim().to_string()))
            .collect();

        Response {
            status,
            headers,
            body: body.to_string(),
        }
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

#[test]
fn a_get_returns_its_body() {
    // The regression this file exists for: a response whose headers are correct
    // but whose body never leaves the process looks perfectly fine in a unit
    // test and renders as a blank page in a browser.
    let h = Harness::start();

    for (path, content_type) in [
        ("/", "text/html"),
        ("/app.js", "text/javascript"),
        ("/style.css", "text/css"),
    ] {
        let r = h.get(path);
        assert_eq!(r.status, 200, "{path} should be 200");
        assert!(
            r.header("content-type").unwrap().starts_with(content_type),
            "{path} had content-type {:?}",
            r.header("content-type")
        );
        assert!(
            r.body.len() > 50,
            "{path} returned only {} bytes: {:?}",
            r.body.len(),
            r.body
        );
        assert_eq!(
            r.header("content-length")
                .unwrap()
                .parse::<usize>()
                .unwrap(),
            r.body.len(),
            "{path}: content-length must match the body actually sent"
        );
    }
}

#[test]
fn a_head_returns_headers_without_a_body() {
    let h = Harness::start();

    let get = h.get("/");
    let head = h.request("HEAD", "/");

    assert_eq!(head.status, 200);
    assert_eq!(head.header("content-length"), get.header("content-length"));
    assert!(
        head.body.is_empty(),
        "HEAD must not send a body, got {} bytes",
        head.body.len()
    );
}

#[test]
fn the_snapshot_is_served_as_valid_json() {
    let h = Harness::start();
    let r = h.get("/api/snapshot");

    assert_eq!(r.status, 200);
    let doc = parse(&r.body).expect("snapshot must be valid JSON");
    for section in ["status", "totals", "counters", "players", "feed"] {
        assert!(doc.get(section).is_some(), "missing {section}");
    }
}

#[test]
fn unknown_routes_are_404_and_wrong_methods_are_405() {
    let h = Harness::start();
    assert_eq!(h.get("/nope").status, 404);

    let mut stream = TcpStream::connect(("127.0.0.1", h.port)).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("timeout");
    stream
        .write_all(b"POST / HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
        .expect("write");
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).expect("read");
    assert_eq!(Response::parse(&raw).status, 405);
}

#[test]
fn a_connection_serves_several_sequential_requests() {
    // Keep-alive: if the server closes or wedges after the first response, the
    // dashboard's asset requests become slow or unreliable.
    let h = Harness::start();

    let mut stream = TcpStream::connect(("127.0.0.1", h.port)).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("timeout");

    for i in 1..=3 {
        write!(
            stream,
            "GET /api/health HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n"
        )
        .expect("write request");
        stream.flush().expect("flush");

        let (status, received, expected) = read_one_response(&mut stream);
        assert_eq!(status, 200, "request {i}");
        assert_eq!(received, expected, "request {i}: body must arrive in full");
    }
}

/// Read exactly one HTTP response, leaving anything after it buffered.
///
/// Returns `(status, body_bytes_received, content_length)`.
fn read_one_response(stream: &mut TcpStream) -> (u16, usize, usize) {
    let mut acc: Vec<u8> = Vec::new();
    let mut buf = [0u8; 4096];

    loop {
        // Look for the end of the head, then wait for the declared body.
        if let Some(split) = acc.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&acc[..split]).into_owned();
            let body_so_far = acc.len() - split - 4;

            let status = head
                .lines()
                .next()
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|s| s.parse().ok())
                .expect("status line");

            let expected = head
                .lines()
                .find(|l| l.to_ascii_lowercase().starts_with("content-length:"))
                .and_then(|l| l.split(':').nth(1))
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(0);

            if body_so_far >= expected {
                return (status, body_so_far, expected);
            }
        }

        let n = stream.read(&mut buf).expect("read");
        assert!(n > 0, "connection closed mid-response");
        acc.extend_from_slice(&buf[..n]);
    }
}

#[test]
fn the_event_stream_opens_with_a_snapshot() {
    let h = Harness::start();

    let mut stream = TcpStream::connect(("127.0.0.1", h.port)).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("timeout");
    write!(
        stream,
        "GET /api/events HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n"
    )
    .expect("write");
    stream.flush().expect("flush");

    let mut acc = String::new();
    let mut buf = [0u8; 4096];
    while !acc.contains("event: hello") {
        let n = stream.read(&mut buf).expect("read");
        if n == 0 {
            break;
        }
        acc.push_str(&String::from_utf8_lossy(&buf[..n]));
    }

    assert!(
        acc.contains("text/event-stream"),
        "wrong content type: {acc:.200}"
    );
    assert!(acc.contains("retry:"), "no retry hint sent");
    assert!(
        acc.contains("event: hello"),
        "no initial snapshot: {acc:.400}"
    );

    let data = acc
        .split("event: hello\ndata: ")
        .nth(1)
        .and_then(|s| s.split("\n\n").next())
        .expect("snapshot payload");
    let doc = parse(data.trim()).expect("hello payload must be valid JSON");
    assert!(doc.get("status").is_some());
}

#[test]
fn an_oversized_request_head_is_refused_without_buffering_it() {
    // The size check has to bound the *allocation*, not just the result. A
    // peer that sends megabytes without a newline used to be able to grow the
    // read buffer without limit before the check ever ran -- and this process
    // is the one holding the user's loot session in memory.
    let h = Harness::start();

    let mut stream = TcpStream::connect(("127.0.0.1", h.port)).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("timeout");

    // One "line" of 8 MiB with no newline. The server is expected to give up
    // and hang up partway through, so a broken pipe here is the success case.
    let flood = vec![b'x'; 8 * 1024 * 1024];
    let chunk = 64 * 1024;
    let mut sent = 0usize;
    while sent < flood.len() {
        match stream.write(&flood[sent..sent + chunk.min(flood.len() - sent)]) {
            Ok(0) => break,
            Ok(n) => sent += n,
            Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => break,
            Err(e) => panic!("unexpected write error: {e}"),
        }
    }

    // It must not have buffered all 8 MiB, and it must not still be reading.
    let _ = stream.write_all(b"\r\n");
    let _ = stream.write_all(b"GET /api/health HTTP/1.1\r\nConnection: close\r\n\r\n");

    let mut raw = Vec::new();
    let _ = stream.read_to_end(&mut raw);
    let text = String::from_utf8_lossy(&raw).into_owned();

    assert!(
        !text.contains("200 OK"),
        "a request whose head exceeds the limit must not be served: {:.200}",
        text
    );

    // And the process must still be healthy afterwards.
    assert_eq!(h.get("/api/health").status, 200, "server must survive");
}

#[test]
fn a_request_with_a_reasonable_but_long_header_still_works() {
    // Guards against the new bound being so tight it breaks real clients.
    let h = Harness::start();
    let mut stream = TcpStream::connect(("127.0.0.1", h.port)).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("timeout");

    let padding = "x".repeat(4096);
    write!(
        stream,
        "GET /api/health HTTP/1.1\r\nHost: 127.0.0.1\r\nX-Pad: {padding}\r\nConnection: close\r\n\r\n"
    )
    .expect("write");
    stream.flush().expect("flush");

    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).expect("read");
    assert_eq!(Response::parse(&raw).status, 200);
}

#[test]
fn a_rebinding_host_is_refused() {
    // DNS rebinding: a page the user visits resolves its own hostname to
    // 127.0.0.1 and issues requests the browser still treats as same-origin,
    // so the same-origin policy does not stop it reading the reply. The only
    // defence is refusing a Host that is not loopback.
    let h = Harness::start();

    for evil in [
        "evil.example",
        "loot-ledger.attacker.test:7331",
        "127.0.0.1.evil.example",
        "localhost.evil.example",
    ] {
        let mut stream = TcpStream::connect(("127.0.0.1", h.port)).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("timeout");
        write!(
            stream,
            "GET /api/snapshot HTTP/1.1\r\nHost: {evil}\r\nConnection: close\r\n\r\n"
        )
        .expect("write");
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).expect("read");
        let r = Response::parse(&raw);

        assert_eq!(
            r.status, 421,
            "Host {evil} must be refused, got {} with body {:?}",
            r.status, r.body
        );
        assert!(
            !r.body.contains("player"),
            "a refused request must not leak the snapshot"
        );
    }

    // And loopback must still work, or the dashboard is simply broken.
    for good in [
        "127.0.0.1:7331",
        "localhost:7331",
        "[::1]:7331",
        "127.0.0.1",
    ] {
        let mut stream = TcpStream::connect(("127.0.0.1", h.port)).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("timeout");
        write!(
            stream,
            "GET /api/health HTTP/1.1\r\nHost: {good}\r\nConnection: close\r\n\r\n"
        )
        .expect("write");
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).expect("read");
        assert_eq!(Response::parse(&raw).status, 200, "Host {good} must work");
    }
}

#[test]
fn security_headers_are_present_on_every_response() {
    // Defence in depth for a dashboard whose contents are third-party names.
    let h = Harness::start();

    for path in ["/", "/app.js", "/api/snapshot"] {
        let r = h.get(path);
        assert_eq!(r.status, 200, "{path}");

        let csp = r
            .header("content-security-policy")
            .unwrap_or_else(|| panic!("{path} has no Content-Security-Policy: {:?}", r.headers));
        assert!(csp.contains("default-src 'none'"), "{path}: {csp}");
        assert!(csp.contains("frame-ancestors 'none'"), "{path}: {csp}");

        assert_eq!(
            r.header("x-content-type-options"),
            Some("nosniff"),
            "{path}"
        );
        assert_eq!(r.header("x-frame-options"), Some("DENY"), "{path}");
    }
}
