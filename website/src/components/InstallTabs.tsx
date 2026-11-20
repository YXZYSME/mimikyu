import { useState } from 'preact/hooks';
import release from '../data/release.json';

interface RegistryOption {
  id: string;
  label: string;
  sublabel: string;
  badge: string;
  accent: string;
  registryUrl: string;
  registryName: string;
  notes: string;
  variants?: { id: string; label: string; code: string }[];
  code: string;
}

const REGISTRIES: RegistryOption[] = [
  {
    id: 'shell',
    label: 'Shell (curl)',
    sublabel: 'macOS & Linux',
    badge: 'SHA-256 + SLSA v1',
    accent: '#F5B841',
    registryUrl: 'https://github.com/YXZYSME/mimikyu/releases',
    registryName: 'GitHub Releases',
    notes:
      'Downloads the prebuilt native Rust binary, verifies SHA256SUMS before extraction, and installs atomically to ~/.local/bin.',
    variants: [
      {
        id: 'install',
        label: 'Install Latest',
        code: 'curl -fsSL https://raw.githubusercontent.com/YXZYSME/mimikyu/main/install.sh | sh',
      },
      {
        id: 'update',
        label: 'Atomic Update',
        code: 'curl -fsSL https://raw.githubusercontent.com/YXZYSME/mimikyu/main/install.sh | sh -s -- --update',
      },
      {
        id: 'attest',
        label: 'Verify SLSA Attestation',
        code: `curl -fsSL https://raw.githubusercontent.com/YXZYSME/mimikyu/main/install.sh | sh -s -- --version ${release.tag} --verify-attestation`,
      },
    ],
    code: 'curl -fsSL https://raw.githubusercontent.com/YXZYSME/mimikyu/main/install.sh | sh',
  },
  {
    id: 'homebrew',
    label: 'Homebrew',
    sublabel: 'YXZYSME/tap',
    badge: 'macOS & Linux',
    accent: '#F5B841',
    registryUrl: 'https://github.com/YXZYSME/homebrew-tap',
    registryName: 'YXZYSME/homebrew-tap',
    notes:
      'Official custom Homebrew tap. Run `brew trust yxzysme/tap` once on Homebrew 5+ before installing.',
    code: `brew tap YXZYSME/tap
brew trust yxzysme/tap
brew install mimikyu`,
  },
  {
    id: 'rust',
    label: 'Rust (crates.io)',
    sublabel: 'cargo / binstall',
    badge: 'MSRV 1.88+',
    accent: '#FF6B8B',
    registryUrl: 'https://crates.io/crates/mimikyu',
    registryName: 'crates.io/crates/mimikyu',
    notes:
      'Published as `mimikyu` (library facade), `mimikyu-cli` (CLI/daemon/MCP binary), and `mimikyu-core` (core runtime).',
    variants: [
      {
        id: 'binstall',
        label: 'cargo-binstall (prebuilt)',
        code: 'cargo binstall mimikyu-cli',
      },
      {
        id: 'install',
        label: 'cargo install (source)',
        code: 'cargo install mimikyu-cli',
      },
      {
        id: 'lib',
        label: 'Embed Rust Library',
        code: 'cargo add mimikyu',
      },
    ],
    code: 'cargo binstall mimikyu-cli',
  },
  {
    id: 'node',
    label: 'Node (npm)',
    sublabel: 'saengil-mimikyu',
    badge: 'Zero JS Deps',
    accent: '#4CE0B3',
    registryUrl: 'https://www.npmjs.com/package/saengil-mimikyu',
    registryName: 'npmjs.com/package/saengil-mimikyu',
    notes:
      'Zero-dependency launchers that download and SHA-256-verify the official native Rust release binary on first run.',
    variants: [
      {
        id: 'saengil',
        label: 'saengil-mimikyu (Official)',
        code: `npm install -g saengil-mimikyu
mimikyu doctor`,
      },
      {
        id: 'npx',
        label: 'npx Ephemeral',
        code: 'npx -y saengil-mimikyu doctor',
      },
      {
        id: 'alias',
        label: 'mimikyu-cli Alias',
        code: `npm install -g mimikyu-cli
mimikyu --version`,
      },
    ],
    code: `npm install -g saengil-mimikyu
mimikyu doctor`,
  },
  {
    id: 'python',
    label: 'Python (PyPI)',
    sublabel: 'pip / uvx',
    badge: 'Zero Python Deps',
    accent: '#9D80EB',
    registryUrl: 'https://pypi.org/project/mimikyu/',
    registryName: 'pypi.org/project/mimikyu',
    notes:
      'Exposes both `mimikyu` and `mimikyu-mcp` console scripts with zero third-party Python dependencies.',
    variants: [
      {
        id: 'pip',
        label: 'pip install',
        code: `pip install mimikyu
mimikyu doctor`,
      },
      {
        id: 'uvx',
        label: 'uvx (Zero Install)',
        code: `uvx mimikyu doctor
uvx --from mimikyu mimikyu-mcp`,
      },
    ],
    code: `pip install mimikyu
mimikyu doctor`,
  },
  {
    id: 'mcp',
    label: 'MCP JSON',
    sublabel: `${release.mcp.total} Stdio Tools`,
    badge: `MCP ${release.mcp.protocolVersion}`,
    accent: '#9D80EB',
    registryUrl:
      'https://registry.modelcontextprotocol.io/v0/servers?search=io.github.YXZYSME/mimikyu',
    registryName: 'io.github.YXZYSME/mimikyu',
    notes:
      'Drop into Claude Desktop, Cursor, Windsurf, or any MCP 2024-11-05 client. Registered on the Official MCP Registry.',
    variants: [
      {
        id: 'npx-mcp',
        label: 'npx (Zero Install)',
        code: `{
  "mcpServers": {
    "mimikyu": {
      "command": "npx",
      "args": ["-y", "mimikyu-mcp"]
    }
  }
}`,
      },
      {
        id: 'uvx-mcp',
        label: 'uvx (Zero Install)',
        code: `{
  "mcpServers": {
    "mimikyu": {
      "command": "uvx",
      "args": ["--from", "mimikyu", "mimikyu-mcp"]
    }
  }
}`,
      },
      {
        id: 'local-mcp',
        label: 'Local Binary',
        code: `{
  "mcpServers": {
    "mimikyu": {
      "command": "mimikyu",
      "args": ["mcp", "--engine", "chromium"]
    }
  }
}`,
      },
    ],
    code: `{
  "mcpServers": {
    "mimikyu": {
      "command": "npx",
      "args": ["-y", "mimikyu-mcp"]
    }
  }
}`,
  },
];

