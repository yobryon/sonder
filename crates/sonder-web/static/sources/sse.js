// `sse` — generic Server-Sent Events source; each `data:` JSON becomes a reading/event. Mirrors adapters/sse.rs.
import { Connector, Emitter, paramStr, streamSse } from './util.js';
import { Converter, genericConfig } from './generic.js';
import { parseHeaders, requireUrl } from './http_json.js';

export default class Adapter {
  constructor(spec, emit, log) {
    this.url = requireUrl(spec, ['http://', 'https://']);
    this.headers = parseHeaders(spec);
    this.conv = new Converter(genericConfig(spec, 'every', 'message'));
    this.eventFilter = paramStr(spec, 'sse_event', null);
    this.em = new Emitter(emit, log);
    this.lastId = null;
  }
  start() { this.conn = new Connector(this.em); this.conn.run((signal) => this.streamOnce(signal)); }
  stop() { this.conn?.stop(); this.em.close(); }
  async streamOnce(signal) {
    let lastData = performance.now();
    const idle = setInterval(() => {
      if (performance.now() - lastData > 30000) this.em.status('idle', 'quiet');
    }, 5000);
    try {
      await streamSse(this.url, {
        headers: this.headers, lastId: this.lastId, signal, idleTimeoutMs: 300000,
        onOpen: () => this.em.status('live', 'connected'),
        onEvent: (ev) => {
          lastData = performance.now();
          if (ev.id !== null) this.lastId = ev.id;
          if (this.eventFilter !== null && (ev.event ?? 'message') !== this.eventFilter) return;
          let v;
          try { v = JSON.parse(ev.data); } catch (_) { return; }
          this.em.status('live', 'live');
          for (const d of this.conv.convert(v)) this.em.datum(d);
        },
      });
    } finally { clearInterval(idle); }
  }
}
