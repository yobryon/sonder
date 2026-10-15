// Shared logic for the generic adapters (`http-json`, `sse`, `websocket`): the tiny
// JSON path language and the object → Reading/Event converter. Mirrors
// crates/sonder-sources/src/{jsonpath.rs,adapters/generic.rs}.

import { paramStr, paramStrList } from './util.js';

export const MAX_FIELDS = 64;
/** Maximum event attributes per object (nested scalars are flattened, see `attrsOf`). */
export const MAX_ATTRS = 48;
/** attrsOf limits: nesting depth, array elements per array, string length. */
export const ATTR_MAX_DEPTH = 4;
export const ATTR_MAX_ARRAY = 8;
export const ATTR_MAX_STR = 200;

// ---------------------------------------------------------------------------
// jsonpath
// ---------------------------------------------------------------------------
/** Parse `.a.b[0]["weird key"]` into segments [{key}|{index}]; null on syntax error. */
export function parsePath(path) {
  let s = String(path).trim();
  if (s.startsWith('$')) s = s.slice(1);
  const segs = [];
  let i = 0;
  while (i < s.length) {
    const c = s[i];
    if (c === '.') { i++; continue; }
    if (c === '[') {
      i++;
      if (i >= s.length) return null;
      if (s[i] === '"' || s[i] === "'") {
        const q = s[i++];
        let key = '';
        for (;;) {
          if (i >= s.length) return null;
          if (s[i] === '\\' && i + 1 < s.length) { key += s[i + 1]; i += 2; continue; }
          if (s[i] === q) { i++; break; }
          key += s[i++];
        }
        if (i >= s.length || s[i] !== ']') return null;
        i++;
        segs.push({ key });
      } else {
        const start = i;
        while (i < s.length && s[i] !== ']') i++;
        if (i >= s.length) return null;
        const inner = s.slice(start, i).trim();
        i++;
        if (/^\d+$/.test(inner)) segs.push({ index: parseInt(inner, 10) });
        else if (inner.length) segs.push({ key: inner });
        else return null;
      }
      continue;
    }
    const start = i;
    while (i < s.length && s[i] !== '.' && s[i] !== '[') i++;
    segs.push({ key: s.slice(start, i) });
  }
  return segs;
}

export function extract(v, path) {
  const segs = parsePath(path);
  if (!segs) return undefined;
  let cur = v;
  for (const seg of segs) {
    if (cur === null || cur === undefined) return undefined;
    if ('key' in seg) {
      if (Array.isArray(cur)) { if (!/^\d+$/.test(seg.key)) return undefined; cur = cur[parseInt(seg.key, 10)]; }
      else if (typeof cur === 'object') cur = cur[seg.key];
      else return undefined;
    } else {
      if (Array.isArray(cur)) cur = cur[seg.index];
      else if (typeof cur === 'object') cur = cur[String(seg.index)];
      else return undefined;
    }
    if (cur === undefined) return undefined;
  }
  return cur;
}

/** Numeric view of a leaf: numbers as-is, bools → 0/1, numeric strings parsed. */
export function toF64(v) {
  if (typeof v === 'number') return Number.isFinite(v) ? v : null;
  if (typeof v === 'boolean') return v ? 1 : 0;
  if (typeof v === 'string') {
    const t = v.trim();
    if (!t) return null;
    if (!/^[-+]?(\d+\.?\d*|\.\d+)([eE][-+]?\d+)?$/.test(t) && !/^[-+]?(inf|infinity|nan)$/i.test(t)) return null;
    const n = Number(t);
    return Number.isFinite(n) ? n : null;
  }
  return null;
}

export function extractF64(v, path) {
  const x = extract(v, path);
  return x === undefined ? null : toF64(x);
}

/** Flatten every numeric leaf into [dottedPath, value] pairs; arrays indexed `a[0]`. */
export function flattenNumeric(v, prefix = '') {
  const out = [];
  const walk = (x, path, depth) => {
    if (out.length >= 4096 || depth > 32) return;
    if (Array.isArray(x)) {
      x.slice(0, 256).forEach((c, i) => walk(c, `${path}[${i}]`, depth + 1));
    } else if (x !== null && typeof x === 'object') {
      for (const [k, c] of Object.entries(x)) walk(c, path ? `${path}.${k}` : k, depth + 1);
    } else {
      const n = toF64(x);
      if (n !== null) out.push([path, n]);
    }
  };
  walk(v, prefix, 0);
  return out;
}

