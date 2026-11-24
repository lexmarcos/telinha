// Telinha: screen sharing between friends over PeerJS.
//
// Whoever opens the channel becomes the "host": it registers the fixed id
// `telinha-canal-v1-NNNN` on the PeerJS signaling server and keeps the list of
// who is in the room. Everyone connects to the host over a data channel and
// receives that list. A sharer calls (peer.call) each person on the list
// directly, and viewers only answer. Media never goes through the host.
//
// Signaling and relay (TURN) live on the server that serves the site (deploy/
// folder). If it does not respond, or the app runs on localhost, it falls back
// to the public PeerJS server with STUN only. To test locally against the
// server, open with ?servidor=server.domain.

import { createStatsPanel } from "./stats.js";
import { turboSupport, hardwareDecoders, openVideoChannel, TurboSender, TurboReceiver } from "./turbo.js";

const SERVER = new URLSearchParams(location.search).get("servidor")
  ?? (/^(localhost|127\.0\.0\.1|\[::1\])$/.test(location.hostname) ? null : location.host);
// A signaling server on this machine (tools/latency/sinal.mjs) speaks plain HTTP.
const LOCAL_SERVER = /^(localhost|127\.0\.0\.1)(:\d+)?$/.test(SERVER ?? "");
const TURBO = turboSupport();
const DECODERS = hardwareDecoders().catch(() => []); // goes with "wc" in the roster
const PREFIX = "telinha-canal-v1-";
const MAX_BITRATE = 8_000_000;
// Everyone in a channel, the host included. Each viewer costs the streamer one
// more copy of the video and 4 or 5 work well (docs/how-it-works.md, Limits):
// this is more than twice that, and it is the most connections a stranger who
// guessed the number can make a streamer open.
const MAX_PEOPLE = 12;
// Connections to the host still opening at once. A real group joins one by one,
// about a second each; a burst past this is closed before it costs anything.
const MAX_JOINING = 6;
// Same wait a guest gives the host (connectTo).
const JOIN_TIMEOUT_MS = 15000;
// Faces kept drawn. Names are other people's and never stop changing.
const MAX_FACES = 64;
const $ = (id) => document.getElementById(id);
const reducedMotion = matchMedia("(prefers-reduced-motion: reduce)");
// Phones and tablets can watch but not share: no screen capture API there.
const CAN_SHARE = !!navigator.mediaDevices?.getDisplayMedia;
// A finger instead of a mouse: the controls need a tap to come back, there is no hover.
const touchOnly = matchMedia("(hover: none) and (pointer: coarse)");

const ui = {
  lobby: $("lobby"), tv: $("tv"), code: $("code"), name: $("name"), tune: $("tune"),
  create: $("create"), controls: $("controls"), lobbyError: $("lobbyError"), tvHint: $("tvHint"),
  room: $("room"), stage: $("stage"), feature: $("feature"), featureCanvas: $("featureCanvas"), grid: $("grid"),
  stageTools: $("stageTools"), gridBtn: $("gridBtn"), noticeSlot: $("noticeSlot"),
  volume: $("volume"), muteBtn: $("muteBtn"), pipBtn: $("pipBtn"), fullBtn: $("fullBtn"), rotateBtn: $("rotateBtn"), exitFsBtn: $("exitFsBtn"),
  emptyDigits: $("emptyDigits"),
  emptyTitle: $("emptyTitle"), emptyText: $("emptyText"),
  people: $("people"), peopleBtn: $("peopleBtn"), peoplePanel: $("peoplePanel"), roster: $("roster"),
  shareBtn: $("shareBtn"), leaveBtn: $("leaveBtn"),
  offair: $("offair"), offairText: $("offairText"), offairBtn: $("offairBtn"),
  toasts: $("toasts"),
  dock: document.querySelector(".dock"), qualityBtn: $("qualityBtn"), qualityLabel: $("qualityLabel"), qualityPanel: $("qualityPanel"),
};

const state = {
  peer: null,
  code: "",
  name: "",
  isHost: false,
  hostConn: null,              // data connection to the host (guests)
  guests: new Map(),           // host: peerId -> DataConnection
  roster: new Map(),           // host: official roster, peerId -> { name, sharing }
  members: new Map(),          // peerId -> { name, sharing }
  localStream: null,
  outgoing: new Map(),         // peerId -> MediaConnection (my screen going to them)
  incoming: new Map(),         // peerId -> MediaConnection (their screen coming to me)
  streams: new Map(),          // peerId -> { stream, name, local, tile, view }
  waiting: new Map(),          // peerId -> grid tile of a screen on the air whose picture has not arrived
  announce: new Map(),         // peerId -> when they went on the air, while their picture is on its way
  turboOut: null,              // TurboSender when sharing in minimum-latency mode
  turboIn: new Map(),          // peerId -> { conn, rx } (their video arriving via WebCodecs)
  audioIn: new Map(),          // peerId -> MediaStream with only their audio (minimum-latency mode)
  audioPc: new Map(),          // peerId -> the connection that audio comes on (for lip sync)
  demoted: new Set(),          // peers moved back to WebRTC due to congestion
  featured: null,
  view: "focus",               // "focus": one screen fills the stage; "grid": every screen side by side
  audioLocked: false,          // the browser refused sound before any click or key press
  canvasView: null,            // CanvasView painting on the stage, if any
  inRoom: false,
};

/* ---------------- Entry ---------------- */

ui.name.value = safeGet("telinha:name") || "";
// The desktop app link only shows when this server publishes it (and not on phones).
if (!/android|iphone|ipad/i.test(navigator.userAgent)) {
  fetch("download/versions.json", { method: "HEAD", cache: "no-cache" })
    .then((r) => { if (r.ok) $("getApp").hidden = false; })
    .catch(() => {});
}
// Invite: #1234.
const hashCode = location.hash.slice(1).split("/")[0].replace(/\D/g, "").slice(0, 4);
// This tab was in a channel and did not close normally: the browser killed the
// page, out of sight ("hidden": a phone that left the browser) or in plain view
// ("visible": it crashed). Sent with the next join, for the streamer's log.
let lastPageEnd = (() => { try { return sessionStorage.getItem("telinha:watching") || ""; } catch { return ""; } })();
window.addEventListener("pagehide", (e) => { if (!e.persisted) safeSession("telinha:watching", null); });
if (hashCode.length === 4) {
  ui.code.value = hashCode;
  // Channel in the link: straight into it, with a funny name if there is none yet.
  setTimeout(() => join(hashCode), 0);
}
syncLobby();

/** Codec names a browser reported (hardwareDecoders), kept short and few. */
function decoderList(v) {
  return Array.isArray(v) ? v.filter((d) => typeof d === "string" && d.length <= 8).slice(0, 4) : [];
}

ui.tv.addEventListener("pointerdown", (e) => {
  if (e.target !== ui.code) { e.preventDefault(); ui.code.focus(); }
});
ui.code.addEventListener("input", () => {
  ui.code.value = ui.code.value.replace(/\D/g, "").slice(0, 4);
  syncLobby();
  // The number complete: try it right away. If that channel is not on air, the error says so.
  if (ui.code.value.length === 4) join(ui.code.value);
});
ui.name.addEventListener("input", syncLobby);
// A phone keyboard slides up over the form: bring the field being typed in back into view.
for (const el of [ui.code, ui.name]) {
  el.addEventListener("focus", () => setTimeout(() => {
    if (document.activeElement === el) el.scrollIntoView({ block: "center", behavior: reducedMotion.matches ? "auto" : "smooth" });
  }, 350));
}

function syncLobby() {
  const full = ui.code.value.length === 4;
  ui.tv.classList.toggle("tuned", full);
  ui.tvHint.textContent = full ? "Pronto pra sintonizar" : "Digite o número do canal que te mandaram";
  ui.tune.disabled = !full;
  hideLobbyError();
}

ui.controls.addEventListener("submit", (e) => {
  e.preventDefault();
  if (ui.code.value.length === 4) join(ui.code.value);
});
ui.create.addEventListener("click", () => host());

// Whoever enters without a name gets one of these, kept on entering like a typed name so it can be changed later.
const FUNNY_ANIMALS = ["Capivara", "Tatu", "Jabuti", "Quati", "Lhama", "Preguiça", "Tucano", "Sagui", "Tamanduá", "Ornitorrinco", "Pinguim", "Coruja", "Lontra", "Guaxinim", "Suricato", "Arara", "Jacaré", "Polvo", "Pato", "Ouriço"];
const FUNNY_TRAITS = ["de Chinelo", "sem Wi-Fi", "com Lag", "de Pijama", "em Reunião", "de Férias", "de Óculos", "de Boné", "no Mudo", "sem Café", "de Terno", "de Patins", "com Sono", "de Pantufa"];
const pick = (list) => list[Math.floor(Math.random() * list.length)];

function displayName() {
  const n = ui.name.value.trim().replace(/\s+/g, " ");
  if (n) safeSet("telinha:name", n);
  return n || `${pick(FUNNY_ANIMALS)} ${pick(FUNNY_TRAITS)}`;
}

function setBusy(btn, text) {
  ui.tune.disabled = ui.create.disabled = !!btn;
  ui.code.readOnly = !!btn; // the number being tried stays put
  if (btn) { btn.dataset.label = btn.textContent; btn.textContent = text; }
  else {
    for (const b of [ui.tune, ui.create]) if (b.dataset.label) { b.textContent = b.dataset.label; delete b.dataset.label; }
    syncLobby();
  }
}

function showLobbyError(msg) {
  ui.lobbyError.textContent = msg;
  setLobbyFailed(true);
  ui.tv.scrollIntoView({ block: "nearest", behavior: reducedMotion.matches ? "auto" : "smooth" }); // short screens
}
function hideLobbyError() { setLobbyFailed(false); }
// The error takes the hint's place on the screen, which turns red.
function setLobbyFailed(failed) {
  ui.lobbyError.hidden = !failed;
  ui.tvHint.hidden = failed;
  ui.tv.classList.toggle("failed", failed);
}

/* ---------------- Open channel (host) ---------------- */

async function host(attempt = 0) {
  primeAudio();
  state.name = displayName();
  setBusy(ui.create, "Abrindo…");
  const code = String(1000 + Math.floor(Math.random() * 9000));
  try {
    const peer = await openPeer(PREFIX + code);
    state.peer = peer;
    state.isHost = true;
    state.code = code;
    state.roster.set(peer.id, { name: state.name, sharing: false, wc: TURBO.receive, dec: await DECODERS });
    peer.on("connection", acceptGuest);
    wirePeer(peer);
    broadcastRoster();
    enterRoom();
    toast("Canal aberto. Copie o convite e mande pros amigos.");
  } catch (err) {
    if (err?.type === "unavailable-id" && attempt < 5) return host(attempt + 1);
    setBusy(null);
    showLobbyError(peerErrorText(err));
  }
}

const joining = new Set(); // host: connections that have not opened yet
function acceptGuest(conn) {
  if (conn.metadata?.kind === "turbo") return; // minimum-latency video, handled in wirePeer
  // More opening at once than a real group ever does: not worth a connection.
  if (joining.size >= MAX_JOINING) { conn.close(); return; }
  joining.add(conn);
  const giveUp = setTimeout(() => { if (joining.delete(conn)) conn.close(); }, JOIN_TIMEOUT_MS);
  conn.on("open", () => {
    clearTimeout(giveUp);
    joining.delete(conn);
    if (state.roster.size >= MAX_PEOPLE && !state.roster.has(conn.peer)) {
      // Full: says so, and hangs up once the message had time to leave.
      conn.send({ t: "full" });
      setTimeout(() => conn.close(), 500);
      return;
    }
    const name = String(conn.metadata?.name || "Alguém").slice(0, 24);
    state.guests.set(conn.peer, conn);
    state.roster.set(conn.peer, { name, sharing: false, wc: !!conn.metadata?.wc, dec: decoderList(conn.metadata?.dec) });
    broadcastRoster();
  });
  conn.on("data", (msg) => {
    if (msg?.t === "sharing" && state.roster.has(conn.peer)) {
      const m = state.roster.get(conn.peer);
      m.sharing = !!msg.on;
      broadcastRoster();
    }
  });
  const gone = () => {
    clearTimeout(giveUp);
    joining.delete(conn);
    if (state.guests.get(conn.peer) !== conn) return;
    state.guests.delete(conn.peer);
    state.roster.delete(conn.peer);
    broadcastRoster();
  };
  conn.on("close", gone);
  conn.on("error", gone);
}

