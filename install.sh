#!/bin/sh
set -eu

DEFAULT_REPO="xxb/herdr-tui"
TMP_DIR=""
STAGED_BIN=""

cleanup() {
    if [ -n "$STAGED_BIN" ] && [ -e "$STAGED_BIN" ]; then
        rm -f "$STAGED_BIN"
    fi
    if [ -n "$TMP_DIR" ] && [ -d "$TMP_DIR" ]; then
        rm -rf "$TMP_DIR"
    fi
}

info() {
    printf '==> %s\n' "$1"
}

warn() {
    printf 'Warning: %s\n' "$1" >&2
}

die() {
    printf 'Error: %s\n' "$1" >&2
    exit 1
}

usage() {
    cat <<'EOF'
Install, update, or uninstall hct (herdr-tui) on Linux and macOS.

Usage:
  install.sh [OPTIONS]

Install / Update:
  curl -fsSL https://raw.githubusercontent.com/xxb/herdr-tui/main/install.sh | sh
  install.sh --version v0.13.0
  install.sh --dir ~/.local/bin

Uninstall:
  curl -fsSL https://raw.githubusercontent.com/xxb/herdr-tui/main/install.sh | sh -s -- --uninstall
  install.sh --uninstall --purge-config

Options:
  -v, --version <VER>   Install a specific release version (e.g. v0.13.0 or 0.13.0)
  -d, --dir <DIR>       Target binary directory (default: $HOME/.local/bin)
      --uninstall       Remove installed hct binary (leaves herdr-core & config untouched)
      --purge-config    Also remove hct config directory ($XDG_CONFIG_HOME/herdr-tui);
                        requires --uninstall
  -h, --help            Show this help message

Environment variables:
  HCT_VERSION           Default version if --version is not passed
  HCT_INSTALL_DIR       Default install directory if --dir is not passed
  HCT_REPO              GitHub repository slug (default: xxb/herdr-tui)
  HCT_RELEASE_URL       Release base URL override (default: https://github.com/<repo>/releases)
EOF
}

strip_trailing_slashes() {
    _sts_path=$1
    while [ "$_sts_path" != "/" ]; do
        case "$_sts_path" in
            */) _sts_path=${_sts_path%/} ;;
            *) break ;;
        esac
    done
    printf '%s\n' "$_sts_path"
}

is_valid_tag() {
    _ivt_tag=$1
    case "$_ivt_tag" in
        */*|*..*|*" "*|*"	"*) return 1 ;;
    esac
    _ivt_lines=$(printf '%s\n' "$_ivt_tag" | wc -l | tr -d ' \t')
    [ "$_ivt_lines" = "1" ] || return 1
    printf '%s\n' "$_ivt_tag" | grep -Eq '^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$'
}

normalize_tag() {
    _nt_raw=$1
    case "$_nt_raw" in
        v*) _nt_norm=$_nt_raw ;;
        *) _nt_norm="v${_nt_raw}" ;;
    esac
    if ! is_valid_tag "$_nt_norm"; then
        die "Invalid version '$_nt_raw'. Expected format like v0.13.0 or 0.13.0."
    fi
    printf '%s\n' "$_nt_norm"
}

validate_repo() {
    _vr_repo=$1
    case "$_vr_repo" in
        *..*|*" "*|*"	"*) return 1 ;;
    esac
    _vr_lines=$(printf '%s\n' "$_vr_repo" | wc -l | tr -d ' \t')
    [ "$_vr_lines" = "1" ] || return 1
    printf '%s\n' "$_vr_repo" | grep -Eq '^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$'
}

create_tmp_dir() {
    if _ctd_dir=$(mktemp -d 2>/dev/null); then
        printf '%s\n' "$_ctd_dir"
        return 0
    fi
    if _ctd_dir=$(mktemp -d -t hct-install 2>/dev/null); then
        printf '%s\n' "$_ctd_dir"
        return 0
    fi
    die "Failed to create a temporary directory."
}

detect_downloader() {
    if command -v curl >/dev/null 2>&1; then
        printf '%s\n' "curl"
        return 0
    fi
    if command -v wget >/dev/null 2>&1; then
        printf '%s\n' "wget"
        return 0
    fi
    die "Neither 'curl' nor 'wget' was found. Please install one and re-run."
}

download_file() {
    _df_dl=$1
    _df_url=$2
    _df_dest=$3
    case "$_df_dl" in
        curl)
            curl -fsSL -o "$_df_dest" "$_df_url"
            ;;
        wget)
            wget -q -O "$_df_dest" "$_df_url"
            ;;
        *)
            die "Internal error: unknown downloader '$_df_dl'."
            ;;
    esac
}

extract_tag_from_url() {
    _et_url=$1
    _et_url=${_et_url%%\?*}
    _et_url=${_et_url%%#*}
    _et_url=$(strip_trailing_slashes "$_et_url")
    case "$_et_url" in
        */tag/*)
            _et_tag=${_et_url##*/tag/}
            if is_valid_tag "$_et_tag"; then
                printf '%s\n' "$_et_tag"
                return 0
            fi
            ;;
    esac
    return 1
}

