// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! The correlation id the gateway sends to a node, and the inventory of every
//! header a node request carries.
//!
//! A node is located by its `ehr_id` alone, and no directly identifying
//! identifier travels in a header the gateway composes (§5.4.1, N33). The
//! gateway cannot tell whether a client's free text names a patient, so no
//! header value toward a node comes from client input. The correlation id is
//! an [`OutboundId`], which only [`OutboundId::mint`] can make: a version 4
//! UUID with no client input, one per client request and the same for every
//! node that request reaches (CP-26).
//!
//! Every header of `POST {base}/v1/query/aql` to a node, and where its value
//! comes from:
//!
//! | Header | Value | Source |
//! |---|---|---|
//! | `Accept` | `application/json` | `openehr-its`'s client runtime, its default when the call sets none |
//! | `Content-Type` | `application/json` | `openehr-its`'s client runtime, for the JSON body |
//! | `Authorization` | `Basic` or `Bearer` | the endpoint's onward credential from the gateway's configuration, or the access token its OAuth 2.0 grant obtained ([`crate::onward`]), only when one is configured |
//! | `X-Request-Id` | a version 4 UUID | [`OutboundId::mint`], only when the caller passes one |
//! | `openEHR-federation-client` | a compact JWS | the caller's verified identity, or the gateway's own for its operator, signed for the node with the gateway's key ([`crate::onward::conveyance`]) |
//! | `Host`, `Content-Length` | the endpoint's authority, the body length | the HTTP engine, from the registry URL and the composed body |
//! | `Accept-Encoding` | the codings the engine decodes | the HTTP engine, from its compression features |
//!
//! No other header is set, and none is copied from the client request.
//!
//! Every header of a request routed to one node (`{base}/v1/ehr/{ehr_id}` and
//! below, §7a.1), and where its value comes from:
//!
//! | Header | Value | Source |
//! |---|---|---|
//! | `Accept`, `Content-Type`, `Prefer` | a value the operation lists | composed from the client request where the matched ITS-REST operation declares it: the best listed match of `Accept` (the first listed for `*/*` or none), the listed media type `Content-Type` names, the listed preferences of `Prefer` ([`crate::declared::held`]) |
//! | `If-Match`, `openehr-version`, `openehr-audit-details`, `openehr-template-id`, `openehr-item-tag`, `openehr-version-item-tag` | the client's, byte for byte | the client request, each only where the matched ITS-REST operation declares it in `openehr-its`'s parameter table, of the kind the table states for it ([`crate::declared::held`]), and through the outbound gate ([`crate::hygiene::forwarded_headers`]) |
//! | `Accept` | `application/json` | `openehr-its`'s client runtime, when the operation declares no `Accept` |
//! | `Authorization` | `Basic` or `Bearer` | the endpoint's onward credential, as above; the client's own is never forwarded |
//! | `X-Request-Id` | a version 4 UUID | [`OutboundId::mint`], one per client request; the client's own is never forwarded |
//! | `openEHR-federation-client` | a compact JWS | the caller's identity signed for the node, as above; the client's own is never forwarded |
//! | `Host`, `Content-Length` | the endpoint's authority, the body length | the HTTP engine, from the registry URL and the client's body |
//! | `Accept-Encoding` | the codings the engine decodes | the HTTP engine, from its compression features |
//!
//! Every other client header is stripped, the federation's own included.
//!
//! A stored-query definition sent to a node or read back from one
//! (`{base}/v1/definition/query/{name}/{version}`, §12.7) carries the headers
//! of `POST {base}/v1/query/aql` above, its `Content-Type` `text/plain` for the
//! AQL text of a `PUT`, and none from the client request
//! ([`crate::dispatch::definition`]).
//!
//! No specification governs the correlation header itself: our own design,
//! under the name every proxy already uses.
//!
//! The outbound gate does not search the minted id for a withheld identifier
//! ([`crate::hygiene`]): it carries no client input, and a short all-hex
//! identifier can occur in a random UUID by chance.

use std::fmt;

use uuid::Uuid;

/// The correlation id one client request carries to every node it reaches.
///
/// It is minted by the gateway and never parsed or built from text, so no
/// client value can become one. `Display` writes the hyphenated UUID, which
/// is always a legal header value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OutboundId(Uuid);

impl OutboundId {
    /// Mints a fresh id: a random version 4 UUID.
    #[must_use]
    pub fn mint() -> Self {
        Self(Uuid::new_v4())
    }

    /// The id `uuid`, so a unit test can pin a value; production code only
    /// mints.
    #[cfg(test)]
    pub(crate) fn fixed(uuid: Uuid) -> Self {
        Self(uuid)
    }
}

impl fmt::Display for OutboundId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0.hyphenated(), f)
    }
}

