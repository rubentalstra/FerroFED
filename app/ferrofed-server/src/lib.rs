// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! The FerroFED server library: the run path the `ferrofed` binary and its
//! integration tests share.
//!
//! [`cli`] parses the command line, [`config`] reads the file and the
//! environment into one [`config::settings::Settings`], [`telemetry`] installs the
//! subscriber, [`router`] builds the HTTP surface over [`state::AppState`],
//! and [`serve`] runs it on a bound listener until the process is asked to
//! stop, while [`reload`] replaces the registry on `SIGHUP`. On a terminal,
//! `serve` prints the [`banner`] before the subscriber starts. `main.rs` only
//! hands in the arguments and returns the exit code.
//!
//! Every route sits under the configured base path ([`base_path`]; §4.1,
//! N28). The ITS-REST façade serves the federated query,
//! `POST {base}/v1/query/aql` and its `GET` form ([`facade`], §7), and routes every request to
//! an EHR resource under a path `ehr_id`, the creation of an EHR, and every
//! definition request, to one node ([`facade::route`], §7a.1, §12.4,
//! §12.6), unless the stored-query registry holds the definition
//! ([`facade::stored`], §12.7). A DEMOGRAPHIC request goes to the one
//! endpoint the deployment declared for it when it names that endpoint, and
//! answers `501` where none is declared (§7a.1, §12.6, N32); every other
//! path under `{base}/v1/` answers `501` until its issue lands.
//!
//! [`admission`] is the `admission check` job: one member exercised against
//! the identifier-integrity conditions of §12b.2 (§12b.1, N42a, CP-33a).
//! [`healthcheck`] is the `healthcheck` job a container runtime runs beside
//! the server, and [`health`] answers liveness, readiness and the last
//! observed state of every dependency. [`jwks`] serves the gateway's public
//! signing keys, which its OAuth 2.0 client assertions to the nodes are
//! verified against (§13.1, N25).
//!
//! The server builds for Unix targets only: it drains on `SIGTERM` and
//! reloads on `SIGHUP`, and every release binary and the container image are
//! Linux.
#![doc(test(attr(deny(warnings))))]

// NOTE: no specification governs this: our own design; a non-Unix target is
// refused here, with the reason, before the Unix signal code fails to resolve.
#[cfg(not(unix))]
compile_error!(
    "ferrofed-server builds for Unix targets only: it drains on SIGTERM and reloads on SIGHUP through tokio::signal::unix, and every release binary and the container image are Linux"
);

pub mod admin;
pub mod admission;
pub mod auth;
pub mod banner;
pub mod base_path;
pub mod body;
pub mod cli;
pub mod config;
mod development;
pub mod directory;
pub mod error;
pub mod facade;
pub mod federation;
pub mod health;
pub mod healthcheck;
pub mod jwks;
pub mod localization;
pub mod metrics;
mod onward;
pub mod panic;
pub mod reload;
pub mod request_id;
pub mod request_log;
pub mod state;
pub mod stored;
pub mod telemetry;

use std::future::Future;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Extension, Json, Router};
use clap::Parser;
use ferrofed_engine::outbound_id::OutboundId;
use ferrofed_identity::dev::Profile;
use http::{HeaderMap, Method, StatusCode, Uri};
use openehr_its::rest::routes::{self, Lookup};
use tokio::net::TcpListener;
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::request_id::{PropagateRequestIdLayer, SetRequestIdLayer};
use tower_http::timeout::TimeoutLayer;

use crate::cli::{AdmissionCommand, Cli, Command, ConfigCommand};
use crate::config::Config;
use crate::config::settings::{ServerSettings, Settings};
use crate::directory::DirectoryRegistry;
use crate::federation::Federation;
use crate::health::lifecycle::{Lifecycle, drain_on};
use crate::state::AppState;

/// The exit code of a command line the binary refuses.
///
/// Two is the conventional usage exit, the code a command line uses when the
/// invocation is understood and refused.
pub const EXIT_USAGE: u8 = 2;

/// The exit code of a refused configuration.
///
/// `EX_CONFIG` from `sysexits`
/// (<https://man.freebsd.org/cgi/man.cgi?query=sysexits>), so an orchestrator
/// tells a bad configuration from a failure to serve.
pub const EXIT_CONFIG: u8 = 78;

/// The path prefix the ITS-REST surface lives under (`{base}/v1/…`).
pub const ITS_REST_PREFIX: &str = "/v1/";

