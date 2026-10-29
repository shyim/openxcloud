#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
mkdir -p fixtures
ffmpeg -hide_banner -loglevel error -y -f lavfi \
  -i testsrc2=size=1920x1080:rate=60 -frames:v 120 \
  -c:v libx264 -preset veryfast -tune zerolatency -profile:v high -level:v 4.2 \
  -x264-params keyint=60:min-keyint=60:scenecut=0:slices=16:bframes=0:aud=0 \
  -pix_fmt yuv420p -f h264 fixtures/testsrc_1080p60.h264
