//! A copy of each workspace's reports in an S3 bucket, so they stay
//! readable once its pod is gone.
//!
//! The reports an agent writes live in its pod (see `ukai_acp::reports`),
//! and a pod idles out: after that a report request found nothing to read.
//! With a bucket configured, the gateway keeps a copy: while a workspace's
//! pod is ready it reads the relay's tree of report files every
//! [`SYNC_INTERVAL`] (and once more before a restart stops it) and mirrors
//! what changed, and once the pod is gone it answers report requests from
//! the copy instead of with 410. The client sees the same API; a listing
//! from the copy says when it was taken (`synced`).
//!
//! The gateway holds the bucket's credentials and the pods never do: what an
//! agent writes is untrusted, and copying is the gateway's work, through the
//! relay's read-only report routes. Anything S3 speaks works — RustFS in
//! `deploy/gateway`, AWS S3, MinIO — configured with the standard `AWS_*`
//! variables (`AmazonS3Builder::from_env`) and `UKAI_REPORTS_BUCKET`.
//!
//! Under `<workspace id>/` the bucket holds `manifest.json` ([`Manifest`]:
//! the relay's listing and each copied file's version) and `files/<path>`,
//! the files themselves with their content type.

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures::StreamExt;
use object_store::aws::{AmazonS3Builder, AmazonS3ConfigKey, AwsAuthorizer, AwsCredential};
use object_store::path::Path;
use object_store::{Attribute, Attributes, ObjectStore, ObjectStoreExt, PutOptions, PutPayload};
use serde::{Deserialize, Serialize};
use ukai_acp::reports::{Entry, FILES_PATH, Listing, TREE_PATH, Tree};

use crate::cluster::{Cluster, RelayEndpoint};

/// How often the reports of a running workspace are copied.
pub const SYNC_INTERVAL: Duration = Duration::from_secs(30);

/// The largest file copied; a bigger one is left out of the copy.
pub const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;

const MANIFEST: &str = "manifest.json";
const FILES: &str = "files";

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// What the copy of one workspace holds: the listing the relay gave, and
/// the version of every file copied.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// When the copy was last brought up to date, in seconds since the epoch.
    pub synced: u64,
    pub listing: Vec<Entry>,
    /// Checkout-relative path → the version the relay gave for it.
    pub files: BTreeMap<String, String>,
}

/// Where a copy is taken from: the relay in a workspace's pod, a fake in
/// tests.
pub trait Source {
    fn tree(&self) -> impl Future<Output = Result<Tree, BoxError>> + Send;
    fn listing(&self) -> impl Future<Output = Result<Listing, BoxError>> + Send;
    /// One file by its checkout-relative path, or `None` when the relay
    /// will not serve it or it is too large.
    fn file(&self, path: &str) -> impl Future<Output = Result<Option<Fetched>, BoxError>> + Send;
}

/// One file as a [`Source`] gave it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fetched {
    pub body: Vec<u8>,
    pub content_type: Option<String>,
}

/// A file read back from the copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stored {
    pub body: Vec<u8>,
    pub content_type: Option<String>,
    pub etag: Option<String>,
}

/// What one round of copying did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Synced {
    pub copied: usize,
    pub removed: usize,
}

pub struct Archive {
    store: Arc<dyn ObjectStore>,
    /// The manifest of each workspace as last written, so a round reads the
    /// bucket only the first time.
    manifests: tokio::sync::Mutex<HashMap<String, Manifest>>,
}

impl Archive {
    pub fn new(store: Arc<dyn ObjectStore>) -> Self {
        Self {
            store,
            manifests: tokio::sync::Mutex::default(),
        }
    }