/// The release of the `openehr-*` crate family this server is built on.
///
/// The family moves in lockstep, so one version names every member the
/// workspace pins (`openehr-query`, `openehr-its`, `openehr-base`,
/// `openehr-rm`, `openehr-sdt`).
pub const OPENEHR_FAMILY: &str = "0.0.81";

/// Runs the binary with `args` and returns the process exit code.
///
/// `args` is the whole argument vector, the program name included, so the
/// command-line parser reports usage under the right name.
#[must_use]
#[expect(
    clippy::print_stderr,
    reason = "a refused configuration is reported before any log subscriber exists"
)]
pub fn run<I>(args: I) -> ExitCode
where
    I: IntoIterator<Item = String>,
{
    let cli = match Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(error) => return ExitCode::from(clap_exit(&error)),
    };
    let settings = match Config::load(cli.config.as_deref()).and_then(|config| config.resolve()) {
        Ok(settings) => settings,
        Err(error) if cli.command == Command::Healthcheck => {
            // NOTE: no specification governs this: our own design; a runtime
            // reads any exit but 0 and 1 as reserved, so a refusal is unhealthy.
            eprintln!("ferrofed: not ready: {}", chain(&error));
            return ExitCode::FAILURE;
        }
        Err(error) => {
            eprintln!("ferrofed: cannot start: {}", chain(&error));
            return ExitCode::from(EXIT_CONFIG);
        }
    };
    match cli.command {
        Command::Healthcheck => healthcheck_command(&settings),
        Command::Config {
            command: ConfigCommand::Check,
        } => match AppState::check(&settings).and_then(|cleartext| {
            state::admits_callers(&settings, settings.federates()).map(|()| cleartext)
        }) {
            Ok(cleartext) => config_checked(&cleartext),
            Err(error) => {
                eprintln!("ferrofed: cannot start: {}", chain(&error));
                ExitCode::from(EXIT_CONFIG)
            }
        },
        Command::Admission {
            command: AdmissionCommand::Check { endpoint, count },
        } => admission_command(&settings, &endpoint, count),
        Command::Serve => serve_job(settings, cli.config),
    }
}

/// Runs `serve`: the banner on a terminal, the subscriber, the state, and
/// the server, until the process is asked to stop.
///
/// The registry document is read once, before the banner, and the state is
/// built over that read after the subscriber starts, so the banner describes
/// the registry the gateway serves and the build still logs.
#[expect(
    clippy::print_stderr,
    reason = "a refused log filter is reported before any log subscriber exists"
)]
fn serve_job(settings: Settings, config: Option<PathBuf>) -> ExitCode {
    let stdout_is_terminal = std::io::stdout().is_terminal();
    let no_color = std::env::var_os("NO_COLOR");
    let format = settings.telemetry.format;
    let (document, directory) = directory::read_source(&settings);
    if banner::prints(format, stdout_is_terminal) {
        let described = document.as_ref().map(Result::as_ref);
        // NOTE: no specification governs this: our own design; a document that
        // does not read has no endpoint URLs, and the build stops on its error.
        let cleartext = config::transport::check(&settings, described.and_then(Result::ok));
        banner::print(
            &banner::Deployment::of(
                settings.server.base_path.clone(),
                settings.server.listen,
                described,
                settings
                    .stored_queries
                    .as_ref()
                    .map(config::stored_queries::Store::backend),
                settings.profile == Profile::Development,
            )
            .with_cleartext(cleartext.as_deref()),
            format.colour(stdout_is_terminal, no_color.as_deref()),
        );
    }
    if let Err(error) = telemetry::init(
        format,
        &settings.telemetry.filter,
        stdout_is_terminal,
        no_color.as_deref(),
    ) {
        eprintln!("ferrofed: cannot start: {}", chain(&error));
        return ExitCode::from(EXIT_CONFIG);
    }
    panic::install_hook();
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            tracing::error!(%error, "cannot start the runtime");
            return ExitCode::FAILURE;
        }
    };
    // NOTE: no specification governs this: our own design; the OTLP push is a
    // tonic client, which is built inside the runtime it will run on.
    if let Err(error) = state::admits_callers(&settings, settings.federates()) {
        tracing::error!(error = chain(&error), "cannot start");
        return ExitCode::from(EXIT_CONFIG);
    }
    let entered = runtime.enter();
    let state = match AppState::build_read(&settings, document) {
        Ok(state) => Arc::new(match &directory {
            Some(directory) => state.watching(Arc::clone(directory)),
            None => state,
        }),
        Err(error) => {
            tracing::error!(error = chain(&error), "cannot start");
            return ExitCode::from(EXIT_CONFIG);
        }
    };
    drop(entered);
    match serve_command(&runtime, settings, &state, config, directory) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(error = format!("{error:#}"), "cannot serve");
            ExitCode::FAILURE
        }
    }
}

