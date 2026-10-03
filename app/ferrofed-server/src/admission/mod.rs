// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! The admission check of one member against the conditions of §12b.2.
//!
//! The federation operator verifies the identifier-integrity conditions by
//! test, with synthetic data (§12b.1, N42a, CP-33a). The check creates a few test EHRs on the node through `POST /ehr`, each
//! for a fresh [`subject::SyntheticSubject`], reads each back through
//! `GET /ehr/{ehr_id}`, and asks the configured cross-reference for each
//! subject. Every call goes through the endpoint's node client with its
//! onward credentials, and passes the outbound gate with the run's subjects
//! withheld (§5.4.1, N33). The [`report::Report`] holds one finding per
//! condition of the §12b.2 table:
//!
//! | Condition | What the check does |
//! |---|---|
//! | `ehr_id` generation | each created `ehr_id` is a version-4 UUID, and no two are equal |
//! | No reuse | cannot be checked: reuse across a restore, a migration or a reset is not observable from outside the node |
//! | No adoption of foreign `ehr_id`s | cannot be checked: the check performs no import |
//! | `system_id` uniqueness | the node reports, in each EHR, the `system_id` the registry records for it, which the registry load holds unique |
//! | `ehr_id` exchange | the cross-reference maps each subject to the `ehr_id` the node created |
//!
//! A node that cannot be reached fails every condition the check exercises,
//! with the typed cause: a check that reached nothing passes nothing. No
//! specification governs the form of the check: our own design.

pub mod report;
pub mod subject;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use ferrofed_engine::dispatch::DispatchOptions;
use ferrofed_engine::ehr::EhrCallError;
use ferrofed_engine::hygiene::Withheld;
use ferrofed_engine::outbound_id::OutboundId;
use ferrofed_identity::resolver::Resolution;
use ferrofed_registry::id::{EhrId, EndpointId, NodeId, SystemId};
use ferrofed_registry::snapshot::{Node, RegistrySnapshot};
use openehr_rm::v1_2::ehr::ehr::Ehr;
use uuid::{Uuid, Variant, Version};

use crate::chain;
use crate::conveyed::{self, Unconveyed};
use crate::federation::Federation;
use report::{Condition, Finding, Report, Verdict};
use subject::SyntheticSubject;

/// The number of test EHRs a check creates when the operator names none.
pub const DEFAULT_COUNT: u8 = 3;

/// An admission check that could not start.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AdmissionError {
    /// The endpoint is not an endpoint of the registry.
    #[error("endpoint {0} is not an endpoint of the registry")]
    UnknownEndpoint(EndpointId),
    /// The per-node budget added to the current instant passes the range of
    /// the clock.
    #[error("the per-node budget passes the range of the clock")]
    Clock,
    /// The gateway cannot convey itself to the node, so nothing is sent
    /// (§13.1, N24).
    #[error("the check cannot convey the gateway to the node")]
    Unconveyed(#[source] Unconveyed),
}

/// One test EHR the node created: the subject it was created for and the
/// `ehr_id` the node named.
struct Created {
    subject: usize,
    ehr_id: String,
}

