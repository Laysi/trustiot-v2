//! Calls from the tools to other services' APIs as the question's user (decision 27,
//! Laysi/trustiot-v2#591): the same routes the web client calls through the ingress gateway, with
//! a token the question's delegation exchanges for that service before every call
//! ([`crate::tools::Delegation::token_for`]). The callee checks the token's audience, the user's
//! tenant and role as it does for any request, so a tool reads exactly what the user may read.
//!
//! - The token's audience is the client's service, fixed when the client is built: a tool
//!   cannot address a token elsewhere.
//! - No tenant or user parameter is sent; the callee takes both from the token.
//! - Only a success body is read. An error answer is reduced to a [`ToolError`] by its status;
//!   its body is never read, so nothing of it reaches the model or the logs. A failure is logged
//!   with the tool, the service and the status class or kind of failure only, never the URL (it
//!   carries the model's input) or the token.
//! - Plain HTTP inside the mesh, where ztunnel adds mutual TLS; no proxy from the environment,
//!   no redirects (the token header would follow one).

use std::time::Duration;

use contract::{INTERNAL_IDENTITY_HEADER, ServiceName};
use reqwest::header::{ACCEPT, HeaderValue};
use serde::de::DeserializeOwned;

use crate::tools::{ToolContext, ToolError};

/// The longest a connection to a service may take to open.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// The largest success body read from a service. The largest answer of the current tools, a raw
/// series of 10,000 readings, is about 2 MB.
pub const MAX_RESPONSE_BYTES: usize = 8 << 20;

/// A service client could not be built.
#[derive(Debug, thiserror::Error)]
pub enum CalleeConfigError {
    #[error("the URL of {service} is invalid: {reason}")]
    Url {
        service: ServiceName,
        reason: &'static str,
    },
    #[error("the HTTP client for {service} could not be built")]
    Client {
        service: ServiceName,
        #[source]
        source: reqwest::Error,
    },
}

/// One service the tools call as the user.
#[derive(Clone, Debug)]
pub struct Callee {
    service: ServiceName,
    http: reqwest::Client,
    base_url: reqwest::Url,
}

/// One GET request: the path (already rendered from a route whose parameters the tool checked)
/// and the query string.
#[derive(Debug)]
pub struct Get<'a> {
    /// The tool that calls, for the logs.
    pub tool: &'static str,
    /// The request path, such as `/api/assets/v1/device-models`.
    pub path: &'a str,
    pub query: &'a [(&'static str, &'a str)],
    /// What the model is told when the service rejects the parameters (400 or 422): which rules
    /// they break and how to call the tool instead. Fixed text; the service's answer is not read.
    pub rejected_input: &'static str,
}

