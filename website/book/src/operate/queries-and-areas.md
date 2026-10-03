<!-- SPDX-FileCopyrightText: Vernum Projecten B.V. -->
<!-- SPDX-License-Identifier: BUSL-1.1 -->

# Queries and API areas

The settings a federated query runs under (completeness, timeouts, paging and
aggregates), the stored-query registry, the DEMOGRAPHIC area and the template
upload fan-out.

## Completeness

A federated query is all-or-nothing by default (N37). If a node that was
asked does not answer, the query fails: `504` when the node timed out or was
unreachable, and `424` when it answered with an error. When both happen, the
answer is `504`. A failing answer returns no rows, and its `meta.federation`
names every node with its status and `complete: false`. A member that does not
know the patient (`not-resolved`) clears `complete` and never fails the
query, and so does a `consent-denied` member, whether a consent pre-filter
dropped it or its node refused with a code the registry lists
([Consent](identity.md#consent)). A member the cross-reference could not answer for fails
the query `424` ([Identity resolution](identity.md)). A member that was never
in scope (`excluded`, `not-localized`) leaves `complete` alone. When
every endpoint is `excluded`, for example because every one is suspended, no
member is in scope and the request cannot be resolved to any destination: the
gateway answers `404` and asks no node (§11.2, §11.3).

A client can opt into best-effort for one request by sending
`openEHR-federation-completeness: partial`. The gateway then answers `200` with
the rows of the nodes that did answer, still names every other node with its
status, and sets `complete: false`. Sending `all` asks for the default
explicitly. Any other value, or the header given twice, is refused with a
`400`. Best-effort is offered by default, and you can withdraw it:

```toml
[federation]
best_effort = false   # a request asking for partial is then refused with a 400
```

The gateway never quietly serves an all-or-nothing answer to a request that
asked for `partial`. The setting is named in the startup log line.

## Timeouts

A federated query runs under two budgets (§11.5, N38): each node's request may
take `per_node_timeout_ms`, and the whole fan-out, resolution included, may
take `overall_timeout_ms`. A node past either is abandoned and reported
`time-out`, so under all-or-nothing the query fails `504` with
`meta.federation` naming every node.

```toml
[server]
request_timeout_ms = 30000    # must exceed overall_timeout_ms by more than 1000

[federation]
per_node_timeout_ms = 10000   # one node's request
overall_timeout_ms = 25000    # the whole fan-out
```

A client can shorten the budget for one request with `Prefer: wait=<seconds>`
(RFC 7240 §4.3). A shorter wait replaces the overall budget, and the answer
names it in `Preference-Applied`; a longer one leaves the configured budget
in force. Only the first `wait` counts, a malformed one is ignored and never
refused, and `wait=0` asks no node and reports each one `time-out`. The
budget in force is the one `meta.federation.timeout` reports.

The server's own `request_timeout_ms` answers `408` with an empty body, which
would drop that envelope. A gateway that federates therefore refuses to boot,
and `config check` refuses the file, unless `server.request_timeout_ms` is
greater than `federation.overall_timeout_ms` plus one second, the time the
gateway keeps for combining the answers. The refusal names both keys. The
defaults leave four seconds to spare. The one second is FerroFED's own choice:
§11.5 promises an answer "within its declared overall budget, plus combining
time" and does not size the combining time.

## Paging with `OFFSET`

A node's rows `k` to `k + n` are not the federation's rows `k` to `k + n`, so
the gateway never sends `OFFSET` to a node (§11.6.2, N39). By default it
computes the page exactly: for `ORDER BY … LIMIT n OFFSET k` it asks each node
for its first `k + n` rows with no `OFFSET`, merges them in the federation
order, and returns rows `k` to `k + n`. A node that returns its `k + n` rows
out of that order is reported `node-error`, as for `LIMIT n`. The ITS-REST
`offset` and `fetch` members page the same way.

The page is computed only where `k + n` is bounded. The gateway answers `400`
for a page whose `k + n` is past `max_offset_window` (the message names the
bound), for an `OFFSET` with no `LIMIT`, and for an `OFFSET` with no
`ORDER BY`, which has no order to page through. You can lower or raise the
bound, or refuse every `OFFSET` past zero:

```toml
[federation]
offset_strategy = "bounded"   # the default; "reject" answers every OFFSET > 0 with a 400
max_offset_window = 1000      # rows asked of one node for a page, k + n; 0 is refused
```

The strategy and the bound are named in the startup log line.

## Aggregates across nodes

Each node answers an aggregate with its own value, so one row per node is
never the federation's answer (§11.6.3, N14). By default the gateway
recombines the aggregate exactly: it sends the aggregate to every node, scoped
to that node's `ehr_id`, and answers one row in your query's columns.

- `COUNT(*)` and `COUNT(path)` are the sum of the node counts.
- `SUM` is the sum of the node sums, or `NULL` when no node holds a value.
- `MIN` and `MAX` are the least or greatest node value, over numbers and
  complete date-times.
- `AVG` is asked of each node as the `SUM` and the `COUNT` of its path, and
  answered as their quotient, or `NULL` when no node counts a value. The type
  of the input decides the type of the answer (AQL 1.1.0 §3.9.1.5), and it
  reaches the gateway as the type of the node sums. Over integers, `AVG` is
  an integer: the one nearest the exact quotient of the federation's sum and
  count, with a tie going to the even integer, so `5 / 2` is `2` and `7 / 2`
  is `4`. The gateway rounds once, after it adds every node's sum and count,
  and never rounds a node's own mean. Over reals, `AVG` is the decimal
  quotient written as the nearest JSON number, so `12.5 / 3` is
  `4.166666666666667`. AQL states no rounding rule, so the rounding is
  FerroFED's own.

Integers add exactly, and reals add in decimal arithmetic, so `0.1 + 0.2` is
`0.3`. A recombination over some of the nodes would be a wrong value, so:

- a node that answers a value the recombination cannot use exactly (a string
  for `MIN`, a real for `COUNT`, more or less than one row) is reported
  `node-error`, and the query fails `424`;
- a node that does not answer fails the query `504`, as for any query;
- a request with `openEHR-federation-completeness: partial` is refused `400`
  with the code `partial-aggregate`.

`COUNT(DISTINCT …)`, `SELECT DISTINCT`, and a column that is not an
aggregate beside the aggregates are refused `400`
(`indecomposable-aggregate`), and the message suggests the two alternatives:
direct the query to one node, or select the rows and aggregate them in your
application. A query directed to one endpoint is sent to it unchanged.

You can narrow the functions the gateway recombines, or turn recombination
off, which refuses every undirected aggregate with `undirected-aggregate`:

```toml
[federation]
decomposable_aggregates = ["COUNT", "SUM", "MIN", "MAX", "AVG"]   # the default; [] declares none
```

The list is named in the startup log line.

## Stored queries

The gateway can hold stored queries itself, the federated stored-query
registry of §12.7 (N44). Setting `[stored_queries]` offers the registry, over
one of three backends that `backend` names:

| `backend` | Where the definitions live | For |
|---|---|---|
| `redb`, the default | the embedded store file `path` names | one gateway process |
| `postgres` | one PostgreSQL database, at the connection string `url` or `url_file` holds | several replicas behind one address |
| `files` | read-only, one file per definition under the directory `path` names | instances that share definitions with no database |

What holds for every backend:

- A stored version must outlive the process, because clients invoke it by
  name after a restart, and a second `PUT` refused before a restart is
  refused after it too (§12.7, N44).
- The store holds each definition's name, version, the instant it was stored
  and its AQL, and nothing else. A definition names its patient through a
  `$parameter`, never a literal, and the values a client binds when it
  invokes a query are never written, so no patient identifier reaches the
  store (§5.4.1, N33).
- The registry needs the federation that runs its queries, so
  `[stored_queries]` without `registry.document` refuses the configuration,
  naming `registry.document`. A key the chosen backend does not read, such
  as `url` beside `backend = "redb"`, refuses it too, naming the key.
- `OPTIONS {base}/` declares `definition.stored_query_registry: true` while
  the registry is offered, whatever the backend, and `false` without it;
  without it a stored-query definition request goes to the one node the
  targeting headers name, both `PUT`s included, and `GET` and
  `POST /v1/query/{name}` answer `501`.
- The startup banner and the startup log line name the backend, and never
  its path or connection string. Changing `[stored_queries]` needs a
  restart.

### One gateway: `redb`

```toml
[stored_queries]
path = "/var/lib/ferrofed/stored-queries.redb"
```

- The file is an embedded `redb` store, created at boot when it does not
  exist. Put it on a persistent volume.
- One gateway process opens the file at a time. A second process pointed at
  the same file refuses to start, so run one gateway per file. Several
  replicas need the `postgres` backend.

### Several replicas: `postgres`

```toml
[stored_queries]
backend = "postgres"
url_file = "/run/secrets/ferrofed-stored-queries-url"
```

- The backend is built into the binary only with the `postgres` feature of
  `ferrofed-server` (`cargo build --release --features postgres`), so a
  single gateway carries no PostgreSQL client. A binary built without it
  refuses `backend = "postgres"` at `config check` and at start.
- The connection string is a secret, a URL
  (`postgres://ferrofed:…@db.example.org:5432/ferrofed?sslmode=require`) or
  libpq key/value pairs. Give it as `url_file`, a file read at boot and
  trimmed, or as `url`, never both. A string that does not parse is refused
  naming the key, never quoting the string. The backend is tested against
  PostgreSQL 18.6.
- TLS uses rustls with the platform's trusted roots, as the gateway's calls
  to the members do, and always verifies the server's certificate.
  `sslmode=require` refuses a server without TLS; the default, `prefer`,
  falls back to a connection without it. Outside the development profile, a
  connection string that carries a password to a networked host must set
  `sslmode=require`, or `config check`, the start and a reload refuse it
  naming `stored_queries.url`; under the development profile it starts and
  is named in the banner and the log
  ([What must travel encrypted](configuration.md#what-must-travel-encrypted)).
- At start, and each time it reconnects, the gateway creates the schema
  `ferrofed` and the table `ferrofed.stored_query_definition` when they are
  absent, one replica at a time. The role needs `CREATE` on the database to
  make the schema, or a `ferrofed` schema made for it on which it holds
  `CREATE` and `USAGE`. A database that cannot be reached within ten seconds
  refuses the start.
- Every replica shares the table, whose primary key is the name and the
  version. Two replicas that store the same new version at once store exactly
  one: one `PUT` answers `200` and the other `409` (`stored-query-held`), and
  both replicas then hold the winner's text. A read or an invocation reads the
  table again before it answers, so a version one replica stored is served by
  every other. A database that fails a request answers it `500` and names
  nothing in the body; the log names the failure.
- `config check` checks that the connection string parses, and does not
  connect.

### Read-only: `files`

```toml
[stored_queries]
backend = "files"
path = "/etc/ferrofed/stored-queries"
```

- Each definition is one file, `{path}/{qualified_query_name}/{version}.aql`,
  mirroring its ITS-REST path, holding the AQL as UTF-8 text:
  `/etc/ferrofed/stored-queries/org.example::patient_compositions/1.0.0.aql`.
  A name and a version each have one spelling, so the layout holds one file
  per name and version.
- The gateway loads every file at start and admits each as a `PUT` would: a
  definition that is not a federated query, or that names its patient by a
  literal, is refused. It holds the canonical print, and reports the file's
  modification time as the instant it was stored.
- Anything else in the directory refuses the start: a file at the top level,
  a directory that is no qualified query name, a file that is not
  `major.minor.patch.aql`, a nested directory, text that is not UTF-8, or an
  unreadable entry. The refusal names the file and never quotes its content.
  `config check` loads the directory the same way.
- A `PUT` answers `405` (`stored-query-read-only`) with `Allow: GET,
  OPTIONS`, and `OPTIONS` on a definition path lists no `PUT`. To add or
  change a definition, add its file and restart. Distributing definitions
  needs a `PUT`, so `federation.fan_out_stored_queries` beside this backend
  refuses the configuration.

The [client contract](../integrate/stored-queries.md) says how
a client stores and invokes a query.

### Distributing stored queries to the members

The registry runs its own copy of every definition, so no member needs one.
A deployment that wants each definition at the members too, for a node that
runs it by name locally or for an audit at the point of execution, can have
the registry distribute it (§12.7):

```toml
[federation]
fan_out_stored_queries = true   # off by default

[stored_queries]
path = "/var/lib/ferrofed/stored-queries.redb"
```

- Distribution is a facility of the registry. Setting
  `fan_out_stored_queries` without `[stored_queries]`, or beside the
  read-only `files` backend, refuses the configuration, at `config check`
  and at start.
- Only a stored-query `PUT` that asks for it is distributed, with
  `openEHR-federation-endpoint: *` (every active member) or headers that
  name members. The registry stores the definition first; each named member
  is then sent the registry's copy independently, within the request's
  budget, and nothing is rolled back. A `PUT` naming no member stores at the
  registry alone. With the setting off, a stored-query `PUT` or version
  `GET` that carries a targeting header is refused `400`
  (`stored-query-fan-out-unsupported`), so a client that asked for
  distribution never mistakes a plain store for it.
- A definition that carries a `FROM ENDPOINT` or `ORGANISATION` directive is
  refused for distribution, because no node can run it. Store it without the
  header, and it runs federated.
- A `GET` of a stored version that names members reports, per member,
  whether its copy matches the registry's. An invocation always runs the
  registry's copy, never a member's.
- `OPTIONS {base}/` declares the setting as `definition.stored_query_fan_out`,
  `true` only while the registry is offered. The setting is named in the
  startup log line, and changing it needs a restart.

The [client
contract](../integrate/stored-queries.md#distributing-a-stored-query) gives
the answers.

## The DEMOGRAPHIC area

The gateway never federates the openEHR DEMOGRAPHIC API (§7a.1, N32): the
patient's identity is resolved through the identity binding, never through a
node's demographic store. By default every request under `/v1/demographic/`
answers `501` and no node is asked.

A deployment that keeps its demographics in one member may declare that
member's endpoint as the one that serves the area:

```toml
[federation]
demographic_endpoint = "hospital-a.demographic"
```

- The request chooses its node, as a definition request does (§7a.1, §12.4,
  §12.6, N23): every DEMOGRAPHIC operation ITS-REST 1.1.0 defines names that
  endpoint in `openEHR-federation-endpoint`, and then goes to it alone,
  through the same single-node path as a definition request:
  the query string and the declared headers are held to what the operation
  declares, the body travels byte-identical, and the node's answer comes back
  as the node sent it, with `openEHR-federation-endpoint` and
  `openEHR-federation-system-id` naming the endpoint (§7a.3, N31). Nothing is
  probed, fanned out or sent to another member.
- The gateway never applies the setting as a default. A request naming no
  endpoint is a `400` (`target-required`), and no node is asked. A header
  naming another endpoint is a `400` (`targeting-conflict`), several
  endpoints an `endpoint-several`, and `*` or an unknown id an
  `endpoint-unknown`.
- The value must be an endpoint id of the registry. Any other value refuses
  the configuration, naming `federation.demographic_endpoint`, and so does
  setting it without `registry.document`. A suspended endpoint answers
  `no-destination`.
- `OPTIONS {base}/` declares `its_rest.demographic` as `unsupported: 501`
  without the setting, and as `routed-single-node` naming the endpoint a
  client names, with it. The setting is named in the startup log line.

## Fan-out template upload

A template lives at the node it was uploaded to, so a deployment whose
clients commit to any member needs the same template at every member
(§12.6). By default the gateway routes every template upload to the one
endpoint the request names, and the deployment keeps templates consistent
another way: distributing them out of band, or through a shared template
repository (§12.6, N43). The gateway can instead fan an upload out:

```toml
[federation]
fan_out_template_upload = true   # off by default
```

- Only an ADL 1.4 or ADL 2 template upload fans out, and only when the
  request asks for it, with `openEHR-federation-endpoint: *` (every active
  member) or headers that select several endpoints. A plain upload still
  names its one node, and every other definition request still routes to
  one node.
- Each member is sent the upload independently, within the request's
  `per_node_timeout_ms` and `overall_timeout_ms`. A member that accepts keeps
  the template: the gateway rolls nothing back.
- The answer names each member's outcome, and a partial success answers
  `207`, never `200` ([client
  contract](../integrate/templates-and-demographics.md#fan-out-template-upload)).
- `OPTIONS {base}/` declares the setting as
  `definition.fan_out_template_upload`, and `its_rest.definition` says that
  an upload naming `*` or several endpoints fans out. The setting is named
  in the startup log line, and changing it needs a restart.