    /// The bucket named `bucket`, reached as the `AWS_*` variables say. An
    /// `http://` endpoint, as an in-cluster RustFS has, is allowed without
    /// also setting `AWS_ALLOW_HTTP`. The bucket is created if it is not
    /// there yet, when the credentials allow it.
    pub async fn connect(bucket: &str) -> Result<Self, BoxError> {
        let mut builder = AmazonS3Builder::from_env().with_bucket_name(bucket);
        let endpoint = builder.get_config_value(&AmazonS3ConfigKey::Endpoint);
        if endpoint
            .as_deref()
            .is_some_and(|e| e.starts_with("http://"))
        {
            builder = builder.with_allow_http(true);
        }
        if builder
            .get_config_value(&AmazonS3ConfigKey::Region)
            .is_none()
        {
            builder = builder.with_region("us-east-1");
        }
        if let Some(endpoint) = &endpoint {
            create_bucket(&builder, endpoint, bucket).await;
        }
        Ok(Self::new(Arc::new(builder.build()?)))
    }

    /// Bring the copy of `workspace` up to date with `source`: copy each file
    /// whose version changed, drop what is gone, and write the manifest.
    pub async fn sync(&self, workspace: &str, source: &impl Source) -> Result<Synced, BoxError> {
        let tree = source.tree().await?;
        let listing = source.listing().await?;
        let known = self.manifests.lock().await.get(workspace).cloned();
        let first = known.is_none();
        let before = match known {
            Some(manifest) => manifest,
            None => self.read_manifest(workspace).await?.unwrap_or_default(),
        };

        let mut done = Synced::default();
        let mut files = BTreeMap::new();
        for entry in &tree.files {
            let Some(key) = file_key(workspace, &entry.path) else {
                continue;
            };
            if before.files.get(&entry.path) == Some(&entry.version) {
                files.insert(entry.path.clone(), entry.version.clone());
                continue;
            }
            let Some(Fetched { body, content_type }) = source.file(&entry.path).await? else {
                continue;
            };
            let mut attributes = Attributes::new();
            if let Some(content_type) = content_type {
                attributes.insert(Attribute::ContentType, content_type.into());
            }
            let options = PutOptions {
                attributes,
                ..PutOptions::default()
            };
            self.store
                .put_opts(&key, PutPayload::from(body), options)
                .await?;
            files.insert(entry.path.clone(), entry.version.clone());
            done.copied += 1;
        }

        // What the copy holds that the tree no longer does. After a restart
        // of the gateway, also what a round cut short left behind.
        let mut stale: Vec<Path> = before
            .files
            .keys()
            .filter(|path| !files.contains_key(*path))
            .filter_map(|path| file_key(workspace, path))
            .collect();
        if first {
            let prefix = Path::from_iter([workspace, FILES]);
            let mut listed = self.store.list(Some(&prefix));
            while let Some(meta) = listed.next().await {
                let location = meta?.location;
                let kept = files
                    .keys()
                    .filter_map(|path| file_key(workspace, path))
                    .any(|key| key == location);
                if !kept && !stale.contains(&location) {
                    stale.push(location);
                }
            }
        }
        for key in stale {
            match self.store.delete(&key).await {
                Ok(()) | Err(object_store::Error::NotFound { .. }) => done.removed += 1,
                Err(error) => return Err(error.into()),
            }
        }

        let listing: Vec<Entry> = listing
            .reports
            .into_iter()
            .filter(|entry| files.contains_key(&entry.path))
            .collect();
        let changed = before.listing != listing || before.files != files;
        let manifest = Manifest {
            synced: if changed || first {
                now()
            } else {
                before.synced
            },
            listing,
            files,
        };
        if changed || first {
            let body = serde_json::to_vec(&manifest)?;
            self.store
                .put(&manifest_key(workspace), PutPayload::from(body))
                .await?;
        }
        self.manifests
            .lock()
            .await
            .insert(workspace.to_owned(), manifest);
        Ok(done)
    }

    /// The listing of `workspace`'s copy, saying when it was taken; `None`
    /// when no copy was ever made.
    pub async fn listing(&self, workspace: &str) -> Result<Option<Listing>, BoxError> {
        Ok(self
            .read_manifest(workspace)
            .await?
            .map(|manifest| Listing {
                reports: manifest.listing,
                synced: Some(manifest.synced),
            }))
    }

