# How it works

## Channels

* Whoever opens a channel registers the id `telinha-canal-v1-NNNN` on the signaling server and keeps the list of who is in it.
* Everyone else connects to that person and receives the list.
* Whoever streams sends video straight to each viewer. When a direct connection is not possible (CGNAT, strict firewalls), the server's coturn relays it. The video stays encrypted.
* If the server does not answer, the site falls back to the public PeerJS server, without a relay.
* If whoever opened the channel leaves, the channel closes.

## Delay

Ideas from Sunshine and Moonlight:

* The streamer picks the codec the graphics card encodes best (H.264, VP9 or AV1) and viewers prefer it.
* The bitrate follows the chosen resolution and frame rate (about 0.08 bits per pixel).
* The quality button sets resolution, frame rate and priority (smoothness or sharpness).
* The **I** key opens a panel with delay, codec, buffer and whether the path is direct or relayed.

**Minimum delay mode** (`turbo.js`, Chrome and Edge): the streamer encodes once with WebCodecs and sends the same packets to everyone over an unordered data channel. Viewers decode and show each frame right away, with no jitter buffer. If a viewer keeps losing data, they switch back to normal WebRTC. On a direct connection at 720p60 this measured 45 to 48 ms, against 66 to 80 ms in normal mode.

`tools/latency` measures the delay with two automated Chromes:

```sh
cd tools/latency && npm install
EXTRA="--warmup 15 --server YOUR_DOMAIN" ./run-suite.sh ../.. novo.jsonl novo 3 1 20
node summarize.mjs novo.jsonl
```

## Audio

* **Windows:** in Chrome's share window, pick the entire screen and check **Share system audio**.
* **Browser tab:** the tab's audio goes along on any system.
* **Linux:** Chrome only sends tab audio. Run this once to add a "Som do computador" input that Telinha picks up:
  ```sh
  mkdir -p ~/.config/pipewire/pipewire.conf.d && curl -fsSL https://YOUR_DOMAIN/som-linux.conf -o ~/.config/pipewire/pipewire.conf.d/telinha-som.conf && systemctl --user restart pipewire
  ```
* **Mac:** needs a virtual device such as BlackHole.
* The desktop app captures computer audio by itself and leaves Discord out.

## Limits

* Each viewer gets their own copy of the video, so the streamer's upload speed limits the group. 4 or 5 people work well.
* Phones can watch, but most cannot stream.
