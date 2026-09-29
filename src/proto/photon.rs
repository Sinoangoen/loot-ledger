//! Photon framing: UDP payload in, decoded protocol bodies out.
//!
//! A Photon packet is a 12-byte header followed by one or more *commands*.
//! Each command carries its own 12-byte header and a payload that is either a
//! whole serialised message or one fragment of a larger one. This module walks
//! that structure, reassembles fragments, and hands each complete payload to
//! the caller.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use super::p16::{self, Body};
use super::reader::{ParseError, Reader, Result};

const PHOTON_HEADER_LEN: usize = 12;
const PHOTON_COMMAND_HEADER_LEN: usize = 12;
const PHOTON_FRAGMENT_HEADER_LEN: usize = 20;

/// Header flag bits.
mod flags {
    /// Payload is encrypted; we hold no session keys and cannot read it.
    pub const ENCRYPTED: u8 = 0x01;
    /// An extended header follows, carrying a 4-byte CRC32-C.
    pub const EXTENDED_HEADER: u8 = 0x40;
}

/// Refuse to buffer more than this for a single reassembled message.
///
/// Albion's largest observed payloads are well under 1 MiB. The cap exists so
/// a malformed length field cannot turn into a multi-gigabyte allocation.
const MAX_REASSEMBLED_LEN: usize = 8 * 1024 * 1024;

/// Refuse to track more than this many concurrently incomplete messages.
const MAX_PENDING_FRAGMENTS: usize = 256;

/// Ceiling on the memory held by *all* incomplete messages combined.
///
/// A per-message cap and a message-count cap multiply: 8 MiB x 256 is 2 GiB,
/// which any host able to send UDP to Albion's port can trigger with 256
/// never-completing fragments. A single budget makes the worst case a constant
/// the process can actually live inside, and is the only limit that stays
/// meaningful when either other number is retuned.
const MAX_PENDING_BYTES: usize = 16 * 1024 * 1024;

/// Drop partial messages that never complete within this window.
const FRAGMENT_TTL: Duration = Duration::from_secs(5);

/// Counters describing what the parser has seen. Surfaced by the HTTP API so a
/// user with a blank dashboard can tell "no traffic" from "traffic we can't read".
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ParseStats {
    /// Datagrams handed to the parser.
    pub packets: u64,
    /// Packets dropped because the header claimed encryption.
    pub encrypted: u64,
    /// Packets that carried a CRC32-C extended header.
    pub crc_checked: u64,
    /// Packets whose CRC32-C did not match.
    pub crc_mismatch: u64,
    /// Complete messages yielded to the caller.
    pub messages: u64,
    /// Messages that failed to decode and were dropped.
    pub decode_errors: u64,
    /// Fragments received.
    pub fragments: u64,
    /// Fragmented messages successfully reassembled.
    pub reassembled: u64,
    /// Fragments discarded for a malformed length or offset.
    pub fragments_rejected: u64,
    /// Partial messages dropped for exceeding the TTL.
    pub fragments_expired: u64,
}

/// A fragment of a larger message that has not been fully received yet.
#[derive(Debug)]
struct Pending {
    total_len: usize,
    /// Payload buffer; written at the fragment's real offset as it arrives.
    buf: Vec<u8>,
    /// Byte ranges already written, keyed by start offset, for dedupe and
    /// coverage checks.
    ranges: BTreeMap<u32, u32>,
    first_seen: Instant,
}

impl Pending {
    /// Write `data` at `offset` and record the range.
    ///
    /// Returns `false` if the exact range was already present (UDP can
    /// genuinely duplicate datagrams) or if the range runs past the declared
    /// total length.
    fn add(&mut self, offset: u32, data: &[u8]) -> bool {
        let end = offset as u64 + data.len() as u64;
        if end > self.total_len as u64 {
            return false;
        }

        if self.ranges.contains_key(&offset) {
            return false;
        }

        self.buf[offset as usize..end as usize].copy_from_slice(data);
        self.ranges.insert(offset, data.len() as u32);
        true
    }

    /// True when the written ranges cover `[0, total_len)` with no gaps.
    fn is_complete(&self) -> bool {
        let mut cursor: u64 = 0;
        for (&start, &len) in &self.ranges {
            if start as u64 > cursor {
                return false; // gap
            }
            cursor = cursor.max(start as u64 + len as u64);
        }
        cursor >= self.total_len as u64
    }
}

