<!-- SPDX-FileCopyrightText: Vernum Projecten B.V. -->
<!-- SPDX-License-Identifier: BUSL-1.1 -->

# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Maintenance rule: every pull request that changes user-visible behaviour adds
an entry under **[Unreleased]** in the same PR. Cutting a release renames
[Unreleased] to the version and date, and adds a fresh link reference.

The architecture is `docs/architecture.md`, the output of the research
program on the v0.0.1 milestone. Releases on the 0.0.x line started with the
repository, its gates, its documentation and the server shape in 0.0.1; the
federated query and identity resolution shipped in 0.0.3; cross-node result
shaping, single-node routing and the targeting mechanisms in 0.0.6.

## [Unreleased]

### Added

- The consent pre-filter applies to the read of an EHR by subject as it does
  to a federated query (#399; N27a, §13.2.1). A member it denies is never
  resolved and never sent a request. When it denies every member that might
  hold the subject and no other member holds one, the answer is
  `403 consent-denied` naming the denied endpoints, never the `404` of a
  subject with no EHR. A pre-filter that cannot answer leaves every member to
  its own consent check, as on a query.
- A consent pre-filter outage is visible (#400). The answer to a query asked
  while the pre-filter could not answer carries `meta.federation.consent.error`,
  beside the `localization.error` of §14.1, with `complete` and the status
  unchanged. `GET /health/dependencies` reports the pre-filter as `consent`
  under the members' rule (a decision or an answer below `500` is `up`, a
  `5xx` is `failing`, no answer is `down`), absent when none is configured.
  Each call is counted in `ferrofed_consent_prefilter_requests_total` by
  `outcome` (`denied`, `no-signal`, `unavailable`). The carriers are
  FerroFED's own design; no specification names one.

- `federation.node_selection = "localized"`: an undirected patient query gets
  its node set from a localizer (N4, N10, §14.1). A member the localizer does
  not name is reported `not-localized` and never asked. A localizer that fails
  or stays silent past `federation.localization.timeout_ms` fails closed: no
  member is asked, every member is `not-localized` with the localizer's error,
  and `meta.federation.localization.error` carries it too.
  `federation.localization.on_failure = "ask-all"` widens to every member
  instead, and `OPTIONS {base}/` declares the policy, the binding as
  `localization.mode` and the budget as `timeout.localization_ms`. A directed
  query is never localized (§8). The development cross-reference serves as
  the localizer under `profile = "development"`; the IHE XCPD binding follows
  (#85).
- Consent stays with the node, and a node's consent refusal can be reported
  as one (#83; §13.2, §13.2.1, §11.1, §11.3, N26, N27, N27a, N40, CP-36).
  ITS-REST defines no consent signal, so a node's `403` is `consent-denied`
  only when its ITS-REST `Error` carries a `code` the registry lists for that
  endpoint in the new `consent_refusal_codes` key (in the FHIR form, one
  `https://ferrofed.eu/fhir/StructureDefinition/consent-refusal-code`
  extension per code). The list is empty by default, so every refusal stays
  `node-error` until you name your nodes' codes; the key is FerroFED's own
  design. A consent-denied node contributes no rows, carries the latency of
  the request it refused, clears `meta.federation.complete`, and fails the
  query in neither completeness mode. The optional Step-1 consent
  pre-filter runs after localization and before resolution: each member it
  denies is reported `consent-denied` with no `latency_ms`, is never
  resolved or contacted, and loses any `ehr_id` the session cached for it. A
  member it does not deny is asked, and its node decides. When the pre-filter
  cannot answer, every candidate is asked (`pass-to-node`), because the node
  checks consent in any case. `OPTIONS {base}/` declares a configured
  pre-filter under `federation.consent`. For development,
  `[[dev.consent_denied]]` rows beside the cross-reference are a static
  pre-filter, accepted only under `profile = "development"` and declared as
  `development-static`.
- A "How it works" part of the book, right after the introduction, explains
  the gateway in Mermaid diagrams (#395; no specification governs the
  documentation: our own design). Its pages cover a federated query end to
  end with each member's status and the answer's status (§4, §11, N37), where
  the patient identifier stops (§5.4, N33), follow-ups and writes (§12, §12a,
  N41, N42), definitions and stored queries (§12.6, §12.7, N43, N44), trust
  and keys (§13.1, N25), and the quickstart and production layouts. Each
  diagram cites the sections it shows, and a planned part is drawn dashed
  with its issue and milestone. The pages of the other parts link to the
  diagram that belongs to them. The diagrams follow the book's theme when
  you choose "Auto" too: `website/book/js/mermaid-theme-sync.js` redraws them
  when the theme crosses between light and dark, which the vendored
  mdbook-mermaid script does only on a click of a named theme.
- The gateway authenticates to a node as itself with OAuth 2.0 client
  credentials and a signed JWT client assertion, the default onward mechanism
  of §13.1 (N25, CP-17 onward half, #81). A `[credentials."<endpoint
  id>".oauth2]` section names the node's token endpoint, the `client_id` and
  the scope, written in the SMART on openEHR grammar and held to the `system`
  compartment, with optional `resource` and `audience`. Each assertion is
  ES384, names the client as `iss` and `sub` and the token endpoint as `aud`,
  lives at most 300 seconds and carries a fresh `jti` (RFC 7523 §3). A token
  is cached until 30 seconds before it expires, with one token request per
  endpoint at a time, and a node's `401` drops it. The admission check
  authenticates the same way.
- `[signing]` holds the gateway's ES384 signing key, read from a file, and
  during a rotation the previous key, published beside it for a configurable
  overlap window of at least the assertion lifetime plus the nodes' JWK Set
  cache time. The gateway serves its public keys as a JWK Set (RFC 7517) at
  `GET {base}/.well-known/jwks.json`, each with its RFC 7638 thumbprint as
  `kid`, and declares the configured location as `federation.auth.jwks_uri` in
  `OPTIONS {base}/` (§13.1, N30).
- XCPD localization (#85, Annex A.3): `[xcpd]` makes the gateway an IHE XCPD
  Initiating Gateway (ITI-55, ITI TF-2 Revision 20.1). An undirected patient
  query asks every configured Responding Gateway by the patient identifier
  alone, and only the members whose communities a gateway names are asked.
  One gateway that faults, answers an error, asks for demographics or stays
  silent fails the discovery closed. Gateway URLs are `https` outside
  `profile = "development"`; a client certificate, extra trust roots and a
  SAML XUA assertion your identity provider signed are configured with
  `_file` keys, and the assertion is sent unchanged. `OPTIONS {base}/`
  declares `localization.mode = "xcpd"`, and `[xcpd]` takes effect on a
  reload.
- `ihe-iti` 0.0.11: the `xcpd` feature, the ITI-55 Initiating Gateway client.
  It is the only feature that compiles `uuid` and `jiff`, and it adds no
  crate to the graph beyond them.
- The read of an EHR by subject is localized (#409; N4, §5.2, §14.1): under
  `node_selection = "localized"` the subject is resolved only at the members
  the localizer names, and no other member learns of the request. A
  localizer that fails closed answers `424 localization-unavailable` with no
  member asked; a targeted read is never localized (§8).
- The localizer on `GET /health/dependencies` as `localizer`, under the
  members' rule, and its calls in `ferrofed_localizer_requests_total` by
  `outcome` (#410).
- The ITI-55 Initiating Gateway audit message (#410, ITI TF-2 §3.55.5.1.1):
  `ihe-iti` 0.0.12 hands every exchange's message to an `AuditRecorder`, and
  `[xcpd] audit = "log"` writes it at the `ferrofed::audit` log target
  without the query parameters, which name the patient. A message the
  recorder cannot accept fails the discovery closed under every
  `on_failure` policy, `ask-all` included, and shows the localizer
  `failing`.
- The PIXm resolver as the localizer (#408; §14.2, N4, §14.1): under
  `node_selection = "localized"` with `[pixm]` and no `[xcpd]`, the members
  whose domain holds an identifier for the patient at the PIX Manager are
  the candidates, and the resolution reuses the same ITI-83 answer, so each
  query asks each Manager once. A Manager that fails or stays silent fails
  the localization closed. `OPTIONS {base}/` declares
  `localization.mode = "pixm"`.
- Client authentication at the gateway (§13.1, N25, CP-17 inbound half,
  #80). A request to the ITS-REST surface and `OPTIONS {base}/` carries an
  RFC 9068 access token from an issuer on the `[[auth.issuer]]` trust list,
  verified against the issuer's JWK Set (fetched from `jwks_uri`, read from
  `jwks_file`, or given inline as `jwks`) or by RFC 7662 introspection. The
  key set is cached, refetched once for a key it does not hold and at most
  once per `auth.key_set_refetch_s`. Scopes are read in the SMART on openEHR
  grammar with `openehr-sdt`, and one table maps every ITS-REST operation to
  what it requires. A purpose of use, in the IHE IUA extension or RFC 9396
  `authorization_details`, is required unless
  `auth.purpose_of_use.required = false` (§13.4). The verified caller rides
  in the request's extensions. An explicit edge mode verifies a proxy's
  signed assertion in a configured header and logs the identity it asserted.
  New codes: `unauthenticated` (401), `scope-insufficient`,
  `purpose-of-use-required` and `operation-refused` (403), and
  `authentication-unavailable` (503). The book has a new page, Client
  authentication.
- `scripts/quickstart/token.sh` mints the token the quickstart gateway
  accepts, from a development issuer whose key pair it generates with
  `openssl` on its first run; the quickstart and the release example
  configuration carry an `[auth]` section.
- The testkit's test issuer: key pairs generated per run, its JWK Set served
  from a mock server, and tokens minted with chosen claims.
- The caller's identity on every request to a node (#82; §13.1, N24, N25,
  §12.4, CP-16). The federated query, every routed read and write, a
  definition request, the template and stored-query fan-outs, the ask-all
  probe and the read of an EHR by subject each carry
  `openEHR-federation-client`: an ES384 JWS, typed
  `openehr-federation-client+jwt`, signed for that one node with the
  gateway's `[signing]` key, the key its JWK Set publishes. Its `aud` is the
  node's endpoint id, `exp` is 60 seconds after `iat`, `jti` is fresh, and
  `iss` is the `client_id` of the endpoint's OAuth 2.0 grant or else the
  federation id. It names the verified caller (`sub`, `iss_upstream`), how
  the gateway verified it (`verified_by`: `signature`, `introspection` or
  `edge`), the caller's organisation (`subject_organization_id`), each
  purpose of use and the scopes as granted, so a node can apply them (N26).
  The admission check and the admin listener's redistribution name the
  gateway itself. The book's
  [Client authentication](https://ferrofed.eu/docs/operate/authentication.html#what-a-node-is-told-about-the-caller)
  page gives the claims and the steps a node verifies the token with. The
  header and its claims are FerroFED's own; §13.1 leaves end-user conveyance
  open.
- `scripts/quickstart/signing-key.sh` writes the compose quickstart's
  development signing key before the first `docker compose up`, and the
  quickstart and the release example configuration carry `[signing]`.

### Changed

- `[signing]` is required whenever `registry.document` is set: a federating
  gateway without a signing key refuses to start, and `config check` refuses
  its configuration with exit code 78, naming the key.
- The ADMIN API under `{base}/v1/admin/` answers `403` (`operation-refused`)
  to every caller, where it answered `501`.
- A gateway that federates and trusts no issuer refuses to start, and
  `config check` refuses its configuration, with exit code 78.
- `[xcpd]` requires `audit` (#410): `"log"`, or `"off"`, which only
  `profile = "development"` accepts and `OPTIONS {base}/` declares as
  `localization.audit`. A configuration without it refuses to boot.
- `ihe-iti` 0.0.12: `XcpdError::Fault` carries the HTTP status it came with,
  `XcpdError::status()` names the status the responding gateway answered
  with, an unreadable answer with a status other than `200` is `Rejected`,
  and `XcpdError::Audit` reports an audit message the recorder refused.
- An onward credential that cannot be obtained fails that node as
  `node-error` with nothing sent to it, on the federated query, the
  definition fan-out and a routed request (`424 node-error`), where it was a
  gateway error before (§13.1, §11.1).
- The localizer error a member and `meta.federation.localization.error`
  carry names the status the localization service answered once, with the
  binding's own reason, for PIXm and XCPD alike (#420). The log keeps the
  whole cause chain.

### Security

- A node is never sent a request without its configured onward credential,
  and never the caller's own `Authorization` header.
- The `error` a client sees for a node no token could be obtained for is a
  fixed sentence and, when the token endpoint refused with one, its
  registered RFC 6749 §5.2 code. The token endpoint's description, its
  address and the network error go to the log alone, and the assertion and
  the token go nowhere.
- A token endpoint URL with a user name, a password, a query or a fragment is
  refused at load, so no secret can ride in it into a log.
- No request reaches a node, a cross-reference service or a store before its
  caller is verified. A token signed with `none` or an HMAC, of another
  `typ` than `at+jwt`, from an untrusted issuer, for another audience, or
  outside its validity window is refused, and an issuer that cannot be asked
  fails closed with `503`. A `patient/` scope admits nothing, because the
  gateway cannot bind it to the token's patient context, and a
  `system/aql-*` scope counts only for a listed backend client. The
  DEMOGRAPHIC API admits only the clients an issuer lists in
  `demographic_clients`. The client's token is never forwarded to a node.
- Outside `profile = "development"`, `serve`, `config check`,
  `admission check` and every reload refuse a credential sent to a URL that
  is not `https`, naming the key of the URL and of the credential: a registry
  endpoint with a `[credentials]` section, an `oauth2` token endpoint, a PIX
  Manager with credentials, and a `metrics.otlp_endpoint` carrying a user
  name or a password. A reload keeps the profile the process started with.
  Under the development profile the same configuration starts, and the
  startup banner, a `WARN` log line and `config check` name each credential
  that travels unencrypted, by key, an XCPD responding gateway's XUA
  assertion included (#402).
- A PIX Manager and an XCPD responding gateway are sent patient identifiers,
  so outside the development profile each must be `https`, with or without
  a credential configured. Under the development profile they are named in
  the banner, the log and `config check` like a cleartext credential (#402).
- A reload whose file changes `profile` is refused with class `profile`, and
  the running configuration stays, so nothing the development profile admits
  can enter a process that started under another profile. A reload under
  another profile could admit an `http` XCPD gateway before (#402).
- One module holds every transport rule: a URL a credential or a patient
  identifier is sent to must be `https` outside the development profile,
  and a key set or introspection endpoint the gateway verifies callers
  against must be `https`, or `http` to loopback, under every profile (#402).
- Outside the development profile, the stored-query store's PostgreSQL
  connection string must set `sslmode=require` when it carries a password
  to a networked host: `disable` and `prefer`, the driver's default, which
  falls back to no TLS, are refused naming `stored_queries.url` by
  `config check`, the start and a reload, even though the store itself
  changes only on a restart. Under the development profile it starts and is
  named in the banner, the log and `config check` (#416).
- No request reaches a node without the caller's identity: one that reaches
  the dispatcher with no verified caller is answered `500` and nothing is
  sent. The conveyed token never carries a patient identifier (§5.4.1, N33):
  it holds no IUA `person_id`, and the outbound gate reads every caller
  claim, refusing with nothing sent a request whose claims would carry the
  identifier its query was resolved on. A client's own
  `openEHR-federation-client` header never reaches a node.

## [0.0.7] - 2026-10-03

The v0.0.7 milestone, definitions and membership (§12.6, §12.7, §12b). A
versioned write goes only to the CDR that created the version. A template goes
to the node you name, or to several on request, with each member's outcome
reported. Stored queries live at the gateway as immutable versions, on an
embedded store, PostgreSQL or a read-only directory, distributed to the
members with drift reported and repaired from the admin listener. A check you
run before admitting a node tests the admission conditions. The gateway
exports metrics through OpenTelemetry, as Prometheus and over OTLP, records
each member's state from the calls it makes, holds every credential in a type
that never renders it, and prints a startup banner. Every release now carries
a compose file that starts the gateway alone in front of the CDRs you already
run. The gateway still authenticates no client; that is v0.0.8 (#80).

### Added

- Every release carries `compose.yaml`, `ferrofed.toml` and `registry.toml`,
  which start the gateway alone in front of the CDRs you already run, with no
  checkout of the repository (#385; no specification governs packaging: our
  own design). You download the three, edit the two TOML files (the gateway
  configuration and the registry document, in their documented formats, every
  value to change marked `EDIT`), put each credential in a file under
  `secrets/`, and run `docker compose up --wait`. The compose file runs the
  published `ghcr.io/ferrohealth/ferrofed` image at the release's version and
  mounts the two files and `secrets/` read-only; its only variables are about
  the container (image tag, host address and port, CPU and memory). The
  gateway runs read-only, as uid 65532, with every capability dropped,
  `no-new-privileges`, CPU and memory limits, a 20-second stop grace period,
  the `ferrofed healthcheck` probe and the `unless-stopped` restart policy, so
  a configuration it refuses (exit 78, naming the key) is retried at Docker's
  doubling delay. The release lane attaches the three files to the draft and
  refuses to publish a draft missing any of them, `scripts/checks/versions.sh`
  and the release plan hold the image tag to the product version, and the CI
  job `release compose` renders the compose file and runs `ferrofed config
  check` over the two examples as attached. The repository's `compose.yaml`
  stays the four-node quickstart. The container page of the book has a
  section "The gateway from a release".
- The operator can send a stored-query version the registry holds to the
  members that missed it (#342; §12.7 stored-query-drift, N44, CP-40; no
  specification gives drift repair a request, so this is our own design).
  `POST /admin/stored-queries/{name}/{version}/distribute` on the admin
  listener (`[metrics] listen`), with no body and the members in the
  targeting headers, sends the registry's held copy to them and changes
  nothing at the registry. The answer has the shape and statuses of a first
  distribution, and its new `meta.registry` member says `held`, where a
  first distribution's says `stored`. It is `404` for a version the
  registry does not hold, `400` for a request naming no member, a body, a
  deployment without distribution or an endpoint-targeted definition, and
  `405` with an empty `Allow` at a read-only registry. Without the admin
  listener the action does not exist. Every second `PUT` of a held version
  stays `409` (`stored-query-held`), the same query naming members
  included. The book's stored-query page has a "Repairing drift" section.
- A metrics surface (#281; no specification governs metrics). One
  OpenTelemetry meter provider counts the integrity incidents by `kind`
  (`ferrofed_integrity_incidents_total`), the requests sent to each member
  endpoint by the §11.1 outcome the per-endpoint report gives them
  (`ferrofed_node_requests_total{endpoint, outcome}`), how long each member
  took to answer (`ferrofed_node_request_duration_seconds{endpoint}`), and
  the registry reloads by `result`, `applied` or `refused`
  (`ferrofed_registry_reloads_total`). `[metrics] listen` serves the
  Prometheus text exposition at `GET /metrics` on an admin listener of its
  own, never the gateway's listener, and `[metrics] otlp_endpoint` pushes the
  same metrics to an OTLP gRPC collector; both are off by default. `serve`
  and `config check` refuse a listener on a non-loopback address unless
  `[metrics] allow_remote = true` is set, a listener on `server.listen`, and
  a collector that is no `http://` URL. Every label value is drawn from a
  closed set or the registry document, never from a request (§5.4.1, N33).
  The book's operate section has a Metrics page listing every metric and its
  labels. The integrity incident webhook the architecture named is removed
  from the design: alert on the counter.
- The stored-query registry runs on one of three backends, chosen with
  `[stored_queries] backend` (#268; §12.7, N44, CP-40). `redb`, the default,
  is the embedded file for one gateway, as before. `postgres`, behind the
  `ferrofed-server` cargo feature of the same name and off by default, keeps
  the definitions in one PostgreSQL database every replica shares: the
  primary key on the name and version with `INSERT … ON CONFLICT DO NOTHING`
  stores exactly one of two racing `PUT`s of a new version, the other is a
  `409` (`stored-query-held`), and a read or an invocation reads the store
  again first, so every replica serves what any of them stored. Its
  connection string is a secret, `url` or `url_file`, TLS is rustls with the
  platform's roots, and the schema `ferrofed` and its table are created when
  absent. `files` is read-only: one file per
  `{qualified_query_name}/{version}.aql`, loaded and admitted at start; a
  `PUT` is a `405` with the new code `stored-query-read-only` and `Allow:
  GET, OPTIONS`, `OPTIONS` lists no `PUT`, and a malformed file refuses the
  start and `config check`, naming the file and never its content. The
  startup banner and log line name the backend kind and never its path or
  connection string.

- `PUT {base}/v1/ehr/{ehr_id}` is checked against what the gateway already
  knows of the `ehr_id` before anything is sent (#289; §12.4, §12.5.2, N23,
  N42, CP-15; ITS-REST 1.1.0 `ehr_create_with_id`). When a resolution
  binding of the session or the `ehr_id` index places the `ehr_id` at a
  member other than the one the targeting headers name, the create is a
  `409` with the new code `ehr-id-held`, no node is sent it, and the
  gateway logs a warning naming endpoint ids only; the message names the
  holding endpoints and the targeted one, never the `ehr_id`. A create at
  the member that holds the `ehr_id` is forwarded, so that node answers its
  own `409`, and an `ehr_id` nothing places is forwarded as before and,
  once created, indexed at the targeted member, where a racing create at
  another member raises the index-insert alarm of §12b.2.
- Stored-query definition fan-out, opt-in with `[federation]
  fan_out_stored_queries` beside the registry (#78; §12.7, N44, N43, CP-40).
  Off by default, refused by `config check` and at start without
  `[stored_queries]`, and declared in `OPTIONS {base}/` as
  `definition.stored_query_fan_out`, `true` only beside the registry. Where
  it is on, a stored-query `PUT` whose `openEHR-federation-endpoint` is `*`
  or whose targeting headers name members is stored at the registry first,
  then sent to each member as the registry's canonical AQL through
  `openehr-its`'s generated client, and answered with the registry's
  `StoredQuery` and `meta.federation` on the template fan-out's statuses:
  `200`, `207` with `complete` false, or `424` and `504`, the registry
  holding the definition whatever the members answered and nothing rolled
  back. A definition carrying a `FROM ENDPOINT` or `ORGANISATION` directive
  is refused for distribution with `400 definition-endpoint-targeted`
  before anything is stored, and stays storable and executable federated.
  A `GET` of a version naming members reports drift per member: `active`
  where its copy is the same query, and `node-error` with
  `error.code` `definition-differs` or `definition-missing` otherwise, `207`
  unless every named member matches. An invocation always runs the
  registry's AQL, never a member's copy. With the setting off, a registry
  `PUT` or version `GET` carrying `openEHR-federation-endpoint` or
  `openEHR-federation-organisation` is refused with
  `400 stored-query-fan-out-unsupported`, storing and reading nothing,
  where the header was ignored before.
- The federated AQL answer carries the provenance headers (#288; §7a.3,
  N31, CP-24). `openEHR-federation-endpoint` lists the endpoints that
  contributed rows and `openEHR-federation-system-id` their nodes'
  `system_id`s, position for position, in registry order and in the
  comma-separated form of the §8.4 request header. An endpoint contributed
  when it answered `active` with a `row_count` above 0; one that answered no
  rows, failed or was never asked is not listed, an answer that fails lists
  none, and `meta.federation.endpoints[]` still names every endpoint (§11.1).
  The headers carry registry ids and `system_id`s, never a request value
  (§5.4.1).
- Fan-out template upload, opt-in with `[federation] fan_out_template_upload`
  (#76; §12.6, N43, CP-34). Off by default, and `OPTIONS {base}/` declares
  the configured value as `definition.fan_out_template_upload`. Where it is
  on, an ADL 1.4 or ADL 2 template upload whose `openEHR-federation-endpoint`
  is `*` (every active member) or whose targeting headers select several
  endpoints is sent to each member independently, byte-identical through the
  routed forwarding path, and nothing is rolled back. The answer is
  `meta.federation` with one `endpoints[]` entry per registry member and no
  node's body: `200` when every named member accepted, `207` with
  `complete: false` when some failed, and `424` or `504` when none accepted.
  The provenance headers list the members that accepted. A plain upload still
  names its one node, and `*` stays `endpoint-unknown` on every other
  definition request and whenever the setting is off.
- `ferrofed serve` opens with a startup banner when its log renders for a
  person (#330): the FerroFED wordmark, the version, the maintainer and the
  repository, the Federation Tier, ITS-REST, AQL and `openehr-*` releases it
  serves, then the base path, the listen address, the member and endpoint
  counts of the registry document, and whether the stored-query registry is
  offered. The document is read once, before the banner, and the gateway is
  built over that same read, so the banner counts the registry it serves.
  Under `profile = "development"` a red notice, in plain words where
  there is no colour, says the deployment must not hold or reach real
  patient data. The banner prints only before the `pretty` rendering, so a
  log collector reading JSON never receives it, and `config check`,
  `healthcheck` and `admission check` print none. It never shows a
  credential, a URL, a header value or request data. Each pin comes from the
  crate constant `scripts/checks/versions.sh` holds to `docs/VERSIONS.md`,
  and the `openehr-*` family version is a new constant the guard checks.
- The console honours `NO_COLOR` (#330, <https://no-color.org>): set and not
  empty, it switches colour off in the `pretty` log and the startup banner,
  whatever the format and the terminal.
- The ITS-REST `GET` forms of query execution are served (#287; N1, CP-1).
  `GET {base}/v1/query/aql` and, where the stored-query registry is offered,
  `GET {base}/v1/query/{name}[/{version}]` carry the request in the query
  string: `q` (ad hoc only), `offset` and `fetch` by name, and every other pair
  a `query_parameters` member, so a stored `GET` binds `q=…` to `$q`. The
  generated `openehr-its` parameters decode it (FerroEHR #3540), a `+` is a
  literal plus, and the request runs the pipeline of the `POST` form
  unchanged: the rewrite, the outbound gate, the fan-out, the merge and the
  completeness rules, with the same node requests and the same answer. An
  `ehr_id` is dropped, as from a `POST` body. A query string the decoder
  refuses is `400 body-invalid` and asks nobody; a `null` parameter is
  `400 parameter-invalid`, as in a body. `OPTIONS` on both paths lists `GET`,
  and the request log records neither `q` nor a parameter value.
- The DEMOGRAPHIC area is never federated, and may be routed to one
  declared endpoint that the request names (#68, #311; §7a.1, §12.4, §12.6,
  N23, N32, N31, CP-25). By default every request under
  `{base}/v1/demographic/` answers `501` and no node is asked. The new
  `[federation] demographic_endpoint` setting declares the one registry
  endpoint that serves the area. A DEMOGRAPHIC request names that endpoint
  in `openEHR-federation-endpoint` and then goes to it alone, through the
  single-node path definition requests use: the body byte-identical, only
  what the ITS-REST operation declares, and the node's answer as the node
  sent it with `openEHR-federation-endpoint` and
  `openEHR-federation-system-id`. The setting is never applied as a default:
  a request naming no endpoint is `400 target-required` and no node is
  asked, one naming another endpoint is `400 targeting-conflict`, and
  several endpoints, `*` or an unknown id are refused as on any routed
  request. `config check` refuses an id the registry does not hold, and the
  setting without `registry.document`. `OPTIONS {base}/` declares
  `its_rest.demographic` as `unsupported: 501`, or as `routed-single-node`
  naming the endpoint a request names, and `OPTIONS` on a DEMOGRAPHIC path
  names its ITS-REST methods only when the area is routed.
- Definition requests are routed to one explicitly chosen node (#75; §7a.1,
  §12.6, §12.7, N43, N31, N33, CP-34). Every request under
  `{base}/v1/definition/`, an ADL 1.4 or ADL 2 template upload, list, read or
  example, and stored-query management where the stored-query registry is not
  offered, goes only to the one endpoint the targeting headers name. The body
  reaches that node byte-identical with only what the ITS-REST operation
  declares, and the node's answer, its `404` or validation `400` included,
  comes back as the node sent it with `openEHR-federation-endpoint` and
  `openEHR-federation-system-id`. Without a header the request is
  `400 target-required`; `*` or an unknown id is `400 endpoint-unknown` and
  two endpoints are `400 endpoint-several`, so no node is ever picked
  implicitly and no two nodes' templates are combined into one catalogue.
  Where the registry is offered it keeps answering stored-query definitions
  itself. Without the registry both stored-query `PUT`s are routed, the
  versioned `PUT {base}/v1/definition/query/{name}/{version}` included
  (#298), and `OPTIONS` on either path lists `PUT`.
  `OPTIONS {base}/` declares `its_rest.definition` as `routed-single-node`,
  and `OPTIONS` on a definition path names its ITS-REST methods.
- `ferrofed admission check --endpoint <id>` exercises one configured
  member against the identifier-integrity conditions of §12b.2 and writes a
  report to standard output (#79; §12b.1, §12b.2, N42a, CP-33a). It creates
  test EHRs on the node (3 by default, `--count` from 2 to 50) through
  `POST {base}/v1/ehr`, each for a fresh synthetic subject in
  `urn:oid:2.999.1.0`, reads each back, and reports every condition as
  `pass`, `fail` or `cannot-check` with its evidence: `ehr_id` generation
  (a version-4 UUID, and no `ehr_id` issued twice), `system_id` uniqueness
  (the `system_id` the node reports is the one the registry records, which
  the registry holds unique), and the `ehr_id` exchange (the configured
  cross-reference maps each subject to the `ehr_id` the node created, or
  `cannot-check` where the gateway cannot write to it). Reuse across
  restores and adoption of foreign `ehr_id`s are always `cannot-check`, with
  the reason. A node the check cannot reach fails every condition it
  exercises. Every request passes the outbound gate with the run's subjects
  withheld (§5.4.1, N33), and the report prints no subject and no node error
  body. The command exits `0` when no condition failed, `1` when one did.
  The book's new page "Admitting a node" states what the check proves and
  what it cannot.
- The registry reloads on `SIGHUP` (#282). `ferrofed serve` reads its
  configuration again from where it started and checks it as at boot; a
  valid one replaces the registry for every request that starts after it,
  while a request in flight finishes on the registry it took. The
  `[registry]`, `[credentials]`, `[dev]` and `[pixm]` sections take effect: an
  added endpoint gets its node client and credentials, and a removed one is
  never called again. A learned `creating_system_id` route the new document
  maps elsewhere is withdrawn with a `RegisteredCreatingSystemConflict`
  incident, and `ehr_id` index entries and resolution bindings naming a
  member that left are dropped; one a request in flight learns after the
  swap is dropped at its next lookup, and a read then probes every member
  while a write needs its target, so an entry is never narrowed to the
  claimant that remains (§12.5.2, N42). A change to any other setting is logged as
  needing a restart while the rest applies. A configuration that does not
  load is refused, the running registry stays, and the `ERROR` line names
  the failure class and the files, never their content. `config check` is
  unchanged. There is no file watch.
- `GET {base}/v1/ehr?subject_id=…&subject_namespace=…`, the ITS-REST read of
  an EHR by subject, is served (#218; §5.2, §5.4.1, §12.5.2, N3, N31, N33,
  CP-24, CP-26). The two parameters are resolution input: the gateway
  resolves the subject through the cross-reference service and sends
  `GET {base}/v1/ehr/{ehr_id}` to the one member that holds it, under that
  member's own `ehr_id`, with no query string, and returns the node's answer
  unmodified with `openEHR-federation-endpoint` and
  `openEHR-federation-system-id`. No node request, error body or log line
  carries the subject. A subject several members hold is refused with the new
  code `409 subject-several` listing the endpoints, unless
  `openEHR-federation-endpoint` names one of them; a subject no member holds
  is the operation's own `404 no-destination`; a cross-reference that cannot
  answer is the new `424 resolution-unavailable`. `OPTIONS {base}/v1/ehr`
  now lists `GET`.
- The gateway serves at a base path of the deployment's choosing (#69;
  §4.1, N28, CP-21). `[server] base_path`, `/` by default, puts every route
  under it, `OPTIONS {base}/`, the health family and `{base}/v1/…` included,
  and every path outside it answers `404`. No prefix is reserved, and
  `/rest/openehr` is served only where a deployment chooses it. A base path
  that does not start with `/`, ends with `/`, carries a query or a fragment,
  or has an empty, `.` or `..` segment refuses to boot, naming
  `server.base_path`. Nodes are still asked at their own base, and a node's
  `Location` still passes through unmodified (N31).
- Both AQL forms of N29 scope a query to one `ehr_id` (#69; §7.1, N29,
  CP-22). `FROM EHR e[ehr_id/value='…']` reaches a node as the canonical
  `WHERE e/ehr_id/value = '…'`, so both forms send the same query and answer
  the same rows. An undirected query scoped to one `ehr_id` goes only to the
  member that owns it, found as a path `ehr_id` is (§12.5.1, N41): the
  session's binding, the `ehr_id` index, then the ask-all probe. Every other
  member is reported `excluded`, the answer names the acting endpoint in
  `openEHR-federation-endpoint` (N31), two claimants are `409
  ehr-id-collision` with neither queried (N42), an `ehr_id` no member holds is
  `404 no-destination`, and one that is not a bare UUID and no earlier step
  routes is `400 probe-requires-uuid` with no member asked (§5.4.1, N33).
  A query directed by `FROM ENDPOINT` or a targeting header still goes to the
  endpoints it names.
- Versioned writes reach only their controlling CDR, and a new EHR only an
  explicit target (#65; §12.4, §12a.1, §10.3, N23, N41, CP-15). An update of
  a composition, the `EHR_STATUS` or the directory, a directory delete (each
  naming its preceding version in `If-Match`) and a composition delete
  (naming it in the path) route by the path `ehr_id` as before, and are sent
  only when the registry maps the preceding version's `creating_system_id`
  to that same node, as the member's own `system_id` or a `[[creating_system]]`
  mapping; a learned mapping never counts. Otherwise the write is refused
  `409 controlling-system-unreachable`, naming the controlling node where the
  registry knows it, and no node is sent it. A versioned write that names no
  single preceding version (no `If-Match`, several, `*`, a weak or unquoted
  tag, or no `OBJECT_VERSION_ID`) is refused with the new code
  `400 preceding-version-invalid`. `POST {base}/v1/ehr` is now served, and it
  and `PUT {base}/v1/ehr/{ehr_id}` go only to the one endpoint the targeting
  headers name: without them the request is `400 target-required`, and with
  several endpoints `400 endpoint-several`; neither a binding nor the index
  routes a new EHR. `OPTIONS {base}/v1/ehr` answers `POST, OPTIONS`.
- A `CONTRIBUTION` that amends versions reaches only the CDR that controls
  every one of them (#286; §12.4, §12a.1, §10.3, N23, CP-15).
  `POST {base}/v1/ehr/{ehr_id}/contribution` routes by its path `ehr_id` as
  before, and the gateway now reads the body with the ITS-REST
  `NewContribution` type: each version's `preceding_version_uid` must be one
  the registry maps to that node, by the rule above, or the request is
  refused `409 controlling-system-unreachable` and no node is sent it. A
  `CONTRIBUTION` of creations alone routes as before. A body that is no
  `CONTRIBUTION` in the representation its `Content-Type` selects, a
  `preceding_version_uid` that is no `OBJECT_VERSION_ID`, or an XML body, is
  refused `400 preceding-version-invalid`. The node receives the body byte
  for byte.
- A `CONTRIBUTION` whose versions' `data` is FLAT or STRUCTURED is routed
  as a canonical-JSON one is (#297; §12.4, §12a.1, N23, CP-15). Under
  `Content-Type: application/openehr.wt.flat+json` or
  `application/openehr.wt.structured+json` the envelope stays canonical
  (ITS-REST 1.1.0 `contribution_create`), and the gateway reads it with the
  generic `NewContribution` of `openehr-its` 0.0.80 for each version's
  `preceding_version_uid`, leaving `data` to the node. A `CONTRIBUTION` of
  creations alone goes by its path `ehr_id`, one whose every amended version
  the path node controls reaches that node, and one with a version another
  member controls, or no member is known to, is
  `409 controlling-system-unreachable` and no node is sent it. The node
  receives the body byte for byte, under the declared media type. A
  `CONTRIBUTION` in canonical XML is still refused
  `400 preceding-version-invalid` (#308), now with a message that names the
  XML form.
- Integrity incidents for an `ehr_id` two members claim (#63; §12.5.2,
  §12b.2, N42, CP-33). A request whose `ehr_id` the session's resolution
  bindings, the `ehr_id` index or the ask-all probe finds at two members or
  more is refused `409 ehr-id-collision` listing the claiming endpoints, and
  the gateway logs one `EhrIdCollision` incident: an `ERROR` line under the
  new log target `ferrofed::integrity` with the `ehr_id`, the routing step
  that found the claimants (`detection`) and the claimants' endpoint ids.
  When the index learns an `ehr_id` it already holds at another member, it
  raises the index-insert alarm of §12b.2 once, an `IndexInsertCollision`
  incident naming the members, keeps both and routes neither; a held
  collision lasts until the entry is forgotten or the gateway restarts. An
  incident names an `ehr_id` only when it is a bare UUID, and never a patient
  identifier. The book's configuration page says what an operator sees and
  does, and the metrics surface (#281) counts each incident by `kind`.
- The obligations checklist (#278): `conformance/obligations.tsv` holds one
  row per normative statement of the pinned Federation Tier specification,
  447 across its 26 pages and both JSON schemas, each with the status
  FerroFED holds for it and the test, code, issue or #212 report behind that
  status. 273 are tested, 52 planned, 3 built but untested (#290), 3 missing
  (the GET forms of query execution, #287, and the provenance headers on the
  federated AQL answer, #288), 5 deferred, and 18 are contradictions or
  silences of the text recorded on #212; the other 93 fall on a member node
  or the operator, or are no obligation of the gateway. The book renders the counts and every gap on a new
  page beside the conformance matrix. A new tier-1 guard,
  `scripts/checks/obligations.sh`, fails on an unknown status, a gap that
  names no issue, a test that does not exist, a point or requirement the
  matrix does not hold, a duplicated row, or a stale page, and it holds a
  digest of each vendored page's keyword lines, so a re-pin fails until the
  changed pages are reclassified. The comment-style guard now reads the
  `evidence` cells of the conformance tables as it reads their `reason`
  cells.
- Container readiness (#303). `ferrofed healthcheck` asks the gateway on this
  host for `GET /health/readiness` over loopback, prints one line and exits
  `0` only on `200`, `1` otherwise; the image carries
  `HEALTHCHECK --interval=30s --timeout=5s --start-period=15s --retries=3`
  over it, and the `compose.yaml` gateway service the same healthcheck.
  Readiness answers `503` until boot completes and from the moment `SIGTERM`
  or `SIGINT` arrives, before the drain starts, and its body names the phase
  and the gateway's own subsystems (the configuration, the registry, the
  outbound clients, the stored-query store). No member node and no identity
  source gates readiness: the new `GET /health/dependencies` always answers
  `200` with the state the gateway last observed of each member endpoint and
  of the resolver (`up`, `failing`, `down`, `unknown`), from the requests it
  already makes, never a probe of its own, naming endpoint ids and states
  only. `deploy/kubernetes/` holds an example ConfigMap, Deployment, Service
  and PodDisruptionBudget, validated in CI by a digest-pinned `kubeconform`
  in strict mode. No specification governs health probes: our own design.
- The supported platforms are stated (#307): FerroFED runs on Unix only.
  The server drains on `SIGTERM` and reloads on `SIGHUP` through Unix
  signals, and every release binary and the container image are Linux. The
  book says so under Operate, and building `ferrofed-server` for a non-Unix
  target, such as Windows, stops at a `compile_error!` that names the reason.
  No specification governs this: our own design.

### Changed

- CI checks every internal link and anchor of the published site (#377; no
  specification governs this: our own design). The new tier-1 job
  `site-links`, in the required `conclusion`, assembles the site as the Docs
  workflow publishes it and runs lychee 0.24.2, pinned by version and
  SHA-256, with `--offline --include-fragments` over every page, and over
  `README.md` against the repository tree, so a link to a page or an anchor
  that does not exist fails the change and no request leaves the runner.
  `scripts/checks/site-links.sh` runs the same check locally, with a
  self-test, and `scripts/site/assemble.sh` takes `SITE_ROADMAP=off` to leave
  the roadmap block unrendered. `docs/VERSIONS.md` pins lychee, and the
  versions and pin-freshness guards hold the pin.
- The errors page names the requests that answer `501 not-implemented`
  (#378; §7a.1, N32): every path under `{base}/v1/`, and `OPTIONS {base}/`,
  without a registry; the DEMOGRAPHIC API unless its endpoint is declared;
  the ADMIN API; stored-query execution without the stored-query registry; a
  path ITS-REST does not define or a method it does not declare; and
  `OPTIONS` on an unserved path. The errors test now sends each of them.
- The quickstart's `compose.yaml` runs only the published
  `ghcr.io/ferrohealth/ferrofed` image: its `build:` block is gone, so
  `docker compose up --build` no longer builds the gateway, and
  `scripts/checks/release-compose.sh` fails when any compose file in the
  repository carries `build:` (#385; no specification governs packaging: our
  own design). The gateway service now has a 20-second stop grace period,
  longer than its 10-second drain, so `docker compose down` no longer cuts a
  drain off at Docker's default 10 seconds.
- The README, the landing page and the book are rewritten against what the
  gateway does on `main` (#363; no specification governs the website). The
  book's claims page lists what v0.0.3, v0.0.6 and v0.0.7 carry and what
  v0.0.8 and v0.0.9 plan, by issue; the operate pages state that the gateway
  authenticates no client yet and which onward credentials it sends, that no
  session-scoped resolution binding is held until client authentication
  (#80), and that a node's consent refusal is reported `node-error` until
  #83. Two pages are new: Identity resolution, covering `[pixm]`, `[dev]`,
  `profile` and `federation.default_namespace`, and Health probes, moved out
  of the container page. `Prefer: wait` is documented beside the budgets.
  The landing page's quickstart runs the four-node compose stack with its
  seed script, and its cards name what each release shipped.
- The ask-all probe is timed (#366; no specification governs metrics): each
  member's probe now carries its latency, so
  `ferrofed_node_request_duration_seconds` times it beside its count in
  `ferrofed_node_requests_total`, and a probe still waiting when the overall
  budget runs out is a `time-out` timed to that moment. The book's Metrics
  page lists which calls each `outcome` and the histogram cover.
- The book's client contract and configuration pages are split along their
  own sections (#348): the client contract continues on Follow-ups,
  Templates, definitions and demographics, and Stored queries, and the
  configuration on The registry, and Queries and API areas. The file-length
  guard now holds the book's Markdown pages to the same limits as the Rust
  sources.
- A node's error in `meta.federation.endpoints[]` follows one rule on every
  path that reports a member: a federated query, the fan-out template upload
  and the stored-query distribution and drift check (#343; §9.5, §11.1,
  §12.6 item 2, N40). A `node-error` carries the node's HTTP status, then
  an excerpt of the node's own message: at most 512 characters, control
  characters and invisible format marks turned into spaces, and the patient
  identifier the query resolved on replaced by `[withheld]` wherever the
  node echoed it, the whole message when the node echoed it percent-encoded
  (§5.4.1, N33). The HTTP client's reason an `offline` or `time-out` node
  was not reached is held to the same rule. The two fan-outs carried the
  node's status alone before,
  and a query copied the node's message with no cleaning and no masking.
- A recombined `AVG` over integers answers an integer (#309; AQL 1.1.0
  §3.9.1.5, "it will also determine the return type"). When every node's
  `SUM` is an integer, the gateway returns the integer nearest the exact
  quotient of the federation's sum and count, a tie going to the even one,
  so `5 / 2` is `2` and `7 / 2` is `4`; it rounds once, after adding every
  node's sum and count, never per node. AQL states no rounding rule, so the
  rounding is FerroFED's own. When a node's `SUM` is a real, `AVG` is the
  decimal mean written as the nearest JSON number, as before.
  `openehr-federation` is 0.0.33.
- The `openehr-*` family moves to 0.0.81 (FerroEHR #3548, #3551, #3552), and
  `openehr-federation` to 0.0.35 with it (#329).
- A body routed to one node without a `Content-Type` travels with the first
  media type the operation lists, `application/json` wherever ITS-REST lists
  several, where it was a `415` for an operation whose body is declared in
  more than one (#329). ITS-REST 1.1.0 makes `Content-Type` optional with no
  default, so the first listed is the gateway's own choice, as for a missing
  `Accept`. The `Content-Type` of a body is held to the media types the
  operation's body is declared in, and a `CONTRIBUTION` is read for its
  preceding versions by the request body reader of `openehr-its`.
- The compose quickstart runs four FerroEHR nodes, `ferroehr-a` to
  `ferroehr-d` on ports 8081 to 8084, each with its own `system_id` (#322).
  They share one FerroEHR PostgreSQL container, `ferroehr-postgres`, with a
  database per node owned by its own role; the image's init script runs once
  per node through `docker/postgres/20-ferrofed-node-databases.sh`, so the
  per-node `ferroehr-a-postgres` and `ferroehr-b-postgres` services and their
  volumes are gone (`docker compose down -v` removes the old ones). Each node
  database admits only its own node's role: `CONNECT` is revoked from
  `PUBLIC`, so one node's role is refused on another node's database. The
  gateway's quickstart configuration is a development profile whose static
  cross-reference maps four synthetic patients in `urn:oid:2.999.1.1`: one at
  all four nodes, one at two, one at one and one at none, and
  `scripts/quickstart/seed.sh` creates exactly those EHRs and a composition in
  each over ITS-REST. A patient query now resolves in the quickstart instead
  of failing closed with `424`. The book's container page walks through a full
  federated query, a patient missing at some nodes and a query directed at one
  node, and states the measured memory use: 238 MiB after
  `docker compose up --wait`, 425 MiB after the seed and the queries.
- The end-to-end harness starts one FerroEHR PostgreSQL container per topology
  with a database per node, through the same init script, instead of one
  container per node (#322, #320). CI keeps two nodes.
- `scripts/conformance/obligations.sh` and `scripts/checks/obligations.sh`
  are executable, as every other script is (#320).
- The query string of a stored-query `PUT {base}/v1/definition/query/{name}/{version}`
  is decoded by the generated `openehr-its` parameters (#292, FerroEHR
  #3540), so a `+` is a literal plus. A `query_type` given twice, or a pair
  that does not percent-decode to UTF-8 text, is now `400 body-invalid` and
  nothing is stored; an undeclared parameter stays
  `400 query-parameter-refused`.
- The `openehr-*` family moves to 0.0.80 (FerroEHR #3539 to #3541, #3543), and
  `openehr-federation` to 0.0.31 with it. The fuzz crate is now held to the
  family pin by `scripts/checks/versions.sh`.
- An `ehr_id` that the session's resolution bindings or the `ehr_id` index
  hold at two members is now refused `409 ehr-id-collision` listing the
  claimants, on a write as on a read (#63; §12.5.2, N42). A read no longer
  goes on to the ask-all probe, and a write is no longer `400
  target-required`. Naming the node in `openEHR-federation-endpoint` still
  routes the request to it (§12.5.1 step 1, N41). The integrity incidents of
  the `creating_system_id` map are logged under `ferrofed::integrity` too.
- The `409 controlling-system-unreachable` message now names the controlling
  system in every case (#66; §10.3 `copy-write-reject`, N36, CP-29). Where
  the registry routes the version's `creating_system_id` to another member,
  it names that `creating_system_id` as the registry spells it, with the
  member's node and endpoint. Where no member is known to control the
  version, it points at the place in the request that names the version
  (`If-Match`, the path, or a `CONTRIBUTION` version by position), and still
  quotes nothing of the request. CP-29 is scored end to end: a write against
  a row that `version-identity` dedup kept, sent through the copy's `ehr_id`,
  is refused `409` whether the creating node is down or up, the copy's node
  is sent nothing and the creating node is never tried, and a write through
  the creating node's own `ehr_id` reaches it alone and its answer names no
  copy.
- A body routed to one node always travels with a `Content-Type` its
  ITS-REST operation declares (#298; §12.6, N43, CP-34). The gateway reads
  each operation's request-body media types from `openehr-its`
  (`request_media`): a client `Content-Type` for an operation that declares
  no `Content-Type` parameter, such as the versioned stored-query `PUT`, is
  composed as the listed media type it names, or refused `415
  media-type-unsupported`; a body sent without one travels with the one
  media type the operation's body is declared in, `text/plain` for a stored
  query and `application/json` for a commit, and an operation whose body is
  declared in several is `415`. A forwarded body no longer reaches a node
  without a `Content-Type`.
- Each path identifier of a routed request is parsed as the identifier class
  `openehr-its` states for it, and the follow-up routing table reads a
  version from the path parameters of that class, with no name list of the
  gateway's own (#291; §5.4.1, §12.2, N33, CP-26). The `uid_based_id` of a
  `DELETE` is an `OBJECT_VERSION_ID`, as ITS-REST declares: a DEMOGRAPHIC
  delete addressing a `HIER_OBJECT_ID` is `400 parameter-value-invalid`,
  and a composition delete stays `400 preceding-version-invalid`, with
  nothing sent.

### Fixed

- Every link in `llms.txt` is now an absolute URL (#392). The file is
  served at the site root, where its repository-relative links resolved to
  nothing.

- A fan-out template upload, a stored-query distribution or repair, and a
  stored-query drift check fail with a `500` when the task for one member
  panics (#380; no specification governs the metrics or health probes). Such
  a task was reported as abandoned by the member: counted as a `time-out` in
  `ferrofed_node_requests_total`, marked `down` on
  `GET /health/dependencies`, and listed as `time-out` in `meta.federation`,
  blaming the node for a defect in the gateway. The probe and the federated
  query already failed this way, and none of these calls records a member
  for a request it fails.
- The admission check reports an EHR call whose deadline passed before it
  left the gateway as a call never sent (#379; §11.5), where it said the
  node did not answer before the deadline.
- A request whose deadline passed before it left the gateway is no longer
  counted as a node `time-out` (#374; §11.5; no specification governs the
  metrics or health probes). A request routed to one node, an ask-all probe,
  a fan-out template upload, a stored-query call and a federated query member
  the budget overtook before sending are now neither counted in
  `ferrofed_node_requests_total` nor timed in
  `ferrofed_node_request_duration_seconds`, and leave the member's state on
  `GET /health/dependencies` as it was, where they counted as `time-out` and
  set the member `down`. A fan-out whose overall budget had already run out
  when it started reported every member as asked and silent; it now reports
  none as asked. What the client receives is unchanged: the routed request
  and the probe answer `504` (`node-timeout`), and the member record in
  `meta.federation` says `time-out`.
- `GET {base}/health/dependencies` records each member a fan-out template
  upload, a stored-query distribution or repair, or a stored-query drift
  check asked (#366; no specification governs health probes), where a member
  those calls found down kept the state an earlier request left. One rule
  now holds for every call: the state is the member's reachability and
  health, never whether the request was valid. Any answer below `500` is
  `up`, a `5xx` is `failing`, and no answer is `down`, read from the node's
  own HTTP status whatever the §11.1 record says. A query member that
  answered `400`, a refused store, a node that refused the gateway's onward
  credentials, and a drift check whose copy differs or is missing are `up`
  where some of them were `failing`; a request that never left the gateway
  changes nothing.
- `ferrofed_node_requests_total` and `ferrofed_node_request_duration_seconds`
  count only requests that left the gateway (#366). A member a fan-out
  template upload or a stored-query call could not send a request to stays
  `offline` in `meta.federation`, which §11.1 leaves no other status for,
  and is no longer counted and timed as an `offline` node request; nor is a
  federated query member whose deadline passed before its request left.
- A request the gateway routes or answers under `{base}/v1/` is logged under
  the path template of the ITS-REST operation it addresses, such as
  `/v1/ehr/{ehr_id}/composition/{uid_based_id}`, where its request line named
  `<unmatched>` (#219). The template comes from `openehr-its`'s route table,
  so the line never carries the `ehr_id` or version uid of the path, and the
  configured base path appears in front of it once. An `OPTIONS` request is
  logged under the template of the resource it describes. A path that names
  no route, or any other method its operation does not declare, is still
  logged as `<unmatched>`.
- `POST {base}/v1/query/aql` and the stored-query `POST {base}/v1/query/{name}`
  hold their `Content-Type` to `application/json`, the media type ITS-REST
  1.1.0 lists for both (#269). Another media type, or a parameter other than
  `charset=utf-8`, is `415 media-type-unsupported`, and no node is asked
  (RFC 9110 §15.5.16); a body sent as `text/plain` used to be read as JSON.
  A body sent without a `Content-Type` is read as JSON, the first listed
  media type, as for a missing `Accept`, since ITS-REST and RFC 9110 §8.3
  name no default. The stored-query definition
  `PUT {base}/v1/definition/query/{name}/{version}` the registry answers
  holds its `Content-Type` to `text/plain` under the same rule: another media
  type is `415 media-type-unsupported` and nothing is stored.
- The `query-parameter-refused` security event says "a request carried a
  query parameter the gateway does not admit for its operation, and was
  refused", which is true of a routed request and of the stored-query
  definition `PUT` that raises it too (#269).
- `OPTIONS {base}/` declares the EHR area as the gateway routes it (#290;
  §7a.2, N30, CP-23). `its_rest.ehr` now says that a new EHR, by
  `POST {base}/v1/ehr` or by `PUT {base}/v1/ehr/{ehr_id}`, goes only to the
  endpoint the targeting headers name, and that the `PUT` is refused when
  another member holds its `ehr_id`. It no longer names the session's
  resolution binding as a routing step, because without client
  authentication no request has a session. Each of the four `its_rest`
  declarations is now tested against the behaviour of its area in every
  configuration mode.
- A query dispatched to a single node, directed at one endpoint by the
  `openEHR-federation-endpoint` header or `FROM ENDPOINT`, or undirected with
  the patient resolving at one member alone, answers with
  `openEHR-federation-endpoint` and `openEHR-federation-system-id` naming that
  endpoint and its node's `system_id`, as N31 requires of a request
  dispatched to a single node (#319; §7a.3, CP-24). Zero rows, a `424` or a
  `504` from that node carries them too; a query that asked no node carries
  neither.
- A request routed by its target alone (`POST {base}/v1/ehr` and a definition
  request) checks its declared header and query values before its target, as
  the EHR route does: a malformed value is `400 parameter-value-invalid`, and
  an `Accept` or `Content-Type` the operation does not list is `406` or `415`,
  before `400 target-required` (§5.4.1, N33).
- A `[credentials."<endpoint id>"]` key is held to the registry's endpoint id
  rule when the configuration resolves, with or without a registry (#272).
  Configuration used to accept 1 to 128 printable ASCII characters and left
  the registry's rule to the federation load, so a key such as `node:a`,
  `-node` or a 65-character id passed when no registry was configured. The
  one rule is now the registry's: 1 to 64 ASCII letters, digits, `.`, `-`
  and `_`, starting with a letter or digit, and the refusal (exit 78) names
  the key. The CI container job now runs the `ENDPOINT`
  attribute test against both FerroEHR nodes, and a new tier-1 guard,
  `scripts/checks/e2e-placement.sh`, refuses a container test placed where
  that job never selects it.

### Security

- A request routed to one node no longer forwards client text in a path
  identifier, a structured query value or a negotiation header (#261;
  §5.4.1, N33, CP-26). The routed path resolves no patient, so the outbound
  gate had nothing to compare a client's `version_at_time`, `Accept` or
  path `version_uid` against, and a patient identifier written there
  reached the node. The gateway now works from what the ITS-REST operation
  declares in `openehr-its`'s parameter table. It composes `Accept`,
  `Content-Type` and `Prefer` itself, so the node receives a value the
  operation lists, in its own spelling: `Accept` is negotiated as an
  RFC 9110 §12.5.1 media-range list, `*/*` or no `Accept` sending the first
  listed type, and one that admits no listed type is refused with the new
  code `media-type-not-acceptable` (`406`); `Content-Type` must name a
  listed type, with `charset=utf-8` accepted and dropped, or it is refused
  with the new code `media-type-unsupported` (`415`); `Prefer` keeps only
  the listed preferences and ignores the rest (RFC 7240 §2). Each path
  identifier parses as the openEHR identifier it names with `openehr-base`
  (`version_uid` an `OBJECT_VERSION_ID`, a text `uid_based_id` a
  `UID_BASED_ID`, a `uuid`-format parameter a canonical UUID), and the path
  then travels byte for byte (N22). Each query value matches its declared
  kind: a date-time is an extended ISO 8601 date-time read by
  `openehr-base`'s `Iso8601_date_time` (ITS-REST Overview, "Datetime
  format"). A malformed value is refused with the new code
  `parameter-value-invalid` (`400`), naming the header, or the parameter by
  position and declared name, never the value; no node is asked, the
  ask-all probe included, and the refusal is logged as the security event
  `parameter-value-refused`. A composition update must address the
  versioned object's UUID, as ITS-REST declares. The free text the table
  states no kind for (`If-Match`, `openehr-audit-details`,
  `openehr-item-tag`, `openehr-template-id`, `openehr-version`,
  `openehr-version-item-tag`, the item-tag `key`, `path`, `tag_key`,
  `tag_value`, `tag_target_path`) still travels as sent; whether N33 covers
  a forwarded client value is recorded on #212. Track 10 gains the
  identifier in a date-time parameter, an `Accept` parameter and a path
  uid. `ferrofed-engine` adds the `declared` module, which takes `mime` for
  the media types, and `ForwardError::Value`.
- The `Debug` output of the configuration no longer prints an inline
  credential (#364). An inline `bearer_token` or `password` of a
  `[credentials]` section or of a PIX Manager was a plain string, so any
  `{:?}` of the configuration, in a log line, a panic message or a test
  failure, showed it in clear, and a PIX Manager URL or a registry endpoint
  URL showed the user name and password of its userinfo. Every credential
  is now held in one of two types of the new `ferrofed_registry::secret`
  module: a `Secret`, which `Debug`, `Display` and `Serialize` render as
  `***`, or a `SecretUrl`, which they render with the userinfo and the query
  replaced by `***`, and as `***` whole for a libpq key/value connection
  string. The stored-query store's PostgreSQL `url` and `url_file` and
  `metrics.otlp_endpoint` are `SecretUrl`s too. A `_file` sibling is read
  straight into the same type, and the text read is zeroed once it is
  trimmed, so the resolved settings, the composed PIXm configuration and
  the registry document carry them as well. What reaches a node, a PIX
  Manager, the store or the collector is unchanged. `ferrofed-identity`
  adds `PixmConfigError::BaseUrl` for a Manager base URL that does not
  parse.
- A PIX Manager `url` that carries a user name or a password is refused by
  `config check` and at start, naming `pixm.manager[N].url` and never
  quoting it, as an endpoint URL in the registry document already is (#364).
  Its credentials go in `[pixm.manager.credentials]`. A configuration that
  put them in the URL must move them there.
- The `Debug` output of `ihe-iti`'s types no longer prints a credential
  (#370). `PixmClient` and `PdqmClient` derived `Debug`, so a base URL that
  carried a user name and password showed both in clear, and the HTTP
  client they hold showed its default headers, an `Authorization` header
  among them. Each client now shows its URLs with the userinfo and the
  query replaced by `***`, and leaves the HTTP client out. mCSD's
  `DirectoryOrganization` and `DirectoryEndpoint` show their `fullUrl` and
  the endpoint `address` the same way, and the endpoint leaves out the rest
  of its resource, whose `header` list may hold a credential. The crate
  carries this redaction itself and depends on nothing in FerroFED. Every
  redacted value in `ihe-iti` now shows the family's `***`, where the
  identifiers, the matched Patients and the page links showed `[REDACTED]`,
  and so do `ferrofed-identity`'s `PatientRef` and the admission probe's
  synthetic subject.
  `ihe-iti` is 0.0.9.
- A URL whose password holds an unencoded `/`, `?` or `#`, such as
  `https://user:pa/ss@host`, no longer shows that password in the rendering
  of a `ferrofed_registry::secret::SecretUrl` or of an `ihe-iti` type
  (#370). The userinfo was taken to end at the first `/`, `?` or `#`, so no
  `@` was found and the text showed as written. Text that parses as a URL
  is now read as the URL parser reads it, so an `@` in a path or a query
  is not userinfo, and text that does not parse is redacted up to its last
  `@`.

## [0.0.6] - 2026-10-03

The v0.0.4, v0.0.5 and v0.0.6 milestones in one release (no v0.0.4 or v0.0.5
tag was cut). The federated query now shapes its answer across nodes the way
a single CDR would: `ORDER BY` with `LIMIT` re-applied at the Tier, bounded
`OFFSET` pages, `DISTINCT`, decomposable aggregates and opt-in
version-identity dedup, each refused with a reason where it cannot be exact.
Every failure carries its §11.2 status and a stable error code. A request
under `{base}/v1/ehr/{ehr_id}` is routed to the one node that holds that EHR,
byte-identical, and every answer teaches the gateway which system created
each version. A client names nodes through the `FROM ENDPOINT` /
`ORGANISATION` directive or the targeting headers and selects their ENDPOINT
attributes, the registry also loads as FHIR `Endpoint` and `Organization`
resources, stored queries are held at the gateway as immutable versions, and
`OPTIONS {base}/` declares all of it. The identifier-hygiene property is now
held by the track 10 leakage suite against both nodes: no client-chosen
request id, targeting header or path segment carries a patient identifier to
a node or into the log.

### Added

- The follow-up routing table learns from every answer (#64; §12.2, N21,
  CP-13). Each version uid in a federated query's rows, and each one a
  routed read names or answers with in its `ETag`, teaches a route for a
  `creating_system_id` the registry document does not map; one seen at two
  nodes raises the `LearnedCreatingSystemConflict` integrity incident and is
  routed on by neither. CP-13 is now scored. A follow-up read of a version
  under `{base}/v1/ehr/{ehr_id}/…` routes by the path `ehr_id` in the order
  of N41, never by the version's `creating_system_id` (§12a.1, §12.5.1, N22,
  N42a), and reaches the node byte-identical.
- The federated stored-query registry (#77; §12.7, N44, N33, CP-40,
  CP-28). With `[stored_queries] path` set, the gateway holds stored queries
  itself in an embedded `redb` file that survives a restart, and
  `OPTIONS {base}/` declares `definition.stored_query_registry: true`.
  `PUT {base}/v1/definition/query/{name}/{version}` stores the AQL on
  ITS-REST's semver segment: the name is `[{namespace}::]{query-name}`, the
  version `major.minor.patch`, and the text is analysed as an inline query
  would be, with each `$parameter` standing in for a bound value, and held
  as its canonical print. A definition that names the patient by a literal
  is refused `400 subject-literal`, so no patient identifier is held at
  rest; a second `PUT` of a held name and version is refused
  `409 stored-query-held` and the held text stands, across a restart too.
  `GET` on the same path reads the ITS-REST `StoredQuery` back, and
  `GET {base}/v1/definition/query/{pattern}` lists the versions of every
  name the pattern starts. `POST {base}/v1/query/{name}[/{version}]` runs
  the stored query over every member exactly as if its text were sent
  inline, with the client's `offset`, `fetch` and `query_parameters`, the
  targeting, completeness and dedup headers and the budget; without a
  version the highest runs, and a `{major}` or `{major}.{minor}` prefix runs
  the highest it matches. The answer carries ITS-REST's `name` naming the
  gateway's definition. New codes: `query-name-invalid`,
  `query-version-invalid`, `query-version-required`, `query-type-unsupported`,
  `subject-literal` (each `400`), `stored-query-held` (`409`) and
  `stored-query-unknown` (`404`). Without `[stored_queries]` the definition
  routes still answer `501`. `openehr-federation` is 0.0.30 and adds
  `aql::definition::Definition`, the admission analysis of a stored
  definition. Definition fan-out to the nodes is #78.

- Routing a path `ehr_id` in the order of §12.5.1 (#62; §12.5, N41,
  CP-33). A request to `{base}/v1/ehr/{ehr_id}` or below it goes to the node
  the targeting headers name, then the node a resolution binding of the
  client session names, then the node the new `ehr_id` index names, and for
  a read only, the one member an ask-all probe finds. The probe sends
  `GET {base}/v1/ehr/{ehr_id}` to every member at once within the per-node
  timeout and the overall budget (§11.5); a read of the EHR itself is
  answered from the owner's probe answer. A read no member holds is
  `404 no-destination`, one two members hold is `409 ehr-id-collision`
  listing the claimants, and one a member did not answer for is `504`
  (`node-timeout`, `node-unreachable`) or `424` (`node-refused`, and the new
  code `node-error`), naming the member: a member that gave no answer may
  hold the `ehr_id`, so the owner is unknown. A read that names no node no
  longer answers `501`. A write none of the first three steps routes stays
  `400 target-required`, and nothing is probed. A step that names two
  members names none, and no later step picks one of them. The path
  `ehr_id` must be an openEHR `HIER_OBJECT_ID`, and any other value is `400`
  with the new code
  `ehr-id-invalid` before any routing. The index learns from resolutions and
  from members' successful answers, holds `ehr_id`s and member ids only, in
  memory, and forgets the least recently used `ehr_id` past
  `federation.ehr_index_capacity` (default 100000; 0 is refused). Session
  bindings answer once client authentication lands (#80); the integrity
  incident of a collision is #63.

- `OPTIONS {base}/`, the gateway's self-description (#73; §7a.2, N30,
  CP-23). The body validates against the vendored `options-root.schema.json`
  and is built from the running configuration: the federation id, the
  `major.minor` of the pinned specification, `aql.fan_out` from the node
  selection, the dedup default, modes and request header, the per-node and
  overall budgets, all-or-nothing with the best-effort opt-in when it is
  offered, the `OFFSET` strategy (`bounded` with its `max_window`, or
  `reject`, never a cursor), the decomposable aggregates (an empty list when
  none), nothing offered in the definition area, `localization.on_failure:
  "closed"`, the ITS-REST areas, and every registry endpoint with its
  organisation, membership status, node, `system_id`, and product and
  version where the registry holds them. It declares no targeting mechanism,
  no resolution carrier, no asynchronous queries and no JWKS location, and it
  needs no patient identifier. `OPTIONS` on a path under `{base}/v1/`
  answers `204` with the methods served there in `Allow`, without asking a
  node, and `501` where the gateway serves nothing.

- ENDPOINT attributes in rows (#72; §9.2, §9.3, §9.4, N12, N17, N18,
  CP-35, CP-37). A directed query that selects `p/id` or `p/endpoint_id`,
  `p/organisation` or `p/organization_id`, `p/system_id` or `p/url` through
  the `FROM ENDPOINT` variable gets each value in every row, as a string, from
  the registry entry of the endpoint the row came from: the values
  `meta.federation.endpoints[]` reports for it. No node is asked for an
  attribute, and a query that selects none keeps the row shape of a single
  CDR. The §9.4 example answers the §9.4 columns and rows, with `columns[]`
  paths in the ITS-REST form (`/id`, `/system_id`, `/uid/value`). An alias on
  an attribute keeps it apart from an EHR-derived column of the same name;
  giving both the same name is refused `400` with the new refusal
  `endpoint-name-collision`, and a path through the variable that is no §9.3
  attribute with `endpoint-attribute-unknown`. Under `DISTINCT` the
  attributes take part in which rows are equal. An attribute beside an
  aggregate recombined across endpoints is refused `400`
  `indecomposable-aggregate`, and ordering on an attribute stays refused `400`
  `endpoint-variable`. The `501 not-implemented` answer for these queries is
  gone. `openehr-federation` 0.0.27 adds the `attribute` module with
  `EndpointAttribute`, `ColumnSource::Endpoint(EndpointAttribute)`,
  `Analysis::attributes`, `NodeAnswer::with_attributes` and
  `Merged::attributes`; the engine adds `Plan::annotating` and
  `FederatedAnswer::attributes`. CP-37 is covered.
- Track 10, the adversarial identifier-leakage suite, judged on node-side
  wire capture (#90; §16.3 track 10, §5.4, N33, N34, N5, CP-26). One
  synthetic patient identifier is supplied in each of the four positions the
  track names: an `EHR_STATUS.subject.external_ref` predicate, a
  `PARTY_IDENTIFIED`/`DV_IDENTIFIER` predicate, a `SELECT` projection, and
  the client's query string or headers (`X-Request-Id`, `Authorization`,
  `Prefer`, the targeting and completeness headers, and a free-form header).
  Each runs on the fan-out, on a query directed at one node, and on the
  single-node route, and the journal of the capturing proxy in front of each
  node must hold no occurrence of the identifier, its namespace or any of its
  fragments in the path, the query string, a header or the dispatched AQL,
  raw or percent-decoded; a refusal must be the `400` the specification
  names, with no node asked. Every dispatched request must locate its node by
  the node's own `ehr_id` alone, and a projected subject column must be the
  re-injected identifier. The converse check commits a `COMPOSITION` with
  the identifier in a `DV_IDENTIFIER` and finds it byte-identical at the
  node, compared by bytes and by digest. The gateway's own log (the request
  lines, the security events and the panic line) is searched too and must
  hold none of it. The suite runs against two mock nodes in the normal suite
  and against the two FerroEHR nodes of the harness behind `FERROFED_E2E`,
  and the conformance matrix records track 10 as covered for the gateway's
  half; CP-27, the node's own obligation, stays with the node profile (#93).
  `ferrofed-testkit` adds the `leak` module, the search over the proxy
  journal.
- The `openEHR-federation-endpoint` and `openEHR-federation-organisation`
  request headers, the targeting mechanism beside the AQL (#71; §8.4,
  §8.4.1, N35, CP-28). Each carries a comma-separated list of registry ids,
  over one field line or several, and selects nodes exactly as the
  `FROM ENDPOINT` and `ORGANISATION` directive does: a listed endpoint where
  the patient is not known is `not-resolved`, every other endpoint is
  `excluded`, an identifier the registry does not know, or a header with no
  identifier in it, is a `400` (`endpoint-unknown`, `organisation-unknown`),
  and a selection of no endpoint is `404 no-destination`. A query directed at
  one endpoint by the header may be an aggregate, as one directed by the AQL
  may. The headers apply to the federated query and to a request routed to
  one node, where together they select exactly one endpoint. When the
  directive and a header, or the two headers, appear in one request, the
  same node set proceeds and different sets are refused `400` with the new
  code `targeting-conflict`, whose message names both sets by their registry
  endpoint ids; the gateway never merges them and never picks one. No query
  parameter targets anything: `?endpoint=` and `?organisation=` have no
  effect on the federated query, and on a routed request they are refused
  `400 query-parameter-refused` like any parameter the operation does not
  declare. Neither header reaches a node: both join `Authorization` and
  `X-Request-Id` in the set a routed request always withholds.
- The registry document in FHIR form (#74; N19, N20, N21, §15.2, CP-13,
  CP-20). With `registry.format = "fhir"`, `registry.document` names a FHIR R4
  JSON `Bundle` of `Organization` and `Endpoint` resources, the shape an mCSD
  directory delivers, which loads into the same members and routes
  identically to the native TOML form. The registry's ids travel as
  identifiers in FerroFED's systems under `https://ferrofed.eu/fhir/sid/`,
  and an endpoint's `connectionType` is `openehr-rest-query` in
  `https://ferrofed.eu/fhir/CodeSystem/connection-type`. `config check`
  refuses, naming the resource, an endpoint whose `connectionType` is
  `hl7-fhir-rest`, an informal string or any other code, an endpoint whose
  managing organisation is missing or not in the Bundle, an endpoint no
  organisation or two organisations operate, an id that is missing or
  repeated, and a node its endpoints disagree on. `ihe-iti` 0.0.8 reads the
  directory content under its `mcsd` feature, resolving references inside the
  Bundle as FHIR R4 §2.36.4.1 does.

- The `FROM ENDPOINT` and `ORGANISATION` directive in AQL (#70; §8.1,
  §8.4.1, N11, N19, N20, CP-6). `FROM ENDPOINT p ["node-a-pub", …]` asks
  exactly the listed endpoints, and `FROM ORGANISATION ["org-a"]` asks every
  endpoint the registry lists as managed by each organisation. The
  identifiers are registry ids, never URLs. The directive does not replace
  resolution: each listed endpoint is still asked about its own `ehr_id`, a
  listed one where the patient is not known is `not-resolved` and fails
  nothing, and every endpoint the directive does not list is `excluded`. The
  directive is parsed by `openehr-query`'s `federation` feature and never
  reaches a node: each node receives the same standard AQL as for the
  undirected query. An identifier the registry does not know is refused `400`
  with the new codes `endpoint-unknown` and `organisation-unknown`, which
  locate it by its place in the list and never quote it, and an organisation
  that manages no endpoint answers `404 no-destination`. A query directed at
  one endpoint dispatches an aggregate or a function AQL does not define
  unchanged (N14, §11.6.3). The directive's variable may only be selected, as
  an ENDPOINT attribute, which #72 adds to the rows, and any other use is
  refused `400` with the new refusal `endpoint-variable`. `openehr-federation` adds
  `aql::directive::FacadeQuery`, `Context::with_targeting`,
  `Analysis::sources` and `ColumnSource::Endpoint`. Golden case 02 now
  passes, and CP-6 is covered. The endpoint and organisation headers of §8.4
  are #71.

- The registry maps every observed `creating_system_id` (#67; §12.2, N21,
  the mapping half of CP-13): a `[[creating_system]]` entry in the registry
  document maps a `creating_system_id` that is no member's own `system_id` to
  an endpoint, and a member's own `system_id` maps to that member without one.
  The document is refused, by `config check` too and naming the
  `creating_system_id`, when a mapping names an undeclared endpoint, maps one
  id twice (ASCII case aside), or maps a member's own `system_id`.
  `ferrofed-registry` adds the learned map: the first sighting of an id the
  document does not route learns a read route to the endpoint it was seen
  at, and a learned mapping never overrides the document. A sighting at a
  second node, or a learned mapping the document contradicts, withdraws it
  and raises an integrity incident, logged at `ERROR` with a stable kind and
  routing ids only. An id nothing routes is a typed miss, never a default
  endpoint. The follow-up read routing that consumes the table is #64.

- Opt-in version-identity dedup (#56; §10, N15, N36, CP-9, CP-29): a request
  that sends `openEHR-federation-dedup: version-identity` gets one copy of a
  version held at several endpoints, keyed on the full `OBJECT_VERSION_ID`
  of the row's `COMPOSITION`, else `VERSION`, uid as `openehr-base` reads
  it. The copy kept is the one from the endpoint whose registry `system_id`
  is the version's `creating_system_id`, else the one from the lowest
  endpoint id. Two versions of one object are two rows, and a row with no
  version uid is never suppressed. `meta.federation.dedup` records the mode
  on every answer, `none` and failing `424` and `504` envelopes included,
  and beside the rows `suppressed_rows` and `suppressed_endpoints[]`, counted
  before `DISTINCT`, `OFFSET` and `LIMIT`. Every node is asked the version
  uid, as a hidden column when the client does not select it (under
  `DISTINCT` only a selected uid is the key), and a query with a `LIMIT` and
  no `ORDER BY` is ordered on it. Under the mode a tie on the `ORDER BY`
  keys breaks on the uid before `endpoint_id`, which keeps a per-node
  `LIMIT n` (or `k + n` for a page) exact after suppression (§11.6.1). The
  default stays `none` (§10.1), which `none` states explicitly; any other
  value, or a repeated header, is `400` `dedup-invalid`. A node whose version
  uid is not an `OBJECT_VERSION_ID` is `node-error`, and the query fails
  `424` under all-or-nothing. A recombined aggregate under the mode is `400`
  `indecomposable-aggregate` (§11.6.3). `openehr-federation` 0.0.20 adds
  `dedup::DedupMode`, `aql::Context::with_dedup`,
  `order::ResultOrder::with_version_key`, `merge::NodeAnswer::with_system_id`,
  `merge::Suppressed`, `merge::Disagreement::VersionId` and
  `aql::refusal::Indecomposable::Dedup`.

- `SELECT DISTINCT` at the Tier (#55; N13, CP-8, CP-32): a row two nodes
  return is answered once, compared on the columns the client selected under
  the Tier comparator (`2` and `2.0` are one value, two spellings of one
  instant are two), and the duplicates are removed before the `LIMIT` and
  the `OFFSET` (AQL 1.1.0 §LIMIT), so a duplicate no longer takes two slots
  of a `LIMIT n` answer or of a bounded `OFFSET` page. The copy kept is the
  first under `ORDER BY`, then `endpoint_id`. A re-injected subject column
  and a column the gateway adds never make two rows distinct. Each
  endpoint's `row_count` stays what it contributed (§9.5). A node that
  returned its full `LIMIT` with two rows the Tier holds equal is
  `node-error`, because a distinct row can lie past its cut, and the query
  fails `424` under all-or-nothing. `openehr-federation` 0.0.18 adds
  `order::ResultOrder::with_distinct` and `distinct`, and
  `merge::Disagreement::Distinct`.

- Single-node routing for the EHR area (#61; §7a.1, §7a.3, §9.6, §11.2, N22,
  N31, N33, CP-24): every ITS-REST operation under `{base}/v1/ehr/{ehr_id}`,
  named by `openehr-its`'s `routes::lookup`, is forwarded once to the node
  the `openEHR-federation-endpoint` header names, through `Client::forward`.
  The body reaches the node byte for byte, a `DV_IDENTIFIER` in a committed
  `COMPOSITION` included, and the node's status, body, `Location` and `ETag`
  come back unmodified, a `404` or `500` included. Every routed answer names
  the acting endpoint and its node's `system_id` in
  `openEHR-federation-endpoint` and `openEHR-federation-system-id`. Only the
  request headers and query parameters the matched ITS-REST operation
  declares travel, read from `openehr-its`'s per-operation parameter table
  (FerroEHR #3530): `If-Match` reaches the node on a `PUT` and never on a
  `GET`. The client's `Authorization` and `X-Request-Id` never travel, and
  the node receives the request's minted `X-Request-Id`. An undeclared query
  parameter, and `subject_id` or `subject_namespace` anywhere, is refused
  before dispatch. New codes: `target-required` (a write that names
  no node, `400`), `endpoint-unknown` and `endpoint-several` (`400`),
  `query-parameter-refused` (`400`), `node-timeout` and `node-unreachable`
  (`504`) and `node-refused` (`424`, a node that refused the gateway's onward
  credentials). A read that names no node still answers `501` until #62
  routes it. A node's `3xx` is its answer, passed on with its `Location`
  unmodified, and no request is re-sent to a host outside the registry.
- Decomposable aggregates (#54; §11.6.3, N14, N39, CP-10, CP-32): an
  undirected `COUNT`, `SUM`, `MIN`, `MAX` or `AVG` is sent to every node and
  answered with one recombined row in the client's columns, never one row
  per node. Counts and sums add exactly, reals in decimal arithmetic; `MIN`
  and `MAX` are re-applied over numbers and complete date-times; `AVG` is
  asked of each node as its `SUM` and `COUNT`. A node value the
  recombination cannot use is `node-error`, so the query fails `424` with no
  value. `DISTINCT`, `COUNT(DISTINCT …)` and a plain column beside an
  aggregate are refused `400` (`indecomposable-aggregate`), and a `partial`
  request for a recombined aggregate is refused `400` (`partial-aggregate`).
  The new `[federation] decomposable_aggregates` list (all five by default,
  `[]` for none) declares the functions, and an undeclared one is still
  `undirected-aggregate`. A directed single-node aggregate is sent unchanged.
  `openehr-federation` 0.0.13 adds the `aggregate` module,
  `aql::Context::with_decomposable_aggregates` and `merge::combine`, and
  takes `rust_decimal` 1.43.0, already in the tree through `openehr-rm`.

- The full per-endpoint report (#49; §9.5, §11.1, N16, N40, CP-11, CP-31):
  every registry member appears in `meta.federation.endpoints[]`, an endpoint
  a directed request did not name as `excluded` with the reason, which stays
  out of scope, so it neither clears `complete` nor fails the query. A
  `[[node]]` of the registry document may record its CDR `product` and
  `version`, which the report carries only from there and omits when the
  registry does not say. `latency_ms` appears exactly for the endpoints the
  gateway dispatched to, and `row_count` counts what each node contributed
  before any federation-level `DISTINCT`, dedup or `LIMIT`.
- Completeness (#50; §11.2 to §11.4, N6, N37, CP-30): all-or-nothing stays
  the default, and a request opts into best-effort with
  `openEHR-federation-completeness: partial`. Under best-effort the gateway
  answers `200` with the rows of the nodes that answered, names every other
  node with its status, and sets `complete: false`. A cross-reference that
  cannot answer is reported there and is not a `424`. `all` is accepted
  explicitly. Any other value, a repeated header, or `partial` where
  `federation.best_effort = false` withdraws the mode is a `400` that asks no
  node and never quotes the value. The `not-resolved` and `consent-denied`
  carve-outs and the scope rule hold in both modes. `complete` is always
  derived from the statuses.
- Timeouts (#51; §11.5, N38, CP-31; RFC 7240): a client shortens the budget
  with `Prefer: wait=<seconds>`, and a longer wait leaves the configured
  budget in force. The effective budget is the one reported in
  `meta.federation.timeout`, and a wait that set it is echoed in
  `Preference-Applied`. Only the first `wait` counts; a malformed one is
  ignored and never refused. `wait=0` asks no node and reports each one
  `time-out`. The overall budget runs from the request's arrival, so the
  patient resolution and the fan-out share it and the gateway answers inside
  it, and abandoning one node never aborts a request in flight to another.
- `ORDER BY` with `LIMIT` re-applied at the Tier, with a deterministic
  tie-break (#52; §11.6.1, N9, N13, N39, CP-8, CP-32). Every node is sent the
  client's `LIMIT n` unchanged, with the row's uid appended as the last
  `ORDER BY` key; an `ORDER BY` path the query does not select travels as a
  hidden column the client never sees. The gateway merges the node answers
  under one total order (null greatest, numbers exactly, complete date-times
  by instant, strings by code point, `DV_ORDERED` values through the openEHR
  RM's own comparison), breaks ties on the endpoint id and then the uid, and
  cuts the result at `n`. A node that returned `n` rows out of that order, or
  more than `n`, is reported `node-error`, so the query fails `424` under
  all-or-nothing. `TOP n` is read as `LIMIT n`; `TOP n BACKWARD`, a `TOP` beside a
  `LIMIT` clause, and a `DISTINCT` query ordered on a path it does not
  select are refused `400`. A query with `LIMIT` and no `ORDER BY` now returns
  at most `n` rows across all nodes.
- `OFFSET` paging across a fan-out (#53; §11.6.2, N9, N39, CP-32). `OFFSET`
  never reaches a node. Under the default `federation.offset_strategy =
  "bounded"`, `ORDER BY … LIMIT n OFFSET k` asks each node for `LIMIT k + n`,
  checks each node's visible order as for `LIMIT n`, merges in the federation
  order and returns rows `k` to `k + n`. A page whose `k + n` is past
  `federation.max_offset_window` (1000 rows per node by default; 0 refuses to
  boot), an `OFFSET` with no `LIMIT`, and an `OFFSET` with no `ORDER BY` are
  refused `400`, the first naming the bound. `offset_strategy = "reject"`
  refuses every `OFFSET` past zero `400`. The ITS-REST `offset` and `fetch`
  members follow the same strategy.
- The `comment-style` guard checks citations (#173). It fails on a Rust
  comment, doc comment or lint `reason` that cites `docs/architecture.md`, a
  path under `.claude/` or a rule file by name, or that names a
  decision-register entry such as `decision A17`. It applies the same check to
  the full-line `#` comments of the shell scripts under `scripts/` and of every
  `Cargo.toml`. `comment-style.sh --self-test` proves each refused form and
  its near misses, and CI runs it before the full-tree pass. Every comment that
  cited one of these now cites the specification section it rests on, or says
  that no specification governs it.
- The error vocabulary (#57; §11.2, N32, N36, N37, N42, CP-12, CP-30): every
  failure the gateway reports on its own behalf answers the ITS-REST `Error`
  body with a stable `code` and the `request_id`, and its status follows one
  table. A refused query is a `400` named by its refusal (`not-aql`,
  `unreducible`, `offset-unsupported`, and the rest), an unknown path a `404`
  `not-found`, an unexposed ITS-REST area a `501` `not-implemented`, and the
  gateway's own fault a `500` `internal`, and a request with no member in
  scope a `404` `no-destination`; `ehr-id-collision` and
  `controlling-system-unreachable` (`409`) are fixed for follow-up routing.
  The codes are API and are only ever added. The book's "Errors and status
  codes" page lists them all, and a test holds the page to the gateway's
  table.
- The `versions` guard holds two hand-typed version facts to their sources
  (#181). The landing page's release note and status panel must name the
  newest `## [x.y.z]` release of `CHANGELOG.md`, so a release cut that forgets
  the page fails. Each specification row of `docs/VERSIONS.md` must name a
  crate constant (`FEDERATION_SPEC`, `ITS_REST`, `AQL`) that carries the
  version the row pins. `versions.sh --self-test` proves both checks, and CI
  runs it before the full pass.
- Generated conformance badges on the README (#175), as shields.io endpoint
  files under `conformance/badges/`: the Gateway points of §17 covered out of
  the Gateway total, the Node and Operator point counts, each labelled with the
  pinned specification version and linked to the book's conformance page, and
  the AQL golden cases passed out of the vendored corpus, linked to its pass
  list. `scripts/conformance/matrix.sh --badges-write` writes the files and the
  README block between `conformance:begin` and `conformance:end`, and the
  `conformance-matrix` guard fails when either drifts from the matrix or the
  pass list. The golden test fails when a case in
  `conformance/aql-golden/pass-list.txt` stops passing or an unlisted case
  passes, and rewrites the list when `FERROFED_CONFORMANCE_UPDATE` is `1`.
  Case 02 (`FROM ENDPOINT`) is refused until #70 and is not counted as
  passing. The static badge row gains the image-pulls badge for
  `ghcr.io/ferrohealth/ferrofed`.
- The `versions` guard holds the book's "Pinned versions" page to
  `docs/VERSIONS.md` (#198). Each row of its pin table names its matrix rows
  by their exact names, and every pin it states, a version, a package, an
  `edition 2024` or an abbreviated `commit`, must be the one those rows pin;
  a pin the guard cannot read fails rather than passing unread. The page now
  lists all five `openehr-*` crates, the vendored PIXm and PDQm packages and
  the specification's source commit as rows of their own. `versions.sh
  --self-test` proves each drift.
- Tests that pin the two optional facilities FerroFED does not offer (#59,
  #60; §11.6.4, §11.7, CP-31, CP-32). A query sent with `Prefer:
  respond-async`, alone or beside `Prefer: wait`, gets the ordinary
  synchronous answer under the same budget: never a `202` or a
  `Content-Location`, and `Preference-Applied` never names `respond-async`
  (RFC 7240 §2, §3). A bounded `OFFSET` page carries no cursor handle or
  expiry in `meta.federation` and runs the fan-out on every request.

### Changed

- A gateway with `registry.document` set now refuses to boot without
  `federation.id`, the federation's own identifier that `OPTIONS {base}/`
  names (#73; §7a.2, N30). Add `id = "<your federation>"` to `[federation]`;
  an empty id is refused.

- The `openehr-*` family moves from 0.0.78 to 0.0.79 (#234). The
  `openehr-rm` attribute model now holds the BASE primitives, the `Ordered`
  marker and the target class of a reference-typed attribute (FerroEHR
  #3537), which the AQL rewrite reads to decide which selected paths a node
  can order under `DISTINCT`. `openehr-federation` is 0.0.28.
- The `openehr-*` family moves from 0.0.77 to 0.0.78 (#237), and an onward
  credential is checked at configuration load by the node client's own
  `Authorization` composition (FerroEHR #3535), for each endpoint and each
  PIX Manager alike. A bearer token must now be the `b64token` of RFC 6750
  §2.1 (letters, digits, `-`, `.`, `_`, `~`, `+` and `/`, then any `=`
  padding), so a token holding a space, a quote or any other character
  outside that set is refused by `config check` and at boot with exit code
  78, naming its key and never its value; it used to pass and then fail at
  the node. The basic rules of RFC 7617 §2 are unchanged.
  `openehr-federation` is 0.0.26.
- The `openehr-*` family moves from 0.0.76 to 0.0.77 (#195). `openehr-query`
  now classifies every AQL function call (FerroEHR #3529): a string, numeric,
  or date and time function of AQL 1.1.0 is a built-in, and any other name is
  not. The rewrite reads that classification where it kept a list of the 16
  function names of its own, so an undirected query that calls a function
  AQL does not define is refused exactly as before (`undefined-function`;
  §11.6.3, N14), and the `CONCAT`, `CONCAT_WS` and `SUBSTRING` folding and
  the `DISTINCT` cut check read the crate's classification too. Every
  `openehr-its` client is built with redirects off (FerroEHR #3531), so a
  node's `3xx` answer is never followed: the node is `node-error`.
- Incompleteness is carried by `meta.federation.complete` alone, and the
  gateway emits no FHIR `OperationOutcome` (#58; §9.1, §11.4, N17, CP-12).
  §11.4, CP-12 and track 4 ask for the resource only for a FHIR-facing
  consumer, and the answer is an ITS-REST `RESULT_SET` whose federation
  additions live under `meta.federation`. The conformance matrix gives CP-12
  that reason beside its scored status codes, a test asserts that a
  best-effort `200` and an all-or-nothing `424` each carry `complete: false`,
  validate against `federated-result-set.schema.json` and hold no
  `OperationOutcome`, and the client contract in the book says so. Where the
  resource would travel for a gateway with FHIR-facing consumers is recorded
  as an upstream report (#204).
- The conformance record's loose ends (#196). The `comment-style` citation
  checks read the conformance tables under `conformance/`: their `#` comment
  lines and their `reason` column, which the book renders. The tables now
  cite §16.2, §16.3, §16.4 and §17 where they cited an internal file or a
  decision-register entry, or say that no specification governs the choice.
  The CP-10 and CP-32 rows name #187. The book page links the specification
  site at the version `docs/VERSIONS.md` pins, with no second copy of it in
  `scripts/conformance/matrix.sh`. `openehr-federation` 0.0.16 marks its
  local list of AQL function names for removal (#195).
- The `comment-style` citation checks cover the rest of the tree (#178): the
  full-line comments of the workflow and composite-action YAML under
  `.github/`, the `echo` and `printf` text of a workflow `run:` block,
  `clippy.toml` with its `reason` strings, and the quickstart TOML under
  `docker/`. The per-edit hook runs the guard on every file kind CI checks.
  Every comment, lint reason and printed line there that cited an internal
  file or a decision-register entry now cites the specification section or
  official documentation it rests on, or says that no specification governs
  it. The six vendor scripts no longer write an internal path into their
  `PROVENANCE.md`.
- The `comment-style` citation checks refuse a citation of any markdown file
  under `docs/` outside the vendored `docs/specs/` tree (#184): a path that
  opens a parenthetical or is followed by `section` or `§`. Naming
  `docs/VERSIONS.md` as the file a script or test reads still passes. The
  checks also read every YAML `description:` scalar and the trailing `#`
  comment after a YAML or TOML value, outside quoted strings and block
  scalars, and the guard runs the same under mawk. The citations this
  catches cite the GitHub documentation or say that no specification
  governs them, among them `fuzz.yml`, the `setup-rust` action
  description, the version and corpus scripts and the CI rule. The crate
  manifests' comment on the 0.0.0 name reservation changes with them, so
  `openehr-federation` moves to 0.0.15, `ihe-iti` to 0.0.7 and
  `nl-generic-functions` to 0.0.4 with no change to their code.
- The `comment-style` citation checks also refuse a citation of a
  `README.md` of the tree, at the root or in a member, outside the vendored
  `docs/specs/` and `vendor/` trees (#198), in the same citation form. Naming
  a README as a file a script reads still passes. `fuzz.yml` cites the
  libFuzzer documentation for how a crashing input is kept, and the scripts
  that cited a README no longer do.
- The release lane fails a tag whose tree has no root `Cargo.toml` (#198),
  with a message saying the tag cannot be checked against the workspace
  version. It used to skip that check.
- The release lane has one path (#201): the binaries and the image build for
  every tag `plan` accepts, and `finalize-release` publishes only when every
  build leg succeeded. The branches for a tag with no workspace, which `plan`
  now refuses, are gone. The book's claims page links to the pinned-versions
  page for the `openehr-*` pin instead of restating the number, and
  `docs/ci-cd.md` names all four self-tests of the `tracker-helpers` job,
  `rel.sh` among them.
- `scripts/gh/fields.sh` checks its arguments before any `gh` call (#198).
  No argument, an unknown command, a wrong operand count or `--help` prints
  the usage to stderr and exits 2, with no network call and no token needed.
  `fields.sh --self-test` proves it against a stub that records every call.
- The per-edit `comment-style` hook also reads the `conformance/*.tsv`
  tables (#198), as CI does, and stays a quiet pass for any other file.
  Conformance track 5 (Dedup + DISTINCT) names #187 beside its issues, as its
  CP-10 and CP-32 rows do, and the book's conformance page is re-rendered.
- Error bodies (#57): the gateway's own refusals (`404`, `501`, a caught
  panic's `500`) answer the ITS-REST `Error` shape with `code` and
  `request_id`, where they named the code in an `error` member, and the codes
  are kebab-case (`not-found`, `not-implemented`). No error body quotes the
  query, a parameter value or a header value (§5.4.3); a `424` or `504` under
  all-or-nothing stays the §11.4 result set and echoes the client's own `q`
  (N17). A node row shorter than the dispatched query selects is now that
  node's `node-error` (`424`, or reported under `partial`), where the whole
  query answered `502` (§11.1, §11.2).
- The `openehr-*` family moves from 0.0.74 to 0.0.76 (#57). 0.0.76 keeps the
  members an open ITS-REST schema admits in an `additional_properties` map
  (FerroEHR #3526), so every error body is the generated ITS-REST `Error`
  with `code` and `request_id` in that map, and no FerroFED-side error type
  remains. The generated request and result-set types carry the same map.
- The site, the README and the book describe the released gateway (#176).
  The landing page opens with the FerroFED mark and the current release, and
  shows the quickstart, the release binaries and the image
  `ghcr.io/ferrohealth/ferrofed`, with what each specification property has
  in v0.0.3 and what is planned. The README's install section names the
  release assets and their verification, and its quickstart runs the
  published image with `docker compose up --wait`. The book's introduction,
  claims, deployment, container and contribution pages, the governance and
  contribution guides, and the working rules no longer describe a design
  phase or a project with nothing to install.
- The quickstart and the end-to-end harness run two FerroEHR nodes (#155,
  decision A44). EHRbase left both, because it refuses a `.` in
  `PARTY_REF.namespace`, which openEHR BASE admits (#118). In `compose.yaml`
  the services are `ferroehr-a` and `ferroehr-b`, each on its own database
  and with its own `system_id` (`node-a.quickstart.local`,
  `node-b.quickstart.local`, which the quickstart registry declares), and
  node B moved from port 8091 to 8082 (`FERROEHR_B_PORT`; node A's
  `FERROEHR_PORT` is now `FERROEHR_A_PORT`). Every end-to-end case seeds the
  patient's subject on both nodes, and the `e2e (containers)` CI job now runs
  the server's container tests as well as the testkit's.
- The tracker records the kind, the urgency and the size of an issue in
  GitHub's native issue type (Bug, Feature, Task) and the organisation's
  Priority and Effort fields (#154). The `bug`, `enhancement` and `P0` to
  `P3` labels are retired, the bug and feature forms set the issue type, and
  `scripts/gh/fields.sh` and `scripts/gh/migrate-fields.sh` carry the model
  and the migration.

### Fixed

- The local crate-bump hook judges a commit or a push against `origin/main`,
  the base CI uses, and leaves finding the merge base to
  `scripts/checks/crate-version-guard.sh` (#243). A branch that bumps a crate
  to the version main already took is now blocked locally, as CI refuses it,
  where it used to pass. The book's checks page lists the `tracker-helpers`
  job, the self-tests of the `scripts/gh` helpers, among the tier-1 checks.
- The outbound gate no longer reads the host and port of a registry
  endpoint URL (#232; §5.4.1, N33, CP-26). An endpoint whose host or port
  contained a withheld identifier, such as a short local identifier that
  matched a port number, refused every query for that patient. The
  authority comes from the operator's registry and never from a request,
  and §5.4.1 names the request path, the query string and the headers. The
  gate still reads the path, the query and the fragment, raw and
  percent-decoded, with the AQL text, the paging and the headers the
  gateway adds.
- A `SELECT DISTINCT` query with `ORDER BY` and `LIMIT` (or a bounded
  `OFFSET` page) that selects a path whose value AQL defines no order for is
  refused `400` (`incomparable-distinct-key`), where a node could cut among
  distinct rows differently on each repeat (#234; §11.6.1, N13, CP-8,
  CP-32). AQL orders on data "available to primitives and `Ordered` types"
  (AQL master03-syntax §ORDER BY), so a whole RM object (`SELECT DISTINCT
  c`), a data value that is not a `DV_ORDERED` (`c/name`, a `DV_TEXT`), the
  `DATA_VALUE` of an `ELEMENT`, a collection and a path the RM does not
  resolve are not keys. The rewrite decides it by walking the path through
  `openehr-rm`'s static model from the class `FROM` binds, asking the
  model's `is_primitive` and `conforms_to_ordered` lookups and following a
  reference to the target class the model names (FerroEHR #3537). Under
  `DISTINCT` only the selected paths to a primitive or a `DV_ORDERED` are
  pushed as `ORDER BY` keys, so the same query without a `LIMIT` is answered
  and the other paths are compared at the Tier alone. A path to a primitive
  value, such as `c/name/value` or `…/value/magnitude`, and an ordered data
  value, such as `c/context/start_time`, are pinned as before. The `aql`
  feature of `openehr-federation` now depends on `openehr-rm`; the crate is
  0.0.28 and adds `Refusal::IncomparableDistinctKey`.
- A query with `ORDER BY` and `LIMIT` (or `TOP`, the `fetch` member, or a
  bounded `OFFSET` page) ordered on a path whose value AQL defines no order
  for is refused `400` with the new code `incomparable-order-key`, where it
  used to be sent to every node and re-ordered at the Tier unchecked (#242;
  §11.6.1, N39, CP-32). AQL ordering "assumes that data identified by the
  path … are comparable", the primitives and `Ordered` types (AQL
  master03-syntax §ORDER BY), and §11.6.1 holds the federated first `n`
  rows in each node's first `n` only "under a total order", so a node's cut
  on `ORDER BY c/name LIMIT 10`, a `DV_TEXT`, could drop a row of the
  answer. The rewrite uses the comparability rule of #234, and the
  refusal points at the key by position. The same order with no `LIMIT` is
  answered: every row reaches the Tier, which orders them all under its own
  total order, the same on every repeat. Under `DISTINCT` the refusal stays
  `incomparable-distinct-key`. `openehr-federation` is 0.0.29 and adds
  `Refusal::IncomparableOrderKey`.
- Under version-identity dedup, `SELECT DISTINCT` compares the version uid
  without regard to case, as the dedup and the Tier order already do (#234;
  §10.2, CP-8, CP-9; BASE `master05-identification_package.adoc`
  §"Composite Identifiers and Case"; AQL master03-syntax §DISTINCT). Two
  rows of one node whose uids differ only in case are one distinct row, the
  first in the Tier order, with its text as the node sent it; a node cut at
  its `LIMIT` that returns both is refused as `node-error`. Outside dedup a
  uid compares as sent, in step with the Tier order.
- A `SELECT DISTINCT` query with `ORDER BY` and `LIMIT` (or a bounded
  `OFFSET` page) whose selected function column the selected paths do not
  fix is refused `400` (`unordered-distinct-cut`), where a node could cut
  among distinct rows tied on every path differently on each repeat (#210;
  §11.6.1, CP-8, CP-32). AQL orders only on paths (AQL master03-syntax
  §ORDER BY), and under `DISTINCT` the gateway adds no column, so the
  selected paths are a node's only keys. A call to a single-row function AQL
  defines whose arguments are literals, parameters and selected paths, such
  as `LENGTH(c/name/value)` beside `c/name/value`, is answered as before; a
  call that reads a path the query does not select, a clock function such as
  `NOW()`, and `TERMINOLOGY` are refused. The same query without a `LIMIT`
  is answered. `openehr-federation` 0.0.22 adds
  `Refusal::UnorderedDistinctCut`.
- Under version-identity dedup, the Tier orders the version uid without
  regard to case (#229; §10.2, §11.6.1, §11.6.2, CP-9, CP-32; BASE
  `master05-identification_package.adoc` §"Composite Identifiers and
  Case"). Two copies of one version whose ids differ only in case rank as
  one in the merged order and in the check of each node's order, as an
  `ORDER BY` key and as the tie-break, so an `ORDER BY` with `LIMIT` and a
  page under dedup stay exact for them. Every row keeps its uid as its node
  sent it. The comparison is `openehr-base`'s `composite_id_key`. A node cut
  at its `LIMIT` that orders uids byte for byte and returns them in an order
  the Tier's disagrees with is refused as `node-error`, since the Tier
  cannot change how a node sorts. Outside dedup the uid orders as before, by
  code point. `openehr-federation` is 0.0.23.
- An onward credential the `Authorization` header cannot carry is refused at
  configuration load, by `config check` and at boot with exit code 78, in
  place of failing every query to its endpoint with a `500` (#231). A bearer
  token, inline or read from `bearer_token_file`, is checked as the
  `Bearer <token>` value the node client sends, by the same `http` parse the
  client applies, and a basic user or password holding a control character,
  or a basic user holding a colon, is refused as RFC 7617 §2 requires. The
  refusal names the key the value came from
  (`credentials.<endpoint>.bearer_token`, its `_file` sibling, `.user` or
  `.password`) and never the value. A PIX Manager's
  `[pixm.manager.credentials]` is held to the same rule.
- Version-identity dedup compares identifiers without regard to case (#225;
  §10.2, CP-9; BASE `master05-identification_package.adoc` §"Composite
  Identifiers and Case"). Two copies whose version ids differ only in case
  are one version, and the node whose `system_id` differs from the version's
  `creating_system_id` only in case keeps the originating copy. Before, both
  were compared byte for byte. A kept row still comes back as its node sent
  it. The comparison is `openehr-base`'s `composite_ids_equal` and
  `composite_id_key`, which the registry identifiers already use, and the
  session-scoped resolution bindings now key on `EhrId` in place of their
  own case fold. `openehr-federation` is 0.0.21.
- A patient query is no longer refused at random when its withheld
  identifier is a short hexadecimal value that occurs inside the minted
  `X-Request-Id` (#227; §5.4.1, N33, CP-26). The outbound gate reads the AQL
  text, the paging, the URL and every other header the gateway adds, and
  skips the minted id, which `OutboundId::mint` makes from no client input.
  A request a panic, the request timeout or the body ceiling answers now has
  its request-log line, with the status it answered (`500`, `408`, `413`)
  under the gateway's id. The "the federated query failed" line carries the
  same `request_id` as the request line. The hygiene assertions of the tests
  compare the raw bytes a mock node received, so a header value or a body
  that is not UTF-8 is searched too, and the CP-26 row of the conformance
  matrix names #217.
- CP-29 is `planned` again in the conformance matrix and the gateway badge
  (#221): only its visibility half, the dedup record of §10.2 and §10.3, is
  built, and its write-routing and `409` half is #66. The dedup tests carry
  CP-9 alone. `scripts/checks/crate-version-guard.sh` run without its base
  ref prints its usage on stderr and exits 2.
- The book's "What FerroFED claims" page lists decomposable aggregates (#54)
  among what has merged since v0.0.3, and no longer as planned; de-duplication
  is what remains planned for v0.0.4 (#211). The `merge` module doc of
  `openehr-federation` is wrapped like the rest of the crate's docs, and
  `openehr-federation` moves to 0.0.19 with no change to its code.
- An `ORDER BY` with `LIMIT` query whose rows carry no uid, such as one that
  selects from `EHR` alone, returns the same rows on every repeat (#157;
  §11.6.1, §11.6.2, N39, CP-32). Each node is asked to order on a row key
  after the client's keys: the uid of the first `COMPOSITION` or `VERSION`
  as before, else `<ehr>/ehr_id/value`, else the uid of an `EHR_STATUS` or
  `EHR_ACCESS`; the key travels as a hidden column and never reaches
  `columns[]`. A node cut at `LIMIT n`, or at the `LIMIT k + n` of a bounded
  `OFFSET` page, then keeps the same tied rows each time, so two pages agree
  on the rows tied across their edge. A patient query, scoped to one
  `ehr_id` per node, gets no `EHR` key; `FOLDER` is never a key; a
  `DISTINCT` query keeps its selected columns as the tie-break and gains no
  column. A query with no row key is still answered: the Tier orders the
  rows it receives, and which tied rows a node returns is the node's.
  `openehr-federation` 0.0.17 carries the change.
- A query that calls a function AQL 1.1.0 does not define, such as a
  product-specific `MEDIAN(x)`, in `SELECT` or `WHERE` is refused `400`
  (`undefined-function`) when it would reach more than one node, where it was
  sent to every node and answered one row per node (#187; §11.6.3, N14, N39,
  CP-10, CP-32). The gateway cannot tell whether such a function aggregates,
  and per-node aggregate rows are the answer §11.6.3 forbids. Directed to one
  endpoint, the query is sent unchanged. The single-row functions AQL defines
  (AQL §Functions: the string, numeric, and date and time functions, and
  `TERMINOLOGY`) are still sent to every node as written, and the five
  aggregates keep the decomposition rules. `openehr-federation` 0.0.14 adds
  `aql::refusal::Refusal::UndefinedFunction`.
- A query that uses `TOP` together with a `LIMIT` clause is refused `400`
  (`top-with-limit`), whether or not the two counts agree, because AQL
  forbids the pair (#162; AQL §TOP, §LIMIT). A `TOP` query sent with the
  ITS-REST `fetch` member is refused `400` (`top-with-fetch`), because
  ITS-REST says `fetch` "cannot be combined with AQL-top". `TOP n` alone is
  still read as `LIMIT n`.
- A federated query that leaves no registry member in scope, because every
  endpoint is `excluded` (suspended by the operator, for example), answers
  `404` and asks no node, where it answered `200` with empty `rows` (#166;
  §11.1, §11.2, §11.3). A candidate set that localization left empty is not
  that case: every member is `not-localized`, and the answer stays `200`
  with `complete: true` (§14.1). A patient who is `not-resolved` at every
  member in scope still answers `200`.
- A gateway that federates refuses to boot, and `config check` refuses the
  file, unless `server.request_timeout_ms` exceeds
  `federation.overall_timeout_ms` by more than one second, the margin kept
  for combining the answers (#167; §11.5). The refusal names both keys.
  Before, a request timeout between the two let the server's `408`, with no
  body, cut a slow fan-out instead of the `504` that carries
  `meta.federation`.
- The documentation describes the gateway that exists (#163). The
  `openehr-federation` README names both subject carriers as resolution
  input. The book's client contract and configuration pages name
  `POST /v1/query/aql` as served, and the `_file` secrets paragraph sits
  under the configuration file it describes. Code and doc comments cite the
  specification sections a decision rests on, or say that no specification
  governs it.
- The landing page no longer scrolls sideways on a phone narrower than about
  380 px (#182). The audience cards and the other card grids take a column
  minimum that shrinks to the page width, so a 320 px viewport shows one
  full-width column in the light and dark themes.
- `llms.txt` describes follow-up routing to the owning CDR as planned for
  v0.0.5, where it described it as built (#180). The CI log, the guard
  scripts, the analyzer configuration and the `research` label no longer
  describe the repository as being in a design phase.
- `scripts/gh/labels.sh` prints its usage and exits `2` on any argument it
  does not know, `--help` included, before it calls `gh` (#180). It ignored
  such an argument and wrote the label taxonomy to the repository. Its
  `--self-test` proves the refusal.
- The `ferrofed-engine` crate description names what it holds (dispatch,
  fan-out, the budgets, the completeness decision and the outbound
  identifier-hygiene gate) and lists follow-up routing on the creating
  system id as planned, where it described routing as built (#191). The
  workflows, the release checklist, the version pages of the book and the
  repository, and the working rules no longer describe the Cargo workspace
  as not yet existing, and the book names the `openehr-*` pin as 0.0.76 and
  the PIXm and PDQm packages as vendored.
- `scripts/gh/rel.sh` prints its usage and exits `2` before it calls `gh` on
  a usage error: no argument, an unknown command, a wrong operand count, a
  flag other than `--replace`, or `--help` (#191). It exited `1` with no
  argument, `0` on `--help`, and resolved the repository through `gh`
  first. Its new `--self-test`, run by the CI `tracker-helpers` job, proves
  each write's endpoint and each refusal.

### Security

- The ask-all probe no longer sends a path `ehr_id` that is not a bare UUID
  to every member (#259; §5.4.1, §12.5.1, N33, CP-26). The path `ehr_id` was
  checked only as a `HIER_OBJECT_ID`, a form that admits a bare national
  number as a one-arc ISO OID, so a patient identifier in the `ehr_id` slot
  of a read nothing routed was broadcast to the whole federation. A read
  whose `ehr_id` is an ISO OID, an internet id or a UUID with an extension,
  and that no targeting header, resolution binding or `ehr_id` index entry
  routes, is now refused with the new code `probe-requires-uuid` (`400`),
  and no member is asked. The same `ehr_id` with the endpoint header is
  forwarded to the named node alone, and the index then routes it, since a
  member may mint `ehr_id`s in a scheme other than UUID (§12b.2, N42a).
  The refusal is logged as the security event `ehr-id-probe-refused`, which
  never names the `ehr_id`. Track 10 gains the identifier in the `ehr_id`
  slot. `ferrofed-registry` adds `EhrId::is_uuid`, and `ferrofed-engine`'s
  `probe::Probe` carries a `ProbedEhrId`, which only a bare UUID converts
  to, in place of the client's path segment.
- A panic no longer prints its message to stderr (#260; §5.4.3, CP-26).
  `ferrofed serve` used Rust's default panic hook, which printed the
  message, and any value formatted into it, to stderr, part of a
  deployment's log stream. The binary now installs its own hook before it
  serves. The hook writes one fixed line through `tracing`, "a thread
  panicked", with the source location and, inside a request, the gateway's
  `request_id`, and never the message. It writes nothing to stderr. A
  panicking handler still answers `500`, and its "the request handler
  panicked" line is unchanged. Track 10's panic case now runs in a child
  process and asserts that its stderr is empty. `ferrofed-server` adds
  `panic::install_hook` and `request_id::serving`.
- A request routed to one node no longer logs the client's `X-Request-Id`
  (#62; §5.4.1, §5.4.3, N33, CP-26). The security events of a refused query
  parameter or a withheld request, and the failure events of the routed path
  and the ask-all probe, named the client's own id, free text that can carry
  a patient identifier. Every event of the routed path now names the
  gateway's own id, the one its request line records; the client's id stays
  in the response and its error body only.
- A client's `X-Request-Id` no longer reaches any node (#217; §5.4.1, N33,
  CP-26). A legal client value was sent to every node of the fan-out as it
  came, so a patient identifier written into it passed the outbound gate,
  which checks only the identifiers the gateway resolved on. The gateway now
  mints its own id, a version 4 UUID, for every request and sends only that,
  the same id to every node of one request. The response header and the
  error bodies still name the client's own id. The request log, the security
  events and the panic line record the gateway's id and never the client's,
  with a `client_named` flag on the request line saying whether the client
  sent one; a client that sends none gets the gateway's id back. The book
  lists every header a node request carries and where its value comes from.
  `ferrofed-engine` adds `outbound_id::OutboundId`, which only
  `OutboundId::mint` makes, and `DispatchOptions::with_request_id` and
  `fanout::fan_out` take one in place of a string.

## [0.0.3] - 2026-10-02

The first two federated milestones in one release (v0.0.2 and v0.0.3; no
v0.0.2 tag was cut). `POST {base}/v1/query/aql` answers one ITS-REST
`RESULT_SET` over two openEHR CDRs, with the patient resolved outside AQL
through a PIXm PIX Manager (or the development cross-reference) on either
patient carrier, and no directly identifying identifier sent to a node: the
rewrite refuses it, and an outbound gate re-checks every request before it
leaves. Also the Cargo workspace and its spec-named crates, the PIXm and PDQm
clients, the container and compose quickstart, the end-to-end harness with a
PIX Manager, the fuzz lane, and the release lane at SLSA Build Level 3,
rehearsed as `v0.0.2-rc.1`.

### Added

- The PDQm ITI-78 patient demographics query client in `ihe-iti`, feature
  `pdqm` (#119; PDQm 3.2.0, ITI TF-2 §2:3.78). `pdqm::PdqmClient` posts a
  `PatientQuery` to `[base]/Patient/_search` as a form body, so no URL carries
  a demographic value, and reads the `searchset` into the matching Patients
  (FHIR R4 `Patient` from `fhir-types` `resources`), the `total`, each match's
  score and `match-grade`, the `OperationOutcome` warnings and the `next` page
  link, which it follows only on the Supplier's origin. A `404` with a
  `not-found` issue for a query that names an identifier domain is the
  profile's unrecognised-domain answer; every other failure is a typed error
  that carries no value, URL or Supplier text. The ITI-78 artefacts of the PDQm
  3.2.0 package are vendored under `docs/specs/ihe-pdqm/` by
  `scripts/vendor/ihe-pdqm.sh`. The `OperationOutcome` issue type moves to
  `ihe_iti::outcome::IssueType`, shared by PIXm and PDQm; `ihe-iti` is 0.0.5.
- The identity-lifecycle hook for track 8 (#48): `ResolutionBindings::identity_changed`
  drops every resolution binding a merge or split at the identity source could
  have made stale, by `ehr_id` in every session or all of them for an
  unscoped change, and `Federation::identity_changed` is the entry point a PMIR
  subscription calls. The binding lifetime, `federation.binding_ttl_ms`, is the
  bound until one exists. Track 8 stays provisional and is not claimed.
- The ask-all node selection of a deployment with no localizer (#46; §4.3
  Variant B, N4, N10). `federation.node_selection = "ask-all"` declares it,
  and a gateway that federates refuses to boot without the declaration
  (`NodeSelectionUndeclared`), so the choice is never a silent default. Every
  active member's cross-reference is asked, the query reaches only the members
  that return an `ehr_id`, and the others are `not-resolved` without failing
  the query (N6, N8). The selection is named in the startup log; the
  `OPTIONS` self-description follows with #73. The quickstart and the
  configuration page declare it.
- A PIX Manager in the test harness, seeded by ITI-104 (#47). The testkit's
  `pix::PixManager` is a test device, not a PIXm implementation: an in-process
  loopback server that takes the PIXm 3.1.0 Patient Identity Feed FHIR
  (ITI-104, a conditional `PUT Patient?identifier=` held to the
  `IHE.PIXm.Patient` minimums, plus the Remove Patient Option's conditional
  `DELETE`) and answers ITI-83 `$ihe-pix` from what was fed, with each case of
  ITI TF-2 §3.83.4.2.2. The seed builder feeds it a synthetic patient in the
  `urn:oid:2.999` arc with one `ehr_id` per node domain (`seed::feed`), and a
  capturing proxy in front of it journals and faults its traffic. The server's
  e2e suite now resolves through it, at one member and at both, while the
  failure classes stay on stubbed Managers. The IG's ITI-104 artefacts (the
  Source `CapabilityStatement`, the `Patient` profiles and the example
  Patients) are added to the vendored PIXm corpus.
- The outbound identifier-hygiene gate and the security events of the
  analysis guard (#45, §5.4, N33, CP-26). Right before a request leaves for a
  node, the engine re-reads its AQL text (raw and as the printer escapes a
  literal), its paging members, its URL (raw and percent-decoded) and the
  headers the gateway adds, against the identifiers resolution consumed, and
  refuses to send a request that still carries one: nothing reaches the node,
  the query fails closed, and the refusal names the part of the request and
  never the value. The gate reads past the `ehr_id` literal the rewrite
  scoped the query to, so a short identifier that occurs inside the node's
  own `ehr_id` is answered. Every patient predicate the rewrite strips, every refused
  query and every gate stop is a security event under `ferrofed::security`,
  located by byte range and carrying no identifier. A clinician or facility
  predicate (composer, care facility, performer, committer) is dispatched
  unchanged; the same path compared with the patient identifier is refused.
  `openehr-federation` 0.0.5 adds `Refusal::kind`, `Refusal::at` and
  `PatientQuery::stripped` for those events.
- The resolution step through a PIX Manager (#43). A `[pixm]` table selects
  the PIXm resolver: each `[[pixm.manager]]` names a PIX Manager's FHIR base,
  its credentials, and the `ehr_id` domain of every member it resolves, and
  `[pixm.namespaces]` maps a client's issuing namespace to a PIX assigning
  authority. One ITI-83 call per Manager resolves the patient at its members;
  only a member that knows the patient is asked its node query, a member that
  does not is `not-resolved` in `meta.federation`, a patient known nowhere is
  a `200` with no rows and `complete: false`, and a Manager that cannot answer
  fails the query `424` with no node asked. The patient identifier and its
  namespace reach the PIX Manager only. Boot is refused when a member has no
  Manager, when a member has two, or when `[dev]` and `[pixm]` are both set.
  The `{node, ehr_id}` set of each resolution is held in memory as the client
  session's resolution bindings, bounded by `federation.binding_ttl_ms`
  (default 15 minutes), and nothing derived from a patient identifier is
  stored or logged.
- The PIXm ITI-83 client (#42): `ihe-iti` 0.0.3, feature `pixm`, asks a PIX
  Manager's `Patient/$ihe-pix` for the identifiers other domains hold for a
  patient, held to the vendored PIXm 3.1.0 `OperationDefinition` and read into
  a cross-reference, the profile's not-found answer, or a typed error (an
  unknown source or target domain, a rejection with its issue types, a
  timeout, a transport failure, a malformed answer). A `404` without a
  `not-found` issue never reads as "patient unknown". Identifier values are
  redacted in `Debug` and carried by no error. The IHE PIXm 3.1.0 package
  artefacts of ITI-83 are vendored under `docs/specs/ihe-pixm/` by
  `scripts/vendor/ihe-pixm.sh`, pinned by version and tarball sha256, and the
  FHIR R4 model comes from `fhir-types` 0.1.107.
- The fuzz lane (#134): four `cargo fuzz` targets over the untrusted inputs
  (the AQL rewrite over arbitrary text and parameters, the ITS-REST
  `AdhocQueryExecute` body through the façade's intake, a federated
  `RESULT_SET` with its `meta.federation`, and the `OPTIONS {base}/` body),
  with seeds generated from the vendored golden cases and the specification's
  JSON examples by `scripts/fuzz/seeds.sh`. The rewrite target asserts the
  identifier-hygiene property of §5.4.1 (N33) on every query it accepts,
  string-function reconstructions included. `fuzz.yml` runs the targets
  weekly, on dispatch and on pull requests touching the code they read.
- Both patient-identifier carriers resolve (#44; §5.4.3, N33, CP-38): an
  `ENTRY`-level `subject` predicate, `…/subject/identifiers/id`, is resolution
  input on equal terms with `EHR_STATUS.subject.external_ref`, with the
  `DV_IDENTIFIER` `issuer` or `type` as its issuing namespace. It is consumed
  and stripped exactly as `external_ref` is, so the same patient query through
  either carrier sends the same node query and returns the same rows. The same
  value in both carriers is consumed once; a second, different value, and
  qualifiers that name two namespaces, are refused with a `400`, as are the
  `assigner`, a predicate on the identifier list, an `ENTRY`-level
  `external_ref`, and the carrier selected, ordered on or inside a function.
  `openehr-federation` is 0.0.4, and the interim `Refusal::EntrySubject` is
  gone.
- The release lane at SLSA Build Level 3 (#31): the reusable
  `release-build.yml` builds `ferrofed` per target with `cargo auditable`,
  writes a CycloneDX and a syft SBOM, and attests the tarball's provenance and
  both SBOMs; the reusable `release-image.yml` builds the `linux/amd64` and
  `linux/arm64` image from the attested musl binaries, pushes it to
  `ghcr.io/ferrohealth/ferrofed` by digest, and attests the index and each
  platform manifest. A release is published only when the draft carries all
  eight assets of every target, and a pre-release is never marked latest.
- The first federated query (#38): `POST {base}/v1/query/aql` answers one
  ITS-REST `RESULT_SET` over every member of the registry, with no federation
  syntax needed (N1, CP-1).
  - The gateway types the `query_parameters`, analyses the query with
    `openehr-federation`'s `aql` rewrite, resolves the patient at every member
    through the configured cross-reference, and sends each member that knows
    the patient standard AQL keyed on its own `ehr_id` (N2, N7, CP-2, CP-4).
  - It fans out under the budget and re-injects the selected subject column
    as the resolution input (N5, CP-7). `meta.federation` names every endpoint
    (N16): a member that does not know the patient is `not-resolved`, a
    suspended endpoint and a second endpoint of one member are `excluded`.
  - A refused query is a `400` ITS-REST error that locates the fault and
    quotes nothing (§5.4.3). Without a cross-reference a patient query fails
    closed with `424` (decision A17), and without a registry the route stays
    `501`.
  - The configuration gains `profile`, `[registry] document`, `[federation]`
    (`per_node_timeout_ms`, `overall_timeout_ms` below the request timeout,
    `default_namespace`) and the `[[dev.crossref]]` rows of the development
    profile. `config check` loads the registry too.
  - The compose quickstart mounts `docker/quickstart/registry.toml` and
    `ferrofed.toml`, and the README shows one federated query returning the
    EHR of both nodes. The end-to-end test runs it against FerroEHR and
    EHRbase behind their capturing proxies, and no node request carries the
    patient identifier in any carrier (N33).
- The fan-out engine, first increment (#37): `ferrofed-engine`'s `fanout`
  module sends one request per in-scope node at once, each under a per-node
  deadline cut to the overall budget, with no retry and no hedging (§11.5,
  N38). A node still outstanding when the overall budget runs out is abandoned
  and reported `time-out`, without touching any other request, and its late
  answer contributes nothing. `meta.federation` is built from every outcome
  before the decision, so a failing answer carries it, with the effective
  `timeout` budget and `complete: false`. Under the all-or-nothing default an
  `offline` or `time-out` node fails the query `504`, a `node-error` fails it
  `424`, `504` taking precedence; `not-resolved` and `consent-denied` fail
  nothing, so a patient found nowhere is a `200` with empty rows (§11.3, §11.4,
  N6, N37). Every endpoint record carries its node, `system_id`, managing
  organisation, base URL and `latency_ms` (§9.5, N40), and the rows of the
  `active` nodes are concatenated in endpoint id order until the merge (#52).
- The crates.io lane behind the workspace `publish` switch (#32):
  `publish-crates.yml` runs on every `v*` tag, reads the publishable set from
  `cargo metadata`, packages it and publishes it in dependency order through
  crates.io Trusted Publishing, and is a successful no-op while the root
  `[workspace.package] publish` is `false`. The `publish-dry-run` job packages
  every library crate on every pull request, and
  `scripts/release/publish-crates.sh` is the shared implementation.
- Node dispatch (#34): `ferrofed-engine`'s `dispatch` module builds one
  `openehr-its` client per registry endpoint, rooted at the endpoint's base URL
  as the registry holds it plus the ITS-REST `v1` segment (N28), and sends each
  node query as the generated `POST {base}/v1/query/aql`, once, with the
  per-node deadline and the gateway's request id. The answer maps to exactly
  one §11.1 status: a result set to `active`, a refused connection or broken
  stream to `offline` with its reason, a passed deadline to `time-out`, and a
  documented error, an undocumented status or a body that is not a result set
  to `node-error` carrying the node's own status and message (§11.2, N16,
  N40). A credential the provider cannot produce, or a request the client
  runtime refuses to compose, is a typed dispatch error, never an endpoint
  status. The engine names no HTTP engine directly, and a test holds it.
- The AQL rewrite, first increment (#35): the `aql` feature of
  `openehr-federation` 0.0.3 parses a façade query with `openehr-query`, binds
  its `query_parameters` before analysis, and consumes the
  `EHR_STATUS.subject.external_ref` patient predicate with its namespace into a
  redacted `Subject` (§5.2). Each node receives the query scoped to its own
  `ehr_id` (§7.1, N7, N29); a selected subject or namespace column is
  re-injected after the merge (N5); and `columns[]` is rendered from the façade
  query alone (N17). The rewrite refuses with a typed `400`, never quoting the
  identifier: a predicate outside the top-level `AND` chain or under another
  operator (§7.1), an integer identifier (decision A6), two different patients
  (A7), an unqualified identifier with no default namespace (A5), a query with
  no patient where a localizer decides the node set (A8), paging members that
  disagree with the query (A10), cross-node `OFFSET` (§11.6.2), an undirected
  aggregate (N14), and the identifier anywhere else in the query (§5.4.1),
  including rebuilt by `CONCAT`, `CONCAT_WS` or `SUBSTRING` over split
  literals, which the rewrite folds before the value test; a string function
  over a literal that cannot be folded is refused on an identifier-bearing path
  (decision A4). The reference implementation's 17 golden cases run as a
  corpus, each adjudicated. CI's test lane runs every feature.
- The static registry (#36): `ferrofed-registry` 0.0.1 loads the federation's
  membership from a reviewed TOML bootstrap document (`[[organisation]]`,
  `[[node]]` with its `system_id` and `[[node.identifier]]`, `[[endpoint]]`)
  with `deny_unknown_fields` throughout into an immutable `RegistrySnapshot`.
  A document with a dangling reference, a duplicate id, a `system_id` shared by
  two nodes (compared without ASCII case), an endpoint without exactly one
  managing organisation, a connection type other than `openehr-rest-query`, an
  unusable base URL or a node without an endpoint refuses to load (N19, N20,
  N21, §12b.2). `NodeId`, `EndpointId` and `SystemId` are distinct types with
  no conversion between them (N32, §12a.1).
- The resolver seam and the development cross-reference (#36):
  `ferrofed-identity` 0.0.1 carries `PatientRef`, the patient identifier
  redacted in every rendering and never serialized (§5.4, N33), the `Resolver`
  trait (N3, §5.2), and `StaticResolver`, a fixed table from a synthetic
  identifier to each member's `ehr_id` that a configuration can enable only
  under `profile = "development"`. It is FerroFED's own testing device, not an
  identity binding.
- The container image: `docker/Dockerfile` puts the `ferrofed` musl binary on
  distroless static, digest-pinned, as the numeric non-root user `65532`, with
  no shell and a read-only root; `scripts/release/stage-dist.sh` stages the
  binaries of a published release after checking their checksums. `compose.yaml`
  starts the gateway beside two member CDRs, FerroEHR 4.3.1 and EHRbase 2.36.0,
  every image pinned by digest in `docs/VERSIONS.md` and held there by the
  versions guard (#30).
- The test harness in `tools/ferrofed-testkit` (#39): FerroEHR 4.3.1 and
  EHRbase 2.36.0 as two nodes on their own documented databases, pinned by
  digest and started only behind the `FERROFED_E2E` gate; a capturing and
  fault proxy in front of each node, whose journal records every request
  (method, path, query, headers, body) and which refuses, delays or answers
  with a chosen status per node; and a synthetic seed builder that writes
  EHRs, the template and compositions over ITS-REST alone, with patient
  identifiers only inside the `urn:oid:2.999` example arc. CI runs the
  container suite in its own `e2e (containers)` job, and the versions guard
  holds the image pins to `docs/VERSIONS.md`.

### Fixed

- `ihe-iti` 0.0.4 names PMIR's transactions as ITI-93 and ITI-94 in its
  description, feature list and README. ITI-104, which it attributed to PMIR,
  is the PIXm Patient Identity Feed FHIR (#47).

### Changed

- The repository moved to the FerroHEALTH organization,
  <https://github.com/FerroHEALTH/FerroFED>, with the roadmap board as
  <https://github.com/orgs/FerroHEALTH/projects/1>; every link, the image name
  `ghcr.io/ferrohealth/ferrofed` and the crates' `repository` follow it (#123).
- The crate layout names every crate that may be published after the
  specification it implements, one crate per specification with a feature per
  layer or profile (#106): `openehr-federation` 0.0.1 (the Federation Tier
  wire types, formerly `ferrofed-wire`, with the `aql` and `merge` features),
  `ihe-iti` 0.0.1 (features `pixm`, `pdqm`, `mcsd`, `pmir`, `xcpd`) and
  `nl-generic-functions` 0.0.1 (features `nvi`, `mitz`, `lrza`, `nuts-auth`).
  The names are held on crates.io by 0.0.0 placeholders. `ferrofed-registry`,
  `ferrofed-identity` and `ferrofed-engine` move under `app/` and are never
  published. A new CI job lints every feature of the published crates on its
  own, and the architecture test also fails when a binding crate depends on
  FerroFED.
- The `openehr-*` family moves to 0.0.74, the lockstep release with the AST
  visitor, spans, parameter binding and the federation directive in
  `openehr-query`, and the router builder, operation matcher, credentials
  provider and per-call options in `openehr-its` (FerroEHR #3505 to #3514).

### Security

- A configuration that does not parse no longer quotes its source line
  (#133). The TOML reader's error printed the offending line, so a malformed
  `[dev]` row could put its patient identifier, and a mistyped inline secret
  its value, into the `config check` output and the boot failure. The refusal
  now names the file, the line and column, the key path (`dev` alone for any
  line of the `[dev]` table) and the kind of fault, and none of the text.

## [0.0.1] - 2026-10-01

The first release: the repository setup, the documentation site on
<https://ferrofed.eu/>, the architecture of record, the Cargo workspace and the
server binary's shape (configuration, telemetry, health, readiness and
shutdown). The `ferrofed` binary serves health and readiness only; the
federated query follows from v0.0.2.

### Added

- The Cargo workspace (#28): the root manifest with the family lint set, the
  release profile and the `publish = false` switch every library crate
  inherits; the `openehr-*` family declared as one lockstep pin group at 0.0.72,
  with the versions guard failing when one member moves alone; `deny.toml`; the
  `serde_json::Value` ban in `clippy.toml`; and every crate of the architecture's
  crate map as a documented placeholder, with the `ferrofed` binary over a thin
  library run path and the testkit's pin-matrix reader asserting each crate's
  specification constant against `docs/VERSIONS.md`.
- The server binary shape (#29): `ferrofed serve` and `ferrofed config check`;
  configuration from a TOML file and `FERROFED__` environment overrides with
  unknown keys refused, `_file` siblings for every secret, outbound
  credentials per endpoint id, and a typed refusal (exit 78) on any bad value,
  a broken log filter included; `auto`, `json` and `pretty` console formats;
  `GET /`, `GET /health` and `GET /health/readiness` over an indicator
  registry; the request id, the panic catch (a `500` that carries neither the
  panic message nor anything the client sent), the request timeout and the
  body ceiling; a bounded drain on `SIGTERM`; and `501` for every path of the
  ITS-REST surface until the façade lands. The request log records the
  method, the matched route, the status, the latency and the request id, and
  never a body, the AQL text, a header value, an unmatched path, or a query
  value other than the digit-only `offset` and `fetch`, so a façade query's
  patient identifier reaches no log line at any level (§5.4.3).
- The federation wire types in `ferrofed-wire` (#33): `meta.federation` with
  `complete` derived from the endpoint statuses (§11.4), the per-endpoint
  record whose shape carries the N40 `error` and `latency_ms` obligations of
  each §11.1 status, the `OPTIONS {base}/` self-description with its two
  schema conditionals (§7a.2), the federation header names, and the one seam
  into the ITS-REST `ResultSetMetadata`, which refuses the flat and
  `_`-prefixed members §9.1 forbids (CP-35). Unknown members of every open
  object round-trip. The tests validate every emitted body against the
  vendored schemas, the specification's §9.4 and §7a.2 examples included,
  fail on drift between the schemas and the types, and pin the rules no
  schema states.

- The conformance matrix (#41): `conformance/matrix.tsv` with every
  conformance point of section 17, `conformance/tracks.tsv` with the section
  16.3 tracks (track 8 deferred as provisional) and
  `conformance/requirements.tsv` with the reachability of all 46 requirements,
  derived from the vendored specification by `scripts/conformance/matrix.sh`;
  the `// conformance:` test marker; the tier-1 `conformance-matrix` guard; and
  the matrix rendered into the book's Evaluate part.
- The clinical-path storage rule (#40): an engine test reads the crate graph
  and fails when a library crate reaches a storage implementation or the
  application crate.

### Fixed

- `scripts/checks/crate-version-guard.sh` no longer exits early on the change
  that adds the root `Cargo.toml`, where the base has none.

## [0.0.1-rc.1] - 2026-10-01

A pre-release that rehearses the release lane (#14): the repository setup,
the documentation site and the architecture of record, with no binaries.

### Added

- The architecture of record, `docs/architecture.md`, from the first research
  pass (#16, evidence on #18 to #27): the published `openehr-*` crates as the
  openEHR surface at the planned 0.0.74 pin, the request pipeline, the AQL
  rewrite and identifier hygiene on `openehr-query`'s AST, the façade split
  over the ITS-REST route tables, the identity seams with each binding (PIXm,
  mCSD, PMIR, XCPD, the Dutch Generic Functions) in its own crate, the
  security handoff, the registry and its storage, the fan-out, completeness
  and cross-node merge with the `LIMIT` agreement check, the hand-written wire
  types, the crate map with the `publish` switch, the conformance instrument,
  the test topology, the milestone map, and the decision register, every
  entry decided by the owner on 2026-10-01.
- The documentation site on ferrofed.eu: a landing page at `/` and an mdBook
  under `/docs/` organised by reader intent (Evaluate, Operate, Integrate,
  Contribute), built on every pull request and deployed from `main` by
  `docs.yml`, with the pinned docs toolchain, the vendored mermaid assets, the
  favicon set and its `favicon-sync` guard, and the README badge block (#12).
- The release lane, `.github/workflows/release.yml`: a signed `v*` tag is
  checked against `CITATION.cff` and the product row of `docs/VERSIONS.md`,
  the version's `CHANGELOG.md` section becomes the release notes, and the
  release is created as a draft and published once the asset set it promises
  (none before there is code) is verified. `docs/release.md` is the cut (#14).
- The Business Source License 1.1 with Vernum Projecten B.V. as the Licensor
  and copyright holder (#1), and the contribution-licence terms, the
  pull-request checkbox and the `contribution-licence-guard` check (#3).
- The working discipline under `.claude/`: the rules, the hooks (no AI
  attribution, dangerous-command guard, format and comment-style on edit,
  the SessionStart tracker summary), the issue-loop skills, the
  `spec-researcher` and `implementer` agents, and the tracked project memory.
  `CLAUDE.md` states the product, the design-phase status and the hard rules
  (#6), with the identifier-hygiene rule for everything the gateway composes
  for a node.
- The vendored specification corpora under `docs/specs/`, each with a
  `PROVENANCE.md` and a fetch script under `scripts/vendor/`: the Federation
  Tier with AQL specification at v0.9.0 (release candidate, CC0 1.0), its
  reference implementation (Apache-2.0) as evidence, the openEHR ITS-REST
  1.1.0 OpenAPI documents and the openEHR AQL 1.1.0 source with its grammar
  (#7).
- One pin matrix, `docs/VERSIONS.md`, and `scripts/checks/versions.sh`, the
  guard that fails on drift between the matrix and every file repeating a pin
  (#8).
- The GitHub setup: issue and pull-request templates, CODEOWNERS, Dependabot,
  the label set and tracker helpers under `scripts/gh/`, and the CI workflows
  (the tier-1 guards with the Rust tier gated until a workspace exists, one
  `conclusion` check, CodeQL, Scorecard and SonarQube Cloud), and
  `docs/ci-cd.md`, the design of the workflows and the repository settings
  (#9, #10, #11).
- The community and governance set: `CODE_OF_CONDUCT.md`, `GOVERNANCE.md`,
  `SUPPORT.md`, `AI_STATEMENT.md`, `CITATION.cff`, `llms.txt`, and the root
  toolchain, format and lint configuration (#15).

[Unreleased]: https://github.com/FerroHEALTH/FerroFED/compare/v0.0.7...HEAD
[0.0.7]: https://github.com/FerroHEALTH/FerroFED/compare/v0.0.6...v0.0.7
[0.0.6]: https://github.com/FerroHEALTH/FerroFED/compare/v0.0.3...v0.0.6
[0.0.3]: https://github.com/FerroHEALTH/FerroFED/compare/v0.0.1...v0.0.3
[0.0.2-rc.1]: https://github.com/FerroHEALTH/FerroFED/compare/v0.0.1...v0.0.2-rc.1
[0.0.1]: https://github.com/FerroHEALTH/FerroFED/compare/v0.0.1-rc.1...v0.0.1
[0.0.1-rc.1]: https://github.com/FerroHEALTH/FerroFED/releases/tag/v0.0.1-rc.1
