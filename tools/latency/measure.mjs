#!/usr/bin/env node
// Glass-to-glass latency harness for Telinha.
//
//   node measure.mjs --app <dir> [--server domain] [--seconds 20] [--relay] [--turbo] [--label name] [--warmup 3] [--headed]
//
// --nativo <binary> puts the native app (native/) in place of the streaming Chrome: it
// joins the channel via the link and sends its own test screen (same time grid).
// Requires --server. E.g.: --nativo ../../native/target/release/telinha
//
// --server points the app at the signaling/TURN server (deploy/). Without it,
// the app served on localhost uses public PeerJS, STUN only, and --relay does not connect.
//
// Flow: serve <dir> on a free localhost port, open two isolated browser contexts.
//   A ("viewer") creates the channel and only watches.
//   B ("sender") joins A's channel by code and shares a synthetic screen.
// The synthetic screen is a 1280x720 canvas.captureStream(60) that, every frame,
// draws a moving textured background plus Date.now() encoded as a 16x4 grid of
// 40px black/white squares (marker + 44-bit timestamp + 8-bit frame counter + CRC-8).
// The viewer samples every presented frame of <video id="feature"> with
// requestVideoFrameCallback, decodes the grid and records
//   latency = displayTime(epoch) - encodedTimestamp
// Prints exactly one JSON line on stdout; progress/diagnostics go to stderr.
//
// Display time: performance.timeOrigin + metadata.expectedDisplayTime, corrected onto the
// wall clock with offset = Date.now() - (timeOrigin + performance.now()) sampled in the same
// callback (the sender stamps Date.now(); the monotonic and wall clocks drift apart over a
// long-lived page). The decoded pixels come from `new VideoFrame(video)`, i.e. the element's
// *current* frame; if the main thread was late that can already be a newer frame than the one
// the metadata describes, so frames whose VideoFrame.timestamp != metadata.mediaTime are
// skipped (counted in extra.skippedStaleMetadata). extra.callbackLatency is the same thing
// measured with Date.now() at callback entry, as a cross-check. In headless Chrome
// expectedDisplayTime ~= the callback's `now`, so the two agree within ~1 ms.
//
// Latency is quantised in ~16.7 ms steps: sender canvas (rAF) and viewer compositor tick on
// the same vsync source, so differences below about one frame between runs are noise.
//
// Flags: --warmup N (default 3 s after the first decoded frame). Chrome's bandwidth estimator
// takes ~10-12 s to ramp to 1280x720, so with the default warm-up the window includes the
// ramp; use --warmup 15 for steadier A/B comparisons. --headed runs a visible Chrome (may use
// hardware codecs). Chrome is told to hold a fake mic track in both pages purely so getStats()
// exposes encoderImplementation/decoderImplementation (gated on active capture).

import http from "node:http";
import fs from "node:fs";
import path from "node:path";
import puppeteer from "puppeteer";

/* ---------------- args ---------------- */

const argv = process.argv.slice(2);
const opt = { app: null, server: null, native: null, seconds: 20, relay: false, turbo: false, label: null, warmup: 3, headed: false, retries: 3 };
for (let i = 0; i < argv.length; i++) {
  const a = argv[i];
  if (a === "--app") opt.app = argv[++i];
  else if (a === "--server") opt.server = argv[++i];
  else if (a === "--nativo") opt.native = argv[++i];
  else if (a === "--seconds") opt.seconds = Number(argv[++i]);
  else if (a === "--warmup") opt.warmup = Number(argv[++i]);
  else if (a === "--label") opt.label = argv[++i];
  else if (a === "--relay") opt.relay = true;
  else if (a === "--turbo") opt.turbo = true; // the app's "minimum delay" mode (WebCodecs)
  else if (a === "--headed") opt.headed = true;
  else if (a === "--retries") opt.retries = Number(argv[++i]);
  else if (a === "--foto") opt.photo = argv[++i]; // saves the last received frame (PNG, full size)
  else { console.error(`unknown arg ${a}`); process.exit(2); }
}
if (!opt.app) { console.error("usage: node measure.mjs --app <dir> [--seconds 20] [--relay] [--label name]"); process.exit(2); }
opt.app = path.resolve(opt.app);
if (!fs.existsSync(path.join(opt.app, "index.html"))) { console.error(`no index.html in ${opt.app}`); process.exit(2); }
opt.label ??= path.basename(opt.app) + (opt.relay ? "-relay" : "");

