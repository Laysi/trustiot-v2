//! The site: every file under the root directory, read into memory once at startup. Requests
//! are answered only from this table, by exact path, so no request can reach a file outside it
//! (no path traversal, no symbolic links, nothing written later).

use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::Path;

use axum::body::Bytes;
use axum::http::HeaderValue;
use sha2::{Digest, Sha256};

/// The file every path that is not a file of the site answers with (deep links).
pub const INDEX: &str = "/index.html";

/// The build's hashed assets (frontend/vite.config.ts, `build.assetsDir`). A path under it that
/// is not a file answers 404, never `index.html` posing as a script or a style sheet.
pub const STATIC_DIR: &str = "/static";

/// The most bytes the site may hold in all. The whole site is in memory, so a build that
/// outgrows this fails at startup rather than at the pod's memory limit while serving.
pub const MAX_SITE_BYTES: u64 = 32 * 1024 * 1024;

/// The file extensions the site may hold and their `Content-Type`. The same list as the build
/// output check (frontend/tools/check-dist.ts) and the release build's check
/// (scripts/release/static-build.ts, whose tests compare them); a file with any other
/// extension, or none, stops the service at startup.
pub const CONTENT_TYPES: &[(&str, &str)] = &[
    ("css", "text/css; charset=utf-8"),
    ("html", "text/html; charset=utf-8"),
    ("ico", "image/x-icon"),
    ("js", "text/javascript; charset=utf-8"),
    ("png", "image/png"),
    ("svg", "image/svg+xml"),
    ("txt", "text/plain; charset=utf-8"),
    ("webmanifest", "application/manifest+json"),
    ("woff2", "font/woff2"),
];

/// How long a browser or a cache may reuse a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CachePolicy {
    /// Asks the server each time (`no-cache`), which answers 304 while the ETag matches:
    /// `index.html` and the other files at the root, whose names stay the same across builds.
    Revalidate,
    /// Reused for a year without asking (`immutable`): the files under [`STATIC_DIR`], whose
    /// names are hashes of their content.
    Immutable,
}

/// One file of the site.
#[derive(Clone, Debug)]
pub struct File {
    /// The bytes, shared by every response.
    pub body: Bytes,
    /// From [`CONTENT_TYPES`].
    pub content_type: &'static str,
    /// A strong ETag: 128 bits of the SHA-256 of the bytes, quoted.
    pub etag: HeaderValue,
    pub cache: CachePolicy,
}

/// Why the site could not be loaded. Paths are relative to the root directory, which is not
/// named (it is a configured value).
#[derive(Debug, thiserror::Error)]
pub enum SiteError {
    #[error("cannot read the site's {path}: {source}")]
    Io { path: String, source: io::Error },
    #[error("the site has no index.html at its root")]
    MissingIndex,
    #[error("the site's {path} is a symbolic link; the site must hold only regular files")]
    Symlink { path: String },
    #[error("the site's {path} is neither a regular file nor a directory")]
    NotAFile { path: String },
    #[error(
        "the site's {path} has a name other than letters, digits, `_`, `-` and `.` (not first)"
    )]
    Name { path: String },
    #[error("the site's {path} has an extension the service does not serve")]
    Extension { path: String },
    #[error("the site holds more than {limit} bytes")]
    TooLarge { limit: u64 },
}

/// The loaded site.
#[derive(Debug)]
pub struct Site {
    files: HashMap<String, File>,
    bytes: u64,
}

impl Site {
    /// Reads every file under `root` (an existing directory). Fails when `root` holds no
    /// `index.html`, a symbolic link or another special file, a name that is not
    /// [plain](plain_name), a file without a known extension, or more than `limit` bytes.
    ///
    /// # Errors
    ///
    /// See [`SiteError`].
    pub fn load(root: &Path, limit: u64) -> Result<Self, SiteError> {
        let mut site = Self {
            files: HashMap::new(),
            bytes: 0,
        };
        site.read_dir(root, "", limit)?;
        if !site.files.contains_key(INDEX) {
            return Err(SiteError::MissingIndex);
        }
        Ok(site)
    }

