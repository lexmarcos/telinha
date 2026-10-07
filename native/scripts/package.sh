#!/bin/sh
# Builds the 4 packages in dist/:
#   telinha_VERSION_amd64.deb         Debian/Ubuntu (glibc 2.35+)
#   Telinha-VERSION-x86_64.AppImage   any Linux with glibc 2.35+
#   Telinha-VERSION-instalador.exe    Windows, per-user install
#   Telinha-VERSION-portatil.zip      Windows, no install
# Usage: scripts/package.sh [linux|windows|tudo]   (run scripts/fetch-deps.sh first)
#
# The default server (site domain) comes from .env.build (not in git):
#   TELINHA_SERVER=your.domain
# Setting TELINHA_SERVER in the environment wins over .env.build. For a public
# release that is not tied to any server (the app asks for an invite link on
# first run):
#   TELINHA_SERVER= scripts/package.sh tudo
set -eu
cd "$(dirname "$0")/.."
ROOT="$PWD"
D="$ROOT/.deps"
WHAT="${1:-tudo}"
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
if [ -z "${TELINHA_SERVER+set}" ] && [ -f .env.build ]; then . ./.env.build; fi
export TELINHA_SERVER="${TELINHA_SERVER:-}"
# Build paths (this machine's folders and user name) stay out of the binaries.
REMAP="--remap-path-prefix=$HOME=~ --remap-path-prefix=$ROOT=."
[ -n "$TELINHA_SERVER" ] || echo "warning: no TELINHA_SERVER; the app will ask for the invite link on first run"
mkdir -p dist

linux() {
  echo "== Linux (glibc 2.35+)"
  # pkg-config only sees the extracted PipeWire files: crates that record the
  # system's library folders (x11-dl) would otherwise bake in a path of this machine.
  PATH="$D/venv/bin:$PATH" \
  BINDGEN_EXTRA_CLANG_ARGS="-I$(ls -d /usr/lib/llvm-*/lib/clang/*/include | tail -1) -I/usr/include/x86_64-linux-gnu -I/usr/include" \
  RUSTFLAGS="-C link-arg=-Wl,--allow-shlib-undefined $REMAP" \
  PKG_CONFIG_LIBDIR="$D/sysroot/usr/lib/x86_64-linux-gnu/pkgconfig" \
    cargo zigbuild --release --target x86_64-unknown-linux-gnu.2.35
  BIN="target/x86_64-unknown-linux-gnu/release/telinha"

  # Bundled libraries: FFmpeg and libva (the rest comes from the system).
  S="dist/stage-linux"
  rm -rf "$S" && mkdir -p "$S/lib"
  for f in "$D"/ffmpeg-linux/lib/lib*.so.*; do
    case "$f" in *.so.[0-9]*.[0-9]*) continue ;; esac   # soname names only (libavcodec.so.63)
    cp -L "$f" "$S/lib/"
  done
  cp -L "$D/libva/lib/libva.so.2" "$D/libva/lib/libva-drm.so.2" "$S/lib/"
  # Each library finds its siblings in its own folder (FFmpeg loads
  # swresample and libva by itself), and the binary uses RPATH, which applies
  # to the whole chain.
  for f in "$S"/lib/*.so*; do "$D/venv/bin/patchelf" --set-rpath '$ORIGIN' "$f"; done
  "$D/venv/bin/patchelf" --force-rpath --set-rpath '$ORIGIN/lib:$ORIGIN/../lib/telinha' "$BIN"

  # .deb (cargo-deb reads the assets from Cargo.toml)
  cargo deb --no-build --no-strip --target x86_64-unknown-linux-gnu -o "dist/telinha_${VERSION}_amd64.deb"

  # AppImage
  A="dist/Telinha.AppDir"
  rm -rf "$A" && mkdir -p "$A/usr/bin" "$A/usr/lib/telinha"
  cp "$BIN" "$A/usr/bin/telinha"
  cp "$S"/lib/* "$A/usr/lib/telinha/"
  cp assets/linux/AppRun "$A/AppRun"
  cp assets/linux/telinha.desktop "$A/telinha.desktop"
  cp assets/icons/telinha.png "$A/telinha.png"
  ARCH=x86_64 "$D/appimagetool" --appimage-extract-and-run --no-appstream "$A" "dist/Telinha-${VERSION}-x86_64.AppImage" >/dev/null
  rm -rf "$A"
}

windows() {
  echo "== Windows"
  XB="$HOME/.cache/telinha-xwin/bin"
  mkdir -p "$XB"
  ln -sf "$(rustc --print sysroot)/lib/rustlib/x86_64-unknown-linux-gnu/bin/rust-lld" "$XB/lld-link"
  ln -sf "$(command -v clang)" "$XB/clang-cl"
  # RUSTFLAGS replaces .cargo/config.toml's target flags, so crt-static is repeated here.
  PATH="$XB:$PATH" FFMPEG_DIR="$D/ffmpeg-win" XWIN_ACCEPT_LICENSE=1 \
  RUSTFLAGS="-C target-feature=+crt-static $REMAP" \
    cargo xwin build --release --target x86_64-pc-windows-msvc
  S="dist/stage-windows"
  rm -rf "$S" && mkdir -p "$S"
  cp target/x86_64-pc-windows-msvc/release/telinha.exe "$S/"
  cp "$D"/ffmpeg-win/bin/*.dll "$S/"

  # Portable: just unzip and open.
  cat > "$S/LEIA-ME.txt" <<TXT
Telinha $VERSION (portátil)

Abra telinha.exe. Nada é instalado: as preferências ficam em
%APPDATA%\\Telinha\\telinha e o registro de erros em
%LOCALAPPDATA%\\Telinha\\telinha\\data\\telinha.log.
TXT
  (cd "$S" && rm -f "$ROOT/dist/Telinha-${VERSION}-portatil.zip" && zip -qr "$ROOT/dist/Telinha-${VERSION}-portatil.zip" .)

  # Installer
  NSISDIR="$D/nsis/usr/share/nsis" "$D/nsis/usr/bin/makensis" -V2 \
    -DVERSION="$VERSION" -DSTAGE="$ROOT/$S" -DOUTFILE="$ROOT/dist/Telinha-${VERSION}-instalador.exe" \
    assets/windows/installer.nsi
}

case "$WHAT" in
  linux) linux ;;
  windows) windows ;;
  tudo) linux; windows ;;
  *) echo "usage: $0 [linux|windows|tudo]"; exit 1 ;;
esac
ls -la dist/*.deb dist/*.AppImage dist/*.exe dist/*.zip 2>/dev/null
