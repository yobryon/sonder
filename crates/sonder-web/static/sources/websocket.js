// `websocket` — generic WebSocket source; each JSON text message becomes a reading/event. Mirrors adapters/websocket.rs.
import { Connector, Emitter, paramSecs } from './util.js';
import { Converter, genericConfig } from './generic.js';
import { requireUrl } from './http_json.js';

export default class Adapter {
  constructor(spec, emit, log) {
    this.url = requireUrl(spec, ['ws://', 'wss://']);
    const sub = spec.params?.subscribe;
    this.subscribe = sub === undefined || sub === null ? null : typeof sub === 'string' ? sub : JSON.stringify(sub);
    this.conv = new Converter(genericConfig(spec, 'every', 'message'));
    this.ping = paramSecs(spec, 'ping', 30, 5); // browsers cannot send ping frames; kept for parity
    this.em = new Emitter(emit, log);
  }
  start() { this.conn = new Connector(this.em); this.conn.run((signal) => this.session(signal)); }
  stop() { this.conn?.stop(); this.em.close(); }
  session(signal) {
    return new Promise((resolve, reject) => {
      let ws;
      try { ws = new WebSocket(this.url); } catch (e) { return reject(`bad url: ${e.message || e}`); }
      let lastMsg = performance.now();
      let opened = false;
      const finish = (fn, v) => { clearInterval(timer); signal.removeEventListener('abort', onAbort); try { ws.close(); } catch (_) { /* ignore */ } fn(v); };
      const onAbort = () => finish(resolve);
      signal.addEventListener('abort', onAbort, { once: true });
      ws.onopen = () => {
        opened = true; lastMsg = performance.now();
        if (this.subscribe !== null) ws.send(this.subscribe);
        this.em.status('live', 'connected');
      };
      ws.onclose = () => finish(reject, opened ? 'server closed' : 'connect failed (network or CORS)');
      ws.onerror = () => { /* onclose follows */ };
      ws.onmessage = async (ev) => {
        lastMsg = performance.now();
        let text = ev.data;
        if (text instanceof Blob) text = await text.text();
        else if (text instanceof ArrayBuffer) text = new TextDecoder().decode(text);
        if (typeof text !== 'string') return;
        let v;
        try { v = JSON.parse(text); } catch (_) { return; }
        if (typeof ev.data === 'string') this.em.status('live', 'live');
        for (const d of this.conv.convert(v)) this.em.datum(d);
      };
      const timer = setInterval(() => {
        if (!opened && performance.now() - lastMsg > 20000) return finish(reject, 'connect timeout');
        if (performance.now() - lastMsg > 120000) return finish(reject, 'no messages for 2 min');
      }, 500);
    });
  }
}
