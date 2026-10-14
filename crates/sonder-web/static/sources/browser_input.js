// `browser.input` — the person at the keyboard as a data source (main thread; DOM events).
// Readings at 4 Hz:  mouse_speed (px/s), mouse_speed01, mouse_x, mouse_y (0..1), click_rate,
//                    key_rate, scroll_rate (px/s), visible (0/1)
// Events:            click {x, y, button}, key {key, code}, scroll {delta}, visibility {visible}
import { Attrs, Emitter, RateWindow, clamp01, nowSec } from './util.js';

export default class Adapter {
  constructor(spec, emit, log, ctx = {}) {
    this.spec = spec;
    this.em = new Emitter(emit, log);
    this.target = ctx.target || (typeof window !== 'undefined' ? window : null);
    this.clicks = new RateWindow();
    this.keys = new RateWindow();
    this.scroll = [];      // [at, |delta|]
    this.mouse = null;     // {x, y, t}
    this.dist = [];        // [at, px]
    this.handlers = [];
  }
  on(type, fn, opts) { this.target.addEventListener(type, fn, opts); this.handlers.push([type, fn, opts]); }
  start() {
    if (!this.target || typeof document === 'undefined') { this.em.statusForce('failed', 'needs a browser window'); return; }
    this.em.status('live', 'listening to mouse, keys and scroll');
    this.on('pointermove', (e) => {
      const t = nowSec();
      if (this.mouse) {
        const dx = e.clientX - this.mouse.x, dy = e.clientY - this.mouse.y;
        this.dist.push([t, Math.hypot(dx, dy)]);
      }
      this.mouse = { x: e.clientX, y: e.clientY, t };
    }, { passive: true });
    this.on('pointerdown', (e) => {
      const t = nowSec();
      this.clicks.push(t);
      const w = Math.max(1, window.innerWidth), h = Math.max(1, window.innerHeight);
      this.em.event('click', 0.6, new Attrs().num('x', clamp01(e.clientX / w)).num('y', clamp01(e.clientY / h)).num('button', e.button));
    }, { passive: true });
    this.on('keydown', (e) => {
      if (e.repeat) return;
      this.keys.push(nowSec());
      this.em.event('key', 0.4, new Attrs().text('key', e.key.length === 1 ? e.key : '').text('code', e.code));
    });
    this.on('wheel', (e) => {
      const d = Math.abs(e.deltaY) + Math.abs(e.deltaX);
      this.scroll.push([nowSec(), d]);
      this.em.event('scroll', clamp01(d / 500), new Attrs().num('delta', e.deltaY));
    }, { passive: true });
    document.addEventListener('visibilitychange', this.onVis = () => {
      const visible = document.visibilityState === 'visible';
      this.em.event('visibility', visible ? 0.5 : 0.8, new Attrs().bool('visible', visible));
    });
    this.timer = setInterval(() => this.tick(), 250);
  }
  tick() {
    const now = nowSec();
    const win = 2;
    this.dist = this.dist.filter(([t]) => now - t <= win);
    this.scroll = this.scroll.filter(([t]) => now - t <= win);
    const speed = this.dist.reduce((s, [, d]) => s + d, 0) / win;
    const scroll = this.scroll.reduce((s, [, d]) => s + d, 0) / win;
    const w = Math.max(1, window.innerWidth), h = Math.max(1, window.innerHeight);
    this.em.reading({
      mouse_speed: speed, mouse_speed01: clamp01(speed / 1500),
      mouse_x: this.mouse ? clamp01(this.mouse.x / w) : 0.5, mouse_y: this.mouse ? clamp01(this.mouse.y / h) : 0.5,
      click_rate: this.clicks.rate(now, 5), key_rate: this.keys.rate(now, 5), scroll_rate: scroll,
      visible: document.visibilityState === 'visible' ? 1 : 0,
    });
  }
  stop() {
    clearInterval(this.timer);
    for (const [t, fn, o] of this.handlers) this.target.removeEventListener(t, fn, o);
    if (this.onVis) document.removeEventListener('visibilitychange', this.onVis);
    this.handlers = [];
    this.em.close();
  }
}
