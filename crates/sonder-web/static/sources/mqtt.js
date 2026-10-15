// `mqtt` — a minimal MQTT 3.1.1 client over WebSocket (binary frames, subprotocol `mqtt`).
// Mirrors adapters/mqtt.rs: every PUBLISH becomes a `message` event; readings every second.
//
//   params: url       ws(s)://host:port/path  (default wss://test.mosquitto.org:8081/mqtt;
//                     mqtt:// is reported as unsupported in the browser — use the ws URL)
//           topics    ["#"]            username/password     client_id
//           keepalive 60 s             max_rate 50 events/s (messages beyond are sampled out)
//   event `message`:  topic, len, value (payload is a number), data.* (<= 16 leaves of a JSON
//                     object payload); intensity = clamp(len/512, 0.05, 1)
//   readings (1 Hz):  rate (msg/s), topics (distinct in the last 60 s), bytes (bytes/s)
//
// Packet codec (encode*/decodePacket) is hand-rolled and exported for tests.
import { Attrs, Connector, Emitter, RateWindow, nowSec, paramF64, paramSecs, paramStr, paramStrList } from './util.js';
import { flattenLeaves } from './generic.js';

export const DEFAULT_URL = 'wss://test.mosquitto.org:8081/mqtt';
export const DEFAULT_MAX_RATE = 50;
export const MAX_DATA_LEAVES = 16;

// --- codec -------------------------------------------------------------------
export const CONNECT = 1, CONNACK = 2, PUBLISH = 3, PUBACK = 4, SUBSCRIBE = 8, SUBACK = 9, PINGREQ = 12, PINGRESP = 13, DISCONNECT = 14;

const enc = new TextEncoder();
const dec = new TextDecoder('utf-8', { fatal: false });

/** Remaining-length varint (1..4 bytes). */
export function encodeVarint(n) {
  if (n < 0 || n > 268435455) throw new Error(`mqtt: remaining length ${n} out of range`);
  const out = [];
  do { let b = n % 128; n = Math.floor(n / 128); if (n > 0) b |= 0x80; out.push(b); } while (n > 0);
  return out;
}
/** → {value, len} or null when the buffer is too short; throws on > 4 bytes. */
export function decodeVarint(buf, off = 0) {
  let mult = 1, value = 0, i = 0;
  for (;;) {
    if (off + i >= buf.length) return null;
    const b = buf[off + i++];
    value += (b & 0x7f) * mult;
    if ((b & 0x80) === 0) return { value, len: i };
    mult *= 128;
    if (i >= 4) throw new Error('mqtt: malformed remaining length');
  }
}
export function encodeString(s) {
  const b = enc.encode(String(s));
  if (b.length > 65535) throw new Error('mqtt: string too long');
  return [b.length >> 8, b.length & 0xff, ...b];
}
function readString(buf, off) {
  if (off + 2 > buf.length) throw new Error('mqtt: truncated string');
  const len = (buf[off] << 8) | buf[off + 1];
  if (off + 2 + len > buf.length) throw new Error('mqtt: truncated string');
  return { str: dec.decode(buf.subarray(off + 2, off + 2 + len)), next: off + 2 + len };
}
function packet(typeFlags, body) {
  return new Uint8Array([typeFlags, ...encodeVarint(body.length), ...body]);
}

export function encodeConnect({ clientId, username = null, password = null, keepalive = 60, cleanSession = true } = {}) {
  let flags = cleanSession ? 0x02 : 0;
  if (username !== null && username !== undefined) flags |= 0x80;
  if (password !== null && password !== undefined) flags |= 0x40;
  const ka = Math.max(0, Math.min(65535, Math.round(keepalive)));
  const body = [...encodeString('MQTT'), 4, flags, ka >> 8, ka & 0xff, ...encodeString(clientId ?? '')];
  if (flags & 0x80) body.push(...encodeString(username));
  if (flags & 0x40) body.push(...encodeString(password));
  return packet(CONNECT << 4, body);
}
export function encodeSubscribe(packetId, topics, qos = 0) {
  const body = [packetId >> 8, packetId & 0xff];
  for (const t of topics) body.push(...encodeString(t), qos & 3);
  return packet((SUBSCRIBE << 4) | 0x02, body);
}
export function encodePublish(topic, payload, { qos = 0, retain = false, packetId = 0 } = {}) {
  const p = typeof payload === 'string' ? enc.encode(payload) : payload;
  const body = [...encodeString(topic)];
  if (qos > 0) body.push(packetId >> 8, packetId & 0xff);
  body.push(...p);
  return packet((PUBLISH << 4) | (qos << 1) | (retain ? 1 : 0), body);
}
export const encodePuback = (packetId) => packet(PUBACK << 4, [packetId >> 8, packetId & 0xff]);
export const encodePingreq = () => new Uint8Array([PINGREQ << 4, 0]);
export const encodeDisconnect = () => new Uint8Array([DISCONNECT << 4, 0]);

export const CONNACK_CODES = ['accepted', 'unacceptable protocol version', 'identifier rejected', 'server unavailable', 'bad user name or password', 'not authorized'];

