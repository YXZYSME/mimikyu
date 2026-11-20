#!/bin/sh
# Verify that a mimikyu release version is live and healthy across all public
# distribution channels before promoting website deployments to mimikyu.saengil.ai.
# Compatible with POSIX /bin/sh and macOS Bash 3.2+.
#
# Usage:
#   ./scripts/verify-release-registries.sh [vX.Y.Z|X.Y.Z]
#   ./scripts/verify-release-registries.sh --retries 6 --delay 15 v0.2.0

set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)
REPO_SLUG="YXZYSME/mimikyu"
HOMEBREW_TAP_REPO="YXZYSME/homebrew-tap"
MCP_REGISTRY_ID="io.github.YXZYSME/mimikyu"

RETRIES=6
DELAY_SECS=15
TARGET_VERSION=""

print_help() {
  cat <<'EOF'
Usage: scripts/verify-release-registries.sh [OPTIONS] [vX.Y.Z|X.Y.Z]

Verifies that the target release (default: [workspace.package].version in Cargo.toml)
is live and serving the expected version across:
  1. GitHub Release (non-draft + SHA256SUMS asset)
  2. crates.io (mimikyu, mimikyu-cli, mimikyu-core)
  3. npm (saengil-mimikyu, mimikyu-cli, mimikyu-mcp)
  4. PyPI (mimikyu)
  5. Official MCP Registry (io.github.YXZYSME/mimikyu)
  6. Homebrew Tap (YXZYSME/homebrew-tap)

Options:
  --retries <N>   Maximum attempts per check for CDN propagation [default: 6]
  --delay <SEC>   Seconds between retry attempts [default: 15]
  -h, --help      Show this help message and exit
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
    --retries)
      [ $# -ge 2 ] || die "--retries requires a number"
      RETRIES="$2"
      shift 2
      ;;
    --retries=*)
      RETRIES="${1#*=}"
      shift
      ;;
    --delay)
      [ $# -ge 2 ] || die "--delay requires seconds"
      DELAY_SECS="$2"
      shift 2
      ;;
    --delay=*)
      DELAY_SECS="${1#*=}"
      shift
      ;;
    -*)
      die "unknown option '$1' (run with --help for usage)"
      ;;
    *)
      if [ -n "$TARGET_VERSION" ]; then
        die "unexpected extra argument '$1'"
      fi
      TARGET_VERSION="$1"
      shift
      ;;
  esac
done

