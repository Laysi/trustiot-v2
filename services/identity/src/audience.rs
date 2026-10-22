//! Which service a checked request goes to, and so the audience (`aud`) of its token.
//!
//! The ingress gateway sends only the original request (Host, method, path, cookie) to the
//! ext_authz check, not the route it picked. The chart therefore gives the identity service the
//! same cookie routes it gives the gateway, as the platform setting [`ROUTES_SETTING`]
//! (`TRUSTIOT_INGRESS_ROUTES`), and this module matches a request the way Gateway API `HTTPRoute`
//! does: the host must equal the route's host (any port), an `Exact` route wins over every
//! `PathPrefix` route, and the longest `PathPrefix` that matches whole path segments wins.
//!
//! A mismatch can only deny, never widen access: a token works only at its audience, so a token
//! for the wrong service is rejected there (`wrong_audience`), and the client never sees tokens.

use std::fmt;
use std::str::FromStr;
use std::sync::Arc;

use contract::ServiceName;
use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer};

/// The platform setting with the ingress gateway's cookie routes: `platform.ingress_routes`,
/// set by the chart as `TRUSTIOT_INGRESS_ROUTES` on the identity service only.
pub const ROUTES_SETTING: &str = "ingress_routes";

/// Most routes accepted; far more than the services need, and a bound on a malformed setting.
pub const MAX_ROUTES: usize = 512;

/// Path prefix the ingress never routes (decision 27); no cookie route may be under it.
const INTERNAL_PREFIX: &str = "/internal";

/// How a route matches the path, as in Gateway API.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
pub enum PathMatch {
    /// The path equals the route's path.
    Exact,
    /// The route's path is a prefix of the path, ending at a segment boundary.
    PathPrefix,
}

/// One cookie route, as the chart renders it.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRoute {
    host: String,
    path: String,
    #[serde(rename = "match")]
    path_match: PathMatch,
    service: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Route {
    host: String,
    path: String,
    path_match: PathMatch,
    service: ServiceName,
}

/// The ingress gateway's cookie routes, checked once at startup. Cloning is cheap.
#[derive(Clone, Debug, Default)]
pub struct IngressRoutes {
    routes: Arc<[Route]>,
}

impl IngressRoutes {
    /// Parses the JSON array the chart renders, for example
    /// `[{"host":"app.example.com","path":"/api/assets","match":"PathPrefix","service":"assets"}]`.
    /// Hosts are lower-case DNS names, paths are `/` or lower-case segments as the chart allows,
    /// never under `/internal`, and each host and path pair appears once. An empty array is
    /// valid: then no request gets a token.
    ///
    /// # Errors
    ///
    /// When the text is not such an array; the error names the position and the rule, never the
    /// text.
    pub fn from_json(text: &str) -> Result<Self, InvalidRoutes> {
        let raw: Vec<RawRoute> =
            serde_json::from_str(text).map_err(|error| InvalidRoutes::NotRoutes(error.into()))?;
        if raw.len() > MAX_ROUTES {
            return Err(InvalidRoutes::TooMany);
        }
        let mut routes: Vec<Route> = Vec::with_capacity(raw.len());
        for (index, route) in raw.into_iter().enumerate() {
            let invalid = |problem| InvalidRoutes::Route { index, problem };
            if !is_host(&route.host) {
                return Err(invalid(RouteProblem::Host));
            }
            if !is_route_path(&route.path) {
                return Err(invalid(RouteProblem::Path));
            }
            if route.path.starts_with(INTERNAL_PREFIX) {
                return Err(invalid(RouteProblem::Internal));
            }
            let service: ServiceName = route
                .service
                .parse()
                .map_err(|_| invalid(RouteProblem::Service))?;
            if routes
                .iter()
                .any(|seen| seen.host == route.host && seen.path == route.path)
            {
                return Err(invalid(RouteProblem::Duplicate));
            }
            routes.push(Route {
                host: route.host,
                path: route.path,
                path_match: route.path_match,
                service,
            });
        }
        Ok(Self {
            routes: routes.into(),
        })
    }

