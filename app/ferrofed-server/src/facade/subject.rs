// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! `GET {base}/v1/ehr?subject_id=…&subject_namespace=…`: the EHR of a
//! subject, answered by the one member that holds it (ITS-REST 1.1.0 EHR
//! API, `ehr_get_by_subject`).
//!
//! The two query parameters name the patient, which N33 forbids the gateway
//! to dispatch, so they are resolution input (§5.2, N3). The gateway resolves
//! the subject at the members through the cross-reference service and sends
//! the member that knows it `GET {base}/v1/ehr/{ehr_id}` under its own local
//! `ehr_id`. That request carries no query string, no body and only the
//! headers its operation declares, and the outbound gate holds it to the
//! subject value as well (§5.4.1, N33). The answer passes through as the node
//! sent it, naming the acting endpoint (N31, §9.6).
//!
//! The targeting headers select the one endpoint the subject is resolved at
//! (§8.4, §7a.1); without them, every member with an active endpoint is a
//! candidate, narrowed by the localizer where one is configured: the read
//! names the patient and no node, so it is undirected (N4, §14.1). The
//! consent pre-filter is asked about the candidates first,
//! as a federated query asks it: a member it denies is never resolved or
//! contacted, and one it does not deny is left to its node (N27a, N27). The resolution settles the answer:
//!
//! - one member knows the subject: its EHR, forwarded once;
//! - several members know it: `409` listing their endpoints, because the
//!   gateway never chooses by where the patient resolved (§12.5.2);
//! - the cross-reference could not answer for a member: `424`, because that
//!   member may hold the EHR too (§11.2, §11.5);
//! - the consent pre-filter denied every member that might hold it, and no
//!   other member knows it: `403 consent-denied` naming the denied endpoints
//!   (N27a; no specification governs the answer: our own design);
//! - the localizer could not answer and the deployment fails closed: `424`
//!   with no member asked, because a holder may be among them (§14.1);
//! - no member knows it: `404`, the operation's own answer for a subject with
//!   no EHR (ITS-REST 1.1.0 `404_EHR_subject`, §11.2).
//!
//! Every `{node, ehr_id}` pair the resolution produced teaches the `ehr_id`
//! index (§12.5.1 step 3). No answer and no log line names the subject: an
//! error body names endpoints and query parameter positions only (§5.4.3).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use axum::response::Response;
use ferrofed_engine::declared::{self, query};
use ferrofed_engine::dispatch::DispatchOptions;
use ferrofed_engine::forward::{ClientRequest, HeldRequest};
use ferrofed_engine::hygiene::Withheld;
use ferrofed_identity::binding::SessionKey;
use ferrofed_identity::patient::{IdentifierNamespace, PatientRef, PatientRefError};
use ferrofed_identity::resolver::Resolution;
use ferrofed_registry::id::{EhrId, EndpointId, NodeId};
use ferrofed_registry::snapshot::{Endpoint, EndpointStatus, RegistrySnapshot};
use http::{HeaderMap, Method};
use openehr_its::rest::client::path_segment;
use openehr_its::rest::generated::ehr::EhrGetBySubjectParams;
use openehr_its::rest::routes::RouteMatch;
use openehr_its::rest::runtime::ApiError;
use secrecy::SecretString;

use crate::error::{self, Code};
use crate::facade::consent;
use crate::facade::localize::{Localized, localize};
use crate::facade::owner::{self, Listed};
use crate::facade::provenance::Provenance;
use crate::facade::route::{self, Arrived, Deadlines, Failure};
use crate::facade::security;
use crate::federation::Federation;
use crate::health::dependencies::Observed;

/// The ITS-REST operation that reads an EHR by its subject.
const OPERATION: &str = "ehr_get_by_subject";

/// Whether `matched` is `GET {base}/v1/ehr`, the read of an EHR by subject.
pub(crate) fn serves(matched: &RouteMatch) -> bool {
    matched.operation_id == OPERATION
}