#[cfg(test)]
mod tests {
    use super::OutboundId;
    use crate::dispatch::{
        DispatchError, DispatchOptions, NodeClient, NodeQuery, NodeReply, REQUEST_ID_HEADER,
    };
    use crate::hygiene::{Part, Withheld};
    use ferrofed_registry::snapshot::RegistrySnapshot;
    use ferrofed_testkit::mock::Server;
    use http::HeaderValue;
    use openehr_its::rest::client::ReqwestTransport;
    use secrecy::SecretString;
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    /// A fixed version 4 UUID, and a short all-hex value that occurs in it.
    ///
    /// The value holds letters, so no port in the mock node's URL can
    /// contain it.
    const FIXED_ID: &str = "7d44b88c-4199-4bad-97dc-d78268e01398";
    const IN_FIXED_ID: &str = "4bad";

    /// What a node answering every query with an empty result set made of
    /// `aql`, sent under [`FIXED_ID`] with [`IN_FIXED_ID`] withheld, and the
    /// raw `X-Request-Id` values it received.
    async fn sent(
        aql: &str,
    ) -> Result<(Result<NodeReply, DispatchError>, Vec<Vec<u8>>), Box<dyn std::error::Error>> {
        let server = Server::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/query/aql"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                br##"{"q":"node","columns":[{"name":"#0"}],"rows":[]}"##.to_vec(),
                "application/json",
            ))
            .mount(&server)
            .await;
        let snapshot = RegistrySnapshot::from_toml_str(&format!(
            "[[organisation]]\nid = \"org-a\"\n\n[[node]]\nid = \"node-a\"\norganisation = \"org-a\"\nsystem_id = \"cdr-a.example.org\"\n\n[[endpoint]]\nid = \"node-a-pub\"\nnode = \"node-a\"\nurl = \"{}\"\nconnection_type = \"openehr-rest-query\"\nmanaging_organisation = \"org-a\"\n",
            server.uri()
        ))?;
        let endpoint = snapshot.endpoints().next().ok_or("one endpoint")?;
        let client = NodeClient::new(
            endpoint,
            ReqwestTransport::with_timeout(Duration::from_secs(5))?,
        )?;
        let deadline = Instant::now()
            .checked_add(Duration::from_secs(5))
            .ok_or("the deadline is past the platform clock")?;
        let options =
            DispatchOptions::new(deadline, crate::onward::conveyance::tests::conveyance())
                .with_withheld(Arc::new(Withheld::new([SecretString::from(IN_FIXED_ID)])))
                .with_request_id(OutboundId::fixed(uuid::Uuid::parse_str(FIXED_ID)?));
        let reply = client.query(&NodeQuery::new(aql), &options).await;
        let requests = server.received_requests().await.ok_or("recording is on")?;
        let ids = requests
            .iter()
            .flat_map(|request| request.headers.get_all(REQUEST_ID_HEADER))
            .map(|value| value.as_bytes().to_vec())
            .collect();
        Ok((reply, ids))
    }

    // conformance: CP-26
    #[tokio::test]
    #[expect(
        clippy::panic_in_result_fn,
        reason = "a test asserts, and returns its setup errors"
    )]
    async fn a_withheld_value_inside_the_minted_id_is_sent_with_that_id() -> TestResult {
        assert!(FIXED_ID.contains(IN_FIXED_ID), "the value is in the id");
        let (reply, ids) = sent("SELECT c/uid/value FROM EHR e CONTAINS COMPOSITION c").await?;
        assert!(reply.is_ok(), "the gate refused a clean request: {reply:?}");
        assert_eq!(vec![FIXED_ID.as_bytes().to_vec()], ids);
        Ok(())
    }

    // conformance: CP-26
    #[tokio::test]
    #[expect(
        clippy::panic_in_result_fn,
        reason = "a test asserts, and returns its setup errors"
    )]
    async fn the_same_value_in_the_aql_is_still_never_sent() -> TestResult {
        let aql = format!(
            "SELECT c/uid/value FROM EHR e CONTAINS COMPOSITION c WHERE c/name/value = '{IN_FIXED_ID}'"
        );
        let (reply, ids) = sent(&aql).await?;
        assert!(
            matches!(
                reply,
                Err(DispatchError::Withheld {
                    part: Part::Aql,
                    ..
                })
            ),
            "{reply:?}"
        );
        assert!(ids.is_empty(), "nothing reached the node");
        Ok(())
    }

    #[test]
    fn a_minted_id_is_a_fresh_hyphenated_v4_uuid_and_a_legal_header_value() {
        let first = OutboundId::mint();
        let second = OutboundId::mint();
        assert_ne!(first, second, "every request gets its own id");
        let text = first.to_string();
        let parsed = uuid::Uuid::parse_str(&text).expect("a minted id is a UUID");
        assert_eq!(Some(uuid::Version::Random), parsed.get_version());
        assert_eq!(36, text.len(), "the hyphenated form: {text}");
        assert!(HeaderValue::from_str(&text).is_ok());
    }
}
