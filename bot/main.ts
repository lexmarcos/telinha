// Bot do Telinha: só quem está na mesma call do Discord assiste.
//
// Quando alguém transmite pelo app nativo (logado com o Discord), o bot acha
// a call em que a pessoa está e posta um "Assistir" no chat dessa call (ou no
// canal escolhido com /telinha-canal). Quem
// clica é identificado pelo próprio Discord; se estiver na mesma call, recebe
// (só ela vê) um link com um passe assinado. O app de quem transmite só manda
// vídeo para quem chega com passe válido.
//
// O bot não fica conectado ao Discord: recebe as interações por HTTP e
// consulta a API quando precisa. Roda atrás do servidor do Telinha, que
// repassa /discord/* para cá.
//
// Variáveis (.env ao lado): DISCORD_TOKEN, DISCORD_PUBLIC_KEY,
// DISCORD_CLIENT_ID, DISCORD_CLIENT_SECRET, TELINHA_HOST (o domínio), PORT.

const env = (k: string) => {
  const v = Deno.env.get(k)?.trim();
  if (!v) throw new Error(`falta ${k} no .env`);
  return v;
};
const TOKEN = env("DISCORD_TOKEN");
const PUBLIC_KEY = env("DISCORD_PUBLIC_KEY");
const CLIENT_ID = env("DISCORD_CLIENT_ID");
const CLIENT_SECRET = env("DISCORD_CLIENT_SECRET");
const HOST = env("TELINHA_HOST");
const PORT = Number(Deno.env.get("PORT") ?? 8790);
const DATA = new URL("./dados.json", import.meta.url);
const REDIRECT = `https://${HOST}/discord/login/volta`;
const API = "https://discord.com/api/v10";
const PASS_HOURS = 12;

/* ---------------- estado salvo: chaves e sessões ---------------- */

type User = { id: string; name: string; avatar: string | null };
type Saved = {
  key?: { pub: JsonWebKey; priv: JsonWebKey };
  sessions: Record<string, User>;
  /** Canal de anúncios escolhido com /telinha-canal, por servidor. Sem ele, o chat da call. */
  channels?: Record<string, string>;
};

let saved: Saved = { sessions: {} };
try {
  saved = JSON.parse(await Deno.readTextFile(DATA));
} catch { /* primeira vez */ }
const persist = () => Deno.writeTextFile(DATA, JSON.stringify(saved, null, 2));

// Par de chaves dos passes: a privada assina aqui; a pública vai para os apps.
if (!saved.key) {
  const k = await crypto.subtle.generateKey({ name: "Ed25519" }, true, ["sign", "verify"]) as CryptoKeyPair;
  saved.key = { pub: await crypto.subtle.exportKey("jwk", k.publicKey), priv: await crypto.subtle.exportKey("jwk", k.privateKey) };
  await persist();
}
const signKey = await crypto.subtle.importKey("jwk", saved.key.priv, { name: "Ed25519" }, false, ["sign"]);
const passPublic = saved.key.pub.x!; // base64url da chave pública crua

const discordKey = await crypto.subtle.importKey("raw", hex(PUBLIC_KEY), { name: "Ed25519" }, false, ["verify"]);

/* ---------------- transmissões no ar (só na memória) ---------------- */

/** `channel` é a call; `posted` é onde o anúncio foi parar (a call ou o canal escolhido). */
type Live = { id: string; user: User; code: string; peer: string; guild: string; channel: string; posted?: string; message?: string; since: number };
const lives = new Map<string, Live>();
/** Logins em andamento: estado do OAuth → sessão criada (o app busca). */
const pending = new Map<string, { session?: string; at: number }>();

/* ---------------- servidor ---------------- */

