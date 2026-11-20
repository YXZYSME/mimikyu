#!/bin/sh
# Official maintainer release CLI for mimikyu.
# Compatible with POSIX /bin/sh and macOS Bash 3.2+.
#
# Usage:
#   ./scripts/release.sh --dry-run 0.1.1
#   ./scripts/release.sh 0.1.1
#   ./scripts/release.sh --push --sign 0.1.1

set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)

DRY_RUN=0
DO_PUSH=0
SIGN_MODE="auto"
RELEASE_DATE=$(date -u +%Y-%m-%d)
TARGET_VERSION=""
TMP_DIR=""

cleanup() {
  if [ -n "${TMP_DIR:-}" ] && [ -d "$TMP_DIR" ]; then
    rm -rf "$TMP_DIR"
  fi
}

trap cleanup EXIT INT TERM HUP

log_info() {
  printf '==> %s\n' "$*"
}

log_warn() {
  printf 'warning: %s\n' "$*" >&2
}

die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

print_help() {
  cat <<'EOF'
Official maintainer release tool for mimikyu (POSIX /bin/sh)

USAGE:
    ./scripts/release.sh [OPTIONS] <X.Y.Z>

ARGUMENTS:
    <X.Y.Z>               Target SemVer release version (e.g. 0.1.1 or v0.1.1)

OPTIONS:
    --dry-run             Validate SemVer, simulate manifest/changelog stamping in a
                          temp directory, verify sync, and print planned git commands
                          without modifying the working tree
    --push                Push release commit and vX.Y.Z tag to 'origin' after tagging
                          [default: never pushes automatically]
    --sign                Create a GPG/SSH-signed annotated tag (`git tag -s`)
    --no-sign             Create an unsigned annotated tag (`git tag -a`)
    --date <YYYY-MM-DD>   Override release date stamped into CHANGELOG.md [default: UTC today]
    -h, --help            Show this help message and exit

EXAMPLES:
    ./scripts/release.sh --dry-run 0.1.1
    ./scripts/release.sh 0.1.1
    ./scripts/release.sh --sign --push 0.2.0
EOF
}

while [ $# -gt 0 ]; do
  case "$1" in
    -h|--help)
      print_help
      exit 0
      ;;
    --dry-run)
      DRY_RUN=1
      shift
      ;;
    --push)
      DO_PUSH=1
      shift
      ;;
    --sign)
      SIGN_MODE="yes"
      shift
      ;;
    --no-sign)
      SIGN_MODE="no"
      shift
      ;;
    --date)
      [ $# -ge 2 ] || die "--date requires YYYY-MM-DD"
      RELEASE_DATE="$2"
      shift 2
      ;;
    --date=*)
      RELEASE_DATE="${1#*=}"
      shift
      ;;
    -*)
      die "unknown option '$1' (run with --help for usage)"
      ;;
    *)
      if [ -n "$TARGET_VERSION" ]; then
        die "unexpected extra argument '$1' (version already set to '$TARGET_VERSION')"
      fi
      TARGET_VERSION="$1"
      shift
      ;;
  esac
done

[ -n "$TARGET_VERSION" ] || die "missing target SemVer version (usage: ./scripts/release.sh [--dry-run] <X.Y.Z>)"

NEW_VERSION="${TARGET_VERSION#v}"
RELEASE_TAG="v${NEW_VERSION}"

if ! printf '%s\n' "$NEW_VERSION" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$'; then
  die "invalid SemVer version '$TARGET_VERSION' (expected MAJOR.MINOR.PATCH, e.g. 0.1.1)"
fi

if ! printf '%s\n' "$RELEASE_DATE" | grep -Eq '^[0-9]{4}-[0-9]{2}-[0-9]{2}$'; then
  die "invalid --date '$RELEASE_DATE' (expected YYYY-MM-DD)"
fi

cd "$REPO_ROOT"

# 1. Verify git repository, branch == main, clean working tree, and unused tag
command -v git >/dev/null 2>&1 || die "'git' is required"

