#!/bin/sh
set -eu

REPO="veegn/serverless-monitor-agent"
GITHUB_PROXY=""
INSTALL_VERSION="latest"

log() {
    printf '%s\n' "$*"
}

die() {
    printf '[ERROR] %s\n' "$*" >&2
    exit 1
}

cmd=""
need_val=""
orig_count=$#

while [ $orig_count -gt 0 ]; do
    arg="$1"
    shift
    orig_count=$((orig_count - 1))
    if [ -n "$need_val" ]; then
        case "$need_val" in
            proxy) GITHUB_PROXY="$arg" ;;
            version) INSTALL_VERSION="$arg" ;;
        esac
        need_val=""
        continue
    fi
    case "$arg" in
        --install-ghproxy=*)
            GITHUB_PROXY="${arg#*=}"
            ;;
        --install-ghproxy)
            need_val="proxy"
            ;;
        --install-version=*)
            INSTALL_VERSION="${arg#*=}"
            ;;
        --install-version)
            need_val="version"
            ;;
        *)
            if [ -z "$cmd" ] && [ "${arg#-}" = "$arg" ]; then
                cmd="$arg"
            fi
            set -- "$@" "$arg"
            ;;
    esac
done

cmd="${cmd:-install}"

# If --install-ghproxy was passed for bootstrap, pass -install-ghproxy to sma install
if [ -n "$GITHUB_PROXY" ] && [ "$cmd" = "install" ]; then
    has_proxy_arg=0
    for a in "$@"; do
        case "$a" in
            -install-ghproxy=*|-install_ghproxy=*) has_proxy_arg=1 ;;
        esac
    done
    if [ "$has_proxy_arg" -eq 0 ]; then
        set -- "$@" "-install-ghproxy=$GITHUB_PROXY"
    fi
fi

detect_os() {
    os="$(uname -s 2>/dev/null || printf unknown)"
    case "$os" in
        Linux) printf linux ;;
        Darwin) printf darwin ;;
        FreeBSD) printf freebsd ;;
        MINGW*|MSYS*|CYGWIN*) printf windows ;;
        *) die "unsupported OS: $os" ;;
    esac
}

check_darwin_version() {
    ver="$(sw_vers -productVersion 2>/dev/null || true)"
    [ -n "$ver" ] || return 0
    major="${ver%%.*}"
    case "$major" in
        ''|*[!0-9]*) return 0 ;;
    esac
    if [ "$major" -ge 11 ]; then
        return 0
    fi
    case "$cmd" in
        uninstall|remove|delete|purge)
            log "[WARN] macOS $ver is older than the supported minimum macOS 11 Big Sur."
            log "[WARN] the temporary uninstaller may not run; if it fails, run the installed 'sma uninstall' directly."
            return 0
            ;;
    esac
    die "macOS $ver is not supported: this release requires macOS 11 Big Sur or later."
}

detect_arch() {
    arch="$(uname -m 2>/dev/null || printf unknown)"
    case "$os_name" in
        darwin)
            case "$arch" in
                x86_64|amd64) printf amd64 ;;
                aarch64|arm64) printf arm64 ;;
                *) die "unsupported architecture for macOS: $arch (supported: amd64, arm64)" ;;
            esac
            ;;
        freebsd)
            case "$arch" in
                x86_64|amd64) printf amd64 ;;
                *) die "unsupported architecture for FreeBSD: $arch (supported: amd64)" ;;
            esac
            ;;
        windows)
            case "$arch" in
                x86_64|amd64) printf amd64 ;;
                aarch64|arm64) printf arm64 ;;
                i386|i686) printf 386 ;;
                *) die "unsupported architecture for Windows: $arch (supported: amd64, arm64, 386)" ;;
            esac
            ;;
        linux)
            case "$arch" in
                x86_64|amd64) printf amd64 ;;
                aarch64|arm64) printf arm64 ;;
                i386|i686) printf 386 ;;
                armv7*|armv8l|armhf) printf armv7 ;;
                armv5*|armv6*) die "ARM architecture '$arch' is not supported (pre-compiled binaries require ARMv7 or ARM64)" ;;
                mips*|loong*) die "architecture '$arch' is not supported by pre-compiled binaries" ;;
                *) die "unsupported architecture for Linux: $arch (supported: amd64, arm64, 386, armv7)" ;;
            esac
            ;;
        *)
            die "unsupported OS: $os_name"
            ;;
    esac
}

