# LocalShred™ Lite Proxy

Client for receiving Solana shreds from LocalShred Lite. It connects to a LocalShred Lite
endpoint, keeps the subscription alive with periodic heartbeats, and forwards the shreds it
receives to one or more local destinations over UDP.

Forked from [jito-labs/shredstream-proxy](https://github.com/jito-labs/shredstream-proxy).
Forwarding, multicast, endpoint discovery, metrics and the gRPC entry service behave as
upstream. Authentication and destination registration differ.

## Signup
Sign up for the free shred stream at
[www.dedicatednodes.io/jito-shredstream-alternative](https://www.dedicatednodes.io/jito-shredstream-alternative/).

## Authentication

Authentication is by API key rather than a keypair. Pass it with `--api-key` (or the
`API_KEY` environment variable); it is sent on every request in the `x-localshred-auth`
header. Override the header name with `--api-key-header` / `API_KEY_HEADER` if your
provider expects a different one.

Prefer the environment variable over the flag. A key passed on the command line is visible
in `ps` output to any local user.

## Destinations are registered automatically

You do not declare a destination address anywhere. The source address of your own heartbeat
connection is registered on sight, so there is no `--public-ip` flag and nothing is fetched
from `ifconfig.me`.

Three distinct addresses are allowed per key. A fourth is refused with a message naming the
three already held; remove one in the LocalShred Lite portal to free a slot.

## Usage

```bash
RUST_LOG=info localshred-lite-proxy shredstream \
    --localshred-url http://localshred-lite.nlams.dedicatednodes.io:9999 \
    --api-key YOUR_API_KEY \
    --dest-ip-ports 127.0.0.1:8001,10.0.0.1:8001
```

Docker:

```bash
docker run -d \
    --name localshred-lite-proxy \
    --network host \
    -e RUST_LOG=info \
    -e LOCALSHRED_URL=http://localshred-lite.nlams.dedicatednodes.io:9999 \
    -e API_KEY=YOUR_API_KEY \
    -e DEST_IP_PORTS=127.0.0.1:8001,10.0.0.1:8001 \
    ghcr.io/dedicatednodes/shredstream-proxy:latest shredstream
```
