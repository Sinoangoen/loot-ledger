//! Decoding Albion's game events into domain types.
//!
//! Every Albion event arrives as one parameter table whose parameter 252 holds
//! the internal event id. Parsing is total: an event we cannot understand
//! yields `None` rather than an error, because the capture loop must keep
//! running regardless of what the game decides to send.

use crate::proto::p16::{Event, Operation, Params};

/// Albion's internal event ids, as observed on the wire.
///
/// These are tied to the current game version. When an update renumbers them,
/// loot stops appearing and these need refreshing — the dashboard surfaces the
/// last-seen event ids precisely so that situation is diagnosable.
pub mod event {
    /// A player entered your visibility range. Carries guild and alliance.
    pub const EV_NEW_CHARACTER: i64 = 29;
    /// Updated stats for a known player.
    pub const EV_CHARACTER_STATS: i64 = 143;
    /// Another player looted something. The event this whole app exists for.
    pub const EV_OTHER_GRABBED_LOOT: i64 = 275;
}

/// Albion's internal operation ids.
pub mod operation {
    /// The response to joining a zone. Identifies the local player.
    pub const OP_JOIN: i64 = 2;
}

/// Parameter ids used by [`EV_OTHER_GRABBED_LOOT`].
mod loot_param {
    /// Who was looted. Absent on silver events.
    pub const LOOTED_FROM: u8 = 1;
    /// Who looted.
    pub const LOOTED_BY: u8 = 2;
    /// Whether the value looted was silver rather than an item.
    pub const IS_SILVER: u8 = 3;
    /// Numeric item id. Absent on silver events.
    pub const ITEM_NUM_ID: u8 = 4;
    /// How many.
    pub const QUANTITY: u8 = 5;
}

/// A loot event, before item-name resolution.
#[derive(Debug, Clone, PartialEq)]
pub struct LootGrab {
    /// Milliseconds since the Unix epoch, local to the machine running loot-ledger.
    pub at_ms: i64,
    /// The player who took the loot.
    pub looted_by: String,
    /// The player or container the loot came from.
    pub looted_from: String,
    /// Numeric item id, absent for silver.
    pub item_num_id: Option<i64>,
    /// How much was taken.
    pub quantity: i64,
    /// True when the value taken was silver.
    pub is_silver: bool,
}

/// A player's identity, as far as loot-ledger can tell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayerIdentity {
    pub name: String,
    pub guild: Option<String>,
    pub alliance: Option<String>,
}

/// Parse `EV_OTHER_GRABBED_LOOT`.
///
/// Silver events legitimately omit the victim and the item id, so those fields
/// are optional rather than an error.
pub fn parse_other_grabbed_loot(event: &Event, at_ms: i64) -> Option<LootGrab> {
    let p = &event.params;

    // The looter is always present and always a string.
    let looted_by = p.get_str(loot_param::LOOTED_BY)?.to_owned();

    let is_silver = p.get_bool(loot_param::IS_SILVER).unwrap_or(false);

    // Silver has no victim; the game sends the looter in that slot instead.
    let looted_from = p
        .get_str(loot_param::LOOTED_FROM)
        .unwrap_or(&looted_by)
        .to_owned();

    let item_num_id = p.get_i64(loot_param::ITEM_NUM_ID);
    let quantity = p.get_i64(loot_param::QUANTITY).unwrap_or(1);

    if !is_silver && item_num_id.is_none() {
        // An item grab with no item id is not something we can report on.
        return None;
    }

    Some(LootGrab {
        at_ms,
        looted_by,
        looted_from,
        item_num_id,
        quantity,
        is_silver,
    })
}

/// Parse `EV_NEW_CHARACTER`, which carries guild and alliance.
pub fn parse_new_character(event: &Event) -> Option<PlayerIdentity> {
    identity_from(&event.params, 8, 51)
}

/// Parse `EV_CHARACTER_STATS`, which carries guild and alliance.
pub fn parse_character_stats(event: &Event) -> Option<PlayerIdentity> {
    identity_from(&event.params, 2, 4)
}

/// Parse the join response, which identifies the local player.
pub fn parse_join(operation: &Operation) -> Option<PlayerIdentity> {
    identity_from(&operation.params, 57, 77)
}

