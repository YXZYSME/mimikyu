import { useMemo, useState } from 'preact/hooks';
import mcpData from '../data/mcp-tools.json';

interface ToolParam {
  name: string;
  type: string;
  required: boolean;
  description: string;
  enum?: string[];
  minimum?: number;
}

interface McpTool {
  name: string;
  namespace: string;
  description: string;
  required: string[];
  params: ToolParam[];
}

type NamespaceFilter = 'all' | 'mimikyu' | 'browser';

const ALL_TOOLS = mcpData.tools as McpTool[];

function buildExamplePayload(tool: McpTool): string {
  const args: Record<string, unknown> = {};
  for (const p of tool.params) {
    if (!p.required && tool.params.length > 3 && p.name !== 'session') continue;
    if (p.enum && p.enum.length > 0) {
      args[p.name] = p.enum[0];
    } else if (p.name === 'url') {
      args[p.name] = 'https://example.com';
    } else if (p.name === 'target') {
      args[p.name] = '@e1';
    } else if (p.name === 'session') {
      args[p.name] = 'demo';
    } else if (p.type === 'boolean') {
      args[p.name] = true;
    } else if (p.type === 'integer' || p.type === 'number') {
      args[p.name] = p.minimum ?? 0;
    } else if (p.type === 'array') {
      args[p.name] = [{ op: 'click', target: '@e1' }];
    } else {
      args[p.name] = `<${p.name}>`;
    }
  }
  return JSON.stringify(
    {
      jsonrpc: '2.0',
      id: 1,
      method: 'tools/call',
      params: {
        name: tool.name,
        arguments: args,
      },
    },
    null,
    2
  );
}