/// Answers the read of an EHR by subject: resolves the subject and forwards
/// `GET {base}/v1/ehr/{ehr_id}` to the one member that holds it.
///
/// The query string and the headers are held to what the operation declares
/// before anything is resolved, and the subject reaches the cross-reference
/// service alone (§5.2, §5.4.1, N33).
pub(crate) async fn serve(
    federation: &Federation,
    arrived: Arrived<'_>,
    matched: &RouteMatch,
) -> Response {
    let started = Instant::now();
    let request_id = arrived.request_id;
    // NOTE: §5.4.1, N33: the client's id is free text that may carry an
    // identifier, so every log event names the gateway's own id instead.
    let logged = arrived.outbound.to_string();
    let subject = match Subject::of(matched, arrived.uri.query()) {
        Ok(subject) => subject,
        Err(unserved) => return unserved.respond(request_id, &logged),
    };
    if let Err(refusal) = declared::held(matched, None, arrived.headers, &arrived.body) {
        return route::declared_refused(&refusal, request_id, &logged);
    }
    security::subject_consumed(&logged);
    let Some(budget) = Deadlines::from(federation, started) else {
        return Unserved::Clock.respond(request_id, &logged);
    };
    let snapshot = federation.snapshot();
    let (candidates, directed) = match candidates(snapshot, arrived.headers) {
        Ok(candidates) => candidates,
        Err(unserved) => return unserved.respond(request_id, &logged),
    };
    let members: Vec<NodeId> = candidates
        .iter()
        .map(|endpoint| endpoint.node().clone())
        .collect();
    // NOTE: N4, §5.2, §14.1: a read that names the patient and no node is undirected,
    // so the localizer narrows which members learn of the patient; a targeted one is not (§8).
    let located = if directed {
        Localized::everyone()
    } else {
        localize(federation, &subject.patient, &members, budget.overall()).await
    };
    if located.failed_closed() {
        return Unserved::Unlocalized.respond(request_id, &logged);
    }
    let candidates: Vec<&Endpoint> = candidates
        .into_iter()
        .filter(|endpoint| located.admits(endpoint.node()))
        .collect();
    let members: Vec<NodeId> = candidates
        .iter()
        .map(|endpoint| endpoint.node().clone())
        .collect();
    let consented =
        consent::prefilter(federation, &subject.patient, &members, budget.overall()).await;
    let (denied, candidates): (Vec<&Endpoint>, Vec<&Endpoint>) = candidates
        .into_iter()
        .partition(|endpoint| consented.denied.contains(endpoint.node()));
    let mut resolved = resolve(federation, candidates, &subject.patient, budget.overall()).await;
    resolved.denied = denied
        .iter()
        .map(|endpoint| endpoint.id().clone())
        .collect();
    learn(federation, &resolved.holders, started);
    let (endpoint, ehr_id) = match resolved.settled() {
        Ok(owner) => owner,
        Err(unserved) => return unserved.respond(request_id, &logged),
    };
    tracing::debug!(
        endpoint = %endpoint.id(),
        request_id = logged,
        "routed the read of an EHR by subject to the member that resolved it"
    );
    let withheld = Arc::new(Withheld::new([subject.value]));
    let options = DispatchOptions::new(budget.per_node(), arrived.conveyance.clone())
        .with_request_id(arrived.outbound)
        .with_withheld(withheld)
        .with_composed_ehr_id(ehr_id.clone());
    // NOTE: §5.4.1, N33: the node is located by its own ehr_id alone, so the
    // request is the gateway's own, with none of the client's query or body.
    let request = ClientRequest {
        method: Method::GET,
        path: format!("/ehr/{}", path_segment(&ehr_id.as_str())),
        query: None,
        headers: arrived.headers.clone(),
        body: Vec::new(),
    };
    let provenance = Provenance::of(snapshot, endpoint);
    let forwarded = match HeldRequest::hold(request) {
        Ok(request) => route::send(federation, endpoint, request, &options, &logged).await,
        Err(refused) => Err(Failure::Forward(refused)),
    };
    match forwarded {
        Ok(forwarded) => {
            route::learn(federation, (&ehr_id, None), endpoint, &forwarded, &logged);
            provenance.stamp(route::answered(forwarded))
        }
        Err(Failure::Internal) => error::fixed(Code::Internal, request_id),
        Err(Failure::Forward(failure)) => {
            route::failed(&failure, provenance, (request_id, &logged))
        }
    }
}

/// The subject a request names, consumed as resolution input.
///
/// `Debug` shows the namespace and redacts the identifier.
#[derive(Debug)]
struct Subject {
    /// The patient reference the cross-reference service is asked about.
    patient: PatientRef,
    /// The identifier itself, which the outbound gate withholds.
    value: SecretString,
}