Deno.serve({ port: PORT, hostname: "127.0.0.1" }, async (req) => {
  const url = new URL(req.url);
  try {
    switch (`${req.method} ${url.pathname}`) {
      case "POST /discord/interacoes":
        return await interaction(req);
      case "GET /discord/login":
        return login(url);
      case "GET /discord/login/volta":
        return await loginBack(url);
      case "GET /discord/login/resultado":
        return loginResult(url);
      case "GET /discord/chave":
        return json({ chave: passPublic });
      case "POST /discord/ao-vivo":
        return await goLive(await req.json());
      case "POST /discord/fim":
        return await endLive(await req.json());
      default:
        return new Response("não existe", { status: 404 });
    }
  } catch (e) {
    console.error(url.pathname, e);
    return json({ erro: "falha no bot" }, 500);
  }
});
console.log(`bot do Telinha na porta ${PORT}`);

/* ---------------- login do app (uma vez) ---------------- */

function login(url: URL) {
  const state = url.searchParams.get("estado") ?? "";
  if (!/^[\w-]{16,64}$/.test(state)) return new Response("estado inválido", { status: 400 });
  pending.set(state, { at: Date.now() });
  const q = new URLSearchParams({ client_id: CLIENT_ID, response_type: "code", redirect_uri: REDIRECT, scope: "identify", state, prompt: "none" });
  return Response.redirect(`https://discord.com/oauth2/authorize?${q}`, 302);
}

async function loginBack(url: URL) {
  const state = url.searchParams.get("state") ?? "";
  const code = url.searchParams.get("code");
  const p = pending.get(state);
  if (!p || !code) return page("O login não deu certo", "Volte para a bolha e tente de novo.");
  const tok = await fetch(`${API}/oauth2/token`, {
    method: "POST",
    headers: { "Content-Type": "application/x-www-form-urlencoded", Authorization: `Basic ${btoa(`${CLIENT_ID}:${CLIENT_SECRET}`)}` },
    body: new URLSearchParams({ grant_type: "authorization_code", code, redirect_uri: REDIRECT }),
  }).then((r) => r.json());
  if (!tok.access_token) return page("O login não deu certo", "O Discord recusou. Volte para a bolha e tente de novo.");
  const me = await fetch(`${API}/users/@me`, { headers: { Authorization: `Bearer ${tok.access_token}` } }).then((r) => r.json());
  const user: User = { id: me.id, name: me.global_name ?? me.username, avatar: me.avatar };
  const session = b64url(crypto.getRandomValues(new Uint8Array(32)));
  saved.sessions[session] = user;
  await persist();
  p.session = session;
  return page(`Pronto, ${user.name}`, "Pode fechar esta aba e voltar para a bolha.");
}

function loginResult(url: URL) {
  const state = url.searchParams.get("estado") ?? "";
  const p = pending.get(state);
  if (!p?.session) return json({ pronto: false }, p ? 202 : 404);
  pending.delete(state);
  return json({ pronto: true, sessao: p.session, usuario: saved.sessions[p.session] });
}

/* ---------------- começo e fim de uma transmissão ---------------- */

async function goLive(body: { sessao?: string; codigo?: string; peer?: string }) {
  const user = body.sessao ? saved.sessions[body.sessao] : undefined;
  if (!user) return json({ erro: "Entre com o Discord de novo na bolha." }, 401);
  if (!body.codigo || !body.peer) return json({ erro: "faltam dados da sala" }, 400);
  const voice = await findVoice(user.id);
  if (!voice) return json({ erro: "Você não está numa call de um servidor onde o bot do Telinha está." }, 409);

  // Uma transmissão por pessoa: a anterior (se houver) sai do ar.
  for (const old of lives.values()) if (old.user.id === user.id) await finish(old);
  const live: Live = { id: b64url(crypto.getRandomValues(new Uint8Array(12))), user, code: body.codigo, peer: body.peer, ...voice, since: Date.now() };
  lives.set(live.id, live);
  live.posted = saved.channels?.[live.guild] ?? live.channel;
  const msg = await discord(`/channels/${live.posted}/messages`, "POST", announcement(live));
  if (msg.ok) live.message = (await msg.json()).id;
  else console.warn(`sem permissão para anunciar no canal ${live.posted}:`, msg.status);
  return json({ ok: true, transmissao: live.id, chave: passPublic, anunciado: !!live.message });
}