/**
 * Decode one packet at the start of `buf`. Returns null when incomplete, else
 * {type, flags, len (bytes consumed), ...fields}: CONNACK {sessionPresent, code};
 * PUBLISH {topic, qos, retain, dup, packetId, payload: Uint8Array}; SUBACK {packetId, codes}.
 */
export function decodePacket(buf) {
  if (buf.length < 2) return null;
  const type = buf[0] >> 4, flags = buf[0] & 0x0f;
  const rl = decodeVarint(buf, 1);
  if (!rl) return null;
  const start = 1 + rl.len, end = start + rl.value;
  if (buf.length < end) return null;
  const body = buf.subarray(start, end);
  const out = { type, flags, len: end };
  switch (type) {
    case CONNACK: out.sessionPresent = !!(body[0] & 1); out.code = body[1]; break;
    case PUBLISH: {
      const { str, next } = readString(body, 0);
      out.topic = str; out.qos = (flags >> 1) & 3; out.retain = !!(flags & 1); out.dup = !!(flags & 8);
      let off = next;
      if (out.qos > 0) { out.packetId = (body[off] << 8) | body[off + 1]; off += 2; } else out.packetId = 0;
      out.payload = body.slice(off);
      break;
    }
    case SUBACK: out.packetId = (body[0] << 8) | body[1]; out.codes = Array.from(body.subarray(2)); break;
    case PUBACK: out.packetId = (body[0] << 8) | body[1]; break;
    default: break;
  }
  return out;
}

/** Accumulates WebSocket binary frames and yields whole packets. */
export class PacketReader {
  constructor() { this.buf = new Uint8Array(0); }
  push(bytes) {
    if (this.buf.length === 0) this.buf = bytes;
    else { const n = new Uint8Array(this.buf.length + bytes.length); n.set(this.buf); n.set(bytes, this.buf.length); this.buf = n; }
    const out = [];
    for (;;) {
      const p = decodePacket(this.buf);
      if (!p) break;
      out.push(p);
      this.buf = this.buf.subarray(p.len);
    }
    if (this.buf.length > 16 * 1024 * 1024) { this.buf = new Uint8Array(0); throw new Error('mqtt: packet too large'); }
    return out;
  }
}

// --- payload → attrs ---------------------------------------------------------
export const messageIntensity = (len) => Math.min(1, Math.max(0.05, len / 512));

/** Attributes for a PUBLISH: topic, len, value (numeric payload) or data.* (JSON object payload). */
export function messageAttrs(topic, payload) {
  const a = new Attrs().text('topic', topic).num('len', payload.length);
  if (payload.length === 0 || payload.length > 65536) return a.intoMap();
  const text = dec.decode(payload).trim();
  if (/^[-+]?(\d+\.?\d*|\.\d+)([eE][-+]?\d+)?$/.test(text)) {
    const n = Number(text);
    if (Number.isFinite(n)) a.num('value', n);
    return a.intoMap();
  }
  if (text.startsWith('{')) {
    let v;
    try { v = JSON.parse(text); } catch (_) { return a.intoMap(); }
    if (v && typeof v === 'object' && !Array.isArray(v)) Object.assign(a.map, flattenLeaves(v, { prefix: 'data', cap: MAX_DATA_LEAVES }));
  }
  return a.intoMap();
}

