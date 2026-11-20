#!/usr/bin/env python3
"""Local offline dark-mode demo fixture server (127.0.0.1:0) for Mimikyu demos.

Serves two deterministic, zero-external-dependency routes:
  - `/`        : Dark-mode SaaS Fleet Catalog for Demo 1 (Hero Agentic Loop).
  - `/parity`  : Light DOM + Open Shadow DOM + embedded `/iframe-panel` workbench
                 for Demo 2 (Dual-Engine Chromium CDP vs Firefox W3C BiDi Parity).

Both routes inject a compact demo-only glowing SVG cursor, curved trail, and
click-ripple overlay listening to real `pointermove` / `pointerdown` events so
`mimikyu --humanize` kinematic motion is unmistakably visible.
"""

from __future__ import annotations

import argparse
import contextlib
import http.server
import socketserver
import threading
from typing import Iterator

# ~25-line demo-only glowing SVG cursor + fading Bezier trail + click-ripple overlay.
CURSOR_OVERLAY_SNIPPET = r"""
<style>
  #mk-overlay{position:fixed;inset:0;width:100%;height:100%;pointer-events:none;z-index:999999;overflow:visible}
  @keyframes mkRipple{0%{r:5;opacity:.95;stroke-width:3}100%{r:30;opacity:0;stroke-width:1}}
  .mk-ripple{fill:rgba(245,184,65,.14);stroke:#F5B841;animation:mkRipple .65s ease-out forwards}
</style>
<svg id="mk-overlay" xmlns="http://www.w3.org/2000/svg">
  <defs><filter id="mkGlow"><feGaussianBlur stdDeviation="2.4" result="b"/><feMerge><feMergeNode in="b"/><feMergeNode in="SourceGraphic"/></feMerge></filter></defs>
  <polyline id="mk-trail" fill="none" stroke="#4CE0B3" stroke-width="2.6" stroke-linecap="round" stroke-linejoin="round" stroke-opacity=".85" filter="url(#mkGlow)"/>
  <g id="mk-ripples"></g>
  <g id="mk-cursor" transform="translate(-80,-80)" filter="url(#mkGlow)">
    <circle cx="0" cy="0" r="10" fill="rgba(245,184,65,.18)" stroke="#4CE0B3" stroke-width="1.4"/>
    <path d="M0,0 L0,16 L4.5,12.2 L8.2,18.5 L10.8,17 L7.2,10.8 L12.8,10.5 Z" fill="#F5B841" stroke="#120924" stroke-width="1.2"/>
  </g>
</svg>
<script>
(() => {
  const ns = 'http://www.w3.org/2000/svg', trail = document.getElementById('mk-trail');
  const cursor = document.getElementById('mk-cursor'), ripples = document.getElementById('mk-ripples');
  const pts = (window.__mimikyuTrail = []);
  const draw = () => trail.setAttribute('points', pts.slice(-48).map(p => `${p.x.toFixed(1)},${p.y.toFixed(1)}`).join(' '));
  const move = (e) => {
    const p = {x: e.clientX, y: e.clientY, t: performance.now(), kind: 'move'};
    pts.push(p); cursor.setAttribute('transform', `translate(${p.x.toFixed(1)},${p.y.toFixed(1)})`); draw();
  };
  const down = (e) => {
    const p = {x: e.clientX, y: e.clientY, t: performance.now(), kind: 'down'};
    pts.push(p); cursor.setAttribute('transform', `translate(${p.x.toFixed(1)},${p.y.toFixed(1)})`); draw();
    const c = document.createElementNS(ns, 'circle');
    c.setAttribute('cx', p.x.toFixed(1)); c.setAttribute('cy', p.y.toFixed(1));
    c.setAttribute('r', '14'); c.setAttribute('class', 'mk-ripple');
    ripples.appendChild(c); while (ripples.childNodes.length > 6) ripples.removeChild(ripples.firstChild);
  };
  window.addEventListener('pointermove', move, true);
  window.addEventListener('pointerdown', down, true);
  window.__mimikyuResetTrail = () => {
    pts.length = 0; trail.setAttribute('points', ''); ripples.innerHTML = '';
    cursor.setAttribute('transform', 'translate(-80,-80)');
    document.querySelectorAll('iframe').forEach(f => {
      try { f.contentWindow.postMessage({type: 'mk-reset-trail'}, '*'); } catch (_) {}
    });
  };
  window.addEventListener('message', (ev) => {
    if (ev.data && ev.data.type === 'mk-reset-trail') window.__mimikyuResetTrail();
  });
})();
</script>
"""

