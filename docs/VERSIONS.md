<!-- SPDX-FileCopyrightText: Vernum Projecten B.V. -->
<!-- SPDX-License-Identifier: BUSL-1.1 -->

# Pinned version matrix

This file is the single source of truth for every version pin in FerroFED.
When it and a file that repeats a pin disagree, that is drift. Fix the
disagreement; never let either side silently win.
`scripts/checks/versions.sh` enforces the cross-file agreement it can reach,
and skips loudly for any file it compares that is absent.

No specification governs this file; it is FerroFED's own design.

## Specifications

The ground for each pin is the pin table in `docs/architecture.md`, the
architecture of record, which records why the value is what it is. The guard
compares the first token of each `Pin` cell below with the first token of the
same row there.

| Item | Pin | Repeated in |
|---|---|---|
| Federation Tier with AQL | 0.9.0 | `docs/architecture.md`, the `FEDERATION_SPEC` constant of `openehr-federation`, the source of the `spec_version` an `OPTIONS {base}/` body carries |
| openEHR ITS-REST | 1.1.0 | `docs/architecture.md`, the `ITS_REST` constant of `ferrofed-engine` |
| openEHR AQL | 1.1.0 | `docs/architecture.md`, the `AQL` constant of `openehr-federation` |

The federation specification is a release candidate circulated for comment by
the openEHR Federation Working Group. Its 1.0 release replaces this row and the
two corpus rows below in one change: re-pin, re-run both vendor scripts, and
diff the trees. The pinned commit is past the `0.9.0` git tag: it carries the
SEC review amendments of 2026-09-28 (the `meta.federation` nesting, the
all-or-nothing default, the `node-error` status) while the document still
declares `spec-version: '0.9.0'`.

The federation specification binds ITS-REST by name, Release-1.1.0, so the
ITS-REST row follows it rather than the latest ITS-REST development line.

## Bindings (decided, vendored with their first consumer)

