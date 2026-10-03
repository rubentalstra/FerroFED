<!-- SPDX-FileCopyrightText: Vernum Projecten B.V. -->
<!-- SPDX-License-Identifier: BUSL-1.1 -->

# The registry

This page covers the registry document and its FHIR form, the registry read
from an mCSD directory, the federation id, node selection, the state the
gateway learns (resolution bindings and the `ehr_id` index), reloading the
registry, and integrity incidents.

See how it works: [follow-ups and writes](../how-it-works/follow-ups-and-writes.md), routed with what the registry holds.

## The registry document

`registry.document` names a second TOML file: the federation's members as
the operator admitted them. It declares each `[[organisation]]`, each
`[[node]]` with its openEHR `system_id`, and each `[[endpoint]]` with its base
URL, connection type and managing organisation. An unknown key, a dangling
reference or a duplicate id refuses the whole document.

The document is also the follow-up routing table (N21). A follow-up for a
version is routed on the `creating_system_id` inside its uid (§12.2). A
member's own `system_id` routes to that member without being written down.
A CDR can hold versions another system created, because an imported
composition keeps its original uid. Map every other `creating_system_id` you
know of to the endpoint that answers for it with a `[[creating_system]]`
entry:

```toml
[[creating_system]]
creating_system_id = "legacy-a.example.org"   # the middle segment of the uid
endpoint = "hospital-a"                       # an endpoint id this document declares
```

`config check` refuses, naming the `creating_system_id`, a mapping that names
an endpoint the document does not declare, a `creating_system_id` mapped
twice, and a mapping of a member's own `system_id`. Two spellings that differ
only in ASCII case are one `creating_system_id`.

