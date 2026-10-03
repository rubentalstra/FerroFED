<!-- SPDX-FileCopyrightText: Vernum Projecten B.V. -->
<!-- SPDX-License-Identifier: BUSL-1.1 -->

# Metrics

The gateway counts what it already observes: the integrity incidents it
raises, the requests it sends to each member node and how long they take,
and the registry reloads. One OpenTelemetry meter provider holds the
counts. A Prometheus server scrapes them from `GET /metrics` on an admin
listener of their own, and the gateway can also push them to an
OpenTelemetry collector over OTLP. Both surfaces read the same provider, so
a metric never exists on one and not the other. Both are off by default.

`GET {base}/health/dependencies` stays as it is: it reports the last state
the gateway observed of each member, and the metrics count every request
over time.

## Turning it on

```toml
[metrics]
listen = "127.0.0.1:9464"     # the admin listener; unset, nothing listens
allow_remote = false          # true lets listen name a non-loopback address
otlp_endpoint = "http://127.0.0.1:4317"   # an OTLP gRPC collector; unset, nothing is pushed
```

The admin listener serves `GET /metrics` and the operator's
[stored-query distribution](../integrate/stored-queries.md#repairing-drift)
(`POST /admin/stored-queries/{name}/{version}/distribute`), answers every
other path `404`, and never sits under the base path. It has no
authentication, and it is never the gateway's own listener, so no client of
the federation reaches it. `serve` and `config check` refuse:

- a `listen` address that is not a loopback address, such as `0.0.0.0:9464`,
  unless `allow_remote = true` is set. Set it only when the address is
  reachable from your scraper and nothing else, for example inside a pod
  network a network policy closes;
- a `listen` address equal to `server.listen`;
- an `otlp_endpoint` that is not an `http://` URL. The push speaks gRPC
  without TLS, so run the collector beside the gateway, on the same host or
  in the same pod, and let it forward over TLS;
- an `otlp_endpoint` with a user name or a password in it, outside
  `profile = "development"`, since that credential would travel in cleartext
  ([What must travel encrypted](configuration.md#what-must-travel-encrypted)).

The push sends every 60 seconds; the standard `OTEL_METRIC_EXPORT_INTERVAL`
environment variable, in milliseconds, changes the interval. A push that
fails is logged at `WARN` under the target `opentelemetry-otlp`, and the
counts stay readable on `/metrics`. On `SIGTERM` the gateway pushes once
more after the drain.

`[metrics]` takes effect on a restart only: a [reload](registry.md#reloading-the-registry)
that changes it logs the key as needing a restart, like `[server]`.

## The metrics

The gateway names its instruments the OpenTelemetry way, and the Prometheus
exporter renders each name with `_` for `.`, `_total` after a counter, and
the unit after a histogram.

| Prometheus name | Instrument | Type | Labels | Counts |
|---|---|---|---|---|
| `ferrofed_integrity_incidents_total` | `ferrofed.integrity.incidents` | counter | `kind` | the [integrity incidents](registry.md#integrity-incidents) the gateway emitted, one per incident line under `ferrofed::integrity` |
| `ferrofed_node_requests_total` | `ferrofed.node.requests` | counter | `endpoint`, `outcome` | the requests the gateway sent to a member endpoint |
| `ferrofed_node_request_duration_seconds` | `ferrofed.node.request.duration` (unit `s`) | histogram | `endpoint` | the time a member endpoint took to answer, with the series `_bucket` (and `le`), `_sum` and `_count` |
| `ferrofed_consent_prefilter_requests_total` | `ferrofed.consent.prefilter.requests` | counter | `outcome` | the calls to the [consent pre-filter](identity.md#consent), by `denied`, `no-signal` or `unavailable` |
| `ferrofed_localizer_requests_total` | `ferrofed.localizer.requests` | counter | `outcome` | the calls to the [localizer](registry.md#node-selection), by `candidates`, `no-records`, `not-configured`, `unavailable`, or `audit-failed` for an XCPD exchange whose audit message could not be recorded |
| `ferrofed_registry_reloads_total` | `ferrofed.registry.reloads` | counter | `result` | the registry reloads `SIGHUP` asked for |
| `target_info` | the resource | gauge | `service_name`, `service_version`, `telemetry_sdk_*` | always `1`: the gateway and its version |

Every label value comes from a closed set or from your registry document,
never from a request, so no patient identifier, query text, header value or
path reaches the surface:

| Label | Values |
|---|---|
| `kind` | `EhrIdCollision`, `IndexInsertCollision`, `LearnedCreatingSystemConflict`, `RegisteredCreatingSystemConflict` |
| `endpoint` | an endpoint `id` of the registry document |
| `outcome` | `active`, `node-error`, `time-out`, `offline`, `consent-denied` |
| `result` | `applied`, `refused` |
| `le` | a bucket bound in seconds: `0.005`, `0.01`, `0.025`, `0.05`, `0.1`, `0.25`, `0.5`, `1`, `2.5`, `5`, `10`, `30`, `+Inf` |

The incident and reload counters show every label value at `0` from the
start, so an alert on their increase works from the first scrape. A node
request series appears with the first request to that endpoint, and an
endpoint a reload removes keeps its series until a restart.

### Which calls the node request series cover

Six kinds of call send a request to a member. Each request one of them
sends is counted once in `ferrofed_node_requests_total` and timed once in
`ferrofed_node_request_duration_seconds`, under the same `endpoint`, so the
histogram's `_count` equals the counter summed over `outcome`. A request
that never left the gateway is in neither: a member that was
`not-resolved`, `excluded` or `not-localized`, and a request the gateway
could not send, for want of a client or a credential, or because the
identifier-hygiene gate withheld it. §11.1 has no status for a request the
gateway could not send, so a member record in `meta.federation` still
reports that member `offline`; the series do not count it. A request whose
deadline passed before it left is not counted either, in any of the six
calls, because the node was never asked. The client still sees the budget
run out: a `time-out` in the member record, or a `504` (`node-timeout`) for
a routed request or a probe. A call the gateway fails with a `500` because
one of its own tasks panicked counts no member at all, since a defect in
the gateway is never a node's `time-out`.

| Call | Requests counted | `outcome` read from | Time recorded |
|---|---|---|---|
| A federated query, `GET` or `POST {base}/v1/query/aql`, and a stored-query invocation | one per member sent the query | its §11.1 status in `meta.federation` | its `latency_ms` |
| A fan-out template upload | one per member sent the upload | its §11.1 status in `meta.federation` | its `latency_ms` |
| A stored-query distribution, a `PUT` naming members or the admin listener's repair | one per member sent the definition | its §11.1 status in `meta.federation` | its `latency_ms` |
| A stored-query drift check, a `GET` naming members | one per member asked for its copy | its §11.1 status in `meta.federation` | its `latency_ms` |
| A request routed to one node: the `{base}/v1/ehr/` area, `GET {base}/v1/ehr?subject_id=…`, a definition request naming one endpoint, a demographic request | one | the node's answer | from sending it to the answer |
| The ask-all probe, `GET /ehr/{ehr_id}` at every member for a read whose owner no earlier step named | one per member probed | the node's answer | from sending it to the answer, or to the moment the overall budget ran out |

The per-member record a federated query, a template upload, a
distribution and a drift check write is the one the client reads in
`meta.federation`, and the counter follows it exactly. A routed request and
a probe have no such record, so their `outcome` is read from the node's
answer by the same rules. Each outcome therefore covers these calls:

| `outcome` | Covers |
|---|---|
| `active` | a member that answered with success; for a routed request or a probe, any answer below `500`, a `404` included, so a probe that finds no EHR at a member is `active` |
| `node-error` | in a member record, any answer that is not a success, a `4xx` included (except a consent refusal the registry names), and a drift check whose copy differs from the registry's definition or is missing; for a routed request or a probe, a `5xx` answer; in every call, a node that refused the gateway's onward credentials |
| `time-out` | a member that gave no answer before its per-node deadline, and a member still being waited on when the overall budget ran out |
| `offline` | a member the gateway sent a request to and could not reach |
| `consent-denied` | a federated query member whose node answered `403` with a consent refusal code the registry lists for it ([Consent](identity.md#consent)); a member a consent pre-filter dropped is sent no request and is not counted |

## Alerting

Alert on the counters rather than on the log:

```text
increase(ferrofed_integrity_incidents_total[15m]) > 0
increase(ferrofed_registry_reloads_total{result="refused"}[15m]) > 0
sum by (endpoint) (rate(ferrofed_node_requests_total{outcome!="active"}[5m]))
  / sum by (endpoint) (rate(ferrofed_node_requests_total[5m])) > 0.1
```

The gateway does not call a webhook. An incident is counted, and its log
line under `ferrofed::integrity` carries the routing ids you act on; route
the alert from your Prometheus or your collector.