/// The 20-byte header that precedes a fragment's data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FragmentHeader {
    /// Identifies the message this fragment belongs to.
    pub sequence: u32,
    /// How many fragments the whole message has.
    pub fragment_count: u32,
    /// This fragment's index within the message.
    pub fragment_number: u32,
    /// Total length of the reassembled message.
    pub total_len: i32,
    /// Byte offset of this fragment's data within the message.
    pub offset: i32,
}

/// Reassembles fragmented Photon messages.
#[derive(Debug, Default)]
pub struct Reassembler {
    pending: BTreeMap<u32, Pending>,
    /// Sum of `total_len` across `pending`, so the budget can be enforced.
    reserved: usize,
}

impl Reassembler {
    /// Feed one fragment, returning the complete payload once every byte has
    /// arrived.
    pub fn push(
        &mut self,
        header: &FragmentHeader,
        data: &[u8],
        stats: &mut ParseStats,
    ) -> Option<Vec<u8>> {
        stats.fragments += 1;

        if header.fragment_count == 0 || header.fragment_number >= header.fragment_count {
            stats.fragments_rejected += 1;
            return None;
        }

        let total_len = usize::try_from(header.total_len).ok()?;
        if total_len > MAX_REASSEMBLED_LEN {
            stats.fragments_rejected += 1;
            return None;
        }

        let offset = u32::try_from(header.offset).ok()?;

        self.expire(Instant::now());

        // A message whose sequence is already being tracked is a continuation;
        // only a *new* sequence has to fit inside the limits. Without this, a
        // stream of never-completing fragments would grow the map forever.
        if !self.pending.contains_key(&header.sequence) {
            let too_many = self.pending.len() >= MAX_PENDING_FRAGMENTS;
            let too_big = self.reserved.saturating_add(total_len) > MAX_PENDING_BYTES;
            if too_many || too_big {
                stats.fragments_rejected += 1;
                return None;
            }
        }

        let entry = self.pending.entry(header.sequence).or_insert_with(|| {
            self.reserved += total_len;
            Pending {
                total_len,
                buf: vec![0u8; total_len],
                ranges: BTreeMap::new(),
                first_seen: Instant::now(),
            }
        });

        // A conflicting re-send with a different total means the stream is out
        // of sync; drop the stale buffer rather than splice mismatched lengths.
        if entry.total_len != total_len {
            stats.fragments_rejected += 1;
            self.drop_pending(&header.sequence);
            return None;
        }

        if !entry.add(offset, data) {
            stats.fragments_rejected += 1;
            return None;
        }

        if entry.is_complete() {
            stats.reassembled += 1;
            // `take` returns `None` rather than asserting, so nothing on the
            // capture path can panic if the map is ever mutated underneath us.
            return self.take(&header.sequence).map(|p| p.buf);
        }

        None
    }

    /// Remove a pending message and return its buffer, releasing its budget.
    fn take(&mut self, sequence: &u32) -> Option<Pending> {
        let pending = self.pending.remove(sequence)?;
        self.reserved = self.reserved.saturating_sub(pending.total_len);
        Some(pending)
    }

    /// Discard a pending message, releasing its budget.
    fn drop_pending(&mut self, sequence: &u32) {
        if let Some(p) = self.pending.remove(sequence) {
            self.reserved = self.reserved.saturating_sub(p.total_len);
        }
    }

    /// Drop partial messages that have outlived [`FRAGMENT_TTL`].
    fn expire(&mut self, now: Instant) {
        self.pending.retain(|_, p| {
            let keep = now.duration_since(p.first_seen) < FRAGMENT_TTL;
            if !keep {
                self.reserved = self.reserved.saturating_sub(p.total_len);
            }
            keep
        });
    }

    /// Drop every partial message, counting how many were abandoned.
    pub fn clear(&mut self, stats: &mut ParseStats) {
        stats.fragments_expired += self.pending.len() as u64;
        self.pending.clear();
        self.reserved = 0;
    }

    /// Memory currently reserved for incomplete messages.
    pub fn reserved_bytes(&self) -> usize {
        self.reserved
    }
}

/// CRC32-C (Castagnoli), the checksum Photon puts in extended headers.
///
/// Implemented without a lookup table: extended headers are rare enough that
/// table setup would cost more than the eight bitwise rounds, and keeping this
/// table-free keeps the dependency count at zero.
pub fn crc32c(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xffff_ffff;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0x82f6_3b78 & mask);
        }
    }
    !crc
}

