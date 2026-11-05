#!/usr/bin/env bash
# Publish main to the public GitHub mirror. The NAS (remote `nas`) is the main
# repository. GitHub gets a separate, clean history: one snapshot commit per
# deploy, authored with the GitHub noreply address, so the private history
# (old files, e-mail addresses) never leaves the NAS.
# Usage: scripts/deploy-github.sh [--dry-run | --confirm] [--release]
#   --release  also tag v<version> and create a source-only GitHub release
#              (needs `gh auth login`). No binaries are attached: the Flatpak
#              would bundle patent-encumbered codecs, so users build it themselves.
#   --confirm  show the dry-run summary, ask, then publish (checks run once).
set -euo pipefail
cd "$(dirname "$0")/.."
URL=git@github.com:oyzmo/encodeR.git
REPO=oyzmo/encodeR
BRANCH=main
AUTHOR="oyzmo"
EMAIL="74640760+oyzmo@users.noreply.github.com"
# Never published, even if they get tracked again by mistake.
EXCLUDE=(CLAUDE.md .claude dev testvideo)
# Abort if any of these (one extended regex per line, kept in the private dev/
# folder so the list itself isn't published) turn up in the published files.
LEAKS_FILE=dev/private-patterns

VERSION=$(sed -n 's/^version = "\([0-9.]*\)"/\1/p' Cargo.toml | head -1)
TAG=v$VERSION
DRY=0
CONFIRM=0
RELEASE=0
for arg in "$@"; do
    case "$arg" in
        --dry-run) DRY=1 ;;
        --confirm) CONFIRM=1 ;;
        --release) RELEASE=1 ;;
        *) sed -n '2,10p' "$0" | sed 's/^# \{0,1\}//'; exit 1 ;;
    esac
done
if [ "$DRY" = 1 ] && [ "$CONFIRM" = 1 ]; then
    echo "error: --dry-run and --confirm can't be combined" >&2
    exit 1
fi

git remote get-url github >/dev/null 2>&1 || { git remote add github "$URL" && echo "added remote github → $URL"; }

if [ -n "$(git status --porcelain --untracked-files=no)" ]; then
    echo "error: uncommitted changes; commit (and push to nas) first" >&2
    exit 1
fi
# Fail fast, and say why, when GitHub can't be reached, instead of hanging for
# minutes (port 22 is blocked on some networks). The check is `git ls-remote
# github`, so it goes through the same remote URL, GIT_SSH_COMMAND or
# core.sshCommand (e.g. a port-443 setup) and shell quoting as the push.
# Without either, ssh gets timeouts: ConnectTimeout bounds the connect and
# ServerAlive a stall after it.
SSH_CMD=${GIT_SSH_COMMAND:-$(git config core.sshCommand || echo "ssh -o ConnectTimeout=10 -o ServerAliveInterval=5 -o ServerAliveCountMax=2")}
export GIT_SSH_COMMAND=$SSH_CMD
# For the check only: BatchMode never prompts (so it can't eat keystrokes),
# accept-new trusts GitHub's host key on first contact (a changed key still
# fails), and `timeout` bounds the whole thing.
check_rc=0
gh_err="$(GIT_SSH_COMMAND="$SSH_CMD -o BatchMode=yes -o StrictHostKeyChecking=accept-new" GIT_TERMINAL_PROMPT=0 \
    timeout 20 git ls-remote -q github HEAD </dev/null 2>&1 >/dev/null)" || check_rc=$?
if [ "$check_rc" != 0 ]; then
    echo "GitHub can't be reached ($(git remote get-url github)):" >&2
    if [ "$check_rc" = 124 ]; then
        echo "  no answer within 20 seconds" >&2
    else
        grep -v '^Warning: Permanently added' <<<"${gh_err:-(no output; exit status $check_rc)}" | sed 's/^/  /' >&2 || true
    fi
    case "$gh_err" in
        *"Host key verification failed"*)
            echo "GitHub's host key doesn't match the one in ~/.ssh/known_hosts; check it:" >&2
            echo "  https://docs.github.com/en/authentication/keeping-your-account-and-data-secure/githubs-ssh-key-fingerprints" >&2 ;;
        *"Permission denied"*)
            echo "If the key has a passphrase, load it first: ssh-add" >&2 ;;
        *)  echo "If port 22 is blocked on this network, use port 443:" >&2
            echo "  https://docs.github.com/en/authentication/troubleshooting-ssh/using-ssh-over-the-https-port" >&2 ;;
    esac
    exit 1
fi

git fetch -q nas "$BRANCH"
if [ "$(git rev-parse "$BRANCH")" != "$(git rev-parse "nas/$BRANCH")" ]; then
    echo "error: $BRANCH differs from nas/$BRANCH; run: git push nas $BRANCH" >&2
    exit 1
