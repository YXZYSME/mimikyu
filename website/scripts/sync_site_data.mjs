#!/usr/bin/env node
/**
 * Build-time single-source-of-truth synchronization for mimikyu.saengil.ai.
 *
 * Reads canonical workspace metadata from:
 *   - ../Cargo.toml ([workspace.package])
 *   - ../CHANGELOG.md (latest stamped release date)
 *   - ./src/data/mcp-tools.json (tool inventory & namespaces)
 *   - ./src/data/benchmarks.json (measured suite & benchmark telemetry)
 *   - Live GitHub Releases API (optional safety guard so unreleased main-branch
 *     version bumps never advertise unpublished download links)
 *
 * Writes:
 *   - ./src/data/release.json
 *   - ./public/llms.txt
 *   - ./public/assets/mimikyu-banner.svg
 */

import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);
const WEBSITE_DIR = path.resolve(__dirname, '..');
const REPO_ROOT = path.resolve(WEBSITE_DIR, '..');

const REPO_SLUG = 'YXZYSME/mimikyu';
const DEFAULT_DEMO_RELEASE_TAG = 'v0.2.0';
const REQUIRED_DEMO_ASSET = 'demo-raw-live-browser.mp4';

const CARGO_TOML_PATH = path.join(REPO_ROOT, 'Cargo.toml');
const CHANGELOG_PATH = path.join(REPO_ROOT, 'CHANGELOG.md');
const MCP_TOOLS_PATH = path.join(WEBSITE_DIR, 'src', 'data', 'mcp-tools.json');
const BENCHMARKS_PATH = path.join(WEBSITE_DIR, 'src', 'data', 'benchmarks.json');
const RELEASE_JSON_PATH = path.join(WEBSITE_DIR, 'src', 'data', 'release.json');
const LLMS_TXT_PATH = path.join(WEBSITE_DIR, 'public', 'llms.txt');
const ROOT_BANNER_SVG_PATH = path.join(REPO_ROOT, 'assets', 'mimikyu-banner.svg');
const SITE_BANNER_SVG_PATH = path.join(WEBSITE_DIR, 'public', 'assets', 'mimikyu-banner.svg');

// 5x7 pixel-art glyph bitmaps matching the native 2px grid typeface in mimikyu-banner.svg
const PIXEL_FONT = {
  '0': [' ### ', '#   #', '#  ##', '# # #', '##  #', '#   #', ' ### '],
  '1': ['  #  ', ' ##  ', '  #  ', '  #  ', '  #  ', '  #  ', ' ### '],
  '2': [' ### ', '#   #', '    #', '  ## ', ' #   ', '#    ', '#####'],
  '3': ['#### ', '    #', '    #', ' ### ', '    #', '    #', '#### '],
  '4': ['   # ', '  ## ', ' # # ', '#  # ', '#####', '   # ', '   # '],
  '5': ['#####', '#    ', '#    ', '#### ', '    #', '    #', '#### '],
  '6': [' ### ', '#    ', '#    ', '#### ', '#   #', '#   #', ' ### '],
  '7': ['#####', '    #', '   # ', '  #  ', ' #   ', ' #   ', ' #   '],
  '8': [' ### ', '#   #', '#   #', ' ### ', '#   #', '#   #', ' ### '],
  '9': [' ### ', '#   #', '#   #', ' ####', '    #', '    #', ' ### '],
  v: ['     ', '     ', '#   #', '#   #', '#   #', ' # # ', '  #  '],
  '.': ['  ', '  ', '  ', '  ', '  ', '##', '##'],
  '-': ['     ', '     ', '     ', '#####', '     ', '     ', '     '],
};

function measurePixelText(text, scale = 2, gapCols = 1) {
  let cols = 0;
  for (let i = 0; i < text.length; i += 1) {
    const glyph = PIXEL_FONT[text[i]];
    if (!glyph) continue;
    if (cols > 0) cols += gapCols;
    cols += glyph[0].length;
  }
  return cols * scale;
}

