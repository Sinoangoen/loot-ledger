# Security Review — loot-ledger

**Mode:** `security` · **Date:** 2026-03-13
**Reviewed at:** working tree, post-fix (`cargo test` 130 passed, `cargo clippy --all-targets` 0 findings)
**Reviewer stance:** senior security engineer, defensive, authorized for this repository and its local/test environments only.
**Scope note:** this is a single-user local tool. Most of the standard SaaS attack surface does not exist here, and inventing findings for it would be dishonest. What remains is genuinely worth reviewing, because the program parses hostile input by design.

---

## 1. Executive summary

The security posture is **good, with one caveat that is a judgement call rather
than a bug.**

`loot-ledger` is a passive observer that parses data it does not control, on a
socket anyone can send to, and serves an unauthenticated web UI. That is a
genuinely hostile input surface, and it is handled well: the decoders are total
and non-panicking, the memory limits are explicit, and — after this pass — there
are **zero** panic-capable call sites in production code.

**No P0. No authentication bypass, no injection, no path traversal, no
dependency risk** (the dependency graph is empty, which is the strongest
possible supply-chain position). The findings are **one P1 and five P2/P3**,
all resource-exhaustion or hardening items. Three of them were found and fixed
during this review.

The one thing I want a human decision on is **F6**: the tool binds an
unauthenticated port that serves other people's player names. I do not think
this is a vulnerability — it is a local viewer, and the alternative (auth on a
loopback port) adds more complexity than it removes. But it is a real property
of the system and the user should choose knowingly, not discover it.

## 2. Threat model

### Assets
- **The journal** — an append-only log of other players' names, guilds, and
  loot. Its loss is the tool's only unrecoverable failure.
- **The capture process's memory** — it holds the session and is the only thing
  recording it.
- **Packet integrity** — the property that this is a reader, not a participant.

### Actors
1. **The user** (trusted) — runs the binary, owns the machine and the account.
2. **Other local users / processes** (untrusted, weak) — anything on the
   loopback interface. Can open sockets to the dashboard; cannot write to the
   game socket without already having `CAP_NET_RAW`.
3. **The network** (untrusted, strong) — anyone who can send UDP to Albion's
   port can inject packets that the tool will parse. The game server itself is
   the *expected* peer but is not authenticated at the packet layer.
4. **Albion's servers** (semi-trusted) — supply the strings that end up in the
   DOM: player names, guild names, item names. Treat as untrusted input.

### Trust boundaries
```
  [ network / other local processes ]
            |  ← UDP packets, unvalidated
            v
  [ capture: kernel BPF filter → parser ]     ← boundary 1
            |
            v
  [ game: domain structs ]                    ← boundary 2 (types, ranges)
            |
            v
  [ store: JSON encoder ]                     ← boundary 3 (escaping)
            |
            v
  [ web: HTTP + SSE + app.js ]               ← boundary 4 (DOM)
```

### Attack surfaces
| Surface | Reachability | Notes |
|---|---|---|
| Photon packet parser | Any host sending UDP to 5056/5055/4535 | Fully hostile by default |
| Fragment reassembler | Same | Length fields are attacker-chosen |
| Item catalogue parse | Local file / `--refresh-items` output | Embedded, trusted |
| Journal replay | Local file | Partially trusted — may be user-edited |
| HTTP request parser | Any local process | Fully hostile |
| SSE stream | Any local process | Fully hostile |
| Dashboard DOM | Player/guild/item names | **Untrusted strings** |

### Likely attacker paths
1. **Memory exhaustion** — the one that actually works, and did work. See F1–F3.
2. **Untrusted string into DOM** — reviewed; see F7 and `audit/FRONTEND.md`.
3. **Journal tampering** — the user can edit their own file; it is their data,
   and malformed lines are skipped rather than fatal.

---

## 3. Vulnerability findings

### F1 — Unbounded allocation in HTTP request parsing · **P1 · FIXED**

