#!/usr/bin/env bash
# Builds emayl_<version>_amd64.deb (Debian, Ubuntu, Mint, Pop!_OS ...).
#
#   packaging/linux/build-deb.sh <path to the emayl program> <version> <output folder>
#
# Run from anywhere. Needs only dpkg-deb, which every Debian-family system has.
set -euo pipefail

if [ "$#" -ne 3 ]; then
  echo "usage: $0 <emayl program> <version> <output folder>" >&2
  exit 2
fi
program=$(realpath "$1")
version=$2
out=$(realpath -m "$3")
here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
root=$(cd "$here/../.." && pwd)

if ! [[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "version must look like 1.2.3, got: $version" >&2
  exit 2
fi
[ -x "$program" ] || { echo "not an executable file: $program" >&2; exit 2; }

# The program is built on the machine running this script, so it needs at least
# that machine's C library.
libc=$(dpkg-query -W -f='${Version}' libc6 | sed -E 's/^[0-9]+://; s/-.*$//')

stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
pkg="$stage/pkg"

install -d "$pkg/DEBIAN" "$pkg/usr/bin" "$pkg/usr/share/applications" \
  "$pkg/usr/share/doc/emayl"
install -m 0755 "$program" "$pkg/usr/bin/emayl"
install -m 0644 "$here/emayl.desktop" "$pkg/usr/share/applications/emayl.desktop"
for size in 16 24 32 48 64 128 256 512; do
  install -D -m 0644 "$root/assets/icon/png/emayl-$size.png" \
    "$pkg/usr/share/icons/hicolor/${size}x${size}/apps/emayl.png"
done
install -d "$pkg/usr/share/man/man1"
gzip -9n -c "$here/emayl.1" > "$pkg/usr/share/man/man1/emayl.1.gz"
chmod 0644 "$pkg/usr/share/man/man1/emayl.1.gz"
install -m 0644 "$root/LICENSE" "$pkg/usr/share/doc/emayl/copyright"
install -m 0644 "$root/README.md" "$pkg/usr/share/doc/emayl/README.md"

# Debian requires a (changelog) file; a one-line one is enough for a first release.
printf 'emayl (%s) stable; urgency=medium\n\n  * Release %s.\n\n -- EMAYL contributors <noreply@users.noreply.github.com>  %s\n' \
  "$version" "$version" "$(date -R)" | gzip -9n > "$pkg/usr/share/doc/emayl/changelog.gz"
chmod 0644 "$pkg/usr/share/doc/emayl/changelog.gz"

# Checksums of every installed file (lets `debsums` detect damaged files).
(cd "$pkg" && find usr -type f -print0 | sort -z | xargs -0 md5sum > DEBIAN/md5sums)

size_kb=$(du -sk "$pkg/usr" | cut -f1)
cat > "$pkg/DEBIAN/control" <<CONTROL
Package: emayl
Version: $version
Section: mail
Priority: optional
Architecture: amd64
Maintainer: EMAYL contributors <noreply@users.noreply.github.com>
Installed-Size: $size_kb
Depends: libc6 (>= $libc), libxkbcommon0, libxkbcommon-x11-0, libx11-6, libxcursor1, libxi6, libxrandr2, libwayland-client0, libegl1
Recommends: gnome-keyring | kwalletd6 | kwalletd5
Description: fast, private email client for all your accounts
 EMAYL reads and sends mail for any number of accounts, through IMAP and
 SMTP (Gmail, Outlook and most other providers). It keeps remote
 content blocked by default and stores passwords in the desktop's password
 store.
CONTROL

mkdir -p "$out"
file="$out/emayl_${version}_amd64.deb"
dpkg-deb --root-owner-group -Zxz --build "$pkg" "$file" >/dev/null
echo "$file"
