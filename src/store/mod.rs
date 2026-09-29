//! Persistence: an append-only journal of everything loot-ledger has seen.
//!
//! The journal is newline-delimited JSON. That format is a deliberate choice:
//! it survives a power cut mid-write (the last line may be partial, and replay
//! skips it), it is trivial to inspect with `tail`, `grep` or a text editor, and
//! it needs no database server or C library.
//!
//! Field names are short because a busy zone produces a lot of lines.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use crate::game::{Change, GameState, LootRecord, PlayerIdentity, ZoneJoin};
use crate::util::json::{parse, Json};

/// Field names used in the journal.
mod key {
    pub const TIME: &str = "t";
    pub const KIND: &str = "k";
    pub const LOOTED_BY: &str = "by";
    pub const LOOTED_FROM: &str = "fr";
    pub const ITEM_NUM_ID: &str = "i";
    pub const ITEM_UNIQUE: &str = "u";
    pub const ITEM_NAME: &str = "n";
    pub const QUANTITY: &str = "q";
    pub const SILVER: &str = "s";

    pub const PLAYER: &str = "p";
    pub const GUILD: &str = "g";
    pub const ALLIANCE: &str = "a";
    pub const IS_SELF: &str = "me";
    pub const NAME: &str = "nm";

    pub const LOOT: &str = "loot";
    pub const PLAYER_KIND: &str = "player";
    pub const ZONE: &str = "zone";
}

/// What a journal line decodes to.
#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    Loot(LootRecord),
    Player {
        at_ms: i64,
        identity: PlayerIdentity,
        is_self: bool,
    },
    Zone(ZoneJoin),
}

/// An open, appending journal.
pub struct Journal {
    path: PathBuf,
    writer: Option<BufWriter<File>>,
    lines_written: u64,
}

impl Journal {
    /// Open a journal for appending, creating it if necessary.
    pub fn open(path: &Path) -> std::io::Result<Journal> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }

        // The journal records other players' names, so it is created
        // owner-only rather than at the process umask's default (typically
        // 0644, which is world-readable).
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(path)?;

        Ok(Journal {
            path: path.to_path_buf(),
            writer: Some(BufWriter::new(file)),
            lines_written: 0,
        })
    }

    /// Where this journal lives.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// How many lines this process has appended.
    pub fn lines_written(&self) -> u64 {
        self.lines_written
    }

    /// Append one change.
    pub fn append(&mut self, change: &Change) -> std::io::Result<()> {
        let line = match change {
            Change::Loot(r) => encode_entry(&Entry::Loot(r.clone())),
            Change::Player(p) => encode_entry(&Entry::Player {
                at_ms: p.last_seen_ms,
                identity: PlayerIdentity {
                    name: p.name.clone(),
                    guild: p.guild.clone(),
                    alliance: p.alliance.clone(),
                },
                is_self: p.is_self,
            }),
        };

        self.write_line(&line)
    }

    /// Append a zone-join marker.
    pub fn append_zone(&mut self, join: &ZoneJoin) -> std::io::Result<()> {
        self.write_line(&encode_entry(&Entry::Zone(join.clone())))
    }

    fn write_line(&mut self, line: &str) -> std::io::Result<()> {
        let Some(writer) = self.writer.as_mut() else {
            return Ok(());
        };
        writer.write_all(line.as_bytes())?;
        writer.write_all(b"\n")?;
        self.lines_written += 1;
        Ok(())
    }

    /// Push buffered lines to disk.
    pub fn flush(&mut self) -> std::io::Result<()> {
        if let Some(writer) = self.writer.as_mut() {
            writer.flush()?;
        }
        Ok(())
    }

    /// Stop writing, keeping what is already buffered.
    pub fn close(&mut self) {
        if let Some(mut writer) = self.writer.take() {
            let _ = writer.flush();
        }
    }
}

impl Drop for Journal {
    fn drop(&mut self) {
        self.close();
    }
}

