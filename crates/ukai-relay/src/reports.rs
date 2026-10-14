//! Read-only endpoints for the reports in the relay's working directory.
//!
//! An agent behind the relay is offered no `fs/*`, so the HTML reports and
//! images it writes land in the pod's checkout. These endpoints, next to
//! `/acp` and behind the same token, let the client read them:
//!
//! - `GET /reports?dir=<dir>&dir=…&glob=<glob>&glob=…` lists what matches,
//!   each with the time it last changed, so a pane can follow updates.
//! - `GET /reports/file/<path>?base=<dir>` serves one file. `path` is
//!   relative to `base`, itself relative to the working directory, and
//!   nothing outside `base` is reachable — the confinement a report's web
//!   view gets locally ([`ukai_core::report::ReportBase`]).
//!
//! The relay has no configuration of its own: `dir`, `glob`, and `base` come
//! from the client's `ukai.toml`, and none of them is trusted to stay inside
//! the working directory.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::http::{StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use percent_encoding::percent_decode_str;
use ukai_config::Reports;
use ukai_core::report::{ReportBase, ReportListing, listing};

/// The route serving one file; the rest of the URL path is the file's.
const FILE_PREFIX: &str = "/reports/file/";

/// The endpoints, serving the reports under `root`.
pub fn router(root: PathBuf) -> Router {
    Router::new()
        .route("/reports", get(list))
        .route("/reports/file/{*path}", get(file))
        .with_state(Arc::new(root))
}

/// One answer, decided without axum so it can be tested directly.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Served {
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
}

async fn list(State(root): State<Arc<PathBuf>>, uri: Uri) -> Response {
    list_reports(&root, uri.query().unwrap_or_default()).into_response()
}

async fn file(State(root): State<Arc<PathBuf>>, uri: Uri) -> Response {
    let Some(path) = uri.path().strip_prefix(FILE_PREFIX) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    read_report(&root, path, uri.query().unwrap_or_default()).into_response()
}

fn list_reports(root: &Path, query: &str) -> Served {
    let listing = ReportListing {
        files: listing(root, &settings(query)),
    };
    match serde_json::to_vec(&listing) {
        Ok(body) => Served {
            status: 200,
            content_type: "application/json",
            body,
        },
        Err(error) => {
            tracing::warn!(%error, "could not serialize a report listing");
            Served::message(500, "the listing could not be serialized")
        }
    }
}

fn read_report(root: &Path, path: &str, query: &str) -> Served {
    let base = param(query, "base").unwrap_or_default();
    let Ok(base) = ReportBase::open(root, Path::new(&base)) else {
        return Served::message(404, "no such report directory");
    };
    let reply = base.read(path);
    Served {
        status: reply.status,
        content_type: reply.content_type,
        body: reply.body,
    }
}

/// The `dir` and `glob` values of a query, which are the client's
/// `reports.dirs` and `reports.globs`. Without a `glob` the defaults apply,
/// as they do when the setting is left out of a `ukai.toml`.
fn settings(query: &str) -> Reports {
    let dirs: Vec<PathBuf> = params(query, "dir").map(PathBuf::from).collect();
    let globs: Vec<String> = params(query, "glob").collect();
    let default = Reports::default();
    Reports {
        dirs,
        globs: if globs.is_empty() {
            default.globs
        } else {
            globs
        },
        // Only the client decides what a report may load; the relay just
        // hands out bytes.
        allow_network: false,
    }
}

/// Every value of `name` in a form-encoded query.
fn params<'a>(query: &'a str, name: &'a str) -> impl Iterator<Item = String> + 'a {
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .filter(move |(key, _)| *key == name)
        .filter_map(|(_, value)| form_decode(value))
}

fn param(query: &str, name: &str) -> Option<String> {
    params(query, name).next()
}

/// A form-encoded value: `+` is a space, the rest is percent-decoded.
fn form_decode(value: &str) -> Option<String> {
    let plus = value.replace('+', " ");
    Some(percent_decode_str(&plus).decode_utf8().ok()?.into_owned())
}

impl Served {
    fn message(status: u16, message: &'static str) -> Self {
        Self {
            status,
            content_type: "text/plain; charset=utf-8",
            body: message.as_bytes().to_owned(),
        }
    }
}

impl IntoResponse for Served {
    fn into_response(self) -> Response {
        let status =
            StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        (
            status,
            [
                (header::CONTENT_TYPE, self.content_type),
                (header::CACHE_CONTROL, "no-store"),
                (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            ],
            self.body,
        )
            .into_response()
    }
}

#[cfg(test)]
mod tests;
