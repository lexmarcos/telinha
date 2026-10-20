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
//
// Parity (flags 4): index = total + group, data = u32 frame size | XOR of the
// group's fragments, each zero-padded to the longest. Fragment i belongs to
// group i % groups, groups = ceil(total / FEC_GROUP), so any one fragment lost
// per group is rebuilt without waiting for a keyframe. Keyframes' parity
// carries the config too (it may be the first fragment that is rebuilt).
// Viewers from before parity drop it (index >= total); senders only add it for
// viewers that announce `fec` in their reports, while they are losing data.

const MAGIC = 0x54;
const HEADER = 20;
const FRAG = 16000;
const FEC_GROUP = 5;          // fragments per parity packet: 20% more data (up to 50% for two-fragment frames)
const FEC_HOLD_MS = 30000;    // parity stays on this long after the last loss a viewer reported
const HOLD_MS = 150;        // minimum wait for a missing frame
const HOLD_MAX_MS = 600;    // ... and maximum (long round trip, via relay)
// Sender-side retransmission, no longer than the viewer waits (HOLD_MS, which
// only grows with the round trip): a later resend is for a frame already given up on.
const PACKET_LIFETIME_MS = 150;
const KEY_INTERVAL_MS = 1000; // keyframe requested due to full queue: at most 1 per second per viewer
const QUEUE_FRAMES = 3;       // send queue allowed per viewer, in average frames (~50 ms)
const VIEWER_KEY_MS = 500;    // keyframe requested by a viewer that lost a frame
// Someone who keeps asking for keyframes waits longer the second time (500 ms,
// then 1 s), back to the start after a few calm seconds: on a link that keeps
// losing, a keyframe as often as allowed only adds to the loss. Not longer:
// the picture stays frozen until the keyframe.
const KEY_BACKOFF_START_MS = 500;
const KEY_BACKOFF_MAX_MS = 1000;
const KEY_CALM_MS = 4000;
// A new viewer starts from the last keyframe if what came after it fits in
// this share of a second of the bitrate (a still screen, or a keyframe just
// sent to someone else), instead of making everyone get a new keyframe.
const REPLAY_SECONDS = 0.1;
const GOP_MAX_BYTES = 8 << 20;
const GOP_MAX_FRAMES = 600;
// Frames waiting in the viewer's decoder. Past this for a while, decoding is
// slower than the stream: delta frames are dropped until a keyframe (each one
// queued would add a frame of delay that never goes away). A short burst (a
// new viewer starting from a cached keyframe, frames held up by the network)
// drains on its own and is left alone.
const MAX_DECODE_QUEUE = 2;
const DECODE_BEHIND_MS = 150;
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

class KeyBackoff {
  constructor() { this.last = -Infinity; this.wait = 0; }
  // Whether a keyframe may be asked for now (and, if so, counts it).
  allow(min = 0) {
    const now = performance.now();
    if (now - this.last >= KEY_CALM_MS) this.wait = 0;
    if (now - this.last < Math.max(min, this.wait)) return false;
    this.last = now;
    this.wait = Math.min(KEY_BACKOFF_MAX_MS, Math.max(KEY_BACKOFF_START_MS, this.wait * 2));
    return true;
  }
}

