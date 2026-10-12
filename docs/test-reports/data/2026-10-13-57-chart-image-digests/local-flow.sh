#!/usr/bin/env bash
# The local release flow of scripts/README.md ("本機跑一次完整流程") with the checks of
# Laysi/trustiot-v2#57 (the published chart pins this release's images by digest):
#
#   1. plan, build, publish, publish again (must find the chart already there), verify;
#   2. render the pulled chart and compare its image references with the pushed digests;
#   3. negative probe: re-push a signed chart whose postgres digest is wrong; verify must refuse it.
#
# Run from the repository root on a machine with podman, skopeo, tar and `proto install`:
#   bash docs/test-reports/data/2026-10-13-57-chart-image-digests/local-flow.sh
# LOCAL_FLOW_IMAGES=synthetic replaces build.ts with tiny FROM-scratch arm64 images, to exercise
# publish and verify quickly. Everything goes to stdout; the work directory under ~/.cache is
# removed at the end.
set -euo pipefail

version=0.0.57-rc.1
tag="v$version"
port=${RELEASE_REGISTRY_PORT:-5157}
registry="127.0.0.1:$port/laysi/trustiot-v2"
container=release-registry-57
mkdir -p ~/.cache
work="$(mktemp -d ~/.cache/trustiot-release.XXXXXX)"
# The ID of the registry container this script started; cleanup stops only that container.
started=""
cleanup() {
  if [[ -n $started ]]; then
    podman stop "$started" > /dev/null 2>&1 || true
  fi
  rm -rf "$work"
}
trap cleanup EXIT
helm="$(proto bin helm --reporter text | tail -1)"
cosign="$(proto bin cosign --reporter text | tail -1)"

step() { printf '\n===== %s (%s)\n' "$1" "$(date -u +%FT%TZ)"; }

step "environment"
echo "commit: $(git rev-parse HEAD)"
uname -srm
podman --version
skopeo --version
"$helm" version --short
"$cosign" version 2>&1 | grep -E '^GitVersion' || true
bun --version

step "registry and test key"
started="$(podman run -d --rm --name "$container" -p "127.0.0.1:$port:5000" \
  docker.io/library/registry:3@sha256:ddf754342cfc8acc51a56d5d0ab6af06826461864460636d8bd5c546dab2a7b8)"
COSIGN_PASSWORD= "$cosign" generate-key-pair --output-key-prefix "$work/test" > /dev/null 2>&1
printf '### 其他\n\n- **測試**：本機發佈（Laysi/trustiot-v2#57）。\n' > "$work/notes.md"

step "plan"
bun scripts/release/plan.ts --tag "$tag" --dry-run --registry "$registry" --output "$work/plan.json"

if [[ ${LOCAL_FLOW_IMAGES:-build} == synthetic ]]; then
  # Publish and verify only: one tiny linux/arm64 image (FROM scratch, no RUN, so no emulator)
  # and a minimal SPDX document per planned image, in the layout build.ts writes.
  step "synthetic images instead of build.ts (LOCAL_FLOW_IMAGES=synthetic)"
  mkdir -p "$work/out/images" "$work/synthetic"
  printf 'FROM scratch\nCOPY marker /marker\n' > "$work/synthetic/Containerfile"
  # Helper results go through files: a tool shim may print notices on stdout.
  bun -e 'const [plan, out] = process.argv.slice(1); const names = JSON.parse(await Bun.file(plan).text()).images.map((i) => i.name); await Bun.write(out, names.join("\n") + "\n");' \
    "$work/plan.json" "$work/image-names"
  for name in $(cat "$work/image-names"); do
    local_tag="localhost/trustiot-57-synthetic/$name:$version"
    printf '%s %s\n' "$name" "$version" > "$work/synthetic/marker"
    podman build -q --platform linux/arm64 --timestamp 0 -t "$local_tag" "$work/synthetic" > /dev/null
    podman push -q --format oci "$local_tag" "oci-archive:$work/out/images/$name.oci.tar"
    podman image rm "$local_tag" > /dev/null
    printf '{"spdxVersion":"SPDX-2.3","packages":[{"name":"%s","versionInfo":"%s"}]}\n' "$name" "$version" > "$work/out/images/$name.spdx.json"
    echo "$name: $(skopeo inspect --raw "oci-archive:$work/out/images/$name.oci.tar" | sha256sum | cut -d' ' -f1)"
  done
else
  step "build (output in the work directory; last lines only)"
  if ! bun scripts/release/build.ts --plan "$work/plan.json" --out "$work/out" > "$work/build.log" 2>&1; then
    tail -60 "$work/build.log"
    exit 1
  fi
  tail -15 "$work/build.log"
fi

publish() {
  COSIGN_PASSWORD= bun scripts/release/publish.ts --plan "$work/plan.json" --artifacts "$work/out" \
    --notes "$work/notes.md" --output "$1" --local-test-key "$work/test"
}

step "publish, first run"
publish "$work/published.json" 2>&1 | tee "$work/publish-1.log"
step "publish, second run (a re-run of the job with the same build)"
publish "$work/published-2.json" 2>&1 | tee "$work/publish-2.log"
grep -q "already holds this chart" "$work/publish-2.log"
cmp "$work/published.json" "$work/published-2.json"
echo "CHECK second publish: the chart was already there and both runs published the same digests"
cat "$work/published.json"