DASHBOARD_HTML = f"""<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8" />
<title>Aether Ops — Fleet Catalog</title>
<style>
  :root {{
    --bg: #120924;
    --panel: #180D2E;
    --card: #21133C;
    --border: #392363;
    --gold: #F5B841;
    --mint: #4CE0B3;
    --lav: #9D80EB;
    --coral: #FF6B8B;
    --text: #F2ECFF;
    --muted: #A998CC;
  }}
  * {{ box-sizing: border-box; margin: 0; padding: 0; }}
  body {{
    background: radial-gradient(circle at 85% 8%, #261448 0%, var(--bg) 58%);
    color: var(--text);
    font-family: -apple-system, BlinkMacSystemFont, "SF Pro Text", "Inter", system-ui, sans-serif;
    padding: 16px 18px;
    min-height: 100vh;
    overflow: hidden;
  }}
  .topbar {{
    display: flex; align-items: center; justify-content: space-between;
    padding-bottom: 12px; margin-bottom: 14px;
    border-bottom: 1px solid var(--border);
  }}
  .brand {{ display: flex; align-items: center; gap: 10px; }}
  .badge-dot {{
    width: 10px; height: 10px; border-radius: 2px; background: var(--mint);
    box-shadow: 0 0 10px rgba(76, 224, 179, .85);
  }}
  h1 {{ font-size: 16px; font-weight: 700; letter-spacing: .04em; color: var(--text); }}
  .sub {{ font-size: 11px; color: var(--muted); font-family: ui-monospace, SFMono-Regular, Menlo, monospace; }}
  .pill {{
    font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
    font-size: 11px; font-weight: 600;
    padding: 4px 9px; border-radius: 5px;
    background: rgba(76, 224, 179, .12); color: var(--mint);
    border: 1px solid rgba(76, 224, 179, .38);
  }}
  .kpis {{
    display: grid; grid-template-columns: repeat(3, 1fr); gap: 10px; margin-bottom: 14px;
  }}
  .kpi {{
    background: var(--panel); border: 1px solid var(--border); border-radius: 8px;
    padding: 10px 12px;
  }}
  .kpi-label {{ font-size: 10px; text-transform: uppercase; letter-spacing: .08em; color: var(--muted); }}
  .kpi-val {{
    margin-top: 4px; font-size: 18px; font-weight: 700;
    font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
  }}
  .controls {{
    display: flex; gap: 8px; align-items: center; margin-bottom: 14px;
  }}
  input[type="text"] {{
    flex: 1; background: #0E071C; color: var(--text);
    border: 1px solid #4A2F7F; border-radius: 6px;
    padding: 8px 11px; font-size: 13px;
    font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
    outline: none;
  }}
  input[type="text"]:focus {{
    border-color: var(--gold); box-shadow: 0 0 0 2px rgba(245, 184, 65, .25);
  }}
  button {{
    cursor: pointer; border-radius: 6px; font-size: 12px; font-weight: 600;
    padding: 8px 11px; border: 1px solid var(--border);
    background: var(--card); color: var(--text);
    transition: transform .08s ease, border-color .12s ease, background .12s ease;
  }}
  button:hover {{ border-color: var(--lav); }}
  button.active {{
    background: rgba(157, 128, 235, .22); border-color: var(--lav); color: #fff;
  }}
  button.primary {{
    background: linear-gradient(135deg, #F5B841, #E09B20);
    color: #120924; border-color: #FFE47A; font-weight: 700;
  }}
  button.primary:hover {{ filter: brightness(1.06); }}
  .table-wrap {{
    background: var(--panel); border: 1px solid var(--border); border-radius: 8px;
    overflow: hidden; margin-bottom: 12px;
  }}
  table {{ width: 100%; border-collapse: collapse; font-size: 12.5px; }}
  th {{
    text-align: left; font-size: 10.5px; text-transform: uppercase; letter-spacing: .07em;
    color: var(--muted); background: #140A28; padding: 9px 11px;
    border-bottom: 1px solid var(--border);
  }}
  td {{
    padding: 9px 11px; border-bottom: 1px solid rgba(57, 35, 99, .55);
    font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
  }}
  tr:last-child td {{ border-bottom: none; }}
  tr.selected {{ background: rgba(76, 224, 179, .08); }}
  .svc {{ color: var(--text); font-weight: 600; }}
  .reg {{ color: var(--lav); }}
  .lat {{ color: var(--gold); }}
  .st {{
    display: inline-block; padding: 2px 7px; border-radius: 4px; font-size: 10.5px; font-weight: 700;
  }}
  .st-healthy {{ background: rgba(76, 224, 179, .14); color: var(--mint); border: 1px solid rgba(76, 224, 179, .4); }}
  .st-active {{ background: rgba(157, 128, 235, .16); color: #C6B2FF; border: 1px solid rgba(157, 128, 235, .4); }}
  .st-canary {{ background: rgba(245, 184, 65, .18); color: var(--gold); border: 1px solid rgba(245, 184, 65, .55); }}
  .banner {{
    display: flex; align-items: center; justify-content: space-between;
    background: #140A28; border: 1px solid var(--border); border-radius: 8px;
    padding: 10px 13px; font-size: 12px;
    font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
  }}
  .banner.deployed {{
    background: rgba(76, 224, 179, .12);
    border-color: var(--mint);
    box-shadow: 0 0 18px rgba(76, 224, 179, .18);
  }}
  .banner-label {{ color: var(--muted); }}
  .banner.deployed .banner-label {{ color: var(--mint); font-weight: 700; }}
  .banner-meta {{ color: var(--gold); font-size: 11px; }}
  .receipts {{
    margin-top: 12px; background: #100720; border: 1px solid #2D1A52; border-radius: 8px;
    padding: 10px 12px; font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: 11px;
  }}
  .receipts-hdr {{
    display: flex; justify-content: space-between; color: var(--muted);
    font-size: 10px; letter-spacing: .07em; text-transform: uppercase;
    padding-bottom: 6px; margin-bottom: 6px; border-bottom: 1px solid #251544;
  }}
  .receipt-row {{ display: flex; justify-content: space-between; padding: 3px 0; color: #C5B6E6; }}
  .receipt-tag {{ color: var(--lav); font-weight: 600; }}
  .receipt-ok {{ color: var(--mint); }}
</style>
</head>
<body>
  <header class="topbar">
    <div class="brand">
      <div class="badge-dot"></div>
      <div>
        <h1>AETHER OPS // FLEET CATALOG</h1>
        <div class="sub">cluster: prod-mesh-01 · release: v2.4.0</div>
      </div>
    </div>
    <div class="pill" id="fleet-pill">4 NODES ONLINE</div>
  </header>

  <section class="kpis" aria-label="Cluster KPIs">
    <div class="kpi">
      <div class="kpi-label">Active Services</div>
      <div class="kpi-val" id="kpi-count" style="color:#4CE0B3">4</div>
    </div>
    <div class="kpi">
      <div class="kpi-label">Median Edge RTT</div>
      <div class="kpi-val" id="kpi-rtt" style="color:#F5B841">5.1 ms</div>
    </div>
    <div class="kpi">
      <div class="kpi-label">Canary Channel</div>
      <div class="kpi-val" id="kpi-channel" style="color:#9D80EB">STANDBY</div>
    </div>
  </section>

  <section class="controls" aria-label="Catalog Controls">
    <input id="filter-input" type="text" aria-label="Filter deployments" placeholder="Filter services (e.g. edge-prod)..." />
    <button id="btn-all" type="button" class="active">All</button>
    <button id="btn-edge" type="button">Edge</button>
    <button id="btn-deploy" type="button" class="primary">Deploy Canary</button>
  </section>

  <section class="table-wrap">
    <table aria-label="Service Deployments">
      <thead>
        <tr>
          <th>Service</th>
          <th>Region</th>
          <th>p50 RTT</th>
          <th>Status</th>
        </tr>
      </thead>
      <tbody id="fleet-rows"></tbody>
    </table>
  </section>

  <footer class="banner" id="deploy-banner" role="status" aria-live="polite">
    <span class="banner-label" id="deploy-msg">READY // Filter target fleet and trigger canary rollout</span>
    <span class="banner-meta" id="deploy-meta">0 pending ops</span>
  </footer>

  <section class="receipts" aria-label="Agent Action Receipts">
    <div class="receipts-hdr">
      <span>LIVE RUNTIME TELEMETRY</span>
      <span style="color:#4CE0B3">ISO-WORLD · ZERO-LEAK</span>
    </div>
    <div id="receipt-list">
      <div class="receipt-row"><span><span class="receipt-tag">[INIT]</span> fleet catalog hydrated (4 services)</span><span class="receipt-ok">READY</span></div>
    </div>
  </section>

<script>
(() => {{
  const SERVICES = [
    {{ name: "edge-prod-us",   region: "us-east-1",      rtt: "4.8 ms", tier: "edge", status: "HEALTHY" }},
    {{ name: "edge-prod-eu",   region: "eu-central-1",   rtt: "5.1 ms", tier: "edge", status: "HEALTHY" }},
    {{ name: "core-auth-gw",   region: "us-west-2",      rtt: "6.4 ms", tier: "core", status: "ACTIVE"  }},
    {{ name: "vector-idx-04",  region: "ap-northeast-1", rtt: "9.2 ms", tier: "core", status: "ACTIVE"  }},
  ];
  let activeTier = "all";
  let deployedSet = new Set();

  const tbody = document.getElementById("fleet-rows");
  const input = document.getElementById("filter-input");
  const btnAll = document.getElementById("btn-all");
  const btnEdge = document.getElementById("btn-edge");
  const btnDeploy = document.getElementById("btn-deploy");
  const kpiCount = document.getElementById("kpi-count");
  const kpiChannel = document.getElementById("kpi-channel");
  const fleetPill = document.getElementById("fleet-pill");
  const banner = document.getElementById("deploy-banner");
  const deployMsg = document.getElementById("deploy-msg");
  const deployMeta = document.getElementById("deploy-meta");
  const receiptList = document.getElementById("receipt-list");
  let filterReceiptEl = null;

  function addReceipt(tag, msg, status) {{
    const row = document.createElement("div");
    row.className = "receipt-row";
    row.innerHTML = `<span><span class="receipt-tag">[${{tag}}]</span> ${{msg}}</span><span class="receipt-ok">${{status}}</span>`;
    receiptList.appendChild(row);
    while (receiptList.children.length > 4) receiptList.removeChild(receiptList.firstChild);
    return row;
  }}

  function visibleServices() {{
    const q = input.value.trim().toLowerCase();
    return SERVICES.filter(s => {{
      const tierOk = activeTier === "all" || s.tier === activeTier;
      const qOk = !q || s.name.toLowerCase().includes(q) || s.region.toLowerCase().includes(q);
      return tierOk && qOk;
    }});
  }}

  function render() {{
    const rows = visibleServices();
    kpiCount.textContent = String(rows.length);
    fleetPill.textContent = `${{rows.length}} NODE${{rows.length === 1 ? "" : "S"}} MATCHED`;
    tbody.innerHTML = rows.map(s => {{
      const isCanary = deployedSet.has(s.name);
      const st = isCanary ? "CANARY v2.4.1" : s.status;
      const cls = isCanary ? "st-canary" : (s.status === "HEALTHY" ? "st-healthy" : "st-active");
      return `<tr class="${{isCanary ? "selected" : ""}}">
        <td class="svc">${{s.name}}</td>
        <td class="reg">${{s.region}}</td>
        <td class="lat">${{s.rtt}}</td>
        <td><span class="st ${{cls}}">${{st}}</span></td>
      </tr>`;
    }}).join("");
  }}

  input.addEventListener("input", () => {{
    render();
    const q = input.value.trim();
    if (q.length > 0) {{
      const cnt = visibleServices().length;
      const html = `<span><span class="receipt-tag">[FILL @e1]</span> filtered fleet -> "${{q}}" (${{cnt}} match${{cnt === 1 ? "" : "es"}})</span><span class="receipt-ok">HUMANIZED</span>`;
      if (!filterReceiptEl) {{
        filterReceiptEl = addReceipt("FILL @e1", "", "HUMANIZED");
      }}
      filterReceiptEl.innerHTML = html;
    }}
  }});
  btnAll.addEventListener("click", () => {{
    activeTier = "all"; btnAll.classList.add("active"); btnEdge.classList.remove("active"); render();
  }});
  btnEdge.addEventListener("click", () => {{
    activeTier = "edge"; btnEdge.classList.add("active"); btnAll.classList.remove("active"); render();
  }});
  btnDeploy.addEventListener("click", () => {{
    const targets = visibleServices();
    targets.forEach(s => deployedSet.add(s.name));
    kpiChannel.textContent = "v2.4.1 LIVE";
    kpiChannel.style.color = "#F5B841";
    banner.classList.add("deployed");
    deployMsg.textContent = `DEPLOYED v2.4.1 -> ${{targets.map(t => t.name).join(", ")}}`;
    deployMeta.textContent = `${{targets.length}} nodes · 0 errors`;
    addReceipt("CLICK @e4", `canary v2.4.1 rolled out to ${{targets.length}} edge nodes`, "COMMITTED");
    render();
  }});
  render();
}})();
</script>
{CURSOR_OVERLAY_SNIPPET}
</body>
</html>
"""

