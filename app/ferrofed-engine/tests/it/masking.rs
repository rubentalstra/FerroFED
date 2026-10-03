// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! The outbound gate masks one part of a URL path, and only when the gateway
//! composed it (§5.4, N33): the node's own `ehr_id` segment after
//! `{base}/ehr/`, which the gateway took from a resolution, never from the
//! client. A value inside it is chance, and passes. The same value anywhere
//! else is still found, the operator's base path included.
#![allow(
    clippy::panic_in_result_fn,
    reason = "test assertions in tests that return their setup errors"
)]

use ferrofed_engine::hygiene::{Composed, Outbound, Part, Withheld};
use secrecy::SecretString;
use url::{ParseError, Url};

type TestResult = Result<(), ParseError>;

/// The base path of an endpoint whose registry URL holds no withheld value.
const BASE_PATH: &str = "/openehr/v1";

/// A node-local `ehr_id` that holds [`SHORT`].
const EHR_ID: &str = "7d44b88c-4199-4bad-97dc-d78268e01398";

/// A short synthetic identifier.
const SHORT: &str = "4199";

fn short() -> Withheld {
    Withheld::new([SecretString::from(SHORT)])
}

/// The path the composed segment follows under `base`.
fn prefix(base: &str) -> String {
    format!("{base}/ehr/")
}

/// The routed read of `target` under the base path `prefix` names, with
/// [`EHR_ID`] composed by the gateway when `composed` is set.
fn routed<'a>(
    target: &'a Url,
    prefix: &'a str,
    composed: bool,
    headers: &'a [(&'static str, &'a str)],
) -> Outbound<'a> {
    Outbound {
        aql: "",
        scope: None,
        paging: &[],
        url: target,
        composed: Composed {
            ehr_prefix: prefix,
            ehr_segment: composed.then_some(EHR_ID),
        },
        headers,
        conveyed: &[],
    }
}

/// The URL of the routed read of the EHR [`EHR_ID`] under `base`.
fn ehr_url(base: &str) -> Result<Url, ParseError> {
    Url::parse(&format!("https://cdr.example.org{base}/ehr/{EHR_ID}"))
}

// conformance: CP-26
#[test]
fn a_short_identifier_inside_the_composed_ehr_id_passes() -> TestResult {
    let at = prefix(BASE_PATH);
    assert_eq!(
        None,
        short().found_in(&routed(&ehr_url(BASE_PATH)?, &at, true, &[]))
    );
    Ok(())
}

// conformance: CP-26
#[test]
fn a_short_identifier_in_the_base_path_is_found() -> TestResult {
    let base = format!("/cdr-{SHORT}/v1");
    let at = prefix(&base);
    assert_eq!(
        Some(Part::Url),
        short().found_in(&routed(&ehr_url(&base)?, &at, true, &[])),
        "the operator's base path is never masked"
    );
    Ok(())
}

// conformance: CP-26
#[test]
fn the_same_short_identifier_in_a_client_part_is_still_found() -> TestResult {
    let at = prefix(BASE_PATH);
    for text in [
        format!("https://cdr.example.org{BASE_PATH}/ehr/{EHR_ID}/composition/{SHORT}"),
        format!("https://cdr.example.org{BASE_PATH}/ehr/{EHR_ID}?version_at_time={SHORT}"),
        format!("https://cdr.example.org{BASE_PATH}/ehr/x{SHORT}/{EHR_ID}"),
    ] {
        let target = Url::parse(&text)?;
        assert_eq!(
            Some(Part::Url),
            short().found_in(&routed(&target, &at, true, &[])),
            "{text}"
        );
    }
    let accept = format!("application/json; x={SHORT}");
    let headers = [("Accept", accept.as_str())];
    assert_eq!(
        Some(Part::Header("Accept")),
        short().found_in(&routed(&ehr_url(BASE_PATH)?, &at, true, &headers))
    );
    Ok(())
}

// conformance: CP-26
#[test]
fn an_ehr_id_segment_the_client_wrote_is_never_masked() -> TestResult {
    let at = prefix(BASE_PATH);
    assert_eq!(
        Some(Part::Url),
        short().found_in(&routed(&ehr_url(BASE_PATH)?, &at, false, &[])),
        "only a segment the gateway composed is masked"
    );
    Ok(())
}

#[test]
fn an_identifier_holding_the_composed_ehr_id_fails_closed() -> TestResult {
    let at = prefix(BASE_PATH);
    let equal = Withheld::new([SecretString::from(EHR_ID)]);
    assert_eq!(
        Some(Part::Url),
        equal.found_in(&routed(&ehr_url(BASE_PATH)?, &at, true, &[]))
    );
    Ok(())
}
