# Architecture Review — loot-ledger

**Mode:** `architect` · **Date:** 2026-03-13
**Reviewed at:** working tree, post-fix (`cargo test` 133 passed, `cargo clippy --all-targets` 0 findings, `cargo fmt --check` clean)
**Companion reports:** `audit/SECURITY.md` (same tree), `audit/FRONTEND.md` (the three dashboard assets). Findings found by those passes and fixed here are listed in §4 as **F1–F8**.
**Reviewer stance:** senior systems architect. Repository evidence only.
**Not a git repository**, so no commit hash is cited; the state is identified by the command output above.

---

## 1. Executive summary

`loot-ledger` is a well-proportioned piece of software for what it does. It is a
single-user, single-process, local tool: read packets, decode a protocol, log
records, serve a dashboard. There is no database, no multi-tenancy, no auth, no
distributed state, and no build-system fragility. The module boundaries match
the actual data flow, the dependency count is zero, and the test suite actually
tests the parts that matter.

The review found **no P0 and no launch blocker**. It did find **one P1 class of
resource-exhaustion bug** (three instances, all now fixed), plus a durability
claim in the README that the code did not support.

The most important structural observation: the two largest risks in this
codebase are both *"a limit that looks like it bounds something, but actually
bounds something else"*. Three of them, in fact. That is a specific, fixable
class of mistake, and all three are now fixed with tests that fail without the
fix.

### The domain lens

This is **not a SaaS**, and the standard SaaS review axes do not apply. There
are no tenants, no billing, no PII lifecycle, no deployment pipeline, no
multi-region concerns. I say so explicitly because applying a generic SaaS
checklist here would produce a report full of non-findings.

The actual domain is a **passive network observer that records other people's
game activity**. Its critical invariants, derived from the code and from what
the tool is for:

| # | Invariant | Why it matters |
|---|---|---|
| I1 | It never writes to a socket carrying game traffic | If it could inject a packet, it becomes a cheat tool and a ToS violation |
| I2 | It never loses or corrupts the journal | The journal is the only durable product; the dashboard is a view |
| I3 | It survives arbitrary traffic indefinitely | Every packet is untrusted and hostile by default |
| I4 | It does not leak the names it records | The data belongs to other people |
| I5 | Startup and steady-state cost do not grow without bound | It runs unattended for hours alongside a game |

This review is organised around those five.

---

## 2. Architecture map

```
  NIC ──▶ capture/sys.rs      AF_PACKET socket, setsockopt(SO_ATTACH_FILTER)
         capture/bpf.rs       classic BPF, assembled in-process
         capture/mod.rs      Ethernet → VLAN → IPv4 → UDP demux
                │
                ▼
         proto/photon.rs      framing, command loop, fragment reassembly
         proto/p16.rs         parameter table
         proto/reader.rs      bounds-checked big-endian reads
                │
                ▼
         game/mod.rs          event ids, player registry, loot feed
         game/events.rs       parameter-id → domain structs
                │
                ├──────────────▶ store/mod.rs   JSONL journal + tail replay
                │                            util/json.rs
                ▼
         web/http.rs          HTTP/1.1, SSE hub, thread-per-connection
         web/mod.rs           routes, JSON shapes
         web/ui.rs            include_str! of the three dashboard assets
```

**Concurrency model.** One capture thread; one accept thread; one thread per
HTTP connection; a `Mutex<App>` for all shared state. No async runtime, no
`Send`/`Sync` gymnastics, no executor. For a single-user local tool this is the
right amount of machinery — an async rewrite would add a runtime dependency and
a class of cancellation bugs to solve a problem that does not exist.

**Lock discipline.** The capture loop takes the state out from under its lock,
decodes, and puts it back; the journal write and the SSE broadcast both happen
with no lock held. Broadcasting inside the lock would let one stalled browser
tab stall packet capture. The SSE hub is a *separate* lock from the state, and
is only ever acquired on its own — no lock ordering to get wrong.

---

## 3. Core design verdict

**Conditionally suitable** — and the condition is now met.

It was *not* suitable at the start of this review for one reason: three
resource limits existed that looked sufficient and were not, and one of them
(O2 below) sat directly on the path an attacker or a malformed stream would
take. Those are fixed. What remains is honest P2/P3 work that does not block
use.

