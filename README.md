# loot-ledger

Record the loot players pick up in your Albion Online zone, and watch it
happen live in your browser.

Albion sends your game client a message whenever a player you can *see* loots
something. loot-ledger reads those messages off your own network interface and
writes them down — who looted, what, how much, from whom — and shows them as a
live list of players and their grabs.

Single Rust binary. No crates.io dependencies at all.

```
  loot-ledger — recording loot in your current zone

  Dashboard   http://127.0.0.1:7331
  Journal     ./loot-ledger.jsonl
  Interface   all
  Filter      kernel-side, matches only Albion traffic

  Press Ctrl+C to stop.
```

---

## Contents

- [Terms of Service: this breaks § 13.3](#terms-of-service-this-breaks-133)
- [Terms of Service: the practical consequences](#terms-of-service-the-practical-consequences)
- [What it is not](#what-it-is-not)
- [What you can actually see](#what-you-can-actually-see)
- [Install](#install)
- [Permissions](#permissions)
- [Usage](#usage)
- [The dashboard](#the-dashboard)
- [The journal file](#the-journal-file)
- [Keeping item names current](#keeping-item-names-current)
- [How it works](#how-it-works)
- [Testing](#testing)
- [Troubleshooting](#troubleshooting)

---

## Terms of Service: this breaks § 13.3

**Read this before you run it against your account.**

Sandbox Interactive's Terms and Conditions, § 13.3 "No Manipulation", forbid
users from using:

> software enabling "data mining" or which intercepts or captures data
> otherwise in connection with the Website and the Game

**loot-ledger is exactly that software.** Capturing Albion's traffic is
interception, which is what the clause prohibits.

The drafting is deliberate about scope. Three of the seven bullets in § 13.3
carry the qualifier *"unless permitted by the Game Rules"*. The data-mining and
interception bullet does not. And § 1.7 provides that where the Terms and the
Game Rules conflict, **the Terms supersede the Game Rules** — so permission in
the linked forum rules could not rescue it either.

Consequences, from the same document:

- **§ 15.4.4** — violating § 13 is a "compelling reason" for termination.
  Sandbox Interactive "will, without prior warning, immediately suspend all
  access".
- **§ 15.5.2** — extraordinary termination can extend to **all other accounts**
  you have, and they may retain data to prevent you opening more.
- **§ 15.6** — permanent bans are an available sanction.

So the accurate statement is: **this tool is not permitted by Albion's Terms,
and using it while logged in puts your account at risk of suspension without
warning.** That is a fact about the text, not a prediction about enforcement.

The original [ao-loot-logger](https://github.com/matheussampaio/ao-loot-logger)
this is built from is in the same position. Its public distribution says
nothing about permission.

What follows below describes the software accurately. None of it is an argument
that the software is allowed.

## What it is not

Not a cheat, and not a map overlay. Specifically, it:

- **draws no map and no positions** — there is no player location, enemy
  blip, or overlay of any kind;
- **shows nothing the game did not already send your client** — every record it
  writes corresponds to a message your own client received and acted on;
- **never sends a packet.** The capture socket is opened, bound, filtered, and
  only ever read from. The six C functions this program declares are
  `socket`, `bind`, `setsockopt`, `recvfrom`, `close` and `signal` — there is
  no `send`, `sendto` or `sendmsg` among them, so there is no code path by
  which a packet could leave. You can check this yourself:

  ```sh
  grep -n '    fn ' src/capture/sys.rs      # the complete FFI surface
  grep -rnE '\b(sendto|sendmsg|send)\(' src/
  ```

- **touches no game files** and does not read or modify the game process;
- **automates no input** and performs no action on your behalf;
- **does not use the official game API**, and makes no request to any Albion
  server.

It does write in two places, both local and both intentional: the journal file
on disk, and HTTP responses to the dashboard bound to `127.0.0.1`. Those are the
only writes in the program.

The name is deliberately plain — it is a ledger, not a radar — and the
distinction matters for a practical reason, not a legal one: a tool that draws
enemy positions is widely understood as cheating, and nothing in this repository
draws positions. That is a description of scope, not a claim of permission.

## What you can actually see

This is the single most important thing to understand before you start.

Albion's servers only tell your client about entities the server considers
*visible* to you. loot-ledger reads that traffic, so it inherits that limit
exactly. You will see loot from:

- **your current zone** — every player near your character;
- **your party**;
- **your guild** — members anywhere in the world;
- **your alliance** — members anywhere in the world.

**You will not see loot happening on another continent**, unless you share a
guild or alliance with that player. That is not a limitation of this software
that could be engineered away — the information is not in your traffic, so no
amount of packet processing can recover it. Anyone claiming a tool can show you
the whole world's loot is either reading a different data source or doing
something else entirely.

The dashboard states this on screen so the limit is never invisible.

## Install

You need a Rust toolchain. Nothing else — there are no crates to download, no
`npm install`, no `libpcap`, no `libnetfilter`.

```sh
git clone <this repository>
cd loot-ledger
cargo build --release
```

The binary lands at `target/release/loot-ledger`. It is dynamically linked
against your system libc, which every Linux Mint install already has. For a
fully static binary with no dynamic dependencies at all:

```sh
RUSTFLAGS="-C target-feature=+crt-static" cargo build --release
```

Check the whole toolchain is present first:

```sh
sudo apt install build-essential   # or: sudo apt install rustc cargo
```

## Permissions

Reading packets needs the `CAP_NET_RAW` capability. You have two options.

**Option 1 — run with `sudo` each time.** Simplest, but the web dashboard then
runs as root too, which is more privilege than it needs.

**Option 2 — grant the capability to the binary once (recommended).** After
this, you run it as your normal user:

```sh
sudo setcap cap_net_raw+ep ./loot-ledger
```

Note that granting a file capability means *anyone who can execute that file*
gets raw-socket access, so keep the binary somewhere only you can write to.

To confirm the setup, and to check your interfaces, before you even start
playing:

```sh
./loot-ledger --check
```

```
loot-ledger self-check

  [ ok ] CAP_NET_RAW
  [ ok ] 5 interface(s) visible
  [ ok ] 11963 items loaded (embedded snapshot)
  [ ok ] capture and decode verified (1 synthetic event)

All checks passed.
```

## Usage

Start it before you log into the game, or leave it running — it picks up traffic
as soon as Albion connects.

```sh
./loot-ledger                       # all interfaces, dashboard on :7331
./loot-ledger --interface eno1      # one interface only
./loot-ledger --port 8080           # dashboard on a different port
./loot-ledger --journal ~/loot.jsonl
./loot-ledger --no-capture          # review a past session, no packet reading
```

`--no-capture` is worth knowing about: it replays a journal and serves the
dashboard without opening a capture socket at all, so it needs no special
permissions. It is how you look at an old session later.

Other commands:

```sh
./loot-ledger --help
./loot-ledger --list-interfaces
./loot-ledger --refresh-items ~/items.tsv
```

The dashboard binds to `127.0.0.1` only. It is not reachable from your network.

### It will not work if

- you are playing through a **VPN** (Exit Lag and similar) or **GeForce Now** —
  the game traffic then leaves encrypted or through a different path, and there
  is nothing to read. This is a property of those services, not a bug here;
- Albion changes its wire protocol. The internal event ids are hard-coded in
  `src/game/events.rs` and shift with game updates. If loot stops appearing,
  that file is where to look, and the dashboard's `eventsSeen` counter tells
  you whether events are arriving at all but not being recognised.

## The dashboard

`http://127.0.0.1:7331` — a single page, no build step, no JavaScript
dependencies, no external requests.

- a **live loot feed** of who took what, updating as it happens;
- a **player roster** with per-player totals, which is the "who looted" answer;
- a **summary strip** of totals, and a clear *waiting* state when no traffic
  has arrived yet;
- a **connection indicator** that shows when the live stream is reconnecting;
- a **filter box** across both feed and roster.

Everything is compiled into the binary and served from `127.0.0.1`, so it is
not reachable over your network. Nothing it displays is transmitted anywhere.

It does have **no authentication**, because it is a local viewer rather than a
service — but on a multi-user machine any local user or process can reach that
port and read the dashboard, which contains player names. If that matters to
you, run it under a network namespace, or simply do not leave it running on a
shared machine.

## The journal file

Every record is appended to a newline-delimited JSON file, one object per line.
The default is `./loot-ledger.jsonl`; change it with `--journal`.

```jsonl
{"t":1700000000000,"k":"player","nm":"Grim","g":"The Vanguished","a":"Caoimhe","me":true}
{"t":1700000042000,"k":"loot","by":"Grim","fr":"BossRat","i":1234,"u":"T4_BAG_H","n":"Adept's Bag","q":3,"s":false}
{"t":1700000050000,"k":"loot","by":"Grim","fr":"Grim","i":null,"u":null,"n":null,"q":5000,"s":true}
{"t":1700000060000,"k":"zone","p":"Grim"}
```

It is a plain text format on purpose:

- **you can read it.** `tail -f loot-ledger.jsonl`, `grep '"by":"Grim"'`, or
  open it in any text editor. `jq` works on it directly.
- **it needs no database.** No SQLite, no server, no schema migrations.
- **a damaged tail does not cost you the file.** A process killed mid-write
  leaves at most a partial last line, which replay skips.

**What durability actually is.** Records are buffered and written to the
operating system every couple of seconds, not `fsync`ed. So:

- *killing the process* (`Ctrl+C`, a crash) loses at most a couple of seconds;
- *losing power or the machine* can lose whatever the OS had not yet flushed,
  which is typically the last few seconds but is not guaranteed to be bounded.

That is a deliberate trade — `fsync` on every record would make a busy zone
churn the disk, and the journal is a log, not a ledger of record. If you need
harder guarantees, put the journal on a filesystem you trust or copy it
periodically; it is plain text precisely so that is easy.

On startup the tail of the journal is replayed, so the dashboard opens with
your recent history already in it. Replay is capped at the last 32 MiB so
startup stays fast however long the file has grown; older records stay on disk
untouched and are simply not loaded. Pass `--no-replay` to start empty.

If you would rather work in CSV, the journal is a small, stable format and a
five-line converter will do it.

## Keeping item names current

Albion renumbers its item ids when the game updates. A snapshot of 11,963 items
is compiled into the binary, so it works with no network access at all. After a
game patch, items that are not in the table still get logged — they just show
their raw numeric id, and the dashboard counts them under `unknownItems`.

To refresh the table from the community-maintained dump:

```sh
./loot-ledger --refresh-items ~/items.tsv
```

Then replace `src/assets/items.tsv` with it and rebuild.

## Terms of Service: the practical consequences

The finding is in [Terms of Service: this breaks § 13.3](#terms-of-service-this-breaks-133).
This section is about what that means in practice.

### If you do not use it against a live account

`--no-capture` mode is a plain log viewer. It reads an existing journal file,
rebuilds the player and loot tables, and serves the dashboard. It opens no
capture socket, needs no special permissions, and makes no network connection.
Once a journal exists, analysing it is not interception.

That makes the tool useful for two things that carry no account risk:

- **analysis of data you already have** — a journal from a session, a
  spreadsheet export, anything you paste in;
- **protocol study** — the decoder, the golden-vector harness and the test
  suite are a complete, self-contained way to work on Photon's wire format
  without an account, a client, or a packet to sniff.

If you want the project as a protocol and data-analysis library and nothing
else, deleting `src/capture/` and removing the `--no-capture` default leaves
you with exactly that, and the remaining code is the majority of it.

### If you do use it against a live account

Then you are relying on Sandbox Interactive not enforcing § 13.3 against you.
You should understand that this is a choice with a real downside:

- suspension is stated to happen **without prior warning**;
- it can reach **every account** you own, not just the one you were playing;
- they may **retain data** to prevent you creating further accounts;
- permanent bans are an available sanction.

None of that is a prediction about how often enforcement happens. It is what the
document says the remedy is. The wide public use of packet-logging tools in
Albion is not evidence that the clause is not applied — plenty of rules go
unenforced for years and then do not.

### Neither of us can settle this

I am not a lawyer, and this is not legal advice. I have read the clause and
applied it to the code, and on that reading the tool is not permitted. If you
want a view with legal weight, ask Sandbox Interactive directly — they publish
a support address and the question is answerable in one email.

## How it works

```
  network ──▶ capture ──▶ proto ──▶ game ──▶ store
                                   │
                                   └──────▶ web ──▶ browser
```

| Module | Responsibility |
|---|---|
| `capture` | `AF_PACKET` raw socket, plus a classic-BPF filter assembled in `capture::bpf` so the kernel discards non-Albion traffic before it is copied to userspace. Ethernet/VLAN/IPv4/UDP demultiplexing. |
| `proto::reader` | Bounds-checked big-endian reads. Every read returns a `Result`, so a malformed datagram is counted and dropped instead of crashing the capture loop. |
| `proto::p16` | Photon's parameter table: typed values, nested slices and dictionaries. |
| `proto::photon` | Packet framing, the command loop, and fragment reassembly. |
| `game` | Albion's event ids, and the player/loot registry. |
| `store` | The append-only journal and its replay. |
| `web` | HTTP/1.1 and Server-Sent Events, hand-rolled on `std::net`. |
| `util::json` | A JSON reader and writer, standing in for `serde_json`. |

Two design notes worth knowing, because both differ deliberately from the
[original ao-loot-logger](https://github.com/matheussampaio/ao-loot-logger)
this is built from:

- **Fragment reassembly is bounded and expires.** The original allocates a
  buffer of whatever length a packet claims, and never discards a partial
  message. A corrupt or hostile length field can therefore be turned into a
  very large allocation, and abandoned fragments leak for the life of the
  process. Here the total is capped, the number of in-flight messages is
  capped, and partials expire.
- **CRC-flagged packets are parsed, not discarded.** The original cannot
  reproduce Photon's CRC and drops every packet carrying one. This computes
  the checksum, counts mismatches, and parses the payload either way —
  Photon payloads are self-describing, so a genuinely corrupt one fails to
  decode on its own. The mismatch counter is visible in the dashboard's
  `counters` so a silent failure cannot hide.

## Testing

```sh
cargo test
```

122 tests, no network access and no game required.

The important ones are the **golden vectors**. `tests/golden/vectors.json` is
generated by running the *original JavaScript ao-loot-logger decoder* over
synthetic Photon packets; the Rust test then feeds the same packets through
this port and asserts the two agree, message for message and parameter for
parameter. This is not loot-ledger checking itself against its own assumptions —
it is being compared against code known to work against live traffic.

```sh
node tools/gen-golden.js ../ao-loot-logger-main tests/golden/vectors.json
```

The vectors are committed, so `cargo test` needs no Node. Regenerate only when
the reference implementation changes.

The corpus covers mixed-type events, Unicode player and guild names, slices,
dictionaries, 64-bit and float parameters, operations and responses,
multi-command packets, unreliable commands, out-of-order fragment
reassembly, encrypted packets, truncated packets, unknown parameter types, and
a fuzz-style check that a stream of garbage cannot poison the parser.

`tests/http.rs` goes further and starts a real listener, speaking HTTP over a
loopback socket: it checks that a `GET` actually delivers its body, that
`HEAD` delivers headers without one, that a `Connection: close` client is
honoured, that one connection serves several requests, and that the event
stream opens with a parseable snapshot. That last file exists because a
response can be correct in memory and still never reach the client — a bug no
unit test catches and a browser renders as a blank page.

`loot-ledger --check` additionally runs a synthetic packet through the real
capture and decode path at startup.

## Troubleshooting

**"loot-ledger needs permission to read packets"**
Run `--check` to confirm, then `sudo setcap cap_net_raw+ep ./loot-ledger`, or
just run it with `sudo`.

**Dashboard is empty, `sawTraffic` is false**
Albion is not sending anything readable yet. Check you are logged in, check
`--list-interfaces` and try `--interface <NAME>`, and remember that a VPN or
GeForce Now makes this impossible by design.

**Dashboard is empty but `packets` is climbing**
The kernel filter is attached and frames are arriving, but nothing decodes.
Compare `eventsSeen` against a working session: if it is empty, the game
renumbered its event ids and `src/game/events.rs` needs updating. If it has
entries but no `275`, the loot event is not being recognised.

**"WARNING: kernel filter unavailable"**
The socket still works; you are just reading more traffic than necessary. The
filter is usually blocked by a security profile (AppArmor, SELinux) rather than
being wrong.

**Two instances**
The dashboard port is already taken. Pass `--port 7332`.

**Port in use by something else on 7331**
Same fix.

## Licence

GPL-3.0-or-later, matching the original ao-loot-logger this is derived from.
