# LocalShred™ Lite Setup

LocalShred Lite delivers the full Solana shred stream to your node, free, from our Amsterdam
fabric. You run a small proxy client; it authenticates with your API key and forwards shreds
to whatever local port you choose.

## 1. Get an API key

Sign up on the LocalShred Lite page. Your key appears on your dashboard in the portal once your
account is activated.

You can show the key again in the portal as often as you need, and showing it changes nothing.
Replacing the key is a separate action. It takes effect immediately and cannot be undone: the old
key stops working at once, every node still using it stops receiving shreds, and the addresses
registered under the old key are cleared. Put the new key into your client and restart it, and
your addresses register again on the next connection. If you have lost your key, show it again;
replace it only if you believe somebody else has it.

DedicatedNodes never sends your key by email and never asks you for it.

## 2. Get the client

The binary is called `localshred-lite-proxy`. There are three ways to get it.

A container image, published to the GitHub registry as `ghcr.io/dedicatednodes/shredstream-proxy`
and to Docker Hub as `dedicatednodes/shredstream-proxy`. `:latest` follows the most recent tagged
release and `:master` follows the branch. Both are public, so no login is needed to pull. The image
is built for amd64 only; on arm64, build from source.

A prebuilt Linux x86_64 binary, attached to each release:

```bash
curl -fLO https://github.com/dedicatednodes/shredstream-proxy/releases/latest/download/localshred-lite-proxy-x86_64-unknown-linux-gnu
chmod +x localshred-lite-proxy-x86_64-unknown-linux-gnu
./localshred-lite-proxy-x86_64-unknown-linux-gnu --version
```

From source:

```bash
git clone https://github.com/dedicatednodes/shredstream-proxy.git
cd shredstream-proxy
./scripts/fetch-solana-ledger.sh
cargo build --release
```

The binary lands at `target/release/localshred-lite-proxy`. The fetch script downloads
`solana-ledger` 4.3.0 from crates.io into `vendor/`, checks its sha256 and applies
`patches/solana-ledger-recover.patch`, which makes shred recovery public. Rerunning it is a
no-op.

## 3. Run it

```bash
export API_KEY=your_key_here

./target/release/localshred-lite-proxy shredstream \
    --localshred-url http://localshred-lite.nlams.dedicatednodes.io:9999 \
    --dest-ip-ports 127.0.0.1:8001
```

The same with the container image, which takes the key from your shell's environment so it never
appears on the command line:

```bash
export API_KEY=your_key_here

docker run -d --name localshred-lite-proxy --network host \
    -e API_KEY \
    -e LOCALSHRED_URL=http://localshred-lite.nlams.dedicatednodes.io:9999 \
    -e DEST_IP_PORTS=127.0.0.1:8001 \
    ghcr.io/dedicatednodes/shredstream-proxy:latest shredstream
```

The endpoint is plain `http://` on port `9999`. There is no HTTPS listener, so a URL starting
with `https://`, or one without the port, will not connect.

Pass the key through the `API_KEY` environment variable rather than `--api-key`. A key given
on the command line is visible in `ps` output to any other user on the machine and is kept in
your shell history.

`--dest-ip-ports` is where shreds are delivered, and it is mandatory. `127.0.0.1:8001` stands for
a validator on the same machine; use whatever local port your consumer listens on. It takes a
comma-separated list, so you can fan out to several local consumers.

### Coming from Jito ShredStream

Use the whole command above rather than editing your Jito command. This client has no
`--block-engine-url`, `--auth-keypair`, `--desired-regions` or `--public-ip`, and it refuses an
unknown flag before it starts, so a Jito command with one line changed exits with
`error: unexpected argument`. The Jito binary itself cannot be pointed at LocalShred Lite.

## 4. Confirm it is working

The client logs at `info` level by default. Shortly after it starts you should see a heartbeat
line, then a stats line like this one:

```
INFO localshred_lite_proxy::forwarder: interval stats received=64216 success_forward=64216 fail_forward=0 duplicate=0
```

The stats line appears every 15 seconds and counts that interval. `received` climbing and
`fail_forward=0` means shreds are arriving and being delivered. At current mainnet rates expect
roughly 4,000 to 5,000 shreds per second, about 50 Mbit/s. With `--grpc-service-port` the same
line also carries `recovered`, `entries`, `txns` and the decode error counts.

Metric datapoints are not printed. `RUST_LOG=info,solana_metrics=info` brings them back.

## Your destination is registered automatically

You do not enter an IP address anywhere in the client. The address your client connects from is
registered the first time it is seen, so there is nothing to declare up front.

Three addresses are allowed per key. Each is a single public IPv4 address: no CIDR ranges, and
no IPv6 anywhere in LocalShred Lite. If you start a fourth node, it is refused and the message
names the three addresses already registered. Remove one in the portal to free the slot, and the
next connection from the new address registers.

Stop a node before removing its address. A node that is still running registers again the next
time it connects and takes the slot back.

## Troubleshooting

| What you see | What it means |
|---|---|
| `Server refused this destination: unknown or disabled api key` | The key is wrong, was copied short or with whitespace, or was replaced. Show the key again in the portal and paste it fresh. Everything else in your command is correct, because this message only appears after the client connected. |
| `Server refused this destination` naming three addresses | Address limit reached. Remove one in the portal. |
| `Failed to connect to block engine, retrying` | Check you used `http://` and port `9999`, not `https://` or no port. |
| `No destinations found` | `--dest-ip-ports` is missing. |
| `error: unexpected argument` | A Jito flag was carried over. Use the whole command from step 3. |
| `Error: Shutdown` | This is the Jito binary, not this client. |
| Stats fine, no shreds at your consumer | Check `--dest-ip-ports` points somewhere that is actually listening, and that no local firewall blocks it. |

Delivery stops when your client stops, and starts again when it reconnects.

## What you receive

Raw Solana shreds over UDP, the same wire format a validator's TVU port receives, deduplicated
upstream, every shred of every slot. LocalShred, included with DedicatedNodes bare metal, has a
3ms latency advantage over LocalShred Lite, measured in Amsterdam.
