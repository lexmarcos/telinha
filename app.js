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
import { turboSupport, openVideoChannel, TurboSender, TurboReceiver } from "./turbo.js";

const SERVER = new URLSearchParams(location.search).get("servidor")
  ?? (/^(localhost|127\.0\.0\.1|\[::1\])$/.test(location.hostname) ? null : location.host);
const TURBO = turboSupport();
const PREFIX = "telinha-canal-v1-";
const MAX_BITRATE = 8_000_000;
const $ = (id) => document.getElementById(id);
const reducedMotion = matchMedia("(prefers-reduced-motion: reduce)");

const ui = {
  lobby: $("lobby"), tv: $("tv"), code: $("code"), name: $("name"), tune: $("tune"),
  create: $("create"), controls: $("controls"), lobbyError: $("lobbyError"), tvHint: $("tvHint"),
  room: $("room"), stage: $("stage"), feature: $("feature"), strip: $("strip"),
  stageBar: $("stageBar"), featureName: $("featureName"), unmute: $("unmute"),
  volume: $("volume"), muteBtn: $("muteBtn"), pipBtn: $("pipBtn"), fullBtn: $("fullBtn"),
  channelDigits: $("channelDigits"), emptyDigits: $("emptyDigits"),
  emptyTitle: $("emptyTitle"), emptyText: $("emptyText"),
  people: $("people"), shareBtn: $("shareBtn"), leaveBtn: $("leaveBtn"),
  offair: $("offair"), offairText: $("offairText"), offairBtn: $("offairBtn"),
  toasts: $("toasts"),
  qualityBtn: $("qualityBtn"), qualityLabel: $("qualityLabel"), qualityPanel: $("qualityPanel"),
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
  streams: new Map(),          // peerId -> { stream, name, local, thumb }
  turboOut: null,              // TurboSender when sharing in minimum-latency mode
  turboIn: new Map(),          // peerId -> { conn, rx } (their video arriving via WebCodecs)
  audioIn: new Map(),          // peerId -> MediaStream with only their audio (minimum-latency mode)
  demoted: new Set(),          // peers moved back to WebRTC due to congestion
  featured: null,
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
// Invite: #1234. The Discord bot's Watch button also brings a pass
// (#1234/passe), which is removed from the address bar so it is not copy-pasted along.
const [hashRaw = "", hashPass = ""] = location.hash.slice(1).split("/");
const hashCode = hashRaw.replace(/\D/g, "").slice(0, 4);
let discordPass = hashPass.trim();
if (discordPass) history.replaceState(null, "", `${location.pathname}${location.search}#${hashCode}`);
// Watch link opened in a tab already in the channel: the browser only changes
// the "#" and does not reload the page, so the new pass arrives here.
window.addEventListener("hashchange", () => {
  const [raw = "", pass = ""] = location.hash.slice(1).split("/");
  if (!pass.trim()) return;
  discordPass = pass.trim();
  history.replaceState(null, "", `${location.pathname}${location.search}#${raw.replace(/\D/g, "").slice(0, 4)}`);
  passStatus.clear();
  if (state.peer) for (const [id, m] of state.members) if (id !== state.peer.id && m.sharing && m.discord) deliverPass(id);
  renderStreams();
});
// Discord identity carried by the pass (name and avatar), used when the
// person has no name saved yet; the avatar is remembered for next visits.
const passIdentity = (() => {
  try { return JSON.parse(atob(discordPass.split(".")[0].replace(/-/g, "+").replace(/_/g, "/"))); } catch { return {}; }
})();
if (!ui.name.value && passIdentity.n) ui.name.value = String(passIdentity.n).slice(0, 24);
if (isDiscordAvatar(passIdentity.a)) safeSet("telinha:avatar", passIdentity.a);
const myAvatar = isDiscordAvatar(safeGet("telinha:avatar")) ? safeGet("telinha:avatar") : "";
if (hashCode.length === 4) {
  ui.code.value = hashCode;
  // Channel in the link and a name already known: straight into the channel.
  if (ui.name.value.trim()) setTimeout(() => join(hashCode), 0);
  else setTimeout(() => ui.name.focus(), 950);
}
syncLobby();

/** Only Discord's avatar CDN: nobody can make others load an arbitrary URL. */
function isDiscordAvatar(url) {
  return typeof url === "string" && url.startsWith("https://cdn.discordapp.com/avatars/");
}

ui.tv.addEventListener("pointerdown", (e) => {
  if (e.target !== ui.code) { e.preventDefault(); ui.code.focus(); }
});
ui.code.addEventListener("input", () => {
  ui.code.value = ui.code.value.replace(/\D/g, "").slice(0, 4);
  syncLobby();
});
ui.name.addEventListener("input", syncLobby);

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

function displayName() {
  const n = ui.name.value.trim().replace(/\s+/g, " ");
  if (n) safeSet("telinha:name", n);
  return n || "Alguém";
}

function setBusy(btn, text) {
  ui.tune.disabled = ui.create.disabled = !!btn;
  if (btn) { btn.dataset.label = btn.textContent; btn.textContent = text; }
  else {
    for (const b of [ui.tune, ui.create]) if (b.dataset.label) { b.textContent = b.dataset.label; delete b.dataset.label; }
    syncLobby();
  }
}

function showLobbyError(msg) { ui.lobbyError.textContent = msg; ui.lobbyError.hidden = false; }
function hideLobbyError() { ui.lobbyError.hidden = true; }

/* ---------------- Open channel (host) ---------------- */

async function host(attempt = 0) {
  state.name = displayName();
  setBusy(ui.create, "Abrindo…");
  const code = String(1000 + Math.floor(Math.random() * 9000));
  try {
    const peer = await openPeer(PREFIX + code);
    state.peer = peer;
    state.isHost = true;
    state.code = code;
    state.roster.set(peer.id, { name: state.name, avatar: myAvatar, sharing: false, wc: TURBO.receive });
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

function acceptGuest(conn) {
  if (conn.metadata?.kind === "turbo") return; // minimum-latency video, handled in wirePeer
  conn.on("open", () => {
    const name = String(conn.metadata?.name || "Alguém").slice(0, 24);
    state.guests.set(conn.peer, conn);
    const avatar = isDiscordAvatar(conn.metadata?.avatar) ? conn.metadata.avatar : "";
    state.roster.set(conn.peer, { name, avatar, sharing: false, wc: !!conn.metadata?.wc });
    broadcastRoster();
  });
  conn.on("data", (msg) => {
    if (msg?.t === "sharing" && state.roster.has(conn.peer)) {
      const m = state.roster.get(conn.peer);
      m.sharing = !!msg.on;
      m.discord = !!msg.on && !!msg.discord; // only people in the Discord call can watch
      broadcastRoster();
    }
  });
  const gone = () => {
    if (!state.guests.has(conn.peer)) return;
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
  state.name = displayName();
  setBusy(ui.tune, "Sintonizando…");
  let peer;
  try {
    peer = await openPeer();
    const conn = await connectTo(peer, PREFIX + code, { name: state.name, avatar: myAvatar, wc: TURBO.receive });
    state.peer = peer;
    state.code = code;
    state.hostConn = conn;
    conn.on("data", (msg) => {
      if (msg?.t === "roster" && Array.isArray(msg.members)) applyRoster(msg.members);
    });
    conn.on("close", () => offAir());
    wirePeer(peer);
    enterRoom();
  } catch (err) {
    peer?.destroy();
    setBusy(null);
    showLobbyError(err?.type !== "peer-unavailable" ? peerErrorText(err)
      : state.fallback ? `Não consegui falar com o servidor do Telinha, então não dá pra achar o canal ${code}. Confira sua internet e tente de novo.`
      : `O canal ${code} não está no ar. Confira o número com quem te convidou.`);
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
      const res = await fetch(`https://${SERVER}/api/ice`, { signal: AbortSignal.timeout(8000) });
      if (!res.ok) throw new Error(res.status);
      const { iceServers } = await res.json();
      const options = { host: SERVER, port: 443, path: "/peer", secure: true, config: { iceServers } };
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
      return "Não deu pra falar com o servidor de sinalização. Confira sua internet e tente de novo.";
    case "browser-incompatible":
      return "Este navegador não suporta WebRTC. Use Chrome, Edge, Firefox ou Safari atualizados.";
    case "timeout":
      return "O canal demorou demais pra responder. Tente de novo em alguns segundos.";
    case "unavailable-id":
      return "Não achei um número de canal livre. Tente de novo.";
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
    onUnsupported: () => { if (conn.open) conn.send({ t: "unsupported" }); },
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
  conn.on("data", (msg) => { if (msg?.t === "tx") entry.remote = msg; });
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
  call.on("stream", (stream) => { state.audioIn.set(id, stream); showTurbo(id); });
  const end = () => {
    if (state.audioIn.get(id)?.id !== call.remoteStream?.id) return;
    state.audioIn.delete(id);
    showTurbo(id);
  };
  call.on("close", end);
  call.on("error", end);
}

// Combines the decoded video with the audio (which comes over WebRTC) into one stream.
function showTurbo(id) {
  const entry = state.turboIn.get(id);
  if (!entry) return;
  const tracks = [entry.rx.track, ...(state.audioIn.get(id)?.getAudioTracks() ?? [])];
  addStream(id, new MediaStream(tracks), state.members.get(id)?.name || "Alguém", false);
}

function closeIncoming(id) {
  state.turboIn.get(id)?.close();
  state.turboIn.delete(id);
  state.audioIn.delete(id);
  removeStream(id);
}

/* ---------------- Discord pass ---------------- */

// A sharer using the app with "call only" sends video only to those who present
// a pass signed by the Discord bot. The pass goes directly to that person
// (never through the channel roster, which everyone sees).
const passStatus = new Map(); // sharer id → { ok, motivo } | "enviando"

function passTarget() {
  try {
    const body = discordPass.split(".")[0].replace(/-/g, "+").replace(/_/g, "/");
    return JSON.parse(atob(body)).p ?? null;
  } catch { return null; }
}

function deliverPass(id) {
  if (!discordPass || passStatus.has(id) || passTarget() !== id) return;
  passStatus.set(id, "enviando");
  const conn = state.peer.connect(id, { metadata: { kind: "passe", passe: discordPass }, reliable: true, serialization: "json" });
  conn.on("data", (msg) => {
    if (msg?.t !== "passe") return;
    setTimeout(() => conn.close(), 500);
    // The sharer is still linking the stream to Discord: try again.
    if (!msg.ok && msg.tentar) {
      setTimeout(() => { passStatus.delete(id); if (state.members.get(id)?.discord) deliverPass(id); }, 3000);
      return;
    }
    passStatus.set(id, { ok: !!msg.ok, motivo: msg.motivo });
    renderStreams();
  });
  conn.on("error", () => { passStatus.delete(id); });
}

function applyRoster(list) {
  const me = state.peer.id;
  const next = new Map(list.map((m) => [m.id, { name: m.name, avatar: isDiscordAvatar(m.avatar) ? m.avatar : "", sharing: !!m.sharing, wc: !!m.wc, discord: !!m.discord }]));

  for (const [id, prev] of state.members) {
    if (id === me) continue;
    if (!next.has(id)) {
      state.outgoing.get(id)?.close(); state.outgoing.delete(id);
      state.demoted.delete(id);
      closeIncoming(id);
    } else if (prev.sharing && !next.get(id).sharing) {
      closeIncoming(id);
    }
  }
  // The first roster received is the initial state, not news.
  if (state.members.size) {
    let joined = false, left = false;
    for (const [id, m] of next) {
      const prev = state.members.get(id);
      if (id === me) continue;
      if (!prev) { toast(`${m.name} entrou`); joined = true; }
      else if (m.sharing && !prev.sharing) toast(`${m.name} começou a transmitir`);
    }
    for (const [id, m] of state.members) if (id !== me && !next.has(id)) { toast(`${m.name} saiu`); left = true; }
    if (joined) playChime("join");
    else if (left) playChime("leave");
  }
  state.members = next;
  for (const [id, m] of next) if (id !== me && m.sharing && m.discord) deliverPass(id);

  if (state.localStream) for (const id of next.keys()) if (id !== me && !state.outgoing.has(id)) connectMember(id);
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
  const handle = {
    kind: "turbo",
    peerConnection: conn.peerConnection,
    turbo: () => state.turboOut?.viewerStats(id),
    close() { state.turboOut?.removeViewer(id); audio?.close(); conn.close(); },
  };
  state.outgoing.set(id, handle);
  state.turboOut.addViewer(id, channel, { onCongested: () => demote(id, true) });
  conn.on("data", (msg) => {
    if (msg?.t === "key") state.turboOut?.requestKey(500);
    else if (msg?.t === "rx") {
      if (msg.relay) demote(id);
      else state.turboOut?.report(id, msg);
    }
    else if (msg?.t === "unsupported") demote(id);
  });
  // Tells viewers how this side is doing (capture, encoder, hidden tab), so the
  // bottleneck can be found from the viewer's panel alone.
  let info = 0;
  conn.on("open", () => {
    const tracks = state.localStream?.getAudioTracks() ?? [];
    if (tracks.length) audio = state.peer.call(id, new MediaStream(tracks), { metadata: { name: state.name, audioOnly: true } });
    info = setInterval(() => {
      const t = state.turboOut?.viewerStats(id);
      if (!t || !conn.open) return;
      const video = state.localStream?.getVideoTracks()[0];
      conn.send({ t: "tx", captured: t.captured, skipped: t.skipped, dropped: t.dropped, hardware: t.hardware, encodeMs: t.encodeMs, captureFps: video?.getSettings().frameRate, hidden: document.hidden });
    }, 1000);
  });
  const end = () => {
    clearInterval(info);
    if (state.outgoing.get(id) !== handle) return;
    state.outgoing.delete(id);
    state.turboOut?.removeViewer(id);
    audio?.close();
  };
  conn.on("close", end);
  conn.on("error", end);
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
  const end = () => { if (state.outgoing.get(id) === call) state.outgoing.delete(id); };
  call.on("close", end);
  call.on("error", end);
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

function tuneReceiver(pc, codec) {
  if (!pc) return;
  pc.addEventListener("track", ({ receiver, transceiver }) => {
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

// Per-person budget, proportional to pixels per second (~0.08 bit per pixel).
// Measured: a cap sized to the video reduces loss and queuing on the relay;
// below ~0.07 the encoder starts lowering resolution on its own.
function targetBitrate() {
  const { width, height } = targetSize();
  return Math.min(20e6, Math.max(2.5e6, width * height * state.quality.fps * 0.08));
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

function applySenderParams(pc) {
  const { height } = targetSize();
  for (const sender of pc.getSenders()) {
    if (sender.track?.kind !== "video") continue;
    const p = sender.getParameters();
    if (!p.encodings?.length) p.encodings = [{}];
    const captured = sender.track.getSettings().height || height;
    p.encodings[0].maxBitrate = Math.round(targetBitrate());
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
}

/* ---------------- Screens in the room ---------------- */

function addStream(id, stream, name, local) {
  const existing = state.streams.get(id);
  if (existing?.stream === stream) return;
  if (existing) existing.thumb.remove();

  const thumb = document.createElement("li");
  thumb.innerHTML = `<button class="thumb" type="button"><video autoplay playsinline muted></video><span class="thumb-name"><i class="tally"></i><span></span></span></button>`;
  const v = thumb.querySelector("video");
  v.srcObject = stream;
  v.play().catch(() => {});
  thumb.querySelector("button").addEventListener("click", () => feature(id));

  state.streams.set(id, { stream, name, local, thumb });
  stream.addEventListener("removetrack", () => { if (!stream.getVideoTracks().length) removeStream(id); });

  // Someone else's screen takes priority over our own preview.
  const cur = state.streams.get(state.featured);
  if (!cur || (cur.local && !local)) state.featured = id;
  renderStreams();
}

// Empty stage: explains what to do when the stream is call-only.
function renderEmpty() {
  const me = state.peer?.id;
  const gated = [...state.members].find(([id, m]) => id !== me && m.sharing && m.discord);
  if (!gated) {
    ui.emptyTitle.textContent = "Ninguém está transmitindo";
    ui.emptyText.textContent = "Compartilhe sua tela ou mande o convite pra galera entrar.";
    return;
  }
  const [id, m] = gated;
  const st = passStatus.get(id);
  ui.emptyTitle.textContent = `${m.name} transmite só pra call`;
  ui.emptyText.textContent =
    st === "enviando" ? "Conferindo seu passe…"
    : st?.ok ? "Passe aceito. A imagem já vai aparecer."
    : st?.motivo ? st.motivo
    // Pass from a previous stream (the sharer's app restarted).
    : discordPass ? "Seu passe é de uma transmissão anterior. Clique em Assistir de novo no chat da call."
    : "Entre na call do Discord e clique em Assistir no chat dela.";
}

function removeStream(id) {
  const s = state.streams.get(id);
  if (!s) return;
  s.thumb.remove();
  state.streams.delete(id);
  if (state.featured === id) {
    const others = [...state.streams].sort((a, b) => a[1].local - b[1].local);
    state.featured = others[0]?.[0] ?? null;
  }
  renderStreams();
}

function feature(id) {
  if (!state.streams.has(id)) return;
  state.featured = id;
  renderStreams();
}

function renderStreams() {
  const s = state.streams.get(state.featured);
  ui.stage.classList.toggle("idle", !s);
  ui.stageBar.hidden = !s;
  renderEmpty();

  if (!s) {
    ui.feature.srcObject = null;
    ui.unmute.hidden = true;
  } else {
    if (ui.feature.srcObject !== s.stream) {
      ui.feature.srcObject = s.stream;
      ui.feature.muted = s.local || state.muted;
      ui.feature.volume = state.volume;
      ui.feature.play().catch(() => {
        // autoplay with sound blocked: play muted and offer the button
        ui.feature.muted = true;
        ui.feature.play().catch(() => {});
        if (!s.local && s.stream.getAudioTracks().length) ui.unmute.hidden = false;
      });
    }
    ui.featureName.textContent = s.local ? "Sua tela" : s.name;
    const hasAudio = !s.local && s.stream.getAudioTracks().length > 0;
    $("volumeWrap").hidden = !hasAudio;
    ui.pipBtn.hidden = !document.pictureInPictureEnabled;
  }

  const showStrip = state.streams.size > 1;
  const items = showStrip ? [...state.streams] : [];
  for (const [id, item] of items) {
    item.thumb.querySelector(".thumb").setAttribute("aria-current", String(id === state.featured));
    item.thumb.querySelector(".thumb-name > span").textContent = item.local ? "Você" : item.name;
    if (item.thumb.parentNode !== ui.strip) {
      ui.strip.append(item.thumb);
      item.thumb.querySelector("video").play().catch(() => {}); // leaving the DOM pauses the video
    }
  }
  if (!showStrip) for (const item of state.streams.values()) item.thumb.remove();
}

/* Volume, fullscreen, picture-in-picture */
state.volume = Number(safeGet("telinha:volume") ?? 1);
state.muted = false;
ui.volume.value = state.volume;

ui.volume.addEventListener("input", () => {
  state.volume = Number(ui.volume.value);
  state.muted = state.volume === 0;
  ui.feature.volume = state.volume;
  ui.feature.muted = state.muted || !!state.streams.get(state.featured)?.local;
  safeSet("telinha:volume", state.volume);
  renderMute();
});
ui.muteBtn.addEventListener("click", () => {
  state.muted = !state.muted;
  if (!state.muted && state.volume === 0) { state.volume = 1; ui.volume.value = 1; ui.feature.volume = 1; }
  ui.feature.muted = state.muted;
  renderMute();
});
function renderMute() {
  ui.muteBtn.querySelector("use").setAttribute("href", state.muted ? "#i-mute" : "#i-sound");
  ui.muteBtn.setAttribute("aria-label", state.muted ? "Ativar o som" : "Silenciar");
}
ui.unmute.addEventListener("click", () => {
  ui.feature.muted = false;
  state.muted = false;
  ui.feature.play().catch(() => {});
  ui.unmute.hidden = true;
  renderMute();
});

const statsBtn = $("statsBtn");
const stats = createStatsPanel({
  container: ui.stage,
  getConnections: () => [
    ...[...state.incoming].map(([peerId, call]) => ({ peerId, name: state.members.get(peerId)?.name || "Alguém", direction: "in", pc: call.peerConnection })),
    ...[...state.turboIn].map(([peerId, e]) => ({ peerId, name: state.members.get(peerId)?.name || "Alguém", direction: "in", pc: e.conn.peerConnection, turbo: () => ({ ...e.rx.stats, remote: e.remote }) })),
    ...[...state.outgoing].map(([peerId, call]) => ({ peerId, name: state.members.get(peerId)?.name || "Alguém", direction: "out", pc: call.peerConnection, turbo: call.turbo })),
  ].filter((c) => c.pc),
});
statsBtn.addEventListener("click", () => stats.toggle());
ui.stage.addEventListener("statstoggle", (e) => statsBtn.setAttribute("aria-pressed", String(e.detail.open)));

ui.fullBtn.addEventListener("click", toggleFullscreen);
ui.feature.addEventListener("dblclick", toggleFullscreen);
function toggleFullscreen() {
  if (document.fullscreenElement) document.exitFullscreen();
  else (ui.stage.requestFullscreen?.() ?? ui.feature.webkitEnterFullscreen?.());
}
ui.pipBtn.addEventListener("click", async () => {
  try {
    if (document.pictureInPictureElement) await document.exitPictureInPicture();
    else await ui.feature.requestPictureInPicture();
  } catch {}
});
document.addEventListener("keydown", (e) => {
  if (!state.inRoom || e.target.matches("input") || e.metaKey || e.ctrlKey || e.altKey) return;
  if (e.key === "f" || e.key === "F") toggleFullscreen();
  if (e.key === "i" || e.key === "I") stats.toggle();
});

// Hides the stream bar when the mouse rests over the screen.
let idleTimer;
function wake() {
  ui.stage.classList.remove("idle-cursor");
  clearTimeout(idleTimer);
  idleTimer = setTimeout(() => {
    if (!ui.stage.classList.contains("idle") && !ui.stageBar.matches(":hover, :focus-within")) {
      ui.stage.classList.add("idle-cursor");
    }
  }, 2600);
}
ui.stage.addEventListener("pointermove", wake);
ui.stage.addEventListener("pointerdown", wake);
ui.stage.addEventListener("focusin", wake);

/* ---------------- People ---------------- */

const AVATAR_COLORS = ["#cfe0ff", "#ffd9a8", "#bff0d4", "#f5c6ff", "#ffe48a", "#a8ecff", "#ffc2c2"];
function renderPeople() {
  const me = state.peer.id;
  const list = [...state.members].sort((a, b) => (a[0] === me ? -1 : b[0] === me ? 1 : 0));
  const shown = list.slice(0, 5);
  ui.people.replaceChildren(...shown.map(([id, m]) => {
    const li = document.createElement("li");
    li.textContent = initials(m.name);
    li.style.background = AVATAR_COLORS[hash(m.name + id) % AVATAR_COLORS.length];
    if (m.avatar) {
      // Discord photo instead of the initials.
      li.textContent = "";
      li.style.background = `center / cover no-repeat url("${m.avatar}")`;
    }
    li.title = (id === me ? `${m.name} (você)` : m.name) + (m.sharing ? ", transmitindo" : "");
    li.classList.toggle("sharing", m.sharing);
    return li;
  }));
  if (list.length > shown.length) {
    const li = document.createElement("li");
    li.className = "more";
    li.textContent = `+${list.length - shown.length}`;
    li.title = list.slice(5).map(([, m]) => m.name).join(", ");
    ui.people.append(li);
  }
  ui.people.setAttribute("aria-label", `${list.length} ${list.length === 1 ? "pessoa" : "pessoas"} no canal`);
}
function initials(name) {
  const parts = name.trim().split(/\s+/);
  return ((parts[0]?.[0] || "?") + (parts.length > 1 ? parts.at(-1)[0] : "")).toUpperCase();
}
function hash(s) { let h = 0; for (const c of s) h = (h * 31 + c.charCodeAt(0)) >>> 0; return h; }

/* ---------------- Join / leave room ---------------- */

function enterRoom() {
  state.inRoom = true;
  history.replaceState(null, "", "#" + state.code);
  document.title = `Canal ${state.code} · Telinha`;
  ui.channelDigits.textContent = state.code;
  ui.emptyDigits.textContent = state.code;
  if (!navigator.mediaDevices?.getDisplayMedia) ui.shareBtn.disabled = true;
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
  state.localStream?.getTracks().forEach((t) => t.stop());
  state.peer?.destroy();
  location.href = location.pathname;
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
  if (state.inRoom) return;
  if (document.hidden) stopStatic();
  else if (!staticRaf) drawStatic(performance.now());
});

/* ---------------- util ---------------- */

function safeGet(k) { try { return localStorage.getItem(k); } catch { return null; } }
function safeSet(k, v) { try { localStorage.setItem(k, v); } catch {} }