function buildPixelTextPath(text, startX, startY, scale = 2, gapCols = 1) {
  let cursorX = startX;
  const cmds = [];
  for (const ch of text) {
    const glyph = PIXEL_FONT[ch];
    if (!glyph) continue;
    for (let row = 0; row < glyph.length; row += 1) {
      const line = glyph[row];
      let col = 0;
      while (col < line.length) {
        if (line[col] !== '#') {
          col += 1;
          continue;
        }
        let run = 1;
        while (col + run < line.length && line[col + run] === '#') {
          run += 1;
        }
        const rx = cursorX + col * scale;
        const ry = startY + row * scale;
        const rw = run * scale;
        cmds.push(`M${rx} ${ry}h${rw}v${scale}h-${rw}Z`);
        col += run;
      }
    }
    cursorX += (glyph[0].length + gapCols) * scale;
  }
  return cmds.join('');
}

function renderSyncedBannerSvg(baseSvgText, release) {
  const cleaned = baseSvgText.replace(/\s*<g id="mimikyu-dynamic-hud">[\s\S]*?<\/g>\s*/g, '\n');

  const entryNumber = String(release.mcp.total).padStart(3, '0');
  const versionLabel = release.tag.split('-')[0];
  const toolsCount = String(release.mcp.total);

  const versionWidth = measurePixelText(versionLabel);
  const versionX = 830 + Math.floor((96 - versionWidth) / 4) * 2;
  const toolsX = 582;

  const entryPath = buildPixelTextPath(entryNumber, 638, 36);
  const versionPath = buildPixelTextPath(versionLabel, versionX, 36);
  const toolsShadowPath = buildPixelTextPath(toolsCount, toolsX + 2, 218);
  const toolsFgPath = buildPixelTextPath(toolsCount, toolsX, 216);

  const hudGroup = [
    '  <g id="mimikyu-dynamic-hud">',
    '    <rect x="638" y="34" width="76" height="18" fill="#0E071C"/>',
    `    <path fill="#9EFCE4" d="${entryPath}"/>`,
    '    <rect x="832" y="34" width="92" height="18" fill="#0E071C"/>',
    `    <path fill="#9EFCE4" d="${versionPath}"/>`,
    '    <rect x="580" y="214" width="28" height="20" fill="#0E071C"/>',
    `    <path fill="#090414" d="${toolsShadowPath}"/>`,
    `    <path fill="#F4EEFF" d="${toolsFgPath}"/>`,
    '  </g>',
  ].join('\n');

  return cleaned.replace(/<\/svg>\s*$/, `${hudGroup}\n</svg>\n`);
}

function parseWorkspacePackage(tomlText) {
  const lines = tomlText.split(/\r?\n/);
  let inSection = false;
  const values = {};

  for (const rawLine of lines) {
    const line = rawLine.trim();
    if (line.startsWith('[')) {
      inSection = line === '[workspace.package]';
      continue;
    }
    if (!inSection || !line || line.startsWith('#')) {
      continue;
    }
    const match = line.match(/^([a-z0-9_-]+)\s*=\s*"([^"]+)"/);
    if (match) {
      values[match[1]] = match[2];
    }
  }

  return {
    version: values.version || '0.2.0',
    edition: values.edition || '2024',
    rustVersion: values['rust-version'] || '1.88',
    license: values.license || 'MIT OR Apache-2.0',
  };
}

