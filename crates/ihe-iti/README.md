<!-- SPDX-FileCopyrightText: Vernum Projecten B.V. -->
<!-- SPDX-License-Identifier: BUSL-1.1 -->

# ihe-iti

The IHE IT Infrastructure (ITI) profiles in Rust: one crate for the technical
framework, with a feature per profile.

| Feature | Profile | Transactions |
|---|---|---|
| `pixm` | Patient Identifier Cross-reference for Mobile | ITI-83 |
| `pdqm` | Patient Demographics Query for Mobile | ITI-78 |
| `mcsd` | Mobile Care Services Discovery | ITI-90, ITI-91 |
| `pmir` | Patient Master Identity Registry | ITI-93, ITI-94 |
| `xcpd` | Cross-Community Patient Discovery | ITI-55 |

The crate depends on no application, so a federation gateway, a master patient
index or any other caller can use it as it is. Only the `xcpd` feature may carry
the SOAP 1.2, HL7 v3 and SAML XUA stack; a build without it compiles none of it.
The profiles are published at <https://profiles.ihe.net/ITI/>.

## PIXm (`pixm`)

`ihe_iti::pixm::PixmClient` is the Patient Identifier Cross-reference
Consumer of ITI-83 (PIXm 3.1.0): `GET [base]/Patient/$ihe-pix` with one
`sourceIdentifier` and any number of `targetSystem` domains, read into a
cross-reference, the profile's not-found answer, or a typed error. A `404` is
"patient unknown" only when its `OperationOutcome` carries a `not-found` issue,
so an outage or a misrouted request is never mistaken for a patient with no
identifiers. Every answer is held to the `$ihe-pix` `OperationDefinition`: the
two out parameters, an assigning authority on each identifier, identifiers only
from the domains asked about, and never the source identifier itself.

Identifier values travel in `secrecy::SecretString`, with redacted `Debug` and
no `Display`, and no error carries a value, the request URL or the Manager's
free text. Build the `reqwest::Client` you pass in with
`redirect::Policy::none()`: the request URL holds the source identifier. The
client's `Debug` shows its URL with the userinfo replaced by `***` and leaves
out the `reqwest::Client`, whose default headers may hold a credential.

## PDQm (`pdqm`)

`ihe_iti::pdqm::PdqmClient` is the Patient Demographics Consumer of ITI-78
(PDQm 3.2.0): a `POST [base]/Patient/_search` with the criteria of a
`PatientQuery` (every Patient search parameter ITI-78 names, with `:exact` on
the string ones), read into a page of matching Patients with the `total`, each
match's `fullUrl`, `search.score` and `match-grade`, the warnings of any
`OperationOutcome` entry, and the `next` link that `next_page` follows on the
Supplier's own origin. No match is a `total` of `0`; a `404` is "identifier
domain not recognised" only when the query names a domain and the
`OperationOutcome` carries a `not-found` issue. Every answer is held to the
Query Patient Resource Response Message profile: a `searchset` with a
`total`, and a `fullUrl` on every entry. The Patients are decoded as FHIR R4,
not held to the PDQm Patient profile, as the profile asks of a Consumer.

The criteria travel in the request body, so no URL carries them. They, every
matched Patient and every page link redact their content in `Debug`, with no
`Display`, and no error carries a value, a URL or the Supplier's free text.
Build the `reqwest::Client` you pass in with `redirect::Policy::none()`. The
client's `Debug` shows its URLs as the PIXm client's does.

## mCSD (`mcsd`)

`ihe_iti::mcsd::directory::Directory` reads the `Organization` and `Endpoint`
resources a care services directory publishes (mCSD 4.0.0) from one FHIR R4
JSON `Bundle` of type `collection` or `searchset`. It resolves the references
between them inside the Bundle as FHIR R4 §2.36.4.1 does: a relative
`[type]/[id]` against the root of the referring entry's REST `fullUrl`, an
absolute reference against an entry's `fullUrl`. A reference it cannot
resolve is reported as outside the Bundle, never guessed. It refuses an entry
of another resource type, a resource with a `modifierExtension`, and a
repeated `fullUrl` or logical id. Each resource stays as `fhir-types` decodes
it, with accessors for what addressing reads; what a caller accepts as a
connection type, a status or an identifier is the caller's policy. The
`Debug` of an entry shows its `fullUrl`, and an endpoint's `address`, with the
userinfo and the query replaced by `***`, and leaves out an endpoint's
`header` list.

