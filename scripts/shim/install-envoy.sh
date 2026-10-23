#!/bin/sh
# Installs the envoy launcher for the current user.
#
# The launcher goes to ~/.envoy/bin as `envoy` (with `en` linked to it). It reads
# the team's redirect file, copies the envoy version it pins to the local cache the
# first time it is needed, and runs it.
#
# Run this from an unpacked release of the launcher, or from the shim folder on the
# share (the one that holds current.json).
#
# Usage: install-envoy.sh [--source DIR] [--modify-profile]
#        install-envoy.sh --uninstall [--remove-cache] [--yes]
#
#   --source DIR       folder holding `envoy`, or the shim folder that holds
#                      current.json (default: the folder this script is in)
#   --modify-profile   put ~/.envoy/bin on PATH in your shell profile
#   --uninstall        remove the launcher, and the profile line this script added
#   --remove-cache     with --uninstall, also delete the local cache
#   --yes              do not ask before deleting the cache
#   -h, --help         show this text

set -eu

here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
source_dir=$here
modify_profile=0
uninstall=0
remove_cache=0
assume_yes=0
marker='# envoy-launcher'

fail() {
    echo "install-envoy: $*" >&2
    exit 1
}

usage() {
    awk 'NR > 1 && /^#/ { sub(/^# ?/, ""); print; next } NR > 1 { exit }' "$0"
}

while [ $# -gt 0 ]; do
    case "$1" in
        --source)
            [ $# -ge 2 ] || fail "--source needs a folder"
            source_dir=$2
            shift 2
            ;;
        --modify-profile) modify_profile=1; shift ;;
        --uninstall) uninstall=1; shift ;;
        --remove-cache) remove_cache=1; shift ;;
        --yes) assume_yes=1; shift ;;
        -h | --help) usage; exit 0 ;;
        *) fail "unknown option $1 (try --help)" ;;
    esac
done

if [ -n "${ENVOY_CONFIG_ROOT:-}" ]; then
    root=$ENVOY_CONFIG_ROOT
else
    [ -n "${HOME:-}" ] || fail "HOME is not set"
    root=$HOME/.envoy
fi
bin=$root/bin

detect_target() {
    case "$(uname -s)" in
        Linux) os=linux ;;
        Darwin) os=macos ;;
        *) fail "unsupported operating system $(uname -s)" ;;
    esac
    case "$(uname -m)" in
        x86_64 | amd64) arch=x86_64 ;;
        arm64 | aarch64) arch=aarch64 ;;
        *) fail "unsupported CPU $(uname -m)" ;;
    esac
    if [ "$os" = linux ]; then
        target=$os-$arch-musl
    else
        target=$os-$arch
    fi
}

find_launcher() {
    if [ -f "$source_dir/envoy" ]; then
        launcher=$source_dir/envoy
        return
    fi
    if [ -f "$source_dir/current.json" ]; then
        version=$(sed -n 's/.*"version"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$source_dir/current.json" | head -n 1)
        case "$version" in
            '' | .* | *[!A-Za-z0-9._+-]*)
                fail "$source_dir/current.json does not name a usable version (it has '$version')"
                ;;
        esac
        detect_target
        launcher=$source_dir/$version/$target/envoy
        [ -f "$launcher" ] || fail "$source_dir/current.json names version $version, but $launcher does not exist"
        return
    fi
    fail "found neither envoy nor current.json in $source_dir; pass --source with the folder that holds the launcher"
}

profile_file() {
    case "${SHELL:-}" in
        */zsh) echo "$HOME/.zprofile" ;;
        *) echo "$HOME/.profile" ;;
    esac
}

# The PATH line as written to a profile: $HOME stays a variable when the folder is under it.
path_line() {
    case "$bin" in
        "$HOME"/*) folder="\$HOME${bin#"$HOME"}" ;;
        *) folder=$bin ;;
    esac
    printf 'export PATH="%s:$PATH" %s\n' "$folder" "$marker"
}

install() {
    find_launcher
    mkdir -p "$bin"
    cp "$launcher" "$bin/envoy.new"
    chmod 755 "$bin/envoy.new"
    mv -f "$bin/envoy.new" "$bin/envoy"
    ln -sfn envoy "$bin/en"
    if [ "$(uname -s)" = Darwin ]; then
        xattr -d com.apple.quarantine "$bin/envoy" 2>/dev/null || true
    fi

    installed=$("$bin/envoy" --envoy-shim-version) || fail "the installed launcher does not run: $bin/envoy"
    echo "Installed $installed in $bin"

    case ":$PATH:" in
        *":$bin:"*) echo "$bin is already on PATH." ;;
        *)
            if [ "$modify_profile" = 1 ]; then
                profile=$(profile_file)
                if [ -f "$profile" ] && grep -q "$marker" "$profile"; then
                    echo "$profile already puts $bin on PATH."
                else
                    path_line >> "$profile"
                    echo "Added $bin to PATH in $profile. Start a new login shell to use it."
                fi
            else
                echo "$bin is not on PATH. Add it to your shell profile:"
                echo "  $(path_line | sed "s/ $marker\$//")"
                echo "or run this script again with --modify-profile."
            fi
            ;;
    esac

    echo
    if [ -n "${ENVOY_CONFIG_PROD:-}" ] || [ -n "${ENVOY_REDIRECT_CONFIG:-}" ]; then
        "$bin/envoy" --envoy-shim-resolve ||
            echo "The launcher is installed, but it cannot yet work out which envoy version you should run." \
                "Fix the problem above (the redirect file may be missing or unreadable), then run: envoy --envoy-shim-resolve" >&2
    else
        echo "Next: set ENVOY_CONFIG_PROD to your team's config folder (ask your administrator), then run: envoy --envoy-shim-resolve"
    fi
}

uninstall() {
    rm -f "$bin/envoy" "$bin/en" "$bin/envoy.new"
    rmdir "$bin" 2>/dev/null || true
    echo "Removed the envoy launcher from $bin"

    for profile in "$HOME/.profile" "$HOME/.zprofile"; do
        if [ -f "$profile" ] && grep -q "$marker" "$profile"; then
            kept=$(mktemp)
            grep -v "$marker" "$profile" > "$kept" || true
            cat "$kept" > "$profile"
            rm -f "$kept"
            echo "Removed the PATH line from $profile."
        fi
    done

    if [ "$remove_cache" = 1 ] && [ -d "$root/cache" ]; then
        if [ "$assume_yes" != 1 ]; then
            printf 'Delete the local cache %s? [y/N] ' "$root/cache"
            read -r answer || answer=n
            case "$answer" in
                y | Y | yes | YES) ;;
                *) echo "Kept the cache."; return ;;
            esac
        fi
        rm -rf "$root/cache"
        echo "Deleted $root/cache"
    fi
}

if [ "$uninstall" = 1 ]; then
    uninstall
else
    install
fi
