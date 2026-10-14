// `camera.webcam` (also used for scene kind `camera.mjpeg`) — main thread. Captures the
// webcam with getUserMedia, downsamples to <= 160 px wide, converts to 8-bit luma and
// posts frames (<= fps_cap, default 15) to the worker, where `Engine.push_frame` runs
// the pure-Rust tracker and emits the same blob/crossing events, readings and viz as
// the native camera source. Needs a user gesture: `start()` only prepares; the UI's
// camera button calls `enable()`.
import { Emitter, paramF64 } from './util.js';

export const MAX_WIDTH = 160;

/** RGBA → 8-bit luma (BT.601 integer weights). Exported for tests. */
export function rgbaToGray(rgba, out) {
  const n = rgba.length >> 2;
  for (let i = 0, j = 0; i < n; i++, j += 4) out[i] = (rgba[j] * 77 + rgba[j + 1] * 150 + rgba[j + 2] * 29) >> 8;
  return out;
}

/** Target capture size for a video of w×h: width <= MAX_WIDTH, aspect preserved, even numbers. */
export function captureSize(w, h) {
  const scale = Math.min(1, MAX_WIDTH / Math.max(1, w));
  const cw = Math.max(2, Math.round(w * scale / 2) * 2);
  const ch = Math.max(2, Math.round(h * scale / 2) * 2);
  return [cw, ch];
}

export default class Adapter {
  /** ctx: { postFrame(gray: Uint8Array, w, h), stage?: HTMLElement } */
  constructor(spec, emit, log, ctx = {}) {
    this.spec = spec;
    this.em = new Emitter(emit, log);
    this.postFrame = ctx.postFrame || (() => {});
    this.stage = ctx.stage || null;
    this.fpsCap = Math.min(30, paramF64(spec, 'fps_cap', 15));
    this.enabled = false;
    this.stream = null;
    this.video = null;
    this.canvas = null;
    this.lastFrame = 0;
    this.raf = 0;
    this.onChange = ctx.onChange || (() => {});
  }
  start() {
    if (typeof navigator === 'undefined' || !navigator.mediaDevices?.getUserMedia) {
      this.em.statusForce('failed', 'webcam unavailable (needs https and getUserMedia)');
      return;
    }
    this.em.statusForce('idle', 'click “camera” to start the webcam');
  }
  async enable() {
    if (this.enabled) return;
    this.em.statusForce('connecting', 'asking for the webcam…');
    try {
      let stream;
      try {
        stream = await navigator.mediaDevices.getUserMedia({ video: { facingMode: { ideal: 'environment' }, width: { ideal: 640 }, height: { ideal: 480 } }, audio: false });
      } catch (_) {
        stream = await navigator.mediaDevices.getUserMedia({ video: true, audio: false });
      }
      this.stream = stream;
      const video = document.createElement('video');
      video.className = 'campreview';
      video.muted = true; video.playsInline = true; video.autoplay = true;
      video.srcObject = stream;
      (this.stage || document.body).appendChild(video);
      await video.play();
      this.video = video;
      this.canvas = document.createElement('canvas');
      this.g = this.canvas.getContext('2d', { willReadFrequently: true });
      this.enabled = true;
      this.em.statusForce('live', 'webcam live');
      this.onChange(true);
      this.loop();
    } catch (e) {
      this.em.statusForce('failed', `webcam: ${e && e.name === 'NotAllowedError' ? 'permission denied' : (e && e.message) || e}`);
      this.onChange(false);
    }
  }
  loop() {
    if (!this.enabled) return;
    const minGap = 1000 / Math.max(0.1, this.fpsCap);
    const now = performance.now();
    if (now - this.lastFrame >= minGap && this.video.readyState >= 2 && this.video.videoWidth > 0) {
      this.lastFrame = now;
      const [w, h] = captureSize(this.video.videoWidth, this.video.videoHeight);
      if (this.canvas.width !== w || this.canvas.height !== h) { this.canvas.width = w; this.canvas.height = h; }
      this.g.drawImage(this.video, 0, 0, w, h);
      const rgba = this.g.getImageData(0, 0, w, h).data;
      const gray = rgbaToGray(rgba, new Uint8Array(w * h));
      this.postFrame(gray, w, h);
    }
    if (typeof this.video.requestVideoFrameCallback === 'function') this.video.requestVideoFrameCallback(() => this.loop());
    else this.raf = requestAnimationFrame(() => this.loop());
  }
  disable() {
    if (!this.enabled) return;
    this.enabled = false;
    cancelAnimationFrame(this.raf);
    if (this.stream) for (const t of this.stream.getTracks()) t.stop();
    this.stream = null;
    if (this.video) { this.video.remove(); this.video = null; }
    this.em.statusForce('idle', 'webcam stopped');
    this.onChange(false);
  }
  stop() { this.disable(); this.em.close(); }
}
