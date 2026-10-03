// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! Resolving the configuration tree into the typed settings the run path
//! holds, refusing every bad value under the key that carries it. No
//! specification governs the configuration: our own design.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::num::NonZeroU32;
use std::time::Duration;

use ferrofed_engine::fanout::Budget;
use ferrofed_registry::id::EndpointId;
use ferrofed_registry::secret::SecretUrl;
use openehr_federation::aggregate::AggregateFunction;
use openehr_federation::aql::OffsetStrategy;
use openehr_federation::id::FederationId;

use crate::base_path::BasePath;
use crate::config::error::Error;
use crate::config::secrets::{resolve_credentials, resolve_signing};
use crate::config::settings::{
    DirectorySettings, FederationSettings, LocalizationSettings, MetricsSettings,
    PixManagerSettings, PixmSettings, Scheme, ServerSettings, Settings, TelemetrySettings,
};
use crate::config::{
    COMBINING_MARGIN_MS, Config, Federation, Localization, McsdDirectory, Metrics, NodeSelection,
    OffsetPaging, Pixm, stored_queries,
};

impl Config {
    /// Resolves this tree into the settings the run path holds.
    ///
    /// Every `_file` sibling is read here, so a secret reaches the process
    /// once, at boot, and never sits in the configuration tree.
    ///
    /// # Errors
    /// Returns [`Error::Conflict`] when a value and its `_file` sibling are
    /// both set, [`Error::Secret`] and [`Error::EmptySecret`] when a `_file`
    /// cannot be read or holds nothing, [`Error::Authorization`] and
    /// [`Error::Basic`] for a credential the `Authorization` header cannot
    /// carry, and the value errors
    /// ([`Error::Listen`], [`Error::BasePath`], [`Error::Zero`], [`Error::Filter`],
    /// [`Error::EndpointId`], [`Error::DemographicEndpoint`], [`Error::Missing`],
    /// [`Error::Scheme`], [`Error::NoScheme`], [`Error::Budget`],
    /// [`Error::LocalizationBudget`], [`Error::Url`]),
    /// each naming the key that carries the fault, the stored-query store
    /// errors of [`stored_queries::resolve`], and
    /// [`Error::StoredQueryFanOutWithoutRegistry`] and
    /// [`Error::StoredQueryFanOutReadOnly`] when definitions would be
    /// distributed with no registry, or no `PUT`, to distribute from. The
    /// metrics surface refuses a remote listener without `metrics.allow_remote`
    /// ([`Error::MetricsRemote`]), a listener on `server.listen`
    /// ([`Error::MetricsShared`]) and a collector that is no `http://` URL
    /// ([`Error::OtlpScheme`]). An OAuth 2.0 grant refuses a missing key, a
    /// scope outside the SMART on openEHR `system` grammar ([`Error::Scope`]),
    /// an unusable endpoint or resource ([`Error::Grant`]), a PIX Manager
    /// section ([`Error::GrantNotHere`]) and a missing `[signing]`
    /// ([`Error::GrantWithoutSigning`]); `[signing]` refuses a key that is no
    /// ES384 key ([`Error::SigningKey`]), an assertion lifetime past five
    /// minutes ([`Error::AssertionLifetime`]), an overlap window shorter than
    /// that lifetime plus the nodes' cache time ([`Error::RotationOverlap`]),
    /// and a `jwks_uri` that is no `http` or `https` URL ([`Error::HttpUrl`]).
    /// `[auth]` is refused as
    /// [`Auth::resolve`](crate::config::auth::Auth::resolve) refuses it.
    pub fn resolve(&self) -> Result<Settings, Error> {
        let listen = self
            .server
            .listen
            .parse::<SocketAddr>()
            .map_err(|source| Error::Listen {
                key: String::from("server.listen"),
                source,
            })?;
        let base_path = self
            .server
            .base_path
            .parse::<BasePath>()
            .map_err(|source| Error::BasePath {
                key: String::from("server.base_path"),
                source,
            })?;
        let request_timeout =
            positive_ms("server.request_timeout_ms", self.server.request_timeout_ms)?;
        let shutdown_timeout = positive_ms(
            "server.shutdown_timeout_ms",
            self.server.shutdown_timeout_ms,
        )?;
        if self.server.body_limit_bytes == 0 {
            return Err(Error::Zero {
                key: String::from("server.body_limit_bytes"),
            });
        }
        tracing_subscriber::EnvFilter::try_new(&self.telemetry.filter)
            .map_err(|source| Error::Filter { source })?;
        let mut credentials = BTreeMap::new();
        for (endpoint, section) in &self.credentials {
            let id = EndpointId::new(endpoint.as_str()).map_err(|source| Error::EndpointId {
                key: endpoint.clone(),
                source,
            })?;
            let scheme = resolve_credentials(&format!("credentials.{endpoint}"), section)?;
            credentials.insert(id, scheme);
        }
        let signing = self.signing.as_ref().map(resolve_signing).transpose()?;
        // NOTE: §13.1, N25: the client assertion of every grant is signed with the
        // gateway's key, so a grant without one is refused at load.
        if signing.is_none()
            && let Some(endpoint) = credentials
                .iter()
                .find(|(_, scheme)| matches!(scheme, Scheme::OAuth2(_)))
                .map(|(endpoint, _)| endpoint)
        {
            return Err(Error::GrantWithoutSigning {
                section: format!("credentials.{endpoint}.oauth2"),
            });
        }
        let federation = self.resolve_federation(request_timeout)?;
        let pixm = self.pixm.as_ref().map(resolve_pixm).transpose()?;
        let xcpd = crate::config::xcpd::resolve(self)?;
        if self.registry.document.is_some() && self.registry.mcsd.is_some() {
            return Err(Error::TwoRegistrySources);
        }
        let registry_directory = self
            .registry
            .mcsd
            .as_ref()
            .map(resolve_directory)
            .transpose()?;
        let stored_queries = stored_queries::resolve(self)?;
        let metrics = resolve_metrics(&self.metrics, listen)?;
        // NOTE: §12.7 stored-query-fanout, N44: definition fan-out is a facility
        // of the registry and is never offered without it.
        if federation.fan_out_stored_queries {
            match stored_queries.as_ref().map(stored_queries::Store::backend) {
                None => return Err(Error::StoredQueryFanOutWithoutRegistry),
                Some(stored_queries::Backend::Files) => {
                    return Err(Error::StoredQueryFanOutReadOnly);
                }
                Some(_) => {}
            }
        }
        Ok(Settings {
            profile: self.profile,
            server: ServerSettings {
                listen,
                base_path,
                request_timeout,
                shutdown_timeout,
                body_limit: self.server.body_limit_bytes,
                auth: self.auth.resolve()?,
            },
            telemetry: TelemetrySettings {
                format: self.telemetry.format,
                filter: self.telemetry.filter.clone(),
            },
            registry_document: self.registry.document.clone(),
            registry_format: self.registry.format,
            registry_directory,
            federation,
            credentials,
            dev: self.dev.clone(),
            pixm,
            xcpd,
            stored_queries,
            metrics,
            signing,
        })
    }

