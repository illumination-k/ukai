//! Reports: HTML pages and images an agent wrote, shown in a pane.
//!
//! A report pane gets a web view of its own (no IPC to the app) that loads
//! the file through a custom protocol answered by [`ReportFile::serve`]. What
//! the page can load is confined to the report's *base*: the `reports.dirs`
//! entry holding it, else the file's own directory. So `../img/plot.png`
//! works inside a reports tree, and nothing outside it is reachable — after
//! resolving symlinks, as with ACP `fs/*`.
//!
//! An agent working in a pod writes its reports there, not on this machine
//! (`ukai-relay` advertises no `fs/*` capability). [`ReportTree`] is that
//! case: the relay serves the whole reports tree by project-relative path
//! and the gateway proxies it, confined to the `reports.dirs` the same way.

use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

use percent_encoding::{AsciiSet, CONTROLS, percent_decode_str, utf8_percent_encode};
use ukai_config::Reports;

/// How many files [`list`] returns at most, so a huge tree cannot stall the UI.
pub const MAX_LISTED: usize = 500;

/// Query that asks [`ReportFile::serve`] for the page framing an image.
pub const IMAGE_VIEW_QUERY: &str = "ukai-view";

/// What a report may load when the project does not allow it the network:
/// its own origin, inline code, and `data:`/`blob:` URLs. Whatever serves a
/// report — a file on this machine or a pod through the gateway — sends it.
pub const CONFINED_CSP: &str = "default-src 'self' ukai-report: data: blob: 'unsafe-inline' \
                                'unsafe-eval'; form-action 'none'; base-uri 'self'";

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

/// One listed report, with when it last changed so a client following the
/// listing can tell that it was rewritten.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    /// Relative to the project root.
    pub path: PathBuf,
    pub modified: SystemTime,
}

/// Files under `reports.dirs` matching `reports.globs`, relative to `root`,
/// newest first. Hidden directories are skipped; unreadable ones are too.
pub fn list(root: &Path, reports: &Reports) -> Vec<PathBuf> {
    list_detailed(root, reports)
        .into_iter()
        .map(|listed| listed.path)
        .collect()
}

/// [`list`], keeping each file's modification time.
pub fn list_detailed(root: &Path, reports: &Reports) -> Vec<Listed> {
    let mut builder = globset::GlobSetBuilder::new();
    for glob in &reports.globs {
        // Validation reports bad globs; here they just match nothing.
        if let Ok(glob) = globset::Glob::new(glob) {
            builder.add(glob);
        }
    }
    let Ok(globs) = builder.build() else {
        return Vec::new();
    };
    let mut found: Vec<(SystemTime, PathBuf)> = Vec::new();
    for dir in &reports.dirs {
        let mut pending = vec![PathBuf::new()];
        while let Some(relative) = pending.pop() {
            let Ok(entries) = std::fs::read_dir(root.join(dir).join(&relative)) else {
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
                    let modified = entry
                        .metadata()
                        .and_then(|m| m.modified())
                        .unwrap_or(SystemTime::UNIX_EPOCH);
                    found.push((modified, dir.join(path)));
                }
            }
        }
    }
    found.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    found.dedup_by(|a, b| a.1 == b.1);
    found
        .into_iter()
        .map(|(modified, path)| Listed { path, modified })
        .collect()
}

/// One report, ready to be served to its web view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportFile {
    /// Canonical directory the page may load from.
    base: PathBuf,
    /// Canonical path of the report itself.
    file: PathBuf,
    kind: ReportKind,
    allow_network: bool,
}

/// A response to one request from a report's web view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
    /// A validator that changes whenever the file does, for an HTTP `ETag`
    /// when the reply is served over the network. `None` for generated pages
    /// and for errors.
    pub etag: Option<String>,
}

impl ReportFile {
    /// The report at `path` (relative to the project `root`, or absolute
    /// inside it).
    pub fn open(root: &Path, reports: &Reports, path: &Path) -> Result<Self, ReportError> {
        let canonical = |path: &Path| {
            path.canonicalize().map_err(|source| ReportError::Io {
                path: path.to_owned(),
                source,
            })
        };
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
            base,
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
        let mut entry = url_path(self.file.strip_prefix(&self.base).unwrap_or(&self.file));
        if self.kind == ReportKind::Image {
            entry.push('?');
            entry.push_str(IMAGE_VIEW_QUERY);
        }
        entry
    }

    /// The `Content-Security-Policy` for every response: same-origin loads
    /// (plus inline code and `data:`/`blob:` URLs) unless `allow_network`.
    pub fn content_security_policy(&self) -> Option<&'static str> {
        (!self.allow_network).then_some(CONFINED_CSP)
    }

    /// Answer a request for `path_and_query` (the URL after the authority).
    pub fn serve(&self, path_and_query: &str) -> Reply {
        let (path, query) = path_and_query
            .split_once('?')
            .unwrap_or((path_and_query, ""));
        let path = path.split_once('#').map_or(path, |(path, _)| path);
        let Some(file) = self.resolve(path) else {
            return Reply::status(403);
        };
        if query == IMAGE_VIEW_QUERY && file == self.file && self.kind == ReportKind::Image {
            return Reply::page(image_page(&self.file));
        }
        Reply::file(&file)
    }

    /// The file a URL path names, if it lies inside the base. `None` for
    /// traversal attempts and escapes through symlinks; a missing file
    /// resolves to a path that then fails to read.
    fn resolve(&self, path: &str) -> Option<PathBuf> {
        let file = join(&self.base, path)?;
        match file.canonicalize() {
            Ok(resolved) => resolved.starts_with(&self.base).then_some(resolved),
            Err(_) => Some(file),
        }
    }
}

