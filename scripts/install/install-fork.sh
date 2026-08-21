#!/usr/bin/env sh
set -eu

REPO="${CODEX_RELEASE_REPOSITORY:-__RELEASE_REPOSITORY__}"
RELEASE="${1:-${CODEX_RELEASE:-latest}}"
BIN_DIR="${CODEX_INSTALL_DIR:-$HOME/.local/bin}"
BIN_NAME="${CODEX_BIN_NAME:-codex++}"
PACKAGE_BIN_NAME="codex++"
CODEX_HOME_DIR="${CODEX_HOME:-$HOME/.codex}"
DAEMON_ONLY="${CODEX_INSTALL_DAEMON_ONLY:-0}"
if [ "$DAEMON_ONLY" = "1" ]; then
  PACKAGE_ROOT="$CODEX_HOME_DIR/packages/app-server-daemon"
else
  PACKAGE_ROOT="$CODEX_HOME_DIR/packages/standalone"
fi
RELEASES_DIR="$PACKAGE_ROOT/releases"
CURRENT_LINK="$PACKAGE_ROOT/current"
AUTO_UPDATE_VERSION="$PACKAGE_ROOT/auto-update-version"
LOCK_FILE="$PACKAGE_ROOT/install.lock"
LOCK_DIR="$PACKAGE_ROOT/install.lock.d"
LOCK_STALE_AFTER_SECS=600
lock_kind=""
tmp_dir=""
staging=""

usage() {
  cat <<'EOF'
Usage: install-fork.sh [VERSION]

VERSION may be "latest", "rust-vyyyymmddhhmm", or "yyyymmddhhmm".
Set CODEX_RELEASE_REPOSITORY=owner/repo to override the release repository.
Set CODEX_BIN_NAME=name to choose the command name installed into CODEX_INSTALL_DIR.
EOF
}

if [ "${1:-}" = "--help" ] || [ "${1:-}" = "-h" ]; then
  usage
  exit 0
fi

download_file() {
  url="$1"
  output="$2"
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL "$url" -o "$output"
  elif command -v wget >/dev/null 2>&1; then
    wget -q -O "$output" "$url"
  else
    echo "curl or wget is required." >&2
    exit 1
  fi
}

download_text() {
  url="$1"
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL "$url"
  elif command -v wget >/dev/null 2>&1; then
    wget -q -O - "$url"
  else
    echo "curl or wget is required." >&2
    exit 1
  fi
}

normalize_version() {
  case "$1" in
    "" | latest) printf 'latest\n' ;;
    rust-v*) printf '%s\n' "${1#rust-v}" ;;
    v*) printf '%s\n' "${1#v}" ;;
    *) printf '%s\n' "$1" ;;
  esac
}

warn() {
  printf 'warning: %s\n' "$*" >&2
}

mkdir_lock_is_stale() {
  [ -d "$LOCK_DIR" ] || return 1

  pid="$(cat "$LOCK_DIR/pid" 2>/dev/null || true)"
  started_at="$(cat "$LOCK_DIR/started_at" 2>/dev/null || true)"
  now="$(date +%s 2>/dev/null || printf '0')"

  case "$started_at" in
    '' | *[!0-9]*) started_at=0 ;;
  esac

  if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
    return 1
  fi

  if [ "$started_at" -eq 0 ] || [ "$now" -eq 0 ]; then
    return 0
  fi

  [ $((now - started_at)) -ge "$LOCK_STALE_AFTER_SECS" ]
}

acquire_install_lock() {
  mkdir -p "$PACKAGE_ROOT"

  if [ "$(uname -s)" = "Darwin" ] && command -v lockf >/dev/null 2>&1; then
    : >>"$LOCK_FILE"
    exec 9<>"$LOCK_FILE"
    lockf 9
    lock_kind="lockf"
    return
  fi

  if command -v flock >/dev/null 2>&1; then
    exec 9>"$LOCK_FILE"
    flock 9
    lock_kind="flock"
    return
  fi

  while ! mkdir "$LOCK_DIR" 2>/dev/null; do
    if mkdir_lock_is_stale; then
      warn "Removing stale installer lock at $LOCK_DIR"
      rm -rf "$LOCK_DIR"
      continue
    fi
    sleep 1
  done

  printf '%s\n' "$$" >"$LOCK_DIR/pid"
  date +%s >"$LOCK_DIR/started_at" 2>/dev/null || true
  lock_kind="mkdir"
}

