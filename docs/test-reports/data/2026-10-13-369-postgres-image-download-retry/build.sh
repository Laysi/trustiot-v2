#!/usr/bin/env bash
# usage: build.sh <tag> [extra docker build args...]  (runs on the build host through rcheck)
set -u
tag=$1; shift
nocache=--no-cache
[ "${KEEPCACHE:-}" = 1 ] && nocache=
start=$(date +%s)
docker build $nocache --progress=plain --platform linux/arm64 -f deploy/images/postgres/Containerfile "$@" -t "pg369:$tag" deploy/images/postgres > "/tmp/pg369-$tag.log" 2>&1
rc=$?
echo "=== $tag: docker build $* exit $rc after $(( $(date +%s) - start )) s"
grep -nE '^#[0-9]+ [0-9.]+ (downloading|curl:|age.tar|no usable|Apache AGE|sha512sum|server-dev)|DONE [0-9.]+s$|ERROR|did not complete' "/tmp/pg369-$tag.log" | grep -B1 -A1 -E 'downloading|curl:|no usable|Apache AGE|age.tar|sha512|ERROR' | cut -c1-300
if [ $rc -eq 0 ]; then
  docker image inspect "pg369:$tag" --format 'id={{.Id}}'
  docker image inspect "pg369:$tag" --format 'layers={{json .RootFS.Layers}}'
fi
exit 0
