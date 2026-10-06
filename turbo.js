// Modo "atraso mínimo": vídeo por WebCodecs + canal de dados, no espírito do
// Sunshine/Moonlight.
//
// Quem transmite codifica cada quadro UMA vez (VideoEncoder em modo realtime,
// taxa constante, keyframe só quando alguém pede) e manda os mesmos pedaços
// para todo mundo por um canal sem ordem e com reenvio curto. Quem assiste
// remonta, decodifica com optimizeForLatency e entrega o quadro na hora para
// uma trilha de vídeo comum, sem buffer de espera.
//
// Pacote (little endian):
//   u8 0x54 | u8 flags (1 = keyframe, 2 = traz config) | u16 índice | u16 total
//   u16 tamanho da config | u32 sequência | f64 timestamp (µs) | [config JSON] | dados

const MAGIC = 0x54;
const HEADER = 20;
const FRAG = 16000;
const HOLD_MS = 150;        // quanto esperar por um quadro que ficou faltando
const KEY_INTERVAL_MS = 1000; // keyframe pedida por fila cheia: no máximo 1 por segundo
const VIEWER_KEY_MS = 500;    // keyframe pedida por quem perdeu um quadro
const REPORT_MS = 1000;       // relatório de quem assiste para quem transmite
// Codificadores de hardware (NVENC/QuickSync via Media Foundation, VAAPI)
// trabalham com alguns quadros em andamento; só descarta se a fila passar disso.
const MAX_ENCODE_QUEUE = 3;
const CHANNEL = { negotiated: true, id: 100, ordered: false, maxPacketLifeTime: HOLD_MS };

export function turboSupport() {
  return {
    send: typeof MediaStreamTrackProcessor === "function" && typeof VideoEncoder === "function",
    receive: typeof MediaStreamTrackGenerator === "function" && typeof VideoDecoder === "function",
  };
}

// Os dois lados criam o mesmo canal (negociado, id fixo) sobre a conexão já aberta.
export function openVideoChannel(pc) {
  const ch = pc.createDataChannel("telinha-video", CHANNEL);
  ch.binaryType = "arraybuffer";
  return ch;
}

/* ---------------- Quem transmite ---------------- */

// Preferência: H.264 na placa de vídeo, depois H.264 e VP8 por software.
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
    ...(codec.startsWith("avc1") ? { avc: { format: "annexb" } } : {}), // SPS/PPS em toda keyframe
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
    this.pending = new Map(); // timestamp -> instante do encode()
    this.stats = { encodeMs: null, frames: 0, captured: 0, skipped: 0 };
    this.reader = new MediaStreamTrackProcessor({ track }).readable.getReader();
    this.done = this.#loop();
  }

  addViewer(id, channel, { onCongested } = {}) {
    const v = { channel, seq: 0, needKey: true, frames: 0, bytes: 0, dropped: 0, drops: [], onCongested, last: null, strikes: 0 };
    this.viewers.set(id, v);
    const start = () => this.requestKey(0);
    if (channel.readyState === "open") start();
    else channel.addEventListener("open", start, { once: true });
  }

  removeViewer(id) { this.viewers.delete(id); }

  // Quem assiste conta o que chegou. O canal descarta em silêncio o que passa
  // do tempo de vida, então a fila daqui não mostra o congestionamento; o
  // relatório mostra. Três relatórios ruins seguidos: a pessoa volta pro WebRTC.
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

  requestKey(minInterval = KEY_INTERVAL_MS) {
    const now = performance.now();
    if (now - this.lastKeyAt < minInterval) return;
    this.lastKeyAt = now;
    this.needKey = true;
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
        console.warn("[turbo] codificação", err);
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
    // Fila do codificador cheia: descarta antes de codificar, como o Sunshine.
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
        error: (err) => { console.warn("[turbo] codificador", err); this.encoder = null; },
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
    const budget = Math.max(256e3, (this.bitrate / 8) * 0.3); // ~300 ms de vídeo na fila
    if (v.needKey && !key) {
      // espera a fila esvaziar antes de pedir a keyframe, senão ela também não cabe
      if (ch.bufferedAmount < budget / 2) this.requestKey();
      return;
    }
    if (ch.bufferedAmount > budget) {
      v.needKey = true;
      v.dropped++;
      const now = performance.now();
      v.drops.push(now);
      while (v.drops.length && now - v.drops[0] > 3000) v.drops.shift();
      if (v.drops.length > 20) v.onCongested?.(); // fila cheia o tempo todo: essa pessoa volta pro WebRTC
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
    if (key) v.needKey = false;
    v.frames++;
    v.bytes += data.byteLength;
  }
}