const log = (...a) => console.error(`[${((Date.now() - T0) / 1000).toFixed(1)}s]`, ...a);
const T0 = Date.now();
const wait = (ms) => new Promise((r) => setTimeout(r, ms));

/* ---------------- static server ---------------- */

const MIME = {
  ".html": "text/html; charset=utf-8", ".js": "text/javascript; charset=utf-8", ".mjs": "text/javascript; charset=utf-8",
  ".css": "text/css; charset=utf-8", ".json": "application/json", ".svg": "image/svg+xml", ".png": "image/png",
  ".jpg": "image/jpeg", ".jpeg": "image/jpeg", ".webp": "image/webp", ".ico": "image/x-icon", ".woff2": "font/woff2",
  ".webmanifest": "application/manifest+json", ".txt": "text/plain; charset=utf-8",
};
function serve(root) {
  const server = http.createServer((req, res) => {
    let p = decodeURIComponent(new URL(req.url, "http://x").pathname);
    if (p.endsWith("/")) p += "index.html";
    const file = path.join(root, path.normalize(p));
    if (!file.startsWith(root)) { res.writeHead(403).end(); return; }
    fs.readFile(file, (err, buf) => {
      if (err) { res.writeHead(404).end("not found"); return; }
      res.writeHead(200, { "content-type": MIME[path.extname(file).toLowerCase()] || "application/octet-stream", "cache-control": "no-store" });
      res.end(buf);
    });
  });
  return new Promise((resolve) => server.listen(0, "127.0.0.1", () => resolve(server)));
}

/* ---------------- injected: RTCPeerConnection wrapper (both pages) ---------------- */

function installPcWrapper(relay) {
  const Native = window.RTCPeerConnection;
  if (!Native || Native.__wrapped) return;
  window.__pcs = [];
  class Wrapped extends Native {
    constructor(cfg = {}, ...rest) {
      super(relay ? { ...cfg, iceTransportPolicy: "relay" } : cfg, ...rest);
      window.__pcs.push(this);
    }
  }
  Wrapped.__wrapped = true;
  window.RTCPeerConnection = Wrapped;
  if (window.webkitRTCPeerConnection) window.webkitRTCPeerConnection = Wrapped;
}

/* ---------------- injected: synthetic screen (sender page) ---------------- */

