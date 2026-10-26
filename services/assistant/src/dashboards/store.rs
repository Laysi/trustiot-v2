//! Dashboards and their versions in Postgres (Laysi/trustiot-v2#150; schema:
//! `migrations/postgres/0002_dashboards.sql`).
//!
//! Every function is one whole operation: call it alone in a [`TenantTransaction`] of the
//! caller's tenant and commit when it returns `Ok` (roll back otherwise). The public API opens
//! the transaction with the request's identity (`TenantDb::begin`); the `dashboard` tool with
//! its question's tenant (`BackgroundTenantDb::begin`). Row-level security keeps the transaction
//! inside its tenant; this module decides who in the tenant sees what:
//!
//! - a saved dashboard: every member of the tenant ([`Access::Member`]);
//! - a draft: its conversation's owner only ([`Access::Owner`]), decided in exactly one place,
//!   [`conversation_access`]. Laysi/trustiot-v2#644 (soft-deleted conversations) adds
//!   `AND deleted_at IS NULL` to its one statement, and Laysi/trustiot-v2#650 (shared readers)
//!   adds a reader's access there;
//! - a dashboard the caller cannot see is [`StoreError::NotFound`], the same as one that does
//!   not exist.
//!
//! Writes lock the dashboard's row (`FOR UPDATE`) before they read its latest version, so the
//! writes of one dashboard queue up and a write based on an older version is refused
//! ([`StoreError::VersionConflict`], [`StoreError::SavedChanged`]). Publishing locks the draft
//! first and then its saved dashboard; nothing locks in the other order. Versions are only ever
//! added, numbered from 1 without gaps; restoring and publishing copy a version in the database,
//! so a copy is byte for byte its source. Who may publish, restore and archive is checked here,
//! from the [`Requester`]'s role and [`Access`], with the same rule the API shows as
//! [`Permissions`] ([`permissions`]).
//!
//! A draft's conversation is read without a row lock: the service role cannot lock
//! conversations (it has no UPDATE on them). A conversation purged while a version is written
//! cannot leave anything behind: the purge's cascade waits for the dashboard's row lock, and a
//! new draft's foreign key check waits for the purge. Laysi/trustiot-v2#644, which adds soft
//! delete and gives the role a column UPDATE on conversations, locks the conversation
//! (`FOR SHARE`) in [`conversation_access`] for writes.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Utc};
use contract::api::dashboard::RuntimeVersion;
use contract::api::dashboard::public::{
    ArchiveState, DashboardKind, DashboardList, DashboardSummary, Permissions, SavedBase,
    VersionContent, VersionCreated, VersionList, VersionOrigin, VersionSummary,
};
use contract::api::dashboard::spec::{Dataset, Spec, SpecError, validate_version};
use contract::{DashboardId, TenantId, TenantRole, UserId};
use service_kit::tenant::TenantTransaction;
use sqlx::Row;
use sqlx::postgres::PgRow;
use uuid::Uuid;

use crate::history::Principal;

/// The most versions a dashboard may have (the table's limit).
pub const MAX_VERSIONS: u32 = 10_000;

/// The most dashboards or versions in one page of a list.
pub const PAGE_SIZE: u32 = 50;

/// `$1` tenant, `$2` conversation: its owner. The one statement that decides who sees a
/// conversation's drafts ([`conversation_access`]). Laysi/trustiot-v2#644 adds
/// `AND deleted_at IS NULL`.
const CONVERSATION_OWNER: &str = "SELECT user_id::text AS user_id
FROM assistant.conversations
WHERE tenant_id = $1::uuid AND conversation_id = $2::uuid";

/// `$1` tenant, `$2` dashboard: the row, locked for a write.
const LOCK_DASHBOARD: &str = "SELECT dashboard_id::text AS dashboard_id, kind,
    conversation_id::text AS conversation_id, saved_dashboard_id::text AS saved_dashboard_id,
    base_saved_version, created_by::text AS created_by, created_at, archived_at
FROM assistant.dashboards
WHERE tenant_id = $1::uuid AND dashboard_id = $2::uuid
FOR UPDATE";

/// `$1` tenant, `$2` dashboard: the row with its latest version's number, title and time.
const SELECT_DASHBOARD: &str = "SELECT d.dashboard_id::text AS dashboard_id, d.kind,
    d.conversation_id::text AS conversation_id, d.saved_dashboard_id::text AS saved_dashboard_id,
    d.base_saved_version, d.created_by::text AS created_by, d.created_at, d.archived_at,
    v.version AS latest_version, v.title, v.created_at AS updated_at
FROM assistant.dashboards d
JOIN LATERAL (
    SELECT version, title, created_at
    FROM assistant.dashboard_versions
    WHERE tenant_id = d.tenant_id AND dashboard_id = d.dashboard_id
    ORDER BY version DESC
    LIMIT 1
) v ON true
WHERE d.tenant_id = $1::uuid AND d.dashboard_id = $2::uuid";

/// `$1` tenant, `$2` whether archived ones are included, `$3` and `$4` the cursor (the last
/// row's update time and ID, or NULL), `$5` the page size: saved dashboards, the most recently
/// updated first.
const LIST_SAVED: &str = "SELECT d.dashboard_id::text AS dashboard_id, d.kind,
    d.conversation_id::text AS conversation_id, d.saved_dashboard_id::text AS saved_dashboard_id,
    d.base_saved_version, d.created_by::text AS created_by, d.created_at, d.archived_at,
    v.version AS latest_version, v.title, v.created_at AS updated_at
FROM assistant.dashboards d
JOIN LATERAL (
    SELECT version, title, created_at
    FROM assistant.dashboard_versions
    WHERE tenant_id = d.tenant_id AND dashboard_id = d.dashboard_id
    ORDER BY version DESC
    LIMIT 1
) v ON true
WHERE d.tenant_id = $1::uuid AND d.kind = 'saved'
    AND ($2::boolean OR d.archived_at IS NULL)
    AND ($3::timestamptz IS NULL OR (v.created_at, d.dashboard_id) < ($3::timestamptz, $4::uuid))
ORDER BY v.created_at DESC, d.dashboard_id DESC
LIMIT $5";

/// `$1` tenant, `$2` conversation, `$3` and `$4` the cursor, `$5` the page size: the
/// conversation's drafts, the most recently updated first.
const LIST_DRAFTS: &str = "SELECT d.dashboard_id::text AS dashboard_id, d.kind,
    d.conversation_id::text AS conversation_id, d.saved_dashboard_id::text AS saved_dashboard_id,
    d.base_saved_version, d.created_by::text AS created_by, d.created_at, d.archived_at,
    v.version AS latest_version, v.title, v.created_at AS updated_at