---

## 4. Top architectural risks

### O1 — Unbounded read in HTTP request parsing · **P1 · FIXED**

**Evidence.** `src/web/http.rs`, `read_request`.

The 16 KiB request-head cap was enforced *after* the read:

```rust
let n = reader.read_until(b'\n', &mut line)?;   // grows without limit
head.extend_from_slice(&line);
if head.len() > MAX_HEAD_BYTES { return Err(...) }   // checked far too late
```

`read_until` reads until it finds a newline **or EOF**. A peer that sends a
gigabyte with no newline grows `line` to a gigabyte before the cap is ever
evaluated. The limit bounded the *result*, not the *allocation* — which for
this process is the one holding the user's entire loot session in memory.

**What could go wrong.** Any local process exhausts the machine's memory and
kills a session the user was relying on to record.

**Fix.** `read_line_bounded` (implemented) reads through `fill_buf`/`consume` and
stops at the limit. **Verified** by
`tests/http.rs::an_oversized_request_head_is_refused_without_buffering_it`,
which streams 8 MiB with no newline and asserts the server refuses it *and*
survives.

### O2 — Two fragment limits that multiply to 2 GiB · **P2 · FIXED**

**Evidence.** `src/proto/photon.rs`.

`MAX_REASSEMBLED_LEN` (8 MiB) × `MAX_PENDING_FRAGMENTS` (256) = **2 GiB**, all
reachable by any host that can send UDP to Albion's port. There is no
authentication of packet source — a fragment header is a length field followed
by a sequence number, and nothing checks that it came from a server.

**What could go wrong.** 256 never-completing fragments claiming 8 MiB each
allocate 2 GiB. On a laptop, that is an OOM kill of the recording process.

**Fix.** Added `MAX_PENDING_BYTES` (16 MiB) as a **single global budget**,
tracked by a `reserved` counter that is released on completion, rejection, and
expiry. Per-message and per-count caps remain as cheap pre-filters, but the
worst case is now a constant the process can live inside. **Verified** by
`total_reserved_memory_stays_inside_the_budget` and
`the_budget_is_released_when_a_message_completes`.

### O3 — Unbounded Server-Sent Events queues · **P1 · FIXED**

**Evidence.** `src/web/http.rs`, `SseHub::broadcast`.

`mpsc::channel()` is unbounded, and the retain predicate only dropped
*disconnected* clients:

```rust
clients.retain(|(_, tx)| tx.send(frame.clone()).is_ok());   // send() never fails while alive
```

A client that opens `/api/events` and then stops reading — a stalled TCP window,
a laptop lid, a `curl` nobody drains — accumulates one frame per loot event
forever, on the capture thread. In a busy zone at several hundred events per
second that is tens of megabytes a second, per stalled tab.

**What could go wrong.** A dashboard left open on a suspended machine
exhausts memory. Note the ordering matters: this runs *after* the state lock is
released, so the lock discipline is fine — the problem is purely accumulation.

**Fix.** `sync_channel(MAX_CLIENT_BACKLOG)` (256 frames) and `try_send`;
`TrySendError::Full` drops the client, which the browser then reconnects on its
own. **Verified** by `a_client_that_stops_reading_is_dropped_not_buffered_forever`
and `a_client_that_keeps_reading_stays_connected`.

### O4 — Unbounded connections and unbounded replay · **P2 · FIXED**

**Evidence.** `src/web/http.rs::serve` spawned a thread per connection with no
ceiling; `src/store/mod.rs::replay` read the entire journal from byte 0 on
every start.

Both grow with something an attacker or a long-running install controls. Replay
in particular made startup time and memory a function of total history — after
a year of play, seconds of startup and hundreds of megabytes of transient
allocation.

**Fix.** `MAX_CONNECTIONS = 64` with a 503 beyond that. Replay reads only the
last `REPLAY_WINDOW_BYTES` (32 MiB), discards the partial first line, and
reports `truncated` so the user is told older records were left on disk rather
than silently losing them. **Verified** by
`replay_reads_only_the_tail_of_a_very_long_journal`, which writes a >32 MiB
journal and asserts the file is untouched on disk afterwards.

