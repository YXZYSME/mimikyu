#!/bin/sh
# mimikyu POSIX CLI installer & self-updater
# Compatible with POSIX /bin/sh and macOS Bash 3.2+.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/<owner>/mimikyu/main/install.sh | sh
#   curl -fsSL https://raw.githubusercontent.com/<owner>/mimikyu/main/install.sh | sh -s -- --update
#   ./install.sh --from-source --bin-dir ~/.local/bin

set -eu

BINARY_NAME="mimikyu"
DEFAULT_REPO_PLACEHOLDER="<owner>/mimikyu"
REPO="${MIMIKYU_REPO:-$DEFAULT_REPO_PLACEHOLDER}"
REPO_EXPLICIT=0
if [ -n "${MIMIKYU_REPO:-}" ]; then
  REPO_EXPLICIT=1
fi

INSTALL_DIR="${MIMIKYU_INSTALL_DIR:-$HOME/.local/bin}"
REQUESTED_VERSION="latest"
VERSION_EXPLICIT=0
TARGET_OVERRIDE="${MIMIKYU_TARGET:-}"
MODE_UPDATE=0
MODE_FROM_SOURCE=0
MODE_UNINSTALL=0
MODE_DRY_RUN=0
VERIFY_ATTESTATION=0
TMP_DIR=""

log_info() {
  printf '==> %s\n' "$*"
}

log_warn() {
  printf 'warning: %s\n' "$*" >&2
}

log_err() {
  printf 'error: %s\n' "$*" >&2
}

die() {
  log_err "$*"
  exit 1
}

cleanup() {
  if [ -n "${TMP_DIR:-}" ] && [ -d "$TMP_DIR" ]; then
    rm -rf "$TMP_DIR"
  fi
}

trap cleanup EXIT INT TERM HUP

print_help() {
  cat <<'EOF'
mimikyu installer & updater (POSIX /bin/sh)

USAGE:
    install.sh [OPTIONS]

OPTIONS:
    --version <vX.Y.Z|latest>   Release version to install [default: latest]
    --update                    Check installed mimikyu version against GitHub Releases
                                and upgrade atomically if a newer release is available
    --bin-dir, --prefix <DIR>   Target directory for the mimikyu binary
                                [default: $MIMIKYU_INSTALL_DIR or $HOME/.local/bin]
    --repo <owner/name>         GitHub repository slug [default: $MIMIKYU_REPO or <owner>/mimikyu]
    --target <TRIPLE>           Override target triple (e.g. aarch64-apple-darwin,
                                x86_64-apple-darwin, x86_64-unknown-linux-gnu,
                                aarch64-unknown-linux-gnu)
    --from-source               Build and install via Cargo (`cargo install --locked --path`
                                when inside a local checkout, or `cargo install --locked --git`)
    --verify-attestation        Verify GitHub Artifact Attestation via `gh attestation verify`
                                in addition to mandatory SHA-256 checksum verification
    --uninstall                 Remove the installed mimikyu binary from the target directory
    --dry-run                   Print planned actions without downloading or modifying files
    -h, --help                  Show this help message and exit

ENVIRONMENT VARIABLES:
    MIMIKYU_INSTALL_DIR         Default installation directory (overridden by --bin-dir/--prefix)
    MIMIKYU_REPO                Default GitHub owner/name slug (overridden by --repo)
    MIMIKYU_TARGET              Default Rust target triple (overridden by --target)
    GITHUB_TOKEN / GH_TOKEN     Optional GitHub token used when querying the Releases API

EXAMPLES:
    # Install latest prebuilt release (verifies SHA256SUMS before extraction)
    curl -fsSL https://raw.githubusercontent.com/<owner>/mimikyu/main/install.sh | sh

    # Atomically update an existing installation if a newer release exists
    curl -fsSL https://raw.githubusercontent.com/<owner>/mimikyu/main/install.sh | sh -s -- --update

    # Pin an exact version and verify SLSA build provenance with GitHub CLI
    ./install.sh --repo <owner>/mimikyu --version v0.1.0 --verify-attestation

    # Build and install directly from a local repository clone
    ./install.sh --from-source --bin-dir "$HOME/.local/bin"

    # Uninstall
    ./install.sh --uninstall
EOF
}