resolve_latest_tag() {
    _rlt_dl=$1
    _rlt_release_url=$2
    _rlt_repo=$3
    _rlt_tmp=$4
    _rlt_latest_url="${_rlt_release_url}/latest"

    if [ "$_rlt_dl" = "curl" ]; then
        _rlt_effective=$(curl -fsSL -o /dev/null -w '%{url_effective}' "$_rlt_latest_url" 2>/dev/null || true)
        if [ -n "$_rlt_effective" ] && _rlt_tag=$(extract_tag_from_url "$_rlt_effective"); then
            printf '%s\n' "$_rlt_tag"
            return 0
        fi
    else
        _rlt_headers=$(wget --max-redirect=5 -S -O /dev/null "$_rlt_latest_url" 2>&1 || true)
        _rlt_loc=$(printf '%s\n' "$_rlt_headers" \
            | tr -d '\r' \
            | sed -n 's/^[[:space:]]*[Ll][Oo][Cc][Aa][Tt][Ii][Oo][Nn]:[[:space:]]*//p' \
            | tail -n 1)
        if [ -n "$_rlt_loc" ] && _rlt_tag=$(extract_tag_from_url "$_rlt_loc"); then
            printf '%s\n' "$_rlt_tag"
            return 0
        fi
    fi

    if [ "$_rlt_release_url" = "https://github.com/${_rlt_repo}/releases" ]; then
        _rlt_api_json="${_rlt_tmp}/latest-release.json"
        if download_file "$_rlt_dl" "https://api.github.com/repos/${_rlt_repo}/releases/latest" "$_rlt_api_json" 2>/dev/null; then
            _rlt_api_tag=$(sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$_rlt_api_json" | head -n 1)
            if [ -n "$_rlt_api_tag" ] && is_valid_tag "$_rlt_api_tag"; then
                printf '%s\n' "$_rlt_api_tag"
                return 0
            fi
        fi
    fi

    die "Failed to determine latest release from ${_rlt_latest_url}. Pass --version <VER> (e.g. --version v0.13.0) to install a specific version."
}

detect_target() {
    _dt_os=$(uname -s)
    _dt_arch=$(uname -m)

    case "$_dt_os" in
        Linux) _dt_os_slug="linux" ;;
        Darwin) _dt_os_slug="darwin" ;;
        *)
            die "Unsupported OS '$_dt_os'. Prebuilt binaries support Linux and macOS only; build from source with 'cargo build --release'."
            ;;
    esac

    case "$_dt_arch" in
        x86_64|amd64) _dt_arch_slug="x86_64" ;;
        aarch64|arm64) _dt_arch_slug="aarch64" ;;
        *)
            die "Unsupported architecture '$_dt_arch'. Prebuilt binaries support x86_64 and aarch64/arm64; build from source with 'cargo build --release'."
            ;;
    esac

    if [ "$_dt_os_slug" = "darwin" ] && [ "$_dt_arch_slug" = "x86_64" ]; then
        if command -v sysctl >/dev/null 2>&1; then
            _dt_translated=$(sysctl -n sysctl.proc_translated 2>/dev/null || printf '0')
            if [ "$_dt_translated" = "1" ]; then
                _dt_arch_slug="aarch64"
            fi
        fi
    fi

    case "${_dt_os_slug}:${_dt_arch_slug}" in
        linux:x86_64) printf '%s\n' "x86_64-unknown-linux-musl" ;;
        linux:aarch64) printf '%s\n' "aarch64-unknown-linux-musl" ;;
        darwin:aarch64) printf '%s\n' "aarch64-apple-darwin" ;;
        darwin:x86_64) printf '%s\n' "x86_64-apple-darwin" ;;
        *) die "Unsupported target '${_dt_os_slug}:${_dt_arch_slug}'." ;;
    esac
}

