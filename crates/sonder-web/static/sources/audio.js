// `audio.features` — low-level audio features of a media stream or the microphone
// (main thread: needs AudioContext + DOM). Mirrors adapters/audio.rs (which uses ffmpeg).
//
//   params: url      a media URL played through <audio crossorigin="anonymous">. Plain
//                    mp3/ogg icecast streams work (SomaFM allows CORS); HLS (.m3u8) only
//                    plays natively in Safari → status failed with a hint otherwise.
//           mic      true → getUserMedia (needs a user gesture: the “🎤 mic” button)
//           monitor  true → also play the stream through the speakers (default false)
//   readings (10 Hz): rms (0..1 over −60..0 dBFS), peak, centroid (Hz), flux, zcr,
//                     low / mid / high band energies (0..1)
//   events:  onset (flux over median + k·MAD of the last 2 s, 100 ms refractory;
//                   intensity = flux normalised by the window max; attr centroid01)
//            loud  (rms rising through 0.6; re-armed below 0.5)
//
// Feature math (`features`, `OnsetDetector`, `LoudDetector`) is pure and exported for tests.
import { Attrs, Emitter, clamp01, paramBool, paramStr } from './util.js';

export const FFT_SIZE = 2048;
export const ANALYSE_MS = 50;
export const READING_MS = 100;
export const MIN_DB = -100;             // AnalyserNode floor for getFloatFrequencyData
export const BAND_LOW_HZ = 250, BAND_HIGH_HZ = 2000;
export const CENTROID_FULL_HZ = 8000;   // centroid01 = centroid / 8 kHz
export const ONSET_K = 3, ONSET_WINDOW = 40, ONSET_REFRACTORY_MS = 100;
export const LOUD_ON = 0.6, LOUD_OFF = 0.5;
export const HLS_MESSAGE = 'HLS needs a server-side ffmpeg bridge (sonder play … audio.features) or a direct mp3/ogg stream';

/** dB → 0..1 over [−60, 0]. */
export const db01 = (db) => clamp01((db + 60) / 60);

/**
 * Pure feature extraction for one analysis frame.
 *   time:  Float32Array time-domain samples (−1..1)
 *   dbs:   Float32Array per-bin magnitudes in dBFS (as AnalyserNode.getFloatFrequencyData)
 *   prev:  Float32Array | null, previous frame's linear magnitudes (for flux); filled in place
 * Returns {rms, peak, centroid, flux, zcr, low, mid, high, mags} where mags are this frame's
 * linear magnitudes (0..1) to pass back as `prev` next time.
 */
export function features(time, dbs, sampleRate, prev = null) {
  const n = time.length;
  let sum = 0, peak = 0, zc = 0;
  for (let i = 0; i < n; i++) {
    const x = time[i];
    sum += x * x;
    const ax = Math.abs(x); if (ax > peak) peak = ax;
    if (i > 0 && ((x >= 0) !== (time[i - 1] >= 0))) zc++;
  }
  const rmsLin = Math.sqrt(sum / Math.max(1, n));
  const rms = rmsLin > 0 ? db01(20 * Math.log10(rmsLin)) : 0;
  const zcr = n > 1 ? zc / (n - 1) : 0;

  const bins = dbs.length;
  const binHz = sampleRate / 2 / Math.max(1, bins);
  const mags = new Float32Array(bins);
  let magSum = 0, weighted = 0, flux = 0;
  let lowP = 0, midP = 0, highP = 0, lowN = 0, midN = 0, highN = 0;
  for (let i = 0; i < bins; i++) {
    const db = Number.isFinite(dbs[i]) ? dbs[i] : MIN_DB;
    const m = db <= MIN_DB ? 0 : Math.pow(10, db / 20);
    mags[i] = m;
    magSum += m;
    const f = i * binHz;
    weighted += f * m;
    if (prev) { const d = m - prev[i]; if (d > 0) flux += d; }
    const p = m * m;
    if (f < BAND_LOW_HZ) { lowP += p; lowN++; } else if (f < BAND_HIGH_HZ) { midP += p; midN++; } else { highP += p; highN++; }
  }
  const centroid = magSum > 1e-9 ? weighted / magSum : 0;
  const band = (p, k) => (k > 0 && p > 0 ? db01(10 * Math.log10(p / k)) : 0);
  return {
    rms: clamp01(rms), peak: clamp01(peak), centroid, flux: prev ? flux / Math.max(1, bins) : 0, zcr,
    low: band(lowP, lowN), mid: band(midP, midN), high: band(highP, highN), mags,
  };
}

