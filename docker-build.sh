#!/usr/bin/env bash
set -eux

# Some container vars
TAG=$(git describe --match=NeVeRmAtCh --always --abbrev=8 --dirty)
ORG="ghcr.io/dedicatednodes"

DOCKER_BUILDKIT=1 docker build -t "$ORG/shredstream-proxy:${TAG}" .

# Build only, which is what the name promises. This used to end with a bare
# `docker run`, and the binary requires a subcommand, so under `set -eux` the
# script failed on its last line every single time it was used. bash -n passes
# on it, which is why a broken happy path went unnoticed.
#
# To try the image, pass the subcommand and the three variables it needs:
#   docker run --rm --network host \
#       -e LOCALSHRED_URL=http://localshred-lite.nlams.dedicatednodes.io:9999 \
#       -e API_KEY=<key> -e DEST_IP_PORTS=127.0.0.1:8001 \
#       "$ORG/shredstream-proxy:${TAG}" shredstream
echo "Built $ORG/shredstream-proxy:${TAG}"
