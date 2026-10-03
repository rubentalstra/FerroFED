// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! Directory content: the `Organization` and `Endpoint` resources of one
//! Bundle, with the references between them resolved inside it.

use std::collections::BTreeSet;
use std::fmt;

use fhir_types::codec::{Json, Path, Value};
use fhir_types::r4::bundle::Bundle;
use fhir_types::r4::endpoint::Endpoint;
use fhir_types::r4::extension::ExtensionValue;
use fhir_types::r4::identifier::Identifier;
use fhir_types::r4::organization::Organization;
use fhir_types::r4::reference::Reference;
use fhir_types::r4::resource::Resource;

use super::error::DirectoryError;
use crate::redact::RedactedUrl;

/// The `Organization` and `Endpoint` resources of one Bundle, in entry order.
///
/// The default is the content of an empty Bundle.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Directory {
    organizations: Vec<DirectoryOrganization>,
    endpoints: Vec<DirectoryEndpoint>,
}

/// One `Organization` of the directory.
///
/// `Debug` shows the `fullUrl` with its userinfo and query replaced by `***`,
/// and the resource by its logical id, name and `active` flag.
#[derive(Clone, PartialEq, Eq)]
pub struct DirectoryOrganization {
    entry: usize,
    full_url: Option<String>,
    resource: Organization,
}

/// One `Endpoint` of the directory.
///
/// `Debug` shows the `fullUrl` and the `address` with their userinfo and
/// query replaced by `***`, and leaves out the rest of the resource, whose
/// `header` list may hold a credential.
#[derive(Clone, PartialEq, Eq)]
pub struct DirectoryEndpoint {
    entry: usize,
    full_url: Option<String>,
    resource: Endpoint,
}