function broadcastRoster() {
  const members = [...state.roster].map(([id, m]) => ({ id, ...m }));
  for (const conn of state.guests.values()) if (conn.open) conn.send({ t: "roster", members });
  applyRoster(members);
}

/* ---------------- Tune in (guest) ---------------- */

async function join(code) {
  primeAudio();
  state.name = displayName();
  setBusy(ui.tune, "Sintonizando…");
  let peer;
  try {
    peer = await openPeer();
    const conn = await connectTo(peer, PREFIX + code, { name: state.name, wc: TURBO.receive, dec: await DECODERS, restart: lastPageEnd });
    lastPageEnd = "";
    state.peer = peer;
    state.code = code;
    state.hostConn = conn;
    conn.on("data", (msg) => {
      if (msg?.t === "roster" && Array.isArray(msg.members)) applyRoster(msg.members);
      else if (msg?.t === "full") channelFull();
    });
    conn.on("close", () => offAir());
    wirePeer(peer);
    enterRoom();
  } catch (err) {
    peer?.destroy();
    setBusy(null);
    showLobbyError(err?.type !== "peer-unavailable" ? peerErrorText(err)
      : state.fallback ? "Sem contato com o servidor. Confira sua internet e tente de novo."
      : `O canal ${code} não está no ar. Confira o número.`);
    if (document.activeElement === ui.code) ui.code.select(); // ready to type it again
  }
}

let ownServer = null; // caches only success; a failure is retried next time
async function peerOptions() {
  if (!SERVER) return { config: { iceServers: [{ urls: "stun:stun.l.google.com:19302" }] } };
  if (ownServer && ownServer.expires > Date.now()) return ownServer.options;
  // Falling back to the public server splits the person from friends on the VPS,
  // so only give up after two generous attempts.
  for (let attempt = 0; attempt < 2; attempt++) {
    try {
      const res = await fetch(`${LOCAL_SERVER ? "http" : "https"}://${SERVER}/api/ice`, { signal: AbortSignal.timeout(8000) });
      if (!res.ok) throw new Error(res.status);
      const { iceServers } = await res.json();
      const [host, port] = SERVER.split(":");
      const options = LOCAL_SERVER
        ? { host, port: Number(port) || 80, path: "/peer", secure: false, config: { iceServers } }
        : { host: SERVER, port: 443, path: "/peer", secure: true, config: { iceServers } };
      ownServer = { options, expires: Date.now() + 12 * 3600_000 };
      state.fallback = false;
      return options;
    } catch {}
  }
  console.warn("[telinha] own server unavailable, using public PeerJS");
  state.fallback = true;
  return { config: { iceServers: [{ urls: "stun:stun.l.google.com:19302" }] } };
}

async function openPeer(id) {
  const options = { ...(await peerOptions()), debug: 1 };
  return new Promise((resolve, reject) => {
    const peer = id ? new Peer(id, options) : new Peer(options);
    const fail = (err) => { peer.destroy(); reject(err); };
    peer.once("open", () => { peer.off("error", fail); resolve(peer); });
    peer.once("error", fail);
  });
}

function connectTo(peer, id, metadata) {
  return new Promise((resolve, reject) => {
    // JSON (not the default BinaryPack) so the native app understands it when it is the host.
    const conn = peer.connect(id, { metadata, reliable: true, serialization: "json" });
    const timer = setTimeout(() => reject({ type: "timeout" }), 15000);
    const onErr = (err) => { clearTimeout(timer); reject(err); };
    peer.once("error", onErr);
    conn.once("open", () => { clearTimeout(timer); peer.off("error", onErr); resolve(conn); });
  });
}

function peerErrorText(err) {
  switch (err?.type) {
    case "network":
    case "server-error":
    case "socket-error":
    case "socket-closed":
      return "Sem contato com o servidor. Confira sua internet e tente de novo.";
    case "browser-incompatible":
      return "Este navegador não tem WebRTC. Use Chrome, Edge, Firefox ou Safari.";
    case "timeout":
      return "O canal demorou pra responder. Tente de novo.";
    case "unavailable-id":
      return "Não achei um canal livre. Tente de novo.";
    default:
      return "Algo deu errado na conexão. Tente de novo.";
  }
}

/* ---------------- Peer: calls and reconnection ---------------- */

function wirePeer(peer) {
  peer.on("connection", (conn) => { if (conn.metadata?.kind === "turbo") acceptTurbo(conn); });
  peer.on("call", (call) => {
    if (call.metadata?.audioOnly) return acceptTurboAudio(call);
    state.incoming.get(call.peer)?.close();
    state.incoming.set(call.peer, call);
    call.answer(undefined, { sdpTransform: hiFiAudio });
    tuneReceiver(call.peerConnection, call.metadata?.codec);
    call.on("stream", (stream) => {
      const name = call.metadata?.name || state.members.get(call.peer)?.name || "Alguém";
      addStream(call.peer, stream, name, false);
    });
    const end = () => {
      if (state.incoming.get(call.peer) !== call) return;
      state.incoming.delete(call.peer);
      removeStream(call.peer);
    };
    call.on("close", end);
    call.on("error", end);
  });
  peer.on("disconnected", () => { if (!peer.destroyed) peer.reconnect(); });
  window.addEventListener("pagehide", () => state.turboOut?.stop());
  peer.on("error", (err) => {
    if (err.type === "peer-unavailable") return; // someone left mid-call
    console.warn("[telinha]", err.type, err);
  });
}

/* Incoming minimum-latency video (WebCodecs) */

function acceptTurbo(conn) {
  if (!TURBO.receive) { conn.close(); return; }
  const id = conn.peer;
  state.turboIn.get(id)?.close();
  state.audioIn.delete(id); // audio from a previous connection; the new one arrives right after this
  const rx = new TurboReceiver(openVideoChannel(conn.peerConnection), {
    requestKey: () => { if (conn.open) conn.send({ t: "key" }); },
    ping: (msg) => { if (conn.open) conn.send(msg); },
    onUnsupported: () => { if (conn.open) conn.send({ t: "unsupported" }); },
    onRenderChange: () => showTurbo(id),
    report: (r) => { if (conn.open) conn.send({ t: "rx", ...r, relay: entry.relay }); },
  });
  // The wait for a lost frame tracks the connection's real round trip; and
  // the sender learns whether the path goes through the relay (there the data
  // channel stalls, and video works better over plain WebRTC).
  const rtt = setInterval(async () => {
    const stats = await conn.peerConnection?.getStats().catch(() => null);
    stats?.forEach((r) => {
      if (r.type !== "candidate-pair" || !r.nominated || r.state !== "succeeded") return;
      if (r.currentRoundTripTime) rx.setRtt(r.currentRoundTripTime * 1000);
      entry.relay = [r.localCandidateId, r.remoteCandidateId].some((id) => stats.get(id)?.candidateType === "relay");
    });
  }, 2000);
  const entry = { conn, rx, remote: null, close() { clearInterval(rtt); rx.close(); conn.close(); } };
  conn.on("data", (msg) => {
    if (msg?.t === "tx") entry.remote = msg;
    else if (msg?.t === "pong") rx.pong(msg);
  });
  state.turboIn.set(id, entry);
  conn.on("open", () => showTurbo(id));
  const end = () => {
    clearInterval(rtt);
    if (state.turboIn.get(id) !== entry) return;
    state.turboIn.delete(id);
    rx.close();
    removeStream(id);
  };
  conn.on("close", end);
  conn.on("error", end);
}

function acceptTurboAudio(call) {
  const id = call.peer;
  call.answer(undefined, { sdpTransform: hiFiAudio });
  tuneReceiver(call.peerConnection);
  call.on("stream", (stream) => { state.audioIn.set(id, stream); state.audioPc.set(id, call.peerConnection); showTurbo(id); });
  const end = () => {
    if (state.audioIn.get(id)?.id !== call.remoteStream?.id) return;
    state.audioIn.delete(id);
    state.audioPc.delete(id);
    showTurbo(id);
  };
  call.on("close", end);
  call.on("error", end);
}

// Combines the decoded video with the audio (which comes over WebRTC) into one
// stream. Where frames are painted on a canvas, the stream carries only the
// audio and the canvas shows the picture.
function showTurbo(id) {
  const entry = state.turboIn.get(id);
  if (!entry) return;
  const audio = state.audioIn.get(id)?.getAudioTracks() ?? [];
  const name = state.members.get(id)?.name || "Alguém";
  if (entry.rx.track) addStream(id, new MediaStream([entry.rx.track, ...audio]), name, false);
  else addStream(id, new MediaStream(audio), name, false, entry.rx.view);
}

/* Lip sync on the minimum delay path. The video is shown the moment it is
 * decoded; the audio comes over WebRTC with its own jitter buffer. Once a
 * second, compare how late each one is: the video's measured capture-to-
 * screen delay against the audio's, taken from the playout time Chrome
 * estimates in the streamer's NTP time when it gives one, otherwise half the
 * round trip plus the jitter buffer's average delay plus ~20 ms for capture,
 * Opus framing and the sound card. Past ±30 ms, the audio's buffer target
 * moves toward the difference (up to 200 ms, 40 ms per step). Only the audio
 * ever waits: the video stays as fast as it is. */
const AV_TOLERANCE_MS = 30;
const AV_MAX_HOLD_MS = 200;
const AV_STEP_MS = 40;
const AUDIO_EDGES_MS = 20;
const NTP_UNIX_MS = 2208988800000;
setInterval(syncAudio, 1000);
async function syncAudio() {
  for (const [id, entry] of state.turboIn) {
    const pc = state.audioPc.get(id);
    const receiver = pc?.getReceivers().find((r) => r.track?.kind === "audio");
    const offset = entry.rx.clock?.best?.offset;
    const video = entry.rx.stats.glassMs;
    if (!receiver || !("jitterBufferTarget" in receiver) || typeof offset !== "number" || typeof video !== "number") continue;
    const stats = await receiver.getStats().catch(() => null);
    let inbound = null;
    stats?.forEach((r) => { if (r.type === "inbound-rtp" && r.kind === "audio") inbound = r; });
    if (!inbound) continue;
    // Jitter buffer delay over the last second, per sample played.
    const prev = entry.audioJb;
    entry.audioJb = { delay: inbound.jitterBufferDelay, count: inbound.jitterBufferEmittedCount };
    const jb = prev && entry.audioJb.count > prev.count ? ((entry.audioJb.delay - prev.delay) / (entry.audioJb.count - prev.count)) * 1000 : null;
    const audio = inbound.estimatedPlayoutTimestamp
      ? Date.now() + offset - (inbound.estimatedPlayoutTimestamp - NTP_UNIX_MS)
      : typeof jb === "number" ? (entry.rx.clock.rtt ?? 0) / 2 + jb + AUDIO_EDGES_MS : null;
    if (audio === null || audio < 0 || audio > 5000) continue; // no estimate yet, or clocks not settled
    const gap = video - audio; // > 0: the audio plays ahead of the picture
    entry.rx.stats.avGap = gap;
    const hold = receiver.jitterBufferTarget ?? 0;
    let next = hold;
    if (gap > AV_TOLERANCE_MS) next = Math.min(AV_MAX_HOLD_MS, hold + Math.min(AV_STEP_MS, gap - AV_TOLERANCE_MS / 2));
    else if (gap < -AV_TOLERANCE_MS && hold > 0) next = Math.max(0, hold - Math.min(AV_STEP_MS, -gap - AV_TOLERANCE_MS / 2));
    if (next !== hold) { try { receiver.jitterBufferTarget = Math.round(next); } catch {} }
    entry.rx.stats.audioHold = receiver.jitterBufferTarget ?? 0;
  }
}

