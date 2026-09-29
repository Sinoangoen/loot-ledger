//! Command-line entry point: argument handling, the capture loop, and wiring
//! between the capture, storage and dashboard layers. The behaviour itself
//! lives in the library so it can be tested and reused.

use loot_ledger::{capture, game, items, proto, store, web};

use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use capture::sys::install_signal_handler;
use game::now_ms;
use store::{Journal, ReplayStats};
use web::http::{serve, SseHub};
use web::App;

/// Albion's game ports, as observed in live traffic.
const DEFAULT_PORTS: [u16; 3] = [5056, 5055, 4535];

/// How long to wait on the capture socket before servicing other work.
const READ_TIMEOUT: Duration = Duration::from_millis(250);

/// How often a quiet dashboard is told the process is still alive.
const PERIODIC: Duration = Duration::from_secs(2);

/// Set by the signal handler; the capture loop polls it.
static SHUTDOWN: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_sig: std::ffi::c_int) {
    SHUTDOWN.store(true, Ordering::Relaxed);
}

fn main() {
    let code = match run() {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("loot-ledger: {e}");
            1
        }
    };
    std::process::exit(code);
}

/// Parsed command line.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Options {
    port: u16,
    interface: Option<String>,
    journal: PathBuf,
    no_browser_hint: bool,
    replay: bool,
    refresh_items: Option<PathBuf>,
    list_interfaces: bool,
    check_only: bool,
    no_capture: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            port: 7331,
            interface: None,
            journal: PathBuf::from("loot-ledger.jsonl"),
            no_browser_hint: false,
            replay: true,
            refresh_items: None,
            list_interfaces: false,
            check_only: false,
            no_capture: false,
        }
    }
}

const USAGE: &str = "\
loot-ledger — record the loot players pick up in your Albion Online zone

USAGE:
    loot-ledger [OPTIONS]

OPTIONS:
    -i, --interface <NAME>   Capture only this interface (default: all)
    -p, --port <PORT>        Dashboard HTTP port [default: 7331]
    -j, --journal <PATH>     Append-only log file [default: ./loot-ledger.jsonl]
        --no-replay          Start empty instead of replaying the journal
        --no-capture         Serve the dashboard from the journal only
        --no-open            Do not print the dashboard URL at startup
        --refresh-items <F>  Write a fresh item table to <F> and exit
        --list-interfaces    Print capture interfaces and exit
        --check              Run all checks, then exit
    -h, --help               Show this help

PERMISSIONS:
    Reading packets needs CAP_NET_RAW. Either run with sudo, or grant it once:
        sudo setcap cap_net_raw+ep ./loot-ledger
";

fn parse_args(args: &[String]) -> Result<Options, String> {
    let mut opts = Options::default();
    let mut i = 0;

    while i < args.len() {
        let arg = args[i].as_str();
        let take = |i: &mut usize| -> Result<String, String> {
            *i += 1;
            args.get(*i)
                .cloned()
                .ok_or_else(|| format!("{arg} needs a value"))
        };

        match arg {
            "-h" | "--help" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            "-i" | "--interface" => opts.interface = Some(take(&mut i)?),
            "-p" | "--port" => {
                let v = take(&mut i)?;
                opts.port = v.parse().map_err(|_| format!("bad --port: {v}"))?;
            }
            "-j" | "--journal" => opts.journal = PathBuf::from(take(&mut i)?),
            "--no-replay" => opts.replay = false,
            "--no-capture" => opts.no_capture = true,
            "--no-open" => opts.no_browser_hint = true,
            "--refresh-items" => opts.refresh_items = Some(PathBuf::from(take(&mut i)?)),
            "--list-interfaces" => opts.list_interfaces = true,
            "--check" => opts.check_only = true,
            other => return Err(format!("unknown option: {other}")),
        }

        i += 1;
    }

    Ok(opts)
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opts = parse_args(&args)?;

    if opts.list_interfaces {
        return list_interfaces();
    }
    if let Some(path) = &opts.refresh_items {
        return refresh_items(path);
    }
    if opts.check_only {
        return run_checks();
    }

    start(opts)
}