if [ -z "$TARGET_VERSION" ]; then
  TARGET_VERSION=$(awk '
    /^\[workspace\.package\][[:space:]]*$/ { in_ws_pkg = 1; next }
    /^\[/ { in_ws_pkg = 0 }
    in_ws_pkg && $0 ~ /^[[:space:]]*version[[:space:]]*=[[:space:]]*"/ {
      line = $0
      sub(/^[[:space:]]*version[[:space:]]*=[[:space:]]*"/, "", line)
      sub(/".*$/, "", line)
      print line
      exit 0
    }
  ' "$REPO_ROOT/Cargo.toml")
fi

VERSION="${TARGET_VERSION#v}"
TAG="v${VERSION}"

if ! printf '%s\n' "$VERSION" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$'; then
  die "invalid SemVer version '${TARGET_VERSION}'"
fi

printf '==> Verifying release %s across all 6 distribution ecosystems (retries=%s, delay=%ss)\n' \
  "$TAG" "$RETRIES" "$DELAY_SECS"

fetch_body() {
  url="$1"
  use_gh_auth="${2:-0}"
  if [ "$use_gh_auth" -eq 1 ] && [ -n "${GITHUB_TOKEN:-}" ]; then
    curl -fsSL --max-time 20 \
      -H "User-Agent: mimikyu-release-verifier/${VERSION}" \
      -H "Accept: application/vnd.github+json" \
      -H "Authorization: Bearer ${GITHUB_TOKEN}" \
      "$url"
  else
    curl -fsSL --max-time 20 \
      -H "User-Agent: mimikyu-release-verifier/${VERSION}" \
      "$url"
  fi
}

check_with_retry() {
  label="$1"
  check_fn="$2"
  shift 2

  attempt=1
  while [ "$attempt" -le "$RETRIES" ]; do
    if "$check_fn" "$@"; then
      printf '  [ok] %s\n' "$label"
      return 0
    fi
    if [ "$attempt" -lt "$RETRIES" ]; then
      printf '  [wait] %s not ready yet (attempt %s/%s); retrying in %ss...\n' \
        "$label" "$attempt" "$RETRIES" "$DELAY_SECS" >&2
      sleep "$DELAY_SECS"
    fi
    attempt=$((attempt + 1))
  done

  printf '  [FAIL] %s did not converge to %s after %s attempts\n' \
    "$label" "$VERSION" "$RETRIES" >&2
  return 1
}

check_github_release() {
  body=$(fetch_body "https://api.github.com/repos/${REPO_SLUG}/releases/tags/${TAG}" 1 2>/dev/null) || return 1
  printf '%s' "$body" | grep -Eq '"draft"[[:space:]]*:[[:space:]]*false' || return 1
  printf '%s' "$body" | grep -Eq '"name"[[:space:]]*:[[:space:]]*"SHA256SUMS"' || return 1
}

check_crate() {
  crate_name="$1"
  body=$(fetch_body "https://crates.io/api/v1/crates/${crate_name}/${VERSION}" 0 2>/dev/null) || return 1
  printf '%s' "$body" | grep -Eq "\"num\"[[:space:]]*:[[:space:]]*\"${VERSION}\"" || return 1
}

check_npm() {
  pkg_name="$1"
  body=$(fetch_body "https://registry.npmjs.org/${pkg_name}/${VERSION}" 0 2>/dev/null) || return 1
  printf '%s' "$body" | grep -Eq "\"version\"[[:space:]]*:[[:space:]]*\"${VERSION}\"" || return 1
}

check_pypi() {
  body=$(fetch_body "https://pypi.org/pypi/mimikyu/${VERSION}/json" 0 2>/dev/null) || return 1
  printf '%s' "$body" | grep -Eq "\"version\"[[:space:]]*:[[:space:]]*\"${VERSION}\"" || return 1
}

check_mcp_registry() {
  body=$(fetch_body "https://registry.modelcontextprotocol.io/v0/servers?search=${MCP_REGISTRY_ID}" 0 2>/dev/null) || return 1
  printf '%s' "$body" | grep -Fq "\"${MCP_REGISTRY_ID}\"" || return 1
  printf '%s' "$body" | grep -Eq "\"version\"[[:space:]]*:[[:space:]]*\"${VERSION}\"" || return 1
}

check_homebrew_tap() {
  body=$(fetch_body "https://raw.githubusercontent.com/${HOMEBREW_TAP_REPO}/main/Formula/mimikyu.rb" 0 2>/dev/null) || return 1
  printf '%s' "$body" | grep -Eq "^[[:space:]]*version[[:space:]]+\"${VERSION}\"" || return 1
}

FAILURES=0

check_with_retry "GitHub Release (${TAG} + SHA256SUMS)" check_github_release || FAILURES=$((FAILURES + 1))
check_with_retry "crates.io: mimikyu ${VERSION}" check_crate "mimikyu" || FAILURES=$((FAILURES + 1))
check_with_retry "crates.io: mimikyu-cli ${VERSION}" check_crate "mimikyu-cli" || FAILURES=$((FAILURES + 1))
check_with_retry "crates.io: mimikyu-core ${VERSION}" check_crate "mimikyu-core" || FAILURES=$((FAILURES + 1))
check_with_retry "npm: saengil-mimikyu ${VERSION}" check_npm "saengil-mimikyu" || FAILURES=$((FAILURES + 1))
check_with_retry "npm: mimikyu-cli ${VERSION}" check_npm "mimikyu-cli" || FAILURES=$((FAILURES + 1))
check_with_retry "npm: mimikyu-mcp ${VERSION}" check_npm "mimikyu-mcp" || FAILURES=$((FAILURES + 1))
check_with_retry "PyPI: mimikyu ${VERSION}" check_pypi || FAILURES=$((FAILURES + 1))
check_with_retry "Official MCP Registry: ${MCP_REGISTRY_ID} ${VERSION}" check_mcp_registry || FAILURES=$((FAILURES + 1))
check_with_retry "Homebrew Tap: ${HOMEBREW_TAP_REPO} ${VERSION}" check_homebrew_tap || FAILURES=$((FAILURES + 1))

if [ "$FAILURES" -ne 0 ]; then
  die "${FAILURES} registry verification check(s) failed for ${TAG}"
fi

printf '==> All 10 release channels verified live for %s!\n' "$TAG"
