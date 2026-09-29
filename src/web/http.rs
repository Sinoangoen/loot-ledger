//! A minimal HTTP/1.1 server.
//!
//! loot-ledger serves exactly one dashboard to the local machine, so this
//! implements only what that needs: `GET`, a handful of routes, and a
//! long-lived Server-Sent Events stream. It is a few hundred lines of
//! `std::net` and removes any web-server dependency from the binary.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Refuse request heads larger than this. A dashboard request is well under a
/// kilobyte; anything larger is a mistake or an attack.
const MAX_HEAD_BYTES: usize = 16 * 1024;

/// How long a client may take to send its request before being dropped.
const HEAD_TIMEOUT: Duration = Duration::from_secs(10);

/// Ceiling on simultaneously served connections.
///
/// Each costs a thread, and the dashboard realistically has a handful open.
/// Without a ceiling, any local process could open sockets and hold them until
/// the process ran out of memory — on the machine that is also recording the
/// user's game session.
const MAX_CONNECTIONS: usize = 64;

/// A parsed request. Only the method and path matter here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub method: String,
    /// Path with any query string removed.
    pub path: String,
    /// Query parameters, first value wins for a repeated key.
    pub query: BTreeMap<String, String>,
    /// Header names, lowercased, to their values.
    pub headers: BTreeMap<String, String>,
}

impl Request {
    /// Whether the client asked for the connection to be closed once this
    /// response is sent. HTTP/1.0 clients always do; HTTP/1.1 clients only
    /// when they send `Connection: close`.
    pub fn wants_close(&self) -> bool {
        self.headers
            .get("connection")
            .is_some_and(|v| v.eq_ignore_ascii_case("close"))
    }
}

/// A response the server knows how to write.
///
/// Only the simple variants are comparable; a `Stream` is a live connection
/// and comparing one would be meaningless, so `PartialEq` is implemented by
/// hand rather than derived.
pub enum Response {
    /// A complete body with a content length.
    Body {
        status: u16,
        content_type: &'static str,
        body: String,
    },
    /// A redirect.
    Redirect(String),
    /// No body.
    Empty(u16),
    /// Hand the connection over to a streaming handler.
    Stream(Box<dyn FnOnce(TcpStream) + Send>),
}

impl PartialEq for Response {
    fn eq(&self, other: &Response) -> bool {
        match (self, other) {
            (
                Response::Body {
                    status: a,
                    content_type: b,
                    body: c,
                },
                Response::Body {
                    status: d,
                    content_type: e,
                    body: f,
                },
            ) => a == d && b == e && c == f,
            (Response::Empty(a), Response::Empty(b)) => a == b,
            (Response::Redirect(a), Response::Redirect(b)) => a == b,
            // A stream is compared by identity of its position: two different
            // streams are never equal, and a stream is never a plain body.
            (Response::Stream(_), Response::Stream(_)) => false,
            _ => false,
        }
    }
}

impl std::fmt::Debug for Response {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Response::Body {
                status,
                content_type,
                body,
            } => f
                .debug_struct("Body")
                .field("status", status)
                .field("content_type", content_type)
                .field("len", &body.len())
                .finish(),
            Response::Redirect(to) => f.debug_tuple("Redirect").field(to).finish(),
            Response::Empty(status) => f.debug_tuple("Empty").field(status).finish(),
            Response::Stream(_) => f.write_str("Stream(..)"),
        }
    }
}

impl Response {
    /// A 200 with a JSON body.
    pub fn json(body: String) -> Response {
        Response::Body {
            status: 200,
            content_type: "application/json; charset=utf-8",
            body,
        }
    }

    /// A 200 with an HTML body.
    pub fn html(body: String) -> Response {
        Response::Body {
            status: 200,
            content_type: "text/html; charset=utf-8",
            body,
        }
    }

    /// A 200 with a CSS body.
    pub fn css(body: String) -> Response {
        Response::Body {
            status: 200,
            content_type: "text/css; charset=utf-8",
            body,
        }
    }

    /// A 200 with a JavaScript body.
    pub fn js(body: String) -> Response {
        Response::Body {
            status: 200,
            content_type: "text/javascript; charset=utf-8",
            body,
        }
    }

    /// A 404.
    pub fn not_found() -> Response {
        Response::Body {
            status: 404,
            content_type: "text/plain; charset=utf-8",
            body: "not found\n".into(),
        }
    }

    /// A 405.
    pub fn method_not_allowed() -> Response {
        Response::Body {
            status: 405,
            content_type: "text/plain; charset=utf-8",
            body: "method not allowed\n".into(),
        }
    }
}

