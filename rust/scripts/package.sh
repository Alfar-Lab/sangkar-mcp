#!/usr/bin/env bash
# Build the one platform the first Rust release publishes and lay it out as the npm package the
# parent's `optionalDependencies` names. `.github/workflows/release.yml` runs this on `ubuntu-22.04`
# — the image decides the glibc floor — and the maintainer then publishes both packages by hand,
# this one first. `npm/<key>/` is committed as the layout template; its `bin/sangkar` is not.
set -euo pipefail

TARGET="x86_64-unknown-linux-gnu"
KEY="linux-x64-gnu"
NAME="sangkar-mcp-$KEY"

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$root/rust"

# One version string, read from the file npm publishes, so the two packages cannot disagree.
version="$(node -e 'process.stdout.write(require(process.argv[1]).version)' "$root/package.json")"
declared="$(node -e 'const p = require(process.argv[1]); process.stdout.write(String((p.optionalDependencies ?? {})[process.argv[2]]))' \
  "$root/package.json" "$NAME")"
if [ "$declared" != "$version" ]; then
  echo "package.json must pin $NAME at $version, not '${declared}'" >&2
  exit 1
fi

# `--locked`: a stale Cargo.lock fails here instead of being rewritten behind the release's back.
cargo build --release --locked -p sangkar-cli --target "$TARGET"

dest="$root/npm/$KEY"
mkdir -p "$dest/bin"
install -m 755 "target/$TARGET/release/sangkar" "$dest/bin/sangkar"
strip "$dest/bin/sangkar"

node -e '
const fs = require("node:fs");
const [parentPath, outPath, name, os, cpu] = process.argv.slice(1);
const parent = JSON.parse(fs.readFileSync(parentPath, "utf8"));
const manifest = {
  name,
  version: parent.version,
  os: [os],
  cpu: [cpu],
  files: ["bin"],
  license: parent.license,
  repository: parent.repository,
};
fs.writeFileSync(outPath, `${JSON.stringify(manifest, null, 2)}\n`);
' "$root/package.json" "$dest/package.json" "$NAME" linux x64

echo "$NAME@$version -> $dest/bin/sangkar ($(du -h "$dest/bin/sangkar" | cut -f1))"