impl Callee {
    /// A client for `service` at `base_url`, a plain `http` origin (checked by the configuration
    /// too). `timeout` bounds a whole call; the tool's own time limit is the real bound.
    pub fn new(
        service: ServiceName,
        base_url: &str,
        timeout: Duration,
    ) -> Result<Self, CalleeConfigError> {
        crate::config::check_service_url(base_url)
            .map_err(|reason| CalleeConfigError::Url { service, reason })?;
        let base_url = reqwest::Url::parse(base_url).map_err(|_| CalleeConfigError::Url {
            service,
            reason: "is not an absolute URL",
        })?;
        let http = reqwest::Client::builder()
            .http1_only()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(timeout)
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|source| CalleeConfigError::Client { service, source })?;
        Ok(Self {
            service,
            http,
            base_url,
        })
    }

    /// The service this client calls, the audience of its tokens.
    pub fn service(&self) -> ServiceName {
        self.service
    }

    /// Calls `request` as the question's user and parses the success body as `T`.
    ///
    /// # Errors
    ///
    /// [`ToolError::Revoked`] when the delegation was revoked (the question ends);
    /// [`ToolError::Input`] with [`Get::rejected_input`] for 400 and 422;
    /// [`ToolError::Denied`] for 403 and 404; [`ToolError::Failed`] for anything else.
    pub async fn get<T: DeserializeOwned>(
        &self,
        context: &ToolContext,
        request: Get<'_>,
    ) -> Result<T, ToolError> {
        // A fresh token for every call: it lives 60 seconds, and the identity service checks the
        // user's session and membership again on each exchange.
        let token = context
            .delegation
            .token_for(self.service)
            .await
            .map_err(ToolError::from)?;
        let mut url = self.base_url.clone();
        url.set_path(request.path);
        if !request.query.is_empty() {
            url.query_pairs_mut().extend_pairs(request.query.iter());
        }
        let mut header = HeaderValue::from_str(token.expose()).map_err(|_| {
            self.log_failure(context, &request, "invalid_token", None);
            ToolError::Failed
        })?;
        header.set_sensitive(true);
        let response = self
            .http
            .get(url)
            .header(INTERNAL_IDENTITY_HEADER, header)
            .header(ACCEPT, "application/json")
            .send()
            .await
            .map_err(|error| {
                let kind = if error.is_timeout() {
                    "timeout"
                } else if error.is_connect() {
                    "connect"
                } else {
                    "transport"
                };
                self.log_failure(context, &request, kind, None);
                ToolError::Failed
            })?;
        let status = response.status().as_u16();
        if !response.status().is_success() {
            // The body is dropped unread.
            drop(response);
            self.log_failure(context, &request, "error_status", Some(status));
            return Err(match status {
                400 | 422 => ToolError::Input(request.rejected_input.to_owned()),
                403 | 404 => ToolError::Denied,
                _ => ToolError::Failed,
            });
        }
        let body = read_limited(response, MAX_RESPONSE_BYTES)
            .await
            .map_err(|kind| {
                self.log_failure(context, &request, kind, Some(status));
                ToolError::Failed
            })?;
        serde_json::from_slice(&body).map_err(|_| {
            self.log_failure(context, &request, "invalid_response", Some(status));
            ToolError::Failed
        })
    }

    /// One line per failed call: which tool and service, the kind of failure and the status
    /// class. Never the URL, the token or anything of the answer.
    fn log_failure(
        &self,
        context: &ToolContext,
        request: &Get<'_>,
        kind: &'static str,
        status: Option<u16>,
    ) {
        let status_class = status.map(status_class);
        let refused = matches!(status, Some(400 | 403 | 404 | 422));
        if refused {
            tracing::info!(
                run_id = %context.run_id,
                conversation_id = %context.conversation_id,
                tenant_id = %context.principal.tenant_id,
                tool = request.tool,
                peer = self.service.as_str(),
                kind,
                status_class,
                "a service refused a tool call"
            );
        } else {
            tracing::warn!(
                run_id = %context.run_id,
                conversation_id = %context.conversation_id,
                tenant_id = %context.principal.tenant_id,
                tool = request.tool,
                peer = self.service.as_str(),
                kind,
                status_class,
                "a tool call to a service failed"
            );
        }
    }
}

/// `2xx` to `5xx`: what the logs say of a service's status.
fn status_class(status: u16) -> &'static str {
    match status {
        100..=199 => "1xx",
        200..=299 => "2xx",
        300..=399 => "3xx",
        400..=499 => "4xx",
        _ => "5xx",
    }
}

async fn read_limited(
    mut response: reqwest::Response,
    limit: usize,
) -> Result<Vec<u8>, &'static str> {
    let limit_u64 = u64::try_from(limit).unwrap_or(u64::MAX);
    if response
        .content_length()
        .is_some_and(|length| length > limit_u64)
    {
        return Err("too_large");
    }
    let mut body = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                if body.len().saturating_add(chunk.len()) > limit {
                    return Err("too_large");
                }
                body.extend_from_slice(&chunk);
            }
            Ok(None) => return Ok(body),
            Err(error) if error.is_timeout() => return Err("timeout"),
            Err(_) => return Err("transport"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_are_logged_by_class() {
        assert_eq!(status_class(301), "3xx");
        assert_eq!(status_class(403), "4xx");
        assert_eq!(status_class(404), "4xx");
        assert_eq!(status_class(503), "5xx");
        assert_eq!(status_class(599), "5xx");
    }

    #[test]
    fn only_plain_http_origins_are_accepted() {
        let timeout = Duration::from_secs(5);
        assert!(Callee::new(ServiceName::Assets, "http://assets:8080", timeout).is_ok());
        for bad in [
            "https://assets:8080",
            "http://assets:8080/api",
            "http://user:secret@assets:8080",
            "http://assets:8080?tenant=1",
            "assets:8080",
        ] {
            assert!(
                Callee::new(ServiceName::Assets, bad, timeout).is_err(),
                "{bad} was accepted"
            );
        }
    }
}