    /// The service a request for `host` (the `Host` header, with or without a port) and `path`
    /// (without the query) is routed to, or `None` when no route matches.
    #[must_use]
    pub fn audience(&self, host: &str, path: &str) -> Option<ServiceName> {
        let host = normalize_host(host)?;
        let candidates = || self.routes.iter().filter(|route| route.host == host);
        if let Some(exact) =
            candidates().find(|route| route.path_match == PathMatch::Exact && route.path == path)
        {
            return Some(exact.service);
        }
        candidates()
            .filter(|route| route.path_match == PathMatch::PathPrefix)
            .filter(|route| prefix_matches(&route.path, path))
            .max_by_key(|route| route.path.len())
            .map(|route| route.service)
    }

    /// How many routes there are.
    #[must_use]
    pub fn len(&self) -> usize {
        self.routes.len()
    }

    /// Whether there are no routes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.routes.is_empty()
    }
}

/// Gateway API `PathPrefix`: `/api/assets` matches `/api/assets`, `/api/assets/` and
/// `/api/assets/x`, but not `/api/assetsx`; `/` matches every path.
fn prefix_matches(prefix: &str, path: &str) -> bool {
    if prefix == "/" {
        return path.starts_with('/');
    }
    path.strip_prefix(prefix)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// The host part of a `Host` header, lower-cased, without a port or a trailing dot. `None` for
/// an IPv6 literal or an empty host, which no route names. The cross-site request check
/// ([`crate::csrf`]) compares an `Origin`'s host with the `Host` the same way.
pub(crate) fn normalize_host(host: &str) -> Option<String> {
    let host = match host.rsplit_once(':') {
        Some((name, port)) if port.bytes().all(|byte| byte.is_ascii_digit()) => name,
        Some(_) => return None,
        None => host,
    };
    let host = host.strip_suffix('.').unwrap_or(host);
    if host.is_empty() || host.starts_with('[') {
        return None;
    }
    Some(host.to_ascii_lowercase())
}

/// A lower-case DNS name, as the chart's schema requires for `hosts.*`.
fn is_host(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 253
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        })
}

/// `/`, or segments that start with a lower-case letter or digit followed by lower-case letters,
/// digits, `.`, `_` or `-` (the chart's `exactPathPattern`): no empty or dot segments, no
/// trailing `/`, no `%`, `;` or wildcards.
fn is_route_path(path: &str) -> bool {
    if path == "/" {
        return true;
    }
    let Some(rest) = path.strip_prefix('/') else {
        return false;
    };
    rest.split('/').all(|segment| {
        let mut bytes = segment.bytes();
        bytes
            .next()
            .is_some_and(|first| first.is_ascii_lowercase() || first.is_ascii_digit())
            && bytes.all(|byte| {
                byte.is_ascii_lowercase()
                    || byte.is_ascii_digit()
                    || matches!(byte, b'.' | b'_' | b'-')
            })
    })
}

impl FromStr for IngressRoutes {
    type Err = InvalidRoutes;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::from_json(text)
    }
}

impl<'de> Deserialize<'de> for IngressRoutes {
    /// From the JSON text, so it can be a configuration value.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct RoutesText;

        impl Visitor<'_> for RoutesText {
            type Value = IngressRoutes;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("the ingress cookie routes as JSON text")
            }

            fn visit_str<E: de::Error>(self, text: &str) -> Result<IngressRoutes, E> {
                IngressRoutes::from_json(text).map_err(E::custom)
            }
        }

        deserializer.deserialize_str(RoutesText)
    }
}

/// Why the routes setting cannot be used. Messages never quote the setting.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum InvalidRoutes {
    /// Not a JSON array of route objects with exactly `host`, `path`, `match` and `service`.
    #[error("not a JSON array of routes with host, path, match and service ({0})")]
    NotRoutes(contract::json::JsonError),
    /// More than [`MAX_ROUTES`] routes.
    #[error("more than {MAX_ROUTES} routes")]
    TooMany,
    /// One route cannot be used.
    #[error("route {index} (counting from 0) {problem}")]
    Route {
        /// Position in the array, from 0.
        index: usize,
        /// What is wrong with it.
        problem: RouteProblem,
    },
}

