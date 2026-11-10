#!/bin/sh
# Build homology-runner for SSH hosts: static Linux programs (musl) for
# x86_64 and aarch64, linked by the rust-lld Rust ships (.cargo/config.toml),
# so Linux, macOS and Windows (Git Bash) build them without a cross
# toolchain. Each is written to <out>/homology-runner-linux-<arch>, the name
# `homology::bundle` reads, without the executable bit: a program carries
# them as data and makes one executable only on the host it sends it to.
# Their SHA-256 are printed (and written to <out>/SHA256SUMS).
#
# usage: scripts/build-runners.sh [--out <dir>]     (default: target/runners)
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
out="$root/target/runners"
while [ $# -gt 0 ]; do
    case "$1" in
        --out) [ $# -ge 2 ] || { echo "usage: $0 [--out <dir>]" >&2; exit 64; }; out=$2; shift 2 ;;
        -h|--help) echo "usage: $0 [--out <dir>]"; exit 0 ;;
        *) echo "usage: $0 [--out <dir>]" >&2; exit 64 ;;
    esac
done
mkdir -p "$out"
target_dir=${CARGO_TARGET_DIR:-"$root/target"}

sha256() {
    if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1
    else shasum -a 256 "$1" | cut -d' ' -f1
    fi
}

sums=""
for arch in x86_64 aarch64; do
    target="$arch-unknown-linux-musl"
    # The standard library for the target, for the toolchain this runs with.
    if command -v rustup >/dev/null 2>&1; then
        rustup target add "$target" >/dev/null
    fi
    (cd "$root" && cargo build --locked --release -p homology-runner --bin homology-runner \
        --target "$target" --target-dir "$target_dir")
    to="$out/homology-runner-linux-$arch"
    cp "$target_dir/$target/release/homology-runner" "$to"
    chmod 0644 "$to"
    line="$(sha256 "$to")  homology-runner-linux-$arch"
    echo "$line"
    sums="$sums$line
"
done
printf '%s' "$sums" > "$out/SHA256SUMS"
