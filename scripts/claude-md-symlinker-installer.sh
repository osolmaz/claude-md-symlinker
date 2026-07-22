#!/bin/sh
set -eu

APP_NAME="claude-md-symlinker"
REPO="osolmaz/claude-md-symlinker"
APP_VERSION="${CLAUDE_MD_SYMLINKER_VERSION:-__CLAUDE_MD_SYMLINKER_VERSION__}"

say() {
    printf '%s\n' "$*"
}

die() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}

need_cmd() {
    command -v "$1" >/dev/null 2>&1 || die "missing required command: $1"
}

download_with_progress() {
    url="$1"
    output="$2"
    label="$3"
    say "$label"
    if [ "${CLAUDE_MD_SYMLINKER_NO_PROGRESS:-0}" = "1" ]; then
        curl -fsSL --retry 3 --retry-delay 2 "$url" -o "$output"
    else
        curl -fL --retry 3 --retry-delay 2 --progress-bar "$url" -o "$output"
    fi
}

download_quiet() {
    url="$1"
    output="$2"
    curl -fsSL --retry 3 --retry-delay 2 "$url" -o "$output"
}

can_prompt() {
    [ -r /dev/tty ] && [ -w /dev/tty ] && ( : < /dev/tty ) 2>/dev/null
}

resolve_auto_migrate_flag() {
    [ "${CLAUDE_MD_SYMLINKER_NO_SETUP:-0}" = "1" ] && return

    if [ "${CLAUDE_MD_SYMLINKER_NO_AUTO_MIGRATE:-0}" = "1" ]; then
        printf '%s\n' "--no-auto-migrate"
        return
    fi
    if [ "${CLAUDE_MD_SYMLINKER_AUTO_MIGRATE:-0}" = "1" ]; then
        printf '%s\n' "--auto-migrate"
        return
    fi
    if ! can_prompt; then
        printf '%s\n' "--auto-migrate"
        return
    fi

    {
        printf '%s\n' "Automatically migrate safe existing CLAUDE.md files to AGENTS.md when Claude finds them while working through directories?"
        printf '\n'
        printf '%s\n' "This does not scan your whole machine or whole repos. It only applies to CLAUDE.md files found in directories Claude actually enters, and only when the migration passes the safe checks."
        printf '\n'
        printf '%s' "Default: yes [Y/n] "
    } > /dev/tty

    answer=""
    IFS= read -r answer < /dev/tty || answer=""
    case "$answer" in
        n|N|no|No|nO|NO)
            printf '%s\n' "--no-auto-migrate"
            ;;
        *)
            printf '%s\n' "--auto-migrate"
            ;;
    esac
}

home_dir() {
    if [ -n "${HOME:-}" ]; then
        printf '%s\n' "$HOME"
        return
    fi
    die "HOME is not set"
}

download_base() {
    case "$APP_VERSION" in
        ""|"latest"|"__CLAUDE_MD_SYMLINKER_VERSION__")
            printf 'https://github.com/%s/releases/latest/download\n' "$REPO"
            ;;
        *)
            printf 'https://github.com/%s/releases/download/%s\n' "$REPO" "$APP_VERSION"
            ;;
    esac
}

detect_target() {
    os="${CLAUDE_MD_SYMLINKER_OS:-$(uname -s)}"
    arch="${CLAUDE_MD_SYMLINKER_ARCH:-$(uname -m)}"
    case "$os:$arch" in
        Darwin:arm64|Darwin:aarch64)
            printf 'aarch64-apple-darwin\n'
            ;;
        Darwin:x86_64|Darwin:amd64)
            printf 'x86_64-apple-darwin\n'
            ;;
        Linux:arm64|Linux:aarch64)
            printf 'aarch64-unknown-linux-gnu\n'
            ;;
        Linux:x86_64|Linux:amd64)
            printf 'x86_64-unknown-linux-gnu\n'
            ;;
        *)
            die "unsupported platform: $os $arch"
            ;;
    esac
}

