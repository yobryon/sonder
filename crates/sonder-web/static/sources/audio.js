// `audio.features` — low-level audio features of a media stream or the microphone
// (main thread: needs AudioContext + DOM). Mirrors adapters/audio.rs (which uses ffmpeg).
//
//   params: url      a media URL. mp3 (icecast/shoutcast) streams are fetch()ed, split at
//                    frame syncs and decoded with decodeAudioData (no <audio> element: Chrome's
//                    media element sends Range requests that SomaFM answers with a redirect,
//                    and it never reaches the graph on some setups). Other codecs (ogg, aac)
//                    go through <audio crossorigin="anonymous">. HLS (.m3u8) only plays natively
//                    in Safari → status failed with a hint otherwise. The stream must allow
//                    CORS (SomaFM does: https://ice1.somafm.com/groovesalad-128-mp3).
//           mic      true → getUserMedia (needs a user gesture: the “🎤 mic” button)
//           monitor  true → also play the stream through the speakers (default false)
//   readings (10 Hz): rms (0..1 over −60..0 dBFS), peak, centroid (Hz), flux (0..1: share of
//                     the spectrum's magnitude that is new since the last frame), zcr,
//                     low / mid / high band energies (0..1; band power in dBFS over −60..0)
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
export const ONSET_K = 4, ONSET_WINDOW = 40, ONSET_REFRACTORY_MS = 100;
export const SMOOTHING = 0.5;           // AnalyserNode spectral smoothing: 50 ms hops do not overlap at fftSize 2048
export const ONSET_MIN_SPREAD = 0.01;   // floor for the MAD (flux units) so a flat floor is not hypersensitive
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
 * linear magnitudes (0..1) to pass back as `prev` next time. Bands are the summed bin power
 * (a full-scale tone in one bin → 1); flux is Σ max(0, m − prev) / Σ m (0..1).
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
  let lowP = 0, midP = 0, highP = 0;
  for (let i = 0; i < bins; i++) {
    const db = Number.isFinite(dbs[i]) ? dbs[i] : MIN_DB;
    const m = db <= MIN_DB ? 0 : Math.pow(10, db / 20);
    mags[i] = m;
    magSum += m;
    const f = i * binHz;
    weighted += f * m;
    if (prev) { const d = m - prev[i]; if (d > 0) flux += d; }
    const p = m * m;
    if (f < BAND_LOW_HZ) lowP += p; else if (f < BAND_HIGH_HZ) midP += p; else highP += p;
  }
  const centroid = magSum > 1e-9 ? weighted / magSum : 0;
  const band = (p) => (p > 0 ? db01(10 * Math.log10(p)) : 0);
  return {
    rms: clamp01(rms), peak: clamp01(peak), centroid, flux: prev && magSum > 1e-9 ? clamp01(flux / magSum) : 0, zcr,
    low: band(lowP), mid: band(midP), high: band(highP), mags,
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
      const thresh = med + this.k * Math.max(mad, 0.1 * med, ONSET_MIN_SPREAD);
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

// --- mp3 chunk streaming ------------------------------------------------------
/** Is there a plausible MPEG audio frame header at b[i]? (sync, valid version/layer/bitrate/samplerate) */
export function isMp3Sync(b, i) {
  if (i + 3 >= b.length) return false;
  return b[i] === 0xff && (b[i + 1] & 0xe0) === 0xe0 && (b[i + 1] & 0x18) !== 0x08 && (b[i + 1] & 0x06) !== 0
    && (b[i + 2] & 0xf0) !== 0xf0 && (b[i + 2] & 0xf0) !== 0 && (b[i + 2] & 0x0c) !== 0x0c;
}
/**
 * Cut a byte run at MPEG frame boundaries: returns {chunk, rest} where `chunk` starts at the
 * first sync and ends just before the last one (so it holds only whole frames) and `rest`
 * is the tail to prepend to the next run. Null chunk when fewer than two syncs are present.
 */
export function splitMp3(bytes) {
  let first = 0;
  while (first < bytes.length && !isMp3Sync(bytes, first)) first++;
  let last = bytes.length - 4;
  while (last > first && !isMp3Sync(bytes, last)) last--;
  if (last <= first) return { chunk: null, rest: bytes.subarray(first) };
  return { chunk: bytes.slice(first, last), rest: bytes.slice(last) };
}
export const MP3_WARMUP_SAMPLES = 1152;   // decodeAudioData's first frame of a headerless run is silence
export const MP3_CHUNK_BYTES = 24 * 1024;  // ≈1.5 s at 128 kbps

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
      this.analyser.smoothingTimeConstant = SMOOTHING;
      if (this.mic) {
        this.stream = await navigator.mediaDevices.getUserMedia({ audio: { echoCancellation: false, noiseSuppression: false, autoGainControl: false }, video: false });
        this.node = this.ctx.createMediaStreamSource(this.stream);
      } else {
        this.node = this.ctx.createGain();   // fan-in for the decoded chunks / the media element
        if (this.ctx.state !== 'running') { this.needsGesture = true; throw Object.assign(new Error('autoplay'), { name: 'NotAllowedError' }); }
        await this.openStream();
      }
      this.node.connect(this.analyser);
      if (this.monitor && !this.mic) this.analyser.connect(this.ctx.destination);
      this.time = new Float32Array(this.analyser.fftSize);
      this.dbs = new Float32Array(this.analyser.frequencyBinCount);
      this.prev = null; this.silentSince = null; this.lastReading = 0; this.gotAudio = this.mic;
      this.enabled = true;
      this.needsGesture = false;
      this.em.statusForce('live', this.mic ? 'microphone live' : 'stream live');
      this.onChange(true);
      this.startClock();
    } catch (e) {
      this.teardown();
      const msg = e && e.name === 'NotAllowedError' ? (this.mic ? 'permission denied' : 'needs a click first — press “🔊 stream”')
        : (e && e.message) || String(e);
      this.em.statusForce(this.needsGesture ? 'idle' : 'failed', `${this.mic ? 'microphone' : 'stream'}: ${msg}`);
      this.onChange(false);
    }
  }

  /**
   * Analysis clock. A ScriptProcessorNode ticks from the audio thread (~43 ms at 48 kHz for
   * 2048 frames) and keeps running when the tab is in the background, where setInterval is
   * throttled to 1 Hz; it needs a path to the destination, muted by a zero gain.
   */
  startClock() {
    this.lastAnalyse = 0;
    if (typeof this.ctx.createScriptProcessor === 'function') {
      try {
        this.clock = this.ctx.createScriptProcessor(FFT_SIZE, 1, 1);
        this.mute = this.ctx.createGain(); this.mute.gain.value = 0;
        this.clock.onaudioprocess = () => this.analyse();
        this.analyser.connect(this.clock); this.clock.connect(this.mute); this.mute.connect(this.ctx.destination);
        return;
      } catch (_) { /* fall through */ }
    }
    this.timer = setInterval(() => this.analyse(), ANALYSE_MS);
  }

  /** GET the stream; mp3 → chunk decoding, anything else → <audio> element. Resolves once data flows. */
  async openStream() {
    this.abort = new AbortController();
    let resp;
    try { resp = await fetch(this.url, { signal: this.abort.signal, cache: 'no-store' }); }
    catch (e) { throw new Error(`cannot fetch stream (offline, or no CORS header on ${new URL(this.url).host})`); }
    if (!resp.ok) throw new Error(`HTTP ${resp.status} from the stream`);
    const ct = (resp.headers.get('content-type') || '').toLowerCase();
    if (!/audio\/mpeg|audio\/mp3|mpeg/.test(ct)) {
      this.abort.abort();
      await this.openElement(ct);
      return;
    }
    this.reader = resp.body.getReader();
    this.nextTime = 0;
    this.pump().catch((e) => { if (this.enabled && !this.abort.signal.aborted) { this.em.statusForce('reconnecting', `stream ended: ${e.message || e}`); this.reopen(); } });
  }
  async pump() {
    let pending = new Uint8Array(0);
    let lastFlush = performance.now();
    for (;;) {
      const { value, done } = await this.reader.read();
      if (done) throw new Error('end of stream');
      const merged = new Uint8Array(pending.length + value.length); merged.set(pending); merged.set(value, pending.length);
      pending = merged;
      if (pending.length >= MP3_CHUNK_BYTES || (performance.now() - lastFlush > 1500 && pending.length > 4096)) {
        const { chunk, rest } = splitMp3(pending);
        pending = rest;
        lastFlush = performance.now();
        if (chunk) this.decodeChunk(chunk);
      }
    }
  }
  async decodeChunk(chunk) {
    let buf;
    try { buf = await this.ctx.decodeAudioData(chunk.buffer); } catch (_) { return; }   // a torn chunk: skip it
    if (!this.enabled) return;
    const src = this.ctx.createBufferSource();
    src.buffer = buf;
    src.connect(this.node);
    const offset = Math.min(MP3_WARMUP_SAMPLES / buf.sampleRate, buf.duration / 2);
    const now = this.ctx.currentTime;
    if (this.nextTime < now + 0.05) this.nextTime = now + 0.25;        // (re)prime a little ahead
    src.start(this.nextTime, offset);
    this.nextTime += buf.duration - offset;
    this.gotAudio = true;
  }
  async reopen() {
    if (!this.enabled) return;
    await new Promise((r) => setTimeout(r, 2000));
    if (!this.enabled) return;
    try { await this.openStream(); this.em.statusForce('live', 'stream live'); }
    catch (e) { this.em.statusForce('reconnecting', `${e.message || e}; retrying`); this.reopen(); }
  }
  /** Non-mp3 streams: an <audio crossorigin="anonymous"> element (plays natively or not at all). */
  async openElement(contentType) {
    const audio = document.createElement('audio');
    audio.crossOrigin = 'anonymous';
    audio.preload = 'auto';
    audio.src = this.url;
    this.audio = audio;
    await new Promise((res, rej) => {
      const t = setTimeout(() => rej(new Error(`stream (${contentType || 'unknown type'}) did not start within 15 s`)), 15000);
      audio.addEventListener('error', () => { clearTimeout(t); rej(new Error(audio.error?.code === 4 ? `stream not playable (${contentType || 'format'} unsupported, offline, or no CORS header)` : `media error ${audio.error?.code ?? '?'}`)); }, { once: true });
      audio.play().then(() => { clearTimeout(t); res(); }, (e) => { clearTimeout(t); if (e && e.name === 'NotAllowedError') this.needsGesture = true; rej(e); });
    });
    this.ctx.createMediaElementSource(audio).connect(this.node);
    this.gotAudio = true;
  }

  analyse() {
    if (!this.enabled) return;
    const nowMs = performance.now();
    if (nowMs - this.lastAnalyse < ANALYSE_MS * 0.8) return;
    this.lastAnalyse = nowMs;
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
      else if (nowMs - this.silentSince > 8000) { this.silentSince = nowMs; this.em.statusForce('idle', this.gotAudio ? 'stream is silent' : 'no decodable audio yet (CORS blocked, or not an mp3/ogg stream)'); }
    }
  }

  teardown() {
    clearInterval(this.timer);
    this.enabled = false;
    if (this.clock) { this.clock.onaudioprocess = null; try { this.clock.disconnect(); this.mute.disconnect(); } catch (_) { /* ignore */ } this.clock = null; this.mute = null; }
    if (this.stream) for (const t of this.stream.getTracks()) t.stop();
    this.stream = null;
    if (this.abort) { try { this.abort.abort(); } catch (_) { /* ignore */ } this.abort = null; }
    this.reader = null;
    if (this.audio) { try { this.audio.pause(); this.audio.removeAttribute('src'); this.audio.load(); } catch (_) { /* ignore */ } this.audio = null; }
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
