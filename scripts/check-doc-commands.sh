#!/usr/bin/env bash
#
# Guards the commands this repository publishes.
#
# A placeholder written in angle brackets is not inert in a shell: `-e
# API_KEY=<API_KEY>` parses as a redirection, so pasting the published block
# fails on a missing file AND silently swallows the `-e` of the line after it.
# That shipped in the README and in the setup document, and it is invisible to
# review because the text looks like every other placeholder convention.
#
# `bash -n` cannot catch it, because the redirection is valid syntax. The check
# is therefore on the characters themselves, inside fenced bash blocks only.

set -euo pipefail

cd "$(dirname "$0")/.."

status=0

for doc in README.md docs/*.md; do
    [ -e "$doc" ] || continue

    # Fenced bash blocks only. Prose may say <your key> without breaking anybody.
    blocks=$(awk '/^```bash$/{inblock=1; next} /^```$/{inblock=0; next} inblock{print FILENAME":"FNR":"$0}' "$doc")

    offenders=$(printf '%s\n' "$blocks" | grep -E '<[A-Za-z_][A-Za-z_ -]*>' || true)
    if [ -n "$offenders" ]; then
        echo "Angle-bracket placeholder inside a runnable block; it parses as a shell redirection:"
        printf '%s\n' "$offenders"
        status=1
    fi

    # Syntax check the block as a whole, which catches an unbalanced quote or a
    # dangling line continuation introduced by an edit.
    body=$(awk '/^```bash$/{inblock=1; next} /^```$/{inblock=0; next} inblock{print}' "$doc")
    if [ -n "$body" ] && ! printf '%s\n' "$body" | bash -n 2>/tmp/doc-syntax.$$; then
        echo "Shell syntax error in a fenced block in $doc:"
        cat /tmp/doc-syntax.$$
        rm -f /tmp/doc-syntax.$$
        status=1
    fi
    rm -f /tmp/doc-syntax.$$
done

if [ "$status" -eq 0 ]; then
    echo "Published commands parse as shell and carry no angle-bracket placeholders."
fi

exit "$status"