FROM assistant.dashboards d
JOIN LATERAL (
    SELECT version, title, created_at
    FROM assistant.dashboard_versions
    WHERE tenant_id = d.tenant_id AND dashboard_id = d.dashboard_id
    ORDER BY version DESC
    LIMIT 1
) v ON true
WHERE d.tenant_id = $1::uuid AND d.kind = 'draft' AND d.conversation_id = $2::uuid
    AND ($3::timestamptz IS NULL OR (v.created_at, d.dashboard_id) < ($3::timestamptz, $4::uuid))
ORDER BY v.created_at DESC, d.dashboard_id DESC
LIMIT $5";

/// `$1` tenant, `$2` dashboard: the number of its latest version (0 when it has none).
const LATEST_VERSION: &str = "SELECT coalesce(max(version), 0) AS latest
FROM assistant.dashboard_versions
WHERE tenant_id = $1::uuid AND dashboard_id = $2::uuid";

/// `$1` tenant, `$2` dashboard, `$3` version: the whole version.
const SELECT_VERSION: &str = "SELECT version, runtime, title, description, spec, page, origin,
    restored_from, created_by::text AS created_by, created_at
FROM assistant.dashboard_versions
WHERE tenant_id = $1::uuid AND dashboard_id = $2::uuid AND version = $3";

/// `$1` tenant, `$2` dashboard, `$3` version: its spec.
const SELECT_SPEC: &str = "SELECT spec
FROM assistant.dashboard_versions
WHERE tenant_id = $1::uuid AND dashboard_id = $2::uuid AND version = $3";

/// `$1` tenant, `$2` dashboard, `$3` the cursor (the last version listed, or NULL), `$4` the
/// page size: versions without their spec and page, the newest first.
const LIST_VERSIONS: &str = "SELECT version, runtime, title, origin, restored_from,
    created_by::text AS created_by, created_at
FROM assistant.dashboard_versions
WHERE tenant_id = $1::uuid AND dashboard_id = $2::uuid
    AND ($3::integer IS NULL OR version < $3::integer)
ORDER BY version DESC
LIMIT $4";

/// `$1` tenant, `$2` conversation, `$3` saved dashboard: the conversation's draft of it.
const DRAFT_OF_SAVED: &str = "SELECT dashboard_id::text AS dashboard_id
FROM assistant.dashboards
WHERE tenant_id = $1::uuid AND conversation_id = $2::uuid AND saved_dashboard_id = $3::uuid
    AND kind = 'draft'";

/// `$1` tenant, `$2` dashboard, `$3` kind, `$4` conversation, `$5` saved dashboard, `$6` its
/// version, `$7` the user.
const INSERT_DASHBOARD: &str = "INSERT INTO assistant.dashboards
    (tenant_id, dashboard_id, kind, conversation_id, saved_dashboard_id, base_saved_version,
     created_by)
VALUES ($1::uuid, $2::uuid, $3, $4::uuid, $5::uuid, $6, $7::uuid)";

/// `$1` tenant, `$2` dashboard, `$3` version, `$4` runtime, `$5` title, `$6` description, `$7`
/// spec, `$8` page, `$9` the user, `$10` the question's run: a version the tool wrote.
const INSERT_AGENT_VERSION: &str = "INSERT INTO assistant.dashboard_versions
    (tenant_id, dashboard_id, version, origin, runtime, title, description, spec, page,
     created_by, run_id)
VALUES ($1::uuid, $2::uuid, $3, 'agent', $4, $5, $6, $7, $8, $9::uuid, $10::uuid)";

/// `$1` tenant, `$2` source dashboard, `$3` source version, `$4` target dashboard, `$5` target
/// version, `$6` origin, `$7` the version restored (or NULL), `$8` the user: a copy of a
/// version, its runtime, title, description, spec and page byte for byte.
const COPY_VERSION: &str = "INSERT INTO assistant.dashboard_versions
    (tenant_id, dashboard_id, version, origin, restored_from, runtime, title, description, spec,
     page, created_by)
SELECT tenant_id, $4::uuid, $5, $6, $7, runtime, title, description, spec, page, $8::uuid
FROM assistant.dashboard_versions
WHERE tenant_id = $1::uuid AND dashboard_id = $2::uuid AND version = $3";

/// `$1` tenant, `$2` draft, `$3` saved dashboard, `$4` its version.
const SET_SAVED_BASE: &str = "UPDATE assistant.dashboards
SET saved_dashboard_id = $3::uuid, base_saved_version = $4
WHERE tenant_id = $1::uuid AND dashboard_id = $2::uuid";

/// `$1` tenant, `$2` dashboard, `$3` archived: the trigger sets the time (the database clock)
/// and keeps the first one when it is archived again.
const SET_ARCHIVED: &str = "UPDATE assistant.dashboards
SET archived_at = CASE WHEN $3::boolean THEN now() END
WHERE tenant_id = $1::uuid AND dashboard_id = $2::uuid
RETURNING archived_at";

/// SQLSTATE `unique_violation`.
const UNIQUE_VIOLATION: &str = "23505";
/// SQLSTATE `foreign_key_violation`.
const FOREIGN_KEY_VIOLATION: &str = "23503";

/// Why an operation failed. Carries no database text and no content.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The dashboard or conversation does not exist in the tenant, or the caller cannot see it.
    #[error("no such dashboard for the caller")]
    NotFound,
    /// The dashboard has no such version.
    #[error("no such version")]
    VersionNotFound,
    /// The write was based on a version that is no longer the latest.
    #[error("the dashboard has a newer version: {latest}")]
    VersionConflict { latest: u32 },
    /// Publishing a draft whose saved dashboard has a version newer than the draft's base.
    #[error("the saved dashboard changed since the draft's base: {latest}")]
    SavedChanged { latest: u32 },
    /// The saved dashboard is archived: no version is added to it, and no draft is taken from it.
    #[error("the dashboard is archived")]
    Archived,
    /// A viewer publishing, or restoring or archiving a saved dashboard.
    #[error("an editor or admin is required")]
    EditorRequired,
    /// Publishing or restoring a draft of a conversation the caller does not own.
    #[error("the conversation's owner is required")]
    OwnerRequired,
    /// The operation needs a draft (publish, a tool's write).
    #[error("a draft is required")]
    DraftRequired,
    /// The operation needs a saved dashboard (archive, taking a draft).
    #[error("a saved dashboard is required")]
    SavedRequired,
    /// The conversation already has a draft of this saved dashboard: write into that one.
    #[error("the conversation already has a draft of this saved dashboard")]
    DraftExists { draft_id: DashboardId },
    /// A draft's new base is not a version of its saved dashboard at or after its current base.
    #[error("the new base is not a version of the draft's saved dashboard after its base")]
    InvalidRebase,
    /// The dashboard has [`MAX_VERSIONS`] versions.
    #[error("the dashboard has the most versions it may have")]
    TooManyVersions,
    /// Another transaction wrote the same row first; the caller may try again.
    #[error("a concurrent write")]
    Concurrent,
    /// The content breaks the dashboard rules (the tool validates before it writes).
    #[error("the content breaks {} rule(s)", .0.len())]
    Invalid(Vec<SpecError>),
    /// The content cannot be stored as it is (U+0000 in text).
    #[error("the content cannot be stored: {0}")]
    Unstorable(&'static str),
    /// The transaction is for another tenant than the caller's.
    #[error("the transaction's tenant is not the caller's")]
    WrongTenant,
    /// A stored row does not read back as a dashboard.
    #[error("a stored dashboard is corrupt: {0}")]
    Corrupt(&'static str),
    /// The database failed. Its text stays here: log [`StoreError::log_kind`] only.
    #[error("database error")]
    Database(#[from] sqlx::Error),
}

impl StoreError {
    /// What to log: the variant, or the SQLSTATE of a database error. Never the database's text.
    #[must_use]
    pub fn log_kind(&self) -> String {
        match self {
            Self::Database(sqlx::Error::Database(database)) => database
                .code()
                .map_or_else(|| "database".to_owned(), |code| format!("SQLSTATE {code}")),
            Self::Database(sqlx::Error::PoolTimedOut) => "pool timed out".to_owned(),
            Self::Database(_) => "database client".to_owned(),
            Self::Corrupt(what) => format!("corrupt: {what}"),
            other => other.to_string(),
        }
    }
}

/// How the caller is related to a dashboard they can see.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    /// A draft, and the caller owns its conversation.
    Owner,
    /// A saved dashboard: every member of the tenant.
    Member,
}

/// A user of the public API: who, and the role the request's identity carries in the tenant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Requester {
    pub principal: Principal,
    pub role: TenantRole,
}