fn list_interfaces() -> Result<(), String> {
    println!("{:<16} {:>6}  NOTE", "NAME", "INDEX");
    for iface in capture::list_interfaces() {
        let note = if iface.is_loopback {
            "loopback"
        } else {
            "candidate"
        };
        println!("{:<16} {:>6}  {}", iface.name, iface.index, note);
    }
    println!("\nPick one with: loot-ledger --interface <NAME>");
    Ok(())
}

fn refresh_items(path: &std::path::Path) -> Result<(), String> {
    eprintln!("Fetching a fresh item table...");
    items::refresh_to_file(path)
        .map(|n| println!("Wrote {n} items to {}", path.display()))
        .map_err(|e| {
            format!(
                "could not refresh items ({e}).\n\
                 The table built into the binary is still used. To adopt a newer \
                 one, save it over src/assets/items.tsv and rebuild."
            )
        })
}

/// Everything `loot-ledger --check` verifies, so permissions and interfaces can
/// be confirmed before a session starts.
fn run_checks() -> Result<(), String> {
    let mut ok = true;
    println!("loot-ledger self-check\n");

    let privs = capture::have_capture_privilege();
    println!("  [{}] CAP_NET_RAW", if privs { " ok " } else { "FAIL" });
    if !privs {
        ok = false;
        println!("        Grant it with: sudo setcap cap_net_raw+ep ./loot-ledger");
    }

    match capture::list_interfaces() {
        ifaces if !ifaces.is_empty() => println!("  [ ok ] {} interface(s) visible", ifaces.len()),
        _ => {
            println!("  [FAIL] no interfaces found in /sys/class/net");
            ok = false;
        }
    }

    let cat = items::catalogue();
    println!("  [ ok ] {} items loaded ({})", cat.len(), cat.source());

    // Decode a synthetic packet end to end, so a green check proves the
    // protocol layer works and not merely that the filesystem is readable.
    match selftest() {
        Ok(n) => println!("  [ ok ] capture and decode verified ({n} synthetic event)"),
        Err(e) => {
            println!("  [FAIL] capture: {e}");
            ok = false;
        }
    }

    println!(
        "\n{}",
        if ok {
            "All checks passed."
        } else {
            "Some checks failed."
        }
    );

    if ok {
        Ok(())
    } else {
        Err("self-check failed".into())
    }
}

/// Open a capture socket and push a synthetic packet through the real pipeline.
fn selftest() -> Result<usize, String> {
    let cap = capture::Capture::open(None, &DEFAULT_PORTS)
        .map_err(|e| format!("cannot open a capture socket: {e}"))?;
    drop(cap);

    let frame = synthetic_frame();
    let datagram = capture::parse_frame(&frame).map_err(|e| format!("frame parsing: {e:?}"))?;

    let mut parser = proto::Parser::new();
    let mut state = game::GameState::new();
    let mut seen = 0usize;

    parser.handle_packet(datagram.payload, &mut |body| {
        state.ingest(&body, now_ms());
        seen += 1;
    });

    if seen == 0 {
        return Err("synthetic packet did not decode".into());
    }
    Ok(seen)
}

