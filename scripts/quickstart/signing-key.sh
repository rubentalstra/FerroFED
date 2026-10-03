#!/usr/bin/env bash
# SPDX-FileCopyrightText: Vernum Projecten B.V.
# SPDX-License-Identifier: BUSL-1.1
# Writes the compose quickstart gateway's ES384 signing key, once, before the
# first `docker compose up` (no specification governs the quickstart key: our
# own design).
#
# The gateway signs every request to a node with this key: the caller's
# identity travels in an openEHR-federation-client token, and the gateway
# publishes the public half at /.well-known/jwks.json (Federation Tier §13.1,
# N24, N25). docker/quickstart/ferrofed.toml names the key, and compose.yaml
# mounts docker/quickstart/signing/ read-only, which git ignores, so no key
# is ever committed. A development key, NOT for anything real.
#
# The file is readable by every user: the container runs the gateway as uid
# 65532, which a key readable by its owner alone would refuse. A key that
# exists is kept.
#
# Usage, from anywhere, before `docker compose up --wait`:
#   scripts/quickstart/signing-key.sh
# Needs openssl. Exit 1 when the key cannot be made.
set -euo pipefail
cd "$(dirname "$0")/../.."

readonly KEY=docker/quickstart/signing/signing-key.pem

die() {
  echo "signing-key: $*" >&2
  exit 1
}

if [ -s "$KEY" ]; then
  echo "signing-key: $KEY exists, kept" >&2
  exit 0
fi
command -v openssl >/dev/null || die "openssl is not installed"
openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-384 -out "$KEY" 2>/dev/null ||
  die "the key could not be generated"
chmod 644 "$KEY"
echo "signing-key: generated the development signing key in $KEY" >&2
