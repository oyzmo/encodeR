#!/usr/bin/env bash
# Publish main to the public GitHub mirror. The NAS (remote `nas`) is the main
# repository. GitHub gets a separate, clean history: one snapshot commit per
# deploy, authored with the GitHub noreply address, so the private history
# (old files, e-mail addresses) never leaves the NAS.
# Usage: scripts/deploy-github.sh [--dry-run] [--release]
#   --release  also tag v<version> and create a source-only GitHub release
#              (needs `gh auth login`). No binaries are attached: the Flatpak
#              would bundle patent-encumbered codecs, so users build it themselves.
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
RELEASE=0
for arg in "$@"; do
    case "$arg" in
        --dry-run) DRY=1 ;;
        --release) RELEASE=1 ;;
        *) sed -n '2,9p' "$0" | sed 's/^# \{0,1\}//'; exit 1 ;;
    esac
done

git remote get-url github >/dev/null 2>&1 || { git remote add github "$URL" && echo "added remote github → $URL"; }

if [ -n "$(git status --porcelain --untracked-files=no)" ]; then
    echo "error: uncommitted changes; commit (and push to nas) first" >&2
    exit 1
fi
# Fail fast, and say why, when GitHub can't be reached over SSH, instead of
# hanging for minutes on the connect (port 22 is blocked on some networks).
export GIT_SSH_COMMAND="${GIT_SSH_COMMAND:-ssh -o ConnectTimeout=10}"
gh_ssh="$(ssh -o ConnectTimeout=10 -T git@github.com 2>&1 || true)"
case "$gh_ssh" in
    *"successfully authenticated"*) ;;
    *)  echo "GitHub can't be reached over SSH: ${gh_ssh%%$'\n'*}" >&2
        echo "If port 22 is blocked on this network, use port 443:" >&2
        echo "  https://docs.github.com/en/authentication/troubleshooting-ssh/using-ssh-over-the-https-port" >&2
        exit 1 ;;
esac

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

if [ "$DRY" = 1 ]; then
    echo "(dry run) would publish $(git rev-parse --short "$COMMIT") to $URL $BRANCH:"
    git log -1 --format='%an <%ae>%n%n%B' "$COMMIT" | sed 's/^/    /'
    echo "    $(git ls-tree -r --name-only "$TREE" | wc -l) files"
    if [ "$RELEASE" = 1 ]; then echo "(dry run) would tag $TAG and create a source-only release"; fi
    exit 0
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
