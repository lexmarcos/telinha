#!/bin/sh
# Windows FFmpeg 9 with an NVENC that runs on not-so-recent NVIDIA drivers.
#
# FFmpeg requires the driver for the NVENC API version it was built with. BtbN's
# prebuilt builds always use the newest headers (13.1: driver 610 or newer,
# from May 2026), which leaves out anyone on a driver from a few months ago.
# Here FFmpeg is built with the same BtbN system (Docker, LGPL, shared), only
# swapping the headers for the 12.1 series: driver 531 or newer. The features
# the app uses (low-latency H.264) have existed since well before that.
#
# Output in .deps/ffmpeg-win. Needs Docker (~10 GB image).
set -eu
cd "$(dirname "$0")/.."
D="$PWD/.deps"
NVENC_SDK=sdk/12.1
IMAGE=ghcr.io/btbn/ffmpeg-builds/win64-lgpl-shared-9.0:latest

[ -d "$D/FFmpeg-Builds" ] || git clone -q --depth 1 https://github.com/BtbN/FFmpeg-Builds "$D/FFmpeg-Builds"
docker pull -q "$IMAGE" >/dev/null
docker tag "$IMAGE" btbn-win64-lgpl-shared-9.0:original
mkdir -p "$D/nvenc-antigo"
cat > "$D/nvenc-antigo/Dockerfile" <<DOCKER
FROM btbn-win64-lgpl-shared-9.0:original
RUN rm -rf /opt/ffbuild/include/ffnvcodec /opt/ffbuild/lib/pkgconfig/ffnvcodec.pc \\
 && git clone --depth 1 --branch $NVENC_SDK https://github.com/FFmpeg/nv-codec-headers /tmp/nv \\
 && make -C /tmp/nv PREFIX=/opt/ffbuild install && rm -rf /tmp/nv
DOCKER
# Same name as the original image: BtbN's build.sh uses the local one.
docker build -q -t "$IMAGE" "$D/nvenc-antigo" >/dev/null

# In GitHub Actions, build.sh would name the image after the running repository
# (GITHUB_REPOSITORY) instead of using the one prepared above.
(cd "$D/FFmpeg-Builds" && rm -rf artifacts && env -u GITHUB_REPOSITORY ./build.sh win64 lgpl-shared 9.0)
ZIP=$(ls "$D"/FFmpeg-Builds/artifacts/*win64-lgpl-shared-9.0.zip)
rm -rf "$D/ffmpeg-win" "$D/ffmpeg-win.tmp"
mkdir -p "$D/ffmpeg-win.tmp"
unzip -q "$ZIP" -d "$D/ffmpeg-win.tmp"
mv "$D"/ffmpeg-win.tmp/* "$D/ffmpeg-win" && rmdir "$D/ffmpeg-win.tmp"
echo "Windows FFmpeg ready (NVENC $NVENC_SDK) in $D/ffmpeg-win"