// `final`: they left or stopped sharing (otherwise their picture may be on its way back).
function closeIncoming(id, final = false) {
  state.turboIn.get(id)?.close();
  state.turboIn.delete(id);
  state.audioIn.delete(id);
  state.audioPc.delete(id);
  removeStream(id, final);
}

function applyRoster(list) {
  const me = state.peer.id;
  // The list comes from whoever hosts the channel: anything that is not a
  // person is dropped, names are cut like a host cuts them, and past
  // MAX_PEOPLE the rest is ignored (this page stays on it).
  const people = list.filter((m) => m && typeof m.id === "string" && typeof m.name === "string").filter((m, i) => i < MAX_PEOPLE || m.id === me);
  const next = new Map(people.map((m) => [m.id, { name: m.name.slice(0, 24), sharing: !!m.sharing, wc: !!m.wc, dec: decoderList(m.dec) }]));

  for (const [id, prev] of state.members) {
    if (id === me) continue;
    if (!next.has(id)) {
      state.outgoing.get(id)?.close(); state.outgoing.delete(id);
      state.demoted.delete(id);
      closeIncoming(id, true);
    } else if (prev.sharing && !next.get(id).sharing) {
      closeIncoming(id, true);
    }
    if (!next.get(id)?.sharing) state.announce.delete(id);
  }
  // The first roster received is the initial state, not news. Arriving while
  // two or more are on the air, the screens show side by side.
  if (!state.members.size) {
    if (people.filter((m) => m.id !== me && m.sharing).length > 1) state.view = "grid";
  } else {
    let joined = false, left = false;
    for (const [id, m] of next) {
      const prev = state.members.get(id);
      if (id === me) continue;
      if (!prev) { toast(`${m.name} entrou`); joined = true; }
      else if (m.sharing && !prev.sharing) {
        // Watching someone else's screen: the news waits for the picture, so it can show it.
        const watching = state.view === "focus" && [...state.streams].some(([k, st]) => k !== id && !st.local);
        if (watching) state.announce.set(id, performance.now());
        else toast(`${m.name} começou a transmitir`);
      }
    }
    for (const [id, m] of state.members) if (id !== me && !next.has(id)) { toast(`${m.name} saiu`); left = true; }
    if (joined) playChime("join");
    else if (left) playChime("leave");
  }
  state.members = next;

  for (const id of next.keys()) if (needsStream(id)) connectMember(id);
  for (const [id, s] of state.streams) if (!s.local && next.has(id)) s.name = next.get(id).name;
  renderPeople();
  renderStreams();
  if ("open" in qp.dataset) renderQuality();
}

/* ---------------- Share ---------------- */

ui.shareBtn.addEventListener("click", () => (state.localStream ? stopShare() : startShare()));

async function startShare() {
  if (!navigator.mediaDevices?.getDisplayMedia) {
    toast("Este aparelho não deixa compartilhar a tela, mas dá pra assistir.");
    return;
  }
  let stream;
  try {
    stream = await navigator.mediaDevices.getDisplayMedia({
      video: { frameRate: { ideal: state.quality.fps, max: 60 } }, // no size limit: quality settings decide
      // restrictOwnAudio: our join/leave chimes stay out of the stream
      audio: { echoCancellation: false, noiseSuppression: false, autoGainControl: false, restrictOwnAudio: true },
      systemAudio: "include",
      selfBrowserSurface: "exclude",
      surfaceSwitching: "include",
    });
  } catch (err) {
    if (err.name !== "NotAllowedError" && err.name !== "AbortError") toast("Não consegui capturar a tela.");
    return;
  }
  const video = stream.getVideoTracks()[0];
  video.addEventListener("ended", stopShare);
  const { width = 1920, height = 1080 } = video.getSettings();
  state.native = { width, height };
  state.codec = await hardwareCodec(video);
  state.audioSource = stream.getAudioTracks().length ? "tela" : null;
  if (!state.audioSource && safeGet("telinha:systemAudio") !== "off") {
    const track = await captureSystemAudio(false).catch(() => null); // only if permission was already granted
    if (track) { stream.addTrack(track); state.audioSource = "computador"; }
  }

  state.localStream = stream;
  await applyQuality();
  addStream(state.peer.id, stream, "Você", true);
  startTransport();
  announceSharing(true);
  renderShareButton();
}

function stopShare() {
  if (!state.localStream) return;
  state.localStream.getTracks().forEach((t) => t.stop());
  state.localStream = null;
  stopTransport();
  removeStream(state.peer.id);
  announceSharing(false);
  renderShareButton();
}

function announceSharing(on) {
  if (state.isHost) {
    const me = state.roster.get(state.peer.id);
    if (me) me.sharing = on;
    broadcastRoster();
  } else if (state.hostConn?.open) {
    state.hostConn.send({ t: "sharing", on });
  }
}

function useTurbo() { return state.quality.turbo && TURBO.send; }

// Tracks changed: reopen connections (PeerJS does not renegotiate).
function restartTransport() {
  if (!state.localStream) return;
  stopTransport();
  startTransport();
}

/* ---------------- Computer audio ----------------
 * On Windows Chrome sends system audio along with the whole screen ("Share
 * system audio" checkbox). On Linux and Mac it only sends tab audio; there the
 * audio output becomes a virtual input ("Som do computador", see
 * som-linux.conf) and we capture it as if it were a microphone. */

const SYSTEM_AUDIO_RE = /som do computador|monitor of|blackhole|loopback/i;
const OS = /win/i.test(navigator.userAgentData?.platform ?? navigator.platform) ? "windows"
  : /mac/i.test(navigator.userAgentData?.platform ?? navigator.platform) ? "mac" : "linux";

async function findSystemAudioDevice() {
  const list = await navigator.mediaDevices.enumerateDevices();
  return list.find((d) => d.kind === "audioinput" && d.deviceId && SYSTEM_AUDIO_RE.test(d.label)) ?? null;
}

async function captureSystemAudio(ask) {
  if (!navigator.mediaDevices?.getUserMedia) return null;
  let device = await findSystemAudioDevice();
  if (!device && ask) {
    // device names only appear after microphone permission
    const probe = await navigator.mediaDevices.getUserMedia({ audio: true });
    probe.getTracks().forEach((t) => t.stop());
    device = await findSystemAudioDevice();
  }
  if (!device) return null;
  const s = await navigator.mediaDevices.getUserMedia({
    audio: { deviceId: { exact: device.deviceId }, echoCancellation: false, noiseSuppression: false, autoGainControl: false, channelCount: 2 },
  });
  return s.getAudioTracks()[0] ?? null;
}

async function enableSystemAudio() {
  let track = null;
  try { track = await captureSystemAudio(true); } catch {}
  if (!track) { $("audioHelp").hidden = false; return; }
  if (!state.localStream) { track.stop(); return; }
  state.localStream.addTrack(track);
  state.audioSource = "computador";
  safeSet("telinha:systemAudio", "on");
  restartTransport();
  renderQuality();
  toast("Som do computador ligado");
}

function disableSystemAudio() {
  for (const t of state.localStream?.getAudioTracks() ?? []) { t.stop(); state.localStream.removeTrack(t); }
  state.audioSource = null;
  safeSet("telinha:systemAudio", "off");
  restartTransport();
  renderQuality();
}

// Game and music audio: stereo Opus with more bandwidth (default is mono voice ~32 kb/s).
function hiFiAudio(sdp) {
  const pt = sdp.match(/a=rtpmap:(\d+) opus\/48000/i)?.[1];
  if (!pt) return sdp;
  return sdp.replace(new RegExp(`a=fmtp:${pt} (.*)`), (_, params) => {
    const kept = params.split(";").filter((p) => p && !/^(stereo|sprop-stereo|maxaveragebitrate)=/.test(p));
    return `a=fmtp:${pt} ${[...kept, "stereo=1", "sprop-stereo=1", "maxaveragebitrate=192000"].join(";")}`;
  });
}

// Pause before opening again a viewer's connection that fell mid-stream; the
// desktop app waits the same (native/src/engine/session.rs, REOPEN_AFTER).
const REOPEN_AFTER_MS = 3000;
// A connection that has not come up by then is given up on and tried again.
const OPEN_TIMEOUT_MS = 15000;
let reopenTimer = 0;

/** Someone else still in the channel, while this page streams, with nothing going to them. */
function needsStream(id) {
  return !!state.localStream && !!state.peer && id !== state.peer.id && state.members.has(id) && !state.outgoing.has(id);
}

// A viewer's connection fell while the stream goes on: nothing else would open
// it again until someone joins or leaves. Opened again after a pause, so a
// viewer who is leaving is gone from the roster by then, and a network that
// keeps failing gets one try per pause instead of a loop.
function reopenLater() {
  if (!state.localStream || reopenTimer) return;
  reopenTimer = setTimeout(() => {
    reopenTimer = 0;
    for (const id of state.members.keys()) {
      if (!needsStream(id)) continue;
      console.warn("[telinha] a viewer's connection fell; opening it again:", state.members.get(id)?.name);
      connectMember(id);
    }
  }, REOPEN_AFTER_MS);
}

function startTransport() {
  if (useTurbo()) {
    const video = state.localStream.getVideoTracks()[0];
    state.turboOut = new TurboSender(video, { bitrate: Math.round(targetBitrate()), framerate: state.quality.fps });
  }
  for (const id of state.members.keys()) if (id !== state.peer.id) connectMember(id);
}

function stopTransport() {
  for (const handle of state.outgoing.values()) handle.close();
  state.outgoing.clear();
  state.turboOut?.stop();
  state.turboOut = null;
  state.demoted.clear();
  clearTimeout(reopenTimer);
  reopenTimer = 0;
}

// Peers that can receive via WebCodecs get the minimum-latency path; the rest, WebRTC.
function connectMember(id) {
  if (state.turboOut && state.members.get(id)?.wc && !state.demoted.has(id)) turboMember(id);
  else callMember(id);
}

function turboMember(id) {
  const conn = state.peer.connect(id, { metadata: { kind: "turbo", name: state.name }, reliable: true });
  if (!conn) return;
  const channel = openVideoChannel(conn.peerConnection);
  let audio = null;
  let opening = 0, audioAgain = 0, closed = false;
  const handle = {
    kind: "turbo",
    peerConnection: conn.peerConnection,
    turbo: () => state.turboOut?.viewerStats(id),
    close() { closed = true; clearTimeout(opening); clearTimeout(audioAgain); state.turboOut?.removeViewer(id); audio?.close(); conn.close(); },
  };
  state.outgoing.set(id, handle);
  state.turboOut.addViewer(id, channel, { onCongested: () => demote(id, true), dec: state.members.get(id)?.dec ?? [] });
  conn.on("data", (msg) => {
    if (msg?.t === "key") state.turboOut?.requestKey(500, id);
    else if (msg?.t === "rx") {
      if (msg.relay) demote(id);
      else state.turboOut?.report(id, msg);
    }
    else if (msg?.t === "unsupported") {
      // Their decoder refused HEVC or AV1: the same path again, now on what
      // everyone decodes (a fresh connection, as their decoder gave up on this one).
      if (state.turboOut?.refuse(id)) { handle.close(); state.outgoing.delete(id); turboMember(id); }
      else demote(id);
    }
    // The viewer's clock: when this side answered, on its wall and frame clocks.
    else if (msg?.t === "ping") conn.send({ t: "pong", a: msg.a, b: Date.now(), s: state.turboOut?.frameClock() });
  });
  // Tells viewers how this side is doing (capture, encoder, hidden tab), so the
  // bottleneck can be found from the viewer's panel alone.
  let info = 0;
  // The sound travels on a call of its own. If it falls while the picture goes
  // on, it is placed again after the same pause.
  const openAudio = () => {
    audioAgain = 0;
    const tracks = state.localStream?.getAudioTracks() ?? [];
    if (closed || !tracks.length || !conn.open) return;
    const call = state.peer.call(id, new MediaStream(tracks), { metadata: { name: state.name, audioOnly: true } });
    if (!call) return;
    audio = call;
    const again = () => {
      if (closed || audio !== call) return;
      audio = null;
      audioAgain = setTimeout(openAudio, REOPEN_AFTER_MS);
    };
    call.on("close", again);
    call.on("error", again);
  };
  conn.on("open", () => {
    clearTimeout(opening); openAudio();
    info = setInterval(() => {
      const t = state.turboOut?.viewerStats(id);
      if (!t || !conn.open) return;
      const video = state.localStream?.getVideoTracks()[0];
      conn.send({ t: "tx", captured: t.captured, skipped: t.skipped, dropped: t.dropped, hardware: t.hardware, encodeMs: t.encodeMs, captureFps: video?.getSettings().frameRate, hidden: document.hidden });
    }, 1000);
  });
  const end = () => {
    closed = true;
    clearInterval(info);
    clearTimeout(opening);
    clearTimeout(audioAgain);
    if (state.outgoing.get(id) !== handle) return;
    state.outgoing.delete(id);
    state.turboOut?.removeViewer(id);
    audio?.close();
    reopenLater();
  };
  conn.on("close", end);
  conn.on("error", end);
  // Never opened: see callMember.
  opening = setTimeout(() => {
    if (state.outgoing.get(id) !== handle || conn.open) return;
    handle.close();
    end();
  }, OPEN_TIMEOUT_MS);
}

