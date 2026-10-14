//! Reports in a gateway agent's workspace.
//!
//! The files are in the pod's checkout, not the project root: `ukai-relay`
//! lists and serves them, `ukai-gateway` proxies that per workspace, and the
//! pane fetches through it ([`ukai_acp::ReportsEndpoint`]). Everything the
//! web view sees is decided here, exactly as for a local
//! [`ReportFile`](super::ReportFile) — the entry URL, the content type, the
//! CSP, and which requests are refused outright — while resolving a path
//! against the base is left to the relay, where the files are.

use std::path::{Path, PathBuf};

use super::{
    IMAGE_VIEW_QUERY, Reply, ReportError, ReportKind, base_of, content_security_policy, entry,
    image_reply, plain, segments, split, url_path,
};
use ukai_config::Reports;

/// One report in a workspace, ready to be served to its web view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteReport {
    /// The directory the page may load from, relative to the workspace root.
    base: PathBuf,
    /// The report itself, relative to [`Self::base`].
    file: PathBuf,
    kind: ReportKind,
    allow_network: bool,
}

/// What answering one request from a remote report's web view takes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fetch {
    /// Ask the relay for `path` (a URL path) under `base`.
    File { base: PathBuf, path: String },
    /// Answer without asking: the page framing an image, or a refusal.
    Ready(Reply),
}

impl RemoteReport {
    /// The report at `path`, relative to the workspace root. Nothing is read,
    /// so this only fails on a path that cannot name a report.
    pub fn open(reports: &Reports, path: &Path) -> Result<Self, ReportError> {
        let kind =
            ReportKind::of(path).ok_or_else(|| ReportError::Unsupported(path.to_owned()))?;
        let relative = plain(path).ok_or_else(|| ReportError::OutsideProject(path.to_owned()))?;
        let base = base_of(reports, &relative);
        let file = relative.strip_prefix(&base).unwrap_or(&relative).to_owned();
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

    /// The report's path relative to the workspace root, as it is listed.
    pub fn path(&self) -> PathBuf {
        self.base.join(&self.file)
    }

    /// The path (and query) the web view starts at: the page itself, or for
    /// an image a page that fits it to the pane.
    pub fn entry(&self) -> String {
        entry(&self.file, self.kind)
    }

    pub fn content_security_policy(&self) -> Option<&'static str> {
        content_security_policy(self.allow_network)
    }

    /// What to do about a request for `path_and_query` (the URL after the
    /// authority). Traversal is refused here as well as at the relay, so a
    /// path that reaches the wire is one plain segment after another.
    pub fn request(&self, path_and_query: &str) -> Fetch {
        let (path, query) = split(path_and_query);
        let Some(segments) = segments(path) else {
            return Fetch::Ready(Reply::status(403));
        };
        let path = url_path(&segments.iter().collect::<PathBuf>());
        if query == IMAGE_VIEW_QUERY && self.kind == ReportKind::Image && path == self.entry_path()
        {
            return Fetch::Ready(image_reply(&self.file));
        }
        Fetch::File {
            base: self.base.clone(),
            path,
        }
    }

    fn entry_path(&self) -> String {
        url_path(&self.file)
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    fn reports(dirs: &[&str]) -> Reports {
        Reports {
            dirs: dirs.iter().map(PathBuf::from).collect(),
            ..Reports::default()
        }
    }

    fn open(path: &str) -> RemoteReport {
        RemoteReport::open(&reports(&["reports", "docs/out"]), Path::new(path))
            .unwrap_or_else(|e| panic!("open {path}: {e}"))
    }

    #[rstest]
    #[case::in_a_reports_dir("reports/2026/run.html", "reports", "/2026/run.html")]
    #[case::longest_dir_wins("docs/out/a.html", "docs/out", "/a.html")]
    #[case::outside_any_dir("notes/a.html", "notes", "/a.html")]
    #[case::at_the_root("a.html", "", "/a.html")]
    fn the_base_holds_the_report(
        #[case] path: &str,
        #[case] base: &str,
        #[case] entry: &str,
    ) {
        let report = open(path);
        assert_eq!(report.base, PathBuf::from(base));
        assert_eq!(report.entry(), entry);
        assert_eq!(report.path(), PathBuf::from(path));
    }

    #[test]
    fn assets_are_fetched_relative_to_the_base() {
        let report = open("reports/2026/run 1.html");
        assert_eq!(report.entry(), "/2026/run%201.html");
        assert_eq!(
            report.request("/img/plot.png"),
            Fetch::File {
                base: PathBuf::from("reports"),
                path: "/img/plot.png".to_owned(),
            }
        );
        // The relay resolves `..` inside the base; the fragment is ours to drop.
        assert_eq!(
            report.request("/2026/../img/plot.png#x"),
            Fetch::Ready(Reply::status(403))
        );
    }

    #[rstest]
    #[case::parent("/../.env")]
    #[case::encoded_parent("/%2e%2e/.env")]
    #[case::encoded_slash("/..%2f.env")]
    #[case::backslash("/..\\.env")]
    fn traversal_never_reaches_the_relay(#[case] path: &str) {
        assert_eq!(open("reports/a.html").request(path), Fetch::Ready(Reply::status(403)));
    }

    #[test]
    fn an_image_opens_in_a_page_that_fits_it() {
        let report = open("reports/グラフ.svg");
        let entry = report.entry();
        assert_eq!(entry, "/%E3%82%B0%E3%83%A9%E3%83%95.svg?ukai-view");
        let Fetch::Ready(page) = report.request(&entry) else {
            panic!("the image page is answered without the relay");
        };
        assert_eq!(page.content_type, "text/html; charset=utf-8");
        // The image itself still comes from the relay.
        assert!(matches!(
            report.request("/%E3%82%B0%E3%83%A9%E3%83%95.svg"),
            Fetch::File { .. }
        ));
    }

    #[rstest]
    #[case::unsupported("reports/data.bin")]
    #[case::outside("../etc/passwd.html")]
    fn refuses_to_open(#[case] path: &str) {
        assert!(RemoteReport::open(&reports(&["reports"]), Path::new(path)).is_err());
    }

    #[rstest]
    #[case::offline(false, true)]
    #[case::online(true, false)]
    fn network_access_is_off_unless_allowed(#[case] allow_network: bool, #[case] has_csp: bool) {
        let config = Reports {
            allow_network,
            ..reports(&["reports"])
        };
        let report = RemoteReport::open(&config, Path::new("reports/a.html"))
            .unwrap_or_else(|e| panic!("open: {e}"));
        assert_eq!(report.content_security_policy().is_some(), has_csp);
    }
}
