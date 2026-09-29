#!/usr/bin/env bash
set -eux

# Some container vars
TAG=${TAG:-${USER}-dev} # READ tag in from env var, defaulting to ${USER}-dev
ORG="ghcr.io/dedicatednodes"

DOCKER_BUILDKIT=1 docker build -t "$ORG/shredstream-proxy:${TAG}" .

docker push "${ORG}/shredstream-proxy:${TAG}"
