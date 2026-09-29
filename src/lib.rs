//! loot-ledger — record the loot players pick up in your Albion Online zone.
//!
//! # What this is
//!
//! Albion sends your client a message whenever a player you can see loots
//! something. loot-ledger reads those messages off your own network interface
//! and writes them down: who looted, what, how much, and from whom. It shows
//! the result as a live list of players and their grabs.
//!
//! # What this is not
//!
//! It is not a cheat and not a map overlay. It draws no positions, shows
//! nothing the game did not already send your client, injects no packets,
//! modifies no game files, and automates no input. It is a passive reader of
//! traffic already crossing your machine — equivalent in kind to running
//! Wireshark on Albion's port.
//!
//! # What it can and cannot see
//!
//! The server only tells your client about entities it considers visible, so
//! the ceiling is: your current zone, your party, your guild, and your
//! alliance. Loot on another continent is not in the traffic and cannot be
//! recovered from it. The dashboard labels what it is showing so that limit is
//! never invisible.
//!
//! # Module map
//!
//! * [`capture`] — raw packet capture through `AF_PACKET`, with a kernel-side
//!   BPF filter assembled in [`capture::bpf`].
//! * [`proto`] — the wire formats: [`proto::photon`] for packet framing and
//!   fragment reassembly, [`proto::p16`] for the serialised parameter table.
//! * [`game`] — Albion's event ids and the in-memory player/loot registry.
//! * [`store`] — the append-only journal and its replay.
//! * [`web`] — the dashboard's HTTP and Server-Sent Events surface.
//! * [`util::json`] — the JSON reader and writer the project uses instead of
//!   a dependency.
//! * [`items`] — the embedded item catalogue.

pub mod capture;
pub mod game;
pub mod items;
pub mod proto;
pub mod store;
pub mod util;
pub mod web;
