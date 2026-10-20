// "Minimum delay" mode: video over WebCodecs + data channel, in the spirit of
// Sunshine/Moonlight.
//
// The streamer encodes each frame ONCE (VideoEncoder in realtime mode,
// constant bitrate, keyframe only on request) and sends the same chunks
// to everyone over an unordered channel with short retransmission. Viewers
// reassemble, decode with optimizeForLatency and hand the frame right away to
// a regular video track, with no wait buffer.
//
// Packet (little endian):
//   u8 0x54 | u8 flags (1 = keyframe, 2 = has config) | u16 index | u16 total
//   u16 config size | u32 sequence | f64 timestamp (µs) | [config JSON] | data

const MAGIC = 0x54;
const HEADER = 20;
const FRAG = 16000;
const HOLD_MS = 150;        // minimum wait for a missing frame
const HOLD_MAX_MS = 600;    // ... and maximum (long round trip, via relay)
// Sender-side retransmission, no longer than the viewer waits (HOLD_MS, which
// only grows with the round trip): a later resend is for a frame already given up on.
const PACKET_LIFETIME_MS = 150;
const KEY_INTERVAL_MS = 1000; // keyframe requested due to full queue: at most 1 per second per viewer
const QUEUE_FRAMES = 3;       // send queue allowed per viewer, in average frames (~50 ms)
const VIEWER_KEY_MS = 500;    // keyframe requested by a viewer that lost a frame
// Frames waiting in the viewer's decoder. Past this, decoding is slower than
// the stream: delta frames are dropped until a keyframe (each one queued
// would add a frame of delay that never goes away).
const MAX_DECODE_QUEUE = 2;
const REPORT_MS = 1000;       // report from viewer to streamer
// Hardware encoders (NVENC/QuickSync via Media Foundation, VAAPI)
// keep a few frames in flight; only drop if the queue goes past that.
const MAX_ENCODE_QUEUE = 3;
const CHANNEL = { negotiated: true, id: 100, ordered: false, maxPacketLifeTime: PACKET_LIFETIME_MS };

// How decoded frames reach the screen. Chrome and Edge feed a regular video
// track (MediaStreamTrackGenerator, or the standard VideoTrackGenerator where
// a browser exposes it on the page); Safari 16.4+ and Firefox 130+ have
// VideoDecoder but neither, so they paint the newest frame on a canvas once
// per display refresh. ?render=canvas forces the canvas and ?render=rtp turns
// the mode off for this viewer (for testing).
function renderMode() {
  if (/[?&]render=rtp\b/.test(globalThis.location?.search ?? "")) return null;
  const forced = /[?&]render=canvas\b/.test(globalThis.location?.search ?? "");
  if (!forced && typeof MediaStreamTrackGenerator === "function") return "track";
  if (!forced && typeof VideoTrackGenerator === "function") return "generator";
  const canvas = typeof HTMLCanvasElement === "function" && typeof VideoFrame === "function"
    && typeof CanvasRenderingContext2D === "function";
  return canvas ? "canvas" : null;
}

export function turboSupport() {
  const render = typeof VideoDecoder === "function" ? renderMode() : null;
  return {
    send: typeof MediaStreamTrackProcessor === "function" && typeof VideoEncoder === "function",
    receive: !!render,
    render,
  };
}

// Both sides create the same channel (negotiated, fixed id) over the already open connection.
export function openVideoChannel(pc) {
  const ch = pc.createDataChannel("telinha-video", CHANNEL);
  ch.binaryType = "arraybuffer";
  return ch;
}

/* ---------------- Streamer ---------------- */

// Preference: H.264 on the GPU, then H.264 and VP8 in software.
const CANDIDATES = [
  ["avc1.640034", "prefer-hardware"],
  ["avc1.4d0034", "prefer-hardware"],
  ["avc1.42e034", "prefer-hardware"],
  ["avc1.42e034", "prefer-software"],
  ["vp8", "prefer-software"],
];

