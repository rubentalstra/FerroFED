<!-- SPDX-FileCopyrightText: Vernum Projecten B.V. -->
<!-- SPDX-License-Identifier: BUSL-1.1 -->

# Health probes

The gateway answers three health routes under its
[base path](configuration.md#the-base-path), and the binary carries a
`healthcheck` command for a runtime that cannot send an HTTP request itself.
No specification governs health probes: our own design.

## The routes

| Route | Answers | Use it as |
|---|---|---|
| `GET {base}/health` | `200` while the process serves; it checks nothing else | liveness |
| `GET {base}/health/readiness` | `200` while the gateway serves and its own subsystems are up; `503` before boot completes and from the moment `SIGTERM` or `SIGINT` arrives | readiness, startup, the image `HEALTHCHECK` |
| `GET {base}/health/dependencies` | always `200`, with the state the gateway last observed of each member endpoint, of the resolver, of the consent pre-filter, of the localizer and of the mCSD directory | monitoring, never a probe |

Readiness reports the gateway's own subsystems by name: the configuration,
the registry and the outbound clients when a registry is configured, and the
stored-query store when one is. Its body names the phase of the process,
`booting`, `serving` or `draining`. On `SIGTERM` readiness turns `503` before
the drain starts, so a load balancer stops sending requests while the
requests in flight finish.

No member node and no identity source gates readiness. A node outage is
reported per query in `meta.federation` (§11), and a gateway that went unready
with one node would turn one CDR outage into a total outage. Their state is on
`GET {base}/health/dependencies` instead:

```json
{
  "endpoints": { "node-a-query": "up", "node-b-query": "down" },
  "resolver": "up",
  "consent": "down",
  "localizer": "up"
}
```

Each state is the one the last request the gateway made for a client
observed, and it reports the member's reachability and health, never whether
that request was valid:

| State | The last request |
|---|---|
| `up` | got an answer below `500`, a refusal such as `400`, `401`, `404` or `409` included |
| `failing` | got a `5xx` answer |
| `down` | got no answer: the member could not be reached, or did not answer in time |
| `unknown` | none has reached the member since the registry was loaded or reloaded |

The state reads the node's own HTTP status, whatever the call's §11.1 record
in `meta.federation` says: a query member that answered `400` is
`node-error` there and `up` here. The gateway sends no request of its own to
find out, and a request that never left the gateway changes nothing. Every
call that sends a request to a member updates it: a federated query, a
request routed to one node, the ask-all probe, a fan-out template upload, and
a stored-query distribution, repair or drift check. A drift check that finds
a member's copy different or missing records the member `up`, because it
answered. A resolution updates `resolver`, which is absent when no resolver
is configured. A call to the consent pre-filter updates `consent` by the same
rule: a decision, or an answer below `500`, is `up`, a `5xx` is `failing`, and
no answer is `down`. It is absent when no pre-filter is configured
([Consent](identity.md#consent)). A call to the localizer updates `localizer`
by the same rule: a candidate set, or an answer that no member holds the
patient, is `up`, a failure answered below `500` is `up`, a `5xx` is
`failing`, an XCPD exchange whose audit message could not be recorded is
`failing`, and no answer, a silent localizer past its budget included, is
`down`. It is absent when no localizer is configured
([Node selection](registry.md#node-selection)). A refresh of the mCSD directory the registry
is read from updates `directory`: an answer is `up`, a `5xx` or an answer
that breaks ITI-90 or ITI-91 is `failing`, and no answer is `down`. It is
absent when the registry is a document
([The registry](registry.md#the-registry-read-from-an-mcsd-directory)). The
body names endpoint ids and states only, never a URL, a credential or a
body.

## `ferrofed healthcheck`

```text
ferrofed healthcheck --config /etc/ferrofed/ferrofed.toml
```

The command reads the configuration the way `serve` does and asks the
gateway running on this host for `GET {base}/health/readiness`. It connects
to the port of `server.listen`, on `127.0.0.1` or `[::1]` when the address
is a wildcard and on the address itself otherwise, and prints one line. It
exits `0` only when readiness answers `200` within three seconds, and `1` for
any other status, a refused connection, no answer in time, or a
configuration that does not load: the two codes a container runtime's health
check reads. The [image](container.md#the-image) runs it as its
`HEALTHCHECK`, and so does the `compose.yaml` gateway service.

A Kubernetes pod probes the routes directly
([Kubernetes](container.md#kubernetes)): startup and readiness on
`{base}/health/readiness`, liveness on `{base}/health`. Point every probe at
the base path you configured, because every path outside it answers `404`.