/* ---------------- Quem assiste ---------------- */

export class TurboReceiver {
  constructor(channel, { requestKey, onUnsupported, report }) {
    this.generator = new MediaStreamTrackGenerator({ kind: "video" });
    this.writer = this.generator.writable.getWriter();
    this.frames = new Map(); // seq -> quadro sendo montado
    this.expected = null;
    this.waitingKey = true;
    this.decoder = null;
    this.config = null;
    this.errors = 0;
    this.timer = 0;
    this.closed = false;
    this.pending = new Map(); // timestamp -> instante do decode()
    this.stats = { codec: null, width: 0, height: 0, frames: 0, bytes: 0, lost: 0, keyRequests: 0, assemblyMs: null, decodeMs: null };
    this.reportTimer = report ? setInterval(() => report({ bytes: this.payload, lost: this.stats.lost }), REPORT_MS) : 0;
    this.payload = 0; // só os dados de vídeo, para comparar com o que foi enviado
    this.onUnsupported = onUnsupported;
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

  get track() { return this.generator; }

  close() {
    this.closed = true;
    clearTimeout(this.timer);
    clearInterval(this.reportTimer);
    try { this.decoder?.close(); } catch {}
    this.writer.close().catch(() => {});
    this.generator.stop();
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
    if (this.expected !== null && seq < this.expected) return; // chegou tarde demais
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

  // Decodifica em ordem. Uma keyframe completa mais à frente pula a fila.
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
      this.#decode(f);
    }
    this.#arm();
  }

  #arm() {
    if (this.timer || this.closed) return;
    if (!this.frames.size && !this.waitingKey) return;
    this.timer = setTimeout(() => { this.timer = 0; this.#check(); }, HOLD_MS / 3);
  }

  // Quadro que não chegou a tempo vira perda: pede keyframe e segue sem ele.
  #check() {
    const now = performance.now();
    if (this.waitingKey) {
      this.requestKey();
    } else {
      const missing = this.frames.get(this.expected);
      const stale = [...this.frames.values()].some((f) => now - f.at > HOLD_MS);
      if (stale && !missing?.done) {
        this.stats.lost++;
        this.waitingKey = true;
        for (const [seq, f] of this.frames) if (!f.key) this.frames.delete(seq);
        this.requestKey();
      }
    }
    for (const [seq, f] of this.frames) if (now - f.at > 2000) this.frames.delete(seq); // lixo antigo
    this.#drain();
  }

  #decode(f) {
    try {
      const c = f.config;
      if (c && (!this.config || c.codec !== this.config.codec || c.w !== this.config.w || c.h !== this.config.h || this.decoder?.state !== "configured")) {
        this.#configure(c);
      }
      if (!this.decoder || this.decoder.state !== "configured") { this.waitingKey = true; return; }
      let data = f.parts[0];
      if (f.parts.length > 1) {
        data = new Uint8Array(f.size);
        let off = 0;
        for (const p of f.parts) { data.set(p, off); off += p.byteLength; }
      }
      if (this.pending.size > 120) this.pending.clear();
      this.pending.set(f.ts, performance.now());
      this.decoder.decode(new EncodedVideoChunk({ type: f.key ? "key" : "delta", timestamp: f.ts, data }));
    } catch (err) {
      this.#fail(err);
    }
  }

  #configure(c) {
    if (!this.decoder || this.decoder.state === "closed") {
      this.decoder = new VideoDecoder({ output: (frame) => this.#output(frame), error: (err) => this.#fail(err) });
    }
    this.decoder.configure({ codec: c.codec, codedWidth: c.w, codedHeight: c.h, optimizeForLatency: true });
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
    if (this.closed) { frame.close(); return; }
    this.writer.write(frame).catch(() => { try { frame.close(); } catch {} });
  }

  #fail(err) {
    if (this.closed) return;
    console.warn("[turbo] decodificação", err);
    try { this.decoder?.close(); } catch {}
    this.decoder = null;
    this.config = null;
    this.waitingKey = true;
    if (err?.name === "NotSupportedError" || ++this.errors > 5) { this.onUnsupported?.(); return; }
    this.requestKey();
  }
}

function ema(prev, v) { return prev == null ? v : prev * 0.9 + v * 0.1; }
