// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! The outbound gate: the last check on every request the gateway composes
//! for a node (§5.4.1, N33).
//!
//! The rewrite already refuses a query whose patient identifier would survive
//! into a node query. The gate is the second layer, independent of how the
//! request was built: right before a request leaves, it re-reads the AQL text
//! and paging of the body, the path and query of the URL, and the headers the
//! gateway adds, against the identifiers resolution consumed, and refuses to
//! send a request that still carries one in any form a node could read it in.
//! The authority of the URL (its host and port) is never read, and neither is
//! the `Host` header the HTTP client writes from it. §5.4.1 governs what the
//! gateway composes for a request, and both are the endpoint URL of the
//! operator's registry, composed from no request, so neither can carry a
//! client-supplied identifier; a client's own `Host` is never forwarded.
//! A refusal names the part of the request, never the value, and nothing is
//! sent. A write body is never inspected or altered (§5.4 scope note).
//!
//! The dispatcher passes every header it adds except the minted
//! `X-Request-Id`: that value is an
//! [`OutboundId`](crate::outbound_id::OutboundId), made from no client input,
//! so it cannot carry an identifier, and a short all-hex identifier can occur
//! inside it by chance, which would refuse a valid request.
//!
//! The [`conveyance::HEADER`] is read by its claims, since the token encodes
//! them: every claim that comes from the caller's credential is searched as
//! it is before encoding ([`Outbound::conveyed`]), and a request whose caller
//! claims carry a withheld identifier is refused like any other header, never
//! sent with the claim masked. The gateway's `iss`, the node's `aud` and the
//! minted `iat`, `exp` and `jti` come from no request, as the minted id does.
//!
//! A single-node route forwards a client request, so the gate also decides
//! which parts of it travel at all, from the parameters the matched ITS-REST
//! operation declares (`openehr-its`'s `routes::lookup`): the client headers
//! the operation declares and nothing else ([`forwarded_headers`]), and a
//! query string only when the operation declares every parameter in it
//! ([`forwarded_query`]). The gateway cannot tell an identifying value from
//! any other by looking at it, so an undeclared header is stripped and an
//! undeclared query parameter is refused (§5.4.1, N33). A declared value that
//! travels is held to the kind the operation declares for it
//! ([`crate::declared`]).
//!
//! [`mask`] holds text the gateway writes but did not compose, a node's error
//! message, to the same identifiers: each one it finds is replaced.

use std::fmt;

use http::HeaderMap;
use openehr_its::rest::routes::RouteMatch;
use openehr_query::printer::escape_string;
use secrecy::{ExposeSecret, SecretString};
use url::Url;

use crate::onward::conveyance;

pub(crate) mod decode;
pub mod mask;

/// The client headers a single-node route never forwards, whatever the
/// operation declares, beside every header under
/// [`WITHHELD_HEADER_PREFIX`].
///
/// `Authorization` is the client's credential at the gateway, and the gateway
/// authenticates onward with the endpoint's own credentials (§13).
/// `X-Request-Id` is the client's free text, and a node receives the
/// gateway's minted [`OutboundId`](crate::outbound_id::OutboundId) instead.
/// Each name is compared without regard to case (RFC 9110 §5.1).
// NOTE: §5.4.1, N33: no ITS-REST operation declares these headers, and a
// client value in either could name a patient, so both stay withheld.
pub const WITHHELD_HEADERS: [&str; 2] = ["authorization", "x-request-id"];

/// The name prefix of the federation's own headers, none of which a
/// single-node route forwards, whatever the operation declares.
///
/// Every request header the specification defines carries it
/// (`openehr_federation::headers::ALL`): the targeting headers (§8.4), the
/// completion strategy (§11.4) and the dedup mode (§10). Each is consumed at
/// the gateway and means nothing at a node. A name is compared without
/// regard to case (RFC 9110 §5.1).
// NOTE: §5.4.1, N33: the gateway copies no header without a rule naming it, so
// the whole family is withheld by name, a header the gateway does not read too.
pub const WITHHELD_HEADER_PREFIX: &str = "openEHR-federation-";