/// What a reference from one directory resource to another answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution<'a, T> {
    /// The resource the reference names, held in the Bundle.
    Found(&'a T),
    /// The literal reference names no resource the Bundle holds, or is
    /// relative to an entry whose `fullUrl` is not a REST URL, which leaves it
    /// without a meaning inside the Bundle.
    Outside(&'a str),
    /// The reference carries no literal `reference` (only an identifier or a
    /// display), so no entry of the Bundle can answer it.
    NotLiteral,
}

impl Directory {
    /// Reads directory content from a FHIR R4 JSON `Bundle` of type
    /// `collection` or `searchset` whose every entry is an `Organization` or an
    /// `Endpoint`.
    ///
    /// # Errors
    ///
    /// A [`DirectoryError`] when the text is not JSON, not a Bundle, does not
    /// decode as FHIR R4, has another Bundle type, holds an entry with no
    /// resource or with another resource, carries a `modifierExtension`
    /// (FHIR R4 §2.5.0.2.1: a resource with a modifier the reader does not
    /// understand is refused), or repeats a `fullUrl` or a logical id.
    pub fn from_json(body: &[u8]) -> Result<Self, DirectoryError> {
        let value: Value =
            serde_json::from_slice(body).map_err(|error| DirectoryError::NotJson {
                line: error.line(),
                column: error.column(),
            })?;
        let object = value.as_object().ok_or(DirectoryError::NotABundle)?;
        if object.get("resourceType").and_then(Value::as_str) != Some("Bundle") {
            return Err(DirectoryError::NotABundle);
        }
        let bundle =
            Bundle::from_json(object, &mut Path::root("Bundle")).map_err(DirectoryError::Decode)?;
        let kind = bundle.r#type.value.as_deref();
        if !matches!(kind, Some("collection" | "searchset")) {
            return Err(DirectoryError::BundleType {
                found: kind.map(str::to_owned),
            });
        }
        let mut builder = Builder::default();
        for (index, entry) in bundle.entry.into_iter().enumerate() {
            let full_url = entry
                .full_url
                .and_then(|full_url| full_url.value)
                .filter(|full_url| !full_url.is_empty());
            match entry.resource {
                Some(Resource::Organization(resource)) => {
                    builder.organization(index, full_url, *resource)?;
                }
                Some(Resource::Endpoint(resource)) => {
                    builder.endpoint(index, full_url, *resource)?;
                }
                Some(_) => return Err(DirectoryError::UnexpectedEntry { index }),
                None => return Err(DirectoryError::NoResource { index }),
            }
        }
        Ok(builder.directory)
    }

    /// Builds directory content from resources held elsewhere, each with its
    /// `fullUrl`: the organisations first, then the endpoints, each in the
    /// order given, numbered as one Bundle's entries would be.
    ///
    /// # Errors
    /// A [`DirectoryError`] for a resource that carries a `modifierExtension`
    /// or repeats a `fullUrl` or a logical id, as [`Directory::from_json`]
    /// refuses them.
    pub(crate) fn from_resources(
        organizations: impl IntoIterator<Item = (String, Organization)>,
        endpoints: impl IntoIterator<Item = (String, Endpoint)>,
    ) -> Result<Self, DirectoryError> {
        let mut builder = Builder::default();
        let mut index = 0_usize;
        for (full_url, resource) in organizations {
            builder.organization(index, Some(full_url), resource)?;
            index = index.saturating_add(1);
        }
        for (full_url, resource) in endpoints {
            builder.endpoint(index, Some(full_url), resource)?;
            index = index.saturating_add(1);
        }
        Ok(builder.directory)
    }

    /// Every `Organization`, in entry order.
    #[must_use]
    pub fn organizations(&self) -> &[DirectoryOrganization] {
        &self.organizations
    }

    /// Every `Endpoint`, in entry order.
    #[must_use]
    pub fn endpoints(&self) -> &[DirectoryEndpoint] {
        &self.endpoints
    }

    /// The organisation an endpoint's `managingOrganization` names, or `None`
    /// when the endpoint carries none.
    #[must_use]
    pub fn managing_organization<'a>(
        &'a self,
        endpoint: &'a DirectoryEndpoint,
    ) -> Option<Resolution<'a, DirectoryOrganization>> {
        let reference = endpoint.resource.managing_organization.as_ref()?;
        Some(resolve(reference, endpoint, &self.organizations))
    }

    /// What each reference of an organisation's `endpoint` list names, in
    /// list order.
    pub fn endpoints_of<'a>(
        &'a self,
        organization: &'a DirectoryOrganization,
    ) -> impl Iterator<Item = Resolution<'a, DirectoryEndpoint>> {
        organization
            .resource
            .endpoint
            .iter()
            .map(move |reference| resolve(reference, organization, &self.endpoints))
    }
}

/// Directory content as it is read, entry by entry, with the checks every
/// entry passes.
#[derive(Default)]
struct Builder {
    directory: Directory,
    full_urls: BTreeSet<String>,
    organization_ids: BTreeSet<String>,
    endpoint_ids: BTreeSet<String>,
}

impl Builder {
    /// Adds the `Organization` of entry `index`.
    fn organization(
        &mut self,
        index: usize,
        full_url: Option<String>,
        resource: Organization,
    ) -> Result<(), DirectoryError> {
        self.full_url(index, full_url.as_ref())?;
        if !resource.modifier_extension.is_empty() {
            return Err(DirectoryError::ModifierExtension { index });
        }
        if let Some(id) = &resource.id
            && !self.organization_ids.insert(id.clone())
        {
            return Err(DirectoryError::DuplicateId { index });
        }
        self.directory.organizations.push(DirectoryOrganization {
            entry: index,
            full_url,
            resource,
        });
        Ok(())
    }