/** Reading channel name for a path: `.a.b[0]` → `a.b[0]`, `["weird key"].x` → `weird key.x`. */
export function channelName(path) {
  const segs = parsePath(path) || [];
  let s = '';
  for (const seg of segs) {
    if ('key' in seg) { if (s) s += '.'; s += seg.key; } else s += `[${seg.index}]`;
  }
  return s || String(path).trim().replace(/^[.$]+/, '');
}

// ---------------------------------------------------------------------------
// Converter
// ---------------------------------------------------------------------------
export function parseEventsOn(s) {
  switch (String(s).trim().toLowerCase()) {
    case 'change': case 'changed': return 'change';
    case 'every': case 'always': case 'all': return 'every';
    case 'none': case 'never': case 'off': return 'none';
    default: return null;
  }
}

/** Parse `fields`, `events_on`, `event_name`, `data_path`, `filter`, `each` from a spec. */
export function genericConfig(spec, defaultEventsOn, defaultEvent) {
  const list = paramStrList(spec, 'fields');
  let fields = 'auto';
  if (list && list.length && !(list.length === 1 && list[0].toLowerCase() === 'auto')) {
    for (const p of list) if (!parsePath(p)) throw new Error(`${spec.kind}: param \`fields\`: bad path \`${p}\``);
    fields = list;
  }
  const eo = spec.params?.events_on;
  let eventsOn = defaultEventsOn;
  if (typeof eo === 'string') {
    eventsOn = parseEventsOn(eo);
    if (!eventsOn) throw new Error(`${spec.kind}: param \`events_on\`: expected one of change|every|none`);
  }
  const eventName = paramStr(spec, 'event_name', defaultEvent);
  let dataPath = paramStr(spec, 'data_path', '').trim() || null;
  if (dataPath && !parsePath(dataPath)) throw new Error(`${spec.kind}: param \`data_path\`: bad path`);
  let filter = null;
  const f = spec.params?.filter;
  if (typeof f === 'string') {
    const idx = f.indexOf('==');
    if (idx < 0) throw new Error(`${spec.kind}: param \`filter\`: expected \`path==value\``);
    const p = f.slice(0, idx).trim();
    const raw = f.slice(idx + 2).trim();
    let val;
    try { val = JSON.parse(raw); } catch (_) { val = raw; }
    filter = [p, val];
  } else if (f && typeof f === 'object' && !Array.isArray(f)) {
    if (typeof f.path !== 'string') throw new Error(`${spec.kind}: param \`filter\`: missing \`path\``);
    if (!('value' in f)) throw new Error(`${spec.kind}: param \`filter\`: missing \`value\``);
    filter = [f.path, f.value];
  } else if (f !== undefined && f !== null) {
    throw new Error(`${spec.kind}: param \`filter\`: expected \`path==value\` or {path, value}`);
  }
  const each = paramStr(spec, 'each', '').trim() || null;
  if (each && !parsePath(each)) throw new Error(`${spec.kind}: param \`each\`: bad path`);
  return { fields, eventsOn, eventName, dataPath, filter, each };
}

/** Loose equality: numbers numerically, strings against the stringified other side. */
export function jsonEq(a, b) {
  if (a === b) return true;
  if (JSON.stringify(a) === JSON.stringify(b)) return true;
  const x = toF64(a), y = toF64(b);
  if (x !== null && y !== null) return Math.abs(x - y) < 1e-9;
  if (typeof a === 'string') return typeof b === 'string' ? a === b : (typeof b === 'boolean' || typeof b === 'number') ? a === String(b) : false;
  if (typeof b === 'string') return (typeof a === 'boolean' || typeof a === 'number') ? b === String(a) : false;
  return false;
}

/**
 * Flatten the scalar leaves of a JSON value into `{dottedKey: value}` pairs, in JSON order:
 * nested objects as `a.b`, arrays as `a[0]` (first `maxArray` elements), depth <= `maxDepth`
 * (0 = top level only), strings truncated to `maxStr` chars, numbers/bools as-is, null and
 * non-finite numbers skipped. Stops after `cap` leaves. Used by `attrsOf` and the MQTT adapter.
 */