    /// Resolves `[federation]`: both budgets positive (§11.5), and the overall
    /// one plus [`COMBINING_MARGIN_MS`] ending before `request_timeout` when a
    /// registry is configured.
    fn resolve_federation(&self, request_timeout: Duration) -> Result<FederationSettings, Error> {
        let per_node = positive_ms(
            "federation.per_node_timeout_ms",
            self.federation.per_node_timeout_ms,
        )?;
        let overall = positive_ms(
            "federation.overall_timeout_ms",
            self.federation.overall_timeout_ms,
        )?;
        // NOTE: §11.5, the budget only bounds a fan-out, so it is held below the
        // request timeout, by the combining margin, only when the gateway federates.
        if self.registry.configured()
            && request_timeout <= overall.saturating_add(Duration::from_millis(COMBINING_MARGIN_MS))
        {
            return Err(Error::Budget {
                overall_ms: self.federation.overall_timeout_ms,
                margin_ms: COMBINING_MARGIN_MS,
                request_ms: self.server.request_timeout_ms,
            });
        }
        let budget = Budget::new(per_node, overall).map_err(|_zero| Error::Zero {
            key: String::from("federation"),
        })?;
        if let Some(namespace) = &self.federation.default_namespace
            && namespace.is_empty()
        {
            return Err(Error::Missing {
                key: String::from("federation.default_namespace"),
            });
        }
        let named = self.federation.id.as_deref().map(FederationId::new);
        let id = named.transpose().map_err(|_empty| Error::Missing {
            key: String::from("federation.id"),
        })?;
        let binding_ttl = positive_ms("federation.binding_ttl_ms", self.federation.binding_ttl_ms)?;
        let ehr_index_capacity =
            NonZeroU32::new(self.federation.ehr_index_capacity).ok_or_else(|| Error::Zero {
                key: String::from("federation.ehr_index_capacity"),
            })?;
        let max_window =
            NonZeroU32::new(self.federation.max_offset_window).ok_or_else(|| Error::Zero {
                key: String::from("federation.max_offset_window"),
            })?;
        let demographic_endpoint = self
            .federation
            .demographic_endpoint
            .as_deref()
            .map(EndpointId::new)
            .transpose()
            .map_err(|source| Error::DemographicEndpoint { source })?;
        let offset = match self.federation.offset_strategy {
            OffsetPaging::Reject => OffsetStrategy::Reject,
            OffsetPaging::Bounded => OffsetStrategy::Bounded { max_window },
        };
        let localization = match (
            &self.federation.localization,
            self.federation.node_selection,
        ) {
            (Some(section), _) => Some(resolve_localization(section, &self.federation)?),
            (None, Some(NodeSelection::Localized)) => Some(resolve_localization(
                &Localization::default(),
                &self.federation,
            )?),
            (None, _) => None,
        };
        Ok(FederationSettings {
            localization,
            id,
            budget,
            default_namespace: self.federation.default_namespace.clone(),
            binding_ttl,
            ehr_index_capacity,
            node_selection: self.federation.node_selection,
            best_effort: self.federation.best_effort,
            offset,
            decomposable: self
                .federation
                .decomposable_aggregates
                .iter()
                .copied()
                .map(AggregateFunction::from)
                .collect(),
            demographic_endpoint,
            fan_out_template_upload: self.federation.fan_out_template_upload,
            fan_out_stored_queries: self.federation.fan_out_stored_queries,
        })
    }
}

