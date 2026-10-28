#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
archs="${ARCHS:-$(uname -m)}"
target_dir="${repo_root}/target"
output_dir="${target_dir}/universal/release"

if ! command -v cargo >/dev/null 2>&1 || ! command -v rustup >/dev/null 2>&1; then
    echo "error: cargo and rustup are required to build ext4-ffi" >&2
    exit 1
fi

triples=()
for arch in ${archs}; do
    case "${arch}" in
        arm64) triple="aarch64-apple-darwin" ;;
        x86_64) triple="x86_64-apple-darwin" ;;
        *)
            echo "error: unsupported Xcode architecture '${arch}' for ext4-ffi" >&2
            exit 1
            ;;
    esac
    if ! rustup target list --installed | command grep -Fxq "${triple}"; then
        echo "error: Rust target '${triple}' is not installed. Install it before building with: rustup target add ${triple}" >&2
        exit 1
    fi
    triples+=("${triple}")
done

for triple in "${triples[@]}"; do
    echo "Building ext4-ffi for ${triple}"
    CARGO_TARGET_DIR="${target_dir}" cargo build --release -p ext4-ffi --target "${triple}" --manifest-path "${repo_root}/Cargo.toml"
done

mkdir -p "${output_dir}"
inputs=()
for triple in "${triples[@]}"; do
    library="${target_dir}/${triple}/release/libext4fs.a"
    if [[ ! -f "${library}" ]]; then
        echo "error: cargo did not produce ${library}" >&2
        exit 1
    fi
    inputs+=("${library}")
done

if [[ ${#inputs[@]} -eq 1 ]]; then
    cp "${inputs[0]}" "${output_dir}/libext4fs.a"
else
    /usr/bin/lipo -create "${inputs[@]}" -output "${output_dir}/libext4fs.a"
fi