/// Build an Ethernet/IPv4/UDP frame carrying one loot event.
fn synthetic_frame() -> Vec<u8> {
    use proto::p16::{command, msg, ty, EVENT_ID_KEY};

    // Event payload: reliable flag, message type, event code, one parameter.
    let mut payload = vec![0xF3u8, msg::EVENT_DATA, 1u8];
    let mut table = vec![EVENT_ID_KEY, ty::INT32];
    table.extend_from_slice(&(game::events::event::EV_OTHER_GRABBED_LOOT as i32).to_be_bytes());
    payload.extend_from_slice(&1i16.to_be_bytes());
    payload.extend_from_slice(&table);

    // SEND_RELIABLE command.
    let mut cmd = vec![command::SEND_RELIABLE, 0, 0, 0];
    cmd.extend_from_slice(&((payload.len() + 12) as i32).to_be_bytes());
    cmd.extend_from_slice(&0i32.to_be_bytes());
    cmd.extend_from_slice(&payload);

    // Photon packet header.
    let mut packet = vec![0x00, 0x00, 0x04, 0x01];
    packet.extend_from_slice(&[0u8; 8]);
    packet.extend_from_slice(&cmd);

    // UDP header.
    let udp_len = 8 + packet.len();
    let mut udp = 5056u16.to_be_bytes().to_vec();
    udp.extend_from_slice(&12345u16.to_be_bytes());
    udp.extend_from_slice(&(udp_len as u16).to_be_bytes());
    udp.extend_from_slice(&[0, 0]);
    udp.extend_from_slice(&packet);

    // IPv4 header, 20 bytes: version/IHL, TOS, total length, id, flags,
    // TTL, protocol, checksum, source, destination.
    let total = 20 + udp.len();
    let mut ip = vec![0x45, 0x00];
    ip.extend_from_slice(&(total as u16).to_be_bytes());
    ip.extend_from_slice(&[0x00, 0x00]); // identification
    ip.extend_from_slice(&[0x40, 0x00]); // flags / fragment offset
    ip.push(64); // TTL
    ip.push(17); // protocol: UDP
    ip.extend_from_slice(&[0x00, 0x00]); // checksum, not verified by the reader
    ip.extend_from_slice(&[127, 0, 0, 1]);
    ip.extend_from_slice(&[127, 0, 0, 1]);
    ip.extend_from_slice(&udp);

    // Ethernet header.
    let mut frame = vec![0x02; 6];
    frame.extend_from_slice(&[0x02; 6]);
    frame.extend_from_slice(&0x0800u16.to_be_bytes());
    frame.extend_from_slice(&ip);
    frame
}