export default function InstallTabs() {
  const [activeTabId, setActiveTabId] = useState<string>('shell');
  const [variantByTab, setVariantByTab] = useState<Record<string, string>>({});
  const [copied, setCopied] = useState(false);

  const current = REGISTRIES.find((r) => r.id === activeTabId) || REGISTRIES[0];
  const activeVariantId =
    variantByTab[current.id] || (current.variants ? current.variants[0].id : '');
  const activeCode =
    current.variants?.find((v) => v.id === activeVariantId)?.code || current.code;

  const copyCode = async () => {
    try {
      await navigator.clipboard.writeText(activeCode);
      setCopied(true);
      setTimeout(() => setCopied(false), 1800);
    } catch {
      setCopied(false);
    }
  };

  return (
    <div class="hud-frame overflow-hidden">
      {/* Ecosystem Tabs */}
      <div
        role="tablist"
        aria-label="Package registries"
        class="grid grid-cols-2 border-b-2 border-[#563A9E] bg-[#120924] sm:grid-cols-3 lg:grid-cols-6"
      >
        {REGISTRIES.map((tab) => {
          const selected = tab.id === current.id;
          return (
            <button
              key={tab.id}
              type="button"
              role="tab"
              aria-selected={selected}
              onClick={() => {
                setActiveTabId(tab.id);
                setCopied(false);
              }}
              class={`cursor-pointer border-r border-b border-[#563A9E]/60 px-3 py-3 text-left font-mono transition-colors ${
                selected
                  ? 'bg-[#22133F] text-[#FFE47A] shadow-[inset_0_-3px_0_0_#F5B841]'
                  : 'bg-[#120924] text-[#C9B8F5] hover:bg-[#180D2E] hover:text-[#F4EEFF]'
              }`}
            >
              <div class="text-xs font-bold">{tab.label}</div>
              <div class="mt-0.5 text-[10px] text-[#9D80EB]">{tab.sublabel}</div>
            </button>
          );
        })}
      </div>

      {/* Active Registry Content */}
      <div class="p-5 sm:p-6">
        <div class="flex flex-wrap items-center justify-between gap-3">
          <div class="flex flex-wrap items-center gap-2">
            <span
              class="border px-2 py-0.5 font-mono text-[11px] font-bold uppercase tracking-wider"
              style={{ borderColor: current.accent, color: current.accent }}
            >
              {current.badge}
            </span>
            <a
              href={current.registryUrl}
              target="_blank"
              rel="noopener noreferrer"
              class="font-mono text-xs text-[#4CE0B3] underline decoration-[#4CE0B3]/40 underline-offset-4 hover:text-[#9EFCE4]"
            >
              {current.registryName} ↗
            </a>
          </div>

          {current.variants && (
            <div class="flex flex-wrap gap-1.5">
              {current.variants.map((v) => {
                const isSelected = v.id === activeVariantId;
                return (
                  <button
                    key={v.id}
                    type="button"
                    onClick={() => {
                      setVariantByTab((prev) => ({ ...prev, [current.id]: v.id }));
                      setCopied(false);
                    }}
                    class={`cursor-pointer border px-2.5 py-1 font-mono text-[11px] font-semibold transition-colors ${
                      isSelected
                        ? 'border-[#4CE0B3] bg-[#22133F] text-[#9EFCE4]'
                        : 'border-[#563A9E] bg-[#120924] text-[#C9B8F5] hover:border-[#9D80EB]'
                    }`}
                  >
                    {v.label}
                  </button>
                );
              })}
            </div>
          )}
        </div>

        {/* Code Box */}
        <div class="relative mt-4 border-2 border-[#563A9E] bg-[#090414] p-4">
          <button
            type="button"
            onClick={copyCode}
            class="hud-btn-gold absolute top-3 right-3 cursor-pointer px-3 py-1 font-mono text-[11px] font-extrabold uppercase"
          >
            {copied ? '✓ Copied' : 'Copy'}
          </button>
          <pre class="overflow-x-auto pr-24 font-mono text-xs leading-relaxed text-[#F4EEFF] sm:text-sm">
            <code>{activeCode}</code>
          </pre>
        </div>

        <p class="mt-3 text-xs leading-relaxed text-[#C9B8F5]">{current.notes}</p>
      </div>
    </div>
  );
}
