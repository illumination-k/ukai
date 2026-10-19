//! Read-only HTTP access to the reports in the relay's working directory.
//!
//! The agent writes its reports into the checkout it works in, because the
//! relay advertises no `fs/*` capability to it (see [`crate::hub`]). A client
//! cannot read that tree — it is in a pod — so the relay serves it next to
//! `/acp`, and `ukai-gateway` proxies it per workspace:
//!
//! - `GET /reports` … what `[reports]` in the checkout's `ukai.toml` lists,
//!   newest first, with each file's modification time.
//! - `GET /reports/files/<project-relative path>` … one file, with an `ETag`
//!   that changes when it does.
//! - `GET /reports/tree` … every file of the reports tree, assets included,
//!   with its `ETag`: what the gateway copies to keep the reports readable
//!   once the pod is gone.
//!
//! Both are confined by [`ReportTree`]: only files inside a `reports.dirs`
//! entry are served, after resolving symlinks, so a report's relative assets
//! load and nothing else in the checkout is reachable. The configuration is
//! read per request, so editing `ukai.toml` in the pod takes effect without
//! a restart.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::extract::State;
use axum::http::{StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use ukai_acp::reports::{Entry, FILES_PATH, Listing, REPORTS_PATH, TREE_PATH, Tree, TreeEntry};
use ukai_core::report::ReportTree;

/// The relay's working directory: the checkout the agent works in.
#[derive(Debug, Clone)]
pub struct Root(PathBuf);

/// `GET /reports` and `GET /reports/files/{*path}` for the tree at `root`.
pub fn router(root: PathBuf) -> Router {
    Router::new()
        .route(REPORTS_PATH, get(listing))
        .route(&format!("{REPORTS_PATH}{FILES_PATH}/{{*path}}"), get(file))
        .route(&format!("{REPORTS_PATH}{TREE_PATH}"), get(tree_files))
        .with_state(Root(root))
}

/// The reports of the checkout. A configuration that cannot be read lists
/// nothing rather than failing: the client would have nothing to do with the
/// error, and the problem belongs in the relay's log.
async fn listing(State(Root(root)): State<Root>) -> Response {
    let Some(tree) = tree(&root) else {
        return axum::Json(Listing::default()).into_response();
    };
    let reports = tree
        .list()
        .into_iter()
        .map(|listed| Entry {
            path: listed.path.to_string_lossy().into_owned(),
            modified: seconds(listed.modified),
        })
        .collect();
    axum::Json(Listing {
        reports,
        synced: None,
    })
    .into_response()
}

/// Every file of the reports tree, for the gateway to copy. Nothing when
/// the configuration cannot be read, as for the listing.
async fn tree_files(State(Root(root)): State<Root>) -> Response {
    let files = tree(&root)
        .map(|tree| {
            tree.files()
                .into_iter()
                .map(|file| TreeEntry {
                    path: file.path.to_string_lossy().into_owned(),
                    version: file.version,
                })
                .collect()
        })
        .unwrap_or_default();
    axum::Json(Tree { files }).into_response()
}

/// One file of the reports tree. The path is taken from the URI as it
/// arrived, not through a percent-decoding extractor, so [`ReportTree`]
/// decodes it exactly once and sees `%2e%2e` for what it is.
async fn file(State(Root(root)): State<Root>, uri: Uri) -> Response {
    let prefix = format!("{REPORTS_PATH}{FILES_PATH}");
    let Some(path) = uri.path().strip_prefix(&prefix) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(tree) = tree(&root) else {
        return StatusCode::FORBIDDEN.into_response();
    };
    let reply = tree.serve(path);
    let mut response = Response::builder()
        .status(reply.status)
        .header(header::CONTENT_TYPE, reply.content_type)
        // A report is read through a web view that must not keep a stale
        // copy of a file the agent is still rewriting.
        .header(header::CACHE_CONTROL, "no-store")
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff");
    if let Some(etag) = &reply.etag {
        response = response.header(header::ETAG, format!("\"{etag}\""));
    }
    response
        .body(reply.body.into())
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

/// The reports tree of the checkout at `root`, as its `ukai.toml` declares
/// it. `None` when that file is unreadable or the root itself is gone.
fn tree(root: &Path) -> Option<ReportTree> {
    let config = match ukai_config::load_layered(None, root) {
        Ok(config) => config,
        Err(error) => {
            tracing::warn!(%error, root = %root.display(), "cannot read the report settings");
            return None;
        }
    };
    match ReportTree::open(root, &config.reports) {
        Ok(tree) => Some(tree),
        Err(error) => {
            tracing::warn!(%error, root = %root.display(), "cannot read the reports");
            None
        }
    }
}

fn seconds(time: SystemTime) -> Option<u64> {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .map(|since| since.as_secs())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use axum::body::Body;
    use axum::http::Request;
    use rstest::rstest;
    use tempfile::TempDir;
    use tower::ServiceExt;

    use super::*;

    /// A checkout with a reports tree, a secret outside it, and a `ukai.toml`
    /// naming `reports/` the way a project does.
    fn checkout(reports_dirs: &str) -> TempDir {
        let root = tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let write = |path: &str, body: &str| {
            let path = root.path().join(path);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap_or_else(|e| panic!("mkdir: {e}"));
            }
            fs::write(path, body).unwrap_or_else(|e| panic!("write: {e}"));
        };
        write("ukai.toml", &format!("[reports]\ndirs = {reports_dirs}\n"));
        write(".env", "TOKEN=secret");
        write("reports/run 1.html", r#"<img src="img/plot.png">"#);
        write("reports/img/plot.png", "png");
        write("notes.html", "<p>notes</p>");
        root
    }

    async fn get(root: &TempDir, uri: &str) -> (u16, Vec<(String, String)>, Vec<u8>) {
        let response = router(root.path().to_owned())
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .body(Body::empty())
                    .unwrap_or_else(|_| Request::new(Body::empty())),
            )
            .await
            .unwrap_or_else(|e| panic!("request {uri}: {e}"));
        let status = response.status().as_u16();
        let headers = response
            .headers()
            .iter()
            .map(|(name, value)| {
                (
                    name.to_string(),
                    value.to_str().unwrap_or_default().to_owned(),
                )
            })
            .collect();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap_or_default()
            .to_vec();
        (status, headers, body)
    }

    fn header(headers: &[(String, String)], name: &str) -> Option<String> {
        headers
            .iter()
            .find(|(header, _)| header == name)
            .map(|(_, value)| value.clone())
    }

    #[tokio::test]
    async fn lists_the_checkouts_reports() {
        let root = checkout(r#"["reports"]"#);
        let (status, _, body) = get(&root, "/reports").await;
        assert_eq!(status, 200);
        let listing: Listing = serde_json::from_slice(&body).unwrap_or_default();
        let mut paths: Vec<_> = listing.reports.iter().map(|e| e.path.clone()).collect();
        paths.sort();
        assert_eq!(paths, ["reports/img/plot.png", "reports/run 1.html"]);
        assert!(
            listing
                .reports
                .iter()
                .all(|e| e.modified.is_some_and(|m| m > 0)),
            "{listing:?}"
        );
    }

    #[tokio::test]
    async fn names_every_file_of_the_tree_with_its_validator() {
        let root = checkout(r#"["reports"]"#);
        let (status, _, body) = get(&root, "/reports/tree").await;
        assert_eq!(status, 200);
        let tree: Tree = serde_json::from_slice(&body).unwrap_or_default();
        let paths: Vec<_> = tree.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, ["reports/img/plot.png", "reports/run 1.html"]);
        let (_, headers, _) = get(&root, "/reports/files/reports/img/plot.png").await;
        assert_eq!(
            header(&headers, "etag"),
            Some(format!("\"{}\"", tree.files[0].version))
        );
    }

    #[tokio::test]
    async fn lists_nothing_without_report_dirs() {
        let root = checkout("[]");
        let (status, _, body) = get(&root, "/reports").await;
        assert_eq!((status, body.as_slice()), (200, &br#"{"reports":[]}"#[..]));
    }

    #[tokio::test]
    async fn serves_a_report_and_its_assets() {
        let root = checkout(r#"["reports"]"#);
        let (status, headers, body) = get(&root, "/reports/files/reports/run%201.html").await;
        assert_eq!(status, 200);
        assert_eq!(
            header(&headers, "content-type").as_deref(),
            Some("text/html; charset=utf-8")
        );
        assert_eq!(
            header(&headers, "cache-control").as_deref(),
            Some("no-store")
        );
        assert_eq!(
            header(&headers, "x-content-type-options").as_deref(),
            Some("nosniff")
        );
        let etag = header(&headers, "etag").unwrap_or_default();
        assert!(etag.starts_with('"') && etag.ends_with('"'), "{etag}");
        assert_eq!(body, br#"<img src="img/plot.png">"#);

        let (status, headers, body) = get(&root, "/reports/files/reports/img/plot.png").await;
        assert_eq!((status, body.as_slice()), (200, &b"png"[..]));
        assert_eq!(
            header(&headers, "content-type").as_deref(),
            Some("image/png")
        );
    }

    #[tokio::test]
    async fn the_validator_follows_the_file() {
        let root = checkout(r#"["reports"]"#);
        let url = "/reports/files/reports/run%201.html";
        let (_, headers, _) = get(&root, url).await;
        let before = header(&headers, "etag");
        assert!(before.is_some());
        let (_, headers, _) = get(&root, url).await;
        assert_eq!(header(&headers, "etag"), before);
        fs::write(root.path().join("reports/run 1.html"), "<p>again</p>")
            .unwrap_or_else(|e| panic!("write: {e}"));
        let (_, headers, _) = get(&root, url).await;
        assert_ne!(header(&headers, "etag"), before);
    }

    #[rstest]
    // Inside the checkout, outside the reports tree.
    #[case::outside_the_reports_dirs("/reports/files/notes.html", 403)]
    #[case::secret("/reports/files/.env", 403)]
    #[case::parent("/reports/files/reports/../.env", 403)]
    #[case::encoded_parent("/reports/files/reports/%2e%2e/.env", 403)]
    #[case::missing("/reports/files/reports/nope.html", 404)]
    #[tokio::test]
    async fn serves_nothing_outside_the_reports_tree(#[case] uri: &str, #[case] expected: u16) {
        let root = checkout(r#"["reports"]"#);
        let (status, _, _) = get(&root, uri).await;
        assert_eq!(status, expected, "{uri}");
    }

    #[tokio::test]
    async fn an_unreadable_configuration_serves_nothing() {
        let root = checkout(r#"["reports"]"#);
        fs::write(root.path().join("ukai.toml"), "[reports\n")
            .unwrap_or_else(|e| panic!("write: {e}"));
        let (status, _, body) = get(&root, "/reports").await;
        assert_eq!((status, body.as_slice()), (200, &br#"{"reports":[]}"#[..]));
        let (status, _, _) = get(&root, "/reports/files/reports/run%201.html").await;
        assert_eq!(status, 403);
    }

    #[tokio::test]
    async fn a_head_request_answers_without_a_body() {
        let root = checkout(r#"["reports"]"#);
        let response = router(root.path().to_owned())
            .oneshot(
                Request::head("/reports/files/reports/img/plot.png")
                    .body(Body::empty())
                    .unwrap_or_else(|_| Request::new(Body::empty())),
            )
            .await
            .unwrap_or_else(|e| panic!("request: {e}"));
        assert_eq!(response.status().as_u16(), 200);
        assert!(response.headers().contains_key("etag"));
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap_or_default();
        assert!(body.is_empty(), "{body:?}");
    }
}