CURRENT_BRANCH=$(git rev-parse --abbrev-ref HEAD 2>/dev/null || echo "")
if [ "$CURRENT_BRANCH" != "main" ]; then
  if [ "$DRY_RUN" -eq 1 ]; then
    log_warn "[dry-run] current branch is '$CURRENT_BRANCH' (live release requires 'main')"
  else
    die "releases must be cut from branch 'main' (current branch: '$CURRENT_BRANCH')"
  fi
fi

GIT_STATUS=$(git status --porcelain 2>/dev/null || echo "")
if [ -n "$GIT_STATUS" ]; then
  if [ "$DRY_RUN" -eq 1 ]; then
    log_warn "[dry-run] working tree has uncommitted changes (live release requires a clean working tree)"
  else
    die "git working tree is not clean. Commit or stash changes before running scripts/release.sh."
  fi
fi

if git rev-parse -q --verify "refs/tags/$RELEASE_TAG" >/dev/null 2>&1; then
  die "git tag '$RELEASE_TAG' already exists"
fi

CURRENT_VERSION=$(awk '
  /^\[workspace\.package\][[:space:]]*$/ { in_ws = 1; next }
  /^\[/ { in_ws = 0 }
  in_ws && $0 ~ /^[[:space:]]*version[[:space:]]*=[[:space:]]*"/ {
    line = $0
    sub(/^[[:space:]]*version[[:space:]]*=[[:space:]]*"/, "", line)
    sub(/".*$/, "", line)
    print line
    exit 0
  }
' "$REPO_ROOT/Cargo.toml")

[ -n "$CURRENT_VERSION" ] || die "unable to read current [workspace.package] version from Cargo.toml"
if [ "$CURRENT_VERSION" = "$NEW_VERSION" ]; then
  die "Cargo.toml is already at version '$NEW_VERSION'"
fi

if ! grep -Eq '^## \[Unreleased\][[:space:]]*$' "$REPO_ROOT/CHANGELOG.md"; then
  die "CHANGELOG.md is missing '## [Unreleased]'"
fi

escape_CheckVer=$(printf '%s' "$NEW_VERSION" | sed 's/\./\\./g')
if grep -Eq "^## \\[${escape_CheckVer}\\]" "$REPO_ROOT/CHANGELOG.md"; then
  die "CHANGELOG.md already contains a section for '[${NEW_VERSION}]'"
fi

# Helper functions that rewrite manifests deterministically from source -> destination
rewrite_cargo_toml() {
  in_file="$1"
  out_file="$2"
  new_ver="$3"
  awk -v ver="$new_ver" '
    /^\[workspace\.package\][[:space:]]*$/ { in_ws = 1; in_deps = 0; print; next }
    /^\[workspace\.dependencies\][[:space:]]*$/ { in_ws = 0; in_deps = 1; print; next }
    /^\[/ { in_ws = 0; in_deps = 0 }
    in_ws && !replaced && $0 ~ /^[[:space:]]*version[[:space:]]*=[[:space:]]*"/ {
      print "version = \"" ver "\""
      replaced = 1
      next
    }
    in_deps && $0 ~ /^[[:space:]]*mimikyu-core[[:space:]]*=.*version[[:space:]]*=[[:space:]]*"/ {
      line = $0
      sub(/version[[:space:]]*=[[:space:]]*"[^"]*"/, "version = \"" ver "\"", line)
      print line
      next
    }
    { print }
  ' "$in_file" > "$out_file"
}

rewrite_homebrew_formula() {
  in_file="$1"
  out_file="$2"
  new_ver="$3"
  awk -v ver="$new_ver" '
    !replaced && $0 ~ /^[[:space:]]*version[[:space:]]+"/ {
      sub(/"[^"]*"/, "\"" ver "\"")
      replaced = 1
    }
    $0 ~ /\/releases\/download\/v[^\/]+\// {
      sub(/\/releases\/download\/v[^\/]+\//, "/releases/download/v" ver "/")
    }
    { print }
  ' "$in_file" > "$out_file"
}

