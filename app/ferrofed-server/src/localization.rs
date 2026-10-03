// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! The localizer an undirected patient query is planned with, and what the
//! gateway does when it does not answer (N4, N10, §14.1).
//!
//! Under `federation.node_selection = "localized"` exactly one localizer is
//! active: the XCPD localizer when `[xcpd]` is set (Annex A.3), the PIXm
//! resolver when `[pixm]` is (§14.2), and the static development
//! cross-reference under `profile = "development"` otherwise. A configured
//! localizer that does not answer fails closed unless
//! `[federation.localization] on_failure = "ask-all"` declares otherwise, and
//! `OPTIONS {base}/` declares the policy either way (§7a.2, N30). Under `node_selection = "ask-all"` there is no localizer and
//! every member is a candidate (§4.3, N4 last sentence).

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use ferrofed_identity::dev::{Profile, StaticResolver};
use ferrofed_identity::localizer::{Localizer, OnFailure};
use ferrofed_identity::patient::{IdentifierNamespace, PatientRefError};
use ferrofed_identity::pixm::PixmResolver;
use ferrofed_identity::xcpd::{
    AssertionSource, FixedAssertion, GatewayConfig, LogAudit, Tls, Transport, XcpdConfig,
    XcpdConfigError, XcpdLocalizer,
};
use ferrofed_registry::error::IdError;
use ferrofed_registry::id::NodeId;
use ferrofed_registry::secret::Secret;
use ferrofed_registry::snapshot::RegistrySnapshot;

use crate::config::settings::{LocalizationSettings, Settings};
use crate::config::xcpd::{AuditDestination, XcpdSettings};
use crate::config::{self, NodeSelection};

/// The localizer of a federation, with its failure policy and budget.
#[derive(Clone)]
pub struct LocalizationPolicy {
    localizer: Option<Arc<dyn Localizer>>,
    mode: Option<&'static str>,
    audit: Option<&'static str>,
    on_failure: OnFailure,
    timeout: Duration,
}

impl LocalizationPolicy {
    /// The policy of a deployment with no localizer: every member is a
    /// candidate, and `OPTIONS {base}/` declares the default `closed`.
    #[must_use]
    pub fn none() -> Self {
        Self {
            localizer: None,
            mode: None,
            audit: None,
            on_failure: OnFailure::Closed,
            timeout: Duration::ZERO,
        }
    }

    /// The policy of `localizer`, the binding `mode` names, which may take
    /// `timeout` and on failure does what `on_failure` says (§14.1).
    #[must_use]
    pub fn new(
        localizer: Arc<dyn Localizer>,
        mode: &'static str,
        on_failure: OnFailure,
        timeout: Duration,
    ) -> Self {
        Self {
            localizer: Some(localizer),
            mode: Some(mode),
            audit: None,
            on_failure,
            timeout,
        }
    }

    /// The configured localizer, if any.
    #[must_use]
    pub fn localizer(&self) -> Option<&dyn Localizer> {
        self.localizer.as_deref()
    }

    /// The name of the configured binding, which `OPTIONS {base}/` declares
    /// as `localization.mode`.
    #[must_use]
    pub fn mode(&self) -> Option<&'static str> {
        self.mode
    }

    /// What the gateway does when the localizer does not answer.
    #[must_use]
    pub fn on_failure(&self) -> OnFailure {
        self.on_failure
    }

    /// How long the localizer may take; zero when none is configured.
    #[must_use]
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// This policy, its localizer's exchanges audited to `audit`, which
    /// `OPTIONS {base}/` declares as `localization.audit`.
    #[must_use]
    pub fn audited(mut self, audit: &'static str) -> Self {
        self.audit = Some(audit);
        self
    }

    /// Where the localizer's audit messages go, when it records any.
    #[must_use]
    pub fn audit(&self) -> Option<&'static str> {
        self.audit
    }
}

impl fmt::Debug for LocalizationPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LocalizationPolicy")
            .field("mode", &self.mode)
            .field("audit", &self.audit)
            .field("on_failure", &self.on_failure)
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

/// The `localization.mode` of the static development cross-reference.
pub const DEVELOPMENT_STATIC: &str = "development-static";

/// The `localization.mode` of the XCPD localizer (Annex A.3).
pub const XCPD: &str = "xcpd";

/// The `localization.mode` of the PIXm localizer (§14.2, Annex A.1).
pub const PIXM: &str = "pixm";