function parseChangelogDate(changelogText, targetVersion) {
  const escaped = targetVersion.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
  const exactRe = new RegExp(`^## \\[${escaped}\\] - (\\d{4}-\\d{2}-\\d{2})`, 'm');
  const exactMatch = changelogText.match(exactRe);
  if (exactMatch) {
    return exactMatch[1];
  }
  const latestMatch = changelogText.match(/^## \[\d+\.\d+\.\d+[^\]]*\] - (\d{4}-\d{2}-\d{2})/m);
  return latestMatch ? latestMatch[1] : '';
}

function readExistingReleaseJson() {
  try {
    return JSON.parse(fs.readFileSync(RELEASE_JSON_PATH, 'utf-8'));
  } catch {
    return null;
  }
}

async function fetchLatestPublishedRelease() {
  if (process.env.SYNC_OFFLINE === '1' || process.env.SYNC_OFFLINE === 'true') {
    return null;
  }

  const headers = {
    Accept: 'application/vnd.github+json',
    'User-Agent': 'mimikyu-website-sync',
  };
  if (process.env.GITHUB_TOKEN) {
    headers.Authorization = `Bearer ${process.env.GITHUB_TOKEN}`;
  }

  const controller = new AbortController();
  const timeoutId = setTimeout(() => controller.abort(), 3500);

  try {
    const res = await fetch(`https://api.github.com/repos/${REPO_SLUG}/releases/latest`, {
      headers,
      signal: controller.signal,
    });
    if (!res.ok) {
      return null;
    }
    const data = await res.json();
    if (!data || data.draft || data.prerelease || typeof data.tag_name !== 'string') {
      return null;
    }
    const assets = Array.isArray(data.assets) ? data.assets.map((a) => a?.name).filter(Boolean) : [];
    const publishedDate =
      typeof data.published_at === 'string' ? data.published_at.slice(0, 10) : '';
    return {
      tag: data.tag_name,
      version: data.tag_name.replace(/^v/, ''),
      date: publishedDate,
      assets,
    };
  } catch {
    return null;
  } finally {
    clearTimeout(timeoutId);
  }
}

function buildLlmsTxt(release, mcpData, benchmarks) {
  const mimikyuTools = mcpData.tools.filter((t) => t.namespace === 'mimikyu');
  const browserTools = mcpData.tools.filter((t) => t.namespace === 'browser');

  const mimikyuList = mimikyuTools
    .map((t) => `- \`${t.name}\`: ${t.description}`)
    .join('\n');
  const browserNames = browserTools.map((t) => `\`${t.name}\``).join(', ');

  const suite = benchmarks.verifiedSuite;
  const token = benchmarks.tokenCompression;
  const metricById = new Map(benchmarks.latencyMetrics.map((m) => [m.id, m]));
  const snapMetric = metricById.get('snapshot');
  const mdMetric = metricById.get('markdown');
  const vctxMetric = metricById.get('virtual_ctx');

  const snapCdp = snapMetric ? snapMetric.chromiumMs.toFixed(1) : '5.0';
  const snapBidi = snapMetric ? snapMetric.firefoxMs.toFixed(1) : '19.0';
  const mdCdp = mdMetric ? mdMetric.chromiumMs.toFixed(1) : '5.4';
  const mdBidi = mdMetric ? mdMetric.firefoxMs.toFixed(1) : '5.5';
  const vctxCdp = vctxMetric ? vctxMetric.chromiumMs.toFixed(1) : '67.1';
  const e2eRuns = suite.e2eRuns ?? suite.e2eTests;

  return `# mimikyu (by Saengil)

> High-speed native Rust agentic browser runtime, two-tier session/profile isolation layer, warm Unix-socket daemon, and ${release.mcp.total}-tool MCP control plane driving local Chromium (CDP) and Firefox (W3C WebDriver BiDi).

- Latest Release: ${release.tag}${release.date ? ` (${release.date})` : ''}
- Release Notes: ${release.releaseUrl}
- Website: https://mimikyu.saengil.ai
- Repository: https://github.com/${REPO_SLUG}
- License: ${release.license}
- Author / Brand: Saengil / YXZYSME
- MCP Registry ID: ${release.mcp.registryId} (MCP Protocol ${release.mcp.protocolVersion})
- MSRV: Rust ${release.rustVersion}+ (Edition ${release.edition})

## Key Capabilities & Measured Performance

- **~${snapCdp} ms semantic \`@eN\` snapshots** (Chromium CDP; \`${snapBidi} ms\` on Firefox BiDi; local \`127.0.0.1\` loopback benchmark medians) across open Shadow DOM and nested/cross-origin iframes.
- **${token.chromium.ratio}x–${token.firefox.ratio}x token compression** (${token.rawTokens} -> ${token.chromium.compressedTokens} est. tokens on Chromium CDP; ${token.rawTokens} -> ${token.firefox.compressedTokens} est. tokens on Firefox BiDi).
- **~${mdCdp} ms Markdown extraction** (\`${mdBidi} ms\` on Firefox BiDi) and **~${vctxCdp} ms warm virtual contexts** (Tier 2 \`VirtualContextPool\`, local loopback benchmark medians).
- **Dual-Engine Native Drivers**: Native WebSocket Chromium DevTools Protocol (\`driver::cdp\`) and W3C WebDriver BiDi (\`driver::bidi\`) behind a strict \`DriverSession\` trait boundary.
- **Managed Browsers & Privacy Vault**: Built-in \`mimikyu browser install\` (SHA-256 verified Chrome-for-Testing & Firefox), \`mimikyu vault launch\`, curated DNS-boundary tracker blocking (\`--block-trackers\`), and headless CDP screencast recording (\`mimikyu record\`).
- **Two-Tier Isolation**: Tier 1 \`fs2\`-locked persistent profiles (\`--profile\`) or self-cleaning ephemeral directories (\`--ephemeral\`), plus Tier 2 warm pooled virtual browser contexts (\`--virtual\`).
- **Coherent Environment Profiles & ProxyRelay**: Built-in \`win11\`, \`macos\`, \`linux\` presets, offline GeoIP alignment (\`--geoip\`), and an embedded local \`127.0.0.1:0\` \`ProxyRelay\` (\`--proxy\`) supporting authenticated HTTP/SOCKS5 upstreams without exposing credentials in browser flags.
- **${release.mcp.total} Turnkey MCP Tools**: ${release.mcp.mimikyu} \`mimikyu_*\` tools (including R3-compliant human-in-the-loop takeover) + ${release.mcp.browser} \`browser_*\` tools over stdio JSON-RPC 2.0.
- **Verified Suite**: ${suite.rustTests} Rust tests (${suite.rustCoreTests} mimikyu-core + ${suite.rustCliTests} mimikyu-cli + ${suite.rustFacadeTests ?? 1} mimikyu; ${suite.clippyWarnings} Clippy warnings) + ${suite.e2eTests} opaque-box Python E2E tests / ${e2eRuns} runs (${suite.workflowsPassed}/${suite.workflowsTotal} agentic workflows passing on both Chromium and Firefox).

## Official Package Registries

- Shell (\`curl\`, SHA-256 + SLSA v1): \`curl -fsSL https://raw.githubusercontent.com/${REPO_SLUG}/main/install.sh | sh\`
- Homebrew (\`YXZYSME/homebrew-tap\`): \`brew tap YXZYSME/tap && brew trust yxzysme/tap && brew install mimikyu\`
- Rust (\`crates.io\` / \`binstall\`): \`cargo install mimikyu-cli\` or \`cargo binstall mimikyu-cli\` or \`cargo add mimikyu\`
- Node (\`npm\` / \`npx\`): \`npm install -g saengil-mimikyu\` or \`npx -y saengil-mimikyu doctor\` or \`npx -y mimikyu-mcp\`
- Python (\`PyPI\` / \`uvx\`): \`pip install mimikyu\` or \`uvx mimikyu doctor\` or \`uvx --from mimikyu mimikyu-mcp\`

## MCP Client Configuration

### npx (zero global install)
\`\`\`json
{
  "mcpServers": {
    "mimikyu": {
      "command": "npx",
      "args": ["-y", "mimikyu-mcp"]
    }
  }
}
\`\`\`

### uvx (zero global install)
\`\`\`json
{
  "mcpServers": {
    "mimikyu": {
      "command": "uvx",
      "args": ["--from", "mimikyu", "mimikyu-mcp"]
    }
  }
}
\`\`\`

### Local binary
\`\`\`json
{
  "mcpServers": {
    "mimikyu": {
      "command": "mimikyu",
      "args": ["mcp", "--engine", "chromium"]
    }
  }
}
\`\`\`

## ${release.mcp.total} MCP Tools

### \`mimikyu_*\` Namespace (${release.mcp.mimikyu} tools)
${mimikyuList}

### \`browser_*\` Namespace (${release.mcp.browser} tools)
- ${browserNames}

## Scope & R3 Compliance Boundary

\`mimikyu\` is strictly a general-purpose AI-agent browser automation, session/profile isolation, and environment consistency runtime. It intentionally excludes any CAPTCHA solver, human-verification challenge bypass, proof-of-work circumvention, or anti-bot gate bypass functionality. Human-in-the-loop takeover (\`mimikyu_request_human_takeover\` / \`mimikyu_resume_from_human\`) pauses automation so a human can complete legitimate login, 2FA, payment, or verification steps directly.

## Documentation URLs

- Home: https://mimikyu.saengil.ai/
- Docs Hub: https://mimikyu.saengil.ai/docs
- Architecture & Scaling Invariants: https://mimikyu.saengil.ai/docs/architecture
- Security Policy & R3 Boundary: https://mimikyu.saengil.ai/docs/security
- Release & Version Control Process: https://mimikyu.saengil.ai/docs/release
- Changelog: https://mimikyu.saengil.ai/docs/changelog
- Contributing Guide: https://mimikyu.saengil.ai/docs/contributing
`;
}

async function main() {
  const cargoToml = fs.readFileSync(CARGO_TOML_PATH, 'utf-8');
  const changelog = fs.readFileSync(CHANGELOG_PATH, 'utf-8');
  const mcpData = JSON.parse(fs.readFileSync(MCP_TOOLS_PATH, 'utf-8'));
  const benchmarks = JSON.parse(fs.readFileSync(BENCHMARKS_PATH, 'utf-8'));
  const existingRelease = readExistingReleaseJson();

  const workspacePkg = parseWorkspacePackage(cargoToml);
  const liveRelease = await fetchLatestPublishedRelease();

  let version = workspacePkg.version;
  let versionSource = 'Cargo.toml';

  if (process.env.SYNC_TARGET_VERSION) {
    version = process.env.SYNC_TARGET_VERSION.replace(/^v/, '');
    versionSource = 'SYNC_TARGET_VERSION';
  } else if (liveRelease) {
    // Safety guard: never advertise an unreleased Cargo.toml bump on mimikyu.saengil.ai
    // before the corresponding GitHub Release is published.
    version = liveRelease.version;
    versionSource = 'GitHub Releases API (latest published)';
    if (liveRelease.version !== workspacePkg.version) {
      console.log(
        `[sync_site_data] Safety hold: Cargo.toml is ${workspacePkg.version}, keeping published release v${liveRelease.version}`
      );
    }
  }

  const tag = `v${version}`;
  const date =
    parseChangelogDate(changelog, version) ||
    liveRelease?.date ||
    existingRelease?.date ||
    '';

  let demoReleaseTag =
    process.env.DEMO_RELEASE_TAG ||
    existingRelease?.demoReleaseTag ||
    DEFAULT_DEMO_RELEASE_TAG;

  if (!process.env.DEMO_RELEASE_TAG && liveRelease && liveRelease.tag === tag) {
    if (liveRelease.assets.includes(REQUIRED_DEMO_ASSET)) {
      demoReleaseTag = liveRelease.tag;
    }
  }

  const mimikyuCount = mcpData.tools.filter((t) => t.namespace === 'mimikyu').length;
  const browserCount = mcpData.tools.filter((t) => t.namespace === 'browser').length;
  const totalTools = mcpData.tools.length;

  const releasePayload = {
    version,
    tag,
    date,
    rustVersion: workspacePkg.rustVersion,
    edition: workspacePkg.edition,
    license: workspacePkg.license,
    releaseUrl: `https://github.com/${REPO_SLUG}/releases/tag/${tag}`,
    demoReleaseTag,
    demoBaseUrl: `https://github.com/${REPO_SLUG}/releases/download/${demoReleaseTag}`,
    mcp: {
      total: totalTools,
      mimikyu: mimikyuCount,
      browser: browserCount,
      protocolVersion: mcpData.protocolVersion || '2024-11-05',
      registryId: mcpData.registryId || 'io.github.YXZYSME/mimikyu',
    },
  };

  fs.writeFileSync(RELEASE_JSON_PATH, `${JSON.stringify(releasePayload, null, 2)}\n`, 'utf-8');
  fs.writeFileSync(LLMS_TXT_PATH, buildLlmsTxt(releasePayload, mcpData, benchmarks), 'utf-8');

  if (fs.existsSync(ROOT_BANNER_SVG_PATH)) {
    const baseBannerSvg = fs.readFileSync(ROOT_BANNER_SVG_PATH, 'utf-8');
    const syncedBannerSvg = renderSyncedBannerSvg(baseBannerSvg, releasePayload);
    fs.writeFileSync(SITE_BANNER_SVG_PATH, syncedBannerSvg, 'utf-8');
  }

  console.log(
    `[sync_site_data] Synced ${tag} (${versionSource}), ${totalTools} MCP tools (${mimikyuCount} mimikyu_* + ${browserCount} browser_*), demos=${demoReleaseTag}`
  );
}

main().catch((err) => {
  console.error('[sync_site_data] fatal:', err);
  process.exit(1);
});