parse_args() {
  while [ $# -gt 0 ]; do
    case "$1" in
      -h|--help)
        print_help
        exit 0
        ;;
      --version)
        [ $# -ge 2 ] || die "--version requires a value (e.g. v0.1.0 or latest)"
        REQUESTED_VERSION="$2"
        VERSION_EXPLICIT=1
        shift 2
        ;;
      --version=*)
        REQUESTED_VERSION="${1#*=}"
        VERSION_EXPLICIT=1
        shift
        ;;
      --update)
        MODE_UPDATE=1
        shift
        ;;
      --bin-dir|--prefix)
        [ $# -ge 2 ] || die "$1 requires a directory path"
        INSTALL_DIR="$2"
        shift 2
        ;;
      --bin-dir=*|--prefix=*)
        INSTALL_DIR="${1#*=}"
        shift
        ;;
      --repo)
        [ $# -ge 2 ] || die "--repo requires an <owner/name> argument"
        REPO="$2"
        REPO_EXPLICIT=1
        shift 2
        ;;
      --repo=*)
        REPO="${1#*=}"
        REPO_EXPLICIT=1
        shift
        ;;
      --target)
        [ $# -ge 2 ] || die "--target requires a Rust target triple"
        TARGET_OVERRIDE="$2"
        shift 2
        ;;
      --target=*)
        TARGET_OVERRIDE="${1#*=}"
        shift
        ;;
      --from-source)
        MODE_FROM_SOURCE=1
        shift
        ;;
      --verify-attestation)
        VERIFY_ATTESTATION=1
        shift
        ;;
      --uninstall)
        MODE_UNINSTALL=1
        shift
        ;;
      --dry-run)
        MODE_DRY_RUN=1
        shift
        ;;
      *)
        die "unknown option '$1' (run with --help for usage)"
        ;;
    esac
  done
}

detect_local_repo_root() {
  # Check current directory first, then directory containing install.sh
  if [ -f "./crates/mimikyu-cli/Cargo.toml" ] && [ -f "./Cargo.toml" ]; then
    pwd
    return 0
  fi
  script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" 2>/dev/null && pwd || true)
  if [ -n "$script_dir" ] && [ -f "$script_dir/crates/mimikyu-cli/Cargo.toml" ] && [ -f "$script_dir/Cargo.toml" ]; then
    printf '%s\n' "$script_dir"
    return 0
  fi
  return 1
}

try_autodetect_git_repo() {
  if [ "$REPO_EXPLICIT" -eq 1 ]; then
    return 0
  fi
  case "$REPO" in
    *"<"*|*"owner/"*)
      if command -v git >/dev/null 2>&1; then
        origin_url=$(git config --get remote.origin.url 2>/dev/null || true)
        if [ -n "$origin_url" ]; then
          parsed_slug=$(printf '%s\n' "$origin_url" | sed -n \
            -e 's#^https://github\.com/\([^/][^/]*/[^/.][^/]*\)/*$#\1#p' \
            -e 's#^https://github\.com/\([^/][^/]*/[^/]*\)\.git$#\1#p' \
            -e 's#^git@github\.com:\([^/][^/]*/[^/]*\)\.git$#\1#p' \
            -e 's#^git@github\.com:\([^/][^/]*/[^/.][^/]*\)$#\1#p' | head -n 1)
          if [ -n "$parsed_slug" ]; then
            REPO="$parsed_slug"
          fi
        fi
      fi
      ;;
  esac
}