/// Whether the client header `name` is one a single-node route never
/// forwards: one of [`WITHHELD_HEADERS`], or any name under
/// [`WITHHELD_HEADER_PREFIX`].
#[must_use]
pub fn is_withheld_header(name: &str) -> bool {
    let federation = name
        .get(..WITHHELD_HEADER_PREFIX.len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(WITHHELD_HEADER_PREFIX));
    federation
        || WITHHELD_HEADERS
            .iter()
            .any(|withheld| name.eq_ignore_ascii_case(withheld))
}

/// The query parameters a single-node route never forwards, whatever the
/// operation declares: the patient identifier and its namespace (§5.4.2).
// NOTE: §5.4.1, N33: only `GET /ehr` declares them, which resolution answers
// at the gateway, so no forwarded request carries them.
pub const WITHHELD_QUERY_PARAMETERS: [&str; 2] = ["subject_id", "subject_namespace"];

/// The client headers of `client` a single-node route forwards for
/// `operation`, less every header [`is_withheld_header`] names.
///
/// Every field line of each header the operation declares is kept, its value
/// byte for byte. A field name is compared without regard to case (RFC 9110 §5.1). The
/// federation's own request headers are consumed at the gateway and never
/// forwarded, even were an operation to declare one.
#[must_use]
pub fn forwarded_headers(operation: &RouteMatch, client: &HeaderMap) -> HeaderMap {
    admitted_headers(|name| operation.header_param(name).is_some(), client)
}

/// The headers of `client` whose name `declared` accepts, less every header
/// [`is_withheld_header`] names, each field line kept byte for byte.
fn admitted_headers(declared: impl Fn(&str) -> bool, client: &HeaderMap) -> HeaderMap {
    let mut forwarded = HeaderMap::new();
    for (name, value) in client {
        if !is_withheld_header(name.as_str()) && declared(name.as_str()) {
            forwarded.append(name.clone(), value.clone());
        }
    }
    forwarded
}

/// `query` when `operation` declares every parameter in it and none is one of
/// [`WITHHELD_QUERY_PARAMETERS`], to be forwarded as received.
///
/// A parameter's name is compared percent-decoded, and an empty pair
/// (`a=1&&b=2`) carries nothing and is ignored.
///
/// # Errors
///
/// Returns [`UnlistedParameter`] naming the first other parameter by its
/// position, never by its name or value, either of which may be the
/// identifier.
pub fn forwarded_query<'q>(
    operation: &RouteMatch,
    query: &'q str,
) -> Result<&'q str, UnlistedParameter> {
    crate::declared::query::admitted(query, |name| {
        !WITHHELD_QUERY_PARAMETERS.contains(&name) && operation.query_key(name).is_some()
    })?;
    Ok(query)
}

/// A query parameter a single-node route does not forward, so the request is
/// refused before anything is sent (§5.4.1, N33).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error(
    "query parameter {position} is not one the ITS-REST operation declares, so the request was not sent"
)]
pub struct UnlistedParameter {
    /// The parameter's position in the query string, counted from 1.
    pub position: usize,
}

/// The identifiers resolution consumed for one query, which no request to a
/// node may carry (§5.4.1).
///
/// `Debug` prints how many there are and never a value.
#[derive(Clone, Default)]
pub struct Withheld(Vec<SecretString>);

