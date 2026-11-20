#!/bin/sh
set -eu

REPO_ROOT=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
INSTALL_SH="${REPO_ROOT}/install.sh"
TEST_ROOT=""
SERVER_PID=""

cleanup() {
    if [ -n "$SERVER_PID" ]; then
        kill "$SERVER_PID" 2>/dev/null || true
        wait "$SERVER_PID" 2>/dev/null || true
    fi
    if [ -n "$TEST_ROOT" ] && [ -d "$TEST_ROOT" ]; then
        rm -rf "$TEST_ROOT"
    fi
}

fail() {
    printf 'FAIL: %s\n' "$1" >&2
    exit 1
}

assert_eq() {
    _ae_got=$1
    _ae_want=$2
    _ae_msg=$3
    if [ "$_ae_got" != "$_ae_want" ]; then
        fail "${_ae_msg}: expected '${_ae_want}', got '${_ae_got}'"
    fi
}

assert_contains() {
    _ac_haystack=$1
    _ac_needle=$2
    _ac_msg=$3
    case "$_ac_haystack" in
        *"$_ac_needle"*) ;;
        *) fail "${_ac_msg}: expected output to contain '${_ac_needle}', got:\n${_ac_haystack}" ;;
    esac
}

detect_host_target() {
    _dht_os=$(uname -s)
    _dht_arch=$(uname -m)
    case "$_dht_os" in
        Linux) _dht_os_slug="linux" ;;
        Darwin) _dht_os_slug="darwin" ;;
        *) fail "Unsupported test OS '$_dht_os'" ;;
    esac
    case "$_dht_arch" in
        x86_64|amd64) _dht_arch_slug="x86_64" ;;
        aarch64|arm64) _dht_arch_slug="aarch64" ;;
        *) fail "Unsupported test arch '$_dht_arch'" ;;
    esac
    if [ "$_dht_os_slug" = "darwin" ] && [ "$_dht_arch_slug" = "x86_64" ]; then
        if command -v sysctl >/dev/null 2>&1; then
            if [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || printf '0')" = "1" ]; then
                _dht_arch_slug="aarch64"
            fi
        fi
    fi
    case "${_dht_os_slug}:${_dht_arch_slug}" in
        linux:x86_64) printf '%s\n' "x86_64-unknown-linux-musl" ;;
        linux:aarch64) printf '%s\n' "aarch64-unknown-linux-musl" ;;
        darwin:aarch64) printf '%s\n' "aarch64-apple-darwin" ;;
        darwin:x86_64) printf '%s\n' "x86_64-apple-darwin" ;;
        *) fail "Unsupported target '${_dht_os_slug}:${_dht_arch_slug}'" ;;
    esac
}

run_installer() {
    HOME="$TEST_HOME" \
    XDG_CONFIG_HOME="$TEST_XDG" \
    HCT_INSTALL_DIR="$TEST_BIN" \
    HCT_RELEASE_URL="http://127.0.0.1:${PORT}/releases" \
    sh "$INSTALL_SH" "$@"
}