/// Walks Photon packets and yields decoded protocol bodies.
#[derive(Debug, Default)]
pub struct Parser {
    reassembler: Reassembler,
    stats: ParseStats,
}

impl Parser {
    /// A parser with zeroed counters.
    pub fn new() -> Self {
        Parser::default()
    }

    /// Current counters.
    pub fn stats(&self) -> ParseStats {
        self.stats
    }

    /// Reset the counters.
    pub fn reset_stats(&mut self) {
        self.stats = ParseStats::default();
    }

    /// Abandon all partially reassembled messages.
    pub fn clear_fragments(&mut self) {
        self.reassembler.clear(&mut self.stats);
    }

    /// Decode one UDP payload, invoking `on_body` for every complete message.
    ///
    /// Never panics and never returns an error: a malformed datagram is
    /// counted in [`ParseStats`] and dropped, because the capture loop must
    /// survive arbitrary traffic.
    pub fn handle_packet<F: FnMut(Body)>(&mut self, packet: &[u8], on_body: &mut F) {
        self.stats.packets += 1;

        if packet.len() < PHOTON_HEADER_LEN {
            self.stats.decode_errors += 1;
            return;
        }

        let mut r = Reader::new(packet);
        let (_peer_id, header_flags, command_count) = match (r.read_u16(), r.read_u8(), r.read_u8())
        {
            (Ok(p), Ok(f), Ok(c)) => (p, f, c),
            _ => {
                self.stats.decode_errors += 1;
                return;
            }
        };

        if let Err(e) = r.read_u32().and_then(|_| r.read_i32()) {
            let _ = e;
            self.stats.decode_errors += 1;
            return;
        }

        if header_flags & flags::ENCRYPTED != 0 {
            self.stats.encrypted += 1;
            return;
        }

        if header_flags & flags::EXTENDED_HEADER != 0 {
            match r.read_i32() {
                Ok(expected) => {
                    self.stats.crc_checked += 1;
                    if !self.crc_matches(&r, packet, expected) {
                        self.stats.crc_mismatch += 1;
                    }
                }
                Err(_) => {
                    self.stats.decode_errors += 1;
                    return;
                }
            }
        }

        for _ in 0..command_count {
            if r.remaining().is_empty() {
                break;
            }
            if !self.handle_command(&mut r, packet, on_body) {
                break;
            }
        }
    }

    /// Verify the extended-header CRC.
    ///
    /// The reference implementation cannot reproduce Photon's CRC and therefore
    /// discards every packet that carries one. We compute the checksum, record
    /// whether it matched, and parse the payload either way — Photon payloads
    /// are self-describing, so a genuinely corrupt one fails to decode and is
    /// dropped by [`Self::handle_command`] on its own.
    fn crc_matches(&self, r: &Reader<'_>, packet: &[u8], expected: i32) -> bool {
        const CRC_OFFSET: usize = 12;
        let mut copy = packet.to_vec();
        if copy.len() < CRC_OFFSET + 4 {
            return false;
        }
        copy[CRC_OFFSET..CRC_OFFSET + 4].fill(0);
        let _ = r;
        crc32c(&copy) == expected as u32
    }

    /// Decode one command. Returns `false` to stop walking the packet.
    fn handle_command<F: FnMut(Body)>(
        &mut self,
        r: &mut Reader<'_>,
        packet: &[u8],
        on_body: &mut F,
    ) -> bool {
        let command_type = match r.read_u8() {
            Ok(v) => v,
            Err(_) => return false,
        };
        if r.read_u8().is_err() || r.read_u8().is_err() || r.read_u8().is_err() {
            return false;
        }

        let command_len = match r.read_i32() {
            Ok(v) => v,
            Err(_) => return false,
        };
        if r.read_i32().is_err() {
            return false;
        }

        // The declared length covers the command header itself.
        let payload_len = match command_len.checked_sub(PHOTON_COMMAND_HEADER_LEN as i32) {
            Some(v) if v >= 0 => v as usize,
            _ => return false,
        };

        if payload_len > r.remaining().len() {
            return false;
        }

        // The length is validated immediately above, but the capture path must
        // not be able to panic at all: a malformed datagram is expected input,
        // not an exceptional one. So the read is handled, not asserted.
        let Ok(payload) = r.read_bytes(payload_len) else {
            return false;
        };

        match command_type {
            p16::command::SEND_RELIABLE => {
                self.handle_payload(payload, on_body);
                true
            }
            p16::command::SEND_UNRELIABLE => {
                // Unreliable commands carry a 4-byte reliable sequence number
                // that Photon uses for ordering; it is not part of the message.
                if payload.len() < 4 {
                    return false;
                }
                self.handle_payload(&payload[4..], on_body);
                true
            }
            p16::command::SEND_RELIABLE_FRAGMENT => {
                if payload.len() < PHOTON_FRAGMENT_HEADER_LEN {
                    self.stats.fragments_rejected += 1;
                    return false;
                }
                self.handle_fragment(payload, on_body);
                true
            }
            p16::command::DISCONNECT => false,
            // PING, CONNECT, ACKNOWLEDGE, VERIFY_CONNECT and anything unknown.
            // The payload was already consumed above, so there is nothing left
            // to advance past — skipping again here would desynchronise the
            // next command in the packet.
            _ => {
                let _ = packet;
                true
            }
        }
    }

