// `http-json` — poll any JSON endpoint; numeric leaves become readings. Mirrors adapters/http_json.rs.
import { Emitter, Poller, getJson, paramSecs } from './util.js';
import { Converter, genericConfig } from './generic.js';

export function parseHeaders(spec) {
  const h = spec.params?.headers;
  if (h === undefined || h === null) return {};
  if (typeof h !== 'object' || Array.isArray(h)) throw new Error(`${spec.kind}: param \`headers\`: expected a map of header → value`);
  const out = {};
  for (const [k, v] of Object.entries(h)) {
    if (!k.trim()) throw new Error(`${spec.kind}: param \`headers\`: empty header name`);
    out[k] = typeof v === 'string' ? v : JSON.stringify(v);
  }
  return out;
}

export function requireUrl(spec, schemes) {
  const u = spec.params?.url;
  if (typeof u !== 'string' || !u.trim()) throw new Error(`${spec.kind}: missing required param \`url\``);
  const url = u.trim();
  if (!schemes.some((s) => url.startsWith(s))) throw new Error(`${spec.kind}: param \`url\`: expected ${schemes.map((s) => s.replace('://', '')).join('(s)')} URL`);
  return url;
}

export default class Adapter {
  constructor(spec, emit, log) {
    this.url = requireUrl(spec, ['http://', 'https://']);
    this.interval = paramSecs(spec, 'interval', 10, 1);
    this.headers = parseHeaders(spec);
    this.conv = new Converter(genericConfig(spec, 'change', 'update'));
    this.em = new Emitter(emit, log);
  }
  start() { this.poller = new Poller(this.em, this.interval); this.poller.run((s) => this.pollOnce(s)); }
  stop() { this.poller?.stop(); this.em.close(); }
  async pollOnce(signal) {
    const v = await getJson(this.url, { signal, headers: this.headers });
    const data = this.conv.convert(v);
    if (!data.length) throw 'no numeric fields in response (check `fields`/`data_path`)';
    const live = data.some((d) => d.type === 'event');
    for (const d of data) this.em.datum(d);
    return live || this.conv.cfg.eventsOn === 'none' ? 'live' : 'idle';
  }
}