/// Reports a resolved configuration and its `cleartext` credentials, and exits.
#[expect(
    clippy::print_stdout,
    reason = "`config check` answers the person or pipeline that ran it"
)]
fn config_checked(cleartext: &[config::transport::ProtectedSite]) -> ExitCode {
    config::transport::print_warnings(cleartext);
    println!("ferrofed: the configuration is valid");
    ExitCode::SUCCESS
}

/// Runs the admission check against `endpoint` with `count` test EHRs and
/// writes the report to standard output.
///
/// The exit code is `0` when no condition failed, `1` when one did,
/// [`EXIT_USAGE`] for an endpoint the registry does not hold, and
/// [`EXIT_CONFIG`] for a configuration that federates nothing or does not
/// load.
#[expect(
    clippy::print_stdout,
    reason = "`admission check` answers the operator who ran it"
)]
#[expect(
    clippy::print_stderr,
    reason = "a refusal is reported to the operator, with no log subscriber installed"
)]
fn admission_command(settings: &Settings, endpoint: &str, count: u8) -> ExitCode {
    let federation = match Federation::load(settings) {
        Ok(Some(federation)) => federation,
        Ok(None) => {
            eprintln!(
                "ferrofed: cannot check admission: set registry.document, whose members the check exercises"
            );
            return ExitCode::from(EXIT_CONFIG);
        }
        Err(error) => {
            eprintln!("ferrofed: cannot start: {}", chain(&error));
            return ExitCode::from(EXIT_CONFIG);
        }
    };
    if let Err(error) = config::transport::check_and_print(settings, Some(federation.snapshot())) {
        eprintln!("ferrofed: cannot start: {}", chain(&error));
        return ExitCode::from(EXIT_CONFIG);
    }
    let endpoint = match ferrofed_registry::id::EndpointId::new(endpoint) {
        Ok(endpoint) => endpoint,
        Err(error) => {
            eprintln!("ferrofed: cannot check admission: {}", chain(&error));
            return ExitCode::from(EXIT_USAGE);
        }
    };
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("ferrofed: cannot check admission: {error}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(admission::check(&federation, &endpoint, count)) {
        Ok(report) => {
            println!("{report}");
            if report.failed() {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            }
        }
        Err(error) => {
            eprintln!("ferrofed: cannot check admission: {}", chain(&error));
            match error {
                admission::AdmissionError::UnknownEndpoint(_) => ExitCode::from(EXIT_USAGE),
                _ => ExitCode::FAILURE,
            }
        }
    }
}

/// Asks the gateway this configuration describes for its readiness and
/// prints one line.
///
/// The exit code is `0` when readiness answered `200` and `1` otherwise,
/// the two codes a container runtime's health check reads.
#[expect(
    clippy::print_stdout,
    reason = "`healthcheck` answers the runtime or operator that ran it"
)]
#[expect(
    clippy::print_stderr,
    reason = "an unready gateway is reported with no log subscriber installed"
)]
fn healthcheck_command(settings: &Settings) -> ExitCode {
    let address = healthcheck::target(settings.server.listen);
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("ferrofed: not ready: {error}");
            return ExitCode::FAILURE;
        }
    };
    let path = settings.server.base_path.join(healthcheck::READINESS);
    let outcome = runtime.block_on(healthcheck::check(address, &path, healthcheck::TIMEOUT));
    if outcome.is_ready() {
        println!("ferrofed: {address}: {outcome}");
        ExitCode::SUCCESS
    } else {
        eprintln!("ferrofed: {address}: {outcome}");
        ExitCode::FAILURE
    }
}

