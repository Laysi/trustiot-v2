//! The assistant service's Postgres schema as this release expects it: its migrations/postgres
//! files in apply order (Laysi/trustiot-v2#553). No outbox: the assistant writes no events yet.
//!
//! Two crates compile this file: the service, whose readiness check
//! ([`service_kit::migrations::MigrationCheck`], registered in `start`) fails until the database
//! has every file (Laysi/trustiot-v2#592), and
//! the migration Job (`tools/migrate/src/registry.rs` includes it with `#[path]`), which applies
//! exactly these files. One list, so the two cannot disagree. Keep the file self-contained: it
//! may use `service_kit` only. A new file is a new entry at the end; applied files never change
//! (`tools/migrate/tests/registry.rs` compares the list with the directory).

use service_kit::migrations::{Migration, Schema};

/// What the assistant service expects of its schema.
pub const SCHEMA: Schema = Schema {
    service: "assistant",
    outbox_producer: false,
    migrations: &[
        Migration {
            name: "0001_assistant.sql",
            sql: include_str!("../migrations/postgres/0001_assistant.sql"),
        },
        // Conversation threads: titles, archiving, soft deletion and the purge's platform role,
        // discarded rounds, the record of questions (Laysi/trustiot-v2#644).
        Migration {
            name: "0002_conversation_threads.sql",
            sql: include_str!("../migrations/postgres/0002_conversation_threads.sql"),
        },
        // The dashboards and their versions (Laysi/trustiot-v2#150). Numbered after the
        // conversation threads, which reached main first: before #150 merged, this file was
        // `0002_dashboards.sql` on its branch only, which no release included, so no database a
        // release migrated has a record of that name. A local database built from the branch
        // stops at the migration Job (the tables exist) and is rebuilt.
        Migration {
            name: "0003_dashboards.sql",
            sql: include_str!("../migrations/postgres/0003_dashboards.sql"),
        },
        // A version's custom page (and its runtime) becomes optional: versions of components
        // only (Laysi/trustiot-v2#934).
        Migration {
            name: "0004_dashboard_pages_optional.sql",
            sql: include_str!("../migrations/postgres/0004_dashboard_pages_optional.sql"),
        },
        // Sharing a conversation with the tenant: `shared_at`, its clock and the deleted-never-
        // shared check, the index of the "shared with me" list (Laysi/trustiot-v2#650). Numbered
        // after the optional pages, which reached main first: before #650 merged, this file was
        // `0004_conversation_sharing.sql` on its branch only, which no release included, so no
        // database a release migrated has a record of that name. A local database built from the
        // branch stops at the migration Job (the column exists) and is rebuilt.
        Migration {
            name: "0005_conversation_sharing.sql",
            sql: include_str!("../migrations/postgres/0005_conversation_sharing.sql"),
        },
        // What the answer guard removed from an answer, stored with the answer's message, and
        // the index of a conversation's latest question (Laysi/trustiot-v2#646). Numbered after
        // conversation sharing (0005, Laysi/trustiot-v2#650), which reaches main first. Before
        // #646 merged, this file was `0004_answer_removed_content.sql`, then
        // `0005_answer_removed_content.sql`, on its branch only, which no release included.
        Migration {
            name: "0006_answer_removed_content.sql",
            sql: include_str!("../migrations/postgres/0006_answer_removed_content.sql"),
        },
        // The position of an answered run's answer, so a conversation's answers list the draft
        // versions their turn wrote (Laysi/trustiot-v2#1043).
        Migration {
            name: "0007_answer_drafts.sql",
            sql: include_str!("../migrations/postgres/0007_answer_drafts.sql"),
        },
    ],
};