impl Subject {
    /// The subject the query string `query` of `matched` names.
    ///
    /// Each parameter must be one the operation declares, and the query
    /// string is decoded by the operation's generated parameters: `subject_id`
    /// and `subject_namespace` each given once (ITS-REST 1.1.0, both
    /// `required`; a `form` scalar is one pair), each percent-decoded to UTF-8
    /// text (RFC 3986 §2.1, so a `+` is a literal plus).
    fn of(matched: &RouteMatch, query: Option<&str>) -> Result<Self, Unserved> {
        if let Some(query) = query {
            query::every_declared(matched, query).map_err(|unlisted| Unserved::Undeclared {
                position: unlisted.position,
            })?;
        }
        // NOTE: no specification governs this: our own design; the headers are held
        // by `declared::held`, so the generated decoder reads the query string alone.
        let params = EhrGetBySubjectParams::from_request(matched, query, &HeaderMap::new())
            .map_err(Unserved::Malformed)?;
        let namespace =
            IdentifierNamespace::new(params.subject_namespace).map_err(Unserved::Patient)?;
        let value = SecretString::from(params.subject_id);
        let patient = PatientRef::new(namespace, value.clone()).map_err(Unserved::Patient)?;
        Ok(Self { patient, value })
    }
}

/// The endpoints the subject is resolved at: the one the targeting headers
/// name, or the endpoint each member is asked through (§8.4, §11.1), and
/// whether the headers named it.
///
/// # Errors
///
/// Returns [`Unserved::Target`] when the targeting headers name no one
/// registry endpoint (§8.4.1), and [`Unserved::Suspended`] when they name a
/// suspended one, which §11.1 never contacts.
fn candidates<'a>(
    snapshot: &'a RegistrySnapshot,
    headers: &HeaderMap,
) -> Result<(Vec<&'a Endpoint>, bool), Unserved> {
    if let Some(endpoint) = owner::targeted(snapshot, headers)? {
        if endpoint.status() == EndpointStatus::Suspended {
            return Err(Unserved::Suspended);
        }
        return Ok((vec![endpoint], true));
    }
    let every = snapshot
        .nodes()
        .filter_map(|node| snapshot.asked_through(node.id()))
        .collect();
    Ok((every, false))
}

/// What the cross-reference said about each candidate.
#[derive(Debug)]
struct Resolved<'a> {
    /// The candidates that know the subject, with its local `ehr_id` there.
    holders: Vec<(&'a Endpoint, EhrId)>,
    /// The candidates the cross-reference could not answer for.
    silent: Vec<EndpointId>,
    /// The candidates the consent pre-filter denied, never resolved or
    /// contacted (N27a).
    denied: Vec<EndpointId>,
}

impl<'a> Resolved<'a> {
    /// The one endpoint that holds the subject's EHR, with its `ehr_id`.
    ///
    /// Two holders are a `409` whatever the others answered. Otherwise a
    /// candidate the cross-reference could not answer for may hold it too,
    /// so neither one holder nor none is an answer then (§11.5). A candidate
    /// the consent pre-filter denied was never asked, so with no holder among
    /// the others the answer names the denial and never claims no EHR exists
    /// (N27a); with one holder among them, that holder answers.
    ///
    /// # Errors
    ///
    /// Returns [`Unserved::Several`], [`Unserved::Unresolved`],
    /// [`Unserved::ConsentDenied`] or [`Unserved::Nowhere`].
    fn settled(self) -> Result<(&'a Endpoint, EhrId), Unserved> {
        let Self {
            mut holders,
            silent,
            denied,
        } = self;
        if holders.len() > 1 {
            // NOTE: §12.5.2: the gateway never breaks a tie by where the patient
            // resolved, so several holders are listed and none is chosen.
            let endpoints = holders
                .iter()
                .map(|(endpoint, _)| endpoint.id().clone())
                .collect();
            return Err(Unserved::Several(endpoints));
        }
        if !silent.is_empty() {
            return Err(Unserved::Unresolved(silent));
        }
        if holders.is_empty() && !denied.is_empty() {
            return Err(Unserved::ConsentDenied(denied));
        }
        // NOTE: ITS-REST 1.1.0 answers 404 for a subject with no EHR; this is
        // one EHR resource, never the §11.3 result set, so §11.3's 200 does not apply.
        holders.pop().ok_or(Unserved::Nowhere)
    }
}