async function endLive(body: { sessao?: string; transmissao?: string }) {
  const user = body.sessao ? saved.sessions[body.sessao] : undefined;
  const live = body.transmissao ? lives.get(body.transmissao) : undefined;
  if (user && live && live.user.id === user.id) await finish(live);
  return json({ ok: true });
}

async function finish(live: Live) {
  lives.delete(live.id);
  if (!live.message) return;
  const mins = Math.max(1, Math.round((Date.now() - live.since) / 60000));
  const dur = mins >= 60 ? `${Math.floor(mins / 60)}h${String(mins % 60).padStart(2, "0")}` : `${mins} min`;
  await discord(`/channels/${live.posted}/messages/${live.message}`, "PATCH", { content: `${live.user.name} transmitiu por ${dur} na call <#${live.channel}>.`, components: [] });
}

function announcement(live: Live) {
  return {
    // <#id> vira o nome da call, clicável: dá para saber de qual call é mesmo num canal de texto.
    content: `📺 **${live.user.name}** está transmitindo a tela na call <#${live.channel}>. Só quem está nela consegue assistir.`,
    components: [{ type: 1, components: [{ type: 2, style: 1, label: "Assistir", custom_id: `assistir:${live.id}` }] }],
    allowed_mentions: { parse: [] },
  };
}

/** Em qual call (servidor e canal) essa pessoa está, entre os servidores do bot. */
async function findVoice(userId: string): Promise<{ guild: string; channel: string } | null> {
  const guilds = await discord("/users/@me/guilds").then((r) => r.json());
  for (const g of Array.isArray(guilds) ? guilds : []) {
    const ch = await voiceChannel(g.id, userId);
    if (ch) return { guild: g.id, channel: ch };
  }
  return null;
}

async function voiceChannel(guild: string, userId: string): Promise<string | null> {
  const r = await discord(`/guilds/${guild}/voice-states/${userId}`);
  if (!r.ok) return null;
  return (await r.json()).channel_id ?? null;
}

/* ---------------- interações (cliques e comandos) ---------------- */

async function interaction(req: Request) {
  const body = await req.text();
  const sig = req.headers.get("X-Signature-Ed25519") ?? "";
  const ts = req.headers.get("X-Signature-Timestamp") ?? "";
  const ok = sig && await crypto.subtle.verify("Ed25519", discordKey, hex(sig), new TextEncoder().encode(ts + body));
  if (!ok) return new Response("assinatura inválida", { status: 401 });
  const it = JSON.parse(body);
  if (it.type === 1) return json({ type: 1 }); // PING do Discord ao salvar o endereço

  const who = it.member?.user ?? it.user;
  if (it.type === 3 && String(it.data?.custom_id).startsWith("assistir:")) {
    const live = lives.get(it.data.custom_id.slice("assistir:".length));
    if (!live) return reply("Essa transmissão já acabou.");
    return await watch(live, who, it.guild_id);
  }
  if (it.type === 2 && it.data?.name === "telinha-canal") {
    // Só quem gerencia o servidor chega aqui (default_member_permissions no registro).
    const chosen = it.data.options?.find((o: { name: string }) => o.name === "canal")?.value as string | undefined;
    saved.channels ??= {};
    if (chosen) saved.channels[it.guild_id] = chosen;
    else delete saved.channels[it.guild_id];
    await persist();
    return reply(chosen
      ? `Pronto: os anúncios do Telinha vão sair em <#${chosen}>. O botão Assistir continua só funcionando para quem está na call.`
      : "Pronto: os anúncios do Telinha voltam a sair no chat da própria call.");
  }
  if (it.type === 2 && it.data?.name === "telinha") {
    // Anuncia de novo a transmissão de quem está nesta call (se o anúncio sumiu).
    const mine = it.guild_id ? await voiceChannel(it.guild_id, who.id) : null;
    for (const live of lives.values()) {
      if (live.guild === it.guild_id && mine && (await voiceChannel(live.guild, live.user.id)) === mine) {
        return json({ type: 4, data: announcement(live) });
      }
    }
    return reply("Ninguém desta call está transmitindo no Telinha agora.");
  }
  return reply("Não entendi esse comando.");
}