step "verify"
bun scripts/release/verify.ts --plan "$work/plan.json" --published "$work/published.json" \
  --output "$work/report.md" --local-test-key "$work/test"
echo "--- report.md"
cat "$work/report.md"

step "the pulled chart renders the pushed image digests"
mkdir -p "$work/pulled"
"$helm" pull "oci://$registry/charts/trustiot" --version "$version" --plain-http --destination "$work/pulled" > /dev/null
tgz="$work/pulled/trustiot-$version.tgz"
echo "--- helm show values | image digests and repositories"
"$helm" show values "$tgz" | grep -n -B2 -A1 -E '^ *digest: sha256|registry: 127|repository: 127' | grep -v '^--$'
echo "--- pushed manifests (skopeo, straight from the registry)"
for name in identity postgres release-selftest; do
  printf '%s:%s -> sha256:%s\n' "$registry/$name" "$version" \
    "$(skopeo inspect --raw --tls-verify=false "docker://$registry/$name:$version" | sha256sum | cut -d' ' -f1)"
done
for shape in saas onprem; do
  "$helm" template trustiot "$tgz" --namespace trustiot \
    -f "deploy/chart/values-$shape.yaml" -f "deploy/chart/tests/values/$shape-instance.yaml" \
    --set services.identity.enabled=true > "$work/rendered-$shape.yaml"
  echo "--- helm template with values-$shape.yaml: image references of this release"
  grep -E "image: $registry/" "$work/rendered-$shape.yaml" | sed 's/^ *//' | sort | uniq -c
done
cat > "$work/compare.ts" << 'EOF'
// Every image the chart deploys must render as <repository>:<version>@<digest the registry holds>.
import { readFileSync } from "node:fs";
const [publishedFile, registry, version, ...rendered] = Bun.argv.slice(2);
const published = JSON.parse(readFileSync(publishedFile!, "utf8")) as { images: { name: string; digest: string }[] };
let failed = false;
for (const file of rendered) {
  const text = readFileSync(file, "utf8");
  for (const image of published.images.filter((i) => i.name !== "release-selftest")) {
    const want = `${registry}/${image.name}:${version}@${image.digest}`;
    const lines = text.split("\n").filter((line) => line.includes(`${registry}/${image.name}:`)).map((line) => line.trim());
    const ok = lines.length > 0 && lines.every((line) => line === `image: ${want}`);
    console.log(`${ok ? "OK  " : "FAIL"} ${file.split("/").at(-1)}: ${image.name} -> ${want} (${lines.length} reference(s))`);
    failed ||= !ok;
  }
}
process.exit(failed ? 1 : 0);
EOF
bun "$work/compare.ts" "$work/published.json" "$registry" "$version" "$work/rendered-saas.yaml" "$work/rendered-onprem.yaml"

step "negative probe: a signed chart with a wrong postgres digest"
mkdir -p "$work/tampered/src" "$work/tampered/pkg"
tar -xzf "$tgz" -C "$work/tampered/src"
bun -e 'const [file, dir] = process.argv.slice(1); for (const i of JSON.parse(await Bun.file(file).text()).images) await Bun.write(`${dir}/${i.name}.digest`, i.digest);' \
  "$work/published.json" "$work/tampered"
identity_digest="$(cat "$work/tampered/identity.digest")"
postgres_digest="$(cat "$work/tampered/postgres.digest")"
# The postgres image is pinned to the identity image's digest instead of its own.
sed -i "s/digest: $postgres_digest/digest: $identity_digest/" "$work/tampered/src/trustiot/values.yaml"
grep -c "digest: $identity_digest" "$work/tampered/src/trustiot/values.yaml"
"$helm" package "$work/tampered/src/trustiot" --version "$version" --app-version "$version" --destination "$work/tampered/pkg" > /dev/null
push="$("$helm" push "$work/tampered/pkg/trustiot-$version.tgz" "oci://$registry/charts" --plain-http 2>&1)"
tampered="$(printf '%s\n' "$push" | sed -n 's/^Digest: *//p')"
echo "re-pushed $registry/charts/trustiot:$version as $tampered"
COSIGN_PASSWORD= "$cosign" sign --yes --key "$work/test.key" --use-signing-config=false --tlog-upload=false --allow-http-registry \
  "$registry/charts/trustiot@$tampered" > /dev/null 2>&1
bun -e 'const [file, out, digest] = process.argv.slice(1); const p = JSON.parse(await Bun.file(file).text()); p.chart.digest = digest; await Bun.write(out, JSON.stringify(p, null, 2));' \
  "$work/published.json" "$work/published-tampered.json" "$tampered"
if bun scripts/release/verify.ts --plan "$work/plan.json" --published "$work/published-tampered.json" \
  --local-test-key "$work/test" > "$work/verify-tampered.log" 2>&1; then
  cat "$work/verify-tampered.log"
  echo "FAIL: verify accepted a chart with a wrong postgres digest"
  exit 1
fi
cat "$work/verify-tampered.log"
grep -q "does not pin this release's images" "$work/verify-tampered.log"
echo "CHECK negative probe: verify refused the chart with the wrong postgres digest"

step "done"
