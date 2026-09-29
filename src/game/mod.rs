//! Game-domain state: who is present, and what they looted.
//!
//! Albion sends loot events and identity events on separate channels and in no
//! particular order, so loot-ledger keeps its own registry and reconciles the two.
//! A player first seen in a loot event has no guild; when their character event
//! finally arrives the record is upgraded in place. Nothing is discarded just
//! because we did not know who they were yet.

pub mod events;

use std::collections::HashMap;

use crate::proto::p16::{Body, Event, Operation, EVENT_ID_KEY};
pub use events::{LootGrab, PlayerIdentity};

/// Milliseconds since the Unix epoch, or 0 if the clock is before it.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// What loot-ledger knows about one player.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Player {
    pub name: String,
    pub guild: Option<String>,
    pub alliance: Option<String>,
    /// When loot-ledger first saw this player.
    pub first_seen_ms: i64,
    /// When loot-ledger most recently saw them.
    pub last_seen_ms: i64,
    /// How many distinct loot grabs they made.
    pub grabs: u64,
    /// Total units looted, ignoring silver.
    pub units: u64,
    /// Whether this is the local player.
    pub is_self: bool,
}

impl Player {
    /// A freshly discovered player, known by name only.
    pub fn new(name: String, at_ms: i64) -> Player {
        Player {
            name,
            guild: None,
            alliance: None,
            first_seen_ms: at_ms,
            last_seen_ms: at_ms,
            grabs: 0,
            units: 0,
            is_self: false,
        }
    }

    /// `GuildName` in brackets, the way the game itself renders a name.
    pub fn decorated(&self) -> String {
        match &self.guild {
            Some(g) if !g.is_empty() => format!("[{g}] {}", self.name),
            _ => self.name.clone(),
        }
    }
}

/// A loot grab with the item name resolved.
#[derive(Debug, Clone, PartialEq)]
pub struct LootRecord {
    pub at_ms: i64,
    pub looted_by: String,
    pub looted_from: String,
    pub quantity: i64,
    pub is_silver: bool,
    /// Numeric item id, absent for silver.
    pub item_num_id: Option<i64>,
    /// Unique item name, absent for silver and unknown items.
    pub item_unique: Option<String>,
    /// Display item name, absent for silver and unknown items.
    pub item_name: Option<String>,
}

/// Why a zone-join marker was recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZoneJoin {
    pub at_ms: i64,
    /// The local player, if the game told us.
    pub player: Option<String>,
}

/// Everything the dashboard needs, plus what the journal needs to replay.
#[derive(Debug, Default)]
pub struct GameState {
    players: HashMap<String, Player>,
    /// Most recent grabs, newest last, capped at `feed_capacity`.
    feed: Vec<LootRecord>,
    feed_capacity: usize,
    zone_joins: Vec<ZoneJoin>,
    self_name: Option<String>,
    totals: Totals,
    /// Event ids seen this session, so a protocol renumbering is visible.
    events_seen: HashMap<i64, u64>,
}

/// Running counts, cheap to render and useful at a glance.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Totals {
    /// Loot events of any kind.
    pub events: u64,
    /// Item grabs, excluding silver.
    pub item_grabs: u64,
    /// Silver transfers.
    pub silver_grabs: u64,
    /// Units of items looted, excluding silver.
    pub units: u64,
    /// Players seen.
    pub players: u64,
    /// Grabs whose item id was not in the catalogue.
    pub unknown_items: u64,
}

/// How much history the dashboard keeps in memory.
const DEFAULT_FEED_CAPACITY: usize = 2_000;
/// How many zone joins to remember.
const ZONE_JOIN_CAPACITY: usize = 200;

impl GameState {
    /// An empty registry.
    pub fn new() -> GameState {
        GameState {
            players: HashMap::new(),
            feed: Vec::new(),
            feed_capacity: DEFAULT_FEED_CAPACITY,
            zone_joins: Vec::new(),
            self_name: None,
            totals: Totals::default(),
            events_seen: HashMap::new(),
        }
    }

    /// The local player, once the game has told us.
    pub fn self_name(&self) -> Option<&str> {
        self.self_name.as_deref()
    }

    /// Every player seen this session, most recently active first.
    pub fn players(&self) -> Vec<Player> {
        let mut out: Vec<Player> = self.players.values().cloned().collect();
        out.sort_by(|a, b| {
            b.last_seen_ms
                .cmp(&a.last_seen_ms)
                .then_with(|| a.name.cmp(&b.name))
        });
        out
    }

    /// The loot feed, oldest first.
    pub fn feed(&self) -> &[LootRecord] {
        &self.feed
    }

    /// The most recent grabs, newest last.
    pub fn recent(&self, n: usize) -> &[LootRecord] {
        let start = self.feed.len().saturating_sub(n);
        &self.feed[start..]
    }

