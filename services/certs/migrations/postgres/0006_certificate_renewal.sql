-- Certificate renewal over HTTPS (Laysi/trustiot-v2#81, rebuild checklist D2, decision 12;
-- services/certs/README.md, "換發"). Run by `certs_owner` like the files before it, in one
-- transaction.
--
-- The renewal endpoint is public: a gateway whose certificate expired or was revoked cannot use
-- mTLS, so it proves who it is by signing a challenge with its TPM key. Neither table holds
-- tenant data (a challenge names a hardware ID only, a refusal count a registered gateway), so
-- neither has `tenant_id` or row-level security. services/certs/tests/cross_tenant.rs exempts the
-- challenges, which the service role reaches, with the reason; the refusal counts are the
-- platform role's alone.

-- The challenges handed out and not yet redeemed. Every redemption deletes its row first, before
-- any check, so a challenge is redeemed at most once whatever the outcome. The service role
-- inserts and redeems them on the endpoint's requests; the platform role gets nothing here.
--
-- Unauthenticated requests insert rows, so the table must stay bounded: every insert deletes the
-- rows that expired more than the service's grace period ago (renewal.rs, `CHALLENGE_GRACE_SECS`)
-- in the same statement, and the service's global rate limit bounds the inserts per replica. The
-- rows of a replica are then at most its rate times the challenge lifetime plus the grace period
-- (services/certs/README.md, "換發", "限制").
CREATE TABLE certs.renewal_challenges (
    -- The nonce's 32 random bytes (protocol::renewal::NONCE_BYTES); the gateway sees them as
    -- base64url.
    nonce bytea PRIMARY KEY CHECK (octet_length(nonce) = 32),
    -- The hardware ID the challenge was issued for (protocol::GatewayId). Not checked against
    -- the registrations: a challenge looks the same for every hardware ID, so the answer does not
    -- tell whether one is registered.
    hardware_id text NOT NULL
        CHECK (hardware_id COLLATE "C" ~ '^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$'),
    -- What it may be redeemed for (protocol::renewal::Purpose), for example `cert-renewal`.
    purpose text NOT NULL CHECK (purpose COLLATE "C" ~ '^[a-z][a-z0-9-]{0,31}$'),
    -- Unix seconds by the database clock: redeemable until then. The gateway signs this value.
    expires bigint NOT NULL CHECK (expires > 0),
    created_at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX renewal_challenges_expiry ON certs.renewal_challenges (expires);

-- Refused renewals of registered gateways, counted per hardware ID and time window
-- (`renewal.refusal_window`). Every refused request of a registered hardware ID adds one to its
-- window's row; a hardware ID that is not registered adds nothing (the insert selects from the
-- registrations), so unauthenticated requests add at most one row per registered gateway and
-- window. Once a window has ended, one replica deletes its row and writes the platform-wide
-- `certs.gateway_renewal_refused` with the counts in the same transaction, so every window
-- yields exactly one event.
CREATE TABLE certs.renewal_refusals (
    hardware_id text NOT NULL REFERENCES certs.gateway_registrations (hardware_id),
    -- The window, [window_start, window_end), aligned to the window length from
    -- 2000-01-01T00:00:00Z, so every replica counts into the same row.
    window_start timestamptz NOT NULL,
    window_end timestamptz NOT NULL CHECK (window_end > window_start),
    refusals bigint NOT NULL CHECK (refusals > 0),
    -- The refusals by reason, for example {"signature_invalid": 3}; the counts add up to
    -- `refusals`.
    reasons jsonb NOT NULL CHECK (jsonb_typeof(reasons) = 'object'),
    PRIMARY KEY (hardware_id, window_start)
);

CREATE INDEX renewal_refusals_window_end ON certs.renewal_refusals (window_end);

-- The service role hands out and redeems challenges (DELETE ... RETURNING reads the row, so it
-- needs SELECT). The platform role counts refusals (it alone may read the registrations, and it
-- writes the platform-wide event) and deletes a window's row when it writes the event.
GRANT SELECT, INSERT, DELETE ON certs.renewal_challenges TO certs_app;
GRANT SELECT, INSERT, UPDATE, DELETE ON certs.renewal_refusals TO certs_platform;