/// Resolves `patient` at every endpoint of `candidates` before `deadline`.
///
/// Without a cross-reference service, every candidate is unanswered: the
/// gateway fails closed, as a federated query does.
async fn resolve<'a>(
    federation: &Federation,
    candidates: Vec<&'a Endpoint>,
    patient: &PatientRef,
    deadline: Instant,
) -> Resolved<'a> {
    let members: Vec<NodeId> = candidates
        .iter()
        .map(|endpoint| endpoint.node().clone())
        .collect();
    let mut answers = match federation.resolver() {
        Some(resolver) if !members.is_empty() => {
            resolver.resolve(patient, &members, deadline).await
        }
        Some(_) | None => BTreeMap::new(),
    };
    if let Some(observed) = Observed::of_resolutions(&answers) {
        federation.dependencies().resolver(observed);
    }
    let mut resolved = Resolved {
        holders: Vec::new(),
        silent: Vec::new(),
        denied: Vec::new(),
    };
    for endpoint in candidates {
        match answers.remove(endpoint.node()) {
            Some(Resolution::Resolved(ehr_id)) => resolved.holders.push((endpoint, ehr_id)),
            Some(Resolution::Unknown) => {}
            Some(Resolution::Unavailable(_)) | None => {
                resolved.silent.push(endpoint.id().clone());
            }
        }
    }
    resolved
}

/// Teaches the `ehr_id` index, and the session's resolution bindings, every
/// `{node, ehr_id}` pair `holders` names, as a federated query's resolution
/// does (§12.5.1 steps 2 and 3).
fn learn(federation: &Federation, holders: &[(&Endpoint, EhrId)], now: Instant) {
    // TODO(#412): the authenticated client session the resolution bindings belong to.
    let session: Option<SessionKey> = None;
    if let Some(session) = &session {
        federation.bindings().record(
            session,
            now,
            holders
                .iter()
                .map(|(endpoint, ehr_id)| (endpoint.node(), ehr_id)),
        );
    }
    for (endpoint, ehr_id) in holders {
        owner::learn(federation.index(), ehr_id, endpoint.node());
    }
}

/// Why the read of an EHR by subject is answered by the gateway, with no
/// node's answer to pass on.
///
/// Every message names query parameters by position and members by endpoint
/// id, and never the subject (§5.4.3).
#[derive(Debug, thiserror::Error)]
enum Unserved {
    /// The query string carries a parameter the operation does not declare.
    #[error(
        "query parameter {position} is not one the ITS-REST operation declares, so nothing is sent (§5.4.1, N33)"
    )]
    Undeclared {
        /// The parameter's position in the query string, counted from 1.
        position: usize,
    },
    /// The query string is no `ehr_get_by_subject` query: `subject_id` or
    /// `subject_namespace` is absent or repeated, or a pair does not
    /// percent-decode to UTF-8 text.
    #[error(
        "GET {{base}}/v1/ehr names its subject by subject_id and subject_namespace, each given once as UTF-8 text (ITS-REST 1.1.0)"
    )]
    Malformed(#[source] ApiError),
    /// The subject cannot form a patient reference.
    #[error("the subject cannot form a patient reference (§5.2)")]
    Patient(#[source] PatientRefError),
    /// The targeting headers name no one registry endpoint.
    #[error(transparent)]
    Target(#[from] owner::Untargeted),
    /// The targeting headers name a suspended endpoint.
    #[error(
        "the targeted endpoint is suspended, so the request can be routed to no destination (§11.1, §11.2)"
    )]
    Suspended,
    /// No candidate knows the subject.
    #[error(
        "no member holds an EHR for this subject, so the request can be routed to no destination (§11.2)"
    )]
    Nowhere,
    /// Several candidates know the subject.
    #[error(
        "the subject resolves at endpoints {}, and the gateway never chooses between them: name one in the openEHR-federation-endpoint header (§8.4, §12.5.2)",
        Listed(.0)
    )]
    Several(Vec<EndpointId>),
    /// The cross-reference could not answer for these candidates.
    #[error(
        "the cross-reference could not answer for endpoints {}, so whether the subject has an EHR there is unknown (§5.2, §11.2)",
        Listed(.0)
    )]
    Unresolved(Vec<EndpointId>),
    /// The consent pre-filter denied these candidates, and no other one
    /// holds the subject's EHR.
    #[error(
        "the consent pre-filter does not permit asking endpoints {} about this subject, and no other member holds an EHR for it (N27a, §13.2.1)",
        Listed(.0)
    )]
    ConsentDenied(Vec<EndpointId>),
    /// The localizer could not answer, the deployment fails closed, and so no
    /// member was asked (§14.1, N4).
    #[error(
        "the localizer could not answer, or its exchange could not be audited, so no member was asked and whether the subject has an EHR is unknown (§14.1, N4)"
    )]
    Unlocalized,
    /// The request's deadline cannot be represented.
    #[error("the request's deadline cannot be represented")]
    Clock,
}

