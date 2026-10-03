<!-- SPDX-FileCopyrightText: Vernum Projecten B.V. -->
<!-- SPDX-License-Identifier: BUSL-1.1 -->

# Trust and keys

The gateway stands in two trust relationships. Callers trust it with their
access tokens, and the nodes trust it to say who is asking. The
specification requires both: the client authenticates to the gateway, the
gateway authenticates onward to each node, and the client's identity
travels with every request (§13.1, N24, N25, CP-16, CP-17).

Client authentication
([#80](https://github.com/FerroHEALTH/FerroFED/issues/80)), OAuth 2.0 to
each node with the gateway's own key
([#81](https://github.com/FerroHEALTH/FerroFED/issues/81)) and the caller's
identity conveyed to each node
([#82](https://github.com/FerroHEALTH/FerroFED/issues/82)) run today.

## The gate

Every request to the ITS-REST surface, and `OPTIONS {base}/`, passes the
gate before anything else reads it: the caller's access token is verified,
the operation's scope and the purpose of use are checked, and a refused
request reaches no node ([Client authentication](../operate/authentication.md)).

```mermaid
flowchart TB
    client["Client application"] -->|"HTTPS, Bearer<br/>access token"| proxy["Your reverse proxy:<br/>TLS"]
    proxy -->|"HTTP"| gate["The gate: token,<br/>scope, purpose of use"]
    gate -->|"401, 403, 503"| client
    gate --> gw["FerroFED gateway"]
    gw -->|"its own credential<br/>per endpoint"| node["Node A"]
```

A caller's `Authorization` header never reaches a node. A proxy that
authenticates callers itself uses the explicit edge mode, signing an
assertion the gate verifies. Each onward credential is a file named by a
`_file` key, sent only to its own endpoint.

## Two trust relationships

```mermaid
flowchart TB
    as["Callers' authorization servers<br/>one signing key each"]
    subgraph gwbox["FerroFED gateway"]
        trust["Trust list<br/>of issuers"]
        key["Gateway key pair, ES384<br/>private half: a _file secret"]
        jwks["{base}/.well-known/jwks.json<br/>current and previous kid"]
    end
    subgraph nodebox["Member node A"]
        tok["Token endpoint"]
        cdr["CDR"]
    end
    as -->|"each issuer's JWKS"| trust
    key -->|"public half"| jwks
    jwks -->|"verifies the<br/>client assertion"| tok
    jwks -->|"verifies openEHR-<br/>federation-client"| cdr
    tok -->|"access token"| cdr
```

- **Callers to the gateway** ([#80](https://github.com/FerroHEALTH/FerroFED/issues/80)):
  each calling organisation's authorization server signs its own access
  tokens. The gateway keeps a trust list of issuers and reads each issuer's
  JWKS, so it holds many public key sets and no caller's private key.
- **The gateway to each node** ([#81](https://github.com/FerroHEALTH/FerroFED/issues/81),
  §13.1, N25, N30): the gateway has one key pair of its own. The private half
  is a secret file; the public half is published at
  `{base}/.well-known/jwks.json` and declared as `federation.auth.jwks_uri`
  in `OPTIONS {base}/`, so a node finds it without an out-of-band arrangement
  (§13.1 `jwks-discovery`). A rotation keeps the current and the previous
  `kid` in the set for one overlap window. The gateway needs no node's key
  for this.
- **The identity sent to the node** ([#82](https://github.com/FerroHEALTH/FerroFED/issues/82)):
  the `openEHR-federation-client` header is a JWS signed with the same
  gateway key, so the node verifies it against the same JWKS.

## The token flow

```mermaid
%%{init: {"sequence": {"actorMargin": 24}}}%%
sequenceDiagram
    participant AS as Caller's AS
    participant C as Client
    participant G as Gateway
    participant T as Node A token<br/>endpoint
    participant N as Node A
    C->>AS: token request
    AS-->>C: JWT, aud: the gateway
    C->>G: request, Bearer<br/>(caller token)
    G->>AS: read JWKS, cached
    Note over G: verify alg, iss,<br/>aud, exp, scope,<br/>purpose of use
    G->>T: client credentials,<br/>RFC 7523 assertion
    T->>G: read the gateway JWKS
    T-->>G: node token, cached
    G->>N: request, Bearer (node token),<br/>openEHR-federation-client
    N-->>G: answer
    G-->>C: answer
```

**Checking the caller** ([#80](https://github.com/FerroHEALTH/FerroFED/issues/80)).
The access token is a JWT (RFC 9068), checked against the issuer's JWKS,
and every check fails closed. The issuer must be on the trust list, and the
algorithm on an allow-list (ES256, ES384, PS256, RS256), so `none` and HMAC
are refused (RFC 8725). The audience must name this gateway, and the token
must not have expired. Any of those is a `401`. The scopes are read in the
SMART on openEHR grammar with `openehr-sdt`, and a token without a scope
that covers the operation is a `403`. A token without a purpose of use is a
`403` too, unless your deployment declares it optional (§13.4). For a
deployment whose proxy already authenticates callers, the edge mode is
configured explicitly: the proxy signs an assertion of the caller, the
gateway verifies it like a token and records which identity the proxy
asserted.

**Authenticating to the node** ([#81](https://github.com/FerroHEALTH/FerroFED/issues/81),
§13.1, N25). The gateway sends the node's token endpoint an OAuth 2.0 client
credentials request with a client assertion of RFC 7523 §2.2. The assertion
lives 300 seconds at most, carries a unique `jti`, and names the node's
token endpoint as its audience. The token endpoint verifies it against the
gateway's JWKS and issues an access token. The gateway caches that token
per node until 30 seconds before it expires, and drops it on a `401`. A
token it cannot obtain fails that node `node-error`; the gateway never
sends a request without one. Where a node's authorization server supports
token exchange (RFC 8693), the issued token can carry the caller as the
delegating subject.

**Telling the node who asks** ([#82](https://github.com/FerroHEALTH/FerroFED/issues/82),
N24, §12.4). Every request to a node carries `openEHR-federation-client`: a
query, a routed read or write, a definition request, the ask-all probe and
the read of an EHR by subject alike. It is a JWS signed ES384 with the
gateway key, typed `openehr-federation-client+jwt`, that lives 60 seconds,
addressed (`aud`) to that node's endpoint id. It names the verified caller
(`sub`) and its issuer (`iss_upstream`), how the gateway verified it
(`verified_by`, `edge` for an identity the edge asserted), the caller's
organisation, the purpose of use and the caller's scopes as granted. It
never carries a patient identifier (N33): the outbound gate reads every
caller claim, and a request whose claims would carry the identifier the
query was resolved on is refused before it is sent. A request that reaches
the dispatcher with no verified caller is a `500`, with nothing sent. The
gateway's own requests for its operator, the admission check and the
redistribution of a held stored query, name the gateway itself as `sub`.
The specification leaves end-user conveyance open (§13.1), so the header
and its claims are FerroFED's own; a node verifies it as
[Client authentication](../operate/authentication.md#verifying-it-at-the-node)
describes.

## The caller's token is never forwarded

The caller's token was issued for the gateway: its audience is the gateway.
Forwarding it would hand every node a credential that unlocks every other
member that accepts the same issuer (RFC 9700 §2.3), and a node could replay
it at its neighbours. So the gateway authenticates to each node as itself,
with a token the node's own authorization server issued, and conveys the
caller as a signed statement the node can verify. It never asks a node for
more than the caller holds: the caller's scopes are enforced at the gateway
and conveyed, so the node applies them too (N26). Consent stays the node's
decision either way (§13.2, N27).

The §13.4 decisions a deployment must publish, such as which identity is
verified across each boundary and who authenticates the end user, are
planned for v0.0.8
([#84](https://github.com/FerroHEALTH/FerroFED/issues/84), CP-39).
