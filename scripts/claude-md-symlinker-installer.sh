#!/bin/sh
set -eu

APP_NAME="claude-md-symlinker"
REPO="dutifuldev/claude-md-symlinker"
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
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum -c "$checksum_name" >/dev/null
        return
    fi
    need_cmd shasum
    expected="$(awk '{print $1}' "$checksum_name")"
    actual="$(shasum -a 256 "$archive_name" | awk '{print $1}')"
    [ "$expected" = "$actual" ] || die "checksum verification failed for $archive_name"
}

append_path_profile() {
    profile="$1"
    env_file="$2"
    [ "${CLAUDE_MD_SYMLINKER_NO_MODIFY_PATH:-0}" = "1" ] && return
    [ -e "$profile" ] || : > "$profile" 2>/dev/null || return
    grep -F "$env_file" "$profile" >/dev/null 2>&1 && return
    {
        printf '\n# %s\n' "$APP_NAME"
        printf '. "%s"\n' "$env_file"
    } >> "$profile" 2>/dev/null || true
}

ensure_path() {
    bin_dir="$1"
    prefix="$2"
    [ "${CLAUDE_MD_SYMLINKER_NO_MODIFY_PATH:-0}" = "1" ] && return

    case ":${PATH:-}:" in
        *:"$bin_dir":*) return ;;
    esac

    env_file="$prefix/env"
    if [ ! -e "$env_file" ]; then
        {
            printf '#!/bin/sh\n'
            printf 'case ":${PATH}:" in\n'
            printf '  *:"%s":*) ;;\n' "$bin_dir"
            printf '  *) export PATH="%s:$PATH" ;;\n' "$bin_dir"
            printf 'esac\n'
        } > "$env_file" 2>/dev/null || true
    fi

    home="$(home_dir)"
    append_path_profile "$home/.profile" "$env_file"
    append_path_profile "$home/.bashrc" "$env_file"
    append_path_profile "$home/.zshrc" "$env_file"
}

run_setup() {
    bin="$1"
    os="${CLAUDE_MD_SYMLINKER_OS:-$(uname -s)}"

    [ "${CLAUDE_MD_SYMLINKER_NO_SETUP:-0}" = "1" ] && return

    auto_flag=""
    prompt_stdin=0
    if [ "${CLAUDE_MD_SYMLINKER_NO_AUTO_MIGRATE:-0}" = "1" ]; then
        auto_flag="--no-auto-migrate"
    elif [ "${CLAUDE_MD_SYMLINKER_AUTO_MIGRATE:-0}" = "1" ]; then
        auto_flag="--auto-migrate"
    elif can_prompt; then
        prompt_stdin=1
    else
        auto_flag="--auto-migrate"
    fi

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

    if [ "$prompt_stdin" = "1" ]; then
        "$bin" "$@" < /dev/tty
    else
        "$bin" "$@"
    fi
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
    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' EXIT HUP INT TERM

    download_with_progress "$base/$archive_name" "$tmp/$archive_name" "Downloading $archive_name"
    download_quiet "$base/$checksum_name" "$tmp/$checksum_name"

    (
        cd "$tmp" || exit 1
        verify_checksum "$archive_name" "$checksum_name"
        tar -xf "$archive_name"
    )

    home="$(home_dir)"
    prefix="${CLAUDE_MD_SYMLINKER_INSTALL_DIR:-${CARGO_HOME:-$home/.cargo}}"
    bin_dir="$prefix/bin"
    bin="$bin_dir/$APP_NAME"
    extracted="$tmp/$APP_NAME-$target/$APP_NAME"

    [ -f "$extracted" ] || die "archive did not contain $APP_NAME"
    mkdir -p "$bin_dir"
    chmod +x "$extracted"
    cp "$extracted" "$bin.tmp.$$"
    mv "$bin.tmp.$$" "$bin"

    ensure_path "$bin_dir" "$prefix"
    say "Installed $APP_NAME to $bin"
    run_setup "$bin"
}

main "$@"