fi

if [ "$RELEASE" = 1 ]; then
    command -v gh >/dev/null || { echo "error: gh is missing: sudo dnf install gh" >&2; exit 1; }
    gh auth status >/dev/null 2>&1 || { echo "error: gh isn't logged in; run: gh auth login" >&2; exit 1; }
    if gh release view "$TAG" --repo "$REPO" >/dev/null 2>&1; then
        echo "error: release $TAG already exists; bump the version first (scripts/bump.sh)" >&2
        exit 1
    fi
fi

# The public tree: main's tree minus the excluded paths.
INDEX=$(mktemp)
trap 'rm -f "$INDEX" "$INDEX.msg"' EXIT
export GIT_INDEX_FILE=$INDEX
git read-tree "$BRANCH"
git rm -r -q --cached --ignore-unmatch -- "${EXCLUDE[@]}"
TREE=$(git write-tree)
unset GIT_INDEX_FILE

if [ -s "$LEAKS_FILE" ]; then
    if git grep -I -q -i -E -f "$LEAKS_FILE" "$TREE"; then
        echo "error: private strings in the files to publish:" >&2
        git grep -I -n -i -E -f "$LEAKS_FILE" "$TREE" >&2
        exit 1
    fi
else
    echo "warning: no $LEAKS_FILE, so the private-string check was skipped" >&2
fi

# Build on top of what GitHub already has (if anything).
PARENT=
SINCE=
if git fetch -q github "$BRANCH" 2>/dev/null; then
    PARENT=$(git rev-parse FETCH_HEAD)
    SINCE=$(git log -1 --format=%B "$PARENT" | sed -n 's/^Source: //p')
fi

if [ -n "$PARENT" ] && [ "$(git rev-parse "$PARENT^{tree}")" = "$TREE" ]; then
    COMMIT=$PARENT
    echo "GitHub already has this tree"
else
    {
        echo "encodeR $VERSION"
        echo
        if [ -n "$SINCE" ] && git merge-base --is-ancestor "$SINCE" "$BRANCH" 2>/dev/null; then
            git log --reverse --format='- %s' "$SINCE..$BRANCH"
        else
            echo "- Initial public release"
        fi
        echo
        echo "Source: $(git rev-parse --short "$BRANCH")"
    } >"$INDEX.msg"
    COMMIT=$(GIT_AUTHOR_NAME=$AUTHOR GIT_AUTHOR_EMAIL=$EMAIL GIT_COMMITTER_NAME=$AUTHOR GIT_COMMITTER_EMAIL=$EMAIL \
        git commit-tree "$TREE" ${PARENT:+-p "$PARENT"} -F "$INDEX.msg")
fi

if [ "$COMMIT" = "$PARENT" ] && [ "$RELEASE" = 0 ]; then
    echo "Nothing to publish."
    exit 0
fi
if [ "$DRY" = 1 ] || [ "$CONFIRM" = 1 ]; then
    # Only a real dry run says so; under --confirm, "y" publishes for real.
    if [ "$DRY" = 1 ]; then will="(dry run) would"; else will="Will"; fi
    if [ "$COMMIT" != "$PARENT" ]; then
        echo "$will publish $(git rev-parse --short "$COMMIT") to $URL $BRANCH:"
        git log -1 --format='%an <%ae>%n%n%B' "$COMMIT" | sed 's/^/    /'
        echo "    $(git ls-tree -r --name-only "$TREE" | wc -l) files"
    fi
    if [ "$RELEASE" = 1 ]; then echo "$will tag $TAG and create a source-only release"; fi
fi
[ "$DRY" = 1 ] && exit 0
if [ "$CONFIRM" = 1 ]; then
    # Ctrl+D always cancels, even after typing "y".
    read -rp "Publish this to GitHub? [y/N] " answer || { echo; answer=; }
    [[ "$answer" == [yY]* ]] || { echo "Cancelled."; exit 0; }
fi

[ "$COMMIT" = "$PARENT" ] || git push github "$COMMIT:refs/heads/$BRANCH"

if [ "$RELEASE" = 1 ]; then
    git push github "$COMMIT:refs/tags/$TAG"
    gh release create "$TAG" --repo "$REPO" --verify-tag --title "encodeR $VERSION" \
        --notes "$(git log -1 --format=%b "$COMMIT" | sed '/^Source: /d')

Build and install: \`scripts/install.sh\` (see the README; no prebuilt binaries)."
    echo "released $TAG"
fi
echo "published $(git rev-parse --short "$COMMIT") to $URL"
