-- Device updates may change only what an update changes (Laysi/trustiot-v2#100, security review
-- of 2026-10-24; services/assets/README.md, "設備"): the settings, the revision and the time.
-- The name, gateway, binding and model never change, so the service role cannot change them even
-- by mistake. The column-level UPDATE also keeps the role able to take the device's row lock
-- (SELECT ... FOR UPDATE). SELECT, INSERT and DELETE stay as 0004_devices.sql granted them.
REVOKE UPDATE ON assets.devices FROM assets_app;
GRANT UPDATE (transport, host, host_key, port, unit_id, poll_interval_ms, revision, updated_at)
    ON assets.devices TO assets_app;
