<!-- SPDX-FileCopyrightText: Vernum Projecten B.V. -->
<!-- SPDX-License-Identifier: BUSL-1.1 -->
# <img src="https://raw.githubusercontent.com/FerroHEALTH/FerroFED/main/assets/brand/ferrofed-icon.svg" alt="" width="40" height="40" align="top"> FerroFED

<!-- badges:begin -->
[![CI](https://github.com/FerroHEALTH/FerroFED/actions/workflows/ci.yml/badge.svg)](https://github.com/FerroHEALTH/FerroFED/actions/workflows/ci.yml)
[![CodeQL](https://github.com/FerroHEALTH/FerroFED/actions/workflows/codeql.yml/badge.svg)](https://github.com/FerroHEALTH/FerroFED/actions/workflows/codeql.yml)
[![OpenSSF Scorecard](https://api.securityscorecards.dev/projects/github.com/FerroHEALTH/FerroFED/badge)](https://scorecard.dev/viewer/?uri=github.com/FerroHEALTH/FerroFED)
[![OpenSSF Best Practices](https://www.bestpractices.dev/projects/15130/badge)](https://www.bestpractices.dev/projects/15130)
[![Quality Gate Status](https://sonarcloud.io/api/project_badges/measure?project=FerroHEALTH_FerroFED&metric=alert_status)](https://sonarcloud.io/summary/overall?id=FerroHEALTH_FerroFED)
[![Coverage](https://sonarcloud.io/api/project_badges/measure?project=FerroHEALTH_FerroFED&metric=coverage)](https://sonarcloud.io/summary/new_code?id=FerroHEALTH_FerroFED)
[![License: BUSL-1.1](https://img.shields.io/badge/License-BUSL--1.1-blue.svg)](LICENSE)
[![GitHub release (latest SemVer)](https://img.shields.io/github/v/release/FerroHEALTH/FerroFED?sort=semver)](https://github.com/FerroHEALTH/FerroFED/releases/latest)
[![Image pulls](https://img.shields.io/badge/dynamic/json?url=https%3A%2F%2Fghcr-badge.elias.eu.org%2Fapi%2FFerroHEALTH%2FFerroFED%2Fferrofed&query=downloadCount&label=image%20pulls&logo=github)](https://github.com/FerroHEALTH/FerroFED/pkgs/container/ferrofed)
<!-- badges:end -->

<!-- conformance:begin -->
[![Federation Tier 0.9.0 gateway points](https://img.shields.io/endpoint?url=https%3A%2F%2Fraw.githubusercontent.com%2FFerroHEALTH%2FFerroFED%2Fmain%2Fconformance%2Fbadges%2Ffederation-gateway.json)](https://ferrofed.eu/docs/evaluate/conformance.html)
[![Federation Tier 0.9.0 node points](https://img.shields.io/endpoint?url=https%3A%2F%2Fraw.githubusercontent.com%2FFerroHEALTH%2FFerroFED%2Fmain%2Fconformance%2Fbadges%2Ffederation-node.json)](https://ferrofed.eu/docs/evaluate/conformance.html)
[![Federation Tier 0.9.0 operator points](https://img.shields.io/endpoint?url=https%3A%2F%2Fraw.githubusercontent.com%2FFerroHEALTH%2FFerroFED%2Fmain%2Fconformance%2Fbadges%2Ffederation-operator.json)](https://ferrofed.eu/docs/evaluate/conformance.html)
[![AQL golden cases](https://img.shields.io/endpoint?url=https%3A%2F%2Fraw.githubusercontent.com%2FFerroHEALTH%2FFerroFED%2Fmain%2Fconformance%2Fbadges%2Faql-golden.json)](conformance/aql-golden/pass-list.txt)
<!-- conformance:end -->

An openEHR federation gateway, in pure Rust: where else the record is.

A record held by another organisation is out of reach today. FerroFED is a
transparent ITS-REST intermediary in front of several openEHR CDRs: a client
sends it an ordinary AQL query and never learns it was federated. The gateway
resolves the patient outside the query, through an identifier
cross-reference service, so no directly identifying identifier travels to a
node. It sends each node standard AQL scoped to that node's own `ehr_id`, and
merges the answers with each node's provenance. It holds no clinical data of
its own.

FerroFED is one of the [FerroHEALTH](https://ferrohealth.eu/) family. The
documentation is at <https://ferrofed.eu/docs/>, and the design of record is
[`docs/architecture.md`](docs/architecture.md).

## Status

FerroFED is at v0.0.7 on its 0.0.x line, where each milestone is a release.
The gateway serves the federated query with the patient resolved outside AQL,
shapes the merged rows as one CDR would, routes follow-up reads and writes to
the node that owns them, sends definition requests to the node you name, and
holds stored queries itself.

Every client authenticates with an RFC 9068 access token from an issuer you
trust, carrying a SMART on openEHR scope for the operation and a purpose of
use, or through a proxy in the explicit edge mode
([client authentication](https://ferrofed.eu/docs/operate/authentication.html)).
Toward the nodes, the gateway authenticates as itself, with OAuth 2.0 client
credentials and a signed assertion or with a bearer token or a user and
password you configure per endpoint, and never sends the client's own token.
Every request to a node carries the verified client in an
`openEHR-federation-client` token the gateway signs with its own key, which
each node can verify against the key set the gateway publishes
([what a node is told about the caller](https://ferrofed.eu/docs/operate/authentication.html#what-a-node-is-told-about-the-caller)).
The
[claims page](https://ferrofed.eu/docs/evaluate/what-ferrofed-claims.html)
lists what each release shipped and what is planned.

## What it implements

- The openEHR Federation Working Group's
  [Federation Tier with AQL](https://syntaric.github.io/openehr-federation-spec/)
  specification, release candidate v0.9.0 at commit `7162d0c`. The
  [conformance matrix](https://ferrofed.eu/docs/evaluate/conformance.html)
  and the
  [obligations checklist](https://ferrofed.eu/docs/evaluate/obligations.html)
  say which points and statements a test holds.
- openEHR ITS-REST 1.1.0 on both faces, and openEHR AQL 1.1.0, through the
  published `openehr-*` crates.
- IHE PIXm ITI-83 for identity resolution. The `ihe-iti` crate also carries
  the PDQm ITI-78 client, and the mCSD resource reader the FHIR form of the
  registry uses.
- IHE XCPD ITI-55 for localization: the `xcpd` feature of `ihe-iti` is an
  Initiating Gateway, and an undirected patient query asks only the members
  whose communities it discovers, failing closed when a gateway does not
  answer. mCSD addressing, PMIR and the Dutch Generic Functions are planned
  for v0.0.8.

## Quickstart

The gateway beside four member CDRs: four FerroEHR instances on one
PostgreSQL server, with a database per node. `compose.yaml` runs the
published image of the product version, and the seed script creates
synthetic patients over each node's ITS-REST API: one at all four nodes, one
at two, one at one and one at none.

```sh
scripts/quickstart/signing-key.sh
docker compose up --wait
scripts/quickstart/seed.sh
```

The first script writes the gateway's development signing key, once, with
`openssl`; the gateway signs the caller's identity onto every request to a
node with it.

Then send one ordinary ITS-REST query to the gateway for the patient every
node knows. `scripts/quickstart/token.sh` mints the access token the
quickstart gateway accepts, from a development issuer whose key pair it
generates with `openssl` on its first run:

```sh
curl -s http://127.0.0.1:8080/v1/query/aql \
  -H "Authorization: Bearer $(scripts/quickstart/token.sh)" \
  -H 'Content-Type: application/json' -d 5820 <<'EOF'
{"q": "SELECT c/uid/value FROM EHR e CONTAINS COMPOSITION c WHERE e/ehr_status/subject/external_ref/id/value = 'ffd-test-0001' AND e/ehr_status/subject/external_ref/namespace = 'urn:oid:2.999.1.1'"}
EOF
```

The answer is one ITS-REST `RESULT_SET` with a composition from each node in
`rows`, and `meta.federation` reports all four endpoints `active`. Each node
received a query scoped to its own `ehr_id`, with no patient identifier in
it. The quickstart resolves its synthetic patients through a static
development cross-reference, and its credentials are development values.
[The container page](https://ferrofed.eu/docs/operate/container.html) walks
through a patient missing at some nodes, a query directed at one node, the
ports and the measured memory use.

## Install

Every release on the
[releases page](https://github.com/FerroHEALTH/FerroFED/releases/latest)
ships the `ferrofed` binary for x86_64 and aarch64 Linux, on glibc and on
musl, each tarball with its checksum, SLSA provenance and SBOMs. The image
`ghcr.io/ferrohealth/ferrofed` carries the musl binary for `linux/amd64` and
`linux/arm64`. Verify what you download before you run it:

```sh
gh attestation verify ferrofed-vX.Y.Z-x86_64-unknown-linux-musl.tar.gz \
  --repo FerroHEALTH/FerroFED \
  --signer-workflow FerroHEALTH/FerroFED/.github/workflows/release-build.yml
gh attestation verify oci://ghcr.io/ferrohealth/ferrofed:X.Y.Z \
  --repo FerroHEALTH/FerroFED \
  --signer-workflow FerroHEALTH/FerroFED/.github/workflows/release-image.yml
```

Every release also carries `compose.yaml`, which runs the gateway alone, at
that release's image, in front of the CDRs you already run, with two example
files it mounts. Download the three, edit `registry.toml` (your members) and
`ferrofed.toml` (your federation id, your PIX Manager and the credentials each
endpoint gets), put each credential file and the gateway's signing key in
`secrets/`, and start it:

```sh
for f in compose.yaml ferrofed.toml registry.toml; do
  curl -LO "https://github.com/FerroHEALTH/FerroFED/releases/latest/download/$f"
done
# edit ferrofed.toml and registry.toml; put credential files in secrets/
openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-384 \
  -out secrets/signing-key.pem
docker compose up --wait
```

A credential is always a file, never a value in `ferrofed.toml`.
[The container page](https://ferrofed.eu/docs/operate/container.html#the-gateway-from-a-release)
walks through each step.

`ferrofed serve --config ferrofed.toml` runs the gateway, and
`ferrofed config check --config ferrofed.toml` reports whether it would start
on that file. The
[configuration chapter](https://ferrofed.eu/docs/operate/configuration.html)
covers every key, the registry and the identity service.

## Documentation

- [Evaluate](https://ferrofed.eu/docs/evaluate/the-federation-tier.html):
  the specification, what FerroFED claims, conformance, versions and the
  licence.
- [Operate](https://ferrofed.eu/docs/operate/deployment-shape.html):
  deployment, the container, configuration, admission, health and metrics.
- [Integrate](https://ferrofed.eu/docs/integrate/client-contract.html): what
  a client sends and gets back, and every error code.
- [Contribute](https://ferrofed.eu/docs/contribute/how-the-work-is-organised.html):
  the tracker and the checks, beside [CONTRIBUTING.md](CONTRIBUTING.md).

## Licence

FerroFED is source-available under the Business Source License 1.1. The
parameters that apply, the Licensor, the Licensed Work, the Additional Use
Grant and the Change Date, are in [LICENSE](LICENSE): free for non-commercial
production use, a commercial licence for any other production use, and Apache
2.0 four years after each version is published. The maintainer named in
[MAINTAINERS.md](MAINTAINERS.md) is the contact for a commercial licence.

The brand assets under `assets/brand/` are part of the Licensed Work.

Contributions carry the terms in
[CONTRIBUTING.md](CONTRIBUTING.md#licensing-of-contributions): you keep your
copyright, and you grant the Licensor the relicensing right that keeps the work
one work under one licensor. There is no separate agreement to sign.
