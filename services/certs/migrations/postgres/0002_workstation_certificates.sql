-- Workstation certificates and their revocation (Laysi/trustiot-v2#534; services/certs/README.md,
-- "工作站憑證"). Run by `certs_owner` like 0001_certs.sql, in one transaction.
--
-- The ledger takes a second profile: production workstations get certificates from the same
-- issuing CA as gateways, told apart by their extended key usage, so the broker never accepts
-- one (README, "工作站憑證"). Their subject is `workstation:<name>`, which is never a gateway
-- id.
ALTER TABLE certs.issued_certificates
    DROP CONSTRAINT issued_certificates_profile_check,
    ADD CONSTRAINT issued_certificates_profile_check
        CHECK (profile IN ('gateway', 'workstation'));

-- Revocation: the service marks a certificate revoked and its CA's next CRL lists it. Only the
-- two revocation columns, so the service still cannot rewrite what was signed; the API of
-- Laysi/trustiot-v2#534 revokes workstation certificates, and gateway revocation
-- (Laysi/trustiot-v2#82) uses the same grant.
GRANT UPDATE (revoked_at, revocation_reason) ON certs.issued_certificates TO certs_app;

-- Revocation is final, for every role. The trigger below keeps two rules the column grant cannot
-- express:
-- - A revoked certificate's row never changes again: nobody takes the revocation back, moves it
--   or changes its reason, so a buggy or compromised service cannot drop a certificate from its
--   CA's next CRL. The service revokes with `WHERE revoked_at IS NULL`, so it never updates a
--   revoked row.
-- - An update changes only the two revocation columns. The grant already limits the service
--   role to them; the trigger holds the owner and superusers to the same. It compares the whole
--   row without those two columns, so a column added later stays frozen until a migration says
--   otherwise.
--
-- An invoker's trigger function: it reads only the row being written and runs with the rights
-- of whoever writes it. Its search_path is pinned, so a session's own search_path cannot change
-- which functions and operators it calls. A trigger fires for every role, the table owner and
-- superusers included: only the owner can disable it (ALTER TABLE) and only a superuser can
-- skip it (session_replication_role). ERRCODE 42501 (insufficient_privilege), like a missing
-- grant.
CREATE FUNCTION certs.issued_certificates_guard() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, pg_temp
    AS $$
BEGIN
    IF OLD.revoked_at IS NOT NULL AND NEW IS DISTINCT FROM OLD THEN
        RAISE EXCEPTION 'certificate % is revoked: its ledger row cannot change',
            encode(OLD.serial, 'hex')
            USING ERRCODE = 'insufficient_privilege';
    END IF;
    IF to_jsonb(NEW) - '{revoked_at,revocation_reason}'::text[]
        IS DISTINCT FROM to_jsonb(OLD) - '{revoked_at,revocation_reason}'::text[] THEN
        RAISE EXCEPTION 'only the revocation of certificate % can change, never what was signed',
            encode(OLD.serial, 'hex')
            USING ERRCODE = 'insufficient_privilege';
    END IF;
    RETURN NEW;
END;
$$;

REVOKE ALL ON FUNCTION certs.issued_certificates_guard() FROM PUBLIC;

CREATE TRIGGER issued_certificates_guard
    BEFORE UPDATE ON certs.issued_certificates
    FOR EACH ROW EXECUTE FUNCTION certs.issued_certificates_guard();