function encoderConfig({ codec, hw, mode }, width, height, bitrate, framerate) {
  return {
    codec, width, height, bitrate, framerate,
    latencyMode: "realtime",
    bitrateMode: mode,
    hardwareAcceleration: hw,
    ...(codec.startsWith("avc1") ? { avc: { format: "annexb" } } : {}), // SPS/PPS on every keyframe
  };
}

async function pickEncoder(width, height, bitrate, framerate) {
  for (const [codec, hw] of CANDIDATES) {
    for (const mode of ["constant", "variable"]) {
      const choice = { codec, hw, mode };
      try {
        const { supported } = await VideoEncoder.isConfigSupported(encoderConfig(choice, width, height, bitrate, framerate));
        if (supported) return choice;
      } catch {}
    }
  }
  return null;
}

export class TurboSender {
  constructor(track, { bitrate, framerate }) {
    this.bitrate = bitrate;
    this.framerate = framerate;
    this.viewers = new Map();
    this.needKey = true;
    this.lastKeyAt = 0;
    this.encoder = null;
    this.choice = null;
    this.size = null;
    this.dirty = false;
    this.closed = false;
    this.pending = new Map(); // timestamp -> time of encode()
    this.stats = { encodeMs: null, frames: 0, captured: 0, skipped: 0 };
    this.reader = new MediaStreamTrackProcessor({ track }).readable.getReader();
    this.done = this.#loop();
  }

  addViewer(id, channel, { onCongested } = {}) {
    const v = { id, channel, seq: 0, needKey: true, frames: 0, bytes: 0, dropped: 0, drops: [], onCongested, last: null, strikes: 0, lastKeyAt: -Infinity, lastKey: null };
    this.viewers.set(id, v);
    const start = () => this.requestKey(0, id);
    if (channel.readyState === "open") start();
    else channel.addEventListener("open", start, { once: true });
  }

  removeViewer(id) { this.viewers.delete(id); }

  // Viewers count what arrived. The channel silently drops anything past its
  // lifetime, so the queue here does not show congestion; the report does.
  // Three bad reports in a row: that person falls back to WebRTC.
  report(id, { bytes, lost }) {
    const v = this.viewers.get(id);
    if (!v) return;
    const now = { sent: v.bytes, bytes, lost };
    const prev = v.last;
    v.last = now;
    if (!prev) return;
    const sent = now.sent - prev.sent, got = now.bytes - prev.bytes, lostNow = now.lost - prev.lost;
    const bad = (sent > 50e3 && got < sent * 0.8) || lostNow >= 2;
    v.strikes = bad ? v.strikes + 1 : 0;
    if (v.strikes >= 3) v.onCongested?.();
  }

  // Each viewer has its own limit, so one asking does not hold back another
  // who just joined; requests within the same frame still make one keyframe.
  requestKey(minInterval = KEY_INTERVAL_MS, id = null) {
    const holder = (id !== null && this.viewers.get(id)) || this;
    const now = performance.now();
    if (now - holder.lastKeyAt < minInterval) return;
    holder.lastKeyAt = now;
    this.needKey = true;
  }

