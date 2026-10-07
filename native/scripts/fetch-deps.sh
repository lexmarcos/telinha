#!/bin/sh
# Downloads and prepares what the app needs to build and package, in .deps/
# (nothing is installed system-wide, no sudo needed):
#   - FFmpeg 9 (BtbN, LGPL): prebuilt for Linux; for Windows, built with the NVENC
#     of older drivers (scripts/ffmpeg-windows.sh, needs Docker)
#   - libva 2.22 (the new FFmpeg's VAAPI requires >= 2.21; LTS distros ship 2.20)
#   - PipeWire headers (extracted from Ubuntu .debs)
#   - Zig (builds targeting glibc 2.35), appimagetool and NSIS
# System requirements: curl, python3, ninja, clang, libdrm (headers), apt-get.
set -eu
cd "$(dirname "$0")/.."
D="$PWD/.deps"
mkdir -p "$D" "$D/debs"
B=https://github.com/BtbN/FFmpeg-Builds/releases/download/latest

if [ ! -d "$D/ffmpeg-linux" ]; then
  echo "FFmpeg (Linux)"
  curl -fsSL "$B/ffmpeg-n9.0-latest-linux64-lgpl-shared-9.0.tar.xz" | tar -xJ -C "$D"
  mv "$D"/ffmpeg-n9.0-latest-linux64-lgpl-shared-9.0 "$D/ffmpeg-linux"
fi
if [ ! -d "$D/ffmpeg-win" ]; then
  echo "FFmpeg (Windows)"
  if command -v docker >/dev/null 2>&1; then
    # Built with the NVENC that runs on NVIDIA drivers from 2023 onward.
    sh scripts/ffmpeg-windows.sh
  else
    echo "warning: no Docker, using the prebuilt BtbN FFmpeg: its NVENC requires driver 610+"
    curl -fsSL -o "$D/win.zip" "$B/ffmpeg-n9.0-latest-win64-lgpl-shared-9.0.zip"
    unzip -q "$D/win.zip" -d "$D" && rm "$D/win.zip"
    mv "$D"/ffmpeg-n9.0-latest-win64-lgpl-shared-9.0 "$D/ffmpeg-win"
  fi
fi

[ -d "$D/venv" ] || python3 -m venv "$D/venv"
"$D/venv/bin/pip" install -q meson ziglang

cat > "$D/zigcc-2.35" <<ZIG
#!/bin/sh
exec "$D/venv/bin/python3" -m ziglang cc -target x86_64-linux-gnu.2.35 -Wl,--allow-shlib-undefined "\$@"
ZIG
chmod +x "$D/zigcc-2.35"

if [ ! -f "$D/libva/lib/libva.so.2" ]; then
  echo "libva 2.22"
  curl -fsSL https://github.com/intel/libva/archive/refs/tags/2.22.0.tar.gz | tar -xz -C "$D"
  mkdir -p "$D/drm-include"
  cp /usr/include/xf86drm*.h "$D/drm-include/" && cp -r /usr/include/libdrm "$D/drm-include/"
  (cd "$D/libva-2.22.0" && CC="$D/zigcc-2.35" CFLAGS="-O2 -I$D/drm-include -I$D/drm-include/libdrm" \
    "$D/venv/bin/meson" setup build --prefix="$D/libva" --libdir=lib -Dwith_x11=no -Dwith_glx=no -Dwith_wayland=no \
      -Denable_docs=false -Ddriverdir=/usr/lib/x86_64-linux-gnu/dri:/usr/lib64/dri:/usr/lib/dri \
    && ninja -C build install)
fi

if [ ! -d "$D/sysroot/usr/include/pipewire-0.3" ]; then
  echo "PipeWire headers"
  (cd "$D/debs" && apt-get download libpipewire-0.3-dev libspa-0.2-dev)
  for f in "$D"/debs/lib*-dev_*.deb; do dpkg-deb -x "$f" "$D/sysroot"; done
  ln -sf /usr/lib/x86_64-linux-gnu/libpipewire-0.3.so.0 "$D/sysroot/usr/lib/x86_64-linux-gnu/libpipewire-0.3.so"
fi

if [ ! -x "$D/appimagetool" ]; then
  curl -fsSL -o "$D/appimagetool" https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-x86_64.AppImage
  chmod +x "$D/appimagetool"
fi
if [ ! -x "$D/nsis/usr/bin/makensis" ]; then
  (cd "$D/debs" && apt-get download nsis nsis-common)
  for f in "$D"/debs/nsis*.deb; do dpkg-deb -x "$f" "$D/nsis"; done
fi
command -v cargo-deb >/dev/null || cargo install cargo-deb --locked
command -v cargo-zigbuild >/dev/null || cargo install cargo-zigbuild --locked
command -v cargo-xwin >/dev/null || cargo install cargo-xwin --locked
rustup target add x86_64-pc-windows-msvc >/dev/null
echo "done"