rewrite_json_versions() {
  in_file="$1"
  out_file="$2"
  new_ver="$3"
  awk -v ver="$new_ver" '
    $0 ~ /^[[:space:]]*"version"[[:space:]]*:[[:space:]]*"/ {
      sub(/"version"[[:space:]]*:[[:space:]]*"[^"]*"/, "\"version\": \"" ver "\"")
    }
    { print }
  ' "$in_file" > "$out_file"
}

rewrite_pyproject_toml() {
  in_file="$1"
  out_file="$2"
  new_ver="$3"
  awk -v ver="$new_ver" '
    /^\[project\][[:space:]]*$/ { in_proj = 1; print; next }
    /^\[/ { in_proj = 0 }
    in_proj && !replaced && $0 ~ /^[[:space:]]*version[[:space:]]*=[[:space:]]*"/ {
      print "version = \"" ver "\""
      replaced = 1
      next
    }
    { print }
  ' "$in_file" > "$out_file"
}

rewrite_pypi_init() {
  in_file="$1"
  out_file="$2"
  new_ver="$3"
  awk -v ver="$new_ver" '
    $0 ~ /^__version__[[:space:]]*=[[:space:]]*"/ {
      print "__version__ = \"" ver "\""
      next
    }
    { print }
  ' "$in_file" > "$out_file"
}

rewrite_citation_cff() {
  in_file="$1"
  out_file="$2"
  new_ver="$3"
  rel_date="$4"
  awk -v ver="$new_ver" -v rdate="$rel_date" '
    $0 ~ /^version:[[:space:]]*"/ {
      print "version: \"" ver "\""
      next
    }
    $0 ~ /^date-released:[[:space:]]*"/ {
      print "date-released: \"" rdate "\""
      next
    }
    { print }
  ' "$in_file" > "$out_file"
}

rewrite_changelog() {
  in_file="$1"
  out_file="$2"
  new_ver="$3"
  rel_date="$4"

  # Count non-blank lines currently inside [Unreleased]
  unreleased_count=$(awk '
    /^## \[Unreleased\][[:space:]]*$/ { in_unrel = 1; next }
    in_unrel && /^## \[/ { exit 0 }
    in_unrel && /[^[:space:]]/ { c++ }
    END { print c + 0 }
  ' "$in_file")

  awk -v ver="$new_ver" -v rdate="$rel_date" -v has_notes="$unreleased_count" '
    /^## \[Unreleased\][[:space:]]*$/ {
      print "## [Unreleased]"
      print ""
      print "## [" ver "] - " rdate
      if (has_notes == 0) {
        print ""
        print "### Changed"
        print ""
        print "- Release v" ver "."
      }
      next
    }
    { print }
  ' "$in_file" > "$out_file"
}

rewrite_cargo_lock_preview() {
  in_file="$1"
  out_file="$2"
  new_ver="$3"
  awk -v ver="$new_ver" '
    /^\[\[package\]\][[:space:]]*$/ { pkg = ""; print; next }
    $0 ~ /^name = "/ {
      line = $0
      sub(/^name = "/, "", line)
      sub(/".*$/, "", line)
      pkg = line
      print
      next
    }
    (pkg == "mimikyu" || pkg == "mimikyu-core" || pkg == "mimikyu-cli") && $0 ~ /^version = "/ {
      print "version = \"" ver "\""
      pkg = ""
      next
    }
    { print }
  ' "$in_file" > "$out_file"
}

# Resolve tag signing mode
USE_SIGNED_TAG=0
if [ "$SIGN_MODE" = "yes" ]; then
  USE_SIGNED_TAG=1
elif [ "$SIGN_MODE" = "auto" ]; then
  tag_gpg=$(git config --bool tag.gpgSign 2>/dev/null || echo "false")
  if [ "$tag_gpg" = "true" ]; then
    USE_SIGNED_TAG=1
  fi
