#!/bin/sh
# Verify version synchronization across Cargo.toml, member crates, Cargo.lock,
# packaging/homebrew/mimikyu.rb, and CHANGELOG.md.
# Compatible with POSIX /bin/sh and macOS Bash 3.2+.

set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)
EXPECTED_TAG=""

print_help() {
  cat <<'EOF'
Usage: scripts/check-version-sync.sh [OPTIONS]

Options:
    --tag <vX.Y.Z|X.Y.Z>   Verify workspace version and stamped CHANGELOG.md entry match release tag
    --root <DIR>           Repository root directory to inspect [default: parent of scripts/]
    -h, --help             Show this help message and exit
EOF
}

die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

while [ $# -gt 0 ]; do
  case "$1" in
    -h|--help)
      print_help
      exit 0
      ;;
    --tag)
      [ $# -ge 2 ] || die "--tag requires a version tag (e.g. v0.1.0)"
      EXPECTED_TAG="$2"
      shift 2
      ;;
    --tag=*)
      EXPECTED_TAG="${1#*=}"
      shift
      ;;
    --root)
      [ $# -ge 2 ] || die "--root requires a directory path"
      REPO_ROOT="$2"
      shift 2
      ;;
    --root=*)
      REPO_ROOT="${1#*=}"
      shift
      ;;
    *)
      die "unknown option '$1' (run with --help for usage)"
      ;;
  esac
done

ROOT_CARGO="$REPO_ROOT/Cargo.toml"
CORE_CARGO="$REPO_ROOT/crates/mimikyu-core/Cargo.toml"
CLI_CARGO="$REPO_ROOT/crates/mimikyu-cli/Cargo.toml"
CARGO_LOCK="$REPO_ROOT/Cargo.lock"
CHANGELOG="$REPO_ROOT/CHANGELOG.md"
BREW_FORMULA="$REPO_ROOT/packaging/homebrew/mimikyu.rb"

[ -f "$ROOT_CARGO" ] || die "missing $ROOT_CARGO"
[ -f "$CORE_CARGO" ] || die "missing $CORE_CARGO"
[ -f "$CLI_CARGO" ] || die "missing $CLI_CARGO"
[ -f "$CARGO_LOCK" ] || die "missing $CARGO_LOCK"
[ -f "$CHANGELOG" ] || die "missing $CHANGELOG"