impl Unserved {
    /// The stable code the error body names.
    fn code(&self) -> Code {
        match self {
            Self::Undeclared { .. } => Code::QueryParameterRefused,
            Self::Malformed(_) | Self::Patient(_) => Code::PatientInvalid,
            Self::Target(untargeted) => untargeted.code(),
            Self::Suspended | Self::Nowhere => Code::NoDestination,
            Self::Several(_) => Code::SubjectSeveral,
            Self::Unresolved(_) => Code::ResolutionUnavailable,
            Self::ConsentDenied(_) => Code::ConsentDenied,
            Self::Unlocalized => Code::LocalizationUnavailable,
            Self::Clock => Code::Internal,
        }
    }

    /// The error answer, naming the client's `request_id`; a refused query
    /// parameter is a security event and a failed resolution is logged, both
    /// under the gateway's `logged` id.
    fn respond(self, request_id: &str, logged: &str) -> Response {
        let code = self.code();
        match &self {
            Self::Undeclared { position } => security::query_parameter_refused(*position, logged),
            Self::Unresolved(_) | Self::Unlocalized | Self::Clock => tracing::error!(
                code = code.as_str(),
                error = %self,
                request_id = logged,
                "the read of an EHR by subject was not served"
            ),
            _ => {}
        }
        if code == Code::Internal {
            return error::fixed(code, request_id);
        }
        error::response(code, crate::chain(&self), request_id)
    }
}

#[cfg(test)]
mod tests {
    use super::{Code, Resolved, Unserved};
    use ferrofed_identity::patient::PatientRefError;
    use ferrofed_registry::id::EndpointId;
    use http::StatusCode;
    use openehr_its::rest::runtime::ApiError;

    fn endpoint(id: &str) -> EndpointId {
        EndpointId::new(id).unwrap()
    }

    #[test]
    fn each_refusal_answers_its_code_and_status() {
        let table = [
            (
                Unserved::Undeclared { position: 2 },
                "query-parameter-refused",
                StatusCode::BAD_REQUEST,
            ),
            (
                Unserved::Malformed(ApiError::BadRequest(
                    "the query parameter `subject_id` is given more than once".to_owned(),
                )),
                "patient-invalid",
                StatusCode::BAD_REQUEST,
            ),
            (
                Unserved::Patient(PatientRefError::EmptyValue),
                "patient-invalid",
                StatusCode::BAD_REQUEST,
            ),
            (Unserved::Suspended, "no-destination", StatusCode::NOT_FOUND),
            (Unserved::Nowhere, "no-destination", StatusCode::NOT_FOUND),
            (
                Unserved::Several(vec![endpoint("a"), endpoint("b")]),
                "subject-several",
                StatusCode::CONFLICT,
            ),
            (
                Unserved::Unresolved(vec![endpoint("a")]),
                "resolution-unavailable",
                StatusCode::FAILED_DEPENDENCY,
            ),
            (
                Unserved::ConsentDenied(vec![endpoint("a")]),
                "consent-denied",
                StatusCode::FORBIDDEN,
            ),
            (
                Unserved::Unlocalized,
                "localization-unavailable",
                StatusCode::FAILED_DEPENDENCY,
            ),
            (
                Unserved::Clock,
                "internal",
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
        ];
        for (unserved, code, status) in table {
            let answered: Code = unserved.code();
            assert_eq!(code, answered.as_str(), "{unserved:?}");
            assert_eq!(status, answered.status(), "{unserved:?}");
        }
    }

    #[test]
    fn two_holders_list_their_endpoints_by_id() {
        let message =
            Unserved::Several(vec![endpoint("node-a-pub"), endpoint("node-b-pub")]).to_string();
        assert!(message.contains("[node-a-pub, node-b-pub]"), "{message}");
    }

    #[test]
    fn nothing_resolved_and_nothing_silent_is_nowhere() {
        let resolved = Resolved {
            holders: Vec::new(),
            silent: Vec::new(),
            denied: Vec::new(),
        };
        assert!(matches!(resolved.settled(), Err(Unserved::Nowhere)));
    }

    #[test]
    fn a_silent_candidate_leaves_the_holder_unknown() {
        let resolved = Resolved {
            holders: Vec::new(),
            silent: vec![endpoint("node-b-pub")],
            denied: Vec::new(),
        };
        assert!(matches!(resolved.settled(), Err(Unserved::Unresolved(_))));
    }

    #[test]
    fn denied_candidates_and_no_holder_is_consent_denied() {
        let resolved = Resolved {
            holders: Vec::new(),
            silent: Vec::new(),
            denied: vec![endpoint("node-b-pub")],
        };
        assert!(matches!(
            resolved.settled(),
            Err(Unserved::ConsentDenied(ref endpoints)) if endpoints == &[endpoint("node-b-pub")]
        ));
    }
}