    /// Adds the `Endpoint` of entry `index`.
    fn endpoint(
        &mut self,
        index: usize,
        full_url: Option<String>,
        resource: Endpoint,
    ) -> Result<(), DirectoryError> {
        self.full_url(index, full_url.as_ref())?;
        if !resource.modifier_extension.is_empty() {
            return Err(DirectoryError::ModifierExtension { index });
        }
        if let Some(id) = &resource.id
            && !self.endpoint_ids.insert(id.clone())
        {
            return Err(DirectoryError::DuplicateId { index });
        }
        self.directory.endpoints.push(DirectoryEndpoint {
            entry: index,
            full_url,
            resource,
        });
        Ok(())
    }

    /// Refuses a `fullUrl` an earlier entry carries.
    fn full_url(&mut self, index: usize, full_url: Option<&String>) -> Result<(), DirectoryError> {
        match full_url {
            Some(full_url) if !self.full_urls.insert(full_url.clone()) => {
                Err(DirectoryError::DuplicateFullUrl { index })
            }
            _ => Ok(()),
        }
    }
}

impl DirectoryOrganization {
    /// The entry's position in the Bundle.
    #[must_use]
    pub fn entry(&self) -> usize {
        self.entry
    }

    /// The entry's `fullUrl`.
    #[must_use]
    pub fn full_url(&self) -> Option<&str> {
        self.full_url.as_deref()
    }

    /// The resource's logical id.
    #[must_use]
    pub fn logical_id(&self) -> Option<&str> {
        self.resource.id.as_deref()
    }

    /// The value of each identifier in `system`, in resource order; `None`
    /// for one that carries the system and no value.
    pub fn identifier_values<'a>(
        &'a self,
        system: &'a str,
    ) -> impl Iterator<Item = Option<&'a str>> {
        values(&self.resource.identifier, system)
    }

    /// The organisation's `name`.
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        self.resource.name.as_ref()?.value.as_deref()
    }

    /// The organisation's `active` flag, `None` when it does not say.
    #[must_use]
    pub fn active(&self) -> Option<bool> {
        self.resource.active.as_ref()?.value
    }

    /// The resource as decoded.
    #[must_use]
    pub fn resource(&self) -> &Organization {
        &self.resource
    }
}

impl DirectoryEndpoint {
    /// The entry's position in the Bundle.
    #[must_use]
    pub fn entry(&self) -> usize {
        self.entry
    }

    /// The entry's `fullUrl`.
    #[must_use]
    pub fn full_url(&self) -> Option<&str> {
        self.full_url.as_deref()
    }

    /// The resource's logical id.
    #[must_use]
    pub fn logical_id(&self) -> Option<&str> {
        self.resource.id.as_deref()
    }

    /// The value of each identifier in `system`, in resource order; `None`
    /// for one that carries the system and no value.
    pub fn identifier_values<'a>(
        &'a self,
        system: &'a str,
    ) -> impl Iterator<Item = Option<&'a str>> {
        values(&self.resource.identifier, system)
    }

    /// The endpoint's `status` code.
    #[must_use]
    pub fn status(&self) -> Option<&str> {
        self.resource.status.value.as_deref()
    }

    /// The `system` of the endpoint's `connectionType`.
    #[must_use]
    pub fn connection_type_system(&self) -> Option<&str> {
        self.resource
            .connection_type
            .system
            .as_ref()?
            .value
            .as_deref()
    }

    /// The `code` of the endpoint's `connectionType`.
    #[must_use]
    pub fn connection_type_code(&self) -> Option<&str> {
        self.resource
            .connection_type
            .code
            .as_ref()?
            .value
            .as_deref()
    }

    /// The `valueCode` of each extension of the endpoint whose `url` is
    /// `url`, in resource order; `None` for one that carries another value
    /// type or no value.
    pub fn extension_codes<'a>(&'a self, url: &'a str) -> impl Iterator<Item = Option<&'a str>> {
        self.resource
            .extension
            .iter()
            .filter(move |extension| extension.url == url)
            .map(|extension| match &extension.value {
                Some(ExtensionValue::Code(code)) => code.value.as_deref(),
                _ => None,
            })
    }

    /// The endpoint's `address`, its technical base address.
    #[must_use]
    pub fn address(&self) -> Option<&str> {
        self.resource.address.value.as_deref()
    }

    /// The resource as decoded.
    #[must_use]
    pub fn resource(&self) -> &Endpoint {
        &self.resource
    }
}

