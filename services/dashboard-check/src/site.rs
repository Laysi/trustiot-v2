//! The files the browser may load: the checker's host page and script, and each shipped runtime
//! version's sandbox files, read once at startup from the frontend build in the image and served
//! to the browser through request interception with the headers of
//! `contract::api::dashboard::sandbox`. No other file exists for the browser.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use contract::api::dashboard::RuntimeVersion;
use contract::api::dashboard::sandbox::{
    CHECK_HOST_HEADERS, CHECK_HOST_PATH, CHECK_HOST_SCRIPT_PATH, SandboxFile,
};
use http::HeaderName;
use serde::Serialize;

/// The origin the browser loads every file from. `.invalid` never resolves (RFC 6761), and the
/// browser could not resolve anything anyway; every request is answered by interception.
pub const ORIGIN: &str = "https://check.invalid";

/// The largest file served: d3 and the runtime are well below it.
pub const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;

/// What a served file is, for the interception's decisions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileKind {
    /// `CHECK_HOST_PATH`.
    HostPage,
    /// `CHECK_HOST_SCRIPT_PATH`.
    HostScript,
    /// One of a runtime version's files.
    Sandbox(RuntimeVersion, SandboxFile),
}

/// One file, ready to hand to `Fetch.fulfillRequest`.
#[derive(Debug)]
pub struct Served {
    pub kind: FileKind,
    /// The response headers: `Content-Type`, `Cache-Control: no-store` and the contract's.
    pub headers: Vec<ResponseHeader>,
    /// The body, base64-encoded as the protocol wants it.
    pub body: String,
}

/// A header in the protocol's form.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ResponseHeader {
    pub name: &'static str,
    pub value: &'static str,
}

/// Every file the browser may load, by URL path.
#[derive(Debug)]
pub struct Site {
    files: HashMap<String, Served>,
    compare: HashMap<RuntimeVersion, String>,
}

/// Why the site could not be loaded; names the file by its URL path, never its content.
#[derive(Debug, thiserror::Error)]
pub enum SiteError {
    #[error("the sandbox file {path} is missing or unreadable in the site directory")]
    Unreadable { path: String },
    #[error("the sandbox file {path} is larger than {MAX_FILE_BYTES} bytes")]
    TooLarge { path: String },
    #[error("the sandbox file {path} is not UTF-8")]
    NotText { path: String },
}

impl Site {
    /// Reads every file of every shipped runtime version and the host page from `site_dir`.
    ///
    /// # Errors
    ///
    /// When a file is missing, unreadable, too large, or not text.
    pub fn load(site_dir: &Path) -> Result<Self, SiteError> {
        let mut files = HashMap::new();
        let mut compare = HashMap::new();
        let host = [
            (
                CHECK_HOST_PATH,
                FileKind::HostPage,
                "text/html; charset=utf-8",
            ),
            (
                CHECK_HOST_SCRIPT_PATH,
                FileKind::HostScript,
                "text/javascript; charset=utf-8",
            ),
        ];
        for (path, kind, content_type) in host {
            let text = read(site_dir, path)?;
            files.insert(
                path.to_owned(),
                served(kind, content_type, &CHECK_HOST_HEADERS, &text),
            );
        }
        for runtime in RuntimeVersion::SHIPPED {
            for file in SandboxFile::ALL {
                let path = file.path(runtime);
                let text = read(site_dir, &path)?;
                if file == SandboxFile::Compare {
                    compare.insert(runtime, text.clone());
                }
                files.insert(
                    path,
                    served(
                        FileKind::Sandbox(runtime, file),
                        file.content_type(),
                        file.headers(),
                        &text,
                    ),
                );
            }
        }
        Ok(Self { files, compare })
    }

    /// The file at `path`, a URL path without query.
    #[must_use]
    pub fn get(&self, path: &str) -> Option<&Served> {
        self.files.get(path)
    }

    /// The comparison program of `runtime`, which the check injects into its isolated world.
    #[must_use]
    pub fn compare_source(&self, runtime: RuntimeVersion) -> Option<&str> {
        self.compare.get(&runtime).map(String::as_str)
    }
}

/// The URL the browser loads a file from.
#[must_use]
pub fn url(path: &str) -> String {
    format!("{ORIGIN}{path}")
}

/// The URL path of `url` when it is a URL of [`ORIGIN`] without query; `None` for anything else.
#[must_use]
pub fn path_of(url: &str) -> Option<&str> {
    let rest = url.strip_prefix(ORIGIN)?;
    if rest.starts_with('/') && !rest.contains(['?', '#']) {
        Some(rest)
    } else {
        None
    }
}

fn file_path(site_dir: &Path, path: &str) -> PathBuf {
    site_dir.join(path.trim_start_matches('/'))
}

fn read(site_dir: &Path, path: &str) -> Result<String, SiteError> {
    let unreadable = || SiteError::Unreadable {
        path: path.to_owned(),
    };
    let file = std::fs::File::open(file_path(site_dir, path)).map_err(|_| unreadable())?;
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| unreadable())?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_FILE_BYTES {
        return Err(SiteError::TooLarge {
            path: path.to_owned(),
        });
    }
    String::from_utf8(bytes).map_err(|_| SiteError::NotText {
        path: path.to_owned(),
    })
}