fn identity_from(p: &Params, guild_key: u8, alliance_key: u8) -> Option<PlayerIdentity> {
    let name = p.get_str(1)?.to_owned();

    if name.is_empty() {
        return None;
    }

    Some(PlayerIdentity {
        name,
        guild: clean(p.get_str(guild_key)),
        alliance: clean(p.get_str(alliance_key)),
    })
}

/// Treat an empty string as "not present" — the game sends one rather than
/// omitting the field for guildless players.
fn clean(s: Option<&str>) -> Option<String> {
    s.filter(|v| !v.is_empty()).map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::p16::{Params, Value, OP_ID_KEY};

    fn s(id: u8, text: &str) -> (u8, Value) {
        (id, Value::String(text.to_owned()))
    }

    fn i(id: u8, n: i64) -> (u8, Value) {
        (id, Value::Int32(n as i32))
    }

    fn event(entries: Vec<(u8, Value)>) -> Event {
        Event {
            code: 1,
            params: Params::from_pairs(entries),
        }
    }

    #[test]
    fn parses_an_item_grab() {
        let e = event(vec![
            s(loot_param::LOOTED_FROM, "BossRat"),
            s(loot_param::LOOTED_BY, "Grim"),
            (loot_param::IS_SILVER, Value::Bool(false)),
            i(loot_param::ITEM_NUM_ID, 1234),
            i(loot_param::QUANTITY, 3),
        ]);
        let g = parse_other_grabbed_loot(&e, 1_700_000_000_000).unwrap();
        assert_eq!(g.looted_by, "Grim");
        assert_eq!(g.looted_from, "BossRat");
        assert_eq!(g.item_num_id, Some(1234));
        assert_eq!(g.quantity, 3);
        assert!(!g.is_silver);
    }

    #[test]
    fn parses_a_silver_transfer_without_an_item() {
        let e = event(vec![
            s(loot_param::LOOTED_BY, "Grim"),
            (loot_param::IS_SILVER, Value::Bool(true)),
            i(loot_param::QUANTITY, 5000),
        ]);
        let g = parse_other_grabbed_loot(&e, 0).unwrap();
        assert!(g.is_silver);
        assert_eq!(g.item_num_id, None);
        assert_eq!(g.looted_from, "Grim", "silver has no distinct victim");
    }

    #[test]
    fn integer_flag_counts_as_silver() {
        // The game sometimes sends a plain 0/1 where a boolean is expected.
        let e = event(vec![
            s(loot_param::LOOTED_BY, "Grim"),
            (loot_param::IS_SILVER, Value::Int8(1)),
        ]);
        assert!(parse_other_grabbed_loot(&e, 0).unwrap().is_silver);
    }

    #[test]
    fn rejects_an_item_grab_with_no_item_id() {
        let e = event(vec![
            s(loot_param::LOOTED_FROM, "BossRat"),
            s(loot_param::LOOTED_BY, "Grim"),
            (loot_param::IS_SILVER, Value::Bool(false)),
        ]);
        assert!(parse_other_grabbed_loot(&e, 0).is_none());
    }

    #[test]
    fn rejects_an_event_with_no_looter() {
        let e = event(vec![i(loot_param::QUANTITY, 1)]);
        assert!(parse_other_grabbed_loot(&e, 0).is_none());
    }

    #[test]
    fn parses_player_identity() {
        let e = event(vec![s(1, "Grim"), s(2, "The Vanguished"), s(4, "Caoimhe")]);
        let id = parse_character_stats(&e).unwrap();
        assert_eq!(id.name, "Grim");
        assert_eq!(id.guild.as_deref(), Some("The Vanguished"));
        assert_eq!(id.alliance.as_deref(), Some("Caoimhe"));
    }

    #[test]
    fn empty_guild_reads_as_absent() {
        let e = event(vec![s(1, "Grim"), s(8, ""), s(51, "Caoimhe")]);
        let id = parse_new_character(&e).unwrap();
        assert_eq!(id.guild, None);
        assert_eq!(id.alliance.as_deref(), Some("Caoimhe"));
    }

    #[test]
    fn missing_name_is_rejected() {
        assert!(parse_new_character(&event(vec![s(8, "Guild")])).is_none());
    }

    #[test]
    fn operation_key_is_the_documented_one() {
        assert_eq!(OP_ID_KEY, 253);
    }
}
