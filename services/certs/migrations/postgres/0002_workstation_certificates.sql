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
