#!/usr/bin/env node
// Minimal PeerJS-compatible signaling server for local tests, so the site and
// the native app can meet without the real server (deploy/) or the public
// PeerJS one: `node sinal.mjs [port]`, or measure.mjs --sinal-local.
//
// Serves what both clients use: the WebSocket at /peer/peerjs (OPEN, ID-TAKEN,
// relaying OFFER/ANSWER/CANDIDATE/LEAVE with `src`, EXPIRE for an unknown
// destination), GET /peer/peerjs/id, and GET /api/ice (no STUN/TURN: loopback
// only needs host candidates; ICE_SERVERS, a JSON list, adds some for lan-netns.sh).
// Plain HTTP and WS: the site and the app use them only for localhost/127.0.0.1 servers.

import http from "node:http";
import { WebSocketServer } from "ws";

export function startSignaling(port = 0) {
  const peers = new Map(); // id -> socket
  const server = http.createServer((req, res) => {
    const url = new URL(req.url, "http://x");
    const headers = { "access-control-allow-origin": "*", "cache-control": "no-store" };
    if (url.pathname === "/api/ice") {
      res.writeHead(200, { ...headers, "content-type": "application/json" }).end(JSON.stringify({ iceServers: JSON.parse(process.env.ICE_SERVERS ?? "[]") }));
    } else if (url.pathname === "/peer/peerjs/id") {
      res.writeHead(200, { ...headers, "content-type": "text/plain" }).end(crypto.randomUUID());
    } else if (url.pathname === "/peer" || url.pathname === "/peer/") {
      res.writeHead(200, { ...headers, "content-type": "application/json" }).end('{"name":"PeerJS Server (local)"}');
    } else {
      res.writeHead(404, headers).end();
    }
  });
  const wss = new WebSocketServer({ noServer: true });
  server.on("upgrade", (req, socket, head) => {
    const url = new URL(req.url, "http://x");
    if (url.pathname !== "/peer/peerjs") { socket.destroy(); return; }
    wss.handleUpgrade(req, socket, head, (ws) => {
      const id = url.searchParams.get("id");
      if (!id || peers.has(id)) {
        ws.send(JSON.stringify({ type: "ID-TAKEN", payload: { msg: `ID "${id}" is taken` } }));
        ws.close();
        return;
      }
      peers.set(id, ws);
      ws.send(JSON.stringify({ type: "OPEN" }));
      ws.on("message", (data) => {
        let msg;
        try { msg = JSON.parse(String(data)); } catch { return; }
        if (!msg?.type || msg.type === "HEARTBEAT" || !msg.dst) return;
        const to = peers.get(msg.dst);
        if (to) to.send(JSON.stringify({ type: msg.type, src: id, dst: msg.dst, payload: msg.payload }));
        else if (msg.type !== "LEAVE" && msg.type !== "EXPIRE") ws.send(JSON.stringify({ type: "EXPIRE", src: msg.dst }));
      });
      ws.on("close", () => { if (peers.get(id) === ws) peers.delete(id); });
    });
  });
  return new Promise((resolve) => server.listen(port, "127.0.0.1", () => resolve({ port: server.address().port, close: () => { wss.close(); server.close(); } })));
}

if (import.meta.url === `file://${process.argv[1]}`) {
  const { port } = await startSignaling(Number(process.argv[2] ?? 9000));
  console.log(`signaling at localhost:${port}`);
}