main() {
    trap cleanup EXIT INT TERM

    command -v python3 >/dev/null 2>&1 || fail "python3 is required to run install_script_test.sh"

    if TEST_ROOT=$(mktemp -d 2>/dev/null); then
        :
    else
        TEST_ROOT=$(mktemp -d -t hct-install-test)
    fi

    HOST_TARGET=$(detect_host_target)
    TEST_HOME="${TEST_ROOT}/home"
    TEST_XDG="${TEST_HOME}/.config"
    TEST_BIN="${TEST_HOME}/.local/bin"
    WWW_ROOT="${TEST_ROOT}/www"
    mkdir -p "$TEST_HOME" "$TEST_XDG" "$WWW_ROOT"

    cat <<'PYEOF' > "${TEST_ROOT}/serve.py"
import hashlib
import http.server
import io
import os
import posixpath
import socketserver
import sys
import tarfile
import urllib.parse

target = sys.argv[1]
www_root = sys.argv[2]
fifo_path = sys.argv[3]


def make_release(tag, ver_str, corrupt_sum=False):
    rel_dir = os.path.join(www_root, "releases", "download", tag)
    os.makedirs(rel_dir, exist_ok=True)
    stem = f"hct-{tag}-{target}"
    archive_name = f"{stem}.tar.gz"
    archive_path = os.path.join(rel_dir, archive_name)

    script_bytes = f"#!/bin/sh\necho 'hct {ver_str}'\n".encode("utf-8")
    with tarfile.open(archive_path, "w:gz") as tf:
        info = tarfile.TarInfo(name=f"{stem}/hct")
        info.size = len(script_bytes)
        info.mode = 0o755
        tf.addfile(info, io.BytesIO(script_bytes))

        for doc_name, body in (("README.md", b"# hct\n"), ("LICENSE", b"Apache-2.0\n")):
            dinfo = tarfile.TarInfo(name=f"{stem}/{doc_name}")
            dinfo.size = len(body)
            dinfo.mode = 0o644
            tf.addfile(dinfo, io.BytesIO(body))

    with open(archive_path, "rb") as f:
        digest = hashlib.sha256(f.read()).hexdigest()
    if corrupt_sum:
        digest = "0" * 64

    with open(os.path.join(rel_dir, "SHA256SUMS"), "w", encoding="utf-8") as f:
        f.write(f"{digest}  {archive_name}\n")


make_release("v9.9.9", "9.9.9")
make_release("v9.9.8", "9.9.8")
make_release("v9.9.7", "9.9.7", corrupt_sum=True)


class ThreadingHTTPServer(socketserver.ThreadingMixIn, http.server.HTTPServer):
    daemon_threads = True


class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, format, *args):
        return

    def do_GET(self):
        parsed = urllib.parse.urlparse(self.path)
        path = parsed.path
        port = self.server.server_address[1]

        if path == "/releases/latest":
            self.send_response(302)
            self.send_header("Location", f"http://127.0.0.1:{port}/releases/tag/v9.9.9")
            self.end_headers()
            return

        if path == "/releases/tag/v9.9.9":
            body = b"v9.9.9\n"
            self.send_response(200)
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return

        norm = posixpath.normpath(path)
        if not norm.startswith("/releases/download/"):
            self.send_error(404)
            return

        rel = norm.lstrip("/")
        full = os.path.realpath(os.path.join(www_root, rel))
        root_real = os.path.realpath(www_root)
        if not full.startswith(root_real + os.sep) or not os.path.isfile(full):
            self.send_error(404)
            return

        with open(full, "rb") as f:
            data = f.read()
        self.send_response(200)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)


try:
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
except Exception:
    with open(fifo_path, "w", encoding="utf-8") as fifo:
        fifo.write("ERR\n")
    raise

with open(fifo_path, "w", encoding="utf-8") as fifo:
    fifo.write(f"{server.server_address[1]}\n")

