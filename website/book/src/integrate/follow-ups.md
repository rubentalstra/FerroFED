<!-- SPDX-FileCopyrightText: Vernum Projecten B.V. -->
<!-- SPDX-License-Identifier: BUSL-1.1 -->

# Follow-ups

A composition id in a result row is an `OBJECT_VERSION_ID`, which already
carries the `creating_system_id` of the CDR that created it (§12.2). A new
object is always created on one node the client names; creation across
nodes is refused (§2.3, N23).

See how it works: [follow-ups and writes](../how-it-works/follow-ups-and-writes.md).

## Reading one version

A read whose path names one version, such as
`GET {base}/v1/ehr/{ehr_id}/composition/{uid_based_id}`, or a `version_uid`
under `versioned_composition`, `ehr_status`, `versioned_ehr_status` or
`directory`, goes to the node you name in `openEHR-federation-endpoint`, or
else to the node the path `ehr_id` resolves to, in the order below (§12.5.1,
N41). The version's `creating_system_id` does not route it: a request under a
path `ehr_id` is routed on that `ehr_id` (§12a.1), and the gateway never
rewrites the path for another node (N22). A copy imported into another node
carries the same version, so reading it where the row came from returns the
same content. To read the copy at the node that created it, send that node's
`ehr_id` for the patient, which a row from that node carries.

The gateway still learns which node holds versions of each
`creating_system_id` the registry document does not map (§12.2, N21). It
reads every version uid in a federated query's rows (as `c/uid/value`, as a
`uid`, or as the `uid` of a selected `COMPOSITION` or `VERSION`) and every
version uid a routed read names or answers with in its `ETag`. A system seen
at two nodes raises an integrity incident for the operator. These learned
routes never route a read under a path `ehr_id`, and never make a node that
holds a copy the CDR that controls it.

## Routing a path `ehr_id`

An `ehr_id` carries no system component, so a request to
`{base}/v1/ehr/{ehr_id}/…` does not say on its face which node holds the EHR
(§12.5). The `ehr_id` in the path must be an openEHR `HIER_OBJECT_ID`; any
other value is a `400` (`ehr-id-invalid`) and nothing is routed. The gateway
then finds the node in this order, and takes no later step once one names
exactly one node (§12.5.1, N41):

1. The targeting headers. Name the node yourself in the
   `openEHR-federation-endpoint` header, with the `endpoint_id` the result
   row carried (§8.4), or in the `openEHR-federation-organisation` header,
   when the organisation manages that one endpoint. This is the recommended
   way. Together the headers select exactly one registry endpoint: an unknown
   identifier, several endpoints or two headers that disagree are a `400`,
   and an organisation that manages no endpoint is a `404`
   (`no-destination`). A query parameter such as `?endpoint=` names no node
   here either; it is one the operation does not declare, so it is a `400`
   (`query-parameter-refused`) and nothing is sent.
