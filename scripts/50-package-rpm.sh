#!/usr/bin/env bash
# Package the built tree (scripts/30-build.sh) as a binary RPM in dist/.
# Install it with `sudo dnf install dist/cmux-lin-*.rpm`.
source "$(dirname "$0")/common.sh"

command -v rpmbuild >/dev/null || die "rpmbuild not found (sudo dnf install rpm-build)"
for f in build/cmux/cmux-tui/target/release/cmux-tui browser-provider/target/release/cmux-browser-provider \
         gui/cmux-gtk/target/release/cmux-gtk; do
  [ -x "$REPO_ROOT/$f" ] || die "$f isn't built; run scripts/30-build.sh"
done

WORK="$(mktemp -d)"
trap 'rm -rf -- "$WORK"' EXIT
python3 "$REPO_ROOT/scripts/gen-completions.py" "$REPO_ROOT/build/cmux/cmux-tui/target/release/cmux-tui" "$WORK/completions" >/dev/null \
  || die "couldn't generate shell completions"

VERSION="0.1.0"
RELEASE="$(date -u +%Y%m%d).git$(git -C "$REPO_ROOT" rev-parse --short HEAD)"
rpmbuild -bb "$REPO_ROOT/packaging/cmux-lin.spec" \
  --define "_topdir $WORK/rpm" \
  --define "repo $REPO_ROOT" \
  --define "completions $WORK/completions" \
  --define "version_ $VERSION" \
  --define "release_ $RELEASE" >"$WORK/rpmbuild.log" 2>&1 \
  || { tail -20 "$WORK/rpmbuild.log"; die "rpmbuild failed"; }

mkdir -p "$REPO_ROOT/dist"
find "$WORK/rpm/RPMS" -name '*.rpm' -exec cp {} "$REPO_ROOT/dist/" \;
log "built: $(ls "$REPO_ROOT"/dist/cmux-lin-*.rpm | tail -1)"