/// The user and question a tool's write belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Author {
    /// The conversation's tenant and owner.
    pub principal: Principal,
    /// The question's run.
    pub run_id: Uuid,
}

/// What the caller may do with a dashboard of `kind` they see with `access`, with `role`: the
/// rule this module enforces and the API shows (`public::Permissions`).
///
/// | | publish | restore | archive |
/// | --- | --- | --- | --- |
/// | a draft, its conversation's owner | editor and up | any role | no |
/// | a saved dashboard | no | editor and up | editor and up |
#[must_use]
pub fn permissions(kind: DashboardKind, access: Access, role: TenantRole) -> Permissions {
    let editor = role.includes(TenantRole::Editor);
    match kind {
        DashboardKind::Draft => Permissions {
            can_publish: editor && access == Access::Owner,
            can_restore: access == Access::Owner,
            can_archive: false,
        },
        DashboardKind::Saved => Permissions {
            can_publish: false,
            can_restore: editor,
            can_archive: editor,
        },
    }
}

/// A version to write: the content the tool built and checked.
#[derive(Clone, Copy, Debug)]
pub struct NewVersion<'a> {
    pub runtime: RuntimeVersion,
    pub title: &'a str,
    pub description: Option<&'a str>,
    pub spec: &'a Spec,
    pub page: &'a str,
}

impl NewVersion<'_> {
    /// The spec as the compact JSON stored, after the rules of
    /// `contract::api::dashboard::spec::validate_version`.
    fn checked(&self) -> Result<String, StoreError> {
        let errors = validate_version(self.title, self.description, self.spec, self.page);
        if !errors.is_empty() {
            return Err(StoreError::Invalid(errors));
        }
        let spec = serde_json::to_string(self.spec)
            .map_err(|_| StoreError::Unstorable("the spec is not JSON"))?;
        // Postgres text cannot hold U+0000 (JSON writes it escaped, so the spec cannot carry it).
        if self.page.contains('\0')
            || self.title.contains('\0')
            || self.description.is_some_and(|text| text.contains('\0'))
        {
            return Err(StoreError::Unstorable("the text contains U+0000"));
        }
        Ok(spec)
    }
}

/// Where a page of dashboards continues: the last one's update time and ID. Opaque to clients.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ListCursor {
    updated_at: DateTime<Utc>,
    dashboard_id: DashboardId,
}

impl ListCursor {
    /// The cursor's text: base64url of the time in microseconds and the ID.
    #[must_use]
    pub fn encode(&self) -> String {
        let mut bytes = Vec::with_capacity(24);
        bytes.extend_from_slice(&self.updated_at.timestamp_micros().to_be_bytes());
        bytes.extend_from_slice(self.dashboard_id.as_uuid().as_bytes());
        URL_SAFE_NO_PAD.encode(bytes)
    }

    /// The cursor `text` encodes; `None` for anything [`ListCursor::encode`] did not write.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        if text.len() != 32 {
            return None;
        }
        let bytes = URL_SAFE_NO_PAD.decode(text).ok()?;
        let (micros, id) = bytes.split_at_checked(8)?;
        let updated_at =
            DateTime::from_timestamp_micros(i64::from_be_bytes(micros.try_into().ok()?))?;
        let dashboard_id = DashboardId::from_uuid(Uuid::from_slice(id).ok()?);
        Some(Self {
            updated_at,
            dashboard_id,
        })
    }
}

/// Where a page of versions continues: the last version listed. Opaque to clients.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VersionCursor(u32);

impl VersionCursor {
    #[must_use]
    pub fn encode(self) -> String {
        self.0.to_string()
    }

    /// `None` unless `text` is a version number as [`VersionCursor::encode`] writes it.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        if text.is_empty() || text.len() > 5 || !text.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let version: u32 = text.parse().ok()?;
        (1..=MAX_VERSIONS)
            .contains(&version)
            .then_some(Self(version))
    }
}

/// A dashboard the caller opened: the dashboard, one version, and how the caller sees it.
#[derive(Clone, Debug, PartialEq)]
pub struct Opened {
    pub dashboard: DashboardSummary,
    pub version: VersionContent,
    pub access: Access,
}

/// A dashboard's row.
struct Stored {
    id: DashboardId,
    kind: DashboardKind,
    conversation_id: Option<Uuid>,
    saved: Option<SavedBase>,
    created_by: UserId,
    created_at: DateTime<Utc>,
    archived_at: Option<DateTime<Utc>>,
}

/// Creates a draft in the author's conversation with `content` as its version 1, written by the
/// author's question. The conversation must be the author's.
///
/// # Errors
///
/// [`StoreError::NotFound`] when the conversation is not the author's (or does not exist);
/// [`StoreError::Invalid`], [`StoreError::Unstorable`]; a database failure.
pub async fn create_draft(
    tx: &mut TenantTransaction,
    author: Author,
    conversation_id: Uuid,
    content: &NewVersion<'_>,
) -> Result<VersionCreated, StoreError> {
    let tenant = author.principal.tenant_id;
    check_tenant(tx, tenant)?;
    let spec = content.checked()?;
    require_owner(tx, author.principal, conversation_id).await?;
    let id = DashboardId::generate();
    insert_dashboard(
        tx,
        tenant,
        id,
        DashboardKind::Draft,
        Some(conversation_id),
        None,
        author.principal.user_id,
    )
    .await?;
    insert_agent_version(tx, tenant, id, 1, author, content, &spec).await?;
    Ok(VersionCreated {
        dashboard_id: id,
        version: 1,
    })
}