export default function McpExplorer() {
  const [query, setQuery] = useState('');
  const [namespace, setNamespace] = useState<NamespaceFilter>('all');
  const [expanded, setExpanded] = useState<Record<string, boolean>>({
    mimikyu_snapshot: true,
  });
  const [copiedTool, setCopiedTool] = useState<string | null>(null);

  const filtered = useMemo(() => {
    const q = query.trim().toLowerCase();
    return ALL_TOOLS.filter((tool) => {
      if (namespace !== 'all' && tool.namespace !== namespace) return false;
      if (!q) return true;
      if (tool.name.toLowerCase().includes(q)) return true;
      if (tool.description.toLowerCase().includes(q)) return true;
      return tool.params.some(
        (p) => p.name.toLowerCase().includes(q) || p.description.toLowerCase().includes(q)
      );
    });
  }, [query, namespace]);

  const toggleTool = (name: string) => {
    setExpanded((prev) => ({ ...prev, [name]: !prev[name] }));
  };

  const copyPayload = async (tool: McpTool) => {
    try {
      await navigator.clipboard.writeText(buildExamplePayload(tool));
      setCopiedTool(tool.name);
      setTimeout(() => setCopiedTool((cur) => (cur === tool.name ? null : cur)), 1600);
    } catch {
      setCopiedTool(null);
    }
  };

  return (
    <div class="hud-frame overflow-hidden">
      {/* Search + Namespace Filter Toolbar */}
      <div class="flex flex-col gap-3 border-b-2 border-[#563A9E] bg-[#120924] p-4 sm:flex-row sm:items-center sm:justify-between sm:px-6">
        <div class="flex flex-wrap gap-1.5">
          {(
            [
              ['all', `All (${mcpData.total})`],
              ['mimikyu', `mimikyu_* (${mcpData.namespaces.mimikyu})`],
              ['browser', `browser_* (${mcpData.namespaces.browser})`],
            ] as [NamespaceFilter, string][]
          ).map(([id, label]) => (
            <button
              key={id}
              type="button"
              onClick={() => setNamespace(id)}
              class={`cursor-pointer border-2 px-3 py-1.5 font-mono text-xs font-bold transition-colors ${
                namespace === id
                  ? 'border-[#9D80EB] bg-[#22133F] text-[#FFE47A]'
                  : 'border-[#563A9E] bg-[#180D2E] text-[#C9B8F5] hover:border-[#9D80EB]'
              }`}
            >
              {label}
            </button>
          ))}
        </div>

        <div class="relative w-full sm:w-72">
          <input
            type="search"
            value={query}
            onInput={(e) => setQuery((e.target as HTMLInputElement).value)}
            placeholder="Filter tool, param, or action..."
            aria-label={`Search ${mcpData.total} MCP tools`}
            class="w-full border-2 border-[#563A9E] bg-[#090414] px-3 py-1.5 font-mono text-xs text-[#F4EEFF] placeholder-[#C9B8F5]/55 focus:border-[#4CE0B3] focus:outline-none"
          />
        </div>
      </div>

      {/* Status Strip */}
      <div class="flex flex-wrap items-center justify-between gap-2 border-b border-[#563A9E]/50 bg-[#180D2E] px-4 py-2 font-mono text-[11px] text-[#C9B8F5] sm:px-6">
        <span>
          Showing <strong class="text-[#FFE47A]">{filtered.length}</strong> of {mcpData.total} stdio
          JSON-RPC 2.0 tools
        </span>
        <span class="text-[#9D80EB]">
          Protocol {mcpData.protocolVersion} · Registry: {mcpData.registryId}
        </span>
      </div>

      {/* Tool Accordion List */}
      <div class="divide-y divide-[#563A9E]/50">
        {filtered.length === 0 ? (
          <div class="p-8 text-center font-mono text-xs text-[#C9B8F5]">
            No MCP tools match &ldquo;{query}&rdquo;. Try searching for{' '}
            <code class="text-[#FFE47A]">snapshot</code>, <code class="text-[#4CE0B3]">humanize</code>
            , or <code class="text-[#9D80EB]">proxy</code>.
          </div>
        ) : (
          filtered.map((tool) => {
            const isOpen = !!expanded[tool.name];
            const isMimikyuNs = tool.namespace === 'mimikyu';
            return (
              <div key={tool.name} class="bg-[#180D2E] transition-colors hover:bg-[#1E1038]">
                <button
                  type="button"
                  onClick={() => toggleTool(tool.name)}
                  aria-expanded={isOpen}
                  class="flex w-full cursor-pointer items-start justify-between gap-4 px-4 py-3.5 text-left sm:px-6"
                >
                  <div class="min-w-0 flex-1">
                    <div class="flex flex-wrap items-center gap-2">
                      <span
                        class={`border px-1.5 py-0.5 font-mono text-[10px] font-bold uppercase ${
                          isMimikyuNs
                            ? 'border-[#F5B841] bg-[#090414] text-[#FFE47A]'
                            : 'border-[#4CE0B3] bg-[#090414] text-[#9EFCE4]'
                        }`}
                      >
                        {tool.namespace}_*
                      </span>
                      <code class="font-mono text-sm font-bold text-[#F4EEFF]">{tool.name}</code>
                      <span class="font-mono text-[11px] text-[#9D80EB]">
                        ({tool.params.length} {tool.params.length === 1 ? 'param' : 'params'}
                        {tool.required.length > 0 ? ` · ${tool.required.length} req` : ''})
                      </span>
                    </div>
                    <p class="mt-1 text-xs text-[#C9B8F5]">{tool.description}</p>
                  </div>
                  <span class="mt-1 shrink-0 font-mono text-xs font-bold text-[#F5B841]">
                    {isOpen ? '−' : '+'}
                  </span>
                </button>

                {isOpen && (
                  <div class="border-t border-[#563A9E]/50 bg-[#120924] px-4 py-4 sm:px-6">
                    {tool.params.length === 0 ? (
                      <p class="font-mono text-xs text-[#C9B8F5]">
                        No parameters required (<code class="text-[#FFE47A]">{'{}'}</code>).
                      </p>
                    ) : (
                      <div class="overflow-x-auto">
                        <table class="w-full border-collapse text-left font-mono text-xs">
                          <thead>
                            <tr class="border-b border-[#563A9E] text-[11px] uppercase text-[#FFE47A]">
                              <th class="py-2 pr-4">Parameter</th>
                              <th class="py-2 pr-4">Type</th>
                              <th class="py-2 pr-4">Requirement</th>
                              <th class="py-2">Description</th>
                            </tr>
                          </thead>
                          <tbody class="divide-y divide-[#563A9E]/35">
                            {tool.params.map((p) => (
                              <tr key={p.name}>
                                <td class="py-2 pr-4 font-bold text-[#4CE0B3]">{p.name}</td>
                                <td class="py-2 pr-4 text-[#9D80EB]">
                                  {p.type}
                                  {p.enum && (
                                    <span class="ml-1.5 text-[10px] text-[#FFE47A]">
                                      [{p.enum.join(' | ')}]
                                    </span>
                                  )}
                                  {p.minimum !== undefined && (
                                    <span class="ml-1 text-[10px] text-[#C9B8F5]">
                                      (≥{p.minimum})
                                    </span>
                                  )}
                                </td>
                                <td class="py-2 pr-4">
                                  {p.required ? (
                                    <span class="border border-[#FF6B8B] bg-[#FF6B8B]/15 px-1.5 py-0.5 text-[10px] font-bold text-[#FF6B8B]">
                                      REQUIRED
                                    </span>
                                  ) : (
                                    <span class="text-[10px] text-[#C9B8F5]/70">optional</span>
                                  )}
                                </td>
                                <td class="py-2 font-sans text-xs text-[#E8DEFF]">
                                  {p.description}
                                </td>
                              </tr>
                            ))}
                          </tbody>
                        </table>
                      </div>
                    )}

                    <div class="mt-4 flex flex-wrap items-center justify-between gap-2 border-t border-[#563A9E]/40 pt-3">
                      <span class="font-mono text-[11px] text-[#C9B8F5]">
                        Extracted directly from{' '}
                        <code class="text-[#FFE47A]">crates/mimikyu-core/src/mcp/tools.rs</code>
                      </span>
                      <button
                        type="button"
                        onClick={() => copyPayload(tool)}
                        class="hud-btn-ghost cursor-pointer px-3 py-1 font-mono text-[11px] font-semibold text-[#4CE0B3]"
                      >
                        {copiedTool === tool.name
                          ? '✓ Copied JSON-RPC Call'
                          : 'Copy JSON-RPC Call'}
                      </button>
                    </div>
                  </div>
                )}
              </div>
            );
          })
        )}
      </div>
    </div>
  );
}