// Codecs past H.264 this browser decodes in hardware, for the streamer to
// choose (only the desktop app uses it so far, and only HEVC, behind a flag).
// isConfigSupported says "maybe" more than "yes" for hardware: a viewer whose
// decoder still fails asks for normal WebRTC, as with any other codec.
// ?dec=hevc claims a list instead (to test the streamer's switch and fallback).
export async function hardwareDecoders() {
  const claimed = new URLSearchParams(globalThis.location?.search ?? "").get("dec");
  if (claimed !== null) return claimed.split(",").filter(Boolean);
  if (typeof VideoDecoder !== "function" || !turboSupport().receive) return [];
  const out = [];
  for (const [name, codec] of [["hevc", "hvc1.1.6.L153.B0"], ["av1", "av01.0.12M.08"]]) {
    try {
      const { supported } = await VideoDecoder.isConfigSupported({ codec, codedWidth: 3840, codedHeight: 2160, hardwareAcceleration: "prefer-hardware" });
      if (supported) out.push(name);
    } catch {}
  }
  return out;
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

/* ---------------- Clocks ---------------- */

// The viewer pings the streamer over the control channel; the pong carries
// the streamer's wall clock (`b`, ms) and its frame clock (`s`, µs, the unit of
// the packets' timestamps) at the moment it answered. The sample with the
// shortest round trip of the recent ones gives the offset between the two
// wall clocks and where frame timestamps sit on the streamer's wall clock, so
// each frame's capture time lands on this viewer's clock: glass to glass.
const PING_MS = 1000;
const PING_KEEP = 10;

export class PeerClock {
  constructor(send) {
    this.send = send;
    this.samples = [];
    this.best = null; // { rtt, offset (streamer - viewer, ms), frameZero (streamer wall ms at frame clock 0) }
    this.timer = setInterval(() => this.ping(), PING_MS);
    this.ping();
  }
  ping() { try { this.send({ t: "ping", a: Date.now() }); } catch {} }
  pong({ a, b, s }) {
    const c = Date.now();
    if (typeof a !== "number" || typeof b !== "number" || c < a) return;
    const sample = { rtt: c - a, offset: b - (a + c) / 2, frameZero: typeof s === "number" ? b - s / 1000 : null };
    this.samples.push(sample);
    if (this.samples.length > PING_KEEP) this.samples.shift();
    this.best = this.samples.reduce((m, x) => (x.rtt < m.rtt ? x : m));
  }
  get rtt() { return this.best?.rtt ?? null; }
  /// A frame timestamp (µs) as a time on this viewer's wall clock (ms).
  captureTime(ts) {
    const b = this.best;
    if (!b || b.frameZero === null) return null;
    return b.frameZero + ts / 1000 - b.offset;
  }
  close() { clearInterval(this.timer); }
}

// Wall-clock time (ms) of a performance.now() instant.
function wall(t) { return Date.now() - (performance.now() - t); }

/* ---------------- Packets ---------------- */

export function fecGroups(count) { return count < 2 ? 0 : Math.ceil(count / FEC_GROUP); }

// A frame's packets: its fragments and, with `parity`, the parity packets.
export function packetize(seq, data, key, ts, config, parity = false) {
  const count = Math.max(1, Math.ceil(data.byteLength / FRAG));
  const out = [];
  const pack = (flags, idx, extra, body) => {
    const extraLen = extra?.byteLength ?? 0;
    const buf = new Uint8Array(HEADER + extraLen + body.byteLength);
    const dv = new DataView(buf.buffer);
    dv.setUint8(0, MAGIC);
    dv.setUint8(1, flags | (key ? 1 : 0) | (extra ? 2 : 0));
    dv.setUint16(2, idx, true);
    dv.setUint16(4, count, true);
    dv.setUint16(6, extraLen, true);
    dv.setUint32(8, seq, true);
    dv.setFloat64(12, ts, true);
    if (extra) buf.set(extra, HEADER);
    buf.set(body, HEADER + extraLen);
    return buf;
  };
  for (let i = 0; i < count; i++) out.push(pack(0, i, i === 0 && key ? config : null, data.subarray(i * FRAG, (i + 1) * FRAG)));
  const groups = parity ? fecGroups(count) : 0;
  for (let g = 0; g < groups; g++) {
    const x = new Uint8Array(4 + Math.min(FRAG, data.byteLength));
    new DataView(x.buffer).setUint32(0, data.byteLength, true);
    for (let i = g; i < count; i += groups) {
      const body = data.subarray(i * FRAG, (i + 1) * FRAG);
      for (let k = 0; k < body.length; k++) x[4 + k] ^= body[k];
    }
    out.push(pack(4, count + g, key ? config : null, x));
  }
  return out;
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
    this.keys = new KeyBackoff();
    this.gop = { frames: [], rest: 0 }; // last keyframe and what came after it
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
    const v = { id, channel, seq: 0, needKey: true, frames: 0, bytes: 0, dropped: 0, drops: [], onCongested, last: null, strikes: 0, keys: new KeyBackoff(), lastKey: null };
    this.viewers.set(id, v);
    const start = () => {
      const { frames, rest } = this.gop;
      if (frames.length && rest <= (this.bitrate / 8) * REPLAY_SECONDS) {
        for (const f of frames) this.#sendTo(v, f.data, f.key, f.ts, f.config);
        if (!v.needKey) return;
      }
      this.requestKey(0, id);
    };
    if (channel.readyState === "open") start();
    else channel.addEventListener("open", start, { once: true });
  }

  removeViewer(id) { this.viewers.delete(id); }

  // Viewers count what arrived. The channel silently drops anything past its
  // lifetime, so the queue here does not show congestion; the report does.
  // Three bad reports in a row: that person falls back to WebRTC.
  report(id, { bytes, lost, recovered, fec }) {
    const v = this.viewers.get(id);
    if (!v) return;
    // Parity while this viewer loses data (and can use it).
    if (fec && v.last && (lost > v.last.lost || (recovered ?? 0) > (v.last.recovered ?? 0))) v.fecUntil = performance.now() + FEC_HOLD_MS;
    const now = { sent: v.bytes, bytes, lost, recovered };
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
    if (holder.keys.allow(minInterval)) this.needKey = true;
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

  /// The frame clock (µs, the unit of frame timestamps) now: viewers place
  /// capture times with it (PeerClock). Estimated from the shortest gap seen
  /// between a frame's timestamp and the moment it was read.
  frameClock() { return this.tsLag == null ? null : performance.now() * 1000 - this.tsLag; }

  async #encode(frame) {
    const lag = performance.now() * 1000 - frame.timestamp;
    this.tsLag = Math.min(this.tsLag ?? lag, lag);
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
    this.gop = { frames: [], rest: 0 }; // frames from before do not lead into the new ones
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
    this.#cache(data, key, chunk.timestamp);
    for (const v of this.viewers.values()) this.#sendTo(v, data, key, chunk.timestamp);
  }

  #cache(data, key, ts) {
    const g = this.gop;
    if (key) { g.frames = []; g.rest = 0; }
    else if (!g.frames.length) return;
    else g.rest += data.byteLength;
    g.frames.push({ data, key, ts, config: this.config });
    if (g.rest > GOP_MAX_BYTES || g.frames.length > GOP_MAX_FRAMES) { g.frames = []; g.rest = 0; }
  }

  #sendTo(v, data, key, ts, config = this.config) {
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
    const parity = (v.fecUntil ?? 0) > performance.now();
    for (const buf of packetize(v.seq++, data, key, ts, config, parity)) {
      try { ch.send(buf); } catch { v.needKey = true; return; }
    }
    if (key) { v.needKey = false; v.lastKey = { size: data.byteLength, at: performance.now() }; }
    v.frames++;
    v.bytes += data.byteLength;
  }
}

