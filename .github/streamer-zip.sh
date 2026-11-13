#!/usr/bin/env bash
# Packs Aura Streamer for Linux or macOS: one folder holding the binary, its
# licence texts and a README, zipped with the binary's execute bit kept.
# The streamer's own updater takes the binary out of this zip by name.
#
#   streamer-zip.sh <binary> <out.zip> <folder name> <linux|macos>
set -euo pipefail
bin=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
out=$2
name=$3
os=$4
root=${GITHUB_WORKSPACE:-$(cd "$(dirname "$0")/.." && pwd)}
case "$out" in /*) ;; *) out="$PWD/$out" ;; esac

stage=$(mktemp -d)
mkdir -p "$stage/$name"
install -m 755 "$bin" "$stage/$name/aura-streamer"
cp "$root/LICENSE" "$stage/$name/LICENSE.txt"
cp "$root/THIRD-PARTY-NOTICES.txt" "$stage/$name/THIRD-PARTY-NOTICES.txt"
if [ "$os" = macos ]; then filters='~/Library/Application Support/AuraEngine/filters'
else filters='~/.local/share/AuraEngine/filters'; fi
tag=${name#aura-streamer-}; tag=${tag%%-*}
cat > "$stage/$name/README.txt" <<EOF
Aura Streamer $tag - the AuraEngine with no window, run from a browser

Run ./aura-streamer on the computer next to the DAC, then open the address
it prints from a phone, a tablet or another computer on the same network.
FIR filters: the ones the AuraEngine app downloaded ($filters)
are found on their own. The page offers a new version when there is one.

https://aura-engine.app
EOF
rm -f "$out"
(cd "$stage" && zip -q -r -X "$out" "$name")
rm -rf "$stage"
unzip -l "$out"
