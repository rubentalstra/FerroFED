<!-- SPDX-FileCopyrightText: Vernum Projecten B.V. -->
<!-- SPDX-License-Identifier: BUSL-1.1 -->

# Identity resolution

A patient query names its patient by an identifier and that identifier's
issuing namespace. The gateway resolves the pair, outside the query, to the
local `ehr_id` each member holds for the patient, and sends each member a
query scoped to that `ehr_id` alone (§5.2, N3, N7). This page covers the
cross-reference the gateway asks and how you configure it.

See how it works: [where the patient identifier stops](../how-it-works/identifier-hygiene.md).

## Where the identifier comes from

The gateway reads the patient on either carrier §5.4.3 names (N33, CP-38):

- the subject predicate of §7,
  `e/ehr_status/subject/external_ref/id/value = '…'`, with the namespace in
  `e/ehr_status/subject/external_ref/namespace`;
- an `ENTRY`-level subject, `…/subject/identifiers/id = '…'`, with the
  namespace in `…/subject/identifiers/issuer` or `…/subject/identifiers/type`.

`GET {base}/v1/ehr?subject_id=…&subject_namespace=…` names the patient in
its query string instead
([Reading an EHR by subject](../integrate/follow-ups.md#reading-an-ehr-by-subject)).

A query that names no namespace resolves in `federation.default_namespace`
when you set one, and is refused `400` (`no-namespace`) when you do not.
§5.2 requires a namespace and names no default, so the setting is
FerroFED's own:

```toml
[federation]
default_namespace = "urn:oid:2.999.1"
```

## A PIX Manager: `[pixm]`

The identity binding FerroFED ships is IHE PIXm, the Patient Identifier
Cross-reference Manager's ITI-83 query (Annex A.1). Each registry member is
resolved by one Manager, in its `ehr_id` domain there: the assigning
authority whose identifiers are that member's `ehr_id`s.

```toml
[[pixm.manager]]
url = "https://pix.example.org/fhir/"

[pixm.manager.members]
"hospital-a" = "urn:oid:2.999.10"   # a registry node id = its ehr_id domain
"clinic-b" = "urn:oid:2.999.20"

[pixm.manager.credentials]
bearer_token_file = "/run/secrets/pix-token"

[pixm.namespaces]
"2.999.1" = "urn:oid:2.999.1"       # a client namespace = the PIX assigning authority
```

- Name one `[[pixm.manager]]` per Manager. Every member of the registry is
  resolved by exactly one: a member no Manager names, a member two Managers
  name, and a name the registry does not hold each refuse the configuration.
- Each domain, and each value of `[pixm.namespaces]`, is an absolute URI.
- `url` is the Manager's FHIR base, `https`, with no query or fragment. The
  Manager is asked for patient identifiers, so `http` is refused naming
  `pixm.manager[N].url`, with or without credentials, unless the profile is
  `development`
  ([What must travel encrypted](configuration.md#what-must-travel-encrypted)). A user name or password in it refuses the configuration, naming
  `pixm.manager[N].url` and never quoting it; the credentials go in
  `[pixm.manager.credentials]`, with the keys of an endpoint's
  [credentials](configuration.md#the-file): `bearer_token`, or `user` and
  `password`, each with its `_file` sibling. Leave the section out when the
  transport, such as mutual TLS or a private network, authenticates the
  gateway.
- `[pixm.namespaces]` maps the namespace a client writes to the assigning
  authority the Manager knows it by. A namespace that is itself an absolute
  URI needs no entry.

For each query, the gateway asks each Manager once, with one `targetSystem`
per member that Manager resolves, and reads the identifier the answer holds
in a member's domain as that member's `ehr_id`. The patient identifier goes
to the Manager and nowhere else; no node, log line or error body carries it
(§5.4.1, N33).

| The Manager answers, for one member | The member is | The query |
|---|---|---|
| exactly one identifier in the domain, which reads as an `ehr_id` | sent its node query | goes on |
| no identifier in the domain, or the patient is unknown | `not-resolved` | goes on; `complete` is false (N6) |
| two identifiers, one that is no `ehr_id`, an error, no answer within the budget, or a namespace with no mapping | `not-resolved`, with the reason in `error` | fails `424` under all-or-nothing |

A member the Manager could not answer for may hold the patient, so the
gateway fails the query by default rather than answer without it (§11.1;
§11.3 covers only an answered lookup, so this rule is FerroFED's own). With
`openEHR-federation-completeness: partial`, the members that did resolve
answer. Resolution runs inside the request's overall budget
([Timeouts](queries-and-areas.md#timeouts)).

### `[pixm]` as the localizer

Under `federation.node_selection = "localized"` with no `[xcpd]`, the
`[pixm]` resolver is also the localizer, the "demographic-registration" kind
of §14.2. The candidates are the members whose domain holds an identifier
for the patient at its Manager. Every other member is `not-localized` and is
never asked. The resolution of the same query reuses that ITI-83 answer, so
each query still asks each Manager once.

A member whose domain holds two identifiers, or one that is no `ehr_id`, is a
candidate, because the Manager holds the patient there; its resolution then
reports why it could not be asked. A Manager that fails, answers in a form
the gateway cannot read, or does not answer within
`[federation.localization] timeout_ms` fails the localization closed
(§14.1): every member is `not-localized` with the error, and no member is
asked unless `on_failure = "ask-all"`. `OPTIONS {base}/` declares
`localization.mode` as `"pixm"`. With `[xcpd]` set, XCPD localizes and
`[pixm]` only resolves.

## The development cross-reference: `[dev]`

For a laptop or a test, the gateway can resolve from a static table in the
configuration instead. It binds nothing, it is no identity binding of N3, and
it is accepted only in a configuration that declares itself for development
(no specification governs this: our own design):

```toml
profile = "development"

[[dev.crossref]]
namespace = "urn:oid:2.999.1.1"
value = "ffd-test-0001"
member = "node-a"
ehr_id = "aaaaaaaa-aaaa-4aaa-8aaa-000000000001"
```

Each row maps one synthetic patient to its `ehr_id` at one member; a patient
with no row at a member is `not-resolved` there. Under any other `profile`,
`[dev]` refuses the configuration. A development deployment prints a red
notice in its [startup banner](configuration.md#the-startup-banner) that it
must not hold or reach real patient data. The
[quickstart](container.md#the-quickstart) runs on this table.

## Consent

Each node checks consent before it releases data, whatever the gateway did
first (N26, N27). The gateway never decides on release itself, and a node it
dispatches to is not thereby cleared: a localization or consent service that
named the node only means nothing upstream ruled it out (§14.3).

**A node's own refusal.** ITS-REST defines no consent signal, so the gateway
does not infer one from a status code. A node's answer is reported
`consent-denied` only when it is a `403` whose ITS-REST `Error` body carries a
`code` that the registry lists for that endpoint in `consent_refusal_codes`
([The registry](registry.md#the-registry-document)). Every other refusal is
`node-error`. The list is empty by default, so until you name the codes a
node uses, its consent refusal fails the query `424` as any node error does.
This key is FerroFED's own design, because no specification defines the
signal. A refusing node contributes no rows, never fails the query in either
completeness mode, clears `meta.federation.complete`, and its record carries
the `latency_ms` of the request it refused (§11.3, N40).

**The optional Step-1 pre-filter.** A deployment with a consent service may
drop members before dispatch (N27a). The pre-filter runs after localization
and before resolution. Each member it denies is reported `consent-denied` with
no `latency_ms`, is never resolved and never sent a request, and any `ehr_id`
the client session cached for it is dropped. A member it does not deny is
asked, and its node decides. When the consent service cannot answer, Step 1
carries no consent signal, which is the state of a deployment with no consent
service at all, so every candidate is asked and each node checks consent
itself (§13.2.1, N27a). This `pass-to-node` policy is FerroFED's own design.
The outage is never silent: the answer to a query carries it as
`meta.federation.consent.error`, mirroring `localization.error` of §14.1
([The client contract](../integrate/client-contract.md)), the pre-filter's
state on `GET {base}/health/dependencies` turns `down` or `failing`
([Health probes](health.md)), and each call is counted in
`ferrofed_consent_prefilter_requests_total` ([Metrics](metrics.md)). The
pre-filter applies to every patient route: a federated query and the read
of an EHR by subject
([Follow-ups](../integrate/follow-ups.md#reading-an-ehr-by-subject)).
`OPTIONS {base}/` declares a configured pre-filter under `federation.consent`,
with its mode and that policy; a deployment with no pre-filter declares
nothing there.

For development, rows under `[[dev.consent_denied]]` beside the
cross-reference are a static pre-filter, accepted only under
`profile = "development"` and declared as `development-static`:

```toml
[[dev.consent_denied]]
namespace = "urn:oid:2.999.1.1"
value = "ffd-test-0001"
member = "node-b"        # this patient's consent denies asking node-b
```

The pre-filter of the Dutch binding, Mitz, is planned for v0.0.8
([#87](https://github.com/FerroHEALTH/FerroFED/issues/87)).

## Choosing one

Set `[pixm]` or `[dev]`, never both: both refuse the configuration. With
neither, the gateway still starts, and every patient query fails closed: each
member is `not-resolved` with "no cross-reference service is configured",
and the query is a `424`. A query that names no patient needs no resolution
and runs as written.

Both sections take effect on a
[reload](registry.md#reloading-the-registry). `GET {base}/health/dependencies`
reports the resolver's last observed state
([Health probes](health.md)).

Under `federation.node_selection = "localized"` the `[dev]` table is also the
localizer: it names the members that hold a row for the patient, and every
other member is `not-localized`
([Node selection](registry.md#node-selection)).

## XCPD localization: `[xcpd]`

The `[xcpd]` table makes the gateway an IHE XCPD Initiating Gateway, the
specification's proposed localization binding (N4, §14.1, Annex A.3). Under
`federation.node_selection = "localized"`, each undirected patient query
first asks every configured Responding Gateway, by the patient's identifier
alone, which communities hold the patient (ITI-55 Cross Gateway Patient
Discovery, ITI TF-2 §3.55, Revision 20.1). The members serving those
communities are the candidates; every other member is `not-localized` and is
not asked. A match is a candidate to ask, never a release decision: each
node still enforces consent (§14.3).

```toml
profile = "production"

[federation]
node_selection = "localized"

[federation.localization]
on_failure = "closed"   # the default (§14.1)
timeout_ms = 5000

[xcpd]
sender_device = "2.999.40.1"           # the gateway's device OID
home_community = "2.999.40"            # optional: the gateway's own community
audit = "log"                          # required: "log", or "off" in development
assertion_file = "/run/secrets/xua.xml"            # optional
client_identity_file = "/run/secrets/xcpd-client.pem"
trust_roots_file = "/etc/ferrofed/xcpd-roots.pem"  # optional

[[xcpd.gateway]]
url = "https://xcpd.region.example.org/RespondingGateway"
device = "2.999.50.1"                  # the receiver device OID
# community = "2.999.50"               # optional: ask for this community only

[xcpd.communities]                     # every member needs one
"2.999.50" = "node-a"
"urn:oid:2.999.60" = "node-b"

[xcpd.namespaces]                      # only for a namespace that is no OID
"region-mrn" = "2.999.1"
```

The request names the patient by the shared identifier mode of ITI-55:
one `LivingSubjectId` whose `root` is the assigning authority and whose
`extension` is the value, with no name, birth date or other demographics.
A namespace that is an OID, dotted or as `urn:oid:`, is the assigning
authority; any other needs an entry in `[xcpd.namespaces]`. A namespace with
no mapping fails the query closed.

What a deployment must provide:

- **The device OIDs** of the gateway (`sender_device`) and of each
  responding gateway (`device`). ITI TF-2 Appendix O requires an ISO OID for
  each, and every identifier here is refused at boot unless it is one.
- **The community map.** Every registry member must be served by a
  community, or boot is refused, since no discovery could ever name it. A
  community a gateway answers that the map does not name belongs to no
  member and adds no candidate.
- **TLS.** Every XCPD actor is an ATNA Secure Node or Secure Application
  (ITI TF-1 Table 27.1.3-1), so a gateway URL must be `https`;
  `client_identity` (or `client_identity_file`) holds the PEM client
  certificate chain and private key for mutual TLS, and `trust_roots_file`
  adds the network's roots to the platform's. A plain `http` URL is refused
  at boot, naming its key, unless the configuration is
  `profile = "development"`.
- **The XUA assertion, where the network requires one.** ITI-55 requires
  none, but many networks require a SAML 2.0 assertion (IHE XUA, ITI-40).
  The gateway signs nothing: your identity provider or security token
  service issues and signs the assertion, and the gateway sends its bytes
  unchanged in a WS-Security header, so its signature still verifies.
  `assertion` (or `assertion_file`) must hold exactly one
  `saml2:Assertion` element that declares every namespace prefix it uses;
  anything else is refused at boot. An assertion expires: replace the file
  before its `NotOnOrAfter` and [reload](registry.md#reloading-the-registry).
- **An audit destination.** The Initiating Gateway records an audit message
  for every exchange (ITI TF-2 §3.55.5.1.1), so `audit` has no default.
  `audit = "log"` writes each message as a structured event at the log
  target `ferrofed::audit`: the event, its outcome (`0` success, `4` the
  gateway answered with a failure, `8` no answer), this process's id, the
  responding gateway's endpoint and host, and the `homeCommunityId` the
  request named. The query parameters, which name the patient identifier,
  are never logged; the event says only that they were recorded. Route that
  target to your audit repository. `audit = "off"` records nothing and is
  refused outside `profile = "development"`. `OPTIONS {base}/` declares the
  choice as `localization.audit`. A message the destination cannot accept
  fails the discovery closed under every policy, `on_failure = "ask-all"`
  included, since that policy covers a localizer outage and never an
  exchange the gateway could not audit: no answer is used without its audit,
  and no member is asked.

The discovery fails closed as a whole. One responding gateway that faults,
answers an error (Case 5 of §3.55.4.2.3), asks for demographics (Case 3),
answers outside ITI-55, or stays silent past `timeout_ms` leaves every member
`not-localized` with the error, and the query asks no node; a community
behind that gateway might hold the patient. `on_failure = "ask-all"` asks
every member instead ([Node selection](registry.md#node-selection)).
`OPTIONS {base}/` declares `localization.mode` as `"xcpd"`.

FerroFED sends the synchronous exchange only, with an immediate response: it
claims neither the Asynchronous Web Services Exchange nor the Deferred
Response option (ITI TF-1 §27.2), and it caches no correlation between
queries. `[xcpd]` takes effect on a reload, and under
`node_selection = "ask-all"` it refuses the configuration.

PMIR notifications of a merge or split are planned for v0.0.8
([#147](https://github.com/FerroHEALTH/FerroFED/issues/147)).