const median = (arr) => { const s = [...arr].sort((a, b) => a - b); const m = s.length >> 1; return s.length ? (s.length % 2 ? s[m] : (s[m - 1] + s[m]) / 2) : 0; };

/** Adaptive onset detector: flux > median + k·MAD over the last `window` frames; refractory. */
export class OnsetDetector {
  constructor({ k = ONSET_K, window = ONSET_WINDOW, refractoryMs = ONSET_REFRACTORY_MS } = {}) {
    this.k = k; this.window = window; this.refractoryMs = refractoryMs; this.hist = []; this.lastOnsetMs = -Infinity;
  }
  /** → intensity (0..1) when this frame is an onset, else null. `nowMs` is any monotonic clock. */
  push(flux, nowMs) {
    let hit = null;
    if (this.hist.length >= 8) {
      const med = median(this.hist);
      const mad = median(this.hist.map((x) => Math.abs(x - med)));
      const thresh = med + this.k * Math.max(mad, 1e-6);
      if (flux > thresh && nowMs - this.lastOnsetMs >= this.refractoryMs) {
        this.lastOnsetMs = nowMs;
        const max = Math.max(flux, ...this.hist);
        hit = clamp01(max > 0 ? flux / max : 0);
      }
    }
    this.hist.push(flux);
    while (this.hist.length > this.window) this.hist.shift();
    return hit;
  }
}

/** `loud`: rms rising through LOUD_ON; re-armed when it falls below LOUD_OFF. */
export class LoudDetector {
  constructor(on = LOUD_ON, off = LOUD_OFF) { this.on = on; this.off = off; this.armed = true; }
  push(rms) {
    if (this.armed && rms >= this.on) { this.armed = false; return true; }
    if (!this.armed && rms < this.off) this.armed = true;
    return false;
  }
}

