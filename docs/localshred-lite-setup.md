# LocalShred™ Lite Setup

LocalShred Lite delivers the full Solana shred stream to your node, free, from our Amsterdam
fabric. You run a small proxy client; it authenticates with your API key and forwards shreds
to whatever local port you choose.

## 1. Get an API key

Request access from the LocalShred Lite page. Once approved you receive a key that looks
like `78Vpnyd-OPHAADg-hv9eiR`.

The key is shown once. Store it somewhere safe.

## 2. Get the client

```bash
git clone https://github.com/dedicatednodes/shredstream-proxy.git
cd shredstream-proxy
cargo build --release
```

The binary lands at `target/release/localshred-lite-proxy`.

Docker images are published to the GitHub registry as
`ghcr.io/dedicatednodes/shredstream-proxy`. `:latest` follows the most recent tagged
release and `:master` follows the branch. Public, so no login is needed to pull.

## 3. Run it

```bash
export API_KEY=<your key>
export RUST_LOG=info

./target/release/localshred-lite-proxy shredstream \
    --localshred-url http://localshred-lite.nlams.dedicatednodes.io:9999 \
    --dest-ip-ports 127.0.0.1:8001
```

Note the endpoint is **`http://`** and port **`9999`**. There is no HTTPS listener; `https://`
resolves to port 443 and will fail to connect.

Pass the key through the `API_KEY` environment variable rather than `--api-key`. A key given
on the command line is visible in `ps` output to any other user on the machine.

`--dest-ip-ports` is where shreds are delivered. It takes a comma-separated list, so you can
fan out to several local consumers.

## 4. Confirm it is working

Within a second or two you should see heartbeats being accepted:

```
[INFO  localshred_lite_proxy::heartbeat] Sending heartbeat every 5s.
[INFO  solana_metrics::metrics] datapoint: localshred_lite_proxy-connection_metrics
    received=56298i success_forward=56298i fail_forward=0i duplicate=0i
```

`received` climbing and `fail_forward=0` means shreds are arriving and being delivered. At
current mainnet rates expect roughly 4,000–5,000 shreds per second, about 50 Mbit/s.

## Your destination is registered automatically

You do not enter an IP address anywhere. The source address of your own heartbeat connection
is registered the first time it is seen, so there is nothing to declare up front and no
`--public-ip` flag.

Three distinct addresses are allowed per key, IPv4 or IPv6, single addresses only, no CIDR
ranges. If you start a fourth node, it is refused and the message names the three addresses
already registered. Remove one in the portal to free the slot.

Stop the node before removing its address. A node that is still running re-registers on
its next heartbeat, within a few seconds, and takes the slot straight back.

An address that goes quiet for more than seven days gives up its slot automatically, so a node
you retire does not strand you. A node that is merely offline for a few days keeps its place.

## Troubleshooting

| What you see | What it means |
|---|---|
| `Unauthenticated: unknown or disabled api key` | Key is wrong, or was just issued. The client retries after 15s; a fresh key propagates within about a second. |
| `Unauthenticated: missing ... header` | Client and server disagree on the header name. Override with `--api-key-header`. |
| `ResourceExhausted` naming three addresses | Address limit reached. Remove one in the portal. |
| Connection refused / timeout | Check you used `http://` and port `9999`, not `https://`. |
| Heartbeats fine, no shreds | Check `--dest-ip-ports` points somewhere that is actually listening, and that no local firewall blocks it. |

Shreds stop if heartbeats stop. If your client dies, delivery ends within about 15 seconds,
and sooner if your port stops accepting packets.

## What you receive

Raw Solana shreds over UDP, the same wire format a validator's TVU port receives, deduplicated
upstream. The feed is delayed by a fixed 3 ms relative to our paid LocalShred stream.