// Queue always full or browser lacks the codec: this person goes back to WebRTC,
// which can lower quality on its own.
function demote(id, congested = false) {
  const handle = state.outgoing.get(id);
  if (handle?.kind !== "turbo") return;
  if (congested) toast(`A conexão de ${state.members.get(id)?.name || "alguém"} não aguentou o atraso mínimo. Mandando no modo normal.`);
  state.demoted.add(id);
  handle.close();
  state.outgoing.delete(id);
  if (state.localStream) callMember(id);
}

function callMember(id) {
  const call = state.peer.call(id, state.localStream, { metadata: { name: state.name, codec: state.codec } });
  if (!call) return;
  state.outgoing.set(id, call);
  let opening = 0;
  const end = () => {
    clearTimeout(opening);
    if (state.outgoing.get(id) !== call) return;
    state.outgoing.delete(id);
    reopenLater();
  };
  call.on("close", end);
  call.on("error", end);
  // Never answered (the viewer could not be reached): PeerJS says nothing on the
  // call and close() is silent for one that never opened, so end() is called here.
  opening = setTimeout(() => {
    if (state.outgoing.get(id) !== call || call.peerConnection?.connectionState === "connected") return;
    call.close();
    end();
  }, OPEN_TIMEOUT_MS);
  tuneSender(call.peerConnection);
}

/* ---------------- Latency (ideas from Sunshine/Moonlight) ----------------
 *
 * - Hardware encoder: Sunshine uses NVENC/QuickSync/VAAPI. Here the sharer
 *   finds which codec the GPU encodes (powerEfficient) and tells viewers,
 *   who put that codec first in their answer.
 * - Bitrate cap sized to the video (Sunshine limits the buffer to one frame):
 *   no bursts beyond what is needed, less loss and queuing on the relay.
 * - Mode: smoothness keeps frames per second (games), sharpness keeps
 *   resolution (text, code). */

async function hardwareCodec(track) {
  if (!navigator.mediaCapabilities?.encodingInfo) return null;
  const { width = 1920, height = 1080 } = track.getSettings();
  const candidates = [
    ["video/H264", "video/H264;level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"],
    ["video/VP9", "video/VP9"],
    ["video/AV1", "video/AV1"],
  ];
  for (const [mime, contentType] of candidates) {
    try {
      const info = await navigator.mediaCapabilities.encodingInfo({
        type: "webrtc",
        video: { contentType, width, height, bitrate: MAX_BITRATE, framerate: 60 },
      });
      if (info.supported && info.powerEfficient) return mime;
    } catch {}
  }
  return null;
}

// No wait on the receive side beyond what the network demands: the jitter
// buffer target is a floor (0 = the least the browser allows), for video and
// audio alike. The native app also asks for zero playout delay in RTP.
function lowLatency(receiver) {
  try {
    if ("jitterBufferTarget" in receiver) receiver.jitterBufferTarget = 0;
    else if ("playoutDelayHint" in receiver) receiver.playoutDelayHint = 0; // older Chrome
  } catch {}
}

function tuneReceiver(pc, codec) {
  if (!pc) return;
  pc.addEventListener("track", ({ receiver, transceiver }) => {
    lowLatency(receiver);
    if (codec && receiver.track.kind === "video" && transceiver.setCodecPreferences) {
      const all = RTCRtpReceiver.getCapabilities("video")?.codecs ?? [];
      const rank = (c) => c.mimeType !== codec ? 2 : /packetization-mode=1/.test(c.sdpFmtpLine ?? "") || codec !== "video/H264" ? 0 : 1;
      const sorted = [...all].sort((a, b) => rank(a) - rank(b));
      if (sorted.length && rank(sorted[0]) < 2) {
        try { transceiver.setCodecPreferences(sorted); } catch {}
      }
    }
  });
}

function tuneSender(pc) {
  if (!pc) return;
  pc.addEventListener("connectionstatechange", () => { if (pc.connectionState === "connected") applySenderParams(pc); });
}

/* ---------------- Stream quality ---------------- */

const PRIORITIES = {
  fluidez: { hint: "motion", degradation: "maintain-framerate", note: "Mantém o movimento suave. Bom pra jogos e vídeos." },
  nitidez: { hint: "detail", degradation: "maintain-resolution", note: "Mantém a imagem nítida. Bom pra texto e código." },
};
state.quality = loadQuality();

function loadQuality() {
  let q = {};
  try { q = JSON.parse(safeGet("telinha:quality")) || {}; } catch {}
  return {
    res: ["720", "1080", "1440", "original"].includes(q.res) ? q.res : "1080",
    fps: q.fps === 30 ? 30 : 60,
    turbo: q.turbo === true,
    priority: PRIORITIES[q.priority] ? q.priority : PRIORITIES[safeGet("telinha:mode")] ? safeGet("telinha:mode") : "fluidez",
  };
}

// Output size, keeping the captured screen's aspect ratio and never
// scaling above the original.
function targetSize() {
  const { width, height } = state.native ?? { width: 1920, height: 1080 };
  const h = state.quality.res === "original" ? height : Math.min(height, Number(state.quality.res));
  return { width: Math.round((h * width) / height / 2) * 2, height: h };
}

// Per-person budget, proportional to pixels per second: ~0.08 bit per pixel
// (0.1 in sharpness mode, where text needs it most), like the native app.
// Measured: a cap sized to the video reduces loss and queuing on the relay;
// below ~0.07 the encoder starts lowering resolution on its own. The ceiling
// (45 Mb/s, 50 for sharpness: ~0.08 bpp at 4K60) stays at the old 20 Mb/s
// for whoever receives through the relay (TURN), which carries everyone.
const RELAY_BITRATE = 20e6;
function targetBitrate(relay = false) {
  const { width, height } = targetSize();
  const sharp = state.quality.priority === "nitidez";
  const cap = relay ? RELAY_BITRATE : sharp ? 50e6 : 45e6;
  return Math.min(cap, Math.max(2.5e6, width * height * state.quality.fps * (sharp ? 0.1 : 0.08)));
}

// Whether this connection's chosen path goes through the relay.
async function viaRelay(pc) {
  const stats = await pc.getStats().catch(() => null);
  let relay = false;
  stats?.forEach((r) => {
    if (r.type !== "candidate-pair" || !r.nominated || r.state !== "succeeded") return;
    relay ||= [r.localCandidateId, r.remoteCandidateId].some((id) => stats.get(id)?.candidateType === "relay");
  });
  return relay;
}

async function applyQuality() {
  const video = state.localStream?.getVideoTracks()[0];
  if (video) {
    const { width, height } = targetSize();
    try { await video.applyConstraints({ width: { max: width }, height: { max: height }, frameRate: { max: state.quality.fps } }); } catch {}
    if ("contentHint" in video) video.contentHint = PRIORITIES[state.quality.priority].hint;
  }
  for (const call of state.outgoing.values()) {
    if (call.kind !== "turbo" && call.peerConnection?.connectionState === "connected") applySenderParams(call.peerConnection);
  }
  state.turboOut?.setRate({ bitrate: Math.round(targetBitrate()), framerate: state.quality.fps });
  renderQuality();
}

async function applySenderParams(pc) {
  const relay = await viaRelay(pc);
  const { height } = targetSize();
  for (const sender of pc.getSenders()) {
    if (sender.track?.kind !== "video") continue;
    const p = sender.getParameters();
    if (!p.encodings?.length) p.encodings = [{}];
    const captured = sender.track.getSettings().height || height;
    p.encodings[0].maxBitrate = Math.round(targetBitrate(relay));
    p.encodings[0].maxFramerate = state.quality.fps;
    p.encodings[0].scaleResolutionDownBy = Math.max(1, captured / height); // if the capture did not accept downscaling
    p.degradationPreference = PRIORITIES[state.quality.priority].degradation;
    sender.setParameters(p).catch(() => {
      delete p.degradationPreference; // not every browser supports it
      sender.setParameters(p).catch(() => {});
    });
  }
}

function setQuality(change) {
  const switchTransport = "turbo" in change && change.turbo !== state.quality.turbo;
  Object.assign(state.quality, change);
  safeSet("telinha:quality", JSON.stringify(state.quality));
  applyQuality();
  if (switchTransport) restartTransport();
}

/* Panel: grows out of the button and returns to it */

const qp = ui.qualityPanel;
const segs = { res: $("resSeg"), fps: $("fpsSeg"), priority: $("prioSeg"), turbo: $("turboSeg") };

function renderSeg(el, key, options, current = String(state.quality[key])) {
  const index = Math.max(0, options.findIndex((o) => String(o.value) === current));
  el.style.setProperty("--n", options.length);
  el.style.setProperty("--i", index);
  const signature = options.map((o) => o.value + o.label).join("|");
  if (el.dataset.signature !== signature) {
    el.dataset.signature = signature;
    el.replaceChildren(Object.assign(document.createElement("span"), { className: "seg-thumb" }));
    for (const o of options) {
      const label = document.createElement("label");
      const input = Object.assign(document.createElement("input"), { type: "radio", name: `q-${key}`, value: o.value });
      const cast = { number: Number, boolean: (v) => v === "true" }[typeof state.quality[key]] ?? String;
      input.addEventListener("change", () => setQuality({ [key]: cast(String(o.value)) }));
      label.append(input, o.label);
      el.append(label);
    }
  }
  for (const input of el.querySelectorAll("input")) input.checked = input.value === String(options[index].value);
}

const AUDIO_NOTES = {
  tela: "O som vai junto com a tela.",
  computador: "Vai tudo que sai no seu fone ou caixa de som, inclusive vozes de chamadas.",
  windows: "Sem som. Pra mandar o som, compartilhe de novo e marque “Compartilhar áudio do sistema”.",
  outro: "Sem som. Neste sistema o Chrome só manda o som de abas.",
};
function renderAudio() {
  const src = state.audioSource;
  $("audioNote").textContent = AUDIO_NOTES[src] ?? (OS === "windows" ? AUDIO_NOTES.windows : AUDIO_NOTES.outro);
  const btn = $("audioBtn");
  btn.hidden = src === "tela" || (!src && OS === "windows");
  btn.textContent = src === "computador" ? "Desligar som" : "Usar som do computador";
  $("audioCmdWrap").hidden = OS !== "linux";
  $("audioCmd").textContent = "mkdir -p ~/.config/pipewire/pipewire.conf.d && curl -fsSL "
    + `${location.origin}/som-linux.conf -o ~/.config/pipewire/pipewire.conf.d/telinha-som.conf && systemctl --user restart pipewire`;
  $("audioMac").hidden = OS !== "mac";
  if (src) $("audioHelp").hidden = true;
}
$("audioBtn").addEventListener("click", () => (state.audioSource === "computador" ? disableSystemAudio() : enableSystemAudio()));
$("audioCopy").addEventListener("click", async () => {
  try { await navigator.clipboard.writeText($("audioCmd").textContent); toast("Comando copiado"); } catch {}
});