    /// One file of `workspace`'s copy, by the percent-encoded URL path a
    /// report's web view asks for; `None` when the copy has no such file or
    /// the path is not one a report may ask for.
    pub async fn file(&self, workspace: &str, url_path: &str) -> Result<Option<Stored>, BoxError> {
        let Some(key) = requested_key(workspace, url_path) else {
            return Ok(None);
        };
        let found = match self.store.get(&key).await {
            Ok(found) => found,
            Err(object_store::Error::NotFound { .. }) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let content_type = found
            .attributes
            .get(&Attribute::ContentType)
            .map(|value| value.as_ref().to_owned());
        let etag = found.meta.e_tag.clone();
        let body = found.bytes().await?.to_vec();
        Ok(Some(Stored {
            body,
            content_type,
            etag,
        }))
    }

    async fn read_manifest(&self, workspace: &str) -> Result<Option<Manifest>, BoxError> {
        match self.store.get(&manifest_key(workspace)).await {
            Ok(found) => Ok(Some(serde_json::from_slice(&found.bytes().await?)?)),
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }
}

fn manifest_key(workspace: &str) -> Path {
    Path::from_iter([workspace, MANIFEST])
}

/// Where the copy of the file at checkout-relative `path` goes: under the
/// workspace's `files/`, segment by segment. `None` for a path with a
/// segment that is not a plain name, which the relay never lists.
pub fn file_key(workspace: &str, path: &str) -> Option<Path> {
    let segments: Vec<&str> = path.split('/').collect();
    let plain = segments
        .iter()
        .all(|segment| !matches!(*segment, "" | "." | "..") && !segment.contains('\\'));
    (plain && !segments.is_empty())
        .then(|| Path::from_iter([workspace, FILES].into_iter().chain(segments)))
}

/// [`file_key`] of a percent-encoded URL path (`/reports/run%201.html`),
/// decoded exactly once, so `%2e%2e` is refused as `..` is.
pub fn requested_key(workspace: &str, url_path: &str) -> Option<Path> {
    let decoded = percent_encoding::percent_decode_str(url_path)
        .decode_utf8()
        .ok()?;
    file_key(workspace, decoded.strip_prefix('/').unwrap_or(&decoded))
}

/// The relay in a workspace's pod, as a [`Source`], through its read-only
/// report routes.
pub struct RelaySource<'a> {
    pub client: &'a reqwest::Client,
    pub endpoint: &'a RelayEndpoint,
}

impl RelaySource<'_> {
    async fn get(&self, suffix: &str) -> Result<reqwest::Response, BoxError> {
        Ok(self
            .client
            .get(self.endpoint.reports_url(suffix))
            .bearer_auth(&self.endpoint.token)
            .timeout(Duration::from_secs(60))
            .send()
            .await?)
    }
}

impl Source for RelaySource<'_> {
    async fn tree(&self) -> Result<Tree, BoxError> {
        let response = self.get(TREE_PATH).await?.error_for_status()?;
        Ok(response.json().await?)
    }

    async fn listing(&self) -> Result<Listing, BoxError> {
        let response = self.get("").await?.error_for_status()?;
        Ok(response.json().await?)
    }

    async fn file(&self, path: &str) -> Result<Option<Fetched>, BoxError> {
        let url_path = ukai_core::report::url_path(std::path::Path::new(path));
        let response = self.get(&format!("{FILES_PATH}{url_path}")).await?;
        let too_large = response
            .content_length()
            .is_some_and(|length| length > MAX_FILE_BYTES);
        if response.status() != reqwest::StatusCode::OK || too_large {
            return Ok(None);
        }
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        Ok(Some(Fetched {
            body: response.bytes().await?.to_vec(),
            content_type,
        }))
    }
}

