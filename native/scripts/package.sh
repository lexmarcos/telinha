#!/bin/sh
# Monta os 4 pacotes em dist/:
#   telinha_VERSÃO_amd64.deb          Debian/Ubuntu (glibc 2.35+)
#   Telinha-VERSÃO-x86_64.AppImage    qualquer Linux com glibc 2.35+
#   Telinha-VERSÃO-instalador.exe     Windows, instala para o usuário
#   Telinha-VERSÃO-portatil.zip       Windows, sem instalar
# Uso: scripts/package.sh [linux|windows|tudo]   (rode scripts/fetch-deps.sh antes)
#
# O servidor padrão (domínio do site) vem de .env.build (fora do git):
#   TELINHA_SERVER=seu.dominio
#   TELINHA_DISCORD_ID=id do aplicativo no Discord Developer Portal (Rich Presence)
set -eu
cd "$(dirname "$0")/.."
ROOT="$PWD"
D="$ROOT/.deps"
WHAT="${1:-tudo}"
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
[ -f .env.build ] && . ./.env.build
export TELINHA_SERVER="${TELINHA_SERVER:-}"
export TELINHA_DISCORD_ID="${TELINHA_DISCORD_ID:-}"
[ -n "$TELINHA_SERVER" ] || echo "aviso: sem TELINHA_SERVER; o app vai pedir o link de convite na primeira vez"
mkdir -p dist

linux() {
  echo "== Linux (glibc 2.35+)"
  PATH="$D/venv/bin:$PATH" \
  BINDGEN_EXTRA_CLANG_ARGS="-I$(ls -d /usr/lib/llvm-*/lib/clang/*/include | tail -1) -I/usr/include/x86_64-linux-gnu -I/usr/include" \
  RUSTFLAGS="-C link-arg=-Wl,--allow-shlib-undefined" \
    cargo zigbuild --release --target x86_64-unknown-linux-gnu.2.35
  BIN="target/x86_64-unknown-linux-gnu/release/telinha"

  # Bibliotecas que vão junto: FFmpeg e libva (o resto vem do sistema).
  S="dist/stage-linux"
  rm -rf "$S" && mkdir -p "$S/lib"
  for f in "$D"/ffmpeg-linux/lib/lib*.so.*; do
    case "$f" in *.so.[0-9]*.[0-9]*) continue ;; esac   # só os nomes de soname (libavcodec.so.63)
    cp -L "$f" "$S/lib/"
  done
  cp -L "$D/libva/lib/libva.so.2" "$D/libva/lib/libva-drm.so.2" "$S/lib/"
  # Cada biblioteca acha as irmãs na própria pasta (o FFmpeg carrega
  # swresample e libva por conta própria), e o binário usa RPATH, que vale
  # para a cadeia toda.
  for f in "$S"/lib/*.so*; do "$D/venv/bin/patchelf" --set-rpath '$ORIGIN' "$f"; done
  "$D/venv/bin/patchelf" --force-rpath --set-rpath '$ORIGIN/lib:$ORIGIN/../lib/telinha' "$BIN"

  # .deb (cargo-deb lê os assets do Cargo.toml)
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
  PATH="$XB:$PATH" FFMPEG_DIR="$D/ffmpeg-win" XWIN_ACCEPT_LICENSE=1 \
    cargo xwin build --release --target x86_64-pc-windows-msvc
  S="dist/stage-windows"
  rm -rf "$S" && mkdir -p "$S"
  cp target/x86_64-pc-windows-msvc/release/telinha.exe "$S/"
  cp "$D"/ffmpeg-win/bin/*.dll "$S/"

  # Portátil: é só descompactar e abrir.
  cat > "$S/LEIA-ME.txt" <<TXT
Telinha $VERSION (portátil)

Abra telinha.exe. Nada é instalado: as preferências ficam em
%APPDATA%\\Telinha\\telinha e o registro de erros em
%LOCALAPPDATA%\\Telinha\\telinha\\data\\telinha.log.
TXT
  (cd "$S" && rm -f "$ROOT/dist/Telinha-${VERSION}-portatil.zip" && zip -qr "$ROOT/dist/Telinha-${VERSION}-portatil.zip" .)

  # Instalador
  NSISDIR="$D/nsis/usr/share/nsis" "$D/nsis/usr/bin/makensis" -V2 \
    -DVERSION="$VERSION" -DSTAGE="$ROOT/$S" -DOUTFILE="$ROOT/dist/Telinha-${VERSION}-instalador.exe" \
    assets/windows/installer.nsi
}

case "$WHAT" in
  linux) linux ;;
  windows) windows ;;
  tudo) linux; windows ;;
  *) echo "uso: $0 [linux|windows|tudo]"; exit 1 ;;
esac
ls -la dist/*.deb dist/*.AppImage dist/*.exe dist/*.zip 2>/dev/null
