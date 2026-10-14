//! Reports: HTML pages and images an agent wrote, shown in a pane.
//!
//! A report pane gets a web view of its own (no IPC to the app) that loads
//! the file through a custom protocol. What the page can load is confined to
//! the report's *base*: the `reports.dirs` entry holding it, else the file's
//! own directory. So `../img/plot.png` works inside a reports tree, and
//! nothing outside it is reachable — after resolving symlinks, as with ACP
//! `fs/*`.
//!
//! The files live either in the project root ([`ReportFile`], read here) or
//! in a gateway agent's workspace ([`RemoteReport`], fetched from the
//! `ukai-relay` in its pod through the gateway). Both decide the entry URL,
//! the content type, and the CSP the same way; the confinement runs wherever
//! the files are, in [`ReportBase`].

mod remote;

use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use percent_encoding::{AsciiSet, CONTROLS, percent_decode_str, utf8_percent_encode};
use ukai_config::Reports;

pub use remote::{Fetch, RemoteReport};
/// The listing a relay answers with, shared with its client.
pub use ukai_acp::{ReportEntry, ReportListing};

/// How many files [`list`] returns at most, so a huge tree cannot stall the UI.
pub const MAX_LISTED: usize = 500;

/// Query that asks for the page framing an image.
const IMAGE_VIEW_QUERY: &str = "ukai-view";

/// What a pane can render.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportKind {
    Html,
    Image,
}

impl ReportKind {
    pub fn of(path: &Path) -> Option<Self> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        match ext.as_str() {
            "html" | "htm" => Some(Self::Html),
            "svg" | "png" | "jpg" | "jpeg" | "gif" | "webp" | "avif" | "bmp" | "ico" => {
                Some(Self::Image)
            }
            _ => None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ReportError {
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{} is not inside the project", .0.display())]
    OutsideProject(PathBuf),
    #[error("{} is neither HTML nor an image", .0.display())]
    Unsupported(PathBuf),
}

/// Files under `reports.dirs` matching `reports.globs`, relative to `root`,
/// newest first. Hidden directories are skipped; unreadable ones are too, as
/// are directories that leave `root` (the relay is sent its client's
/// `reports` settings, so it cannot assume they were validated).
pub fn listing(root: &Path, reports: &Reports) -> Vec<ReportEntry> {
    let mut builder = globset::GlobSetBuilder::new();
    for glob in &reports.globs {
        // Validation reports bad globs; here they just match nothing.
        if let Ok(glob) = globset::Glob::new(glob) {
            builder.add(glob);
        }
    }
    let (Ok(globs), Ok(root)) = (builder.build(), root.canonicalize()) else {
        return Vec::new();
    };
    let mut found: Vec<ReportEntry> = Vec::new();
    for dir in confined_dirs(&root, &reports.dirs) {
        let mut pending = vec![PathBuf::new()];
        while let Some(relative) = pending.pop() {
            let Ok(entries) = std::fs::read_dir(root.join(&dir).join(&relative)) else {
                continue;
            };
            for entry in entries.flatten() {
                let name = entry.file_name();
                if name.to_string_lossy().starts_with('.') {
                    continue;
                }
                let path = relative.join(&name);
                let Ok(file_type) = entry.file_type() else {
                    continue;
                };
                if file_type.is_dir() {
                    pending.push(path);
                } else if globs.is_match(&path) && found.len() < MAX_LISTED {
                    found.push(ReportEntry {
                        path: dir.join(path),
                        modified: entry.metadata().ok().and_then(|m| unix_seconds(&m)),
                    });
                }
            }
        }
    }
    found.sort_by(|a, b| {
        b.modified
            .unwrap_or_default()
            .cmp(&a.modified.unwrap_or_default())
            .then_with(|| a.path.cmp(&b.path))
    });
    found.dedup_by(|a, b| a.path == b.path);
    found
}

/// The paths [`listing`] found, newest first.
pub fn list(root: &Path, reports: &Reports) -> Vec<PathBuf> {
    listing(root, reports)
        .into_iter()
        .map(|entry| entry.path)
        .collect()
}

fn unix_seconds(metadata: &std::fs::Metadata) -> Option<u64> {
    let modified = metadata.modified().ok()?;
    Some(modified.duration_since(UNIX_EPOCH).ok()?.as_secs())
}

/// The `dirs` that stay inside `root` (already canonical), as plain relative
/// paths. A directory that escapes through `..` or a symlink is dropped
/// rather than walked.
fn confined_dirs(root: &Path, dirs: &[PathBuf]) -> Vec<PathBuf> {
    dirs.iter()
        .filter_map(|dir| plain(dir))
        .filter(|dir| match root.join(dir).canonicalize() {
            Ok(resolved) => resolved.starts_with(root),
            // A missing directory cannot be walked anyway; keeping it here
            // leaves the "listed nothing" path to `read_dir`.
            Err(_) => true,
        })
        .collect()
}

/// `path` as plain, relative components, or `None` when it names anything
/// outside the tree it is relative to (the rule `reports.dirs` is validated
/// with).
fn plain(path: &Path) -> Option<PathBuf> {
    let mut plain = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(name) => plain.push(name),
            Component::CurDir => {}
            _ => return None,
        }
    }
    Some(plain)
}

