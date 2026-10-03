// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! mCSD: the directory content reader, the ITI-90 and ITI-91 client against a
//! stub directory, the client held to the vendored capability statements and
//! profiles, and the replica kept in step with ITI-91.

mod client;
mod contract;
mod directory;
mod replica;

use std::fmt::Write;
use std::path::PathBuf;
use std::time::Duration;

use ihe_iti::mcsd::client::McsdClient;
use url::Url;
use wiremock::MockServer;

/// The FHIR base the stub directory serves under.
pub(crate) const BASE: &str = "/fhir/";

/// The timeout of a request the stub answers at once.
pub(crate) const PROMPT: Duration = Duration::from_secs(5);

/// The FHIR JSON media type.
pub(crate) const FHIR_JSON: &str = "application/fhir+json";

/// The synthetic identifier system the scoped tests select organisations by,
/// under the example arc.
pub(crate) const ORG_SYSTEM: &str = "urn:oid:2.999.10";

/// The synthetic identifier system the scoped tests select endpoints by.
pub(crate) const ENDPOINT_SYSTEM: &str = "urn:oid:2.999.11";

/// A vendored file of the mCSD package, by its path under `package/`.
pub(crate) fn vendored(file: &str) -> String {
    let path: PathBuf = [
        env!("CARGO_MANIFEST_DIR"),
        "../../docs/specs/ihe-mcsd/package",
        file,
    ]
    .iter()
    .collect();
    std::fs::read_to_string(&path).expect("the vendored mCSD package (scripts/vendor/ihe-mcsd.sh)")
}

fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("an HTTP client")
}

/// A client for `server`, built the way the module documentation asks: no
/// redirects.
pub(crate) fn client(server: &MockServer) -> McsdClient {
    let base = Url::parse(&format!("{}{BASE}", server.uri())).expect("the stub base");
    McsdClient::new(base, http()).expect("a client")
}

/// The `fullUrl` of the resource `kind/id` on `server`.
pub(crate) fn full_url(server: &MockServer, kind: &str, id: &str) -> String {
    format!("{}{BASE}{kind}/{id}", server.uri())
}

/// An `Organization` with `id`, carrying one identifier in `system`, listing
/// the endpoints `endpoints`.
pub(crate) fn organization(id: &str, system: &str, endpoints: &[&str]) -> String {
    let mut resource = format!(
        r#"{{"resourceType":"Organization","id":"{id}","identifier":[{{"system":"{system}","value":"{id}"}}],"name":"Organisation {id}""#
    );
    if !endpoints.is_empty() {
        let references: Vec<String> = endpoints
            .iter()
            .map(|endpoint| format!(r#"{{"reference":"Endpoint/{endpoint}"}}"#))
            .collect();
        write!(resource, r#","endpoint":[{}]"#, references.join(","))
            .expect("a String takes any text");
    }
    resource.push('}');
    resource
}

/// An active `Endpoint` with `id` at `address`, carrying one identifier in
/// `system`, managed by `manager`.
pub(crate) fn endpoint(id: &str, system: &str, manager: &str, address: &str) -> String {
    format!(
        r#"{{"resourceType":"Endpoint","id":"{id}","identifier":[{{"system":"{system}","value":"{id}"}}],"status":"active","connectionType":{{"system":"https://example.org/connection-type","code":"example"}},"managingOrganization":{{"reference":"Organization/{manager}"}},"payloadType":[{{"text":"example"}}],"address":"{address}"}}"#
    )
}

/// A Bundle of `kind` (`searchset` or `history`) with the JSON `entries` and
/// the JSON `links`; an empty list is left out, as FHIR JSON requires
/// (<http://hl7.org/fhir/R4/json.html#arrays>).
pub(crate) fn bundle(kind: &str, entries: &[String], links: &[String]) -> String {
    let mut bundle = format!(r#"{{"resourceType":"Bundle","type":"{kind}""#);
    if kind == "searchset" {
        write!(bundle, r#","total":{}"#, entries.len()).expect("a String takes any text");
    }
    if !links.is_empty() {
        write!(bundle, r#","link":[{}]"#, links.join(",")).expect("a String takes any text");
    }
    if !entries.is_empty() {
        write!(bundle, r#","entry":[{}]"#, entries.join(",")).expect("a String takes any text");
    }
    bundle.push('}');
    bundle
}

/// A search entry holding `resource` at `full_url`.
pub(crate) fn matched(full_url: &str, resource: &str) -> String {
    format!(r#"{{"fullUrl":"{full_url}","resource":{resource},"search":{{"mode":"match"}}}}"#)
}

/// A history entry recording `method` of `resource` at `full_url`.
pub(crate) fn version(full_url: &str, method: &str, url: &str, resource: &str) -> String {
    format!(
        r#"{{"fullUrl":"{full_url}","resource":{resource},"request":{{"method":"{method}","url":"{url}"}},"response":{{"status":"200 OK"}}}}"#
    )
}

/// A history entry recording the deletion of the resource at `url`.
pub(crate) fn deletion(url: &str) -> String {
    format!(
        r#"{{"request":{{"method":"DELETE","url":"{url}"}},"response":{{"status":"204 No Content"}}}}"#
    )
}