function renderQuality() {
  const native = state.native ?? { width: 1920, height: 1080 };
  const { width, height } = targetSize();
  const resOptions = [720, 1080, 1440]
    .filter((h) => h < native.height)
    .map((h) => ({ value: String(h), label: `${h}p` }));
  resOptions.push({ value: "original", label: "Original" });
  // a saved choice larger than the current screen shows as "Original"
  const shownRes = resOptions.some((o) => o.value === state.quality.res) ? state.quality.res : "original";
  renderSeg(segs.res, "res", resOptions, shownRes);
  renderSeg(segs.fps, "fps", [{ value: 30, label: "30" }, { value: 60, label: "60" }]);
  renderSeg(segs.priority, "priority", [{ value: "fluidez", label: "Fluidez" }, { value: "nitidez", label: "Nitidez" }]);

  $("resNote").textContent = `Sua tela: ${native.width}×${native.height}. Saindo em ${width}×${height}.`;
  $("prioNote").textContent = PRIORITIES[state.quality.priority].note;
  renderAudio();
  $("turboField").hidden = !TURBO.send;
  renderSeg(segs.turbo, "turbo", [{ value: false, label: "Normal" }, { value: true, label: "Mínimo" }]);
  $("turboNote").textContent = state.quality.turbo
    ? "Codifica uma vez só e manda sem fila de espera, como o Sunshine. Experimental: quem não tem suporte recebe no modo normal."
    : "Usa o WebRTC padrão do navegador, que se adapta bem a conexões instáveis.";
  const mbps = (n) => n.toLocaleString("pt-BR", { maximumFractionDigits: 1 });
  const viewers = Math.max(0, state.members.size - 1);
  const perPerson = Math.round(targetBitrate() / 1e5) / 10; // in Mb/s, already rounded

  $("qualityUpload").textContent = viewers > 1
    ? `Até ${mbps(perPerson)} Mb/s de upload por pessoa. Com ${viewers} pessoas assistindo, até ${mbps(perPerson * viewers)} Mb/s.`
    : `Até ${mbps(perPerson)} Mb/s de upload por pessoa assistindo.`;
  ui.qualityLabel.textContent = `${height}p${state.quality.fps}`;
}

function openQuality(viaKeyboard) {
  renderQuality();
  qp.hidden = false;
  const b = ui.qualityBtn.getBoundingClientRect();
  const w = qp.offsetWidth;
  const left = Math.min(Math.max(12, b.left + b.width / 2 - w / 2), innerWidth - w - 12);
  qp.style.left = `${left}px`;
  qp.style.bottom = `${innerHeight - b.top + 10}px`;
  qp.style.maxHeight = `${b.top - 22}px`;
  qp.style.transformOrigin = `${b.left + b.width / 2 - left}px 100%`;
  qp.getBoundingClientRect(); // commits the initial state before the transition
  qp.dataset.open = "";
  ui.qualityBtn.setAttribute("aria-expanded", "true");
  if (viaKeyboard) qp.querySelector("input:checked")?.focus();
}

function closeQuality(returnFocus) {
  if (!("open" in qp.dataset)) return;
  delete qp.dataset.open;
  ui.qualityBtn.setAttribute("aria-expanded", "false");
  if (returnFocus) ui.qualityBtn.focus();
}

qp.addEventListener("transitionend", (e) => {
  if (e.target === qp && e.propertyName === "opacity" && !("open" in qp.dataset)) qp.hidden = true;
});
ui.qualityBtn.addEventListener("click", (e) => ("open" in qp.dataset ? closeQuality() : openQuality(e.detail === 0)));
document.addEventListener("pointerdown", (e) => {
  if ("open" in qp.dataset && !qp.contains(e.target) && !ui.qualityBtn.contains(e.target)) closeQuality();
});
qp.addEventListener("keydown", (e) => { if (e.key === "Escape") { e.stopPropagation(); closeQuality(true); } });
addEventListener("resize", () => closeQuality());

function renderShareButton() {
  const live = !!state.localStream;
  ui.shareBtn.classList.toggle("live", live);
  ui.shareBtn.querySelector("use").setAttribute("href", live ? "#i-stop" : "#i-screen");
  ui.shareBtn.querySelector("span").textContent = live ? "Parar de transmitir" : "Compartilhar tela";
  ui.shareBtn.setAttribute("aria-label", live ? "Parar de transmitir" : "Compartilhar tela");
  ui.qualityBtn.hidden = !live;
  if (!live) closeQuality();
  fitChrome();
}

/* ---------------- Screens in the room ---------------- */

// Each screen has a tile for the grid, kept while it is on the air so its
// video does not start over each time the grid comes back. The tile's own
// video carries that screen's sound while the grid shows. `view`: the
// CanvasView with the picture, when it does not come as a video track.
function addStream(id, stream, name, local, view = null) {
  const existing = state.streams.get(id);
  if (existing?.stream === stream) return;
  if (existing) dropTile(existing);
  if (notice?.id === id) closeNotice();

  const tile = document.createElement("li");
  tile.innerHTML = `<button class="tile" type="button"><span class="tile-media"><video autoplay playsinline muted></video>${view ? "<canvas></canvas>" : ""}</span><span class="tile-name"></span></button>`;
  const v = tile.querySelector("video");
  v.srcObject = stream;
  if (view) v.classList.add("audio-only"); // the canvas shows the picture; the video only plays the sound
  else v.play().catch(() => {});
  tile.querySelector("button").addEventListener("click", (e) => {
    // The second click of a double click that began on a focused screen: that screen goes fullscreen, not this one.
    if (e.detail > 1) return;
    feature(id, tile);
  });

  state.streams.set(id, { stream, name, local, tile, view });
  if (!view) stream.addEventListener("removetrack", () => { if (!stream.getVideoTracks().length) removeStream(id); });

  // Someone else's screen takes priority over our own preview. A screen
  // whose picture is on its way back keeps its place (removeStream).
  const cur = state.streams.get(state.featured);
  const held = !cur && state.featured !== id && !!refocusTimer;
  if ((!cur && !held) || (cur?.local && !local)) state.featured = id;
  renderStreams();

  // Someone went on the air while another screen was being watched: say so, with their picture.
  const since = state.announce.get(id);
  if (since !== undefined && !local) {
    state.announce.delete(id);
    const fresh = performance.now() - since < 20000;
    if (fresh && state.view === "focus" && state.featured !== id) showNotice(id);
    else if (fresh && state.view === "focus") toast(`${name} começou a transmitir`);
  }
}

// Empty stage: says whether a picture is on its way.
function renderEmpty() {
  const me = state.peer?.id;
  const waitingFor = state.members.get(state.featured);
  const sharing = waitingFor?.sharing && state.featured !== me ? [state.featured, waitingFor]
    : [...state.members].find(([id, m]) => id !== me && m.sharing);
  if (sharing) {
    // Someone is on the air and the picture is on its way (joining, or
    // switching how it travels): not an empty channel.
    ui.emptyTitle.textContent = `Conectando à tela de ${sharing[1].name}…`;
    ui.emptyText.textContent = "A imagem aparece em instantes.";
    return;
  }
  ui.emptyTitle.textContent = "Ninguém está transmitindo";
  ui.emptyText.textContent = CAN_SHARE
    ? "Compartilhe sua tela ou mande o convite pra galera entrar."
    : "Quando alguém transmitir, a imagem aparece aqui.";
}

function dropTile(s) {
  const canvas = s.tile.querySelector("canvas");
  if (canvas) s.view?.detach(canvas);
  s.tile.querySelector("video").srcObject = null;
  s.tile.remove();
}

// `final`: its owner left or stopped sharing.
let refocusTimer = 0;
function removeStream(id, final = false) {
  const s = state.streams.get(id);
  if (!s) {
    // Its picture had already gone and the stage was waiting for it: no longer.
    if (final && state.featured === id && refocusTimer) { clearTimeout(refocusTimer); refocusTimer = 0; leaveFeatured(); renderStreams(); }
    return;
  }
  dropTile(s);
  state.streams.delete(id);
  if (notice?.id === id) closeNotice();
  if (state.featured === id) {
    clearTimeout(refocusTimer);
    refocusTimer = 0;
    if (!final && !s.local && state.members.get(id)?.sharing) {
      // Still on the air: the picture is coming back (a connection opened again, or
      // switching how it travels). The stage waits for it a moment instead of jumping.
      refocusTimer = setTimeout(() => {
        refocusTimer = 0;
        if (state.featured !== id || state.streams.has(id)) return;
        leaveFeatured();
        renderStreams();
      }, 8000);
    } else leaveFeatured();
  }
  renderStreams();
}
// The screen being watched is gone: with two or more left all of them show, otherwise the one left.
function leaveFeatured() {
  const others = [...state.streams].sort((a, b) => a[1].local - b[1].local);
  state.featured = others[0]?.[0] ?? null;
  if (state.view === "focus" && gridOrder().length > 1) state.view = "grid";
}

// One screen fills the stage. `from`: the tile it grows out of.
function feature(id, from = null) {
  if (!state.streams.has(id)) return;
  const fromRect = from && contentRect(from.querySelector(".tile-media"), mediaSize(state.streams.get(id)));
  state.featured = id;
  state.view = "focus";
  renderStreams();
  if (fromRect) zoom(state.canvasView ? ui.featureCanvas : ui.feature, fromRect);
}

// Every screen side by side; the one that was on the stage shrinks back into its tile.
function showGrid() {
  clearTimeout(gridTimer);
  const s = state.streams.get(state.featured);
  const fromRect = s && contentRect(ui.stage, mediaSize(s));
  state.view = "grid";
  renderStreams();
  if (state.view !== "grid") return;
  if (s && fromRect) zoom(s.tile, fromRect, s.tile.querySelector(".tile-media"));
  if (!reducedMotion.matches) {
    for (const li of ui.grid.children) if (li !== s?.tile) li.animate([{ opacity: 0, transform: "scale(0.96)" }, { opacity: 1, transform: "none" }], { duration: 320, easing: "cubic-bezier(0.32, 0.72, 0, 1)" });
  }
}

// Screens in the grid, in the order people came in, our own last. A sharer
// whose picture is still on its way already has a place.
function gridOrder() {
  const me = state.peer?.id;
  const ids = [];
  for (const [id, m] of state.members) if (id !== me && (m.sharing || state.streams.has(id))) ids.push(id);
  for (const id of state.streams.keys()) if (id !== me && !ids.includes(id)) ids.push(id);
  if (state.streams.has(me)) ids.push(me);
  return ids;
}

function renderStreams() {
  if (state.streams.has(state.featured)) { clearTimeout(refocusTimer); refocusTimer = 0; }
  const order = gridOrder();
  if (order.length < 2) state.view = "focus";
  const grid = state.view === "grid";
  const s = grid ? null : state.streams.get(state.featured);
  const onStage = grid || !!s;
  const wasIdle = ui.stage.classList.contains("idle");
  ui.stage.classList.toggle("idle", !onStage);
  ui.stage.classList.toggle("is-grid", grid);
  if (onStage && wasIdle) wake(); // a picture just arrived: the controls rest after a moment
  ui.stageTools.hidden = !onStage;
  ui.gridBtn.hidden = grid || order.length < 2;
  renderEmpty();

  showCanvas(s?.view ?? null);
  if (!s) {
    if (!onStage) setImmersive(false);
    ui.feature.srcObject = null;
  } else {
    if (ui.feature.srcObject !== s.stream) ui.feature.srcObject = s.stream;
    syncAspect();
  }
  const remote = grid ? [...state.streams.values()].filter((x) => !x.local) : s && !s.local ? [s] : [];
  $("volumeWrap").hidden = !remote.some((x) => x.stream.getAudioTracks().length > 0);
  // On a canvas, picture in picture goes through a captured copy of it.
  ui.pipBtn.hidden = grid || !document.pictureInPictureEnabled || (!!s?.view && !ui.featureCanvas.captureStream);

  // The floating window shows the stage, which the grid empties.
  if (grid && document.pictureInPictureElement) document.exitPictureInPicture().catch(() => {});
  renderGrid(order, grid);
  syncPlayback();
  fitChrome();
}