/** Parse + validate the url param. Returns {url} or {error} (a status message). */
export function resolveUrl(spec, pageProtocol = (typeof location !== 'undefined' ? location.protocol : 'https:')) {
  const raw = paramStr(spec, 'url', DEFAULT_URL).trim() || DEFAULT_URL;
  if (/^mqtts?:\/\//i.test(raw)) {
    const host = raw.replace(/^mqtts?:\/\//i, '').replace(/[:/].*$/, '');
    return { error: `mqtt:// (raw TCP) is not possible in the browser; use the broker's WebSocket URL, e.g. wss://${host}:8081/mqtt` };
  }
  if (!/^wss?:\/\//i.test(raw)) return { error: `param \`url\`: expected ws:// or wss:// (got ${raw})` };
  if (pageProtocol === 'https:' && /^ws:\/\//i.test(raw)) {
    return { error: `an https page cannot open ws:// (mixed content); use wss:// — e.g. ${raw.replace(/^ws:/i, 'wss:').replace(':8080', ':8081')}` };
  }
  return { url: raw };
}

export class Stats {
  constructor() { this.msgs = new RateWindow(); this.bytes = []; this.topics = []; }
  push(at, topic, len) {
    this.msgs.push(at); this.bytes.push([at, len]); this.topics.push([at, topic]);
    while (this.bytes.length > 100000) this.bytes.shift();
    while (this.topics.length > 100000) this.topics.shift();
  }
  readings(now) {
    while (this.bytes.length && now - this.bytes[0][0] > 1) this.bytes.shift();
    while (this.topics.length && now - this.topics[0][0] > 60) this.topics.shift();
    const set = new Set(); for (const [, t] of this.topics) set.add(t);
    return { rate: this.msgs.count(now, 1), topics: set.size, bytes: this.bytes.reduce((s, [, b]) => s + b, 0) };
  }
}

// --- adapter -----------------------------------------------------------------
export default class Adapter {
  constructor(spec, emit, log) {
    this.spec = spec;
    this.em = new Emitter(emit, log);
    this.resolved = resolveUrl(spec);
    const topics = paramStrList(spec, 'topics') || ['#'];
    this.topics = topics.length ? topics : ['#'];
    this.username = paramStr(spec, 'username', null);
    this.password = paramStr(spec, 'password', null);
    this.clientId = paramStr(spec, 'client_id', '') || `sonder-${Math.random().toString(36).slice(2, 10)}`;
    this.keepalive = paramSecs(spec, 'keepalive', 60, 5);
    this.maxRate = Math.max(1, paramF64(spec, 'max_rate', DEFAULT_MAX_RATE));
    this.stats = new Stats();
    this.emitted = new RateWindow();
  }
  start() {
    if (this.resolved.error) { this.em.statusForce('failed', this.resolved.error); return; }
    this.conn = new Connector(this.em);
    this.conn.run((signal) => this.session(signal));
  }
  stop() { this.conn?.stop(); this.em.close(); }

  session(signal) {
    return new Promise((resolve, reject) => {
      let ws;
      try { ws = new WebSocket(this.resolved.url, ['mqtt']); } catch (e) { return reject(`connect failed: ${e.message || e}`); }
      ws.binaryType = 'arraybuffer';
      const reader = new PacketReader();
      let opened = false, connected = false, subscribed = false;
      let lastRx = performance.now(), lastPing = performance.now();
      let nextReading = nowSec() + 1;
      const finish = (fn, v) => {
        clearInterval(timer); signal.removeEventListener('abort', onAbort);
        try { if (ws.readyState === WebSocket.OPEN) ws.send(encodeDisconnect()); } catch (_) { /* ignore */ }
        try { ws.close(); } catch (_) { /* ignore */ }
        fn(v);
      };
      const onAbort = () => finish(resolve);
      signal.addEventListener('abort', onAbort, { once: true });
      ws.onopen = () => {
        opened = true; lastRx = performance.now();
        ws.send(encodeConnect({ clientId: this.clientId, username: this.username, password: this.password, keepalive: this.keepalive }));
      };
      ws.onerror = () => { /* onclose follows */ };
      ws.onclose = (ev) => finish(reject, opened ? `server closed${ev && ev.code && ev.code !== 1000 && ev.code !== 1005 ? ` (${ev.code})` : ''}` : 'connect failed (network, wrong port or no `mqtt` subprotocol)');
      ws.onmessage = (ev) => {
        lastRx = performance.now();
        if (!(ev.data instanceof ArrayBuffer)) return;
        let packets;
        try { packets = reader.push(new Uint8Array(ev.data)); } catch (e) { return finish(reject, e.message || String(e)); }
        for (const p of packets) {
          switch (p.type) {
            case CONNACK:
              if (p.code !== 0) return finish(reject, `broker refused: ${CONNACK_CODES[p.code] || `code ${p.code}`}`);
              connected = true;
              ws.send(encodeSubscribe(1, this.topics, 0));
              this.em.status('connecting', 'connected, subscribing');
              break;
            case SUBACK:
              if (p.codes.every((c) => c === 0x80)) return finish(reject, `subscription refused for ${this.topics.join(', ')}`);
              subscribed = true;
              this.em.statusForce('live', `subscribed to ${this.topics.join(', ')}`);
              break;
            case PUBLISH: this.onPublish(p, ws); break;
            case PINGRESP: default: break;
          }
        }
      };
      const timer = setInterval(() => {
        const now = performance.now();
        if (!opened && now - lastRx > 20000) return finish(reject, 'connect timeout');
        if (opened && !connected && now - lastRx > 10000) return finish(reject, 'no CONNACK');
        if (connected && now - lastRx > this.keepalive * 1500) return finish(reject, `keepalive timeout (${this.keepalive} s)`);
        if (connected && now - lastPing > this.keepalive * 500) { lastPing = now; try { ws.send(encodePingreq()); } catch (_) { /* closing */ } }
        const t = nowSec();
        if (subscribed && t >= nextReading) { nextReading = t + 1; this.em.reading(this.stats.readings(t)); }
      }, 250);
    });
  }

  onPublish(p, ws) {
    if (p.qos === 1) { try { ws.send(encodePuback(p.packetId)); } catch (_) { /* ignore */ } }
    const now = nowSec();
    this.stats.push(now, p.topic, p.payload.length);
    // Received data: the broker is talking to us, so a reconnect after a server-side cut
    // (test.mosquitto.org's `wildcard` user is dropped after 20 s) may retry quickly.
    if (this.conn) this.conn.attempt = 0;
    if (this.emitted.count(now, 1) >= this.maxRate) return;    // sampled out; still counted above
    this.emitted.push(now);
    this.em.status('live', 'live');
    this.em.event('message', messageIntensity(p.payload.length), messageAttrs(p.topic, p.payload));
  }
}
