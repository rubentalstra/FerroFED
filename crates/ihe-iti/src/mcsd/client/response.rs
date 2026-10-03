// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! One page of an ITI-90 or ITI-91 answer: a `searchset` or a `history`
//! Bundle on success, an `OperationOutcome` on failure (ITI TF-2 §3.90.4.2,
//! §3.91.4.2; FHIR R4 search and history).

use fhir_types::codec::{Json, Object, Path, Value};
use fhir_types::r4::bundle::{Bundle, BundleEntry, BundleLink};
use fhir_types::r4::resource::Resource;
use http::StatusCode;
use url::Url;

use super::{CareResource, CareService, Change, Match, Version};
use crate::mcsd::budget::Budget;
use crate::mcsd::error::{Malformation, McsdError};
use crate::outcome;

/// One page the directory answered `200` with.
pub(super) struct Page {
    /// The body.
    pub(super) body: Vec<u8>,
    /// The `Date` of the answer.
    pub(super) date: Option<String>,
}

/// The matches of one `searchset` page and its `next` link.
pub(super) struct Searchset {
    pub(super) matches: Vec<Match>,
    /// How many entries the page held, outcomes included.
    pub(super) entries: usize,
    pub(super) next: Option<Url>,
}

/// The changes of one `history` page and its `next` link.
pub(super) struct History {
    pub(super) changes: Vec<Change>,
    pub(super) next: Option<Url>,
}

/// A transport failure, with the request URL removed: the URL may carry the
/// base's credentials.
pub(super) fn transport(error: reqwest::Error) -> McsdError {
    let error = error.without_url();
    if error.is_timeout() {
        McsdError::Timeout
    } else {
        McsdError::Transport(error)
    }
}

/// Reads the answer's body, spending its bytes from `budget` as they arrive.
pub(super) async fn body(
    mut response: reqwest::Response,
    budget: &mut Budget,
) -> Result<Vec<u8>, McsdError> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(transport)? {
        budget.bytes(chunk.len())?;
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// The page an answer with `status`, media type `media` and `body` holds.
pub(super) fn page(
    status: StatusCode,
    media: Option<&str>,
    body: Vec<u8>,
    date: Option<String>,
) -> Result<Page, McsdError> {
    if status != StatusCode::OK {
        return Err(McsdError::Rejected {
            status,
            issues: outcome::issues(media, &body),
        });
    }
    if !outcome::fhir_json(media) {
        return Err(Malformation::NotFhirJson.into());
    }
    Ok(Page { body, date })
}

/// The matches a `searchset` page of `kind` holds, and its `next` link,
/// resolved against `base`.
pub(super) fn searchset(
    body: &[u8],
    kind: CareService,
    base: &Url,
) -> Result<Searchset, Malformation> {
    let bundle = bundle(body, "searchset")?;
    let next = next(&bundle.link, base)?;
    let entries = bundle.entry.len();
    let mut matches = Vec::new();
    for (index, entry) in bundle.entry.into_iter().enumerate() {
        let full_url = full_url(&entry);
        let resource = match entry.resource {
            // NOTE: FHIR R4 search, search.mode `outcome`: an OperationOutcome
            // in a searchset tells about the search and is no match.
            Some(Resource::OperationOutcome(_)) => continue,
            Some(resource) => care_resource(index, resource, kind)?,
            None => return Err(Malformation::NoResource { index }),
        };
        if resource.logical_id().is_none() {
            return Err(Malformation::NoLogicalId { index });
        }
        let full_url = full_url.ok_or(Malformation::NoFullUrl { index })?;
        matches.push(Match { full_url, resource });
    }
    Ok(Searchset {
        matches,
        entries,
        next,
    })
}

/// The changes a `history` page of `kind` holds, newest first, and its `next`
/// link, resolved against `base`.
pub(super) fn history(body: &[u8], kind: CareService, base: &Url) -> Result<History, Malformation> {
    let bundle = bundle(body, "history")?;
    let next = next(&bundle.link, base)?;
    let mut changes = Vec::new();
    for (index, entry) in bundle.entry.into_iter().enumerate() {
        let full_url = full_url(&entry);
        let request = entry
            .request
            .as_ref()
            .ok_or(Malformation::NoRequest { index })?;
        let change = match request.method.value.as_deref() {
            Some("POST" | "PUT" | "PATCH") => {
                let resource = entry
                    .resource
                    .ok_or(Malformation::NoResource { index })
                    .and_then(|resource| care_resource(index, resource, kind))?;
                let logical_id = resource
                    .logical_id()
                    .ok_or(Malformation::NoLogicalId { index })?
                    .to_owned();
                let full_url = full_url.ok_or(Malformation::NoFullUrl { index })?;
                Change {
                    logical_id,
                    version: Version::Current { full_url, resource },
                }
            }
            Some("DELETE") => {
                let logical_id = request
                    .url
                    .value
                    .as_deref()
                    .and_then(|url| deleted(url, kind))
                    .ok_or(Malformation::RequestUrl { index })?;
                Change {
                    logical_id,
                    version: Version::Deleted,
                }
            }
            _ => return Err(Malformation::Method { index }),
        };
        changes.push(change);
    }
    Ok(History { changes, next })
}

/// The Bundle of type `expected` the page holds.
fn bundle(body: &[u8], expected: &'static str) -> Result<Bundle, Malformation> {
    let value: Value = serde_json::from_slice(body).map_err(|error| Malformation::NotJson {
        line: error.line(),
        column: error.column(),
    })?;
    let object: &Object = value.as_object().ok_or(Malformation::NotAResource)?;
    match object.get("resourceType").and_then(Value::as_str) {
        Some("Bundle") => {}
        Some(_) => return Err(Malformation::UnexpectedResource),
        None => return Err(Malformation::NotAResource),
    }
    let bundle = Bundle::from_json(object, &mut Path::root("Bundle"))
        .map_err(|error| Malformation::Decode { kind: error.kind })?;
    if bundle.r#type.value.as_deref() != Some(expected) {
        return Err(Malformation::BundleType { expected });
    }
    Ok(bundle)
}

