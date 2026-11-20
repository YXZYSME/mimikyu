# typed: strict
# frozen_string_literal: true

# Homebrew Tap Formula Template for `mimikyu`
#
# How `.github/workflows/release.yml` updates this formula on every `vX.Y.Z` tag:
#   1. The `build` matrix compiles release tarballs (`mimikyu-<target>.tar.gz`) for:
#      - `aarch64-apple-darwin`      (macOS Apple Silicon)
#      - `x86_64-apple-darwin`       (macOS Intel)
#      - `aarch64-unknown-linux-gnu` (Linux ARM64)
#      - `x86_64-unknown-linux-gnu`  (Linux x86_64)
#   2. The `publish` job computes `SHA256SUMS` across all 4 tarballs and renders a
#      production-ready `mimikyu.rb` release asset by substituting:
#      - `YXZYSME/mimikyu`                     -> `${GITHUB_REPOSITORY}`
#      - `version "0.1.0"`                     -> `version "${RELEASE_VERSION}"`
#      - `/releases/download/v0.2.0/`          -> `/releases/download/v${RELEASE_VERSION}/`
#      - `PLACEHOLDER_SHA256_DARWIN_ARM64`     -> SHA-256 of `mimikyu-aarch64-apple-darwin.tar.gz`
#      - `PLACEHOLDER_SHA256_DARWIN_X86_64`    -> SHA-256 of `mimikyu-x86_64-apple-darwin.tar.gz`
#      - `PLACEHOLDER_SHA256_LINUX_ARM64`      -> SHA-256 of `mimikyu-aarch64-unknown-linux-gnu.tar.gz`
#      - `PLACEHOLDER_SHA256_LINUX_X86_64`     -> SHA-256 of `mimikyu-x86_64-unknown-linux-gnu.tar.gz`
#   3. Copy the rendered `mimikyu.rb` from the GitHub Release assets into `Formula/mimikyu.rb`
#      in your `homebrew-tap` repository (or configure `HOMEBREW_TAP_REPO` + `HOMEBREW_TAP_TOKEN`
#      repository secrets to push automatically).
#
# Users then install & upgrade via:
#   brew tap YXZYSME/tap
#   brew install mimikyu
#   brew upgrade mimikyu

class Mimikyu < Formula
  desc "Fast Rust agentic browser runtime, isolation layer, and CLI/Daemon/MCP toolkit"
  homepage "https://github.com/YXZYSME/mimikyu"
  version "0.2.0"
  license any_of: ["MIT", "Apache-2.0"]

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/YXZYSME/mimikyu/releases/download/v0.2.0/mimikyu-aarch64-apple-darwin.tar.gz"
      sha256 "PLACEHOLDER_SHA256_DARWIN_ARM64"
    else
      url "https://github.com/YXZYSME/mimikyu/releases/download/v0.2.0/mimikyu-x86_64-apple-darwin.tar.gz"
      sha256 "PLACEHOLDER_SHA256_DARWIN_X86_64"
    end
  end

  on_linux do
    if Hardware::CPU.arm?
      url "https://github.com/YXZYSME/mimikyu/releases/download/v0.2.0/mimikyu-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "PLACEHOLDER_SHA256_LINUX_ARM64"
    else
      url "https://github.com/YXZYSME/mimikyu/releases/download/v0.2.0/mimikyu-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "PLACEHOLDER_SHA256_LINUX_X86_64"
    end
  end

  def install
    bin.install "mimikyu"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/mimikyu --version")
    assert_match "doctor", shell_output("#{bin}/mimikyu doctor --help")
  end
end
