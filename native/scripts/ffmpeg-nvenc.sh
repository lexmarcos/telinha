#!/bin/sh
# FFmpeg 9 (Linux or Windows) with an NVENC that runs on not-so-recent NVIDIA drivers.
#
#   sh scripts/ffmpeg-nvenc.sh linux64|win64
#
# FFmpeg requires the driver for the NVENC API version it was built with. BtbN's
# prebuilt builds always use the newest headers (13.1: driver 610 or newer,
# from May 2026), which leaves out anyone on a driver from a few months ago.
# Here FFmpeg is built with the same BtbN system (Docker, LGPL, shared), only
# swapping the headers for the 12.1 series: driver 530 or newer on Linux, 531
# on Windows. The features
# the app uses (low-latency H.264) have existed since well before that.
#
# Output in .deps/ffmpeg-linux or .deps/ffmpeg-win. Needs Docker (~10 GB image).
set -eu
cd "$(dirname "$0")/.."
D="$PWD/.deps"
T="${1:?usage: ffmpeg-nvenc.sh linux64|win64}"
NVENC_SDK=sdk/12.1
IMAGE=ghcr.io/btbn/ffmpeg-builds/$T-lgpl-shared-9.0:latest
OUT=$D/ffmpeg-$([ "$T" = win64 ] && echo win || echo linux)

[ -d "$D/FFmpeg-Builds" ] || git clone -q --depth 1 https://github.com/BtbN/FFmpeg-Builds "$D/FFmpeg-Builds"
docker pull -q "$IMAGE" >/dev/null
docker tag "$IMAGE" btbn-$T-lgpl-shared-9.0:original
mkdir -p "$D/nvenc-antigo"
cat > "$D/nvenc-antigo/Dockerfile" <<DOCKER
FROM btbn-$T-lgpl-shared-9.0:original
RUN rm -rf /opt/ffbuild/include/ffnvcodec /opt/ffbuild/lib/pkgconfig/ffnvcodec.pc \\
 && git clone --depth 1 --branch $NVENC_SDK https://github.com/FFmpeg/nv-codec-headers /tmp/nv \\
 && make -C /tmp/nv PREFIX=/opt/ffbuild install && rm -rf /tmp/nv
DOCKER
# Same name as the original image: BtbN's build.sh uses the local one.
docker build -q -t "$IMAGE" "$D/nvenc-antigo" >/dev/null

# In GitHub Actions, build.sh would name the image after the running repository
# (GITHUB_REPOSITORY) instead of using the one prepared above.
(cd "$D/FFmpeg-Builds" && rm -rf artifacts && env -u GITHUB_REPOSITORY ./build.sh $T lgpl-shared 9.0)
PKG=$(ls "$D"/FFmpeg-Builds/artifacts/*$T-lgpl-shared-9.0.zip "$D"/FFmpeg-Builds/artifacts/*$T-lgpl-shared-9.0.tar.xz 2>/dev/null | head -1)
rm -rf "$OUT" "$OUT.tmp"
mkdir -p "$OUT.tmp"
case "$PKG" in
  *.zip) unzip -q "$PKG" -d "$OUT.tmp" ;;
  *) tar -xJf "$PKG" -C "$OUT.tmp" ;;
esac
mv "$OUT".tmp/* "$OUT" && rmdir "$OUT.tmp"
echo "FFmpeg ready (NVENC $NVENC_SDK) in $OUT"
