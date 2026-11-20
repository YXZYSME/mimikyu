# `mimikyu-mcp`

Zero-dependency `npx` launcher for [`mimikyu`](https://github.com/YXZYSME/mimikyu) — the fast Rust browser automation, session/profile isolation, and 33-tool Model Context Protocol (`MCP`) server over **Chromium (CDP)** and **Firefox (WebDriver BiDi)**.

## MCP Client Configuration (Antigravity, Claude Desktop, Cursor, VS Code)

```json
{
  "mcpServers": {
    "mimikyu": {
      "command": "npx",
      "args": ["-y", "mimikyu-mcp"]
    }
  }
}
```

When launched without arguments, `mimikyu-mcp` downloads the matching GitHub Release tarball for your OS/architecture (`darwin-arm64`, `darwin-x64`, `linux-arm64`, `linux-x64`), verifies its `SHA-256` digest against `SHA256SUMS`, caches the binary locally, and starts `mimikyu mcp` over `stdio`.

You can also forward any `mimikyu` CLI subcommand directly:

```sh
npx -y mimikyu-mcp doctor
npx -y mimikyu-mcp --version
```

## License

Dual-licensed under **MIT OR Apache-2.0**. See https://github.com/YXZYSME/mimikyu.
