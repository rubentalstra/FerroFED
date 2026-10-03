<!-- SPDX-FileCopyrightText: Vernum Projecten B.V. -->
<!-- SPDX-License-Identifier: BUSL-1.1 -->

# What FerroFED claims

FerroFED claims what a release ships and a test holds. This page lists the
standing decisions, what each release shipped, and what is planned, so you
can tell them apart. The
[changelog](https://github.com/FerroHEALTH/FerroFED/blob/main/CHANGELOG.md)
has the detail of every change.

## Decided

The architecture of record was decided on 2026-10-01 and is
[`docs/architecture.md`](https://github.com/FerroHEALTH/FerroFED/blob/main/docs/architecture.md).
These decisions hold across every release:

- **The specification is the authority.** The Federation Tier with AQL text
  decides; its reference implementation is read as evidence, never as an
  oracle. Where the specification is silent, the decision is FerroFED's own
  and is labelled that way in the code and in this book.
- **The openEHR surface comes from the published `openehr-*` crates.** The
  ITS-REST contract, the AQL parser and printer, the RM and the typed
  identifiers are those crates. A gap in one of them is fixed in that crate.
- **No clinical data of its own.** The gateway holds the registry, the
  `ehr_id` index and the stored-query definitions it is authoritative for.
  The record stays on the nodes.
- **Identifier hygiene is a hard rule.** Nothing the gateway composes for a
  node carries a directly identifying patient identifier, and every carrier
  the specification names has a negative test (§5.4, N33).
- **Pure Rust, a single binary**, as across the FerroHEALTH family.

## Shipped

### v0.0.3: the federated query and identity resolution

- `POST {base}/v1/query/aql` answers one ITS-REST `RESULT_SET` over every
  member of a registry, with no federation syntax needed (N1, CP-1).
- The patient is resolved outside AQL through an IHE PIXm PIX Manager
  (ITI-83) on either patient-identifier carrier, and each member that knows
  the patient receives standard AQL keyed on its own `ehr_id` (§5, §7.1, N7).
- The rewrite refuses a query that would carry the identifier to a node, and
  an outbound gate checks every request again before it leaves (§5.4, CP-26).

### v0.0.6: the federated answer, routing and targeting

v0.0.6 carries the milestones v0.0.4, v0.0.5 and v0.0.6; no v0.0.4 or v0.0.5
was tagged.

- The full per-endpoint report in `meta.federation`, all-or-nothing
  completeness with the best-effort opt-in, the per-node and overall budgets
  with `Prefer: wait`, and the §11.2 statuses with stable error codes (§9.5,
  §11.1 to §11.5).
- `ORDER BY` with `LIMIT` re-applied at the Tier, bounded `OFFSET` pages,
  `SELECT DISTINCT`, recombined `COUNT`, `SUM`, `MIN`, `MAX` and `AVG`, and
  opt-in version-identity de-duplication (§10, §11.6).
- Routing of the EHR resources under a path `ehr_id` to the one node that
  holds the EHR, byte for byte, through the targeting headers, the `ehr_id`
  index and the ask-all probe; the `creating_system_id` map learned from
  every answer (§7a.1, §12.2, §12.5).
- The `FROM ENDPOINT` and `ORGANISATION` directive, the targeting headers,
  and ENDPOINT attributes in rows (§8, §9.3).
- The registry document in FHIR form, `OPTIONS {base}/`, and the stored-query
  registry with immutable versions invoked by name (N19, §7a.2, §12.7).
- Track 10, the identifier-leakage suite, run against two FerroEHR nodes.

### v0.0.7: definitions and membership

The v0.0.7 milestone. `main` carries it, and its release is cut from `main`
once the milestone closes.

- Definition requests routed to one named node, the opt-in fan-out template
  upload, and stored-query distribution with drift reporting and an
  operator's repair (§12.6, §12.7, N43, N44).
- Versioned writes, `CONTRIBUTION`s included, sent only to the CDR that
  controls the version; a new EHR created only at a named node; `ehr_id`
  collisions refused and raised as integrity incidents (§12.4, §12.5.2,
  §12a.1, N23, N42).
- `GET {base}/v1/ehr` by subject, the `GET` forms of query execution, both
  `ehr_id` forms of N29, the provenance headers, a configurable base path, and
  the DEMOGRAPHIC area routed to one declared endpoint (§4.1, §7a.3, N28,
  N29, N31, N32).
- `ferrofed admission check` for the identifier-integrity conditions of
  §12b.2 (N42a), the registry reload on `SIGHUP`, the stored-query registry
  on `redb`, PostgreSQL or read-only files, health probes, and metrics.

### v0.0.8, on `main` so far

The v0.0.8 milestone is in progress; `main` carries these parts of it.

- Undirected patient queries localized by IHE XCPD ITI-55: a member whose
  community no responding gateway names is `not-localized` and never asked,
  and a localizer that does not answer fails closed, with its error on every
  member and in `meta.federation` (N4, N10, §14.1, Annex A.3, CP-5).

## Planned

Each milestone on the
[milestones page](https://github.com/FerroHEALTH/FerroFED/milestones) is a
release, and every issue in it names the sections it answers.

v0.0.8, security and the bindings (§13 to §15, Annex A, Annex B):

- client authentication at the gateway, built: RFC 9068 access tokens by key
  set or introspection, SMART on openEHR scopes per route, the purpose of
  use, and an edge mode
  ([#80](https://github.com/FerroHEALTH/FerroFED/issues/80),
  [Client authentication](../operate/authentication.md)); the client's
  identity conveyed on every request to a node, built: a token the gateway
  signs for each node, verifiable against its published JWKS
  ([#82](https://github.com/FerroHEALTH/FerroFED/issues/82),
  [What a node is told about the caller](../operate/authentication.md#what-a-node-is-told-about-the-caller));
- OAuth 2.0 client credentials with an RFC 7523 signed JWT assertion to each
  node, and the gateway's JWKS published
  ([#81](https://github.com/FerroHEALTH/FerroFED/issues/81));
- consent left to the node with the optional Step-1 pre-filter
  ([#83](https://github.com/FerroHEALTH/FerroFED/issues/83)), and the §13.4
  deployment decisions ([#84](https://github.com/FerroHEALTH/FerroFED/issues/84));
- the registry read from an mCSD directory
  ([#86](https://github.com/FerroHEALTH/FerroFED/issues/86)), and PMIR
  identity-lifecycle notifications
  ([#147](https://github.com/FerroHEALTH/FerroFED/issues/147));
- the Dutch Generic Functions as optional regional adapters
  ([#87](https://github.com/FerroHEALTH/FerroFED/issues/87),
  [#88](https://github.com/FerroHEALTH/FerroFED/issues/88));
- a `CONTRIBUTION` in canonical XML
  ([#308](https://github.com/FerroHEALTH/FerroFED/issues/308)), and traces
  exported through OpenTelemetry
  ([#353](https://github.com/FerroHEALTH/FerroFED/issues/353)).

v0.0.9, conformance (§16, §17): every conformance point scored
([#89](https://github.com/FerroHEALTH/FerroFED/issues/89)), the Connectathon
tracks as runnable suites
([#91](https://github.com/FerroHEALTH/FerroFED/issues/91),
[#92](https://github.com/FerroHEALTH/FerroFED/issues/92)), the node profile
([#93](https://github.com/FerroHEALTH/FerroFED/issues/93)), a differential run
against the reference implementation
([#94](https://github.com/FerroHEALTH/FerroFED/issues/94)), the conformance
statement ([#95](https://github.com/FerroHEALTH/FerroFED/issues/95)), and an
operator and query console
([#274](https://github.com/FerroHEALTH/FerroFED/issues/274)).

## Not claimed

FerroFED claims a conformance point only when a test carries its marker and CI
runs it. The [conformance matrix](conformance.md) records where each point
stands, and the [obligations checklist](obligations.md) does the same for
every normative statement of the specification. The specification is a
release candidate; when 1.0 is published, the vendored text is re-pinned and
every citation is checked against it
([#17](https://github.com/FerroHEALTH/FerroFED/issues/17)).
