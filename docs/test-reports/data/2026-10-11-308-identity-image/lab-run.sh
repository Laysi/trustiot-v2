#!/usr/bin/env bash
# Laysi/trustiot-v2#308: runs the identity image (loaded from ~/e2e-308/identity.oci.tar) on the
# lab Mac with podman and checks /healthz on the health port. Every key and password here is a
# throwaway made at run time in ~/e2e-308 (0700) and deleted with it; nothing is printed.
set -euo pipefail
dir=$HOME/e2e-308
image=localhost/trustiot-e2e-308/identity:0.0.0-rc.1
name=e2e-308-identity
cleanup() {
  podman rm -f "$name" > /dev/null 2>&1 || true
  podman rmi "$image" > /dev/null 2>&1 || true
  cd "$HOME"; rm -rf "$dir"
}
trap cleanup EXIT
cd "$dir"
echo "== host: $(uname -srm), podman $(podman --version | cut -d' ' -f3)"
echo "== load the release build's OCI archive"
sha256sum identity.oci.tar
loaded=$(podman load -q -i identity.oci.tar | sed -n 's/^Loaded image: *//p' | tail -1)
echo "loaded: $loaded"
podman tag "$loaded" "$image"
podman image inspect "$image" --format 'image os={{.Os}} arch={{.Architecture}} size={{.Size}} user={{.Config.User}} entrypoint={{.Config.Entrypoint}}'

echo "== throwaway settings"
umask 077
mkdir -p keys
openssl genpkey -algorithm ed25519 -out keys/signing-key.pem
# The container runs as UID 65532, which rootless podman maps to a subordinate UID: the
# throwaway key must be world-readable for it, and is deleted with the directory.
chmod 0755 keys; chmod 0444 keys/signing-key.pem
x=$(openssl pkey -in keys/signing-key.pem -pubout -outform DER | tail -c 32 | basenc --base64url | tr -d '=')
keys="{\"keys\":[{\"kty\":\"OKP\",\"crv\":\"Ed25519\",\"kid\":\"e2e-308\",\"x\":\"$x\"}]}"
db_password=$(openssl rand -hex 16)
cat > env.list <<EOF
TRUSTIOT_INTERNAL_IDENTITY_KEYS=$keys
TRUSTIOT_IDENTITY_SIGNING_KEY_FILE=/etc/trustiot/identity/signing-key.pem
TRUSTIOT_INGRESS_ROUTES=[{"host":"app.e2e-308.invalid","path":"/","match":"PathPrefix","service":"assets"}]
TRUSTIOT_POSTGRES_HOST=postgres.e2e-308.invalid
TRUSTIOT_POSTGRES_PORT=5432
TRUSTIOT_POSTGRES_DATABASE=identity
TRUSTIOT_POSTGRES_SSLMODE=verify-full
TRUSTIOT_POSTGRES_CA_FILE=/etc/trustiot/postgres-ca/ca.crt
TRUSTIOT__DATABASE__USERNAME=identity_app
TRUSTIOT__DATABASE__PASSWORD=$db_password
EOF
echo "settings: $(cut -d= -f1 env.list | tr '\n' ' ')"

echo "== run (read-only root filesystem, image's own user, health port 8081 on 127.0.0.1:18308)"
podman rm -f "$name" > /dev/null 2>&1 || true
podman run -d --name "$name" --read-only --cap-drop=ALL --security-opt no-new-privileges \
  -p 127.0.0.1:18308:8081 -p 127.0.0.1:18309:8080 \
  -v "$dir/keys:/etc/trustiot/identity:ro,Z" --env-file env.list "$image" > /dev/null
code=000
for i in $(seq 1 60); do
  code=$(curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:18308/healthz || true)
  [ "$code" = 200 ] && break
  sleep 1
done
echo "GET /healthz on the health port -> $code (after ${i} s)"
curl -s -i http://127.0.0.1:18308/healthz | tr -d '\r' | head -5
echo "GET /readyz on the health port -> $(curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:18308/readyz) (no database: expected 503)"
echo "GET /internal/ext-authz/ on the API port without a cookie -> $(curl -s -o /dev/null -w '%{http_code}' -H 'Host: app.e2e-308.invalid' http://127.0.0.1:18309/internal/ext-authz/) (expected 401)"
podman inspect "$name" --format 'container state={{.State.Status}} restarts={{.RestartCount}} user={{.Config.User}} image={{.ImageName}}'
echo "== container log (first 20 lines)"
podman logs "$name" 2>&1 | head -20 | cut -c1-400
if podman logs "$name" 2>&1 | grep -q -e "$db_password" -e "PRIVATE KEY"; then echo "LEAK: log contains a secret"; else echo "log holds neither the database password nor the private key"; fi

echo "== cleanup"
cleanup
podman images --format '{{.Repository}}:{{.Tag}}' | grep -c e2e-308 || true
test ! -e "$dir" && echo "removed $dir"
[ "$code" = 200 ]
