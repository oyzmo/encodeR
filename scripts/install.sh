#!/usr/bin/env bash
# Install encodeR. Run without arguments for a menu, or pass one of:
#   --native           build and install for this user (~/.local)
#   --uninstall        remove the native install and the user Flatpak
#   --make-flatpak     build flatpak/no.oyzmo.EncodeR-<version>.flatpak (does not install)
#   --install-flatpak  install that bundle for this user (makes it first if needed)
set -euo pipefail
SELF="$(realpath "$0")"
cd "$(dirname "$0")/.."
APP_ID=no.oyzmo.EncodeR
OLD_IDS=(io.github.oyzmo.Reencoder no.oyzmo.Reencoder)   # IDs from before the renames
OLD_BINS=(reencoder)
BIN="$HOME/.local/bin"
SHARE="$HOME/.local/share"
MANIFEST=build-aux/flatpak/$APP_ID.yml
FP_DIR=flatpak
VERSION=$(sed -n 's/^version = "\([0-9.]*\)"/\1/p' Cargo.toml | head -1)
BUNDLE="$FP_DIR/$APP_ID-$VERSION.flatpak"

remove_old_ids() {
    for old in "${OLD_IDS[@]}"; do
        for base in "$SHARE" /usr/local/share; do
            for f in "$base/applications/$old.desktop" "$base/icons/hicolor/scalable/apps/$old.svg" "$base/metainfo/$old.metainfo.xml"; do
                if [ -e "$f" ]; then
                    if [ -w "$(dirname "$f")" ]; then rm -f "$f" && echo "removed stale $f"
                    else echo "warning: stale $f needs: sudo rm '$f'"; fi
                fi
            done
        done
    done
    for b in "${OLD_BINS[@]}"; do
        if [ -e "$BIN/$b" ]; then rm -f "$BIN/$b" && echo "removed stale $BIN/$b"; fi
    done
    if command -v flatpak >/dev/null; then
        for old in "${OLD_IDS[@]}"; do
            if flatpak info --user "$old" >/dev/null 2>&1; then
                flatpak uninstall --user -y "$old" && echo "removed old Flatpak $old"
            fi
        done
    fi
}

# Carry the old Flatpak's settings over to the new ID (its sandbox can't see them).
migrate_flatpak_config() {
    local new="$HOME/.var/app/$APP_ID/config/encoder"
    [ -e "$new/config.json" ] && return 0
    for old in "${OLD_IDS[@]}"; do
        local f="$HOME/.var/app/$old/config/reencoder/config.json"
        if [ -f "$f" ]; then
            install -Dm644 "$f" "$new/config.json" && echo "copied settings from $old"
            return 0
        fi
    done
}

refresh() {
    update-desktop-database "$SHARE/applications" 2>/dev/null || true
    gtk-update-icon-cache -q -t "$SHARE/icons/hicolor" 2>/dev/null || true
}

flatpak_installed() {
    command -v flatpak >/dev/null && flatpak info --user "$APP_ID" >/dev/null 2>&1
}

# The native desktop entry and icon (same app ID) would hide the Flatpak's.
remove_native() {
    local removed=0
    for f in "$BIN/encoder" "$SHARE/applications/$APP_ID.desktop" "$SHARE/icons/hicolor/scalable/apps/$APP_ID.svg"; do
        if [ -e "$f" ]; then rm -f "$f" && echo "removed $f" && removed=1; fi
    done
    remove_old_ids
    [ "$removed" = 1 ] && refresh
    return 0
}

install_native() {
    cargo build --release
    install -Dm755 target/release/encoder "$BIN/encoder"
    install -Dm644 "data/$APP_ID.desktop" "$SHARE/applications/$APP_ID.desktop"
    install -Dm644 "data/icons/hicolor/scalable/apps/$APP_ID.svg" "$SHARE/icons/hicolor/scalable/apps/$APP_ID.svg"
    remove_old_ids
    refresh
    echo "Installed encoder $VERSION to $BIN"
    if flatpak_installed; then
        echo "note: the Flatpak is installed too; this native entry hides it in the app grid."
    fi
}

uninstall() {
    remove_native
    if flatpak_installed; then
        flatpak uninstall --user -y "$APP_ID"
    fi
    refresh
    echo "Uninstalled."
}

# Everything the build needs is fetched automatically: flatpak-builder (from
# Flathub if it isn't installed), the GNOME SDK and Rust extension, and the
# ffmpeg, libvmaf, dovi_tool and crate sources.
make_flatpak() {
    command -v flatpak >/dev/null || { echo "error: flatpak is missing: sudo dnf install flatpak" >&2; exit 1; }
    flatpak remote-add --user --if-not-exists flathub https://dl.flathub.org/repo/flathub.flatpakrepo
    local builder=(flatpak-builder)
    if ! command -v flatpak-builder >/dev/null; then
        if ! flatpak info org.flatpak.Builder >/dev/null 2>&1; then
            echo "flatpak-builder isn't installed; installing it from Flathub for this user"
            flatpak install --user -y flathub org.flatpak.Builder
        fi
        builder=(flatpak run org.flatpak.Builder)
    fi
    echo "Building the Flatpak. The first build downloads the GNOME SDK and compiles"
    echo "ffmpeg and libvmaf, which takes a while and a few GB of disk; later builds are cached."
    mkdir -p "$FP_DIR"
    "${builder[@]}" --user --install-deps-from=flathub --force-clean --ccache \
        --state-dir="$FP_DIR/.state" --repo="$FP_DIR/repo" "$FP_DIR/build" "$MANIFEST"
    flatpak build-bundle --runtime-repo=https://dl.flathub.org/repo/flathub.flatpakrepo \
        "$FP_DIR/repo" "$BUNDLE" "$APP_ID"
    echo "Made $BUNDLE"
}

