<!-- SPDX-FileCopyrightText: Vernum Projecten B.V. -->
<!-- SPDX-License-Identifier: BUSL-1.1 -->

# Deployment

The gateway is one static binary in one container image. What it needs
around it is the roles of §4.1: member CDRs over ITS-REST, an identifier
cross-reference, and the registry that addresses each member (§15, N21).
This page draws the two shapes the book describes: the quickstart you run on
one machine, and a production layout. No specification governs the process
model or the topology: our own design.

## The quickstart

The repository's `compose.yaml` runs the gateway beside four member CDRs, four
FerroEHR instances, on one PostgreSQL server that holds a database per node
([The quickstart](../operate/container.md#the-quickstart)). The gateway runs
in the development profile, with a static cross-reference from four synthetic
patients to their EHRs in place of a PIX Manager.

```mermaid
flowchart TB
    you["You: curl and<br/>scripts/quickstart/seed.sh"]
    subgraph compose["docker compose, project ferrofed"]
        gw["ferrofed, :8080<br/>static cross-reference"]
        subgraph nodes["Four member CDRs"]
            a["ferroehr-a<br/>:8081"]
            b["ferroehr-b<br/>:8082"]
            c["ferroehr-c<br/>:8083"]
            d["ferroehr-d<br/>:8084"]
        end
        pg[("ferroehr-postgres<br/>ferroehr_a to ferroehr_d")]
    end
    you -->|"AQL"| gw
    you -->|"seed over ITS-REST"| nodes
    gw -->|"AQL, Basic auth,<br/>the signed caller"| nodes
    nodes -->|"a database each"| pg
```

- Every port binds the loopback interface, so nothing is reachable from the
  network until you name an interface.
- The seed script writes over each node's ITS-REST API only, with
  identifiers in the example arc `urn:oid:2.999`.
- The node credentials and database passwords are development values that
  must not reach anything real.

## A production layout

In production the gateway sits behind your reverse proxy, from the release's
`compose.yaml` or the Kubernetes example, and resolves patients through your
PIX Manager ([The gateway from a
release](../operate/container.md#the-gateway-from-a-release),
[Kubernetes](../operate/container.md#kubernetes)). A dashed box is planned
for v0.0.8.

```mermaid
flowchart TB
    classDef planned stroke-dasharray: 6 4
    clients["Client applications"] -->|"HTTPS"| proxy["Your reverse proxy"]
    proxy -->|"HTTP"| gw["FerroFED gateway"]
    cfg["ferrofed.toml, registry,<br/>secret files"] -->|"start, SIGHUP"| gw
    gw -->|"ITI-83"| pix["PIX Manager"]
    gw -->|"ITS-REST, the<br/>signed caller"| nodes["Member CDRs"]
    gw -->|"definitions"| store[("redb, PostgreSQL<br/>or files")]
    gw -->|"serves"| admin["Admin listener<br/>/metrics, /admin"]
    prom["Prometheus"] -->|"scrapes"| admin
    gw -.->|"v0.0.8"| planned["XCPD localizer (#85)<br/>Mitz consent service (#87)"]:::planned
```

- **The proxy** terminates TLS. The gateway authenticates each caller
  itself, against the callers' issuers, or verifies the assertion of a proxy
  in the explicit edge mode
  ([Client authentication](../operate/authentication.md)).
- **The configuration** is your reviewed files. The registry reloads on
  `SIGHUP` with no restart, and every credential is a file named by a
  `_file` key ([Configuration](../operate/configuration.md)).
- **The admin listener** is a second listener for your operators, off unless
  `[metrics] listen` is set and on loopback unless you allow otherwise
  ([Metrics](../operate/metrics.md)).
- **The planned services** are XCPD
  localization ([#85](https://github.com/FerroHEALTH/FerroFED/issues/85))
  and the Mitz consent pre-filter of the Dutch binding
  ([#87](https://github.com/FerroHEALTH/FerroFED/issues/87)). The
  localization and pre-filter seams they plug into are built. OAuth 2.0 to
  each node's token endpoint
  ([#81](https://github.com/FerroHEALTH/FerroFED/issues/81)) replaces the
  static credential per endpoint ([Trust and keys](trust-and-keys.md)).

## Several replicas

Replicas share nothing in memory: each holds its own `ehr_id` index and
learned routes, and a miss costs a probe or an explicit target, never a
wrong route. The stored-query registry is the one state they must share,
because a stored version must be the same on every replica and a second
`PUT` refused on every replica (§12.7, N44).

```mermaid
flowchart TB
    lb["Your proxy or<br/>Kubernetes Service"] -->|"any request"| r1["Replica 1<br/>own index in memory"]
    lb -->|"any request"| r2["Replica 2<br/>own index in memory"]
    r1 -->|"stored queries"| pg[("One shared<br/>PostgreSQL database")]
    r2 -->|"stored queries"| pg
    r1 -->|"node queries"| nodes["Member CDRs"]
    r2 -->|"node queries"| nodes
```

A `redb` file opens in one process at a time, so replicas that store queries
use the `postgres` backend, or the read-only `files` backend when your
operator publishes the definitions
([Running several replicas](../operate/deployment-shape.md#running-several-replicas)).
The Kubernetes example runs two replicas under a PodDisruptionBudget.
