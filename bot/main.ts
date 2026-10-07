// Telinha bot: only people in the same Discord call can watch.
//
// When someone streams from the native app (logged in with Discord), the bot finds
// the call that person is in and posts a "Watch" button in that call's chat (or in
// the channel chosen with /telinha-canal). Whoever
// clicks is identified by Discord itself; if they are in the same call, they get
// (visible only to them) a link with a signed pass. The streamer's app only sends
// video to people who arrive with a valid pass.
//
// The bot does not stay connected to Discord: it receives interactions over HTTP
// and queries the API when needed. It runs behind the Telinha server, which
// forwards /discord/* here.
//
// Variables (.env alongside): DISCORD_TOKEN, DISCORD_PUBLIC_KEY,
// DISCORD_CLIENT_ID, DISCORD_CLIENT_SECRET, TELINHA_HOST (the domain), PORT,
// and optionally BIND (listen address, 0.0.0.0 inside Docker) and DATA_FILE.

const env = (k: string) => {
  const v = Deno.env.get(k)?.trim();
  if (!v) throw new Error(`missing ${k} in .env`);
  return v;
};
const TOKEN = env("DISCORD_TOKEN");
const PUBLIC_KEY = env("DISCORD_PUBLIC_KEY");
const CLIENT_ID = env("DISCORD_CLIENT_ID");
const CLIENT_SECRET = env("DISCORD_CLIENT_SECRET");
const HOST = env("TELINHA_HOST");
const PORT = Number(Deno.env.get("PORT") ?? 8790);
const DATA = new URL(Deno.env.get("DATA_FILE") ?? "./dados.json", import.meta.url);
const REDIRECT = `https://${HOST}/discord/login/volta`;
const API = "https://discord.com/api/v10";
const PASS_HOURS = 12;

/* ---------------- saved state: keys and sessions ---------------- */

type User = { id: string; name: string; avatar: string | null };
type Saved = {
  key?: { pub: JsonWebKey; priv: JsonWebKey };
  sessions: Record<string, User>;
  /** Announcement channel chosen with /telinha-canal, per server. Without it, the call's chat. */
  channels?: Record<string, string>;
  /** Live streams: survive a bot restart (the Watch button keeps working). */
  lives?: Record<string, Live>;
};

let saved: Saved = { sessions: {} };
try {
  saved = JSON.parse(await Deno.readTextFile(DATA));
} catch { /* first run */ }
const persist = () => Deno.writeTextFile(DATA, JSON.stringify(saved, null, 2));

// Pass key pair: the private key signs here; the public one goes to the apps.
if (!saved.key) {
  const k = await crypto.subtle.generateKey({ name: "Ed25519" }, true, ["sign", "verify"]) as CryptoKeyPair;
  saved.key = { pub: await crypto.subtle.exportKey("jwk", k.publicKey), priv: await crypto.subtle.exportKey("jwk", k.privateKey) };
  await persist();
}
const signKey = await crypto.subtle.importKey("jwk", saved.key.priv, { name: "Ed25519" }, false, ["sign"]);
const passPublic = saved.key.pub.x!; // base64url of the raw public key

const discordKey = await crypto.subtle.importKey("raw", hex(PUBLIC_KEY), { name: "Ed25519" }, false, ["verify"]);

/* ---------------- live streams ---------------- */

/** `channel` is the call; `posted` is where the announcement ended up (the call or the chosen channel). */
type Live = { id: string; user: User; code: string; peer: string; guild: string; channel: string; posted?: string; message?: string; since: number };
const lives = new Map<string, Live>(Object.entries(saved.lives ?? {}));
const saveLives = () => {
  saved.lives = Object.fromEntries(lives);
  return persist();
};
/** Logins in progress: OAuth state → created session (the app fetches it). */
const pending = new Map<string, { session?: string; at: number }>();

/* ---------------- server ---------------- */

