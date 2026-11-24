//! The SurrealDB server everything `ukai-store` keeps lives in: one
//! namespace ([`NAMESPACE`]), a database per kind of thing kept (notes in
//! `knowledge`, which session published which report in `reports`, what each
//! turn took in `metrics`), each on
//! a connection of its own, since a connection is in one database at a time.

use surrealdb::Surreal;
use surrealdb::engine::any::Any;
use surrealdb::opt::auth::Root;

pub const NAMESPACE: &str = "ukai";

/// Where the store is.
#[derive(Clone)]
pub struct StoreConfig {
    /// `ws://host:port`, or `mem://` for one that lives with the process.
    pub url: String,
    /// The root user and password, from a Secret. Never logged.
    pub credentials: Option<(String, String)>,
}

impl std::fmt::Debug for StoreConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoreConfig")
            .field("url", &self.url)
            .field("credentials", &self.credentials.as_ref().map(|_| "…"))
            .finish()
    }
}

/// Connect to `database`, sign in, and apply `schema`, whose statements
/// must each leave what exists alone. A WebSocket connection reconnects by
/// itself once made.
pub async fn connect(
    config: &StoreConfig,
    database: &str,
    schema: &str,
) -> Result<Surreal<Any>, surrealdb::Error> {
    let db = surrealdb::engine::any::connect(config.url.as_str()).await?;
    if let Some((username, password)) = &config.credentials {
        db.signin(Root {
            username: username.clone(),
            password: password.clone(),
        })
        .await?;
    }
    db.use_ns(NAMESPACE).use_db(database).await?;
    db.query(schema).await?.check()?;
    Ok(db)
}