/// Adds `content` as a new version of the draft `draft_id` of the author's conversation
/// `conversation_id`, when its latest version is `base_version`. With `rebase`, the draft is
/// based on that version of its saved dashboard from now on: the author's question merged the
/// saved dashboard's newer versions into this content, so publishing it no longer conflicts.
///
/// # Errors
///
/// [`StoreError::NotFound`] when the draft is not one of this conversation's (another
/// conversation's draft included) or the conversation is not the author's;
/// [`StoreError::DraftRequired`] for a saved dashboard (take a draft with [`fork`]);
/// [`StoreError::VersionConflict`] when the draft has a newer version than `base_version`;
/// [`StoreError::InvalidRebase`]; [`StoreError::TooManyVersions`]; [`StoreError::Invalid`],
/// [`StoreError::Unstorable`]; a database failure.
pub async fn write_draft(
    tx: &mut TenantTransaction,
    author: Author,
    conversation_id: Uuid,
    draft_id: DashboardId,
    base_version: u32,
    rebase: Option<u32>,
    content: &NewVersion<'_>,
) -> Result<VersionCreated, StoreError> {
    let tenant = author.principal.tenant_id;
    check_tenant(tx, tenant)?;
    let spec = content.checked()?;
    let row = lock(tx, tenant, draft_id)
        .await?
        .ok_or(StoreError::NotFound)?;
    if row.kind == DashboardKind::Saved {
        return Err(StoreError::DraftRequired);
    }
    if row.conversation_id != Some(conversation_id) {
        return Err(StoreError::NotFound);
    }
    require_owner(tx, author.principal, conversation_id).await?;
    let latest = latest_version(tx, tenant, draft_id).await?;
    if latest != base_version {
        return Err(StoreError::VersionConflict { latest });
    }
    let next = next_version(latest)?;
    if let Some(new_base) = rebase {
        let saved = row.saved.ok_or(StoreError::InvalidRebase)?;
        let saved_latest = latest_version(tx, tenant, saved.dashboard_id).await?;
        if new_base < saved.version || new_base > saved_latest {
            return Err(StoreError::InvalidRebase);
        }
        set_saved_base(tx, tenant, draft_id, saved.dashboard_id, new_base).await?;
    }
    insert_agent_version(tx, tenant, draft_id, next, author, content, &spec).await?;
    Ok(VersionCreated {
        dashboard_id: draft_id,
        version: next,
    })
}

/// Takes a draft of the saved dashboard `saved_id` into the author's conversation, based on its
/// version `base_saved_version` (the one the question read), with `content` as the draft's
/// version 1.
///
/// # Errors
///
/// [`StoreError::NotFound`] when the dashboard or the conversation cannot be seen;
/// [`StoreError::SavedRequired`] for a draft; [`StoreError::Archived`];
/// [`StoreError::VersionNotFound`] when the saved dashboard has no such version;
/// [`StoreError::DraftExists`] when the conversation already has a draft of it;
/// [`StoreError::Concurrent`] when another write took one at the same time;
/// [`StoreError::Invalid`], [`StoreError::Unstorable`]; a database failure.
pub async fn fork(
    tx: &mut TenantTransaction,
    author: Author,
    conversation_id: Uuid,
    saved_id: DashboardId,
    base_saved_version: u32,
    content: &NewVersion<'_>,
) -> Result<VersionCreated, StoreError> {
    let tenant = author.principal.tenant_id;
    check_tenant(tx, tenant)?;
    let spec = content.checked()?;
    require_owner(tx, author.principal, conversation_id).await?;
    let saved = lock(tx, tenant, saved_id)
        .await?
        .ok_or(StoreError::NotFound)?;
    visible(tx, author.principal, &saved).await?;
    if saved.kind != DashboardKind::Saved {
        return Err(StoreError::SavedRequired);
    }
    if saved.archived_at.is_some() {
        return Err(StoreError::Archived);
    }
    let saved_latest = latest_version(tx, tenant, saved_id).await?;
    if base_saved_version == 0 || base_saved_version > saved_latest {
        return Err(StoreError::VersionNotFound);
    }
    if let Some(row) = sqlx::query(DRAFT_OF_SAVED)
        .bind(tenant.to_string())
        .bind(conversation_id.to_string())
        .bind(saved_id.to_string())
        .fetch_optional(&mut **tx)
        .await?
    {
        return Err(StoreError::DraftExists {
            draft_id: DashboardId::from_uuid(uuid(&row, "dashboard_id")?),
        });
    }
    let id = DashboardId::generate();
    insert_dashboard(
        tx,
        tenant,
        id,
        DashboardKind::Draft,
        Some(conversation_id),
        Some(SavedBase {
            dashboard_id: saved_id,
            version: base_saved_version,
        }),
        author.principal.user_id,
    )
    .await?;
    insert_agent_version(tx, tenant, id, 1, author, content, &spec).await?;
    Ok(VersionCreated {
        dashboard_id: id,
        version: 1,
    })
}

/// Publishes the draft's `version`: the first time as version 1 of a new saved dashboard, which
/// the requester then made, later as the next version of the saved dashboard the draft is based
/// on. The draft is then based on the version written.
///
/// Checked in this order: the draft can be seen ([`StoreError::NotFound`]); the requester is an
/// editor or admin ([`StoreError::EditorRequired`]); it is a draft
/// ([`StoreError::DraftRequired`]); the requester owns its conversation
/// ([`StoreError::OwnerRequired`]); the version exists ([`StoreError::VersionNotFound`]); the
/// saved dashboard is not archived ([`StoreError::Archived`]) and has no version newer than the
/// draft's base ([`StoreError::SavedChanged`]).
///
/// # Errors
///
/// As above; [`StoreError::TooManyVersions`]; a database failure.
pub async fn publish(
    tx: &mut TenantTransaction,
    requester: Requester,
    draft_id: DashboardId,
    version: u32,
) -> Result<VersionCreated, StoreError> {
    let tenant = requester.principal.tenant_id;
    check_tenant(tx, tenant)?;
    let draft = lock(tx, tenant, draft_id)
        .await?
        .ok_or(StoreError::NotFound)?;
    let access = visible(tx, requester.principal, &draft).await?;
    if !requester.role.includes(TenantRole::Editor) {
        return Err(StoreError::EditorRequired);
    }
    if draft.kind != DashboardKind::Draft {
        return Err(StoreError::DraftRequired);
    }
    if !permissions(draft.kind, access, requester.role).can_publish {
        return Err(StoreError::OwnerRequired);
    }
    let draft_latest = latest_version(tx, tenant, draft_id).await?;
    if version == 0 || version > draft_latest {
        return Err(StoreError::VersionNotFound);
    }
    let user = requester.principal.user_id;
    let (saved_id, next) = match draft.saved {
        None => {
            let saved_id = DashboardId::generate();
            insert_dashboard(tx, tenant, saved_id, DashboardKind::Saved, None, None, user).await?;
            (saved_id, 1)
        }
        Some(base) => {
            let saved = lock(tx, tenant, base.dashboard_id)
                .await?
                .ok_or(StoreError::Corrupt("a draft's saved dashboard is missing"))?;
            if saved.archived_at.is_some() {
                return Err(StoreError::Archived);
            }
            let saved_latest = latest_version(tx, tenant, base.dashboard_id).await?;
            if saved_latest != base.version {
                return Err(StoreError::SavedChanged {
                    latest: saved_latest,
                });
            }
            (base.dashboard_id, next_version(saved_latest)?)
        }
    };
    copy_version(
        tx,
        tenant,
        (draft_id, version),
        (saved_id, next),
        VersionOrigin::Publish,
        user,
    )
    .await?;
    set_saved_base(tx, tenant, draft_id, saved_id, next).await?;
    Ok(VersionCreated {
        dashboard_id: saved_id,
        version: next,
    })
}

