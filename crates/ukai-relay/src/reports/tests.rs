use std::fs;

use rstest::rstest;
use tempfile::TempDir;

use super::*;

/// A checkout like an agent leaves behind:
///
/// ```text
/// .env
/// reports/run.html
/// reports/img/plot.png
/// reports/notes.txt
/// src/main.rs
/// ```
fn workspace() -> TempDir {
    let root = tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
    let write = |path: &str, body: &str| {
        let path = root.path().join(path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap_or_else(|e| panic!("mkdir: {e}"));
        }
        fs::write(path, body).unwrap_or_else(|e| panic!("write: {e}"));
    };
    write(".env", "TOKEN=secret");
    write("reports/run.html", r#"<img src="img/plot.png">"#);
    write("reports/img/plot.png", "png");
    write("reports/notes.txt", "notes");
    write("src/main.rs", "fn main() {}");
    root
}

fn files(root: &TempDir, query: &str) -> Vec<String> {
    let served = list_reports(root.path(), query);
    assert_eq!(
        (served.status, served.content_type),
        (200, "application/json")
    );
    let listing: ReportListing =
        serde_json::from_slice(&served.body).unwrap_or_else(|e| panic!("listing: {e}"));
    assert!(
        listing.files.iter().all(|file| file.modified.is_some()),
        "the listing carries when each file changed: {listing:?}"
    );
    let mut paths: Vec<String> = listing
        .files
        .iter()
        .map(|file| file.path.display().to_string())
        .collect();
    paths.sort();
    paths
}

#[test]
fn lists_the_requested_directories() {
    let root = workspace();
    assert_eq!(
        files(&root, "dir=reports"),
        ["reports/img/plot.png", "reports/run.html"]
    );
    assert_eq!(files(&root, "dir=reports&glob=*.html"), ["reports/run.html"]);
    // A query without directories lists nothing, as an empty `dirs` does.
    assert!(files(&root, "").is_empty());
}

#[rstest]
#[case::parent("dir=..")]
#[case::through_a_child("dir=reports/../..")]
#[case::absolute("dir=%2Fetc")]
fn a_directory_that_leaves_the_workspace_lists_nothing(#[case] query: &str) {
    let root = workspace();
    assert!(files(&root, query).is_empty(), "{query}");
}

#[rstest]
#[case::the_report("/run.html", "reports", 200, "text/html; charset=utf-8")]
#[case::an_asset("/img/plot.png", "reports", 200, "image/png")]
#[case::a_file_in_the_base("/notes.txt", "reports", 200, "text/plain; charset=utf-8")]
#[case::not_in_the_base("/run.html", "", 404, "text/plain; charset=utf-8")]
#[case::missing("/none.html", "reports", 404, "text/plain; charset=utf-8")]
#[case::parent("/../.env", "reports", 403, "text/plain; charset=utf-8")]
#[case::encoded_parent("/%2e%2e/.env", "reports", 403, "text/plain; charset=utf-8")]
#[case::out_of_the_base("/../src/main.rs", "reports", 403, "text/plain; charset=utf-8")]
#[case::base_outside_the_workspace("/.env", "reports/../..", 404, "text/plain; charset=utf-8")]
#[case::absolute_base("/.env", "%2Fetc", 404, "text/plain; charset=utf-8")]
fn serving_stays_inside_the_base(
    #[case] path: &str,
    #[case] base: &str,
    #[case] status: u16,
    #[case] content_type: &str,
) {
    let root = workspace();
    let served = read_report(root.path(), path, &format!("base={base}"));
    assert_eq!((served.status, served.content_type), (status, content_type));
}

/// The base may be the workspace root, and then only its own tree shows.
#[test]
fn the_workspace_root_can_be_the_base() {
    let root = workspace();
    assert_eq!(
        read_report(root.path(), "/reports/run.html", "base=").status,
        200
    );
    assert_eq!(read_report(root.path(), "/.env", "base=").status, 200);
}

#[cfg(unix)]
#[test]
fn symlinks_cannot_leave_the_base() {
    let root = workspace();
    std::os::unix::fs::symlink(root.path().join(".env"), root.path().join("reports/env"))
        .unwrap_or_else(|e| panic!("symlink: {e}"));
    assert_eq!(read_report(root.path(), "/env", "base=reports").status, 403);
    // Nor can the base itself be a link out of the workspace.
    let elsewhere = tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
    std::os::unix::fs::symlink(elsewhere.path(), root.path().join("out"))
        .unwrap_or_else(|e| panic!("symlink: {e}"));
    assert_eq!(read_report(root.path(), "/x", "base=out").status, 404);
}

#[rstest]
#[case::plain("base=reports", Some("reports"))]
#[case::space("base=my+reports", Some("my reports"))]
#[case::encoded("base=my%20reports%2Fout", Some("my reports/out"))]
#[case::other_keys("glob=*&base=reports&dir=x", Some("reports"))]
#[case::missing("dir=reports", None)]
fn reads_form_encoded_parameters(#[case] query: &str, #[case] expected: Option<&str>) {
    assert_eq!(param(query, "base").as_deref(), expected);
}

#[test]
fn reads_repeated_parameters_in_order() {
    let asked = settings("dir=reports&glob=**/*.html&dir=docs&glob=**/*.png");
    assert_eq!(asked.dirs, [PathBuf::from("reports"), PathBuf::from("docs")]);
    assert_eq!(asked.globs, ["**/*.html", "**/*.png"]);
    // Without globs the defaults apply, so a listing is never empty by accident.
    assert_eq!(settings("dir=reports").globs, Reports::default().globs);
}