impl Withheld {
    /// No identifier is withheld: a query that named no patient.
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }

    /// The identifiers `values`, ignoring empty ones, which no request could
    /// be checked against.
    #[must_use]
    pub fn new(values: impl IntoIterator<Item = SecretString>) -> Self {
        Self(
            values
                .into_iter()
                .filter(|value| !value.expose_secret().is_empty())
                .collect(),
        )
    }

    /// Whether no identifier is withheld.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The first part of `request` that carries a withheld identifier, in any
    /// form a node could read it in, or `None` for a clean request.
    ///
    /// The scope literal the rewrite wrote (`'<ehr_id>'`) is masked out of
    /// the AQL text before it is read, so a short identifier that happens to
    /// occur inside the node's own `ehr_id` is no false refusal. An
    /// identifier that contains the whole `ehr_id` is never masked: the
    /// cross-reference maps a patient to an `ehr_id` the node minted, so the
    /// two are equal only through a defect, and the gate then fails closed.
    ///
    /// The URL path is masked the same way, by position, in one place only
    /// ([`Composed`]): the node's own `ehr_id` segment after `{base}/ehr/`,
    /// when the gateway wrote it. The base path, every other part of the
    /// path, the query and the headers are searched whole.
    #[must_use]
    pub fn found_in(&self, request: &Outbound<'_>) -> Option<Part> {
        self.0.iter().find_map(|value| {
            let value = value.expose_secret();
            let escaped = escape_string(value);
            let in_aql = |fragment: &str| fragment.contains(value) || fragment.contains(&escaped);
            let aql_carries = match request.scope.filter(|scope| !value.contains(*scope)) {
                Some(scope) => {
                    let token = format!("'{}'", escape_string(scope));
                    request.aql.split(token.as_str()).any(in_aql)
                }
                None => in_aql(request.aql),
            };
            let carries =
                |text: &str| text.contains(value) || decode::percent_decoded(text).contains(value);
            if aql_carries {
                Some(Part::Aql)
            } else if request.paging.iter().any(|number| number.contains(value)) {
                Some(Part::Paging)
            } else if carried_in_target(request.url, request.composed, value) {
                Some(Part::Url)
            } else if request.conveyed.iter().any(|claim| carries(claim)) {
                Some(Part::Header(conveyance::HEADER))
            } else {
                request
                    .headers
                    .iter()
                    .find(|(_, text)| carries(text))
                    .map(|(name, _)| Part::Header(name))
            }
        })
    }
}

impl fmt::Debug for Withheld {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Withheld")
            .field("identifiers", &self.0.len())
            .finish()
    }
}

/// What a request to a node carries that a node can read: the parts the gate
/// checks.
#[derive(Debug, Clone, Copy)]
pub struct Outbound<'a> {
    /// The AQL text of the body.
    pub aql: &'a str,
    /// The node's own `ehr_id` the rewrite scoped the AQL to, if any.
    pub scope: Option<&'a str>,
    /// The body's paging members, as the text they are sent as.
    pub paging: &'a [String],
    /// The URL the request is sent to, of which the gate reads the path, the
    /// query and the fragment, and never the authority.
    pub url: &'a Url,
    /// The node's own `ehr_id` segment the gateway composed into the path.
    pub composed: Composed<'a>,
    /// The headers the gateway adds, by name, other than the minted
    /// `X-Request-Id` and the [`conveyance::HEADER`].
    pub headers: &'a [(&'static str, &'a str)],
    /// Every claim of the [`conveyance::HEADER`] that comes from the
    /// caller's credential ([`Conveyance::carried`]), each read as it is
    /// before the token encodes it.
    ///
    /// [`Conveyance::carried`]: crate::onward::conveyance::Conveyance::carried
    pub conveyed: &'a [&'a str],
}

/// The node's own `ehr_id` segment of a request's URL path, when the
/// gateway composed it, which the gate masks by position before it searches
/// the path.
///
/// The `ehr_id` is never client text: it comes from a resolution or the
/// `ehr_id` index. The path before it, the endpoint's base path included,
/// stays searched.
#[derive(Debug, Clone, Copy, Default)]
pub struct Composed<'a> {
    /// The path the segment follows, `{base}/ehr/`, which is searched.
    pub ehr_prefix: &'a str,
    /// The node's own `ehr_id` segment, percent-encoded as the path carries
    /// it right after [`Composed::ehr_prefix`]; `None` when that segment is
    /// the client's.
    pub ehr_segment: Option<&'a str>,
}

/// The part of a request that carried a withheld identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Part {
    /// The AQL text of the body, raw or as an AQL string literal.
    Aql,
    /// A paging member of the body.
    Paging,
    /// The path, query or fragment of the URL, raw or percent-decoded.
    Url,
    /// A header the gateway adds.
    Header(&'static str),
}

impl fmt::Display for Part {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Aql => f.write_str("the AQL text"),
            Self::Paging => f.write_str("a paging member"),
            Self::Url => f.write_str("the URL"),
            Self::Header(name) => write!(f, "the {name} header"),
        }
    }
}