install_flatpak() {
    [ -f "$BUNDLE" ] || make_flatpak
    migrate_flatpak_config
    flatpak install --user -y --reinstall --bundle "$BUNDLE"
    remove_native
    echo "Installed the Flatpak ($VERSION). Run it from the app grid or with: flatpak run $APP_ID"
}

# ------------------------------------------------------------ release
# Only in the author's checkout (the one with the NAS remote): bump, back up,
# and publish the source on GitHub. There is no published Flatpak — it would
# bundle patent-encumbered codecs (see the README) — so the release steps are
# Bindle's minus "Make distribution" and flatpak.oyzmo.no, same numbers.
is_author() { git remote get-url nas >/dev/null 2>&1; }

bump_version() {
    local level
    echo "Now at $VERSION."
    read -rp "Bump which part? [p]atch (fixes) / [m]inor (features) / ma[j]or: " level || { echo; level=; }  # Ctrl+D cancels
    case "$level" in
        p|P|patch) level=patch ;;
        m|M|minor) level=minor ;;
        j|J|major) level=major ;;
        *) echo "Cancelled."; return 0 ;;
    esac
    scripts/bump.sh "$level"
    local new
    new=$(sed -n 's/^version = "\([0-9.]*\)"/\1/p' Cargo.toml | head -1)
    git commit -q -m "encodeR $new" -- Cargo.toml Cargo.lock
    echo "Committed encodeR $new."
}

# The GitHub release's notes are the commit subjects since the last deploy
# (deploy-github.sh writes them), so there is nothing else to prepare.
# --confirm shows the dry run and asks before publishing, in one run.
release_github() {
    scripts/deploy-github.sh --confirm --release
}

full_release() {
    if [ -n "$(git status --porcelain --untracked-files=no)" ]; then
        echo "error: uncommitted changes; commit them first" >&2
        exit 1
    fi
    echo "Full release of encodeR $VERSION"
    echo "Backing up to the NAS…"
    git push -q nas main
    release_github
}

bold() { printf '\033[1m%s\033[0m\n' "$*"; }

# Each choice runs this script again with its option, so a failing step
# (set -e) ends that step only, not the menu.
run_step() {
    "$SELF" "$1" || printf '\033[31m✘ That step failed (see above).\033[0m\n'
}

menu() {
    while true; do
        VERSION=$(sed -n 's/^version = "\([0-9.]*\)"/\1/p' Cargo.toml | head -1)
        echo
        bold "── encodeR $VERSION ─────────────────────────"
        echo "  1) Install (native, ~/.local)"
        echo "  2) Uninstall"
        echo "  3) Make flatpak"
        echo "  4) Install flatpak"
        if is_author; then
            bold "── Release (in this order) ──────────────"
            echo " 11) Bump version (commits)"
            echo " 13) Full release: NAS → 9 in one go"
            echo "     or step by step:"
            echo "  9)   Release on GitHub (source only, no binaries)"
        fi
        echo "  q) Quit"
        # Ctrl+D on an empty line quits. After typed text it takes two presses to
        # reach end of input, and then the typed choice still counts.
        read -rp "Choose: " choice || { echo; [ -n "$choice" ] || exit 0; }
        case "$choice" in
            1) run_step --native ;;
            2) run_step --uninstall ;;
            3) run_step --make-flatpak ;;
            4) run_step --install-flatpak ;;
            9) if is_author; then run_step --release-github; else echo "Unknown choice."; fi ;;
            11) if is_author; then run_step --bump; else echo "Unknown choice."; fi ;;
            13) if is_author; then run_step --full-release; else echo "Unknown choice."; fi ;;
            q|Q) exit 0 ;;
            *) echo "Unknown choice." ;;
        esac
    done
}

case "${1:-}" in
    --native) install_native ;;
    --uninstall) uninstall ;;
    --make-flatpak) make_flatpak ;;
    --install-flatpak) install_flatpak ;;
    --bump) bump_version ;;
    --release-github) release_github ;;
    --full-release) full_release ;;
    "")
        if [ -t 0 ]; then menu
        else sed -n '2,6p' "$SELF" | sed 's/^# \{0,1\}//'; exit 1; fi
        ;;
    *) sed -n '2,6p' "$SELF" | sed 's/^# \{0,1\}//'; exit 1 ;;
esac