    /// Zone-join markers, oldest first.
    pub fn zone_joins(&self) -> &[ZoneJoin] {
        &self.zone_joins
    }

    /// Record a zone-join marker, used when replaying the journal.
    pub fn note_zone_join(&mut self, join: ZoneJoin) {
        self.zone_joins.push(join);
        if self.zone_joins.len() > ZONE_JOIN_CAPACITY {
            let excess = self.zone_joins.len() - ZONE_JOIN_CAPACITY;
            self.zone_joins.drain(0..excess);
        }
    }

    /// Running counts.
    pub fn totals(&self) -> Totals {
        self.totals
    }

    /// How many times each internal event id has been seen this session.
    pub fn events_seen(&self) -> &HashMap<i64, u64> {
        &self.events_seen
    }

    /// Look up a player.
    pub fn player(&self, name: &str) -> Option<&Player> {
        self.players.get(name)
    }

    /// Route one decoded protocol body into the registry.
    ///
    /// Returns the records that changed, for the journal and the live feed.
    /// A body we do not model is counted and ignored.
    pub fn ingest(&mut self, body: &Body, at_ms: i64) -> Vec<Change> {
        let mut changes = Vec::new();

        match body {
            Body::Event(e) => self.ingest_event(e, at_ms, &mut changes),
            Body::Operation(o) => self.ingest_operation(o, at_ms, &mut changes),
        }

        changes
    }

    fn ingest_event(&mut self, event: &Event, at_ms: i64, out: &mut Vec<Change>) {
        let Some(id) = event.params.get_i64(EVENT_ID_KEY) else {
            return;
        };
        *self.events_seen.entry(id).or_insert(0) += 1;

        match id {
            events::event::EV_OTHER_GRABBED_LOOT => {
                if let Some(grab) = events::parse_other_grabbed_loot(event, at_ms) {
                    if let Some(record) = self.record_grab(grab) {
                        out.push(Change::Loot(record));
                    }
                }
            }
            events::event::EV_NEW_CHARACTER => {
                if let Some(identity) = events::parse_new_character(event) {
                    if let Some(p) = self.apply_identity(&identity, at_ms, false) {
                        out.push(Change::Player(p));
                    }
                }
            }
            events::event::EV_CHARACTER_STATS => {
                if let Some(identity) = events::parse_character_stats(event) {
                    if let Some(p) = self.apply_identity(&identity, at_ms, false) {
                        out.push(Change::Player(p));
                    }
                }
            }
            _ => {}
        }
    }

    fn ingest_operation(&mut self, op: &Operation, at_ms: i64, out: &mut Vec<Change>) {
        let id = op.params.get_i64(crate::proto::p16::OP_ID_KEY).unwrap_or(0);

        if id == events::operation::OP_JOIN {
            let identity = events::parse_join(op);

            if let Some(ref i) = identity {
                if let Some(p) = self.apply_identity(i, at_ms, true) {
                    out.push(Change::Player(p));
                }
            }

            self.note_zone_join(ZoneJoin {
                at_ms,
                player: identity.map(|i| i.name),
            });
        }
    }

    /// Fold a raw grab into a fully resolved record and apply it.
    fn record_grab(&mut self, grab: LootGrab) -> Option<LootRecord> {
        let (item_unique, item_name, unknown) = match grab.item_num_id {
            Some(num_id) => match crate::items::catalogue().get(num_id) {
                Some(item) => (
                    Some(item.unique_name.clone()),
                    Some(item.display_name.clone()),
                    false,
                ),
                None => (None, None, true),
            },
            None => (None, None, false),
        };

        let record = LootRecord {
            at_ms: grab.at_ms,
            looted_by: grab.looted_by,
            looted_from: grab.looted_from,
            quantity: grab.quantity,
            is_silver: grab.is_silver,
            item_num_id: grab.item_num_id,
            item_unique,
            item_name,
        };

        self.apply_loot(&record, unknown);
        Some(record)
    }