/// Serves `state` on `runtime` until the process is asked to stop,
/// reloading the registry on `SIGHUP` from `config`, the file `settings`
/// were read from ([`reload`]), and serving `GET /metrics` and the operator's
/// stored-query distribution on the admin listener when `metrics.listen` is
/// set ([`admin`]).
///
/// The metrics are flushed once the gateway has stopped, while the runtime
/// an OTLP push runs on is still up.
fn serve_command(
    runtime: &tokio::runtime::Runtime,
    settings: Settings,
    state: &Arc<AppState>,
    config: Option<PathBuf>,
    directory: Option<Arc<DirectoryRegistry>>,
) -> anyhow::Result<()> {
    let server = settings.server.clone();
    let admin = admin::listener(&settings.metrics, state);
    let outcome = runtime.block_on(async {
        use anyhow::Context;

        tracing::info!(
            version = body::VERSION,
            indicators = state.health().names().join(","),
            "ferrofed starting"
        );
        let listener = TcpListener::bind(server.listen)
            .await
            .with_context(|| format!("binding {}", server.listen))?;
        tracing::info!(listen = %server.listen, "listening");
        if let Some((address, app)) = admin {
            let metrics = TcpListener::bind(address)
                .await
                .with_context(|| format!("binding metrics.listen {address}"))?;
            tracing::info!(
                listen = %address,
                path = metrics::PATH,
                distribute = admin::DISTRIBUTE,
                "serving the admin listener"
            );
            tokio::spawn(async move {
                if let Err(error) = axum::serve(metrics, app).await {
                    tracing::error!(%error, "the metrics listener stopped");
                }
            });
        }
        let reloader = Arc::new(reload::Reloader::new(config, settings, Arc::clone(state)));
        if let Some(directory) = directory {
            tokio::spawn(directory.keep_in_step(Arc::clone(&reloader)));
        }
        tokio::spawn(reload::on_hangup(reloader));
        let app = router(Arc::clone(state), &server);
        state.lifecycle().booted();
        serve(listener, app, &server, state.lifecycle().clone())
            .await
            .context("serving HTTP")?;
        tracing::info!("ferrofed stopped");
        anyhow::Ok(())
    });
    if let Err(error) = state.metrics().shutdown() {
        tracing::warn!(error = chain(&error), "the metrics could not be flushed");
    }
    outcome
}

/// Returns the exit code a command-line refusal deserves.
///
/// `--help` and `--version` are not failures: clap reports both as an error
/// whose kind says the text was printed
/// (<https://docs.rs/clap/4/clap/error/enum.ErrorKind.html>).
#[expect(
    clippy::print_stdout,
    reason = "clap renders help and version to stdout, which is where a person reads them"
)]
#[expect(
    clippy::print_stderr,
    reason = "a usage refusal is reported before any log subscriber exists"
)]
fn clap_exit(error: &clap::Error) -> u8 {
    if error.use_stderr() {
        eprint!("{error}");
        EXIT_USAGE
    } else {
        print!("{error}");
        0
    }
}

/// Returns `error` and every cause behind it as one line.
pub(crate) fn chain(error: &dyn std::error::Error) -> String {
    let mut line = error.to_string();
    let mut cause = error.source();
    while let Some(source) = cause {
        line.push_str(": ");
        line.push_str(&source.to_string());
        cause = source.source();
    }
    line
}

/// Builds the HTTP application over `state`, with the shared middleware.
///
/// Every route sits under the configured base path, `{base}`
/// ([`ServerSettings::base_path`]; §4.1, N28). `GET {base}/` answers a small
/// JSON document naming the product and its version, `OPTIONS {base}/` the
/// federation's self-description ([`facade::options::options_root`]),
/// `GET {base}/health` answers `200` while the process is up,
/// `GET {base}/health/readiness` answers `200` while the process serves
/// and every registered indicator is up and `503` with the phase and each
/// indicator's state otherwise, and `GET {base}/health/dependencies`
/// answers `200` with the last observed state of each member endpoint and of
/// the resolver ([`health::dependencies`]). `GET {base}/.well-known/jwks.json`
/// answers the gateway's public signing keys with no client authentication
/// ([`jwks`]), and `404` when none are configured.
/// `POST {base}/v1/query/aql` answers the federated query when a registry is
/// configured ([`facade::query_aql`]), and so does `GET {base}/v1/query/aql`
/// from its query string ([`facade::query_aql_get`]). Every other path under
/// [`ITS_REST_PREFIX`] is routed or answers `501`, and every path outside it,
/// or outside the base, answers `404`. Under a base other than `/`, the base
/// itself and the base with a trailing `/` are both `{base}/`.
pub fn router(state: Arc<AppState>, server: &ServerSettings) -> Router {
    let surface = Router::new()
        .route("/health", get(liveness))
        .route("/health/readiness", get(readiness))
        .route("/health/dependencies", get(dependencies))
        // NOTE: RFC 7517 §5, §13.1 jwks-discovery: public keys are public material,
        // so the JWK Set stays outside every client authentication layer.
        .route(jwks::JWKS_PATH, get(jwks::jwks))
        .route(
            facade::QUERY_AQL,
            get(facade::query_aql_get)
                .post(facade::query_aql)
                .fallback(unrouted),
        )
        .fallback(unrouted);
    let routes = if server.base_path.is_root() {
        surface.route("/", base_root())
    } else {
        // NOTE: no specification governs this: our own design; `{base}/` is the
        // root N28 names, and `{base}` without the slash is served the same.
        let base = server.base_path.as_str();
        Router::new()
            .route(base, base_root())
            .route(&server.base_path.join("/"), base_root())
            .nest(base, surface)
            .fallback(outside_the_base)
    };
    let guard = Arc::new(auth::Guard::new(
        auth::Gate::new(&server.auth),
        server.base_path.clone(),
    ));
    let guarded = routes
        .with_state(state)
        .layer(axum::middleware::from_fn_with_state(guard, auth::guard));
    with_middleware(guarded, server)
}