/// Adds a copy of `from_version` as the dashboard's next version, when its latest version is
/// `base_version`.
///
/// Checked in this order: the dashboard can be seen ([`StoreError::NotFound`]); for a saved
/// dashboard the requester is an editor or admin ([`StoreError::EditorRequired`]), for a draft
/// the requester owns its conversation ([`StoreError::OwnerRequired`]); `from_version` exists
/// ([`StoreError::VersionNotFound`]); the dashboard is not archived ([`StoreError::Archived`]);
/// `base_version` is the latest ([`StoreError::VersionConflict`]).
///
/// # Errors
///
/// As above; [`StoreError::TooManyVersions`]; a database failure.
pub async fn restore(
    tx: &mut TenantTransaction,
    requester: Requester,
    dashboard_id: DashboardId,
    from_version: u32,
    base_version: u32,
) -> Result<VersionCreated, StoreError> {
    let tenant = requester.principal.tenant_id;
    check_tenant(tx, tenant)?;
    let row = lock(tx, tenant, dashboard_id)
        .await?
        .ok_or(StoreError::NotFound)?;
    let access = visible(tx, requester.principal, &row).await?;
    if !permissions(row.kind, access, requester.role).can_restore {
        return Err(match row.kind {
            DashboardKind::Saved => StoreError::EditorRequired,
            DashboardKind::Draft => StoreError::OwnerRequired,
        });
    }
    let latest = latest_version(tx, tenant, dashboard_id).await?;
    if from_version == 0 || from_version > latest {
        return Err(StoreError::VersionNotFound);
    }
    if row.archived_at.is_some() {
        return Err(StoreError::Archived);
    }
    if base_version != latest {
        return Err(StoreError::VersionConflict { latest });
    }
    let next = next_version(latest)?;
    copy_version(
        tx,
        tenant,
        (dashboard_id, from_version),
        (dashboard_id, next),
        VersionOrigin::Restore,
        requester.principal.user_id,
    )
    .await?;
    Ok(VersionCreated {
        dashboard_id,
        version: next,
    })
}

/// Archives (`archived` true) or unarchives a saved dashboard. Archiving an archived one keeps
/// its archive time; unarchiving one that is not archived changes nothing.
///
/// Checked in this order: the dashboard can be seen ([`StoreError::NotFound`]); the requester
/// is an editor or admin ([`StoreError::EditorRequired`]); it is a saved dashboard
/// ([`StoreError::SavedRequired`]).
///
/// # Errors
///
/// As above; a database failure.
pub async fn set_archived(
    tx: &mut TenantTransaction,
    requester: Requester,
    dashboard_id: DashboardId,
    archived: bool,
) -> Result<ArchiveState, StoreError> {
    let tenant = requester.principal.tenant_id;
    check_tenant(tx, tenant)?;
    let row = lock(tx, tenant, dashboard_id)
        .await?
        .ok_or(StoreError::NotFound)?;
    let access = visible(tx, requester.principal, &row).await?;
    if !requester.role.includes(TenantRole::Editor) {
        return Err(StoreError::EditorRequired);
    }
    if !permissions(row.kind, access, requester.role).can_archive {
        return Err(StoreError::SavedRequired);
    }
    let archived_at: Option<DateTime<Utc>> = sqlx::query(SET_ARCHIVED)
        .bind(tenant.to_string())
        .bind(dashboard_id.to_string())
        .bind(archived)
        .fetch_one(&mut **tx)
        .await?
        .try_get("archived_at")?;
    Ok(ArchiveState {
        dashboard_id,
        archived_at,
    })
}

/// The dashboard with its `version` (the latest without one), as `reader` sees it.
///
/// # Errors
///
/// [`StoreError::NotFound`] when the reader cannot see it; [`StoreError::VersionNotFound`];
/// [`StoreError::Corrupt`]; a database failure.
pub async fn open(
    tx: &mut TenantTransaction,
    reader: Principal,
    dashboard_id: DashboardId,
    version: Option<u32>,
) -> Result<Opened, StoreError> {
    check_tenant(tx, reader.tenant_id)?;
    let row = sqlx::query(SELECT_DASHBOARD)
        .bind(reader.tenant_id.to_string())
        .bind(dashboard_id.to_string())
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(StoreError::NotFound)?;
    let dashboard = summary(&row)?;
    let access = visible_summary(tx, reader, &dashboard).await?;
    let number = version.unwrap_or(dashboard.latest_version);
    let version = select_version(tx, reader.tenant_id, dashboard_id, number)
        .await?
        .ok_or(StoreError::VersionNotFound)?;
    Ok(Opened {
        dashboard,
        version,
        access,
    })
}

/// The dataset `name` of the dashboard's `version`, as `reader` sees the dashboard: what a run
/// sends to telemetry-query.
///
/// # Errors
///
/// [`StoreError::NotFound`] when the reader cannot see the dashboard;
/// [`StoreError::VersionNotFound`]; `Ok(None)` when the version has no such dataset;
/// [`StoreError::Corrupt`]; a database failure.
pub async fn dataset(
    tx: &mut TenantTransaction,
    reader: Principal,
    dashboard_id: DashboardId,
    version: u32,
    name: &str,
) -> Result<Option<Dataset>, StoreError> {
    check_tenant(tx, reader.tenant_id)?;
    let row = sqlx::query(SELECT_DASHBOARD)
        .bind(reader.tenant_id.to_string())
        .bind(dashboard_id.to_string())
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(StoreError::NotFound)?;
    let dashboard = summary(&row)?;
    visible_summary(tx, reader, &dashboard).await?;
    let version = i32::try_from(version).map_err(|_| StoreError::VersionNotFound)?;
    let spec: String = sqlx::query(SELECT_SPEC)
        .bind(reader.tenant_id.to_string())
        .bind(dashboard_id.to_string())
        .bind(version)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(StoreError::VersionNotFound)?
        .try_get("spec")?;
    let spec = parse_spec(&spec)?;
    Ok(spec.dataset(name).cloned())
}