function installFakeScreen() {
  const W = 1280, H = 720;
  // Code grid geometry (must match the decoder below).
  const CELL = 40, COLS = 16, ROWS = 4, GX = 48, GY = 48, PAD = 16;
  const MARKER = [1, 0, 1, 1];

  function crc8(bits) {
    let crc = 0;
    for (const b of bits) {
      const fb = ((crc >> 7) & 1) ^ b;
      crc = (crc << 1) & 0xff;
      if (fb) crc ^= 0x07;
    }
    return crc;
  }
  function encode(ts, counter) {
    const bits = [...MARKER];
    for (let i = 43; i >= 0; i--) bits.push(Math.floor(ts / 2 ** i) % 2);
    for (let i = 7; i >= 0; i--) bits.push((counter >> i) & 1);
    const c = crc8(bits.slice(4));
    for (let i = 7; i >= 0; i--) bits.push((c >> i) & 1);
    return bits; // 64 bits
  }

  let stream = null;
  window.__sender = { drawn: 0, startedAt: 0 };

  function makeStream() {
    const c = document.createElement("canvas");
    c.width = W; c.height = H;
    const g = c.getContext("2d", { alpha: false });
    // Textured tile, scrolled every frame so the encoder has real texture+motion to code.
    // Seeded (deterministic across runs) and mostly low-frequency: per-pixel white noise is
    // incompressible and makes VP8's frame dropper / BWE dominate the result, which real
    // screen content does not do. 32x32 random blocks upscaled with smoothing + light grain.
    let seed = 12345;
    const rnd = () => ((seed = (seed * 1103515245 + 12345) >>> 0) / 2 ** 32);
    const small = document.createElement("canvas"); small.width = small.height = 32;
    const sg = small.getContext("2d");
    const simg = sg.createImageData(32, 32);
    for (let i = 0; i < simg.data.length; i += 4) {
      const v = rnd() * 255;
      simg.data[i] = v; simg.data[i + 1] = v * 0.8; simg.data[i + 2] = 255 - v; simg.data[i + 3] = 255;
    }
    sg.putImageData(simg, 0, 0);
    const tile = document.createElement("canvas"); tile.width = tile.height = 256;
    const tg = tile.getContext("2d");
    tg.imageSmoothingEnabled = true; tg.imageSmoothingQuality = "high";
    tg.drawImage(small, 0, 0, 256, 256);
    const img = tg.getImageData(0, 0, 256, 256);
    for (let i = 0; i < img.data.length; i += 4) {
      const n = (rnd() - 0.5) * 40;
      img.data[i] += n; img.data[i + 1] += n; img.data[i + 2] += n;
    }
    tg.putImageData(img, 0, 0);
    const pattern = g.createPattern(tile, "repeat");
    let n = 0;

    function draw() {
      n++;
      // moving gradient
      const grad = g.createLinearGradient((n * 7) % W, 0, W - ((n * 5) % W), H);
      grad.addColorStop(0, `hsl(${(n * 2) % 360} 70% 40%)`);
      grad.addColorStop(1, `hsl(${(n * 2 + 160) % 360} 70% 55%)`);
      g.fillStyle = grad; g.fillRect(0, 0, W, H);
      // scrolling noise texture over part of the frame
      g.save();
      g.globalAlpha = 0.45;
      g.translate((n * 6) % 256, (n * 3) % 256);
      g.fillStyle = pattern; g.fillRect(-256, 260 - 256, W + 256, 460);
      g.restore();
      // moving text and shapes
      g.fillStyle = "#fff"; g.font = "bold 56px sans-serif";
      g.fillText(`Telinha latency probe  #${n}`, 60 + ((n * 4) % 400), 330 + Math.sin(n / 15) * 40);
      g.font = "28px monospace";
      for (let r = 0; r < 6; r++) g.fillText(`line ${r} ${(n * 37 + r * 101).toString(36).padStart(8, "0")} ${"=".repeat((n + r * 7) % 30)}`, 60, 440 + r * 42);
      g.beginPath(); g.arc(W / 2 + Math.cos(n / 20) * 400, H / 2 + Math.sin(n / 13) * 250, 50, 0, Math.PI * 2);
      g.fillStyle = "#ffe14d"; g.fill();

      // timestamp grid, last, so it is taken as late as possible before capture
      const ts = Date.now();
      const bits = encode(ts, n & 0xff);
      g.fillStyle = "#000";
      g.fillRect(GX - PAD, GY - PAD, COLS * CELL + 2 * PAD, ROWS * CELL + 2 * PAD);
      g.fillStyle = "#fff";
      for (let i = 0; i < bits.length; i++) {
        if (bits[i]) g.fillRect(GX + (i % COLS) * CELL, GY + Math.floor(i / COLS) * CELL, CELL, CELL);
      }
      window.__sender.drawn = n;
      requestAnimationFrame(draw);
    }
    draw();
    window.__sender.startedAt = Date.now();
    return c.captureStream(60);
  }

  const fake = async () => {
    if (!stream || stream.getVideoTracks().every((t) => t.readyState === "ended")) stream = makeStream();
    return stream.clone();
  };
  if (navigator.mediaDevices) {
    navigator.mediaDevices.getDisplayMedia = fake;
  }
}

/* ---------------- injected: decoder (viewer page) ---------------- */

