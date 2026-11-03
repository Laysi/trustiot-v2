#!/usr/bin/env bash
# Runs on the build host, from the root of the copied worktree.
set -uo pipefail
# A separate instance with its own ports, so cleanup only purges what this run created.
export TRUSTIOT_DEV_INSTANCE=e2e-780-$$ TRUSTIOT_DEV_POSTGRES_PORT=25432 TRUSTIOT_DEV_CLICKHOUSE_PORT=28123 \
  TRUSTIOT_DEV_MQTT_PORT=21883 TRUSTIOT_DEV_PORT_EXAMPLE_SERVICE=28080
W=$(mktemp -d)
PW=pw780-$$
cleanup() {
  kill $(jobs -p) 2> /dev/null
  podman rm --force --volumes "$PW" 2> /dev/null
  bun scripts/dev.ts purge > "$W/purge.log" 2>&1
}
trap cleanup EXIT

bun install --cwd scripts --frozen-lockfile > /dev/null 2>&1 || exit 1
bun install --cwd frontend --frozen-lockfile > /dev/null 2>&1 || exit 1
export CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-4}

echo "== up"
bun scripts/dev.ts up > "$W/up.log" 2>&1 || { tail -20 "$W/up.log"; exit 1; }
tail -15 "$W/up.log"
echo "== seed"
bun scripts/dev.ts seed > "$W/seed.out" 2>&1 || { tail -20 "$W/seed.out"; exit 1; }
sed -n 's/^ *Cookie: __Host-trustiot-session=//p' "$W/seed.out" > "$W/cookie"
test -s "$W/cookie" || { echo "no cookie"; tail "$W/seed.out"; exit 1; }

echo "== data"
PGC=trustiot-dev-$TRUSTIOT_DEV_INSTANCE-postgres
test "$(podman inspect --format '{{index .Config.Labels "io.laysi.trustiot-dev.instance"}}' "$PGC")" = "$TRUSTIOT_DEV_INSTANCE" \
  || { echo "$PGC is not this run's Postgres"; exit 1; }
PGU=$(podman exec "$PGC" printenv POSTGRES_USER)
PGD=$(podman exec "$PGC" printenv POSTGRES_DB)
podman exec -i "$PGC" psql -U "$PGU" -d "$PGD" --no-psqlrc --set=ON_ERROR_STOP=1 <<'SQL'
SET ROLE identity_owner;
INSERT INTO identity.tenants (tenant_id, name) VALUES
  ('00000000-0000-4000-8000-000000000b01', 'Acme Factory'),
  ('00000000-0000-4000-8000-000000000b02', 'Beta Plant')
ON CONFLICT (tenant_id) DO UPDATE SET name = excluded.name;
UPDATE identity.users SET platform_admin = true WHERE id = '00000000-0000-4000-8000-000000000a01';
INSERT INTO identity.memberships (tenant_id, user_id, tenant_role) VALUES
  ('00000000-0000-4000-8000-000000000b02', '00000000-0000-4000-8000-000000000a01', 'viewer')
ON CONFLICT (tenant_id, user_id) DO NOTHING;
SQL

echo "== build frontend"
rm -rf frontend/dist
bun run --cwd frontend build > "$W/build.log" 2>&1 || { tail -20 "$W/build.log"; exit 1; }
test -f frontend/dist/index.html || { echo "no build output"; exit 1; }
cp -r frontend/dist "$W/dist"

echo "== start services"
set -a
eval "$(bun scripts/dev.ts env)"
set +a
(
  TRUSTIOT_INGRESS_ROUTES='[{"host":"localhost","path":"/api/identity","match":"PathPrefix","service":"identity"},{"host":"localhost","path":"/","match":"PathPrefix","service":"assets"}]' \
    TRUSTIOT__DATABASE__USERNAME="$TRUSTIOT_DEV_IDENTITY_DATABASE_USERNAME" \
    TRUSTIOT__DATABASE__PASSWORD="$TRUSTIOT_DEV_IDENTITY_DATABASE_PASSWORD" \
    TRUSTIOT__DATABASE__PLATFORM_USERNAME="$TRUSTIOT_DEV_IDENTITY_PLATFORM_DATABASE_USERNAME" \
    TRUSTIOT__DATABASE__PLATFORM_PASSWORD="$TRUSTIOT_DEV_IDENTITY_PLATFORM_DATABASE_PASSWORD" \
    TRUSTIOT__HTTP__LISTEN_ADDR=127.0.0.1:28180 TRUSTIOT__HTTP__HEALTH_LISTEN_ADDR=127.0.0.1:28181 \
    TRUSTIOT__EXCHANGE__LISTEN_ADDR=127.0.0.1:28182 \
    cargo run --locked -p identity > "$W/identity.log" 2>&1
) &
(
  TRUSTIOT__ROOT_DIR="$W/dist" TRUSTIOT__HTTP__LISTEN_ADDR=127.0.0.1:28280 \
    TRUSTIOT__HTTP__HEALTH_LISTEN_ADDR=127.0.0.1:28281 cargo run --locked -p web > "$W/web.log" 2>&1
) &
for i in $(seq 1 180); do
  curl -fs http://127.0.0.1:28181/readyz > /dev/null 2>&1 && curl -fs http://127.0.0.1:28281/readyz > /dev/null 2>&1 && break
  sleep 5
done
curl -s -o /dev/null -w "identity readyz %{http_code}\n" http://127.0.0.1:28181/readyz
curl -s -o /dev/null -w "web readyz %{http_code}\n" http://127.0.0.1:28281/readyz
tail -5 "$W/identity.log" "$W/web.log"
bun "${STANDIN:?directory with proxy.ts and drive.mjs}"/proxy.ts &
sleep 2
curl -sI http://localhost:28300/ | head -5

echo "== browser"
PW=pw780-$$
mkdir -p "$W/drive"
cp "${STANDIN:?directory with proxy.ts and drive.mjs}"/drive.mjs "$W/drive/"
cp "$W/cookie" "$W/drive/cookie"
podman run --rm --name "$PW" --network host -v "$W/drive:/work:Z" -w /work -e COOKIE_FILE=/work/cookie \
  mcr.microsoft.com/playwright:v1.49.1-noble \
  bash -c 'npm init -y > /dev/null && npm i playwright-core@1.49.1 > /dev/null 2>&1 && node drive.mjs'
rc=$?
echo "browser exit $rc"
echo "== identity log (suspension related)"
grep -i "suspend\|tenant's state" "$W/identity.log" | sed -E 's/\x1b\[[0-9;]*m//g' | tail -15
exit $rc