/// The reports of one project tree, served by project-relative path.
///
/// Where [`ReportFile`] is one report opened for one pane, this is every
/// report of a checkout a client cannot read itself: `ukai-relay` answers
/// the gateway's report requests from it. A request is served only if it
/// resolves inside one of the `reports.dirs` — the base a report's relative
/// assets may reach — so a path that leaves one through `..` or a symlink is
/// refused even though it stays in the project.
#[derive(Debug, Clone, PartialEq)]
pub struct ReportTree {
    root: PathBuf,
    /// Canonical `reports.dirs` inside the root; a request must land in one.
    bases: Vec<PathBuf>,
    reports: Reports,
}

impl ReportTree {
    /// The tree at `root`. `Err` only if the root itself cannot be resolved;
    /// a configuration without `reports.dirs` opens and serves nothing.
    pub fn open(root: &Path, reports: &Reports) -> Result<Self, ReportError> {
        let root = root.canonicalize().map_err(|source| ReportError::Io {
            path: root.to_owned(),
            source,
        })?;
        let bases = reports
            .dirs
            .iter()
            .filter_map(|dir| root.join(dir).canonicalize().ok())
            .filter(|dir| dir.starts_with(&root))
            .collect();
        Ok(Self {
            root,
            bases,
            reports: reports.clone(),
        })
    }

    /// The reports to offer, newest first, as [`list_detailed`].
    pub fn list(&self) -> Vec<Listed> {
        list_detailed(&self.root, &self.reports)
    }

    /// Answer a request for a project-relative URL path. 403 for anything
    /// outside the report directories, 404 for a file that is not there.
    pub fn serve(&self, path: &str) -> Reply {
        match self.resolve(path) {
            Some(file) => Reply::file(&file),
            None => Reply::status(403),
        }
    }

    /// The file a URL path names, if it lies inside one of the bases. `None`
    /// for traversal attempts and escapes through symlinks; a missing file
    /// resolves to a path that then fails to read.
    fn resolve(&self, path: &str) -> Option<PathBuf> {
        let file = join(&self.root, path)?;
        let inside = |path: &Path| self.bases.iter().any(|base| path.starts_with(base));
        match file.canonicalize() {
            Ok(resolved) => inside(&resolved).then_some(resolved),
            Err(_) => inside(&file).then_some(file),
        }
    }
}

impl Reply {
    /// The contents of `file`, or 404 when it cannot be read.
    fn file(file: &Path) -> Self {
        match std::fs::read(file) {
            Ok(body) => Self {
                status: 200,
                content_type: content_type(file),
                etag: etag(file, body.len()),
                body,
            },
            Err(_) => Self::status(404),
        }
    }

    /// A page generated here rather than read from the tree.
    fn page(html: String) -> Self {
        Self {
            status: 200,
            content_type: "text/html; charset=utf-8",
            body: html.into_bytes(),
            etag: None,
        }
    }

    fn status(status: u16) -> Self {
        Self {
            status,
            content_type: "text/plain; charset=utf-8",
            body: Vec::new(),
            etag: None,
        }
    }
}

/// A validator of `file`'s contents: its modification time and length. Both
/// change when an agent rewrites a report; neither reveals anything the
/// contents do not.
fn etag(file: &Path, len: usize) -> Option<String> {
    let modified = std::fs::metadata(file)
        .and_then(|meta| meta.modified())
        .ok()?;
    let since_epoch = modified.duration_since(SystemTime::UNIX_EPOCH).ok()?;
    Some(format!(
        "{}.{:09}-{len}",
        since_epoch.as_secs(),
        since_epoch.subsec_nanos()
    ))
}

/// `base` with the segments of a URL `path` appended, or `None` if a segment
/// is anything but a plain name (`..`, an absolute path, a Windows
/// separator). Percent-encoding is decoded first, so `%2e%2e` is refused too.
fn join(base: &Path, path: &str) -> Option<PathBuf> {
    let decoded = percent_decode_str(path).decode_utf8().ok()?;
    let mut file = base.to_owned();
    for segment in decoded.split('/').filter(|s| !s.is_empty()) {
        let mut components = Path::new(segment).components();
        match (components.next(), components.next()) {
            (Some(Component::Normal(name)), None) if !segment.contains('\\') => {
                file.push(name);
            }
            _ => return None,
        }
    }
    Some(file)
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
pub fn url_path(relative: &Path) -> String {
    relative
        .components()
        .map(|c| utf8_percent_encode(&c.as_os_str().to_string_lossy(), SEGMENT).to_string())
        .fold(String::new(), |mut url, segment| {
            url.push('/');
            url.push_str(&segment);
            url
        })
}

/// Whether a URL path from a web view names `relative`, however it was
/// encoded: a view may send back `%E3%82%B0` or the character itself.
pub fn is_url_path(path: &str, relative: &Path) -> bool {
    let decode = |path: &str| {
        percent_decode_str(path)
            .decode_utf8()
            .ok()
            .map(String::from)
    };
    match (decode(path), decode(&url_path(relative))) {
        (Some(asked), Some(named)) => asked == named,
        _ => false,
    }
}

/// A page showing one image fitted to the view; a click toggles actual size.
/// The image is loaded by name, so the page works wherever it is served from.
pub fn image_page(file: &Path) -> String {
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

fn content_type(file: &Path) -> &'static str {
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