validate_repo_slug() {
  case "$REPO" in
    ""|*"<"*|*">"*|*" "*)
      die "GitHub repository placeholder '$REPO' has not been configured yet. Pass --repo <owner/mimikyu>, set MIMIKYU_REPO=<owner/mimikyu>, or install locally with --from-source."
      ;;
  esac
  if ! printf '%s\n' "$REPO" | grep -Eq '^[A-Za-z0-9._-]+/[A-Za-z0-9._-]+$'; then
    die "invalid --repo '$REPO' (expected GitHub slug in '<owner>/<name>' format)"
  fi
}

normalize_tag() {
  raw_ver="$1"
  if [ "$raw_ver" = "latest" ]; then
    printf 'latest\n'
    return 0
  fi
  case "$raw_ver" in
    v*) tag_ver="$raw_ver" ;;
    *)  tag_ver="v$raw_ver" ;;
  esac
  if ! printf '%s\n' "$tag_ver" | grep -Eq '^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$'; then
    die "invalid version '$raw_ver' (expected 'latest' or SemVer like 'v0.1.0' / '0.1.0')"
  fi
  printf '%s\n' "$tag_ver"
}

detect_target() {
  if [ -n "$TARGET_OVERRIDE" ]; then
    case "$TARGET_OVERRIDE" in
      aarch64-apple-darwin|x86_64-apple-darwin|x86_64-unknown-linux-gnu|aarch64-unknown-linux-gnu)
        printf '%s\n' "$TARGET_OVERRIDE"
        return 0
        ;;
      *)
        die "unsupported target '$TARGET_OVERRIDE' (supported: aarch64-apple-darwin, x86_64-apple-darwin, x86_64-unknown-linux-gnu, aarch64-unknown-linux-gnu)"
        ;;
    esac
  fi

  os_name=$(uname -s 2>/dev/null || echo unknown)
  arch_name=$(uname -m 2>/dev/null || echo unknown)

  case "$os_name" in
    Darwin)
      # If running under Rosetta 2 on Apple Silicon, prefer native arm64 binary
      if [ "$arch_name" = "x86_64" ] && command -v sysctl >/dev/null 2>&1; then
        if [ "$(sysctl -n hw.optional.arm64 2>/dev/null || echo 0)" = "1" ]; then
          arch_name="arm64"
        fi
      fi
      case "$arch_name" in
        arm64|aarch64) printf 'aarch64-apple-darwin\n' ;;
        x86_64|amd64)  printf 'x86_64-apple-darwin\n' ;;
        *) die "unsupported macOS architecture '$arch_name'" ;;
      esac
      ;;
    Linux)
      case "$arch_name" in
        x86_64|amd64)  printf 'x86_64-unknown-linux-gnu\n' ;;
        aarch64|arm64) printf 'aarch64-unknown-linux-gnu\n' ;;
        *) die "unsupported Linux architecture '$arch_name'" ;;
      esac
      ;;
    *)
      die "unsupported operating system '$os_name' (supported: macOS Darwin and Linux)"
      ;;
  esac
}

make_temp_dir() {
  TMP_DIR=$(mktemp -d "${TMPDIR:-/tmp}/mimikyu-install.XXXXXX")
}

http_download() {
  src_url="$1"
  dest_path="$2"
  auth_token="${GITHUB_TOKEN:-${GH_TOKEN:-}}"

  if command -v curl >/dev/null 2>&1; then
    if [ -n "$auth_token" ]; then
      curl -fsSL --retry 3 --connect-timeout 15 \
        -H "Authorization: Bearer $auth_token" \
        -o "$dest_path" "$src_url"
    else
      curl -fsSL --retry 3 --connect-timeout 15 -o "$dest_path" "$src_url"
    fi
  elif command -v wget >/dev/null 2>&1; then
    if [ -n "$auth_token" ]; then
      wget -q --header="Authorization: Bearer $auth_token" -O "$dest_path" "$src_url"
    else
      wget -q -O "$dest_path" "$src_url"
    fi
  else
    die "neither 'curl' nor 'wget' was found in PATH"
  fi
}