  // Bytes allowed in a viewer's send queue: a few average frames, plus what is
  // left of the last keyframe still draining at the bitrate (without it the
  // frames after a keyframe would be dropped and another one asked for).
  #budget(v) {
    const perSecond = this.bitrate / 8;
    const keyLeft = v.lastKey ? Math.max(0, v.lastKey.size - ((performance.now() - v.lastKey.at) / 1000) * perSecond) : 0;
    return Math.max(32 * 1024, (perSecond / Math.max(1, this.framerate)) * QUEUE_FRAMES) + keyLeft;
  }

  setRate({ bitrate, framerate }) {
    if (bitrate === this.bitrate && framerate === this.framerate) return;
    this.bitrate = bitrate;
    this.framerate = framerate;
    this.dirty = true;
  }

  viewerStats(id) {
    const v = this.viewers.get(id);
    if (!v) return null;
    return {
      codec: this.choice?.codec, hardware: this.choice ? this.choice.hw === "prefer-hardware" : undefined,
      width: this.size?.w, height: this.size?.h,
      frames: v.frames, bytes: v.bytes, dropped: v.dropped, encodeMs: this.stats.encodeMs,
      captured: this.stats.captured, skipped: this.stats.skipped, queue: this.encoder?.encodeQueueSize ?? 0,
    };
  }

  stop() {
    this.closed = true;
    this.reader.cancel().catch(() => {});
    try { this.encoder?.close(); } catch {}
    this.viewers.clear();
  }

  async #loop() {
    while (!this.closed) {
      let result;
      try { result = await this.reader.read(); } catch { break; }
      if (result.done) break;
      try { await this.#encode(result.value); } catch (err) {
        console.warn("[turbo] encoding", err);
        try { result.value.close(); } catch {}
      }
    }
  }

  async #encode(frame) {
    if (!this.viewers.size) { frame.close(); this.needKey = true; return; }
    this.stats.captured++;
    const w = frame.displayWidth & ~1, h = frame.displayHeight & ~1;
    if (!this.encoder || this.encoder.state !== "configured" || this.dirty || this.size?.w !== w || this.size?.h !== h) {
      await this.#configure(w, h);
    }
    // Encoder queue full: drop before encoding, like Sunshine.
    if (!this.encoder || this.encoder.encodeQueueSize >= MAX_ENCODE_QUEUE) { frame.close(); this.stats.skipped++; return; }
    const keyFrame = this.needKey;
    this.needKey = false;
    if (this.pending.size > 120) this.pending.clear();
    this.pending.set(frame.timestamp, performance.now());
    this.encoder.encode(frame, { keyFrame });
    frame.close();
  }

  async #configure(w, h) {
    if (!this.choice || this.dirty) this.choice = await pickEncoder(w, h, this.bitrate, this.framerate);
    if (!this.choice) throw new Error("nenhum codificador disponível");
    if (!this.encoder || this.encoder.state === "closed") {
      this.encoder = new VideoEncoder({
        output: (chunk) => this.#output(chunk),
        error: (err) => { console.warn("[turbo] encoder", err); this.encoder = null; },
      });
    }
    this.encoder.configure(encoderConfig(this.choice, w, h, this.bitrate, this.framerate));
    this.size = { w, h };
    this.dirty = false;
    this.needKey = true;
    this.config = new TextEncoder().encode(JSON.stringify({ codec: this.choice.codec, w, h }));
  }

  #output(chunk) {
    const t0 = this.pending.get(chunk.timestamp);
    this.pending.delete(chunk.timestamp);
    if (t0) this.stats.encodeMs = ema(this.stats.encodeMs, performance.now() - t0);
    this.stats.frames++;
    const data = new Uint8Array(chunk.byteLength);
    chunk.copyTo(data);
    const key = chunk.type === "key";
    for (const v of this.viewers.values()) this.#sendTo(v, data, key, chunk.timestamp);
  }

  #sendTo(v, data, key, ts) {
    const ch = v.channel;
    if (ch.readyState !== "open") { v.needKey = true; return; }
    const budget = this.#budget(v);
    if (v.needKey && !key) {
      // wait for the queue to drain before requesting the keyframe, or it won't fit either
      if (ch.bufferedAmount < budget / 2) this.requestKey(KEY_INTERVAL_MS, v.id);
      return;
    }
    if (ch.bufferedAmount > budget) {
      v.needKey = true;
      v.dropped++;
      const now = performance.now();
      v.drops.push(now);
      while (v.drops.length && now - v.drops[0] > 3000) v.drops.shift();
      if (v.drops.length > 20) v.onCongested?.(); // queue full all the time: this person falls back to WebRTC
      return;
    }
    const count = Math.max(1, Math.ceil(data.byteLength / FRAG));
    const seq = v.seq++;
    for (let i = 0; i < count; i++) {
      const body = data.subarray(i * FRAG, (i + 1) * FRAG);
      const extra = i === 0 && key ? this.config : null;
      const extraLen = extra?.byteLength ?? 0;
      const buf = new Uint8Array(HEADER + extraLen + body.byteLength);
      const dv = new DataView(buf.buffer);
      dv.setUint8(0, MAGIC);
      dv.setUint8(1, (key ? 1 : 0) | (extra ? 2 : 0));
      dv.setUint16(2, i, true);
      dv.setUint16(4, count, true);
      dv.setUint16(6, extraLen, true);
      dv.setUint32(8, seq, true);
      dv.setFloat64(12, ts, true);
      if (extra) buf.set(extra, HEADER);
      buf.set(body, HEADER + extraLen);
      try { ch.send(buf); } catch { v.needKey = true; return; }
    }
    if (key) { v.needKey = false; v.lastKey = { size: data.byteLength, at: performance.now() }; }
    v.frames++;
    v.bytes += data.byteLength;
  }
}

