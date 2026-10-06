// Painel de estatísticas da conexão (latência, codec, caminho).
//
// Lê pc.getStats() uma vez por segundo enquanto está aberto. Os valores "por
// quadro" e as taxas vêm da diferença entre duas leituras seguidas; o atraso
// estimado soma meia ida e volta, o buffer de jitter e a decodificação.

const TICK = 1000;
const DASH = "—";
const n0 = new Intl.NumberFormat("pt-BR", { maximumFractionDigits: 0 });
const n1 = new Intl.NumberFormat("pt-BR", { minimumFractionDigits: 1, maximumFractionDigits: 1 });

const LIMIT = { none: "nenhum", cpu: "CPU", bandwidth: "banda", other: "outro" };

const ARROW_IN = `<svg class="stats-dir" viewBox="0 0 16 16" aria-hidden="true"><path d="M12 4 4.5 11.5M4 6.5v5.5h5.5" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"/></svg>`;
const ARROW_OUT = `<svg class="stats-dir" viewBox="0 0 16 16" aria-hidden="true"><path d="M4 12l7.5-7.5M6.5 4H12v5.5" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"/></svg>`;
const CLOSE = `<svg viewBox="0 0 16 16" aria-hidden="true"><path d="m4.5 4.5 7 7m0-7-7 7" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round"/></svg>`;

const ROWS = {
  in: [
    ["codec", "Codec"], ["res", "Resolução"], ["rate", "Taxa recebida"],
    ["buffer", "Buffer"], ["decode", "Decodificação"], ["rtt", "Ida e volta"],
    ["lost", "Pacotes perdidos"], ["freeze", "Travamentos"],
  ],
  out: [
    ["codec", "Codec"], ["res", "Resolução"], ["rate", "Taxa enviada"],
    ["encode", "Codificação"], ["limit", "Limitação"], ["retx", "Retransmissões"],
  ],
};

export function createStatsPanel({ container, getConnections }) {
  const panel = document.createElement("aside");
  panel.className = "stats-panel";
  panel.hidden = true;
  panel.setAttribute("aria-label", "Estatísticas da conexão");
  panel.innerHTML = `<div class="stats-top"><span class="stats-title">Conexão</span><button class="stats-close" type="button" aria-label="Fechar estatísticas" title="Fechar">${CLOSE}</button></div><div class="stats-list"></div><p class="stats-empty">Nenhuma conexão de vídeo agora.</p>`;
  const list = panel.querySelector(".stats-list");
  const empty = panel.querySelector(".stats-empty");
  panel.querySelector(".stats-close").addEventListener("click", () => toggle(false));
  container.append(panel);

  const blocks = new Map(); // "in:peerId" -> { el, f, pc, prev }
  let timer = 0, busy = false, alive = true;

  function toggle(force) {
    if (!alive) return;
    const open = typeof force === "boolean" ? force : panel.hidden;
    if (open === !panel.hidden) return;
    panel.hidden = !open;
    clearInterval(timer);
    timer = 0;
    if (open) { tick(); timer = setInterval(tick, TICK); }
    // avisa quem tem o botão de abrir (o painel também fecha pelo próprio x)
    panel.dispatchEvent(new CustomEvent("statstoggle", { bubbles: true, detail: { open } }));
  }

  async function tick() {
    if (busy || panel.hidden) return;
    busy = true;
    try {
      let conns = [];
      try { conns = (getConnections() || []).filter((c) => c && c.peerId && c.pc); } catch {}
      conns.sort((a, b) => (a.direction === "out") - (b.direction === "out"));

      const seen = new Set();
      for (const c of conns) {
        const dir = c.direction === "out" ? "out" : "in";
        const key = `${dir}:${c.peerId}`;
        if (seen.has(key)) continue;
        seen.add(key);
        let b = blocks.get(key);
        if (!b || b.pc !== c.pc) { b?.el.remove(); b = makeBlock(dir); b.pc = c.pc; blocks.set(key, b); }
        b.dir = dir;
        b.name = c.name;
        if (b.el.parentNode !== list || list.children[seen.size - 1] !== b.el) {
          list.insertBefore(b.el, list.children[seen.size - 1] || null);
        }
      }
      for (const [key, b] of blocks) if (!seen.has(key)) { b.el.remove(); blocks.delete(key); }
      empty.hidden = blocks.size > 0;

      await Promise.all([...blocks.values()].map(async (b) => {
        const report = await readStats(b.pc);
        if (!alive || panel.hidden) return;
        try { render(b, report); } catch {}
      }));
    } finally {
      busy = false;
    }
  }

  function destroy() {
    alive = false;
    clearInterval(timer);
    panel.remove();
    blocks.clear();
  }

  return { toggle, isOpen: () => !panel.hidden, destroy };
}

/* ---------------- Leitura ---------------- */