/// Encode a journal entry as one line of JSON.
pub fn encode_entry(entry: &Entry) -> String {
    match entry {
        Entry::Loot(r) => Json::obj([
            (key::TIME, Json::Int(r.at_ms)),
            (key::KIND, Json::Str(key::LOOT.into())),
            (key::LOOTED_BY, Json::Str(r.looted_by.clone())),
            (key::LOOTED_FROM, Json::Str(r.looted_from.clone())),
            (
                key::ITEM_NUM_ID,
                r.item_num_id.map_or(Json::Null, Json::Int),
            ),
            (
                key::ITEM_UNIQUE,
                r.item_unique.clone().map_or(Json::Null, Json::Str),
            ),
            (
                key::ITEM_NAME,
                r.item_name.clone().map_or(Json::Null, Json::Str),
            ),
            (key::QUANTITY, Json::Int(r.quantity)),
            (key::SILVER, Json::Bool(r.is_silver)),
        ])
        .to_string(),

        Entry::Player {
            at_ms,
            identity,
            is_self,
        } => Json::obj([
            (key::TIME, Json::Int(*at_ms)),
            (key::KIND, Json::Str(key::PLAYER_KIND.into())),
            (key::NAME, Json::Str(identity.name.clone())),
            (
                key::GUILD,
                identity.guild.clone().map_or(Json::Null, Json::Str),
            ),
            (
                key::ALLIANCE,
                identity.alliance.clone().map_or(Json::Null, Json::Str),
            ),
            (key::IS_SELF, Json::Bool(*is_self)),
        ])
        .to_string(),

        Entry::Zone(z) => Json::obj([
            (key::TIME, Json::Int(z.at_ms)),
            (key::KIND, Json::Str(key::ZONE.into())),
            (key::PLAYER, z.player.clone().map_or(Json::Null, Json::Str)),
        ])
        .to_string(),
    }
}

/// Decode one journal line.
///
/// Returns `None` for a line that is not a journal entry — including a
/// truncated final line, which is the expected shape of a journal that was cut
/// off mid-write.
pub fn decode_entry(line: &str) -> Option<Entry> {
    let v = parse(line.trim()).ok()?;

    match v.get(key::KIND)?.as_str()? {
        key::LOOT => Some(Entry::Loot(LootRecord {
            at_ms: v.get(key::TIME)?.as_i64()?,
            looted_by: v.get(key::LOOTED_BY)?.as_str()?.to_owned(),
            looted_from: v.get(key::LOOTED_FROM)?.as_str()?.to_owned(),
            quantity: v.get(key::QUANTITY)?.as_i64()?,
            is_silver: v.get(key::SILVER)?.as_bool()?,
            item_num_id: v.get(key::ITEM_NUM_ID)?.as_i64(),
            item_unique: v
                .get(key::ITEM_UNIQUE)
                .and_then(Json::as_str)
                .map(str::to_owned),
            item_name: v
                .get(key::ITEM_NAME)
                .and_then(Json::as_str)
                .map(str::to_owned),
        })),

        key::PLAYER_KIND => Some(Entry::Player {
            at_ms: v.get(key::TIME)?.as_i64()?,
            identity: PlayerIdentity {
                name: v.get(key::NAME)?.as_str()?.to_owned(),
                guild: v.get(key::GUILD).and_then(Json::as_str).map(str::to_owned),
                alliance: v
                    .get(key::ALLIANCE)
                    .and_then(Json::as_str)
                    .map(str::to_owned),
            },
            is_self: v.get(key::IS_SELF).and_then(Json::as_bool).unwrap_or(false),
        }),

        key::ZONE => Some(Entry::Zone(ZoneJoin {
            at_ms: v.get(key::TIME)?.as_i64()?,
            player: v.get(key::PLAYER).and_then(Json::as_str).map(str::to_owned),
        })),

        _ => None,
    }
}

/// What a replay managed to read.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ReplayStats {
    /// Lines successfully applied.
    pub applied: u64,
    /// Lines skipped: blank, malformed, or truncated.
    pub skipped: u64,
    /// True when the journal was longer than the replay window and older
    /// records were left on disk rather than loaded.
    pub truncated: bool,
}

/// How much of the journal's tail is read back at startup.
///
/// The file itself is never truncated — it is the complete record — but
/// replaying every line ever written would make startup time and memory grow
/// without limit as the journal accumulates. Reading a bounded tail keeps
/// startup flat however long the journal has been running.
///
/// The trade-off is honest and worth stating: a player's `firstSeen` reflects
/// the start of this window, not their first appearance in the game.
const REPLAY_WINDOW_BYTES: u64 = 32 * 1024 * 1024;