/// The directory a report at `path` (relative to the root) may load from,
/// decided from the paths alone: the longest `reports.dirs` entry holding it,
/// else its own directory.
pub fn base_of(reports: &Reports, path: &Path) -> PathBuf {
    reports
        .dirs
        .iter()
        .filter_map(|dir| plain(dir))
        .filter(|dir| !dir.as_os_str().is_empty() && path.starts_with(dir) && path != dir)
        .max_by_key(|dir| dir.components().count())
        .or_else(|| path.parent().map(Path::to_owned))
        .unwrap_or_default()
}

/// A directory a report's web view may load from, with everything it can
/// reach confined to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportBase {
    /// Canonical directory the page may load from.
    base: PathBuf,
}

/// A response to one request from a report's web view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
}

impl ReportBase {
    /// The base at `path`, a relative directory inside `root`. `path` may
    /// come from a client ([`crate::report`] serves a relay's working
    /// directory too), so it has to be plain and to stay inside the root
    /// after both are resolved.
    pub fn open(root: &Path, path: &Path) -> Result<Self, ReportError> {
        let relative = plain(path).ok_or_else(|| ReportError::OutsideProject(path.to_owned()))?;
        let root = canonical(root)?;
        let base = canonical(&root.join(relative))?;
        if !base.starts_with(&root) {
            return Err(ReportError::OutsideProject(path.to_owned()));
        }
        Ok(Self { base })
    }

    pub fn path(&self) -> &Path {
        &self.base
    }

    /// The file a URL path names, if it lies inside the base. `None` for
    /// traversal attempts and escapes through symlinks; a missing file
    /// resolves to a path that then fails to read.
    pub fn resolve(&self, path: &str) -> Option<PathBuf> {
        let mut file = self.base.clone();
        for segment in segments(path)? {
            file.push(segment);
        }
        match file.canonicalize() {
            Ok(resolved) => resolved.starts_with(&self.base).then_some(resolved),
            Err(_) => Some(file),
        }
    }

    /// Read what a URL path names: 403 outside the base, 404 when missing.
    pub fn read(&self, path: &str) -> Reply {
        let Some(file) = self.resolve(path) else {
            return Reply::status(403);
        };
        match std::fs::read(&file) {
            Ok(body) => Reply {
                status: 200,
                content_type: content_type(&file),
                body,
            },
            Err(_) => Reply::status(404),
        }
    }

    /// When what a URL path names last changed, in seconds since the Unix
    /// epoch.
    pub fn modified(&self, path: &str) -> Option<u64> {
        let file = self.resolve(path)?;
        unix_seconds(&std::fs::metadata(file).ok()?)
    }
}

/// The decoded segments of a URL path, or `None` for a traversal attempt:
/// `..`, an absolute path, a backslash, or a bad encoding.
fn segments(path: &str) -> Option<Vec<String>> {
    let decoded = percent_decode_str(path).decode_utf8().ok()?;
    decoded
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(|segment| {
            let mut components = Path::new(segment).components();
            match (components.next(), components.next()) {
                (Some(Component::Normal(name)), None) if !segment.contains('\\') => {
                    name.to_str().map(str::to_owned)
                }
                _ => None,
            }
        })
        .collect()
}

fn canonical(path: &Path) -> Result<PathBuf, ReportError> {
    path.canonicalize().map_err(|source| ReportError::Io {
        path: path.to_owned(),
        source,
    })
}

/// One report in the project root, ready to be served to its web view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportFile {
    base: ReportBase,
    /// Canonical path of the report itself.
    file: PathBuf,
    kind: ReportKind,
    allow_network: bool,
}

impl ReportFile {
    /// The report at `path` (relative to the project `root`, or absolute
    /// inside it).
    pub fn open(root: &Path, reports: &Reports, path: &Path) -> Result<Self, ReportError> {
        let root = canonical(root)?;
        let file = canonical(&root.join(path))?;
        if !file.starts_with(&root) {
            return Err(ReportError::OutsideProject(path.to_owned()));
        }
        let kind =
            ReportKind::of(&file).ok_or_else(|| ReportError::Unsupported(path.to_owned()))?;
        let base = reports
            .dirs
            .iter()
            .filter_map(|dir| root.join(dir).canonicalize().ok())
            .filter(|dir| dir.starts_with(&root) && file.starts_with(dir))
            .max_by_key(|dir| dir.components().count())
            .or_else(|| file.parent().map(Path::to_owned))
            .unwrap_or_else(|| root.clone());
        Ok(Self {
            base: ReportBase { base },
            file,
            kind,
            allow_network: reports.allow_network,
        })
    }