# 1. Extract [workspace.package] version from root Cargo.toml
WORKSPACE_VERSION=$(awk '
  /^\[workspace\.package\][[:space:]]*$/ { in_ws_pkg = 1; next }
  /^\[/ { in_ws_pkg = 0 }
  in_ws_pkg && $0 ~ /^[[:space:]]*version[[:space:]]*=[[:space:]]*"/ {
    line = $0
    sub(/^[[:space:]]*version[[:space:]]*=[[:space:]]*"/, "", line)
    sub(/".*$/, "", line)
    print line
    exit 0
  }
' "$ROOT_CARGO")

[ -n "$WORKSPACE_VERSION" ] || die "could not parse [workspace.package] version from $ROOT_CARGO"

if ! printf '%s\n' "$WORKSPACE_VERSION" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$'; then
  die "workspace version '$WORKSPACE_VERSION' is not valid SemVer (MAJOR.MINOR.PATCH)"
fi
printf '[ok] Cargo.toml [workspace.package] version = %s\n' "$WORKSPACE_VERSION"

WS_CORE_DEP_VERSION=$(awk '
  /^\[workspace\.dependencies\][[:space:]]*$/ { in_ws_deps = 1; next }
  /^\[/ { in_ws_deps = 0 }
  in_ws_deps && $0 ~ /^[[:space:]]*mimikyu-core[[:space:]]*=.*version[[:space:]]*=[[:space:]]*"/ {
    line = $0
    sub(/^.*version[[:space:]]*=[[:space:]]*"/, "", line)
    sub(/".*$/, "", line)
    print line
    exit 0
  }
' "$ROOT_CARGO")

if [ -n "$WS_CORE_DEP_VERSION" ]; then
  [ "$WS_CORE_DEP_VERSION" = "$WORKSPACE_VERSION" ] || die "[workspace.dependencies] mimikyu-core version '$WS_CORE_DEP_VERSION' does not match workspace version '$WORKSPACE_VERSION'"
  printf '[ok] Cargo.toml [workspace.dependencies] mimikyu-core = %s\n' "$WS_CORE_DEP_VERSION"
fi

# 2. Verify member crates inherit workspace version (or match explicitly)
check_member_manifest() {
  manifest_path="$1"
  crate_label="$2"
  inherits_ws=$(awk '
    /^\[package\][[:space:]]*$/ { in_pkg = 1; next }
    /^\[/ { in_pkg = 0 }
    in_pkg && $0 ~ /^[[:space:]]*version\.workspace[[:space:]]*=[[:space:]]*true/ {
      print "yes"
      exit 0
    }
  ' "$manifest_path")

  if [ "$inherits_ws" = "yes" ]; then
    printf '[ok] %s inherits version.workspace = true\n' "$crate_label"
    return 0
  fi

  explicit_ver=$(awk '
    /^\[package\][[:space:]]*$/ { in_pkg = 1; next }
    /^\[/ { in_pkg = 0 }
    in_pkg && $0 ~ /^[[:space:]]*version[[:space:]]*=[[:space:]]*"/ {
      line = $0
      sub(/^[[:space:]]*version[[:space:]]*=[[:space:]]*"/, "", line)
      sub(/".*$/, "", line)
      print line
      exit 0
    }
  ' "$manifest_path")

  [ "$explicit_ver" = "$WORKSPACE_VERSION" ] || die "$crate_label version '$explicit_ver' does not match workspace version '$WORKSPACE_VERSION'"
  printf '[ok] %s version = %s\n' "$crate_label" "$explicit_ver"
}

check_member_manifest "$CORE_CARGO" "crates/mimikyu-core/Cargo.toml"
check_member_manifest "$CLI_CARGO" "crates/mimikyu-cli/Cargo.toml"

# 3. Verify Cargo.lock is synchronized for workspace crates
check_lockfile_pkg() {
  pkg_name="$1"
  lock_ver=$(awk -v target="$pkg_name" '
    /^\[\[package\]\][[:space:]]*$/ { name = ""; ver = ""; next }
    $0 ~ /^[[:space:]]*name[[:space:]]*=[[:space:]]*"/ {
      line = $0
      sub(/^[[:space:]]*name[[:space:]]*=[[:space:]]*"/, "", line)
      sub(/".*$/, "", line)
      name = line
    }
    $0 ~ /^[[:space:]]*version[[:space:]]*=[[:space:]]*"/ {
      line = $0
      sub(/^[[:space:]]*version[[:space:]]*=[[:space:]]*"/, "", line)
      sub(/".*$/, "", line)
      ver = line
      if (name == target) {
        print ver
        exit 0
      }
    }
  ' "$CARGO_LOCK")

  [ -n "$lock_ver" ] || die "package '$pkg_name' not found in $CARGO_LOCK"
  if [ "$lock_ver" != "$WORKSPACE_VERSION" ]; then
    die "Cargo.lock version for '$pkg_name' is '$lock_ver', expected '$WORKSPACE_VERSION' (run 'cargo check --workspace' to sync Cargo.lock)"
  fi
  printf '[ok] Cargo.lock %s = %s\n' "$pkg_name" "$lock_ver"
}

check_lockfile_pkg "mimikyu-core"
check_lockfile_pkg "mimikyu-cli"

# 4. Verify Homebrew formula version sync if present
if [ -f "$BREW_FORMULA" ]; then
  brew_ver=$(awk '
    $0 ~ /^[[:space:]]*version[[:space:]]+"/ {
      line = $0
      sub(/^[[:space:]]*version[[:space:]]+"/, "", line)
      sub(/".*$/, "", line)
      print line
      exit 0
    }
  ' "$BREW_FORMULA")
  if [ "$brew_ver" != "$WORKSPACE_VERSION" ]; then
    die "packaging/homebrew/mimikyu.rb version '$brew_ver' does not match workspace version '$WORKSPACE_VERSION'"
  fi
  printf '[ok] packaging/homebrew/mimikyu.rb version = %s\n' "$brew_ver"
fi

# 5. Verify CHANGELOG.md structure and release entry
if ! grep -Eq '^## \[Unreleased\][[:space:]]*$' "$CHANGELOG"; then
  die "CHANGELOG.md is missing the required '## [Unreleased]' section header"
fi

check_changelog_version_entry() {
  ver_to_check="$1"
  require_notes="$2"

  escaped_ver=$(printf '%s' "$ver_to_check" | sed 's/\./\\./g')
  if ! grep -Eq "^## \\[${escaped_ver}\\] - [0-9]{4}-[0-9]{2}-[0-9]{2}[[:space:]]*$" "$CHANGELOG"; then
    die "CHANGELOG.md is missing stamped release heading '## [${ver_to_check}] - YYYY-MM-DD'"
  fi

  if [ "$require_notes" -eq 1 ]; then
    notes_lines=$(awk -v ver="$ver_to_check" '
      $0 ~ "^## \\[" ver "\\] - " { in_sec = 1; next }
      in_sec && $0 ~ "^## \\[" { exit 0 }
      in_sec && $0 ~ /[^[:space:]]/ { count++ }
      END { print count + 0 }
    ' "$CHANGELOG")
    if [ "$notes_lines" -eq 0 ]; then
      die "CHANGELOG.md section '## [${ver_to_check}]' has no release notes"
    fi
  fi

  printf '[ok] CHANGELOG.md contains stamped entry for [%s]\n' "$ver_to_check"
}

if [ -n "$EXPECTED_TAG" ]; then
  TAG_VERSION="${EXPECTED_TAG#v}"
  if ! printf '%s\n' "$TAG_VERSION" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$'; then
    die "--tag '$EXPECTED_TAG' is not valid SemVer (expected vX.Y.Z or X.Y.Z)"
  fi
  if [ "$TAG_VERSION" != "$WORKSPACE_VERSION" ]; then
    die "tag version '$TAG_VERSION' (from '$EXPECTED_TAG') does not match Cargo.toml workspace version '$WORKSPACE_VERSION'"
  fi
  check_changelog_version_entry "$TAG_VERSION" 1
  printf '[ok] Release tag %s matches Cargo.toml, Cargo.lock, and CHANGELOG.md\n' "v$TAG_VERSION"
else
  check_changelog_version_entry "$WORKSPACE_VERSION" 1
  printf '[ok] Workspace version %s is synchronized across all release manifests\n' "$WORKSPACE_VERSION"
fi
