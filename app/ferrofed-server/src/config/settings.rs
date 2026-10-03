// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! The settings the run path holds, resolved from the configuration tree.

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use ferrofed_engine::fanout::Budget;
use ferrofed_engine::onward::Grant;
use ferrofed_engine::onward::keys::KeyRing;
use ferrofed_identity::dev::Profile;
use ferrofed_identity::localizer::OnFailure;
use ferrofed_registry::id::EndpointId;
use ferrofed_registry::secret::{Secret, SecretUrl};
use openehr_federation::aggregate::AggregateFunction;
use openehr_federation::aql::OffsetStrategy;
use openehr_federation::id::FederationId;
use openehr_federation::object::Uri;

use crate::base_path::BasePath;
use crate::config::auth::AuthSettings;
use crate::config::stored_queries::Store;
use crate::config::{DevSection, NodeSelection, RegistryFormat};
use crate::telemetry::Format;

/// The settings the run path holds, with every secret already read.
#[derive(Debug)]
pub struct Settings {
    /// The deployment profile.
    pub profile: Profile,
    /// The HTTP surface.
    pub server: ServerSettings,
    /// The console.
    pub telemetry: TelemetrySettings,
    /// The registry document, when the gateway federates.
    pub registry_document: Option<PathBuf>,
    /// The form the registry document is written in.
    pub registry_format: RegistryFormat,
    /// The mCSD care services directory the registry is read from, when it
    /// is read from one (§15.1, Annex A.5).
    pub registry_directory: Option<DirectorySettings>,
    /// The federated query.
    pub federation: FederationSettings,
    /// The outbound credentials, by endpoint id.
    pub credentials: BTreeMap<EndpointId, Scheme>,
    /// The static development cross-reference, as written.
    pub dev: Option<DevSection>,
    /// The PIXm resolver, with every secret read.
    pub pixm: Option<PixmSettings>,
    /// The XCPD localizer, with every secret and file read.
    pub xcpd: Option<crate::config::xcpd::XcpdSettings>,
    /// The store of the stored-query registry, when it is offered (§12.7).
    pub stored_queries: Option<Store>,
    /// The metrics surface.
    pub metrics: MetricsSettings,
    /// The gateway's signing keys and where they are published, when
    /// `[signing]` is set (§13.1, N25).
    pub signing: Option<SigningSettings>,
}

/// The gateway's signing keys, resolved.
#[derive(Debug, Clone)]
pub struct SigningSettings {
    /// The current key and, while its window lasts, the previous one.
    pub keys: Arc<KeyRing>,
    /// The absolute URL `OPTIONS {base}/` declares for the JWK Set.
    pub jwks_uri: Uri,
    /// How long a client assertion is valid.
    pub assertion_lifetime: Duration,
}

/// The mCSD care services directory the registry is read from, resolved.
#[derive(Debug)]
pub struct DirectorySettings {
    /// The directory's FHIR base URL, already known to parse as an `http` or
    /// `https` URL with no user name or password.
    pub url: SecretUrl,
    /// How the gateway authenticates to it: a bearer token or basic
    /// credentials.
    pub credentials: Option<Scheme>,
    /// How often the changes are asked for.
    pub refresh_interval: Duration,
    /// How long one page of an answer may take.
    pub timeout: Duration,
}

impl DirectorySettings {
    /// Whether `other` names the same directory, credentials, interval and
    /// timeout.
    #[must_use]
    pub fn same_as(&self, other: &Self) -> bool {
        let credentials = match (&self.credentials, &other.credentials) {
            (None, None) => true,
            (Some(Scheme::Bearer(was)), Some(Scheme::Bearer(now))) => was == now,
            (
                Some(Scheme::Basic { user, password }),
                Some(Scheme::Basic {
                    user: now_user,
                    password: now_password,
                }),
            ) => user == now_user && password == now_password,
            _ => false,
        };
        credentials
            && self.url.expose() == other.url.expose()
            && self.refresh_interval == other.refresh_interval
            && self.timeout == other.timeout
    }
}

/// The PIXm resolver, resolved.
#[derive(Debug)]
pub struct PixmSettings {
    /// The PIX Managers.
    pub managers: Vec<PixManagerSettings>,
    /// A client's issuing namespace mapped to a PIX assigning authority.
    pub namespaces: BTreeMap<String, String>,
}

/// One PIX Manager, resolved.
#[derive(Debug)]
pub struct PixManagerSettings {
    /// The Manager's FHIR base URL, already known to parse.
    pub url: SecretUrl,
    /// Each member it resolves, mapped to that member's `ehr_id` domain.
    pub members: BTreeMap<String, String>,
    /// How the gateway authenticates to it.
    pub credentials: Option<Scheme>,
}

