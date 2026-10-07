// Telinha server on the VPS: static site, PeerJS signaling at /peer
// and temporary coturn credentials at /api/ice.
import crypto from "node:crypto";
import http from "node:http";
import express from "express";
import { ExpressPeerServer } from "peer";

const PORT = 9000;
const SECRET = process.env.TURN_SECRET;
const TURN_HOST = process.env.TURN_HOST;
const CRED_TTL = 24 * 3600;
// TURN over TLS (port 5349) needs coturn to have the site's certificate; the
// all-in-one setup (setup.sh) runs without it.
const TURN_TLS = process.env.TURN_TLS !== "off";
if (!SECRET || !TURN_HOST) throw new Error("set TURN_SECRET and TURN_HOST in .env");

const app = express();
app.disable("x-powered-by");
// Only the site itself (and a local copy for development) may call this server
// from a browser: other sites cannot hand out this server's TURN credentials.
// The desktop app is not a browser, so this does not affect it.
const SITE = `https://${TURN_HOST}`;
const LOCAL = /^http:\/\/(localhost|127\.0\.0\.1)(:\d+)?$/;
app.use((req, res, next) => {
  const origin = req.get("Origin");
  if (origin === SITE || LOCAL.test(origin ?? "")) res.set("Access-Control-Allow-Origin", origin);
  res.set("Vary", "Origin");
  next();
});

// Credential in coturn's `use-auth-secret` format: username = expiry in
// seconds, password = HMAC-SHA1 of the username with the shared secret.
app.get("/api/ice", (req, res) => {
  const username = `${Math.floor(Date.now() / 1000) + CRED_TTL}:telinha`;
  const credential = crypto.createHmac("sha1", SECRET).update(username).digest("base64");
  res.set("Cache-Control", "no-store");
  res.json({
    iceServers: [
      { urls: `stun:${TURN_HOST}:3478` },
      {
        urls: [
          `turn:${TURN_HOST}:3478?transport=udp`,
          `turn:${TURN_HOST}:3478?transport=tcp`,
          ...(TURN_TLS ? [`turns:${TURN_HOST}:5349?transport=tcp`] : []),
        ],
        username,
        credential,
      },
    ],
  });
});

// Discord bot (bot/), wherever it runs: BOT_URL in .env is its address as seen
// from this container (e.g. http://bot:8790); without it, /discord does not exist.
// The body passes through untouched: the bot verifies Discord's signature over the bytes.
const BOT_URL = process.env.BOT_URL;
if (BOT_URL) {
  const bot = new URL(BOT_URL);
  app.use("/discord", (req, res) => {
    const up = http.request(
      { host: bot.hostname, port: bot.port, method: req.method, path: req.originalUrl, headers: { ...req.headers, host: bot.host } },
      (r) => { res.writeHead(r.statusCode ?? 502, r.headers); r.pipe(res); },
    );
    up.on("error", () => { if (!res.headersSent) res.status(502).json({ erro: "o bot do Discord está fora do ar" }); });
    req.pipe(up);
  });
}

const server = app.listen(PORT, () => console.log(`telinha on port ${PORT}`));
app.use("/peer", ExpressPeerServer(server, { path: "/", proxied: true, alive_timeout: 60000 }));
app.use(express.static("/srv/public", {
  setHeaders: (res) => res.set("Cache-Control", "no-cache"),
}));
