#!/bin/sh
# Build one of the supported flavors into dist/.
#
#   ./build.sh glibc          GUI, dynamic glibc (host toolchain)            -> dist/SystemMonitor
#   ./build.sh glibc-compat   same, built in a Debian 11 container (glibc 2.31) -> dist/SystemMonitor-glibc-compat
#   ./build.sh musl           GUI, dynamic musl (for Alpine/Void-musl): native on a musl host,
#                             otherwise inside an Alpine container (podman/docker)   -> dist/SystemMonitor-musl
#   ./build.sh core           no GUI, fully static musl executable            -> dist/SystemMonitor-core-static
#   ./build.sh all            every flavor that can be built on this machine
#   ./build.sh test           unit tests for the GUI and the core build
#
# There is deliberately no "static GUI" flavor: see README ("Static linking").
set -eu
cd "$(dirname "$0")"
mkdir -p dist

arch=$(uname -m)
case "$arch" in
    x86_64|aarch64) musl_target="$arch-unknown-linux-musl" ;;
    *) musl_target="" ;;
esac
container=$(command -v podman || command -v docker || true)

is_musl_host() { (ldd --version 2>&1 || true) | grep -qi musl; }

finish() { chmod +x "$1"; ls -l "$1"; }

build_glibc() {
    cargo build --release --locked
    cp target/release/SystemMonitor dist/SystemMonitor
    finish dist/SystemMonitor
    echo "needs glibc >= $(objdump -T dist/SystemMonitor 2>/dev/null | grep -o 'GLIBC_[0-9.]*' | sort -uV | tail -1 | cut -d_ -f2)"
}

in_container() { # image, script
    [ -n "$container" ] || { echo "error: this flavor needs podman or docker" >&2; exit 1; }
    "$container" run --rm --security-opt label=disable -v "$PWD":/src -v "${CARGO_HOME:-$HOME/.cargo}/registry":/usr/local/cargo/registry -w /src "$1" sh -c "$2"
}

build_glibc_compat() {
    # Linking against an old glibc is what lowers the requirement; the code is identical.
    in_container docker.io/library/rust:bullseye \
        'CARGO_TARGET_DIR=/src/target-compat cargo build --release --locked && chown -R '"$(id -u):$(id -g)"' /src/target-compat 2>/dev/null || true'
    cp target-compat/release/SystemMonitor dist/SystemMonitor-glibc-compat
    finish dist/SystemMonitor-glibc-compat
    echo "needs glibc >= $(objdump -T dist/SystemMonitor-glibc-compat | grep -o 'GLIBC_[0-9.]*' | sort -uV | tail -1 | cut -d_ -f2)"
}

build_musl() {
    if is_musl_host; then
        RUSTFLAGS="${RUSTFLAGS:-} -C target-feature=-crt-static" cargo build --release --locked --target-dir target-musl
    else
        in_container docker.io/library/rust:alpine \
            'RUSTFLAGS="-C target-feature=-crt-static" CARGO_TARGET_DIR=/src/target-musl cargo build --release --locked'
    fi
    cp target-musl/release/SystemMonitor dist/SystemMonitor-musl
    finish dist/SystemMonitor-musl
}

build_core() {
    [ -n "$musl_target" ] || { echo "error: no musl target for $arch" >&2; exit 1; }
    rustup target add "$musl_target" >/dev/null 2>&1 || true
    cargo build --release --locked --no-default-features --target "$musl_target" --target-dir target-core
    cp "target-core/$musl_target/release/SystemMonitor" dist/SystemMonitor-core-static
    finish dist/SystemMonitor-core-static
    file dist/SystemMonitor-core-static 2>/dev/null || true
}

run_tests() {
    cargo test --locked --test-threads=1 2>/dev/null || cargo test --locked -- --test-threads=1
    cargo test --locked --no-default-features -- --test-threads=1
}

case "${1:-glibc}" in
    glibc) build_glibc ;;
    glibc-compat) build_glibc_compat ;;
    musl) build_musl ;;
    core) build_core ;;
    test) run_tests ;;
    all)
        build_glibc
        build_core
        [ -n "$container" ] && { build_glibc_compat; build_musl; } || echo "skipping glibc-compat and musl: no podman/docker"
        ;;
    *) sed -n '2,14p' "$0"; exit 2 ;;
esac
