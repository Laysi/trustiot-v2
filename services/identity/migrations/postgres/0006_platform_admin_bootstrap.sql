-- The first platform administrator (Laysi/trustiot-v2#582; checklist A2). The migration Job's
-- `bootstrap-platform-admin` command (tools/migrate/README.md) makes one user a platform
-- administrator, once per deployment: it writes this table's only row in the same transaction,
-- and finds the row there on every later run. No HTTP route creates a platform administrator.
--
-- Once ever, not "while there is none": the row stays when that user loses the role or is
-- deleted (no foreign key to identity.users), so the command never works again by accident.
-- Running it again is a deliberate recovery step: a database administrator deletes the row
-- (docs/runbooks/platform-admin.md).
--
-- Run by `identity_owner` like 0001_identity.sql. Neither the service role nor the platform
-- role gets any privilege here: only the table's owner (the migration Job) reads or writes it.
CREATE TABLE identity.platform_admin_bootstrap (
    -- At most one row: the primary key can only be true. A second insert, also one racing the
    -- first in another transaction, fails on the key.
    singleton boolean PRIMARY KEY DEFAULT true CHECK (singleton),
    -- The user the command made a platform administrator; kept after that user is gone.
    user_id uuid NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now()
);