export function flattenLeaves(v, { prefix = '', maxDepth = ATTR_MAX_DEPTH, maxArray = ATTR_MAX_ARRAY, maxStr = ATTR_MAX_STR, cap = MAX_ATTRS } = {}) {
  const out = {};
  let n = 0;
  const walk = (x, path, depth) => {
    if (n >= cap) return;
    if (Array.isArray(x)) {
      if (depth >= maxDepth) return;
      for (let i = 0; i < x.length && i < maxArray && n < cap; i++) walk(x[i], `${path}[${i}]`, depth + 1);
    } else if (x !== null && typeof x === 'object') {
      if (depth >= maxDepth) return;
      for (const [k, c] of Object.entries(x)) { if (n >= cap) break; walk(c, path ? `${path}.${k}` : k, depth + 1); }
    } else if (typeof x === 'number') { if (Number.isFinite(x)) { out[path] = x; n++; } }
    else if (typeof x === 'boolean') { out[path] = x; n++; }
    else if (typeof x === 'string') { out[path] = x.length > maxStr ? [...x].slice(0, maxStr).join('') : x; n++; }
  };
  walk(v, prefix, 0);
  return out;
}

/**
 * Event attributes of an object: every scalar leaf, nested ones flattened with dotted keys
 * (`data.p`, `data[0].px`), depth <= ATTR_MAX_DEPTH, <= ATTR_MAX_ARRAY array elements,
 * strings <= ATTR_MAX_STR chars, capped at MAX_ATTRS, JSON order. A bare scalar becomes
 * `value` (number/bool) or `text` (string). Mirrors generic.rs::attrs_of.
 */
export function attrsOf(obj) {
  if (obj === null || obj === undefined) return {};
  if (typeof obj === 'object') return flattenLeaves(obj);
  if (typeof obj === 'number') return Number.isFinite(obj) ? { value: obj } : {};
  if (typeof obj === 'string') return { text: [...obj].slice(0, ATTR_MAX_STR).join('') };
  if (typeof obj === 'boolean') return { value: obj };
  return {};
}

const sameValues = (a, b) => {
  if (!a || !b) return false;
  const ka = Object.keys(a), kb = Object.keys(b);
  if (ka.length !== kb.length) return false;
  for (const k of ka) if (b[k] !== a[k]) return false;
  return true;
};

/**
 * Stateful converter: remembers the previous reading for `events_on = change`.
 * Order per message: `data_path` → `filter` (on that object) → `each`: when the value at
 * that path is an array, every element is converted as its own object (its own reading and
 * event); otherwise the object is converted as a whole.
 */
export class Converter {
  constructor(cfg) { this.cfg = cfg; this.prev = null; }
  /** One incoming message → array of datum objects (may be empty). */
  convert(msg) {
    let obj = msg;
    if (this.cfg.dataPath) { obj = extract(msg, this.cfg.dataPath); if (obj === undefined) return []; }
    if (this.cfg.filter) {
      const got = extract(obj, this.cfg.filter[0]);
      if (got === undefined || !jsonEq(got, this.cfg.filter[1])) return [];
    }
    if (this.cfg.each) {
      const items = extract(obj, this.cfg.each);
      if (Array.isArray(items)) {
        const out = [];
        for (const it of items) out.push(...this.convertOne(it));
        return out;
      }
    }
    return this.convertOne(obj);
  }
  convertOne(obj) {
    const out = [];
    const values = this.readingOf(obj);
    const changed = !sameValues(this.prev, values);
    const emitEvent = this.cfg.eventsOn === 'every' ? true : this.cfg.eventsOn === 'change' ? (changed && this.prev !== null) : false;
    if (Object.keys(values).length) out.push({ type: 'reading', values: { ...values } });
    if (emitEvent) out.push({ type: 'event', name: this.cfg.eventName, intensity: 1.0, attrs: attrsOf(obj) });
    this.prev = values;
    return out;
  }
  /** Numeric channels of obj according to the field selection (sorted keys, like BTreeMap). */
  readingOf(obj) {
    const m = {};
    if (this.cfg.fields === 'auto') {
      const flat = flattenNumeric(obj, '');
      for (const [k, v] of flat.slice(0, MAX_FIELDS)) m[k] = v;
    } else {
      for (const p of this.cfg.fields.slice(0, MAX_FIELDS)) {
        const v = extractF64(obj, p);
        if (v !== null) m[channelName(p)] = v;
      }
    }
    return Object.fromEntries(Object.entries(m).sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0)));
  }
}