async function watch(live: Live, who: { id: string; username: string; global_name?: string; avatar?: string }, guild: string) {
  if (who.id === live.user.id) return reply("Essa é a sua própria transmissão.");
  const [theirs, mine] = await Promise.all([voiceChannel(live.guild, live.user.id), guild === live.guild ? voiceChannel(guild, who.id) : null]);
  if (!theirs) return reply(`${live.user.name} saiu da call, então a transmissão está fechada.`);
  if (mine !== theirs) return reply(`Entre na call em que ${live.user.name} está para assistir.`);
  const pass = await signPass({ v: 1, s: live.code, p: live.peer, u: who.id, n: who.global_name ?? who.username, e: Math.floor(Date.now() / 1000) + PASS_HOURS * 3600 });
  return json({
    type: 4,
    data: {
      flags: 64, // só quem clicou vê
      content: `Seu link para assistir ${live.user.name}. Ele é só seu e vale por ${PASS_HOURS} horas.`,
      components: [{ type: 1, components: [{ type: 2, style: 5, label: "Abrir a transmissão", url: `https://${HOST}/#${live.code}/${pass}` }] }],
    },
  });
}

async function signPass(payload: Record<string, unknown>) {
  const body = b64url(new TextEncoder().encode(JSON.stringify(payload)));
  const sig = new Uint8Array(await crypto.subtle.sign("Ed25519", signKey, new TextEncoder().encode(body)));
  return `${body}.${b64url(sig)}`;
}

/* ---------------- utilidades ---------------- */

function discord(path: string, method = "GET", body?: unknown) {
  return fetch(`${API}${path}`, {
    method,
    headers: { Authorization: `Bot ${TOKEN}`, "Content-Type": "application/json", "User-Agent": "TelinhaBot (telinha, 1)" },
    body: body ? JSON.stringify(body) : undefined,
  });
}

function reply(content: string) {
  return json({ type: 4, data: { flags: 64, content } });
}

function json(v: unknown, status = 200) {
  return new Response(JSON.stringify(v), { status, headers: { "Content-Type": "application/json", "Cache-Control": "no-store" } });
}

function page(title: string, text: string) {
  const esc = (s: string) => s.replace(/[&<>"]/g, (c) => `&#${c.charCodeAt(0)};`);
  return new Response(
    `<!doctype html><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>Telinha</title>
<body style="margin:0;min-height:100vh;display:grid;place-items:center;background:#070b1a;color:#d8e2ff;font:16px/1.5 system-ui,sans-serif">
<main style="text-align:center;padding:24px"><h1 style="font-size:22px;margin:0 0 8px">${esc(title)}</h1><p style="margin:0;color:#8f9ccc">${esc(text)}</p></main>`,
    { headers: { "Content-Type": "text/html; charset=utf-8" } },
  );
}

function hex(s: string) {
  return new Uint8Array(s.match(/../g)!.map((h) => parseInt(h, 16)));
}

function b64url(b: Uint8Array) {
  return btoa(String.fromCharCode(...b)).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

// Logins abandonados somem depois de 10 minutos.
setInterval(() => {
  for (const [k, p] of pending) if (Date.now() - p.at > 600_000) pending.delete(k);
}, 60_000);

// Comandos (registrados de novo a cada início; o Discord ignora se não mudou).
await discord(`/applications/${CLIENT_ID}/commands`, "PUT", [
  { name: "telinha", description: "Mostra o botão para assistir quem está transmitindo nesta call", contexts: [0], integration_types: [0] },
  {
    name: "telinha-canal",
    description: "Escolhe onde o Telinha anuncia as transmissões (sem canal: no chat da própria call)",
    contexts: [0],
    integration_types: [0],
    default_member_permissions: "32", // Gerenciar servidor
    options: [{ type: 7, name: "canal", description: "Canal dos anúncios", required: false, channel_types: [0, 2, 5] }],
  },
]).then(async (r) => {
  if (!r.ok) console.warn("registrar /telinha:", await r.text());
});
