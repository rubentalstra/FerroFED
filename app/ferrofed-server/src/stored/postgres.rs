// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! The shared store: one PostgreSQL database every gateway replica uses, so
//! a version one replica stores is held for all of them (§12.7, N44).
//!
//! The definitions live in one table, `ferrofed.stored_query_definition`,
//! whose primary key is the qualified name and the version. An insert is
//! `INSERT … ON CONFLICT (name, version) DO NOTHING`, and the affected-row
//! count says whether it stored the row or found the pair held, so two
//! replicas inserting one pair at once store exactly one of them, in one
//! statement, with no read before the write
//! (<https://www.postgresql.org/docs/18/sql-insert.html#SQL-ON-CONFLICT>).
//! The schema and the table are created when absent each time the store
//! connects, under a transaction-level advisory lock, so replicas starting
//! together do not race on the catalogue
//! (<https://www.postgresql.org/docs/18/explicit-locking.html#ADVISORY-LOCKS>).
//!
//! The client is `tokio-postgres`, on a thread of the store's own with a
//! runtime of its own, because [`DefinitionStore`] is called where a thread
//! may block. TLS is rustls with the platform's roots, as the node client
//! uses, and the connection string's `sslmode` decides whether it is
//! required. No specification governs the storage: our own design.

use std::str::FromStr;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::thread;
use std::time::Duration;

use ferrofed_registry::definition::store::{DefinitionStore, Insertion, StoreError};
use ferrofed_registry::definition::{QueryName, StoredDefinition};
use ferrofed_registry::secret::SecretUrl;
use jiff::Timestamp;
use rustls::ClientConfig;
use rustls_platform_verifier::BuilderVerifierExt;
use tokio::runtime::Runtime;
use tokio_postgres::config::{Host, SslMode};
use tokio_postgres::{Client, Config, Row};
use tokio_postgres_rustls::MakeRustlsConnect;

/// The schema the store's table lives in.
pub const SCHEMA: &str = "ferrofed";

/// The table, qualified by [`SCHEMA`].
pub const TABLE: &str = "ferrofed.stored_query_definition";

/// Creates the schema and the table when absent, one replica at a time.
///
/// The schema is created only when the catalogue lacks it, so a role
/// without `CREATE` on the database can use a schema made for it.
const PREPARE: &str = "BEGIN;
SELECT pg_advisory_xact_lock(x'666572726f666564'::bigint);
DO $$ BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_namespace WHERE nspname = 'ferrofed') THEN
    CREATE SCHEMA ferrofed;
  END IF;
END $$;
CREATE TABLE IF NOT EXISTS ferrofed.stored_query_definition (
  name text NOT NULL,
  version text NOT NULL,
  saved text NOT NULL,
  aql text NOT NULL,
  PRIMARY KEY (name, version)
);
COMMIT;";

/// Stores a definition unless its name and version are held.
const INSERT: &str = "INSERT INTO ferrofed.stored_query_definition (name, version, saved, aql)
VALUES ($1, $2, $3, $4) ON CONFLICT (name, version) DO NOTHING";

/// Every definition.
const SELECT_ALL: &str = "SELECT name, version, saved, aql FROM ferrofed.stored_query_definition";

/// Every version of one name.
const SELECT_NAMED: &str =
    "SELECT name, version, saved, aql FROM ferrofed.stored_query_definition WHERE name = $1";

/// How long a connection, or one statement, may take.
// NOTE: no specification governs this: our own design; a store that does not
// answer fails the one request loudly rather than holding a blocking thread.
const BUDGET: Duration = Duration::from_secs(10);

/// A failure of the store's own, beside the client's errors.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PostgresError {
    /// The connection string does not parse.
    ///
    /// The parser's message is not kept: it may quote a character of the
    /// string, which holds a password.
    #[error("the PostgreSQL connection string does not parse")]
    Unparsable,
    /// The store's thread has stopped, so nothing reaches the database.
    #[error("the stored-query store's thread has stopped")]
    Stopped,
    /// The database did not answer within the budget.
    #[error("the stored-query database did not answer within {}s", .0.as_secs())]
    Timeout(Duration),
    /// An insert reported a row count other than one or none.
    #[error("an insert reported {0} rows, where one or none is possible")]
    Rows(u64),
}

/// The stored-query definitions in one shared PostgreSQL database.
#[derive(Debug)]
pub struct PostgresStore {
    commands: Sender<Command>,
}

/// One call into the store's thread, with where its answer goes.
enum Command {
    /// [`DefinitionStore::insert_if_absent`].
    Insert {
        definition: StoredDefinition,
        reply: SyncSender<Result<Insertion, StoreError>>,
    },
    /// [`DefinitionStore::load`], or with a name
    /// [`DefinitionStore::load_named`].
    Load {
        name: Option<QueryName>,
        reply: SyncSender<Result<Vec<StoredDefinition>, StoreError>>,
    },
}

