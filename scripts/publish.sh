#!/usr/bin/env bash
# Publish the workspace crates to crates.io in the order listed in scripts/publish-order.txt.
#
#   scripts/publish.sh                 # same as --dry-run
#   scripts/publish.sh --dry-run       # package and verify every crate; uploads nothing
#   scripts/publish.sh --execute       # publish for real (needs CARGO_REGISTRY_TOKEN)
#
# Options: --allow-dirty   pass --allow-dirty to cargo (local dry runs on an uncommitted tree)
#
# A dry run packages all crates in ONE cargo invocation (`cargo publish --dry-run -p a -p b ...`):
# cargo then verifies each crate against its not-yet-published workspace dependencies. Running
# `cargo publish --dry-run -p <crate>` one crate at a time fails for every crate whose
# dependencies are not on crates.io yet, which is expected before the first release.
# A real publish goes crate by crate, skips versions that are already on crates.io (so a failed
# release can be re-run) and waits for each one to appear in the index before the next.

set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
order_file="$root/scripts/publish-order.txt"
mode="dry-run"
extra=()

for arg in "$@"; do
  case "$arg" in
    --dry-run) mode="dry-run" ;;
    --execute) mode="execute" ;;
    --allow-dirty) extra+=("--allow-dirty") ;;
    -h | --help) sed -n '2,17p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "publish.sh: unknown argument '$arg'" >&2; exit 2 ;;
  esac
done

crates=()
while IFS= read -r line; do
  line="${line%%#*}"
  line="$(echo "$line" | tr -d '[:space:]')"
  [ -n "$line" ] && crates+=("$line")
done <"$order_file"

if [ "${#crates[@]}" -eq 0 ]; then
  echo "publish.sh: $order_file lists no crates" >&2
  exit 1
fi

cd "$root"

# Every publishable workspace member must be listed, and nothing else.
if command -v jq >/dev/null 2>&1; then
  publishable="$(cargo metadata --no-deps --format-version 1 --locked |
    jq -r '.packages[] | select(.publish == null or (.publish | length) > 0) | .name' | sort)"
  listed="$(printf '%s\n' "${crates[@]}" | sort)"
  if [ "$publishable" != "$listed" ]; then
    echo "publish.sh: $order_file does not match the publishable workspace crates" >&2
    diff <(echo "$publishable") <(echo "$listed") >&2 || true
    exit 1
  fi
else
  echo "publish.sh: jq not found, skipping the order-file consistency check" >&2
fi

version="$(sed -n '/^\[workspace.package\]/,/^\[/{s/^version *= *"\(.*\)"/\1/p}' Cargo.toml | head -n 1)"

if [ "$mode" = "dry-run" ]; then
  args=()
  for crate in "${crates[@]}"; do args+=("-p" "$crate"); done
  echo "dry run: ${crates[*]}"
  cargo publish --dry-run --locked "${args[@]}" ${extra[@]+"${extra[@]}"}
  exit 0
fi

is_published() { # crate, version
  [ "$(curl -s -o /dev/null -w '%{http_code}' -A 'xstate-statemachine-release' \
    "https://crates.io/api/v1/crates/$1/$2")" = "200" ]
}

needed=()
for crate in "${crates[@]}"; do
  if is_published "$crate" "$version"; then
    echo "$crate $version is already on crates.io, skipping"
  else
    needed+=("$crate")
  fi
done

if [ "${#needed[@]}" -eq 0 ]; then
  echo "All ${#crates[@]} crates are already published on crates.io at $version."
  exit 0
fi

if [ -z "${CARGO_REGISTRY_TOKEN:-}" ]; then
  echo "publish.sh: CARGO_REGISTRY_TOKEN must be set for --execute to publish: ${needed[*]}" >&2
  exit 1
fi

for crate in "${needed[@]}"; do
  echo "publishing $crate $version"
  cargo publish --locked -p "$crate" ${extra[@]+"${extra[@]}"}
  for _ in $(seq 1 30); do
    is_published "$crate" "$version" && break
    sleep 10
  done
  if ! is_published "$crate" "$version"; then
    echo "publish.sh: $crate $version did not appear on crates.io in 5 minutes" >&2
    exit 1
  fi
done
echo "published ${#needed[@]} crates at $version"
