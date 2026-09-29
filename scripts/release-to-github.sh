#!/usr/bin/env bash
# Publish this repository to the public GitHub mirror as ONE squashed commit,
# authored by the DedicatedNodes account.
#
# THE TWO REPOSITORIES ARE NOT MIRRORS OF EACH OTHER, by decision.
#   GitLab  keeps the real history: who changed what, and when.
#   GitHub  carries one commit per release and nothing else.
#
# Danny, 2026-09-17: "Zorg er voor dat de commits op github altijd gesquashed
# zijn met 'Releasing LocalShred(tm) Lite Proxy' en niet onder mijn naam, maar
# onder de DedicatedNodes user."
#
# WHY IT MATTERS BEYOND TIDINESS. The first publish carried three commits, and
# the root one was authored by a contributor's PERSONAL GMAIL ADDRESS. Squashing
# under one identity is what keeps a private address out of a public repository,
# and nobody notices it is there until it is.
#
# The identity is GitHub's noreply form for the account, so the commit attributes
# to it on the web without publishing any real address and without needing a
# verified email anywhere.
#
#   ./scripts/release-to-github.sh            show what would happen
#   ./scripts/release-to-github.sh --push     do it
#
# It force-pushes, because a single orphan commit can never fast-forward over
# the last one.
#
# THE LEASE IS TAKEN BEFORE THE WORK, NOT AT PUSH TIME. Read at push time a lease
# always matches whatever is there, which is --force with extra words: a hotfix
# pushed from elsewhere while this script ran was reproducibly destroyed, with
# the script printing "published" and exiting 0. Reading the head first makes the
# lease mean "the head I showed you at the start", which is a value the operator
# can check before answering.
#
# Set EXPECT_HEAD to the full sha you believe master carries to make it explicit;
# the script then refuses to run against anything else.

set -euo pipefail

REMOTE="${REMOTE:-git@github.com:dedicatednodes/shredstream-proxy.git}"
BRANCH="${BRANCH:-master}"
MESSAGE="${MESSAGE:-Releasing LocalShred™ Lite Proxy}"

AUTHOR_NAME="DedicatedNodes"
AUTHOR_EMAIL="162886540+dedicatednodes@users.noreply.github.com"

die() { printf '\nrelease-to-github: %s\n' "$1" >&2; exit 1; }
say() { printf '%s\n' "$1"; }

PUSH=0
[[ "${1:-}" == "--push" ]] && PUSH=1
[[ -z "${1:-}" || "${1:-}" == "--push" ]] || die "unknown argument '${1}'. Use --push or nothing."

[[ -z "$(git status --porcelain)" ]] || die "the working tree is dirty. What gets published is the tree as it stands, so commit or discard first."

source_ref="$(git rev-parse --abbrev-ref HEAD)"

# Before anything is built, so the value leased below is the one printed here.
leased="$(git ls-remote "$REMOTE" "refs/heads/${BRANCH}" | cut -f1)"
[[ -n "$leased" ]] || die "could not read ${BRANCH} on the remote. Refusing to push blind."
if [[ -n "${EXPECT_HEAD:-}" && "$EXPECT_HEAD" != "$leased" ]]; then
    die "${BRANCH} is ${leased}, but EXPECT_HEAD says ${EXPECT_HEAD}. Somebody else published; look before overwriting."
fi

say "Publishing the tree at ${source_ref} ($(git rev-parse --short HEAD))"
say "  to ${REMOTE} ${BRANCH}"
say "  replacing ${leased:0:8}"
say "  as \"${MESSAGE}\""
say "  by ${AUTHOR_NAME} <${AUTHOR_EMAIL}>"

# A worktree, so the branch in play is never touched and a failure here cannot
# leave somebody's checkout detached.
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"; git worktree prune; git branch -D "published-$$" >/dev/null 2>&1 || true' EXIT
git worktree add --quiet --detach "$tmp" HEAD

(
    cd "$tmp"
    # A unique name per run. The branch namespace is shared with the repository
    # this worktree came from, so a fixed name exists the second time and
    # `checkout --orphan` fails with "a branch named ... already exists" AFTER
    # it has already printed what it was about to publish, which reads like the
    # push failed rather than the branch creation.
    branch="published-$$"
    git checkout -q --orphan "$branch"
    git add -A
    GIT_AUTHOR_NAME="$AUTHOR_NAME" GIT_AUTHOR_EMAIL="$AUTHOR_EMAIL" \
    GIT_COMMITTER_NAME="$AUTHOR_NAME" GIT_COMMITTER_EMAIL="$AUTHOR_EMAIL" \
        git commit -q -m "$MESSAGE"

    new="$(git rev-parse HEAD)"
    say ""
    say "  single commit ${new:0:8}, $(git ls-files | wc -l | tr -d ' ') files, no parents"

    if [[ $PUSH -eq 0 ]]; then
        say ""
        say "Nothing was pushed. Run with --push."
        exit 0
    fi

    # Re-read, and compare against what was leased before the tree was built. A
    # push that arrived during this run aborts here instead of being discarded.
    current="$(git ls-remote "$REMOTE" "refs/heads/${BRANCH}" | cut -f1)"
    [[ -n "$current" ]] || die "could not read ${BRANCH} on the remote. Refusing to push blind."
    [[ "$current" == "$leased" ]] || die "${BRANCH} moved from ${leased} to ${current} while this ran. Refusing to overwrite it."

    git push --force-with-lease="refs/heads/${BRANCH}:${leased}" "$REMOTE" "HEAD:${BRANCH}"

    landed="$(git ls-remote "$REMOTE" "refs/heads/${BRANCH}" | cut -f1)"
    [[ "$landed" == "$new" ]] || die "${BRANCH} is ${landed}, expected ${new}. The push did not land."

    say "  published ${new:0:8}"
)