release_install_lock() {
  if [ "$lock_kind" = "mkdir" ]; then
    rm -rf "$LOCK_DIR" 2>/dev/null || true
  elif [ "$lock_kind" = "flock" ] || [ "$lock_kind" = "lockf" ]; then
    exec 9>&- 2>/dev/null || true
  fi
  lock_kind=""
}

cleanup() {
  [ -n "$tmp_dir" ] && rm -rf "$tmp_dir"
  [ -n "$staging" ] && rm -rf "$staging"
  release_install_lock
}

release_request="$(normalize_version "$RELEASE")"
if [ "$release_request" = "latest" ]; then
  is_latest=1
else
  is_latest=0
fi

check_update_guard() {
  if [ "${CODEX_INSTALL_IF_LATEST:-0}" != "1" ] &&
    [ "${CODEX_INSTALL_IF_CURRENT:-0}" != "1" ]; then
    return
  fi

  guarded_release="${CODEX_UPDATE_FROM_RELEASE:-}"
  if [ "$is_latest" != "1" ] || [ -z "$guarded_release" ]; then
    if [ "${CODEX_INSTALL_IF_CURRENT:-0}" = "1" ]; then
      echo "Guarded updates require the latest release and CODEX_UPDATE_FROM_RELEASE." >&2
      exit 1
    fi
    exit 0
  fi

  current_release_dir="$(cd -P "$CURRENT_LINK" 2>/dev/null && pwd)" || current_release_dir=""
  releases_dir="$(cd -P "$RELEASES_DIR" 2>/dev/null && pwd)" || releases_dir=""
  if [ -z "$current_release_dir" ] || [ -z "$releases_dir" ] ||
    [ "$current_release_dir" != "$releases_dir/$guarded_release" ]; then
    if [ "${CODEX_INSTALL_IF_CURRENT:-0}" = "1" ]; then
      echo "Daemon selection changed; retry the update." >&2
      exit 1
    fi
    exit 0
  fi

  if [ "${CODEX_INSTALL_IF_CURRENT:-0}" != "1" ] &&
    [ "$(cat "$AUTO_UPDATE_VERSION" 2>/dev/null || true)" != "$guarded_release" ]; then
    exit 0
  fi
}

resolve_version() {
  normalized="$(normalize_version "$RELEASE")"
  if [ "$normalized" != "latest" ]; then
    printf '%s\n' "$normalized"
    return
  fi
  if [ -n "${CODEX_RELEASE_VERSION_OVERRIDE:-}" ]; then
    printf '%s\n' "$CODEX_RELEASE_VERSION_OVERRIDE"
    return
  fi
  release_json="$(download_text "${CODEX_RELEASE_API_URL:-https://api.github.com/repos/$REPO/releases/latest}")"
  resolved="$(printf '%s\n' "$release_json" | sed -n 's/.*"tag_name":[[:space:]]*"rust-v\([^"]*\)".*/\1/p' | head -n 1)"
  if [ -z "$resolved" ]; then
    echo "Failed to resolve latest release for $REPO." >&2
    exit 1
  fi
  printf '%s\n' "$resolved"
}

installed_version() {
  installed_bin="$1"
  if [ ! -x "$installed_bin" ]; then
    return
  fi
  "$installed_bin" --version 2>/dev/null |
    sed -n 's/.* \([0-9][0-9A-Za-z.+-]*\)$/\1/p' |
    head -n 1
}

release_install_is_complete() {
  candidate_dir="$1"
  [ -d "$candidate_dir" ] &&
    [ -f "$candidate_dir/codex-package.json" ] &&
    [ -x "$candidate_dir/bin/$PACKAGE_BIN_NAME" ] &&
    [ -x "$candidate_dir/bin/codex-code-mode-host" ] &&
    [ -x "$candidate_dir/codex-path/rg" ] ||
    return 1

  if [ "$target" = "x86_64-unknown-linux-musl" ]; then
    [ -x "$candidate_dir/codex-resources/bwrap" ] || return 1
  fi
}

replace_path_with_symlink() {
  link_path="$1"
  link_target="$2"
  tmp_link="$3"

  rm -f "$tmp_link"
  ln -s "$link_target" "$tmp_link"

  if mv -Tf "$tmp_link" "$link_path" 2>/dev/null; then
    return
  fi
  if mv -hf "$tmp_link" "$link_path" 2>/dev/null; then
    return
  fi
  rm -f "$link_path"
  mv -f "$tmp_link" "$link_path"
}

activate_release() {
  selected_dir="$1"
  release_name="$(basename "$selected_dir")"
  mkdir -p "$RELEASES_DIR"
  replace_path_with_symlink "$CURRENT_LINK" "$selected_dir" "$PACKAGE_ROOT/.current.$$"
  if [ "$DAEMON_ONLY" != "1" ]; then
    mkdir -p "$BIN_DIR"
    replace_path_with_symlink \
      "$BIN_DIR/$BIN_NAME" "$selected_dir/bin/$PACKAGE_BIN_NAME" "$BIN_DIR/.codex.$$"
    replace_path_with_symlink \
      "$BIN_DIR/codex-code-mode-host" "$selected_dir/bin/codex-code-mode-host" "$BIN_DIR/.codex-host.$$"
  fi
  if [ "$is_latest" = "1" ]; then
    marker="$AUTO_UPDATE_VERSION.tmp.$$"
    printf '%s' "$release_name" > "$marker"
    mv -f "$marker" "$AUTO_UPDATE_VERSION"
  else
    rm -f "$AUTO_UPDATE_VERSION"
  fi
}

file_sha256() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  else
    shasum -a 256 "$1" | awk '{print $1}'
  fi
}

cleanup_stale_install_artifacts() {
  mkdir -p "$RELEASES_DIR"
  find "$RELEASES_DIR" -mindepth 1 -maxdepth 1 -name '.staging.*' -exec rm -rf {} +
  find "$PACKAGE_ROOT" -mindepth 1 -maxdepth 1 -name '.current.*' -exec rm -f {} +
  if [ "$DAEMON_ONLY" != "1" ] && [ -d "$BIN_DIR" ]; then
    find "$BIN_DIR" -mindepth 1 -maxdepth 1 \
      \( -name '.codex.*' -o -name '.codex-host.*' \) -exec rm -f {} +
  fi
}

trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
acquire_install_lock
cleanup_stale_install_artifacts
check_update_guard

case "$(uname -s)" in
  Linux) target="x86_64-unknown-linux-musl" ;;
  Darwin)
    if [ "$(uname -m)" = "arm64" ] || [ "$(uname -m)" = "aarch64" ]; then
      target="aarch64-apple-darwin"
    else
      echo "This fork release only supports Apple Silicon macOS." >&2
      exit 1
    fi
    ;;
  *)
    echo "This fork release only supports Linux x64 and Apple Silicon macOS." >&2
    exit 1
    ;;