fi

if [ "$USE_SIGNED_TAG" -eq 1 ]; then
  TAG_FLAG="-s"
else
  TAG_FLAG="-a"
fi

# 2. Dry-run simulation (never touches the working tree!)
if [ "$DRY_RUN" -eq 1 ]; then
  TMP_DIR=$(mktemp -d "${TMPDIR:-/tmp}/mimikyu-release-dryrun.XXXXXX")
  mkdir -p "$TMP_DIR/crates/mimikyu-core" "$TMP_DIR/crates/mimikyu-cli" "$TMP_DIR/crates/mimikyu" "$TMP_DIR/packaging/homebrew" "$TMP_DIR/packaging/npm/mimikyu-mcp" "$TMP_DIR/packaging/npm/saengil-mimikyu" "$TMP_DIR/packaging/npm/mimikyu-cli" "$TMP_DIR/packaging/pypi/src/mimikyu_pypi"

  rewrite_cargo_toml "$REPO_ROOT/Cargo.toml" "$TMP_DIR/Cargo.toml" "$NEW_VERSION"
  cp "$REPO_ROOT/crates/mimikyu-core/Cargo.toml" "$TMP_DIR/crates/mimikyu-core/Cargo.toml"
  cp "$REPO_ROOT/crates/mimikyu-cli/Cargo.toml" "$TMP_DIR/crates/mimikyu-cli/Cargo.toml"
  if [ -f "$REPO_ROOT/crates/mimikyu/Cargo.toml" ]; then
    cp "$REPO_ROOT/crates/mimikyu/Cargo.toml" "$TMP_DIR/crates/mimikyu/Cargo.toml"
  fi
  rewrite_cargo_lock_preview "$REPO_ROOT/Cargo.lock" "$TMP_DIR/Cargo.lock" "$NEW_VERSION"
  rewrite_changelog "$REPO_ROOT/CHANGELOG.md" "$TMP_DIR/CHANGELOG.md" "$NEW_VERSION" "$RELEASE_DATE"
  if [ -f "$REPO_ROOT/packaging/homebrew/mimikyu.rb" ]; then
    rewrite_homebrew_formula "$REPO_ROOT/packaging/homebrew/mimikyu.rb" "$TMP_DIR/packaging/homebrew/mimikyu.rb" "$NEW_VERSION"
  fi
  for npm_dir in mimikyu-mcp saengil-mimikyu mimikyu-cli; do
    if [ -f "$REPO_ROOT/packaging/npm/$npm_dir/package.json" ]; then
      rewrite_json_versions "$REPO_ROOT/packaging/npm/$npm_dir/package.json" "$TMP_DIR/packaging/npm/$npm_dir/package.json" "$NEW_VERSION"
    fi
  done
  if [ -f "$REPO_ROOT/packaging/pypi/pyproject.toml" ]; then
    rewrite_pyproject_toml "$REPO_ROOT/packaging/pypi/pyproject.toml" "$TMP_DIR/packaging/pypi/pyproject.toml" "$NEW_VERSION"
  fi
  if [ -f "$REPO_ROOT/packaging/pypi/src/mimikyu_pypi/__init__.py" ]; then
    rewrite_pypi_init "$REPO_ROOT/packaging/pypi/src/mimikyu_pypi/__init__.py" "$TMP_DIR/packaging/pypi/src/mimikyu_pypi/__init__.py" "$NEW_VERSION"
  fi
  if [ -f "$REPO_ROOT/CITATION.cff" ]; then
    rewrite_citation_cff "$REPO_ROOT/CITATION.cff" "$TMP_DIR/CITATION.cff" "$NEW_VERSION" "$RELEASE_DATE"
  fi
  if [ -f "$REPO_ROOT/server.json" ]; then
    rewrite_json_versions "$REPO_ROOT/server.json" "$TMP_DIR/server.json" "$NEW_VERSION"
  fi

  log_info "[dry-run] Validating simulated release manifests for $RELEASE_TAG ($CURRENT_VERSION -> $NEW_VERSION)..."
  "$SCRIPT_DIR/check-version-sync.sh" --root "$TMP_DIR" --tag "$RELEASE_TAG"

  log_info "[dry-run] Planned execution steps (working tree NOT modified):"
  printf '  1. Update Cargo.toml [workspace.package] version: %s -> %s\n' "$CURRENT_VERSION" "$NEW_VERSION"
  if [ -f "$REPO_ROOT/packaging/homebrew/mimikyu.rb" ]; then
    printf '  2. Update packaging/homebrew/mimikyu.rb version:  %s -> %s\n' "$CURRENT_VERSION" "$NEW_VERSION"
  fi
  printf '  3. Sync lockfile:  cargo check --workspace\n'
  printf '  4. Stamp changelog: [Unreleased] -> [%s] - %s\n' "$NEW_VERSION" "$RELEASE_DATE"
  printf '  5. Verify sync:    ./scripts/check-version-sync.sh --tag %s\n' "$RELEASE_TAG"
  printf '  6. Commit release: git commit -m "chore(release): %s"\n' "$RELEASE_TAG"
  printf '  7. Create tag:     git tag %s %s -m "mimikyu %s"\n' "$TAG_FLAG" "$RELEASE_TAG" "$RELEASE_TAG"
  if [ "$DO_PUSH" -eq 1 ]; then
    printf '  8. Push to origin: git push origin main && git push origin %s\n' "$RELEASE_TAG"
  else
    printf '  8. Push skipped (pass --push or run manually: git push origin main && git push origin %s)\n' "$RELEASE_TAG"
  fi
  exit 0
