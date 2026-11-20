# `saengil-mimikyu`

Official **Saengil** CLI & MCP binary distribution for [`mimikyu`](https://github.com/YXZYSME/mimikyu) — fast Rust browser automation, two-tier session/profile isolation, background daemon, and 31-tool MCP server over **Chromium (CDP)** and **Firefox (WebDriver BiDi)**.

## Install & Usage

```sh
# Install the `mimikyu` CLI binary globally
npm install -g saengil-mimikyu
mimikyu doctor

# Or run directly without global install via npx
npx -y saengil-mimikyu doctor
npx -y saengil-mimikyu mcp
```

On first run, `saengil-mimikyu` downloads the official GitHub Release tarball for your OS/CPU (`darwin-arm64`, `darwin-x64`, `linux-arm64`, `linux-x64`), verifies its `SHA-256` digest against `SHA256SUMS`, caches the native Rust `mimikyu` binary locally, and forwards all CLI arguments verbatim.

## License

Dual-licensed under **MIT OR Apache-2.0**. See https://github.com/YXZYSME/mimikyu.
