<!-- SPDX-FileCopyrightText: Vernum Projecten B.V. -->
<!-- SPDX-License-Identifier: BUSL-1.1 -->

# The client contract

A client of a federation gateway is an ordinary openEHR client. This page sets
out what the specification promises that client. Once a registry is
configured, FerroFED serves the federated query at
`POST {base}/v1/query/aql`, and at its `GET` form
([the GET form](#the-get-form)), and routes the EHR resources under a path
`ehr_id`, `{base}/v1/ehr/{ehr_id}` and below it, to one node (§7a.1). It
reads an EHR by subject at `GET {base}/v1/ehr?subject_id=…&subject_namespace=…`
from the one member that resolves the subject
([reading an EHR by subject](follow-ups.md#reading-an-ehr-by-subject)). A
request under `{base}/v1/definition/` goes to the one node you name
([templates and definitions](templates-and-demographics.md#templates-and-definitions), §12.6). A
deployment that offers the stored-query registry stores queries under
`{base}/v1/definition/query/` itself instead, and runs them by name at
`GET` or `POST {base}/v1/query/{name}` ([stored queries](stored-queries.md), §12.7).
The DEMOGRAPHIC API under `{base}/v1/demographic/` is never federated: it
answers `501`, or goes to the one endpoint the deployment declared for it when
you name that endpoint ([demographics](templates-and-demographics.md#demographics), §7a.1, N32). Every other ITS-REST path under
`/v1/` answers `501` (N32).

The contract continues on three pages: [follow-ups](follow-ups.md), the
reads and writes routed to one node; [templates, definitions and
demographics](templates-and-demographics.md); and [stored
queries](stored-queries.md).

See how it works: [a federated query, end to end](../how-it-works/federated-query.md).

## The base URL

Every path in the client contract is relative to `{base}`, the base URL the
deployment chose and the registry or service discovery gives you (§4.1, N28). The
specification mandates and reserves no prefix: `/rest/openehr` is a vendor
convention, so do not hard-code it or any other. A gateway mounted at the
root serves `POST /v1/query/aql`; one the operator mounted at `/fed/openehr`
serves `POST /fed/openehr/v1/query/aql`, `OPTIONS /fed/openehr/` and
`GET /fed/openehr/v1/ehr/{ehr_id}`, and answers `404` for every path outside
that base. The gateway asks each node at the node's own base URL, so its own
base never reaches a node, and it passes a node's `Location` through
unmodified (N31).

## What a client sends

A conformant openEHR AQL request to `POST {base}/v1/query/aql`, where `{base}`
is the deployment's ITS-REST base URL; no prefix is mandated (§4.1, N28). The
patient is identified the openEHR way:

```sql
SELECT c/uid/value AS composition_id, c/context/start_time/value AS start_time
FROM EHR e CONTAINS COMPOSITION c
WHERE e/ehr_status/subject/external_ref/id/value = '12345'
  AND c/archetype_node_id = 'openEHR-EHR-COMPOSITION.encounter.v1'
```

No federation-specific syntax is needed for a basic patient query (§3.2, N1).
A client that wants to pin a query to named systems can do so in the AQL,
with `FROM ENDPOINT …` or `ORGANISATION …`, or beside it, with a request
header (§8).

The body is the ITS-REST `AdhocQueryExecute` in JSON, the one media type
ITS-REST lists for the operation. Send `Content-Type: application/json`, with
`charset=utf-8` if you like, or none, and the body is read as JSON. Any other
`Content-Type` is a `415` (`media-type-unsupported`), and no node is asked
(RFC 9110 §15.5.16). ITS-REST makes the header optional and names no default,
so reading a body without one as the listed media type is the gateway's own
choice.

### The GET form

ITS-REST also defines the ad hoc query as a `GET`, with the members of the
`AdhocQueryExecute` body in the query string, and FerroFED serves it (N1):

```http
GET {base}/v1/query/aql?q=SELECT%20c%2Fuid%2Fvalue%20FROM%20EHR%20e%20CONTAINS%20COMPOSITION%20c%20WHERE%20e%2Fehr_status%2Fsubject%2Fexternal_ref%2Fid%2Fvalue%20%3D%20%24patient&patient=P-12345&fetch=10
```

- `q`, `offset` and `fetch` are the body's members of the same name, and
  every other pair is one member of `query_parameters`: `patient=P-12345`
  binds `$patient`. A value that reads as JSON other than a JSON string (a
  number, a boolean, `null`) is that value, and anything else is text. So a
  value of digits alone binds as a number: bind an identifier of digits
  through the `POST` form, where it stays a string. A `+` is a literal plus,
  never a space (RFC 3986 §2.1); send a space as `%20`.
- The request runs the pipeline of the `POST` form unchanged: each node
  receives the same request, and you get the same `RESULT_SET`, status and
  headers. The headers of [pinning a query](#pinning-a-query-to-named-systems),
  completeness and dedup apply as they do to a `POST`.
- `ehr_id` is dropped, as it is from a `POST` body: the gateway scopes each
  node by its own `ehr_id` (§5.4.1, N33).
- A query string the ITS-REST decoder refuses is a `400` (`body-invalid`),
  the answer to a malformed `POST` body, and no node is asked: no `q`, a `q`,
  `offset` or `fetch` given twice, a query parameter given twice, an `offset`
  or `fetch` that is no integer, or a pair that does not percent-decode to
  UTF-8 text. A parameter of `null` is a `400` (`parameter-invalid`), as in
  a body.
- The request log never records `q` or a parameter value, only `offset` and
  `fetch` when they are digits.

## Pinning a query to named systems

The gateway accepts both mechanisms of §8.4, and they select nodes the same
way (N35). Use either one; there is nothing to discover first, because every
conformant gateway accepts both (§7a.2).

Put the directive at the start of `FROM`, and the gateway asks exactly the
endpoints it lists (§8.1, N11):

```sql
SELECT c/uid/value AS composition_id
FROM ENDPOINT p [ "node_1", "node_2" ]
  CONTAINS EHR e CONTAINS COMPOSITION c
WHERE e/ehr_status/subject/external_ref/id/value = '12345'
```

`FROM ORGANISATION [ "org-a" ] CONTAINS …` asks every endpoint the registry
lists as managed by each organisation (N20). The identifiers are the stable
registry identifiers of `meta.federation.endpoints[].id`, never URLs (§8.1,
N19), and the keywords are case-insensitive like every AQL keyword.

- The directive does not replace finding the patient. Each listed endpoint
  is still asked about its own `ehr_id` for the patient (N7, N11). A listed
  endpoint where the patient is not known is reported `not-resolved`, and the
  query still answers `200` (§8.1, §11.3).
- Every registry endpoint the directive does not list is reported `excluded`.
  It is not asked and does not clear `meta.federation.complete` (§11.1).
- No node receives the directive. The gateway sends each node standard AQL,
  so the node query is the same as for the undirected request (N7).
- An identifier the registry does not know is refused `400` with
  `endpoint-unknown` or `organisation-unknown`, before any node is asked
  (§8.4.1). A known organisation that manages no endpoint leaves the request
  without a destination, `404` with `no-destination`.
- A query directed at one endpoint may use what a fan-out refuses: an
  aggregate, which goes to the node unchanged (N14, §11.6.3), and a function
  AQL does not define. Pinned to more than one endpoint, the same query
  follows the rules for an undirected one.
- The variable (`p` above) may only be selected, as an ENDPOINT attribute
  (§9.3). Using it in `WHERE`, in `ORDER BY` or inside a function, or binding
  its name again in `FROM`, is refused `400` with `endpoint-variable`.

Or leave the AQL as it is and send the node set in a header (§8.4). This is
the form for a stored query or a query a user wrote, since the query text
stays the same however it is targeted:

```http
POST {base}/v1/query/aql
openEHR-federation-endpoint: node_1, node_2
Content-Type: application/json
```

`openEHR-federation-organisation: org-a` is the organisation form. Each
header carries a comma-separated list of the same registry identifiers, and
may be sent as several field lines; empty list elements are ignored (RFC 9110
§5.6.1).

- The header selects nodes exactly as the directive does: `not-resolved` for
  a listed endpoint that does not know the patient, `excluded` for every
  other endpoint, `400` with `endpoint-unknown` or `organisation-unknown` for
  an identifier the registry does not know or a header with no identifier in
  it, and `404` with `no-destination` when the selection holds no endpoint.
  A query directed at one endpoint by the header may use an aggregate, as one
  directed by the AQL may.
- Send the directive and a header, or both headers, only when they select the
  same endpoints. Then the request proceeds. When they differ, it is refused
  `400` with `targeting-conflict`, naming both sets; the gateway never merges
  them and never picks one (§8.4.1, N35).
- No node receives either header, and no query parameter targets anything:
  `?endpoint=` and `?organisation=` on the federated query have no effect
  (§8.4).
- The headers apply to every federated request, the routed requests of
  [follow-ups](follow-ups.md) included; the directive applies to AQL only
  (§8.4).

## Provenance columns: ENDPOINT attributes

A directed query can ask for the endpoint each row came from. Select an
attribute through the directive's variable, and the gateway fills it in for
every row from the registry entry of the endpoint that answered (§9.3, N12):

```sql
SELECT p/id AS endpoint_id, p/system_id AS system_id, c/uid/value AS composition_id
FROM ENDPOINT p [ "node_1", "node_2" ]
  CONTAINS EHR e CONTAINS COMPOSITION c
WHERE e/ehr_status/subject/external_ref/id/value = '12345'
```

| Path | Value in the row |
|---|---|
| `p/id`, `p/endpoint_id` | the endpoint id, as `meta.federation.endpoints[].id` reports it |
| `p/organisation`, `p/organization_id` | the endpoint's managing organisation (N20) |
| `p/system_id` | the openEHR `system_id` of the endpoint's node, the follow-up routing key (§12) |
| `p/url` | the CDR base URL the registry holds |

- Each value is a JSON string. No node is asked for it, and no node receives
  the variable or the directive (§8.1).
- A query that selects no attribute gets exactly the columns and rows a single
  CDR would return (N17).
- `columns[]` is the gateway's rendering of your query: an attribute column is
  named by its alias, and its path is the ITS-REST form with the variable
  stripped, `/id` for `p/id` (§9.2).
- An alias keeps an attribute apart from an EHR-derived column of the same
  name: `p/system_id AS node_system, e/system_id/value AS system_id` returns
  both (N18). Giving the attribute and an EHR-derived column the same alias is
  refused `400` with `endpoint-name-collision`, so no column is shadowed
  (CP-35). Any other path through the variable, such as `p/name` or
  `p/id/value`, is refused `400` with `endpoint-attribute-unknown`.
- Under `SELECT DISTINCT` the attributes count as selected columns: two rows
  of two endpoints are two rows when their attributes differ (N13).
- An attribute beside an aggregate over more than one endpoint would count per
  endpoint, so it is refused `400` with `indecomposable-aggregate`; pinned to
  one endpoint, the aggregate row carries the attribute (§11.6.3).
- Ordering on an attribute is refused `400` with `endpoint-variable`: §9.3
  defines the attributes as selectable only. The gateway already orders tied
  rows by endpoint id (§11.6.1).

## What a client gets back

An openEHR `RESULT_SET` exactly as ITS-REST 1.1.0 defines it, rows as ordered
arrays matched to `columns` (§9.1). A client that parses the AQL response of a
single CDR parses a federated one. Everything the federation adds lives in one
member of the open `meta` object, `meta.federation`:

- `complete`, whether the answer covers every node in scope (§11.4);
- `endpoints[]`, one entry per node with its status and provenance (§9.5,
  §11.1);
- `timeout`, the budget that applied (§11.5);
- `dedup`, the de-duplication policy that applied (§10);
- `localization`, only when a configured localizer did not answer:
  `localization.error` carries its error, which every member also carries
  (§14.1). Under the default fail-closed policy no member is asked, every
  member is `not-localized`, and `complete` stays `true`, so this member is
  how you tell "the localizer is down" from "no node holds this patient".
- `consent`, only when the consent pre-filter could not answer:
  `consent.error` carries its error. Every candidate was then asked and each
  node applied its own consent check, so `complete` and the status are what
  the nodes made them ([Consent](../operate/identity.md#consent)).

By default you get every row every node returned, duplicates included
(§10.1, N15). Send `openEHR-federation-dedup: version-identity` to get one
copy of a composition version held at several nodes: the copy from the CDR
that created it when that CDR answered, else the one from the lowest
endpoint id. Version ids and system ids that differ only in case name the
same thing (openEHR BASE), and the kept row comes back as its node sent it.
The gateway then orders the version uid without regard to case too, in an
`ORDER BY` on it and as the tie-break, so the page a `LIMIT` cuts does not
depend on the case a node wrote it in (§11.6.1).
`meta.federation.dedup` then names the endpoints whose
copies were dropped and counts the rows (§10.2, §10.3). A write you derive
from a kept row is routed like any other versioned write (see
[Writing a new version](follow-ups.md#writing-a-new-version)): it reaches the CDR that
created the version, and it does not update the copies the dedup dropped.
Two versions of one
composition are two rows either way, and `none` states the default
explicitly. Any other value is refused `400` with the code `dedup-invalid`.

An aggregate across nodes comes back as one row in your query's columns
([Aggregates across nodes](../operate/queries-and-areas.md#aggregates-across-nodes)).
`AVG` answers in the type of its input (AQL 1.1.0 §3.9.1.5): over integers,
the integer nearest the federation's mean, a tie going to the even one, and
over reals, a real.

Two more request headers shape the answer
([Queries and API areas](../operate/queries-and-areas.md)):

- `openEHR-federation-completeness: partial` asks for the rows of the nodes
  that answered when another node failed, with `200` and
  `complete: false`, where the deployment offers it; `all`, the default,
  fails the query instead (§11.4, N37).
- `Prefer: wait=<seconds>` shortens the time budget for this request, and
  `Preference-Applied` names it when it did (§11.5, N38; RFC 7240 §4.3).

Read whether an answer is complete from `meta.federation.complete`, never
from the status code; the gateway emits no FHIR `OperationOutcome`, because
its answer is an ITS-REST `RESULT_SET` (§11.4, N17).

The answer also names who answered in two response headers (§7a.3, N31):
`openEHR-federation-endpoint` carries registry endpoint identifiers and
`openEHR-federation-system-id` the `system_id`s of their nodes, position for
position, each as a comma-separated list in the form of the targeting header
(§8.4):

```http
HTTP/1.1 200 OK
openEHR-federation-endpoint: node_1, node_3
openEHR-federation-system-id: cdr1.example.org, cdr3.example.org
```

- A query the gateway sent to a single node names that node whatever it
  answered, zero rows, a `424` or a `504` included (N31). That is a query you
  directed at one endpoint, with the header or `FROM ENDPOINT`, a query whose
  patient resolves at one member alone, and a query scoped to one `ehr_id`,
  which names the node it was routed to. A query that asked no node, because
  the patient is `not-resolved` wherever it was looked for, carries neither
  header.
- An answer from several nodes lists the endpoints that contributed rows, in registry
  order. An endpoint contributed when it answered `active` with a
  `row_count` above 0, the count §9.5 takes before `DISTINCT`, dedup and
  `LIMIT`. An endpoint that answered no rows, failed, timed out, or was
  skipped, `excluded` or `not-resolved` is not listed, and an answer that
  fails (`424`, `504`) lists none.
- `meta.federation.endpoints[]` stays the record of every endpoint and its
  status (§11.1); the headers are a convenience. No specification fixes the
  list form, so FerroFED uses the form of the targeting header.
- The headers carry registry identifiers and nothing your request sent.

A request that fails answers the status §11.2 names and a stable code; the
[errors and status codes](errors.md) page lists every one.

### A node's error in `endpoints[]`

A member that answered with an error is `node-error`, and its `error` carries
the node's own failure (§9.5, §11.1): the node's HTTP status, then an excerpt
of the node's message, the ITS-REST `Error.message` when the body is one and
the body's text otherwise.

```json
{ "id": "node_3", "status": "node-error", "latency_ms": 41,
  "error": "the node answered 400 Bad Request: unknown archetype path in WHERE" }
```

- The excerpt is at most 512 characters, with `…` where it was cut.
- Control characters and invisible format marks (line breaks, tabs, the
  bidirectional overrides, zero-width marks) become one space.
- The patient identifier your query resolved on never comes back: where the
  node echoed it, raw or as an AQL string literal, it reads `[withheld]`,
  and where the node echoed it in a form the gateway cannot replace in place,
  percent-encoded for one, the whole message reads `[withheld]`.
- A node that sent no message, or nothing printable, is reported by its
  status alone.
- A node reported `offline` or `time-out` in a federated query carries the
  HTTP client's reason it was not reached, held to the same three rules.

One rule covers every per-member record FerroFED writes: a federated query,
the [fan-out template upload](templates-and-demographics.md#fan-out-template-upload), and the
[stored-query distribution and drift check](stored-queries.md#distributing-a-stored-query).
A node's body that is no error, an accepted upload's body or a member's copy
of a stored query, is never copied.

## Self-description

`OPTIONS {base}/` returns what the gateway does and which members stand
behind it, as JSON that validates against the specification's
`options-root.schema.json` (§7a.2, N30). It needs no patient identifier and
carries none. It answers only an authenticated caller, and `401` to any other
(§7a.2, §13; [Client authentication](../operate/authentication.md)); the
caller needs no scope and no purpose of use for it. Every value comes from the running
configuration, so the body says what the gateway does today:

| Member | What FerroFED declares |
|---|---|
| `federation.id` | the deployment's `federation.id` |
| `federation.spec_version` | `0.9`, the `major.minor` of the pinned specification release |
| `aql.fan_out` | `true`: an undirected query asks every member (§4.3, N4) |
| `dedup` | `default: "none"`, `modes: ["none", "version-identity"]`, and the request header `openEHR-federation-dedup` (§10, N15) |
| `timeout` | the configured `per_node_ms` and `overall_ms`, with `policy: "abandon-and-mark"`: a node past its budget is abandoned and reported `time-out` (§11.5, N38) |
| `completeness` | `default: "all-or-nothing"`; `best_effort` and, when it is offered, `opt_in` naming `openEHR-federation-completeness: partial` (§11.4, N37) |
| `paging` | `offset_strategy: "bounded"` with the configured `max_window`, or `"reject"`; never `"cursor"`, because no cursor is offered (§11.6.2, N39) |
| `aggregates.decomposable` | the configured functions, of `COUNT`, `SUM`, `MIN`, `MAX` and `AVG`; an empty list means none (§11.6.3) |
| `definition` | `fan_out_template_upload` as `federation.fan_out_template_upload` sets it (`false` by default); `stored_query_registry` is `true` while `[stored_queries]` is set and `false` otherwise; `stored_query_fan_out` is `true` while `federation.fan_out_stored_queries` is set beside the registry and `false` otherwise (N43, N44, §12.7) |
| `localization.on_failure` | `"closed"`, the default: a localizer that does not answer leaves every member `not-localized` with its error, and nothing is asked; `"ask-all"` only where the deployment set `federation.localization.on_failure` (§14.1, N30) |
| `localization.mode` | present with a localizer configured: the binding that localizes: `"xcpd"`, `"pixm"`, or `"development-static"` for the development cross-reference |
| `localization.audit` | present with the XCPD localizer: where its ITI-55 audit messages go, `"log"`, or `"off"` in a development deployment (ITI TF-2 §3.55.5.1) |
| `timeout.localization_ms` | present with a localizer configured: the localizer's own budget, a part of `overall_ms` |
| `its_rest` | `query` federated, `ehr` routed to the one node that owns the `ehr_id` (§12.5.1), `definition` `routed-single-node`, to the one endpoint the targeting headers name, naming the template upload fan-out where it is offered, with stored queries held at the gateway registry when it is offered and routed with the rest when it is not (§12.6, §12.7, §7a.2), and `demographic` unsupported (`501`), or `routed-single-node` naming the endpoint a request names when `federation.demographic_endpoint` is set; never federated (§7a.1, §12.6, N32) |
| `endpoints[]` | every registry endpoint with its `id`, its managing `organisation`, its `status` (`active`, or `suspended` for one the operator took out of service), its `node_id` and `system_id`, and the node's `product` and `version` where the registry holds them |

What is absent is absent on purpose:

- No targeting mechanism and no patient-resolution carrier. Both forms of
  each are mandatory at every gateway, so there is nothing to choose
  (§7a.2, N33, N35).
- No asynchronous queries. The schema has no member for them, and the gateway
  does not offer them (§11.7).
- No `auth.jwks_uri`. The gateway publishes no JWKS yet, and the schema says
  a gateway with none configured omits the key (§13.1).
- No latency. The gateway keeps no latency statistic per member.
- `paging.max_window` is FerroFED's own member inside the open `paging`
  object: the specification names no member for the bound of the `bounded`
  strategy.

`OPTIONS` on a path under `{base}/v1/` answers `204` with the methods served
there in `Allow`: `GET, POST, OPTIONS` for `/v1/query/aql`, and the ITS-REST
methods of the resource for an EHR resource under a path `ehr_id`, such as
`GET, PUT, OPTIONS` for `/v1/ehr/{ehr_id}`, and `GET, POST, OPTIONS` for
`/v1/ehr`. A definition resource answers the
ITS-REST methods of the resource, such as `GET, POST, OPTIONS` for
`/v1/definition/template/adl1.4`. Where the stored-query registry is
offered, a stored query answers `GET, POST, OPTIONS`. Where the DEMOGRAPHIC area is
routed, a DEMOGRAPHIC resource answers its ITS-REST methods, such as
`GET, PUT, DELETE, OPTIONS` for `/v1/demographic/person/{uid_based_id}`. The
gateway answers it without asking a node. A path the gateway does not serve
answers `501`.
