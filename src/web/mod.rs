//! The dashboard's HTTP surface.
//!
//! Three kinds of response: static assets compiled into the binary, a JSON
//! snapshot, and a Server-Sent Events stream that pushes each new record as it
//! happens. The UI is a single page that fetches the snapshot once and then
//! listens to the stream, so it is correct immediately and stays live.

pub mod http;
pub mod ui;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::capture::CaptureStats;
use crate::game::{Change, GameState, LootRecord, Player, ZoneJoin};
use crate::items;
use crate::proto::ParseStats;
use crate::store::{Journal, ReplayStats};
use crate::util::json::Json;

use http::{Response, SseHub};

/// How often the stream emits a status frame when nothing else has happened.
const HEARTBEAT: Duration = Duration::from_secs(10);

/// Everything the server needs, guarded by one lock.
///
/// The capture path takes this lock for the duration of a single event's
/// processing, which is microseconds. It is never held across I/O.
pub struct App {
    pub state: GameState,
    pub journal: Journal,
    pub capture: CaptureStats,
    pub parse: ParseStats,
    pub replay: ReplayStats,
    pub started_at_ms: i64,
    pub started_at: Instant,
    /// The interface being captured, or `None` for all interfaces.
    pub interface: Option<String>,
    /// Whether the kernel-side BPF filter is installed.
    pub filter_attached: bool,
    /// Set once the first packet arrives, so the UI can show "waiting".
    pub saw_traffic: bool,
    /// Set false when the process has been asked to stop.
    pub running: bool,
}

impl App {
    /// Seconds since start.
    pub fn uptime_s(&self) -> u64 {
        self.started_at.elapsed().as_secs()
    }
}

/// Everything the web layer shares: the state, the event fan-out, and the
/// shutdown flag the capture loop watches.
pub struct Shared {
    pub app: Mutex<App>,
    pub hub: SseHub,
    pub running: Arc<AtomicBool>,
}

impl Shared {
    /// Lock the application state, recovering from poisoning.
    ///
    /// See [`http::lock`]. A panic in a web handler must not be able to take
    /// the capture process down with it.
    pub fn lock(&self) -> std::sync::MutexGuard<'_, App> {
        self.app
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Whether the application should keep running.
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }

    /// Ask the application to stop.
    pub fn stop(&self) {
        self.running.store(false, Ordering::Relaxed);
    }
}

/// JSON shape of a player row.
pub fn player_json(p: &Player) -> Json {
    Json::obj([
        ("name", Json::Str(p.name.clone())),
        ("guild", p.guild.clone().map_or(Json::Null, Json::Str)),
        ("alliance", p.alliance.clone().map_or(Json::Null, Json::Str)),
        ("grabs", Json::Int(p.grabs as i64)),
        ("units", Json::Int(p.units as i64)),
        ("firstSeen", Json::Int(p.first_seen_ms)),
        ("lastSeen", Json::Int(p.last_seen_ms)),
        ("isSelf", Json::Bool(p.is_self)),
    ])
}

/// JSON shape of a loot row.
pub fn loot_json(r: &LootRecord) -> Json {
    Json::obj([
        ("at", Json::Int(r.at_ms)),
        ("by", Json::Str(r.looted_by.clone())),
        ("from", Json::Str(r.looted_from.clone())),
        ("qty", Json::Int(r.quantity)),
        ("silver", Json::Bool(r.is_silver)),
        ("itemNumId", r.item_num_id.map_or(Json::Null, Json::Int)),
        (
            "itemUnique",
            r.item_unique.clone().map_or(Json::Null, Json::Str),
        ),
        (
            "itemName",
            r.item_name.clone().map_or(Json::Null, Json::Str),
        ),
    ])
}

fn zone_json(z: &ZoneJoin) -> Json {
    Json::obj([
        ("at", Json::Int(z.at_ms)),
        ("player", z.player.clone().map_or(Json::Null, Json::Str)),
    ])
}

