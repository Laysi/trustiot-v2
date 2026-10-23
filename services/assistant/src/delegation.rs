//! A question's run delegation (Laysi/trustiot-v2#551, #591): how the tools act as the user for
//! the whole question, longer than the 60 seconds of the request's token.
//!
//! The chat API (Laysi/trustiot-v2#146) calls [`QuestionRuns::start`] while it handles the user's
//! request, with the request's verified identity and the conversation's principal, and hands the
//! [`QuestionRun`] to [`crate::Agent::ask`]:
//!
//! ```ignore
//! let run = runs.start(&user, conversation.principal()).await?;
//! let answer = agent.ask(&mut conversation, &question, Arc::new(run)).await;
//! ```
//!
//! - The identity service issues the delegation for the request's token, bound to a key this
//!   process makes for the run; every tool call exchanges it for a 60-second token addressed to
//!   the service it calls (service-kit's `RunDelegation`).
//! - The question's deadline is the delegation's expiry minus [`RUN_END_MARGIN`]: the agent stops
//!   the question there ([`crate::AgentError::RunExpired`]), so no tool call is left without a
//!   token.
//! - When the identity service refuses an exchange because the user may no longer act (signed
//!   out, session ended or revoked, removed from the tenant), the question ends at that tool call
//!   ([`crate::AgentError::AccessRevoked`]) and the run asks for no other token. Any other failure
//!   of the identity service fails only that tool call.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::response::{IntoResponse, Response};
use contract::client::InternalClient;
use contract::{ErrorBody, ErrorCode, InternalToken, RunId, ServiceName, service};
use service_kit::identity::{RunDelegation, RunDelegationError, RunDelegations, UserIdentity};
use tokio::time::Instant;
use uuid::Uuid;

use crate::history::Principal;
use crate::model::BoxFuture;
use crate::tools::{Delegation, DelegationError};

/// How long before the delegation expires the question ends. It covers the clocks of this
/// service and the identity service drifting apart and the last exchange's round trip, so every
/// tool call that starts before the deadline still gets a token.
pub const RUN_END_MARGIN: Duration = Duration::from_secs(30);

/// The deadline of the calls to the identity service (starting and exchanging a delegation).
pub const IDENTITY_TIMEOUT: Duration = Duration::from_secs(15);

/// Starts the run delegation of each question.
#[derive(Clone, Debug)]
pub struct QuestionRuns {
    delegations: RunDelegations,
    lifetime: Duration,
}

impl QuestionRuns {
    /// Runs that start their delegations at the identity service's token exchange
    /// (`identity.exchange_url`), each living `lifetime` (`identity.run_lifetime`, checked by the
    /// configuration).
    pub fn new(identity: InternalClient<service::Identity>, lifetime: Duration) -> Self {
        Self {
            delegations: RunDelegations::new(identity),
            lifetime,
        }
    }

    /// Starts the delegation of one question of `principal`'s conversation, for `user`, the
    /// request's verified identity. Call it while the request's token is valid, before
    /// [`crate::Agent::ask`].
    ///
    /// # Errors
    ///
    /// [`StartError::NotTheConversationsUser`] when `user` is not the conversation's user in
    /// its tenant; [`StartError::Delegation`] when the identity service does not issue the
    /// delegation (the user's session ended, the user left the tenant, the service is down).
    pub async fn start(
        &self,
        user: &UserIdentity,
        principal: Principal,
    ) -> Result<QuestionRun, StartError> {
        if user.user_id() != principal.user_id || user.tenant_id() != Some(principal.tenant_id) {
            return Err(StartError::NotTheConversationsUser);
        }
        let run_id = RunId::generate();
        let delegation = self
            .delegations
            .start(user, run_id, self.lifetime)
            .await
            .map_err(StartError::Delegation)?;
        let deadline = deadline(
            delegation.expires_at(),
            unix_now(),
            Instant::now(),
            self.lifetime,
        );
        tracing::info!(
            run_id = %run_id,
            tenant_id = %principal.tenant_id,
            lifetime_secs = self.lifetime.as_secs(),
            deadline_in_secs = deadline.saturating_duration_since(Instant::now()).as_secs(),
            "question run started"
        );
        Ok(QuestionRun {
            inner: Arc::new(Inner {
                delegation,
                principal,
                run_id: Uuid::from(run_id),
                deadline,
                revoked: AtomicBool::new(false),
                exchanged: AtomicBool::new(false),
            }),
        })
    }
}

/// The question must end `RUN_END_MARGIN` before the delegation expires at `expires_at` (Unix
/// seconds, the identity service's clock), and never later than the lifetime it asked for.
fn deadline(expires_at: i64, now_unix: i64, now: Instant, lifetime: Duration) -> Instant {
    let left = u64::try_from(expires_at.saturating_sub(now_unix))
        .map(Duration::from_secs)
        .unwrap_or(Duration::ZERO)
        .min(lifetime);
    now + left.saturating_sub(RUN_END_MARGIN)
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
        })
}

/// One question's delegation, for [`crate::Agent::ask`]. Cheap to clone; it cannot be printed
/// with the delegation or the run's key (`Debug` shows the run and the deadline).
#[derive(Clone)]
pub struct QuestionRun {
    inner: Arc<Inner>,
}

struct Inner {
    delegation: RunDelegation,
    /// The conversation's tenant and user the delegation was started for: the agent refuses to
    /// run it for any other conversation's principal.
    principal: Principal,
    run_id: Uuid,
    deadline: Instant,
    /// Set by the first exchange the identity service refused for the user: no later call asks
    /// again.
    revoked: AtomicBool,
    /// Set by the first exchange that gave a token.
    exchanged: AtomicBool,
}

