-- Factory registration (Laysi/trustiot-v2#79, rebuild checklist D1, decision 12;
-- services/certs/README.md, "出廠登錄"). Run by `certs_owner` like 0001_certs.sql, in one
-- transaction.
--
-- A production workstation registers each gateway once: its hardware ID, the public key of its
-- TPM key (from a CSR), the public part of the TPM's endorsement key, and the initial gateway
-- certificate the CA signed for it. A registration is platform data, not tenant data: a gateway
-- belongs to no tenant when it leaves the factory, and the assets service assigns it to one
-- later. So no `tenant_id` and no row-level security.
--
-- Only the platform role `certs_platform` reaches these tables: the registration writes the
-- platform-wide event `certs.gateway_registered` in the same transaction, and only a producer's
-- platform role may write platform-wide events (crates/outbox). `certs_app` gets nothing here.

GRANT USAGE ON SCHEMA certs TO certs_platform;

-- One row per registered gateway. For now a hardware ID is registered once and stays taken even
-- after decommissioning: its certificates, MQTT identity and event history keep naming the
-- same device. A replaced board or TPM is a new gateway with a new hardware ID. This rule is
-- provisional (2026-10-23): whether a decommissioned hardware ID may be registered again is
-- decided in Laysi/trustiot-v2#115, and allowing it needs a new migration, since the hardware
-- ID is this table's primary key and the aggregate id of its events.
CREATE TABLE certs.gateway_registrations (
    -- The gateway's identity everywhere (protocol::GatewayId): its certificates' common name,
    -- its MQTT username and the gateway segment of its topics.
    hardware_id text PRIMARY KEY CHECK (hardware_id ~ '^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$'),
    -- The TPM key's public key, DER SubjectPublicKeyInfo (ECDSA P-256): never changes for this
    -- hardware ID, and never registered twice.
    public_key bytea NOT NULL UNIQUE CHECK (octet_length(public_key) BETWEEN 1 AND 1024),
    -- The TPM's endorsement key, DER SubjectPublicKeyInfo as the workstation sent it.
    endorsement_key bytea NOT NULL CHECK (octet_length(endorsement_key) BETWEEN 1 AND 2048),
    -- Its SHA-256, for the uniqueness of active registrations below.
    endorsement_key_sha256 bytea NOT NULL CHECK (octet_length(endorsement_key_sha256) = 32),
    -- The initial certificate the CA signed for it, in the ledger.
    certificate_serial bytea NOT NULL UNIQUE REFERENCES certs.issued_certificates (serial),
    -- The workstation that registered it: its name and the certificate it signed the request
    -- with.
    workstation text NOT NULL CHECK (workstation ~ '^[A-Za-z0-9][A-Za-z0-9._-]{0,47}$'),
    workstation_certificate_serial bytea NOT NULL
        REFERENCES certs.issued_certificates (serial),
    -- The platform administrator signed in on the workstation.
    registered_by uuid NOT NULL,
    registered_at timestamptz NOT NULL DEFAULT now(),
    -- Set once when the gateway is decommissioned (Laysi/trustiot-v2#115); the row stays, so the
    -- hardware ID stays taken (provisional, see the table's comment).
    decommissioned_at timestamptz
);

-- One TPM, one active gateway: a decommissioned gateway's TPM may be registered again under a
-- new hardware ID.
CREATE UNIQUE INDEX gateway_registrations_endorsement_key
    ON certs.gateway_registrations (endorsement_key_sha256)
    WHERE decommissioned_at IS NULL;

-- The nonces of signed workstation requests, so a captured request cannot be sent again. A nonce
-- is kept until its request's timestamp can no longer be accepted; the service deletes expired
-- rows.
CREATE TABLE certs.workstation_request_nonces (
    certificate_serial bytea NOT NULL REFERENCES certs.issued_certificates (serial),
    nonce text NOT NULL CHECK (nonce ~ '^[A-Za-z0-9_-]{22,64}$'),
    expires_at timestamptz NOT NULL,
    PRIMARY KEY (certificate_serial, nonce)
);

CREATE INDEX workstation_request_nonces_expiry
    ON certs.workstation_request_nonces (expires_at);

-- A registration is never deleted and never rewritten, for every role: the hardware ID stays
-- taken, and the keys and the certificate stay what was registered. The one change is setting
-- `decommissioned_at` once. The trigger holds the owner and superusers to the same rules as the
-- service, which has no UPDATE or DELETE grant at all. An invoker's function with a pinned
-- search_path, like certs.issued_certificates_guard; ERRCODE 42501 (insufficient_privilege).
CREATE FUNCTION certs.gateway_registrations_guard() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, pg_temp
    AS $$
BEGIN
    IF TG_OP = 'UPDATE' THEN
        IF OLD.decommissioned_at IS NULL AND NEW.decommissioned_at IS NOT NULL
            AND to_jsonb(NEW) - 'decommissioned_at' = to_jsonb(OLD) - 'decommissioned_at' THEN
            RETURN NEW;
        END IF;
        RAISE EXCEPTION 'gateway registration % can only be decommissioned, once',
            OLD.hardware_id
            USING ERRCODE = 'insufficient_privilege';
    END IF;
    RAISE EXCEPTION 'gateway registrations are never removed'
        USING ERRCODE = 'insufficient_privilege';
END;
$$;

REVOKE ALL ON FUNCTION certs.gateway_registrations_guard() FROM PUBLIC;

CREATE TRIGGER gateway_registrations_guard
    BEFORE UPDATE OR DELETE ON certs.gateway_registrations
    FOR EACH ROW EXECUTE FUNCTION certs.gateway_registrations_guard();

CREATE TRIGGER gateway_registrations_no_truncate
    BEFORE TRUNCATE ON certs.gateway_registrations
    FOR EACH STATEMENT EXECUTE FUNCTION certs.gateway_registrations_guard();

-- The registration reads the CRLs (an issuing CA signs only once it has one), reads the
-- workstation's ledger row, and records the gateway certificate it signs.
GRANT SELECT ON certs.crls TO certs_platform;
GRANT SELECT, INSERT ON certs.issued_certificates TO certs_platform;
GRANT SELECT, INSERT ON certs.gateway_registrations TO certs_platform;
GRANT SELECT, INSERT, DELETE ON certs.workstation_request_nonces TO certs_platform;