/// What is wrong with one route.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum RouteProblem {
    /// The host is not a lower-case DNS name.
    #[error("has a host that is not a lower-case DNS name")]
    Host,
    /// The path is not `/` or lower-case segments.
    #[error("has a path that is not '/' or lower-case segments")]
    Path,
    /// The path is under `/internal`, which the ingress never routes.
    #[error("has a path under /internal, which the ingress never routes")]
    Internal,
    /// The service is not one the identity service can issue tokens for.
    #[error("names a service that cannot receive internal identity tokens")]
    Service,
    /// An earlier route has the same host and path.
    #[error("repeats the host and path of an earlier route")]
    Duplicate,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn routes(value: serde_json::Value) -> IngressRoutes {
        IngressRoutes::from_json(&value.to_string()).expect("valid routes")
    }

    fn route(host: &str, path: &str, kind: &str, service: &str) -> serde_json::Value {
        json!({"host": host, "path": path, "match": kind, "service": service})
    }

    fn table() -> IngressRoutes {
        routes(json!([
            route("app.example.com", "/", "PathPrefix", "assets"),
            route("app.example.com", "/api/certs", "PathPrefix", "certs"),
            route(
                "app.example.com",
                "/api/certs/admin",
                "PathPrefix",
                "alerts"
            ),
            route(
                "app.example.com",
                "/api/certs/admin/x",
                "Exact",
                "knowledge"
            ),
            route("app.example.com", "/api/members", "Exact", "identity"),
            route(
                "gw.example.com",
                "/api/gateway",
                "PathPrefix",
                "gateway-ops"
            ),
        ]))
    }

    #[test]
    fn exact_wins_then_the_longest_prefix_on_segment_boundaries() {
        let table = table();
        let cases = [
            ("/", Some(ServiceName::Assets)),
            ("/anything/else", Some(ServiceName::Assets)),
            ("/api/certs", Some(ServiceName::Certs)),
            ("/api/certs/", Some(ServiceName::Certs)),
            ("/api/certs/123", Some(ServiceName::Certs)),
            // Not a segment boundary: falls back to the catch-all.
            ("/api/certsx", Some(ServiceName::Assets)),
            ("/api/certs/admin", Some(ServiceName::Alerts)),
            ("/api/certs/admin/y", Some(ServiceName::Alerts)),
            ("/api/certs/admin/x", Some(ServiceName::Knowledge)),
            // Exact means exact: a trailing slash is another path.
            ("/api/certs/admin/x/", Some(ServiceName::Alerts)),
            ("/api/members", Some(ServiceName::Identity)),
            ("/api/members/1", Some(ServiceName::Assets)),
        ];
        for (path, expected) in cases {
            assert_eq!(table.audience("app.example.com", path), expected, "{path}");
        }
    }

    #[test]
    fn the_host_must_match_with_any_port_and_in_any_case() {
        let table = table();
        for host in [
            "app.example.com",
            "app.example.com:443",
            "app.example.com:8443",
            "APP.Example.COM",
            "app.example.com.",
            "app.example.com.:80",
        ] {
            assert_eq!(
                table.audience(host, "/api/certs"),
                Some(ServiceName::Certs),
                "{host}"
            );
        }
        for host in [
            "",
            "example.com",
            "evil.app.example.com",
            "app.example.com.evil",
            "app.example.com:https",
            "[::1]:443",
            "gw.example.com",
        ] {
            assert_eq!(table.audience(host, "/api/certs"), None, "{host}");
        }
        assert_eq!(
            table.audience("gw.example.com:443", "/api/gateway/1"),
            Some(ServiceName::GatewayOps)
        );
        assert_eq!(table.audience("gw.example.com", "/"), None);
    }

    #[test]
    fn without_routes_nothing_has_an_audience() {
        let empty = routes(json!([]));
        assert!(empty.is_empty());
        assert_eq!(empty.audience("app.example.com", "/"), None);
        assert_eq!(table().len(), 6);
    }

    #[test]
    fn unusable_routes_stop_startup_without_quoting_the_setting() {
        let cases = [
            (
                json!([route("App.example.com", "/", "PathPrefix", "assets")]),
                RouteProblem::Host,
            ),
            (
                json!([route("app.example.com:443", "/", "PathPrefix", "assets")]),
                RouteProblem::Host,
            ),
            (
                json!([route("", "/", "PathPrefix", "assets")]),
                RouteProblem::Host,
            ),
            (
                json!([route("app.example.com", "", "PathPrefix", "assets")]),
                RouteProblem::Path,
            ),
            (
                json!([route("app.example.com", "api", "PathPrefix", "assets")]),
                RouteProblem::Path,
            ),
            (
                json!([route("app.example.com", "/api/", "PathPrefix", "assets")]),
                RouteProblem::Path,
            ),
            (
                json!([route("app.example.com", "//api", "PathPrefix", "assets")]),
                RouteProblem::Path,
            ),
            (
                json!([route(
                    "app.example.com",
                    "/api/../x",
                    "PathPrefix",
                    "assets"
                )]),
                RouteProblem::Path,
            ),
            (
                json!([route("app.example.com", "/API", "PathPrefix", "assets")]),
                RouteProblem::Path,
            ),
            (
                json!([route("app.example.com", "/a%2fb", "PathPrefix", "assets")]),
                RouteProblem::Path,
            ),
            (
                json!([route("app.example.com", "/internal", "Exact", "assets")]),
                RouteProblem::Internal,
            ),
            (
                json!([route(
                    "app.example.com",
                    "/internal/x",
                    "PathPrefix",
                    "assets"
                )]),
                RouteProblem::Internal,
            ),
            (
                json!([route("app.example.com", "/", "PathPrefix", "ai-gateway")]),
                RouteProblem::Service,
            ),
            (
                json!([route(
                    "app.example.com",
                    "/",
                    "PathPrefix",
                    "do-not-echo-this-value"
                )]),
                RouteProblem::Service,
            ),
            (
                json!([
                    route("app.example.com", "/x", "PathPrefix", "assets"),
                    route("app.example.com", "/x", "Exact", "certs"),
                ]),
                RouteProblem::Duplicate,
            ),
        ];
        for (value, expected) in cases {
            let error = IngressRoutes::from_json(&value.to_string()).expect_err("invalid");
            let index = value.as_array().map_or(0, |routes| routes.len() - 1);
            assert_eq!(
                error,
                InvalidRoutes::Route {
                    index,
                    problem: expected
                },
                "{value}"
            );
            assert!(
                !error.to_string().contains("do-not-echo-this-value"),
                "{error}"
            );
        }
    }

    #[test]
    fn the_setting_must_be_an_array_of_exact_route_objects() {
        for text in [
            "",
            "{}",
            r#"[{"host":"app.example.com","path":"/","match":"PathPrefix"}]"#,
            r#"[{"host":"app.example.com","path":"/","match":"Regex","service":"assets"}]"#,
            r#"[{"host":"app.example.com","path":"/","match":"PathPrefix","service":"assets","port":"http"}]"#,
            r#"[{"host":"do-not-echo-this-value","path":"/","match":"PathPrefix","service":"assets","host":"x"}]"#,
        ] {
            let error = IngressRoutes::from_json(text).expect_err("invalid");
            assert!(
                matches!(error, InvalidRoutes::NotRoutes(_)),
                "{text}: {error:?}"
            );
            assert!(
                !error.to_string().contains("do-not-echo-this-value"),
                "{error}"
            );
        }
        let too_many: Vec<_> = (0..=MAX_ROUTES)
            .map(|i| route("app.example.com", &format!("/r{i}"), "Exact", "assets"))
            .collect();
        assert_eq!(
            IngressRoutes::from_json(&json!(too_many).to_string()).err(),
            Some(InvalidRoutes::TooMany)
        );
    }

    #[test]
    fn the_setting_deserializes_from_its_json_text() {
        #[derive(Debug, Deserialize)]
        struct Config {
            ingress_routes: IngressRoutes,
        }
        let text = json!([route("app.example.com", "/", "PathPrefix", "assets")]).to_string();
        let config: Config =
            serde_json::from_value(json!({ "ingress_routes": text })).expect("deserialize");
        assert_eq!(
            config.ingress_routes.audience("app.example.com", "/x"),
            Some(ServiceName::Assets)
        );
        assert_eq!(text.parse::<IngressRoutes>().map(|r| r.len()), Ok(1));
    }
}