/// `GET` and `OPTIONS` of `{base}/` (§7a.2).
fn base_root() -> axum::routing::MethodRouter<Arc<AppState>> {
    get(root).options(facade::options::options_root)
}

/// Every path outside the configured base: `404`, naming no path.
async fn outside_the_base(headers: HeaderMap) -> Response {
    error::fixed(
        error::Code::NotFound,
        request_id::of(&headers).unwrap_or_default(),
    )
}

/// Applies the middleware stack every FerroFED surface carries to `router`.
///
/// Outermost first: the request-id normalizer, the panic renderer, the layer
/// that mints the gateway's outbound id, the layer that sets the exchange id,
/// the layer that propagates the exchange id onto the response, the request
/// log, the panic catcher, the request timeout, and the body-size ceiling.
/// The renderer sits outside the outbound and propagate layers because it
/// reads both ids from the response they have just stamped, and the exchange
/// id is set inside the outbound layer so an unnamed request takes the
/// outbound id as its exchange id ([`request_id`]). The log sits outside the
/// catcher, the timeout and the ceiling, so a request one of them answers,
/// a panicking one included, still gets its line with the status it answered.
pub fn with_middleware(router: Router, server: &ServerSettings) -> Router {
    router
        .layer(RequestBodyLimitLayer::new(server.body_limit))
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            server.request_timeout,
        ))
        .layer(CatchPanicLayer::custom(panic::caught))
        .layer(axum::middleware::from_fn_with_state(
            Arc::new(server.base_path.clone()),
            request_log::log,
        ))
        .layer(PropagateRequestIdLayer::new(request_id::HEADER))
        .layer(SetRequestIdLayer::new(request_id::HEADER, request_id::Mint))
        .layer(axum::middleware::from_fn(request_id::mint_outbound))
        .layer(axum::middleware::map_response(panic::render))
        .layer(axum::middleware::map_request(request_id::strip_illegal))
}

/// `GET /`: the product and the version, as JSON.
async fn root() -> Json<body::Root> {
    Json(body::Root::default())
}

/// `GET /health`: `200` while the process is up.
async fn liveness() -> Json<body::Liveness> {
    Json(body::Liveness {
        state: health::State::Up,
    })
}

/// `GET /health/readiness`: the phase of the process and the state of every
/// registered indicator.
async fn readiness(State(state): State<Arc<AppState>>) -> Response {
    let report = state.health().evaluate().await;
    let readiness = health::Readiness::new(state.lifecycle().phase(), report);
    (readiness.status(), Json(readiness)).into_response()
}

/// `GET /health/dependencies`: the last observed state of each dependency,
/// always `200` ([`AppState::dependencies`]).
async fn dependencies(State(state): State<Arc<AppState>>) -> Json<health::dependencies::Report> {
    Json(state.dependencies())
}