download() {
    url="$1"
    out="$2"
    if command -v curl >/dev/null 2>&1; then
        curl -fL --connect-timeout 10 -m 120 -o "$out" "$url"
    elif command -v wget >/dev/null 2>&1; then
        wget -O "$out" "$url"
    else
        die "curl or wget is required for bootstrap download"
    fi
}

run_payload() {
    bin="$1"
    shift
    case "${cmd:-${1:-install}}" in
        uninstall|remove|delete|purge)
            "$bin" "$cmd"
            return
            ;;
    esac
    if [ "$#" -eq 0 ]; then
        "$bin" install
        return
    fi
    "$bin" "$@"
}

run_payload_checked() {
    err_file="$1"
    shift
    if run_payload "$@" 2>"$err_file"; then
        return 0
    else
        rc=$?
        return "$rc"
    fi
}

dir_has_noexec() {
    dir="$1"
    if command -v findmnt >/dev/null 2>&1; then
        opts="$(findmnt -no OPTIONS -T "$dir" 2>/dev/null || true)"
        case ",$opts," in
            *,noexec,*) return 0 ;;
        esac
    fi
    return 1
}

stage_and_run_payload() {
    dir="$1"
    shift
    [ -n "$dir" ] || return 125
    if dir_has_noexec "$dir"; then
        return 126
    fi
    mkdir -p "$dir" 2>/dev/null || return 125
    stage="$dir/.sma-bootstrap.$$"
    stage_err="$stage.err"
    cp "$tmp" "$stage" 2>/dev/null || return 125
    chmod +x "$stage" 2>/dev/null || {
        rm -f "$stage"
        return 125
    }
    if run_payload_checked "$stage_err" "$stage" "$@"; then
        rc=0
    else
        rc=$?
        if [ "$rc" -ne 126 ]; then
            cat "$stage_err" >&2 2>/dev/null || true
        fi
    fi
    rm -f "$stage" "$stage_err"
    return "$rc"
}

case "$cmd" in
    uninstall|remove|delete|purge)
        log "[INFO] downloading temporary uninstaller"
        ;;
esac

os_name="$(detect_os)"
# Only the default "latest" path is gated. An explicit --install-version is a
# deliberate choice, and pinning a version is exactly how macOS 12 users follow
# the "install v1.0.20 or earlier" advice; gating it would make that impossible.
if [ "$os_name" = "darwin" ] && [ "$INSTALL_VERSION" = "latest" ]; then
    check_darwin_version
fi
arch_name="$(detect_arch)"
asset="sma-${os_name}-${arch_name}"
if [ "$os_name" = "windows" ]; then
    asset="${asset}.exe"
fi

if [ "$INSTALL_VERSION" = "latest" ]; then
    path="latest/download"
else
    path="download/$INSTALL_VERSION"
fi
url="https://github.com/$REPO/releases/$path/$asset"
if [ -n "$GITHUB_PROXY" ]; then
    url="${GITHUB_PROXY%/}/$url"
fi

tmp_dir="${TMPDIR:-/tmp}"
tmp="${tmp_dir%/}/sma-bootstrap.$$"
tmp_err="$tmp.err"
trap 'rm -f "$tmp" "$tmp_err"' EXIT INT TERM

log "Serverless Monitor Agent (sma) bootstrap"
log "  repo    : $REPO"
log "  version : $INSTALL_VERSION"
log "  target  : $os_name/$arch_name"
log "  asset   : $asset"
log "  url     : $url"

download "$url" "$tmp"
chmod +x "$tmp"

status=126
if ! dir_has_noexec "$tmp_dir"; then
    if run_payload_checked "$tmp_err" "$tmp" "$@"; then
        exit 0
    else
        status=$?
    fi
    if [ "$status" -ne 126 ]; then
        cat "$tmp_err" >&2 2>/dev/null || true
        exit "$status"
    fi
fi

log "[WARN] cannot execute bootstrap binary from $tmp; trying executable staging directories"
if [ -n "${HOME:-}" ]; then
    if stage_and_run_payload "$HOME/.sma/tmp" "$@"; then
        exit 0
    else
        status=$?
    fi
    if [ "$status" -ne 125 ] && [ "$status" -ne 126 ]; then
        exit "$status"
    fi
fi
if [ "$os_name" = "darwin" ]; then
    fallback_dirs="."
else
    fallback_dirs="/usr/local/bin /usr/bin /root ."
fi
for dir in $fallback_dirs; do
    if stage_and_run_payload "$dir" "$@"; then
        exit 0
    else
        status=$?
    fi
    if [ "$status" -ne 125 ] && [ "$status" -ne 126 ]; then
        exit "$status"
    fi
done

die "downloaded binary could not be executed. /tmp may be mounted noexec."