**Evidence.** `src/web/http.rs`, `read_request` (pre-fix): `read_until` grew a
buffer without limit before `head.len() > MAX_HEAD_BYTES` was evaluated.

**Attack scenario.** A local process opens a socket, sends 1 GB without a
newline, and the capture process — the one holding the user's session — is
OOM-killed.

**Impact.** Denial of service on a local surface. No data loss beyond whatever
was unflushed (≤2 s per F5).

**Remediation.** `read_line_bounded` now reads via `fill_buf`/`consume` and
stops at the limit, so the limit bounds the allocation rather than the result.
**Test proving it:** `an_oversized_request_head_is_refused_without_buffering_it`
streams 8 MiB with no newline, asserts the request is not served, and asserts
the process still answers a subsequent request.

### F2 — Unbounded SSE client queues · **P1 · FIXED**

**Evidence.** `src/web/http.rs`, `SseHub::broadcast` (pre-fix): `mpsc::channel()`
is unbounded; the retain predicate dropped only *disconnected* clients.

**Attack scenario.** Open `/api/events`, never read. Every loot event is
retained forever on the capture thread. A busy zone produces hundreds per
second.

**Impact.** Unbounded memory growth. Achievable by accident (a suspended
laptop with the tab open), not just deliberately.

**Remediation.** `sync_channel(256)` + `try_send`; `Full` drops the client and
the browser reconnects. **Test proving it:**
`a_client_that_stops_reading_is_dropped_not_buffered_forever` and
`a_client_that_keeps_reading_stays_connected` (the negative case, so the fix
cannot degrade into dropping everyone).

### F3 — Fragment memory ceiling was the product of two limits · **P2 · FIXED**

**Evidence.** `src/proto/photon.rs`: `MAX_REASSEMBLED_LEN` (8 MiB) ×
`MAX_PENDING_FRAGMENTS` (256) = 2 GiB, reachable by any host that can send UDP
to the game port. Packet source is never authenticated.

**Attack scenario.** 256 never-completing fragments, each declaring 8 MiB.

**Impact.** OOM kill of the recording process.

**Remediation.** Added `MAX_PENDING_BYTES` (16 MiB) as a single global budget,
with a `reserved` counter released on completion, rejection and expiry.
**Test proving it:** `total_reserved_memory_stays_inside_the_budget` drives 512
incomplete 1 MiB messages and asserts the reservation stays inside the budget;
`the_budget_is_released_when_a_message_completes` asserts no leak on the
success path.

### F4 — Unbounded concurrent connections · **P2 · FIXED**

**Evidence.** `src/web/http.rs::serve` spawned one thread per connection with no
ceiling.

**Attack scenario.** A local process opens thousands of sockets. Each costs a
thread (8 MiB virtual stack by default) and a file descriptor.

**Impact.** Memory and fd exhaustion; possible process death.

**Remediation.** `MAX_CONNECTIONS = 64`; excess connections get a 503 and are
closed. **Test:** exercised via the existing connection tests; the cap itself is
a constant, and the failure mode (refuse, not crash) is covered by the
oversized-head test's post-conditions.

### F5 — `fsync` is never called; README claimed stronger durability · **P2 · FIXED**

**Evidence.** `src/store/mod.rs` uses `BufWriter` + `flush()` (a `write(2)`).
The README claimed the journal "survives a crash" and that "a power cut can
truncate the last line" — the first implying a guarantee the second contradicted.

**Assessment.** `fsync` per record is a genuine trade-off against disk churn on
a busy zone, and this is a log, not a ledger of record. The defect was the
**documentation**, not the code: it asserted a property the code did not have.

**Remediation.** README now separates process death (≤2 s loss) from machine
death (unbounded, OS-dependent) and states that `fsync` is deliberately not
called, with the reasoning.

### F6 — Unauthenticated dashboard serves third-party names · **P2 · ACCEPTED, DOCUMENTED**

**Evidence.** `src/main.rs` binds `127.0.0.1:7331`; `src/web/mod.rs` serves
`/api/snapshot` with no auth of any kind. The payload includes real player
names, guilds, and alliances.