    pub fn kind(&self) -> ReportKind {
        self.kind
    }

    /// The report's canonical path, e.g. to watch it for changes.
    pub fn file(&self) -> &Path {
        &self.file
    }

    /// When the report last changed, to reload the view after an agent
    /// rewrites it.
    pub fn modified(&self) -> Option<SystemTime> {
        std::fs::metadata(&self.file)
            .and_then(|m| m.modified())
            .ok()
    }

    /// The path (and query) the web view starts at: the page itself, or for
    /// an image a page that fits it to the pane.
    pub fn entry(&self) -> String {
        entry(
            self.file.strip_prefix(self.base.path()).unwrap_or(&self.file),
            self.kind,
        )
    }

    /// The `Content-Security-Policy` for every response: same-origin loads
    /// (plus inline code and `data:`/`blob:` URLs) unless `allow_network`.
    pub fn content_security_policy(&self) -> Option<&'static str> {
        content_security_policy(self.allow_network)
    }

    /// Answer a request for `path_and_query` (the URL after the authority).
    pub fn serve(&self, path_and_query: &str) -> Reply {
        let (path, query) = split(path_and_query);
        if query == IMAGE_VIEW_QUERY
            && self.kind == ReportKind::Image
            && self.base.resolve(path) == Some(self.file.clone())
        {
            return image_reply(&self.file);
        }
        self.base.read(path)
    }
}

impl Reply {
    /// An empty reply with just a status, for a refusal the view renders as a
    /// blank page.
    pub fn status(status: u16) -> Self {
        Self {
            status,
            content_type: "text/plain; charset=utf-8",
            body: Vec::new(),
        }
    }

    /// A reply the view shows in place of the report, e.g. when the
    /// workspace holding it is gone.
    pub fn error(status: u16, message: &str) -> Self {
        Self {
            status,
            content_type: "text/plain; charset=utf-8",
            body: message.as_bytes().to_owned(),
        }
    }
}

/// Where the web view starts for a report at `file`, relative to its base.
fn entry(file: &Path, kind: ReportKind) -> String {
    let mut entry = url_path(file);
    if kind == ReportKind::Image {
        entry.push('?');
        entry.push_str(IMAGE_VIEW_QUERY);
    }
    entry
}

/// The path and query of a URL, with the fragment dropped.
fn split(path_and_query: &str) -> (&str, &str) {
    let (path, query) = path_and_query
        .split_once('?')
        .unwrap_or((path_and_query, ""));
    let path = path.split_once('#').map_or(path, |(path, _)| path);
    (path, query)
}

fn content_security_policy(allow_network: bool) -> Option<&'static str> {
    (!allow_network).then_some(
        "default-src 'self' ukai-report: data: blob: 'unsafe-inline' 'unsafe-eval'; \
         form-action 'none'; base-uri 'self'",
    )
}

fn image_reply(file: &Path) -> Reply {
    Reply {
        status: 200,
        content_type: "text/html; charset=utf-8",
        body: image_page(file).into_bytes(),
    }
}

/// Characters escaped in a URL path segment.
const SEGMENT: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'`')
    .add(b'{')
    .add(b'}')
    .add(b'/')
    .add(b'\\');

/// `/`-joined, percent-encoded URL path of a relative file path.
fn url_path(relative: &Path) -> String {
    relative
        .components()
        .map(|c| utf8_percent_encode(&c.as_os_str().to_string_lossy(), SEGMENT).to_string())
        .fold(String::new(), |mut url, segment| {
            url.push('/');
            url.push_str(&segment);
            url
        })
}

/// A page showing one image fitted to the view; a click toggles actual size.
fn image_page(file: &Path) -> String {
    let name = file
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let src = utf8_percent_encode(&name, SEGMENT);
    let alt = name
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('"', "&quot;");
    format!(
        r#"<!doctype html>
<meta charset="utf-8">
<title>{alt}</title>
<style>
  :root {{ color-scheme: light dark; }}
  html, body {{ margin: 0; height: 100%; }}
  body {{ display: grid; place-items: center; overflow: auto; }}
  img {{ max-width: 100%; max-height: 100vh; object-fit: contain; cursor: zoom-in; }}
  body.actual {{ place-items: start; }}
  body.actual img {{ max-width: none; max-height: none; cursor: zoom-out; }}
</style>
<body><img src="{src}" alt="{alt}" onclick="document.body.classList.toggle('actual')"></body>
"#
    )
}

/// The content type a path implies, so a reply never has to be trusted for
/// its type.
pub fn content_type(file: &Path) -> &'static str {
    let ext = file
        .extension()
        .and_then(|ext| ext.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    match ext.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" | "map" => "application/json",
        "txt" | "md" | "log" => "text/plain; charset=utf-8",
        "csv" => "text/csv; charset=utf-8",
        "xml" => "application/xml",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "bmp" => "image/bmp",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "wasm" => "application/wasm",
        "pdf" => "application/pdf",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests;
