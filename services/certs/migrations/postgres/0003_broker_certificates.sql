-- The MQTT broker's server certificate (Laysi/trustiot-v2#557; services/certs/README.md, "broker
-- 伺服器憑證"). Run by `certs_owner` like 0001_certs.sql, in one transaction.
--
-- The ledger takes a third profile: the broker's TLS server certificate, from the same issuing CA
-- as gateways and workstations, told apart by its extended key usage (server authentication
-- only), so the broker never accepts one as a gateway's client certificate. Its subject is the
-- fixed name `mqtt-broker.trustiot.internal`, which the CA never gives a gateway.
ALTER TABLE certs.issued_certificates
    DROP CONSTRAINT issued_certificates_profile_check,
    ADD CONSTRAINT issued_certificates_profile_check
        CHECK (profile IN ('gateway', 'workstation', 'broker'));