/// The entry's `fullUrl`, when it carries a non-empty one.
fn full_url(entry: &BundleEntry) -> Option<String> {
    entry
        .full_url
        .as_ref()
        .and_then(|full_url| full_url.value.clone())
        .filter(|full_url| !full_url.is_empty())
}

/// The resource of entry `index` as a [`CareResource`] of `kind`.
fn care_resource(
    index: usize,
    resource: Resource,
    kind: CareService,
) -> Result<CareResource, Malformation> {
    match (resource, kind) {
        (Resource::Organization(resource), CareService::Organization) => {
            Ok(CareResource::Organization(resource))
        }
        (Resource::Endpoint(resource), CareService::Endpoint) => {
            Ok(CareResource::Endpoint(resource))
        }
        _ => Err(Malformation::UnexpectedEntry { index }),
    }
}

/// The logical id a deletion's `request.url` names, `[type]/[id]` or
/// `[type]/[id]/_history/[vid]` under any base, when its type is `kind`.
fn deleted(url: &str, kind: CareService) -> Option<String> {
    let path = url.split(['?', '#']).next()?;
    let segments: Vec<&str> = path.split('/').collect();
    let at = segments
        .iter()
        .rposition(|segment| *segment == kind.resource_type())?;
    match segments.get(at.saturating_add(1)..)? {
        [id] | [id, "_history", _] if !id.is_empty() => Some((*id).to_owned()),
        _ => None,
    }
}

/// The `next` link, resolved against the FHIR base (FHIR R4 paging,
/// <http://hl7.org/fhir/R4/http.html#paging>).
fn next(links: &[BundleLink], base: &Url) -> Result<Option<Url>, Malformation> {
    let Some(link) = links
        .iter()
        .find(|link| link.relation.value.as_deref() == Some("next"))
    else {
        return Ok(None);
    };
    let url = link.url.value.as_deref().ok_or(Malformation::NextLink)?;
    base.join(url)
        .map(Some)
        .map_err(|_unparsable| Malformation::NextLink)
}

#[cfg(test)]
mod tests {
    use super::deleted;
    use crate::mcsd::client::CareService;

    #[test]
    fn a_deletion_names_its_resource_relative_absolute_or_versioned() {
        for url in [
            "Endpoint/ep-a",
            "Endpoint/ep-a/_history/3",
            "https://directory.example.org/fhir/Endpoint/ep-a",
            "Endpoint/ep-a?_format=json",
        ] {
            assert_eq!(
                deleted(url, CareService::Endpoint).as_deref(),
                Some("ep-a"),
                "{url}"
            );
        }
        for url in [
            "Organization/ep-a",
            "Endpoint",
            "Endpoint/",
            "Endpoint/ep-a/extra",
        ] {
            assert_eq!(deleted(url, CareService::Endpoint), None, "{url}");
        }
    }
}