IFRAME_PANEL_HTML = f"""<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8" />
<title>Embedded Policy Frame</title>
<style>
  * {{ box-sizing: border-box; margin: 0; padding: 0; }}
  body {{
    background: #140A28; color: #F2ECFF;
    font-family: -apple-system, BlinkMacSystemFont, "SF Pro Text", system-ui, sans-serif;
    padding: 10px 12px; overflow: hidden;
  }}
  .hdr {{
    display: flex; justify-content: space-between; align-items: center;
    margin-bottom: 8px;
  }}
  .tag {{
    font-family: ui-monospace, Menlo, monospace; font-size: 10px; font-weight: 700;
    color: #FF6B8B; letter-spacing: .06em; text-transform: uppercase;
  }}
  .status {{
    font-family: ui-monospace, Menlo, monospace; font-size: 10px; font-weight: 700;
    padding: 2px 7px; border-radius: 4px;
    background: rgba(255, 107, 139, .15); color: #FF6B8B;
    border: 1px solid rgba(255, 107, 139, .45);
  }}
  .status.ok {{
    background: rgba(76, 224, 179, .15); color: #4CE0B3;
    border-color: rgba(76, 224, 179, .5);
  }}
  p {{
    font-size: 11px; color: #A998CC; margin-bottom: 10px; line-height: 1.35;
  }}
  button {{
    width: 100%; cursor: pointer; border-radius: 6px; padding: 7px 10px;
    font-size: 11.5px; font-weight: 700;
    background: linear-gradient(135deg, #9D80EB, #7A5AD8);
    color: #fff; border: 1px solid #BFA8FF;
  }}
</style>
</head>
<body>
  <div class="hdr">
    <span class="tag">IFRAME // SANDBOX</span>
    <span class="status" id="frame-status">PENDING</span>
  </div>
  <p>Nested document boundary pierced cleanly via deterministic <code>@eN</code> refs.</p>
  <button id="btn-attest" type="button">Attest Frame</button>
<script>
  const btn = document.getElementById("btn-attest");
  const st = document.getElementById("frame-status");
  btn.addEventListener("click", () => {{
    st.textContent = "VERIFIED";
    st.classList.add("ok");
    btn.textContent = "Frame Attested";
    btn.style.background = "linear-gradient(135deg, #4CE0B3, #27B389)";
    btn.style.color = "#120924";
    window.parent.postMessage({{ type: "frame-verified" }}, "*");
  }});
</script>
{CURSOR_OVERLAY_SNIPPET}
</body>
</html>
"""