/// The federated query, resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FederationSettings {
    /// The federation's own identifier, as declared; a federation refuses to
    /// load without it (§7a.2, N30).
    pub id: Option<FederationId>,
    /// The per-node timeout and the overall budget of each fan-out.
    pub budget: Budget,
    /// The issuing namespace an unqualified patient identifier resolves in.
    pub default_namespace: Option<String>,
    /// How long the resolution bindings of a client session live.
    pub binding_ttl: Duration,
    /// How many `ehr_id`s the `ehr_id` to node index holds.
    pub ehr_index_capacity: NonZeroU32,
    /// How the node set of an undirected patient query is chosen, as
    /// declared; a federation refuses to load without it.
    pub node_selection: Option<NodeSelection>,
    /// The localizer's failure policy and budget, when
    /// `[federation.localization]` is set (§14.1).
    pub localization: Option<LocalizationSettings>,
    /// Whether a request may opt into best-effort completion (§11.4).
    pub best_effort: bool,
    /// How `OFFSET k > 0` is answered across a fan-out, with its bound: the
    /// `paging` an `OPTIONS {base}/` body declares (§11.6.2, §7a.2, N39).
    pub offset: OffsetStrategy,
    /// The aggregate functions recombined across a fan-out: the
    /// `aggregates.decomposable` an `OPTIONS {base}/` body declares
    /// (§11.6.3, §7a.2).
    pub decomposable: BTreeSet<AggregateFunction>,
    /// The one member endpoint that may serve the DEMOGRAPHIC area, or
    /// `None` when that area answers `501` (§7a.1, N32).
    pub demographic_endpoint: Option<EndpointId>,
    /// Whether a template upload may fan out to several members (§12.6,
    /// N43), as `definition.fan_out_template_upload` declares it (§7a.2).
    pub fan_out_template_upload: bool,
    /// Whether a stored-query definition may be distributed to members and
    /// checked there for drift (§12.7, N44), as
    /// `definition.stored_query_fan_out` declares it (§7a.2); only ever on
    /// beside the stored-query registry.
    pub fan_out_stored_queries: bool,
}

/// The localizer's failure policy and budget, resolved (§14.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalizationSettings {
    /// What the gateway does when the localizer does not answer.
    pub on_failure: OnFailure,
    /// How long the localizer may take, already known to end before the
    /// overall budget.
    pub timeout: Duration,
}

/// The HTTP surface, resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerSettings {
    /// The socket address to bind.
    pub listen: SocketAddr,
    /// The path every route sits under (§4.1, N28).
    pub base_path: BasePath,
    /// How long one request may take before the server answers `408`.
    pub request_timeout: Duration,
    /// How long the drain may take after the stop signal.
    pub shutdown_timeout: Duration,
    /// The largest request body the server reads before answering `413`.
    pub body_limit: usize,
    /// Who may call the ITS-REST surface and `OPTIONS {base}/`, from
    /// `[auth]` (§13.1, N25).
    pub auth: AuthSettings,
}

/// The console, resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TelemetrySettings {
    /// The rendering.
    pub format: Format,
    /// The `tracing` filter directive, already known to parse.
    pub filter: String,
}

/// The metrics surface, resolved.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MetricsSettings {
    /// The admin listener's address, already held to loopback unless remote
    /// serving was allowed; `None` runs no listener.
    pub listen: Option<SocketAddr>,
    /// The OTLP collector the metrics are pushed to; `None` pushes nothing.
    pub otlp_endpoint: Option<SecretUrl>,
}

/// The authentication scheme a credentials section resolves to.
///
/// `Debug` redacts every secret, because [`Secret`] does.
#[derive(Debug)]
#[non_exhaustive]
pub enum Scheme {
    /// An RFC 6750 bearer token.
    Bearer(Secret),
    /// RFC 7617 basic authentication.
    Basic {
        /// The user name, which is not a secret.
        user: String,
        /// The password.
        password: Secret,
    },
    /// An OAuth 2.0 client-credentials grant with a JWT client assertion
    /// (RFC 6749 §4.4, RFC 7523 §2.2).
    OAuth2(Box<Grant>),
}

impl Settings {
    /// Whether the gateway federates: a registry document or a care services
    /// directory names its members.
    #[must_use]
    pub fn federates(&self) -> bool {
        self.registry_document.is_some() || self.registry_directory.is_some()
    }

    /// Logs what this process is configured to reach, never a value.
    ///
    /// The line names the endpoints that carry credentials and never the
    /// credentials, so a start-up log states what the process can reach
    /// without stating any of it.
    pub fn log_summary(&self) {
        let endpoints: Vec<&str> = self.credentials.keys().map(EndpointId::as_str).collect();
        let decomposable: Vec<&str> = self
            .federation
            .decomposable
            .iter()
            .map(|function| function.name())
            .collect();
        tracing::info!(
            listen = %self.server.listen,
            base_path = %self.server.base_path,
            profile = ?self.profile,
            registry = self.registry_document.is_some(),
            registry_format = ?self.registry_format,
            registry_directory = self.registry_directory.is_some(),
            registry_refresh_s = self
                .registry_directory
                .as_ref()
                .map(|directory| directory.refresh_interval.as_secs()),
            federation_id = self.federation.id.as_ref().map(FederationId::as_str),
            node_selection = ?self.federation.node_selection,
            best_effort = self.federation.best_effort,
            offset_strategy = self.federation.offset.name(),
            max_offset_window = self.federation.offset.max_window().map(NonZeroU32::get),
            decomposable_aggregates = decomposable.join(","),
            demographic_endpoint = self
                .federation
                .demographic_endpoint
                .as_ref()
                .map(EndpointId::as_str),
            fan_out_template_upload = self.federation.fan_out_template_upload,
            fan_out_stored_queries = self.federation.fan_out_stored_queries,
            pix_managers = self.pixm.as_ref().map_or(0, |pixm| pixm.managers.len()),
            stored_query_backend = self
                .stored_queries
                .as_ref()
                .map(|store| store.backend().name()),
            metrics_listen = self.metrics.listen.map(|address| address.to_string()),
            metrics_otlp_push = self.metrics.otlp_endpoint.is_some(),
            credentials = endpoints.join(","),
            auth_issuers = self.server.auth.issuers.len(),
            auth_edge = matches!(self.server.auth.mode, crate::config::auth::AuthMode::Edge(_)),
            purpose_of_use_required = self.server.auth.purpose_required,
            "configuration resolved"
        );
    }
}