resolve_latest_tag() {
  api_url="https://api.github.com/repos/${REPO}/releases/latest"
  json_file="$TMP_DIR/latest_release.json"

  if http_download "$api_url" "$json_file" 2>/dev/null; then
    resolved_tag=$(tr '\n' ' ' < "$json_file" | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p')
    if [ -n "$resolved_tag" ]; then
      printf '%s\n' "$resolved_tag"
      return 0
    fi
  fi

  # Fallback: inspect redirect Location header from github.com/<repo>/releases/latest
  if command -v curl >/dev/null 2>&1; then
    redirect_url=$(curl -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/${REPO}/releases/latest" 2>/dev/null || true)
    case "$redirect_url" in
      */releases/tag/*)
        resolved_tag="${redirect_url##*/releases/tag/}"
        resolved_tag="${resolved_tag%%\?*}"
        if [ -n "$resolved_tag" ]; then
          printf '%s\n' "$resolved_tag"
          return 0
        fi
        ;;
    esac
  fi

  die "unable to resolve latest GitHub release tag for '${REPO}' (check repository name, network connectivity, or pass --version vX.Y.Z)"
}

compute_sha256() {
  file_path="$1"
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$file_path" | awk '{print $1}' | tr 'A-F' 'a-f'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$file_path" | awk '{print $1}' | tr 'A-F' 'a-f'
  elif command -v openssl >/dev/null 2>&1; then
    openssl dgst -sha256 "$file_path" | awk '{print $NF}' | tr 'A-F' 'a-f'
  else
    die "cannot verify SHA-256 checksum: neither 'sha256sum', 'shasum', nor 'openssl' is installed"
  fi
}

extract_expected_sha256() {
  sums_file="$1"
  asset_name="$2"
  awk -v target="$asset_name" '
    NF >= 2 {
      hash = $1
      file = $2
      sub(/^\*/, "", file)
      sub(/^\.\//, "", file)
      if (file == target) {
        print hash
        exit 0
      }
    }
  ' "$sums_file" | tr 'A-F' 'a-f'
}

get_installed_version() {
  candidate_bin=""
  if [ -x "$INSTALL_DIR/$BINARY_NAME" ]; then
    candidate_bin="$INSTALL_DIR/$BINARY_NAME"
  elif command -v "$BINARY_NAME" >/dev/null 2>&1; then
    candidate_bin=$(command -v "$BINARY_NAME")
  fi

  if [ -z "$candidate_bin" ]; then
    return 1
  fi

  ver_out=$("$candidate_bin" --version 2>/dev/null | head -n 1 || true)
  parsed_ver=$(printf '%s\n' "$ver_out" | awk '{for (i=1; i<=NF; i++) if ($i ~ /^v?[0-9]+\.[0-9]+\.[0-9]+/) { sub(/^v/, "", $i); print $i; exit }}')
  if [ -z "$parsed_ver" ]; then
    return 1
  fi
  printf '%s\n' "$parsed_ver"
}

# Returns 0 (true) if $1 is strictly newer than $2 (SemVer MAJOR.MINOR.PATCH[-pre])
semver_is_newer() {
  ver_a="${1#v}"
  ver_b="${2#v}"
  if [ "$ver_a" = "$ver_b" ]; then
    return 1
  fi

  base_a="${ver_a%%-*}"
  base_b="${ver_b%%-*}"
  pre_a=""
  pre_b=""
  case "$ver_a" in *-*) pre_a="${ver_a#*-}" ;; esac
  case "$ver_b" in *-*) pre_b="${ver_b#*-}" ;; esac

  maj_a=$(printf '%s' "$base_a" | cut -d. -f1)
  min_a=$(printf '%s' "$base_a" | cut -d. -f2)
  pat_a=$(printf '%s' "$base_a" | cut -d. -f3)

  maj_b=$(printf '%s' "$base_b" | cut -d. -f1)
  min_b=$(printf '%s' "$base_b" | cut -d. -f2)
  pat_b=$(printf '%s' "$base_b" | cut -d. -f3)

  if [ "$maj_a" -gt "$maj_b" ]; then return 0; fi
  if [ "$maj_a" -lt "$maj_b" ]; then return 1; fi
  if [ "$min_a" -gt "$min_b" ]; then return 0; fi
  if [ "$min_a" -lt "$min_b" ]; then return 1; fi
  if [ "$pat_a" -gt "$pat_b" ]; then return 0; fi
  if [ "$pat_a" -lt "$pat_b" ]; then return 1; fi

  # Stable release is newer than prerelease of same X.Y.Z
  if [ -z "$pre_a" ] && [ -n "$pre_b" ]; then return 0; fi
  if [ -n "$pre_a" ] && [ -z "$pre_b" ]; then return 1; fi

  # Lexical tie-break on prerelease identifier
  if [ "$(printf '%s\n%s\n' "$pre_a" "$pre_b" | sort | tail -n 1)" = "$pre_a" ]; then
    return 0
  fi
  return 1
}