    /// Reassemble and dispatch a fragment command.
    fn handle_fragment<F: FnMut(Body)>(&mut self, fragment: &[u8], on_body: &mut F) {
        // The fragment payload *begins* with its own 20-byte header; the
        // header fields and the data that follows share one cursor.
        let mut r = Reader::new(fragment);

        let header = (|| -> Result<FragmentHeader> {
            Ok(FragmentHeader {
                sequence: r.read_i32()? as u32,
                fragment_count: r.read_i32()? as u32,
                fragment_number: r.read_i32()? as u32,
                total_len: r.read_i32()?,
                offset: r.read_i32()?,
            })
        })();

        let header = match header {
            Ok(v) => v,
            Err(_) => {
                self.stats.fragments_rejected += 1;
                return;
            }
        };

        if let Some(complete) = self
            .reassembler
            .push(&header, r.remaining(), &mut self.stats)
        {
            self.handle_payload(&complete, on_body);
        }
    }

    /// Decode and dispatch one complete reliable payload.
    fn handle_payload<F: FnMut(Body)>(&mut self, payload: &[u8], on_body: &mut F) {
        match p16::decode_body(payload) {
            Ok(Some(body)) => {
                self.stats.messages += 1;
                on_body(body);
            }
            Ok(None) => {}
            Err(ParseError::UnknownType(_)) | Err(_) => {
                self.stats.decode_errors += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::p16::{msg, ty};

    /// Build a Photon packet: header, then the given commands.
    fn packet(flags: u8, commands: &[Vec<u8>]) -> Vec<u8> {
        let mut p = vec![0x00, 0x00, flags, commands.len() as u8];
        p.extend_from_slice(&[0; 8]); // timestamp + challenge
        for c in commands {
            p.extend_from_slice(c);
        }
        p
    }

    /// Wrap a message payload in a SEND_RELIABLE command.
    fn reliable_command(sequence: i32, payload: &[u8]) -> Vec<u8> {
        let mut c = vec![p16::command::SEND_RELIABLE, 0, 0, 0];
        c.extend_from_slice(&((payload.len() + PHOTON_COMMAND_HEADER_LEN) as i32).to_be_bytes());
        c.extend_from_slice(&sequence.to_be_bytes());
        c.extend_from_slice(payload);
        c
    }

    /// A minimal event payload: 275 = EvOtherGrabbedLoot.
    fn event_payload() -> Vec<u8> {
        let mut p = vec![0xF3, msg::EVENT_DATA, 1];
        let mut body = vec![p16::EVENT_ID_KEY, ty::INT32];
        body.extend_from_slice(&275i32.to_be_bytes());
        p.extend_from_slice(&1i16.to_be_bytes()); // one parameter
        p.extend_from_slice(&body);
        p
    }

    #[test]
    fn decodes_a_single_reliable_event() {
        let mut parser = Parser::new();
        let mut seen = Vec::new();
        let pkt = packet(0x04, &[reliable_command(1, &event_payload())]);

        parser.handle_packet(&pkt, &mut |b| seen.push(b));

        assert_eq!(seen.len(), 1);
        assert_eq!(parser.stats().messages, 1);
        assert_eq!(parser.stats().decode_errors, 0);
        match &seen[0] {
            Body::Event(e) => assert_eq!(e.params.get_i64(252), Some(275)),
            other => panic!("expected event, got {other:?}"),
        }
    }

    #[test]
    fn skips_encrypted_packets() {
        let mut parser = Parser::new();
        let mut seen = 0;
        let pkt = packet(0x01, &[reliable_command(1, &event_payload())]);
        parser.handle_packet(&pkt, &mut |_| seen += 1);
        assert_eq!(seen, 0);
        assert_eq!(parser.stats().encrypted, 1);
    }

    #[test]
    fn parses_crc_flagged_packets_that_the_reference_drops() {
        // Extended header: 4 extra bytes after the 12-byte header.
        let cmd = reliable_command(1, &event_payload());
        let mut pkt = vec![0x00, 0x00, 0x44, 0x01];
        pkt.extend_from_slice(&[0; 8]);
        pkt.extend_from_slice(&0i32.to_be_bytes()); // CRC placeholder
        pkt.extend_from_slice(&cmd);

        let crc = crc32c(&pkt) as i32;
        pkt[12..16].copy_from_slice(&crc.to_be_bytes());

        let mut parser = Parser::new();
        let mut seen = 0;
        parser.handle_packet(&pkt, &mut |_| seen += 1);

        assert_eq!(seen, 1, "CRC packets must still be parsed");
        assert_eq!(parser.stats().crc_checked, 1);
        assert_eq!(parser.stats().crc_mismatch, 0);
    }

    #[test]
    fn detects_a_corrupt_crc_without_dropping_the_packet() {
        let mut pkt = vec![0x00, 0x00, 0x44, 0x01];
        pkt.extend_from_slice(&[0; 8]);
        pkt.extend_from_slice(&(-0x215241i32).to_be_bytes());
        pkt.extend_from_slice(&reliable_command(1, &event_payload()));

        let mut parser = Parser::new();
        let mut seen = 0;
        parser.handle_packet(&pkt, &mut |_| seen += 1);

        assert_eq!(parser.stats().crc_mismatch, 1);
        assert_eq!(seen, 1);
    }

    #[test]
    fn reassembles_fragments_in_any_order() {
        let payload = event_payload();
        let (a, b) = payload.split_at(payload.len() / 2);

        let fragment_command = |number: i32, total: i32, offset: i32, data: &[u8]| {
            let mut body = Vec::new();
            body.extend_from_slice(&1i32.to_be_bytes()); // sequence
            body.extend_from_slice(&2i32.to_be_bytes()); // count
            body.extend_from_slice(&number.to_be_bytes());
            body.extend_from_slice(&total.to_be_bytes());
            body.extend_from_slice(&offset.to_be_bytes());
            body.extend_from_slice(data);

            let mut c = vec![p16::command::SEND_RELIABLE_FRAGMENT, 0, 0, 0];
            c.extend_from_slice(&((body.len() + PHOTON_COMMAND_HEADER_LEN) as i32).to_be_bytes());
            c.extend_from_slice(&7i32.to_be_bytes());
            c.extend_from_slice(&body);
            c
        };

        let mut parser = Parser::new();
        let mut seen = 0;

        // Second fragment first, to prove ordering does not matter.
        let p2 = packet(
            0x04,
            &[fragment_command(1, payload.len() as i32, a.len() as i32, b)],
        );
        parser.handle_packet(&p2, &mut |_| seen += 1);
        assert_eq!(seen, 0);

        let p1 = packet(0x04, &[fragment_command(0, payload.len() as i32, 0, a)]);
        parser.handle_packet(&p1, &mut |_| seen += 1);
        assert_eq!(seen, 1);
        assert_eq!(parser.stats().reassembled, 1);
    }

    #[test]
    fn rejects_absurd_fragment_length_without_allocating() {
        let mut parser = Parser::new();
        let mut body = Vec::new();
        body.extend_from_slice(&1i32.to_be_bytes());
        body.extend_from_slice(&2i32.to_be_bytes());
        body.extend_from_slice(&0i32.to_be_bytes());
        body.extend_from_slice(&i32::MAX.to_be_bytes()); // total length
        body.extend_from_slice(&0i32.to_be_bytes());
        body.extend_from_slice(b"x");

        let mut c = vec![p16::command::SEND_RELIABLE_FRAGMENT, 0, 0, 0];
        c.extend_from_slice(&((body.len() + PHOTON_COMMAND_HEADER_LEN) as i32).to_be_bytes());
        c.extend_from_slice(&1i32.to_be_bytes());
        c.extend_from_slice(&body);

        let pkt = packet(0x04, &[c]);
        parser.handle_packet(&pkt, &mut |_| panic!("must not dispatch"));

        assert_eq!(parser.stats().fragments_rejected, 1);
    }

    #[test]
    fn duplicate_fragment_does_not_complete_early() {
        let payload = event_payload();
        let (a, b) = payload.split_at(payload.len() / 2);

        let make = |number: i32, offset: i32, data: &[u8]| {
            let mut body = Vec::new();
            body.extend_from_slice(&1i32.to_be_bytes());
            body.extend_from_slice(&2i32.to_be_bytes());
            body.extend_from_slice(&number.to_be_bytes());
            body.extend_from_slice(&(payload.len() as i32).to_be_bytes());
            body.extend_from_slice(&offset.to_be_bytes());
            body.extend_from_slice(data);
            let mut c = vec![p16::command::SEND_RELIABLE_FRAGMENT, 0, 0, 0];
            c.extend_from_slice(&((body.len() + PHOTON_COMMAND_HEADER_LEN) as i32).to_be_bytes());
            c.extend_from_slice(&1i32.to_be_bytes());
            c.extend_from_slice(&body);
            packet(0x04, &[c])
        };

        let mut parser = Parser::new();
        let mut seen = 0;
        let p0 = make(0, 0, a);
        parser.handle_packet(&p0, &mut |_| seen += 1);
        parser.handle_packet(&p0, &mut |_| seen += 1); // duplicate
        assert_eq!(seen, 0, "duplicate must not close the message");

        let p1 = make(1, a.len() as i32, b);
        parser.handle_packet(&p1, &mut |_| seen += 1);
        assert_eq!(seen, 1);
    }

    #[test]
    fn stale_fragments_are_reclaimed() {
        let mut reassembler = Reassembler::default();
        let mut stats = ParseStats::default();
        reassembler.push(
            &FragmentHeader {
                sequence: 1,
                fragment_count: 2,
                fragment_number: 0,
                total_len: 100,
                offset: 0,
            },
            b"aaaa",
            &mut stats,
        );
        assert_eq!(reassembler.pending.len(), 1);

        // Simulate the TTL elapsing.
        reassembler.expire(Instant::now() + FRAGMENT_TTL + Duration::from_secs(1));
        assert_eq!(reassembler.pending.len(), 0);
    }

    #[test]
    fn short_and_garbage_packets_are_survivable() {
        let mut parser = Parser::new();
        let mut seen = 0;
        for junk in [
            vec![],
            vec![0u8],
            vec![0xff; 11],
            vec![0x00, 0x00, 0x04, 0x05, 0, 0, 0, 0, 0, 0, 0, 0],
            vec![
                0x00, 0x00, 0x04, 0x01, 0, 0, 0, 0, 0, 0, 0, 0, 0x06, 0, 0, 0, 0xff, 0xff, 0xff,
            ],
        ] {
            parser.handle_packet(&junk, &mut |_| seen += 1);
        }
        assert_eq!(seen, 0);
    }

    #[test]
    fn pending_fragment_count_is_capped() {
        // A stream of never-completing fragments must not grow the map
        // without bound, or a long session would leak memory.
        let mut re = Reassembler::default();
        let mut stats = ParseStats::default();

        for seq in 0..(MAX_PENDING_FRAGMENTS as u32 + 50) {
            re.push(
                &FragmentHeader {
                    sequence: seq,
                    fragment_count: 2,
                    fragment_number: 0,
                    total_len: 100,
                    offset: 0,
                },
                b"aaaa",
                &mut stats,
            );
        }

        assert_eq!(re.pending.len(), MAX_PENDING_FRAGMENTS);
        assert!(stats.fragments_rejected >= 50);
    }

    #[test]
    fn a_capped_sequence_can_still_be_continued() {
        // A sequence no filler will reuse, so the only thing under test is the
        // cap and not sequence-number collision.
        const INFLIGHT_SEQ: i32 = 9001;
        // Dropping a *new* sequence must not break reassembly of one already
        // in flight.
        let payload = event_payload();
        let cut = payload.len() / 2;

        let make = |number: i32, offset: i32, data: &[u8]| {
            let mut body = Vec::new();
            body.extend_from_slice(&INFLIGHT_SEQ.to_be_bytes());
            body.extend_from_slice(&2i32.to_be_bytes());
            body.extend_from_slice(&number.to_be_bytes());
            body.extend_from_slice(&(payload.len() as i32).to_be_bytes());
            body.extend_from_slice(&offset.to_be_bytes());
            body.extend_from_slice(data);
            let mut c = vec![p16::command::SEND_RELIABLE_FRAGMENT, 0, 0, 0];
            c.extend_from_slice(&((body.len() + PHOTON_COMMAND_HEADER_LEN) as i32).to_be_bytes());
            c.extend_from_slice(&1i32.to_be_bytes());
            c.extend_from_slice(&body);
            packet(0x04, &[c])
        };

        let mut parser = Parser::new();
        let mut seen = 0;

        parser.handle_packet(&make(0, 0, &payload[..cut]), &mut |_| seen += 1);
        for seq in 0..(MAX_PENDING_FRAGMENTS as u32 + 10) {
            let mut body = Vec::new();
            body.extend_from_slice(&seq.to_be_bytes());
            body.extend_from_slice(&2i32.to_be_bytes());
            body.extend_from_slice(&0i32.to_be_bytes());
            body.extend_from_slice(&100i32.to_be_bytes());
            body.extend_from_slice(&0i32.to_be_bytes());
            body.extend_from_slice(b"zzzz");
            let mut c = vec![p16::command::SEND_RELIABLE_FRAGMENT, 0, 0, 0];
            c.extend_from_slice(&((body.len() + PHOTON_COMMAND_HEADER_LEN) as i32).to_be_bytes());
            c.extend_from_slice(&2i32.to_be_bytes());
            c.extend_from_slice(&body);
            parser.handle_packet(&packet(0x04, &[c]), &mut |_| seen += 1);
        }

        parser.handle_packet(&make(1, cut as i32, &payload[cut..]), &mut |_| seen += 1);

        assert_eq!(seen, 1, "the in-flight message must still complete");
    }

    #[test]
    fn total_reserved_memory_stays_inside_the_budget() {
        // A per-message cap times a message count is 2 GiB, which is not a
        // number a laptop survives. The budget is a single global ceiling.
        let mut re = Reassembler::default();
        let mut stats = ParseStats::default();
        // Each message claims 1 MiB but only one byte ever arrives, so none
        // of them can complete and every one stays pending.
        for seq in 0..512u32 {
            re.push(
                &FragmentHeader {
                    sequence: seq,
                    fragment_count: 2,
                    fragment_number: 0,
                    total_len: 1024 * 1024,
                    offset: 0,
                },
                b"z",
                &mut stats,
            );
        }

        assert!(
            re.reserved_bytes() <= MAX_PENDING_BYTES,
            "reserved {} exceeds the {} budget",
            re.reserved_bytes(),
            MAX_PENDING_BYTES
        );
        assert!(stats.fragments_rejected > 0, "the budget must have bitten");
    }

    #[test]
    fn the_budget_is_released_when_a_message_completes() {
        let payload = event_payload();
        let cut = payload.len() / 2;

        let make = |number: i32, offset: i32, data: &[u8]| {
            let mut body = Vec::new();
            body.extend_from_slice(&4242i32.to_be_bytes());
            body.extend_from_slice(&2i32.to_be_bytes());
            body.extend_from_slice(&number.to_be_bytes());
            body.extend_from_slice(&(payload.len() as i32).to_be_bytes());
            body.extend_from_slice(&offset.to_be_bytes());
            body.extend_from_slice(data);
            let mut c = vec![p16::command::SEND_RELIABLE_FRAGMENT, 0, 0, 0];
            c.extend_from_slice(&((body.len() + PHOTON_COMMAND_HEADER_LEN) as i32).to_be_bytes());
            c.extend_from_slice(&1i32.to_be_bytes());
            c.extend_from_slice(&body);
            packet(0x04, &[c])
        };

        let mut parser = Parser::new();
        let mut seen = 0;
        parser.handle_packet(&make(0, 0, &payload[..cut]), &mut |_| seen += 1);
        parser.handle_packet(&make(1, cut as i32, &payload[cut..]), &mut |_| seen += 1);

        assert_eq!(seen, 1);
        assert_eq!(
            parser.reassembler.reserved_bytes(),
            0,
            "a completed message must release its reservation"
        );
    }

    #[test]
    fn crc32c_matches_known_vector() {
        // "123456789" under CRC32-C is 0xE3069283.
        assert_eq!(crc32c(b"123456789"), 0xe306_9283);
    }
}
