//! Cross-validation against the reference implementation.
//!
//! `tests/golden/vectors.json` is produced by running the original
//! ao-loot-logger decoder over synthetic Photon packets that loot-ledger is
//! then fed. Both implementations must agree, message for message and
//! parameter for parameter.
//!
//! The point is that loot-ledger's decoder is not being checked against my own
//! reading of the protocol — it is checked against code that is known to work
//! against live Albion traffic.
//!
//! Regenerate with:
//!
//! ```text
//! node tools/gen-golden.js ../ao-loot-logger-main tests/golden/vectors.json
//! ```
//!
//! The vectors are committed, so a normal `cargo test` needs no Node.

use loot_ledger::proto::p16::{Body, Params, Value};
use loot_ledger::proto::Parser;
use loot_ledger::util::json::{parse, Json};

const VECTORS: &str = include_str!("golden/vectors.json");

/// One decoded message, reduced to what both implementations agree on.
#[derive(Debug, PartialEq)]
struct Decoded {
    is_event: bool,
    code: u8,
    params: String,
}

fn decode(packets: &[Vec<u8>]) -> Vec<Decoded> {
    let mut parser = Parser::new();
    let mut out = Vec::new();

    for packet in packets {
        parser.handle_packet(packet, &mut |body| {
            let (is_event, code, params) = match body {
                Body::Event(e) => (true, e.code, e.params),
                Body::Operation(o) => (false, o.code, o.params),
            };
            out.push(Decoded {
                is_event,
                code,
                params: params_json(&params).to_string(),
            });
        });
    }

    out
}

/// Render a parameter table the way the reference's plain JavaScript values
/// serialise: objects keyed by the decimal parameter id, with the wire type
/// discarded because the reference discards it too.
fn params_json(params: &Params) -> Json {
    let mut map = std::collections::BTreeMap::new();
    for (id, value) in params.iter() {
        map.insert(id.to_string(), value_json(value));
    }
    Json::Object(map)
}

fn value_json(v: &Value) -> Json {
    match v {
        Value::Nil => Json::Null,
        Value::Bool(b) => Json::Bool(*b),
        Value::Int8(n) => Json::Int(*n as i64),
        Value::Int16(n) => Json::Int(*n as i64),
        Value::Int32(n) => Json::Int(*n as i64),
        Value::Int64(n) => Json::Int(*n),
        Value::Float32(f) => Json::Float(*f as f64),
        Value::Double(d) => Json::Float(*d),
        Value::String(s) => Json::Str(s.clone()),
        // A byte slice is a run of unsigned bytes on the wire; render it as
        // the array of numbers the reference produces.
        Value::ByteSlice(bytes) => {
            Json::Array(bytes.iter().map(|b| Json::Int(*b as i64)).collect())
        }
        Value::Slice(items) => Json::Array(items.iter().map(value_json).collect()),
        // Dictionary keys are stringified on both sides.
        Value::Dictionary(entries) => {
            let mut map = std::collections::BTreeMap::new();
            for (k, val) in entries {
                map.insert(key_string(k), value_json(val));
            }
            Json::Object(map)
        }
    }
}

fn key_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_display_string(),
    }
}

fn hex_to_bytes(hex: &str) -> Vec<u8> {
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).expect("vector hex must be valid"))
        .collect()
}

struct Case {
    name: String,
    packets: Vec<Vec<u8>>,
    expected: Vec<Decoded>,
}

fn load_cases() -> Vec<Case> {
    let doc = parse(VECTORS).expect("vectors.json must be valid JSON");
    let cases = doc
        .get("cases")
        .and_then(Json::as_array)
        .expect("cases array");

    cases
        .iter()
        .map(|case| {
            let packets: Vec<Vec<u8>> =
                if let Some(list) = case.get("packets").and_then(Json::as_array) {
                    list.iter()
                        .map(|p| hex_to_bytes(p.as_str().expect("hex string")))
                        .collect()
                } else {
                    vec![hex_to_bytes(
                        case.get("payload")
                            .and_then(Json::as_str)
                            .expect("payload string"),
                    )]
                };

            let emitted = case
                .get("emitted")
                .and_then(Json::as_array)
                .expect("emitted array");

            let expected = emitted
                .iter()
                .map(|m| Decoded {
                    is_event: m.get("kind").and_then(Json::as_str) == Some("event"),
                    code: m.get("code").and_then(Json::as_i64).unwrap_or(-1) as u8,
                    params: m
                        .get("params")
                        .map(|p| p.to_string())
                        .unwrap_or_else(|| "{}".into()),
                })
                .collect();

            Case {
                name: case
                    .get("name")
                    .and_then(Json::as_str)
                    .unwrap_or("<unnamed>")
                    .to_owned(),
                packets,
                expected,
            }
        })
        .collect()
}