Deno.serve({ port: PORT, hostname: Deno.env.get("BIND") ?? "127.0.0.1" }, async (req) => {
  const url = new URL(req.url);
  try {
    const route = url.pathname.startsWith("/discord/assistir/") ? "/discord/assistir" : url.pathname;
  switch (`${req.method} ${route}`) {
      case "POST /discord/interacoes":
        return await interaction(req);
      case "GET /discord/assistir":
        return await watchStart(url);
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
console.log(`Telinha bot on port ${PORT}`);

/* ---------------- app login (once) ---------------- */

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
  if (state.startsWith("w.")) {
    pending.delete(state);
    const live = lives.get(state.split(".")[1]);
    if (!live) return page("Essa transmissão já acabou", "Peça um convite novo a quem estava transmitindo.");
    const r = await admit(live, user);
    return "link" in r ? Response.redirect(r.link, 302) : page("Não deu para assistir", r.refused);
  }
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

/* ---------------- start and end of a stream ---------------- */

async function goLive(body: { sessao?: string; codigo?: string; peer?: string }) {
  const user = body.sessao ? saved.sessions[body.sessao] : undefined;
  if (!user) return json({ erro: "Entre com o Discord de novo na bolha." }, 401);
  if (!body.codigo || !body.peer) return json({ erro: "faltam dados da sala" }, 400);
  const voice = await findVoice(user.id);
  if (!voice) return json({ erro: "Você não está numa call de um servidor onde o bot do Telinha está." }, 409);

  // One stream per person: the previous one (if any) goes offline.
  for (const old of lives.values()) if (old.user.id === user.id) await finish(old);
  const live: Live = { id: b64url(crypto.getRandomValues(new Uint8Array(12))), user, code: body.codigo, peer: body.peer, ...voice, since: Date.now() };
  lives.set(live.id, live);
  await saveLives();
  console.log(`live: ${user.name} in room ${live.code} (call ${live.channel})`);
  live.posted = saved.channels?.[live.guild] ?? live.channel;
  const msg = await discord(`/channels/${live.posted}/messages`, "POST", announcement(live));
  if (msg.ok) {
    live.message = (await msg.json()).id;
    await saveLives();
  }
  else console.warn(`no permission to announce in channel ${live.posted}:`, msg.status);
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
  await saveLives();
  // The announcement goes away with the stream: only live streams stay in the chat.
  if (live.message) await discord(`/channels/${live.posted}/messages/${live.message}`, "DELETE");
}

function announcement(live: Live) {
  return {
    // <#id> becomes the clickable call name: shows which call it is even in a text channel.
    // A mention (not the Discord display name), short and clear. allowed_mentions
    // empty: it shows as @person without pinging anyone.
    content: `<@${live.user.id}> iniciou uma transmissão. Clique no botão abaixo para ver.`,
    // Link button: opens the browser directly. The bot identifies who clicked
    // through Discord login (automatic after the first authorization).
    components: [{ type: 1, components: [{ type: 2, style: 5, label: "Assistir", url: `https://${HOST}/discord/assistir/${live.id}` }] }],
    allowed_mentions: { parse: [] },
  };
}

/** Which call (server and channel) this person is in, among the bot's servers. */
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

/* ---------------- interactions (clicks and commands) ---------------- */

async function interaction(req: Request) {
  const body = await req.text();
  const sig = req.headers.get("X-Signature-Ed25519") ?? "";
  const ts = req.headers.get("X-Signature-Timestamp") ?? "";
  const ok = sig && await crypto.subtle.verify("Ed25519", discordKey, hex(sig), new TextEncoder().encode(ts + body));
  if (!ok) return new Response("assinatura inválida", { status: 401 });
  const it = JSON.parse(body);
  if (it.type === 1) return json({ type: 1 }); // Discord PING when saving the endpoint URL

  const who = it.member?.user ?? it.user;
  if (it.type === 3 && String(it.data?.custom_id).startsWith("assistir:")) {
    const live = lives.get(it.data.custom_id.slice("assistir:".length));
    if (!live) return reply("Essa transmissão já acabou.");
    return await watch(live, who, it.guild_id);
  }
  if (it.type === 2 && it.data?.name === "telinha-canal") {
    // Only server managers get here (default_member_permissions at registration).
    const chosen = it.data.options?.find((o: { name: string }) => o.name === "canal")?.value as string | undefined;
    saved.channels ??= {};
    if (chosen) saved.channels[it.guild_id] = chosen;
    else delete saved.channels[it.guild_id];
    await persist();
    return reply(chosen
      ? `Pronto: os anúncios do Telinha vão sair em <#${chosen}>. O botão Assistir continua só funcionando para quem está na call.`
      : "Pronto: os anúncios do Telinha voltam a sair no chat da própria call.");
  }
  if (it.type === 2 && it.data?.name === "new-telinha") {
    // Discord cannot open the browser by itself: a link button does it in one
    // click. The link carries the person's Discord name and avatar, so the site
    // opens the channel as them.
    const name = it.member?.nick ?? who.global_name ?? who.username;
    const avatar = who.avatar ? `https://cdn.discordapp.com/avatars/${who.id}/${who.avatar}.png?size=64` : "";
    const url = `https://${HOST}/#novo/${encodeURIComponent(name)}${avatar ? `/${encodeURIComponent(avatar)}` : ""}`;
    return json({
      type: 4,
      data: {
        flags: 64,
        content: "Clique no botão para abrir um canal novo do Telinha no navegador.",
        components: [{ type: 1, components: [{ type: 2, style: 5, label: "Abrir canal novo", url }] }],
      },
    });
  }
  if (it.type === 2 && it.data?.name === "telinha") {
    // Re-announces the stream of whoever is in this call (if the announcement is gone).
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

/** Checks the call and returns the watch link with a pass, or why not. */
async function admit(live: Live, who: { id: string; name: string; avatar?: string | null }): Promise<{ link: string } | { refused: string }> {
  const said = (why: string) => console.log(`Watch: ${who.name} → ${live.user.name} (room ${live.code}): ${why}`);
  if (who.id === live.user.id) return { refused: "Essa é a sua própria transmissão." };
  const [theirs, mine] = await Promise.all([voiceChannel(live.guild, live.user.id), voiceChannel(live.guild, who.id)]);
  if (!theirs) {
    said("streamer is not in a call");
    return { refused: `${live.user.name} saiu da call, então a transmissão está fechada.` };
  }
  if (mine !== theirs) {
    said(`outside the call (in ${mine ?? "none"}, stream in ${theirs})`);
    return { refused: `Entre na call em que ${live.user.name} está para assistir.` };
  }
  said("pass issued");
  // Name and avatar go along so the website can show the person as on Discord.
  const a = who.avatar ? `https://cdn.discordapp.com/avatars/${who.id}/${who.avatar}.png?size=64` : undefined;
  const pass = await signPass({ v: 1, s: live.code, p: live.peer, u: who.id, n: who.name, a, e: Math.floor(Date.now() / 1000) + PASS_HOURS * 3600 });
  return { link: `https://${HOST}/#${live.code}/${pass}` };
}

/** Assistir link: Discord login (to know who clicked), then straight to the stream. */
async function watchStart(url: URL) {
  const id = url.pathname.split("/").pop() ?? "";
  if (!lives.has(id)) return page("Essa transmissão já acabou", "Peça um convite novo a quem estava transmitindo.");
  const state = `w.${id}.${b64url(crypto.getRandomValues(new Uint8Array(12)))}`;
  pending.set(state, { at: Date.now() });
  const q = new URLSearchParams({ client_id: CLIENT_ID, response_type: "code", redirect_uri: REDIRECT, scope: "identify", state, prompt: "none" });
  return Response.redirect(`https://discord.com/oauth2/authorize?${q}`, 302);
}

async function watch(live: Live, who: { id: string; username: string; global_name?: string; avatar?: string | null }, _guild: string) {
  const r = await admit(live, { id: who.id, name: who.global_name ?? who.username, avatar: who.avatar });
  if ("refused" in r) return reply(r.refused);
  return json({
    type: 4,
    data: {
      flags: 64, // only the clicker sees it
      content: `Seu link para assistir ${live.user.name}. Ele é só seu e vale por ${PASS_HOURS} horas.`,
      components: [{ type: 1, components: [{ type: 2, style: 5, label: "Abrir a transmissão", url: r.link }] }],
    },
  });
}

async function signPass(payload: Record<string, unknown>) {
  const body = b64url(new TextEncoder().encode(JSON.stringify(payload)));
  const sig = new Uint8Array(await crypto.subtle.sign("Ed25519", signKey, new TextEncoder().encode(body)));
  return `${body}.${b64url(sig)}`;
}

/* ---------------- utilities ---------------- */

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

// Abandoned logins are dropped after 10 minutes.
setInterval(() => {
  for (const [k, p] of pending) if (Date.now() - p.at > 600_000) pending.delete(k);
}, 60_000);

// Commands (re-registered on every start; Discord ignores them if unchanged).
await discord(`/applications/${CLIENT_ID}/commands`, "PUT", [
  { name: "telinha", description: "Mostra o botão para assistir quem está transmitindo nesta call", contexts: [0], integration_types: [0] },
  { name: "new-telinha", description: "Abre um canal novo do Telinha no navegador", contexts: [0, 1], integration_types: [0] },
  {
    name: "telinha-canal",
    description: "Escolhe onde o Telinha anuncia as transmissões (sem canal: no chat da própria call)",
    contexts: [0],
    integration_types: [0],
    default_member_permissions: "32", // Manage Server
    options: [{ type: 7, name: "canal", description: "Canal dos anúncios", required: false, channel_types: [0, 2, 5] }],
  },
]).then(async (r) => {
  if (!r.ok) console.warn("register /telinha:", await r.text());
});