/// Serve requests until `running` is cleared.
///
/// One thread per connection, bounded by [`MAX_CONNECTIONS`]. A dashboard has
/// a handful of clients, each either short-lived or a single long-lived SSE
/// stream, so a pool would add machinery for nothing — but "one thread per
/// connection" with no ceiling means any local process can exhaust memory by
/// opening sockets and holding them, so the count is capped and excess
/// connections are refused rather than served.
pub fn serve<F>(listener: TcpListener, running: Arc<AtomicBool>, handler: F)
where
    F: Fn(Request) -> Response + Send + Sync + 'static,
{
    let handler = Arc::new(handler);
    let live = Arc::new(AtomicUsize::new(0));

    for stream in listener.incoming() {
        if !running.load(Ordering::Relaxed) {
            break;
        }

        match stream {
            Ok(stream) => {
                if live.load(Ordering::Relaxed) >= MAX_CONNECTIONS {
                    // Politely refuse rather than accept and then fail. The
                    // client sees a 503 and can retry.
                    let mut stream = stream;
                    let _ = write_response(
                        &mut stream,
                        503,
                        "text/plain; charset=utf-8",
                        b"too many connections\\n",
                        None,
                        false,
                        true,
                    );
                    let _ = stream.shutdown(Shutdown::Both);
                    continue;
                }

                live.fetch_add(1, Ordering::Relaxed);
                let handler = Arc::clone(&handler);
                let live_count = Arc::clone(&live);

                std::thread::spawn(move || {
                    let _ = handle_connection(stream, handler.as_ref());
                    live_count.fetch_sub(1, Ordering::Relaxed);
                });
            }
            Err(_) => {
                // A failed accept is usually a transient descriptor limit.
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

fn handle_connection<F>(stream: TcpStream, handler: &F) -> std::io::Result<()>
where
    F: Fn(Request) -> Response,
{
    stream.set_read_timeout(Some(HEAD_TIMEOUT))?;
    // The SSE stream is long-lived, so the write side must not time out.
    stream.set_write_timeout(None)?;

    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = stream.try_clone()?;

    loop {
        let request = match read_request(&mut reader) {
            Ok(Some(r)) => r,
            // Clean close, or a client that hung up before asking.
            Ok(None) => break,
            Err(e) => {
                let _ = write_simple(
                    &mut writer,
                    400,
                    "text/plain; charset=utf-8",
                    &e.to_string(),
                );
                break;
            }
        };

        let send_body = request.method != "HEAD";

        match handler(request.clone()) {
            Response::Stream(f) => {
                // The stream handler owns the connection from here; there is
                // nothing left for this function to say or shut down.
                f(stream);
                return Ok(());
            }
            Response::Redirect(to) => {
                write_response(
                    &mut writer,
                    302,
                    "text/plain; charset=utf-8",
                    b"",
                    Some(&format!("location: {to}\r\n")),
                    false,
                    true,
                )?;
                break;
            }
            Response::Empty(status) => {
                write_response(
                    &mut writer,
                    status,
                    "text/plain; charset=utf-8",
                    b"",
                    None,
                    false,
                    send_body,
                )?;
            }
            Response::Body {
                status,
                content_type,
                body,
            } => {
                let keep_alive = !wants_close(&request);
                write_response(
                    &mut writer,
                    status,
                    content_type,
                    body.as_bytes(),
                    None,
                    keep_alive,
                    send_body,
                )?;
                if !keep_alive {
                    break;
                }
            }
        }
    }

    let _ = stream.shutdown(Shutdown::Both);
    Ok(())
}

fn wants_close(request: &Request) -> bool {
    // With HTTP/1.1 the default is keep-alive, so the connection is closed
    // only when the client asks for it, or for HEAD, which has no body.
    request.method == "HEAD" || request.wants_close()
}

fn write_simple(
    writer: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &str,
) -> std::io::Result<()> {
    write_response(
        writer,
        status,
        content_type,
        body.as_bytes(),
        None,
        false,
        true,
    )
}

fn write_response(
    writer: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
    extra_headers: Option<&str>,
    keep_alive: bool,
    send_body: bool,
) -> std::io::Result<()> {
    let mut head = String::with_capacity(512);
    head.push_str(&format!("HTTP/1.1 {status} {}\r\n", reason(status)));
    head.push_str(&format!("content-type: {content_type}\r\n"));
    head.push_str(&format!("content-length: {}\r\n", body.len()));
    head.push_str("cache-control: no-store\r\n");
    head.push_str(CONTENT_SECURITY_POLICY);
    head.push_str("x-content-type-options: nosniff\r\n");
    head.push_str("x-frame-options: DENY\r\n");
    head.push_str("referrer-policy: no-referrer\r\n");
    if let Some(extra) = extra_headers {
        head.push_str(extra);
    }
    head.push_str(if keep_alive {
        "connection: keep-alive\r\n\r\n"
    } else {
        "connection: close\r\n\r\n"
    });

    writer.write_all(head.as_bytes())?;
    // A HEAD response carries the headers the GET would have produced, but no
    // body, even though content-length still describes it.
    if send_body && !body.is_empty() {
        writer.write_all(body)?;
    }
    writer.flush()
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        302 => "Found",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        421 => "Misdirected Request",
        503 => "Service Unavailable",
        500 => "Internal Server Error",
        _ => "Unknown",
    }
}

/// Read up to and including the next newline, replacing the contents of `out`.
///
/// Unlike `BufRead::read_until`, this never allocates more than `limit` bytes:
/// once `out` reaches the limit the read stops and reports truncation instead
/// of continuing to grow. Returns the number of bytes written into `out`, or 0
/// at EOF.
///
/// `limit` must bound the *allocation*, not merely the result. A peer that
/// sends a gigabyte without a newline is not malformed input to be detected
/// after the fact — by then the memory is already gone.
fn read_line_bounded(
    reader: &mut BufReader<TcpStream>,
    out: &mut Vec<u8>,
    limit: usize,
) -> std::io::Result<usize> {
    out.clear();
    let mut appended = 0usize;

    loop {
        let (found_newline, consumed) = {
            let available = match reader.fill_buf() {
                Ok(b) => b,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            };
            if available.is_empty() {
                return Ok(appended);
            }

            match available.iter().position(|&b| b == b'\n') {
                Some(i) => {
                    let take = (i + 1).min(available.len());
                    let room = limit.saturating_sub(out.len());
                    out.extend_from_slice(&available[..take.min(room)]);
                    (true, take)
                }
                None => {
                    let room = limit.saturating_sub(out.len());
                    let take = available.len().min(room);
                    out.extend_from_slice(&available[..take]);
                    (false, take)
                }
            }
        };

        reader.consume(consumed);
        appended += consumed;

        if found_newline {
            return Ok(appended);
        }

        if out.len() >= limit {
            // Truncated. The caller's size check rejects the request; the point
            // is that we stop reading rather than keep allocating.
            return Ok(appended);
        }
    }
}

/// Read one request head.
///
/// `Ok(None)` means the peer closed the connection cleanly.
fn read_request(reader: &mut BufReader<TcpStream>) -> Result<Option<Request>, String> {
    let mut head = Vec::with_capacity(512);
    let mut line = Vec::new();

    loop {
        // The read is bounded, not just the result: `read_until` would happily
        // grow without limit on a line that never ends, and this process holds
        // the user's whole loot session in memory. The limit has to bound the
        // allocation, so it is enforced during the read.
        let n = read_line_bounded(reader, &mut line, MAX_HEAD_BYTES)
            .map_err(|e| format!("read failed: {e}"))?;

        if n == 0 {
            return if head.is_empty() {
                Ok(None)
            } else {
                Err("connection closed mid-request".into())
            };
        }

        head.extend_from_slice(&line);
        if head.len() > MAX_HEAD_BYTES {
            return Err("request head too large".into());
        }

        if line == b"\r\n" || line == b"\n" {
            break;
        }
    }

    let text = String::from_utf8_lossy(&head);
    let mut lines = text
        .split("\r\n")
        .flat_map(|l| l.split('\n'))
        .filter(|l| !l.is_empty());

    let request_line = lines.next().ok_or("empty request")?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().ok_or("missing method")?.to_uppercase();
    let target = parts.next().ok_or("missing path")?.to_string();

    let (path, query_string) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target, String::new()),
    };

    let mut query = BTreeMap::new();
    for pair in query_string.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        query.insert(percent_decode(k), percent_decode(v));
    }

    Ok(Some(Request {
        method,
        path,
        query,
        headers: {
            let mut h = BTreeMap::new();
            for line in lines {
                if let Some((k, v)) = line.split_once(':') {
                    h.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
                }
            }
            h
        },
    }))
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;

    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(b) => {
                        out.push(b);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }

    String::from_utf8_lossy(&out).into_owned()
}