compute_sha256() {
    _cs_file=$1
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$_cs_file" | awk '{ print tolower($1) }'
        return 0
    fi
    if command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$_cs_file" | awk '{ print tolower($1) }'
        return 0
    fi
    if command -v openssl >/dev/null 2>&1; then
        openssl dgst -sha256 "$_cs_file" | awk '{ print tolower($NF) }'
        return 0
    fi
    die "No SHA-256 verification tool found (need sha256sum, shasum, or openssl)."
}

verify_checksum() {
    _vc_archive=$1
    _vc_name=$2
    _vc_sums=$3

    _vc_expected=$(
        awk -v target="$_vc_name" '
            {
                sum = $1
                file = $2
                sub(/\r$/, "", file)
                sub(/^\*/, "", file)
                sub(/^\.\//, "", file)
                if (file == target) {
                    print tolower(sum)
                    exit
                }
            }
        ' "$_vc_sums"
    )

    if [ -z "$_vc_expected" ]; then
        die "Checksum for '${_vc_name}' was not found in SHA256SUMS."
    fi
    if ! printf '%s\n' "$_vc_expected" | grep -Eq '^[0-9a-f]{64}$'; then
        die "Invalid SHA-256 format for '${_vc_name}' in SHA256SUMS."
    fi

    _vc_actual=$(compute_sha256 "$_vc_archive")
    if ! printf '%s\n' "$_vc_actual" | grep -Eq '^[0-9a-f]{64}$'; then
        die "Failed to compute SHA-256 checksum for '${_vc_name}'."
    fi

    if [ "$_vc_expected" != "$_vc_actual" ]; then
        die "SHA-256 verification failed for '${_vc_name}' (expected ${_vc_expected}, got ${_vc_actual}). Existing installation was not modified."
    fi
}

resolve_install_dir() {
    _rid_raw=$1
    if [ -z "$_rid_raw" ]; then
        if [ -z "${HOME:-}" ]; then
            die "HOME is not set; pass --dir <DIR> or set HCT_INSTALL_DIR."
        fi
        _rid_home=$(strip_trailing_slashes "$HOME")
        _rid_raw="${_rid_home}/.local/bin"
    fi
    strip_trailing_slashes "$_rid_raw"
}

resolve_config_dir() {
    _rcd_base=""
    if [ -n "${XDG_CONFIG_HOME:-}" ]; then
        _rcd_base=$(strip_trailing_slashes "$XDG_CONFIG_HOME")
    elif [ -n "${HOME:-}" ]; then
        _rcd_home=$(strip_trailing_slashes "$HOME")
        _rcd_base="${_rcd_home}/.config"
    fi

    if [ -n "$_rcd_base" ]; then
        printf '%s/herdr-tui\n' "${_rcd_base%/}"
    fi
}

run_uninstall() {
    _ru_dir=$1
    _ru_purge=$2
    _ru_bin="${_ru_dir%/}/hct"

    if [ -e "$_ru_bin" ] || [ -L "$_ru_bin" ]; then
        if [ ! -w "$_ru_dir" ]; then
            die "Cannot remove '${_ru_bin}': directory '${_ru_dir}' is not writable."
        fi
        rm -f "$_ru_bin"
        info "Removed ${_ru_bin}"
    else
        info "hct is not installed at ${_ru_bin} (nothing to remove)."
    fi

    hash -r 2>/dev/null || true
    _ru_other=$(command -v hct 2>/dev/null || true)
    if [ -n "$_ru_other" ]; then
        warn "Another 'hct' remains on PATH at '${_ru_other}' (not removed)."
    fi

    _ru_cfg=$(resolve_config_dir)
    if [ -n "$_ru_cfg" ]; then
        if [ "$_ru_purge" = "1" ]; then
            case "$_ru_cfg" in
                */herdr-tui)
                    if [ -d "$_ru_cfg" ] || [ -L "$_ru_cfg" ]; then
                        rm -rf "$_ru_cfg"
                        info "Removed config directory ${_ru_cfg}"
                    else
                        info "Config directory ${_ru_cfg} does not exist (nothing to remove)."
                    fi
                    ;;
                *)
                    die "Refusing to remove unexpected config path '${_ru_cfg}'."
                    ;;
            esac
        elif [ -e "$_ru_cfg" ]; then
            info "Config preserved at ${_ru_cfg} (re-run with --uninstall --purge-config to remove it)."
        fi
    fi

    info "Left herdr-core (hc), active sessions, and ~/.config/herdr untouched."
}

