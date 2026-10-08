#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
# scripts/install.sh against a fake local release: installs and verifies,
# passes setup its arguments, refuses a bad checksum, honors POLIS_NO_SETUP.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
case "$(uname -s)-$(uname -m)" in
  Darwin-*) if [ "$(uname -m)" = x86_64 ] && [ "$(sysctl -n hw.optional.arm64 2>/dev/null || echo 0)" != 1 ]; then t=x86_64-apple-darwin; else t=aarch64-apple-darwin; fi ;;
  Linux-x86_64) t=x86_64-unknown-linux-gnu ;;
  Linux-aarch64) t=aarch64-unknown-linux-gnu ;;
  *) echo "skip: unsupported platform"; exit 0 ;;
esac
mkdir -p "$work/src/polis-memory-$t" "$work/rel"
cat > "$work/src/polis-memory-$t/polis" <<'FAKE'
#!/bin/sh
[ "$1" = --version ] && { echo "polis 9.9.9"; exit 0; }
echo "setup-args: $*" > "$(dirname "$0")/ran"
FAKE
chmod 755 "$work/src/polis-memory-$t/polis"
tar -cJf "$work/rel/polis-memory-$t.tar.xz" -C "$work/src" "polis-memory-$t"
sum() { if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1"; else shasum -a 256 "$1"; fi | awk '{print $1}'; }
printf '%s *polis-memory-%s.tar.xz\n' "$(sum "$work/rel/polis-memory-$t.tar.xz")" "$t" > "$work/rel/polis-memory-$t.tar.xz.sha256"

POLIS_INSTALL_BASE="file://$work/rel" POLIS_BIN_DIR="$work/bin" sh "$here/install.sh" --yes --clients claude 2>"$work/log"
[ -x "$work/bin/polis" ] || { cat "$work/log"; echo "FAIL: not installed"; exit 1; }
grep -q "setup-args: setup --polis $work/bin/polis --yes --clients claude" "$work/bin/ran" || { cat "$work/bin/ran"; echo "FAIL: setup args"; exit 1; }
grep -q "polis 9.9.9" "$work/log" || { cat "$work/log"; echo "FAIL: version line"; exit 1; }

rm -rf "${work:?}/bin"
POLIS_NO_SETUP=1 POLIS_INSTALL_BASE="file://$work/rel" POLIS_BIN_DIR="$work/bin" sh "$here/install.sh" 2>/dev/null
[ -x "$work/bin/polis" ] && [ ! -e "$work/bin/ran" ] || { echo "FAIL: POLIS_NO_SETUP"; exit 1; }

rm -rf "${work:?}/bin"
printf '%064d *x\n' 0 > "$work/rel/polis-memory-$t.tar.xz.sha256"
if POLIS_INSTALL_BASE="file://$work/rel" POLIS_BIN_DIR="$work/bin" sh "$here/install.sh" 2>"$work/log"; then echo "FAIL: accepted a bad checksum"; exit 1; fi
grep -q "checksum mismatch" "$work/log" && [ ! -e "$work/bin/polis" ] || { cat "$work/log"; echo "FAIL: bad checksum handling"; exit 1; }
echo "install.sh: ok"
