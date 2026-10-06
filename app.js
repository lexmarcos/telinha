// Telinha: compartilhamento de tela entre amigos via PeerJS.
//
// Quem abre o canal vira o "anfitrião": registra o id fixo `telinha-canal-v1-NNNN`
// no servidor de sinalização do PeerJS e mantém a lista de quem está na sala.
// Todos se conectam ao anfitrião por um canal de dados e recebem essa lista.
// Quem transmite liga (peer.call) direto para cada pessoa da lista, e quem
// assiste só atende. A mídia nunca passa pelo anfitrião.
//
// Sinalização e repasse (TURN) ficam no servidor que serve o site (pasta
// deploy/). Se ele não responder, ou se o app roda em localhost, cai para o
// servidor público do PeerJS, só com STUN. Para testar localmente contra o
// servidor, abra com ?servidor=dominio.do.servidor.

import { createStatsPanel } from "./stats.js";

const SERVER = new URLSearchParams(location.search).get("servidor")
  ?? (/^(localhost|127\.0\.0\.1|\[::1\])$/.test(location.hostname) ? null : location.host);
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
  people: $("people"), shareBtn: $("shareBtn"), leaveBtn: $("leaveBtn"),
  offair: $("offair"), offairText: $("offairText"), offairBtn: $("offairBtn"),
  toasts: $("toasts"),
};

const state = {
  peer: null,
  code: "",
  name: "",
  isHost: false,
  hostConn: null,              // conexão de dados com o anfitrião (quem entrou)
  guests: new Map(),           // anfitrião: peerId -> DataConnection
  roster: new Map(),           // anfitrião: lista oficial, peerId -> { name, sharing }
  members: new Map(),          // peerId -> { name, sharing }
  localStream: null,
  outgoing: new Map(),         // peerId -> MediaConnection (minha tela indo para ele)
  incoming: new Map(),         // peerId -> MediaConnection (a tela dele vindo para mim)
  streams: new Map(),          // peerId -> { stream, name, local, thumb }
  featured: null,
  inRoom: false,
};

/* ---------------- Entrada ---------------- */

ui.name.value = safeGet("telinha:name") || "";
const hashCode = location.hash.replace(/\D/g, "").slice(0, 4);
if (hashCode.length === 4) {
  ui.code.value = hashCode;
  setTimeout(() => (ui.name.value ? ui.tune : ui.name).focus(), 950);
}
syncLobby();

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

/* ---------------- Abrir canal (anfitrião) ---------------- */

