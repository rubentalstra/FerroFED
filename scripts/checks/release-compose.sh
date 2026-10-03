#!/usr/bin/env bash
# SPDX-FileCopyrightText: Vernum Projecten B.V.
# SPDX-License-Identifier: BUSL-1.1
# The release compose guard (no specification governs this: our own design).
# Every release attaches deploy/compose/compose.yaml, ferrofed.toml and
# registry.toml under those names. This guard checks, over copies laid out as
# a downloader has them:
#
#   1. no compose file in the repository carries `build:`, because every one
#      runs the published image and only release-image.yml builds it;
#   2. Docker Compose renders the release compose file;
#   3. given a static Linux ferrofed binary, `ferrofed config check` accepts
#      the example ferrofed.toml and registry.toml exactly as attached, mounted
#      at the paths the rendered compose file mounts them, in the pinned base
#      image of docker/Dockerfile, with a synthetic file for each credential
#      the example names and a synthetic ES384 key for each signing key; and
#      it refuses the same configuration once a member is left out of the
#      PIX Manager, naming that member, and once the PIX Manager's credential
#      would travel over plain http, naming its URL key.
#
# Usage:
#   scripts/checks/release-compose.sh                  checks 1 and 2
#   scripts/checks/release-compose.sh <ferrofed binary> all three; the binary
#                                                      is a static Linux
#                                                      build for this host's
#                                                      architecture
# Needs `docker compose`, `jq` and `openssl`. Exit 1 naming each failure; 0
# otherwise.
set -euo pipefail
cd "$(dirname "$0")/../.."

readonly RELEASE=deploy/compose
readonly ASSETS="compose.yaml ferrofed.toml registry.toml"

case "$#" in
  0) binary="" ;;
  1) binary="$1" ;;
  *)
    sed -n '/^# Usage:/,/^# Needs/p' "$0" >&2
    exit 2
    ;;
esac
if [ -n "$binary" ] && [ ! -x "$binary" ]; then
  echo "::error::$binary is not an executable ferrofed binary." >&2
  exit 2
fi

fail=0
bad() {
  echo "::error::$1" >&2
  fail=1
}

echo "== no compose file builds the image"
built=0
while IFS= read -r file; do
  [ -n "$file" ] || continue
  if grep -n -E '^[[:space:]]*build:' "$file" >&2; then
    bad "$file carries build:; every compose file runs the published image, which only release-image.yml builds"
    built=1
  fi
done < <(git ls-files -- '*compose*.yaml' '*compose*.yml' ':(exclude)docs/specs/**' ':(glob,exclude)**/vendor/**')
[ "$built" -eq 0 ] && echo "OK: no compose file carries build:"

docker compose version

# The downloader's directory: the three assets, byte for byte, and secrets/.
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
for asset in $ASSETS; do
  cp "$RELEASE/$asset" "$work/$asset"
done
mkdir "$work/secrets"
# A synthetic value for every credential file the example names.
while IFS= read -r secret; do
  printf 'synthetic-%s\n' "$secret" > "$work/secrets/$secret"
done < <(sed -nE 's|^[a-z_]+_file[[:space:]]*=[[:space:]]*"/run/secrets/ferrofed/([^"]+)"[[:space:]]*$|\1|p' "$work/ferrofed.toml")
# A signing key is read as one, so each key_file holds a synthetic ES384 key.
while IFS= read -r key; do
  openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-384 \
    -out "$work/secrets/$key" 2> /dev/null ||
    bad "a synthetic signing key could not be generated for $key"
done < <(sed -nE 's|^key_file[[:space:]]*=[[:space:]]*"/run/secrets/ferrofed/([^"]+)"[[:space:]]*$|\1|p' "$work/ferrofed.toml")
chmod -R a+rX "$work"

echo "== the release compose file renders"
if rendered="$(docker compose --project-directory "$work" -f "$work/compose.yaml" config --format json)"; then
  echo "OK: $RELEASE/compose.yaml renders"
else
  bad "$RELEASE/compose.yaml does not render"
  rendered=""
fi

if [ -z "$binary" ]; then
  echo "no ferrofed binary given: config check skipped"
elif [ -n "$rendered" ]; then
  echo "== config check over the attached examples"
  base="$(sed -nE 's|^FROM[[:space:]]+([^[:space:]]+).*|\1|p' docker/Dockerfile | head -n1)"
  config="$(jq -r '.services.ferrofed.environment.FERROFED_CONFIG // empty' <<< "$rendered")"
  mounts=()
  while IFS= read -r mount; do
    mounts+=(--volume "$mount")
  done < <(jq -r '.services.ferrofed.volumes[] | select(.type == "bind") | "\(.source):\(.target):ro"' <<< "$rendered")
  # check: runs config check as the image does, read-only and unprivileged.
  check() {
    docker run --rm --read-only --user 65532:65532 --cap-drop ALL \
      --security-opt no-new-privileges:true --network none \
      --env FERROFED_CONFIG="$config" \
      --volume "$(cd "$(dirname "$binary")" && pwd)/$(basename "$binary"):/usr/local/bin/ferrofed:ro" \
      "${mounts[@]}" --entrypoint /usr/local/bin/ferrofed "$base" config check
  }
  docker pull --quiet "$base" > /dev/null
  if [ -z "$config" ] || [ "${#mounts[@]}" -eq 0 ]; then
    bad "the rendered compose file names no FERROFED_CONFIG or no bind mount"
  elif out="$(check 2>&1)"; then
    echo "OK: $out"
  else
    bad "ferrofed config check refuses the attached examples: $out"
  fi
  # The check has teeth: a member the PIX Manager does not resolve is refused.
  # Rewritten in place, so the mounted file keeps its inode.
  sed '/^"node-b" = /d' "$work/ferrofed.toml" > "$work/refused.toml"
  cat "$work/refused.toml" > "$work/ferrofed.toml"
  if out="$(check 2>&1)"; then
    bad "ferrofed config check accepts a PIX Manager that resolves only one of two members"
  elif ! grep -q 'node-b' <<< "$out"; then
    bad "ferrofed config check refuses a missing member without naming it: $out"
  else
    echo "OK: a member no PIX Manager resolves is refused by name"
  fi
  # A credential sent over plain http is refused under the production profile.
  sed -E 's|^url = "https://(pix\.[^"]*)"$|url = "http://\1"|' "$RELEASE/ferrofed.toml" > "$work/refused.toml"
  cat "$work/refused.toml" > "$work/ferrofed.toml"
  if cmp -s "$RELEASE/ferrofed.toml" "$work/ferrofed.toml"; then
    bad "the example names no https PIX Manager URL to rewrite"
  elif out="$(check 2>&1)"; then
    bad "ferrofed config check accepts a PIX Manager credential sent over plain http"
  elif ! grep -qF 'pixm.manager[0].url' <<< "$out"; then
    bad "ferrofed config check refuses a cleartext credential without naming its key: $out"
  else
    echo "OK: a credential sent over plain http is refused by key"
  fi
fi

if [ "$fail" -ne 0 ]; then
  echo "release compose: FAILED" >&2
  exit 1
fi
echo "release compose: OK"