    /// Apply an already-resolved loot record.
    ///
    /// Shared by the live path and by journal replay, so a restarted process
    /// rebuilds exactly the state it had before. `unknown` records that the
    /// item id was missing from the catalogue, which replay cannot infer
    /// without re-running the lookup.
    pub fn apply_loot(&mut self, record: &LootRecord, unknown: bool) {
        let entry = self
            .players
            .entry(record.looted_by.clone())
            .or_insert_with(|| Player::new(record.looted_by.clone(), record.at_ms));
        entry.last_seen_ms = record.at_ms;
        entry.grabs += 1;
        if !record.is_silver {
            entry.units += record.quantity.max(0) as u64;
        }

        // The victim is a player too, and seeing them is how their profile gets
        // established even when they have not looted anything themselves.
        self.players
            .entry(record.looted_from.clone())
            .or_insert_with(|| Player::new(record.looted_from.clone(), record.at_ms));
        self.totals.players = self.players.len() as u64;

        self.totals.events += 1;
        if record.is_silver {
            self.totals.silver_grabs += 1;
        } else {
            self.totals.item_grabs += 1;
            self.totals.units += record.quantity.max(0) as u64;
        }
        if unknown {
            self.totals.unknown_items += 1;
        }

        self.feed.push(record.clone());
        if self.feed.len() > self.feed_capacity {
            let excess = self.feed.len() - self.feed_capacity;
            self.feed.drain(0..excess);
        }
    }

    /// Apply a player identity, used by the live path and by replay.
    ///
    /// Returns the updated profile when something actually changed.
    pub fn apply_identity(
        &mut self,
        identity: &PlayerIdentity,
        at_ms: i64,
        is_self: bool,
    ) -> Option<Player> {
        let snapshot = {
            let entry = self
                .players
                .entry(identity.name.clone())
                .or_insert_with(|| Player::new(identity.name.clone(), at_ms));

            let mut changed = false;

            if let Some(g) = &identity.guild {
                if entry.guild.as_deref() != Some(g.as_str()) {
                    entry.guild = Some(g.clone());
                    changed = true;
                }
            }
            if let Some(a) = &identity.alliance {
                if entry.alliance.as_deref() != Some(a.as_str()) {
                    entry.alliance = Some(a.clone());
                    changed = true;
                }
            }
            if is_self && !entry.is_self {
                entry.is_self = true;
                changed = true;
            }
            if entry.last_seen_ms < at_ms {
                entry.last_seen_ms = at_ms;
            }

            if changed {
                Some(entry.clone())
            } else {
                None
            }
        };

        self.totals.players = self.players.len() as u64;
        if is_self {
            self.self_name = Some(identity.name.clone());
        }

        snapshot
    }
}

/// A mutation worth persisting and broadcasting.
#[derive(Debug, Clone, PartialEq)]
pub enum Change {
    /// A new or updated player profile.
    Player(Player),
    /// A new loot record.
    Loot(LootRecord),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::p16::{ty, Params, Value};

    fn loot_event(looted_by: &str, looted_from: &str, num_id: i64, qty: i64) -> Body {
        Body::Event(Event {
            code: 1,
            params: Params::from_pairs(vec![
                (1, Value::String(looted_from.to_owned())),
                (2, Value::String(looted_by.to_owned())),
                (3, Value::Bool(false)),
                (4, Value::Int32(num_id as i32)),
                (5, Value::Int32(qty as i32)),
                (
                    EVENT_ID_KEY,
                    Value::Int32(events::event::EV_OTHER_GRABBED_LOOT as i32),
                ),
            ]),
        })
    }

    fn character_event(name: &str, guild: &str, alliance: &str) -> Body {
        Body::Event(Event {
            code: 1,
            params: Params::from_pairs(vec![
                (1, Value::String(name.to_owned())),
                (8, Value::String(guild.to_owned())),
                (51, Value::String(alliance.to_owned())),
                (
                    EVENT_ID_KEY,
                    Value::Int32(events::event::EV_NEW_CHARACTER as i32),
                ),
            ]),
        })
    }

    #[test]
    fn records_a_grab_and_attaches_the_item_name() {
        let mut state = GameState::new();
        let changes = state.ingest(&loot_event("Grim", "BossRat", 2, 4), 1_000);

        assert_eq!(changes.len(), 1);
        let Change::Loot(record) = &changes[0] else {
            panic!("expected a loot change")
        };
        assert_eq!(record.looted_by, "Grim");
        assert_eq!(
            record.item_name.as_deref(),
            Some("Journeyman's Tracking Toolkit")
        );

        let player = state.player("Grim").unwrap();
        assert_eq!(player.grabs, 1);
        assert_eq!(player.units, 4);
        assert_eq!(state.totals().item_grabs, 1);
    }

    #[test]
    fn a_later_character_event_upgrades_an_existing_player() {
        // The whole point of the registry: loot arrives before identity.
        let mut state = GameState::new();
        state.ingest(&loot_event("Grim", "BossRat", 2, 1), 1_000);
        assert_eq!(state.player("Grim").unwrap().guild, None);

        let changes = state.ingest(&character_event("Grim", "The Vanguished", "Caoimhe"), 2_000);
        assert_eq!(changes.len(), 1);

        let player = state.player("Grim").unwrap();
        assert_eq!(player.guild.as_deref(), Some("The Vanguished"));
        assert_eq!(player.alliance.as_deref(), Some("Caoimhe"));
        assert_eq!(player.grabs, 1, "upgrade must not reset the counters");
        assert_eq!(player.first_seen_ms, 1_000);
        assert_eq!(player.last_seen_ms, 2_000);
    }