/// Response headers applied to every reply.
///
/// The dashboard loads no inline script, no inline style, and no `data:` or
/// `blob:` URL — verified — so this can be strict. It is defence in depth:
/// the client is clean today, and this is the net under it for the day
/// someone is less careful.
const CONTENT_SECURITY_POLICY: &str = concat!(
    "content-security-policy: ",
    "default-src 'none'; ",
    "style-src 'self'; ",
    "script-src 'self'; ",
    "connect-src 'self'; ",
    "img-src 'self'; ",
    "base-uri 'none'; ",
    "form-action 'none'; ",
    "frame-ancestors 'none'",
    "\r\n",
);

/// True when a `Host` header names the loopback interface.
///
/// This is the defence against DNS rebinding. A page the user visits can
/// resolve its own hostname to 127.0.0.1 and issue requests that the browser
/// still considers *same-origin*, so the same-origin policy will not stop it
/// reading the response. The only thing that stops it is refusing a request
/// whose `Host` is not a loopback name — and a browser always sends `Host`, so
/// its absence (HTTP/1.0, some CLI clients) is not an attack path.
pub fn is_loopback_host(host: Option<&str>) -> bool {
    let Some(host) = host else {
        return true;
    };

    let host = host.trim();
    if host.is_empty() {
        return true;
    }

    // Bracketed IPv6, e.g. "[::1]:7331".
    let hostname = if let Some(rest) = host.strip_prefix('[') {
        rest.split(']').next().unwrap_or("")
    } else {
        // Otherwise drop an optional ":port". Hostnames never contain a colon,
        // so the first colon is the port separator.
        host.split(':').next().unwrap_or("")
    };

    // 0.0.0.0 is deliberately absent: it is not a loopback name, and the
    // server only ever binds 127.0.0.1.
    matches!(hostname, "localhost" | "127.0.0.1" | "::1")
}