/// The dashboard's versions, the newest first, after `cursor`.
///
/// # Errors
///
/// [`StoreError::NotFound`] when the reader cannot see the dashboard; [`StoreError::Corrupt`];
/// a database failure.
pub async fn versions(
    tx: &mut TenantTransaction,
    reader: Principal,
    dashboard_id: DashboardId,
    cursor: Option<VersionCursor>,
) -> Result<VersionList, StoreError> {
    check_tenant(tx, reader.tenant_id)?;
    let row = sqlx::query(SELECT_DASHBOARD)
        .bind(reader.tenant_id.to_string())
        .bind(dashboard_id.to_string())
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(StoreError::NotFound)?;
    let dashboard = summary(&row)?;
    visible_summary(tx, reader, &dashboard).await?;
    let rows = sqlx::query(LIST_VERSIONS)
        .bind(reader.tenant_id.to_string())
        .bind(dashboard_id.to_string())
        .bind(cursor.map(|cursor| i64::from(cursor.0)))
        .bind(i64::from(PAGE_SIZE) + 1)
        .fetch_all(&mut **tx)
        .await?;
    let mut versions = rows
        .iter()
        .map(version_summary)
        .collect::<Result<Vec<_>, _>>()?;
    let next_cursor = page(&mut versions)
        .then(|| {
            versions
                .last()
                .map(|last| VersionCursor(last.version).encode())
        })
        .flatten();
    Ok(VersionList {
        versions,
        next_cursor,
    })
}

/// The tenant's saved dashboards (archived ones too with `include_archived`), the most recently
/// updated first, after `cursor`. Every member of the tenant sees them.
///
/// # Errors
///
/// [`StoreError::Corrupt`]; a database failure.
pub async fn list_saved(
    tx: &mut TenantTransaction,
    reader: Principal,
    include_archived: bool,
    cursor: Option<ListCursor>,
) -> Result<DashboardList, StoreError> {
    check_tenant(tx, reader.tenant_id)?;
    let rows = sqlx::query(LIST_SAVED)
        .bind(reader.tenant_id.to_string())
        .bind(include_archived)
        .bind(cursor.map(|cursor| cursor.updated_at))
        .bind(cursor.map(|cursor| cursor.dashboard_id.to_string()))
        .bind(i64::from(PAGE_SIZE) + 1)
        .fetch_all(&mut **tx)
        .await?;
    dashboard_page(&rows)
}

/// The drafts of the conversation, the most recently updated first, after `cursor`, when the
/// reader can see the conversation's drafts.
///
/// # Errors
///
/// [`StoreError::NotFound`] when the reader cannot see them (the conversation is another
/// user's, or does not exist); [`StoreError::Corrupt`]; a database failure.
pub async fn list_drafts(
    tx: &mut TenantTransaction,
    reader: Principal,
    conversation_id: Uuid,
    cursor: Option<ListCursor>,
) -> Result<DashboardList, StoreError> {
    check_tenant(tx, reader.tenant_id)?;
    conversation_access(tx, reader, conversation_id)
        .await?
        .ok_or(StoreError::NotFound)?;
    let rows = sqlx::query(LIST_DRAFTS)
        .bind(reader.tenant_id.to_string())
        .bind(conversation_id.to_string())
        .bind(cursor.map(|cursor| cursor.updated_at))
        .bind(cursor.map(|cursor| cursor.dashboard_id.to_string()))
        .bind(i64::from(PAGE_SIZE) + 1)
        .fetch_all(&mut **tx)
        .await?;
    dashboard_page(&rows)
}

/// How `reader` may see the drafts of `conversation_id`: [`Access::Owner`] for its owner,
/// `None` for anyone else and for a conversation that does not exist. The one place that
/// decides who sees a draft.
///
/// # Errors
///
/// A database failure; [`StoreError::Corrupt`].
pub async fn conversation_access(
    tx: &mut TenantTransaction,
    reader: Principal,
    conversation_id: Uuid,
) -> Result<Option<Access>, StoreError> {
    let Some(row) = sqlx::query(CONVERSATION_OWNER)
        .bind(reader.tenant_id.to_string())
        .bind(conversation_id.to_string())
        .fetch_optional(&mut **tx)
        .await?
    else {
        return Ok(None);
    };
    let owner = UserId::from_uuid(uuid(&row, "user_id")?);
    Ok((owner == reader.user_id).then_some(Access::Owner))
}

/// How `reader` sees the dashboard `row`; [`StoreError::NotFound`] when they cannot.
async fn visible(
    tx: &mut TenantTransaction,
    reader: Principal,
    row: &Stored,
) -> Result<Access, StoreError> {
    access(tx, reader, row.kind, row.conversation_id).await
}

async fn visible_summary(
    tx: &mut TenantTransaction,
    reader: Principal,
    dashboard: &DashboardSummary,
) -> Result<Access, StoreError> {
    access(tx, reader, dashboard.kind, dashboard.conversation_id).await
}

async fn access(
    tx: &mut TenantTransaction,
    reader: Principal,
    kind: DashboardKind,
    conversation_id: Option<Uuid>,
) -> Result<Access, StoreError> {
    match kind {
        DashboardKind::Saved => Ok(Access::Member),
        DashboardKind::Draft => {
            let conversation =
                conversation_id.ok_or(StoreError::Corrupt("a draft without a conversation"))?;
            conversation_access(tx, reader, conversation)
                .await?
                .ok_or(StoreError::NotFound)
        }
    }
}

/// The conversation must be `principal`'s: the tool writes only drafts of its own conversation.
async fn require_owner(
    tx: &mut TenantTransaction,
    principal: Principal,
    conversation_id: Uuid,
) -> Result<(), StoreError> {
    match conversation_access(tx, principal, conversation_id).await? {
        Some(Access::Owner) => Ok(()),
        _ => Err(StoreError::NotFound),
    }
}

fn check_tenant(tx: &TenantTransaction, tenant: TenantId) -> Result<(), StoreError> {
    if tx.tenant_id() == tenant {
        Ok(())
    } else {
        Err(StoreError::WrongTenant)
    }
}

/// The version after `latest`, within [`MAX_VERSIONS`].
fn next_version(latest: u32) -> Result<u32, StoreError> {
    if latest >= MAX_VERSIONS {
        Err(StoreError::TooManyVersions)
    } else {
        Ok(latest + 1)
    }
}

async fn lock(
    tx: &mut TenantTransaction,
    tenant: TenantId,
    dashboard_id: DashboardId,
) -> Result<Option<Stored>, StoreError> {
    let row = sqlx::query(LOCK_DASHBOARD)
        .bind(tenant.to_string())
        .bind(dashboard_id.to_string())
        .fetch_optional(&mut **tx)
        .await?;
    row.as_ref().map(stored).transpose()
}