function startDecoder() {
  const CELL = 40, COLS = 16, ROWS = 4, GX = 48, GY = 48, SRC_W = 1280;
  const MARKER = [1, 0, 1, 1];
  const RW = COLS * CELL, RH = ROWS * CELL;
  const video = document.getElementById("feature");
  const cv = document.createElement("canvas"); cv.width = RW; cv.height = RH;
  const g = cv.getContext("2d", { willReadFrequently: true });

  function crc8(bits) {
    let crc = 0;
    for (const b of bits) {
      const fb = ((crc >> 7) & 1) ^ b;
      crc = (crc << 1) & 0xff;
      if (fb) crc ^= 0x07;
    }
    return crc;
  }

  const S = (window.__lat = { recording: false, samples: [], fails: 0, failReasons: {}, mismatch: 0, firstOkAt: 0, total: 0, sizes: {} });
  const fail = (why) => { if (S.recording) { S.fails++; S.failReasons[why] = (S.failReasons[why] || 0) + 1; } };

  function onFrame(now, md) {
    const cbWall = Date.now();
    // Map the page's monotonic clock (performance.now timebase) onto the wall clock used by the sender.
    const offset = cbWall - (performance.timeOrigin + performance.now());
    const toWall = (t) => (t == null ? null : performance.timeOrigin + t + offset);
    try {
      const vw = video.videoWidth, vh = video.videoHeight;
      if (!vw || !vh) { fail("nosize"); return; }
      const k = vw / SRC_W; // the encoder may downscale (degradationPreference maintain-framerate)
      S.sizes[`${vw}x${vh}`] = (S.sizes[`${vw}x${vh}`] || 0) + 1;
      // Grab the element's *current* frame as a VideoFrame. If the main thread was late, this can
      // already be a newer frame than the one `md` describes; compare timestamps to detect that.
      let vf = null, match = null;
      try { vf = new VideoFrame(video); match = Math.abs(vf.timestamp - md.mediaTime * 1e6) < 1000; } catch {}
      g.drawImage(vf || video, GX * k, GY * k, RW * k, RH * k, 0, 0, RW, RH);
      vf?.close();
      const d = g.getImageData(0, 0, RW, RH).data;
      const bits = [];
      let ambiguous = 0;
      for (let i = 0; i < COLS * ROWS; i++) {
        const cx = (i % COLS) * CELL + CELL / 2, cy = Math.floor(i / COLS) * CELL + CELL / 2;
        // Average a 9x9 patch at the cell centre (robust to ringing/blocking at edges).
        let sum = 0, cnt = 0;
        for (let y = cy - 4; y <= cy + 4; y += 2) for (let x = cx - 4; x <= cx + 4; x += 2) {
          const o = (y * RW + x) * 4;
          sum += 0.299 * d[o] + 0.587 * d[o + 1] + 0.114 * d[o + 2]; cnt++;
        }
        const l = sum / cnt;
        if (l > 70 && l < 185) ambiguous++;
        bits.push(l >= 128 ? 1 : 0);
      }
      if (MARKER.some((b, i) => bits[i] !== b)) { fail("marker"); return; }
      let crc = 0;
      for (let i = 56; i < 64; i++) crc = (crc << 1) | bits[i];
      if (crc8(bits.slice(4, 56)) !== crc) { fail("crc"); return; }
      let ts = 0;
      for (let i = 4; i < 48; i++) ts = ts * 2 + bits[i];
      let counter = 0;
      for (let i = 48; i < 56; i++) counter = (counter << 1) | bits[i];
      if (Math.abs(cbWall - ts) > 10_000) { fail("range"); return; }
      if (!S.firstOkAt) S.firstOkAt = cbWall;
      S.total++;
      if (!S.recording) return;
      // The decoded frame is not the one md describes: its display time is unknown, skip it.
      if (match === false) { S.mismatch++; return; }
      const disp = toWall(md.expectedDisplayTime);
      S.samples.push({
        ts, counter, ambiguous, at: cbWall,
        disp: disp - ts,                          // primary: expected display time - encoded ts
        cb: cbWall - ts,                           // callback wall clock - encoded ts
        recv: md.receiveTime != null ? toWall(md.receiveTime) - ts : null, // last packet received - ts
        present: md.presentationTime != null ? toWall(md.presentationTime) - ts : null,
        presented: md.presentedFrames,
        match,
        w: vw,
      });
    } catch (e) {
      fail("exception:" + e.message);
    } finally {
      video.requestVideoFrameCallback(onFrame);
    }
  }
  video.requestVideoFrameCallback(onFrame);
}

/* ---------------- injected: stats snapshot ---------------- */