fi

# 3. Live execution on clean `main`
TMP_DIR=$(mktemp -d "${TMPDIR:-/tmp}/mimikyu-release.XXXXXX")

log_info "Updating Cargo.toml version: $CURRENT_VERSION -> $NEW_VERSION"
rewrite_cargo_toml "$REPO_ROOT/Cargo.toml" "$TMP_DIR/Cargo.toml" "$NEW_VERSION"
mv "$TMP_DIR/Cargo.toml" "$REPO_ROOT/Cargo.toml"

if [ -f "$REPO_ROOT/packaging/homebrew/mimikyu.rb" ]; then
  log_info "Updating packaging/homebrew/mimikyu.rb version -> $NEW_VERSION"
  rewrite_homebrew_formula "$REPO_ROOT/packaging/homebrew/mimikyu.rb" "$TMP_DIR/mimikyu.rb" "$NEW_VERSION"
  mv "$TMP_DIR/mimikyu.rb" "$REPO_ROOT/packaging/homebrew/mimikyu.rb"
fi

for npm_dir in mimikyu-mcp saengil-mimikyu mimikyu-cli; do
  if [ -f "$REPO_ROOT/packaging/npm/$npm_dir/package.json" ]; then
    log_info "Updating packaging/npm/$npm_dir/package.json version -> $NEW_VERSION"
    rewrite_json_versions "$REPO_ROOT/packaging/npm/$npm_dir/package.json" "$TMP_DIR/package.json" "$NEW_VERSION"
    mv "$TMP_DIR/package.json" "$REPO_ROOT/packaging/npm/$npm_dir/package.json"
  fi
done

if [ -f "$REPO_ROOT/packaging/pypi/pyproject.toml" ]; then
  log_info "Updating packaging/pypi/pyproject.toml version -> $NEW_VERSION"
  rewrite_pyproject_toml "$REPO_ROOT/packaging/pypi/pyproject.toml" "$TMP_DIR/pyproject.toml" "$NEW_VERSION"
  mv "$TMP_DIR/pyproject.toml" "$REPO_ROOT/packaging/pypi/pyproject.toml"
fi