/* ---------------- Viewer ---------------- */

export class TurboReceiver {
  constructor(channel, { requestKey, onUnsupported, report, ping, render = turboSupport().render }) {
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
    if (!this.writer) {
      this.canvas = new CanvasView();
      // Painted inside the animation frame that shows it.
      this.canvas.onPaint = (frame) => this.displayed(frame.timestamp, Date.now());
    }
    // Glass to glass: capture (streamer's clock) to display (here).
    this.clock = ping ? new PeerClock(ping) : null;
    this.times = new Map(); // timestamp -> { at, done, decode, output } (performance.now())
    this.glass = [];        // recent capture-to-display samples (ms)
    // H.264 goes to the decoder as it travels (Annex B); a decoder that refuses
    // it before decoding anything gets length-prefixed NAL units and an avcC
    // description instead (?avc=avcc forces that, for testing).
    this.avcc = /[?&]avc=avcc\b/.test(globalThis.location?.search ?? "");
    this.decodedAny = false;
    this.sinkSkips = 0;
    this.behindSince = null;
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
      decodeQueue: 0, decodeDrops: 0, renderDrops: 0, recovered: 0,
      glassMs: null, arriveMs: null, displayMs: null, clockRtt: null,
    };
    // fec: this viewer rebuilds fragments from parity packets. The rest is
    // for the streamer's log (where a stream froze, and why).
    this.reportTimer = report ? setInterval(() => {
      const st = this.stats;
      report({ bytes: this.payload, lost: st.lost, recovered: st.recovered, fec: 1, decodeDrops: st.decodeDrops, renderDrops: st.renderDrops, keyRequests: st.keyRequests, glassMs: st.glassMs });
    }, REPORT_MS) : 0;
    this.payload = 0; // video data only, to compare with what was sent
    this.onUnsupported = onUnsupported;
    this.hold = HOLD_MS;
    const keys = new KeyBackoff();
    this.requestKey = () => {
      if (!keys.allow(VIEWER_KEY_MS)) return;
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
    this.clock?.close();
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
    const parity = !!(flags & 4);
    if (parity ? idx - count >= fecGroups(count) : idx >= count) return;

    let f = this.frames.get(seq);
    if (!f) {
      f = { parts: new Array(count), parity: new Array(fecGroups(count)), total: 0, got: 0, size: 0, key: !!(flags & 1), ts, at: performance.now(), done: 0, config: null };
      this.frames.set(seq, f);
    }
    if (f.done || (parity ? f.parity[idx - count] : f.parts[idx])) return;
    let off = HEADER;
    if (flags & 2) {
      if (!f.config) try { f.config = JSON.parse(new TextDecoder().decode(new Uint8Array(buf, HEADER, cfgLen))); } catch {}
      off += cfgLen;
    }
    if (parity) {
      if (buf.byteLength < off + 4) return;
      f.total = new DataView(buf, off, 4).getUint32(0, true);
      f.parity[idx - count] = new Uint8Array(buf, off + 4);
    } else {
      f.parts[idx] = new Uint8Array(buf, off);
      this.payload += buf.byteLength - off;
      f.got++;
      f.size += f.parts[idx].byteLength;
    }
    if (f.got < f.parts.length) this.#recover(f);
    if (f.got === f.parts.length) {
      f.done = performance.now();
      this.stats.assemblyMs = ema(this.stats.assemblyMs, f.done - f.at);
    }
    this.#drain();
  }