atomic_install_binary() {
  src_bin="$1"
  dest_dir="$2"
  dest_bin="$dest_dir/$BINARY_NAME"

  [ -f "$src_bin" ] || die "compiled/extracted binary not found at '$src_bin'"
  mkdir -p "$dest_dir"

  staged_bin="$dest_dir/.${BINARY_NAME}.tmp.$$"
  cp "$src_bin" "$staged_bin"
  chmod 0755 "$staged_bin"
  mv -f "$staged_bin" "$dest_bin"
}

check_path_hint() {
  case ":${PATH:-}:" in
    *":$INSTALL_DIR:"*) ;;
    *)
      log_warn "'$INSTALL_DIR' is not currently on your PATH."
      printf '  Add it to your shell profile:\n    export PATH="%s:$PATH"\n' "$INSTALL_DIR"
      ;;
  esac
}

run_uninstall() {
  target_bin="$INSTALL_DIR/$BINARY_NAME"
  if [ "$MODE_DRY_RUN" -eq 1 ]; then
    log_info "[dry-run] Would remove '$target_bin' (if present)"
    return 0
  fi

  if [ ! -e "$target_bin" ] && [ ! -L "$target_bin" ]; then
    log_info "No '$BINARY_NAME' binary found at '$target_bin' (nothing to remove)"
    return 0
  fi

  if [ -d "$target_bin" ] && [ ! -L "$target_bin" ]; then
    die "refusing to remove '$target_bin' because it is a directory"
  fi

  rm -f "$target_bin"
  log_info "Removed '$target_bin'"
  printf '  Note: persistent profiles/state directories (if any) were preserved.\n'
}

