//! The draft versions each answer lists (Laysi/trustiot-v2#1043; docs/architecture/ai-assistant.md,
//! 「儀表板」, 「回答帶出的元件」): every version of a draft that the `dashboard` tool wrote
//! during the question whose answer it is, in the order they were written. Schema:
//! `migrations/postgres/0007_answer_drafts.sql` (an answered run's `answer_seq`) and the run each
//! of the tool's versions records (`0003_dashboards.sql`, `dashboard_versions.run_id`).
//!
//! Only the conversation's own drafts and its own answered runs count: a version reaches an
//! answer only through a run of this conversation that ended `answered` with that answer, and a
//! draft of this conversation, all in the transaction's tenant (row-level security, and
//! `tenant_id` in every join). A run that failed, was cancelled or was interrupted left no answer
//! and has no `answer_seq`, so the versions it wrote are listed under no answer; they stay in the
//! conversation's drafts. A discarded round's run keeps its `answer_seq`, but the readers show no
//! answer of a discarded round, so its versions are listed nowhere either.
//!
//! Who reads: the conversation's owner, like [`super::load`], or another member of the tenant
//! while the owner shares it, like [`super::shared_view`]; both only while it is not deleted. The
//! statement checks that itself, so the list and the access verdict come from one snapshot, the
//! same verdict `dashboards::store::conversation_access` gives for the drafts themselves. A copy
//! is a conversation of its own, which has no drafts: its answers list nothing.

use contract::DashboardId;
use service_kit::tenant::TenantTransaction;
use sqlx::Row;
use uuid::Uuid;

use super::{StoreError, check_tenant};
use crate::history::Principal;

/// `$1` tenant, `$2` conversation, `$3` the caller, `$4` whether the caller reads as its owner
/// (else as a reader of the shared conversation): the versions each answered run of the
/// conversation wrote into the conversation's drafts, by the position of the run's answer, then
/// in the order they were written.
///
/// The order written: a version's `created_at` is the database's `now()` of the tool's
/// transaction that wrote it, and the tool's calls of one question run step by step, each step's
/// writes committed before the model gets their results, so a later step's version starts its
/// transaction after the earlier one committed and has the later time. Calls of one step run at
/// once and have no order of their own; the time their transactions started orders them, then
/// the dashboard's ID (UUIDv7) and the version, so the list is the same on every read. Versions
/// of one draft are also ordered by their number.
///
/// The drafts of the conversation are found through `dashboards_conversation` and their versions
/// through the primary key, so the statement reads only this conversation's rows.
const ANSWER_DRAFTS: &str = "SELECT r.answer_seq, v.dashboard_id::text AS dashboard_id, v.version
FROM assistant.conversations c
JOIN assistant.runs r
    ON r.tenant_id = c.tenant_id AND r.conversation_id = c.conversation_id
JOIN assistant.dashboards d
    ON d.tenant_id = c.tenant_id AND d.conversation_id = c.conversation_id
JOIN assistant.dashboard_versions v
    ON v.tenant_id = d.tenant_id AND v.dashboard_id = d.dashboard_id AND v.run_id = r.run_id
WHERE c.tenant_id = $1::uuid AND c.conversation_id = $2::uuid AND c.deleted_at IS NULL
    AND (($4::boolean AND c.user_id = $3::uuid)
        OR (NOT $4::boolean AND c.shared_at IS NOT NULL AND c.user_id <> $3::uuid))
    AND r.status = 'answered' AND r.answer_seq IS NOT NULL
    AND d.kind = 'draft' AND v.origin = 'agent'
ORDER BY r.answer_seq, v.created_at, v.dashboard_id, v.version";

/// Who reads a conversation's answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnswerReader {
    /// The conversation's owner ([`super::load`]).
    Owner,
    /// Another member of the tenant, while the owner shares it ([`super::shared_view`]).
    SharedReader,
}

/// One draft version an answer lists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AnswerDraft {
    /// The position of the answer's message in the database: [`super::Loaded::positions`] and
    /// [`super::ShownEntry::position`] give the same for the messages they read.
    pub answer_seq: usize,
    /// The draft.
    pub dashboard_id: DashboardId,
    /// Its version.
    pub version: u32,
}

/// The draft versions the answers of conversation `id` list, for `principal` reading as
/// `reader`: by the position of their answer, then in the order they were written. Empty when
/// `principal` may not read the conversation that way (not theirs, not shared with them,
/// deleted, or none), exactly as when no answer lists any.
///
/// # Errors
///
/// [`StoreError::WrongTenant`], [`StoreError::Corrupt`] or a database failure.
pub async fn answer_drafts(
    tx: &mut TenantTransaction,
    principal: Principal,
    id: Uuid,
    reader: AnswerReader,
) -> Result<Vec<AnswerDraft>, StoreError> {
    check_tenant(tx, principal.tenant_id)?;
    let rows = sqlx::query(ANSWER_DRAFTS)
        .bind(principal.tenant_id.to_string())
        .bind(id.to_string())
        .bind(principal.user_id.to_string())
        .bind(reader == AnswerReader::Owner)
        .fetch_all(&mut **tx)
        .await?;
    rows.iter()
        .map(|row| {
            let answer_seq = usize::try_from(row.try_get::<i32, _>("answer_seq")?)
                .map_err(|_| StoreError::Corrupt("a negative answer position"))?;
            let dashboard_id: Uuid = row
                .try_get::<String, _>("dashboard_id")?
                .parse()
                .map_err(|_| StoreError::Corrupt("a dashboard ID"))?;
            let version = u32::try_from(row.try_get::<i32, _>("version")?)
                .map_err(|_| StoreError::Corrupt("a dashboard version"))?;
            Ok(AnswerDraft {
                answer_seq,
                dashboard_id: DashboardId::from_uuid(dashboard_id),
                version,
            })
        })
        .collect()
}
