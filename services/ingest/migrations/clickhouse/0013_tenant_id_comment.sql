-- Where telemetry.tenant_id comes from (Laysi/trustiot-v2#641). 0002_telemetry.sql described it
-- as taken from the gateway connection certificate; the ingest service takes it from its gateway
-- binding table instead (services/ingest/README.md, 「閘道器綁定投影」 and 「寫入流程」): the
-- certificate names only the gateway, and the tenant a gateway is bound to can change. The
-- wording follows stream_commits.tenant_id (0012_gateway_stream_ids.sql).
--
-- COMMENT COLUMN changes only the table's metadata: no part is rewritten and no mutation runs.
-- Applying it again writes the same comment.

ALTER TABLE trustiot.telemetry COMMENT COLUMN tenant_id 'Tenant the gateway was bound to when the batch was written, from the ingest service''s binding table, never from the payload';