/* ---------------- Viewer ---------------- */

export class TurboReceiver {
  constructor(channel, { requestKey, onUnsupported, report, render = turboSupport().render }) {
    this.generator = null;
    this.writer = null;
    this.canvas = null;
    if (render === "track" || render === "generator") {
      try {
        this.generator = render === "track" ? new MediaStreamTrackGenerator({ kind: "video" }) : new VideoTrackGenerator();
        this.writer = this.generator.writable.getWriter();
      } catch (err) {
        console.warn("[turbo] video track unavailable, painting on a canvas", err);
        this.generator = null;
      }
    }
    if (!this.writer) this.canvas = new CanvasView();
    // H.264 goes to the decoder as it travels (Annex B); a decoder that refuses
    // it before decoding anything gets length-prefixed NAL units and an avcC
    // description instead (?avc=avcc forces that, for testing).
    this.avcc = /[?&]avc=avcc\b/.test(globalThis.location?.search ?? "");
    this.decodedAny = false;
    this.sinkSkips = 0;
    this.frames = new Map(); // seq -> frame being assembled
    this.expected = null;
    this.waitingKey = true;
    this.decoder = null;
    this.config = null;
    this.errors = 0;
    this.timer = 0;
    this.closed = false;
    this.pending = new Map(); // timestamp -> time of decode()
    this.stats = {
      codec: null, width: 0, height: 0, frames: 0, bytes: 0, lost: 0, keyRequests: 0, assemblyMs: null, decodeMs: null,
      decodeQueue: 0, decodeDrops: 0, renderDrops: 0,
    };
    this.reportTimer = report ? setInterval(() => report({ bytes: this.payload, lost: this.stats.lost }), REPORT_MS) : 0;
    this.payload = 0; // video data only, to compare with what was sent
    this.onUnsupported = onUnsupported;
    this.hold = HOLD_MS;
    let last = 0;
    this.requestKey = () => {
      const now = performance.now();
      if (now - last < VIEWER_KEY_MS) return;
      last = now;
      this.stats.keyRequests++;
      requestKey();
    };
    channel.binaryType = "arraybuffer";
    channel.addEventListener("message", (e) => this.#onPacket(e.data));
  }

  /** The video track (Chrome, Edge), or null when frames go to `view`. */
  get track() { return this.generator ? (this.generator.track ?? this.generator) : null; }
  /** Canvas painter (Safari, Firefox), or null when frames go to `track`. */
  get view() { return this.canvas; }

  /// A retransmission takes one round trip: the wait for a missing frame tracks the connection.
  setRtt(ms) {
    if (ms > 0) this.hold = Math.min(HOLD_MAX_MS, Math.max(HOLD_MS, ms * 2.5));
  }

  close() {
    this.closed = true;
    clearTimeout(this.timer);
    clearInterval(this.reportTimer);
    try { this.decoder?.close(); } catch {}
    this.writer?.close().catch(() => {});
    try { (this.generator?.track ?? this.generator)?.stop(); } catch {}
    this.canvas?.close();
    this.frames.clear();
  }

  #onPacket(buf) {
    if (this.closed || !(buf instanceof ArrayBuffer) || buf.byteLength < HEADER) return;
    const dv = new DataView(buf);
    if (dv.getUint8(0) !== MAGIC) return;
    const flags = dv.getUint8(1);
    const idx = dv.getUint16(2, true), count = dv.getUint16(4, true), cfgLen = dv.getUint16(6, true);
    const seq = dv.getUint32(8, true), ts = dv.getFloat64(12, true);
    this.stats.bytes += buf.byteLength;
    if (this.expected !== null && seq < this.expected) return; // arrived too late
    if (idx >= count) return;

    let f = this.frames.get(seq);
    if (!f) {
      f = { parts: new Array(count), got: 0, size: 0, key: !!(flags & 1), ts, at: performance.now(), done: 0, config: null };
      this.frames.set(seq, f);
    }
    if (f.parts[idx]) return;
    let off = HEADER;
    if (flags & 2) {
      try { f.config = JSON.parse(new TextDecoder().decode(new Uint8Array(buf, HEADER, cfgLen))); } catch {}
      off += cfgLen;
    }
    f.parts[idx] = new Uint8Array(buf, off);
    this.payload += buf.byteLength - off;
    f.got++;
    f.size += f.parts[idx].byteLength;
    if (f.got === f.parts.length) {
      f.done = performance.now();
      this.stats.assemblyMs = ema(this.stats.assemblyMs, f.done - f.at);
    }
    this.#drain();
  }