fn start(opts: Options) -> Result<(), String> {
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "loot-ledger".into());

    if !opts.no_capture && !capture::have_capture_privilege() {
        eprintln!(
            "loot-ledger needs permission to read packets (CAP_NET_RAW).\n\
             \n  Run it with sudo:\n      sudo {exe}\n\
             \n  Or grant the capability once and run it as yourself:\n      \
             sudo setcap cap_net_raw+ep {exe}\n\
             \n  See what is available with: loot-ledger --check"
        );
        return Err("insufficient permissions".into());
    }

    let ifindex = match &opts.interface {
        Some(name) => Some(
            capture::list_interfaces()
                .into_iter()
                .find(|i| &i.name == name)
                .ok_or_else(|| format!("no such interface: {name}"))?
                .index,
        ),
        None => None,
    };

    // --- state ------------------------------------------------------------
    let mut state = game::GameState::new();
    let mut replay_stats = ReplayStats::default();

    if opts.replay {
        match store::replay(&opts.journal, &mut state) {
            Ok(stats) => {
                if stats.applied > 0 || stats.truncated {
                    eprintln!(
                        "Replayed {} record(s) from {} ({} skipped{})",
                        stats.applied,
                        opts.journal.display(),
                        stats.skipped,
                        if stats.truncated {
                            ", older records left on disk"
                        } else {
                            ""
                        }
                    );
                }
                replay_stats = stats;
            }
            Err(e) => eprintln!("Could not replay {}: {e}", opts.journal.display()),
        }
    }

    let journal = Journal::open(&opts.journal)
        .map_err(|e| format!("cannot open journal {}: {e}", opts.journal.display()))?;

    let running = Arc::new(AtomicBool::new(true));
    let shared = Arc::new(web::Shared {
        app: Mutex::new(App {
            state,
            journal,
            capture: capture::CaptureStats::default(),
            parse: proto::ParseStats::default(),
            replay: replay_stats,
            started_at_ms: now_ms(),
            started_at: Instant::now(),
            interface: opts.interface.clone(),
            filter_attached: false,
            saw_traffic: false,
            running: true,
        }),
        hub: SseHub::new(),
        running: Arc::clone(&running),
    });

    // --- capture ----------------------------------------------------------
    //
    // `--no-capture` opens no socket at all, which is how a past session is
    // reviewed: the dashboard and the journal work without CAP_NET_RAW.
    let mut cap = if opts.no_capture {
        None
    } else {
        let (cap, filter_ok) = capture::Capture::open(ifindex, &DEFAULT_PORTS)
            .map_err(|e| format!("cannot start capture: {e}"))?;
        let mut app = shared.lock();
        app.filter_attached = filter_ok;
        Some(cap)
    };
    let filter_ok = cap.as_ref().map(|c| c.has_filter()).unwrap_or(false);

    install_signal_handler(on_signal);

    // --- web --------------------------------------------------------------
    let listener = TcpListener::bind(("127.0.0.1", opts.port)).map_err(|e| {
        format!(
            "cannot bind the dashboard to 127.0.0.1:{}: {e}\n\
             Another instance is probably already running; try --port {}.",
            opts.port,
            opts.port + 1
        )
    })?;

    let addr = listener
        .local_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|_| format!("127.0.0.1:{}", opts.port));

    {
        let web_running = Arc::clone(&running);
        let web_shared = Arc::clone(&shared);
        std::thread::spawn(move || serve(listener, web_running, web::router(web_shared)));
    }

    print_banner(&opts, &addr, filter_ok);

    // --- capture loop -----------------------------------------------------
    //
    // The state is moved out from under its lock, decoded, and moved back, so
    // that the change records can be persisted and broadcast afterwards with
    // no lock held. The lock is therefore held for the length of one datagram
    // — microseconds — and never across any I/O, so neither the dashboard nor
    // the journal can stall capture.
    let mut parser = proto::Parser::new();
    let mut last_periodic = Instant::now();

    while !SHUTDOWN.load(Ordering::Relaxed) && shared.is_running() {
        let frame_len = match cap.as_mut() {
            Some(cap) => match cap.next_frame(READ_TIMEOUT) {
                Ok(Some((len, _))) => len,
                Ok(None) => 0,
                Err(e) => {
                    eprintln!("Capture read failed: {e}");
                    break;
                }
            },
            // Nothing to read from; just service the dashboard.
            None => {
                std::thread::sleep(READ_TIMEOUT);
                0
            }
        };

        let mut outbound: Vec<(&'static str, String)> = Vec::new();

        if let Some(cap) = cap.as_ref().filter(|_| frame_len > 0) {
            let datagram = capture::parse_frame(cap.frame(frame_len));

            // Phase 1: decode with the state checked out.
            let (state, changes) = {
                let mut app = shared.lock();
                let mut scratch = std::mem::take(&mut app.state);
                app.capture.frames += 1;
                app.saw_traffic = true;

                let mut changes = Vec::new();
                match &datagram {
                    Ok(d) => {
                        app.capture.udp_frames += 1;
                        parser.handle_packet(d.payload, &mut |body| {
                            changes.extend(scratch.ingest(&body, now_ms()));
                        });
                    }
                    Err(reason) => capture::record_skip(&mut app.capture, *reason),
                }

                (scratch, changes)
            };

            // Phase 2: persist and record counters.
            //
            // Records go into the buffered writer, which decides when to hit
            // the disk. Flushing per frame would cost a `write` syscall for
            // every captured packet; the periodic tick below bounds how much a
            // crash could cost instead.
            {
                let mut app = shared.lock();
                app.state = state;
                app.parse = parser.stats();
                for change in &changes {
                    if let Err(e) = app.journal.append(change) {
                        eprintln!("Journal write failed: {e}");
                    }
                }
            }

            for change in &changes {
                if let Some((event, json)) = web::change_event(change) {
                    outbound.push((event, json.to_string()));
                }
            }
        }

        // Broadcast outside every lock, so a stalled browser cannot stall capture.
        if !shared.hub.is_empty() {
            for (event, data) in &outbound {
                shared.hub.broadcast(event, data);
            }
        }

        if last_periodic.elapsed() >= PERIODIC {
            last_periodic = Instant::now();

            if let Err(e) = shared.lock().journal.flush() {
                eprintln!("Journal flush failed: {e}");
            }

            if !shared.hub.is_empty() {
                let status = {
                    let app = shared.lock();
                    web::status_json(&app, shared.hub.len())
                };
                shared.hub.broadcast("status", &status.to_string());
            }
        }
    }

    // --- shutdown ---------------------------------------------------------
    {
        let mut app = shared.lock();
        app.running = false;
        let _ = app.journal.flush();
        eprintln!(
            "\nWrote {} line(s) to {}. Goodbye.",
            app.journal.lines_written(),
            opts.journal.display()
        );
    }

    shared.stop();
    Ok(())
}

