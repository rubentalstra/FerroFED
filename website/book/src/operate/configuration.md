<!-- SPDX-FileCopyrightText: Vernum Projecten B.V. -->
<!-- SPDX-License-Identifier: BUSL-1.1 -->

# Configuration

The `ferrofed` binary reads one TOML file and the environment. It serves the
process shape (health, readiness, the request log, graceful shutdown) and, once
a registry is configured, the federated query `POST {base}/v1/query/aql`, the
EHR resources and the definition area routed to one node, and, when
`[stored_queries]` is set, the stored-query registry. The DEMOGRAPHIC area
answers `501` unless `federation.demographic_endpoint` declares the one
endpoint a request names to reach it, and every other path under
`{base}/v1/` answers `501`. `{base}` is `/` unless you set
[the base path](#the-base-path).

Three pages carry the rest of the configuration: [the registry](registry.md),
its document and the state learned from it; [identity
resolution](identity.md), the cross-reference that finds each member's
`ehr_id`; and [queries and API areas](queries-and-areas.md), the settings a
federated query and each ITS-REST area run under.

## Running it

```text
ferrofed serve --config /etc/ferrofed/ferrofed.toml
ferrofed config check --config /etc/ferrofed/ferrofed.toml
ferrofed healthcheck --config /etc/ferrofed/ferrofed.toml
```

`--config` names the file; without it the file is the one `FERROFED_CONFIG`
names, and without that every default stands. `config check` reads and
resolves the configuration exactly as `serve` would, secrets included, loads
a read-only stored-query directory, prints one line and exits, so a deployment
pipeline can test a file without binding a socket. It opens no store file and
connects to no database. `ferrofed admission check --endpoint <id>` checks
one member against the admission conditions ([Admitting a node](admission.md)).
`healthcheck` asks the gateway running on this host for its readiness and
exits `0` or `1`, the two codes a container runtime's health check reads
([Health probes](health.md#ferrofed-healthcheck)).

A configuration the gateway refuses exits with code 78 (`EX_CONFIG`) and one
line naming the key at fault. It refuses an unknown key, a value of the wrong
type, a zero timeout or body limit, a log filter that does not parse, a secret
set both inline and through its `_file` sibling, a credentials section
that names no scheme or two, and a credential the `Authorization` header
cannot carry: a bearer token that is not a `b64token` (RFC 6750 §2.1: letters,
digits, `-`, `.`, `_`, `~`, `+` and `/`, then any `=` padding), or a basic
user or password holding a control character such as a newline (RFC 7617
§2), or a basic user holding a colon. That refusal names the key the value came from, and never
the value. The gateway never falls back to a default for a value you set.

### The startup banner

When the log renders for a person, `serve` prints a banner before the first
log line: the wordmark, the version, the releases the gateway serves, and the
deployment facts to check first. Run on a terminal, the development
configuration of [the quickstart](container.md#the-quickstart) prints this:

```text
 _____                   _____ _____ ____
|  ___|__ _ __ _ __ ___ |  ___| ____|  _ \
| |_ / _ \ '__| '__/ _ \| |_  |  _| | | | |
|  _|  __/ |  | | | (_) |  _| | |___| |_| |
|_|  \___|_|  |_|  \___/|_|   |_____|____/

  openEHR federation gateway · v0.0.6
  Maintained by Ruben Talstra · https://github.com/FerroHEALTH/FerroFED

  Federation Tier  0.9.0
  ITS-REST         1.1.0
  AQL              1.1.0
  openehr-*        0.0.81

  Base path        /
  Listen           0.0.0.0:8080
  Registry         4 members, 4 endpoints
  Stored queries   off
  Unencrypted      credentials.node-a-query
                   credentials.node-b-query
                   credentials.node-c-query
                   credentials.node-d-query

  DEVELOPMENT: this deployment runs the development profile, which may resolve
  patients from a static development table. It must not hold or reach real
  patient data.
```

The `Registry` line counts the members and endpoints of the registry
document. The gateway reads the document once, before the banner, and serves
that same read, so the counts are those of the registry it serves. The line
reads `none` when no document is set, and says the document does not load
when it cannot be read, in which case the boot stops on the next lines with
the reason. The development notice prints only under
`profile = "development"`, in red on a terminal with colour and in the same
words without it. The `Unencrypted` lines name, by key, each credential or
patient identifier that travels unencrypted, which only the development
profile allows
([What must travel encrypted](#what-must-travel-encrypted)).

Colour follows the terminal, and an explicit `format = "pretty"` keeps it
into a pipe. A `NO_COLOR` environment variable that is set and not empty
switches colour off in both the banner and the log, whatever the format
(<https://no-color.org>).

The banner prints only when the log renders as `pretty`: with
`telemetry.format = "pretty"`, or with `auto` when stdout is a terminal. With
`json`, or with `auto` and stdout piped to a collector, the first line on
stdout is a JSON log line. `config check`, `healthcheck` and
`admission check` print no banner. The banner shows counts, an address, a
path, switches and configuration keys, and never a credential, a URL, a
header value or anything from a request.

## The file

The sections, and the page that covers each:

| Key or section | What it sets | Page |
|---|---|---|
| `profile` | `production`, the default, or `development`, the only profile that admits `[dev]` | [Identity resolution](identity.md#the-development-cross-reference-dev) |
| `[server]`, `[telemetry]`, `[credentials]`, `[signing]` | the listener, the console, the onward credentials, the signing keys | this page |
| `[metrics]` | the admin listener and the OTLP push | [Metrics](metrics.md) |
| `[registry]` | the registry document and its form, or the mCSD directory of `[registry.mcsd]` the registry is read from | [The registry](registry.md) |
| `[pixm]`, `[dev]` | the cross-reference | [Identity resolution](identity.md) |
| `[xcpd]` | the XCPD localizer | [XCPD localization](identity.md#xcpd-localization-xcpd) |
| `[federation]` | the federation id, node selection, budgets, completeness, paging, aggregates and the optional facilities | [The registry](registry.md), [Queries and API areas](queries-and-areas.md) |
| `[stored_queries]` | the stored-query registry and its backend | [Queries and API areas](queries-and-areas.md#stored-queries) |

```toml
[server]
listen = "127.0.0.1:8080"     # the socket address to bind
base_path = "/"               # the path of the base URL every route sits under; see The base path
request_timeout_ms = 30000    # a request past this answers 408; see Timeouts
shutdown_timeout_ms = 10000   # the drain after SIGTERM is bounded by this
body_limit_bytes = 1048576    # a body past this answers 413

[telemetry]
format = "auto"   # auto, json or pretty; auto is json unless stdout is a terminal
filter = "info,hyper=warn,tower=warn,h2=warn"

# The metrics surface, off by default; see Metrics.
[metrics]
listen = "127.0.0.1:9464"     # the admin listener: GET /metrics and the stored-query distribution; loopback unless allow_remote
otlp_endpoint = "http://127.0.0.1:4317"   # an OTLP gRPC collector the same metrics are pushed to

# Outbound credentials, one section per endpoint id. Each section names one
# scheme: a bearer token, a user and a password, or an OAuth 2.0 grant.
[credentials."hospital-a"]
bearer_token_file = "/run/secrets/hospital-a-token"

[credentials."clinic-b"]
user = "ferrofed"
password_file = "/run/secrets/clinic-b-password"

# OAuth 2.0 client credentials with a signed JWT client assertion (§13.1).
[credentials."cdr-c".oauth2]
grant = "client_credentials"
client_auth = "private_key_jwt"
token_endpoint = "https://auth.cdr-c.example.org/oauth2/token"
client_id = "ferrofed-gateway"
scope = "system/aql-*.s system/composition-*.cru"
resource = "https://cdr-c.example.org/openehr"   # optional, RFC 8707
# audience = "cdr-c"                             # optional

# The gateway's signing keys, which sign every client assertion and are
# published as a JWK Set. Needed by any oauth2 section.
[signing]
key_file = "/run/secrets/ferrofed-signing-key.pem"
# previous_key_file = "/run/secrets/ferrofed-signing-key-previous.pem"
jwks_uri = "https://gateway.example.org/.well-known/jwks.json"
assertion_lifetime_s = 300    # at most 300
node_jwks_cache_s = 3600      # how long the nodes cache the JWK Set
rotation_overlap_s = 3900     # at least assertion_lifetime_s + node_jwks_cache_s
```

Every secret has a `_file` sibling, read at boot and trimmed, so a secret
can come from a mounted file and never sit in the configuration or the
environment. The credentials are read and checked at boot and again on each
[reload](registry.md#reloading-the-registry), and the node client of an endpoint with a
credentials section sends them on every request to that endpoint.

The gateway never prints a secret. A bearer token or a password, inline, from
the environment or from a `_file` sibling, shows as `***` wherever the
configuration is rendered: a debug log line, a panic message, a test failure.
A URL that may carry a credential, such as the stored-query store's
`url` or `metrics.otlp_endpoint`, shows with its userinfo and its query
replaced: `postgres://***@db.example.org:5432/ferrofed?***`. A libpq
key/value connection string shows as `***` whole. The value itself is sent
unchanged.

A PIX Manager's `url` carries no credential: one with a user name or a
password in it is refused naming the key, as an endpoint URL in the registry
document is. Its credentials go in `[pixm.manager.credentials]`, which takes
a bearer token or a user and a password, never an `oauth2` grant.

### What must travel encrypted

Every outbound connection in the configuration is held to one of three
rules, by what it carries.

**Credentials and patient identifiers.** Outside `profile = "development"`,
a URL that a configured credential or a patient identifier is sent to must
be `https`. `serve`, `config check`, `admission check` and every
[reload](registry.md#reloading-the-registry) refuse anything else, with exit
code 78 and one line naming the URL's key and what would travel over it,
never a value. The rule covers:

- the URL of a registry endpoint that has a `[credentials."<id>"]` section,
  which receives its bearer token, its basic credentials or the access token
  its `oauth2` grant obtains;
- the `token_endpoint` of an `oauth2` section, which receives the client
  assertion;
- the `url` of every PIX Manager, which is asked for patient identifiers
  with or without `[pixm.manager.credentials]`;
- the `url` of every XCPD responding gateway, which is sent the patient
  identifier and, when one is configured, the XUA assertion;
- `metrics.otlp_endpoint` when it carries a user name or a password.

```text
ferrofed: cannot start: the url of endpoint hospital-a in registry.document is not an https URL, and credentials.hospital-a would travel over it in cleartext: outside profile = "development" a credential or a patient identifier is sent only encrypted
```

A node URL no credential is sent to may stay `http`, for example a node on a
private network whose transport a sidecar protects with mutual TLS: the
gateway sends a node its own `ehr_id`, never the patient identifier (§5.4,
N33).

**A database password.** Outside the development profile, the stored-query
store's PostgreSQL connection string must set `sslmode=require` when it
carries a password and reaches its host over the network. The driver reads
`sslmode` as `disable`, `prefer` (its default, which falls back to no TLS) or
`require`, and refuses any other value, so `disable` and `prefer` are
refused, naming `stored_queries.url`, by the same commands and on a reload.
A connection with no password, or whose every host is a Unix socket and
that names no `hostaddr`, crosses no network with a secret and may leave
`sslmode` as it is
([Stored queries](queries-and-areas.md#stored-queries)).

Under the development profile the same configuration starts, so the
[quickstart](container.md#the-quickstart) can reach its nodes over `http`
inside its Docker network. Each credential or patient identifier that
travels unencrypted is named by key in the startup banner, in a `WARN` log
line at boot and after each reload, and on stderr by `config check`.

**Trust anchors.** A URL the gateway verifies its callers against, an
issuer's `jwks_uri` or `introspection_endpoint` in
[`[auth]`](authentication.md), must be `https`, or `http` to a loopback
host, under every profile, development included. A key set fetched in the
clear would let anyone on the network substitute keys and forge callers.

**The profile takes a restart.** A reload whose file changes `profile` is
refused (class `profile`), and the running configuration stays. Every
decision the development profile admits, these transport rules, the
development cross-reference and consent table, and an `http` XCPD gateway,
reads the profile the process started with.

The specification assumes a protected transport and leaves it to the
security profiles (§2.2, §13); no specification governs these rules, which
are FerroFED's own design.

### OAuth 2.0 to a node

An `oauth2` section makes the gateway authenticate to that node as itself
(§13.1, N25). Before a request, it asks the node's token endpoint for an
access token with the client-credentials grant (RFC 6749 §4.4). It
authenticates there with a JWT client assertion (RFC 7523 §2.2), signed
ES384 with the `[signing]` key. The assertion names `client_id` as its
`iss` and `sub` and the token endpoint as its `aud`, lives
`assertion_lifetime_s` seconds, and carries a fresh `jti`. The token request
carries `scope` and, when set, `resource` and `audience`. Every key of the
section is required except those two:

- `scope` is space-separated SMART on openEHR scopes, each a resource scope
  of the `system` compartment (`system/aql-*.s`, `system/composition-*.cru`);
  anything else is refused at load.
- `token_endpoint` is an `https` URL with no user name, password, query or
  fragment; `http` is accepted only under `profile = "development"`
  ([What must travel encrypted](#what-must-travel-encrypted)).

The gateway caches a token until 30 seconds before the end of the lifetime
its `expires_in` states, with one token request per endpoint at a time. A
token with no stated lifetime serves one request. A node that answers `401`
drops the cached token, and the next request obtains a new one. When no
token can be obtained, that node is reported `node-error` and is sent
nothing: the gateway never dispatches unauthenticated. The answer says only
`no onward credential could be obtained, so nothing was sent`, followed by
the token endpoint's RFC 6749 §5.2 `error` code when it refused with a
registered one. The full account, the token endpoint's description
included, goes to the log at `warn` with the endpoint and the request id.
The admission check authenticates the same way.

The caller's own `Authorization` header never reaches a node.

### Signing keys and the JWK Set

`[signing]` holds the gateway's ES384 keys: P-384 private keys in PKCS#8
PEM, each read from a file. To make one:

```text
openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-384 -out ferrofed-signing-key.pem
```

The gateway serves its public keys as a JWK Set (RFC 7517) at
`GET {base}/.well-known/jwks.json`, with no client authentication, and
declares `jwks_uri` as `federation.auth.jwks_uri` in `OPTIONS {base}/`
(§13.1, N30). Point `jwks_uri` at that route on the gateway's public
address, or at wherever your deployment publishes the keys. Each key's `kid`
is its RFC 7638 thumbprint, so the same key always has the same `kid`.

To rotate, make a new key, set it as `key_file`, move the old one to
`previous_key_file`, and restart. The new key signs from then on. The JWK
Set publishes both keys for `rotation_overlap_s` seconds from the start of
the process, then the current key alone. The overlap must be at least the
assertion lifetime plus the time the nodes cache the JWK Set, so a node
still holding the old set, or an assertion the old key signed, finds its
key. Once the window has passed, remove `previous_key_file`. A change to
`[signing]` takes effect only on a restart; a reload reports it.

## The environment

Any key can be set or overridden with `FERROFED__<SECTION>__<KEY>`, upper or
lower case, with `__` between the segments:

```text
FERROFED__SERVER__LISTEN=0.0.0.0:8080
FERROFED__CREDENTIALS__HOSPITAL_A__BEARER_TOKEN_FILE=/run/secrets/token
```

A value reads as TOML syntax when it is one (`9`, `true`) and as the string it
is otherwise. An override that names no key, or a key the file does not
define, is refused like any other unknown key.

## The base path

`server.base_path` is the path of the base URL the gateway is served at,
`{base}` in the specification, and every route in the table below sits under
it (§4.1, N28). It is `/` by default, so the gateway serves at the root. Set
it to mount the gateway under a path of your choosing, for example behind a
reverse proxy that does not strip the path:

```toml
[server]
base_path = "/fed/openehr"
```

The gateway then serves `GET /fed/openehr/`, `OPTIONS /fed/openehr/`,
`GET /fed/openehr/health`, `POST /fed/openehr/v1/query/aql` and the rest of
the table, serves `{base}` without the trailing slash as `{base}/`, and
answers `404` for every path outside the base, the root included, so point a
health probe at `{base}/health`; `ferrofed healthcheck` asks under the base
by itself. The specification reserves no prefix, and
the gateway reserves none either: `/rest/openehr` is a valid base when you
choose it, and is not served unless you do. The base is checked at boot: it
starts with `/`, has no trailing `/` unless it is `/`, has no query or
fragment, and has no empty, `.` or `..` segment, or the gateway refuses to
start and names `server.base_path`. Tell clients the full base URL, scheme,
host and this path, through the registry or service discovery; nodes never
see it, because the gateway asks each node at the node's own base URL.

## The HTTP surface

Every route is under the [base path](#the-base-path); with the default `/`,
`{base}/` is `/`.

| Route | Answers |
|---|---|
| `GET {base}/` | the product name and version |
| `OPTIONS {base}/` | the federation's self-description (§7a.2) |
| `GET {base}/health` | `200` while the process is up |
| `GET {base}/health/readiness` | `200` while the gateway serves and its own subsystems (the configuration, the registry, the outbound clients, the stored-query store) are up; `503` before boot completes and from the moment `SIGTERM` or `SIGINT` arrives, with the phase and each subsystem's state; no member node and no identity source gates it |
| `GET {base}/health/dependencies` | always `200`, with the state the gateway last observed of each member endpoint, of the resolver, of the consent pre-filter and of the mCSD directory: `up`, `failing`, `down` or `unknown`; endpoint ids and states only |
| `GET` and `POST {base}/v1/query/aql` | the federated `RESULT_SET`, the `GET` form reading the request from its query string; `501` when no registry is configured |
| `{base}/v1/ehr/{ehr_id}` and below | routed to the one node that owns the `ehr_id`, found in the order of §12.5.1: the `openEHR-federation-endpoint` header, the session's resolution binding, the `ehr_id` index, then for a read the ask-all probe; answered as that node answered; `501` when no registry is configured |
| `GET {base}/v1/ehr?subject_id=…&subject_namespace=…` | the subject resolved at the gateway, and `GET /v1/ehr/{ehr_id}` sent to the one member that holds it, at that member's own base, answered as that node answered; `501` when no registry is configured |
| `{base}/v1/definition/` and below | routed to the one node `openEHR-federation-endpoint` names, never merged; without the header a `400`; a template upload naming `*` or several endpoints fanned out to each when `federation.fan_out_template_upload` is set; stored-query definitions held at the gateway when `[stored_queries]` is set, and distributed to the members a `PUT` names when `federation.fan_out_stored_queries` is set beside it; without `[stored_queries]`, routed like every other definition request; `501` when no registry is configured |
| `{base}/v1/demographic/` and below | `501`, never federated; when `federation.demographic_endpoint` is set, routed to that endpoint when `openEHR-federation-endpoint` names it, and a `400` without the header |
| any other path under `{base}/v1/` | `501` |
| any other path | `404` |

Every response carries an `x-request-id`: the client's value when it is short
printable ASCII, the gateway's own id otherwise.

The metrics are not on this surface: they have a listener of their own, off
by default ([Metrics](metrics.md)).

## Request ids

The gateway mints its own id, a fresh version 4 UUID, for every request. That
id is the `X-Request-Id` of every request the gateway sends to a node for it,
the same id on every node of one fan-out. The client's `x-request-id` never
reaches a node: it is free text, and the gateway cannot tell whether it names
a patient (§5.4.1, N33). When the client sends no id, the response carries the
gateway's id, so the client, the log and every node name the same request.
The outbound gate searches every other part of a node request for the
identifiers resolution consumed, and skips the minted id: it holds no client
input, and a short hexadecimal identifier can occur inside a random UUID.

Every other header the gateway sends to a node for a federated query is fixed
by the gateway: `Accept` and `Content-Type` (`application/json`),
`Authorization` (the endpoint's configured onward credential, when it has
one), and the `Host`, `Content-Length` and `Accept-Encoding` the HTTP client
writes. None is copied from the client request. `Host` is the authority of
the endpoint URL in your registry, never a value from a request, so the
outbound gate does not search it, or the URL's host and port, for a
withheld identifier (§5.4.1, N33). It searches the path, the query string
and the other headers, except the minted id above.

A request routed to one node (`{base}/v1/ehr/{ehr_id}` and below) carries the
same `Authorization`, `X-Request-Id`, `Host`, `Content-Length` and
`Accept-Encoding`, and each request header the matched ITS-REST operation
declares, of `Accept`, `Content-Type`, `If-Match`, `Prefer`,
`openehr-version`, `openehr-audit-details`, `openehr-template-id`,
`openehr-item-tag` and `openehr-version-item-tag`, only those that operation
lists. The list comes from the `openehr-its` parameter table, never from the
gateway's own copy. `Accept`, `Content-Type` and `Prefer` are composed by the
gateway as values the operation lists; every other declared header is the
client's value byte for byte (see [Declared values](#declared-values)). Every
other client header is stripped, the client's `Authorization` and
`x-request-id` and the federation's own headers included, and the outbound
gate reads every forwarded value.

## Declared values

A routed request resolves no patient, so the outbound gate has no identifier
to compare a forwarded value against. The gateway works from what the
operation declares instead, before anything is sent.

It composes three headers itself, so a node receives a value the operation
lists, in the operation's own spelling, and never the client's text:

- `Accept` is read as a list of media ranges with weights (RFC 9110
  §12.5.1). Each listed media type takes the weight of the most specific
  range that covers it, and the node receives the heaviest one, the first
  listed on a tie. `*/*`, or no `Accept` at all, sends the first media type
  the operation lists; ITS-REST declares no default media type, so the first
  listed is the gateway's own choice, `application/json` in every EHR
  operation. A range with a parameter other than `q` or `charset=utf-8`
  covers nothing. An `Accept` that admits no listed type is a `406`
  (`media-type-not-acceptable`), as a node would answer it.
- `Content-Type` is read as a media type (RFC 9110 §8.3). When its type and
  subtype are a listed value, the node receives that value. A
  `charset=utf-8` is accepted and dropped, since the listed value carries no
  parameter and JSON is UTF-8 (RFC 8259 §8.1). Any other parameter, or a
  type that is not listed, is a `415` (`media-type-unsupported`). The listed
  values are the media types the operation's request body is declared in
  (`openehr-its`'s `request_media`), which are the values of its
  `Content-Type` parameter where it declares one; the versioned stored-query
  `PUT` declares no parameter and takes `text/plain`. A body never travels
  without a `Content-Type`: when the client sends none, the node receives
  the first media type the operation lists. ITS-REST makes `Content-Type`
  optional and declares no default, and RFC 9110 §8.3 names none, so the
  first listed is the gateway's own choice, as for `Accept`. Wherever an
  ITS-REST operation lists several, `application/json`, the canonical JSON,
  comes first.
- `Prefer` is read as a list of preferences (RFC 7240 §2). The node receives
  only the preferences the operation lists, in their listed spelling: a
  preference name is compared without regard to case, its value exactly, and
  only its first instance counts. Any other preference is dropped and never
  refused, as RFC 7240 allows a server to ignore it.

It holds every other value to what the operation declares, and a value that
does not match is a `400` (`parameter-value-invalid`) with nothing sent:

- a path identifier is the openEHR identifier class the `openehr-its` table
  states for it: a `version_uid`, and the `uid_based_id` of a delete, an
  `OBJECT_VERSION_ID`; any other text `uid_based_id` an `OBJECT_VERSION_ID`
  or a `HIER_OBJECT_ID`; and a path parameter the table states as a UUID,
  such as `versioned_object_uid` or the `uid_based_id` of a composition
  update, a UUID in its canonical hyphenated form, each parsed by
  `openehr-base` (a composition delete whose path names no
  `OBJECT_VERSION_ID` is `preceding-version-invalid` instead, since the path
  names the version it amends). The path then travels as
  the client sent it, since an openEHR uid is never rewritten (N22). The
  `ehr_id` is parsed by the routing itself;
- a date-time, such as `version_at_time`, is an extended ISO 8601 date-time
  in the openEHR BASE sense, with an offset only when needed (ITS-REST
  Overview, "Datetime format"), read by the `openehr-base` parser;
- an enumerated query value, such as `detail_level`, is exactly one of the
  values the operation lists;
- a UUID, an integer, a number and a boolean are each parsed as one.

The refusal names the header, or the path or query parameter by its position
and declared name, and never the value (§5.4.3). A value of such a kind
carries only what the kind admits: a four-digit year is a valid partial
date-time, and a `HIER_OBJECT_ID` admits a bare number as a one-arc ISO OID.

ITS-REST states the identifier class of a path parameter only in its
description; the `openehr-its` table carries it, and the gateway reads it
from there, never from a copy of its own.

The parameter table states no kind for the rest, so the gateway cannot
classify them and forwards them as the client sent them. In the EHR area they
are the headers `If-Match`, `openehr-audit-details`, `openehr-item-tag`,
`openehr-template-id`, `openehr-version` and `openehr-version-item-tag`, the
path parameter `key` of an item tag, and the query parameters `path`,
`tag_key`, `tag_value` and `tag_target_path`. The creation of an EHR adds
none beyond `openehr-version` and `openehr-audit-details`. The definition
area has the path parameters `qualified_query_name`, `template_id` and
`version`, and the query parameters `concept`, `query_type`, `template_id`
and `version`. The DEMOGRAPHIC area, where it is routed, has the same ones as
the EHR area except `path`.
N33 forbids an identifier in the parts of a request the gateway composes
(§5.4.1). Whether a client value the gateway forwards unchanged is one of
those parts is a question the specification leaves open, recorded on
[#212](https://github.com/FerroHEALTH/FerroFED/issues/212).

## What the log records

One line per request: the method, the matched route, the status, the latency,
the gateway's request id (`request_id`) and whether the client sent its own
(`client_named`). A façade query carries the patient identifier, so the line
never carries a request body, the AQL text, a header value, the request path,
or a query value other than the ITS-REST paging parameters `offset` and
`fetch`, and those only when they are digits. The route is a path template,
never the path the client sent: a request the gateway routes or answers
under `{base}/v1/` is logged under the template of the ITS-REST operation it
addresses, so a routed composition read is logged as
`/v1/ehr/{ehr_id}/composition/{uid_based_id}`, with the configured base path
in front of it once, and its `ehr_id` and version uid stay out of the line.
An `OPTIONS` request is logged under the template of the resource it
describes. A path that names no route, and any other method the ITS-REST
operation does not declare, is logged as `<unmatched>`. The client's own
`x-request-id` is a header value too, and it is never logged, so a request the client named is found in the log by its time, route
and status, and in a node's log by the `request_id` of that line. A request
the gateway answers before its handler finishes has its line too, with the
status it answered: `500` for a handler panic, `408` past the request
timeout, `413` over the body ceiling. A handler panic is also logged under
the gateway's id, without its message, which could quote a value the handler
held. Every panic in `ferrofed serve`, inside a request or not, writes one
more line, "a thread panicked", with the source location and, inside a
request, the gateway's `request_id`. The panic message is never written:
the gateway replaces Rust's default panic hook, which prints it to stderr,
so a panic writes nothing to stderr. A federated query the gateway fails with a `500` also logs "the
federated query failed" with its error code and the same `request_id` as its
request line. A fan-out template upload, a stored-query distribution or
repair, and a drift check whose task for one member panics fail the same
way, with a `500`, and log that the fan-out could not tell what became of
every member.