PARITY_HTML = f"""<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8" />
<title>Mimikyu Dual-Engine Parity Workbench</title>
<style>
  :root {{
    --bg: #120924;
    --panel: #180D2E;
    --border: #392363;
    --gold: #F5B841;
    --mint: #4CE0B3;
    --lav: #9D80EB;
    --text: #F2ECFF;
    --muted: #A998CC;
  }}
  * {{ box-sizing: border-box; margin: 0; padding: 0; }}
  body {{
    background: radial-gradient(circle at 80% 10%, #241345 0%, var(--bg) 60%);
    color: var(--text);
    font-family: -apple-system, BlinkMacSystemFont, "SF Pro Text", system-ui, sans-serif;
    padding: 12px 14px;
    overflow: hidden;
  }}
  .top {{
    display: flex; justify-content: space-between; align-items: center;
    padding-bottom: 8px; margin-bottom: 10px;
    border-bottom: 1px solid var(--border);
  }}
  .title {{ font-size: 13.5px; font-weight: 700; letter-spacing: .04em; }}
  .score {{
    font-family: ui-monospace, Menlo, monospace; font-size: 10.5px; font-weight: 700;
    padding: 3px 8px; border-radius: 5px;
    background: rgba(245, 184, 65, .14); color: var(--gold);
    border: 1px solid rgba(245, 184, 65, .45);
  }}
  .score.all-ok {{
    background: rgba(76, 224, 179, .16); color: var(--mint);
    border-color: var(--mint);
  }}
  .card {{
    background: var(--panel); border: 1px solid var(--border); border-radius: 8px;
    padding: 10px 12px; margin-bottom: 10px;
  }}
  .card-hdr {{
    display: flex; justify-content: space-between; align-items: center; margin-bottom: 8px;
  }}
  .tag {{
    font-family: ui-monospace, Menlo, monospace; font-size: 10px; font-weight: 700;
    letter-spacing: .06em; text-transform: uppercase; color: var(--gold);
  }}
  .badge {{
    font-family: ui-monospace, Menlo, monospace; font-size: 10px; font-weight: 700;
    padding: 2px 7px; border-radius: 4px;
    background: rgba(157, 128, 235, .14); color: var(--lav);
    border: 1px solid rgba(157, 128, 235, .4);
  }}
  .badge.ok {{
    background: rgba(76, 224, 179, .16); color: var(--mint);
    border-color: rgba(76, 224, 179, .5);
  }}
  .row {{ display: flex; gap: 8px; }}
  input {{
    flex: 1; background: #0E071C; color: var(--text);
    border: 1px solid #4A2F7F; border-radius: 6px;
    padding: 6px 9px; font-size: 12px;
    font-family: ui-monospace, Menlo, monospace; outline: none;
  }}
  input:focus {{ border-color: var(--gold); }}
  button {{
    cursor: pointer; border-radius: 6px; padding: 6px 11px;
    font-size: 11.5px; font-weight: 700;
    background: linear-gradient(135deg, #F5B841, #DF9A1F);
    color: #120924; border: 1px solid #FFE47A;
  }}
  .grid {{ display: grid; grid-template-columns: 1fr 1fr; gap: 10px; margin-bottom: 10px; }}
  iframe {{
    width: 100%; height: 122px; border: 1px solid var(--border);
    border-radius: 8px; background: #140A28; display: block;
  }}
  .footer {{
    background: #140A28; border: 1px solid var(--border); border-radius: 7px;
    padding: 8px 11px; display: flex; justify-content: space-between; align-items: center;
    font-family: ui-monospace, Menlo, monospace; font-size: 11px; color: var(--muted);
  }}
  .footer.complete {{
    border-color: var(--mint); color: var(--mint);
    background: rgba(76, 224, 179, .10); font-weight: 700;
  }}
  .matrix {{
    margin-top: 8px; display: grid; grid-template-columns: repeat(3, 1fr); gap: 8px;
    font-family: ui-monospace, Menlo, monospace; font-size: 10.5px;
  }}
  .mcell {{
    background: #100720; border: 1px solid #2C1A4F; border-radius: 6px;
    padding: 7px 9px; display: flex; justify-content: space-between; align-items: center;
    color: var(--muted);
  }}
  .mcell.ok {{ border-color: rgba(76, 224, 179, .55); color: var(--mint); background: rgba(76, 224, 179, .08); }}
  .mref {{ color: var(--gold); font-weight: 700; }}
</style>
</head>
<body>
  <div class="top">
    <div class="title">PARITY WORKBENCH // LIGHT + SHADOW + IFRAME</div>
    <div class="score" id="parity-score">0 / 3 VERIFIED</div>
  </div>

  <section class="card">
    <div class="card-hdr">
      <span class="tag">1. LIGHT DOM // SESSION BINDING</span>
      <span class="badge" id="light-badge">IDLE</span>
    </div>
    <div class="row">
      <input id="agent-id" type="text" aria-label="Agent token" placeholder="Agent ID..." />
      <button id="btn-bind" type="button">Bind Session</button>
    </div>
  </section>

  <section class="grid">
    <mimikyu-vault id="vault-host"></mimikyu-vault>
    <iframe id="policy-frame" src="/iframe-panel" title="Embedded Policy Frame"></iframe>
  </section>

  <footer class="footer" id="parity-footer">
    <span id="parity-summary">Awaiting atomic @eN batch execution...</span>
    <span id="engine-ua"></span>
  </footer>

  <section class="matrix" aria-label="Boundary Matrix">
    <div class="mcell" id="mc-light"><span><span class="mref">@e1-2</span> Light</span><span id="ms-light">WAIT</span></div>
    <div class="mcell" id="mc-shadow"><span><span class="mref">@e3-4</span> Shadow</span><span id="ms-shadow">WAIT</span></div>
    <div class="mcell" id="mc-frame"><span><span class="mref">@e5</span> Iframe</span><span id="ms-frame">WAIT</span></div>
  </section>

<script>
(() => {{
  const state = {{ light: false, shadow: false, frame: false }};
  const score = document.getElementById("parity-score");
  const footer = document.getElementById("parity-footer");
  const summary = document.getElementById("parity-summary");
  const uaEl = document.getElementById("engine-ua");
  uaEl.textContent = navigator.userAgent.includes("Firefox") ? "Gecko / BiDi" : "Blink / CDP";

  function updateOverall() {{
    const map = [
      ["light", "mc-light", "ms-light"],
      ["shadow", "mc-shadow", "ms-shadow"],
      ["frame", "mc-frame", "ms-frame"],
    ];
    map.forEach(([k, cid, sid]) => {{
      if (state[k]) {{
        document.getElementById(cid).classList.add("ok");
        document.getElementById(sid).textContent = "OK";
      }}
    }});
    const n = [state.light, state.shadow, state.frame].filter(Boolean).length;
    score.textContent = `${{n}} / 3 VERIFIED`;
    if (n === 3) {{
      score.classList.add("all-ok");
      footer.classList.add("complete");
      summary.textContent = "PARITY OK: Light DOM + Shadow DOM + Iframe verified";
    }}
  }}

  const agentInput = document.getElementById("agent-id");
  const lightBadge = document.getElementById("light-badge");
  document.getElementById("btn-bind").addEventListener("click", () => {{
    const val = agentInput.value.trim() || "anon";
    state.light = true;
    lightBadge.textContent = `BOUND: ${{val}}`;
    lightBadge.classList.add("ok");
    updateOverall();
  }});

  class MimikyuVault extends HTMLElement {{
    constructor() {{
      super();
      const root = this.attachShadow({{ mode: "open" }});
      root.innerHTML = `
        <style>
          *{{box-sizing:border-box;margin:0;padding:0}}
          .box{{background:#180D2E;border:1px solid #392363;border-radius:8px;padding:10px 12px;height:122px;display:flex;flex-direction:column;justify-content:space-between}}
          .hdr{{display:flex;justify-content:space-between;align-items:center}}
          .tag{{font-family:ui-monospace,Menlo,monospace;font-size:10px;font-weight:700;color:#4CE0B3;letter-spacing:.06em;text-transform:uppercase}}
          .st{{font-family:ui-monospace,Menlo,monospace;font-size:10px;font-weight:700;padding:2px 7px;border-radius:4px;background:rgba(245,184,65,.15);color:#F5B841;border:1px solid rgba(245,184,65,.45)}}
          .st.ok{{background:rgba(76,224,179,.16);color:#4CE0B3;border-color:rgba(76,224,179,.55)}}
          input{{width:100%;background:#0E071C;color:#F2ECFF;border:1px solid #4A2F7F;border-radius:6px;padding:6px 8px;font-size:11.5px;font-family:ui-monospace,Menlo,monospace;outline:none}}
          button{{width:100%;cursor:pointer;border-radius:6px;padding:6px 10px;font-size:11.5px;font-weight:700;background:linear-gradient(135deg,#4CE0B3,#29B58C);color:#120924;border:1px solid #9EFCE4}}
        </style>
        <div class="box">
          <div class="hdr">
            <span class="tag">OPEN SHADOW DOM</span>
            <span class="st" id="sh-st">LOCKED</span>
          </div>
          <input id="vault-key" type="text" aria-label="Shadow vault key" placeholder="Vault key..." />
          <button id="btn-unlock" type="button">Unlock Shadow</button>
        </div>
      `;
      const btn = root.getElementById("btn-unlock");
      const inp = root.getElementById("vault-key");
      const st = root.getElementById("sh-st");
      btn.addEventListener("click", () => {{
        state.shadow = true;
        st.textContent = `OPEN: ${{inp.value.trim() || "ok"}}`;
        st.classList.add("ok");
        updateOverall();
      }});
    }}
  }}
  customElements.define("mimikyu-vault", MimikyuVault);

  window.addEventListener("message", (ev) => {{
    if (ev.data && ev.data.type === "frame-verified") {{
      state.frame = true;
      updateOverall();
    }}
  }});
}})();
</script>
{CURSOR_OVERLAY_SNIPPET}
</body>
</html>
"""

