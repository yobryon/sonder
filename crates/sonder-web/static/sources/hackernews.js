// `hackernews.firehose` — every new HN item as an `item` event. Mirrors adapters/hackernews.rs.
import { Attrs, Emitter, Poller, RateWindow, getJson, jf, js, nowSec, paramF64, paramSecs } from './util.js';

export const MAXITEM_URL = 'https://hacker-news.firebaseio.com/v0/maxitem.json';
export const itemUrl = (id) => `https://hacker-news.firebaseio.com/v0/item/${id}.json`;

export function parseItem(v) {
  if (!v || typeof v !== 'object' || !Number.isInteger(v.id) || v.id < 0) return null;
  if (v.deleted === true || v.dead === true) return null;
  const text = js(v, 'text');
  return {
    id: v.id, ty: js(v, 'type') ?? 'unknown', by: js(v, 'by') ?? '', score: jf(v, 'score') ?? 0,
    title: [...(js(v, 'title') ?? '')].slice(0, 200).join(''),
    text_len: text === null ? 0 : new TextEncoder().encode(text).length, // Rust `str::len` is bytes
    descendants: jf(v, 'descendants') ?? 0,
  };
}
export function itemIntensity(it) {
  switch (it.ty) { case 'story': return 0.8; case 'job': return 0.6; case 'poll': return 0.7; case 'comment': return 0.3; default: return 0.4; }
}
export const itemAttrs = (it) => new Attrs().num('id', it.id).text('type', it.ty).num('score', it.score).text('by', it.by)
  .text('title', it.title).num('text_len', it.text_len).num('descendants', it.descendants).intoMap();

export default class Adapter {
  constructor(spec, emit, log) {
    this.interval = paramSecs(spec, 'interval', 20, 5);
    const cap = paramF64(spec, 'max_per_poll', 30);
    if (!(cap >= 1 && cap <= 200)) throw new Error(`hackernews.firehose: param \`max_per_poll\`: expected a number in 1..=200, got ${cap}`);
    this.cap = Math.trunc(cap);
    this.em = new Emitter(emit, log);
    this.lastMax = null;
    this.window = new RateWindow();
  }
  start() { this.poller = new Poller(this.em, this.interval); this.poller.run((s) => this.pollOnce(s)); }
  stop() { this.poller?.stop(); this.em.close(); }
  async pollOnce(signal) {
    const v = await getJson(MAXITEM_URL, { signal });
    if (!Number.isInteger(v) || v < 0) throw 'maxitem is not an integer';
    const max = v;
    const start = this.lastMax === null ? Math.max(0, max - 3) : this.lastMax + 1;
    let ids = [];
    for (let i = start; i <= max; i++) ids.push(i);
    const skipped = Math.max(0, ids.length - this.cap);
    if (skipped > 0) ids = ids.slice(skipped);
    // Fetch in parallel, emit in id order (the native source fetches sequentially).
    const results = await Promise.allSettled(ids.map((id) => getJson(itemUrl(id), { signal })));
    let fetched = 0;
    results.forEach((r, i) => {
      if (r.status !== 'fulfilled') { this.em.log(`item ${ids[i]} fetch failed: ${r.reason}`); return; }
      const item = parseItem(r.value);
      if (!item) return;
      fetched++;
      this.window.push(nowSec());
      this.em.event('item', itemIntensity(item), itemAttrs(item));
    });
    this.lastMax = max;
    this.em.reading([['rate', this.window.rate(nowSec(), 60)], ['new_items', ids.length], ['skipped', skipped], ['max_id', max]]);
    return fetched > 0 ? 'live' : 'idle';
  }
}