async fn latest_version(
    tx: &mut TenantTransaction,
    tenant: TenantId,
    dashboard_id: DashboardId,
) -> Result<u32, StoreError> {
    let latest: i32 = sqlx::query(LATEST_VERSION)
        .bind(tenant.to_string())
        .bind(dashboard_id.to_string())
        .fetch_one(&mut **tx)
        .await?
        .try_get("latest")?;
    u32::try_from(latest).map_err(|_| StoreError::Corrupt("a negative version"))
}

async fn insert_dashboard(
    tx: &mut TenantTransaction,
    tenant: TenantId,
    id: DashboardId,
    kind: DashboardKind,
    conversation_id: Option<Uuid>,
    saved: Option<SavedBase>,
    created_by: UserId,
) -> Result<(), StoreError> {
    sqlx::query(INSERT_DASHBOARD)
        .bind(tenant.to_string())
        .bind(id.to_string())
        .bind(kind_name(kind))
        .bind(conversation_id.map(|id| id.to_string()))
        .bind(saved.map(|saved| saved.dashboard_id.to_string()))
        .bind(
            saved
                .map(|saved| version_column(saved.version))
                .transpose()?,
        )
        .bind(created_by.to_string())
        .execute(&mut **tx)
        .await
        .map_err(|error| match sqlstate(&error).as_deref() {
            Some(UNIQUE_VIOLATION) => StoreError::Concurrent,
            // The conversation was purged since it was read.
            Some(FOREIGN_KEY_VIOLATION) => StoreError::NotFound,
            _ => StoreError::Database(error),
        })?;
    Ok(())
}

async fn insert_agent_version(
    tx: &mut TenantTransaction,
    tenant: TenantId,
    id: DashboardId,
    version: u32,
    author: Author,
    content: &NewVersion<'_>,
    spec: &str,
) -> Result<(), StoreError> {
    sqlx::query(INSERT_AGENT_VERSION)
        .bind(tenant.to_string())
        .bind(id.to_string())
        .bind(version_column(version)?)
        .bind(
            i16::try_from(content.runtime.get())
                .map_err(|_| StoreError::Unstorable("the runtime"))?,
        )
        .bind(content.title)
        .bind(content.description)
        .bind(spec)
        .bind(content.page)
        .bind(author.principal.user_id.to_string())
        .bind(author.run_id.to_string())
        .execute(&mut **tx)
        .await
        .map_err(version_insert_error)?;
    Ok(())
}

async fn copy_version(
    tx: &mut TenantTransaction,
    tenant: TenantId,
    (source, source_version): (DashboardId, u32),
    (target, target_version): (DashboardId, u32),
    origin: VersionOrigin,
    created_by: UserId,
) -> Result<(), StoreError> {
    let restored_from = (origin == VersionOrigin::Restore)
        .then(|| version_column(source_version))
        .transpose()?;
    let copied = sqlx::query(COPY_VERSION)
        .bind(tenant.to_string())
        .bind(source.to_string())
        .bind(version_column(source_version)?)
        .bind(target.to_string())
        .bind(version_column(target_version)?)
        .bind(origin_name(origin))
        .bind(restored_from)
        .bind(created_by.to_string())
        .execute(&mut **tx)
        .await
        .map_err(version_insert_error)?;
    if copied.rows_affected() == 1 {
        Ok(())
    } else {
        Err(StoreError::VersionNotFound)
    }
}

async fn set_saved_base(
    tx: &mut TenantTransaction,
    tenant: TenantId,
    draft_id: DashboardId,
    saved_id: DashboardId,
    version: u32,
) -> Result<(), StoreError> {
    sqlx::query(SET_SAVED_BASE)
        .bind(tenant.to_string())
        .bind(draft_id.to_string())
        .bind(saved_id.to_string())
        .bind(version_column(version)?)
        .execute(&mut **tx)
        .await
        .map_err(|error| match sqlstate(&error).as_deref() {
            // Another draft of the conversation took the same saved dashboard meanwhile.
            Some(UNIQUE_VIOLATION) => StoreError::Concurrent,
            _ => StoreError::Database(error),
        })?;
    Ok(())
}

async fn select_version(
    tx: &mut TenantTransaction,
    tenant: TenantId,
    dashboard_id: DashboardId,
    version: u32,
) -> Result<Option<VersionContent>, StoreError> {
    let Ok(version) = i32::try_from(version) else {
        return Ok(None);
    };
    let row = sqlx::query(SELECT_VERSION)
        .bind(tenant.to_string())
        .bind(dashboard_id.to_string())
        .bind(version)
        .fetch_optional(&mut **tx)
        .await?;
    row.as_ref().map(version_content).transpose()
}

/// A version written at the same position by another transaction is a conflict; a database
/// check that refuses the content is content that cannot be stored.
fn version_insert_error(error: sqlx::Error) -> StoreError {
    match sqlstate(&error).as_deref() {
        Some(UNIQUE_VIOLATION) => StoreError::Concurrent,
        Some("23514") => StoreError::Unstorable("a check of the version table"),
        _ => StoreError::Database(error),
    }
}

fn sqlstate(error: &sqlx::Error) -> Option<String> {
    error
        .as_database_error()
        .and_then(|database| database.code())
        .map(|code| code.into_owned())
}

/// The first `PAGE_SIZE` of `items`, which holds up to one more; whether there was more.
fn page<T>(items: &mut Vec<T>) -> bool {
    let limit = usize::try_from(PAGE_SIZE).unwrap_or(usize::MAX);
    let more = items.len() > limit;
    items.truncate(limit);
    more
}

fn dashboard_page(rows: &[PgRow]) -> Result<DashboardList, StoreError> {
    let mut dashboards = rows.iter().map(summary).collect::<Result<Vec<_>, _>>()?;
    let next_cursor = page(&mut dashboards)
        .then(|| {
            dashboards.last().map(|last| {
                ListCursor {
                    updated_at: last.updated_at,
                    dashboard_id: last.dashboard_id,
                }
                .encode()
            })
        })
        .flatten();
    Ok(DashboardList {
        dashboards,
        next_cursor,
    })
}

fn stored(row: &PgRow) -> Result<Stored, StoreError> {
    Ok(Stored {
        id: DashboardId::from_uuid(uuid(row, "dashboard_id")?),
        kind: kind_of(&row.try_get::<String, _>("kind")?)?,
        conversation_id: optional_uuid(row, "conversation_id")?,
        saved: saved_base(row)?,
        created_by: UserId::from_uuid(uuid(row, "created_by")?),
        created_at: row.try_get("created_at")?,
        archived_at: row.try_get("archived_at")?,
    })
}

fn summary(row: &PgRow) -> Result<DashboardSummary, StoreError> {
    let stored = stored(row)?;
    Ok(DashboardSummary {
        dashboard_id: stored.id,
        kind: stored.kind,
        title: row.try_get("title")?,
        latest_version: version_number(row.try_get("latest_version")?)?,
        created_by: stored.created_by,
        created_at: stored.created_at,
        updated_at: row.try_get("updated_at")?,
        archived_at: stored.archived_at,
        conversation_id: stored.conversation_id,
        saved: stored.saved,
    })
}