  // Decode in order. A complete keyframe further ahead jumps the queue.
  #drain() {
    for (let guard = 0; guard < 1000; guard++) {
      if (this.waitingKey || this.expected === null) {
        let best = null;
        for (const [seq, f] of this.frames) if (f.done && f.key && (best === null || seq > best)) best = seq;
        if (best === null) break;
        for (const seq of this.frames.keys()) if (seq < best) this.frames.delete(seq);
        this.expected = best;
        this.waitingKey = false;
      }
      const f = this.frames.get(this.expected);
      if (!f?.done) {
        for (const [seq, g] of this.frames) if (g.done && g.key && seq > this.expected) { this.waitingKey = true; break; }
        if (this.waitingKey) continue;
        break;
      }
      this.frames.delete(this.expected);
      this.expected++;
      const queue = this.decoder?.decodeQueueSize ?? 0;
      this.stats.decodeQueue = queue;
      if (!f.key && queue > MAX_DECODE_QUEUE) {
        // The decoder is behind: skip ahead to a keyframe instead of queueing.
        this.stats.decodeDrops++;
        this.waitingKey = true;
        for (const [seq, g] of this.frames) if (!g.key) this.frames.delete(seq);
        this.requestKey();
        continue;
      }
      this.#decode(f);
    }
    this.#arm();
  }

  #arm() {
    if (this.timer || this.closed) return;
    if (!this.frames.size && !this.waitingKey) return;
    this.timer = setTimeout(() => { this.timer = 0; this.#check(); }, this.hold / 3);
  }

  // A frame that did not arrive in time is a loss: request a keyframe and move on without it.
  #check() {
    const now = performance.now();
    if (this.waitingKey) {
      this.requestKey();
    } else {
      const missing = this.frames.get(this.expected);
      const stale = [...this.frames.values()].some((f) => now - f.at > this.hold);
      if (stale && !missing?.done) {
        this.stats.lost++;
        this.waitingKey = true;
        for (const [seq, f] of this.frames) if (!f.key) this.frames.delete(seq);
        this.requestKey();
      }
    }
    for (const [seq, f] of this.frames) if (now - f.at > 2000) this.frames.delete(seq); // stale leftovers
    this.#drain();
  }

  #decode(f) {
    try {
      const c = f.config;
      if (c && (!this.config || c.codec !== this.config.codec || c.w !== this.config.w || c.h !== this.config.h || this.decoder?.state !== "configured")) {
        this.#configure(c, f);
      }
      if (!this.decoder || this.decoder.state !== "configured") { this.waitingKey = true; return; }
      let data = f.parts[0];
      if (f.parts.length > 1) {
        data = new Uint8Array(f.size);
        let off = 0;
        for (const p of f.parts) { data.set(p, off); off += p.byteLength; }
      }
      // avcC: parameter sets travel in the description, not in the frames.
      if (this.avccMode) data = toAvcc(splitAnnexB(data).filter((n) => ![7, 8, 9].includes(n[0] & 0x1f)));
      if (this.pending.size > 120) this.pending.clear();
      this.pending.set(f.ts, performance.now());
      this.decoder.decode(new EncodedVideoChunk({ type: f.key ? "key" : "delta", timestamp: f.ts, data }));
    } catch (err) {
      this.#fail(err);
    }
  }

  // `f` is the keyframe carrying the config (its SPS/PPS make the avcC description).
  #configure(c, f) {
    if (!this.decoder || this.decoder.state === "closed") {
      this.decoder = new VideoDecoder({ output: (frame) => this.#output(frame), error: (err) => this.#fail(err) });
    }
    const config = { codec: c.codec, codedWidth: c.w, codedHeight: c.h, optimizeForLatency: true };
    this.avccMode = false;
    if (this.avcc && /^avc[13]\./.test(c.codec)) {
      const description = avcDescription(splitAnnexB(f.parts[0]));
      if (description) { config.description = description; this.avccMode = true; }
    }
    this.decoder.configure(config);
    this.config = c;
    this.stats.codec = c.codec;
  }

  #output(frame) {
    const t0 = this.pending.get(frame.timestamp);
    this.pending.delete(frame.timestamp);
    if (t0) this.stats.decodeMs = ema(this.stats.decodeMs, performance.now() - t0);
    this.stats.frames++;
    this.stats.width = frame.displayWidth;
    this.stats.height = frame.displayHeight;
    this.errors = 0;
    this.decodedAny = true;
    if (this.closed) { frame.close(); return; }
    if (this.canvas) {
      this.canvas.push(frame);
      this.stats.renderDrops = this.canvas.superseded;
      return;
    }
    // The track has not taken the previous frame yet: this one would only
    // wait behind it, so it goes (the next one carries the newer picture).
    // (Never more than a few in a row, in case a browser reports a full sink
    // until something renders: a still picture beats a black one.)
    if (this.writer.desiredSize !== null && this.writer.desiredSize <= 0 && this.sinkSkips < 3) {
      frame.close();
      this.stats.renderDrops++;
      this.sinkSkips++;
      return;
    }
    this.sinkSkips = 0;
    this.writer.write(frame).catch(() => { try { frame.close(); } catch {} });
  }

  #fail(err) {
    if (this.closed) return;
    console.warn("[turbo] decoding", err);
    try { this.decoder?.close(); } catch {}
    this.decoder = null;
    this.config = null;
    this.waitingKey = true;
    // Safari's and Firefox's H.264 support differs in which bitstream format
    // they take: before giving up, try the other one once.
    if (!this.decodedAny && !this.avcc && this.stats.codec?.startsWith("avc")) {
      console.warn("[turbo] retrying H.264 as avcC");
      this.avcc = true;
      this.errors = 0;
      this.requestKey();
      return;
    }
    if (err?.name === "NotSupportedError" || ++this.errors > 5) { this.onUnsupported?.(); return; }
    this.requestKey();
  }
}

