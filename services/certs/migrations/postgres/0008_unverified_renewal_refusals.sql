-- Refusals that need no key, counted per UTC day (Laysi/trustiot-v2#847; services/certs/README.md,
-- "換發", "規則與取捨"). Run by `certs_owner` like the files before it, in one transaction.
--
-- A refused renewal with `challenge_unknown`, `challenge_mismatch`, `challenge_expired` or
-- `signature_invalid` comes before the signature held, so anyone who knows a registered hardware
-- ID can cause it; each window of such refusals becomes one platform-wide
-- `certs.gateway_renewal_refused`, kept forever by the identity service's event log (decision 34).
-- From this release they are counted here, in windows of one day aligned to
-- 2000-01-01T00:00:00Z, so traffic without the key writes at most one event per registered
-- gateway and day. The refusals that only happen after the signature held stay in
-- `renewal_refusals`, in windows of `renewal.refusal_window`.
--
-- A table of its own rather than a category in the primary key of `renewal_refusals`: the build
-- before this file upserts into that table on (hardware_id, window_start), which a changed key
-- would break while it still runs (during the rollout, or after rolling the service back). With
-- this file the older build keeps counting every reason in `renewal_refusals` as before and never
-- touches this table; rows left here while it runs are reported once the service rolls forward.
--
-- The same shape and rules as `renewal_refusals` (0006_certificate_renewal.sql): one row per
-- registered hardware ID and window (the insert selects from the registrations, and the primary
-- key makes concurrent refusals add up in one row), deleted when the window's event is written.
CREATE TABLE certs.renewal_unverified_refusals (
    hardware_id text NOT NULL REFERENCES certs.gateway_registrations (hardware_id),
    -- The window, [window_start, window_end), one day aligned to 2000-01-01T00:00:00Z.
    window_start timestamptz NOT NULL,
    window_end timestamptz NOT NULL CHECK (window_end > window_start),
    refusals bigint NOT NULL CHECK (refusals > 0),
    -- The refusals by reason, for example {"challenge_unknown": 3}; the counts add up to
    -- `refusals`. Only the reasons that need no key: a refusal after the signature held belongs
    -- in `renewal_refusals`, and storing it here would delay its event by up to a day.
    reasons jsonb NOT NULL CHECK (jsonb_typeof(reasons) = 'object')
        CHECK (reasons - ARRAY['challenge_unknown', 'challenge_mismatch', 'challenge_expired',
            'signature_invalid'] = '{}'::jsonb),
    PRIMARY KEY (hardware_id, window_start)
);

CREATE INDEX renewal_unverified_refusals_window_end
    ON certs.renewal_unverified_refusals (window_end);

-- The platform role alone, as for `renewal_refusals`: it counts (only it may read the
-- registrations) and deletes a window's row when it writes the platform-wide event.
GRANT SELECT, INSERT, UPDATE, DELETE ON certs.renewal_unverified_refusals TO certs_platform;
