---
paths: ["scripts/vendor/*.sh", "**/vendor/**", "docs/specs/**"]
---

<!-- SPDX-FileCopyrightText: Vernum Projecten B.V. -->
<!-- SPDX-License-Identifier: BUSL-1.1 -->

# Vendored inputs

External material enters this repository one way only: a committed fetch
script, vendored verbatim, stamped with provenance. The specification corpora
live under `docs/specs/`, one directory per corpus, each fetched by its own
`scripts/vendor/*.sh` from the pins in `docs/VERSIONS.md`:

- `docs/specs/federation-spec/`: the Federation Tier with AQL specification
  source (CC0 1.0), whole tree, including its two JSON schemas.
- `docs/specs/federation-ref/`: the reference implementation (Apache License
  2.0), whole tree, as evidence and a test corpus. Its code is never copied
  into this repository (`spec-adherence.md`).
- `docs/specs/its-rest/`: the openEHR ITS-REST 1.1.0 OpenAPI documents
  (content CC-BY-ND 3.0).
- `docs/specs/aql/`: the openEHR AQL 1.1.0 specification source, its examples
  and the grammar `.g4` files (CC-BY-SA 3.0).
- `docs/specs/ihe-pixm/`: the ITI-83 artefacts of the IHE PIXm 3.1.0 FHIR
  package (CC-BY-4.0): the `$ihe-pix` OperationDefinition, the Query
  Parameters profiles, the capability statements and the IG's examples,
  pinned by package version and tarball sha256.
- `docs/specs/ihe-pdqm/`: the ITI-78 artefacts of the IHE PDQm 3.2.0 FHIR
  package (CC-BY-4.0): the Consumer and Supplier capability statements, the
  Query Patient Resource Response Message and Patient profiles and the IG's
  examples, pinned by package version and tarball sha256.
- `docs/specs/ihe-mcsd/`: the ITI-90 and ITI-91 artefacts of the IHE mCSD
  4.0.0 FHIR package (CC-BY-4.0): the Directory, Query Client and Update
  Client capability statements, the Organization, Endpoint and Location
  profiles, the endpoint type code system and value sets and the IG's
  Organization and Endpoint examples, pinned by package version and tarball
  sha256.
- `website/book/vendor/mermaid/`: the mermaid browser bundle and the
  mdbook-mermaid init script the book loads, fetched by
  `scripts/vendor/mdbook-mermaid-assets.sh` (MIT and MPL 2.0).

A new corpus (an IHE profile's published artefacts, the Dutch
Generic Functions IG) gets its own directory, script, pin and `PROVENANCE.md`
in the change that first needs it, and this list grows with it.

## The rule

Every external corpus (a specification's machine-readable artifacts, a
schema, a test corpus, a grammar) is:

- **Fetched by a committed `scripts/vendor/*.sh` script.** Never hand-download
  into the tree, never hand-edit a vendored file, and never paste material in
  from a chat transcript. To refresh or extend a corpus, change the script,
  re-run it, and commit the result. A hand-edit of a vendored file is a defect
  to revert.
- **Vendored verbatim**, byte for byte as the publisher ships it. Reformatting,
  pretty-printing, or trimming a vendored file destroys the property that makes
  it checkable against its source.
- **Stamped with a `PROVENANCE.md`** in its own directory, recording the
  upstream source (the URL or registry), the exact version or commit pin, the
  fetch date, and the upstream licence, with the upstream `LICENSE` vendored
  alongside. A vendored tree with no provenance is unusable: nobody can tell
  what it is or whether it may be redistributed.
- **Exercised.** A vendored input is not done until something reads it: a
  codegen drift check, a schema-validation test, or a corpus test. An unread
  corpus is dead weight that rots without anyone noticing.
- **Marked in `.gitattributes`** as `linguist-vendored`, and `-text` where the
  bytes must not be line-ending normalized, so the tree matches the upstream
  archive exactly.

## Licensing

Vendored material keeps its upstream terms, and those terms are recorded in the
`PROVENANCE.md` rather than assumed. The project's own code and text are
under the Business Source License 1.1 (`CLAUDE.md` §Licence); a vendored tree
is not, and the two are never conflated. If a corpus's licence does not permit redistribution, it is
not vendored: the fetch script pulls it into an ignored directory at build time
and the repository ships none of it.

## Never commit clinical or patient data

No patient data, no identifiable health information, and no extract from a
production system belongs in this repository, in a fixture, or in a vendored
tree. Test fixtures are synthetic content invented for the test
(`testing.md`). A patient identifier in a fixture is synthetic too: never a
real national identifier such as a BSN, even one that looks like a test
value. A licence-gated code system or clinical corpus is a deployment
input, never a committed artifact, and `.gitignore` refuses its shapes so a
copy dropped into a working tree cannot be committed by accident. Widen
`.gitignore` in the same change when a genuinely new shape appears.
