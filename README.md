# Telinha

Screen sharing between friends in the browser, using PeerJS (WebRTC).

One person opens a channel and gets a 4-digit number. The others type the number or open the invite link. Anyone in the channel can stream, and several people can stream at the same time.

## Run locally

```sh
bun server.ts        # http://localhost:5180
```

## Deploy

The app is static, but signaling and relay (TURN) run on your own server: the app uses the address it was opened from. When running on `localhost`, it uses the public PeerJS server (STUN only); to test locally against your server, open it with `?servidor=your.domain`.

The `deploy/` folder has everything for a VPS with Docker and a reverse proxy (Caddy) already running:

- `server.js` + `Dockerfile` + `docker-compose.yml`: `telinha` container in `/opt/telinha`, with the site (`/opt/telinha/public`), PeerJS signaling at `/peer` and temporary TURN credentials at `/api/ice`. It joins the proxy's Docker network (`PROXY_NETWORK`).
- `.env.example`: copy to `/opt/telinha/.env` and fill it in (TURN secret, domain, public IP, proxy network, Caddy certificates folder). The `.env` never goes into the repository.
- `Caddyfile.snippet`: block for the proxy's Caddyfile.
- `turnserver.conf`: coturn template (installed via apt). Generate `/etc/turnserver.conf` by replacing the `__FIELDS__` with the values from `.env`:

  ```sh
  . /opt/telinha/.env
  sed -e "s/__TURN_SECRET__/$TURN_SECRET/" -e "s/__PUBLIC_IP__/$PUBLIC_IP/" -e "s/__TURN_HOST__/$TURN_HOST/" \
    /opt/telinha/turnserver.conf > /etc/turnserver.conf
  ```
- `turn-certs.sh`: copies Caddy's certificate to coturn (TURN over TLS on port 5349). Install it at `/usr/local/bin/telinha-turn-certs` and run it daily from cron.

Update only the site:

```sh
scp index.html style.css app.js stats.js stats.css turbo.js som-linux.conf root@YOUR_VPS:/opt/telinha/public/
```

Ports open in the firewall: 3478 udp/tcp, 5349 tcp and 49160–49999 udp.

## How it works

- Whoever opens the channel registers the id `telinha-canal-v1-NNNN` on the signaling server and keeps the list of who is in the room.
- The others connect to that person over a data channel and receive the list.
- Whoever streams calls each person in the room directly. Video goes end to end, without passing through whoever opened the channel. When the direct connection fails (CGNAT, for example), the VPS's coturn relays the data, which stays encrypted.
- If the VPS does not respond, the app uses the public PeerJS server, STUN only. In this mode there is no relay: PeerJS's TURN servers no longer exist.
- If whoever opened the channel leaves, the channel goes down.

## Latency

Ideas taken from Sunshine/Moonlight:

- The streamer detects whether the GPU encodes H.264, VP9 or AV1 (`mediaCapabilities`) and tells viewers, who put that codec first in their answer.
- The bitrate cap follows the chosen resolution and frame rate (about 0.08 bits per pixel) instead of a fixed value. Over the relay this cut the median delay from 300 to about 216 ms in testing.
- The streamer picks resolution, frame rate and priority (smoothness or sharpness) in the quality button.
- The connection panel (I key) shows estimated delay, codec, buffer, decoding and whether the path is direct or relayed.

### Minimum delay (WebCodecs)

The "Atraso: Mínimo" option in the quality panel (`turbo.js`). It is Sunshine's approach in the browser:

- The streamer reads raw frames from the capture (`MediaStreamTrackProcessor`), encodes once with `VideoEncoder` (`realtime` mode, constant bitrate, hardware H.264 when available, keyframe only on request) and sends the same packets to everyone.
- Video goes over an unordered data channel with at most 150 ms of retransmission, opened over the PeerJS connection. Keyframe requests and reports go over the reliable channel.
- The viewer reassembles, decodes with `optimizeForLatency` and hands the frame immediately to a regular track (`MediaStreamTrackGenerator`), with no jitter buffer. Audio still goes over WebRTC.
- The viewer sends one report per second. If they receive much less than was sent three times in a row, they fall back to WebRTC, which adapts to bandwidth. Viewers on browsers without these APIs (anything but Chrome and Edge) use WebRTC from the start.

Measured on a direct connection, same quality (720p60): median of 45 to 48 ms in minimum mode versus 66 to 80 ms in normal mode, and p95 of 74 to 78 ms versus 83 to 97 ms. Over the relay with both sides on the same home connection, bandwidth could not sustain the fixed rate and the viewer fell back to WebRTC in about 3 seconds, as expected.

`tools/latency` measures end-to-end delay with two automated Chromes: the fake screen draws the time as blocks and the viewer reads those blocks back from the video.

```sh
cd tools/latency && npm install
EXTRA="--warmup 15 --server YOUR_DOMAIN" ./run-suite.sh ../.. novo.jsonl novo 3 1 20
EXTRA="--warmup 15 --server YOUR_DOMAIN --turbo" ./run-suite.sh ../.. turbo.jsonl turbo 3 0 20   # minimum delay mode
node summarize.mjs novo.jsonl
```

Headless Chrome numbers use a software codec with both sides on the same machine, so they are good for comparing versions, not as absolute values.

## Audio

- **Windows:** when choosing "Entire screen", check "Share system audio" in Chrome's picker.
- **Browser tab:** the tab's audio is included on any system.
- **Linux:** Chrome only sends tab audio. The sound output becomes a virtual input "Som do computador" (`som-linux.conf`, a PipeWire loopback that follows the default output), and Telinha picks that input up as a microphone. Install once:

  ```sh
  mkdir -p ~/.config/pipewire/pipewire.conf.d && curl -fsSL https://YOUR_DOMAIN/som-linux.conf -o ~/.config/pipewire/pipewire.conf.d/telinha-som.conf && systemctl --user restart pipewire
  ```

  The quality panel shows this command with the right address already filled in. After the first time (which asks for microphone permission), audio is added automatically when sharing. It can be turned off in the quality panel. Everything that plays in your headphones goes along, including voices from a Discord call.
- **Mac:** needs a virtual device such as BlackHole.
- Viewers receive stereo Opus at up to 192 kb/s (the browser default is mono voice).

## Limits

- Each viewer receives their own copy of the streamer's video, so the streamer's upload limits the group size. It works well for 4 or 5 people.
- Phones can watch, but most cannot stream.