run_from_source() {
  if ! command -v cargo >/dev/null 2>&1; then
    die "'cargo' is required for --from-source (install Rust 1.88+ via https://rustup.rs)"
  fi

  local_repo=""
  if local_repo=$(detect_local_repo_root); then
    if [ "$MODE_DRY_RUN" -eq 1 ]; then
      log_info "[dry-run] Source mode: local workspace at '$local_repo'"
      log_info "[dry-run] Would run: cargo install --locked --path '$local_repo/crates/mimikyu-cli' and atomically install to '$INSTALL_DIR/$BINARY_NAME'"
      return 0
    fi

    make_temp_dir
    cargo_root="$TMP_DIR/cargo-root"
    mkdir -p "$cargo_root/bin"
    log_info "Building and installing '$BINARY_NAME' from local source ($local_repo/crates/mimikyu-cli)..."
    PATH="$cargo_root/bin:${PATH:-}" CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$local_repo/target}" \
      cargo install --locked --path "$local_repo/crates/mimikyu-cli" --root "$cargo_root" --force
    atomic_install_binary "$cargo_root/bin/$BINARY_NAME" "$INSTALL_DIR"
  else
    try_autodetect_git_repo
    validate_repo_slug
    norm_tag=$(normalize_tag "$REQUESTED_VERSION")
    git_url="https://github.com/${REPO}.git"

    if [ "$MODE_DRY_RUN" -eq 1 ]; then
      if [ "$norm_tag" = "latest" ]; then
        log_info "[dry-run] Would run: cargo install --locked --git '$git_url' mimikyu-cli and atomically install to '$INSTALL_DIR/$BINARY_NAME'"
      else
        log_info "[dry-run] Would run: cargo install --locked --git '$git_url' --tag '$norm_tag' mimikyu-cli and atomically install to '$INSTALL_DIR/$BINARY_NAME'"
      fi
      return 0
    fi

    make_temp_dir
    cargo_root="$TMP_DIR/cargo-root"
    mkdir -p "$cargo_root/bin"
    log_info "Building and installing '$BINARY_NAME' from $git_url..."
    if [ "$norm_tag" = "latest" ]; then
      PATH="$cargo_root/bin:${PATH:-}" cargo install --locked --git "$git_url" mimikyu-cli --root "$cargo_root" --force
    else
      PATH="$cargo_root/bin:${PATH:-}" cargo install --locked --git "$git_url" --tag "$norm_tag" mimikyu-cli --root "$cargo_root" --force
    fi
    atomic_install_binary "$cargo_root/bin/$BINARY_NAME" "$INSTALL_DIR"
  fi

  installed_ver=$("$INSTALL_DIR/$BINARY_NAME" --version 2>/dev/null || echo "$BINARY_NAME")
  log_info "Installed $installed_ver -> $INSTALL_DIR/$BINARY_NAME"
  check_path_hint
}