/// Rebuild game state from the tail of a journal.
///
/// A missing file is not an error — it just means there is no history yet.
pub fn replay(path: &Path, state: &mut GameState) -> std::io::Result<ReplayStats> {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(ReplayStats::default()),
        Err(e) => return Err(e),
    };

    let len = file.metadata()?.len();
    let start = len.saturating_sub(REPLAY_WINDOW_BYTES);

    let mut reader = BufReader::new(file);
    if start > 0 {
        use std::io::Seek;
        reader.seek(std::io::SeekFrom::Start(start))?;

        // The seek almost certainly lands mid-line. The first line read is
        // therefore partial and must be discarded, or a truncated JSON object
        // would be counted as a skipped record on every start.
        let mut discard = String::new();
        if reader.read_line(&mut discard)? > 0 {
            // Drop the partial first line; everything after it is intact.
        }
    }

    let mut stats = ReplayStats {
        truncated: start > 0,
        ..Default::default()
    };

    for line in reader.lines() {
        // A line that is not valid UTF-8 is corruption; stop rather than
        // silently skip, because the rest of the file is probably fine.
        let line = line?;

        let Some(entry) = decode_entry(&line) else {
            stats.skipped += 1;
            continue;
        };

        match entry {
            Entry::Loot(r) => {
                // The item was already named when it was logged, so replay
                // does not need the catalogue.
                state.apply_loot(&r, false);
            }
            Entry::Player {
                at_ms,
                identity,
                is_self,
            } => {
                state.apply_identity(&identity, at_ms, is_self);
            }
            Entry::Zone(z) => {
                state.note_zone_join(z);
            }
        }

        stats.applied += 1;
    }

    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game::Player;

    fn record() -> LootRecord {
        LootRecord {
            at_ms: 1_700_000_000_000,
            looted_by: "Grim".into(),
            looted_from: "BossRat".into(),
            quantity: 7,
            is_silver: false,
            item_num_id: Some(2),
            item_unique: Some("T3_2H_TOOL_TRACKING".into()),
            item_name: Some("Journeyman's Tracking Toolkit".into()),
        }
    }

    fn temp_path(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "loot-ledger-test-{tag}-{}-{:?}.jsonl",
            std::process::id(),
            std::thread::current().id()
        ));
        p
    }

    #[test]
    fn journal_is_created_owner_only() {
        // It records other players' names. The process umask would otherwise
        // make it world-readable on a typical install.
        let path = temp_path("mode");
        let _ = std::fs::remove_file(&path);

        {
            let mut j = Journal::open(&path).unwrap();
            j.close();
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(
                mode, 0o600,
                "journal must not be readable by other users, got {mode:o}"
            );
        }

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn loot_entry_round_trips() {
        let line = encode_entry(&Entry::Loot(record()));
        assert_eq!(decode_entry(&line), Some(Entry::Loot(record())));
    }

    #[test]
    fn silver_entry_round_trips_with_nulls() {
        let r = LootRecord {
            at_ms: 1,
            looted_by: "Grim".into(),
            looted_from: "Grim".into(),
            quantity: 500,
            is_silver: true,
            item_num_id: None,
            item_unique: None,
            item_name: None,
        };
        let line = encode_entry(&Entry::Loot(r.clone()));
        assert!(line.contains("\"i\":null"));
        assert_eq!(decode_entry(&line), Some(Entry::Loot(r)));
    }

    #[test]
    fn player_entry_round_trips_including_the_self_flag() {
        for is_self in [true, false] {
            let e = Entry::Player {
                at_ms: 99,
                identity: PlayerIdentity {
                    name: "Grim".into(),
                    guild: Some("Vanguard".into()),
                    alliance: None,
                },
                is_self,
            };
            assert_eq!(decode_entry(&encode_entry(&e)), Some(e));
        }
    }

    #[test]
    fn replay_restores_which_player_is_the_local_one() {
        let path = temp_path("self");
        let _ = std::fs::remove_file(&path);

        {
            let mut j = Journal::open(&path).unwrap();
            j.append(&Change::Player(Player {
                name: "Grim".into(),
                guild: None,
                alliance: None,
                first_seen_ms: 1,
                last_seen_ms: 1,
                grabs: 0,
                units: 0,
                is_self: true,
            }))
            .unwrap();
            j.flush().unwrap();
        }

        let mut state = GameState::new();
        replay(&path, &mut state).unwrap();

        assert_eq!(state.self_name(), Some("Grim"));
        assert!(state.player("Grim").unwrap().is_self);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn zone_entry_round_trips() {
        let e = Entry::Zone(ZoneJoin {
            at_ms: 5,
            player: Some("Grim".into()),
        });
        assert_eq!(decode_entry(&encode_entry(&e)), Some(e));
    }

    #[test]
    fn names_with_quotes_and_unicode_survive() {
        let mut r = record();
        r.looted_by = "Grim \"The\" 日本語".into();
        r.item_name = Some("Bag\nwith\ttabs".into());
        assert_eq!(
            decode_entry(&encode_entry(&Entry::Loot(r.clone()))),
            Some(Entry::Loot(r))
        );
    }

    #[test]
    fn garbage_lines_decode_to_none_rather_than_panicking() {
        for bad in [
            "",
            "not json",
            "{}",
            "{\"k\":\"loot\"}",
            "{\"k\":\"unknown\",\"t\":1}",
            "{\"k\":\"loot\",\"t\":\"x\"}",
            "[1,2,3]",
        ] {
            assert_eq!(decode_entry(bad), None, "should not decode: {bad:?}");
        }
    }

    #[test]
    fn writes_and_replays_a_session() {
        let path = temp_path("replay");
        let _ = std::fs::remove_file(&path);

        {
            let mut j = Journal::open(&path).unwrap();
            j.append(&Change::Loot(record())).unwrap();
            j.append(&Change::Player(Player {
                name: "Grim".into(),
                guild: Some("Vanguard".into()),
                alliance: Some("Caoimhe".into()),
                first_seen_ms: 1,
                last_seen_ms: 2,
                grabs: 1,
                units: 7,
                is_self: false,
            }))
            .unwrap();
            j.append_zone(&ZoneJoin {
                at_ms: 1,
                player: Some("Grim".into()),
            })
            .unwrap();
            j.flush().unwrap();
            // Loot, player and zone are all counted.
            assert_eq!(j.lines_written(), 3);
        }

        let mut state = GameState::new();
        let stats = replay(&path, &mut state).unwrap();

        assert_eq!(stats.applied, 3);
        assert_eq!(state.totals().item_grabs, 1);
        let p = state.player("Grim").unwrap();
        assert_eq!(p.grabs, 1);
        assert_eq!(p.units, 7);
        assert_eq!(p.guild.as_deref(), Some("Vanguard"));
        assert_eq!(state.zone_joins().len(), 1);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn replaying_a_missing_file_is_not_an_error() {
        let mut state = GameState::new();
        let stats = replay(Path::new("/nonexistent/loot-ledger.jsonl"), &mut state).unwrap();
        assert_eq!(stats, ReplayStats::default());
    }

    #[test]
    fn replay_reads_only_the_tail_of_a_very_long_journal() {
        // Startup must not grow with the size of the journal. Build a file
        // comfortably larger than the window and confirm the old records stay
        // on disk while the recent ones are loaded.
        let path = temp_path("window");
        let _ = std::fs::remove_file(&path);

        {
            let file = std::fs::File::create(&path).unwrap();
            let mut w = std::io::BufWriter::new(file);
            let filler = "x".repeat(512);
            // Write well past the window: 32 MiB / ~600 bytes per record.
            for i in 0..120_000i64 {
                let line = format!(
                    "{{\"t\":{i},\"k\":\"loot\",\"by\":\"Old{i}\",\"fr\":\"R\",\"i\":2,\"u\":\"U\",\"n\":\"{filler}\",\"q\":1,\"s\":false}}\n"
                );
                use std::io::Write;
                w.write_all(line.as_bytes()).unwrap();
            }
            w.flush().unwrap();
        }

        let size = std::fs::metadata(&path).unwrap().len();
        assert!(
            size > REPLAY_WINDOW_BYTES,
            "test file must exceed the window"
        );

        let mut state = GameState::new();
        let stats = replay(&path, &mut state).unwrap();

        assert!(stats.truncated, "replay must report the window was applied");
        assert!(
            stats.applied > 0 && stats.applied < 120_000,
            "expected a bounded subset, applied {}",
            stats.applied
        );
        // The file itself is untouched: the journal remains the full record.
        assert_eq!(std::fs::metadata(&path).unwrap().len(), size);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_short_journal_is_replayed_whole_and_not_marked_truncated() {
        let path = temp_path("short");
        let _ = std::fs::remove_file(&path);
        {
            let mut j = Journal::open(&path).unwrap();
            j.append(&Change::Loot(record())).unwrap();
            j.flush().unwrap();
        }

        let mut state = GameState::new();
        let stats = replay(&path, &mut state).unwrap();
        assert!(!stats.truncated);
        assert_eq!(stats.applied, 1);
        assert_eq!(state.totals().item_grabs, 1);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_truncated_final_line_is_skipped_not_fatal() {
        // Exactly what a power cut mid-write leaves behind.
        let path = temp_path("truncated");
        let mut body = String::new();
        body.push_str(&encode_entry(&Entry::Loot(record())));
        body.push('\n');
        body.push_str("{\"k\":\"loot\",\"t\":1,\"by\":\"Gr");
        std::fs::write(&path, body).unwrap();

        let mut state = GameState::new();
        let stats = replay(&path, &mut state).unwrap();

        assert_eq!(stats.applied, 1);
        assert_eq!(stats.skipped, 1);
        assert_eq!(state.totals().item_grabs, 1);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn appending_creates_missing_directories() {
        let mut p = temp_path("nested");
        p = p.with_file_name("loot-ledger-test-nested-dir/deep/journal.jsonl");
        let _ = std::fs::remove_dir_all(p.parent().unwrap().parent().unwrap());

        let mut j = Journal::open(&p).unwrap();
        j.append(&Change::Loot(record())).unwrap();
        j.flush().unwrap();
        assert!(p.exists());

        let _ = std::fs::remove_dir_all(p.parent().unwrap().parent().unwrap());
    }
}