ROUTES: dict[str, tuple[str, str]] = {
    "/": (DASHBOARD_HTML, "text/html; charset=utf-8"),
    "/dashboard": (DASHBOARD_HTML, "text/html; charset=utf-8"),
    "/parity": (PARITY_HTML, "text/html; charset=utf-8"),
    "/iframe-panel": (IFRAME_PANEL_HTML, "text/html; charset=utf-8"),
}


class _DemoFixtureHandler(http.server.BaseHTTPRequestHandler):
    def do_GET(self) -> None:  # noqa: N802
        path = self.path.split("?", 1)[0]
        if path in ROUTES:
            body, ctype = ROUTES[path]
            raw = body.encode("utf-8")
            self.send_response(200)
            self.send_header("Content-Type", ctype)
            self.send_header("Content-Length", str(len(raw)))
            self.send_header("Cache-Control", "no-store")
            self.end_headers()
            self.wfile.write(raw)
        else:
            self.send_error(404, f"Unknown fixture route: {path}")

    def log_message(self, format: str, *args: object) -> None:  # noqa: A002
        # Quiet by default during automated recording.
        return


class _ReusableTCPServer(socketserver.ThreadingMixIn, http.server.HTTPServer):
    daemon_threads = True
    allow_reuse_address = True


class FixtureServer:
    """Context manager serving offline demo fixtures on 127.0.0.1:0."""

    def __init__(self, host: str = "127.0.0.1", port: int = 0) -> None:
        self._server = _ReusableTCPServer((host, port), _DemoFixtureHandler)
        self.host, self.port = self._server.server_address[:2]
        self._thread = threading.Thread(
            target=self._server.serve_forever,
            kwargs={"poll_interval": 0.1},
            daemon=True,
        )

    @property
    def base_url(self) -> str:
        return f"http://{self.host}:{self.port}"

    def url(self, path: str = "/") -> str:
        return f"{self.base_url}{path}"

    def start(self) -> FixtureServer:
        self._thread.start()
        return self

    def stop(self) -> None:
        self._server.shutdown()
        self._server.server_close()
        self._thread.join(timeout=2.0)

    def __enter__(self) -> FixtureServer:
        return self.start()

    def __exit__(self, *_: object) -> None:
        self.stop()


def main() -> None:
    parser = argparse.ArgumentParser(description="Run local Mimikyu demo fixture server.")
    parser.add_argument("--host", default="127.0.0.1", help="Bind host (default: 127.0.0.1)")
    parser.add_argument("--port", type=int, default=0, help="Bind port (default: 0 = ephemeral)")
    args = parser.parse_args()

    with FixtureServer(host=args.host, port=args.port) as srv:
        print(f"Mimikyu demo fixtures serving on {srv.base_url}")
        print(f"  Demo 1 (Hero Dashboard):   {srv.url('/dashboard')}")
        print(f"  Demo 2 (Parity Workbench): {srv.url('/parity')}")
        print("Press Ctrl+C to stop.")
        with contextlib.suppress(KeyboardInterrupt):
            threading.Event().wait()


if __name__ == "__main__":
    main()