/// Whether `url` parses as a PostgreSQL connection string, a URL or libpq
/// key/value pairs.
#[must_use]
pub fn parses(url: &SecretUrl) -> bool {
    // NOTE: no specification governs this: our own design; a parse failure is
    // the answer here, and its message may quote the secret.
    Config::from_str(url.expose()).is_ok()
}

/// Whether the connection `url` names sends a password over a network
/// without requiring TLS.
///
/// The driver reads `sslmode` as `disable`, `prefer` (its default) or
/// `require`, and refuses any other value, so only `require` encrypts for
/// certain; the connector then verifies the server against the platform's
/// roots. A connection whose every host is a Unix socket, with no `hostaddr`,
/// crosses no network.
/// A string that does not parse cannot be shown to require TLS, so it counts
/// as exposing its password.
#[must_use]
pub fn exposes_password(url: &SecretUrl) -> bool {
    let Ok(config) = Config::from_str(url.expose()) else {
        return true;
    };
    let networked = !config.get_hostaddrs().is_empty()
        || config
            .get_hosts()
            .iter()
            .any(|host| !matches!(host, Host::Unix(_)));
    config.get_password().is_some()
        && networked
        && !matches!(config.get_ssl_mode(), SslMode::Require)
}

impl PostgresStore {
    /// Connects to the database `url` names and creates the schema and the
    /// table when absent.
    ///
    /// # Errors
    ///
    /// [`StoreError::Backend`] when `url` does not parse, the TLS setup
    /// fails, the database cannot be reached in time, or the schema cannot
    /// be created.
    pub fn open(url: &SecretUrl) -> Result<Self, StoreError> {
        let config =
            Config::from_str(url.expose()).map_err(|_quoted| backend(PostgresError::Unparsable))?;
        let tls = tls().map_err(backend)?;
        let (commands, received) = mpsc::channel();
        let (ready, opened) = mpsc::sync_channel(1);
        thread::Builder::new()
            .name(String::from("ferrofed-stored-queries"))
            .spawn(move || {
                let worker = match Worker::start(config, tls) {
                    Ok(worker) => worker,
                    Err(error) => {
                        if ready.send(Err(error)).is_err() {
                            tracing::debug!("the stored-query store's opener stopped waiting");
                        }
                        return;
                    }
                };
                if ready.send(Ok(())).is_ok() {
                    worker.serve(&received);
                }
            })
            .map_err(backend)?;
        opened
            .recv()
            .map_err(|_stopped| backend(PostgresError::Stopped))??;
        Ok(Self { commands })
    }

    /// Sends `command`, built around a reply channel, and waits for its
    /// answer.
    fn call<T>(
        &self,
        command: impl FnOnce(SyncSender<Result<T, StoreError>>) -> Command,
    ) -> Result<T, StoreError> {
        let (reply, answer) = mpsc::sync_channel(1);
        self.commands
            .send(command(reply))
            .map_err(|_stopped| backend(PostgresError::Stopped))?;
        answer
            .recv()
            .map_err(|_stopped| backend(PostgresError::Stopped))?
    }
}

impl DefinitionStore for PostgresStore {
    fn insert_if_absent(&self, definition: &StoredDefinition) -> Result<Insertion, StoreError> {
        let definition = definition.clone();
        self.call(|reply| Command::Insert { definition, reply })
    }

    fn load(&self) -> Result<Vec<StoredDefinition>, StoreError> {
        self.call(|reply| Command::Load { name: None, reply })
    }

    fn load_named(&self, name: &QueryName) -> Result<Vec<StoredDefinition>, StoreError> {
        let name = Some(name.clone());
        self.call(|reply| Command::Load { name, reply })
    }

    fn is_shared(&self) -> bool {
        true
    }
}

/// The store's thread: a runtime, and the connection it reopens when it
/// closes.
struct Worker {
    runtime: Runtime,
    config: Config,
    tls: MakeRustlsConnect,
    client: Option<Client>,
}

impl Worker {
    /// Builds the runtime and connects once, so a store that cannot be
    /// reached refuses the start.
    fn start(config: Config, tls: MakeRustlsConnect) -> Result<Self, StoreError> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(backend)?;
        let client = runtime.block_on(within(connect(&config, &tls)))?;
        Ok(Self {
            runtime,
            config,
            tls,
            client: Some(client),
        })
    }

    /// Answers every command until the store is dropped.
    fn serve(mut self, commands: &Receiver<Command>) {
        for command in commands {
            let delivered = match command {
                Command::Insert { definition, reply } => {
                    let answer = self.run(|client| Box::pin(insert(client, definition)));
                    reply.send(answer).is_ok()
                }
                Command::Load { name, reply } => {
                    let answer = self.run(|client| Box::pin(load(client, name)));
                    reply.send(answer).is_ok()
                }
            };
            if !delivered {
                tracing::debug!("a stored-query store caller stopped waiting");
            }
        }
    }

    /// Runs `work` on an open connection, reconnecting first when the last
    /// one closed, and drops a connection that did not answer in time.
    fn run<T>(&mut self, work: impl FnOnce(&Client) -> Statement<'_, T>) -> Result<T, StoreError> {
        let client = match self.client.take() {
            Some(client) if !client.is_closed() => client,
            Some(_) | None => self
                .runtime
                .block_on(within(connect(&self.config, &self.tls)))?,
        };
        let answer = self.runtime.block_on(within(work(&client)));
        let timed_out = matches!(
            &answer,
            Err(StoreError::Backend(error))
                if matches!(error.downcast_ref(), Some(PostgresError::Timeout(_)))
        );
        if !timed_out {
            self.client = Some(client);
        }
        answer
    }
}

