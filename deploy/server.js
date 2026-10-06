// Servidor do Telinha na VPS: site estático, sinalização PeerJS em /peer
// e credenciais temporárias do coturn em /api/ice.
import crypto from "node:crypto";
import http from "node:http";
import express from "express";
import { ExpressPeerServer } from "peer";

const PORT = 9000;
const SECRET = process.env.TURN_SECRET;
const TURN_HOST = process.env.TURN_HOST;
const CRED_TTL = 24 * 3600;
if (!SECRET || !TURN_HOST) throw new Error("defina TURN_SECRET e TURN_HOST no .env");

const app = express();
app.disable("x-powered-by");
app.use((req, res, next) => { res.set("Access-Control-Allow-Origin", "*"); next(); });

// Credencial no formato do `use-auth-secret` do coturn: usuário = validade em
// segundos, senha = HMAC-SHA1 do usuário com o segredo compartilhado.
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
          `turns:${TURN_HOST}:5349?transport=tcp`,
        ],
        username,
        credential,
      },
    ],
  });
});

// Bot do Discord (bot/), que roda em outra máquina e chega por um túnel SSH.
// BOT_URL no .env (ex.: http://172.18.0.1:8790); sem ele, /discord não existe.
// O corpo passa intacto: o bot confere a assinatura do Discord sobre os bytes.
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

const server = app.listen(PORT, () => console.log(`telinha na porta ${PORT}`));
app.use("/peer", ExpressPeerServer(server, { path: "/", proxied: true, alive_timeout: 60000 }));
app.use(express.static("/srv/public", {
  setHeaders: (res) => res.set("Cache-Control", "no-cache"),
}));