server.serve_forever()
PYEOF

    FIFO_PATH="${TEST_ROOT}/port.fifo"
    mkfifo "$FIFO_PATH"
    python3 "${TEST_ROOT}/serve.py" "$HOST_TARGET" "$WWW_ROOT" "$FIFO_PATH" >/dev/null 2>&1 &
    SERVER_PID=$!
    read -r PORT < "$FIFO_PATH"
    if [ -z "$PORT" ] || [ "$PORT" = "ERR" ]; then
        fail "Failed to start local test HTTP server"
    fi

    # Test 1: --help
    help_out=$(sh "$INSTALL_SH" --help)
    assert_contains "$help_out" "--uninstall" "help output"
    assert_contains "$help_out" "--purge-config" "help output"

    # Test 2: Fresh install (latest via 302 redirect)
    run_installer >/dev/null
    [ -x "${TEST_BIN}/hct" ] || fail "hct binary not installed at ${TEST_BIN}/hct"
    assert_eq "$("${TEST_BIN}/hct" --version)" "hct 9.9.9" "fresh install version"

    # Test 3: Pinned version without 'v', then update back to latest
    run_installer --version 9.9.8 >/dev/null
    assert_eq "$("${TEST_BIN}/hct" --version)" "hct 9.9.8" "pinned install version"

    update_out=$(run_installer)
    assert_eq "$("${TEST_BIN}/hct" --version)" "hct 9.9.9" "updated version"
    assert_contains "$update_out" "Updated hct 9.9.8 -> hct 9.9.9" "update message"

    # Test 4: Piped install & piped uninstall
    HOME="$TEST_HOME" \
    XDG_CONFIG_HOME="$TEST_XDG" \
    HCT_INSTALL_DIR="$TEST_BIN" \
    HCT_RELEASE_URL="http://127.0.0.1:${PORT}/releases" \
    sh < "$INSTALL_SH" >/dev/null
    assert_eq "$("${TEST_BIN}/hct" --version)" "hct 9.9.9" "piped install version"

    HOME="$TEST_HOME" \
    XDG_CONFIG_HOME="$TEST_XDG" \
    HCT_INSTALL_DIR="$TEST_BIN" \
    HCT_RELEASE_URL="http://127.0.0.1:${PORT}/releases" \
    sh -s -- --uninstall < "$INSTALL_SH" >/dev/null
    [ ! -e "${TEST_BIN}/hct" ] || fail "piped uninstall did not remove hct"

    # Test 5: Checksum mismatch fails without clobbering existing binary
    run_installer >/dev/null
    if run_installer --version v9.9.7 >/dev/null 2>&1; then
        fail "checksum mismatch should have failed"
    fi
    assert_eq "$("${TEST_BIN}/hct" --version)" "hct 9.9.9" "existing binary after checksum mismatch"
    if find "$TEST_BIN" -name '.hct.tmp.*' | grep -q .; then
        fail "leftover staged temp file found in ${TEST_BIN}"
    fi

    # Test 6: Uninstall preserves config and is idempotent
    mkdir -p "${TEST_XDG}/herdr-tui" "${TEST_XDG}/herdr"
    printf 'eink = true\n' > "${TEST_XDG}/herdr-tui/config.toml"
    printf 'theme = "dark"\n' > "${TEST_XDG}/herdr/config.toml"

    uninstall_out=$(run_installer --uninstall)
    [ ! -e "${TEST_BIN}/hct" ] || fail "uninstall did not remove binary"
    [ -f "${TEST_XDG}/herdr-tui/config.toml" ] || fail "uninstall deleted herdr-tui config"
    [ -f "${TEST_XDG}/herdr/config.toml" ] || fail "uninstall deleted herdr config"
    assert_contains "$uninstall_out" "Config preserved" "uninstall output"

    idempotent_out=$(run_installer --uninstall)
    assert_contains "$idempotent_out" "nothing to remove" "idempotent uninstall output"

    # Test 7: --uninstall --purge-config removes herdr-tui config only
    run_installer >/dev/null
    run_installer --uninstall --purge-config >/dev/null
    [ ! -e "${TEST_BIN}/hct" ] || fail "purge uninstall did not remove binary"
    [ ! -e "${TEST_XDG}/herdr-tui" ] || fail "purge uninstall did not remove herdr-tui config dir"
    [ -f "${TEST_XDG}/herdr/config.toml" ] || fail "purge uninstall must never delete ~/.config/herdr"

    # Test 8: Invalid argument rejections
    if run_installer --purge-config >/dev/null 2>&1; then
        fail "--purge-config without --uninstall should fail"
    fi
    if run_installer --bogus >/dev/null 2>&1; then
        fail "unknown flag should fail"
    fi
    if run_installer --version ../evil >/dev/null 2>&1; then
        fail "invalid version should fail"
    fi
    if run_installer --dir "" >/dev/null 2>&1; then
        fail "empty --dir should fail"
    fi
    if run_installer --version "" >/dev/null 2>&1; then
        fail "empty --version should fail"
    fi

    printf 'All install_script_test.sh checks passed (%s).\n' "$HOST_TARGET"
}

main "$@"