impl fmt::Debug for QuestionRun {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QuestionRun")
            .field("run_id", &self.inner.run_id)
            .field("deadline", &self.inner.deadline)
            .finish_non_exhaustive()
    }
}

impl QuestionRun {
    /// The question's run, as the identity service knows it.
    pub fn id(&self) -> Uuid {
        self.inner.run_id
    }

    /// The conversation's tenant and user the run was started for.
    pub fn principal(&self) -> Principal {
        self.inner.principal
    }
}

impl Delegation for QuestionRun {
    fn token_for(
        &self,
        audience: ServiceName,
    ) -> BoxFuture<'_, Result<InternalToken, DelegationError>> {
        Box::pin(async move {
            let inner = &self.inner;
            if inner.revoked.load(Ordering::Acquire) {
                return Err(DelegationError::Revoked);
            }
            match inner.delegation.token_for(audience).await {
                Ok(token) => {
                    inner.exchanged.store(true, Ordering::Release);
                    Ok(token)
                }
                Err(error) => {
                    let code = error.code();
                    let error = delegation_error(&error);
                    if error == DelegationError::Revoked {
                        let already_refused = inner.revoked.swap(true, Ordering::AcqRel);
                        // The delegation was issued moments ago while the user's session was
                        // valid, so a refusal before any exchange succeeded more likely means
                        // the deployment's mesh.calls lacks this call than a revocation.
                        if !already_refused && !inner.exchanged.load(Ordering::Acquire) {
                            tracing::warn!(
                                run_id = %inner.run_id,
                                audience = audience.as_str(),
                                code = code.as_str(),
                                "the identity service refused the first token exchange of a \
                                 question's run: check that mesh.calls lists this call"
                            );
                        }
                    }
                    Err(error)
                }
            }
        })
    }

    fn run_id(&self) -> Option<Uuid> {
        Some(self.inner.run_id)
    }

    fn deadline(&self) -> Option<Instant> {
        Some(self.inner.deadline)
    }

    fn principal(&self) -> Option<Principal> {
        Some(self.inner.principal)
    }
}

/// What a refused exchange means for the question: `unauthenticated` (the session ended or was
/// revoked, the delegation expired) and `permission_denied` (the user left the tenant, or the
/// deployment's `mesh.calls` lacks the call) end it; a timeout or an unavailable or failing
/// identity service fails only the call. service-kit logs both refusals at INFO, and the
/// question cannot tell a missing `mesh.calls` entry from a revocation: both end it with
/// [`crate::AgentError::AccessRevoked`].
fn delegation_error(error: &RunDelegationError) -> DelegationError {
    match error.code() {
        ErrorCode::Unauthenticated | ErrorCode::PermissionDenied => DelegationError::Revoked,
        _ => DelegationError::Unavailable,
    }
}

/// A question's delegation that could not be started.
///
/// Its `Debug` names the variant and the M1 code only: the identity service's error body, which
/// [`RunDelegationError`]'s own `Debug` carries, stays out of anything that logs it.
#[derive(thiserror::Error)]
pub enum StartError {
    /// The request's user is not the user of the conversation in its tenant: a fault of the
    /// caller. M1: `permission_denied`.
    #[error("the request's user is not the conversation's user")]
    NotTheConversationsUser,
    /// The identity service did not issue the delegation. M1: as
    /// [`RunDelegationError::code`] (`unauthenticated` when the user's session ended,
    /// `permission_denied` when the user left the tenant, `timeout`, `unavailable`, `internal`).
    #[error(transparent)]
    Delegation(RunDelegationError),
}

impl StartError {
    /// The M1 code to answer the request with.
    pub fn code(&self) -> ErrorCode {
        match self {
            Self::NotTheConversationsUser => ErrorCode::PermissionDenied,
            Self::Delegation(error) => error.code(),
        }
    }
}

impl fmt::Debug for StartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotTheConversationsUser => f.write_str("NotTheConversationsUser"),
            Self::Delegation(error) => f
                .debug_struct("Delegation")
                .field("code", &error.code())
                .finish_non_exhaustive(),
        }
    }
}

impl IntoResponse for StartError {
    /// The M1 body of [`StartError::code`], without detail.
    fn into_response(self) -> Response {
        let body = ErrorBody::new(self.code());
        (body.http_status(), axum::Json(body)).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIFETIME: Duration = Duration::from_secs(900);

    #[test]
    fn the_question_ends_a_margin_before_the_delegation_expires() {
        let now = Instant::now();
        let at = |secs| now + Duration::from_secs(secs);
        // 900 s left by the identity service's clock: 870 s.
        assert_eq!(deadline(1_000_900, 1_000_000, now, LIFETIME), at(870));
        // The identity service's clock is 5 s ahead: 5 s less.
        assert_eq!(deadline(1_000_900, 1_000_005, now, LIFETIME), at(865));
        // Never later than the lifetime asked for, whatever the identity service said.
        assert_eq!(deadline(9_000_000, 1_000_000, now, LIFETIME), at(870));
        // Less than the margin left, or already expired: the deadline is now.
        assert_eq!(deadline(1_000_020, 1_000_000, now, LIFETIME), now);
        assert_eq!(deadline(999_000, 1_000_000, now, LIFETIME), now);
        assert_eq!(deadline(i64::MIN, i64::MAX, now, LIFETIME), now);
    }

    #[test]
    fn start_errors_answer_with_m1_codes_and_no_detail() {
        let error = StartError::NotTheConversationsUser;
        assert_eq!(error.code(), ErrorCode::PermissionDenied);
        assert_eq!(
            error.into_response().status(),
            axum::http::StatusCode::FORBIDDEN
        );
        let error = StartError::Delegation(RunDelegationError::Lifetime);
        assert_eq!(error.code(), ErrorCode::Internal);
    }
}