/// Runs the admission check against `endpoint` of `federation`, creating
/// `count` test EHRs on its node.
///
/// # Errors
///
/// Returns [`AdmissionError::UnknownEndpoint`] when the registry does not
/// hold `endpoint`, [`AdmissionError::Clock`] when no deadline can be set,
/// and [`AdmissionError::Unconveyed`] when the federation holds no signer.
/// Every failure of the node or the cross-reference is a finding.
pub async fn check(
    federation: &Federation,
    endpoint: &EndpointId,
    count: u8,
) -> Result<Report, AdmissionError> {
    let snapshot = federation.snapshot();
    let unknown = || AdmissionError::UnknownEndpoint(endpoint.clone());
    let declared = snapshot.endpoint(endpoint).ok_or_else(unknown)?;
    let node = snapshot.node(declared.node()).ok_or_else(unknown)?;
    let client = federation.clients().get(endpoint).ok_or_else(unknown)?;
    let per_node = federation.budget().per_node();
    let deadline = || {
        Instant::now()
            .checked_add(per_node)
            .ok_or(AdmissionError::Clock)
    };

    let subjects: Vec<SyntheticSubject> = (0..count).map(|_| SyntheticSubject::fresh()).collect();
    let withheld = Arc::new(Withheld::new(
        subjects.iter().map(|subject| subject.value().clone()),
    ));
    let conveyance = conveyed::gateway(federation).map_err(AdmissionError::Unconveyed)?;
    let options = |at: Instant| {
        DispatchOptions::new(at, conveyance.clone())
            .with_withheld(Arc::clone(&withheld))
            .with_request_id(OutboundId::mint())
    };

    let mut created = Vec::new();
    let mut refused = None;
    for (index, subject) in subjects.iter().enumerate() {
        match client
            .create_ehr(&subject.ehr_status(), &options(deadline()?))
            .await
        {
            Ok(ehr_id) => created.push(Created {
                subject: index,
                ehr_id,
            }),
            Err(error) => {
                refused = Some((index, error));
                break;
            }
        }
    }
    let mut read = Vec::with_capacity(created.len());
    for one in &created {
        read.push(client.read_ehr(&one.ehr_id, &options(deadline()?)).await);
    }
    let refused = refused.map(|(index, error)| {
        format!(
            "creating test EHR {} of {count} failed: {}",
            index.saturating_add(1),
            chain(&error)
        )
    });

    let exchange = exchange(federation, node, &subjects, &created, deadline()?).await;
    let findings = vec![
        generation(&created, &read, refused.as_deref()),
        no_reuse(),
        no_foreign_adoption(),
        system_id(snapshot, node, &created, &read, created.is_empty()),
        exchange.unwrap_or_else(|line| {
            Finding::new(Condition::EhrIdExchange, Verdict::Fail, vec![line])
        }),
    ];
    let values: Vec<_> = subjects.iter().map(SyntheticSubject::value).collect();
    Ok(Report::new(
        endpoint.clone(),
        node.id().clone(),
        created.into_iter().map(|one| one.ehr_id).collect(),
        findings,
        &values,
    ))
}

/// The verdict of a set of lines: a failure fails it, else a line that
/// cannot be checked leaves it unchecked, else it passes.
fn verdict(lines: &[(Verdict, String)]) -> Verdict {
    if lines.iter().any(|(verdict, _)| *verdict == Verdict::Fail) {
        Verdict::Fail
    } else if lines
        .iter()
        .any(|(verdict, _)| *verdict == Verdict::CannotCheck)
    {
        Verdict::CannotCheck
    } else {
        Verdict::Pass
    }
}

/// The finding on `condition` from its evidence lines.
fn finding(condition: Condition, lines: Vec<(Verdict, String)>) -> Finding {
    let verdict = verdict(&lines);
    Finding::new(
        condition,
        verdict,
        lines.into_iter().map(|(_, line)| line).collect(),
    )
}

/// The `ehr_id` generation condition: each created `ehr_id` is a version-4
/// UUID, the node reads it back under the same `ehr_id`, and no two are
/// equal (§12b.2, N42a).
fn generation(
    created: &[Created],
    read: &[Result<Ehr, EhrCallError>],
    refused: Option<&str>,
) -> Finding {
    let mut lines: Vec<(Verdict, String)> = Vec::new();
    if let Some(refused) = refused {
        lines.push((Verdict::Fail, refused.to_owned()));
    }
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    for (one, answer) in created.iter().zip(read) {
        lines.push(uuid_form(&one.ehr_id));
        if let Ok(ehr) = answer
            && !ehr.ehr_id.value().eq_ignore_ascii_case(&one.ehr_id)
        {
            lines.push((
                Verdict::Fail,
                format!(
                    "GET /ehr/{} answered an EHR whose ehr_id is {}",
                    one.ehr_id,
                    ehr.ehr_id.value()
                ),
            ));
        }
        let times = seen.entry(one.ehr_id.to_ascii_lowercase()).or_default();
        *times = times.saturating_add(1);
    }
    let repeated: Vec<String> = seen
        .iter()
        .filter(|(_, times)| **times > 1)
        .map(|(ehr_id, times)| format!("{ehr_id} was issued for {times} different subjects"))
        .collect();
    if repeated.is_empty() {
        if created.len() > 1 {
            lines.push((
                Verdict::Pass,
                format!("the {} ehr_ids are distinct", created.len()),
            ));
        }
    } else {
        lines.extend(repeated.into_iter().map(|line| (Verdict::Fail, line)));
    }
    if created.is_empty() && refused.is_none() {
        lines.push((Verdict::Fail, "the node created no EHR".to_owned()));
    }
    finding(Condition::EhrIdGeneration, lines)
}