/// Every path no route serves.
///
/// A path under [`ITS_REST_PREFIX`] is part of the ITS-REST surface: a
/// request to an EHR resource under a path `ehr_id`, the creation of an
/// EHR, a definition request the stored-query registry does not hold, and a
/// DEMOGRAPHIC request naming the endpoint the deployment declared for it, is
/// routed to one node ([`facade::route`]; §7a.1, §12.4, §12.6), `OPTIONS`
/// names the methods the gateway serves for the path
/// ([`facade::options::allow`]; §7a.2), and every other path answers `501`
/// (§7a.1, N32), because a `404` would claim the resource does not exist.
/// Every other path answers `404`. No answer of the gateway's own echoes the path.
///
/// A routed request reaches its node under the request's [`OutboundId`],
/// never the client's `x-request-id` (§5.4.1, N33).
async fn unrouted(
    State(state): State<Arc<AppState>>,
    outbound: Option<Extension<OutboundId>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let request_id = request_id::of(&headers).unwrap_or_default();
    let outbound = outbound.map_or_else(OutboundId::mint, |Extension(id)| id);
    let Some(path) = uri
        .path()
        .strip_prefix(ITS_REST_PREFIX.trim_end_matches('/'))
        .filter(|path| path.starts_with('/'))
    else {
        return error::fixed(error::Code::NotFound, request_id);
    };
    if method == Method::OPTIONS {
        return facade::options::allow(&state, path, request_id);
    }
    let mut arrived = facade::route::Arrived {
        method: &method,
        path,
        uri: &uri,
        headers: &headers,
        body,
        request_id,
        outbound,
    };
    let federation = state.federation();
    if let (Some(federation), Some(definitions)) = (federation.as_deref(), state.definitions())
        && let Lookup::Matched(matched) = routes::lookup(&method, path)
    {
        match facade::stored::serve(federation, definitions, &matched, arrived).await {
            Ok(response) => return response,
            Err(unanswered) => arrived = unanswered,
        }
    }
    facade::route::serve(federation.as_deref(), arrived).await
}

/// Serves `app` on an already-bound listener until the process receives
/// `SIGTERM` or `SIGINT`, then drains.
///
/// The signal moves `lifecycle` to draining before the drain starts, so
/// readiness answers `503` from the moment the signal arrives
/// ([`drain_on`]).
///
/// # Errors
/// Returns the I/O error from accepting or serving connections.
pub async fn serve(
    listener: TcpListener,
    app: Router,
    server: &ServerSettings,
    lifecycle: Lifecycle,
) -> std::io::Result<()> {
    serve_until(
        listener,
        app,
        server.shutdown_timeout,
        drain_on(shutdown_signal(), lifecycle),
    )
    .await
}

/// Serves `app` on an already-bound listener until `shutdown` completes, then
/// finishes the requests in flight within `drain`.
///
/// A container runtime stops a container with `SIGTERM` to PID 1 and kills it
/// after a grace period, so the drain is bounded here too: a connection still
/// open when `drain` elapses is dropped and the function returns.
///
/// # Errors
/// Returns the I/O error from accepting or serving connections.
pub async fn serve_until<F>(
    listener: TcpListener,
    app: Router,
    drain: Duration,
    shutdown: F,
) -> std::io::Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    let signalled = Arc::new(tokio::sync::Notify::new());
    let inner = Arc::clone(&signalled);
    let server = axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            shutdown.await;
            inner.notify_one();
        })
        .into_future();
    let mut server = std::pin::pin!(server);
    tokio::select! {
        result = &mut server => return result,
        () = signalled.notified() => {}
    }
    if let Ok(result) = tokio::time::timeout(drain, server).await {
        return result;
    }
    tracing::warn!(
        drain_ms = drain.as_millis(),
        "the drain did not finish in time; the remaining connections are dropped"
    );
    Ok(())
}

/// Completes when the process receives `SIGTERM` or `SIGINT`.
///
/// A failure to install a handler is logged and that arm never completes, so
/// the server keeps serving and the runtime's own kill stays the backstop.
pub async fn shutdown_signal() {
    let interrupt = async {
        match tokio::signal::ctrl_c().await {
            Ok(()) => {}
            Err(error) => {
                tracing::error!(%error, "cannot listen for SIGINT");
                std::future::pending::<()>().await;
            }
        }
    };
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(error) => {
                tracing::error!(%error, "cannot listen for SIGTERM");
                std::future::pending::<()>().await;
            }
        }
    };
    tokio::select! {
        () = interrupt => tracing::info!("SIGINT received, draining"),
        () = terminate => tracing::info!("SIGTERM received, draining"),
    }
}
