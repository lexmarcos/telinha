#!/usr/bin/env node
// Tiny STUN server (Binding requests over UDP only) for lan-netns.sh, where it sits on
// the "internet" side of an emulated NAT router so that browsers and the native app
// gather server-reflexive candidates: `node stun.mjs [host] [port]`.
import dgram from "node:dgram";

const [host, port] = [process.argv[2] ?? "0.0.0.0", Number(process.argv[3] ?? 3478)];
const MAGIC = [0x21, 0x12, 0xa4, 0x42];
const socket = dgram.createSocket("udp4");
socket.on("message", (msg, from) => {
  if (msg.length < 20 || msg.readUInt16BE(0) !== 0x0001) return;
  // XOR-MAPPED-ADDRESS (IPv4) carrying the sender's address as seen here.
  const attr = Buffer.alloc(12);
  attr.writeUInt16BE(0x0020, 0);
  attr.writeUInt16BE(8, 2);
  attr.writeUInt8(1, 5);
  attr.writeUInt16BE(from.port ^ 0x2112, 6);
  from.address.split(".").forEach((octet, i) => { attr[8 + i] = Number(octet) ^ MAGIC[i]; });
  const header = Buffer.alloc(20);
  header.writeUInt16BE(0x0101, 0);
  header.writeUInt16BE(attr.length, 2);
  msg.copy(header, 4, 4, 20); // magic cookie and transaction id
  socket.send(Buffer.concat([header, attr]), from.port, from.address);
});
socket.bind(port, host, () => console.error("stun on", host, port));