print_post_install_hints() {
    _pih_dir=$1
    _pih_bin="${_pih_dir%/}/hct"

    case ":${PATH:-}:" in
        *":${_pih_dir}:"*)
            _pih_resolved=$(command -v hct 2>/dev/null || true)
            if [ -n "$_pih_resolved" ] && [ "$_pih_resolved" != "$_pih_bin" ]; then
                warn "'hct' currently resolves to '${_pih_resolved}' instead of '${_pih_bin}'. Adjust your PATH order to prefer '${_pih_dir}'."
            fi
            ;;
        *)
            printf '\n'
            printf "Add %s to your PATH so you can run 'hct' directly:\n" "$_pih_dir"
            printf '  # bash / zsh:\n'
            printf "  export PATH=\"%s:\$PATH\"\n" "$_pih_dir"
            printf '  # fish:\n'
            printf '  fish_add_path "%s"\n' "$_pih_dir"
            ;;
    esac

    if ! command -v hc >/dev/null 2>&1 \
        && ! command -v herdr-core >/dev/null 2>&1 \
        && { [ -z "${HOME:-}" ] || [ ! -x "${HOME%/}/.local/bin/herdr-core" ]; }
    then
        printf '\n'
        printf 'Note: hct requires herdr-core (hc) on PATH (or ~/.local/bin/herdr-core).\n'
    fi
}

run_install() {
    _ri_version_raw=$1
    _ri_dir=$2
    _ri_repo=$3
    _ri_release_url=$4

    command -v tar >/dev/null 2>&1 || die "'tar' is required to extract the release archive."
    _ri_dl=$(detect_downloader)
    _ri_target=$(detect_target)

    TMP_DIR=$(create_tmp_dir)

    if [ -n "$_ri_version_raw" ]; then
        _ri_tag=$(normalize_tag "$_ri_version_raw")
    else
        info "Resolving latest hct release..."
        _ri_tag=$(resolve_latest_tag "$_ri_dl" "$_ri_release_url" "$_ri_repo" "$TMP_DIR")
    fi

    _ri_archive="hct-${_ri_tag}-${_ri_target}.tar.gz"
    _ri_base_dl="${_ri_release_url}/download/${_ri_tag}"
    _ri_archive_path="${TMP_DIR}/${_ri_archive}"
    _ri_sums_path="${TMP_DIR}/SHA256SUMS"

    info "Downloading ${_ri_archive}..."
    if ! download_file "$_ri_dl" "${_ri_base_dl}/${_ri_archive}" "$_ri_archive_path"; then
        die "Failed to download '${_ri_base_dl}/${_ri_archive}'."
    fi
    if ! download_file "$_ri_dl" "${_ri_base_dl}/SHA256SUMS" "$_ri_sums_path"; then
        die "Failed to download '${_ri_base_dl}/SHA256SUMS'."
    fi

    info "Verifying SHA-256 checksum..."
    verify_checksum "$_ri_archive_path" "$_ri_archive" "$_ri_sums_path"

    _ri_extract_dir="${TMP_DIR}/extract"
    mkdir -p "$_ri_extract_dir"
    tar -xzf "$_ri_archive_path" -C "$_ri_extract_dir"

    _ri_bin_src="${_ri_extract_dir}/hct-${_ri_tag}-${_ri_target}/hct"
    if [ ! -f "$_ri_bin_src" ]; then
        _ri_bin_src="${_ri_extract_dir}/hct"
    fi
    if [ ! -f "$_ri_bin_src" ]; then
        die "Archive '${_ri_archive}' did not contain an 'hct' binary."
    fi

    _ri_dest="${_ri_dir%/}/hct"
    _ri_prev_ver=""
    if [ -x "$_ri_dest" ]; then
        _ri_prev_ver=$("$_ri_dest" --version 2>/dev/null | head -n 1 || true)
    fi

    mkdir -p "$_ri_dir" || die "Failed to create install directory '${_ri_dir}'."
    [ -d "$_ri_dir" ] || die "Install target '${_ri_dir}' is not a directory."
    [ -w "$_ri_dir" ] || die "Install directory '${_ri_dir}' is not writable."

    STAGED_BIN=$(mktemp "${_ri_dir%/}/.hct.tmp.XXXXXX") || die "Failed to create temporary file in '${_ri_dir}'."
    cp "$_ri_bin_src" "$STAGED_BIN"
    chmod 755 "$STAGED_BIN"
    mv -f "$STAGED_BIN" "$_ri_dest"
    STAGED_BIN=""

    _ri_new_ver=$("$_ri_dest" --version 2>/dev/null | head -n 1 || true)
    if [ -z "$_ri_new_ver" ]; then
        die "Installed '${_ri_dest}', but running '${_ri_dest} --version' failed."
    fi

    if [ -z "$_ri_prev_ver" ]; then
        info "Installed ${_ri_new_ver} to ${_ri_dest}"
    elif [ "$_ri_prev_ver" = "$_ri_new_ver" ]; then
        info "Reinstalled ${_ri_new_ver} at ${_ri_dest}"
    else
        info "Updated ${_ri_prev_ver} -> ${_ri_new_ver} at ${_ri_dest}"
    fi

    print_post_install_hints "$_ri_dir"
}