#[test]
fn vectors_are_present_and_meaningful() {
    let cases = load_cases();
    assert!(
        cases.len() >= 15,
        "expected a decent corpus, got {}",
        cases.len()
    );
    assert!(
        cases.iter().any(|c| !c.expected.is_empty()),
        "no case decodes to anything, so the comparison would be vacuous"
    );
    assert!(
        cases.iter().any(|c| c.expected.is_empty()),
        "no case exercises a packet that must be dropped"
    );
    assert!(
        cases.iter().any(|c| c.packets.len() > 1),
        "no case exercises multi-packet reassembly"
    );
}

#[test]
fn decoding_matches_the_reference_implementation() {
    let cases = load_cases();

    for case in &cases {
        let actual = decode(&case.packets);

        assert_eq!(
            actual.len(),
            case.expected.len(),
            "case {:?}: message count differs from the reference",
            case.name
        );

        for (i, (got, want)) in actual.iter().zip(case.expected.iter()).enumerate() {
            assert_eq!(
                got.is_event, want.is_event,
                "case {:?} message {i}: kind differs",
                case.name
            );
            assert_eq!(
                got.code, want.code,
                "case {:?} message {i}: code differs",
                case.name
            );
            assert_eq!(
                &got.params, &want.params,
                "case {:?} message {i}: parameters differ",
                case.name
            );
        }
    }
}

#[test]
fn the_loot_event_vector_carries_the_fields_the_app_relies_on() {
    // Guards the vectors themselves: if regeneration ever produced a case that
    // no longer exercises the real event shape, the comparison above would
    // still pass while proving nothing about the app.
    let cases = load_cases();
    let case = cases
        .iter()
        .find(|c| c.name == "loot-event-mixed-types")
        .expect("the loot vector must exist");

    let params = parse(&case.expected[0].params).unwrap();
    assert_eq!(params.get("1").and_then(Json::as_str), Some("BossRat"));
    assert_eq!(params.get("2").and_then(Json::as_str), Some("Grim"));
    assert_eq!(params.get("3").and_then(Json::as_bool), Some(false));
    assert_eq!(params.get("4").and_then(Json::as_i64), Some(1234));
    assert_eq!(params.get("5").and_then(Json::as_i64), Some(3));
    assert_eq!(params.get("252").and_then(Json::as_i64), Some(275));
}

#[test]
fn drops_everything_a_malformed_packet_carries() {
    // The reference throws on these; we count a decode error and carry on.
    // Either way nothing reaches the game layer.
    let cases = load_cases();
    for name in [
        "encrypted-packet",
        "truncated-packet",
        "empty-packet",
        "event-with-unknown-param-type",
        "fragment-incomplete",
    ] {
        let case = cases
            .iter()
            .find(|c| c.name == name)
            .unwrap_or_else(|| panic!("missing vector {name}"));
        assert!(
            decode(&case.packets).is_empty(),
            "case {name} must decode to nothing"
        );
    }
}

#[test]
fn a_lone_corrupt_packet_does_not_poison_the_parser() {
    // Whatever a bad packet does to internal state, the next good one must
    // still decode. This is the property that keeps a live capture alive.
    let cases = load_cases();
    let good = cases
        .iter()
        .find(|c| c.name == "loot-event-mixed-types")
        .unwrap();

    let mut parser = Parser::new();
    let mut seen = 0;

    for junk in [
        vec![0xffu8; 7],
        vec![0x00, 0x00, 0x04, 0x01, 0x00, 0x00],
        vec![0x00, 0x00, 0x01, 0x01, 0, 0, 0, 0, 0, 0, 0, 0],
        (0u8..200).collect(),
    ] {
        parser.handle_packet(&junk, &mut |_| seen += 1);
    }

    for packet in &good.packets {
        parser.handle_packet(packet, &mut |_| seen += 1);
    }

    assert_eq!(seen, 1, "the good packet must still decode after garbage");
}