async function readStats(pc) {
  try {
    if (!pc || pc.signalingState === "closed" || pc.connectionState === "closed") return null;
    return await pc.getStats();
  } catch {
    return null;
  }
}

function parse(report, dir) {
  const all = new Map();
  try { report?.forEach((s) => all.set(s.id, s)); } catch {}

  let rtp = null;
  const type = dir === "in" ? "inbound-rtp" : "outbound-rtp";
  for (const s of all.values()) {
    if (s.type !== type || (s.kind ?? s.mediaType) !== "video") continue;
    // com simulcast pode haver mais de um; fica com o que mais trafegou
    const bytes = dir === "in" ? s.bytesReceived : s.bytesSent;
    const best = dir === "in" ? rtp?.bytesReceived : rtp?.bytesSent;
    if (!rtp || (bytes ?? 0) > (best ?? 0)) rtp = s;
  }

  let pair = null;
  const transport = all.get(rtp?.transportId) || [...all.values()].find((s) => s.type === "transport");
  if (transport?.selectedCandidatePairId) pair = all.get(transport.selectedCandidatePairId);
  if (!pair) {
    for (const s of all.values()) {
      if (s.type === "candidate-pair" && s.nominated && s.state === "succeeded") { pair = s; break; }
    }
  }

  return {
    rtp,
    codec: all.get(rtp?.codecId),
    pair,
    local: all.get(pair?.localCandidateId),
    remote: all.get(pair?.remoteCandidateId),
  };
}

/* ---------------- Desenho ---------------- */

function makeBlock(dir) {
  const el = document.createElement("section");
  el.className = "stats-block";
  el.dataset.dir = dir;
  const rows = ROWS[dir].map(([k, label]) => `<dt>${label}</dt><dd data-f="${k}">${DASH}</dd>`).join("");
  el.innerHTML = `
    <header class="stats-head">${dir === "in" ? ARROW_IN : ARROW_OUT}<h3 class="stats-who"><span class="stats-verb">${dir === "in" ? "Recebendo de" : "Enviando para"}</span> <span data-f="name"></span></h3></header>
    <div class="stats-hero">
      <div class="stats-hero-main">
        <span class="stats-hero-label">${dir === "in" ? "Atraso estimado" : "Ida e volta"}</span>
        <span class="stats-hero-value" data-f="hero" data-empty>${DASH}</span>
      </div>
      <span class="stats-path" data-f="path" data-kind="unknown">${DASH}</span>
    </div>
    <dl class="stats-grid">${rows}</dl>`;
  const f = {};
  for (const node of el.querySelectorAll("[data-f]")) f[node.dataset.f] = node;
  return { el, f, prev: null };
}