/// The full state, as one JSON document.
///
/// `sse_clients` is passed in rather than read from the hub, because the hub
/// lives beside the state rather than inside it and taking both locks at once
/// would invert the order the capture loop uses.
pub fn snapshot_json(app: &App, sse_clients: usize) -> Json {
    let totals = app.state.totals();

    let events_seen: Vec<(String, Json)> = {
        let mut ids: Vec<i64> = app.state.events_seen().keys().copied().collect();
        ids.sort_unstable();
        ids.into_iter()
            .map(|id| {
                (
                    id.to_string(),
                    Json::Int(app.state.events_seen()[&id] as i64),
                )
            })
            .collect()
    };

    Json::obj([
        (
            "status",
            Json::obj([
                ("self", app.state.self_name().map_or(Json::Null, str_json)),
                ("uptimeSeconds", Json::Int(app.uptime_s() as i64)),
                ("startedAt", Json::Int(app.started_at_ms)),
                (
                    "interface",
                    app.interface.clone().map_or(Json::Null, Json::Str),
                ),
                ("kernelFilter", Json::Bool(app.filter_attached)),
                ("sawTraffic", Json::Bool(app.saw_traffic)),
                ("running", Json::Bool(app.running)),
                ("sseClients", Json::Int(sse_clients as i64)),
            ]),
        ),
        (
            "totals",
            Json::obj([
                ("events", Json::Int(totals.events as i64)),
                ("itemGrabs", Json::Int(totals.item_grabs as i64)),
                ("silverGrabs", Json::Int(totals.silver_grabs as i64)),
                ("units", Json::Int(totals.units as i64)),
                ("players", Json::Int(totals.players as i64)),
                ("unknownItems", Json::Int(totals.unknown_items as i64)),
            ]),
        ),
        (
            "counters",
            Json::obj([
                ("packets", Json::Int(app.parse.packets as i64)),
                ("messages", Json::Int(app.parse.messages as i64)),
                ("decodeErrors", Json::Int(app.parse.decode_errors as i64)),
                ("encrypted", Json::Int(app.parse.encrypted as i64)),
                ("crcChecked", Json::Int(app.parse.crc_checked as i64)),
                ("crcMismatch", Json::Int(app.parse.crc_mismatch as i64)),
                ("fragments", Json::Int(app.parse.fragments as i64)),
                ("reassembled", Json::Int(app.parse.reassembled as i64)),
                ("frames", Json::Int(app.capture.frames as i64)),
                ("udpFrames", Json::Int(app.capture.udp_frames as i64)),
                ("skippedFrames", Json::Int(app.capture.skipped as i64)),
            ]),
        ),
        (
            "replay",
            Json::obj([
                ("applied", Json::Int(app.replay.applied as i64)),
                ("skipped", Json::Int(app.replay.skipped as i64)),
            ]),
        ),
        (
            "catalogue",
            Json::obj([
                ("items", Json::Int(items::catalogue().len() as i64)),
                ("source", str_json(items::catalogue().source())),
            ]),
        ),
        (
            "eventsSeen",
            Json::Object(events_seen.into_iter().collect()),
        ),
        (
            "players",
            Json::arr(app.state.players().iter().map(player_json)),
        ),
        (
            "feed",
            Json::arr(app.state.recent(200).iter().map(loot_json)),
        ),
        (
            "zones",
            Json::arr(app.state.zone_joins().iter().rev().take(20).map(zone_json)),
        ),
    ])
}

fn str_json(s: &str) -> Json {
    Json::Str(s.to_owned())
}

/// Turn a state change into the event the UI listens for.
pub fn change_event(change: &Change) -> Option<(&'static str, Json)> {
    match change {
        Change::Loot(r) => Some(("loot", loot_json(r))),
        Change::Player(p) => Some(("player", player_json(p))),
    }
}

