#!/bin/sh
# Install the sangkar release binary: download the release tarball, check it against the release's
# SHA256SUMS, and put one binary in ~/.local/bin. This is the only installation path.
#
#   curl -fsSL https://raw.githubusercontent.com/Alfar-Lab/sangkar-mcp/main/install.sh | sh
#
# Test hooks, not part of the interface: SANGKAR_API_URL replaces the release list URL and
# SANGKAR_DOWNLOAD_BASE replaces the ".../releases/download/v<version>" prefix. Each is joined with
# the same version and file names the default is, so a local fixture can serve a fake release.
set -eu

api_url_default="https://api.github.com/repos/Alfar-Lab/sangkar-mcp/releases?per_page=1"
download_base_default="https://github.com/Alfar-Lab/sangkar-mcp/releases/download"
arch_name="x86_64-unknown-linux-gnu"
min_glibc="2.35"

version=""
bin_dir=""
dry_run=0
uninstall=0

usage() {
  cat <<'EOF'
usage: install.sh [options]

  --version V     release to install, with or without the leading v (default: the newest)
  --bin-dir DIR   directory for the binary (default: $HOME/.local/bin)
  --uninstall     remove DIR/sangkar, if it is there
  --dry-run       print the version, the URL and the destination, then stop
  -h, --help      this text

Only one platform is built: Linux x86_64 with glibc 2.35 or newer.
EOF
}

die() {
  printf 'install.sh: %s\n' "$1" >&2
  exit 1
}

while [ $# -gt 0 ]; do
  case "$1" in
    --version) [ $# -ge 2 ] || die "--version needs a value (try --help)"; version="${2#v}"; shift 2 ;;
    --bin-dir) [ $# -ge 2 ] || die "--bin-dir needs a value (try --help)"; bin_dir="$2"; shift 2 ;;
    --uninstall) uninstall=1; shift ;;
    --dry-run) dry_run=1; shift ;;
    -h | --help) usage; exit 0 ;;
    *) die "unknown argument: $1 (try --help)" ;;
  esac
done

if [ -z "$bin_dir" ]; then
  [ -n "${HOME:-}" ] || die "HOME is not set"
  bin_dir="$HOME/.local/bin"
fi
# A trailing slash would break both the PATH comparison below and the line printed for it.
while [ "$bin_dir" != / ] && [ "${bin_dir%/}" != "$bin_dir" ]; do
  bin_dir=${bin_dir%/}
done
dest="$bin_dir/sangkar"

if [ "$uninstall" = 1 ]; then
  if [ -e "$dest" ] || [ -L "$dest" ]; then
    if [ "$dry_run" = 1 ]; then
      printf 'would remove %s\n' "$dest"
    else
      rm -f "$dest" || die "cannot remove $dest"
      printf 'removed %s\n' "$dest"
    fi
  else
    printf 'install.sh: nothing at %s\n' "$dest"
  fi
  exit 0
fi

# One supported platform, checked before any network call and before anything is written. `uname`
# and `getconf` are called unqualified so a PATH shim can exercise these refusals.
supported="this build supports Linux x86_64 with glibc 2.35 or newer only"
kernel=$(uname -s 2>/dev/null || true)
if [ "$kernel" != Linux ]; then
  die "uname -s says $kernel, not Linux; $supported"
fi
machine=$(uname -m 2>/dev/null || true)
case "$machine" in
  x86_64 | amd64) ;;
  *) die "uname -m says $machine, not x86_64; $supported" ;;
esac
# getconf prints "glibc 2.35"; musl and other libcs have no GNU_LIBC_VERSION and print nothing.
libc=$(getconf GNU_LIBC_VERSION 2>/dev/null || true)
case "$libc" in
  "glibc "*) ;;
  *) die "no glibc was detected (musl and other libcs are not built); $supported" ;;