function render(b, report) {
  const { f, dir } = b;
  setText(f.name, b.name || "Alguém");
  const state = safe(() => b.pc.connectionState);
  const { rtp, codec, pair, local, remote } = parse(report, dir);

  const cur = rtp ? snapshot(rtp, dir) : null;
  const prev = b.prev && cur && b.prev.id === cur.id && cur.t > b.prev.t ? b.prev : null;
  b.prev = cur;
  const dt = prev ? (cur.t - prev.t) / 1000 : 0;
  const d = (k) => (prev && isNum(cur[k]) && isNum(prev[k]) ? cur[k] - prev[k] : null);
  // por quadro: usa o intervalo se houve quadros nele; na primeira leitura, a média acumulada
  const perFrame = (sumKey, countKey) => {
    const ds = d(sumKey), dc = d(countKey);
    if (prev) return dc > 0 ? (ds / dc) * 1000 : null;
    return cur && cur[countKey] > 0 && isNum(cur[sumKey]) ? (cur[sumKey] / cur[countKey]) * 1000 : null;
  };

  const rtt = isNum(pair?.currentRoundTripTime) ? pair.currentRoundTripTime * 1000 : null;

  // codec
  const parts = [];
  if (codec?.mimeType) parts.push(codec.mimeType.replace(/^video\//i, ""));
  const impl = dir === "in" ? rtp?.decoderImplementation : rtp?.encoderImplementation;
  if (impl && impl !== "unknown") parts.push(impl);
  const hw = dir === "in" ? rtp?.powerEfficientDecoder : rtp?.powerEfficientEncoder;
  if (typeof hw === "boolean") parts.push(hw ? "hardware" : "software");
  setText(f.codec, parts.join(" · ") || DASH);

  // resolução e quadros
  const w = rtp?.frameWidth, h = rtp?.frameHeight, fps = rtp?.framesPerSecond;
  const res = isNum(w) && isNum(h) ? `${w}×${h}` : "";
  const rate = isNum(fps) ? `${n0.format(fps)} fps` : "";
  setText(f.res, [res, rate].filter(Boolean).join(" · ") || DASH);

  const bytes = d("bytes");
  setText(f.rate, dt > 0 && bytes >= 0 ? bitrate((bytes * 8) / dt) : DASH);

  if (dir === "in") {
    const buffer = perFrame("jbDelay", "jbEmitted");
    const decode = perFrame("decodeTime", "framesDecoded");
    setHTML(f.buffer, msPerFrame(buffer));
    setHTML(f.decode, msPerFrame(decode));
    setText(f.rtt, ms(rtt));

    const lost = d("lost");
    if (isNum(cur?.lost)) {
      setHTML(f.lost, `${isNum(lost) ? n0.format(Math.max(0, lost)) : DASH}<span class="stats-unit"> · ${n0.format(cur.lost)} no total</span>`);
    } else setText(f.lost, DASH);
    setText(f.freeze, isNum(rtp?.freezeCount) ? n0.format(rtp.freezeCount) : DASH);

    const est = isNum(rtt) && isNum(buffer) && isNum(decode) ? rtt / 2 + buffer + decode : null;
    hero(f.hero, est);
    f.hero.parentNode.title = isNum(est)
      ? `Estimativa: metade da ida e volta (${ms(rtt)}) + buffer (${ms(buffer)}) + decodificação (${ms(decode)}). Não inclui captura, codificação e exibição.`
      : "";
  } else {
    setHTML(f.encode, msPerFrame(perFrame("encodeTime", "framesEncoded")));
    const reason = rtp?.qualityLimitationReason;
    setText(f.limit, reason ? LIMIT[reason] ?? reason : DASH);
    const retx = rtp?.retransmittedPacketsSent, nack = rtp?.nackCount;
    const bits = [];
    if (isNum(retx)) bits.push(`${n0.format(retx)} pacotes`);
    if (isNum(nack)) bits.push(`${n0.format(nack)} NACK`);
    setText(f.retx, bits.join(", ") || DASH);
    hero(f.hero, rtt);
  }

  renderPath(f.path, state, local, remote);
}

function renderPath(el, state, local, remote) {
  let kind = "unknown", text = DASH, title = "";
  if (state === "closed" || state === "failed") {
    kind = "off"; text = "Encerrada";
  } else if (local || remote) {
    const lt = local?.candidateType, rt = remote?.candidateType;
    title = `Daqui: ${lt ?? "?"}${local?.protocol ? ` (${local.protocol})` : ""} · De lá: ${rt ?? "?"}`;
    if (lt === "relay" || rt === "relay") {
      kind = "relay";
      const proto = lt === "relay" ? local?.relayProtocol : null;
      text = `Repasse (TURN${proto ? ` ${proto}` : ""})`;
      title += lt === "relay" ? "" : " · o repasse está do lado de lá";
    } else if (lt || rt) {
      kind = "direct"; text = "Direta";
    }
  } else if (state === "new" || state === "connecting") {
    text = "Conectando…";
  }
  if (el.dataset.kind !== kind) el.dataset.kind = kind;
  setText(el, text);
  if (el.title !== title) el.title = title;
}

function snapshot(s, dir) {
  const base = { id: s.id, t: s.timestamp };
  if (dir === "in") {
    return {
      ...base, bytes: s.bytesReceived, lost: s.packetsLost,
      jbDelay: s.jitterBufferDelay, jbEmitted: s.jitterBufferEmittedCount,
      decodeTime: s.totalDecodeTime, framesDecoded: s.framesDecoded,
    };
  }
  return { ...base, bytes: s.bytesSent, encodeTime: s.totalEncodeTime, framesEncoded: s.framesEncoded };
}

/* ---------------- Formatação ---------------- */

function isNum(v) { return typeof v === "number" && Number.isFinite(v); }
function safe(fn) { try { return fn(); } catch { return undefined; } }
function fmtMs(v) { return v < 10 ? n1.format(v) : n0.format(v); }
function ms(v) { return isNum(v) ? `${fmtMs(v)} ms` : DASH; }
function msPerFrame(v) {
  return isNum(v) ? `${fmtMs(v)} ms<span class="stats-unit"> por quadro</span>` : DASH;
}
function bitrate(bps) {
  if (!isNum(bps)) return DASH;
  if (bps >= 1e6) return `${n1.format(bps / 1e6)} Mb/s`;
  return `${n0.format(bps / 1e3)} kb/s`;
}
function hero(el, v) {
  setHTML(el, isNum(v) ? `${fmtMs(v)}<span class="stats-hero-unit">ms</span>` : DASH);
  el.toggleAttribute("data-empty", !isNum(v));
}
function setText(el, text) {
  if (el.textContent === text && !("html" in el.dataset)) return;
  el.textContent = text;
  delete el.dataset.html;
}
// só para marcação gerada aqui (números e unidades), nunca para nomes
function setHTML(el, html) { if (el.dataset.html !== html) { el.innerHTML = html; el.dataset.html = html; } }