/// A localization configuration that cannot be set up.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum LocalizationError {
    /// `node_selection = "localized"` is declared and no localizer is
    /// configured, so no undirected patient query could find its node set
    /// (N4).
    #[error(
        "federation.node_selection = \"localized\" needs a localizer: [xcpd], [pixm], or the [dev] cross-reference under profile = \"development\" (N4, §14.1)"
    )]
    NoLocalizer,
    /// `[federation.localization]` is set under a node selection that uses
    /// no localizer.
    #[error(
        "[federation.localization] applies only under federation.node_selection = \"localized\"; remove it, or declare the localized selection"
    )]
    NotLocalized,
    /// `[xcpd]` is set under a node selection that uses no localizer.
    #[error(
        "[xcpd] is a localizer and applies only under federation.node_selection = \"localized\"; remove it, or declare the localized selection"
    )]
    XcpdUnused,
    /// The XUA assertion is not one SAML 2.0 `Assertion` element.
    #[error("{key} is not one SAML 2.0 Assertion element")]
    XcpdAssertion {
        /// The key the assertion was read from.
        key: &'static str,
    },
    /// An `[xcpd.communities]` value is not a node id.
    #[error("xcpd.communities.{community:?} is not a node id")]
    XcpdMember {
        /// The community key, an OID.
        community: String,
        /// What the id rules reported.
        #[source]
        source: IdError,
    },
    /// An `[xcpd.namespaces]` key is empty.
    #[error("xcpd.namespaces has an empty namespace")]
    XcpdNamespace(#[source] PatientRefError),
    /// The XCPD localizer refuses its configuration.
    #[error("the [xcpd] localizer cannot be enabled")]
    Xcpd(#[source] XcpdConfigError),
}

/// The resolver the federation runs, as a localizer it can also be.
#[derive(Debug, Default)]
pub struct Resolving {
    /// The static development cross-reference, when `[dev]` is set.
    pub development: Option<Arc<StaticResolver>>,
    /// The PIXm resolver, when `[pixm]` is set.
    pub pixm: Option<Arc<PixmResolver>>,
}

/// The localization policy `settings` declare under `selection` over the
/// members of `snapshot`, with the resolver the federation runs, `resolving`.
///
/// The localizer is the XCPD one when `[xcpd]` is set. Otherwise it is the
/// resolver itself: the PIXm resolver, which names the members whose domain
/// holds the patient over the ITI-83 call its resolution reuses (§14.2), or
/// the development cross-reference.
///
/// # Errors
///
/// Returns [`LocalizationError::NoLocalizer`] for the localized selection
/// with no localizer, [`LocalizationError::NotLocalized`] and
/// [`LocalizationError::XcpdUnused`] for a localization table under the
/// ask-all selection, and the XCPD errors for an `[xcpd]` table the
/// localizer refuses.
pub fn policy(
    settings: &Settings,
    selection: NodeSelection,
    resolving: Resolving,
    snapshot: &RegistrySnapshot,
) -> Result<LocalizationPolicy, LocalizationError> {
    let federation = &settings.federation;
    match selection {
        NodeSelection::AskAll if federation.localization.is_some() => {
            Err(LocalizationError::NotLocalized)
        }
        NodeSelection::AskAll if settings.xcpd.is_some() => Err(LocalizationError::XcpdUnused),
        NodeSelection::AskAll => Ok(LocalizationPolicy::none()),
        NodeSelection::Localized => {
            let declared = federation.localization.unwrap_or_else(|| {
                let default = config::Localization::default();
                LocalizationSettings {
                    on_failure: default.on_failure,
                    timeout: Duration::from_millis(default.timeout_ms),
                }
            });
            let Resolving { development, pixm } = resolving;
            let (localizer, mode): (Arc<dyn Localizer>, _) =
                match (&settings.xcpd, pixm, development) {
                    (Some(xcpd), _, _) => (
                        Arc::new(xcpd_localizer(xcpd, settings.profile, snapshot)?),
                        XCPD,
                    ),
                    (None, Some(pixm), _) => (pixm, PIXM),
                    (None, None, Some(development)) => (development, DEVELOPMENT_STATIC),
                    (None, None, None) => return Err(LocalizationError::NoLocalizer),
                };
            let policy =
                LocalizationPolicy::new(localizer, mode, declared.on_failure, declared.timeout);
            Ok(match &settings.xcpd {
                Some(xcpd) => policy.audited(xcpd.audit.as_str()),
                None => policy,
            })
        }
    }
}

/// The XCPD localizer `xcpd` describes over the members of `snapshot`, which
/// admits an `http` gateway only when `profile` is development.
fn xcpd_localizer(
    xcpd: &XcpdSettings,
    profile: Profile,
    snapshot: &RegistrySnapshot,
) -> Result<XcpdLocalizer, LocalizationError> {
    let assertion = xcpd
        .assertion
        .as_ref()
        .map(|written| {
            FixedAssertion::from_xml(&written.to_secret_string()).map_err(|_refused| {
                LocalizationError::XcpdAssertion {
                    key: xcpd.assertion_key,
                }
            })
        })
        .transpose()?
        .map(|assertion| -> Arc<dyn AssertionSource> { Arc::new(assertion) });
    let mut communities = BTreeMap::new();
    for (community, member) in &xcpd.communities {
        let member =
            NodeId::new(member.as_str()).map_err(|source| LocalizationError::XcpdMember {
                community: community.clone(),
                source,
            })?;
        communities.insert(community.clone(), member);
    }
    let mut namespaces = BTreeMap::new();
    for (namespace, authority) in &xcpd.namespaces {
        let namespace = IdentifierNamespace::new(namespace.as_str())
            .map_err(LocalizationError::XcpdNamespace)?;
        namespaces.insert(namespace, authority.clone());
    }
    let config = XcpdConfig {
        sender_device: xcpd.sender_device.clone(),
        home_community: xcpd.home_community.clone(),
        gateways: xcpd
            .gateways
            .iter()
            .map(|gateway| GatewayConfig {
                endpoint: gateway.url.clone(),
                device: gateway.device.clone(),
                community: gateway.community.clone(),
            })
            .collect(),
        communities,
        namespaces,
        transport: if profile == Profile::Development {
            Transport::UnencryptedForDevelopment
        } else {
            Transport::Encrypted
        },
        tls: Tls {
            identity: xcpd.client_identity.as_ref().map(Secret::to_secret_string),
            roots: xcpd.trust_roots.clone(),
        },
    };
    let localizer =
        XcpdLocalizer::from_config(config, assertion, snapshot).map_err(LocalizationError::Xcpd)?;
    Ok(match xcpd.audit {
        AuditDestination::Log => localizer.audited(Arc::new(LogAudit)),
        AuditDestination::Off => {
            tracing::warn!(
                "[xcpd] audit = \"off\": no ITI-55 audit message is recorded (development only)"
            );
            localizer
        }
    })
}
