// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! mCSD, Mobile Care Services Discovery (feature `mcsd`).
//!
//! A care services directory publishes `Organization` and `Endpoint`
//! resources (mCSD 4.0.0, ITI-90). [`directory::Directory`] reads them from one
//! FHIR R4 `Bundle`, as a directory search answers or a reviewed document
//! carries them, and resolves the references between them inside the Bundle.
//! Each resource stays as `fhir-types` decodes it; what a caller makes of a
//! connection type, a status or an identifier is the caller's policy.
//!
//! [`client::McsdClient`] asks a directory over HTTP: ITI-90, Find Matching
//! Care Services, and ITI-91, Request Care Services Updates.
//! [`replica::Replica`] holds a directory's resources read with the first and
//! keeps them in step with the second, and answers them as a
//! [`directory::Directory`].
//!
//! ```
//! use ihe_iti::mcsd::directory::{Directory, Resolution};
//!
//! let directory = Directory::from_json(br#"{
//!   "resourceType": "Bundle",
//!   "type": "collection",
//!   "entry": [
//!     {
//!       "fullUrl": "https://directory.example.org/fhir/Organization/org-a",
//!       "resource": {"resourceType": "Organization", "id": "org-a", "name": "Hospital A"}
//!     },
//!     {"fullUrl": "https://directory.example.org/fhir/Endpoint/ep-a", "resource": {
//!       "resourceType": "Endpoint",
//!       "id": "ep-a",
//!       "status": "active",
//!       "connectionType": {"system": "https://example.org/connection-type", "code": "example"},
//!       "managingOrganization": {"reference": "Organization/org-a"},
//!       "payloadType": [{"text": "example"}],
//!       "address": "https://cdr-a.example.org/openehr"
//!     }}
//!   ]
//! }"#)?;
//!
//! let endpoint = directory.endpoints().first().ok_or("one endpoint")?;
//! assert_eq!(endpoint.address(), Some("https://cdr-a.example.org/openehr"));
//! let manager = match directory.managing_organization(endpoint) {
//!     Some(Resolution::Found(manager)) => manager,
//!     _ => return Err("the managing organisation is in the Bundle".into()),
//! };
//! assert_eq!(manager.name(), Some("Hospital A"));
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

pub mod budget;
pub mod client;
pub mod directory;
pub mod error;
pub mod replica;