/// How many frames a single dashboard may fall behind by before it is
/// disconnected.
///
/// Each frame is a whole loot record, and a busy zone produces hundreds a
/// second. A client that opens the stream and then stops reading would
/// otherwise accumulate frames forever, so the queue is bounded and a client
/// that fills it is dropped. Losing a stalled viewer is always better than
/// growing without bound, and the browser reconnects on its own.
const MAX_CLIENT_BACKLOG: usize = 256;

/// Lock a mutex, recovering from poisoning instead of panicking.
///
/// Poisoning means some thread panicked while holding the lock. The data
/// behind these locks is plain counters and a client list, not a
/// half-updated invariant, so continuing is safe — and it is the difference
/// between one bad HTTP connection and the whole capture process dying, since
/// every later `.expect` on a poisoned lock would panic in turn.
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A registry of Server-Sent Events clients.
///
/// Slow clients are dropped rather than allowed to block the capture loop or
/// exhaust memory.
pub struct SseHub {
    clients: Mutex<Vec<(u64, SyncSender<String>)>>,
    next_id: AtomicU64,
    dropped: AtomicU64,
}

impl SseHub {
    /// An empty hub.
    pub fn new() -> SseHub {
        SseHub {
            clients: Mutex::new(Vec::new()),
            next_id: AtomicU64::new(1),
            dropped: AtomicU64::new(0),
        }
    }

    /// Register a client, returning its id and the receiving end of its channel.
    pub fn subscribe(&self) -> (u64, Receiver<String>) {
        let (tx, rx) = std::sync::mpsc::sync_channel(MAX_CLIENT_BACKLOG);
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        lock(&self.clients).push((id, tx));
        (id, rx)
    }

    /// Remove a client that has gone away.
    pub fn unsubscribe(&self, id: u64) {
        lock(&self.clients).retain(|(cid, _)| *cid != id);
    }

    /// How many clients are attached.
    pub fn len(&self) -> usize {
        lock(&self.clients).len()
    }

    /// True when nobody is listening.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// How many sends have failed because a client fell behind.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Send one already-formatted event to every client.
    ///
    /// Returns how many received it. A client is removed if its queue is full
    /// (it has stopped reading) or its channel has closed (it has hung up);
    /// neither is allowed to accumulate indefinitely, because this runs on the
    /// capture path where a stalled viewer must not become the process's
    /// memory problem.
    pub fn broadcast(&self, event: &str, data: &str) -> usize {
        let frame = format!("event: {event}\ndata: {data}\n\n");

        let mut clients = lock(&self.clients);
        let before = clients.len();

        clients.retain(|(_, tx)| match tx.try_send(frame.clone()) {
            Ok(()) => true,
            // Full: the client is not draining. Drop it and let it reconnect.
            Err(TrySendError::Full(_)) => false,
            // Disconnected: it hung up.
            Err(TrySendError::Disconnected(_)) => false,
        });

        let delivered = clients.len();
        let lost = (before - delivered) as u64;
        if lost > 0 {
            self.dropped.fetch_add(lost, Ordering::Relaxed);
        }

        delivered
    }
}

