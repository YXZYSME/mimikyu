# `mimikyu` (Python / `uvx` CLI & MCP Launcher)

Zero-dependency Python / `uvx` binary launcher for [`mimikyu`](https://github.com/YXZYSME/mimikyu) — fast Rust browser automation, two-tier session/profile isolation, warm background daemon, and 31-tool Model Context Protocol (`MCP`) server over **Chromium (CDP)** and **Firefox (WebDriver BiDi)**.

## Quick Start (`pip` & `uvx`)

```sh
# Install `mimikyu` and `mimikyu-mcp` onto your PATH
pip install mimikyu
mimikyu doctor

# Or run ephemerally with uvx
uvx mimikyu doctor
uvx --from mimikyu mimikyu-mcp
```

## MCP Client Configuration (`uvx`)

```json
{
  "mcpServers": {
    "mimikyu": {
      "command": "uvx",
      "args": ["--from", "mimikyu", "mimikyu-mcp"]
    }
  }
}
```

On first invocation, this package downloads the matching native Rust release archive for your OS/CPU (`aarch64-apple-darwin`, `x86_64-apple-darwin`, `aarch64-unknown-linux-gnu`, `x86_64-unknown-linux-gnu`), verifies its `SHA-256` digest against `SHA256SUMS`, caches the binary locally, and `exec`s `mimikyu` directly.

## License

Dual-licensed under **MIT OR Apache-2.0**. See https://github.com/YXZYSME/mimikyu.
