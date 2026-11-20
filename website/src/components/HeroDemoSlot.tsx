import { useEffect, useState } from 'preact/hooks';

interface Props {
  baseUrl: string;
}

const QUICK_INSTALL_CMD =
  'curl -fsSL https://raw.githubusercontent.com/YXZYSME/mimikyu/main/install.sh | sh';

interface DemoStep {
  id: string;
  label: string;
  timing: string;
  command: string;
  output: string[];
  badge: string;
  accent: string;
}

const DEMO_STEPS: DemoStep[] = [
  {
    id: 'session',
    label: '01 · Virtual Session',
    timing: '67.1 ms',
    badge: 'TIER 2 · VIRTUAL POOL',
    accent: '#9D80EB',
    command:
      'mimikyu --session demo session new --virtual --preset macos --viewport 1280x720 --block-media --json',
    output: [
      '{',
      '  "ok": true,',
      '  "data": { "session": "demo", "engine": "chromium", "isolation": "virtual_ctx", "elapsed_ms": 67.1 }',
      '}',
    ],
  },
  {
    id: 'snapshot',
    label: '02 · @eN Snapshot',
    timing: '5.0 ms',
    badge: '2.79× COMPRESSION',
    accent: '#F5B841',
    command: 'mimikyu --session demo snapshot --interactive-only',
    output: [
      '[document "Saengil Workspace — Catalog"]',
      '  @e1  textbox   "Search packages"  value=""',
      '  @e2  combobox  "Engine"           value="chromium"',
      '  @e3  button    "Apply Filter"     enabled',
      '  @e4  link      "Inspect #031"     href="/entries/031"',
      '# 347 est. tokens (down from 967 raw DOM tokens · 5.0 ms)',
    ],
  },
  {
    id: 'batch',
    label: '03 · Kinematic Batch',
    timing: '204.2 ms',
    badge: 'CHOKEPOINT INPUT',
    accent: '#4CE0B3',
    command:
      'mimikyu --session demo batch --ops \'[{"op":"fill","target":"@e1","value":"bidi"},{"op":"click","target":"@e3","humanize":true},{"op":"wait","idle":true}]\'',
    output: [
      '✓ fill   @e1 "bidi"                  (isolated ChokepointInput)',
      '✓ click  @e3 --humanize              (viewport-clamped trajectory)',
      '✓ wait   --idle                      (network quiet + layout stable)',
      '# 3/3 atomic ops succeeded in 1 daemon round trip (204.2 ms)',
    ],
  },
  {
    id: 'extract',
    label: '04 · Markdown Extract',
    timing: '5.4 ms',
    badge: 'ZERO RUNTIME BLOAT',
    accent: '#4CE0B3',
    command: 'mimikyu --session demo extract markdown --json',
    output: [
      '## Filtered Entries (1 match)',
      '| ID | Engine | Protocol | Median Snapshot |',
      '| #031 | Firefox / Chromium | W3C BiDi + CDP | 5.0 ms – 19.0 ms |',
      '# extracted in 5.4 ms · 0 orphan processes · R3 compliant',
    ],
  },
];