async function snapshotStats() {
  const out = { inbound: [], outbound: [], pairs: [], sources: [] };
  for (const pc of window.__pcs || []) {
    if (pc.connectionState === "closed") continue;
    let st;
    try { st = await pc.getStats(); } catch { continue; }
    st.forEach((r) => {
      if (r.type === "inbound-rtp" && r.kind === "video") {
        out.inbound.push({
          codec: r.codecId ? st.get(r.codecId)?.mimeType : null,
          decoder: r.decoderImplementation ?? null,
          powerEfficientDecoder: r.powerEfficientDecoder ?? null,
          fps: r.framesPerSecond ?? null,
          framesDecoded: r.framesDecoded ?? 0,
          framesDropped: r.framesDropped ?? 0,
          jitterBufferDelay: r.jitterBufferDelay ?? 0,
          jitterBufferEmittedCount: r.jitterBufferEmittedCount ?? 0,
          jitterBufferTargetDelay: r.jitterBufferTargetDelay ?? 0,
          totalDecodeTime: r.totalDecodeTime ?? 0,
          totalProcessingDelay: r.totalProcessingDelay ?? 0,
          frameWidth: r.frameWidth, frameHeight: r.frameHeight,
          bytesReceived: r.bytesReceived, packetsLost: r.packetsLost, nackCount: r.nackCount, freezeCount: r.freezeCount,
          jitter: r.jitter,
        });
      }
      if (r.type === "outbound-rtp" && r.kind === "video") {
        out.outbound.push({
          codec: r.codecId ? st.get(r.codecId)?.mimeType : null,
          encoder: r.encoderImplementation ?? null,
          qualityLimitationReason: r.qualityLimitationReason ?? null,
          qualityLimitationDurations: r.qualityLimitationDurations ?? null,
          fps: r.framesPerSecond ?? null,
          framesEncoded: r.framesEncoded ?? 0,
          totalEncodeTime: r.totalEncodeTime ?? 0,
          frameWidth: r.frameWidth, frameHeight: r.frameHeight,
          bytesSent: r.bytesSent, targetBitrate: r.targetBitrate,
        });
      }
      if (r.type === "media-source" && r.kind === "video") out.sources.push({ fps: r.framesPerSecond ?? null, frames: r.frames ?? 0 });
      if (r.type === "transport" && r.selectedCandidatePairId) {
        const p = st.get(r.selectedCandidatePairId);
        if (p) out.pairs.push({
          local: st.get(p.localCandidateId)?.candidateType, remote: st.get(p.remoteCandidateId)?.candidateType,
          rttMs: p.currentRoundTripTime != null ? p.currentRoundTripTime * 1000 : null,
        });
      }
    });
  }
  return out;
}

/* ---------------- helpers ---------------- */

function pct(sorted, p) {
  if (!sorted.length) return null;
  const idx = (sorted.length - 1) * p;
  const lo = Math.floor(idx), hi = Math.ceil(idx);
  return sorted[lo] + (sorted[hi] - sorted[lo]) * (idx - lo);
}
const r1 = (x) => (x == null || Number.isNaN(x) ? null : Math.round(x * 10) / 10);
function summarize(values) {
  const v = values.filter((x) => x != null && Number.isFinite(x)).sort((a, b) => a - b);
  if (!v.length) return { n: 0, p50: null, p95: null, p99: null, mean: null, min: null, max: null };
  return {
    n: v.length, p50: r1(pct(v, 0.5)), p95: r1(pct(v, 0.95)), p99: r1(pct(v, 0.99)),
    mean: r1(v.reduce((a, b) => a + b, 0) / v.length), min: r1(v[0]), max: r1(v.at(-1)),
  };
}
const pickVideo = (list, key) => list.slice().sort((a, b) => (b[key] || 0) - (a[key] || 0))[0] || null;

/* ---------------- main ---------------- */

const server = await serve(opt.app);
const URL_ = `http://localhost:${server.address().port}/${opt.server ? `?servidor=${encodeURIComponent(opt.server)}` : ""}`;
log(`serving ${opt.app} at ${URL_} relay=${opt.relay}`);

const browser = await puppeteer.launch({
  headless: !opt.headed,
  args: [
    "--autoplay-policy=no-user-gesture-required",
    "--use-fake-ui-for-media-stream",
    "--use-fake-device-for-media-stream",
    "--disable-background-timer-throttling",
    "--disable-renderer-backgrounding",
    "--disable-backgrounding-occluded-windows",
  ],
});

let exitCode = 0;
const hardTimeout = setTimeout(() => { log("hard timeout"); exitCode = 1; cleanup().then(() => process.exit(1)); }, (opt.seconds + opt.warmup + 90 * opt.retries + 60) * 1000);

async function cleanup() {
  try { nativeProc?.kill("SIGINT"); } catch {}
  try { await browser.close(); } catch {}
  try { server.close(); } catch {}
}

// The app silently falls back to the public PeerJS server (STUN only) when
// the signaling server's /api/ice does not answer in time. A run on the fallback is not a
// measurement of the real infra (and --relay cannot connect at all), so detect it and retry.
let attemptState = null;
const FALLBACK_RE = /servidor próprio indispon|PeerJS público|fallback/i;