/// Copy the reports of every ready workspace pod, every [`SYNC_INTERVAL`],
/// for as long as the gateway runs.
pub async fn run(archive: Arc<Archive>, cluster: Cluster) {
    let client = reqwest::Client::new();
    // Workspaces whose last round failed, so a failure is logged once.
    let mut failing: HashMap<String, String> = HashMap::new();
    loop {
        tokio::time::sleep(SYNC_INTERVAL).await;
        let relays = match cluster.ready_relays().await {
            Ok(relays) => relays,
            Err(error) => {
                tracing::warn!(%error, "cannot list the workspace pods to copy reports from");
                continue;
            }
        };
        for (workspace, endpoint) in relays {
            let source = RelaySource {
                client: &client,
                endpoint: &endpoint,
            };
            match archive.sync(&workspace, &source).await {
                Ok(done) => {
                    if failing.remove(&workspace).is_some() || done != Synced::default() {
                        tracing::info!(%workspace, copied = done.copied, removed = done.removed, "copied reports");
                    }
                }
                Err(error) => {
                    let error = error.to_string();
                    if failing.get(&workspace) != Some(&error) {
                        tracing::warn!(%workspace, %error, "cannot copy the workspace's reports");
                        failing.insert(workspace, error);
                    }
                }
            }
        }
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

/// Ask `endpoint` to create `bucket`, as a self-hosted store (RustFS)
/// starts with none. An existing bucket answers 409, which is as good;
/// anything else is logged, and copying will say what is wrong.
async fn create_bucket(builder: &AmazonS3Builder, endpoint: &str, bucket: &str) {
    let value = |key: AmazonS3ConfigKey| builder.get_config_value(&key);
    let (Some(key_id), Some(secret_key)) = (
        value(AmazonS3ConfigKey::AccessKeyId),
        value(AmazonS3ConfigKey::SecretAccessKey),
    ) else {
        return;
    };
    let region = value(AmazonS3ConfigKey::Region).unwrap_or_else(|| "us-east-1".to_owned());
    let credential = AwsCredential {
        key_id,
        secret_key,
        token: value(AmazonS3ConfigKey::Token),
    };
    let url = format!("{}/{bucket}", endpoint.trim_end_matches('/'));
    let Ok(mut request) = axum::http::Request::builder()
        .method("PUT")
        .uri(&url)
        .body(object_store::client::HttpRequestBody::from(Vec::new()))
    else {
        return;
    };
    if let Err(error) =
        AwsAuthorizer::new(&credential, "s3", &region).try_authorize(&mut request, None)
    {
        tracing::warn!(%error, "cannot sign the request creating the reports bucket");
        return;
    }
    let mut headers = reqwest::header::HeaderMap::new();
    for (name, value) in request.headers() {
        headers.insert(name.clone(), value.clone());
    }
    let sent = reqwest::Client::new()
        .put(&url)
        .headers(headers)
        .timeout(Duration::from_secs(10))
        .send()
        .await;
    match sent.map(|response| response.status().as_u16()) {
        Ok(200 | 204) => tracing::info!(%bucket, "created the reports bucket"),
        Ok(409) => {}
        Ok(status) => tracing::warn!(%bucket, status, "cannot create the reports bucket"),
        Err(error) => tracing::warn!(%bucket, %error, "cannot reach the reports bucket"),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use object_store::memory::InMemory;
    use rstest::rstest;
    use ukai_acp::reports::TreeEntry;

    use super::*;

    /// A relay's reports tree in memory.
    #[derive(Default)]
    struct Fake {
        files: Mutex<BTreeMap<String, (String, Vec<u8>)>>,
        listed: Mutex<Vec<String>>,
        fetched: Mutex<Vec<String>>,
    }

    impl Fake {
        fn write(&self, path: &str, version: &str, body: &str) {
            if let Ok(mut files) = self.files.lock() {
                files.insert(
                    path.to_owned(),
                    (version.to_owned(), body.as_bytes().to_vec()),
                );
            }
        }

        fn remove(&self, path: &str) {
            if let Ok(mut files) = self.files.lock() {
                files.remove(path);
            }
        }

        fn list(&self, paths: &[&str]) {
            if let Ok(mut listed) = self.listed.lock() {
                *listed = paths.iter().map(|p| (*p).to_owned()).collect();
            }
        }

        fn fetched(&self) -> Vec<String> {
            self.fetched
                .lock()
                .map(|mut fetched| std::mem::take(&mut *fetched))
                .unwrap_or_default()
        }
    }

    impl Source for Fake {
        async fn tree(&self) -> Result<Tree, BoxError> {
            let files = self.files.lock().map_err(|e| e.to_string())?;
            Ok(Tree {
                files: files
                    .iter()
                    .map(|(path, (version, _))| TreeEntry {
                        path: path.clone(),
                        version: version.clone(),
                    })
                    .collect(),
            })
        }

        async fn listing(&self) -> Result<Listing, BoxError> {
            let listed = self.listed.lock().map_err(|e| e.to_string())?;
            Ok(Listing {
                reports: listed
                    .iter()
                    .map(|path| Entry {
                        path: path.clone(),
                        modified: Some(1),
                    })
                    .collect(),
                synced: None,
            })
        }

        async fn file(&self, path: &str) -> Result<Option<Fetched>, BoxError> {
            self.fetched
                .lock()
                .map_err(|e| e.to_string())?
                .push(path.to_owned());
            let files = self.files.lock().map_err(|e| e.to_string())?;
            Ok(files.get(path).map(|(_, body)| Fetched {
                body: body.clone(),
                content_type: Some("text/html; charset=utf-8".to_owned()),
            }))
        }
    }

    async fn sync(archive: &Archive, relay: &Fake) -> Synced {
        archive
            .sync("w1", relay)
            .await
            .unwrap_or_else(|e| panic!("sync: {e}"))
    }

    async fn read(archive: &Archive, url_path: &str) -> Option<Stored> {
        archive
            .file("w1", url_path)
            .await
            .unwrap_or_else(|e| panic!("file: {e}"))
    }

    #[tokio::test]
    async fn keeps_a_copy_that_follows_the_tree() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let archive = Archive::new(store.clone());
        let relay = Fake::default();
        relay.write("reports/run 1.html", "v1", "<p>one</p>");
        relay.write("reports/img/plot.png", "v1", "png");
        relay.list(&["reports/run 1.html"]);

        assert_eq!(
            sync(&archive, &relay).await,
            Synced {
                copied: 2,
                removed: 0
            }
        );
        let listing = archive
            .listing("w1")
            .await
            .unwrap_or_else(|e| panic!("listing: {e}"))
            .unwrap_or_default();
        assert_eq!(
            listing
                .reports
                .iter()
                .map(|e| e.path.as_str())
                .collect::<Vec<_>>(),
            ["reports/run 1.html"]
        );
        assert!(listing.synced.is_some_and(|synced| synced > 0));
        let page = read(&archive, "/reports/run%201.html").await;
        assert_eq!(
            page.as_ref()
                .map(|p| (p.body.as_slice(), p.content_type.as_deref())),
            Some((&b"<p>one</p>"[..], Some("text/html; charset=utf-8")))
        );
        assert!(page.is_some_and(|p| p.etag.is_some()));

        // Nothing changed: nothing is fetched again.
        relay.fetched();
        assert_eq!(sync(&archive, &relay).await, Synced::default());
        assert!(relay.fetched().is_empty());

        // One file rewritten, one removed.
        relay.write("reports/run 1.html", "v2", "<p>two</p>");
        relay.remove("reports/img/plot.png");
        assert_eq!(
            sync(&archive, &relay).await,
            Synced {
                copied: 1,
                removed: 1
            }
        );
        assert_eq!(relay.fetched(), ["reports/run 1.html"]);
        assert_eq!(
            read(&archive, "/reports/run%201.html")
                .await
                .map(|p| p.body),
            Some(b"<p>two</p>".to_vec())
        );
        assert_eq!(read(&archive, "/reports/img/plot.png").await, None);

        // A gateway that starts over reads the manifest instead of copying
        // everything again, and drops what a cut-short round left behind.
        store
            .put(
                &Path::from_iter(["w1", "files", "reports", "orphan.html"]),
                PutPayload::from_static(b"x"),
            )
            .await
            .unwrap_or_else(|e| panic!("put: {e}"));
        let restarted = Archive::new(store);
        relay.fetched();
        assert_eq!(
            sync(&restarted, &relay).await,
            Synced {
                copied: 0,
                removed: 1
            }
        );
        assert!(relay.fetched().is_empty());
        assert_eq!(read(&restarted, "/reports/orphan.html").await, None);
    }

    /// The same against a real S3 endpoint, which creates the bucket first:
    /// `AWS_ENDPOINT=http://127.0.0.1:9000 AWS_ACCESS_KEY_ID=… AWS_SECRET_ACCESS_KEY=…
    /// cargo test -p ukai-gateway -- --ignored copies_into_a_real_bucket`, e.g.
    /// against `docker run -p 9000:9000 rustfs/rustfs`.
    #[tokio::test]
    #[ignore = "needs an S3 endpoint in AWS_ENDPOINT"]
    async fn copies_into_a_real_bucket() {
        let archive = Archive::connect("ukai-test-reports")
            .await
            .unwrap_or_else(|e| panic!("connect: {e}"));
        let relay = Fake::default();
        relay.write("reports/run 1.html", "v1", "<p>one</p>");
        relay.list(&["reports/run 1.html"]);
        let done = sync(&archive, &relay).await;
        assert_eq!(done.copied, 1);
        let page = read(&archive, "/reports/run%201.html").await;
        assert_eq!(
            page.as_ref()
                .map(|p| (p.body.as_slice(), p.content_type.as_deref())),
            Some((&b"<p>one</p>"[..], Some("text/html; charset=utf-8")))
        );
        let listing = archive
            .listing("w1")
            .await
            .ok()
            .flatten()
            .unwrap_or_default();
        assert_eq!(listing.reports.len(), 1);
        relay.remove("reports/run 1.html");
        assert_eq!(sync(&archive, &relay).await.removed, 1);
        assert_eq!(read(&archive, "/reports/run%201.html").await, None);
    }

    #[tokio::test]
    async fn a_workspace_never_copied_has_no_listing() {
        let archive = Archive::new(Arc::new(InMemory::new()));
        assert_eq!(archive.listing("w1").await.ok().flatten(), None);
        assert_eq!(read(&archive, "/reports/run.html").await, None);
    }

    #[rstest]
    #[case::plain("/reports/run.html", Some("w1/files/reports/run.html"))]
    #[case::encoded("/reports/run%201.html", Some("w1/files/reports/run 1.html"))]
    #[case::parent("/reports/../manifest.json", None)]
    #[case::encoded_parent("/reports/%2e%2e/%2e%2e/w2/manifest.json", None)]
    #[case::empty_segment("/reports//run.html", None)]
    #[case::backslash("/reports/..\\manifest.json", None)]
    #[case::nothing("/", None)]
    fn reads_only_files_of_the_workspaces_copy(
        #[case] url_path: &str,
        #[case] expected: Option<&str>,
    ) {
        assert_eq!(
            requested_key("w1", url_path).map(|key| {
                key.parts()
                    .map(|part| {
                        percent_encoding::percent_decode_str(part.as_ref())
                            .decode_utf8_lossy()
                            .into_owned()
                    })
                    .collect::<Vec<_>>()
                    .join("/")
            }),
            expected.map(str::to_owned),
            "{url_path}"
        );
    }
}