/// What the form of one `ehr_id` shows about its generation.
// NOTE: §12b.2 names version-4 UUIDs and no other scheme, so another UUID
// version is left to the operator's judgement of equivalence.
fn uuid_form(ehr_id: &str) -> (Verdict, String) {
    let Some(uuid) = Uuid::try_parse(ehr_id)
        .ok()
        .filter(|uuid| uuid.hyphenated().to_string().eq_ignore_ascii_case(ehr_id))
    else {
        return (
            Verdict::Fail,
            format!(
                "{ehr_id} is not a UUID in its hyphenated form; §12b.2 asks for version-4 UUIDs, and a sequential, short or deployment-local ehr_id is not admitted without remediation"
            ),
        );
    };
    match (uuid.get_version(), uuid.get_variant()) {
        (Some(Version::Random), Variant::RFC4122) => (
            Verdict::Pass,
            format!("{ehr_id} is a version-4 UUID (RFC 9562 §5.4)"),
        ),
        (Some(_), Variant::RFC4122) => (
            Verdict::CannotCheck,
            format!(
                "{ehr_id} is a version-{} UUID; §12b.2 admits a scheme other than version 4 only with equivalent collision resistance and no coordination requirement, which the operator judges",
                uuid.get_version_num()
            ),
        ),
        _ => (
            Verdict::Fail,
            format!("{ehr_id} is a UUID of no RFC 9562 version and variant"),
        ),
    }
}

/// The no-reuse condition, which no check from outside the node can decide.
fn no_reuse() -> Finding {
    Finding::new(
        Condition::NoReuse,
        Verdict::CannotCheck,
        vec![
            "reuse happens across a restore, a migration or a test-data reset, which a check sees no trace of: an ehr_id the node issued before one is not observable from outside the node".to_owned(),
            "the ehr_ids this run created are compared with each other under ehr_id generation".to_owned(),
            "the evidence is the node's documented ehr_id generation and its restore and reset procedures".to_owned(),
        ],
    )
}

/// The no-adoption condition, which the check cannot exercise.
fn no_foreign_adoption() -> Finding {
    Finding::new(
        Condition::NoForeignAdoption,
        Verdict::CannotCheck,
        vec![
            "adoption happens when the node imports EHRs from elsewhere, and the check performs no import".to_owned(),
            "whether an import issues a fresh local ehr_id is the node's import procedure, and a registration as holder of the origin node's ehr_id space is the operator's registry decision".to_owned(),
        ],
    )
}

/// The `system_id` uniqueness condition: the registry holds the node's
/// `system_id` unique, and the node reports that `system_id` in each EHR it
/// created (§12b.2, §12.2, N42a).
fn system_id(
    snapshot: &RegistrySnapshot,
    node: &Node,
    created: &[Created],
    read: &[Result<Ehr, EhrCallError>],
    created_none: bool,
) -> Finding {
    let recorded = node.system_id();
    let mut lines = vec![(
        Verdict::Pass,
        format!(
            "the registry records system_id {recorded} for node {}, and its load refuses a second member with the same system_id, compared without regard to ASCII case",
            node.id()
        ),
    )];
    if created_none {
        lines.push((
            Verdict::Fail,
            "no EHR was created, so the node's own system_id could not be read (the cause is under ehr_id generation)".to_owned(),
        ));
    }
    for (one, answer) in created.iter().zip(read) {
        lines.push(match answer {
            Ok(ehr) => reported(snapshot, node.id(), recorded, &one.ehr_id, ehr),
            Err(error) => (
                Verdict::Fail,
                format!("reading EHR {} failed: {}", one.ehr_id, chain(error)),
            ),
        });
    }
    finding(Condition::SystemIdUniqueness, lines)
}