impl Default for SseHub {
    fn default() -> Self {
        SseHub::new()
    }
}

/// Write the head of an SSE response, then stream `frames` until it ends.
pub fn stream_events<F>(mut stream: TcpStream, mut frames: F) -> std::io::Result<()>
where
    F: FnMut(&mut dyn FnMut(&str) -> std::io::Result<()>) -> std::io::Result<()>,
{
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;

    let head = "HTTP/1.1 200 OK\r\n\
                content-type: text/event-stream\r\n\
                cache-control: no-store\r\n\
                connection: keep-alive\r\n\
                x-accel-buffering: no\r\n\
                \r\n\
                retry: 2000\n\n";

    stream.write_all(head.as_bytes())?;
    stream.flush()?;

    let mut send = |frame: &str| -> std::io::Result<()> {
        stream.write_all(frame.as_bytes())?;
        stream.flush()
    };

    let result = frames(&mut send);

    let _ = stream.shutdown(Shutdown::Both);
    result
}

/// Format one SSE frame.
pub fn sse_frame(event: &str, data: &str) -> String {
    format!("event: {event}\ndata: {data}\n\n")
}

/// Drain whatever a disconnected client left in its channel.
pub fn discard_pending(rx: &Receiver<String>) {
    while rx.try_recv().is_ok() {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_frame_has_a_blank_line_terminator() {
        let f = sse_frame("loot", "{}");
        assert_eq!(f, "event: loot\ndata: {}\n\n");
    }

    #[test]
    fn sse_hub_delivers_to_every_subscriber() {
        let hub = SseHub::new();
        let (_a, ra) = hub.subscribe();
        let (_b, rb) = hub.subscribe();

        assert_eq!(hub.broadcast("loot", "{\"n\":1}"), 2);
        assert!(ra.try_recv().unwrap().contains("{\"n\":1}"));
        assert!(rb.try_recv().unwrap().contains("{\"n\":1}"));
    }

    #[test]
    fn a_client_that_stops_reading_is_dropped_not_buffered_forever() {
        // An unbounded queue means one stalled browser tab grows the capture
        // process's memory by a loot record per event, forever. The queue is
        // bounded, so a client that fills it is disconnected instead.
        let hub = SseHub::new();
        let (id, _rx) = hub.subscribe();

        let mut delivered = 0;
        for i in 0..(MAX_CLIENT_BACKLOG * 3) {
            delivered = hub.broadcast("loot", &format!("{{\"i\":{i}}}"));
            if delivered == 0 {
                break;
            }
        }

        assert_eq!(delivered, 0, "the stalled client must be dropped");
        assert_eq!(hub.len(), 0);
        assert!(hub.dropped() >= 1);
        let _ = id;
    }

    #[test]
    fn a_client_that_keeps_reading_stays_connected() {
        let hub = SseHub::new();
        let (id, rx) = hub.subscribe();

        for i in 0..(MAX_CLIENT_BACKLOG * 3) {
            assert_eq!(hub.broadcast("loot", &format!("{{\"i\":{i}}}")), 1);
            // Drain each frame so the queue never fills.
            let _ = rx.try_recv().expect("a frame should be waiting");
        }

        assert_eq!(hub.len(), 1, "a healthy client must not be dropped");
        hub.unsubscribe(id);
    }

    #[test]
    fn sse_hub_forgets_clients_that_hang_up() {
        let hub = SseHub::new();
        let (id, rx) = hub.subscribe();
        drop(rx);

        assert_eq!(hub.broadcast("loot", "{}"), 0);
        assert_eq!(hub.len(), 0);
        assert_eq!(hub.dropped(), 1);
        let _ = id;
    }

    #[test]
    fn unsubscribing_stops_delivery() {
        let hub = SseHub::new();
        let (id, rx) = hub.subscribe();
        hub.unsubscribe(id);
        assert!(hub.is_empty());
        assert_eq!(hub.broadcast("loot", "{}"), 0);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn percent_decoding_handles_the_usual_shapes() {
        assert_eq!(percent_decode("a%20b"), "a b");
        assert_eq!(percent_decode("a+b"), "a b");
        assert_eq!(percent_decode("Gr%C3%BC%C3%9Fe"), "Grüße");
        assert_eq!(percent_decode("plain"), "plain");
        assert_eq!(percent_decode("%zz"), "%zz");
        assert_eq!(percent_decode("trailing%"), "trailing%");
    }
}