**Note on the trade-off, stated plainly:** windowed replay means a player's
`firstSeen` reflects the window, not their true first appearance. That is
documented in both the doc comment and the README rather than left to be
discovered.

### F1 — DNS rebinding: the `Host` header was never checked · **P1 · FIXED**

Found by the frontend audit, not by this one. `web::router` dispatched on
`request.path` alone; `write_response` sent no CSP and no framing headers. A
page the user visits can resolve its own hostname to `127.0.0.1` and issue
requests the browser still treats as *same-origin*, so the same-origin policy
does not stop it reading the reply — which is the whole player roster. Fixed
with a loopback `Host` allowlist returning 421, plus CSP / `nosniff` / `XFO`.
Full detail in `audit/SECURITY.md` §F10–F11.

**Why it belongs in an architecture review too:** this is a boundary defect
between two layers that each behaved correctly in isolation. The HTTP server did
what it was asked; the router routed what it was given. Nothing owned the seam.

### O5 — README durability claim was stronger than the code · **P2 · FIXED**

**Evidence.** The README asserted the journal "survives a crash" and that "a
power cut can truncate the last line."

`Journal::flush` is a `write(2)`, not an `fsync`. Process death loses at most
the ~2 s buffer — that part was true. **Machine** death can lose whatever the
OS had not yet flushed, which is not bounded to one line. The first claim
implied a guarantee the second contradicted, and neither matched the code.

**Fix.** The README now separates the two cases explicitly and states that
`fsync` is deliberately not called, with the reason.

---

## 5. Correctness review

### Invariant I1 — never writes to game traffic · **holds, and is verifiable**

The entire FFI surface is six functions in `src/capture/sys.rs`: `socket`,
`bind`, `setsockopt`, `recvfrom`, `close`, `signal`. There is no `send`,
`sendto`, or `sendmsg` to call. A repo-wide grep confirms no send-capable call
exists. The README publishes both greps so the user can check the claim rather
than take it on trust. This is the single most important property of the
codebase and it is structurally enforced, not merely intended.

### Invariant I2 — journal integrity · **holds**

`store::decode_entry` is total: a malformed or truncated line yields `None`
and is counted in `skipped`, never fatal. The `a_truncated_final_line_is_skipped_not_fatal`
test writes a deliberately torn final line and asserts the rest still loads.
Writes are `BufWriter`, so a partial line is the only possible damage.

The one residual gap is `fsync`, covered in O5.

### Invariant I3 — survives arbitrary traffic · **holds, after O1–O3**

`Parser::handle_packet` is total and non-panicking: every read is a `Result`,
every command length is range-checked against `r.remaining()`, and a decode
failure increments a counter and moves on. `short_and_garbage_packets_are_survivable`
and the golden-vector fuzz check both assert this. After this review there are
**zero** panic-capable call sites in production code (down from 21), and all
ten `unsafe` blocks are confined to the single FFI file.

### Invariant I4 — no leakage · **holds, with a documented caveat**

No outbound connection exists in normal operation; the dashboard binds
`127.0.0.1`. The one network call in the codebase is `--refresh-items`, which
shells out to `curl` against a `const` URL — see U2.

**Caveat, now in the README:** there is no authentication, so on a multi-user
machine any local process can read player names from the port. Stated rather
than papered over.

### Invariant I5 — bounded cost · **holds, after O3/O4**

Steady-state memory is bounded by the feed cap (2 000), zone-join cap (200),
fragment budget (16 MiB), SSE backlog (256 frames/client), and connection cap
(64). Startup is bounded by the replay window. One unbounded map remains —
`GameState::players`, keyed by player name — which grows with distinct names
seen. At a realistic few thousand names this is negligible; I would bound it
only if I had evidence it mattered, and I do not. Recorded as P3 below.

### Time and date modelling · **P3, accepted**

Records are stamped with `SystemTime` (wall clock) and uptime with `Instant`
(monotonic) — the correct split, since the journal needs absolute times and
runtimes must not jump when NTP steps the clock. The consequence is that the
roster is *sorted* by wall clock, so a backwards clock step can briefly
misorder it. For a display sorted by recency this is cosmetic and
self-correcting. Not worth complicating.