/// Resolves `[pixm]`: every Manager URL parses and carries no userinfo, and
/// every secret is read.
fn resolve_pixm(pixm: &Pixm) -> Result<PixmSettings, Error> {
    let mut managers = Vec::with_capacity(pixm.manager.len());
    for (index, manager) in pixm.manager.iter().enumerate() {
        let key = format!("pixm.manager[{index}]");
        let url = url::Url::parse(manager.url.expose()).map_err(|source| Error::Url {
            key: format!("{key}.url"),
            source,
        })?;
        // NOTE: no specification governs this: our own design; as the registry
        // refuses it on an endpoint URL, a credential goes in its own section.
        if !url.username().is_empty() || url.password().is_some() {
            return Err(Error::UrlCredentials {
                key: format!("{key}.url"),
                section: format!("{key}.credentials"),
            });
        }
        let section = format!("{key}.credentials");
        let credentials = manager
            .credentials
            .as_ref()
            .map(|credentials| resolve_credentials(&section, credentials))
            .transpose()?;
        if matches!(credentials, Some(Scheme::OAuth2(_))) {
            return Err(Error::GrantNotHere { section });
        }
        managers.push(PixManagerSettings {
            url: manager.url.clone(),
            members: manager.members.clone(),
            credentials,
        });
    }
    Ok(PixmSettings {
        managers,
        namespaces: pixm.namespaces.clone(),
    })
}