function renderGrid(order, grid) {
  for (const [id, li] of state.waiting) {
    if (state.streams.has(id) || !order.includes(id)) { li.remove(); state.waiting.delete(id); }
  }
  const items = order.map((id) => {
    const s = state.streams.get(id);
    if (s) {
      const label = s.local ? "Você" : s.name;
      s.tile.querySelector(".tile-name").textContent = label;
      s.tile.querySelector("button").setAttribute("aria-label", s.local ? "Ver a sua tela" : `Ver a tela de ${s.name}`);
      const canvas = s.tile.querySelector("canvas");
      // A tile's canvas paints only while the grid shows.
      if (canvas) grid ? s.view.attach(canvas, { thumb: true, every: 1, width: 1280 }) : s.view.detach(canvas);
      return s.tile;
    }
    let li = state.waiting.get(id);
    if (!li) {
      li = document.createElement("li");
      li.innerHTML = `<div class="tile is-waiting"><span class="tile-wait">Conectando…</span><span class="tile-name"></span></div>`;
      state.waiting.set(id, li);
    }
    li.querySelector(".tile-name").textContent = state.members.get(id)?.name ?? "Alguém";
    return li;
  });
  // Moving a video in the page pauses it: only what is out of place moves.
  items.forEach((li, i) => { if (ui.grid.children[i] !== li) ui.grid.insertBefore(li, ui.grid.children[i] ?? null); });
  while (ui.grid.children.length > items.length) ui.grid.lastChild.remove();
  layoutGrid();
}

// The biggest 16:9 tiles that fit the stage, rows centred like on a table.
const GRID_GAP = 12;
function layoutGrid() {
  if (!ui.stage.classList.contains("is-grid")) return;
  const n = ui.grid.children.length;
  const cs = getComputedStyle(ui.grid);
  const w = ui.grid.clientWidth - parseFloat(cs.paddingLeft) - parseFloat(cs.paddingRight);
  const h = ui.grid.clientHeight - parseFloat(cs.paddingTop) - parseFloat(cs.paddingBottom);
  let best = 0;
  for (let cols = 1; cols <= n; cols++) {
    const rows = Math.ceil(n / cols);
    best = Math.max(best, Math.min((w - GRID_GAP * (cols - 1)) / cols, ((h - GRID_GAP * (rows - 1)) / rows) * 16 / 9));
  }
  ui.grid.style.setProperty("--tile-w", `${Math.max(0, Math.floor(best))}px`);
}
new ResizeObserver(() => { layoutGrid(); fitChrome(); }).observe(ui.stage);

// The dock and the tools share the bottom edge of the picture. When they would
// touch (a narrow window, a full channel, the quality button while sharing),
// the volume slider goes first, then the share button's words.
function fitChrome() {
  ui.room.classList.remove("tight", "tighter");
  if (ui.stageTools.hidden || ui.room.hidden) return;
  const touching = () => {
    const a = ui.dock.getBoundingClientRect(), b = ui.stageTools.getBoundingClientRect();
    return a.right + 8 > b.left && b.right + 8 > a.left && a.bottom > b.top && b.bottom > a.top;
  };
  if (!touching()) return;
  ui.room.classList.add("tight");
  if (touching()) ui.room.classList.add("tighter");
}

// Width and height of a screen's picture, for the zoom between a tile and the stage.
function mediaSize(s) {
  if (s.view) return { w: s.view.width, h: s.view.height };
  const v = s.tile.querySelector("video");
  return { w: v.videoWidth || ui.feature.videoWidth, h: v.videoHeight || ui.feature.videoHeight };
}
// Where the picture sits inside a box that shows it whole (object-fit: contain).
function contentRect(el, { w, h }) {
  const r = el.getBoundingClientRect();
  if (!w || !h || !r.width || !r.height) return null;
  const scale = Math.min(r.width / w, r.height / h);
  const cw = w * scale, ch = h * scale;
  return { left: r.left + (r.width - cw) / 2, top: r.top + (r.height - ch) / 2, width: cw, height: ch };
}
// Moves `el` so the picture inside `box` (el itself by default) starts where `from` was, then
// lets it settle in place: the same picture, travelling between the tile and the stage.
function zoom(el, from, box = el) {
  if (reducedMotion.matches || !from) return;
  const s = state.streams.get(state.featured);
  const to = s && contentRect(box, mediaSize(s));
  if (!to) return;
  const r = el.getBoundingClientRect();
  const k = from.width / to.width;
  const dx = from.left - (r.left + (to.left - r.left) * k);
  const dy = from.top - (r.top + (to.top - r.top) * k);
  for (const a of el.getAnimations()) a.cancel();
  const anim = el.animate(
    [{ transformOrigin: "0 0", transform: `translate(${dx}px, ${dy}px) scale(${k})` }, { transformOrigin: "0 0", transform: "none" }],
    { duration: 420, easing: "cubic-bezier(0.32, 0.72, 0, 1)" },
  );
  if (box !== el) { // a tile flies over its neighbours
    el.style.zIndex = "1";
    anim.finished.catch(() => {}).then(() => { el.style.zIndex = ""; });
  }
}

// On a tall screen the stage is only as tall as the picture (style.css, --ar):
// no black bands above and below a desktop screen shown on a phone.
function syncAspect() {
  const s = state.streams.get(state.featured);
  const w = s?.view ? s.view.width : ui.feature.videoWidth;
  const h = s?.view ? s.view.height : ui.feature.videoHeight;
  const bottom = `${Math.round(ui.stage.getBoundingClientRect().bottom)}px`;
  if (document.documentElement.style.getPropertyValue("--stage-bottom") !== bottom) document.documentElement.style.setProperty("--stage-bottom", bottom);
  if (!w || !h) return;
  const ar = Math.min(2.4, Math.max(0.6, w / h)).toFixed(3);
  if (ui.stage.style.getPropertyValue("--ar") !== ar) ui.stage.style.setProperty("--ar", ar);
}
ui.feature.addEventListener("resize", syncAspect);
setInterval(syncAspect, 500); // a canvas has no resize event; this is a few property reads

// Glass to glass on the video element path: when each frame of the featured
// minimum delay stream is shown (the canvas path reports its own paints).
if ("requestVideoFrameCallback" in HTMLVideoElement.prototype) {
  const onFrame = (now, md) => {
    const entry = state.turboIn.get(state.featured);
    if (entry?.rx.track && ui.feature.srcObject?.getVideoTracks().includes(entry.rx.track)) {
      entry.rx.displayed(Math.round(md.mediaTime * 1e6), Date.now() - (performance.now() - md.expectedDisplayTime));
    }
    ui.feature.requestVideoFrameCallback(onFrame);
  };
  ui.feature.requestVideoFrameCallback(onFrame);
}

// The stage canvas follows the featured screen's CanvasView; the video
// element underneath keeps playing that screen's audio.
function showCanvas(view) {
  if (state.canvasView === view) return;
  state.canvasView?.detach(ui.featureCanvas);
  state.canvasView = view;
  ui.featureCanvas.hidden = !view;
  ui.feature.classList.toggle("audio-only", !!view);
  view?.attach(ui.featureCanvas);
}

/* Volume, fullscreen, picture-in-picture */
state.volume = Number(safeGet("telinha:volume") ?? 1);
// iOS ignores the volume of a media element (always 1): leave only the mute button there.
{ const probe = document.createElement("video"); probe.volume = 0.5; if (probe.volume !== 0.5) document.documentElement.classList.add("fixed-volume"); }
state.muted = false;
ui.volume.value = state.volume;

ui.volume.addEventListener("input", () => {
  state.volume = Number(ui.volume.value);
  state.muted = state.volume === 0;
  safeSet("telinha:volume", state.volume);
  applyAudio();
  renderMute();
});
ui.muteBtn.addEventListener("click", () => {
  // The press that brought the sound back (unlockAudio) does only that.
  if (performance.now() - unlockedAt < 1000) return;
  state.muted = !state.muted;
  if (!state.muted && state.volume === 0) { state.volume = 1; ui.volume.value = 1; }
  applyAudio();
  renderMute();
});
function renderMute() {
  const off = state.muted || state.audioLocked;
  ui.muteBtn.querySelector("use").setAttribute("href", off ? "#i-mute" : "#i-sound");
  ui.muteBtn.setAttribute("aria-label", off ? "Ativar o som" : "Silenciar");
}

/* Sound. What is on the stage is what is heard: the screen in focus through
 * the stage video, or in the grid every screen through its tile. Joining is a
 * click or a key press, which lets the page play sound: Chrome and Firefox
 * remember the page was touched, and Safari unlocks the stage video because
 * it is played right then (primeAudio). Opened straight from a link there was
 * no touch yet: if the browser refuses, the picture plays silent and the sound
 * comes on with the first click, tap or key press anywhere, with no button. */
function primeAudio() { ui.feature.play().catch(() => {}); }

// Elements that carry sound right now.
function audibleMedia() {
  if (state.view === "grid") return [...state.streams.values()].filter((s) => !s.local).map((s) => s.tile.querySelector("video"));
  const s = state.streams.get(state.featured);
  return s && !s.local ? [ui.feature] : [];
}
function applyAudio() {
  const audible = new Set(audibleMedia());
  const silent = state.muted || state.audioLocked;
  for (const v of [ui.feature, ...[...state.streams.values()].map((s) => s.tile.querySelector("video"))]) {
    v.volume = state.volume;
    v.muted = silent || !audible.has(v);
  }
}
// Plays what should be playing; a refusal of sound turns it off until the first touch.
function syncPlayback() {
  applyAudio();
  const media = state.view === "grid" ? [...state.streams.values()].map((s) => s.tile.querySelector("video")) : [ui.feature];
  for (const v of media) {
    // One that should be heard is asked again even while playing: unmuting it
    // without a touch first may have been refused, or have paused it.
    if (!v.srcObject || (!v.paused && v.muted)) continue;
    v.play().catch((err) => {
      if (err?.name !== "NotAllowedError" || v.muted) return;
      lockAudio();
      v.play().catch(() => {});
    });
  }
}
const UNLOCK_EVENTS = ["pointerup", "click", "keydown"];
function lockAudio() {
  if (state.audioLocked) return;
  state.audioLocked = true;
  applyAudio();
  renderMute();
  for (const ev of UNLOCK_EVENTS) addEventListener(ev, unlockAudio, { capture: true });
}
let unlockedAt = -Infinity;
function unlockAudio() {
  if (!state.audioLocked) return;
  state.audioLocked = false;
  unlockedAt = performance.now();
  applyAudio();
  renderMute();
  const media = audibleMedia().filter((v) => v.srcObject && !v.muted);
  Promise.all(media.map((v) => v.play())).then(() => {
    for (const ev of UNLOCK_EVENTS) removeEventListener(ev, unlockAudio, { capture: true });
  }, () => {
    // Not a press that counts (a modifier key, say): silent again until the next one.
    if (state.audioLocked) return;
    state.audioLocked = true;
    applyAudio();
    renderMute();
    for (const v of media) v.play().catch(() => {});
  });
}

const statsBtn = $("statsBtn");
const stats = createStatsPanel({
  container: ui.stage,
  getConnections: () => [
    ...[...state.incoming].map(([peerId, call]) => ({ peerId, name: state.members.get(peerId)?.name || "Alguém", direction: "in", pc: call.peerConnection })),
    ...[...state.turboIn].map(([peerId, e]) => ({ peerId, name: state.members.get(peerId)?.name || "Alguém", direction: "in", pc: e.conn.peerConnection, turbo: () => ({ ...e.rx.stats, render: e.rx.view ? "canvas" : "track", remote: e.remote }) })),
    ...[...state.outgoing].map(([peerId, call]) => ({ peerId, name: state.members.get(peerId)?.name || "Alguém", direction: "out", pc: call.peerConnection, turbo: call.turbo })),
  ].filter((c) => c.pc),
});
statsBtn.addEventListener("click", () => stats.toggle());
ui.stage.addEventListener("statstoggle", (e) => statsBtn.setAttribute("aria-pressed", String(e.detail.open)));