esac

if [ "$target" = "x86_64-unknown-linux-musl" ] && [ "$(uname -m)" != "x86_64" ] && [ "$(uname -m)" != "amd64" ]; then
  echo "This fork release only supports Linux x64." >&2
  exit 1
fi

version="$(resolve_version)"
tag="rust-v$version"
asset="codex-package-$target.tar.gz"
base_url="${CODEX_RELEASE_BASE_URL:-https://github.com/$REPO/releases/download/$tag}"
release_dir="$RELEASES_DIR/$version-$target"

current_version="$(installed_version "$release_dir/bin/$PACKAGE_BIN_NAME")"
if [ "$current_version" = "$version" ] && release_install_is_complete "$release_dir"; then
  activate_release "$release_dir"
  if [ "$DAEMON_ONLY" = "1" ]; then
    echo "Daemon package $version is already up to date."
  else
    echo "$BIN_NAME $version is already up to date."
  fi
  exit 0
fi

tmp_dir="$(mktemp -d)"

mkdir -p "$RELEASES_DIR"
download_file "$base_url/codex-package_SHA256SUMS" "$tmp_dir/codex-package_SHA256SUMS"
expected="$(awk -v asset="$asset" '$2 == asset { print $1; found = 1 } END { if (!found) exit 1 }' "$tmp_dir/codex-package_SHA256SUMS")"
download_file "$base_url/$asset" "$tmp_dir/$asset"
actual="$(file_sha256 "$tmp_dir/$asset")"
if [ "$actual" != "$expected" ]; then
  echo "Checksum mismatch for $asset." >&2
  echo "expected: $expected" >&2
  echo "actual:   $actual" >&2
  exit 1
fi

staging="$RELEASES_DIR/.staging.$$"
mkdir -p "$staging"
tar -C "$staging" -xzf "$tmp_dir/$asset"
if ! release_install_is_complete "$staging"; then
  echo "Installed package is incomplete." >&2
  exit 1
fi
if [ -e "$release_dir" ]; then
  rm -rf "$release_dir"
fi
mv "$staging" "$release_dir"
staging=""

activate_release "$release_dir"
if [ "$DAEMON_ONLY" = "1" ]; then
  echo "Installed daemon package $version for $target at $release_dir"
else
  echo "Installed $BIN_NAME $version for $target at $release_dir"
  echo "Ensure $BIN_DIR is on your PATH."
fi
echo "Restart any running Codex++ processes to use this version."