/* ---------------- Canvas painting (Safari, Firefox) ---------------- */

// Paints the newest decoded frame on every attached canvas, once per display
// refresh: a frame replaced before its turn is closed unseen, so a slow
// display never builds a queue. The last painted frame stays open so a canvas
// attached later (switching the featured screen) shows it right away.
// Thumbnails are small and repainted at a fraction of the rate.
const THUMB_EVERY = 4;
const THUMB_WIDTH = 320;

export class CanvasView {
  constructor() {
    this.pending = null;
    this.shown = null;
    this.targets = new Map(); // canvas -> { ctx, thumb }
    this.raf = 0;
    this.painted = 0;
    this.superseded = 0;
    this.closed = false;
    this.onPaint = null;
  }

  get width() { return this.shown?.displayWidth ?? 0; }
  get height() { return this.shown?.displayHeight ?? 0; }

  attach(canvas, { thumb = false } = {}) {
    if (this.closed || this.targets.has(canvas)) return;
    // desynchronized: Chrome may skip the compositor's extra frame of delay;
    // others ignore it. No alpha: the video is opaque.
    let ctx = null;
    try { ctx = canvas.getContext("2d", { alpha: false, desynchronized: !thumb }); } catch {}
    if (!ctx) return;
    this.targets.set(canvas, { ctx, thumb });
    if (this.shown) this.#draw(canvas, this.targets.get(canvas), this.shown);
  }