ui.fullBtn.addEventListener("click", toggleFullscreen);
ui.exitFsBtn.addEventListener("click", () => setImmersive(false));
ui.gridBtn.addEventListener("click", () => showGrid());
// Double click or double tap on the picture; a touch double tap raises both events, so only one counts.
let lastFullscreenGesture = 0;
function fullscreenGesture() {
  clearTimeout(gridTimer);
  const now = performance.now();
  if (now - lastFullscreenGesture < 500) return;
  lastFullscreenGesture = now;
  toggleFullscreen();
}
ui.feature.addEventListener("dblclick", fullscreenGesture);
ui.featureCanvas.addEventListener("dblclick", fullscreenGesture);
// Older iPadOS Safari only has the prefixed API; iPhone has none for elements
// (fullscreenEnabled is false there, if the methods exist at all).
function fullscreenApi() {
  const st = ui.stage, d = document;
  const enabled = (d.fullscreenEnabled ?? d.webkitFullscreenEnabled) !== false;
  return {
    element: d.fullscreenElement ?? d.webkitFullscreenElement,
    request: enabled ? st.requestFullscreen?.bind(st) ?? st.webkitRequestFullscreen?.bind(st) : undefined,
    exit: d.exitFullscreen?.bind(d) ?? d.webkitExitFullscreen?.bind(d),
  };
}
// The button always does something: the Fullscreen API where it exists; on iPhone
// the video's own player when the picture is in the <video> (real fullscreen, no
// added delay), otherwise the immersive stage below.
function toggleFullscreen() {
  const fs = fullscreenApi();
  if (fs.element) fs.exit?.();
  else if (state.immersive) setImmersive(false);
  else if (fs.request) fs.request()?.catch?.(() => { if (touchOnly.matches && state.streams.get(state.featured)) setImmersive(true); });
  else if (state.streams.get(state.featured) && !videoFullscreen()) setImmersive(true);
}
// The system player only takes a <video> that has its picture, and the canvas path has none
// (a captured copy would add a frame or more of delay). It also needs a tap: if it does not
// open within a moment (the browser refused it), the immersive stage takes over.
let videoFullscreenOpen = false;
function videoFullscreen() {
  const v = ui.feature;
  if (state.canvasView || !v.webkitSupportsFullscreen || !v.webkitEnterFullscreen) return false;
  try { v.webkitEnterFullscreen(); } catch { return false; }
  videoFullscreenOpen = false;
  setTimeout(() => { if (!videoFullscreenOpen && !v.webkitDisplayingFullscreen) setImmersive(true); }, 800);
  return true;
}
ui.feature.addEventListener("webkitbeginfullscreen", () => { videoFullscreenOpen = true; });
// iOS tends to leave the video paused after its player closes.
ui.feature.addEventListener("webkitendfullscreen", () => { videoFullscreenOpen = false; ui.feature.play().catch(() => {}); });

// Immersive stage: the stage covers the viewport (style.css) with the controls resting
// over the picture. In portrait it can turn a quarter so a wide picture fills the tall screen.
state.immersive = false;
state.rotateFs = safeGet("telinha:fsRotate") !== "off";
const portrait = matchMedia("(orientation: portrait)");
function setImmersive(on) {
  if (state.immersive === on) return;
  state.immersive = on;
  ui.stage.classList.toggle("is-immersive", on);
  syncFullscreenLayout();
  renderImmersive();
  if (on) wake();
  else syncAspect();
}
function renderImmersive() {
  ui.stage.classList.toggle("is-rotated", state.immersive && state.rotateFs);
  ui.exitFsBtn.hidden = !state.immersive;
  ui.rotateBtn.hidden = !state.immersive || !portrait.matches;
  ui.rotateBtn.setAttribute("aria-pressed", String(state.rotateFs));
}
portrait.addEventListener("change", renderImmersive);
ui.rotateBtn.addEventListener("click", () => {
  state.rotateFs = !state.rotateFs;
  safeSet("telinha:fsRotate", state.rotateFs ? "on" : "off");
  renderImmersive();
});
// The layout drops its portrait arrangement while the stage is fullscreen.
function syncFullscreenLayout() {
  ui.stage.classList.toggle("is-fs", state.immersive || fullscreenApi().element === ui.stage);
}
// A phone held upright turns to landscape for it (Android; iOS has no such lock).
for (const ev of ["fullscreenchange", "webkitfullscreenchange"]) {
  document.addEventListener(ev, () => {
    const on = fullscreenApi().element === ui.stage;
    syncFullscreenLayout();
    if (!touchOnly.matches) return;
    if (on) screen.orientation?.lock?.("landscape").catch(() => {});
    else screen.orientation?.unlock?.();
  });
}
ui.pipBtn.addEventListener("click", async () => {
  try {
    if (document.pictureInPictureElement) await document.exitPictureInPicture();
    else if (state.canvasView) await canvasPictureInPicture();
    else await ui.feature.requestPictureInPicture();
  } catch {}
});
// The floating window needs a video: a captured copy of the stage canvas,
// only while it is open (it costs a copy per frame; the canvas keeps painting
// with the tab hidden, see CanvasView.push).
async function canvasPictureInPicture() {
  const track = ui.featureCanvas.captureStream().getVideoTracks()[0];
  const v = Object.assign(document.createElement("video"), { muted: true, playsInline: true, className: "pip-source" });
  v.srcObject = new MediaStream([track]);
  ui.stage.append(v);
  const done = () => { track.stop(); v.srcObject = null; v.remove(); };
  v.addEventListener("leavepictureinpicture", done, { once: true });
  try {
    await v.play();
    await v.requestPictureInPicture();
  } catch (err) {
    done();
    throw err;
  }
}
document.addEventListener("keydown", (e) => {
  if (!state.inRoom || e.target.matches("input") || e.metaKey || e.ctrlKey || e.altKey) return;
  if (e.key === "f" || e.key === "F") toggleFullscreen();
  if (e.key === "Escape" && state.immersive) setImmersive(false);
  if (e.key === "i" || e.key === "I") stats.toggle();
});

// With two or more screens on the air, a click on the one in focus goes back
// to the grid. A double click still means fullscreen, so the single click
// waits to see whether a second one follows: only then, and only with a mouse
// (a tap shows and hides the controls; the grid button is there for touch and
// keyboard). The wait is about as long as a double click on most systems.
const DOUBLE_CLICK_MS = 300;
let gridTimer = 0;
let downPointer = "mouse", panelAtDown = false;
ui.stage.addEventListener("pointerdown", (e) => {
  downPointer = e.pointerType;
  // A click that closes an open panel only closes it.
  panelAtDown = !ui.peoplePanel.hidden || "open" in qp.dataset;
}, { capture: true });
ui.stage.addEventListener("click", (e) => {
  if (downPointer === "touch" || (e.target !== ui.feature && e.target !== ui.featureCanvas)) return;
  if (e.detail > 1) { clearTimeout(gridTimer); return; }
  if (panelAtDown || state.view !== "focus" || gridOrder().length < 2) return;
  clearTimeout(gridTimer);
  gridTimer = setTimeout(showGrid, DOUBLE_CLICK_MS);
});

// The controls over the picture (the tools in its corner and the dock) show
// while the pointer moves over it and rest when it stops or leaves. On touch a
// tap on the picture brings them back, and another tap sends them away.
// Keyboard focus and an open panel keep them up.
let idleTimer;
const chrome = [ui.stageTools, ui.dock, ui.noticeSlot];
function chromeBusy() {
  if (!ui.peoplePanel.hidden || "open" in qp.dataset) return true;
  return chrome.some((el) => el.querySelector(":focus-visible") || (!touchOnly.matches && el.matches(":hover")));
}
function wake() {
  ui.stage.classList.remove("idle-cursor");
  clearTimeout(idleTimer);
  idleTimer = setTimeout(rest, touchOnly.matches ? 3600 : 2600);
}
function rest() {
  clearTimeout(idleTimer);
  if (ui.stage.classList.contains("idle")) return;
  if (chromeBusy()) idleTimer = setTimeout(rest, 1200); // looked at again in a moment
  else ui.stage.classList.add("idle-cursor");
}
const overChrome = (el) => !!el && (ui.stage.contains(el) || ui.dock.contains(el) || ui.peoplePanel.contains(el) || ui.qualityPanel.contains(el));
for (const el of [ui.stage, ui.dock]) {
  el.addEventListener("pointermove", (e) => { if (e.pointerType === "mouse") wake(); });
  // From the picture to the dock floating over it is not leaving.
  el.addEventListener("pointerleave", (e) => { if (e.pointerType === "mouse" && !overChrome(e.relatedTarget)) rest(); });
  el.addEventListener("focusin", wake);
}
let hiddenAtTouch = false; // the pointer down already wakes the controls; the tap needs to know what it found
ui.stage.addEventListener("pointerdown", () => { hiddenAtTouch = ui.stage.classList.contains("idle-cursor"); wake(); });
ui.dock.addEventListener("pointerdown", wake);
ui.stage.addEventListener("click", (e) => {
  if (!touchOnly.matches || e.target.closest("button, input, label, .stats-panel")) return;
  if (!hiddenAtTouch && !ui.stage.classList.contains("idle")) {
    clearTimeout(idleTimer);
    ui.stage.classList.add("idle-cursor");
  }
});
// A double tap on the picture is a fullscreen gesture (not for the controls).
let lastTap = { t: 0, x: 0, y: 0 };
// A click, not a pointer event: the system player on iPhone only opens from one.
ui.stage.addEventListener("click", (e) => {
  if (!touchOnly.matches || e.target.closest("button, input, label, .stats-panel") || !state.streams.get(state.featured) || state.view === "grid") return;
  const now = performance.now();
  if (now - lastTap.t < 320 && Math.hypot(e.clientX - lastTap.x, e.clientY - lastTap.y) < 40) fullscreenGesture();
  lastTap = { t: now, x: e.clientX, y: e.clientY };
});

/* "Fulano iniciou uma transmissão": someone went on the air while another
 * screen was being watched. In the corner, with their live picture; a click
 * goes to it. It leaves by itself, but not while the pointer or focus is on it. */
const NOTICE_MS = 6000;
let notice = null;
function showNotice(id) {
  const s = state.streams.get(id);
  if (!s) return;
  closeNotice();
  const el = document.createElement("button");
  el.type = "button";
  el.className = "notice";
  el.title = "Assistir";
  el.innerHTML = `<span class="notice-media"></span><span class="notice-text"><strong></strong> iniciou uma transmissão</span>`;
  el.querySelector("strong").textContent = s.name;
  const media = el.querySelector(".notice-media");
  let release;
  if (s.view) {
    const c = media.appendChild(document.createElement("canvas"));
    s.view.attach(c, { thumb: true });
    release = () => s.view.detach(c);
  } else {
    const v = media.appendChild(Object.assign(document.createElement("video"), { muted: true, playsInline: true, autoplay: true }));
    v.srcObject = s.stream;
    v.play().catch(() => {});
    release = () => { v.srcObject = null; };
  }
  const n = { id, el, release, timer: 0 };
  const hold = () => clearTimeout(n.timer);
  const resume = () => { clearTimeout(n.timer); n.timer = setTimeout(() => { if (!el.matches(":hover, :focus-visible")) closeNotice(); }, NOTICE_MS / 2); };
  el.addEventListener("pointerenter", hold);
  el.addEventListener("pointerleave", resume);
  el.addEventListener("focus", hold);
  el.addEventListener("blur", resume);
  el.addEventListener("keydown", (e) => { if (e.key === "Escape") { e.stopPropagation(); closeNotice(); } });
  // Opens that stream in fullscreen (still inside the click, so the browser allows it).
  el.addEventListener("click", () => {
    closeNotice();
    feature(id);
    if (!fullscreenApi().element && !state.immersive) toggleFullscreen();
  });
  n.timer = setTimeout(() => closeNotice(), NOTICE_MS);
  ui.noticeSlot.append(el);
  notice = n;
}
function closeNotice() {
  const n = notice;
  if (!n) return;
  notice = null;
  clearTimeout(n.timer);
  const done = () => { n.release(); n.el.remove(); };
  if (reducedMotion.matches || document.hidden) { done(); return; }
  n.el.classList.add("out");
  n.el.addEventListener("animationend", done, { once: true });
  setTimeout(done, 500); // in case the animation never runs
}