export default function HeroDemoSlot({ baseUrl }: Props) {
  const [copied, setCopied] = useState(false);
  const [videoAvailable, setVideoAvailable] = useState(false);
  const [activeStep, setActiveStep] = useState(1);

  const normalizedBase = baseUrl.endsWith('/') ? baseUrl : `${baseUrl}/`;
  const videoSrc = `${normalizedBase}demos/demo-1-hero.mp4`;
  const posterSrc = `${normalizedBase}demos/demo-1-poster.png`;

  useEffect(() => {
    let cancelled = false;
    fetch(videoSrc, { method: 'HEAD' })
      .then((res) => {
        if (!cancelled && res.ok) {
          const ct = res.headers.get('content-type') || '';
          if (ct.includes('video') || ct.includes('octet-stream') || ct === '') {
            setVideoAvailable(true);
          }
        }
      })
      .catch(() => {
        // Fallback HUD stays active cleanly when demo-1-hero.mp4 is not yet bundled
      });
    return () => {
      cancelled = true;
    };
  }, [videoSrc]);

  const handleCopy = async () => {
    try {
      await navigator.clipboard.writeText(QUICK_INSTALL_CMD);
      setCopied(true);
      setTimeout(() => setCopied(false), 2000);
    } catch {
      setCopied(false);
    }
  };

  const step = DEMO_STEPS[activeStep];

  return (
    <div class="space-y-6">
      {/* Primary 1-Click Install Command Bar */}
      <div class="hud-frame-bright p-3 sm:p-4">
        <div class="flex flex-col gap-3 sm:flex-row sm:items-center sm:justify-between">
          <div class="min-w-0 flex-1 overflow-x-auto">
            <div class="mb-1 flex items-center gap-2 font-mono text-[11px] uppercase tracking-wider text-[#9D80EB]">
              <span class="inline-block h-2 w-2 bg-[#4CE0B3]" />
              <span>1-Line POSIX Install · SHA-256 + SLSA v1 Verified</span>
            </div>
            <code class="block whitespace-nowrap font-mono text-xs text-[#FFE47A] sm:text-sm">
              <span class="select-none text-[#4CE0B3]">$ </span>
              {QUICK_INSTALL_CMD}
            </code>
          </div>
          <div class="flex shrink-0 items-center gap-2">
            <button
              type="button"
              onClick={handleCopy}
              class="hud-btn-gold inline-flex cursor-pointer items-center gap-2 px-4 py-2.5 font-mono text-xs font-extrabold uppercase tracking-wide"
            >
              {copied ? '✓ Copied!' : 'Copy Install'}
            </button>
            <a
              href="#install"
              class="hud-btn-ghost inline-flex items-center px-3.5 py-2.5 font-mono text-xs font-semibold text-[#C9B8F5]"
            >
              All 6 Registries ↓
            </a>
          </div>
        </div>
      </div>

      {/* Styled Demo Preview Slot (demo-1-hero.mp4 with graceful 16-bit HUD fallback) */}
      <div class="hud-frame overflow-hidden">
        <div class="flex flex-wrap items-center justify-between gap-2 border-b-2 border-[#563A9E] bg-[#120924] px-4 py-2.5">
          <div class="flex items-center gap-2">
            <span class="inline-block h-2.5 w-2.5 bg-[#FF6B8B]" />
            <span class="inline-block h-2.5 w-2.5 bg-[#F5B841]" />
            <span class="inline-block h-2.5 w-2.5 bg-[#4CE0B3]" />
            <span class="ml-2 font-mono text-xs font-bold tracking-wider text-[#F4EEFF]">
              {videoAvailable
                ? 'LIVE DEMO · demo-1-hero.mp4 (5ms Agentic Loop)'
                : 'INTERACTIVE HUD PREVIEW · The 5ms Agentic Loop (@eN → --humanize → Markdown)'}
            </span>
          </div>
          <span class="border border-[#563A9E] bg-[#22133F] px-2 py-0.5 font-mono text-[11px] font-semibold text-[#4CE0B3]">
            DAEMON: WARM · 127.0.0.1
          </span>
        </div>

        {videoAvailable ? (
          <div class="relative bg-[#090414]">
            <video
              class="aspect-video w-full object-contain"
              src={videoSrc}
              poster={posterSrc}
              controls
              autoplay
              loop
              muted
              playsInline
              onError={() => setVideoAvailable(false)}
            />
          </div>
        ) : (
          <div class="bg-[#090414] p-4 sm:p-5">
            {/* Step selector tabs */}
            <div class="mb-4 grid grid-cols-2 gap-2 lg:grid-cols-4">
              {DEMO_STEPS.map((item, index) => {
                const isActive = index === activeStep;
                return (
                  <button
                    key={item.id}
                    type="button"
                    onClick={() => setActiveStep(index)}
                    class={`cursor-pointer border-2 px-3 py-2 text-left font-mono transition-colors ${
                      isActive
                        ? 'border-[#F5B841] bg-[#22133F] text-[#FFE47A]'
                        : 'border-[#563A9E]/70 bg-[#120924] text-[#C9B8F5] hover:border-[#9D80EB] hover:text-[#F4EEFF]'
                    }`}
                  >
                    <div class="text-[11px] font-bold">{item.label}</div>
                    <div class="mt-0.5 text-[11px] text-[#4CE0B3]">{item.timing}</div>
                  </button>
                );
              })}
            </div>

            {/* Split-pane HUD terminal + live browser ref pane */}
            <div class="grid gap-4 lg:grid-cols-12">
              <div class="border-2 border-[#563A9E] bg-[#120924] p-4 lg:col-span-7">
                <div class="mb-3 flex flex-wrap items-center justify-between gap-2">
                  <span class="font-mono text-[11px] font-bold uppercase tracking-widest text-[#9D80EB]">
                    mimikyu-cli // Unix Daemon Socket
                  </span>
                  <span
                    class="border px-2 py-0.5 font-mono text-[10px] font-bold"
                    style={{ borderColor: step.accent, color: step.accent }}
                  >
                    {step.badge} · {step.timing}
                  </span>
                </div>
                <div class="overflow-x-auto border border-[#563A9E]/60 bg-[#090414] p-3 font-mono text-xs leading-relaxed">
                  <div class="text-[#FFE47A]">
                    <span class="text-[#4CE0B3]">$ </span>
                    {step.command}
                  </div>
                  <div class="mt-3 space-y-1 text-[#C9B8F5]">
                    {step.output.map((line, i) => (
                      <div
                        key={i}
                        class={
                          line.startsWith('#')
                            ? 'pt-1 text-[#4CE0B3]'
                            : line.startsWith('✓')
                              ? 'text-[#9EFCE4]'
                              : line.includes('@e')
                                ? 'text-[#F4EEFF]'
                                : 'text-[#C9B8F5]'
                        }
                      >
                        {line}
                      </div>
                    ))}
                  </div>
                </div>
              </div>

              <div class="flex flex-col justify-between border-2 border-[#563A9E] bg-[#120924] p-4 lg:col-span-5">
                <div>
                  <div class="mb-3 flex items-center justify-between border-b border-[#563A9E]/70 pb-2 font-mono text-[11px]">
                    <span class="text-[#FFE47A]">VIEWPORT · 1280×720</span>
                    <span class="text-[#4CE0B3]">CDP + BiDi</span>
                  </div>
                  <div class="space-y-2.5 font-mono text-xs">
                    <div class="flex items-center justify-between border border-[#563A9E] bg-[#180D2E] px-2.5 py-2">
                      <span class="text-[#C9B8F5]">Search packages</span>
                      <span class="border border-[#F5B841] bg-[#090414] px-1.5 py-0.5 text-[10px] font-bold text-[#FFE47A]">
                        @e1
                      </span>
                    </div>
                    <div class="flex items-center justify-between border border-[#563A9E] bg-[#180D2E] px-2.5 py-2">
                      <span class="text-[#C9B8F5]">Engine: chromium | firefox</span>
                      <span class="border border-[#9D80EB] bg-[#090414] px-1.5 py-0.5 text-[10px] font-bold text-[#C9B8F5]">
                        @e2
                      </span>
                    </div>
                    <div class="flex items-center justify-between border border-[#4CE0B3] bg-[#22133F] px-2.5 py-2">
                      <span class="font-semibold text-[#F4EEFF]">Apply Filter</span>
                      <span class="bg-[#4CE0B3] px-1.5 py-0.5 text-[10px] font-extrabold text-[#090414]">
                        @e3 · CLICK
                      </span>
                    </div>
                  </div>
                </div>
                <div class="mt-4 flex items-center justify-between border-t border-[#563A9E]/60 pt-3 font-mono text-[11px] text-[#C9B8F5]">
                  <span>Shadow DOM + iframes indexed</span>
                  <span class="text-[#FFE47A]">demo-1-hero.mp4 ready</span>
                </div>
              </div>
            </div>
          </div>
        )}
      </div>
    </div>
  );
}