fn print_banner(opts: &Options, addr: &str, filter_ok: bool) {
    println!("\n  loot-ledger — recording loot in your current zone\n");
    println!("  Dashboard   http://{addr}");
    if !opts.no_browser_hint {
        println!("               (open it in a browser; it updates live)");
    }
    println!("  Journal     {}", opts.journal.display());
    if opts.no_capture {
        println!("  Capture     off — showing recorded history only");
    } else {
        println!(
            "  Interface   {}",
            opts.interface.as_deref().unwrap_or("all")
        );
        println!(
            "  Filter      {}",
            if filter_ok {
                "kernel-side, matches only Albion traffic"
            } else {
                "WARNING: kernel filter unavailable, reading all traffic"
            }
        );
    }
    println!("\n  Press Ctrl+C to stop.\n");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sensible() {
        let o = Options::default();
        assert_eq!(o.port, 7331);
        assert!(o.interface.is_none());
        assert!(o.replay);
    }

    #[test]
    fn parses_short_options() {
        let args: Vec<String> = [
            "-i",
            "eno1",
            "-p",
            "9000",
            "--no-replay",
            "-j",
            "/tmp/a.jsonl",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let o = parse_args(&args).unwrap();
        assert_eq!(o.interface.as_deref(), Some("eno1"));
        assert_eq!(o.port, 9000);
        assert!(!o.replay);
        assert_eq!(o.journal, PathBuf::from("/tmp/a.jsonl"));
    }

    #[test]
    fn parses_long_options() {
        let args: Vec<String> = ["--interface", "lo", "--port", "1", "--no-open"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let o = parse_args(&args).unwrap();
        assert_eq!(o.interface.as_deref(), Some("lo"));
        assert!(o.no_browser_hint);
    }

    #[test]
    fn no_capture_is_off_by_default_and_parsed() {
        assert!(!Options::default().no_capture);
        let args: Vec<String> = vec!["--no-capture".to_string()];
        assert!(parse_args(&args).unwrap().no_capture);
    }

    #[test]
    fn rejects_bad_input() {
        assert!(parse_args(&["--nope".to_string()]).is_err());
        assert!(parse_args(&["--port".to_string()]).is_err());
        assert!(parse_args(&["--port".to_string(), "abc".to_string()]).is_err());
        assert!(parse_args(&["--interface".to_string()]).is_err());
    }

    #[test]
    fn synthetic_frame_survives_the_whole_pipeline() {
        let frame = synthetic_frame();
        let d = capture::parse_frame(&frame).expect("frame should parse");
        // Traffic arrives *from* Albion's port to an ephemeral client port.
        assert_eq!(d.src_port, 5056);
        assert!(!d.payload.is_empty());

        let mut parser = proto::Parser::new();
        let mut state = game::GameState::new();
        let mut seen = 0;
        parser.handle_packet(d.payload, &mut |body| {
            state.ingest(&body, 0);
            seen += 1;
        });

        assert_eq!(seen, 1);
        assert_eq!(parser.stats().messages, 1);
        assert_eq!(parser.stats().decode_errors, 0);
        assert_eq!(state.events_seen()[&275], 1);
    }
}
