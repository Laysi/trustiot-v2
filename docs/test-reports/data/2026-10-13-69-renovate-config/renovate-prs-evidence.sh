#!/usr/bin/env bash
# Evidence that Renovate's PRs keep SHA and digest pins: the pinned lines each merge commit
# changed, and what the upstream tag resolves to now. Usage: pr-evidence.sh <merge-commit>...
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
resolve_action() { # owner/repo tag -> commit SHA the tag points at (annotated tags dereferenced)
  local ref type sha
  ref=$(gh api "repos/$1/git/ref/tags/$2")
  type=$(jq -r .object.type <<<"$ref"); sha=$(jq -r .object.sha <<<"$ref")
  if [ "$type" = tag ]; then sha=$(gh api "repos/$1/git/tags/$sha" -q .object.sha); fi
  echo "$sha"
}
ghcr_digest() { # repo tag
  local tok; tok=$(curl -fsS "https://ghcr.io/token?scope=repository:$1:pull" | jq -r .token)
  curl -fsSI -H "Authorization: Bearer $tok" -H 'Accept: application/vnd.oci.image.index.v1+json, application/vnd.docker.distribution.manifest.list.v2+json, application/vnd.oci.image.manifest.v1+json' "https://ghcr.io/v2/$1/manifests/$2" | tr -d '\r' | awk -F': ' 'tolower($1)=="docker-content-digest"{print $2}'
}
dockerhub_digest() { # repo tag
  local tok; tok=$(curl -fsS "https://auth.docker.io/token?service=registry.docker.io&scope=repository:$1:pull" | jq -r .token)
  curl -fsSI -H "Authorization: Bearer $tok" -H 'Accept: application/vnd.oci.image.index.v1+json, application/vnd.docker.distribution.manifest.list.v2+json, application/vnd.oci.image.manifest.v1+json' "https://registry-1.docker.io/v2/$1/manifests/$2" | tr -d '\r' | awk -F': ' 'tolower($1)=="docker-content-digest"{print $2}'
}
echo "checked at $(date -u +%FT%TZ)"
for c in "$@"; do
  echo; echo "== $(git log --format='%h %s' -1 "$c")"
  git diff "$c^" "$c" -U0 | rg '^[-+](\s*-?\s*uses:|FROM|.*image:)' || true
  git diff "$c^" "$c" -U0 | rg '^\+' | rg -o '[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+@[0-9a-f]{40} # v[0-9.]+' | sort -u | while read -r ref _ tag; do
    repo=${ref%@*}; sha=${ref#*@}; up=$(resolve_action "$repo" "$tag")
    echo "action $repo $tag: pinned $sha, tag resolves to $up: $([ "$sha" = "$up" ] && echo MATCH || echo MISMATCH)"
  done || true
  git diff "$c^" "$c" -U0 | rg '^\+' | rg -o '(ghcr\.io|docker\.io)/[a-z0-9./_-]+:[A-Za-z0-9._-]+@sha256:[0-9a-f]{64}' | sort -u | while read -r img; do
    name=${img%%@*}; digest=${img#*@}; host=${name%%/*}; rest=${name#*/}; repo=${rest%:*}; tag=${rest##*:}
    if [ "$host" = ghcr.io ]; then up=$(ghcr_digest "$repo" "$tag"); else up=$(dockerhub_digest "$repo" "$tag"); fi
    echo "image $host/$repo:$tag: pinned $digest, tag resolves to $up: $([ "$digest" = "$up" ] && echo MATCH || echo MISMATCH)"
  done || true
done