fn served(
    kind: FileKind,
    content_type: &'static str,
    contract: &'static [(HeaderName, &'static str)],
    text: &str,
) -> Served {
    let mut headers = vec![
        ResponseHeader {
            name: "content-type",
            value: content_type,
        },
        ResponseHeader {
            name: "cache-control",
            value: "no-store",
        },
    ];
    headers.extend(contract.iter().map(|(name, value)| ResponseHeader {
        name: name.as_str(),
        value,
    }));
    Served {
        kind,
        headers,
        body: STANDARD.encode(text),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use contract::api::dashboard::sandbox::{SANDBOX_ASSET_HEADERS, SANDBOX_DOCUMENT_HEADERS};

    use super::*;

    /// Writes a minimal site with every file to `dir`; each file's text is its own path.
    pub(crate) fn write_site(dir: &Path) {
        let mut paths = vec![
            CHECK_HOST_PATH.to_owned(),
            CHECK_HOST_SCRIPT_PATH.to_owned(),
        ];
        for runtime in RuntimeVersion::SHIPPED {
            paths.extend(SandboxFile::ALL.iter().map(|file| file.path(runtime)));
        }
        for path in paths {
            let file = file_path(dir, &path);
            std::fs::create_dir_all(file.parent().expect("has a parent")).expect("mkdir");
            std::fs::write(file, &path).expect("write");
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "dashboard-check-site-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    fn header<'a>(served: &'a Served, name: &str) -> Option<&'a str> {
        served
            .headers
            .iter()
            .find(|header| header.name == name)
            .map(|header| header.value)
    }

    #[test]
    fn serves_every_file_with_the_contract_headers() {
        let dir = temp_dir("all");
        write_site(&dir);
        let site = Site::load(&dir).expect("loads");
        let host = site.get(CHECK_HOST_PATH).expect("host page");
        assert_eq!(host.kind, FileKind::HostPage);
        assert_eq!(
            header(host, "content-type"),
            Some("text/html; charset=utf-8")
        );
        assert_eq!(header(host, "cache-control"), Some("no-store"));
        for (name, value) in &CHECK_HOST_HEADERS {
            assert_eq!(header(host, name.as_str()), Some(*value));
        }
        assert_eq!(
            STANDARD.decode(&host.body).expect("base64"),
            CHECK_HOST_PATH.as_bytes()
        );
        let script = site.get(CHECK_HOST_SCRIPT_PATH).expect("host script");
        assert_eq!(script.kind, FileKind::HostScript);
        for runtime in RuntimeVersion::SHIPPED {
            for file in SandboxFile::ALL {
                let served = site.get(&file.path(runtime)).expect("sandbox file");
                assert_eq!(served.kind, FileKind::Sandbox(runtime, file));
                assert_eq!(header(served, "content-type"), Some(file.content_type()));
                let contract: &[(HeaderName, &str)] = if file == SandboxFile::Document {
                    &SANDBOX_DOCUMENT_HEADERS
                } else {
                    &SANDBOX_ASSET_HEADERS
                };
                for (name, value) in contract {
                    assert_eq!(header(served, name.as_str()), Some(*value));
                }
            }
            assert_eq!(
                site.compare_source(runtime),
                Some(SandboxFile::Compare.path(runtime).as_str())
            );
        }
        assert!(site.get("/sandbox/").is_none());
        assert!(site.get("/sandbox/v1/").is_none());
        assert!(site.get("/index.html").is_none());
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn a_missing_or_oversized_file_stops_the_load() {
        let dir = temp_dir("missing");
        write_site(&dir);
        let runtime = RuntimeVersion::CURRENT;
        std::fs::remove_file(file_path(&dir, &SandboxFile::D3.path(runtime))).expect("rm");
        let error = Site::load(&dir).expect_err("missing");
        assert!(error.to_string().contains("/sandbox/v1/d3.js"), "{error}");

        write_site(&dir);
        let big = vec![b'x'; usize::try_from(MAX_FILE_BYTES).expect("fits") + 1];
        std::fs::write(file_path(&dir, CHECK_HOST_SCRIPT_PATH), big).expect("write");
        let error = Site::load(&dir).expect_err("too large");
        assert!(matches!(error, SiteError::TooLarge { .. }), "{error}");

        write_site(&dir);
        std::fs::write(file_path(&dir, CHECK_HOST_PATH), [0xff, 0xfe]).expect("write");
        let error = Site::load(&dir).expect_err("not text");
        assert!(matches!(error, SiteError::NotText { .. }), "{error}");
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn only_urls_of_the_origin_without_query_have_a_path() {
        assert_eq!(
            path_of("https://check.invalid/sandbox/check.html"),
            Some("/sandbox/check.html")
        );
        assert_eq!(
            url("/sandbox/check.js"),
            "https://check.invalid/sandbox/check.js"
        );
        for other in [
            "https://check.invalid",
            "https://check.invalid/sandbox/v1/runtime.js?x=1",
            "https://check.invalid/sandbox/v1/runtime.js#x",
            "https://check.invalid.example/sandbox/check.html",
            "https://check.invalid@evil/sandbox/check.html",
            "http://check.invalid/sandbox/check.html",
            "data:text/html,1",
            "about:blank",
        ] {
            assert_eq!(path_of(other), None, "{other}");
        }
    }
}