/* ---------------- People ---------------- */

const AVATAR_COLORS = ["#cfe0ff", "#ffd9a8", "#bff0d4", "#f5c6ff", "#ffe48a", "#a8ecff", "#ffc2c2"];
// Fewer avatars fit the dock on a narrow phone.
const narrow = matchMedia("(max-width: 480px)");
narrow.addEventListener("change", () => { if (state.peer) renderPeople(); });

// A face drawn from the name's hash, the same for everyone in the channel. Loaded on the way
// into a channel; until then, or if the file fails, the initials stay. Names come from other
// people: they only seed the drawing, and it shows as an image, where an SVG cannot run scripts.
let makeFace = null;
const faces = new Map();
function loadFaces() {
  if (makeFace) return;
  import("./avatars.js").then((m) => {
    makeFace = m.face;
    if (state.peer) renderPeople();
  }).catch(() => {});
}
function faceFor(name) {
  if (!makeFace) return "";
  if (!faces.has(name)) {
    let face = "";
    try { face = makeFace(name.slice(0, 24)); } catch {}
    // The oldest drawing goes when there are too many (a Map keeps insertion order).
    if (faces.size >= MAX_FACES) faces.delete(faces.keys().next().value);
    faces.set(name, face);
  }
  return faces.get(name);
}

function avatarFor(el, id, m) {
  // The face drawn here from the name, or the initials until it loads.
  const face = faceFor(m.name);
  el.textContent = face ? "" : initials(m.name);
  el.style.background = face ? `center / cover no-repeat url("${face}")` : AVATAR_COLORS[hash(m.name + id) % AVATAR_COLORS.length];
  return el;
}

function renderPeople() {
  const me = state.peer.id;
  const list = [...state.members].sort((a, b) => (a[0] === me ? -1 : b[0] === me ? 1 : 0));
  const max = narrow.matches ? 3 : 5;
  const shown = list.slice(0, max);
  ui.people.replaceChildren(...shown.map(([id, m]) => {
    const li = avatarFor(document.createElement("li"), id, m);
    li.title = (id === me ? `${m.name} (você)` : m.name) + (m.sharing ? ", transmitindo" : "");
    li.classList.toggle("sharing", m.sharing);
    return li;
  }));
  if (list.length > shown.length) {
    const li = document.createElement("li");
    li.className = "more";
    li.textContent = `+${list.length - shown.length}`;
    ui.people.append(li);
  }
  const label = `${list.length} ${list.length === 1 ? "pessoa" : "pessoas"} no canal`;
  ui.people.setAttribute("aria-label", label);
  ui.peopleBtn.setAttribute("aria-label", `${label}. Ver quem está no canal`);
  fitChrome();

  // The same people by name, for touch screens that have no tooltips.
  ui.roster.replaceChildren(...list.map(([id, m]) => {
    const li = document.createElement("li");
    const who = document.createElement("span");
    who.className = "who";
    who.textContent = m.name;
    if (id === me) who.append(Object.assign(document.createElement("small"), { textContent: "você" }));
    li.append(avatarFor(Object.assign(document.createElement("span"), { className: "avatar" }), id, m), who);
    if (m.sharing) li.insertAdjacentHTML("beforeend", '<span class="live"><i class="tally"></i>transmitindo</span>');
    return li;
  }));
}

function openRoster() {
  ui.peoplePanel.hidden = false;
  ui.peopleBtn.setAttribute("aria-expanded", "true");
}
function closeRoster(returnFocus) {
  if (ui.peoplePanel.hidden) return;
  ui.peoplePanel.hidden = true;
  ui.peopleBtn.setAttribute("aria-expanded", "false");
  if (returnFocus) ui.peopleBtn.focus();
}
ui.peopleBtn.addEventListener("click", () => (ui.peoplePanel.hidden ? openRoster() : closeRoster()));
document.addEventListener("pointerdown", (e) => {
  if (!ui.peoplePanel.hidden && !ui.peoplePanel.contains(e.target) && !ui.peopleBtn.contains(e.target)) closeRoster();
});
ui.peoplePanel.addEventListener("keydown", (e) => { if (e.key === "Escape") closeRoster(true); });
ui.peopleBtn.addEventListener("keydown", (e) => { if (e.key === "Escape") closeRoster(); });
// Not "resize": Safari fires it whenever its address bar slides away.
matchMedia("(orientation: portrait)").addEventListener("change", () => closeRoster());
function initials(name) {
  const parts = name.trim().split(/\s+/);
  return ((parts[0]?.[0] || "?") + (parts.length > 1 ? parts.at(-1)[0] : "")).toUpperCase();
}
function hash(s) { let h = 0; for (const c of s) h = (h * 31 + c.charCodeAt(0)) >>> 0; return h; }

/* ---------------- Join / leave room ---------------- */

function enterRoom() {
  loadFaces();
  if (!ui.name.value.trim()) { ui.name.value = state.name; safeSet("telinha:name", state.name); }
  document.activeElement?.blur(); // the number pad goes away with the lobby
  state.inRoom = true;
  safeSession("telinha:watching", "visible");
  history.replaceState(null, "", "#" + state.code);
  document.title = `Canal ${state.code} · Telinha`;
  ui.emptyDigits.textContent = state.code;
  ui.shareBtn.hidden = !CAN_SHARE;
  renderPeople();
  renderStreams();
  renderShareButton();
  renderMute();

  // The entry screen expands and becomes the room.
  const r = ui.tv.getBoundingClientRect();
  ui.room.hidden = false;
  ui.lobby.classList.add("leaving");
  if (!reducedMotion.matches) {
    const inset = `inset(${r.top}px ${innerWidth - r.right}px ${innerHeight - r.bottom}px ${r.left}px round 28px)`;
    ui.room.animate(
      [{ clipPath: inset }, { clipPath: "inset(0px 0px 0px 0px round 0px)" }],
      { duration: 560, easing: "cubic-bezier(0.32, 0.72, 0, 1)" },
    );
  } else {
    ui.room.animate([{ opacity: 0 }, { opacity: 1 }], { duration: 200 });
  }
  setTimeout(() => { ui.lobby.hidden = true; }, 300);
  stopStatic();
}

ui.leaveBtn.addEventListener("click", leave);
ui.offairBtn.addEventListener("click", leave);
function leave() {
  safeSession("telinha:watching", null);
  state.localStream?.getTracks().forEach((t) => t.stop());
  state.peer?.destroy();
  location.href = location.pathname;
}

// The host turned this page away: the channel has as many people as it takes.
function channelFull() {
  $("offairTitle").textContent = "O canal está cheio";
  ui.offairText.textContent = `Não cabe mais ninguém no canal ${state.code}. Tente de novo daqui a pouco ou abra outro canal.`;
  offAir();
}

function offAir() {
  if (!state.inRoom) return;
  stopShare();
  for (const id of [...state.streams.keys()]) removeStream(id);
  ui.offair.hidden = false;
  ui.offairBtn.focus();
}

window.addEventListener("beforeunload", () => state.peer?.destroy());

/* Invite */
for (const btn of document.querySelectorAll('[data-action="invite"]')) {
  btn.addEventListener("click", async () => {
    const url = `${location.origin}${location.pathname}#${state.code}`;
    try {
      await navigator.clipboard.writeText(url);
      toast("Convite copiado");
    } catch {
      prompt("Copie o convite:", url);
    }
  });
}

/* ---------------- Join and leave sounds ---------------- */

// Two short glassy notes, rising when someone joins and falling when someone
// leaves. The desktop app (native/src/audio/chime.rs) uses the same formula.
const CHIME = { join: [659.25, 880], leave: [880, 659.25] };
const chimeBuffers = {};
let chimeCtx = null;
let chimeNext = 0; // several joins at once make one sound

// Browsers only let a page make sound after a click or key press.
function unlockChime() {
  if (!chimeCtx) { try { chimeCtx = new AudioContext(); } catch { return; } }
  if (chimeCtx.state === "suspended") chimeCtx.resume().catch(() => {});
}
for (const ev of ["pointerdown", "keydown"]) addEventListener(ev, unlockChime, { capture: true, passive: true });

function playChime(kind) {
  const now = performance.now();
  if (chimeCtx?.state !== "running" || now < chimeNext) return;
  chimeNext = now + 250;
  const src = chimeCtx.createBufferSource();
  src.buffer = chimeBuffers[kind] ??= renderChime(CHIME[kind]);
  src.connect(chimeCtx.destination);
  src.start();
}

function renderChime(notes) {
  const rate = chimeCtx.sampleRate, step = 0.09, len = 0.5;
  const buf = chimeCtx.createBuffer(1, Math.floor((step + len) * rate), rate);
  const out = buf.getChannelData(0);
  for (let i = 0; i < out.length; i++) {
    const t = i / rate;
    out[i] = 0.14 * notes.reduce((v, f, n) => v + chimeNote(f, t - n * step, len), 0);
  }
  return buf;
}

// One note: quick attack, exponential decay and a brighter overtone that fades first.
function chimeNote(freq, t, len) {
  if (t < 0 || t >= len) return 0;
  const env = t < 0.006 ? t / 0.006 : Math.exp(-(t - 0.006) / 0.11);
  const tail = Math.min(1, (len - t) / 0.02); // no click at the end
  const w = 2 * Math.PI * freq * t;
  return env * tail * (Math.sin(w) + 0.2 * Math.sin(2 * w) * Math.exp(-t / 0.04));
}

/* ---------------- Notices ---------------- */

function toast(text) {
  const el = document.createElement("div");
  el.className = "toast";
  el.textContent = text;
  ui.toasts.append(el);
  while (ui.toasts.children.length > 3) ui.toasts.firstChild.remove();
  setTimeout(() => {
    el.classList.add("out");
    el.addEventListener("animationend", () => el.remove(), { once: true });
  }, 3200);
}

/* ---------------- TV static ---------------- */

const staticCtx = $("static").getContext("2d");
const img = staticCtx.createImageData(160, 100);
let staticRaf = 0, lastFrame = 0;
function drawStatic(t) {
  if (t - lastFrame > 60) {
    lastFrame = t;
    const d = img.data;
    for (let i = 0; i < d.length; i += 4) {
      const v = Math.random() * 255;
      d[i] = v * 0.85; d[i + 1] = v * 0.9; d[i + 2] = v; d[i + 3] = 255;
    }
    staticCtx.putImageData(img, 0, 0);
  }
  if (!reducedMotion.matches) staticRaf = requestAnimationFrame(drawStatic);
}
function stopStatic() { cancelAnimationFrame(staticRaf); staticRaf = 0; }
drawStatic(100);
document.addEventListener("visibilitychange", () => {
  if (state.inRoom) {
    // For the streamer's log: a phone that leaves the browser can lose the stream.
    if (state.hostConn?.open) state.hostConn.send({ t: "visible", on: !document.hidden });
    safeSession("telinha:watching", document.hidden ? "hidden" : "visible");
    return;
  }
  if (document.hidden) stopStatic();
  else if (!staticRaf) drawStatic(performance.now());
});

/* ---------------- util ---------------- */

function safeGet(k) { try { return localStorage.getItem(k); } catch { return null; } }
function safeSet(k, v) { try { localStorage.setItem(k, v); } catch {} }
function safeSession(k, v) { try { v === null ? sessionStorage.removeItem(k) : sessionStorage.setItem(k, v); } catch {} }