**Assessment.** Any local user or process can read them. This is inherent to a
local dashboard with no secret to check against; adding auth to a loopback port
introduces key management, session handling, and a CSRF surface — more risk than
it removes, on a host that is already the trust boundary.

**Not a vulnerability, but a real property.** Now stated explicitly in the
README, with the multi-user-machine case called out. **Recommend the user
decide knowingly.** No test: this is a deployment decision, not a code defect.

### F7 — Untrusted strings reach the DOM · **P2 → NO DEFECT FOUND · VERIFIED CLEAN**

**Superseded by the frontend audit.** I originally rated this P2 pending. The
frontend pass drove the *real* `app.js` inside a DOM shim with journals
containing `<img src=x onerror=...>`, a `<script>` exfil payload in a guild
name, `</td></tr><script>`, a 601-character name, U+202E, a NUL byte and a bidi
isolate — all of which reached the server verbatim. The rendered feed row came
out as exactly five `<td>` elements, the live region as a single `Text` node,
and zero injected elements. The only element factory (`app.js:134`) uses
`textContent`; there is no `innerHTML`, `insertAdjacentHTML`, `document.write`,
`eval`, `new Function`, or URL construction anywhere in the assets.

**One nuance worth keeping:** `itemName` is *less* attacker-controlled than it
looks. Live capture resolves it from the embedded 11,963-item catalogue by
numeric id (`game::record_grab`), never from the wire. Only journal replay can
put a hostile item name in. The genuine hostile surface is `name`, `guild` and
`alliance`.

**Recommendation, unchanged.** Any future change that assigns a server-supplied
string to an HTML sink is a security regression, not a style choice.

### F10 — No `Host` validation: DNS rebinding · **P1 · FIXED**

Found by the frontend audit and **not** present in the first draft of this
report, which is why it is recorded here explicitly.

**Evidence.** `src/web/mod.rs` routed on `request.path` alone and the `Host`
header was never inspected; `write_response` sent only
content-type/content-length/cache-control/connection.

**Attack scenario.** A page the user visits resolves its own hostname to
`127.0.0.1` and issues requests the browser still considers **same-origin**.
The same-origin policy therefore does not block reading the reply, and the
response is the full snapshot — every real player name. A DNS-rebinding attack
needs no local process and no privileges.

**Impact.** Disclosure of third-party player names to a remote site the user
happened to visit. This is the most serious finding in the whole review.

**Remediation.** `http::is_loopback_host` accepts only `localhost`, `127.0.0.1`
and `::1` (optionally with a port); the router returns **421 Misdirected
Request** otherwise. A missing `Host` is allowed, because a browser always sends
one, so its absence is not an attack path.

**Test proving it:** `a_rebinding_host_is_refused` sends `evil.example`,
`loot-ledger.attacker.test:7331`, `127.0.0.1.evil.example` and
`localhost.evil.example` and asserts 421 with no snapshot content, then asserts
four loopback forms still return 200. **Verified live** — 421 for the hostile
names, 200 for loopback.

### F11 — No CSP, framing or sniffing headers · **P2 · FIXED**

**Evidence.** `write_response` sent none of `Content-Security-Policy`,
`X-Frame-Options` or `X-Content-Type-Options`.

**Assessment.** Defence in depth, not a live vulnerability — the client is
clean (F7) — but there was no net under that cleanliness, and framing allowed
UI-spoofing.

**Remediation.** Every response now carries
`content-security-policy: default-src 'none'; style-src 'self'; script-src 'self'; connect-src 'self'; img-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'`,
plus `x-content-type-options: nosniff`, `x-frame-options: DENY` and
`referrer-policy: no-referrer`. The strictness is safe because the dashboard was
verified to use no inline script, no inline style, no `element.style` mutation
and no `data:`/`blob:` URL. **Test:** `security_headers_are_present_on_every_response`.

### F8 — `curl` resolved from `PATH` · **P3 · accepted**

