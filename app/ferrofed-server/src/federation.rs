// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! What the federated query runs over.
//!
//! The registry snapshot, one node client per endpoint with its onward
//! credentials, the cross-reference resolver, the optional consent
//! pre-filter, the rewrite's context and the
//! fan-out budget (§5.2, §7.1, §11.5).
//!
//! [`Federation::load`] builds it at boot from the resolved settings, and
//! [`Federation::reloaded`] builds its successor when the registry is
//! reloaded ([`crate::reload`]). Without a registry document the gateway
//! federates nothing, so there is no federation and the ITS-REST surface
//! stays unserved.

use std::collections::{BTreeMap, BTreeSet};
use std::num::{NonZeroU32, NonZeroUsize};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use ferrofed_engine::dispatch::{NodeClients, SetupError};
use ferrofed_engine::fanout::Budget;
use ferrofed_identity::binding::{IdentityChange, ResolutionBindings};
use ferrofed_identity::consent::ConsentPrefilter;
use ferrofed_identity::dev::DevCrossRefError;
use ferrofed_identity::directory;
use ferrofed_identity::directory::error::FhirFormError;
use ferrofed_identity::patient::{IdentifierNamespace, PatientRefError};
use ferrofed_identity::pixm::{ManagerConfig, PixAuth, PixmConfigError, PixmResolver};
use ferrofed_identity::resolver::Resolver;
use ferrofed_registry::creating_system::LearnedMap;
use ferrofed_registry::ehr_index::EhrIndex;
use ferrofed_registry::error::{IdError, LoadError};
use ferrofed_registry::id::{EndpointId, NodeId};
use ferrofed_registry::incident::Incident;
use ferrofed_registry::snapshot::{Endpoint, RegistrySnapshot};
use openehr_federation::aggregate::AggregateFunction;
use openehr_federation::aql::{Context, OffsetStrategy, Targeting};
use openehr_federation::dedup::DedupMode;
use openehr_federation::id::FederationId;
use openehr_its::rest::client::ReqwestTransport;

use crate::config::settings::{PixmSettings, Scheme, Settings, SigningSettings};
use crate::config::{NodeSelection, RegistryFormat};
use crate::directory::DirectoryFailure;
use crate::facade::options::{self, DescribeError};
use crate::health::dependencies::Dependencies;
use crate::localization::{self, LocalizationPolicy};
use crate::metrics::nodes::{Instruments, NodeRequests};

/// The federation a server serves the federated query over.
pub struct Federation {
    id: FederationId,
    snapshot: Arc<RegistrySnapshot>,
    clients: NodeClients<ReqwestTransport>,
    resolver: Option<Arc<dyn Resolver>>,
    localization: LocalizationPolicy,
    consent: Option<Arc<dyn ConsentPrefilter>>,
    observed: Arc<Observed>,
    context: Context,
    budget: Budget,
    best_effort: bool,
    demographic: Option<EndpointId>,
    dependencies: Dependencies,
    requests: NodeRequests,
    template_fan_out: bool,
    stored_query_fan_out: bool,
    signing: Option<SigningSettings>,
}

/// What the process learns while it serves, which a registry reload carries
/// over to the federation it builds.
struct Observed {
    bindings: ResolutionBindings,
    index: EhrIndex,
    learned: Mutex<LearnedMap>,
}

impl Observed {
    /// Nothing learned yet: bindings that live `ttl`, and an `ehr_id` index
    /// of `capacity` entries.
    fn new(ttl: std::time::Duration, capacity: NonZeroU32) -> Self {
        Self {
            bindings: ResolutionBindings::new(ttl),
            index: ehr_index(capacity),
            learned: Mutex::new(LearnedMap::new()),
        }
    }
}

/// What a registry reload did to what the process had learned
/// ([`Federation::reconcile`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Reconciled {
    /// The incidents the reload raised, each emitted once when it was raised.
    pub incidents: Vec<Incident>,
    /// How many `ehr_id` index entries named a member that left.
    pub index_dropped: usize,
    /// How many resolution bindings named a member that left.
    pub bindings_dropped: usize,
}

