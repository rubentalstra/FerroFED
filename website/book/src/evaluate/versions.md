<!-- SPDX-FileCopyrightText: Vernum Projecten B.V. -->
<!-- SPDX-License-Identifier: BUSL-1.1 -->

# Pinned versions

Every version the repository depends on is pinned once, in
[`docs/VERSIONS.md`](https://github.com/FerroHEALTH/FerroFED/blob/main/docs/VERSIONS.md),
and `scripts/checks/versions.sh` fails a change that lets a repeated pin drift
from it, this page included. This page summarises the pins that shape the
design. Each Item names its rows in `docs/VERSIONS.md` by their exact names,
so the guard can hold every row here to its source.

| Item | Pin | Why |
|---|---|---|
| Federation Tier with AQL | 0.9.0, release candidate | the governing specification; re-pinned when 1.0 is published |
| Federation Tier with AQL specification | commit `7162d0c` | the vendored source of that release candidate |
| openEHR ITS-REST | 1.1.0 | the façade a client sees and the API each node exposes |
| openEHR AQL | 1.1.0 | the query language on both sides of the gateway |
| `openehr-query`, `openehr-its`, `openehr-base`, `openehr-rm` | 0.0.81 | the published crates the gateway builds on, moved together as one family |
| IHE PIXm FHIR package | `ihe.iti.pixm`, version 3.1.0 | the identifier cross-reference binding (ITI-83) |
| IHE PDQm FHIR package | `ihe.iti.pdqm`, version 3.2.0 | the demographics query capability of `ihe-iti` (ITI-78) |
| IHE mCSD FHIR package | `ihe.iti.mcsd`, version 4.0.0 | the addressing binding: the registry read from a care services directory (ITI-90, ITI-91) |
| Rust toolchain | 1.98.1, edition 2024 | the toolchain the workspace builds with |

The other identity bindings (IHE PMIR, XCPD and the Dutch Generic
Functions) are listed in `docs/VERSIONS.md` with the version each
binding uses, and each package is vendored by the issue that first reads it.

## Vendored specifications

The specification, its reference implementation, the ITS-REST OpenAPI
documents and the AQL source are vendored verbatim under
[`docs/specs/`](https://github.com/FerroHEALTH/FerroFED/tree/main/docs/specs),
each fetched by a committed script and stamped with a `PROVENANCE.md` that
records the source, the pin, the licence and a tree digest.