The specification names the IHE ITI profiles as its proposed binding (Annex
A) without naming their versions, and the Dutch Generic Functions IG as a
regional alternative (Annex B), which it names at `fhir.nl.gf#0.3.0`. The
bindings and their versions are decided (`docs/architecture.md` §6, decision
A18): PIXm 3.1.0, mCSD 4.0.0 and PMIR 1.6.0 (CC-BY-4.0) and `fhir.nl.gf`
0.3.0 (EUPL-1.2), each vendored under `docs/specs/` and moved into the corpus
table below by the issue that first reads it (#42, #74 and #86, #48, #87).
XCPD has no FHIR package; its adapter (#85) binds the ITI Technical Framework
revision below, cited and not vendored: IHE International licenses its own
text for reproduction (General Introduction ch. 9), but the ITI-55 pages
reproduce HL7 v3 tables whose rights HL7 reserves. The client is held to the
revision by synthetic fixtures shaped after its examples
(`crates/ihe-iti/tests/fixtures/xcpd/`). PDQm is not
used by the gateway; its ITI-78 client is a capability of `crates/ihe-iti`
for other callers (#119), vendored with it. The versions are what the FHIR
package registry listed as latest on 2026-10-01.

| Binding | Package | Latest on 2026-10-01 |
|---|---|---|
| PIXm (ITI-83, ITI-104) | `ihe.iti.pixm` | 3.1.0, vendored by #42 (the corpus table below) |
| PDQm (ITI-78, ITI-119) | `ihe.iti.pdqm` | 3.2.0, vendored by #119 (the corpus table below) |
| PMIR (ITI-93, ITI-94) | `ihe.iti.pmir` | 1.6.0 |
| mCSD (ITI-90, ITI-91) | `ihe.iti.mcsd` | 4.0.0, vendored by #86 (the corpus table below) |
| XCPD (ITI-55) | the IHE ITI Technical Framework, no FHIR package | Vol 2 Rev 20.1 (2024-12-12, Final Text) |
| Netherlands Generic Functions | `fhir.nl.gf` | 0.3.0, as Annex B names it |

## Corpora and machine-readable inputs

A corpus is pinned by commit or immutable tag, or, for a FHIR package, by its
version and the sha256 of the registry tarball; never by a moving tag or a
`latest` URL, and vendored by a committed `scripts/vendor/*.sh` with a
`PROVENANCE.md` (`.claude/rules/vendored-inputs.md`). Each script below reads
its pin from this table, and `scripts/checks/versions.sh` reads each vendored
`PROVENANCE.md` back and fails when it names a different commit or tag.

| Item | Pin | Repeated in |
|---|---|---|
| Federation Tier with AQL specification | `syntaric/openehr-federation-spec` commit `7162d0c760d23105d62a743bf0ad1073c45fdb85` | `scripts/vendor/federation-spec.sh`, `docs/specs/federation-spec/PROVENANCE.md` |
| Federation Tier reference implementation | `syntaric/openehr-federation-ref` commit `92aff3cb1d8738ea0ce0e013b5a8fc2942438fd5` | `scripts/vendor/federation-ref.sh`, `docs/specs/federation-ref/PROVENANCE.md` |
| openEHR ITS-REST OpenAPI | `openEHR/specifications-ITS-REST` tag `Release-1.1.0`, all seven API modules and the Query validation document | `scripts/vendor/its-rest.sh`, `docs/specs/its-rest/PROVENANCE.md` |
| openEHR AQL specification source | `openEHR/specifications-QUERY` tag `Release-1.1.0`, the AQL and AQL examples documents and the grammar | `scripts/vendor/aql.sh`, `docs/specs/aql/PROVENANCE.md` |
| IHE PIXm FHIR package | `ihe.iti.pixm` version `3.1.0` from `packages.fhir.org`, tarball sha256 `19e2e8eaf3030ac7b4d809c5e1eeb8face02c8635318aeb6d35bc2bb889de0d0`, the ITI-83 artefacts | `scripts/vendor/ihe-pixm.sh`, `docs/specs/ihe-pixm/PROVENANCE.md` |
| IHE PDQm FHIR package | `ihe.iti.pdqm` version `3.2.0` from `packages.fhir.org`, tarball sha256 `61e09fbee991ff7c131b6ba5474921001e07782209961f3e85cee5f3ebcaedc2`, the ITI-78 artefacts | `scripts/vendor/ihe-pdqm.sh`, `docs/specs/ihe-pdqm/PROVENANCE.md` |
| IHE mCSD FHIR package | `ihe.iti.mcsd` version `4.0.0` from `packages.fhir.org`, tarball sha256 `933a143d7bb14c66731a32f52a084c6cb92476aca1b917db77a4640f8a5290ad`, the ITI-90 and ITI-91 artefacts | `scripts/vendor/ihe-mcsd.sh`, `docs/specs/ihe-mcsd/PROVENANCE.md` |

## openEHR model crates (crates.io)

The openEHR surface comes from the published `openehr-*` crates, consumed by
version like any other dependency (`docs/architecture.md` §2). FerroEHR
releases them as one lockstep family, so the five rows below are one group:
they move together, and `scripts/checks/versions.sh` fails when one member
moves alone, here or in the root `Cargo.toml` `[workspace.dependencies]`. The
pin is the latest version on crates.io, 0.0.81 since 2026-10-03.
`openehr-sdt` (the SMART on openEHR scope grammar) joined the group at the
family pin with its default features off, so only the grammar is compiled:
the onward grant (#81) writes and checks the scope it requests with it, and client
authentication (#80) reads every caller's scopes with it.

| Item | Pin | Repeated in |
|---|---|---|
| `openehr-query` | 0.0.81 | `docs/architecture.md`, the root `Cargo.toml` `[workspace.dependencies]`, the `OPENEHR_FAMILY` constant of `ferrofed-server` |
| `openehr-its` | 0.0.81 | `docs/architecture.md`, the root `Cargo.toml` `[workspace.dependencies]`, the `OPENEHR_FAMILY` constant of `ferrofed-server` |
| `openehr-base` | 0.0.81 | the root `Cargo.toml` `[workspace.dependencies]`, the `OPENEHR_FAMILY` constant of `ferrofed-server` |
| `openehr-rm` | 0.0.81 | the root `Cargo.toml` `[workspace.dependencies]`, the `OPENEHR_FAMILY` constant of `ferrofed-server` |
| `openehr-sdt` | 0.0.81 | the root `Cargo.toml` `[workspace.dependencies]`, the `OPENEHR_FAMILY` constant of `ferrofed-server` |

**0.0.74 is the lockstep release of the whole `openehr-*` family that carries
the federation gaps FerroFED raised, FerroEHR #3505 to #3514 (the AST visitor,
spans, parameter binding, the `FROM ENDPOINT` directive, the parser fix, the
router builder, the operation matcher with `forward`, the credentials provider
and per-call options; `docs/architecture.md` §2), published on 2026-10-01.
0.0.75 adds the crates' `repository` field. 0.0.76, published on 2026-10-02,
carries FerroEHR #3526: every generated ITS-REST type whose schema is open
keeps the extra members in an `additional_properties` map, so the open `Error`
carries the gateway's `code` and `request_id` (#57). 0.0.77, published on
2026-10-02, carries FerroEHR #3529, which classifies every AQL function call
as a built-in function of AQL or another name, and FerroEHR #3531, which
builds every `ReqwestTransport` client with redirects switched off (#195).
0.0.78, published on 2026-10-02, carries FerroEHR #3535: the
`Authorization` value of a `Credentials` is public, checked against RFC 7617
§2 for basic credentials and the `b64token` of RFC 6750 §2.1 for a bearer
token, so configuration load checks exactly what the node client sends
(#237). 0.0.79, published on 2026-10-02, carries FerroEHR #3537: the
`openehr-rm` attribute model holds the BASE primitives, the `Ordered` marker
and the reference targets of `OBJECT_REF` attributes, with the
`is_primitive` and `conforms_to_ordered` lookups the rewrite uses to decide
which paths a node can order under `DISTINCT` (#234). 0.0.80, published on
2026-10-03, carries FerroEHR #3539 (the openEHR identifier class of each path
parameter, #291), #3540 (a public decoder from a request to each operation's
params, #287, #292), #3541 (a reader for a Simplified Formats CONTRIBUTION,
#297) and #3543 (every operation's request-body media types, #298). FerroEHR
#3542, the canonical XML CONTRIBUTION reader, stays open upstream (#308).
0.0.81, published on 2026-10-03, carries FerroEHR #3548 (the `201_EHR` body of
`ehr_create` typed), #3551 (the request-body media-type picker made public)
and #3552 (`ROUTE_REQUEST_MEDIA` agreeing with each operation's `Content-Type`
parameter); #329 drops the three workarounds they replace.

## FHIR model crate (crates.io)

The FHIR model of the IHE bindings comes from the published `fhir-types`
crate (`docs/architecture.md` §6, decision A16), compiled only in
`crates/ihe-iti` with its `pixm` and `pdqm` features, so the gateway core
never builds it. `scripts/checks/versions.sh` fails when this row and the root
`Cargo.toml` `[workspace.dependencies]` disagree.

| Item | Pin | Repeated in |
|---|---|---|
| `fhir-types` | 0.1.107 | `docs/architecture.md`, the root `Cargo.toml` `[workspace.dependencies]` |

The `pixm` feature takes `r4` and `terminology`: that root set holds every
type ITI-83 reads (`Parameters`, `OperationOutcome`, `Identifier`, `Reference`,
`Bundle`). The `pdqm` feature takes `r4` and `resources`, every R4 resource,
because ITI-78 answers with `Patient` resources, which only `resources`
carries (#119); the mCSD directory (#86) reads `Organization` and `Endpoint`
from the same set.

## Metrics crates (crates.io)

The metrics surface (#281) is one OpenTelemetry `MeterProvider` read by the
Prometheus pull reader and the optional OTLP push, the stack FerroEHR runs.
The four `opentelemetry` crates are released in lockstep, so their rows are
one group: they move together, and `scripts/checks/versions.sh` fails when
one member moves alone, here or in the root `Cargo.toml`
`[workspace.dependencies]`. `prometheus` is the registry and text encoder
the pull reader renders through, on its own release line, checked against
the root `Cargo.toml` alone. Every version was the latest on crates.io on
2026-10-03.

| Item | Pin | Repeated in |
|---|---|---|
| `opentelemetry` | 0.33.0 | the root `Cargo.toml` `[workspace.dependencies]` |
| `opentelemetry_sdk` | 0.33.0 | the root `Cargo.toml` `[workspace.dependencies]` |
| `opentelemetry-prometheus` | 0.33.0 | the root `Cargo.toml` `[workspace.dependencies]` |
| `opentelemetry-otlp` | 0.33.0 | the root `Cargo.toml` `[workspace.dependencies]` |
| `prometheus` | 0.14.0 | the root `Cargo.toml` `[workspace.dependencies]` |

## Language and runtime

`rust-toolchain.toml` carries the toolchain, and the root `Cargo.toml` carries
the edition, the resolver and the MSRV. The release lane builds every
published binary on this toolchain, with no cache.

| Item | Pin | Repeated in |
|---|---|---|
| Rust toolchain | 1.98.1 | `rust-toolchain.toml` `channel` (stable) |
| Edition | 2024 | root `Cargo.toml` `[workspace.package]` `edition` |
| Cargo resolver | 3 | root `Cargo.toml` `[workspace]` `resolver` |
| MSRV | 1.98 | root `Cargo.toml` `[workspace.package]` `rust-version` |

The deliverable is a server binary, so the MSRV tracks the pinned stable
toolchain.

## Databases

A single gateway needs no database (`docs/architecture.md` §8). PostgreSQL is
used only as the optional backend of the stored-query store when several
gateway replicas run. Every PostgreSQL FerroFED itself tests against or
documents is the latest release line. A member node in the test harness runs
FerroEHR's documented database image, which is part of the product under test
(§13). The stored-query store's end-to-end tests run on that same image, built
on `postgres:18.6`, one database per use (#268), so no other PostgreSQL image
is pinned.

| Item | Pin | Repeated in |
|---|---|---|
| PostgreSQL | 18 | the stored-query `postgres` backend's tests, on the FerroEHR node database image below; the configuration page of the book |

## Container images

The gateway image builds on distroless static, and the quickstart runs four
member CDRs beside it, four FerroEHR instances on the same pin, each with its
own `system_id` and its own database on one FerroEHR PostgreSQL container
(`docs/architecture.md` §13, decisions A44 and A47). Every
image is pinned by tag and by the digest of its image index, resolved on
2026-10-01. `scripts/checks/versions.sh` holds every row equal to the file
that repeats it.

| Item | Pin | Repeated in |
|---|---|---|
| Container base image | `gcr.io/distroless/static-debian13:nonroot@sha256:e2e927ec666bae08560abb3c55d0659eceabb657f56b6782ab500a9fc7f555e3` | `docker/Dockerfile` `FROM` |
| FerroEHR node image | `ghcr.io/rubentalstra/ferroehr:4.3.1@sha256:b64f752aefe010629191f8c1d990d286c6ed28a62e457300a237a596f1116ac6` | `compose.yaml`, the `FERROEHR` constant in `tools/ferrofed-testkit/src/containers.rs` |
| FerroEHR node database image | `ghcr.io/rubentalstra/ferroehr-postgres:4.3.1@sha256:17d5772dba1c6689fccb1095a8774f3ed636f4968256a37fc505207ca75a99b9` | `compose.yaml`, the `FERROEHR_POSTGRES` constant in `tools/ferrofed-testkit/src/containers.rs` |

The quickstart's gateway image, `ghcr.io/ferrohealth/ferrofed`, carries the
product version below as its tag default, and the guard holds the two equal.

The end-to-end lane starts the same two node images through the testkit
harness, behind the `FERROFED_E2E` gate (`docs/ci-cd.md`): each is a
`PinnedImage` constant in `tools/ferrofed-testkit/src/containers.rs`, and the
guard holds every constant equal to its row here, so the quickstart and the
test suite always run the same nodes.

## Product and citation version

The product version is the workspace `version` in the root `Cargo.toml`, which
every member inherits. The milestone line is 0.0.x, starting at v0.0.1. v0.0.1 is
the first release (the `v0.0.1-rc.1` pre-release rehearsed the lane, #14);
each cut moves this row and every file that repeats it in one pull request.

| Item | Pin | Repeated in |
|---|---|---|
| Product version | 0.0.7 | `CITATION.cff` `version`, the root `Cargo.toml` `[workspace.package]` `version`, the `compose.yaml` gateway image tag default |

`CITATION.cff` tracks this row exactly, and the guard compares the two, and
the root `Cargo.toml` `[workspace.package]` `version` with both.

## Documentation toolchain

The site is an mdBook rendered by `.github/workflows/docs.yml`. Every tool it
installs is pinned here and repeated in the composite action that installs
them, so the book renders the same way in CI as it does on a laptop.

| Item | Pin | Repeated in |
|---|---|---|
| mdBook | 0.5.4 | `.github/actions/docs-toolchain/action.yml` `mdbook-version` |
| mdbook-toc | 0.15.4 | `.github/actions/docs-toolchain/action.yml` `mdbook-toc-version` |
| mdbook-mermaid | 0.17.1 | `.github/actions/docs-toolchain/action.yml` `mdbook-mermaid-version` |

## Licence

| Item | Pin | Repeated in |
|---|---|---|
| Project licence | BUSL-1.1 | `LICENSE`, `NOTICE`, the SPDX header of every first-party file, later the `license` field of every own `Cargo.toml`, the container `image.licenses` label, the README badge |

`LICENSE` names Apache License 2.0 as a licence of its own, as the Change
License four years after each version. `scripts/checks/versions.sh` fails on an
Apache-2.0 or MIT claim in any first-party file.

Third-party and vendored material keeps its upstream terms, recorded beside the
vendored tree (`.claude/rules/vendored-inputs.md`): the federation
specification is CC0-1.0, its reference implementation Apache-2.0, and the
openEHR specifications carry the licences their `PROVENANCE.md` files quote.

## Rust dependency pins

The root `Cargo.toml` `[workspace.dependencies]` table will be the
authoritative, fully pinned third-party crate set. Beyond the openEHR model
crates above, this file does not duplicate crate versions; on any discrepancy
the manifest wins. A crate joins a member with `dep.workspace = true`.

## CI tool pins

The tier-1 lanes of `.github/workflows/ci.yml` run the analyzers below, each
pinned to an exact version so a CI result matches the local one. `zizmor` and
`shellcheck` are fetched by `taiki-e/install-action`, which verifies the
upstream release checksum; `actionlint`, `hadolint` and `kubeconform` run from
their official container images, pinned by tag and by digest. `kubeconform`
validates the example manifests under `deploy/kubernetes/` against the schemas
of one Kubernetes release, read from `yannh/kubernetes-json-schema` at a pinned
commit, so neither a new schema nor a new release moves the result. `lychee`,
the offline link checker of the `site-links` job, is its upstream release
binary, fetched by version and checked against the SHA-256 its release
publishes, which the job carries beside the version.

| Item | Pin | Repeated in |
|---|---|---|
| `zizmor` | 1.30.1 | `.github/workflows/ci.yml` |
| `actionlint` | 1.7.12 | `.github/workflows/ci.yml` |
| `shellcheck` | 0.11.0 | `.github/workflows/ci.yml` |
| `hadolint` | 2.15.1 | `.github/workflows/ci.yml` |
| `kubeconform` | 0.8.0 | `.github/workflows/ci.yml` |
| `kubeconform schema version` | 1.34.0 | `.github/workflows/ci.yml` |
| `kubernetes-json-schema` | `8df8a883b68a24a104b4a9e43c1288090ae60b3b` | `.github/workflows/ci.yml` |
| `lychee` | 0.24.2 | `.github/workflows/ci.yml` |

Keep the locally installed versions on these numbers, so a finding costs a
local run rather than a CI round trip (`.claude/rules/ci-cd.md`).

## Release tool pins

The release lane builds and describes every published artifact with the tools
below, each fetched by the digest-pinned `taiki-e/install-action`, which
verifies the upstream release checksum. They decide what a consumer can prove
about a binary, so a floating version here would change the contents of a
release without a reviewed change.

| Item | Pin | Repeated in |
|---|---|---|
| `cargo-auditable` | 0.7.5 | `.github/workflows/release-build.yml` |
| `cargo-cyclonedx` | 0.5.9 | `.github/workflows/release-build.yml` |
| `syft` | 1.51.1 | `.github/workflows/release-build.yml`, `.github/workflows/release-image.yml` |
| `cargo-fuzz` | 0.13.2 | `.github/workflows/fuzz.yml` (the fuzz lane, the one nightly-toolchain job; #134) |

`scripts/checks/versions.sh` reads every `tool:` line of the release and fuzz
workflows back against these rows, so a bump moves one row and the workflows
follow it.

## GitHub Actions pins

Every `uses:` in `.github/workflows/**` is pinned to a full commit SHA with a
trailing `# vX.Y.Z` comment (`.claude/rules/ci-cd.md`). Dependabot bumps them,
and zizmor checks the form.