2. A resolution binding of your client session: the node your earlier query
   resolved that `ehr_id` at. A session needs a client identity. The gateway verifies
   every caller ([Client authentication](../operate/authentication.md)), and
   keeping bindings per verified caller is planned
   ([#412](https://github.com/FerroHEALTH/FerroFED/issues/412)), so this step
   never answers yet.
3. The gateway's `ehr_id` index, which it learns from resolutions and from
   the nodes' successful answers.
4. For a read only, an ask-all probe: the gateway sends
   `GET {base}/v1/ehr/{ehr_id}` to every member at once, within its per-node
   timeout and overall budget (§11.5). The one member that answers with the
   EHR, while every other member answers `404`, gets your read; a read of
   the EHR itself is answered from that probe. When every member answers
   `404`, the read is a `404` (`no-destination`). When two members hold the
   `ehr_id`, the read is a `409` (`ehr-id-collision`) that lists them, and
   neither is read (§12.5.2, N42). When a member does not answer in time,
   cannot be reached, or answers an error, the owner is unknown and the read
   fails: `504` (`node-timeout`, `node-unreachable`) or `424`
   (`node-error`, `node-refused`), naming the member. The probe is sent only
   for an `ehr_id` that is a bare UUID. Any other `HIER_OBJECT_ID` form, such
   as an ISO OID, could be a patient identifier, and the probe would carry it
   to every member, so the read is a `400` (`probe-requires-uuid`) and no
   member is asked (§5.4.1, N33). Such an `ehr_id` is routed by the first
   three steps only: name its node in the endpoint header, and the gateway
   forwards it to that node alone.

A binding or an index entry that names two members is a collision, and the
gateway never picks one of them: the request, a read or a write, is a `409`
(`ehr-id-collision`) that lists them, nothing is probed, and neither member
is sent it (§12.5.2, N42). The explicit target of step 1 still routes such an
`ehr_id` to the node you name. A write that none of the first three steps routes is a `400`
with the code `target-required`, and nothing is probed, because the gateway
never finds a write's destination by trial (§12.5.1, N41).

## Querying one EHR by its `ehr_id`

An AQL query can address one EHR the way the path does, in either form
(N29), and the gateway treats the two as the same query:

```sql
SELECT c/uid/value FROM EHR e CONTAINS COMPOSITION c
WHERE e/ehr_id/value = '7d44b88c-4199-4bad-97dc-d78268e01398'

SELECT c/uid/value
FROM EHR e[ehr_id/value='7d44b88c-4199-4bad-97dc-d78268e01398'] CONTAINS COMPOSITION c
```

The node receives the first form, the canonical one of §7.1, whichever you
sent, so both answer the same rows. An `ehr_id` belongs to the node that
issued it (§12.5), so a query scoped to one `ehr_id` and naming no endpoint
goes only to the node that owns it, found in the order above: your session's
binding, the `ehr_id` index, then the ask-all probe, a query being a read.
`meta.federation.endpoints[]` reports that node and every other member as
`excluded`, and the `openEHR-federation-endpoint` header names the node
(N31). The answers of the probe apply as they do to a path: two members
holding the `ehr_id` are a `409` (`ehr-id-collision`) and neither is queried,
an `ehr_id` no member holds is a `404` (`no-destination`), an `ehr_id` that
is not a bare UUID and that no earlier step routes is a `400`
(`probe-requires-uuid`), and one that is not an openEHR `HIER_OBJECT_ID` is a
`400` (`ehr-id-invalid`). A query that names its endpoints, by the directive
or a header, goes to those endpoints as named. The query is scoped only when
every top-level `ehr_id` predicate names the same `ehr_id`, joined by `AND`;
a query that names two `ehr_id`s, or one under `OR`, is answered as a query
over every member.

A routed request reaches the node as you sent it:

- the body byte for byte, a `DV_IDENTIFIER` in a committed `COMPOSITION`
  included, because a commit body is clinical content the gateway has no
  right to alter (§5.4, N33);
- the method and the path, under the node's own base URL;
- the request headers the ITS-REST operation you address declares (of
  `Accept`, `Content-Type`, `If-Match`, `Prefer` and the `openehr-*` commit
  headers, the ones that operation lists), and no other header. `If-Match`
  reaches the node on a `PUT`, for example, and never on a `GET`. Your
  `Authorization` never reaches a node: the gateway authenticates to each node
  with that node's own credentials, and tells the node who you are in an
  `openEHR-federation-client` token it signs itself (§13.1, N24,
  [What a node is told about the caller](../operate/authentication.md#what-a-node-is-told-about-the-caller)).
  Neither does your `X-Request-Id`:
  the node receives the gateway's own id for the request. Nor do the
  targeting headers, which mean nothing at a node (§8.4);
- the query string, when the operation declares every parameter in it (such
  as `version_at_time` on a read, or `path` on a directory read). Any other
  parameter is a `400` (`query-parameter-refused`) and nothing is sent,
  because the gateway cannot tell an identifying value from any other
  (§5.4.1, N33);
- `Accept`, `Content-Type` and `Prefer` as the gateway composes them from
  yours: the media type the operation lists that your `Accept` prefers (the
  first listed for `*/*` or no `Accept`), the listed media type your
  `Content-Type` names, with a `charset=utf-8` dropped, and only the listed
  preferences of your `Prefer`. An `Accept` that admits no listed media type
  is a `406` (`media-type-not-acceptable`), and a `Content-Type` that names
  none is a `415` (`media-type-unsupported`), as a node would answer. A body
  never reaches a node without a `Content-Type`: when you send none, the
  node receives the one media type ITS-REST declares the operation's body
  in, and an operation that declares several is a `415`;
- each path identifier and each other declared value only when it is what
  the operation declares: a `version_uid` that is an `OBJECT_VERSION_ID`, a
  `versioned_object_uid` that is a UUID, a `version_at_time` in the extended
  ISO 8601 format. Any other value is a `400` (`parameter-value-invalid`) and
  nothing is sent. A value of free text, such as `If-Match` or a directory
  `path`, travels as you sent it (see
  [Configuration](../operate/configuration.md#declared-values)).

The answer is the node's: its status, its body, and its `Location` and `ETag`
unmodified, since openEHR uids are never rewritten (N22, N31). Every routed
answer, `POST`, `PUT` and `DELETE` included, names the acting endpoint in
`openEHR-federation-endpoint` and its node's `system_id` in
`openEHR-federation-system-id` (§7a.3, §9.6). A node's `Location` is the
node's own URL or path, so following it bypasses the gateway; send the
follow-up to the gateway with the version uid instead.

## Writing a new version

A versioned write amends a version that exists: an update of a composition,
of the `EHR_STATUS` or of the directory, a delete of the directory, each
naming the version it amends in `If-Match`, and a delete of a composition,
naming it in the path. It goes to the CDR that controls that version, the one
whose `system_id` equals the version's `creating_system_id`, and to no other
node (§12.4, §12a.1, N23). Writing at a node that holds only an imported copy
would fork the object (§10.3).

The gateway routes the write by its path `ehr_id`, in the order above, and
never by ask-all (§12a.1, N41). It then checks that node against the version:
the registry must map the version's `creating_system_id` to it, as the
node's own `system_id` or as a `[[creating_system]]` mapping the operator
registered. A mapping the gateway learned from answers never counts, because
a node that holds versions of a system need not have created them. The
outcomes:

- The path node controls the version: the write goes there once, as you sent
  it, and the node's answer comes back with its `ETag` and `Location`.
- Another member controls it, or no member is known to: the write is a `409`
  (`controlling-system-unreachable`), and no node is sent it. The message
  names the controlling system: the version's `creating_system_id` as the
  registry spells it, with the controlling node and its endpoint. When no
  member is known to control the version, the message points at the place in
  your request that names it (`If-Match`, the path, or the version of a
  `CONTRIBUTION` by its position), because the only spelling of that system
  is your own and the gateway never quotes your request. The path `ehr_id`
  belongs to the node it routes to, so the gateway cannot send the write to
  the controlling node instead; send it there under that node's own `ehr_id`
  for the patient.
- The write names no single version: `If-Match` is absent, repeated, a list,
  `*`, a weak tag, unquoted, or no `OBJECT_VERSION_ID`, or a composition
  delete's path is no `OBJECT_VERSION_ID`. That is a `400`
  (`preceding-version-invalid`), and nothing is sent.

A `CONTRIBUTION` names the versions it amends in its body, as each version's
`preceding_version_uid` (ITS-REST 1.1.0 `contribution_create`). The gateway
reads the body to find them, routes the request by its path `ehr_id` as above,
and holds the node to every amended version by the same rule: one version
another member controls, or no member is known to, makes the whole
`CONTRIBUTION` a `409` (`controlling-system-unreachable`), and no node is sent
it. A version with no `preceding_version_uid` creates an object, so a
`CONTRIBUTION` of creations alone goes where its path `ehr_id` routes it. The
node receives the body byte for byte as you sent it.

The gateway reads a `CONTRIBUTION` in the representation its `Content-Type`
selects:

- `application/json`, or no `Content-Type`: canonical JSON;
- `application/openehr.wt.flat+json` or
  `application/openehr.wt.structured+json`: a canonical envelope whose
  versions' `data` is FLAT or STRUCTURED (ITS-REST 1.1.0
  `contribution_create`). The gateway reads the envelope for each
  `preceding_version_uid` and leaves `data` to the node, so such a
  `CONTRIBUTION` routes exactly as a canonical one does, and the node
  receives it under the media type you declared.

A body it cannot read as the representation you declared, or a
`preceding_version_uid` that is no `OBJECT_VERSION_ID`, is a `400`
(`preceding-version-invalid`), because the gateway cannot tell which
versions it amends (§12.4), and nothing is sent. A `CONTRIBUTION` in
canonical XML (`application/xml`) is not read yet, so it is the same `400`
(#308).

The same rule covers a write against a row that de-duplication kept
(§10.3, N36). Suppose node A created a composition and node B holds an
imported copy under its own `ehr_id`. The kept row names node A's endpoint,
and `meta.federation.dedup` names node B's. A write through node B's
`ehr_id` is a `409` naming node A's system and endpoint, and node B is sent
nothing. That holds while node A is down too: the gateway never asks node A
before refusing, and it never falls back to writing at the copy. Send the
write through node A's `ehr_id`. Its answer names node A alone in
`openEHR-federation-endpoint` and `openEHR-federation-system-id`. Copies do
not converge: node B keeps the old version until it imports again.

## Creating an EHR

A new EHR has no owner yet, so neither a binding nor the index can name its
node, and nothing is probed for it. `POST {base}/v1/ehr` and
`PUT {base}/v1/ehr/{ehr_id}` go only to the one endpoint you name in
`openEHR-federation-endpoint` or `openEHR-federation-organisation` (§12.4,
N23). Without a header the request is a `400` (`target-required`); headers
that select several endpoints are a `400` (`endpoint-several`), because an
EHR is created at one node only (§2.3). The body, its `EHR_STATUS` subject
included, reaches that node byte for byte, and the node's `Location` and
`ETag` come back unmodified. A composition or a directory created inside an
existing EHR is routed by its path `ehr_id` like any other request under it.

`PUT {base}/v1/ehr/{ehr_id}` chooses its `ehr_id`, and the gateway checks it
against what it already knows before sending anything. When your session's
resolution bindings or the `ehr_id` index place that `ehr_id` at a member
other than the one you name, the request is a `409` (`ehr-id-held`), and no
node is sent it: the create would put one `ehr_id` at two members, the
collision of §12.5.2. The message names the holding endpoints and the one
you named, never the `ehr_id`. When they place it at the member you name,
the request goes there, and that node answers its own `409` for an `ehr_id`
it already uses (ITS-REST 1.1.0 `ehr_create_with_id`). An `ehr_id` the
gateway does not know goes to the member you name, and once that member
answers with a success, the `ehr_id` index holds the `ehr_id` there. Two
creates of one `ehr_id` at two members that race past the check both reach
their nodes, and the second to succeed raises the index-insert alarm of
§12b.2 for the federation operator; from then on a request under that
`ehr_id` is an `ehr-id-collision` until you name the endpoint.

## Reading an EHR by subject

`GET {base}/v1/ehr?subject_id=…&subject_namespace=…` names the patient in
its query string, and no node may receive a directly identifying identifier
(§5.4.1, N33). The gateway therefore consumes both parameters as resolution
input: it resolves the subject at the members through the cross-reference
service (§5.2) and sends the member that holds it
`GET {base}/v1/ehr/{ehr_id}` under that member's own `ehr_id`, with no query
string and no client header the operation does not declare. The node's
`EHR`, `ETag` included, comes back as the node sent it, with the acting
endpoint in `openEHR-federation-endpoint` and its `system_id` in
`openEHR-federation-system-id` (N31, §9.6).

- **Several members hold the subject:** a patient can have an EHR at more
  than one member, and this operation returns one. The gateway never picks
  one by where the patient resolved (§12.5.2), so the answer is a `409`
  (`subject-several`) listing the endpoints. Name one in
  `openEHR-federation-endpoint` to read its EHR; the header limits the
  resolution to that endpoint (§8.4).
- **No member holds the subject**, or not the one the header names: the
  operation's own `404` for a subject with no EHR (`no-destination`). This
  request reads one EHR resource, so the `200` with no rows that §11.3 sets
  for a query does not apply.
- **The cross-reference cannot answer** for a member: a `424`
  (`resolution-unavailable`), never a `404`, because that member may hold the
  EHR.
- **A localizer is configured** (`node_selection = "localized"`): the read
  names the patient and no node, so it is localized as an undirected query
  is (N4, §5.2, §14.1). The subject is resolved only at the members the
  localizer names, and every other member learns nothing of the request. A
  localizer that answers that no member holds the patient leaves the `404`.
  A localizer that does not answer is a `424`
  (`localization-unavailable`) with no member asked, under the default
  fail-closed policy; under `on_failure = "ask-all"` every member is a
  candidate. A read with `openEHR-federation-endpoint` is never localized
  (§8).
- **The consent pre-filter denies a member:** the subject is never resolved
  there and the member is sent nothing, as on a federated query (N27a). If
  no other member holds the subject, the answer is a `403`
  (`consent-denied`) naming the denied endpoints, never a `404`, because a
  denied member may hold the EHR. A pre-filter that cannot answer leaves
  every member to its own consent check
  ([Consent](../operate/identity.md#consent)).
- `subject_id` and `subject_namespace` are each given once; anything else in
  the query string is a `400`, and nothing is resolved or sent.

No error body and no log line carries the subject.
