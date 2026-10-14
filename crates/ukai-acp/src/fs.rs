//! The client side of ACP `fs/read_text_file` and `fs/write_text_file`,
//! confined to one directory tree.
//!
//! An agent — possibly a remote one — names absolute paths. Every path is
//! resolved through the file system (so symlinks cannot point outside) and
//! must land under the root, or the request is refused.

use std::io;
use std::path::{Component, Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum FsError {
    #[error("path must be absolute: {0}")]
    NotAbsolute(PathBuf),
    #[error("path escapes the project root: {0}")]
    OutsideRoot(PathBuf),
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// A directory tree that file requests are confined to.
#[derive(Debug, Clone)]
pub struct FsRoot {
    /// Canonical form, so prefix checks compare resolved paths.
    root: PathBuf,
}

impl FsRoot {
    pub fn new(root: &Path) -> Result<Self, FsError> {
        let root = root.canonicalize().map_err(|source| FsError::Io {
            path: root.to_owned(),
            source,
        })?;
        Ok(Self { root })
    }

    /// Resolve `path` to a location inside the root.
    ///
    /// The path does not have to exist yet (an agent may write a new file in a
    /// new directory): the deepest existing ancestor is canonicalized and the
    /// rest is appended, which is sound because `..` is rejected up front.
    pub fn confine(&self, path: &Path) -> Result<PathBuf, FsError> {
        if !path.is_absolute() {
            return Err(FsError::NotAbsolute(path.to_owned()));
        }
        if path.components().any(|c| matches!(c, Component::ParentDir)) {
            return Err(FsError::OutsideRoot(path.to_owned()));
        }
        let mut existing = path;
        let mut rest = Vec::new();
        let resolved = loop {
            match existing.canonicalize() {
                Ok(resolved) => break resolved,
                Err(source) if source.kind() == io::ErrorKind::NotFound => {
                    let (Some(parent), Some(name)) = (existing.parent(), existing.file_name())
                    else {
                        return Err(FsError::Io {
                            path: path.to_owned(),
                            source,
                        });
                    };
                    rest.push(name);
                    existing = parent;
                }
                Err(source) => {
                    return Err(FsError::Io {
                        path: path.to_owned(),
                        source,
                    });
                }
            }
        };
        if !resolved.starts_with(&self.root) {
            return Err(FsError::OutsideRoot(path.to_owned()));
        }
        Ok(rest
            .into_iter()
            .rev()
            .fold(resolved, |acc, name| acc.join(name)))
    }

    /// Read a text file; `line` is 1-based, `limit` caps the number of lines.
    pub fn read(
        &self,
        path: &Path,
        line: Option<u32>,
        limit: Option<u32>,
    ) -> Result<String, FsError> {
        let path = self.confine(path)?;
        let content = std::fs::read_to_string(&path).map_err(|source| FsError::Io {
            path: path.clone(),
            source,
        })?;
        if line.is_none() && limit.is_none() {
            return Ok(content);
        }
        let skip = line.map_or(0, |line| line.saturating_sub(1)) as usize;
        let take = limit.map_or(usize::MAX, |limit| limit as usize);
        Ok(content
            .split_inclusive('\n')
            .skip(skip)
            .take(take)
            .collect())
    }

    /// Write a text file, creating missing parent directories inside the root.
    /// Returns the resolved path that was written.
    pub fn write(&self, path: &Path, content: &str) -> Result<PathBuf, FsError> {
        let path = self.confine(path)?;
        let io_error = |source| FsError::Io {
            path: path.clone(),
            source,
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(io_error)?;
        }
        std::fs::write(&path, content).map_err(io_error)?;
        Ok(path)
    }
}

#[cfg(test)]
mod tests {
    use rstest::{fixture, rstest};
    use tempfile::TempDir;

    use super::*;

    struct Fixture {
        _dir: TempDir,
        root: PathBuf,
        outside: PathBuf,
        fs: FsRoot,
    }

    #[fixture]
    fn fx() -> Fixture {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let base = dir.path().canonicalize().unwrap_or_else(|e| panic!("{e}"));
        let root = base.join("project");
        let outside = base.join("outside");
        for d in [&root, &outside] {
            std::fs::create_dir(d).unwrap_or_else(|e| panic!("{e}"));
        }
        std::fs::write(root.join("a.txt"), "1\n2\n3\n4\n").unwrap_or_else(|e| panic!("{e}"));
        std::fs::write(outside.join("secret"), "s").unwrap_or_else(|e| panic!("{e}"));
        let fs = FsRoot::new(&root).unwrap_or_else(|e| panic!("{e}"));
        Fixture {
            _dir: dir,
            root,
            outside,
            fs,
        }
    }

    #[rstest]
    #[case::whole(None, None, "1\n2\n3\n4\n")]
    #[case::from_line(Some(3), None, "3\n4\n")]
    #[case::limited(Some(2), Some(2), "2\n3\n")]
    #[case::line_zero_is_start(Some(0), Some(1), "1\n")]
    fn reads_line_ranges(
        fx: Fixture,
        #[case] line: Option<u32>,
        #[case] limit: Option<u32>,
        #[case] expected: &str,
    ) {
        let got = fx.fs.read(&fx.root.join("a.txt"), line, limit);
        assert_eq!(got.ok().as_deref(), Some(expected));
    }

    #[rstest]
    fn writes_new_file_in_new_directory(fx: Fixture) {
        let target = fx.root.join("reports/deep/r.html");
        let written = fx.fs.write(&target, "<h1>hi</h1>");
        assert_eq!(written.ok(), Some(target.clone()));
        assert_eq!(
            std::fs::read_to_string(&target).ok().as_deref(),
            Some("<h1>hi</h1>")
        );
    }

    #[rstest]
    fn rejects_relative(fx: Fixture) {
        assert!(matches!(
            fx.fs.read(Path::new("a.txt"), None, None),
            Err(FsError::NotAbsolute(_))
        ));
    }

    #[rstest]
    fn rejects_parent_components(fx: Fixture) {
        let sneaky = fx.root.join("../outside/secret");
        assert!(matches!(
            fx.fs.read(&sneaky, None, None),
            Err(FsError::OutsideRoot(_))
        ));
    }

    #[rstest]
    fn rejects_paths_outside(fx: Fixture) {
        assert!(matches!(
            fx.fs.write(&fx.outside.join("new"), "x"),
            Err(FsError::OutsideRoot(_))
        ));
    }

    #[cfg(unix)]
    #[rstest]
    fn rejects_symlink_escape(fx: Fixture) {
        let link = fx.root.join("link");
        std::os::unix::fs::symlink(&fx.outside, &link).unwrap_or_else(|e| panic!("{e}"));
        assert!(matches!(
            fx.fs.read(&link.join("secret"), None, None),
            Err(FsError::OutsideRoot(_))
        ));
        assert!(matches!(
            fx.fs.write(&link.join("planted"), "x"),
            Err(FsError::OutsideRoot(_))
        ));
        assert!(!fx.outside.join("planted").exists());
    }
}
