// Shared rooms (docs/ROOMS.md).
//
// Two jobs, one panel:
//   guest (worker mode, `?room=wss://relay/r/<id>`): join the relay, start the engine with the
//     host's scene + seed, feed the host's samples to the worker, estimate the clock offset,
//     show listeners + chat (chat lines also become captions);
//   ws mode (a browser on the host's own server): the panel reads `snapshot.room` and sends
//     `{"cmd":"chat"}` — that server already shares snapshots and audio, so it *is* a room.
//
// Pure helpers (`joinUrl`, `parseFrame`, `estimateOffset`, `pickOffset`) are exported for the
// node tests; `initRooms` wires the DOM.

/** `…/r/<id>` (ws/wss/http/https, optional trailing `/`, `/join`, `/host`) → `…/r/<id>/join`. */
export function joinUrl(room) {
  let u = String(room || '').trim();
  if (!u) return null;
  if (!/^[a-z]+:\/\//i.test(u)) u = `wss://${u}`;
  u = u.replace(/^http:/i, 'ws:').replace(/^https:/i, 'wss:');
  u = u.replace(/\/+$/, '').replace(/\/(join|host)$/, '');
  if (!/\/r\/[A-Za-z0-9_-]{1,64}$/.test(u)) return null;
  return `${u}/join`;
}

/** The room id in a room URL ("" when it is not one). */
export function roomId(room) {
  const j = joinUrl(room);
  if (!j) return '';
  return j.replace(/\/join$/, '').split('/').pop();
}

/** The host's seed as a decimal string: `seed_str` (exact) over `seed` (a double past 2^53). */
export function helloSeed(hello) {
  if (hello && typeof hello.seed_str === 'string' && /^\d+$/.test(hello.seed_str)) return hello.seed_str;
  return hello && hello.seed != null ? String(hello.seed) : null;
}

/** A relay frame as an object with a string `type`, or null. */
export function parseFrame(text) {
  try {
    const v = JSON.parse(text);
    return v && typeof v === 'object' && typeof v.type === 'string' ? v : null;
  } catch (_) {
    return null;
  }
}

/**
 * One clock-offset sample from a `pong`. `sent`/`nowMs` are this page's clock (ms);
 * `pong.host_time` is the host's engine time when the relay answered. Half the round trip
 * later it has moved on, so hostNow ≈ host_time + rtt/2. Returns {offset, rtt} where
 * host engine time ≈ guest engine time + offset (seconds), or null for a useless pong.
 */
export function estimateOffset(pong, guestEngineTime, nowMs) {
  if (!pong || typeof pong.host_time !== 'number' || typeof pong.sent !== 'number') return null;
  const rtt = Math.max(0, nowMs - pong.sent);
  if (pong.present === false || pong.host_age_ms < 0) return null;
  const hostNow = pong.host_time + rtt / 2000;
  return { offset: hostNow - guestEngineTime, rtt };
}

/** The offset of the lowest-rtt sample among the most recent `keep` (NTP-style). */
export function pickOffset(samples, keep = 5) {
  const recent = samples.slice(-keep).filter((s) => s && Number.isFinite(s.offset));
  if (!recent.length) return 0;
  return recent.reduce((best, s) => (s.rtt < best.rtt ? s : best)).offset;
}

/** `{t,s,d}` items → `[source, datumJson]` pairs for `transport.postSample`. */
export function sampleItems(frame) {
  const out = [];
  for (const it of (frame && frame.items) || []) {
    if (!it || typeof it.s !== 'string' || !it.d || typeof it.d !== 'object') continue;
    out.push([it.s, JSON.stringify(it.d)]);
  }
  return out;
}

const PING_MS = 5000;
const esc = (v) => String(v).replace(/[&<>"]/g, (ch) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[ch]));
const mmss = (t) => { const s = Math.max(0, Math.floor(t || 0)); return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, '0')}`; };

/** A relay guest connection with reconnect. Events: hello, samples, snapshot, chat, presence, pong, closed, status. */
export class RoomClient {
  constructor(url) {
    this.url = url;
    this.ws = null;
    this.handlers = new Map();
    this.closed = false;
    this.backoff = 1000;
    this.pingTimer = null;
    this.samples = [];        // offset samples
    this.offset = 0;
    this.rtt = null;
  }
  on(type, fn) { if (!this.handlers.has(type)) this.handlers.set(type, []); this.handlers.get(type).push(fn); return this; }
  emit(type, ...args) { for (const fn of this.handlers.get(type) || []) { try { fn(...args); } catch (e) { console.error(e); } } }
  open(getEngineTime) {
    if (this.closed) return this;
    this.getEngineTime = getEngineTime || (() => 0);
    this.emit('status', 'connecting');
    const ws = new WebSocket(this.url);
    ws.onopen = () => { this.backoff = 1000; this.emit('status', 'live'); this.ping(); this.pingTimer = setInterval(() => this.ping(), PING_MS); };
    ws.onmessage = (ev) => {
      const f = parseFrame(ev.data);
      if (!f) return;
      if (f.type === 'pong') {
        const s = estimateOffset(f, this.getEngineTime(), performance.timeOrigin + performance.now());
        if (s) { this.samples.push(s); if (this.samples.length > 12) this.samples.shift(); this.offset = pickOffset(this.samples); this.rtt = s.rtt; }
      }
      this.emit(f.type, f);
    };
    ws.onclose = () => {
      clearInterval(this.pingTimer); this.pingTimer = null; this.ws = null;
      this.emit('status', 'closed');
      if (!this.closed) { setTimeout(() => this.open(this.getEngineTime), this.backoff); this.backoff = Math.min(15000, this.backoff * 1.7); }
    };
    ws.onerror = () => { /* onclose follows */ };
    this.ws = ws;
    return this;
  }
  send(obj) { if (this.ws && this.ws.readyState === WebSocket.OPEN) this.ws.send(JSON.stringify(obj)); }
  chat(from, text) { this.send({ type: 'chat', from, text }); }
  ping() { this.send({ type: 'ping', sent: performance.timeOrigin + performance.now() }); }
  close() { this.closed = true; clearInterval(this.pingTimer); if (this.ws) { try { this.ws.close(); } catch (_) { /* ignore */ } } }
}

/**
 * Wire the room panel. `root` is the panel element (filled here), `joinButton` the header
 * button. Hooks from app.js: `params` (URLSearchParams), `send(cmd)` (to the engine),
 * `getTransport()`, `startScene(toml, seed)`, `pushCaption({at,text,importance})`,
 * `getEngineTime()`, `banner(text, bad)`.
 * Returns { active(), inRoom(), engineReady(), update(snapshot), leave() }.
 */
export function initRooms({ root, joinButton, params, send, getTransport, startScene, pushCaption, getEngineTime, banner }) {
  const wanted = params.get('room');
  const url = joinUrl(wanted);
  let client = null;            // RoomClient when a guest
  let hello = null;             // latest hello from the host
  let engineReady = false;      // worker engine loaded (scenes known)
  let startedFor = '';          // `${seed}|${toml}` the engine currently runs
  let listeners = 0;
  let status = '';
  const chat = [];              // {at, from, text} newest last (≤ 32)
  let lastWsChatKey = '';
  const name = () => { try { return localStorage.getItem('sonder.name') || ''; } catch (_) { return ''; } };
  const setName = (n) => { try { localStorage.setItem('sonder.name', n); } catch (_) { /* private mode */ } };

  // --- DOM -----------------------------------------------------------------------------
  root.innerHTML = `
    <div class="rhead">
      <span class="rtitle">room <b id="room-id"></b></span>
      <span id="room-listeners" class="dim mono"></span>
      <button id="room-leave" class="btn ghost" title="leave this room">leave</button>
    </div>
    <ol id="room-chat" class="rchat"></ol>
    <form id="room-form" class="rform" autocomplete="off">
      <input id="room-name" class="mono" placeholder="name" maxlength="32" title="your name in the chat">
      <input id="room-text" placeholder="say something… (/moment saves the last 10 min)" maxlength="240">
      <button class="btn" type="submit">send</button>
    </form>
    <div id="room-status" class="rstatus dim mono"></div>`;
  const $ = (sel) => root.querySelector(sel);
  $('#room-name').value = name();
  $('#room-name').addEventListener('change', () => setName($('#room-name').value.trim()));
  $('#room-form').addEventListener('submit', (e) => {
    e.preventDefault();
    const text = $('#room-text').value.trim();
    const from = $('#room-name').value.trim() || 'anon';
    if (!text) return;
    setName(from);
    if (client) client.chat(from, text); else send({ cmd: 'chat', from, text });
    $('#room-text').value = '';
  });
  $('#room-leave').addEventListener('click', () => api.leave());
  if (joinButton) {
    joinButton.addEventListener('click', () => {
      const u = prompt('Room URL (wss://relay.example/r/<id>):', wanted || '');
      if (!u) return;
      if (!joinUrl(u)) { banner('that is not a room URL (…/r/<id>)', true); return; }
      const next = new URL(location.href);
      next.searchParams.set('room', u);
      next.searchParams.set('mode', 'worker');
      next.hash = '';
      location.href = next.href;
    });
  }

  function renderChat() {
    $('#room-chat').innerHTML = chat.slice(-32).map((c) => `<li><span class="mono dim">${mmss(c.at)}</span> <b>${esc(c.from)}</b> ${esc(c.text)}</li>`).join('');
    const ol = $('#room-chat');
    ol.scrollTop = ol.scrollHeight;
  }
  function renderHead(id, role) {
    root.hidden = false;
    $('#room-id').textContent = id;
    $('#room-listeners').textContent = `${listeners} listening`;
    $('#room-listeners').title = role === 'host' ? 'you are the host' : 'you are a guest';
    const bits = [];
    if (status) bits.push(status);
    if (client && client.rtt != null) bits.push(`offset ${client.offset >= 0 ? '+' : ''}${client.offset.toFixed(2)}s · rtt ${Math.round(client.rtt)}ms`);
    if (hello && hello.name) bits.push(`host: ${hello.name}`);
    $('#room-status').textContent = bits.join(' · ');
  }
  function addChat(c, at) {
    chat.push({ at, from: c.from || 'anon', text: c.text || '' });
    if (chat.length > 32) chat.splice(0, chat.length - 32);
    renderChat();
  }

  // --- guest ----------------------------------------------------------------------------
  function maybeStart() {
    if (!client || !hello || !engineReady) return;
    const seed = helloSeed(hello);
    const key = `${seed}|${hello.scene_toml}`;
    if (key === startedFor) return;
    startedFor = key;
    startScene(hello.scene_toml, seed);
    banner(`in room ${roomId(url)}: playing the host's performance${hello.title ? ` — ${hello.title}` : ''}`);
  }
  function connect() {
    client = new RoomClient(url);
    client.on('status', (s) => { status = s; renderHead(roomId(url), 'guest'); });
    client.on('hello', (h) => { hello = h; maybeStart(); renderHead(roomId(url), 'guest'); });
    client.on('samples', (f) => {
      const t = getTransport();
      if (!t || t.mode !== 'worker' || !t.started) return;
      for (const [source, json] of sampleItems(f)) t.postSample(source, json);
    });
    client.on('presence', (p) => { listeners = p.listeners | 0; renderHead(roomId(url), 'guest'); });
    client.on('pong', () => renderHead(roomId(url), 'guest'));
    client.on('chat', (c) => {
      const at = getEngineTime();
      addChat(c, at);
      pushCaption({ at, layer: null, text: `${c.from || 'anon'}: ${c.text || ''}`, importance: 0.6 });
    });
    client.on('error', (e) => banner(e.error || 'room error', true));
    client.on('closed', () => { status = 'room closed'; renderHead(roomId(url), 'guest'); banner('the room closed (host gone for a minute)', true); });
    client.open(getEngineTime);
    if (joinButton) joinButton.hidden = true;
    renderHead(roomId(url), 'guest');
  }

  const api = {
    /** Was a room requested in the URL? */
    active: () => !!url,
    /** Currently a guest fed by a host (the worker must not run its own adapters)? */
    inRoom: () => !!client,
    /** Worker engine is loaded: start as soon as the hello is here. */
    engineReady() { engineReady = true; maybeStart(); },
    /** Every snapshot (ws mode: the panel mirrors `snapshot.room`). */
    update(snap) {
      if (client) return;
      const r = snap && snap.room;
      if (!r) { if (!root.hidden) root.hidden = true; return; }
      listeners = r.listeners | 0;
      const last = r.chat && r.chat.length ? r.chat[r.chat.length - 1] : null;
      const key = last ? `${last.at}|${last.from}|${last.text}` : '';
      if (key !== lastWsChatKey) {
        lastWsChatKey = key;
        chat.length = 0;
        for (const c of r.chat || []) chat.push({ at: c.at, from: c.from, text: c.text });
        renderChat();
      }
      renderHead(r.id || '', r.role);
    },
    leave() {
      if (client) {
        client.close();
        client = null;
        const next = new URL(location.href);
        next.searchParams.delete('room');
        location.href = next.href;
      } else {
        root.hidden = true;
      }
    },
  };

  if (url) connect();
  else if (wanted) banner(`?room= is not a room URL (…/r/<id>): ${wanted}`, true);
  return api;
}