---

## 6. Simplification opportunities

- **`std::mem::take` in the capture loop.** The state is moved out from under
  the lock and moved back so the change records can be persisted and broadcast
  with no lock held. It works, but the reason is borrow-checker ergonomics
  rather than a design requirement, and it is the least obvious thing in
  `main.rs`. A comment now explains it; splitting the journal out of `App` into
  its own lock would remove the need entirely and is the single cleanest
  structural improvement available.
- **One `Request` type with an owned `headers` map.** Fine at this size; worth
  revisiting only if the route surface grows.
- **`Params` as a linear-scan `Vec`.** Correct choice, already commented. A
  `HashMap` would be slower for the tens-of-entries-per-packet case.

## 7. Target architecture

Unchanged in shape. The codebase is already at the right altitude; the work is
depth (bounds, durability, tests), not restructuring. The one structural change
I would actually make is separating the journal from the in-memory state so
the capture loop stops needing `mem::take`.

## 8. Launch blockers

**None now. There was one before this pass.** The DNS-rebinding hole (F1) was a
real disclosure path to a remote site, and it is fixed and tested. The resource
exhaustion issues (O1–O4) were real and are fixed. What remains is P2/P3 work.

## 9. Implementation roadmap

**Immediate (done in this pass)**
- O1 bounded request-head reading — fixed + tested
- O2 global fragment memory budget — fixed + tested
- O3 bounded SSE client queues — fixed + tested
- O4 connection cap, windowed replay — fixed + tested
- O5 README durability correction — done
- Remove all 21 panic-capable production sites; recover from mutex poisoning
  so one panicking handler cannot cascade into the capture loop

**Pre-launch hardening (not required)**
- Bound `GameState::players` if real deployments show growth
- Consider `sync_data` on the journal at session end, so a deliberate shutdown
  is durable immediately rather than at the next 2 s tick

**Found by the frontend pass and fixed here**
- F1 loopback `Host` allowlist (421) — the one genuine security boundary defect
- F2 strict CSP + `nosniff` + `X-Frame-Options` on every response
- F3 journal created `0600` instead of the umask default `0644`

**Post-launch**
- Rotation, if the journal size becomes a user complaint
- A `--follow` tail mode for the journal, for users who prefer a terminal

## 10. Brutally honest assessment

This is good work, and the reason I can say that is that it was verified
against something real rather than against itself. The golden-vector corpus —
generated by running the original JavaScript decoder and requiring agreement —
is the single best thing in the repository, and it earned its keep during this
review: a refactor I made while removing `expect` calls silently double-advanced
the packet reader, and the cross-validation caught it immediately. Without
that harness I would have shipped a regression that no unit test in the file
would have noticed.

The weakness is a consistent bias toward *local* limits that do not compose.
Three separate times, a cap existed that bounded the right thing in isolation
and the wrong thing in combination — bytes per read, bytes per message ×
message count, frames per client × clients. Each was individually reasonable.
The composite was not. That is a class of mistake, and the durable fix is the
habit of asking "what is the worst case *across* all the limits at once",
which the new global fragment budget now makes explicit in code.

The second weakness is structural and more interesting: **this review missed the
worst bug in the codebase.** The DNS-rebinding hole sat exactly on the seam
between the HTTP server and the router, and neither layer was wrong on its own
terms. I audited the Rust, found four real limits, and called it. A separate
pass that looked at the same code from the browser's side found the one that
actually mattered. Auditing only your own layer does not work; the seam is where
the dangerous bugs are, and it has no owner.

The third weakness is that the honest bounds are mostly untested against real
game traffic, because no test in this repository has ever seen a real Albion
packet. The golden vectors prove the decoder agrees with a working
implementation on synthetic input; they do not prove the event ids are still
current. The mitigation already in the product — surfacing `eventsSeen` so an
empty map is distinguishable from "no traffic" — is the right instinct, and
it deserves to be the first thing a user checks when the dashboard is blank.

Not production-hardened in the SaaS sense, and it does not need to be. It is a
well-built tool for one person on one machine, and the effort is better spent
on the event ids staying right than on anything in this report.
