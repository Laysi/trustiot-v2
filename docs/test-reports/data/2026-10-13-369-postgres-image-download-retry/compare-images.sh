#!/usr/bin/env bash
# compare the filesystems of two images: contents and modes, then listing with mtimes
set -u
w=$(mktemp -d ~/cmp369.XXXX)
for t in before after; do
  cid=$(docker create --platform linux/arm64 "pg369:$t" true)
  mkdir "$w/$t"
  docker export "$cid" | tar -x -C "$w/$t" --no-same-owner 2>/dev/null
  docker export "$cid" | tar -tvf - | sort -k6 > "$w/$t.list"
  docker rm "$cid" > /dev/null
  (cd "$w/$t" && find . -type f -print0 | sort -z | xargs -0 sha256sum | sha256sum) > "$w/$t.content"
  (cd "$w/$t" && find . -printf '%m %y %p %l\n' | sort | sha256sum) > "$w/$t.modes"
  docker image inspect "pg369:$t" --format 'config-without-time: {{json .Config}}' | sha256sum > "$w/$t.config"
  docker image inspect "pg369:$t" --format '{{.Architecture}} {{.Os}} {{.Config.User}} {{json .Config.Entrypoint}} {{json .Config.Cmd}} {{json .Config.Env}}' | sha256sum > "$w/$t.cfg2"
done
for k in content modes config cfg2; do echo "$k: before=$(cut -c1-16 $w/before.$k) after=$(cut -c1-16 $w/after.$k)"; done
echo "--- listing differences, mtime column removed (empty = identical)"
diff <(awk '{ $4=""; $5=""; print }' "$w/before.list") <(awk '{ $4=""; $5=""; print }' "$w/after.list") | head -20
echo "--- listing lines that differ only by mtime"
diff "$w/before.list" "$w/after.list" | grep -c '^[<>]'
diff "$w/before.list" "$w/after.list" | grep '^[<>]' | head -12
rm -rf "$w"
