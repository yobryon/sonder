// Sonder AudioWorklet: plays interleaved stereo f32 chunks posted from the main
// thread through a ring buffer, resampling linearly from the server's rate to
// the context's rate when they differ.
//
// Messages in:  { type: 'config', srcRate, targetMs, maxMs }
//               { type: 'pcm', data: Float32Array }  (interleaved L/R)
// Messages out: { type: 'stats', bufferedMs, underruns, playing }

class SonderPlayer extends AudioWorkletProcessor {
  constructor() {
    super();
    this.srcRate = sampleRate;
    this.targetFrames = Math.round(sampleRate * 0.2);
    this.maxFrames = Math.round(sampleRate * 0.5);
    this.capacity = 0;
    this.ring = null;
    this.readPos = 0;   // fractional absolute frame index
    this.writePos = 0;  // absolute frame index
    this.underruns = 0;
    this.playing = false;
    this.lastReport = 0;
    this.allocate(sampleRate * 3);
    this.port.onmessage = (e) => this.onMessage(e.data);
  }

  allocate(frames) {
    this.capacity = Math.max(1024, Math.floor(frames));
    this.ring = new Float32Array(this.capacity * 2);
    this.readPos = 0;
    this.writePos = 0;
    this.playing = false;
  }

  onMessage(msg) {
    if (!msg) return;
    if (msg.type === 'config') {
      const rate = msg.srcRate > 0 ? msg.srcRate : sampleRate;
      this.srcRate = rate;
      this.targetFrames = Math.round(rate * (msg.targetMs || 200) / 1000);
      this.maxFrames = Math.round(rate * (msg.maxMs || 500) / 1000);
      this.allocate(rate * 3);
    } else if (msg.type === 'pcm' && msg.data) {
      this.write(msg.data);
    }
  }

  buffered() {
    return this.writePos - Math.floor(this.readPos);
  }

  write(data) {
    const frames = data.length >> 1;
    const cap = this.capacity;
    if (frames >= cap) return;
    // Hard cap: never overwrite unread data; drop oldest instead.
    const overflow = this.buffered() + frames - cap;
    if (overflow > 0) this.readPos += overflow;
    let w = this.writePos % cap;
    for (let i = 0; i < frames; i++) {
      this.ring[w * 2] = data[i * 2];
      this.ring[w * 2 + 1] = data[i * 2 + 1];
      w++;
      if (w === cap) w = 0;
    }
    this.writePos += frames;
  }

  report(force) {
    if (!force && currentTime - this.lastReport < 0.1) return;
    this.lastReport = currentTime;
    this.port.postMessage({
      type: 'stats',
      bufferedMs: (this.buffered() / this.srcRate) * 1000,
      underruns: this.underruns,
      playing: this.playing,
    });
  }

  process(inputs, outputs) {
    const out = outputs[0];
    const L = out[0];
    const R = out.length > 1 ? out[1] : out[0];
    const n = L.length;
    const ratio = this.srcRate / sampleRate;
    const needed = Math.ceil(n * ratio) + 2;

    // Latency guard: if the buffer has grown past maxMs (network burst, tab
    // was throttled), skip ahead so we sit at the target latency again.
    if (this.buffered() > this.maxFrames) {
      this.readPos = this.writePos - this.targetFrames;
    }

    if (!this.playing) {
      // Pre-roll: wait until we have ~target latency before starting.
      if (this.buffered() >= this.targetFrames) this.playing = true;
      else { L.fill(0); R.fill(0); this.report(false); return true; }
    }

    if (this.buffered() < needed) {
      L.fill(0); R.fill(0);
      this.underruns++;
      this.playing = false;
      this.report(true);
      return true;
    }

    const ring = this.ring;
    const cap = this.capacity;
    let pos = this.readPos;
    for (let i = 0; i < n; i++) {
      const i0 = Math.floor(pos);
      const frac = pos - i0;
      const a = (i0 % cap) * 2;
      const b = ((i0 + 1) % cap) * 2;
      L[i] = ring[a] + (ring[b] - ring[a]) * frac;
      R[i] = ring[a + 1] + (ring[b + 1] - ring[a + 1]) * frac;
      pos += ratio;
    }
    this.readPos = pos;
    this.report(false);
    return true;
  }
}

registerProcessor('sonder-player', SonderPlayer);