/** Is this an HLS playlist URL the browser cannot play natively? */
export function hlsUnsupported(url, canPlay = (t) => (typeof document !== 'undefined' ? document.createElement('audio').canPlayType(t) : '')) {
  if (!/\.m3u8(\?|#|$)/i.test(url)) return false;
  return !canPlay('application/vnd.apple.mpegurl') && !canPlay('application/x-mpegURL');
}

export default class Adapter {
  /** ctx: { onChange?(on: boolean) } */
  constructor(spec, emit, log, ctx = {}) {
    this.spec = spec;
    this.em = new Emitter(emit, log);
    this.url = (paramStr(spec, 'url', '') || '').trim() || null;
    this.mic = paramBool(spec, 'mic', false);
    this.monitor = paramBool(spec, 'monitor', false);
    if (!this.url && !this.mic) throw new Error(`${spec.kind}: set \`url\` (a media URL) or \`mic = true\``);
    if (this.url && !/^https?:\/\//i.test(this.url)) throw new Error(`${spec.kind}: param \`url\`: expected http(s) URL`);
    this.onChange = ctx.onChange || (() => {});
    this.enabled = false;
    this.needsGesture = false;
    this.ctx = null; this.analyser = null; this.node = null; this.stream = null; this.audio = null;
    this.prev = null; this.onsets = new OnsetDetector(); this.loud = new LoudDetector();
    this.last = null; this.silentSince = null;
  }
  /** Button label for the UI. */
  get label() { return this.mic ? '🎤 mic' : '🔊 stream'; }

  start() {
    if (typeof window === 'undefined' || typeof (window.AudioContext || window.webkitAudioContext) === 'undefined') {
      this.em.statusForce('failed', 'audio analysis needs a browser with Web Audio'); return;
    }
    if (this.mic) {
      if (!navigator.mediaDevices?.getUserMedia) { this.em.statusForce('failed', 'microphone unavailable (needs https and getUserMedia)'); return; }
      this.em.statusForce('idle', 'click “🎤 mic” to start listening');
      return;
    }
    if (hlsUnsupported(this.url)) { this.em.statusForce('failed', HLS_MESSAGE); return; }
    // A stream can usually start without a gesture once the page has one (the Listen button);
    // if autoplay is blocked we fall back to the button.
    this.enable();
  }

  async enable() {
    if (this.enabled) return;
    this.em.statusForce('connecting', this.mic ? 'asking for the microphone…' : `opening ${this.url}`);
    try {
      const AC = window.AudioContext || window.webkitAudioContext;
      this.ctx = this.ctx || new AC();
      if (this.ctx.state === 'suspended') await this.ctx.resume();
      this.analyser = this.ctx.createAnalyser();
      this.analyser.fftSize = FFT_SIZE;
      this.analyser.smoothingTimeConstant = 0;
      if (this.mic) {
        this.stream = await navigator.mediaDevices.getUserMedia({ audio: { echoCancellation: false, noiseSuppression: false, autoGainControl: false }, video: false });
        this.node = this.ctx.createMediaStreamSource(this.stream);
      } else {
        const audio = document.createElement('audio');
        audio.crossOrigin = 'anonymous';
        audio.preload = 'auto';
        audio.src = this.url;
        this.audio = audio;
        await new Promise((res, rej) => {
          const onErr = () => rej(new Error(audio.error?.code === 4 ? 'stream not playable (format unsupported, offline, or no CORS header)' : `media error ${audio.error?.code ?? '?'}`));
          audio.addEventListener('error', onErr, { once: true });
          audio.play().then(res, (e) => { if (e && e.name === 'NotAllowedError') { this.needsGesture = true; } rej(e); });
        });
        this.node = this.ctx.createMediaElementSource(audio);
      }
      this.node.connect(this.analyser);
      if (this.monitor && !this.mic) this.analyser.connect(this.ctx.destination);
      this.time = new Float32Array(this.analyser.fftSize);
      this.dbs = new Float32Array(this.analyser.frequencyBinCount);
      this.prev = null; this.silentSince = null; this.lastReading = 0;
      this.enabled = true;
      this.needsGesture = false;
      this.em.statusForce('live', this.mic ? 'microphone live' : 'stream live');
      this.onChange(true);
      this.timer = setInterval(() => this.analyse(), ANALYSE_MS);
    } catch (e) {
      this.teardown();
      const msg = e && e.name === 'NotAllowedError' ? (this.mic ? 'permission denied' : 'autoplay blocked — click “🔊 stream”')
        : (e && e.message) || String(e);
      this.em.statusForce(this.needsGesture ? 'idle' : 'failed', `${this.mic ? 'microphone' : 'stream'}: ${msg}`);
      this.onChange(false);
    }
  }

  analyse() {
    if (!this.enabled) return;
    const nowMs = performance.now();
    this.analyser.getFloatTimeDomainData(this.time);
    this.analyser.getFloatFrequencyData(this.dbs);
    const f = features(this.time, this.dbs, this.ctx.sampleRate, this.prev);
    this.prev = f.mags;
    this.last = f;
    const onset = this.onsets.push(f.flux, nowMs);
    if (onset !== null) this.em.event('onset', onset, new Attrs().num('centroid01', clamp01(f.centroid / CENTROID_FULL_HZ)));
    if (this.loud.push(f.rms)) this.em.event('loud', f.rms, new Attrs().num('rms', f.rms).num('centroid01', clamp01(f.centroid / CENTROID_FULL_HZ)));
    if (nowMs - this.lastReading >= READING_MS - 1) {
      this.lastReading = nowMs;
      this.em.reading({ rms: f.rms, peak: f.peak, centroid: f.centroid, flux: f.flux, zcr: f.zcr, low: f.low, mid: f.mid, high: f.high });
    }
    // A CORS-blocked <audio> plays audibly nowhere and feeds the graph silence: say so.
    if (!this.mic) {
      if (f.peak > 0) this.silentSince = null;
      else if (this.silentSince === null) this.silentSince = nowMs;
      else if (nowMs - this.silentSince > 8000) { this.silentSince = nowMs; this.em.statusForce('idle', 'stream is silent (no CORS header on the stream blocks analysis, or it is quiet)'); }
    }
  }

  teardown() {
    clearInterval(this.timer);
    this.enabled = false;
    if (this.stream) for (const t of this.stream.getTracks()) t.stop();
    this.stream = null;
    if (this.audio) { try { this.audio.pause(); this.audio.src = ''; } catch (_) { /* ignore */ } this.audio = null; }
    try { this.node?.disconnect(); } catch (_) { /* ignore */ }
    try { this.analyser?.disconnect(); } catch (_) { /* ignore */ }
    this.node = null; this.analyser = null;
  }
  disable() {
    if (!this.enabled) return;
    this.teardown();
    this.em.statusForce('idle', this.mic ? 'microphone stopped' : 'stream stopped');
    this.onChange(false);
  }
  stop() { this.teardown(); if (this.ctx) { this.ctx.close().catch(() => {}); this.ctx = null; } this.em.close(); }
}
