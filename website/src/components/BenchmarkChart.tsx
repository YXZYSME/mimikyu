import { useState } from 'preact/hooks';
import benchmarks from '../data/benchmarks.json';

type ViewMode = 'latency' | 'tokens' | 'memory';
type EngineFilter = 'both' | 'chromium' | 'firefox';

export default function BenchmarkChart() {
  const [view, setView] = useState<ViewMode>('latency');
  const [engine, setEngine] = useState<EngineFilter>('both');
  const [copiedCmd, setCopiedCmd] = useState(false);

  const maxLatency = Math.max(
    ...benchmarks.latencyMetrics.flatMap((m) => [
      engine === 'firefox' ? 0 : m.chromiumMs,
      engine === 'chromium' ? 0 : m.firefoxMs,
    ]),
    1
  );

  const copyBenchCmd = async () => {
    try {
      await navigator.clipboard.writeText(benchmarks.command);
      setCopiedCmd(true);
      setTimeout(() => setCopiedCmd(false), 1800);
    } catch {
      setCopiedCmd(false);
    }
  };

  return (
    <div class="hud-frame overflow-hidden">
      {/* Control Bar */}
      <div class="flex flex-wrap items-center justify-between gap-3 border-b-2 border-[#563A9E] bg-[#120924] px-4 py-3 sm:px-6">
        <div class="flex flex-wrap gap-1.5">
          {(
            [
              ['latency', 'Latency (ms)'],
              ['tokens', 'Token Compression'],
              ['memory', 'Memory RSS (MB)'],
            ] as [ViewMode, string][]
          ).map(([id, label]) => (
            <button
              key={id}
              type="button"
              onClick={() => setView(id)}
              class={`cursor-pointer border-2 px-3 py-1.5 font-mono text-xs font-bold transition-colors ${
                view === id
                  ? 'border-[#F5B841] bg-[#22133F] text-[#FFE47A]'
                  : 'border-[#563A9E] bg-[#180D2E] text-[#C9B8F5] hover:border-[#9D80EB]'
              }`}
            >
              {label}
            </button>
          ))}
        </div>

        <div class="flex items-center gap-1.5 font-mono text-xs">
          <span class="mr-1 hidden text-[11px] text-[#C9B8F5] sm:inline">Engine:</span>
          {(
            [
              ['both', 'Both'],
              ['chromium', 'Chromium CDP'],
              ['firefox', 'Firefox BiDi'],
            ] as [EngineFilter, string][]
          ).map(([id, label]) => (
            <button
              key={id}
              type="button"
              onClick={() => setEngine(id)}
              class={`cursor-pointer border px-2.5 py-1 text-[11px] font-semibold transition-colors ${
                engine === id
                  ? 'border-[#4CE0B3] bg-[#22133F] text-[#9EFCE4]'
                  : 'border-[#563A9E] bg-[#180D2E] text-[#C9B8F5] hover:border-[#9D80EB]'
              }`}
            >
              {label}
            </button>
          ))}
        </div>
      </div>

      {/* Legend */}
      <div class="flex flex-wrap items-center justify-between gap-3 border-b border-[#563A9E]/50 bg-[#180D2E] px-4 py-2.5 font-mono text-[11px] sm:px-6">
        <div class="flex flex-wrap items-center gap-4">
          {engine !== 'firefox' && (
            <span class="inline-flex items-center gap-2 text-[#FFE47A]">
              <span class="inline-block h-2.5 w-2.5 bg-[#F5B841]" />
              Chromium (WebSocket CDP)
            </span>
          )}
          {engine !== 'chromium' && (
            <span class="inline-flex items-center gap-2 text-[#9EFCE4]">
              <span class="inline-block h-2.5 w-2.5 bg-[#4CE0B3]" />
              Firefox (W3C WebDriver BiDi)
            </span>
          )}
        </div>
        <span class="text-[#C9B8F5]">
          Median · {benchmarks.verifiedSuite.workflowsPassed}/{benchmarks.verifiedSuite.workflowsTotal}{' '}
          agentic workflows (100%)
        </span>
      </div>

      {/* Visualization Body */}
      <div class="p-5 sm:p-6">
        {view === 'latency' && (
          <div class="space-y-5">
            {benchmarks.latencyMetrics.map((metric) => {
              const chromPct = Math.max(2, (metric.chromiumMs / maxLatency) * 100);
              const ffPct = Math.max(2, (metric.firefoxMs / maxLatency) * 100);
              return (
                <div
                  key={metric.id}
                  class={`border p-3.5 ${
                    metric.highlight
                      ? 'border-[#F5B841]/60 bg-[#22133F]/75'
                      : 'border-[#563A9E]/60 bg-[#120924]'
                  }`}
                >
                  <div class="mb-2.5 flex flex-wrap items-baseline justify-between gap-2">
                    <div>
                      <span class="font-mono text-sm font-bold text-[#F4EEFF]">{metric.label}</span>
                      <span class="ml-2 text-xs text-[#C9B8F5]">{metric.detail}</span>
                    </div>
                    <div class="font-mono text-xs">
                      {engine !== 'firefox' && (
                        <span class="font-bold text-[#FFE47A]">
                          CDP {metric.chromiumMs.toFixed(1)} ms
                        </span>
                      )}
                      {engine === 'both' && <span class="mx-2 text-[#563A9E]">/</span>}
                      {engine !== 'chromium' && (
                        <span class="font-bold text-[#9EFCE4]">
                          BiDi {metric.firefoxMs.toFixed(1)} ms
                        </span>
                      )}
                    </div>
                  </div>

                  <div class="space-y-2">
                    {engine !== 'firefox' && (
                      <div class="flex items-center gap-3">
                        <span class="w-16 shrink-0 font-mono text-[10px] uppercase text-[#F5B841]">
                          CDP
                        </span>
                        <div class="h-3.5 flex-1 bg-[#090414] ring-1 ring-[#563A9E]/60">
                          <div
                            class="h-full bg-[#F5B841] transition-all duration-300"
                            style={{ width: `${chromPct}%` }}
                          />
                        </div>
                      </div>
                    )}
                    {engine !== 'chromium' && (
                      <div class="flex items-center gap-3">
                        <span class="w-16 shrink-0 font-mono text-[10px] uppercase text-[#4CE0B3]">
                          BiDi
                        </span>
                        <div class="h-3.5 flex-1 bg-[#090414] ring-1 ring-[#563A9E]/60">
                          <div
                            class="h-full bg-[#4CE0B3] transition-all duration-300"
                            style={{ width: `${ffPct}%` }}
                          />
                        </div>
                      </div>
                    )}
                  </div>
                </div>
              );
            })}
          </div>
        )}

        {view === 'tokens' && (
          <div class="grid gap-5 md:grid-cols-2">
            {/* Raw Baseline */}
            <div class="border-2 border-[#563A9E] bg-[#120924] p-5 md:col-span-2">
              <div class="flex flex-wrap items-center justify-between gap-2 font-mono">
                <span class="text-xs font-bold uppercase tracking-wider text-[#C9B8F5]">
                  Raw DOM Baseline (Uncompressed)
                </span>
                <span class="text-lg font-extrabold text-[#FF6B8B]">
                  {benchmarks.tokenCompression.rawTokens} est. tokens
                </span>
              </div>
              <div class="mt-3 h-4 w-full bg-[#FF6B8B]/30 ring-1 ring-[#FF6B8B]">
                <div class="h-full w-full bg-[#FF6B8B]/75" />
              </div>
            </div>

            {engine !== 'firefox' && (
              <div class="border-2 border-[#F5B841] bg-[#22133F] p-5">
                <div class="flex items-baseline justify-between font-mono">
                  <span class="text-xs font-bold uppercase text-[#FFE47A]">Chromium (CDP)</span>
                  <span class="text-2xl font-extrabold text-[#FFE47A]">
                    {benchmarks.tokenCompression.chromium.ratio}×
                  </span>
                </div>
                <p class="mt-1 font-mono text-xs text-[#C9B8F5]">
                  {benchmarks.tokenCompression.rawTokens} →{' '}
                  <strong class="text-[#F4EEFF]">
                    {benchmarks.tokenCompression.chromium.compressedTokens} est. tokens
                  </strong>{' '}
                  (-{benchmarks.tokenCompression.chromium.reductionPct}%)
                </p>
                <div class="mt-4 h-4 w-full bg-[#090414] ring-1 ring-[#563A9E]">
                  <div
                    class="h-full bg-[#F5B841]"
                    style={{
                      width: `${(benchmarks.tokenCompression.chromium.compressedTokens / benchmarks.tokenCompression.rawTokens) * 100}%`,
                    }}
                  />
                </div>
              </div>
            )}

            {engine !== 'chromium' && (
              <div class="border-2 border-[#4CE0B3] bg-[#22133F] p-5">
                <div class="flex items-baseline justify-between font-mono">
                  <span class="text-xs font-bold uppercase text-[#9EFCE4]">
                    Firefox (WebDriver BiDi)
                  </span>
                  <span class="text-2xl font-extrabold text-[#9EFCE4]">
                    {benchmarks.tokenCompression.firefox.ratio}×
                  </span>
                </div>
                <p class="mt-1 font-mono text-xs text-[#C9B8F5]">
                  {benchmarks.tokenCompression.rawTokens} →{' '}
                  <strong class="text-[#F4EEFF]">
                    {benchmarks.tokenCompression.firefox.compressedTokens} est. tokens
                  </strong>{' '}
                  (-{benchmarks.tokenCompression.firefox.reductionPct}%)
                </p>
                <div class="mt-4 h-4 w-full bg-[#090414] ring-1 ring-[#563A9E]">
                  <div
                    class="h-full bg-[#4CE0B3]"
                    style={{
                      width: `${(benchmarks.tokenCompression.firefox.compressedTokens / benchmarks.tokenCompression.rawTokens) * 100}%`,
                    }}
                  />
                </div>
              </div>
            )}
          </div>
        )}

        {view === 'memory' && (
          <div class="space-y-5">
            <div class="border-2 border-[#563A9E] bg-[#120924] p-5">
              <div class="mb-4 flex flex-wrap items-center justify-between gap-2 font-mono">
                <span class="text-sm font-bold text-[#F4EEFF]">{benchmarks.memory.label}</span>
                <span class="text-xs text-[#C9B8F5]">Full browser + native Rust daemon tree</span>
              </div>
              <div class="space-y-4">
                {engine !== 'firefox' && (
                  <div>
                    <div class="mb-1.5 flex justify-between font-mono text-xs">
                      <span class="font-bold text-[#FFE47A]">Chromium (CDP)</span>
                      <span class="font-extrabold text-[#FFE47A]">
                        {benchmarks.memory.chromiumMb} MB
                      </span>
                    </div>
                    <div class="h-5 w-full bg-[#090414] ring-1 ring-[#563A9E]">
                      <div
                        class="h-full bg-[#F5B841]"
                        style={{
                          width: `${(benchmarks.memory.chromiumMb / benchmarks.memory.firefoxMb) * 100}%`,
                        }}
                      />
                    </div>
                  </div>
                )}
                {engine !== 'chromium' && (
                  <div>
                    <div class="mb-1.5 flex justify-between font-mono text-xs">
                      <span class="font-bold text-[#9EFCE4]">Firefox (WebDriver BiDi)</span>
                      <span class="font-extrabold text-[#9EFCE4]">
                        {benchmarks.memory.firefoxMb} MB
                      </span>
                    </div>
                    <div class="h-5 w-full bg-[#090414] ring-1 ring-[#563A9E]">
                      <div class="h-full w-full bg-[#4CE0B3]" />
                    </div>
                  </div>
                )}
              </div>
            </div>
          </div>
        )}

        {/* Reproducible CLI footer */}
        <div class="mt-6 flex flex-wrap items-center justify-between gap-3 border-t border-[#563A9E]/70 pt-4">
          <div class="min-w-0 flex-1">
            <div class="font-mono text-[11px] uppercase tracking-wider text-[#9D80EB]">
              Reproduce Locally ({benchmarks.harness})
            </div>
            <code class="mt-1 block overflow-x-auto whitespace-nowrap font-mono text-xs text-[#F4EEFF]">
              $ {benchmarks.command}
            </code>
          </div>
          <button
            type="button"
            onClick={copyBenchCmd}
            class="hud-btn-ghost cursor-pointer px-3 py-1.5 font-mono text-xs font-semibold text-[#FFE47A]"
          >
            {copiedCmd ? '✓ Copied' : 'Copy Command'}
          </button>
        </div>
      </div>
    </div>
  );
}