async function mkPage(name, { sender }) {
  const ctx = await browser.createBrowserContext();
  attemptState.contexts.push(ctx);
  const st = attemptState;
  const p = await ctx.newPage();
  p.on("console", (m) => { if (FALLBACK_RE.test(m.text())) st.fallback = `${name}: ${m.text()}`; });
  p.on("requestfailed", (r) => { if (r.url().includes("/api/ice")) st.fallback = `${name}: ICE fetch failed (${r.failure()?.errorText})`; });
  p.on("response", (r) => { if (r.url().includes("/api/ice") && !r.ok()) st.fallback = `${name}: ICE fetch HTTP ${r.status()}`; });
  await p.setViewport({ width: 1280, height: 800 });
  await p.evaluateOnNewDocument(installPcWrapper, opt.relay);
  if (sender) await p.evaluateOnNewDocument(installFakeScreen);
  // same quality in both modes, to compare only the transport
  await p.evaluateOnNewDocument((turbo) => {
    try { localStorage.setItem("telinha:quality", JSON.stringify({ res: "original", fps: 60, priority: "fluidez", turbo })); } catch {}
  }, opt.turbo);
  p.on("console", (m) => { if (m.type() === "error" || m.type() === "warn") log(name, m.type(), m.text()); });
  p.on("pageerror", (e) => log(name, "PAGEERROR", e.message));
  await p.goto(URL_, { waitUntil: "load" });
  // Chrome only exposes encoderImplementation/decoderImplementation in getStats() to documents
  // that are currently capturing camera/mic ("exposing hardware is allowed"). Hold a fake mic
  // track (fake device, auto-accepted) for the whole run; the app itself never sees it.
  const gum = await p.evaluate(async () => {
    try { window.__gum = await navigator.mediaDevices.getUserMedia({ audio: true }); return "ok"; }
    catch (e) { return e.name; }
  });
  if (gum !== "ok") log(name, "getUserMedia for stats exposure failed:", gum);
  await p.waitForSelector("#name");
  await p.type("#name", name);
  return p;
}

