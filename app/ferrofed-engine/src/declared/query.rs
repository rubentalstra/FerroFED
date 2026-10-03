// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! The parameters of a client query string: their names held to those its
//! ITS-REST operation declares, and on a routed request, the value of each
//! declared one to its declared kind (§5.4.1, N33).
//!
//! Names are read percent-decoded, to find the first one a rule does not
//! admit. A value the gateway consumes is decoded by the generated
//! `*Params` of `openehr-its`; a value a route forwards is held to its kind
//! (`values`) and travels as received.

use openehr_its::rest::routes::RouteMatch;

use super::kind::fits;
use super::{Carrier, Expected, MalformedValue};
use crate::hygiene::{self, UnlistedParameter};

/// Checks that `operation` declares every parameter of `query`, a query
/// string the gateway consumes and never forwards.
///
/// A parameter's name is compared percent-decoded, and an empty pair
/// (`a=1&&b=2`) carries nothing and is ignored.
///
/// # Errors
///
/// Returns [`UnlistedParameter`] naming the first other parameter by its
/// position, never by its name or value, either of which may be the
/// identifier.
pub fn every_declared(operation: &RouteMatch, query: &str) -> Result<(), UnlistedParameter> {
    admitted(query, |name| operation.query_key(name).is_some())
}

/// Checks that `admits` accepts the percent-decoded name of every parameter
/// of `query`, an empty pair ignored.
pub(crate) fn admitted(
    query: &str,
    admits: impl Fn(&str) -> bool,
) -> Result<(), UnlistedParameter> {
    let unlisted = query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .position(|pair| {
            let name = pair.split('=').next().unwrap_or(pair);
            !admits(&hygiene::decode::percent_decoded(name))
        });
    match unlisted {
        Some(index) => Err(UnlistedParameter {
            position: index.saturating_add(1),
        }),
        None => Ok(()),
    }
}

/// Holds each declared parameter of `query` to its kind.
pub(super) fn values(operation: &RouteMatch, query: &str) -> Result<(), MalformedValue> {
    let pairs = query.split('&').filter(|pair| !pair.is_empty());
    for (index, pair) in pairs.enumerate() {
        let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
        let Some(param) = operation.query_key(&hygiene::decode::percent_decoded(name)) else {
            continue;
        };
        let list = !param.explode;
        if !fits(&param.kind, &hygiene::decode::percent_decoded(value), list) {
            return Err(MalformedValue {
                carrier: Carrier::Query {
                    position: index.saturating_add(1),
                    name: param.name,
                },
                expected: Expected::Kind(param.kind),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::every_declared;
    use crate::hygiene::UnlistedParameter;
    use http::Method;
    use openehr_its::rest::routes::{Lookup, RouteMatch, lookup};

    fn by_subject() -> RouteMatch {
        match lookup(&Method::GET, "/ehr") {
            Lookup::Matched(matched) => matched,
            other => panic!("GET /ehr names no operation: {other:?}"),
        }
    }

    #[test]
    fn a_consumed_query_admits_the_subject_parameters_and_nothing_undeclared() {
        let operation = by_subject();
        let declared = "subject_id=4711&&subject%5Fnamespace=x";
        assert_eq!(Ok(()), every_declared(&operation, declared));
        assert_eq!(Ok(()), every_declared(&operation, ""));
        let refused = every_declared(&operation, "subject_id=4711&patient=O%27Sentinel-4711");
        assert_eq!(Err(UnlistedParameter { position: 2 }), refused);
    }
}