async function host(attempt = 0) {
  state.name = displayName();
  setBusy(ui.create, "Abrindo…");
  const code = String(1000 + Math.floor(Math.random() * 9000));
  try {
    const peer = await openPeer(PREFIX + code);
    state.peer = peer;
    state.isHost = true;
    state.code = code;
    state.roster.set(peer.id, { name: state.name, sharing: false });
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
  conn.on("open", () => {
    const name = String(conn.metadata?.name || "Alguém").slice(0, 24);
    state.guests.set(conn.peer, conn);
    state.roster.set(conn.peer, { name, sharing: false });
    broadcastRoster();
  });
  conn.on("data", (msg) => {
    if (msg?.t === "sharing" && state.roster.has(conn.peer)) {
      state.roster.get(conn.peer).sharing = !!msg.on;
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

/* ---------------- Sintonizar (convidado) ---------------- */

async function join(code) {
  state.name = displayName();
  setBusy(ui.tune, "Sintonizando…");
  let peer;
  try {
    peer = await openPeer();
    const conn = await connectTo(peer, PREFIX + code, { name: state.name });
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
    showLobbyError(err?.type === "peer-unavailable"
      ? `O canal ${code} não está no ar. Confira o número com quem te convidou.`
      : peerErrorText(err));
  }
}

let ownServer = null; // guarda só o sucesso; uma falha é tentada de novo na próxima vez
async function peerOptions() {
  if (!SERVER) return { config: { iceServers: [{ urls: "stun:stun.l.google.com:19302" }] } };
  if (ownServer && ownServer.expires > Date.now()) return ownServer.options;
  try {
    const res = await fetch(`https://${SERVER}/api/ice`, { signal: AbortSignal.timeout(4000) });
    const { iceServers } = await res.json();
    const options = { host: SERVER, port: 443, path: "/peer", secure: true, config: { iceServers } };
    ownServer = { options, expires: Date.now() + 12 * 3600_000 };
    return options;
  } catch {
    console.warn("[telinha] servidor próprio indisponível, usando o PeerJS público");
    return { config: { iceServers: [{ urls: "stun:stun.l.google.com:19302" }] } };
  }
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
    const conn = peer.connect(id, { metadata, reliable: true });
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

/* ---------------- Peer: chamadas e reconexão ---------------- */

function wirePeer(peer) {
  peer.on("call", (call) => {
    state.incoming.get(call.peer)?.close();
    state.incoming.set(call.peer, call);
    call.answer();
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
  peer.on("error", (err) => {
    if (err.type === "peer-unavailable") return; // alguém saiu no meio de uma chamada
    console.warn("[telinha]", err.type, err);
  });
}

function applyRoster(list) {
  const me = state.peer.id;
  const next = new Map(list.map((m) => [m.id, { name: m.name, sharing: !!m.sharing }]));

  for (const [id, prev] of state.members) {
    if (id === me) continue;
    if (!next.has(id)) {
      state.outgoing.get(id)?.close(); state.outgoing.delete(id);
      removeStream(id);
    } else if (prev.sharing && !next.get(id).sharing) {
      removeStream(id);
    }
  }
  // A primeira lista que chega é o estado inicial, não novidade.
  if (state.members.size) {
    for (const [id, m] of next) {
      const prev = state.members.get(id);
      if (id === me) continue;
      if (!prev) toast(`${m.name} entrou`);
      else if (m.sharing && !prev.sharing) toast(`${m.name} começou a transmitir`);
    }
    for (const [id, m] of state.members) if (!next.has(id)) toast(`${m.name} saiu`);
  }
  state.members = next;

  if (state.localStream) for (const id of next.keys()) if (id !== me && !state.outgoing.has(id)) callMember(id);
  for (const [id, s] of state.streams) if (!s.local && next.has(id)) s.name = next.get(id).name;
  renderPeople();
  renderStreams();
}

/* ---------------- Transmitir ---------------- */

ui.shareBtn.addEventListener("click", () => (state.localStream ? stopShare() : startShare()));

async function startShare() {
  if (!navigator.mediaDevices?.getDisplayMedia) {
    toast("Este aparelho não deixa compartilhar a tela, mas dá pra assistir.");
    return;
  }
  let stream;
  try {
    stream = await navigator.mediaDevices.getDisplayMedia({
      video: { frameRate: { ideal: 60, max: 60 }, width: { ideal: 1920 }, height: { ideal: 1080 } },
      audio: { echoCancellation: false, noiseSuppression: false, autoGainControl: false },
      systemAudio: "include",
      selfBrowserSurface: "exclude",
      surfaceSwitching: "include",
    });
  } catch (err) {
    if (err.name !== "NotAllowedError" && err.name !== "AbortError") toast("Não consegui capturar a tela.");
    return;
  }
  const video = stream.getVideoTracks()[0];
  if ("contentHint" in video) video.contentHint = "motion";
  video.addEventListener("ended", stopShare);

  state.localStream = stream;
  addStream(state.peer.id, stream, "Você", true);
  for (const id of state.members.keys()) if (id !== state.peer.id) callMember(id);
  announceSharing(true);
  renderShareButton();
}

function stopShare() {
  if (!state.localStream) return;
  state.localStream.getTracks().forEach((t) => t.stop());
  state.localStream = null;
  for (const call of state.outgoing.values()) call.close();
  state.outgoing.clear();
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

function callMember(id) {
  const call = state.peer.call(id, state.localStream, { metadata: { name: state.name } });
  if (!call) return;
  state.outgoing.set(id, call);
  const end = () => { if (state.outgoing.get(id) === call) state.outgoing.delete(id); };
  call.on("close", end);
  call.on("error", end);
  tuneSender(call.peerConnection);
}

// Sobe o teto de bitrate do vídeo (o padrão do navegador deixa texto borrado).
function tuneSender(pc) {
  if (!pc) return;
  const apply = () => {
    for (const sender of pc.getSenders()) {
      if (sender.track?.kind !== "video") continue;
      const p = sender.getParameters();
      if (!p.encodings?.length) p.encodings = [{}];
      p.encodings[0].maxBitrate = MAX_BITRATE;
      p.encodings[0].maxFramerate = 60;
      sender.setParameters(p).catch(() => {});
    }
  };
  pc.addEventListener("connectionstatechange", () => { if (pc.connectionState === "connected") apply(); });
}

function renderShareButton() {
  const live = !!state.localStream;
  ui.shareBtn.classList.toggle("live", live);
  ui.shareBtn.querySelector("use").setAttribute("href", live ? "#i-stop" : "#i-screen");
  ui.shareBtn.querySelector("span").textContent = live ? "Parar de transmitir" : "Compartilhar tela";
  ui.shareBtn.setAttribute("aria-label", live ? "Parar de transmitir" : "Compartilhar tela");
}

/* ---------------- Telas na sala ---------------- */

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

  // A tela de alguém tem prioridade sobre a própria pré-visualização.
  const cur = state.streams.get(state.featured);
  if (!cur || (cur.local && !local)) state.featured = id;
  renderStreams();
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

  if (!s) {
    ui.feature.srcObject = null;
    ui.unmute.hidden = true;
  } else {
    if (ui.feature.srcObject !== s.stream) {
      ui.feature.srcObject = s.stream;
      ui.feature.muted = s.local || state.muted;
      ui.feature.volume = state.volume;
      ui.feature.play().catch(() => {
        // autoplay com som bloqueado: toca mudo e oferece o botão
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
      item.thumb.querySelector("video").play().catch(() => {}); // sair do DOM pausa o vídeo
    }
  }
  if (!showStrip) for (const item of state.streams.values()) item.thumb.remove();
}

/* Volume, tela cheia, janela flutuante */
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
    ...[...state.outgoing].map(([peerId, call]) => ({ peerId, name: state.members.get(peerId)?.name || "Alguém", direction: "out", pc: call.peerConnection })),
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

// Esconde a barra da transmissão quando o mouse fica parado sobre a tela.
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

/* ---------------- Pessoas ---------------- */

const AVATAR_COLORS = ["#cfe0ff", "#ffd9a8", "#bff0d4", "#f5c6ff", "#ffe48a", "#a8ecff", "#ffc2c2"];
function renderPeople() {
  const me = state.peer.id;
  const list = [...state.members].sort((a, b) => (a[0] === me ? -1 : b[0] === me ? 1 : 0));
  const shown = list.slice(0, 5);
  ui.people.replaceChildren(...shown.map(([id, m]) => {
    const li = document.createElement("li");
    li.textContent = initials(m.name);
    li.style.background = AVATAR_COLORS[hash(m.name + id) % AVATAR_COLORS.length];
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

/* ---------------- Entrar / sair da sala ---------------- */

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

  // A telinha da entrada se expande e vira a sala.
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

/* Convite */
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

/* ---------------- Avisos ---------------- */

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

/* ---------------- Chiado da TV ---------------- */

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