/// Whether the path, query or fragment of `url`, raw or percent-decoded,
/// carries `value`, once the `composed` segment of the path is masked.
fn carried_in_target<'a>(url: &'a Url, composed: Composed<'a>, value: &str) -> bool {
    // NOTE: §5.4.1, N33 name the parts the gateway composes; the authority, and the `Host`
    // written from it, is the registry endpoint URL, composed from no request, so it is unread.
    let (before, after) = searched_path(url.path(), composed, value);
    let mut target = after.to_owned();
    if let Some(query) = url.query() {
        target.push('?');
        target.push_str(query);
    }
    if let Some(fragment) = url.fragment() {
        target.push('#');
        target.push_str(fragment);
    }
    [before, target.as_str()]
        .into_iter()
        .any(|text| text.contains(value) || decode::percent_decoded(text).contains(value))
}

/// The two stretches of `path` the gate searches for `value`: the text
/// before a masked `ehr_id` segment, and the text after it.
///
/// The segment is masked only where `composed` names it, right after
/// [`Composed::ehr_prefix`] and followed by the end or a `/`, and never when
/// `value` contains it, so an identifier equal to it fails closed. Without a
/// masked segment, the first stretch is empty and the second is the path.
fn searched_path<'a>(path: &'a str, composed: Composed<'a>, value: &str) -> (&'a str, &'a str) {
    // NOTE: §5.4, N33: a node is located by the ehr_id it minted, and the gateway composed this one
    // from a resolution, never from the client, so a value inside it is chance, not a leak.
    let head = composed.ehr_prefix;
    let segment = composed
        .ehr_segment
        .filter(|segment| !head.is_empty() && !segment.is_empty() && !value.contains(*segment));
    let tail = segment.and_then(|segment| {
        path.strip_prefix(head)?
            .strip_prefix(segment)
            .filter(|tail| tail.is_empty() || tail.starts_with('/'))
    });
    match tail {
        Some(tail) => (head, tail),
        None => ("", path),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::LazyLock;

    use super::{Composed, Outbound, Part, Withheld};
    use openehr_its::rest::routes::{Lookup, RouteMatch, lookup};
    use secrecy::SecretString;
    use url::Url;

    const SENTINEL: &str = "O'Sentinel-4711";

    /// The query URL of an endpoint whose authority and path hold no
    /// withheld value.
    static QUERY_URL: LazyLock<Url> = LazyLock::new(|| url("https://cdr.example.org/v1/query/aql"));

    fn url(text: &str) -> Url {
        Url::parse(text).expect("a test URL")
    }

    fn withheld() -> Withheld {
        Withheld::new([SecretString::from(SENTINEL)])
    }

    fn request<'a>(
        aql: &'a str,
        url: &'a Url,
        headers: &'a [(&'static str, &'a str)],
    ) -> Outbound<'a> {
        Outbound {
            aql,
            scope: None,
            paging: &[],
            url,
            composed: Composed::default(),
            headers,
            conveyed: &[],
        }
    }

    // conformance: CP-26
    #[test]
    fn the_identifier_in_a_conveyed_claim_names_the_conveyance_header() {
        for claim in ["O'Sentinel-4711", "urn:x:O%27Sentinel-4711"] {
            let conveyed = ["user/aql-*.s", claim];
            let outbound = Outbound {
                conveyed: &conveyed,
                ..request("SELECT 1", &QUERY_URL, &[])
            };
            assert_eq!(
                Some(Part::Header(super::conveyance::HEADER)),
                withheld().found_in(&outbound),
                "{claim}"
            );
        }
    }

    #[test]
    fn a_clean_request_passes() {
        let clean = request(
            "SELECT c FROM EHR e CONTAINS COMPOSITION c WHERE e/ehr_id/value='7d44'",
            &QUERY_URL,
            &[("X-Request-Id", "req-1")],
        );
        assert_eq!(None, withheld().found_in(&clean));
    }

    #[test]
    fn the_identifier_as_an_aql_literal_is_found_escaped() {
        let aql =
            "SELECT c FROM EHR e CONTAINS COMPOSITION c WHERE c/name/value='O\\'Sentinel-4711'";
        assert_eq!(
            Some(Part::Aql),
            withheld().found_in(&request(aql, &QUERY_URL, &[]))
        );
    }

    #[test]
    fn the_identifier_percent_encoded_in_the_url_is_found() {
        let target = url("https://cdr.example.org/v1/query/aql?x=O%27Sentinel-4711");
        assert_eq!(
            Some(Part::Url),
            withheld().found_in(&request("SELECT 1", &target, &[]))
        );
    }

    #[test]
    fn the_identifier_in_a_header_names_the_header() {
        let headers = [("X-Request-Id", "req-O'Sentinel-4711")];
        assert_eq!(
            Some(Part::Header("X-Request-Id")),
            withheld().found_in(&request("SELECT 1", &QUERY_URL, &headers))
        );
    }

    #[test]
    fn no_identifier_withheld_finds_nothing() {
        let aql =
            "SELECT c FROM EHR e CONTAINS COMPOSITION c WHERE c/name/value='O\\'Sentinel-4711'";
        assert_eq!(
            None,
            Withheld::none().found_in(&request(aql, &QUERY_URL, &[]))
        );
    }

    const EHR_ID: &str = "7d44b88c-4199-4bad-97dc-d78268e01398";

    fn scoped(aql: &str) -> Outbound<'_> {
        Outbound {
            scope: Some(EHR_ID),
            ..request(aql, &QUERY_URL, &[])
        }
    }

    fn short() -> Withheld {
        Withheld::new([SecretString::from("4199")])
    }

    #[test]
    fn a_short_identifier_inside_the_scope_ehr_id_passes() {
        let aql =
            format!("SELECT c FROM EHR e CONTAINS COMPOSITION c WHERE e/ehr_id/value = '{EHR_ID}'");
        assert_eq!(None, short().found_in(&scoped(&aql)));
    }

    #[test]
    fn the_same_short_identifier_elsewhere_is_found() {
        let aql = format!(
            "SELECT c FROM EHR e CONTAINS COMPOSITION c WHERE e/ehr_id/value = '{EHR_ID}' AND c/name/value = '4199'"
        );
        assert_eq!(Some(Part::Aql), short().found_in(&scoped(&aql)));
    }

    #[test]
    fn without_a_scope_the_ehr_id_is_read_like_any_text() {
        let aql =
            format!("SELECT c FROM EHR e CONTAINS COMPOSITION c WHERE e/ehr_id/value = '{EHR_ID}'");
        assert_eq!(
            Some(Part::Aql),
            short().found_in(&request(&aql, &QUERY_URL, &[]))
        );
    }

    #[test]
    fn an_identifier_equal_to_the_scope_ehr_id_fails_closed() {
        let aql =
            format!("SELECT c FROM EHR e CONTAINS COMPOSITION c WHERE e/ehr_id/value = '{EHR_ID}'");
        let equal = Withheld::new([SecretString::from(EHR_ID)]);
        assert_eq!(Some(Part::Aql), equal.found_in(&scoped(&aql)));
    }

    // conformance: CP-26
    #[test]
    fn the_authority_of_the_url_is_never_read() {
        let target = url("https://4199@cdr-4199.example.org:4199/v1/query/aql");
        assert_eq!(
            None,
            short().found_in(&request(
                "SELECT c FROM EHR e CONTAINS COMPOSITION c",
                &target,
                &[]
            ))
        );
    }

    // conformance: CP-26
    #[test]
    fn the_path_query_and_fragment_are_read_under_any_authority() {
        for text in [
            "https://cdr.example.org/openehr-4199/v1/query/aql",
            "https://cdr.example.org/v1/query/aql?ehr=4199",
            "https://cdr.example.org/v1/query/aql?ehr=%34199",
            "https://cdr.example.org/v1/query/aql#4199",
            "https://cdr.example.org:4199/v1/query/aql?ehr=4199",
        ] {
            let target = url(text);
            assert_eq!(
                Some(Part::Url),
                short().found_in(&request("SELECT 1", &target, &[])),
                "{text}"
            );
        }
    }

    #[test]
    fn a_value_across_the_path_and_the_query_is_found() {
        let target = url("https://cdr.example.org/v1/query/aql?x=1");
        let across = Withheld::new([SecretString::from("aql?x")]);
        assert_eq!(
            Some(Part::Url),
            across.found_in(&request("SELECT 1", &target, &[]))
        );
    }

    #[test]
    fn debug_counts_and_never_shows_a_value() {
        let shown = format!("{:?}", withheld());
        assert!(!shown.contains("Sentinel"), "{shown}");
        assert!(shown.contains('1'), "{shown}");
    }

    /// The operation `method` and `path` address, from `openehr-its`'s table.
    fn operation(method: &http::Method, path: &str) -> RouteMatch {
        match lookup(method, path) {
            Lookup::Matched(matched) => matched,
            other => panic!("{method} {path} names no operation: {other:?}"),
        }
    }

    fn composition_update() -> RouteMatch {
        operation(&http::Method::PUT, "/ehr/7d44/composition/u::s::1")
    }

    #[test]
    fn only_the_declared_client_headers_are_forwarded_byte_for_byte() {
        let mut client = http::HeaderMap::new();
        client.insert("authorization", "Bearer client-token".parse().unwrap());
        client.insert("x-request-id", "req-O'Sentinel-4711".parse().unwrap());
        client.insert("x-patient", SENTINEL.parse().unwrap());
        client.insert("if-match", "\"uid::cdr-a.example.org::1\"".parse().unwrap());
        client.append("openehr-audit-details", "change_type=249".parse().unwrap());
        client.append("openehr-audit-details", "committer=c-1".parse().unwrap());
        client.insert("openehr-federation-endpoint", "node-a-pub".parse().unwrap());
        let forwarded = super::forwarded_headers(&composition_update(), &client);
        assert_eq!(3, forwarded.len(), "{forwarded:?}");
        assert_eq!(
            Some("\"uid::cdr-a.example.org::1\""),
            forwarded.get("if-match").and_then(|v| v.to_str().ok())
        );
        assert_eq!(2, forwarded.get_all("openehr-audit-details").iter().count());
        assert!(forwarded.get("authorization").is_none());
        assert!(forwarded.get("x-request-id").is_none());
        assert!(forwarded.get("x-patient").is_none());
        assert!(forwarded.get("openehr-federation-endpoint").is_none());
    }

    // conformance: CP-28
    #[test]
    fn the_targeting_headers_are_withheld_from_every_operation() {
        let mut client = http::HeaderMap::new();
        client.insert("openEHR-federation-endpoint", "node-a-pub".parse().unwrap());
        client.insert("openEHR-Federation-Organisation", "org-a".parse().unwrap());
        client.insert("accept", "application/json".parse().unwrap());
        for operation in [
            composition_update(),
            operation(&http::Method::GET, "/ehr/7d44/composition/u::s::1"),
            operation(&http::Method::POST, "/ehr/7d44/composition"),
            operation(&http::Method::GET, "/ehr/7d44/ehr_status"),
        ] {
            let forwarded = super::forwarded_headers(&operation, &client);
            assert!(
                forwarded.get("openehr-federation-endpoint").is_none()
                    && forwarded.get("openehr-federation-organisation").is_none(),
                "§8.4: targeting means nothing at a node: {forwarded:?}"
            );
        }
        for name in [
            openehr_federation::headers::ENDPOINT,
            openehr_federation::headers::ORGANISATION,
        ] {
            assert!(super::is_withheld_header(name), "{name}");
        }
    }

    // conformance: CP-26
    #[test]
    fn every_federation_header_is_withheld_even_where_an_operation_declares_it() {
        let mut client = http::HeaderMap::new();
        for name in openehr_federation::headers::ALL {
            client.insert(name, SENTINEL.parse().unwrap());
        }
        client.insert("OPENEHR-FEDERATION-PATIENT", SENTINEL.parse().unwrap());
        client.insert("openehr-federation-", SENTINEL.parse().unwrap());
        client.insert("authorization", "Bearer client-token".parse().unwrap());
        client.insert("x-request-id", SENTINEL.parse().unwrap());
        client.insert("openehr-version", "1".parse().unwrap());
        client.insert("openehr-federationless", "kept".parse().unwrap());
        let declares_everything = |_: &str| true;
        let forwarded = super::admitted_headers(declares_everything, &client);
        let mut names: Vec<&str> = forwarded.keys().map(http::HeaderName::as_str).collect();
        names.sort_unstable();
        assert_eq!(
            vec!["openehr-federationless", "openehr-version"],
            names,
            "§5.4.1, N33: a federation header never reaches a node"
        );
        for name in openehr_federation::headers::ALL {
            assert!(
                name.starts_with(super::WITHHELD_HEADER_PREFIX),
                "{name} is outside the withheld family"
            );
        }
    }

    #[test]
    fn a_name_shorter_than_the_prefix_is_never_read_as_it() {
        assert!(!super::is_withheld_header("openehr-fed"));
        assert!(!super::is_withheld_header(""));
        assert!(super::is_withheld_header("Authorization"));
        assert!(super::is_withheld_header("X-Request-ID"));
    }

    #[test]
    fn a_header_one_operation_declares_is_stripped_from_one_that_does_not() {
        let mut client = http::HeaderMap::new();
        client.insert("if-match", "\"uid::cdr-a.example.org::1\"".parse().unwrap());
        client.insert("accept", "application/json".parse().unwrap());
        let update = super::forwarded_headers(&composition_update(), &client);
        assert!(update.get("if-match").is_some(), "{update:?}");
        let read = operation(&http::Method::GET, "/ehr/7d44/composition/u::s::1");
        let read = super::forwarded_headers(&read, &client);
        assert!(read.get("if-match").is_none(), "{read:?}");
        assert_eq!(
            Some("application/json"),
            read.get("accept").and_then(|v| v.to_str().ok())
        );
    }

    #[test]
    fn a_query_of_declared_parameters_is_forwarded_as_received() {
        let directory = operation(&http::Method::GET, "/ehr/7d44/directory");
        let query = "version_at_time=2026-01-01T00:00:00Z&path=a%2Fb&";
        assert_eq!(Ok(query), super::forwarded_query(&directory, query));
        assert_eq!(Ok(""), super::forwarded_query(&directory, ""));
        assert_eq!(
            Ok("version%5Fat%5Ftime=x"),
            super::forwarded_query(&directory, "version%5Fat%5Ftime=x")
        );
        let tags = operation(&http::Method::GET, "/ehr/7d44/tags");
        let query = "tag_key=k&&tag_value=v&tag_target_path=p";
        assert_eq!(Ok(query), super::forwarded_query(&tags, query));
    }

    #[test]
    fn a_parameter_another_operation_declares_is_refused() {
        let composition = operation(&http::Method::GET, "/ehr/7d44/composition/u::s::1");
        assert_eq!(
            Ok("version_at_time=x"),
            super::forwarded_query(&composition, "version_at_time=x")
        );
        assert_eq!(
            Err(super::UnlistedParameter { position: 2 }),
            super::forwarded_query(&composition, "version_at_time=x&path=a")
        );
    }

    #[test]
    fn an_undeclared_query_parameter_is_refused_by_position_never_by_name() {
        let directory = operation(&http::Method::GET, "/ehr/7d44/directory");
        let refused = super::forwarded_query(&directory, "path=a&patient=O%27Sentinel-4711");
        assert_eq!(Err(super::UnlistedParameter { position: 2 }), refused);
        let shown = refused.map_err(|e| e.to_string()).unwrap_err();
        assert!(
            !shown.contains("patient") && !shown.contains("Sentinel"),
            "{shown}"
        );
        assert_eq!(
            Err(super::UnlistedParameter { position: 1 }),
            super::forwarded_query(&directory, "subject_id=4711&subject_namespace=x")
        );
        assert_eq!(
            Err(super::UnlistedParameter { position: 1 }),
            super::forwarded_query(&directory, "O%27Sentinel-4711")
        );
    }

    #[test]
    fn the_subject_parameters_are_refused_even_where_declared() {
        let by_subject = operation(&http::Method::GET, "/ehr");
        assert!(by_subject.query_param("subject_id").is_some());
        assert_eq!(
            Err(super::UnlistedParameter { position: 1 }),
            super::forwarded_query(&by_subject, "subject_id=4711&subject_namespace=x")
        );
        assert_eq!(
            Err(super::UnlistedParameter { position: 1 }),
            super::forwarded_query(&by_subject, "subject%5Fnamespace=x")
        );
    }
}
