// `wikipedia.recent-changes` — the Wikimedia EventStreams SSE firehose. Mirrors adapters/wikipedia.rs.
// Uses EventSource (CORS-friendly, auto Last-Event-ID); our own loop supervises
// silence/idle and reconnects with backoff like the native source.
import { Attrs, Connector, Emitter, RateWindow, clamp01, jb, jf, js, nowSec, paramBool, paramStrList } from './util.js';

export const URL = 'https://stream.wikimedia.org/v2/stream/recentchange';

export function wikiConfig(spec) {
  const wikis = new Set(paramStrList(spec, 'wikis') || []);
  const includeBots = paramBool(spec, 'include_bots', false);
  const types = new Set((paramStrList(spec, 'types') || ['edit', 'new']).map((t) => t.toLowerCase()));
  for (const t of types) if (!['edit', 'new', 'log', 'categorize'].includes(t)) throw new Error(`wikipedia.recent-changes: param \`types\`: unknown type \`${t}\` (edit|new|log|categorize)`);
  return { wikis, includeBots, types };
}

export function accepts(cfg, rc) {
  const ty = js(rc, 'type') ?? '';
  if (!cfg.types.has(ty)) return false;
  if (!cfg.includeBots && (jb(rc, 'bot') ?? false)) return false;
  if (cfg.wikis.size) { const w = js(rc, 'wiki'); if (w === null || !cfg.wikis.has(w)) return false; }
  return true;
}

export function parseEdit(rc) {
  const ty = js(rc, 'type');
  if (ty === null) return null;
  const old = jf(rc?.length, 'old') ?? 0, nw = jf(rc?.length, 'new') ?? 0;
  const bytes = nw - old;
  return {
    wiki: js(rc, 'wiki') ?? '', bytes, bot: jb(rc, 'bot') ?? false, user: js(rc, 'user') ?? '',
    title: [...(js(rc, 'title') ?? '')].slice(0, 200).join(''), ty, minor: jb(rc, 'minor') ?? false,
    namespace: jf(rc, 'namespace') ?? 0, intensity: clamp01(Math.abs(bytes) / 2000),
  };
}

export const editAttrs = (e) => new Attrs()
  .text('wiki', e.wiki).num('bytes', e.bytes).bool('bot', e.bot).text('user', e.user).text('title', e.title)
  .text('type', e.ty).bool('minor', e.minor).num('namespace', e.namespace).intoMap();

export default class Adapter {
  constructor(spec, emit, log) {
    this.cfg = wikiConfig(spec);
    this.em = new Emitter(emit, log);
    this.window = new RateWindow();
    this.netBytes = 0;
  }
  start() { this.conn = new Connector(this.em); this.conn.run((signal) => this.streamOnce(signal)); }
  stop() { this.conn?.stop(); this.em.close(); }

  streamOnce(signal) {
    return new Promise((resolve, reject) => {
      if (typeof EventSource === 'undefined') return reject('EventSource unsupported');
      const es = new EventSource(URL);
      let lastData = performance.now();
      let opened = false;
      let nextReading = nowSec() + 2;
      const finish = (fn, v) => { clearInterval(timer); es.close(); signal.removeEventListener('abort', onAbort); fn(v); };
      const onAbort = () => finish(resolve);
      signal.addEventListener('abort', onAbort, { once: true });
      es.onopen = () => { opened = true; lastData = performance.now(); };
      es.onerror = () => {
        // EventSource retries by itself while CONNECTING; a CLOSED state means it gave up.
        if (es.readyState === EventSource.CLOSED) return finish(reject, opened ? 'stream ended' : 'connect failed (network or CORS)');
        if (!opened && performance.now() - lastData > 20000) return finish(reject, 'connect timeout');
      };
      es.onmessage = (ev) => {
        lastData = performance.now();
        let rc;
        try { rc = JSON.parse(ev.data); } catch (_) { return; }
        if (!accepts(this.cfg, rc)) return;
        const edit = parseEdit(rc);
        if (!edit) return;
        this.window.push(nowSec());
        this.netBytes += edit.bytes;
        this.em.status('live', 'live');
        this.em.event('edit', edit.intensity, editAttrs(edit));
      };
      const timer = setInterval(() => {
        const idleMs = performance.now() - lastData;
        if (idleMs > 90000) return finish(reject, 'no data for 90 s');
        const now = nowSec();
        if (now >= nextReading) {
          nextReading = now + 2;
          const rate = this.window.rate(now, 10);
          this.em.reading({ rate, net_bytes: this.netBytes });
          if (rate === 0 && idleMs > 20000) this.em.status('idle', 'quiet');
        }
      }, 500);
    });
  }
}
