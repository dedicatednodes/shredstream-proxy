#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."

version=4.3.0
sha256=af91feedf850de2afd5d543db97c4cfc76711fd77895993b0d317ba474894a5f
dest=vendor/solana-ledger
stamp="$dest/.patched-$version-$(sha256sum patches/solana-ledger-recover.patch | cut -c1-16)"

[ -e "$stamp" ] && exit 0

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

curl -fsSL "https://static.crates.io/crates/solana-ledger/solana-ledger-$version.crate" -o "$tmp/solana-ledger.crate"
echo "$sha256  $tmp/solana-ledger.crate" | sha256sum -c --quiet -
tar -xzf "$tmp/solana-ledger.crate" -C "$tmp"
patch -s -d "$tmp/solana-ledger-$version" -p1 < patches/solana-ledger-recover.patch

rm -rf "$dest"
mkdir -p vendor
mv "$tmp/solana-ledger-$version" "$dest"
touch "$stamp"