/// Resolves `[registry.mcsd]`: an `http` or `https` base URL with no user name
/// or password, a bearer token or basic credentials, and a positive interval
/// and timeout.
fn resolve_directory(directory: &McsdDirectory) -> Result<DirectorySettings, Error> {
    let key = "registry.mcsd.url";
    if directory.url.is_empty() {
        return Err(Error::Missing {
            key: key.to_owned(),
        });
    }
    let url = url::Url::parse(directory.url.expose()).map_err(|source| Error::Url {
        key: key.to_owned(),
        source,
    })?;
    // NOTE: no specification governs this: our own design; as on a PIX Manager
    // URL, a credential goes in its own section and never in the URL.
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(Error::HttpUrl {
            key: key.to_owned(),
        });
    }
    let section = String::from("registry.mcsd.credentials");
    let credentials = directory
        .credentials
        .as_ref()
        .map(|credentials| resolve_credentials(&section, credentials))
        .transpose()?;
    if matches!(credentials, Some(Scheme::OAuth2(_))) {
        return Err(Error::GrantNotHere { section });
    }
    let refresh_interval = Duration::from_secs(directory.refresh_interval_s);
    if refresh_interval.is_zero() {
        return Err(Error::Zero {
            key: String::from("registry.mcsd.refresh_interval_s"),
        });
    }
    Ok(DirectorySettings {
        url: directory.url.clone(),
        credentials,
        refresh_interval,
        timeout: positive_ms("registry.mcsd.timeout_ms", directory.timeout_ms)?,
    })
}

/// Resolves `[metrics]`: the listener on a loopback address unless
/// `allow_remote` is set and never on `server`, the gateway's own address,
/// and the OTLP collector an `http://` URL.
fn resolve_metrics(metrics: &Metrics, server: SocketAddr) -> Result<MetricsSettings, Error> {
    let listen = metrics
        .listen
        .as_deref()
        .map(|listen| {
            listen
                .parse::<SocketAddr>()
                .map_err(|source| Error::Listen {
                    key: String::from("metrics.listen"),
                    source,
                })
        })
        .transpose()?;
    if let Some(address) = listen {
        // NOTE: no specification governs this: our own design; the listener has
        // no authentication, so it stays on the host unless the operator says.
        if !address.ip().is_loopback() && !metrics.allow_remote {
            return Err(Error::MetricsRemote { address });
        }
        if address == server {
            return Err(Error::MetricsShared { address });
        }
    }
    let otlp_endpoint = metrics
        .otlp_endpoint
        .as_ref()
        .map(|endpoint| {
            url::Url::parse(endpoint.expose()).map_err(|source| Error::Url {
                key: String::from("metrics.otlp_endpoint"),
                source,
            })
        })
        .transpose()?;
    if otlp_endpoint
        .as_ref()
        .is_some_and(|endpoint| endpoint.scheme() != "http")
    {
        return Err(Error::OtlpScheme);
    }
    Ok(MetricsSettings {
        listen,
        otlp_endpoint: otlp_endpoint.map(|endpoint| SecretUrl::new(String::from(endpoint))),
    })
}

/// Returns the duration `millis` names, refusing zero under `key`.
fn positive_ms(key: &str, millis: u64) -> Result<Duration, Error> {
    if millis == 0 {
        return Err(Error::Zero {
            key: key.to_owned(),
        });
    }
    Ok(Duration::from_millis(millis))
}

/// Resolves `[federation.localization]`: a positive budget that ends before
/// the overall one, of which it is a part (§11.5, §14.1).
fn resolve_localization(
    section: &Localization,
    federation: &Federation,
) -> Result<LocalizationSettings, Error> {
    let timeout = positive_ms("federation.localization.timeout_ms", section.timeout_ms)?;
    if section.timeout_ms >= federation.overall_timeout_ms {
        return Err(Error::LocalizationBudget {
            timeout_ms: section.timeout_ms,
            overall_ms: federation.overall_timeout_ms,
        });
    }
    Ok(LocalizationSettings {
        on_failure: section.on_failure,
        timeout,
    })
}