if [ -f "$REPO_ROOT/packaging/pypi/src/mimikyu_pypi/__init__.py" ]; then
  log_info "Updating packaging/pypi/src/mimikyu_pypi/__init__.py version -> $NEW_VERSION"
  rewrite_pypi_init "$REPO_ROOT/packaging/pypi/src/mimikyu_pypi/__init__.py" "$TMP_DIR/__init__.py" "$NEW_VERSION"
  mv "$TMP_DIR/__init__.py" "$REPO_ROOT/packaging/pypi/src/mimikyu_pypi/__init__.py"
fi

if [ -f "$REPO_ROOT/CITATION.cff" ]; then
  log_info "Updating CITATION.cff version -> $NEW_VERSION"
  rewrite_citation_cff "$REPO_ROOT/CITATION.cff" "$TMP_DIR/CITATION.cff" "$NEW_VERSION" "$RELEASE_DATE"
  mv "$TMP_DIR/CITATION.cff" "$REPO_ROOT/CITATION.cff"
fi

if [ -f "$REPO_ROOT/server.json" ]; then
  log_info "Updating server.json version -> $NEW_VERSION"
  rewrite_json_versions "$REPO_ROOT/server.json" "$TMP_DIR/server.json" "$NEW_VERSION"
  mv "$TMP_DIR/server.json" "$REPO_ROOT/server.json"
fi

log_info "Running 'cargo check --workspace' to synchronize Cargo.lock..."
cargo check --workspace

log_info "Stamping CHANGELOG.md: [Unreleased] -> [$NEW_VERSION] - $RELEASE_DATE"
rewrite_changelog "$REPO_ROOT/CHANGELOG.md" "$TMP_DIR/CHANGELOG.md" "$NEW_VERSION" "$RELEASE_DATE"
mv "$TMP_DIR/CHANGELOG.md" "$REPO_ROOT/CHANGELOG.md"

log_info "Verifying version sync for $RELEASE_TAG..."
"$SCRIPT_DIR/check-version-sync.sh" --tag "$RELEASE_TAG"

log_info "Creating release commit and annotated git tag $RELEASE_TAG..."
git add Cargo.toml Cargo.lock CHANGELOG.md
if [ -f "$REPO_ROOT/packaging/homebrew/mimikyu.rb" ]; then
  git add packaging/homebrew/mimikyu.rb
fi
for npm_dir in mimikyu-mcp saengil-mimikyu mimikyu-cli; do
  if [ -f "$REPO_ROOT/packaging/npm/$npm_dir/package.json" ]; then
    git add "packaging/npm/$npm_dir/package.json"
  fi
done
if [ -f "$REPO_ROOT/packaging/pypi/pyproject.toml" ]; then
  git add packaging/pypi/pyproject.toml
fi
if [ -f "$REPO_ROOT/packaging/pypi/src/mimikyu_pypi/__init__.py" ]; then
  git add packaging/pypi/src/mimikyu_pypi/__init__.py
fi
if [ -f "$REPO_ROOT/CITATION.cff" ]; then
  git add CITATION.cff
fi
if [ -f "$REPO_ROOT/server.json" ]; then
  git add server.json
fi

git commit -m "chore(release): ${RELEASE_TAG}"
git tag "$TAG_FLAG" "$RELEASE_TAG" -m "mimikyu ${RELEASE_TAG}"

if [ "$DO_PUSH" -eq 1 ]; then
  log_info "Pushing main and $RELEASE_TAG to origin..."
  git push origin main
  git push origin "$RELEASE_TAG"
  log_info "Release $RELEASE_TAG pushed! GitHub Actions (.github/workflows/release.yml) will now build, attest, and publish the release."
else
  log_info "Created local release commit and tag '$RELEASE_TAG'."
  printf '\nNext step — review and push to trigger production release:\n'
  printf '  git show %s\n' "$RELEASE_TAG"
  printf '  git push origin main && git push origin %s\n' "$RELEASE_TAG"
fi
