// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! Why a Bundle does not read as directory content, and why an ITI-90 or
//! ITI-91 exchange did not end in any.

use fhir_types::codec::DecodeError;

use crate::outcome::IssueType;

/// A Bundle that does not read as `Organization` and `Endpoint` resources.
///
/// An entry is named by its position in the Bundle's `entry` list.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum DirectoryError {
    /// The text is not JSON.
    #[error("the directory content is not JSON (line {line}, column {column})")]
    NotJson {
        /// The line of the syntax error.
        line: usize,
        /// The column of the syntax error.
        column: usize,
    },
    /// The JSON is not a FHIR resource of type `Bundle`.
    #[error("the directory content is not a FHIR Bundle")]
    NotABundle,
    /// The Bundle does not decode as FHIR R4.
    #[error("the Bundle does not decode as FHIR R4")]
    Decode(#[source] DecodeError),
    /// The Bundle's `type` is neither `collection` nor `searchset`.
    #[error("the Bundle's type {found:?} is neither collection nor searchset")]
    BundleType {
        /// The `type` the Bundle carries.
        found: Option<String>,
    },
    /// An entry has no `resource`.
    #[error("entry {index} has no resource")]
    NoResource {
        /// The entry's position.
        index: usize,
    },
    /// An entry holds a resource other than an `Organization` or an
    /// `Endpoint`.
    #[error("entry {index} holds a resource that is neither an Organization nor an Endpoint")]
    UnexpectedEntry {
        /// The entry's position.
        index: usize,
    },
    /// An entry's resource carries a `modifierExtension`, which changes its
    /// meaning in a way this reader does not know.
    #[error("entry {index} carries a modifierExtension")]
    ModifierExtension {
        /// The entry's position.
        index: usize,
    },
    /// Two entries share a `fullUrl`.
    #[error("entry {index} repeats the fullUrl of an earlier entry")]
    DuplicateFullUrl {
        /// The position of the later entry.
        index: usize,
    },
    /// Two resources of one type share a logical id.
    #[error("entry {index} repeats the logical id of an earlier resource of its type")]
    DuplicateId {
        /// The position of the later entry.
        index: usize,
    },
}

/// A directory base URL the client refuses: not an `http` or `https` URL
/// without a query or a fragment (FHIR R4 HTTP, Service Base URL,
/// <http://hl7.org/fhir/R4/http.html#root>).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the directory base URL is not an http(s) URL without a query or a fragment")]
pub struct InvalidBase;

/// Why an ITI-90 or ITI-91 exchange did not end in directory content.
///
/// No error carries a request URL, a page link or text the directory wrote:
/// a base URL may hold a credential, and an `OperationOutcome`'s free text is
/// the directory's own, so only its issue codes are kept.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum McsdError {
    /// The directory answered a status other than `200`, with the issue types
    /// its `OperationOutcome` carried, if it sent one.
    #[error("the directory answered {status}")]
    Rejected {
        /// The HTTP status.
        status: http::StatusCode,
        /// The issue types of the `OperationOutcome`, in order.
        issues: Vec<IssueType>,
    },
    /// A page link points away from the directory the client is bound to, so
    /// the client does not follow it.
    #[error("the page link points away from the directory")]
    ForeignPage,
    /// The answer runs to more pages than the client reads.
    #[error("the answer runs to more than {limit} pages")]
    TooManyPages {
        /// The limit, in pages.
        limit: usize,
    },
    /// No answer arrived within the timeout.
    #[error("the directory did not answer within the timeout")]
    Timeout,
    /// The request could not be sent or the answer could not be read.
    #[error("the directory could not be reached")]
    Transport(#[source] reqwest::Error),
    /// The answer does not hold to ITI-90 or ITI-91.
    #[error("the directory's answer does not hold to mCSD")]
    Malformed(#[from] Malformation),
}

impl McsdError {
    /// The HTTP status the directory answered with, when the client refused
    /// that status.
    #[must_use]
    pub fn status(&self) -> Option<http::StatusCode> {
        match self {
            Self::Rejected { status, .. } => Some(*status),
            _ => None,
        }
    }

    /// Whether the directory answered at all: `false` for a timeout and for
    /// a request that could not be sent or whose answer could not be read.
    #[must_use]
    pub fn answered(&self) -> bool {
        !matches!(self, Self::Timeout | Self::Transport(_))
    }
}

/// How an answer departs from ITI-90 or ITI-91.
///
/// ITI-90 answers with a `searchset` Bundle and ITI-91 with a `history`
/// Bundle (FHIR R4 search and history,
/// <http://hl7.org/fhir/R4/http.html#search>,
/// <http://hl7.org/fhir/R4/http.html#history>). An entry is named by its position in the page's `entry` list, never by its
/// content.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum Malformation {
    /// The answer's media type is not FHIR JSON.
    #[error("the answer is not application/fhir+json")]
    NotFhirJson,
    /// A page is longer than the client reads.
    #[error("a page exceeds {limit} bytes")]
    TooLarge {
        /// The limit, in bytes.
        limit: usize,
    },
    /// The answer is not JSON.
    #[error("the answer is not JSON (line {line}, column {column})")]
    NotJson {
        /// The line of the syntax error.
        line: usize,
        /// The column of the syntax error.
        column: usize,
    },
    /// The answer is not a JSON object with a `resourceType`.
    #[error("the answer is not a FHIR resource")]
    NotAResource,
    /// The answer is a resource other than a Bundle.
    #[error("the answer is not a Bundle")]
    UnexpectedResource,
    /// The Bundle does not decode as FHIR R4.
    #[error("the Bundle does not decode as FHIR R4: {kind}")]
    Decode {
        /// Why it was refused.
        kind: fhir_types::codec::DecodeErrorKind,
    },
    /// The Bundle's `type` is not the one the interaction answers with.
    #[error("the Bundle is not of type {expected}")]
    BundleType {
        /// The type the interaction answers with.
        expected: &'static str,
    },
    /// A resource entry has no `fullUrl`.
    #[error("entry {index} has no fullUrl")]
    NoFullUrl {
        /// The entry's position.
        index: usize,
    },
    /// An entry that should hold a resource holds none.
    #[error("entry {index} has no resource")]
    NoResource {
        /// The entry's position.
        index: usize,
    },
    /// An entry holds a resource of another type than the one asked for.
    #[error("entry {index} holds a resource of another type than the one asked for")]
    UnexpectedEntry {
        /// The entry's position.
        index: usize,
    },
    /// A resource has no logical id.
    #[error("entry {index} holds a resource with no logical id")]
    NoLogicalId {
        /// The entry's position.
        index: usize,
    },
    /// A history entry has no `request` (FHIR R4 Bundle invariant `bdl-3`).
    #[error("history entry {index} has no request")]
    NoRequest {
        /// The entry's position.
        index: usize,
    },
    /// A history entry's `request.method` is none of `POST`, `PUT`, `PATCH`
    /// and `DELETE`.
    #[error("history entry {index} has a method a history does not record")]
    Method {
        /// The entry's position.
        index: usize,
    },
    /// A deletion's `request.url` does not name a resource of the type asked
    /// for as `[type]/[id]`.
    #[error("history entry {index} deletes no resource of the type asked for")]
    RequestUrl {
        /// The entry's position.
        index: usize,
    },
    /// The `next` link is not a URL.
    #[error("the next link is not a URL")]
    NextLink,
}