/// A federation that cannot be built from the settings.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum FederationError {
    /// The registry document could not be read or refused to load.
    #[error("the registry document {} could not be loaded", path.display())]
    Registry {
        /// The document named by `registry.document`.
        path: PathBuf,
        /// What the registry reported.
        #[source]
        source: Box<LoadError>,
    },
    /// The registry document in FHIR form could not be read or refused to
    /// load (N19, N20, §15.2).
    #[error("the registry document {} could not be loaded", path.display())]
    FhirRegistry {
        /// The document named by `registry.document`.
        path: PathBuf,
        /// What the FHIR form reported.
        #[source]
        source: Box<FhirFormError>,
    },
    /// The registry could not be read from the directory of `[registry.mcsd]`
    /// (§15.1, §15.2, N19, N20).
    #[error("the registry could not be read from the care services directory")]
    Directory(#[source] Box<DirectoryFailure>),
    /// The `[dev]` table is set but no registry document is, so its rows name
    /// members that do not exist.
    #[error("the [dev] cross-reference needs registry.document, whose members its rows name")]
    DevWithoutRegistry,
    /// The `[dev]` table does not read as the static cross-reference.
    #[error("the [dev] cross-reference is not valid")]
    DevTable(#[source] crate::config::error::Error),
    /// The static cross-reference refuses its rows or the profile.
    #[error("the [dev] cross-reference cannot be enabled")]
    DevCrossRef(#[source] DevCrossRefError),
    /// `federation.demographic_endpoint` is set but no registry document is,
    /// so it names an endpoint that does not exist.
    #[error("federation.demographic_endpoint needs registry.document, whose endpoint it names")]
    DemographicWithoutRegistry,
    /// `federation.demographic_endpoint` names no endpoint of the registry
    /// (§7a.1, §12.6, N32).
    #[error(
        "federation.demographic_endpoint names {endpoint}, which is no endpoint of the registry"
    )]
    DemographicEndpointUnknown {
        /// The endpoint id that was given.
        endpoint: EndpointId,
    },
    /// `[pixm]` is set but no registry document is, so it names members that
    /// do not exist.
    #[error("the [pixm] resolver needs registry.document, whose members it names")]
    PixmWithoutRegistry,
    /// Both `[dev]` and `[pixm]` are set, and exactly one resolver is active
    /// (no specification governs this: our own design).
    #[error("set one resolver: [dev] and [pixm] are both configured")]
    TwoResolvers,
    /// A registry is configured, but `federation.node_selection` is not: how
    /// an undirected patient query finds its nodes is a deployment decision,
    /// declared and never defaulted (§4.3, N4).
    #[error(
        "set federation.node_selection when registry.document is set: \"ask-all\" asks every member's cross-reference (§4.3, N4)"
    )]
    NodeSelectionUndeclared,
    /// A registry is configured, but `federation.id` is not: the
    /// `OPTIONS {base}/` body names the federation (§7a.2, N30), and the
    /// identifier is the deployment's to choose, never defaulted.
    #[error(
        "set federation.id when registry.document is set: the OPTIONS {{base}}/ self-description names the federation (§7a.2, N30)"
    )]
    IdUndeclared,
    /// The `OPTIONS {base}/` self-description cannot be built from the
    /// configuration (§7a.2, N30).
    #[error("the OPTIONS {{base}}/ self-description cannot be built")]
    Describe(#[source] DescribeError),
    /// A `[pixm]` member key is not a node id.
    #[error("pixm.manager[{manager}].members.{key:?} is not a node id")]
    PixmMember {
        /// The Manager's index.
        manager: usize,
        /// The key that was given.
        key: String,
        /// What the id rules reported.
        #[source]
        source: IdError,
    },
    /// A `[pixm.namespaces]` key is not a namespace.
    #[error("pixm.namespaces has an empty namespace")]
    PixmNamespace(#[source] PatientRefError),
    /// The PIXm resolver refuses its Managers or members.
    #[error("the [pixm] resolver cannot be enabled")]
    Pixm(#[source] PixmConfigError),
    /// The localizer of `node_selection = "localized"` cannot be set up
    /// (§14.1, N4).
    #[error("the localizer cannot be set up")]
    Localization(#[source] localization::LocalizationError),
    /// An OAuth 2.0 grant that cannot be used: one in a PIX Manager's
    /// credentials, or a node's with no `[signing]` key (§13.1, N25).
    #[error("{section} names an oauth2 grant it cannot use: only a node takes one, with [signing]")]
    Grant {
        /// The credentials section.
        section: String,
    },
    /// The node clients could not be built.
    #[error("the node clients could not be built")]
    Clients(#[source] SetupError),
    /// The HTTP client every node client shares could not be built.
    #[error("the HTTP client for the nodes could not be built")]
    Transport(#[source] Box<dyn std::error::Error + Send + Sync>),
}

impl Federation {
    /// Builds the federation `settings` describe, or `None` when no registry
    /// document is configured.
    ///
    /// The registry document is read and checked, every endpoint gets a node
    /// client over one shared connection pool, an endpoint with a
    /// `[credentials]` section sends them on every request, and the static
    /// cross-reference is enabled when `[dev]` is set under the development
    /// profile. Without a resolver, a query that names a patient fails closed
    /// (§11.3 covers only an answered lookup; no specification governs this:
    /// our own design).
    ///
    /// # Errors
    /// Returns a [`FederationError`] for a registry document that does not
    /// load, a `[dev]` table that is refused, a credentials key or a
    /// `federation.demographic_endpoint` that names no endpoint of the
    /// registry, and an HTTP client that cannot be built.
    pub fn load(settings: &Settings) -> Result<Option<Self>, FederationError> {
        Self::assemble(settings, read_registry(settings), None)
    }

    /// Builds the federation `settings` describe over `document`, the
    /// registry document [`read_registry`] read from the same settings.
    ///
    /// The boot reads the document once, describes it in the startup banner,
    /// and builds over it here, so the gateway serves the document the banner
    /// described. The checks are [`Federation::load`]'s, in the same order.
    ///
    /// # Errors
    /// Returns the [`FederationError`] [`Federation::load`] returns, the
    /// read's own error included.
    pub fn load_read(
        settings: &Settings,
        document: Option<Result<RegistrySnapshot, FederationError>>,
    ) -> Result<Option<Self>, FederationError> {
        Self::assemble(settings, document, None)
    }

    /// Builds the federation `settings` describe after a registry reload over
    /// `document`, the registry [`read_registry`] read again or a refresh of
    /// the care services directory read, checked as [`Federation::load_read`]
    /// checks it at boot.
    ///
    /// The new federation has its own snapshot, node clients and resolver,
    /// and keeps what this one learned: the resolution bindings, the `ehr_id`
    /// index and the learned `creating_system_id` map, which
    /// [`Federation::reconcile`] then holds to the new snapshot, and records
    /// its node requests through this one's instruments. This federation is
    /// unchanged, so a request that took it finishes on it.
    ///
    /// # Errors
    /// Returns the [`FederationError`] [`Federation::load_read`] returns for
    /// the same settings and document.
    pub fn reloaded(
        &self,
        settings: &Settings,
        document: Option<Result<RegistrySnapshot, FederationError>>,
    ) -> Result<Option<Self>, FederationError> {
        let mut next = Self::assemble(settings, document, Some(Arc::clone(&self.observed)))?;
        if let (Some(next), Some(instruments)) = (next.as_mut(), self.requests.instruments()) {
            next.requests.metered(instruments.clone());
        }
        Ok(next)
    }

    /// Holds what the process learned to this federation's snapshot, after a
    /// reload in which the members `departed` left the registry.
    ///
    /// Every learned `creating_system_id` route the new document contradicts
    /// is withdrawn with its incident ([`LearnedMap::reconcile`]), and every
    /// `ehr_id` index entry and resolution binding naming a member that left
    /// is dropped.
    #[must_use]
    pub fn reconcile(&self, departed: &BTreeSet<NodeId>) -> Reconciled {
        let incidents = self.learned().reconcile(&self.snapshot);
        Reconciled {
            incidents,
            index_dropped: self.observed.index.forget_members(departed),
            bindings_dropped: self.observed.bindings.forget_members(departed),
        }
    }

    /// Builds the federation over `document`, and over `observed` when a
    /// reload carries it over.
    fn assemble(
        settings: &Settings,
        document: Option<Result<RegistrySnapshot, FederationError>>,
        observed: Option<Arc<Observed>>,
    ) -> Result<Option<Self>, FederationError> {
        let Some(document) = document else {
            if settings.dev.is_some() {
                return Err(FederationError::DevWithoutRegistry);
            }
            if settings.pixm.is_some() {
                return Err(FederationError::PixmWithoutRegistry);
            }
            if settings.federation.demographic_endpoint.is_some() {
                return Err(FederationError::DemographicWithoutRegistry);
            }
            return Ok(None);
        };
        let Some(selection) = settings.federation.node_selection else {
            return Err(FederationError::NodeSelectionUndeclared);
        };
        let Some(id) = settings.federation.id.clone() else {
            return Err(FederationError::IdUndeclared);
        };
        let snapshot = document?;
        if let Some(endpoint) = &settings.federation.demographic_endpoint
            && snapshot.endpoint(endpoint).is_none()
        {
            return Err(FederationError::DemographicEndpointUnknown {
                endpoint: endpoint.clone(),
            });
        }
        let mut development = None;
        let mut consent = None;
        let resolver = match (&settings.dev, &settings.pixm) {
            (Some(_), Some(_)) => return Err(FederationError::TwoResolvers),
            (None, None) => None,
            (Some(section), None) => {
                (development, consent) = crate::development::seams(settings, section, &snapshot)?;
                development
                    .clone()
                    .map(|resolver| -> Arc<dyn Resolver> { resolver })
            }
            (None, Some(pixm)) => Some(pixm_resolver(pixm, &snapshot)?),
        };
        let localization = localization::policy(settings, selection, development, &snapshot)
            .map_err(FederationError::Localization)?;
        // NOTE: §11.5 deadlines live on each call; the client's own timeout
        // only backstops a connection the call deadline cannot reach.
        let transport = ReqwestTransport::with_timeout(settings.federation.budget.overall())
            .map_err(|source| FederationError::Transport(Box::new(source)))?;
        let credentials = crate::onward::onward_credentials(settings, &transport)?;
        let clients = NodeClients::from_snapshot(&snapshot, &transport, &credentials)
            .map_err(FederationError::Clients)?;
        let mut context = Context::new(targeting(selection))
            .with_offset_strategy(settings.federation.offset)
            .with_decomposable_aggregates(settings.federation.decomposable.iter().copied());
        if let Some(namespace) = &settings.federation.default_namespace {
            context = context.with_default_namespace(namespace.clone());
        }
        let dependencies =
            Dependencies::new(snapshot.endpoints().map(Endpoint::id), resolver.is_some())
                .with_consent(consent.is_some())
                .with_localizer(localization.localizer().is_some());
        let requests = NodeRequests::new(snapshot.endpoints().map(Endpoint::id));
        let federation = Self {
            id,
            snapshot: Arc::new(snapshot),
            clients,
            resolver,
            localization,
            consent,
            observed: observed.unwrap_or_else(|| {
                Arc::new(Observed::new(
                    settings.federation.binding_ttl,
                    settings.federation.ehr_index_capacity,
                ))
            }),
            context,
            budget: settings.federation.budget,
            best_effort: settings.federation.best_effort,
            demographic: settings.federation.demographic_endpoint.clone(),
            dependencies,
            requests,
            template_fan_out: settings.federation.fan_out_template_upload,
            stored_query_fan_out: settings.federation.fan_out_stored_queries,
            signing: settings.signing.clone(),
        };
        options::describe(&federation, false).map_err(FederationError::Describe)?;
        Ok(Some(federation))
    }

    /// Assembles a federation from parts, for a test that builds its own.
    ///
    /// It offers best-effort completion, as the configuration does by
    /// default; [`Federation::with_best_effort`] withdraws it. It fans no
    /// template upload out, as the configuration does not by default;
    /// [`Federation::with_template_fan_out`] offers it.
    #[must_use]
    pub fn new(
        id: FederationId,
        snapshot: RegistrySnapshot,
        clients: NodeClients<ReqwestTransport>,
        resolver: Option<Arc<dyn Resolver>>,
        context: Context,
        budget: Budget,
    ) -> Self {
        let dependencies =
            Dependencies::new(snapshot.endpoints().map(Endpoint::id), resolver.is_some());
        let requests = NodeRequests::new(snapshot.endpoints().map(Endpoint::id));
        Self {
            id,
            snapshot: Arc::new(snapshot),
            clients,
            resolver,
            localization: LocalizationPolicy::none(),
            consent: None,
            observed: Arc::new(Observed::new(
                std::time::Duration::from_millis(
                    crate::config::Federation::default().binding_ttl_ms,
                ),
                default_index_capacity(),
            )),
            context,
            budget,
            best_effort: crate::config::Federation::default().best_effort,
            demographic: None,
            dependencies,
            requests,
            template_fan_out: crate::config::Federation::default().fan_out_template_upload,
            stored_query_fan_out: crate::config::Federation::default().fan_out_stored_queries,
            signing: None,
        }
    }

    /// This federation, pre-filtering the candidates of every patient query
    /// through `prefilter` at Step 1 (N27a, §13.2.1).
    #[must_use]
    pub fn with_consent_prefilter(mut self, prefilter: Arc<dyn ConsentPrefilter>) -> Self {
        self.consent = Some(prefilter);
        self.dependencies = self.dependencies.with_consent(true);
        self
    }

    /// The gateway's signing keys and where they are published, when
    /// `[signing]` is set: the JWK Set `{base}/.well-known/jwks.json` serves,
    /// and the `auth.jwks_uri` `OPTIONS {base}/` declares (§13.1, N25, N30).
    #[must_use]
    pub fn signing(&self) -> Option<&SigningSettings> {
        self.signing.as_ref()
    }

    /// This federation, offering best-effort completion when `offered` is
    /// `true` (§11.4).
    #[must_use]
    pub fn with_best_effort(mut self, offered: bool) -> Self {
        self.best_effort = offered;
        self
    }

    /// This federation, fanning a template upload out to several members
    /// when `offered` is `true` (§12.6, N43).
    #[must_use]
    pub fn with_template_fan_out(mut self, offered: bool) -> Self {
        self.template_fan_out = offered;
        self
    }

    /// This federation, deriving the node set of an undirected patient query
    /// from the localizer `localization` names (N4, §14.1).
    #[must_use]
    pub fn with_localization(mut self, localization: LocalizationPolicy) -> Self {
        self.context = self.context.with_targeting(Targeting::Localized);
        self.dependencies = self
            .dependencies
            .with_localizer(localization.localizer().is_some());
        self.localization = localization;
        self
    }

    /// The localizer of an undirected patient query, with its failure policy
    /// and budget (§14.1).
    #[must_use]
    pub fn localization(&self) -> &LocalizationPolicy {
        &self.localization
    }

    /// This federation, recording its node requests through `instruments`
    /// ([`NodeRequests`]); a registry reload keeps them.
    #[must_use]
    pub fn metered(mut self, instruments: Instruments) -> Self {
        self.requests.metered(instruments);
        self
    }

    /// The resolution bindings of every client session (§12.5.1 step 2).
    #[must_use]
    pub fn bindings(&self) -> &ResolutionBindings {
        &self.observed.bindings
    }

    /// The `ehr_id` to node index every request of this process shares
    /// (§12.5.1 step 3).
    #[must_use]
    pub fn index(&self) -> &EhrIndex {
        &self.observed.index
    }

    /// The `creating_system_id` mappings learned from answers, which every
    /// request of this process shares (§12.2, N21).
    ///
    /// The guard is held for one lookup or one answer's sightings, never
    /// across an `.await`.
    pub fn learned(&self) -> MutexGuard<'_, LearnedMap> {
        // NOTE: a panic while the lock was held leaves mappings that may be
        // incomplete; each is still only a routing hint, so they stay usable.
        self.observed
            .learned
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// The PMIR hook (track 8, provisional): a merge or split at the identity
    /// source drops every resolution binding it could have made stale, in
    /// every session (§5.2: PMIR for the identity lifecycle; §12.5.1).
    ///
    /// A PMIR subscription (ITI-94) that receives a merge or split (ITI-93)
    /// calls this; until one is configured, the bindings' time-to-live is the
    /// bound. The event names how many bindings went, never an identifier.
    pub fn identity_changed(&self, change: &IdentityChange) -> usize {
        let dropped = self.observed.bindings.identity_changed(change);
        tracing::info!(
            dropped,
            scoped = matches!(change, IdentityChange::Ehrs(_)),
            "an identity change dropped resolution bindings"
        );
        dropped
    }

    /// The last state the gateway observed of each member endpoint and of
    /// the resolver, one slot each.
    ///
    /// Every slot is unknown when the federation is built, on a registry
    /// reload too, because a reload can move an endpoint or swap the resolver.
    #[must_use]
    pub fn dependencies(&self) -> &Dependencies {
        &self.dependencies
    }

    /// The record of the requests sent to each member endpoint, for the
    /// metrics surface.
    #[must_use]
    pub fn requests(&self) -> &NodeRequests {
        &self.requests
    }

    /// The federation's own identifier (§7a.2, N30).
    #[must_use]
    pub fn id(&self) -> &FederationId {
        &self.id
    }

    /// The registry snapshot every query that took this federation runs over.
    #[must_use]
    pub fn snapshot(&self) -> &RegistrySnapshot {
        &self.snapshot
    }

    /// The node clients, one per endpoint.
    #[must_use]
    pub fn clients(&self) -> &NodeClients<ReqwestTransport> {
        &self.clients
    }

    /// The cross-reference resolver, when one is configured.
    #[must_use]
    pub fn resolver(&self) -> Option<&dyn Resolver> {
        self.resolver.as_deref()
    }

    /// The Step-1 consent pre-filter, when one is configured (N27a).
    #[must_use]
    pub fn consent_prefilter(&self) -> Option<&dyn ConsentPrefilter> {
        self.consent.as_deref()
    }

    /// What the deployment adds to the query text: the targeting, the
    /// default issuing namespace, the `OFFSET` strategy and the decomposable
    /// aggregates.
    #[must_use]
    pub fn context(&self) -> &Context {
        &self.context
    }

    /// The per-node timeout and the overall budget of each fan-out.
    #[must_use]
    pub fn budget(&self) -> Budget {
        self.budget
    }

    /// Whether a request may opt into best-effort completion with
    /// `openEHR-federation-completeness: partial` (§11.4, N37), as
    /// `completeness` declares it in `OPTIONS {base}/` (§7a.2).
    #[must_use]
    pub fn best_effort(&self) -> bool {
        self.best_effort
    }

    /// The one member endpoint a DEMOGRAPHIC request may name and be routed
    /// to, or `None` when that area answers `501` (§7a.1, §12.6, N32), as
    /// `its_rest.demographic` declares it in `OPTIONS {base}/` (§7a.2).
    #[must_use]
    pub fn demographic_endpoint(&self) -> Option<&EndpointId> {
        self.demographic.as_ref()
    }

    /// Whether a template upload naming `*` or several endpoints fans out to
    /// each of them (§12.6, N43), as `definition.fan_out_template_upload`
    /// declares it in `OPTIONS {base}/` (§7a.2).
    #[must_use]
    pub fn fans_out_template_upload(&self) -> bool {
        self.template_fan_out
    }

    /// Whether the stored-query registry distributes a definition to the
    /// members a `PUT` names, and reports per member whether its copy
    /// matches (§12.7, N44), as `definition.stored_query_fan_out` declares it
    /// in `OPTIONS {base}/` where the registry is offered (§7a.2).
    #[must_use]
    pub fn fans_out_stored_queries(&self) -> bool {
        self.stored_query_fan_out
    }

    /// How `OFFSET k > 0` is answered across the fan-out, with its bound
    /// (§11.6.2, N39), as `paging` declares it in `OPTIONS {base}/` (§7a.2).
    #[must_use]
    pub fn offset_strategy(&self) -> OffsetStrategy {
        self.context.offset_strategy()
    }

    /// The dedup modes a request may select with `openEHR-federation-dedup`,
    /// the default `none` first (§10, N15), as `dedup` declares them in
    /// `OPTIONS {base}/` (§7a.2).
    #[must_use]
    pub fn dedup_modes() -> &'static [DedupMode] {
        &DedupMode::OFFERED
    }

    /// The aggregate functions recombined across the fan-out, in declaration
    /// order (§11.6.3), as `aggregates.decomposable` declares them in
    /// `OPTIONS {base}/` (§7a.2).
    #[must_use]
    pub fn decomposable_aggregates(&self) -> &BTreeSet<AggregateFunction> {
        self.context.decomposable_aggregates()
    }
}

impl std::fmt::Debug for Federation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Federation")
            .field("id", &self.id)
            .field("endpoints", &self.clients.len())
            .field("resolver", &self.resolver.is_some())
            .field("localization", &self.localization)
            .field(
                "consent",
                &self.consent.as_ref().map(|consent| consent.mode()),
            )
            .field("budget", &self.budget)
            .field("best_effort", &self.best_effort)
            .field("demographic", &self.demographic)
            .field("template_fan_out", &self.template_fan_out)
            .field("stored_query_fan_out", &self.stored_query_fan_out)
            .field("offset_strategy", &self.context.offset_strategy())
            .field(
                "decomposable_aggregates",
                &self.context.decomposable_aggregates(),
            )
            .finish_non_exhaustive()
    }
}

/// Reads and checks the registry document or the care services directory
/// `settings` name, blocking the caller, or returns `None` for neither.
///
/// The read fails with [`FederationError::Registry`] or
/// [`FederationError::FhirRegistry`] for a document that cannot be read or
/// refuses to load, and with [`FederationError::Directory`] for a directory
/// that cannot be read or holds no valid registry; [`Federation::load_read`]
/// stops on that error.
#[must_use]
pub fn read_registry(settings: &Settings) -> Option<Result<RegistrySnapshot, FederationError>> {
    if let Some(directory) = &settings.registry_directory {
        return Some(crate::directory::read(directory));
    }
    let path = settings.registry_document.as_deref()?;
    Some(read_document(path, settings.registry_format))
}

/// Reads the registry document at `path`, written in `format`.
fn read_document(path: &Path, format: RegistryFormat) -> Result<RegistrySnapshot, FederationError> {
    match format {
        RegistryFormat::Toml => {
            RegistrySnapshot::read(path).map_err(|source| FederationError::Registry {
                path: path.to_path_buf(),
                source: Box::new(source),
            })
        }
        RegistryFormat::Fhir => {
            directory::read(path).map_err(|source| FederationError::FhirRegistry {
                path: path.to_path_buf(),
                source: Box::new(source),
            })
        }
    }
}

/// An empty `ehr_id` index of `capacity` entries.
fn ehr_index(capacity: NonZeroU32) -> EhrIndex {
    // NOTE: no specification governs this: our own design; a capacity past
    // `usize` is bounded by `usize`, which only a platform under 32 bits reaches.
    EhrIndex::new(NonZeroUsize::try_from(capacity).unwrap_or(NonZeroUsize::MAX))
}

/// The configuration's default `ehr_id` index capacity.
#[expect(
    clippy::expect_used,
    reason = "the default capacity is a positive literal in the Federation Default impl"
)]
fn default_index_capacity() -> NonZeroU32 {
    NonZeroU32::new(crate::config::Federation::default().ehr_index_capacity)
        .expect("the default ehr_id index capacity should be positive")
}