    fn read_dir(&mut self, dir: &Path, prefix: &str, limit: u64) -> Result<(), SiteError> {
        let io_error = |source| SiteError::Io {
            path: display(prefix),
            source,
        };
        let mut entries = fs::read_dir(dir)
            .map_err(io_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(io_error)?;
        entries.sort_by_key(fs::DirEntry::file_name);
        for entry in entries {
            let name = entry.file_name();
            let Some(name) = name.to_str().filter(|name| plain_name(name)) else {
                return Err(SiteError::Name {
                    path: format!("{prefix}/{}", name.to_string_lossy()),
                });
            };
            let path = format!("{prefix}/{name}");
            let metadata = fs::symlink_metadata(entry.path()).map_err(|source| SiteError::Io {
                path: path.clone(),
                source,
            })?;
            let kind = metadata.file_type();
            if kind.is_symlink() {
                return Err(SiteError::Symlink { path });
            }
            if kind.is_dir() {
                self.read_dir(&entry.path(), &path, limit)?;
                continue;
            }
            if !kind.is_file() {
                return Err(SiteError::NotAFile { path });
            }
            let Some(content_type) = content_type(name) else {
                return Err(SiteError::Extension { path });
            };
            self.bytes = self.bytes.saturating_add(metadata.len());
            if self.bytes > limit {
                return Err(SiteError::TooLarge { limit });
            }
            let body = fs::read(entry.path()).map_err(|source| SiteError::Io {
                path: path.clone(),
                source,
            })?;
            let cache = if path.starts_with(&format!("{STATIC_DIR}/")) {
                CachePolicy::Immutable
            } else {
                CachePolicy::Revalidate
            };
            let file = File {
                etag: etag(&body),
                body: Bytes::from(body),
                content_type,
                cache,
            };
            self.files.insert(path, file);
        }
        Ok(())
    }

    /// The file at exactly `path` (for example `/static/B7f3kQ2x.js`), if the site has one.
    #[must_use]
    pub fn get(&self, path: &str) -> Option<&File> {
        self.files.get(path)
    }

    /// `index.html`, which [`Site::load`] made sure exists.
    #[must_use]
    pub fn index(&self) -> &File {
        // `load` refuses a site without it, and nothing removes files afterwards.
        self.files
            .get(INDEX)
            .expect("Site::load requires index.html")
    }

    /// How many files the site holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.files.len()
    }

    /// Whether the site holds no file; never true for a loaded site.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// The size of all files.
    #[must_use]
    pub fn bytes(&self) -> u64 {
        self.bytes
    }
}

/// Whether `path` is in the hashed assets' directory, where a missing file is 404.
#[must_use]
pub fn is_static(path: &str) -> bool {
    path == STATIC_DIR
        || path
            .strip_prefix(STATIC_DIR)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// Whether a file or directory name can be served as it is: ASCII letters, digits, `_`, `-`
/// and `.`, not starting with `.` (no hidden files, no `..`). Such a name needs no percent
/// encoding in a URL, so the request path matches it exactly.
#[must_use]
pub fn plain_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

/// The `Content-Type` of a file name's extension, from [`CONTENT_TYPES`].
#[must_use]
pub fn content_type(name: &str) -> Option<&'static str> {
    let (_, extension) = name.rsplit_once('.')?;
    CONTENT_TYPES
        .iter()
        .find(|(known, _)| *known == extension)
        .map(|(_, content_type)| *content_type)
}

fn etag(body: &[u8]) -> HeaderValue {
    let digest = Sha256::digest(body);
    let mut tag = String::with_capacity(34);
    tag.push('"');
    for byte in &digest[..16] {
        // Writing to a String cannot fail.
        let _ = write!(tag, "{byte:02x}");
    }
    tag.push('"');
    // Quotes and hex digits only.
    HeaderValue::from_str(&tag).expect("an ETag of hex digits")
}

fn display(prefix: &str) -> String {
    if prefix.is_empty() {
        "root directory".to_owned()
    } else {
        prefix.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_names() {
        for name in ["index.html", "B7f3kQ2x.js", "a_b-c.d.css", "static", "x"] {
            assert!(plain_name(name), "{name}");
        }
        for name in [
            "",
            ".",
            "..",
            ".DS_Store",
            ".htaccess",
            "a b.js",
            "a%2F.js",
            "a/b",
            "ä.js",
            "a\\b",
        ] {
            assert!(!plain_name(name), "{name}");
        }
    }

    #[test]
    fn content_types_by_extension() {
        assert_eq!(content_type("index.html"), Some("text/html; charset=utf-8"));
        assert_eq!(
            content_type("B7f3kQ2x.js"),
            Some("text/javascript; charset=utf-8")
        );
        assert_eq!(
            content_type("a.webmanifest"),
            Some("application/manifest+json")
        );
        for name in [
            "a.map",
            "a.JS",
            "a.json",
            "a.mjs",
            "Makefile",
            "a.",
            "a.html.gz",
        ] {
            assert_eq!(content_type(name), None, "{name}");
        }
    }

    #[test]
    fn the_static_directory() {
        for path in ["/static", "/static/", "/static/a.js", "/static/x/y"] {
            assert!(is_static(path), "{path}");
        }
        for path in [
            "/",
            "/staticx",
            "/static.js",
            "/a/static/b.js",
            "/index.html",
        ] {
            assert!(!is_static(path), "{path}");
        }
    }

    #[test]
    fn etags_are_quoted_hex_of_the_sha256() {
        // `printf '' | sha256sum`: e3b0c44298fc1c149afbf4c8996fb924...
        assert_eq!(etag(b""), "\"e3b0c44298fc1c149afbf4c8996fb924\"");
        assert_ne!(etag(b"a"), etag(b"b"));
    }
}