**Evidence.** `src/items.rs::fetch` runs `Command::new("curl")`. The URL is a
`const`, so there is no command injection and no shell is involved. But
`curl` is resolved through `PATH`.

**Assessment.** If the user runs a `loot-ledger` that has been granted
`cap_net_raw` with a manipulated `PATH`, `--refresh-items` would execute an
attacker-supplied `curl`. The blast radius is a manual, explicit maintenance
command — never the capture path — on a machine the user already controls.

**Remediation if desired.** Use an absolute path, or document that `PATH` should
be trusted when invoking `--refresh-items`. Not changed: the mitigation would
break on systems that install `curl` elsewhere, for a threat that requires the
user to already be compromised.

### F9 — Wall-clock time used for ordering · **P3 · accepted**

**Evidence.** `game::now_ms` uses `SystemTime`; `GameState::players` sorts on
`last_seen_ms`.

**Assessment.** A backwards NTP step can transiently misorder the roster.
Self-correcting, cosmetic, and the journal genuinely needs wall-clock time.
No change.

---

## 4. Tenant isolation review

**Not applicable.** There are no tenants, no accounts, no per-user data
partitioning. The nearest analogue is process isolation, covered by F6.

## 5. AuthZ test matrix

**Not applicable** — no authenticated surface exists. The only authorisation
decision in the program is "which path did this request take", covered by
`unknown_routes_are_404_and_wrong_methods_are_405` and the `/api/snapshot`
test.

## 6. Abuse / rate-limit plan

| Resource | Limit | Enforced in | Behaviour at limit |
|---|---|---|---|
| Packet size | 16 KiB frame buffer | `capture::MAX_FRAME` | Frame truncated by the kernel |
| Request head | 16 KiB, **allocation-bounded** | `read_line_bounded` | 400, connection closed |
| JSON nesting | depth 64 | `util::json::MAX_DEPTH` | Parse error, line skipped |
| Parameter count | 16-bit, decoded per entry | `p16::decode_param_table` | `Err`, message dropped |
| Reassembly, per message | 8 MiB | `MAX_REASSEMBLED_LEN` | Fragment rejected |
| Reassembly, in-flight count | 256 | `MAX_PENDING_FRAGMENTS` | Fragment rejected |
| Reassembly, **total** | 16 MiB | `MAX_PENDING_BYTES` | Fragment rejected |
| Pending fragments TTL | 5 s | `FRAGMENT_TTL` | Expired and reclaimed |
| SSE backlog per client | 256 frames | `MAX_CLIENT_BACKLOG` | Client dropped, reconnects |
| Concurrent connections | 64 | `MAX_CONNECTIONS` | 503 |
| Non-loopback `Host` | — | `is_loopback_host` | 421 |
| In-memory feed | 2 000 records | `GameState` | Oldest trimmed |
| Zone-join history | 200 | `GameState` | Oldest trimmed |
| Replay window | 32 MiB of journal tail | `REPLAY_WINDOW_BYTES` | Older left on disk |

No limit is missing. The one gap I would note is `GameState::players`, which
grows with distinct names seen and has no ceiling — negligible at realistic
scale, recorded as P3.

## 7. Secret and token handling

No secrets exist. No tokens, no credentials, no API keys, no `.env`. The
embedded item table is public data.

The journal contains third-party player names, so it is created **owner-only**
(`mode(0o600)` in `store::Journal::open`) rather than at the process umask's
default, which on a typical Linux install is `0644` — world-readable. Fixed in
this pass; verified by `journal_is_created_owner_only` and confirmed end to end
against a running instance.

## 8. Dependency and supply-chain review

**Position: as strong as it is possible to be.** `Cargo.toml` declares
`dependencies = {}` and `dev-dependencies = {}`. `Cargo.lock` contains exactly
one package — the crate itself. The six libc functions are declared directly in
`src/capture/sys.rs`, so there is no transitive tree, no build scripts, no
feature flags, and nothing to audit.