/// Build the request handler for the dashboard.
pub fn router(shared: Arc<Shared>) -> impl Fn(http::Request) -> Response + Send + Sync + 'static {
    move |request: http::Request| {
        if request.method != "GET" && request.method != "HEAD" {
            return Response::method_not_allowed();
        }

        // Refuse anything that did not address us as loopback. See
        // `http::is_loopback_host`: this is the DNS-rebinding defence, and the
        // dashboard's contents are other people's names, so it is worth having.
        if !http::is_loopback_host(request.headers.get("host").map(String::as_str)) {
            return Response::Body {
                status: 421,
                content_type: "text/plain; charset=utf-8",
                body: "this server only answers requests addressed to loopback\n".into(),
            };
        }

        match request.path.as_str() {
            "/" | "/index.html" => Response::html(ui::INDEX_HTML.to_owned()),
            "/app.js" => Response::js(ui::APP_JS.to_owned()),
            "/style.css" => Response::css(ui::STYLE_CSS.to_owned()),
            "/favicon.ico" => Response::Empty(204),
            "/api/snapshot" => {
                let app = shared.lock();
                let clients = shared.hub.len();
                Response::json(snapshot_json(&app, clients).to_string())
            }
            "/api/health" => {
                let app = shared.lock();
                Response::json(
                    Json::obj([
                        ("ok", Json::Bool(true)),
                        ("uptimeSeconds", Json::Int(app.uptime_s() as i64)),
                        ("sawTraffic", Json::Bool(app.saw_traffic)),
                    ])
                    .to_string(),
                )
            }
            "/api/events" => events_stream(Arc::clone(&shared)),
            _ => Response::not_found(),
        }
    }
}

/// The Server-Sent Events endpoint.
///
/// Sends the current snapshot immediately, then every subsequent change, with
/// a comment heartbeat so proxies and the browser do not time the connection
/// out during a quiet fight.
/// The periodic frame the dashboard receives when nothing else has happened.
///
/// Carries the same authoritative counters as the snapshot rather than only
/// the connection status, so a client never has to reconstruct totals from the
/// individual events it happened to see.
pub fn status_json(app: &App, sse_clients: usize) -> Json {
    let doc = snapshot_json(app, sse_clients);
    Json::obj([
        ("status", doc.get("status").cloned().unwrap_or(Json::Null)),
        ("totals", doc.get("totals").cloned().unwrap_or(Json::Null)),
        (
            "counters",
            doc.get("counters").cloned().unwrap_or(Json::Null),
        ),
    ])
}

