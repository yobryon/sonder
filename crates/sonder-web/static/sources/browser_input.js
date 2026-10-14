// `browser.input` — the person at the keyboard as a data source (main thread; DOM events).
// Channel/event names are what scenes/this-browser.toml reads:
//   readings (4 Hz): mouse_speed  0..~1  (px/ms over the last 2 s, so 1 ≈ a brisk swipe)
//                    visible      0/1    (tab visibility)
//                    scroll       0..1   (scrollY / (scrollHeight - innerHeight); 0 when nothing scrolls)
//   events:          click {x, y}       (0..1 of the viewport)
//                    key   {key}        (text attr: the key name, e.g. "a", "Enter")
import { Attrs, Emitter, clamp01, nowSec } from './util.js';

export const SPEED_WINDOW_SEC = 2;

/** Pure: px moved within the window → normalized speed (px/ms). */
export const speedOf = (pxInWindow, windowSec = SPEED_WINDOW_SEC) => pxInWindow / (windowSec * 1000);

/** Pure: document scroll fraction. */
export function scrollFraction(scrollY, scrollHeight, innerHeight) {
  const range = scrollHeight - innerHeight;
  return range > 0 ? clamp01(scrollY / range) : 0;
}

export default class Adapter {
  constructor(spec, emit, log, ctx = {}) {
    this.spec = spec;
    this.em = new Emitter(emit, log);
    this.target = ctx.target || (typeof window !== 'undefined' ? window : null);
    this.mouse = null;     // {x, y}
    this.dist = [];        // [at, px]
    this.handlers = [];
  }
  on(type, fn, opts) { this.target.addEventListener(type, fn, opts); this.handlers.push([type, fn, opts]); }
  start() {
    if (!this.target || typeof document === 'undefined') { this.em.statusForce('failed', 'needs a browser window'); return; }
    this.em.status('live', 'listening to mouse, clicks, scroll and keys');
    this.on('pointermove', (e) => {
      if (this.mouse) this.dist.push([nowSec(), Math.hypot(e.clientX - this.mouse.x, e.clientY - this.mouse.y)]);
      this.mouse = { x: e.clientX, y: e.clientY };
    }, { passive: true });
    this.on('pointerdown', (e) => {
      const w = Math.max(1, window.innerWidth), h = Math.max(1, window.innerHeight);
      this.em.event('click', 0.6, new Attrs().num('x', clamp01(e.clientX / w)).num('y', clamp01(e.clientY / h)));
    }, { passive: true });
    this.on('keydown', (e) => {
      if (e.repeat || e.metaKey || e.ctrlKey || e.altKey) return;
      this.em.event('key', 0.5, new Attrs().text('key', e.key));
    });
    this.timer = setInterval(() => this.tick(), 250);
    this.tick();
  }
  tick() {
    const now = nowSec();
    this.dist = this.dist.filter(([t]) => now - t <= SPEED_WINDOW_SEC);
    const px = this.dist.reduce((s, [, d]) => s + d, 0);
    const doc = document.documentElement;
    this.em.reading({
      mouse_speed: speedOf(px),
      visible: document.visibilityState === 'visible' ? 1 : 0,
      scroll: scrollFraction(window.scrollY || doc.scrollTop || 0, doc.scrollHeight, window.innerHeight),
    });
  }
  stop() {
    clearInterval(this.timer);
    for (const [t, fn, o] of this.handlers) this.target.removeEventListener(t, fn, o);
    this.handlers = [];
    this.em.close();
  }
}