/* Native app as the streamer: its side's numbers do not exist in the page. */
let nativeProc = null;
async function nativeSender(viewer, code) {
  const { spawn } = await import("node:child_process");
  if (!opt.server) throw new Error("--nativo requires --server");
  nativeProc = spawn(opt.native, ["--entrar", `https://${opt.server}/#${code}`, "--segundos", String(opt.seconds + opt.warmup + 90)], {
    env: { TELINHA_FONTE_TESTE: "1", ...process.env, TELINHA_NOME: "Nativo" },
    stdio: ["ignore", "ignore", "pipe"],
  });
  const nativeLog = process.env.NATIVO_LOG ? (await import("node:fs")).createWriteStream(process.env.NATIVO_LOG) : null;
  nativeProc.stderr.on("data", (d) => nativeLog?.write(d));
  nativeProc.stderr.on("data", (d) => { const t = d.toString().replace(/\x1b\[[0-9;]*m/g, ""); if (/escolhido|ERROR|WARN/.test(t)) log("nativo:", t.trim().slice(0, 160)); });
  await viewer.waitForFunction(() => {
    const v = document.querySelector("#feature");
    return v.videoWidth > 0 && v.readyState >= 2;
  }, { timeout: 30000, polling: 200 });
  log("viewer receiving video from native app");
  await viewer.evaluate(startDecoder);
  await viewer.waitForFunction(() => window.__lat.firstOkAt > 0, { timeout: 15000, polling: 100 });
  return { evaluate: async (fn) => (fn === snapshotStats ? { inbound: [], outbound: [], sources: [], pairs: [] } : null) };
}

async function setup() {
  const checkFallback = () => { if (attemptState.fallback) throw new Error(`app fell back off the signaling server (${attemptState.fallback})`); };
  // A: creates the channel, watches.
  const viewer = await mkPage("Viewer", { sender: false });
  await viewer.click("#create");
  await viewer.waitForSelector("#room:not([hidden])", { timeout: 20000 });
  await viewer.waitForFunction(() => /^\d{4}$/.test(document.querySelector("#channelDigits")?.textContent || ""), { timeout: 10000 });
  const code = await viewer.$eval("#channelDigits", (e) => e.textContent);
  checkFallback();
  log("channel", code);

  if (opt.native) return { viewer, sender: await nativeSender(viewer, code) };

  // B: guest, joins by code, shares.
  const sender = await mkPage("Sender", { sender: true });
  await sender.type("#code", code);
  await sender.waitForFunction(() => !document.querySelector("#tune").disabled, { timeout: 5000 });
  await sender.click("#tune");
  await sender.waitForSelector("#room:not([hidden])", { timeout: 20000 });
  // wait until the sender knows about the viewer (roster arrived), otherwise there is no one to call
  await sender.waitForFunction(() => document.querySelectorAll("#people li").length >= 2, { timeout: 15000 });
  checkFallback();
  log("joined; sharing");
  // The room opens with a ~560ms clip-path animation and the lobby fades out over it;
  // click only once the lobby is gone, and retry until the share button turns "live".
  await sender.waitForSelector("#lobby", { hidden: true, timeout: 10000 });
  for (let i = 0; ; i++) {
    await sender.click("#shareBtn");
    const live = await sender.waitForFunction(() => document.querySelector("#shareBtn").classList.contains("live"), { timeout: 3000 }).then(() => true, () => false);
    if (live) break;
    if (i >= 4) throw new Error("share button never went live");
    log("share click did not take, retrying");
  }

  await viewer.waitForFunction(() => {
    const v = document.querySelector("#feature");
    return v.videoWidth > 0 && v.readyState >= 2 && !document.querySelector("#stage").classList.contains("idle");
  }, { timeout: 30000, polling: 200 }).catch(async (e) => {
    log("viewer state", JSON.stringify(await viewer.evaluate(() => {
      const v = document.querySelector("#feature");
      return { w: v.videoWidth, rs: v.readyState, paused: v.paused, stage: document.querySelector("#stage").className, pcs: (window.__pcs || []).map((p) => p.connectionState + "/" + p.getReceivers().length) };
    })));
    log("sender state", JSON.stringify(await sender.evaluate(() => ({ s: window.__sender, pcs: (window.__pcs || []).map((p) => p.connectionState + "/" + p.getSenders().length) }))));
    throw e;
  });
  log("viewer receiving video");

  await viewer.evaluate(startDecoder);
  await viewer.waitForFunction(() => window.__lat.firstOkAt > 0, { timeout: 15000, polling: 100 }).catch(async () => {
    const shot = path.join(path.dirname(new URL(import.meta.url).pathname), `fail-${opt.label}.png`);
    await viewer.screenshot({ path: shot });
    throw new Error(`no decodable frame within 15s (screenshot: ${shot})`);
  });
  return { viewer, sender };
}

try {
  let viewer, sender;
  for (let attempt = 1; ; attempt++) {
    attemptState = { contexts: [], fallback: null };
    try {
      ({ viewer, sender } = await setup());
      break;
    } catch (e) {
      log(`setup attempt ${attempt} failed: ${e.message.split("\n")[0]}${attemptState.fallback ? ` [fallback: ${attemptState.fallback}]` : ""}`);
      for (const c of attemptState.contexts) await c.close().catch(() => {});
      if (attempt >= opt.retries) throw e;
      await wait(2000);
    }
  }
  log(`first decoded frame; warming up ${opt.warmup}s`);
  await wait(opt.warmup * 1000);

  const vs0 = await viewer.evaluate(snapshotStats);
  const ss0 = await sender.evaluate(snapshotStats);
  const t0 = Date.now();
  await viewer.evaluate(() => { window.__lat.samples = []; window.__lat.fails = 0; window.__lat.mismatch = 0; window.__lat.failReasons = {}; window.__lat.recording = true; });
  log(`collecting ${opt.seconds}s`);
  await wait(opt.seconds * 1000);
  const lat = await viewer.evaluate(() => { window.__lat.recording = false; return window.__lat; });
  const elapsed = (Date.now() - t0) / 1000;
  const vs1 = await viewer.evaluate(snapshotStats);
  const ss1 = await sender.evaluate(snapshotStats);
  const drawn = await sender.evaluate(() => window.__sender);
  if (opt.photo) {
    const png = await viewer.evaluate(() => {
      const v = document.getElementById("feature");
      const c = document.createElement("canvas"); c.width = v.videoWidth; c.height = v.videoHeight;
      c.getContext("2d").drawImage(v, 0, 0);
      return c.toDataURL("image/png").split(",")[1];
    });
    (await import("node:fs")).writeFileSync(opt.photo, Buffer.from(png, "base64"));
    log("foto:", opt.photo);
  }

  const in0 = pickVideo(vs0.inbound, "framesDecoded"), in1 = pickVideo(vs1.inbound, "framesDecoded");
  const out0 = pickVideo(ss0.outbound, "framesEncoded"), out1 = pickVideo(ss1.outbound, "framesEncoded");
  const dEmit = in1 && in0 ? in1.jitterBufferEmittedCount - in0.jitterBufferEmittedCount : 0;
  const dJb = in1 && in0 ? in1.jitterBufferDelay - in0.jitterBufferDelay : 0;
  const dDec = in1 && in0 ? in1.framesDecoded - in0.framesDecoded : 0;
  const dEnc = out1 && out0 ? out1.framesEncoded - out0.framesEncoded : 0;

  const S = lat.samples;
  const disp = summarize(S.map((s) => s.disp));
  const cb = summarize(S.map((s) => s.cb));
  const recv = summarize(S.map((s) => s.recv));
  const pairs = [...vs1.pairs];

  const result = {
    label: opt.label,
    frames: S.length,
    decodeFailures: lat.fails,
    p50: disp.p50, p95: disp.p95, p99: disp.p99, mean: disp.mean, min: disp.min, max: disp.max,
    codec: in1?.codec ?? null,
    decoder: in1?.decoder ?? null,
    encoder: out1?.encoder ?? null,
    fps: in1?.fps ?? null,
    jitterBufferMsPerFrame: dEmit > 0 ? r1((dJb / dEmit) * 1000) : null,
    qualityLimitationReason: out1?.qualityLimitationReason ?? null,
    relay: opt.relay,
    turbo: opt.turbo,
    // extras
    extra: {
      presentedFps: r1(S.length / elapsed),
      callbackLatency: { p50: cb.p50, p95: cb.p95, mean: cb.mean, min: cb.min, max: cb.max },          // Date.now() in rVFC callback - ts
      tsToReceive: { p50: recv.p50, p95: recv.p95, mean: recv.mean },       // metadata.receiveTime - ts (capture->encode->network)
      decodeMsPerFrame: dDec > 0 ? r1(((in1.totalDecodeTime - in0.totalDecodeTime) / dDec) * 1000) : null,
      encodeMsPerFrame: dEnc > 0 ? r1(((out1.totalEncodeTime - out0.totalEncodeTime) / dEnc) * 1000) : null,
      jitterBufferTargetMs: in1 && dEmit > 0 ? r1(((in1.jitterBufferTargetDelay - in0.jitterBufferTargetDelay) / dEmit) * 1000) : null,
      recvResolution: in1 ? `${in1.frameWidth}x${in1.frameHeight}` : null,
      sendResolution: out1 ? `${out1.frameWidth}x${out1.frameHeight}` : null,
      sendFps: out1?.fps ?? null,
      sourceFps: (() => { const a = pickVideo(ss0.sources, "frames"), b = pickVideo(ss1.sources, "frames"); return a && b ? r1((b.frames - a.frames) / elapsed) : null; })(),
      encodedFps: dEnc > 0 ? r1(dEnc / elapsed) : null,
      sendKbps: out1 && out0 ? Math.round(((out1.bytesSent - out0.bytesSent) * 8) / elapsed / 1000) : null,
      framesDropped: in1 && in0 ? in1.framesDropped - in0.framesDropped : null,
      packetsLost: in1 && in0 ? in1.packetsLost - in0.packetsLost : null,
      freezes: in1 && in0 ? (in1.freezeCount ?? 0) - (in0.freezeCount ?? 0) : null,
      qualityLimitationDurations: out1?.qualityLimitationDurations ?? null,
      candidatePairs: pairs.map((p) => `${p.local}/${p.remote}${p.rttMs != null ? ` rtt=${Math.round(p.rttMs)}ms` : ""}`),
      failReasons: lat.failReasons,
      skippedStaleMetadata: lat.mismatch,
      unverifiedFrameMatch: S.filter((s) => s.match == null).length,
      // samples >= 15 s after the first decoded frame (past Chrome's BWE ramp-up)
      after15s: (() => { const a = summarize(S.filter((x) => x.at - lat.firstOkAt >= 15000).map((x) => x.disp)); return { n: a.n, p50: a.p50, p95: a.p95, mean: a.mean }; })(),
      p50FirstHalf: summarize(S.slice(0, S.length >> 1).map((s) => s.disp)).p50,
      p50SecondHalf: summarize(S.slice(S.length >> 1).map((s) => s.disp)).p50,
      ambiguousCellsMax: S.reduce((m, s) => Math.max(m, s.ambiguous), 0),
      senderCanvasFps: drawn ? r1(drawn.drawn / ((Date.now() - drawn.startedAt) / 1000)) : null,
      seconds: r1(elapsed),
    },
  };
  console.log(JSON.stringify(result));
} catch (e) {
  exitCode = 1;
  log("FAILED:", e.stack || e.message);
} finally {
  clearTimeout(hardTimeout);
  await cleanup();
  process.exit(exitCode);
}
