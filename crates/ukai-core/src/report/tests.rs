use std::fs;

use rstest::rstest;
use tempfile::TempDir;

use super::*;

/// A project with a reports tree and a secret outside it:
///
/// ```text
/// .env
/// notes.html
/// reports/2026/run 1.html
/// reports/img/plot.png
/// reports/グラフ.svg
/// reports/data.bin
/// reports/.cache/hidden.html
/// ```
fn project() -> TempDir {
    let root = tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
    let write = |path: &str, body: &str| {
        let path = root.path().join(path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap_or_else(|e| panic!("mkdir: {e}"));
        }
        fs::write(path, body).unwrap_or_else(|e| panic!("write: {e}"));
    };
    write(".env", "TOKEN=secret");
    write("notes.html", "<p>notes</p>");
    write("reports/2026/run 1.html", r#"<img src="../img/plot.png">"#);
    write("reports/img/plot.png", "png");
    write("reports/グラフ.svg", "<svg/>");
    write("reports/data.bin", "bin");
    write("reports/.cache/hidden.html", "hidden");
    root
}

fn reports(dirs: &[&str]) -> Reports {
    Reports {
        dirs: dirs.iter().map(PathBuf::from).collect(),
        ..Reports::default()
    }
}

fn open(root: &TempDir, path: &str) -> ReportFile {
    ReportFile::open(root.path(), &reports(&["reports"]), Path::new(path))
        .unwrap_or_else(|e| panic!("open {path}: {e}"))
}

#[rstest]
#[case::html("a/b.HTML", Some(ReportKind::Html))]
#[case::htm("b.htm", Some(ReportKind::Html))]
#[case::png("plot.png", Some(ReportKind::Image))]
#[case::svg("chart.svg", Some(ReportKind::Image))]
#[case::markdown("README.md", None)]
#[case::no_extension("Makefile", None)]
fn kind_follows_the_extension(#[case] path: &str, #[case] expected: Option<ReportKind>) {
    assert_eq!(ReportKind::of(Path::new(path)), expected);
}

#[test]
fn lists_matching_files_and_skips_hidden_ones() {
    let root = project();
    let mut listed = list(root.path(), &reports(&["reports", "missing"]));
    listed.sort();
    assert_eq!(
        listed,
        [
            "reports/2026/run 1.html",
            "reports/img/plot.png",
            "reports/グラフ.svg",
        ]
        .map(PathBuf::from)
    );
}

#[test]
fn lists_only_what_the_globs_match() {
    let root = project();
    let config = Reports {
        globs: vec!["2026/*.html".into()],
        ..reports(&["reports"])
    };
    assert_eq!(
        list(root.path(), &config),
        [PathBuf::from("reports/2026/run 1.html")]
    );
}

#[test]
fn serves_assets_across_the_reports_dir() {
    let root = project();
    let report = open(&root, "reports/2026/run 1.html");
    assert_eq!(report.kind(), ReportKind::Html);
    assert_eq!(report.entry(), "/2026/run%201.html");
    let page = report.serve(&report.entry());
    assert_eq!(
        (page.status, page.content_type),
        (200, "text/html; charset=utf-8")
    );
    // `../img/plot.png` from /2026/ resolves to /img/plot.png.
    let image = report.serve("/img/plot.png");
    assert_eq!((image.status, image.content_type), (200, "image/png"));
    assert_eq!(image.body, b"png");
}

#[test]
fn a_file_outside_the_reports_dirs_is_confined_to_its_directory() {
    let root = project();
    let report = open(&root, "notes.html");
    assert_eq!(report.entry(), "/notes.html");
    assert_eq!(report.serve("/notes.html").status, 200);
    // The project root is the base here, but the escape checks still hold.
    assert_eq!(report.serve("/../notes.html").status, 403);
}

#[test]
fn an_image_opens_in_a_page_that_fits_it() {
    let root = project();
    let report = open(&root, "reports/グラフ.svg");
    assert_eq!(report.kind(), ReportKind::Image);
    let entry = report.entry();
    assert_eq!(entry, "/%E3%82%B0%E3%83%A9%E3%83%95.svg?ukai-view");
    let page = report.serve(&entry);
    assert_eq!(page.content_type, "text/html; charset=utf-8");
    let html = String::from_utf8_lossy(&page.body);
    assert!(
        html.contains(r#"src="%E3%82%B0%E3%83%A9%E3%83%95.svg""#),
        "{html}"
    );
    let raw = report.serve("/%E3%82%B0%E3%83%A9%E3%83%95.svg");
    assert_eq!(
        (raw.content_type, raw.body.as_slice()),
        ("image/svg+xml", &b"<svg/>"[..])
    );
}

#[rstest]
#[case::parent("/../.env", 403)]
#[case::encoded_parent("/%2e%2e/.env", 403)]
#[case::encoded_slash("/..%2f.env", 403)]
#[case::backslash("/..\\.env", 403)]
#[case::missing("/nope.html", 404)]
#[case::directory("/img", 404)]
#[case::fragment("/img/plot.png#x", 200)]
#[case::octet_stream("/data.bin", 200)]
fn serving_stays_inside_the_base(#[case] path: &str, #[case] status: u16) {
    let root = project();
    let report = open(&root, "reports/2026/run 1.html");
    assert_eq!(report.serve(path).status, status, "{path}");
}

#[cfg(unix)]
#[test]
fn symlinks_cannot_leave_the_base() {
    let root = project();
    std::os::unix::fs::symlink(root.path().join(".env"), root.path().join("reports/env"))
        .unwrap_or_else(|e| panic!("symlink: {e}"));
    std::os::unix::fs::symlink(root.path(), root.path().join("reports/up"))
        .unwrap_or_else(|e| panic!("symlink: {e}"));
    let report = open(&root, "reports/2026/run 1.html");
    assert_eq!(report.serve("/env").status, 403);
    assert_eq!(report.serve("/up/.env").status, 403);
}

#[rstest]
#[case::missing("reports/none.html")]
#[case::unsupported("reports/data.bin")]
fn refuses_to_open(#[case] path: &str) {
    let root = project();
    let result = ReportFile::open(root.path(), &reports(&["reports"]), Path::new(path));
    assert!(
        matches!(
            (path, &result),
            ("reports/none.html", Err(ReportError::Io { .. }))
                | ("reports/data.bin", Err(ReportError::Unsupported(_)))
        ),
        "{path}: {result:?}"
    );
}

#[cfg(unix)]
#[test]
fn refuses_a_report_linked_from_outside_the_project() {
    let root = project();
    let elsewhere = tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
    let outside = elsewhere.path().join("x.html");
    fs::write(&outside, "x").unwrap_or_else(|e| panic!("write: {e}"));
    std::os::unix::fs::symlink(&outside, root.path().join("reports/x.html"))
        .unwrap_or_else(|e| panic!("symlink: {e}"));
    let result = ReportFile::open(
        root.path(),
        &reports(&["reports"]),
        Path::new("reports/x.html"),
    );
    assert!(
        matches!(result, Err(ReportError::OutsideProject(_))),
        "{result:?}"
    );
}

#[rstest]
#[case::offline(false, true)]
#[case::online(true, false)]
fn network_access_is_off_unless_allowed(#[case] allow_network: bool, #[case] has_csp: bool) {
    let root = project();
    let config = Reports {
        allow_network,
        ..reports(&["reports"])
    };
    let report = ReportFile::open(root.path(), &config, Path::new("notes.html"))
        .unwrap_or_else(|e| panic!("open: {e}"));
    assert_eq!(report.content_security_policy().is_some(), has_csp);
}