impl fmt::Debug for DirectoryOrganization {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DirectoryOrganization")
            .field("entry", &self.entry)
            .field("full_url", &self.full_url.as_deref().map(RedactedUrl))
            .field("logical_id", &self.logical_id())
            .field("name", &self.name())
            .field("active", &self.active())
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for DirectoryEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DirectoryEndpoint")
            .field("entry", &self.entry)
            .field("full_url", &self.full_url.as_deref().map(RedactedUrl))
            .field("logical_id", &self.logical_id())
            .field("status", &self.status())
            .field("connection_type_system", &self.connection_type_system())
            .field("connection_type_code", &self.connection_type_code())
            .field("address", &self.address().map(RedactedUrl))
            .finish_non_exhaustive()
    }
}

/// The resource among `held` that `reference`, made by `referrer`, names.
// NOTE: FHIR R4 §2.36.4.1: a relative `[type]/[id]` is made absolute on the
// root of the referrer's RESTful fullUrl, and an absolute one matches a fullUrl.
fn resolve<'a, T: Held>(
    reference: &'a Reference,
    referrer: &impl Held,
    held: &'a [T],
) -> Resolution<'a, T> {
    let Some(literal) = reference
        .reference
        .as_ref()
        .and_then(|reference| reference.value.as_deref())
    else {
        return Resolution::NotLiteral;
    };
    let absolute = if literal.contains(':') {
        Some(literal.to_owned())
    } else if is_relative(literal) {
        referrer
            .full_url()
            .and_then(|full_url| restful_root(full_url, referrer.kind()))
            .map(|root| format!("{root}/{literal}"))
    } else {
        None
    };
    absolute
        .and_then(|absolute| {
            held.iter()
                .find(|resource| resource.full_url() == Some(absolute.as_str()))
        })
        .map_or(Resolution::Outside(literal), Resolution::Found)
}

/// Whether `literal` has the `[type]/[id]` form of a relative reference.
fn is_relative(literal: &str) -> bool {
    literal
        .split_once('/')
        .is_some_and(|(kind, id)| !kind.is_empty() && !id.is_empty() && !id.contains('/'))
}

/// The `[root]` of a REST fullUrl `[root]/[type]/[id]` naming a resource of
/// type `kind` on `http` or `https` (FHIR R4 §2.3.0.2).
fn restful_root<'a>(full_url: &'a str, kind: &str) -> Option<&'a str> {
    let (rest, id) = full_url.rsplit_once('/')?;
    let (root, found) = rest.rsplit_once('/')?;
    let restful = found == kind
        && !id.is_empty()
        && (root.starts_with("https://") || root.starts_with("http://"));
    restful.then_some(root)
}

/// The two kinds of directory resource a reference is made by or names.
trait Held {
    fn full_url(&self) -> Option<&str>;
    fn kind(&self) -> &'static str;
}

impl Held for DirectoryOrganization {
    fn full_url(&self) -> Option<&str> {
        self.full_url.as_deref()
    }

    fn kind(&self) -> &'static str {
        "Organization"
    }
}

impl Held for DirectoryEndpoint {
    fn full_url(&self) -> Option<&str> {
        self.full_url.as_deref()
    }

    fn kind(&self) -> &'static str {
        "Endpoint"
    }
}

fn values<'a>(
    identifiers: &'a [Identifier],
    system: &'a str,
) -> impl Iterator<Item = Option<&'a str>> {
    identifiers
        .iter()
        .filter(move |identifier| {
            identifier
                .system
                .as_ref()
                .and_then(|uri| uri.value.as_deref())
                == Some(system)
        })
        .map(|identifier| identifier.value.as_ref()?.value.as_deref())
}