/// One statement's future, borrowing the connection it runs on.
type Statement<'a, T> = std::pin::Pin<Box<dyn Future<Output = Result<T, StoreError>> + 'a>>;

/// `future`, failed with [`PostgresError::Timeout`] past [`BUDGET`].
async fn within<T>(future: impl Future<Output = Result<T, StoreError>>) -> Result<T, StoreError> {
    tokio::time::timeout(BUDGET, future)
        .await
        .map_err(|_elapsed| backend(PostgresError::Timeout(BUDGET)))?
}

/// Opens a connection, drives it on the store's runtime, and creates the
/// schema and the table when absent.
async fn connect(config: &Config, tls: &MakeRustlsConnect) -> Result<Client, StoreError> {
    let (client, connection) = config.connect(tls.clone()).await.map_err(backend)?;
    tokio::spawn(async move {
        if let Err(error) = connection.await {
            tracing::warn!(error = %error, "the stored-query store's connection closed");
        }
    });
    client.batch_execute(PREPARE).await.map_err(backend)?;
    Ok(client)
}

/// Inserts `definition` unless its name and version are held.
async fn insert(client: &Client, definition: StoredDefinition) -> Result<Insertion, StoreError> {
    let version = definition.version().to_string();
    let saved = definition.saved().to_string();
    let rows = client
        .execute(
            INSERT,
            &[
                &definition.name().as_str(),
                &version.as_str(),
                &saved.as_str(),
                &definition.aql(),
            ],
        )
        .await
        .map_err(backend)?;
    match rows {
        1 => Ok(Insertion::Stored),
        0 => Ok(Insertion::Held),
        other => Err(backend(PostgresError::Rows(other))),
    }
}

/// Every definition, or every version of `name`.
async fn load(
    client: &Client,
    name: Option<QueryName>,
) -> Result<Vec<StoredDefinition>, StoreError> {
    let rows = match &name {
        Some(name) => client.query(SELECT_NAMED, &[&name.as_str()]).await,
        None => client.query(SELECT_ALL, &[]).await,
    }
    .map_err(backend)?;
    rows.iter().map(definition).collect()
}

/// The definition one row holds.
fn definition(row: &Row) -> Result<StoredDefinition, StoreError> {
    let text = |index: usize| row.try_get::<usize, &str>(index).map_err(corrupt);
    Ok(StoredDefinition::new(
        text(0)?.parse().map_err(corrupt)?,
        text(1)?.parse().map_err(corrupt)?,
        text(3)?.to_owned(),
        text(2)?.parse::<Timestamp>().map_err(corrupt)?,
    ))
}

/// The TLS connector: rustls over aws-lc-rs, verifying the server against
/// the platform's roots.
fn tls() -> Result<MakeRustlsConnect, rustls::Error> {
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_platform_verifier()?
        .with_no_client_auth();
    Ok(MakeRustlsConnect::new(config))
}

/// The backend failure `error`.
fn backend(error: impl std::error::Error + Send + Sync + 'static) -> StoreError {
    StoreError::Backend(Box::new(error))
}

/// A held row that does not read as a definition, for `error`.
fn corrupt(error: impl std::error::Error + Send + Sync + 'static) -> StoreError {
    StoreError::Corrupt(Box::new(error))
}

#[cfg(test)]
mod tests {
    use super::{PostgresError, PostgresStore, parses};
    use ferrofed_registry::definition::store::StoreError;
    use ferrofed_registry::secret::SecretUrl;

    #[test]
    fn a_url_and_libpq_pairs_parse_and_garbage_does_not() {
        for parsed in [
            "postgres://ferrofed:secret@db.example.org:5432/ferrofed?sslmode=require",
            "host=db.example.org user=ferrofed password=secret dbname=ferrofed",
        ] {
            assert!(parses(&SecretUrl::new(parsed)), "{parsed}");
        }
        for refused in ["postgres://a:b@host:notaport/db", "host='unterminated"] {
            assert!(!parses(&SecretUrl::new(refused)), "{refused}");
        }
    }

    #[test]
    fn an_unparsable_url_is_refused_without_quoting_it() {
        let refused = PostgresStore::open(&SecretUrl::new("postgres://u:s3cr3t@h:x/d"));
        let Err(StoreError::Backend(error)) = refused else {
            panic!("refused: {refused:?}");
        };
        assert!(error.is::<PostgresError>(), "{error}");
        assert!(!error.to_string().contains("s3cr3t"), "{error}");
    }
}