  detach(canvas) { this.targets.delete(canvas); }

  push(frame) {
    if (this.closed) { frame.close(); return; }
    if (this.pending) { this.pending.close(); this.superseded++; }
    this.pending = frame;
    // A hidden page gets no animation frames: paint right away (picture in
    // picture and a later return to the tab still see the newest image).
    if (document.hidden) { this.#paint(); return; }
    if (!this.raf) this.raf = requestAnimationFrame(() => { this.raf = 0; this.#paint(); });
  }

  #paint() {
    const frame = this.pending;
    if (!frame) return;
    this.pending = null;
    this.painted++;
    for (const [canvas, t] of this.targets) {
      if (!t.thumb || this.painted % THUMB_EVERY === 1) this.#draw(canvas, t, frame);
    }
    this.shown?.close();
    this.shown = frame;
    this.onPaint?.(frame);
  }

  #draw(canvas, t, frame) {
    const w = frame.displayWidth, h = frame.displayHeight;
    const scale = t.thumb ? Math.min(1, THUMB_WIDTH / w) : 1;
    const cw = Math.max(1, Math.round(w * scale)), ch = Math.max(1, Math.round(h * scale));
    if (canvas.width !== cw || canvas.height !== ch) { canvas.width = cw; canvas.height = ch; }
    try { t.ctx.drawImage(frame, 0, 0, cw, ch); } catch (err) { console.warn("[turbo] painting", err); }
  }

  close() {
    this.closed = true;
    cancelAnimationFrame(this.raf);
    this.pending?.close();
    this.shown?.close();
    this.pending = this.shown = null;
    this.targets.clear();
  }
}

/* ---------------- H.264 bitstream formats ---------------- */

// NAL units of an Annex B access unit (start codes 00 00 01 / 00 00 00 01).
export function splitAnnexB(data) {
  const nals = [];
  let start = -1;
  for (let i = 0; i + 2 < data.length; i++) {
    if (data[i] === 0 && data[i + 1] === 0 && data[i + 2] === 1) {
      if (start >= 0) nals.push(data.subarray(start, i > 0 && data[i - 1] === 0 ? i - 1 : i));
      start = i + 3;
      i += 2;
    }
  }
  if (start >= 0 && start < data.length) nals.push(data.subarray(start));
  return nals.filter((n) => n.length);
}

// The same NAL units, each prefixed with its 4-byte length (the MP4 format).
export function toAvcc(nals) {
  const out = new Uint8Array(nals.reduce((n, x) => n + 4 + x.length, 0));
  const dv = new DataView(out.buffer);
  let off = 0;
  for (const n of nals) { dv.setUint32(off, n.length); out.set(n, off + 4); off += 4 + n.length; }
  return out;
}

// avcC record (ISO 14496-15) from a keyframe's SPS and PPS.
export function avcDescription(nals) {
  const sps = nals.find((n) => (n[0] & 0x1f) === 7), pps = nals.find((n) => (n[0] & 0x1f) === 8);
  if (!sps || !pps || sps.length < 4) return null;
  const out = new Uint8Array(11 + sps.length + pps.length);
  out.set([1, sps[1], sps[2], sps[3], 0xff, 0xe1, sps.length >> 8, sps.length & 0xff]);
  out.set(sps, 8);
  let off = 8 + sps.length;
  out.set([1, pps.length >> 8, pps.length & 0xff], off);
  out.set(pps, off + 3);
  return out;
}

function ema(prev, v) { return prev == null ? v : prev * 0.9 + v * 0.1; }