An endpoint may list the ITS-REST `Error` codes its node marks a consent
refusal with. A `403` from that node whose `Error` carries one of them is
reported `consent-denied`; every other refusal is `node-error`
([Consent](identity.md#consent)). The list is empty by default, and an empty
code refuses the document:

```toml
[[endpoint]]
id = "hospital-b"
# ... node, url, connection_type, managing_organisation
consent_refusal_codes = ["consent-refused"]   # what this node writes in Error.code
```

## The registry document in FHIR form

The specification recommends the FHIR `Endpoint` and `Organization` resources
for the registry (N19). Set `registry.format = "fhir"` and `registry.document`
names a FHIR R4 JSON `Bundle` of type `collection` or `searchset` instead,
holding only `Organization` and `Endpoint` resources: the shape an mCSD
directory delivers (§15.1). The default, `registry.format = "toml"`, is the
native form above.

```toml
[registry]
document = "/etc/ferrofed/registry.json"
format = "fhir"
```

The form loads into the same members as the native form, and the gateway
routes over it identically. FHIR has no place for a node or an openEHR
`system_id`, and a resource's logical id belongs to the server that holds it,
so FerroFED carries the registry's ids as identifiers in its own systems (no
specification governs these systems; they are FerroFED's design):

| FHIR element | Registry fact |
|---|---|
| `Organization.identifier` with system `https://ferrofed.eu/fhir/sid/organisation-id` | the organisation id, exactly one |
| `Organization.name` | the organisation's display name |
| `Organization.endpoint` | the endpoints whose node the organisation operates |
| `Endpoint.identifier` with system `https://ferrofed.eu/fhir/sid/endpoint-id` | the stable endpoint id used in directives (N19), exactly one |
| `Endpoint.identifier` with system `https://ferrofed.eu/fhir/sid/node-id` | the node the endpoint belongs to, exactly one |
| `Endpoint.identifier` with system `https://ferrofed.eu/fhir/sid/system-id` | that node's openEHR `system_id`, exactly one |
| `Endpoint.identifier` with system `https://ferrofed.eu/fhir/sid/creating-system-id` | each further `creating_system_id` the endpoint answers for (N21), zero or more |
| `Endpoint.connectionType` | `openehr-rest-query` in `https://ferrofed.eu/fhir/CodeSystem/connection-type` |
| `Endpoint.managingOrganization` | the one managing organisation (N20) |
| `Endpoint.status` | `active`, or `suspended` for an endpoint taken out of service |
| `Endpoint.address` | the ITS-REST base URL |
| `Endpoint.extension` with url `https://ferrofed.eu/fhir/StructureDefinition/consent-refusal-code` | one `valueCode` per consent refusal code, zero or more; the `consent_refusal_codes` of the native form |

An endpoint for the openEHR Query API never carries `hl7-fhir-rest` (§15.2).
No openEHR or HL7 code for it is registered yet, so FerroFED binds the one
code N19 names, `openehr-rest-query`, in a code system of its own. The mCSD
4.0.0 `Endpoint` profile binds `connectionType` to the HL7 endpoint connection
types extensibly, so a code from another system is admitted where the value
set has none for the purpose.

References resolve inside the Bundle as FHIR R4 §2.36.4.1 resolves them: a
relative `Organization/org-a` against the root of a REST `fullUrl` such as
`https://registry.example.org/fhir/Endpoint/node-a-pub`, and an absolute
reference, a `urn:uuid:` included, against an entry's `fullUrl`. Give every
entry a `fullUrl`. Other elements (`payloadType`, `period`, `header` and the
rest) are not read. A node's `product`, `version` and node identifiers have
no place in this form; a registry that needs them uses the native form.

`config check` refuses the document with the configuration exit code, naming
the resource, when:

- an endpoint's `connectionType` is `hl7-fhir-rest`, carries no system (an
  informal string), or is any other system and code (N19, §15.2). CP-20 is
  an operator point, and this check is how the gateway helps the operator
  meet it;
- an endpoint has no `managingOrganization`, or one that names no
  `Organization` of the Bundle (N20);
- an endpoint is listed by no organisation, or by two;
- an organisation or an endpoint has no id in its system or more than one, or
  an id repeats;
- the endpoints of one node disagree on its `system_id` or its operator;
- an endpoint's status is neither `active` nor `suspended`, or an organisation
  is marked inactive;
- a resource carries a `modifierExtension`, which FerroFED does not read;
- a consent refusal code extension carries no `valueCode`;
- anything the native form refuses: a duplicate `system_id`, an unusable base
  URL, or a `creating_system_id` that is a member's own.

## The registry read from an mCSD directory

The gateway can read its members from an IHE mCSD 4.0.0 care services
directory instead of a document, and keep them in step with it. The
specification proposes mCSD for addressing, and the registry is the local
materialisation of that mapping (§15.1, N21, Annex A.5). Set
`[registry.mcsd]` in place of `registry.document`; a configuration that sets
both is refused.

```toml
[registry.mcsd]
url = "https://directory.example.org/fhir"   # the directory's FHIR base
refresh_interval_s = 300                    # the default; 0 is refused
timeout_ms = 10000                          # per page, the default

[registry.mcsd.credentials]                 # when the transport does not authenticate
bearer_token_file = "/run/secrets/directory-token"
```

The members are the directory's `Organization`s that carry an
`https://ferrofed.eu/fhir/sid/organisation-id` identifier and its `Endpoint`s
that carry an `https://ferrofed.eu/fhir/sid/endpoint-id` identifier, written
exactly as the FHIR form above. The rest of the directory is not the
federation's and is never read into the registry. The URL is `http` or
`https` with no user name or password; the credentials take a bearer token or
basic credentials, each through its `_file` sibling, and never an OAuth 2.0
grant.

At start, the gateway reads the members with ITI-90, Find Matching Care
Services: `GET [base]/Organization?identifier=…|` and
`GET [base]/Endpoint?identifier=…|`, every page. The content then passes
every check the FHIR form passes: the connection type of §15.2 (N19), one
managing organisation per endpoint (N20), unique ids, and everything the
native form refuses. A directory that cannot be read, or holds a registry
that breaks a rule, stops the start; `config check` reads the directory the
same way and names the fault.

Every `refresh_interval_s` the gateway asks for the changes since the last
read with ITI-91, Request Care Services Updates:
`GET [base]/Organization/_history?_since=…` and the same for `Endpoint`. It
asks from 60 seconds before the `Date` the directory stamped its previous
answer with, so the directory's own clock decides and no change is missed;
a directory that sent no readable `Date` is read again whole with ITI-90. The
newest version of each resource wins, a deletion removes it, and a version
that no longer carries the federation's identifier takes it out of the
registry. A query never waits on the directory: the refresh runs on its own,
and a request that started before a refresh finishes on the registry it
started with.

A refresh that changed something goes through the same checks as a reload:

- When the changed registry passes, it replaces the running one, with the
  effects of a reload (learned routes held to it, entries for a member that
  left dropped). It logs `registry reloaded` and counts as an applied reload.
- When it breaks a rule (an endpoint relying on `hl7-fhir-rest`, a `system_id`
  given to two nodes, an endpoint deleted while an organisation still lists
  it, a member the resolver does not cover), it is refused. The running
  registry stays, the gateway logs `registry reload refused` with
  `class = "registry-invalid"`, and the refusal counts as a refused reload.
  The next refresh asks again from the same instant, so the registry follows
  the directory once the directory is put right.
- When the directory does not answer, or answers a `5xx` or something that
  is not ITI-91, the running registry stays and the gateway logs a warning.
  `GET {base}/health/dependencies` reports the directory as `directory`:
  `up` after its last answer, `failing` after a `5xx` or a malformed answer,
  and `down` when it did not answer. The directory's state never gates
  readiness.

A `SIGHUP` reload with a directory rebuilds the federation over the registry
the directory gave, applying `[credentials]`, `[dev]` and `[pixm]`; it never
asks the directory. A change to `[registry.mcsd]` takes a restart, and a
change between a document and a directory is refused as `registry-presence`.

## Federation id

A gateway that federates names its federation, and refuses to boot without
the name:

```toml
[federation]
id = "rso-example"
```

The id is `federation.id` of the `OPTIONS {base}/` self-description (§7a.2,
N30). It has no default, because it is the deployment's to choose, and an
empty id is refused. It is named in the startup log line.

## Node selection

A gateway that federates (`registry.document` or `[registry.mcsd]` is set)
declares how an undirected patient query finds its nodes, and refuses to
boot without the declaration:

```toml
[federation]
node_selection = "ask-all"
```

`ask-all` is the selection for a deployment with no localization service (the
specification's reference flow, Variant B; N4). Every active member is a
candidate: the gateway asks every member's cross-reference where the patient
is, dispatches the query only to the members that return an `ehr_id`, and
reports the others as `not-resolved` without failing the query. The selection
is named in the startup log line.

`localized` derives the node set from a localizer (N4, N10, §14.1):

```toml
[federation]
node_selection = "localized"

[federation.localization]
on_failure = "closed"   # the default; "ask-all" widens on failure
timeout_ms = 5000       # the default; below overall_timeout_ms, 0 is refused
```

For an undirected patient query, the gateway first asks the localizer which
members might hold the patient's data. A member it does not name is reported
`not-localized` and is never asked, and `complete` stays `true`, because that
member was never in scope. The cross-reference then resolves the patient at
the named members only. A directed query (the `FROM ENDPOINT` directive or the
targeting headers) is never localized: the directive selects its node set
(§8). A query that names no patient is refused with a `400` under this
selection, because localization is keyed on the patient and no node set is
defined (N4). The read of an EHR by subject, `GET {base}/v1/ehr?subject_id=…`,
names the patient and no node, so it is localized as an undirected query is:
the specification does not limit N4 to AQL, and a deployment that relies on
its localizer to narrow which members learn of a patient keeps that narrowing
on every patient route
([Reading an EHR by subject](../integrate/follow-ups.md#reading-an-ehr-by-subject)).

When the localizer does not answer within `timeout_ms`, or fails, the gateway
fails closed: it asks no member, reports every member `not-localized` with the
localizer's error, and carries the same error as
`meta.federation.localization.error`, so an outage never reads as a patient
with no data (§14.1). The status stays `200` and `complete` stays `true`,
since no member in scope failed. `on_failure = "ask-all"` asks every member
instead, still with the error in `meta.federation`; it holds only where it is
written, and `OPTIONS {base}/` declares the policy either way. A localizer
that answers that no member holds the patient's data leaves every member
`not-localized` with no error.

The localizer is the IHE XCPD binding when `[xcpd]` is set
([XCPD localization](identity.md#xcpd-localization-xcpd)), and otherwise the
[development cross-reference](identity.md), under `profile = "development"`,
which names the members its `[dev]` rows map the patient at. The localized
selection with no localizer refuses to boot, and so do
`[federation.localization]` and `[xcpd]` under `ask-all`.

## Resolution bindings

The specification lets a query's resolution leave a binding behind for the
client session: which member holds which `ehr_id`, so a follow-up on a path
`ehr_id` reaches the right node (§12.5.1 step 2). A session needs a client
identity. The gateway verifies every caller
([Client authentication](authentication.md)), and keeping bindings per
verified caller is planned
([#412](https://github.com/FerroHEALTH/FerroFED/issues/412)), so no binding
is held yet, and the `ehr_id` index below carries what a resolution teaches.

The lifetime a binding will have is already a setting, checked at boot:

```toml
[federation]
binding_ttl_ms = 900000   # 15 minutes, the default; 0 is refused
```

The lifetime is a correctness bound. An identity merge or split at the
identity source can make a binding stale, and a binding never outlives its
lifetime, so set it no longer than you would accept a follow-up being routed
on a superseded identity. A PMIR subscription that reports a merge or split
as it happens is planned for v0.0.8
([#147](https://github.com/FerroHEALTH/FerroFED/issues/147)); the
specification marks this lifecycle track provisional.

## The `ehr_id` index

The gateway also keeps an index of which member holds which `ehr_id`, shared
by every client. It learns an entry when a resolution finds the patient's
`ehr_id` at a member, and when a member answers a request under that `ehr_id`
with a success. A follow-up on a path `ehr_id` that names no node and has no
binding is routed by the index before the gateway falls back to asking every
member (§12.5.1). The index holds `ehr_id`s and member ids only, lives in
memory, and forgets the least recently used `ehr_id` once it is full:

```toml
[federation]
ehr_index_capacity = 100000   # ehr_ids held, the default; 0 is refused
```

A forgotten or never-learned entry costs a later request one fallback step,
never a wrong route: a read then asks every member, and a write is refused
until the client names its node. An `ehr_id` seen at two members is held at
both, the index raises the index-insert alarm of §12b.2 once (an
`IndexInsertCollision` incident, see [Integrity incidents](#integrity-incidents)),
and from then on it routes neither: a request for that `ehr_id` that names
no node is refused `409` (`ehr-id-collision`). A held collision has no expiry
of its own, because nothing the gateway observes shows that a node was
remedied. It lasts until the entry is forgotten as least recently used or the
gateway restarts; after that, a read probes every member again, and a
collision that still stands is found and reported again.

## Reloading the registry

Send `SIGHUP` to a running `ferrofed serve` to apply a changed registry
document without a restart:

```text
kill -HUP <pid of ferrofed>
docker kill --signal HUP <container>
```

The gateway reads the configuration again from where it read it at start:
the `--config` file, or the file `FERROFED_CONFIG` names, with the process's
`FERROFED__` environment over it. It checks the result exactly as `serve` and
`config check` do at start, secrets and `_file` siblings included. The
gateway reloads on the signal only and never watches the file, so write the
new document completely, then send the signal.

Five sections take effect on a reload:

| Reloaded | Needs a restart |
|---|---|
| `[registry]`: the document's contents, its path and its `format` | `[server]` |
| `[credentials]` | `[signing]` |
| `[dev]` | `[telemetry]` and `[metrics]` |
| `[pixm]` | `[federation]`, `federation.demographic_endpoint` included, and `[stored_queries]` |
| `[xcpd]` | |

`federation.demographic_endpoint` keeps its running value until a restart,
and the document must still declare it: a reload whose document drops that
endpoint is refused (`demographic-endpoint`, below).

A reload whose file changes `profile` is refused (`profile`, below), so
everything the development profile admits, the development cross-reference
and consent table, and a credential or patient identifier over plain `http`
([What must travel over https](configuration.md#what-must-travel-over-https)),
follows the profile the process started with.

A valid configuration replaces the running registry at once. A request that
started before the reload finishes on the registry it started with, nodes
and credentials included; every request that starts after it uses the new
one. An added endpoint gets its node client and its credentials, and a
removed endpoint is never called again. What the gateway has learned stays,
held to the new document:

- a learned `creating_system_id` route the new document maps to another node
  is withdrawn and raises a `RegisteredCreatingSystemConflict` incident (see
  [Integrity incidents](#integrity-incidents)), and stays withdrawn;
- every `ehr_id` index entry and resolution binding that names a member the
  document no longer holds is dropped. An entry that names such a member
  beside others is dropped whole, so a collision is never narrowed to the
  member that remains; a later read asks every member again. An entry a
  request already running learns after the reload, naming a member that
  left, is dropped the first time a request looks it up, with the same
  effect: a read asks every member, and a write without a target header is
  refused `400` (`target-required`).

The reload logs `registry reloaded` at `INFO` with `members` (how many the
registry now holds), `endpoints_added`, `endpoints_removed`,
`members_removed`, `incidents`, `index_dropped` and `bindings_dropped`. A
changed setting outside the four sections is logged at `WARN` under
`settings`, by key (`server.listen`, `federation.binding_ttl_ms`), and keeps
its running value until a restart; the rest of the reload applies.

A configuration that does not load is refused, and the running registry
stays. The gateway logs `registry reload refused` at `ERROR` with the failure
`class`, the `config` file and the registry `document`, and never a value of
either file, a credential or a header. Run `ferrofed config check` against
the same file to see the fault. The classes are:

| `class` | The fault |
|---|---|
| `configuration` | the configuration file does not read or resolve |
| `registry-unreadable` | the registry document, or the directory, cannot be read |
| `registry-invalid` | the registry document, or the directory's content, breaks a registry rule |
| `registry-directory` | the directory of `[registry.mcsd]` cannot be asked as configured |
| `credentials` | a `[credentials]` section names an endpoint the document does not declare |
| `demographic-endpoint` | `federation.demographic_endpoint` names an endpoint the new document does not declare |
| `dev-cross-reference`, `pixm`, `resolvers` | the resolver refuses the new members, or both resolvers are set |
| `localization` | the localizer refuses the new members, or the node selection has none |
| `node-clients`, `http-client`, `self-description` | the node clients or the `OPTIONS {base}/` body cannot be built |
| `registry-presence` | `registry.document` or `[registry.mcsd]` was set, unset or swapped for the other, which takes a restart |
| `profile` | `profile` was changed, which takes a restart |
| `cleartext` | a credential or a patient identifier would travel over a URL that is not `https`, outside the development profile the process started with |

Reloading uses a Unix signal, and FerroFED runs on Unix only
([Supported platforms](deployment-shape.md#supported-platforms)). Each
reload is counted by `result` on the [metrics](metrics.md) surface, and the
log lines record what it changed.

## Integrity incidents

A federation integrity defect is reported to you, the federation operator, as
an incident: one `ERROR` line under the log target `ferrofed::integrity`,
written once when the gateway detects the defect (§12.5.2, §12b.2, N42). The
line carries a stable `kind`, the routing ids involved and a message, and
never a request body, a header value or a patient identifier. These kinds
reach the log today:

| `kind` | When | Fields |
|---|---|---|
| `EhrIdCollision` | A request addressed an `ehr_id` that two members or more claim, and was refused `409` (`ehr-id-collision`). One line per refused request. | `ehr_id`, `detection` (`binding`, `index` or `ask-all`: the routing step that found the claimants), `claimants` (their endpoint ids) |
| `IndexInsertCollision` | The `ehr_id` index learned an `ehr_id` it already held at another member: the index-insert alarm of §12b.2. One line when the second claimant is learned, and one more for each further claimant. | `ehr_id`, `claimants` (the member node ids) |
| `LearnedCreatingSystemConflict` | A `creating_system_id` the registry document does not map was seen at two nodes, so the route learned for it is withdrawn and neither node is routed on (§12.2, N21). One line when the route is withdrawn. | `creating_system_id`, `first_endpoint_id`, `second_endpoint_id` |
| `RegisteredCreatingSystemConflict` | A route learned for a `creating_system_id` names another node than the registry document maps it to, seen in an answer or found when the registry is reloaded (see [Reloading the registry](#reloading-the-registry)). The learned route is withdrawn and the document's mapping is used. One line when the route is withdrawn. | `creating_system_id`, `node_id` (the node the document maps it to), `endpoint_id` (the endpoint the learned route named) |

The `ehr_id` is node-local and names no patient (§5.2), so the line names it
when it is a bare UUID. Any other form could be a patient identifier a client
wrote in a path, so the line then leaves the `ehr_id` field out.

Two nodes holding one `ehr_id` breaks the identifier-integrity conditions of
§12b.2, which admission should have checked, so the remedy is at the node.
The `claimants` name the members that hold the `ehr_id`. Have the node that
issued or adopted it in error fix it, then restart the gateway, which forgets
the collision the index holds (the index also forgets it when the entry is
the least recently used one past the index capacity). Until then, requests
that name no node are refused, and a client can still reach one of the
members by naming its endpoint in the `openEHR-federation-endpoint` header.

The gateway counts every incident by `kind` on its [metrics](metrics.md)
surface, `ferrofed_integrity_incidents_total`, and the line carries the
routing ids you act on. The request line of a refused request carries
its `409` and its `request_id`; the incident line does not name the request.
