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
* The bitrate follows the chosen resolution and frame rate: about 0.08 bits per pixel (0.1 when sharpness comes first), up to 45 Mb/s, or 20 for whoever receives through the relay.
* The quality button sets resolution, frame rate and priority (smoothness or sharpness).
* The **I** key opens a panel with delay, codec, buffer and whether the path is direct or relayed.

**Minimum delay mode** (`turbo.js`, Chrome and Edge stream it): the streamer encodes once with WebCodecs and sends the same packets to everyone over an unordered data channel. Viewers decode and show each frame right away, with no jitter buffer. If a viewer keeps losing data, they switch back to normal WebRTC. On a direct connection at 720p60 this measured 45 to 48 ms, against 66 to 80 ms in normal mode.

Chrome and Edge viewers hand each decoded frame to a video track. Safari 16.4+ (iPad included) and Firefox 130+ have the decoder but not that track, so they paint the newest frame on a canvas once per screen refresh, dropping any frame that a newer one replaced before its turn. Their audio still comes over WebRTC. If their decoder rejects the H.264 as it travels (Annex B), they retry it in the MP4 layout (avcC); if it still fails, that viewer gets normal WebRTC. `?render=canvas` forces the canvas in Chrome, and `measure.mjs --canvas` measures it.

**Desktop app over RTP** (viewers on the relay, or whose browser lacks WebCodecs): each viewer's connection runs Google Congestion Control on the browser's transport-wide feedback, and a pacer releases packets at 2.5 times the estimate, so a keyframe is not held back for long. The app's single encoder follows the lowest estimate among RTP viewers: far below the bitrate it drops at once, otherwise in the usual big, spaced steps, and it only climbs as far as the estimates allow. A viewer whose estimate is under half of the next worst one's does not set the bitrate for everyone; it keeps getting the stream, paced and retransmitted. Giving that viewer a lighter stream of its own needs temporal layers (dropping every other frame for them only) or a second encoder, which is not done yet.

**HEVC and AV1 (desktop app, behind `TELINHA_HEVC=1`)**: viewers list the codecs their browser says it decodes in hardware (`VideoDecoder.isConfigSupported` with `prefer-hardware`) next to `wc` in the roster. The app encodes HEVC (NVENC, VAAPI, AMF or Quick Sync, with the same low-latency settings as H.264, Main profile) only while every viewer gets the data channel video and listed `hevc`; anyone on RTP, or joining without it, switches the encoder back to H.264 with a keyframe. A viewer whose decoder still refuses HEVC is noted and the room goes back to H.264, keeping that viewer on the data channel. HEVC needs no SPS rewrite: its reorder count is a mandatory field, and the app logs a warning if an encoder ever writes more than zero. Still to do before turning it on: try real hardware decoders (Safari and Chrome on Windows and macOS decode HEVC in hardware; Linux Chrome usually does not), make the site's own sender encode HEVC, and use AV1 the same way where the GPU encodes it (RTX 40, Arc, RDNA3: `av1_nvenc`, `av1_qsv`, `av1_amf`, `av1_vaapi` are in the bundled FFmpeg) and viewers list `av1`.

**Mouse pointer as data (not done yet)**: the pointer is drawn into the captured image, so every move changes pixels and costs encoded bits on an otherwise still screen. The plan: on Linux ask the portal for `CursorMode::Metadata` (when the portal offers it) and read `SPA_META_Cursor` from each PipeWire buffer (position, hotspot, and a bitmap only when the shape changes); on Windows draw nothing in `ddagrab` (`draw_mouse=0`) and read the pointer with `GetCursorInfo`/`GetIconInfo`. Send position at up to the display rate and the bitmap once per shape over the control channel (`{t:"cursor", x, y}` and `{t:"cursorShape", id, w, h, hx, hy, png}`, coordinates in video pixels), and draw it in the viewer as an absolutely positioned element over the stage (or on the canvas path, on the canvas after the frame), scaled with the video's object-fit box. Viewers that do not announce support keep the pointer in the picture: the app would capture with it embedded while any of them watches. A pointer that moves more smoothly than the video is also a latency win, since it no longer waits for an encoded frame.

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