/// The PIXm resolver `[pixm]` describes over the members of `snapshot`.
fn pixm_resolver(
    pixm: &PixmSettings,
    snapshot: &RegistrySnapshot,
) -> Result<Arc<dyn Resolver>, FederationError> {
    let mut managers = Vec::with_capacity(pixm.managers.len());
    for (index, manager) in pixm.managers.iter().enumerate() {
        let mut members = BTreeMap::new();
        for (key, domain) in &manager.members {
            let member =
                NodeId::new(key.as_str()).map_err(|source| FederationError::PixmMember {
                    manager: index,
                    key: key.clone(),
                    source,
                })?;
            members.insert(member, domain.clone());
        }
        let auth = match &manager.credentials {
            None => PixAuth::None,
            Some(Scheme::Bearer(token)) => PixAuth::Bearer(token.to_secret_string()),
            Some(Scheme::Basic { user, password }) => PixAuth::Basic {
                user: user.clone(),
                password: password.to_secret_string(),
            },
            Some(Scheme::OAuth2(_)) => {
                return Err(FederationError::Grant {
                    section: format!("pixm.manager[{index}].credentials"),
                });
            }
        };
        managers.push(ManagerConfig {
            base: manager.url.clone(),
            auth,
            members,
        });
    }
    let mut namespaces = BTreeMap::new();
    for (namespace, system) in &pixm.namespaces {
        let namespace =
            IdentifierNamespace::new(namespace.as_str()).map_err(FederationError::PixmNamespace)?;
        namespaces.insert(namespace, system.clone());
    }
    let resolver =
        PixmResolver::from_config(managers, namespaces, snapshot).map_err(FederationError::Pixm)?;
    Ok(Arc::new(resolver))
}

/// The rewrite's targeting for an undirected query under the declared node
/// selection (§4.3, N4), which `OPTIONS {base}/` declares as `aql.fan_out`
/// (§7a.2).
fn targeting(selection: NodeSelection) -> Targeting {
    match selection {
        NodeSelection::AskAll => Targeting::AskAll,
        NodeSelection::Localized => Targeting::Localized,
    }
}