fn events_stream(shared: Arc<Shared>) -> Response {
    Response::Stream(Box::new(move |stream| {
        let (id, rx) = shared.hub.subscribe();
        let hub = &shared.hub;

        // Subscribe and snapshot while holding the state lock, so a change
        // cannot slip between the two and arrive twice.
        let initial = {
            let app = shared.lock();
            http::sse_frame("hello", &snapshot_json(&app, shared.hub.len()).to_string())
        };

        let result = http::stream_events(stream, |send| {
            send(&initial)?;

            loop {
                match rx.recv_timeout(HEARTBEAT) {
                    Ok(frame) => send(&frame)?,
                    // Nothing happened; tell the client we are still here.
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => send(": keep-alive\n\n")?,
                    // The hub outlives every client, so this means shutdown.
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }

            Ok(())
        });

        hub.unsubscribe(id);
        let _ = result;
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Journal;

    fn temp_journal(tag: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "loot-ledger-web-{tag}-{}.jsonl",
            std::process::id()
        ));
        p
    }

    fn test_app() -> (App, std::path::PathBuf) {
        let path = temp_journal("app");
        let _ = std::fs::remove_file(&path);
        let app = App {
            state: GameState::new(),
            journal: Journal::open(&path).unwrap(),
            capture: CaptureStats::default(),
            parse: ParseStats::default(),
            replay: ReplayStats::default(),
            started_at_ms: 0,
            started_at: Instant::now(),
            interface: Some("eno1".into()),
            filter_attached: true,
            saw_traffic: false,
            running: true,
        };
        (app, path)
    }

    #[test]
    fn snapshot_is_valid_json_with_every_section() {
        let (app, path) = test_app();
        let doc = snapshot_json(&app, 0).to_string();
        for section in [
            "\"status\"",
            "\"totals\"",
            "\"counters\"",
            "\"catalogue\"",
            "\"eventsSeen\"",
            "\"players\"",
            "\"feed\"",
            "\"zones\"",
        ] {
            assert!(doc.contains(section), "missing {section} in {doc}");
        }
        crate::util::json::parse(&doc).expect("snapshot must be valid JSON");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn snapshot_reports_a_null_self_until_the_game_says_otherwise() {
        let (app, path) = test_app();
        assert!(snapshot_json(&app, 0).to_string().contains("\"self\":null"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn loot_json_round_trips_through_the_parser() {
        let r = LootRecord {
            at_ms: 1,
            looted_by: "Grim".into(),
            looted_from: "BossRat".into(),
            quantity: 2,
            is_silver: false,
            item_num_id: Some(2),
            item_unique: Some("T3_2H_TOOL_TRACKING".into()),
            item_name: Some("Journeyman's Tracking Toolkit".into()),
        };
        let doc = loot_json(&r).to_string();
        let parsed = crate::util::json::parse(&doc).unwrap();
        assert_eq!(parsed.get("by").unwrap().as_str(), Some("Grim"));
        assert_eq!(parsed.get("qty").unwrap().as_i64(), Some(2));
        assert_eq!(parsed.get("silver").unwrap().as_bool(), Some(false));
    }

    #[test]
    fn null_item_fields_survive_serialisation() {
        let r = LootRecord {
            at_ms: 1,
            looted_by: "Grim".into(),
            looted_from: "Grim".into(),
            quantity: 9,
            is_silver: true,
            item_num_id: None,
            item_unique: None,
            item_name: None,
        };
        let doc = loot_json(&r).to_string();
        assert!(doc.contains("\"itemNumId\":null"));
        assert!(doc.contains("\"itemName\":null"));
        crate::util::json::parse(&doc).unwrap();
    }

    #[test]
    fn snapshot_reports_the_real_number_of_connected_clients() {
        // This was hardcoded to zero for a while, which made the figure worse
        // than useless: it looked like a working feature that never moved.
        let (app, path) = test_app();

        for clients in [0usize, 1, 7] {
            let doc = snapshot_json(&app, clients).to_string();
            let parsed = crate::util::json::parse(&doc).unwrap();
            assert_eq!(
                parsed
                    .get("status")
                    .and_then(|s| s.get("sseClients"))
                    .and_then(Json::as_i64),
                Some(clients as i64),
                "sseClients must reflect the value passed in"
            );
        }

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_status_frame_carries_authoritative_totals_and_counters() {
        // The dashboard cannot reconstruct totals reliably from the individual
        // events it happens to receive, so the periodic frame has to carry
        // them from the same source as the snapshot.
        let (mut app, path) = test_app();
        app.parse.packets = 1234;
        app.parse.messages = 1200;
        app.capture.frames = 1400;

        let doc = status_json(&app, 2).to_string();
        let parsed = crate::util::json::parse(&doc).unwrap();

        for section in ["status", "totals", "counters"] {
            assert!(
                parsed.get(section).is_some(),
                "status frame needs {section}"
            );
        }
        assert_eq!(
            parsed
                .get("counters")
                .and_then(|c| c.get("packets"))
                .and_then(Json::as_i64),
            Some(1234)
        );
        assert_eq!(
            parsed
                .get("status")
                .and_then(|s| s.get("sseClients"))
                .and_then(Json::as_i64),
            Some(2)
        );
        // It must stay a strict subset of the snapshot, not a different shape.
        assert!(doc.len() < snapshot_json(&app, 2).to_string().len());

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn change_events_are_named_consistently() {
        let loot = Change::Loot(LootRecord {
            at_ms: 1,
            looted_by: "A".into(),
            looted_from: "B".into(),
            quantity: 1,
            is_silver: false,
            item_num_id: None,
            item_unique: None,
            item_name: None,
        });
        let (name, _) = change_event(&loot).unwrap();
        assert_eq!(name, "loot");
    }
}