/// What the `system_id` the node reports in `ehr` shows.
fn reported(
    snapshot: &RegistrySnapshot,
    node: &NodeId,
    recorded: &SystemId,
    ehr_id: &str,
    ehr: &Ehr,
) -> (Verdict, String) {
    let value = ehr.system_id.value();
    let Ok(reported) = SystemId::new(value) else {
        return (
            Verdict::Fail,
            format!("EHR {ehr_id} reports system_id {value}, which is not an openEHR uid"),
        );
    };
    if reported == *recorded {
        return (
            Verdict::Pass,
            format!("EHR {ehr_id} reports system_id {value}, the one the registry records"),
        );
    }
    match snapshot.node_for_system_id(&reported) {
        Some(other) if other.id() != node => (
            Verdict::Fail,
            format!(
                "EHR {ehr_id} reports system_id {value}, which the registry records for node {}",
                other.id()
            ),
        ),
        _ => (
            Verdict::Fail,
            format!(
                "EHR {ehr_id} reports system_id {value}, and the registry records {recorded}: a creating_system_id is routed by the registry (§12.2)"
            ),
        ),
    }
}

/// The `ehr_id` exchange condition: the configured cross-reference maps each
/// synthetic subject to the `ehr_id` the node created for it (§12b.2, §5.5,
/// N34), or the line that fails it outright.
async fn exchange(
    federation: &Federation,
    node: &Node,
    subjects: &[SyntheticSubject],
    created: &[Created],
    deadline: Instant,
) -> Result<Finding, String> {
    let Some(resolver) = federation.resolver() else {
        return Err(
            "no cross-reference is configured ([dev] or [pixm]), so nothing turns a patient into this node's ehr_id (§5.5)".to_owned(),
        );
    };
    if created.is_empty() {
        return Err("no EHR was created, so there is no ehr_id to resolve to".to_owned());
    }
    let mut lines = Vec::new();
    for one in created {
        let Some(patient) = subjects
            .get(one.subject)
            .and_then(|subject| subject.patient().ok())
        else {
            return Err(format!(
                "the subject of EHR {} is not a patient reference",
                one.ehr_id
            ));
        };
        let mut answer = resolver
            .resolve(&patient, std::slice::from_ref(node.id()), deadline)
            .await;
        lines.push(match answer.remove(node.id()) {
            Some(Resolution::Resolved(found)) => {
                if EhrId::new(one.ehr_id.as_str()).is_ok_and(|created| created == found) {
                    (
                        Verdict::Pass,
                        format!(
                            "the cross-reference maps the subject of EHR {0} to {0}",
                            one.ehr_id
                        ),
                    )
                } else {
                    (
                        Verdict::Fail,
                        format!(
                            "the cross-reference maps the subject of EHR {} to {found}, another ehr_id",
                            one.ehr_id
                        ),
                    )
                }
            }
            Some(Resolution::Unknown) => (
                Verdict::CannotCheck,
                format!(
                    "the cross-reference does not know the subject of EHR {}",
                    one.ehr_id
                ),
            ),
            Some(Resolution::Unavailable(error)) => (
                Verdict::CannotCheck,
                format!(
                    "the cross-reference did not answer for the subject of EHR {}: {}",
                    one.ehr_id,
                    chain(&error)
                ),
            ),
            None => (
                Verdict::CannotCheck,
                format!(
                    "the cross-reference gave no answer for node {} about the subject of EHR {}",
                    node.id(),
                    one.ehr_id
                ),
            ),
        });
    }
    if verdict(&lines) == Verdict::CannotCheck {
        lines.push((
            Verdict::CannotCheck,
            "the gateway writes to no cross-reference: the [dev] table is static configuration and a PIX Manager is fed by its own identity sources, so the round trip passes only where the node's environment registers a new EHR's subject itself".to_owned(),
        ));
    }
    Ok(finding(Condition::EhrIdExchange, lines))
}