    #[test]
    fn the_victim_is_registered_too() {
        let mut state = GameState::new();
        state.ingest(&loot_event("Grim", "BossRat", 2, 1), 1_000);
        assert!(state.player("BossRat").is_some());
    }

    #[test]
    fn repeated_identity_events_do_not_emit_duplicate_changes() {
        let mut state = GameState::new();
        assert_eq!(
            state
                .ingest(&character_event("Grim", "G", "A"), 1_000)
                .len(),
            1
        );
        assert_eq!(
            state
                .ingest(&character_event("Grim", "G", "A"), 2_000)
                .len(),
            0
        );
    }

    #[test]
    fn guild_changes_are_recorded() {
        let mut state = GameState::new();
        state.ingest(&character_event("Grim", "Old", ""), 1_000);
        state.ingest(&character_event("Grim", "New", ""), 2_000);
        assert_eq!(state.player("Grim").unwrap().guild.as_deref(), Some("New"));
    }

    #[test]
    fn unknown_item_ids_are_counted_not_fatal() {
        let mut state = GameState::new();
        let changes = state.ingest(&loot_event("Grim", "BossRat", 9_999_999, 1), 1_000);
        assert_eq!(changes.len(), 1);
        let Change::Loot(r) = &changes[0] else {
            panic!("expected loot")
        };
        assert!(r.item_name.is_none());
        assert_eq!(r.item_num_id, Some(9_999_999));
        assert_eq!(state.totals().unknown_items, 1);
    }

    #[test]
    fn silver_is_tracked_separately() {
        let mut state = GameState::new();
        let body = Body::Event(Event {
            code: 1,
            params: Params::from_pairs(vec![
                (2, Value::String("Grim".into())),
                (3, Value::Bool(true)),
                (5, Value::Int32(1000)),
                (
                    EVENT_ID_KEY,
                    Value::Int32(events::event::EV_OTHER_GRABBED_LOOT as i32),
                ),
            ]),
        });
        state.ingest(&body, 1_000);
        assert_eq!(state.totals().silver_grabs, 1);
        assert_eq!(state.totals().units, 0);
        assert_eq!(state.player("Grim").unwrap().units, 0);
    }

    #[test]
    fn the_feed_is_capped_and_keeps_the_newest() {
        let mut state = GameState::new();
        state.feed_capacity = 5;
        for i in 0..10 {
            state.ingest(&loot_event("Grim", "Rat", 2, 1), i);
        }
        assert_eq!(state.feed().len(), 5);
        assert_eq!(state.recent(1)[0].at_ms, 9);
        assert_eq!(
            state.totals().item_grabs,
            10,
            "totals must survive trimming"
        );
    }

    #[test]
    fn players_are_ordered_most_recent_first() {
        let mut state = GameState::new();
        state.ingest(&loot_event("A", "Rat", 2, 1), 100);
        state.ingest(&loot_event("B", "Rat", 2, 1), 300);
        state.ingest(&loot_event("C", "Rat", 2, 1), 200);
        let names: Vec<String> = state.players().iter().map(|p| p.name.clone()).collect();
        assert_eq!(names[0], "B");
        assert_eq!(names[1], "C");
        assert_eq!(names[2], "A");
    }

    #[test]
    fn event_ids_are_counted_for_diagnostics() {
        let mut state = GameState::new();
        state.ingest(&loot_event("Grim", "Rat", 2, 1), 1);
        state.ingest(&loot_event("Grim", "Rat", 2, 1), 2);
        assert_eq!(state.events_seen()[&275], 2);
    }

    #[test]
    fn unmodelled_events_are_ignored_safely() {
        let mut state = GameState::new();
        let body = Body::Event(Event {
            code: 1,
            params: Params::from_pairs(vec![(EVENT_ID_KEY, Value::Int32(9999))]),
        });
        assert!(state.ingest(&body, 1).is_empty());
    }

    #[test]
    fn bodies_with_no_event_id_are_ignored() {
        let mut state = GameState::new();
        let body = Body::Event(Event {
            code: 1,
            params: Params::from_pairs(vec![]),
        });
        assert!(state.ingest(&body, 1).is_empty());
    }

    #[test]
    fn decorated_name_uses_guild_when_known() {
        let mut p = Player::new("Grim".into(), 0);
        assert_eq!(p.decorated(), "Grim");
        p.guild = Some("Vanguard".into());
        assert_eq!(p.decorated(), "[Vanguard] Grim");
    }

    #[test]
    fn unused_type_import_guard() {
        let _ = ty::INT32;
    }
}
