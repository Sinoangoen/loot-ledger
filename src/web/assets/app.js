/* ===========================================================================
   loot-ledger dashboard
   ---------------------------------------------------------------------------
   Plain ES2020, no build step, no dependencies. Three rules keep this file
   honest:

   1. `EventSource` owns the stream. It reconnects by itself; this file only
      listens for `open` and `error` and reflects them. There is deliberately
      no reconnect loop here.

   2. Rows are updated, never re-rendered. A new grab prepends one <tr>; the
      list is capped and trimmed from the tail. Nothing re-renders a table, so
      there is no flicker and no scroll jump.

   3. Nothing from the network is ever parsed as markup. Every player, guild
      and item string reaches the document through `textContent`.

   Field names below are exactly the ones the server sends; see
   src/web/mod.rs for the serialisation that produces them.
   =========================================================================== */

'use strict';

(function () {
  /* ------------------------------ constants ------------------------------- */

  var MAX_FEED_ROWS = 300;
  var MAX_ROSTER_ROWS = 500;
  var ANNOUNCE_MS = 2000;
  var TICK_MS = 1000;
  var RECENT_MS = 60000;
  var HELLO_FALLBACK_MS = 6000;

  var STREAM_URL = '/api/events';
  var SNAPSHOT_URL = '/api/snapshot';

  var ARROW = '\u2190'; // leftwards arrow
  var QUOTE_OPEN = '\u201c';
  var QUOTE_CLOSE = '\u201d';
  var TIMES = '\u00d7';
  var MIDDOT = '\u00b7';
  var ELLIPSIS = '\u2026';
  var EM_DASH = '\u2014'; // the "no value" placeholder

  /** `totals` keys do not share names with the local tally, so map them. */
  var TOTALS_TO_LIVE = {
    events: 'events',
    itemGrabs: 'items',
    silverGrabs: 'silver',
    units: 'units',
    unknownItems: 'unknown'
  };

  /* -------------------------------- state --------------------------------- */

  var dom = {
    conn: document.getElementById('conn'),
    connDot: document.getElementById('conn-dot'),
    connText: document.getElementById('conn-text'),

    q: document.getElementById('q'),

    tEvents: document.getElementById('t-events'),
    tItems: document.getElementById('t-items'),
    tUnits: document.getElementById('t-units'),
    tSilver: document.getElementById('t-silver'),
    tPlayers: document.getElementById('t-players'),
    tUnknown: document.getElementById('t-unknown'),

    feedScroll: document.getElementById('feed-scroll'),
    feedTable: document.getElementById('feed-table'),
    feedBody: document.getElementById('feed-body'),
    feedMeta: document.getElementById('feed-meta'),
    feedEmpty: document.getElementById('feed-empty'),
    emptyTitle: document.getElementById('empty-title'),
    emptyBody: document.getElementById('empty-body'),

    rosterScroll: document.getElementById('roster-scroll'),
    rosterTable: document.getElementById('roster-table'),
    rosterBody: document.getElementById('roster-body'),
    rosterMeta: document.getElementById('roster-meta'),
    rosterEmpty: document.getElementById('roster-empty'),
    rosterEmptyTitle: null,
    rosterEmptyBody: null,

    fUptime: document.getElementById('f-uptime'),
    fIface: document.getElementById('f-iface'),
    fZone: document.getElementById('f-zone'),

    dSelf: document.getElementById('d-self'),
    dStarted: document.getElementById('d-started'),
    dFilter: document.getElementById('d-filter'),
    dPackets: document.getElementById('d-packets'),
    dMessages: document.getElementById('d-messages'),
    dDecode: document.getElementById('d-decode'),
    dUnknown: document.getElementById('d-unknown'),
    dSkipped: document.getElementById('d-skipped'),
    dCrc: document.getElementById('d-crc'),
    dReplay: document.getElementById('d-replay'),
    dCatalogue: document.getElementById('d-catalogue'),

    announce: document.getElementById('announce')
  };

  var state = {
    snapshot: null,
    status: null,
    counters: null,
    zones: [],
    selfName: null,
    filter: '',
    filterText: '',
    feedVisible: 0,
    rosterVisible: 0
  };

  /** name -> player record. The single source of truth for the roster. */
  var players = new Map();

  /** Loot events received since the last snapshot, added on top of
   *  `snapshot.totals` so the summary strip stays live. */
  var live = { events: 0, items: 0, silver: 0, units: 0, unknown: 0 };

  /** <tr> -> { rec, by, from, tr } for feed rows. */
  var rowData = new WeakMap();

  /** Every <time> element that still needs a once-a-second refresh. */
  var timeCells = [];

  var numberFormat = new Intl.NumberFormat();

  /* ------------------------------- helpers -------------------------------- */

  function el(tag, className, text) {
    var node = document.createElement(tag);
    if (className) node.className = className;
    if (text !== undefined && text !== null) node.textContent = String(text);
    return node;
  }

  /** Coerce anything the server sent to a finite integer. */
  function int(value) {
    var n = typeof value === 'number' ? value : parseInt(value, 10);
    return isFinite(n) ? Math.trunc(n) : 0;
  }

  function str(value) {
    return typeof value === 'string' ? value : value == null ? '' : String(value);
  }

  function num(value) {
    return numberFormat.format(int(value));
  }

  function optStr(value) {
    return value == null || value === '' ? null : str(value);
  }

  function pad2(n) {
    return n < 10 ? '0' + n : String(n);
  }

  function quote(text) {
    return QUOTE_OPEN + str(text) + QUOTE_CLOSE;
  }

  /**
   * Relative for the last minute, wall clock after that. `at` is a millisecond
   * epoch value and the age is always measured against the browser's clock, so
   * a small difference between the two clocks cannot make a row read as
   * having happened in the future.
   */
  function stamp(at) {
    if (typeof at !== 'number' || !isFinite(at) || at <= 0) return EM_DASH;
    var age = Date.now() - at;
    if (age < 0) return 'now';
    if (age < RECENT_MS) return Math.floor(age / 1000) + 's ago';
    var d = new Date(at);
    return pad2(d.getHours()) + ':' + pad2(d.getMinutes()) + ':' + pad2(d.getSeconds());
  }

  function makeTimeCell(at) {
    var node = el('time');
    if (typeof at === 'number' && isFinite(at) && at > 0) {
      node.dateTime = new Date(at).toISOString();
      node.title = new Date(at).toLocaleString();
    }
    node.textContent = stamp(at);
    timeCells.push({ el: node, at: at });
    return node;
  }

  function formatDuration(seconds) {
    var s = int(seconds);
    if (s < 0) return EM_DASH;
    var h = Math.floor(s / 3600);
    var m = Math.floor((s % 3600) / 60);
    if (h > 0) return h + 'h ' + pad2(m) + 'm';
    if (m > 0) return m + 'm ' + pad2(s % 60) + 's';
    return s + 's';
  }

  /* ----------------------------- item labels ------------------------------ */

  /**
   * `itemName` and `itemUnique` are null for silver, and null for an item id
   * the catalogue does not know, usually because a game patch added the item
   * this binary was built. Fall back to the id rather than printing "null".
   */
  function itemLabel(rec) {
    if (rec.silver === true) return 'Silver';
    if (rec.itemName != null && rec.itemName !== '') return str(rec.itemName);
    if (rec.itemNumId != null) return 'Item #' + num(rec.itemNumId);
    return 'Unknown item';
  }

  function unknownItem(rec) {
    return rec.silver !== true && rec.itemName == null && rec.itemNumId != null;
  }

  /** A loot payload is usable when it has a looter. */
  function isLootEvent(rec) {
    return !!rec && typeof rec === 'object' && typeof rec.by === 'string' && rec.by !== '';
  }

  function normalizeLoot(rec) {
    rec.from = str(rec.from);
    if (typeof rec.at !== 'number') rec.at = int(rec.at);
    if (typeof rec.silver !== 'boolean') rec.silver = rec.silver === true;
    return rec;
  }

  /* ------------------------------- players -------------------------------- */

  function makePlayer(payload) {
    return {
      name: str(payload.name),
      guild: optStr(payload.guild),
      alliance: optStr(payload.alliance),
      isSelf: payload.isSelf === true,
      firstSeen: int(payload.firstSeen),
      lastSeen: int(payload.lastSeen),
      /* Authoritative counters as last reported by the server, plus a local
         tally of loot events it has not yet folded into a `player` payload.
         A `player` event re-anchors the pair so it *replaces* the local tally
         instead of adding to it, which keeps the number exact whichever order
         the two arrive in. */
      baseGrabs: int(payload.grabs),
      baseUnits: int(payload.units),
      seenGrabs: 0,
      seenUnits: 0,
      anchorGrabs: 0,
      anchorUnits: 0,
      el: null,
      cells: null
    };
  }

  function grabsOf(p) {
    return p.baseGrabs + (p.seenGrabs - p.anchorGrabs);
  }

  function unitsOf(p) {
    return p.baseUnits + (p.seenUnits - p.anchorUnits);
  }

  /** A player the game has named that we have no record of yet. */
  function ensurePlayer(name, at) {
    var key = str(name);
    if (!key) return null;
    var existing = players.get(key);
    if (existing) return existing;
    var created = makePlayer({
      name: key,
      guild: null,
      alliance: null,
      isSelf: state.selfName !== null && state.selfName === key,
      firstSeen: at,
      lastSeen: at,
      grabs: 0,
      units: 0
    });
    players.set(key, created);
    return created;
  }

  /**
   * Insert or update a roster record. `fromSnapshot` re-anchors everything,
   * which is what makes a reconnect correct rather than additive.
   */
  function upsertPlayer(payload, fromSnapshot) {
    if (!payload || typeof payload.name !== 'string' || payload.name === '') return null;
    var name = payload.name;
    var entry = players.get(name);

    if (!entry) {
      entry = makePlayer(payload);
      if (state.selfName !== null && state.selfName === name) entry.isSelf = true;
      players.set(name, entry);
      return entry;
    }

    if ('guild' in payload) entry.guild = optStr(payload.guild);
    if ('alliance' in payload) entry.alliance = optStr(payload.alliance);
    if (payload.isSelf === true) entry.isSelf = true;

    entry.baseGrabs = int(payload.grabs);
    entry.baseUnits = int(payload.units);
    if (fromSnapshot) {
      entry.seenGrabs = 0;
      entry.seenUnits = 0;
      entry.anchorGrabs = 0;
      entry.anchorUnits = 0;
    } else {
      entry.anchorGrabs = entry.seenGrabs;
      entry.anchorUnits = entry.seenUnits;
    }

    if (int(payload.firstSeen) > 0) entry.firstSeen = int(payload.firstSeen);
    if (int(payload.lastSeen) > entry.lastSeen) entry.lastSeen = int(payload.lastSeen);
    if (state.selfName !== null && state.selfName === name) entry.isSelf = true;
    return entry;
  }

  /** Most recently active first, name ascending as the tie-break. */
  function rosterCompare(a, b) {
    if (a.lastSeen !== b.lastSeen) return b.lastSeen - a.lastSeen;
    return a.name < b.name ? -1 : a.name > b.name ? 1 : 0;
  }

  /* ------------------------------ name cells ------------------------------ */

  /**
   * `[Guild] Name`, the way the game renders it, with the brackets omitted
   * when the guild is not known yet. The three spans are built once and then
   * mutated, so a later `player` event can upgrade the guild in place.
   *
   * The space before the "you" chip is a real text node rather than only a CSS
   * margin: an accessible name that reads "Grimyou" is a bug a margin hides on
   * screen and a screen reader still reports.
   */
  function nameNodes(name, entry, isSelf) {
    var guild = el('span', 'guild');
    var pname = el('span', 'pname');
    var you = el('span', 'you', 'you');
    var frag = document.createDocumentFragment();
    frag.append(guild, pname, document.createTextNode(' '), you);
    var cells = { frag: frag, guild: guild, pname: pname, you: you };
    applyName(cells, name, entry, isSelf);
    return cells;
  }

  function applyName(cells, name, entry, isSelf) {
    var guild = entry ? entry.guild : null;
    cells.guild.textContent = guild ? '[' + guild + '] ' : '';
    cells.guild.hidden = !guild;
    cells.pname.textContent = name;
    cells.you.hidden = !isSelf;
  }

  function searchForName(name, entry) {
    if (!entry) return name.toLowerCase();
    return (name + ' ' + (entry.guild || '') + ' ' + (entry.alliance || '')).toLowerCase();
  }

  /* --------------------------------- feed --------------------------------- */

  function buildFeedRow(rec) {
    var tr = el('tr');
    tr.dataset.player = rec.by;

    if (rec.silver === true) tr.classList.add('is-silver');
    if (unknownItem(rec)) tr.classList.add('is-unknown');

    var self = state.selfName !== null && rec.by === state.selfName;
    if (self) tr.classList.add('is-self');

    var tdTime = el('td', 'c-time');
    tdTime.appendChild(makeTimeCell(rec.at));
    tr.appendChild(tdTime);

    var tdBy = el('td', 'c-by');
    var by = nameNodes(rec.by, players.get(rec.by), self);
    tdBy.appendChild(by.frag);
    tr.appendChild(tdBy);

    var from = null;
    var tdFrom = el('td', 'c-from');
    if (rec.from) {
      tdFrom.appendChild(document.createTextNode(ARROW + ' '));
      from = nameNodes(
        rec.from,
        players.get(rec.from),
        state.selfName !== null && rec.from === state.selfName
      );
      tdFrom.appendChild(from.frag);
    }
    tr.appendChild(tdFrom);

    var tdItem = el('td', 'c-item', itemLabel(rec));
    if (unknownItem(rec)) {
      tdItem.title =
        'This item id is not in the catalogue the binary was built with, ' +
        'usually because the item arrived with a game patch.';
    }
    tr.appendChild(tdItem);

    tr.appendChild(el('td', 'c-qty num', num(rec.qty)));

    var byEntry = players.get(rec.by);
    var fromEntry = players.get(rec.from);
    tr.dataset.search = [
      rec.by,
      searchForName(rec.by, byEntry),
      rec.from,
      searchForName(rec.from, fromEntry),
      itemLabel(rec),
      rec.itemUnique == null ? '' : str(rec.itemUnique),
      rec.itemNumId == null ? '' : String(rec.itemNumId)
    ]
      .join(' ')
      .toLowerCase();

    rowData.set(tr, { rec: rec, by: by, from: from, tr: tr });
    return tr;
  }

  /** Prepend one grab. The list reads newest first and trims from the tail. */
  function addFeedRow(rec, animate) {
    var tr = buildFeedRow(rec);
    if (animate) tr.classList.add('is-new');
    dom.feedBody.insertBefore(tr, dom.feedBody.firstChild);
    applyFilterTo(tr);
    if (!tr.hidden) state.feedVisible += 1;

    while (dom.feedBody.childElementCount > MAX_FEED_ROWS) {
      var dropped = dom.feedBody.lastElementChild;
      if (!dropped.hidden) state.feedVisible -= 1;
      dropped.remove();
    }
    return tr;
  }

  /**
   * The server subscribes before it reads the snapshot, so a grab that lands in
   * between reaches the client twice. Only the newest row is compared, and
   * only on every field that has to match, so two genuinely distinct grabs in
   * the same millisecond are still both shown.
   */
  function isDuplicate(rec) {
    var top = dom.feedBody.firstElementChild;
    if (!top) return false;
    var data = rowData.get(top);
    if (!data) return false;
    var prev = data.rec;
    return (
      prev.at === rec.at &&
      prev.by === rec.by &&
      prev.from === rec.from &&
      prev.qty === rec.qty &&
      prev.silver === rec.silver &&
      prev.itemNumId === rec.itemNumId
    );
  }

  /** Re-render a player's name cells in the feed after an upsert. */
  function refreshNamesInFeed(name) {
    var entry = players.get(name);
    var isSelf = state.selfName !== null && state.selfName === name;
    var children = dom.feedBody.children;
    for (var i = 0; i < children.length; i++) {
      var data = rowData.get(children[i]);
      if (!data) continue;
      if (data.rec.by === name) {
        applyName(data.by, name, entry, isSelf);
      }
      if (data.from && data.rec.from === name) {
        applyName(data.from, name, entry, isSelf);
      }
      children[i].classList.toggle(
        'is-self',
        data.rec.by !== null && data.rec.by === state.selfName
      );
    }
  }

  /* -------------------------------- roster -------------------------------- */

  function buildRosterRow(entry) {
    var tr = el('tr');
    tr.dataset.player = entry.name;
    tr.classList.toggle('is-self', entry.isSelf);

    var th = el('th', 'c-name');
    th.setAttribute('scope', 'row');
    var cells = nameNodes(entry.name, entry, entry.isSelf);
    th.appendChild(cells.frag);
    tr.appendChild(th);

    // Filled here, not left for the next update: a row is never painted with
    // empty numbers while it waits for the first event to refresh it.
    var grabs = el('td', 'c-num num', num(grabsOf(entry)));
    tr.appendChild(grabs);

    var units = el('td', 'c-num num', num(unitsOf(entry)));
    tr.appendChild(units);

    var timeNode = makeTimeCell(entry.lastSeen);
    var seen = el('td', 'c-seen');
    seen.appendChild(timeNode);
    tr.appendChild(seen);

    tr.dataset.search = searchForName(entry.name, entry);
    entry.el = tr;
    entry.cells = {
      tr: tr,
      guild: cells.guild,
      pname: cells.pname,
      you: cells.you,
      grabs: grabs,
      units: units,
      time: timeNode
    };
    return tr;
  }

  function findRosterInsertionPoint(entry) {
    var node = dom.rosterBody.firstChild;
    while (node) {
      if (node === entry.el) return node;
      var other = players.get(node.dataset.player);
      if (other && rosterCompare(entry, other) < 0) return node;
      node = node.nextSibling;
    }
    return null;
  }

  function repositionRosterRow(entry) {
    var tr = entry.el;
    if (!tr || tr.parentNode !== dom.rosterBody) return;
    var ref = findRosterInsertionPoint(entry);
    if (ref === tr) return;
    dom.rosterBody.insertBefore(tr, ref);
  }

  function ensureRosterRow(entry) {
    if (entry.el && entry.el.isConnected && entry.el.parentNode === dom.rosterBody) {
      return entry.el;
    }
    var tr = buildRosterRow(entry);
    dom.rosterBody.appendChild(tr);
    repositionRosterRow(entry);
    applyFilterTo(tr);
    if (!tr.hidden) state.rosterVisible += 1;

    while (dom.rosterBody.childElementCount > MAX_ROSTER_ROWS) {
      var dropped = dom.rosterBody.lastElementChild;
      if (!dropped.hidden) state.rosterVisible -= 1;
      dropped.remove();
    }
    return tr;
  }

  function updateRosterRow(entry) {
    var cells = entry.cells;
    if (!cells || !cells.grabs.isConnected) {
      ensureRosterRow(entry);
      return;
    }
    applyName(cells, entry.name, entry, entry.isSelf);
    cells.grabs.textContent = num(grabsOf(entry));
    cells.units.textContent = num(unitsOf(entry));
    cells.time.textContent = stamp(entry.lastSeen);
    cells.tr.classList.toggle('is-self', entry.isSelf);
    cells.tr.dataset.search = searchForName(entry.name, entry);
    repositionRosterRow(entry);
  }

  /* ------------------------------- filtering ------------------------------ */

  function matches(tr) {
    if (state.filter === '') return true;
    return (tr.dataset.search || '').indexOf(state.filter) !== -1;
  }

  function applyFilterTo(tr) {
    tr.hidden = !matches(tr);
  }

  function applyFilter() {
    var feedVisible = 0;
    var rosterVisible = 0;
    var i;
    var children = dom.feedBody.children;
    for (i = 0; i < children.length; i++) {
      children[i].hidden = !matches(children[i]);
      if (!children[i].hidden) feedVisible++;
    }
    children = dom.rosterBody.children;
    for (i = 0; i < children.length; i++) {
      children[i].hidden = !matches(children[i]);
      if (!children[i].hidden) rosterVisible++;
    }
    state.feedVisible = feedVisible;
    state.rosterVisible = rosterVisible;
    renderMeta();
    updateEmptyState();
  }

  /* ------------------------------ empty states ---------------------------- */

  function updateEmptyState() {
    var filtering = state.filter !== '';
    var feedRows = dom.feedBody.childElementCount;
    var rosterRows = dom.rosterBody.childElementCount;

    var showFeedEmpty = feedRows === 0 || (filtering && state.feedVisible === 0);
    dom.feedTable.hidden = showFeedEmpty;
    dom.feedEmpty.hidden = !showFeedEmpty;

    if (showFeedEmpty) {
      if (filtering) {
        dom.emptyTitle.textContent = 'No matches';
        dom.emptyBody.textContent =
          'Nothing in the feed matches ' + quote(state.filterText) + '.';
      } else if (!state.status || state.status.sawTraffic !== true) {
        var iface =
          state.status && state.status.interface
            ? ' on ' + str(state.status.interface)
            : '';
        dom.emptyTitle.textContent = 'Waiting for the game';
        dom.emptyBody.textContent =
          'Nothing recorded yet. loot-ledger is listening' + iface +
          ', but no packet from Albion has arrived. Start or join the game ' +
          'and grabs will appear here within a second.';
      } else {
        dom.emptyTitle.textContent = 'No loot yet';
        dom.emptyBody.textContent =
          'Game traffic is arriving, but nobody you can see has looted ' +
          'anything so far. The moment somebody does, it will be the top row.';
      }
    }

    var showRosterEmpty = rosterRows === 0 || (filtering && state.rosterVisible === 0);
    dom.rosterTable.hidden = showRosterEmpty;
    dom.rosterEmpty.hidden = !showRosterEmpty;

    if (showRosterEmpty) {
      if (filtering && rosterRows > 0) {
        dom.rosterEmptyTitle.textContent = 'No matches';
        dom.rosterEmptyBody.textContent =
          'No player matches ' + quote(state.filterText) + '.';
      } else {
        dom.rosterEmptyTitle.textContent = 'No players yet';
        dom.rosterEmptyBody.textContent =
          'A name appears here the first time loot-ledger sees someone loot ' +
          'something, or when the game introduces them.';
      }
    }
  }

  /* --------------------------------- meta --------------------------------- */

  function totalOf(key) {
    var totals = state.snapshot && state.snapshot.totals;
    var liveKey = TOTALS_TO_LIVE[key];
    return (totals ? int(totals[key]) : 0) + (liveKey ? live[liveKey] : 0);
  }

  function renderTotals() {
    var totals = state.snapshot && state.snapshot.totals;
    dom.tEvents.textContent = num(totalOf('events'));
    dom.tItems.textContent = num(totalOf('itemGrabs'));
    dom.tUnits.textContent = num(totalOf('units'));
    dom.tSilver.textContent = num(totalOf('silverGrabs'));
    dom.tUnknown.textContent = num(totalOf('unknownItems'));
    dom.tPlayers.textContent = num(
      Math.max(totals ? int(totals.players) : 0, players.size)
    );
  }

  function renderMeta() {
    var total = totalOf('events');
    var shown = dom.feedBody.childElementCount;

    if (state.filter !== '') {
      dom.feedMeta.textContent =
        num(state.feedVisible) + ' of ' + num(shown) +
        ' rows match ' + quote(state.filterText);
    } else if (shown >= MAX_FEED_ROWS) {
      dom.feedMeta.textContent =
        num(total) + ' grabs ' + MIDDOT +
        ' newest ' + num(shown) + ' shown';
    } else {
      dom.feedMeta.textContent = num(total) + (total === 1 ? ' grab' : ' grabs');
    }

    /* The roster is capped at MAX_ROSTER_ROWS, so reporting players.size
     * alone would claim to show more than it does -- while the feed, capped
     * the same way, says "newest N shown". Use the same shape here. */
    var rosterShown = dom.rosterBody.childElementCount;
    dom.rosterMeta.textContent =
      state.filter !== ''
        ? num(state.rosterVisible) + ' of ' + num(players.size) + ' players'
        : rosterShown < players.size
          ? num(players.size) + ' players ' + MIDDOT + ' most recent ' + num(rosterShown)
          : num(players.size) + (players.size === 1 ? ' player' : ' players');
  }

  /* ------------------------------ status bar ------------------------------ */

  function renderStatusFacts() {
    var status = state.status;
    dom.fUptime.textContent = status ? formatDuration(status.uptimeSeconds) : EM_DASH;
    dom.fIface.textContent = status
      ? status.interface
        ? str(status.interface)
        : 'all interfaces'
      : EM_DASH;
  }

  function renderZone() {
    var latest = state.zones.length ? state.zones[0] : null;
    if (!latest) {
      dom.fZone.textContent = EM_DASH;
      return;
    }
    dom.fZone.textContent =
      stamp(latest.at) + (latest.player ? ' ' + MIDDOT + ' ' + str(latest.player) : '');
  }

  function renderDiagnostics() {
    var snapshot = state.snapshot;
    var counters = state.counters;
    var status = snapshot && snapshot.status;

    dom.dSelf.textContent = state.selfName || 'not reported yet';
    dom.dStarted.textContent = status ? stamp(status.startedAt) : EM_DASH;
    dom.dFilter.textContent = status
      ? status.kernelFilter === true
        ? 'installed'
        : 'not installed ' + MIDDOT + ' reading all traffic'
      : EM_DASH;

    dom.dPackets.textContent = counters ? num(counters.packets) : EM_DASH;
    dom.dMessages.textContent = counters ? num(counters.messages) : EM_DASH;
    dom.dDecode.textContent = counters ? num(counters.decodeErrors) : EM_DASH;
    dom.dUnknown.textContent = num(totalOf('unknownItems'));
    dom.dSkipped.textContent = counters ? num(counters.skippedFrames) : EM_DASH;
    dom.dCrc.textContent = counters ? num(counters.crcChecked) : EM_DASH;

    var replay = snapshot && snapshot.replay;
    dom.dReplay.textContent = replay
      ? num(replay.applied) +
        (int(replay.skipped) ? ' ' + MIDDOT + ' ' + num(replay.skipped) + ' skipped' : '')
      : EM_DASH;

    var catalogue = snapshot && snapshot.catalogue;
    dom.dCatalogue.textContent = catalogue
      ? num(catalogue.items) +
        (catalogue.source ? ' ' + MIDDOT + ' ' + str(catalogue.source) : '')
      : EM_DASH;
  }

  function setConn(stateName, text) {
    dom.conn.dataset.state = stateName;
    dom.connDot.dataset.state = stateName;
    dom.connText.textContent = text;
  }

  function applyStatus(status) {
    if (!status || typeof status !== 'object') return;
    if ('self' in status) {
      var name = typeof status.self === 'string' && status.self !== '' ? status.self : null;
      if (name !== state.selfName) {
        state.selfName = name;
        markSelf();
      }
    }
    state.status = status;
    if (state.snapshot) renderStatusFacts();
    updateEmptyState();
  }

  /** Re-apply the is-self marker everywhere, e.g. when the game first names
   *  the local character. */
  function markSelf() {
    players.forEach(function (entry) {
      entry.isSelf = state.selfName !== null && entry.name === state.selfName;
      if (entry.el && entry.el.isConnected) updateRosterRow(entry);
    });
    refreshNamesInFeed(state.selfName || ' ');
  }

  /* ------------------------------ announcements --------------------------- */

  var announceTimer = null;
  var pendingAnnounce = null;

  function queueAnnouncement(rec) {
    if (pendingAnnounce) {
      pendingAnnounce.count += 1;
      /* Keep the *most recent* record. This used to be left at the first one,
       * so a burst of five would announce "Latest: <the first of the five>".
       * This is the only screen-reader channel, so a wrong "Latest" is a
       * factually wrong statement, not a cosmetic slip. */
      pendingAnnounce.rec = rec;
    } else {
      pendingAnnounce = { count: 1, rec: rec };
    }
    if (announceTimer === null) {
      announceTimer = setTimeout(flushAnnouncement, ANNOUNCE_MS);
    }
  }

  function flushAnnouncement() {
    announceTimer = null;
    var pending = pendingAnnounce;
    pendingAnnounce = null;
    if (!pending) return;

    var rec = pending.rec;
    /* The game can send an empty victim name (a solo chest, or a container
     * rather than a player). Naming the source only when there is one avoids
     * announcing "looted X from ." */
    var from = rec.from ? ' from ' + str(rec.from) : '';
    var line;
    if (rec.silver === true) {
      line = rec.by + ' picked up ' + num(rec.qty) + ' silver' + from + '.';
    } else {
      var qty = Math.max(0, int(rec.qty));
      line =
        rec.by + ' looted ' + (qty > 1 ? num(qty) + ' ' + TIMES + ' ' : '') +
        itemLabel(rec) + from + '.';
    }
    dom.announce.textContent =
      pending.count > 1 ? pending.count + ' new grabs. Latest: ' + line : line;
  }

  /* ----------------------------- stream events ---------------------------- */

  function readJson(event) {
    try {
      var value = JSON.parse(event.data);
      return value && typeof value === 'object' ? value : null;
    } catch (err) {
      return null;
    }
  }

  function onLoot(rec) {
    if (!isLootEvent(rec)) return;
    normalizeLoot(rec);
    if (isDuplicate(rec)) return;

    // Keep the summary strip live: `totals` only ever arrives with a snapshot.
    live.events += 1;
    if (rec.silver === true) {
      live.silver += 1;
    } else {
      live.items += 1;
      live.units += Math.max(0, int(rec.qty));
      if (unknownItem(rec)) live.unknown += 1;
    }

    // The server counts a grab against the looter and registers the victim as
    // a player too, so the roster has to do the same.
    var looter = ensurePlayer(rec.by, rec.at);
    if (looter) {
      looter.seenGrabs += 1;
      if (rec.silver !== true) looter.seenUnits += Math.max(0, int(rec.qty));
      if (int(rec.at) > looter.lastSeen) looter.lastSeen = int(rec.at);
      if (state.selfName !== null && state.selfName === rec.by) looter.isSelf = true;
      updateRosterRow(looter);
    }
    if (rec.from && rec.from !== rec.by) {
      var victim = ensurePlayer(rec.from, rec.at);
      if (victim && int(rec.at) > victim.lastSeen) {
        victim.lastSeen = int(rec.at);
        updateRosterRow(victim);
      }
    }

    addFeedRow(rec, true);
    renderTotals();
    renderMeta();
    updateEmptyState();
    queueAnnouncement(rec);
  }

  function onPlayer(payload) {
    if (!payload || typeof payload.name !== 'string' || payload.name === '') return;
    var entry = upsertPlayer(payload, false);
    if (!entry) return;
    ensureRosterRow(entry);
    updateRosterRow(entry);
    refreshNamesInFeed(entry.name);
    renderTotals();
    renderMeta();
    updateEmptyState();
  }

  /* -------------------------------- snapshot ------------------------------ */

  function renderHello(snapshot) {
    var first = state.snapshot === null;
    var keepScroll = dom.feedScroll.scrollTop;

    state.snapshot = snapshot;
    state.counters =
      snapshot.counters && typeof snapshot.counters === 'object' ? snapshot.counters : null;
    state.zones = Array.isArray(snapshot.zones) ? snapshot.zones : [];
    state.status =
      snapshot.status && typeof snapshot.status === 'object' ? snapshot.status : null;

    var self = state.status ? state.status.self : null;
    state.selfName = typeof self === 'string' && self !== '' ? self : null;

    live.events = 0;
    live.items = 0;
    live.silver = 0;
    live.units = 0;
    live.unknown = 0;

    state.feedVisible = 0;
    state.rosterVisible = 0;

    players.clear();
    var roster = Array.isArray(snapshot.players) ? snapshot.players : [];
    for (var r = 0; r < roster.length; r++) upsertPlayer(roster[r], true);

    dom.rosterBody.replaceChildren();
    dom.feedBody.replaceChildren();

    // `feed` arrives oldest first and addFeedRow prepends, so walking the
    // snapshot forwards leaves the newest grab on top.
    var feed = Array.isArray(snapshot.feed) ? snapshot.feed : [];
    for (var i = 0; i < feed.length; i++) {
      if (isLootEvent(feed[i])) addFeedRow(normalizeLoot(feed[i]), false);
    }

    var ordered = Array.from(players.values()).sort(rosterCompare);
    for (var p = 0; p < ordered.length; p++) ensureRosterRow(ordered[p]);

    applyFilter();
    renderTotals();
    renderMeta();
    renderStatusFacts();
    renderZone();
    renderDiagnostics();

    if (!first) dom.feedScroll.scrollTop = keepScroll;
  }

  /* ------------------------------- connection ----------------------------- */

  function connect() {
    if (typeof EventSource !== 'function') {
      setConn('stopped', 'unsupported browser');
      dom.feedEmpty.hidden = false;
      dom.feedTable.hidden = true;
      dom.emptyTitle.textContent = 'This browser cannot run the dashboard';
      dom.emptyBody.textContent =
        'EventSource is not available here, so the live stream cannot be ' +
        'read. The same data is available as JSON at /api/snapshot.';
      return;
    }

    var stream = new EventSource(STREAM_URL);

    // EventSource reconnects by itself; these two handlers only report it.
    stream.addEventListener('open', function () {
      if (state.status && state.status.running === false) {
        setConn('stopped', 'capture stopped');
      } else {
        setConn('live', 'live');
      }
    });

    stream.addEventListener('error', function () {
      setConn('reconnecting', 'reconnecting' + ELLIPSIS);
    });

    stream.addEventListener('hello', function (event) {
      var snapshot = readJson(event);
      if (snapshot) renderHello(snapshot);
    });

    stream.addEventListener('loot', function (event) {
      var rec = readJson(event);
      if (rec) onLoot(rec);
    });

    stream.addEventListener('player', function (event) {
      var payload = readJson(event);
      if (payload) onPlayer(payload);
    });

    stream.addEventListener('status', function (event) {
      var payload = readJson(event);
      if (!payload) return;

      /* The server sends {status, totals, counters}. The totals are
       * authoritative -- they come from the same state as the snapshot -- so
       * prefer them over the locally-derived `live` deltas and reset those to
       * zero. Without this the summary strip is only ever as right as the
       * events this client happened to see. The bare-status shape is still
       * accepted, so an older server keeps working. */
      var status = 'status' in payload ? payload.status : payload;
      applyStatus(status);

      if (state.snapshot && payload.totals && typeof payload.totals === 'object') {
        state.snapshot.totals = payload.totals;
        live.events = 0;
        live.items = 0;
        live.silver = 0;
        live.units = 0;
        live.unknown = 0;
        renderTotals();
        renderMeta();
      }

      if (payload.counters && typeof payload.counters === 'object') {
        state.counters = payload.counters;
        renderDiagnostics();
      }
    });
  }

  /** Belt and braces: the `hello` frame is the real initial render, but if the
   *  stream has not produced one yet, ask the same server over plain HTTP. */
  function fallbackSnapshot() {
    if (state.snapshot !== null) return;
    if (typeof fetch !== 'function') return;
    fetch(SNAPSHOT_URL, { headers: { accept: 'application/json' } })
      .then(function (response) {
        return response && response.ok ? response.json() : null;
      })
      .then(function (snapshot) {
        if (snapshot && typeof snapshot === 'object') renderHello(snapshot);
      })
      .catch(function () {
        /* The stream is the primary path; if it recovers it sends `hello`. */
      });
  }

  /* ---------------------------------- boot -------------------------------- */

  function onFilterInput() {
    state.filterText = str(dom.q.value).trim();
    state.filter = state.filterText.toLowerCase();
    applyFilter();
  }

  function boot() {
    if (dom.rosterEmpty) {
      dom.rosterEmptyTitle = dom.rosterEmpty.querySelector('.empty-title');
      dom.rosterEmptyBody = dom.rosterEmpty.querySelector('.empty-body');
    }

    dom.q.addEventListener('input', onFilterInput);
    dom.q.addEventListener('keydown', function (event) {
      if (event.key === 'Escape' && dom.q.value !== '') {
        event.preventDefault();
        dom.q.value = '';
        onFilterInput();
      }
    });

    setInterval(function () {
      var now = Date.now();
      for (var i = timeCells.length - 1; i >= 0; i--) {
        var cell = timeCells[i];
        if (!cell.el.isConnected) {
          timeCells.splice(i, 1);
          continue;
        }
        if (now - cell.at < RECENT_MS + TICK_MS) cell.el.textContent = stamp(cell.at);
      }
      if (state.zones.length) renderZone();
    }, TICK_MS);

    setTimeout(fallbackSnapshot, HELLO_FALLBACK_MS);

    updateEmptyState();
    connect();
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', boot);
  } else {
    boot();
  }
})();