main() {
    trap cleanup EXIT INT TERM

    _m_version=${HCT_VERSION:-}
    _m_dir=${HCT_INSTALL_DIR:-}
    _m_repo=${HCT_REPO:-$DEFAULT_REPO}
    _m_release_url=${HCT_RELEASE_URL:-}
    _m_uninstall=0
    _m_purge_config=0

    while [ $# -gt 0 ]; do
        case "$1" in
            -h|--help)
                usage
                exit 0
                ;;
            -v|--version)
                [ $# -ge 2 ] || die "Option '$1' requires a version argument."
                [ -n "$2" ] || die "Option '$1' requires a non-empty version."
                _m_version=$2
                shift 2
                ;;
            --version=*)
                _m_version=${1#*=}
                [ -n "$_m_version" ] || die "Option '--version' requires a non-empty version."
                shift
                ;;
            -d|--dir)
                [ $# -ge 2 ] || die "Option '$1' requires a directory argument."
                [ -n "$2" ] || die "Option '$1' requires a non-empty directory."
                _m_dir=$2
                shift 2
                ;;
            --dir=*)
                _m_dir=${1#*=}
                [ -n "$_m_dir" ] || die "Option '--dir' requires a non-empty directory."
                shift
                ;;
            --uninstall)
                _m_uninstall=1
                shift
                ;;
            --purge-config)
                _m_purge_config=1
                shift
                ;;
            --)
                shift
                if [ $# -gt 0 ]; then
                    die "Unexpected positional argument: $1"
                fi
                ;;
            -*)
                die "Unknown option: $1 (run with --help for usage)"
                ;;
            *)
                die "Unexpected positional argument: $1 (run with --help for usage)"
                ;;
        esac
    done

    if [ "$_m_purge_config" = "1" ] && [ "$_m_uninstall" = "0" ]; then
        die "--purge-config can only be used together with --uninstall."
    fi

    if ! validate_repo "$_m_repo"; then
        die "Invalid HCT_REPO '${_m_repo}'. Expected '<owner>/<repo>'."
    fi

    if [ -z "$_m_release_url" ]; then
        _m_release_url="https://github.com/${_m_repo}/releases"
    fi
    _m_release_url=$(strip_trailing_slashes "$_m_release_url")
    case "$_m_release_url" in
        http://*|https://*) ;;
        *) die "Invalid HCT_RELEASE_URL '${_m_release_url}'. Must start with http:// or https://." ;;
    esac

    _m_install_dir=$(resolve_install_dir "$_m_dir")

    if [ "$_m_uninstall" = "1" ]; then
        run_uninstall "$_m_install_dir" "$_m_purge_config"
    else
        run_install "$_m_version" "$_m_install_dir" "$_m_repo" "$_m_release_url"
    fi
}

main "$@"