verify_checksum() {
    archive_name="$1"
    checksum_name="$2"
    expected="$(awk 'NF {print $1; exit}' "$checksum_name")"
    [ -n "$expected" ] || die "checksum file is empty for $archive_name"
    if command -v sha256sum >/dev/null 2>&1; then
        actual="$(sha256sum "$archive_name" | awk '{print $1}')"
    else
        need_cmd shasum
        actual="$(shasum -a 256 "$archive_name" | awk '{print $1}')"
    fi
    [ "$expected" = "$actual" ] || die "checksum verification failed for $archive_name"
}

append_path_profile() {
    profile="$1"
    bin_dir="$2"
    [ "${CLAUDE_MD_SYMLINKER_NO_MODIFY_PATH:-0}" = "1" ] && return 0
    [ -e "$profile" ] || ( : > "$profile" ) 2>/dev/null || return 0
    grep -F "$bin_dir" "$profile" >/dev/null 2>&1 && return 0
    {
        printf '\n# %s\n' "$APP_NAME"
        printf 'case ":$PATH:" in\n'
        printf '  *:"%s":*) ;;\n' "$bin_dir"
        printf '  *) export PATH="%s:$PATH" ;;\n' "$bin_dir"
        printf 'esac\n'
    } >> "$profile" 2>/dev/null || true
}

ensure_path() {
    bin_dir="$1"
    [ "${CLAUDE_MD_SYMLINKER_NO_MODIFY_PATH:-0}" = "1" ] && return 0

    case ":${PATH:-}:" in
        *:"$bin_dir":*) return 0 ;;
    esac

    home="$(home_dir)"
    append_path_profile "$home/.profile" "$bin_dir"
    append_path_profile "$home/.bashrc" "$bin_dir"
    append_path_profile "$home/.zshrc" "$bin_dir"
}

install_bin_dir() {
    if [ -n "${CLAUDE_MD_SYMLINKER_INSTALL_DIR:-}" ]; then
        printf '%s\n' "$CLAUDE_MD_SYMLINKER_INSTALL_DIR"
        return
    fi
    printf '%s/.local/bin\n' "$(home_dir)"
}

run_setup() {
    bin="$1"
    auto_flag="$2"
    os="${CLAUDE_MD_SYMLINKER_OS:-$(uname -s)}"

    [ "${CLAUDE_MD_SYMLINKER_NO_SETUP:-0}" = "1" ] && return

    no_service=0
    if [ "$os" = "Darwin" ] || [ "${CLAUDE_MD_SYMLINKER_NO_SERVICE:-0}" = "1" ]; then
        no_service=1
    fi

    set -- install
    if [ "$no_service" = "1" ]; then
        set -- "$@" --no-service
    fi
    if [ -n "$auto_flag" ]; then
        set -- "$@" "$auto_flag"
    fi

    "$bin" "$@"
}

main() {
    need_cmd uname
    need_cmd curl
    need_cmd tar
    need_cmd mktemp
    need_cmd chmod
    need_cmd mkdir
    need_cmd cp
    need_cmd mv
    need_cmd rm
    need_cmd awk

    target="$(detect_target)"
    archive_name="$APP_NAME-$target.tar.xz"
    checksum_name="$archive_name.sha256"
    base="$(download_base)"
    auto_flag="$(resolve_auto_migrate_flag)"
    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' EXIT HUP INT TERM

    say "Installing $APP_NAME for $target"
    download_with_progress "$base/$archive_name" "$tmp/$archive_name" "Downloading $archive_name"
    download_quiet "$base/$checksum_name" "$tmp/$checksum_name"

    (
        cd "$tmp" || exit 1
        say "Verifying $archive_name"
        verify_checksum "$archive_name" "$checksum_name"
        say "Extracting $archive_name"
        tar -xf "$archive_name"
    )

    bin_dir="$(install_bin_dir)"
    bin="$bin_dir/$APP_NAME"
    extracted="$tmp/$APP_NAME-$target/$APP_NAME"

    [ -f "$extracted" ] || die "archive did not contain $APP_NAME"
    mkdir -p "$bin_dir"
    chmod +x "$extracted"
    say "Installing binary to $bin"
    cp "$extracted" "$bin.tmp.$$"
    mv "$bin.tmp.$$" "$bin"

    ensure_path "$bin_dir"
    say "Installed $APP_NAME to $bin"
    run_setup "$bin" "$auto_flag"
}

main "$@"
