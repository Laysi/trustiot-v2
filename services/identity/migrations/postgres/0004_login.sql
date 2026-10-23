-- Email verification-code login (Laysi/trustiot-v2#103; services/identity/README.md, "登入"):
-- users' email addresses, the login codes, and the writes on sessions that logging in, logging
-- out and extending a session need. Run by `identity_owner` like 0001_identity.sql.
--
-- The service role gets exactly those writes. On sessions it may create one, move its expiry
-- and revoke it; it still cannot delete a session, hand it to another user, change when it
-- started or which tenant it acts in. The trigger at the end keeps two rules that column grants
-- cannot express: a revoked session stays revoked, and no session lives longer than 7 days from
-- its creation (the absolute lifetime, `SESSION_ABSOLUTE_LIFETIME` in
-- services/identity/src/session.rs, whose test compares the two). Both catch a bug in the
-- service, not a compromised one: a role that may create sessions can always create a new one.

-- Fail fast, not queue every ext_authz read behind an ALTER's lock; the Job fails and retries.
SET LOCAL lock_timeout = '5s';

-- The address login codes go to: ASCII, lower case and trimmed, as the service normalizes it
-- (whose own check is stricter, see services/identity/src/login.rs). NULL for a user who cannot
-- log in by email. Personal data: never logged.
ALTER TABLE identity.users
    ADD COLUMN email text UNIQUE
        CHECK (
            email = lower(btrim(email))
            AND octet_length(email) BETWEEN 3 AND 254
            AND octet_length(email) = char_length(email)
            AND strpos(email, '@') > 1
        );

-- One row per login code sent. A code is a short human-typed number, so guessing it is limited
-- instead: it is valid for 10 minutes, dies after 5 attempts, and a newer code invalidates it
-- (services/identity/README.md, "登入"). The service deletes rows after a day.
CREATE TABLE identity.login_codes (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id uuid NOT NULL REFERENCES identity.users (id) ON DELETE CASCADE,
    -- HMAC-SHA-256 of the purpose, the address it was sent to and the code, under a key derived
    -- from the identity signing key, which is not in the database; never the code.
    code_hash bytea NOT NULL CHECK (octet_length(code_hash) = 32),
    created_at timestamptz NOT NULL DEFAULT now(),
    expires_at timestamptz NOT NULL,
    -- Verification attempts so far, the right one included.
    attempts integer NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    -- Set by the one attempt that matched, in the transaction that creates the session.
    consumed_at timestamptz,
    -- Set when a newer code was issued, or when the code could not be sent.
    invalidated_at timestamptz,
    CHECK (expires_at > created_at)
);

-- At most one code per user can still be used: issuing a new one first invalidates the others,
-- and two concurrent issues for one user cannot both commit (the second fails on this index).
CREATE UNIQUE INDEX login_codes_one_open_per_user ON identity.login_codes (user_id)
    WHERE consumed_at IS NULL AND invalidated_at IS NULL;

-- The send limits count a user's codes of the last minute, hour and day.
CREATE INDEX login_codes_user_created ON identity.login_codes (user_id, created_at);

-- The daily clean-up deletes by age.
CREATE INDEX login_codes_created ON identity.login_codes (created_at);

-- Logging out everywhere revokes a user's sessions, and a new session takes the tenant of the
-- user's latest one.
CREATE INDEX sessions_user_created ON identity.sessions (user_id, created_at);

GRANT SELECT, INSERT, DELETE ON identity.login_codes TO identity_app;
GRANT UPDATE (attempts, consumed_at, invalidated_at) ON identity.login_codes TO identity_app;
-- Only the columns a login writes: `created_at` always takes its default, the transaction's
-- now(), so the 7-day check below measures from the real creation time; `revoked_at` starts
-- NULL.
GRANT INSERT (id, token_hash, user_id, active_tenant_id, expires_at) ON identity.sessions
    TO identity_app;
GRANT UPDATE (expires_at, revoked_at) ON identity.sessions TO identity_app;

-- An invoker's trigger function: it reads only the row being written and runs with the rights
-- of whoever writes it. ERRCODE 42501 (insufficient_privilege), like a missing grant.
CREATE FUNCTION identity.sessions_guard() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF TG_OP = 'UPDATE' AND OLD.revoked_at IS NOT NULL
        AND NEW.revoked_at IS DISTINCT FROM OLD.revoked_at THEN
        RAISE EXCEPTION 'a revoked session stays revoked'
            USING ERRCODE = 'insufficient_privilege';
    END IF;
    IF (TG_OP = 'INSERT' OR NEW.expires_at > OLD.expires_at)
        AND NEW.expires_at > NEW.created_at + interval '7 days' THEN
        RAISE EXCEPTION 'a session lives at most 7 days from its creation'
            USING ERRCODE = 'insufficient_privilege';
    END IF;
    RETURN NEW;
END;
$$;

REVOKE ALL ON FUNCTION identity.sessions_guard() FROM PUBLIC;

CREATE TRIGGER sessions_guard
    BEFORE INSERT OR UPDATE OF expires_at, revoked_at ON identity.sessions
    FOR EACH ROW EXECUTE FUNCTION identity.sessions_guard();