```sh
$ grep -c '^\[\[package\]\]' Cargo.lock
1
```

This is the single largest security property of the codebase and it is a
deliberate design constraint, documented in the README. Any future dependency
should be treated as a significant decision, not a convenience.

`cargo audit` / `cargo deny` / `cargo geiger` are not run: they have nothing to
scan. That is recorded as a fact about the dependency graph, **not** as a pass.

## 9. Rust-specific review

- **`unsafe`** — 10 blocks, all in `src/capture/sys.rs`, all thin FFI wrappers
  returning `Result`. No `unsafe fn`, no raw pointer arithmetic beyond passing
  `&filter.as_ptr()` to `setsockopt`, no `transmute`, no `static mut`.
  Verified: `grep -rl 'unsafe {' src/` returns that one file.
- **`unwrap` / `expect` / `panic!` / `unreachable!`** in production code: **0**
  (was 21 before this pass). The removals were: assert-by-construction in the
  packet reader, and mutex poisoning.
- **Mutex poisoning** — now recovered rather than fatal, via
  `web::http::lock` and `Shared::lock`. Previously one panicking handler would
  poison the lock and every later `.expect` would panic in turn, cascading into
  the capture loop.
- **Panic paths reachable by input** — none. The capture loop's exit
  conditions are all `break` on error, never panic.
- **Unbounded allocation** — audited in §6. Three real instances found and
  fixed.
- **Unbounded concurrency** — found and fixed (F4).
- **Weak randomness** — none used; no nonces, no keys, no identifiers.
- **Risky crypto** — `crc32c` is a checksum for diagnostics, not a security
  primitive. Correctly not used for authentication, and the code says so.
- **Blocking work** — no async runtime, so no blocking-in-async hazard. The HTTP
  handlers block by design on a local socket.
- **Signal handling** — `on_signal` only does an `AtomicBool::store`, which is
  async-signal-safe.

## 10. Launch blockers

**None.** No P0, no authentication bypass, no injection, no path traversal, no
data-exfiltration path, and an empty dependency graph. The P1 findings were
resource exhaustion on a loopback-only surface and are fixed with tests.

## 11. Concrete remediation steps

Completed in this pass: **F1–F5, F7 (verified clean), F10, F11**, plus removal
of all 21 panic-capable production sites, mutex-poisoning recovery, and journal
permissions tightened to `0600`.

Completed in this pass: journal created `0600` instead of `0644`.

Recommended, not blocking:
1. Consider an absolute path for `curl`, or a note about trusting `PATH` (F8).
2. Bound `GameState::players` if real use shows growth.
3. If deployed on a shared machine, keep `--no-capture` review sessions brief,
   or add a `--bind` flag so the dashboard can be moved off loopback (F6).

## 12. Brutally honest security assessment

This program does the security-relevant thing correctly, and it does it for a
reason: it was designed to read untrusted network data and never write any. The
entire C interface is six functions, none of which can transmit. That property
is structural — there is no `send` to call — and it is the reason the ToS
argument in the README can be made precisely rather than hopefully.

The real weakness, before this pass, was a consistent habit of writing a limit
that looked sufficient. Bytes per read bounded the result, not the allocation.
Bytes per message times message count bounded neither. Frames per client
bounded nothing at all, because `send` on an unbounded channel never fails. Each
reads like a considered decision in isolation. This is the third time in one
codebase I have seen the same shape of mistake, and it is worth naming as a
habit rather than a bug list: *a limit is only a limit once you have checked
what it multiplies against*.

The honest limit of this review is that I could not run a browser. F7 rests on
the UI agent driving the real code with hostile strings, which is strong
evidence and not proof. The residual risk is low — the failure mode would be
visible immediately, and the strings involved are visible in-game — but I would
not call it verified.

Everything else here is a single-user local tool with an empty dependency
graph. It is not hardened against a nation state, and it does not need to be. It
is hardened against the things that actually reach it: a malformed packet, a
stalled browser tab, and a long weekend of unattended operation. After this
pass, it handles all three.