fn saved_base(row: &PgRow) -> Result<Option<SavedBase>, StoreError> {
    let id = optional_uuid(row, "saved_dashboard_id")?;
    let version: Option<i32> = row.try_get("base_saved_version")?;
    match (id, version) {
        (Some(id), Some(version)) => Ok(Some(SavedBase {
            dashboard_id: DashboardId::from_uuid(id),
            version: version_number(version)?,
        })),
        (None, None) => Ok(None),
        _ => Err(StoreError::Corrupt(
            "a draft's saved dashboard without its version",
        )),
    }
}

fn version_content(row: &PgRow) -> Result<VersionContent, StoreError> {
    Ok(VersionContent {
        version: version_number(row.try_get("version")?)?,
        runtime: runtime(row.try_get("runtime")?)?,
        title: row.try_get("title")?,
        description: row.try_get("description")?,
        spec: parse_spec(&row.try_get::<String, _>("spec")?)?,
        page: row.try_get("page")?,
        origin: origin_of(&row.try_get::<String, _>("origin")?)?,
        restored_from: row
            .try_get::<Option<i32>, _>("restored_from")?
            .map(version_number)
            .transpose()?,
        created_by: UserId::from_uuid(uuid(row, "created_by")?),
        created_at: row.try_get("created_at")?,
    })
}

fn version_summary(row: &PgRow) -> Result<VersionSummary, StoreError> {
    Ok(VersionSummary {
        version: version_number(row.try_get("version")?)?,
        runtime: runtime(row.try_get("runtime")?)?,
        title: row.try_get("title")?,
        origin: origin_of(&row.try_get::<String, _>("origin")?)?,
        restored_from: row
            .try_get::<Option<i32>, _>("restored_from")?
            .map(version_number)
            .transpose()?,
        created_by: UserId::from_uuid(uuid(row, "created_by")?),
        created_at: row.try_get("created_at")?,
    })
}

fn parse_spec(text: &str) -> Result<Spec, StoreError> {
    serde_json::from_str(text).map_err(|_| StoreError::Corrupt("a version's spec"))
}

fn runtime(number: i16) -> Result<RuntimeVersion, StoreError> {
    u16::try_from(number)
        .ok()
        .and_then(RuntimeVersion::shipped)
        .ok_or(StoreError::Corrupt("a runtime this build does not ship"))
}

fn version_number(version: i32) -> Result<u32, StoreError> {
    u32::try_from(version).map_err(|_| StoreError::Corrupt("a negative version"))
}

fn version_column(version: u32) -> Result<i32, StoreError> {
    i32::try_from(version).map_err(|_| StoreError::TooManyVersions)
}

fn uuid(row: &PgRow, column: &str) -> Result<Uuid, StoreError> {
    Uuid::try_parse(&row.try_get::<String, _>(column)?).map_err(|_| StoreError::Corrupt("an ID"))
}

fn optional_uuid(row: &PgRow, column: &str) -> Result<Option<Uuid>, StoreError> {
    row.try_get::<Option<String>, _>(column)?
        .map(|text| Uuid::try_parse(&text).map_err(|_| StoreError::Corrupt("an ID")))
        .transpose()
}

const fn kind_name(kind: DashboardKind) -> &'static str {
    match kind {
        DashboardKind::Draft => "draft",
        DashboardKind::Saved => "saved",
    }
}

fn kind_of(name: &str) -> Result<DashboardKind, StoreError> {
    match name {
        "draft" => Ok(DashboardKind::Draft),
        "saved" => Ok(DashboardKind::Saved),
        _ => Err(StoreError::Corrupt("a dashboard's kind")),
    }
}

const fn origin_name(origin: VersionOrigin) -> &'static str {
    match origin {
        VersionOrigin::Agent => "agent",
        VersionOrigin::Publish => "publish",
        VersionOrigin::Restore => "restore",
    }
}

fn origin_of(name: &str) -> Result<VersionOrigin, StoreError> {
    match name {
        "agent" => Ok(VersionOrigin::Agent),
        "publish" => Ok(VersionOrigin::Publish),
        "restore" => Ok(VersionOrigin::Restore),
        _ => Err(StoreError::Corrupt("a version's origin")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permissions_follow_the_owners_table() {
        use DashboardKind::{Draft, Saved};
        use TenantRole::{Admin, Editor, Viewer};
        let allowed = |kind, access, role| {
            let p = permissions(kind, access, role);
            (p.can_publish, p.can_restore, p.can_archive)
        };
        // A draft: its owner restores with any role and publishes as an editor or admin.
        assert_eq!(allowed(Draft, Access::Owner, Viewer), (false, true, false));
        assert_eq!(allowed(Draft, Access::Owner, Editor), (true, true, false));
        assert_eq!(allowed(Draft, Access::Owner, Admin), (true, true, false));
        // A saved dashboard: editors and admins restore and archive; nobody publishes it.
        assert_eq!(
            allowed(Saved, Access::Member, Viewer),
            (false, false, false)
        );
        assert_eq!(allowed(Saved, Access::Member, Editor), (false, true, true));
        assert_eq!(allowed(Saved, Access::Member, Admin), (false, true, true));
        // A role this build does not know grants nothing beyond a viewer's.
        assert_eq!(
            allowed(Saved, Access::Member, TenantRole::Unrecognized),
            (false, false, false)
        );
        // A draft seen without owning its conversation (shared readers, Laysi/trustiot-v2#650)
        // allows nothing.
        assert_eq!(allowed(Draft, Access::Member, Admin), (false, false, false));
    }

    #[test]
    fn cursors_read_back_and_refuse_anything_else() {
        let cursor = ListCursor {
            updated_at: DateTime::from_timestamp_micros(1_793_000_000_123_456).expect("a time"),
            dashboard_id: DashboardId::generate(),
        };
        let text = cursor.encode();
        assert_eq!(text.len(), 32);
        assert_eq!(ListCursor::parse(&text), Some(cursor));
        let shorter = text.get(1..).expect("ASCII");
        let longer = format!("{text}A");
        let wrong = "!".repeat(32);
        for bad in ["", "abc", longer.as_str(), shorter, wrong.as_str()] {
            assert_eq!(ListCursor::parse(bad), None, "{bad}");
        }
        assert_eq!(VersionCursor::parse("17"), Some(VersionCursor(17)));
        assert_eq!(VersionCursor(17).encode(), "17");
        for bad in ["", "0", "10001", "-1", "+5", "1.0", "007x", "999999"] {
            assert_eq!(VersionCursor::parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn a_full_page_tells_there_is_more() {
        let limit = usize::try_from(PAGE_SIZE).expect("fits");
        let mut items: Vec<usize> = (0..=limit).collect();
        assert!(page(&mut items));
        assert_eq!(items.len(), limit);
        let mut items: Vec<usize> = (0..limit).collect();
        assert!(!page(&mut items));
        assert_eq!(items.len(), limit);
    }
}
