-- certs service schema (Laysi/trustiot-v2#78): the ledger of the certificates the online issuing
-- CA signed and the current CRL of each issuing CA (decisions 6 and 12). Run by the migration
-- role `certs_owner`, which owns everything here; the service role `certs_app` owns nothing
-- (crates/service-kit/README.md, "角色與 policy 範本"). The migration Job (tools/migrate) creates
-- both roles and the schema `certs` owned by `certs_owner` and applies these files in order as
-- `certs_owner`, each in one transaction.
--
-- No private key is ever stored here: the issuing CA keys live in the Secret the certs pod's
-- init container fills (services/certs/README.md, "CA 階層與私鑰保管"), the root key is offline,
-- and a gateway's key never leaves its TPM. Certificates and CRLs are public data.
--
-- Neither table holds tenant data: a certificate belongs to a gateway, and which tenant a gateway
-- belongs to is the registration's record (Laysi/trustiot-v2#79), which links to the serial
-- here. So no `tenant_id` and no row-level security; the service reads and writes these tables
-- in transactions without a tenant.

GRANT USAGE ON SCHEMA certs TO certs_app;

-- One row per certificate the service signed. The service only inserts: revocation
-- (Laysi/trustiot-v2#82) grants the UPDATE of `revoked_at` and `revocation_reason` in its own
-- file.
CREATE TABLE certs.issued_certificates (
    -- The serial number: the content octets of its DER INTEGER, positive, at most 20 octets
    -- (RFC 5280 section 4.1.2.2).
    serial bytea PRIMARY KEY CHECK (octet_length(serial) BETWEEN 1 AND 20),
    -- The issuing CA key that signed it: the SHA-256 of its public key (crates/ca-keys).
    ca_key_id text NOT NULL CHECK (ca_key_id ~ '^[0-9a-f]{64}$'),
    -- The profile that decided its subject and extensions.
    profile text NOT NULL CHECK (profile IN ('gateway')),
    -- The common name the CA set: the gateway id for the gateway profile.
    subject text NOT NULL CHECK (length(subject) BETWEEN 1 AND 64),
    not_before timestamptz NOT NULL,
    not_after timestamptz NOT NULL,
    issued_at timestamptz NOT NULL DEFAULT now(),
    -- The certificate, DER.
    certificate bytea NOT NULL,
    -- When it was revoked; a revoked certificate stays on its CA's CRL until it expires.
    revoked_at timestamptz,
    -- RFC 5280 CRLReason code (7 is unused); NULL leaves the reason out of the CRL.
    revocation_reason smallint CHECK (revocation_reason BETWEEN 0 AND 10 AND revocation_reason <> 7),
    CHECK (not_before < not_after),
    CHECK (revocation_reason IS NULL OR revoked_at IS NOT NULL)
);

-- What a CRL lists: the revoked certificates of one CA that have not expired.
CREATE INDEX issued_certificates_revoked
    ON certs.issued_certificates (ca_key_id, not_after)
    WHERE revoked_at IS NOT NULL;

-- A gateway's certificates, for renewal (Laysi/trustiot-v2#81) and revocation.
CREATE INDEX issued_certificates_subject ON certs.issued_certificates (subject, not_after);

-- The current CRL of each issuing CA. The CRL job replaces a row only with a higher CRL number
-- (RFC 5280 section 5.2.3), so two replicas that both regenerate cannot go back.
CREATE TABLE certs.crls (
    ca_key_id text PRIMARY KEY CHECK (ca_key_id ~ '^[0-9a-f]{64}$'),
    crl_number bigint NOT NULL CHECK (crl_number > 0),
    this_update timestamptz NOT NULL,
    next_update timestamptz NOT NULL,
    -- The CRL, DER.
    crl bytea NOT NULL,
    CHECK (this_update < next_update)
);

GRANT SELECT, INSERT ON certs.issued_certificates TO certs_app;
GRANT SELECT, INSERT, UPDATE ON certs.crls TO certs_app;
