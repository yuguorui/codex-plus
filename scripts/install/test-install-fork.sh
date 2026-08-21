#!/usr/bin/env bash
set -euo pipefail

: "${RELEASE_VERSION:?RELEASE_VERSION is required}"
release_version="${RELEASE_VERSION#rust-v}"
script_dir="$(cd -- "$(dirname -- "$0")" && pwd)"
installer="$script_dir/install-fork.sh"

case "$(uname -s)" in
  Linux) target="x86_64-unknown-linux-musl" ;;
  Darwin)
    if [ "$(uname -m)" = "arm64" ] || [ "$(uname -m)" = "aarch64" ]; then
      target="aarch64-apple-darwin"
    else
      echo "This installer test only supports Apple Silicon macOS and Linux x64." >&2
      exit 1
    fi
    ;;
  *)
    echo "This installer test only supports Apple Silicon macOS and Linux x64." >&2
    exit 1
    ;;
esac

root="$(mktemp -d)"
trap 'rm -rf "$root"' EXIT INT TERM

make_release() {
  release_dir="$1"
  version="$2"
  mkdir -p "$release_dir/bin" "$release_dir/codex-resources" "$release_dir/codex-path"
  cat > "$release_dir/bin/codex++" <<EOF
#!/bin/sh
printf 'codex-cli %s\\n' "$version"
EOF
  chmod +x "$release_dir/bin/codex++"
  : > "$release_dir/codex-package.json"
  : > "$release_dir/bin/codex-code-mode-host"
  : > "$release_dir/codex-path/rg"
  chmod +x "$release_dir/bin/codex-code-mode-host" "$release_dir/codex-path/rg"
  if [ "$target" = "x86_64-unknown-linux-musl" ]; then
    : > "$release_dir/codex-resources/bwrap"
    chmod +x "$release_dir/codex-resources/bwrap"
  fi
}

standalone_home="$root/standalone-home"
standalone_bin="$root/standalone-bin"
standalone_release="$standalone_home/packages/standalone/releases/$release_version-$target"
make_release "$standalone_release" "$release_version"
output="$(CODEX_HOME="$standalone_home" CODEX_INSTALL_DIR="$standalone_bin" \
  sh "$installer" "$release_version")"
grep -Fx "codex++ $release_version is already up to date." <<<"$output"
test -x "$standalone_home/packages/standalone/current/bin/codex++"
test -x "$standalone_bin/codex++"
test ! -e "$standalone_home/packages/standalone/auto-update-version"

download_home="$root/download-home"
download_bin="$root/download-bin"
package_src="$root/package-src"
archive="$root/releases/codex-package-$target.tar.gz"
make_release "$package_src" "$release_version"
mkdir -p "$root/releases"
tar -C "$package_src" -czf "$archive" .
if command -v sha256sum >/dev/null 2>&1; then
  (cd "$root/releases" && sha256sum "codex-package-$target.tar.gz" > codex-package_SHA256SUMS)
else
  (cd "$root/releases" && shasum -a 256 "codex-package-$target.tar.gz" > codex-package_SHA256SUMS)
fi
printf '{"tag_name":"rust-v%s"}
' "$release_version" > "$root/releases/latest.json"
output="$(CODEX_HOME="$download_home" CODEX_INSTALL_DIR="$download_bin" \
  CODEX_RELEASE_API_URL="file://$root/releases/latest.json" \
  CODEX_RELEASE_BASE_URL="file://$root/releases" \
  sh "$installer")"
grep -Fx "Installed codex++ $release_version for $target at $download_home/packages/standalone/releases/$release_version-$target" <<<"$output"
test -x "$download_home/packages/standalone/current/bin/codex++"
test "$(cat "$download_home/packages/standalone/auto-update-version")" = "$release_version-$target"
test -x "$download_bin/codex++"

previous_release_name="local-test-$target"
daemon_home="$root/daemon-home"
daemon_bin="$root/daemon-bin"
daemon_root="$daemon_home/packages/app-server-daemon"
previous_release="$daemon_root/releases/$previous_release_name"
target_release="$daemon_root/releases/$release_version-$target"
make_release "$previous_release" "previous"
make_release "$target_release" "$release_version"
ln -sfn "$previous_release" "$daemon_root/current"

output="$(CODEX_HOME="$daemon_home" CODEX_INSTALL_DIR="$daemon_bin" \
  CODEX_RELEASE=latest CODEX_RELEASE_VERSION_OVERRIDE="$release_version" \
  CODEX_INSTALL_DAEMON_ONLY=1 CODEX_INSTALL_IF_CURRENT=1 \
  CODEX_UPDATE_FROM_RELEASE="$previous_release_name" \
  sh "$installer")"
grep -Fx "Daemon package $release_version is already up to date." <<<"$output"
test "$(cd -P "$daemon_root/current" && pwd)" = "$(cd -P "$target_release" && pwd)"
test "$(cat "$daemon_root/auto-update-version")" = "$release_version-$target"
test ! -e "$daemon_bin/codex++"

output="$(CODEX_HOME="$daemon_home" CODEX_INSTALL_DIR="$daemon_bin" \
  CODEX_RELEASE=latest CODEX_RELEASE_VERSION_OVERRIDE="$release_version" \
  CODEX_INSTALL_DAEMON_ONLY=1 CODEX_INSTALL_IF_LATEST=1 \
  CODEX_UPDATE_FROM_RELEASE="$release_version-$target" \
  sh "$installer")"
grep -Fx "Daemon package $release_version is already up to date." <<<"$output"

ln -sfn "$previous_release" "$daemon_root/current"
if output="$(CODEX_HOME="$daemon_home" CODEX_INSTALL_DIR="$daemon_bin" \
  CODEX_RELEASE=latest CODEX_RELEASE_VERSION_OVERRIDE="$release_version" \
  CODEX_INSTALL_DAEMON_ONLY=1 CODEX_INSTALL_IF_CURRENT=1 \
  CODEX_UPDATE_FROM_RELEASE="local-other-$target" \
  sh "$installer" 2>&1)"; then
  echo "expected a changed daemon selection to fail the guarded update" >&2
  exit 1
fi
grep -F "Daemon selection changed; retry the update." <<<"$output"
test "$(cd -P "$daemon_root/current" && pwd)" = "$(cd -P "$previous_release" && pwd)"

ln -sfn "$previous_release" "$daemon_root/current"
if output="$(CODEX_HOME="$daemon_home" CODEX_INSTALL_DIR="$daemon_bin" \
  CODEX_INSTALL_DAEMON_ONLY=1 CODEX_INSTALL_IF_CURRENT=1 \
  CODEX_UPDATE_FROM_RELEASE="$previous_release_name" \
  sh "$installer" "$release_version" 2>&1)"; then
  echo "expected guarded updates to reject a pinned version" >&2
  exit 1
fi
grep -F "Guarded updates require the latest release" <<<"$output"

echo "fork installer tests passed"
