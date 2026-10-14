// `coinbase.trades` — live matches from the Coinbase Exchange WebSocket feed. Mirrors adapters/coinbase.rs.
import { Attrs, Connector, Emitter, RateWindow, clamp01, jf, js, nowSec, paramStrList } from './util.js';

export const URL = 'wss://ws-feed.exchange.coinbase.com';

export const tradeIntensity = (t) => clamp01(Math.log10(1 + t.usd) / 5);
export const tradeAttrs = (t) => new Attrs()
  .text('product', t.product).text('side', t.side).num('price', t.price).num('size', t.size).num('usd', t.usd)
  .bool('buy', t.side === 'buy').intoMap();

/** Parse a `match`/`last_match` message; other message types → null. */
export function parseMatch(v) {
  const ty = js(v, 'type');
  if (ty !== 'match' && ty !== 'last_match') return null;
  const price = jf(v, 'price'), size = jf(v, 'size');
  if (price === null || size === null) return null;
  return { product: js(v, 'product_id') ?? '', side: js(v, 'side') ?? '', price, size, usd: price * size };
}

export const subscribeMsg = (products) => ({ type: 'subscribe', product_ids: products, channels: ['matches', 'heartbeat'] });

/** Rolling stats for the 1 Hz readings. */
export class TradeStats {
  constructor() { this.lastPrice = null; this.prices = []; this.trades = new RateWindow(); this.buys = new RateWindow(); this.volume = []; }
  push(at, t) {
    this.lastPrice = t.price;
    this.prices.push([at, t.price]);
    this.trades.push(at);
    if (t.side === 'buy') this.buys.push(at);
    this.volume.push([at, t.usd]);
    while (this.prices.length > 50000) this.prices.shift();
    while (this.volume.length > 50000) this.volume.shift();
  }
  readings(now) {
    while (this.prices.length && now - this.prices[0][0] > 60) this.prices.shift();
    while (this.volume.length && now - this.volume[0][0] > 10) this.volume.shift();
    const price = this.lastPrice ?? 0;
    const oldest = this.prices.length ? this.prices[0][1] : price;
    const change = oldest > 0 ? (price - oldest) / oldest * 100 : 0;
    const n = this.trades.count(now, 10), b = this.buys.count(now, 10);
    const usd = this.volume.reduce((s, [, u]) => s + u, 0) / 10;
    return [['price', price], ['price_change', change], ['rate', n / 10], ['buy_ratio', n > 0 ? b / n : 0.5], ['usd_rate', usd]];
  }
}

export default class Adapter {
  constructor(spec, emit, log) {
    let products = (paramStrList(spec, 'products') || []).map((p) => p.toUpperCase());
    if (!products.length) products = ['BTC-USD'];
    for (const p of products) if (!p.includes('-')) throw new Error(`coinbase.trades: param \`products\`: \`${p}\` is not a product id like BTC-USD`);
    this.products = products;
    this.em = new Emitter(emit, log);
    this.stats = new TradeStats();
  }
  start() { this.conn = new Connector(this.em); this.conn.run((signal) => this.session(signal)); }
  stop() { this.conn?.stop(); this.em.close(); }

  session(signal) {
    return new Promise((resolve, reject) => {
      let ws;
      try { ws = new WebSocket(URL); } catch (e) { return reject(`connect failed: ${e.message || e}`); }
      let lastMsg = performance.now();
      let nextReading = nowSec() + 1;
      let opened = false;
      const finish = (fn, v) => { clearInterval(timer); signal.removeEventListener('abort', onAbort); try { ws.close(); } catch (_) { /* ignore */ } fn(v); };
      const onAbort = () => finish(resolve);
      signal.addEventListener('abort', onAbort, { once: true });
      ws.onopen = () => { opened = true; lastMsg = performance.now(); ws.send(JSON.stringify(subscribeMsg(this.products))); };
      ws.onerror = () => { /* onclose follows */ };
      ws.onclose = () => finish(reject, opened ? 'server closed' : 'connect failed (network or CORS)');
      ws.onmessage = (ev) => {
        lastMsg = performance.now();
        if (typeof ev.data !== 'string') return;
        let v;
        try { v = JSON.parse(ev.data); } catch (_) { return; }
        const ty = js(v, 'type');
        if (ty === 'error') return finish(reject, `feed error: ${js(v, 'message') ?? '?'}`);
        if (ty === 'subscriptions') { this.em.status('live', 'subscribed'); return; }
        const t = parseMatch(v);
        if (t) {
          this.stats.push(nowSec(), t);
          this.em.status('live', 'live');
          this.em.event('trade', tradeIntensity(t), tradeAttrs(t));
        }
      };
      const timer = setInterval(() => {
        if (!opened && performance.now() - lastMsg > 20000) return finish(reject, 'connect timeout');
        if (performance.now() - lastMsg > 60000) return finish(reject, 'no messages for 60 s');
        const now = nowSec();
        if (now >= nextReading) {
          nextReading = now + 1;
          if (this.stats.lastPrice !== null) this.em.reading(this.stats.readings(now));
        }
      }, 250);
    });
  }
}