esac
libc_version=${libc#glibc }
oldest=$(printf '%s\n%s\n' "$min_glibc" "$libc_version" | sort -V | head -n 1)
if [ "$oldest" != "$min_glibc" ]; then
  die "glibc $libc_version is older than $min_glibc; $supported"
fi

if command -v curl >/dev/null 2>&1; then
  get() { curl -fsSL "$1" 2>/dev/null; }
  get_to() { curl -fsSL "$1" -o "$2" 2>/dev/null; }
elif command -v wget >/dev/null 2>&1; then
  get() { wget -qO- "$1"; }
  get_to() { wget -q -O "$2" "$1"; }
else
  die "neither curl nor wget is installed; install curl and rerun"
fi

if [ -z "$version" ]; then
  api_url="${SANGKAR_API_URL:-$api_url_default}"
  # The list endpoint, never releases/latest: a release marked pre-release is skipped by latest,
  # and every release so far is a pre-release. One entry is the newest by publish time.
  if ! releases=$(get "$api_url"); then
    die "cannot read the release list from $api_url (HTTP error or no network); pick one with --version"
  fi
  if [ "$(printf '%s' "$releases" | tr -d '[:space:]')" = "[]" ]; then
    die "the release list at $api_url is empty; there is nothing to install"
  fi
  tag=$(printf '%s\n' "$releases" | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -n 1)
  [ -n "$tag" ] || die "no tag_name in the release list from $api_url; pick one with --version"
  version=${tag#v}
  [ -n "$version" ] || die "the newest release has an empty tag; pick one with --version"
fi

download_base="${SANGKAR_DOWNLOAD_BASE:-$download_base_default/v$version}"
tarball="sangkar-$version-$arch_name.tar.gz"
tarball_url="$download_base/$tarball"
sums_url="$download_base/SHA256SUMS"

if [ "$dry_run" = 1 ]; then
  printf 'version: %s\n' "$version"
  printf 'url: %s\n' "$tarball_url"
  printf 'dest: %s\n' "$dest"
  exit 0
fi

command -v tar >/dev/null 2>&1 || die "tar is not installed; install it and rerun"
if command -v sha256sum >/dev/null 2>&1; then
  digest_of() { sha256sum "$1" | cut -d' ' -f1; }
elif command -v shasum >/dev/null 2>&1; then
  digest_of() { shasum -a 256 "$1" | cut -d' ' -f1; }
else
  die "neither sha256sum nor shasum is installed; install one and rerun"
fi

# The digest of the line whose file name is exactly $2, or nothing. `sha256sum -c` is not used: it
# reads every line, so a second platform in the same file would fail this install.
checksum_for() {
  while IFS= read -r line || [ -n "$line" ]; do
    name=$(printf '%s\n' "$line" | sed -n 's/^[0-9a-fA-F]\{64\}  //p')
    if [ -n "$name" ] && [ "$name" = "$2" ]; then
      printf '%s\n' "$line" | cut -c1-64
      return 0
    fi
  done < "$1"
  return 0
}

tmp=$(mktemp -d) || die "cannot create a temporary directory"
trap 'rm -rf "$tmp"' EXIT

get_to "$tarball_url" "$tmp/$tarball" || die "cannot download $tarball_url; nothing was installed"
get_to "$sums_url" "$tmp/SHA256SUMS" || die "cannot download $sums_url; nothing was installed"

expected=$(checksum_for "$tmp/SHA256SUMS" "$tarball")
[ -n "$expected" ] || die "SHA256SUMS from $download_base has no line for $tarball; nothing was installed"
actual=$(digest_of "$tmp/$tarball") || die "cannot hash $tmp/$tarball; nothing was installed"
[ "$expected" = "$actual" ] || die "checksum mismatch for $tarball: SHA256SUMS says $expected, the download is $actual; nothing was installed"

tar -xzf "$tmp/$tarball" -C "$tmp" sangkar 2>/dev/null || die "cannot extract sangkar from $tarball; nothing was installed"
mkdir -p "$bin_dir" || die "cannot create $bin_dir"
install -m 755 "$tmp/sangkar" "$dest" || die "cannot install $dest"

printf 'installed %s (sangkar %s)\n' "$dest" "$version"
printf "next: run 'sangkar index' in a repository to build its graph, or 'sangkar status' to see what\n"
printf 'is indexed already.\n'

case ":${PATH:-}:" in
  *":$bin_dir:"*) ;;
  *)
    printf '%s is not on PATH; add it with:\n' "$bin_dir" >&2
    printf 'export PATH="%s:$PATH"\n' "$bin_dir" >&2
    ;;
esac