run_release_install() {
  try_autodetect_git_repo
  validate_repo_slug
  target_triple=$(detect_target)
  norm_tag=$(normalize_tag "$REQUESTED_VERSION")
  archive_name="${BINARY_NAME}-${target_triple}.tar.gz"

  installed_ver=""
  if installed_ver=$(get_installed_version); then
    :
  else
    installed_ver=""
  fi

  if [ "$MODE_DRY_RUN" -eq 1 ]; then
    if [ "$norm_tag" = "latest" ]; then
      dl_base="https://github.com/${REPO}/releases/latest/download"
    else
      dl_base="https://github.com/${REPO}/releases/download/${norm_tag}"
    fi
    log_info "[dry-run] Repository:   $REPO"
    log_info "[dry-run] Target:       $target_triple"
    log_info "[dry-run] Version:      $norm_tag (currently installed: ${installed_ver:-none})"
    if [ "$MODE_UPDATE" -eq 1 ]; then
      log_info "[dry-run] Update mode:  enabled (would compare installed version vs GitHub release tag)"
    fi
    log_info "[dry-run] Archive URL:  $dl_base/$archive_name"
    log_info "[dry-run] Checksum URL: $dl_base/SHA256SUMS"
    if [ "$VERIFY_ATTESTATION" -eq 1 ]; then
      log_info "[dry-run] Attestation:  gh attestation verify '$archive_name' --repo '$REPO'"
    fi
    log_info "[dry-run] Destination:  $INSTALL_DIR/$BINARY_NAME (atomic temp staging + mv)"
    return 0
  fi

  make_temp_dir

  if [ "$norm_tag" = "latest" ]; then
    log_info "Resolving latest release tag for $REPO..."
    resolved_tag=$(resolve_latest_tag)
  else
    resolved_tag="$norm_tag"
  fi

  target_semver="${resolved_tag#v}"

  if [ "$MODE_UPDATE" -eq 1 ] && [ -n "$installed_ver" ]; then
    if [ "$VERSION_EXPLICIT" -eq 1 ]; then
      if [ "$installed_ver" = "$target_semver" ]; then
        log_info "$BINARY_NAME v$installed_ver is already installed at the requested version."
        return 0
      fi
      log_info "Updating $BINARY_NAME v$installed_ver -> v$target_semver..."
    else
      if [ "$installed_ver" = "$target_semver" ]; then
        log_info "$BINARY_NAME v$installed_ver is already up to date."
        return 0
      fi
      if ! semver_is_newer "$target_semver" "$installed_ver"; then
        log_info "$BINARY_NAME v$installed_ver is already newer than latest published release v$target_semver."
        return 0
      fi
      log_info "Upgrading $BINARY_NAME v$installed_ver -> v$target_semver..."
    fi
  elif [ "$MODE_UPDATE" -eq 1 ] && [ -z "$installed_ver" ]; then
    log_info "$BINARY_NAME is not currently installed; performing fresh install of $resolved_tag..."
  else
    log_info "Installing $BINARY_NAME $resolved_tag for $target_triple..."
  fi

  release_base="https://github.com/${REPO}/releases/download/${resolved_tag}"
  archive_path="$TMP_DIR/$archive_name"
  sums_path="$TMP_DIR/SHA256SUMS"

  log_info "Downloading $archive_name and SHA256SUMS..."
  http_download "$release_base/$archive_name" "$archive_path"
  http_download "$release_base/SHA256SUMS" "$sums_path"

  expected_sha=$(extract_expected_sha256 "$sums_path" "$archive_name")
  if [ -z "$expected_sha" ] || ! printf '%s\n' "$expected_sha" | grep -Eq '^[0-9a-f]{64}$'; then
    die "SHA256SUMS does not contain a valid 64-char hex digest for '$archive_name'"
  fi

  actual_sha=$(compute_sha256 "$archive_path")
  if [ "$actual_sha" != "$expected_sha" ]; then
    die "SHA-256 checksum mismatch for '$archive_name'! Expected $expected_sha, got $actual_sha. Aborting before extraction."
  fi
  log_info "Verified SHA-256 checksum ($actual_sha)"

  if [ "$VERIFY_ATTESTATION" -eq 1 ]; then
    if ! command -v gh >/dev/null 2>&1; then
      die "'--verify-attestation' was requested, but GitHub CLI ('gh') is not installed"
    fi
    log_info "Verifying GitHub Artifact Attestation via 'gh attestation verify'..."
    gh attestation verify "$archive_path" --repo "$REPO"
  fi

  extract_dir="$TMP_DIR/extract"
  mkdir -p "$extract_dir"
  tar -xzf "$archive_path" -C "$extract_dir"

  extracted_bin=""
  if [ -f "$extract_dir/$BINARY_NAME" ]; then
    extracted_bin="$extract_dir/$BINARY_NAME"
  else
    for candidate in "$extract_dir"/*/"$BINARY_NAME"; do
      if [ -f "$candidate" ]; then
        extracted_bin="$candidate"
        break
      fi
    done
  fi

  [ -n "$extracted_bin" ] || die "archive '$archive_name' did not contain '$BINARY_NAME' binary"

  atomic_install_binary "$extracted_bin" "$INSTALL_DIR"
  final_ver=$("$INSTALL_DIR/$BINARY_NAME" --version 2>/dev/null || echo "$BINARY_NAME $resolved_tag")
  log_info "Successfully installed $final_ver -> $INSTALL_DIR/$BINARY_NAME"
  check_path_hint
}

main() {
  parse_args "$@"

  if [ "$MODE_UNINSTALL" -eq 1 ]; then
    if [ "$MODE_UPDATE" -eq 1 ] || [ "$MODE_FROM_SOURCE" -eq 1 ]; then
      die "--uninstall cannot be combined with --update or --from-source"
    fi
    run_uninstall
    exit 0
  fi

  if [ "$MODE_FROM_SOURCE" -eq 1 ]; then
    run_from_source
    exit 0
  fi

  run_release_install
}

main "$@"