  // Rebuilds the one fragment missing from a parity group, if that is all.
  #recover(f) {
    const groups = f.parity.length;
    for (let g = 0; g < groups; g++) {
      const p = f.parity[g];
      if (!p) continue;
      let missing = -1, n = 0;
      for (let i = g; i < f.parts.length; i += groups) if (!f.parts[i]) { missing = i; n++; }
      if (n !== 1) continue;
      const len = missing === f.parts.length - 1 ? f.total - FRAG * (f.parts.length - 1) : FRAG;
      if (len <= 0 || len > p.length) continue;
      const x = p.slice(0, len);
      for (let i = g; i < f.parts.length; i += groups) {
        if (i === missing) continue;
        const b = f.parts[i];
        for (let k = 0; k < Math.min(len, b.length); k++) x[k] ^= b[k];
      }
      f.parts[missing] = x;
      f.got++;
      f.size += len;
      this.stats.recovered++;
    }
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
      const now = performance.now();
      if (queue <= MAX_DECODE_QUEUE) this.behindSince = null;
      else this.behindSince ??= now;
      if (!f.key && this.behindSince !== null && now - this.behindSince > DECODE_BEHIND_MS) {
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
      if (this.times.size > 120) this.times.clear();
      this.times.set(f.ts, { at: f.at, done: f.done, decode: performance.now(), output: 0 });
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
    const times = this.times.get(frame.timestamp);
    if (times) times.output = performance.now();
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

  /// The streamer answered a ping.
  pong(msg) { this.clock?.pong(msg); }

  /// A frame (its timestamp, µs) reached the screen at `at` (wall clock, ms):
  /// the canvas when it paints, the page's video frame callback otherwise.
  displayed(ts, at) {
    const times = this.times.get(ts);
    this.times.delete(ts);
    const captured = this.clock?.captureTime(ts);
    this.stats.clockRtt = this.clock?.rtt ?? null;
    if (captured == null || !times?.output) return;
    const glass = at - captured;
    if (glass < 0 || glass > 5000) return; // clocks not settled yet
    this.glass.push(glass);
    if (this.glass.length > 120) this.glass.shift();
    this.stats.glassMs = median(this.glass);
    // First packet in (capture, encoding, sending and the network), and from
    // the decoder's output to the screen.
    this.stats.arriveMs = ema(this.stats.arriveMs, wall(times.at) - captured);
    this.stats.displayMs = ema(this.stats.displayMs, at - wall(times.output));
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
function median(v) { const s = [...v].sort((a, b) => a - b); return s[s.length >> 1]; }
