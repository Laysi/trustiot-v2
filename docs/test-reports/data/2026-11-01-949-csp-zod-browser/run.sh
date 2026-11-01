#!/usr/bin/env bash
# Runs on the build host, from the root of the copied worktree. STANDIN is the directory with
# proxy.ts and drive.mjs (docs/test-reports/data/2026-11-01-949-csp-zod-browser).
set -uo pipefail
STANDIN=${STANDIN:?directory with proxy.ts and drive.mjs}
# A separate instance with its own ports, so cleanup only purges what this run created.
export TRUSTIOT_DEV_INSTANCE=e2e-949-$$ TRUSTIOT_DEV_POSTGRES_PORT=25432 TRUSTIOT_DEV_CLICKHOUSE_PORT=28123 \
  TRUSTIOT_DEV_MQTT_PORT=21883 TRUSTIOT_DEV_PORT_EXAMPLE_SERVICE=28080
W=$(mktemp -d)
PW=pw949-$$
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
tail -8 "$W/up.log"
echo "== seed"
bun scripts/dev.ts seed > "$W/seed.out" 2>&1 || { tail -20 "$W/seed.out"; exit 1; }
sed -n 's/^ *Cookie: __Host-trustiot-session=//p' "$W/seed.out" > "$W/cookie"
test -s "$W/cookie" || { echo "no cookie"; tail "$W/seed.out"; exit 1; }

echo "== build frontend"
test ! -e frontend/dist || { echo "frontend/dist already exists: remove it first, this run builds its own"; exit 1; }
bun run --cwd frontend build > "$W/build.log" 2>&1 || { tail -20 "$W/build.log"; exit 1; }
test -f frontend/dist/index.html || { echo "no build output"; exit 1; }
cp -r frontend/dist "$W/dist"

echo "== does the built JavaScript still hold zod's JIT code? (it must not run)"
find "$W/dist" -name '*.js' -printf '%P %s bytes\n' | sort
for f in $(find "$W/dist" -name '*.js' | sort); do
  n=$(basename "$f")
  echo "$n: new Function x$(grep -o 'new Function' "$f" | wc -l), Function( x$(grep -o '[^a-zA-Z_.]Function(' "$f" | wc -l), eval( x$(grep -o '[^a-zA-Z_.]eval(' "$f" | wc -l), jitless x$(grep -o 'jitless' "$f" | wc -l)"
  grep -o '.\{80\}[^a-zA-Z_.]Function(.\{60\}' "$f" | head -3 | sed "s|^|$n: |"
  grep -o '.\{80\}jitless.\{60\}' "$f" | head -4 | sed "s|^|$n: |"
done

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
# Negative control: the same build with zod's `jitless` switched off, served by a second web service.
cp -r "$W/dist" "$W/dist-jit"
for f in "$W"/dist-jit/static/*.js; do sed -i 's/({jitless:!0})/({jitless:!1})/' "$f"; done
echo "jitless switched off in the control copy: $(grep -l '({jitless:!1})' "$W"/dist-jit/static/*.js | wc -l) file(s)"
(
  TRUSTIOT__ROOT_DIR="$W/dist-jit" TRUSTIOT__HTTP__LISTEN_ADDR=127.0.0.1:28290 \
    TRUSTIOT__HTTP__HEALTH_LISTEN_ADDR=127.0.0.1:28291 cargo run --locked -p web > "$W/web-jit.log" 2>&1
) &
for i in $(seq 1 180); do
  curl -fs http://127.0.0.1:28181/readyz > /dev/null 2>&1 && curl -fs http://127.0.0.1:28281/readyz > /dev/null 2>&1 \
    && curl -fs http://127.0.0.1:28291/readyz > /dev/null 2>&1 && break
  sleep 5
done
curl -s -o /dev/null -w "identity readyz %{http_code}\n" http://127.0.0.1:28181/readyz
curl -s -o /dev/null -w "web readyz %{http_code}\n" http://127.0.0.1:28281/readyz
tail -n 3 "$W/identity.log" "$W/web.log"
bun "$STANDIN"/proxy.ts &
sleep 2
echo "== the page's headers, as the browser gets them"
curl -sI http://localhost:28300/settings | grep -i "^HTTP\|content-security-policy"

echo "== browser"
mkdir -p "$W/drive"
cp "$STANDIN"/drive.mjs "$W/drive/"
cp frontend/src/i18n/locales/en.json frontend/src/i18n/locales/zh-TW.json "$W/drive/"
cp "$W/cookie" "$W/drive/cookie"
podman run --rm --name "$PW" --network host -v "$W/drive:/work:Z" -w /work -e COOKIE_FILE=/work/cookie \
  mcr.microsoft.com/playwright:v1.49.1-noble \
  bash -c 'npm init -y > /dev/null && npm i playwright-core@1.49.1 > /dev/null 2>&1 && node drive.mjs'
rc=$?
echo "browser exit $rc"
exit $rc
