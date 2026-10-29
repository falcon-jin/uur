#!/usr/bin/env bash
set -euo pipefail

project_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
version=$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$project_root/Cargo.toml" | head -n1)
output=${1:-$project_root/dist}
mkdir -p "$output"
output=$(cd -- "$output" && pwd -P)
readonly output
build_dir=$(mktemp -d "${TMPDIR:-/tmp}/uur-makepkg.XXXXXX")
trap 'rm -rf -- "$build_dir"' EXIT

command -v makepkg >/dev/null 2>&1 || {
    printf 'makepkg is required to build the Arch package\n' >&2
    exit 2
}

cargo build --manifest-path "$project_root/Cargo.toml" --release --locked
"$project_root/hook/build.sh"
"$project_root/capture/build.sh"

sed -e "s|@UUR_VERSION@|$version|g" \
    -e "s|@UUR_PROJECT_ROOT@|$project_root|g" \
    "$project_root/packaging/arch/PKGBUILD.in" >"$build_dir/PKGBUILD"
cp "$project_root/packaging/aur/uur.install" "$build_dir/uur.install"

(
    cd "$build_dir"
    PKGDEST="$output" makepkg --force --nodeps --noconfirm
)