`ihe_iti::mcsd::client::McsdClient` asks a directory over HTTP. `find` is
ITI-90, Find Matching Care Services: `GET [base]/Organization` or
`GET [base]/Endpoint` with the search parameters you give, answered with a
`searchset` Bundle. `updates` is ITI-91, Request Care Services Updates:
`GET [base]/[type]/_history?_since=[instant]`, answered with a `history`
Bundle, newest version first, a deletion as a `DELETE` entry. Both follow
every `next` link on the directory's own origin, up to 1000 pages, and
return the `Date` the directory stamped its first page with. A status other
than `200`, an answer that is not the interaction's Bundle, a match of
another type and a page link to another origin are errors that carry no URL
and no text of the directory's. Build the `reqwest::Client` you pass in with
`redirect::Policy::none()`.

`ihe_iti::mcsd::replica::Replica` holds a directory's `Organization`s and
`Endpoint`s in a `Scope` (every resource, or those carrying an identifier in
a system you name), read with ITI-90 and kept in step with ITI-91 from 60
seconds before the directory's own clock reading. The newest version of each
resource wins, a deletion or a version that leaves the scope removes it, and
a directory that sent no readable `Date` is read again whole. A refresh
returns a new replica and leaves the old one as it was, and `directory`
answers the content as a `Directory`.

## XCPD (`xcpd`)

`ihe_iti::xcpd::XcpdClient` is the Initiating Gateway of ITI-55, Cross
Gateway Patient Discovery (ITI TF-2 §3.55, Revision 20.1): a
`PRPA_IN201305UV02` in a SOAP 1.2 envelope with the WS-Addressing headers of
Appendix V, posted to one Responding Gateway, which asks by the shared
patient identifier alone (the identifier mode of §3.55.1, no demographics).
The answer reads into a `Discovery` for Cases 1 to 4 of §3.55.4.2.3, each
match with the `homeCommunityId` that holds it, or a typed error for Case 5,
a SOAP fault, a status with no answer, a timeout, and any answer that does not
hold to the transaction: a `RelatesTo` naming another request, a match with no
community, a `NF` with matches, a document type declaration. Only the
synchronous exchange with an immediate response is offered.

A `RespondingGateway` is `https` only, since the request carries the
patient identifier and any XUA assertion; `unencrypted_for_development`
admits `http` for development and tests. The `reqwest::Client` you pass in
carries the mutual TLS of the ATNA Secure Node the actor is grouped with;
build it with `redirect::Policy::none()`.

The crate signs nothing. `XuaAssertion` holds a SAML 2.0 assertion your
identity provider signed, checked to be exactly one `saml2:Assertion`
element that declares every prefix it uses, and the client sends its bytes
unchanged in a WS-Security header. The patient identifier, every identifier
an answer returns, and the assertion redact their content in `Debug`, and no
error carries a value or the gateway's free text.

`XcpdClient::audited` hands the ITI-55 Initiating Gateway audit message of
every exchange (§3.55.5.1.1) to an `AuditRecorder` you route to your ATNA
audit repository. The message carries the query parameters, which name the
patient, as a `SecretString`. A message the recorder refuses fails the
discovery with `XcpdError::Audit`, so no answer is used without its audit.

The XML is read and written with `quick-xml`, which the FHIR features
compile already through `fhir-types`; `xcpd` adds `uuid` for the message
ids, which no other feature compiles, and `jiff` for the creation time,
which `mcsd` compiles too for the history's `_since` and the directory's
`Date`.

The other profile modules hold their place and land with their FerroFED issues
(<https://github.com/FerroHEALTH/FerroFED>).

## Licence

Business Source License 1.1 (`LICENSE`): free for every non-production use and
for non-commercial production use; a commercial licence for other production
use; Apache License 2.0 four years after each version.
