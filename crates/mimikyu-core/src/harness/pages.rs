pub fn escape_html(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

pub fn index_html() -> String {
    r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <title>Mimikyu Local Test Harness</title>
  <style>
    body { font-family: system-ui, sans-serif; margin: 24px; }
    a, button, input { min-height: 28px; margin: 4px; }
  </style>
</head>
<body>
  <header>
    <h1>Mimikyu Local Test Harness</h1>
    <nav aria-label="Harness Routes">
      <ul>
        <li><a href="/wizard">Wizard Workflow</a></li>
        <li><a href="/form">Application Form</a></li>
        <li><a href="/table">Structured Tables</a></li>
        <li><a href="/catalog">Service Catalog</a></li>
        <li><a href="/dynamic">Dynamic Hydration</a></li>
        <li><a href="/shadow">Shadow DOM</a></li>
        <li><a href="/frames">Nested Frames</a></li>
        <li><a href="/env-probe">Environment Probe</a></li>
      </ul>
    </nav>
  </header>
  <main>
    <label for="inp">Quick Input</label>
    <input id="inp" type="text" placeholder="Type here" />
    <button id="btn" type="button" onclick="window.__clicked = (window.__clicked || 0) + 1; document.getElementById('status').textContent = 'clicked:' + window.__clicked;">Go</button>
    <div id="status" role="status">idle</div>
    <a id="next-link" href="/target">Next Target</a>
  </main>
  <script>
    window.__mainSecret = 'main-only-value';
  </script>
</body>
</html>"#
        .to_string()
}

pub fn target_html() -> String {
    r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <title>Target Reached</title>
</head>
<body>
  <main>
    <h1>Target Reached</h1>
    <p id="ok" role="status">arrived</p>
    <a href="/">Back to Index</a>
  </main>
</body>
</html>"#
        .to_string()
}

pub fn wizard_step1_html() -> String {
    r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <title>Onboarding Wizard - Step 1</title>
  <style>
    body { font-family: system-ui, sans-serif; margin: 24px; }
    .field { margin-bottom: 12px; }
    label { display: block; margin-bottom: 4px; }
    input, select, button { min-height: 28px; padding: 4px 8px; }
  </style>
</head>
<body>
  <main>
    <h1>Workspace Onboarding — Step 1</h1>
    <p>Enter profile details to initialize your workspace.</p>
    <form action="/wizard/step2" method="GET">
      <div class="field">
        <label for="wiz-name">Full Name</label>
        <input id="wiz-name" name="name" type="text" required />
      </div>
      <div class="field">
        <label for="wiz-role">Workspace Role</label>
        <select id="wiz-role" name="role">
          <option value="admin">Admin</option>
          <option value="editor">Editor</option>
          <option value="viewer">Viewer</option>
        </select>
      </div>
      <button id="wiz-next" type="submit">Continue to Step 2</button>
    </form>
  </main>
</body>
</html>"#
        .to_string()
}

pub fn wizard_step2_html(name: &str, role: &str) -> String {
    let esc_name = escape_html(name);
    let esc_role = escape_html(role);
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <title>Onboarding Wizard - Step 2</title>
  <style>
    body {{ font-family: system-ui, sans-serif; margin: 24px; }}
    .field {{ margin-bottom: 12px; }}
    label {{ display: block; margin-bottom: 4px; }}
    input, button {{ min-height: 28px; padding: 4px 8px; }}
  </style>
</head>
<body>
  <main>
    <h1>Workspace Onboarding — Step 2</h1>
    <p id="step1-summary">Profile: {esc_name} ({esc_role})</p>
    <form action="/wizard/done" method="POST">
      <input type="hidden" name="name" value="{esc_name}" />
      <input type="hidden" name="role" value="{esc_role}" />
      <div class="field">
        <label for="wiz-region">Region</label>
        <input id="wiz-region" name="region" type="text" required />
      </div>
      <div class="field">
        <label><input id="wiz-agree" name="agree" type="checkbox" value="yes" /> Confirm Settings</label>
      </div>
      <button id="wiz-complete" type="submit">Complete Setup</button>
    </form>
  </main>
</body>
</html>"#
    )
}

pub fn wizard_done_html(name: &str, role: &str, region: &str, agree: &str) -> String {
    let esc_name = escape_html(name);
    let esc_role = escape_html(role);
    let esc_region = escape_html(region);
    let esc_agree = escape_html(agree);
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <title>Wizard Complete</title>
</head>
<body>
  <main>
    <h1 id="wiz-done-heading">Wizard Complete</h1>
    <dl id="wiz-summary">
      <dt>Name</dt><dd id="done-name">{esc_name}</dd>
      <dt>Role</dt><dd id="done-role">{esc_role}</dd>
      <dt>Region</dt><dd id="done-region">{esc_region}</dd>
      <dt>Confirmed</dt><dd id="done-agree">{esc_agree}</dd>
    </dl>
    <div id="wiz-status" role="status">WIZARD_DONE:{esc_name}:{esc_role}:{esc_region}:{esc_agree}</div>
  </main>
</body>
</html>"#
    )
}

pub fn form_html() -> String {
    r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <title>Application Form</title>
  <style>
    body { font-family: system-ui, sans-serif; margin: 24px; }
    .field { margin-bottom: 12px; }
    label { display: inline-block; margin-right: 8px; }
    input, textarea, select, button { min-height: 28px; padding: 4px 8px; }
  </style>
</head>
<body>
  <main>
    <h1>Application Form</h1>
    <form action="/submit" method="POST">
      <div class="field">
        <label for="form-name">Applicant Name</label>
        <input id="form-name" name="name" type="text" />
      </div>
      <div class="field">
        <label for="form-notes">Notes</label>
        <textarea id="form-notes" name="notes" rows="3" cols="30"></textarea>
      </div>
      <div class="field">
        <label for="form-country">Country</label>
        <select id="form-country" name="country">
          <option value="us">United States</option>
          <option value="de">Germany</option>
          <option value="jp">Japan</option>
        </select>
      </div>
      <fieldset class="field">
        <legend>Tier</legend>
        <label><input type="radio" name="tier" value="starter" /> Starter</label>
        <label><input type="radio" name="tier" value="pro" /> Pro</label>
      </fieldset>
      <div class="field">
        <label for="form-terms">Accept Terms</label>
        <input id="form-terms" name="terms" type="checkbox" value="yes" />
      </div>
      <div class="field">
        <input id="form-file" name="attachment" type="file" aria-label="Attachment" />
      </div>
      <button id="form-submit" type="submit">Submit Application</button>
    </form>
    <button id="hidden-btn" type="button" style="display:none">Hidden Control</button>
    <a id="aria-hidden-link" href="/target" aria-hidden="true">Hidden Nav</a>
  </main>
</body>
</html>"#
        .to_string()
}

pub fn submit_echo_html(
    json_pretty: &str,
    name: &str,
    country: &str,
    tier: &str,
    terms: &str,
) -> String {
    let esc_json = escape_html(json_pretty);
    let esc_name = escape_html(name);
    let esc_country = escape_html(country);
    let esc_tier = escape_html(tier);
    let esc_terms = escape_html(terms);
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <title>Submission Confirmed</title>
</head>
<body>
  <main>
    <h1>Submission Confirmed</h1>
    <div id="submit-status" role="status">SUBMITTED:{esc_name}:{esc_country}:{esc_tier}:{esc_terms}</div>
    <pre id="submit-json">{esc_json}</pre>
  </main>
</body>
</html>"#
    )
}

pub fn table_html() -> String {
    r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <title>Structured Tables</title>
  <style>
    body { font-family: system-ui, sans-serif; margin: 24px; }
    table { border-collapse: collapse; margin-bottom: 24px; }
    th, td { border: 1px solid #999; padding: 6px 10px; }
  </style>
</head>
<body>
  <main>
    <h1>Infrastructure Allocations</h1>
    <table id="plan-matrix">
      <caption>Plan Matrix</caption>
      <thead>
        <tr>
          <th>Plan</th>
          <th colspan="2">Allocation</th>
        </tr>
      </thead>
      <tbody>
        <tr>
          <td rowspan="2">Core</td>
          <td>us-east</td>
          <td>4 vCPU</td>
        </tr>
        <tr>
          <td>eu-central</td>
          <td>8 vCPU</td>
        </tr>
        <tr>
          <td>Edge</td>
          <td>ap-northeast</td>
          <td>2 vCPU</td>
        </tr>
      </tbody>
    </table>

    <table id="metrics-table">
      <caption>Service Metrics</caption>
      <thead>
        <tr>
          <th>Service</th>
          <th>Region</th>
          <th>LatencyMs</th>
          <th>Status</th>
        </tr>
      </thead>
      <tbody>
        <tr><td>Gateway</td><td>us-east</td><td>12</td><td>healthy</td></tr>
        <tr><td>Worker</td><td>eu-central</td><td>19</td><td>healthy</td></tr>
        <tr><td>Indexer</td><td>ap-northeast</td><td>27</td><td>healthy</td></tr>
        <tr><td>Storage</td><td>us-west</td><td>15</td><td>healthy</td></tr>
      </tbody>
    </table>
  </main>
</body>
</html>"#
        .to_string()
}

pub fn catalog_html() -> String {
    r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <title>Service Catalog</title>
  <style>
    body { font-family: system-ui, sans-serif; margin: 24px; }
    .controls { display: flex; gap: 12px; align-items: center; margin-bottom: 16px; flex-wrap: wrap; }
    input, select, button { min-height: 28px; padding: 4px 8px; }
    table { border-collapse: collapse; width: 100%; max-width: 720px; }
    th, td { border: 1px solid #999; padding: 6px 10px; text-align: left; }
  </style>
</head>
<body>
  <main>
    <h1>Service Catalog</h1>
    <div class="controls">
      <label for="cat-search">Filter Items</label>
      <input id="cat-search" type="search" placeholder="Search name or SKU" />
      <label for="cat-category">Category</label>
      <select id="cat-category">
        <option value="all">All</option>
        <option value="compute">Compute</option>
        <option value="storage">Storage</option>
      </select>
      <button id="cat-sort-price" type="button">Sort by Price</button>
      <button id="cat-reset" type="button">Reset Filter</button>
    </div>
    <div id="cat-count" role="status">Showing 5 of 5 items</div>
    <table id="catalog-table">
      <caption>Service Catalog</caption>
      <thead>
        <tr>
          <th>SKU</th>
          <th>Name</th>
          <th>Category</th>
          <th>Price</th>
        </tr>
      </thead>
      <tbody id="catalog-body">
        <tr><td>SKU-101</td><td>Alpha Compute</td><td>compute</td><td>$120</td></tr>
        <tr><td>SKU-102</td><td>Beta Storage</td><td>storage</td><td>$80</td></tr>
        <tr><td>SKU-103</td><td>Gamma Compute</td><td>compute</td><td>$240</td></tr>
        <tr><td>SKU-104</td><td>Delta Archive</td><td>storage</td><td>$45</td></tr>
        <tr><td>SKU-105</td><td>Epsilon Mesh</td><td>compute</td><td>$160</td></tr>
      </tbody>
    </table>
  </main>
  <script>
    const ITEMS = [
      { sku: 'SKU-101', name: 'Alpha Compute', category: 'compute', price: 120 },
      { sku: 'SKU-102', name: 'Beta Storage', category: 'storage', price: 80 },
      { sku: 'SKU-103', name: 'Gamma Compute', category: 'compute', price: 240 },
      { sku: 'SKU-104', name: 'Delta Archive', category: 'storage', price: 45 },
      { sku: 'SKU-105', name: 'Epsilon Mesh', category: 'compute', price: 160 }
    ];
    let sortAsc = true;
    let sorted = false;
    const searchEl = document.getElementById('cat-search');
    const catEl = document.getElementById('cat-category');
    const sortBtn = document.getElementById('cat-sort-price');
    const resetBtn = document.getElementById('cat-reset');
    const tbody = document.getElementById('catalog-body');
    const countEl = document.getElementById('cat-count');

    function render() {
      const q = (searchEl.value || '').trim().toLowerCase();
      const cat = catEl.value || 'all';
      let rows = ITEMS.filter(item => {
        const matchQ = !q || item.name.toLowerCase().includes(q) || item.sku.toLowerCase().includes(q);
        const matchC = cat === 'all' || item.category === cat;
        return matchQ && matchC;
      });
      if (sorted) {
        rows = rows.slice().sort((a, b) => sortAsc ? (a.price - b.price) : (b.price - a.price));
      }
      tbody.innerHTML = rows.map(r =>
        `<tr><td>${r.sku}</td><td>${r.name}</td><td>${r.category}</td><td>$${r.price}</td></tr>`
      ).join('');
      countEl.textContent = `Showing ${rows.length} of ${ITEMS.length} items`;
    }

    searchEl.addEventListener('input', render);
    catEl.addEventListener('change', render);
    sortBtn.addEventListener('click', () => {
      if (sorted) sortAsc = !sortAsc;
      sorted = true;
      render();
    });
    resetBtn.addEventListener('click', () => {
      searchEl.value = '';
      catEl.value = 'all';
      sorted = false;
      sortAsc = true;
      render();
    });
  </script>
</body>
</html>"#
        .to_string()
}

pub fn dynamic_html() -> String {
    r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <title>Dynamic Hydration</title>
  <style>
    body { font-family: system-ui, sans-serif; margin: 24px; position: relative; }
    #dyn-box { position: relative; left: 0px; top: 0px; display: inline-block; padding: 8px 12px; border: 1px solid #888; }
    button, input { min-height: 28px; padding: 4px 8px; }
  </style>
</head>
<body>
  <main>
    <h1>Dynamic Hydration Page</h1>
    <div id="dyn-status" role="status">loading</div>
    <div id="dyn-box">settling</div>
    <ul id="dyn-list"></ul>
    <div id="dyn-actions"></div>
  </main>
  <script>
    setTimeout(async () => {
      try {
        const res = await fetch('/api/items?delay=80');
        const data = await res.json();
        const list = document.getElementById('dyn-list');
        list.innerHTML = (data.items || []).map(it => `<li>${it.label} ($${it.price})</li>`).join('');
        const box = document.getElementById('dyn-box');
        let tick = 0;
        const step = () => {
          tick += 1;
          box.style.left = (tick * 12) + 'px';
          if (tick < 3) {
            setTimeout(step, 25);
          } else {
            box.textContent = 'settled';
            document.getElementById('dyn-status').textContent = 'ready';
            const actions = document.getElementById('dyn-actions');
            const ready = document.createElement('div');
            ready.id = 'dyn-ready';
            ready.setAttribute('role', 'status');
            ready.textContent = 'DYNAMIC_HYDRATED';
            actions.appendChild(ready);

            const label = document.createElement('label');
            label.setAttribute('for', 'dyn-note');
            label.textContent = 'Hydration Note';
            actions.appendChild(label);

            const inp = document.createElement('input');
            inp.id = 'dyn-note';
            inp.type = 'text';
            actions.appendChild(inp);

            const btn = document.createElement('button');
            btn.id = 'dyn-action';
            btn.type = 'button';
            btn.textContent = 'Confirm Hydrated';
            btn.addEventListener('click', () => {
              let done = document.getElementById('dyn-confirmed');
              if (!done) {
                done = document.createElement('div');
                done.id = 'dyn-confirmed';
                done.setAttribute('role', 'status');
                actions.appendChild(done);
              }
              done.textContent = 'CONFIRMED:' + (inp.value || '');
            });
            actions.appendChild(btn);
          }
        };
        setTimeout(step, 25);
      } catch (e) {
        document.getElementById('dyn-status').textContent = 'error';
      }
    }, 30);
  </script>
</body>
</html>"#
        .to_string()
}

pub fn shadow_html() -> String {
    r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <title>Shadow DOM Fixture</title>
  <style>
    body { font-family: system-ui, sans-serif; margin: 24px; }
    #open-shadow-host, #closed-shadow-host { margin: 12px 0; padding: 12px; border: 1px solid #999; }
  </style>
</head>
<body>
  <main>
    <h1>Shadow DOM Workspace</h1>
    <div id="shadow-result" role="status">shadow-idle</div>
    <div id="open-shadow-host"></div>
    <div id="closed-shadow-host"></div>
  </main>
  <script>
    const resultEl = document.getElementById('shadow-result');

    const openHost = document.getElementById('open-shadow-host');
    const openRoot = openHost.attachShadow({ mode: 'open' });
    openRoot.innerHTML = `
      <style>input, button, a { min-height: 28px; display: inline-block; margin: 4px; }</style>
      <h2>Open Shadow Panel</h2>
      <input id="open-sh-input" aria-label="Open Shadow Input" type="text" />
      <button id="open-sh-btn" type="button">Open Shadow Action</button>
      <a href="/target">Open Shadow Link</a>
      <table>
        <caption>Open Shadow Table</caption>
        <thead><tr><th>Key</th><th>Value</th></tr></thead>
        <tbody><tr><td>mode</td><td>open</td></tr></tbody>
      </table>
    `;
    openRoot.getElementById('open-sh-btn').addEventListener('click', () => {
      const val = openRoot.getElementById('open-sh-input').value || '';
      resultEl.textContent = 'open-clicked:' + val;
    });

    const closedHost = document.getElementById('closed-shadow-host');
    const closedRoot = closedHost.attachShadow({ mode: 'closed' });
    closedRoot.innerHTML = `
      <style>input, button, a { min-height: 28px; display: inline-block; margin: 4px; }</style>
      <h2>Closed Shadow Panel</h2>
      <input id="closed-sh-input" aria-label="Closed Shadow Input" type="text" />
      <button id="closed-sh-btn" type="button">Closed Shadow Action</button>
      <a href="/target">Closed Shadow Link</a>
      <table>
        <caption>Closed Shadow Table</caption>
        <thead><tr><th>Key</th><th>Value</th></tr></thead>
        <tbody><tr><td>mode</td><td>closed</td></tr></tbody>
      </table>
    `;
    closedRoot.getElementById('closed-sh-btn').addEventListener('click', () => {
      const val = closedRoot.getElementById('closed-sh-input').value || '';
      resultEl.textContent = 'closed-clicked:' + val;
    });
  </script>
</body>
</html>"#
        .to_string()
}

pub fn frames_html(cross_origin_url: &str) -> String {
    let esc_xo = escape_html(cross_origin_url);
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <title>Nested Frames Fixture</title>
  <style>
    body {{ font-family: system-ui, sans-serif; margin: 24px; }}
    input, button {{ min-height: 28px; margin: 4px; }}
    iframe {{ display: block; border: 1px solid #888; margin-top: 12px; }}
  </style>
</head>
<body>
  <main>
    <h1>Frame Host</h1>
    <input id="host-input" aria-label="Host Input" type="text" />
    <button id="host-btn" type="button" onclick="document.getElementById('host-status').textContent = 'host-clicked:' + (document.getElementById('host-input').value || '');">Host Action</button>
    <div id="host-status" role="status">host-idle</div>
    <iframe id="same-frame" title="Same Origin Child" src="/frames/same-child" width="640" height="240"></iframe>
    <iframe id="cross-frame" title="Cross Origin Child" src="{esc_xo}" width="640" height="160"></iframe>
  </main>
</body>
</html>"#
    )
}

pub fn frames_same_child_html() -> String {
    r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <title>Same Origin Child</title>
  <style>
    body { font-family: system-ui, sans-serif; margin: 8px; }
    input, button { min-height: 28px; margin: 4px; }
    iframe { display: block; border: 1px solid #aaa; margin-top: 8px; }
  </style>
</head>
<body>
  <h2>Same Origin Child</h2>
  <input id="so-input" aria-label="Same Origin Input" type="text" />
  <button id="so-btn" type="button" onclick="document.getElementById('so-status').textContent = 'so-clicked:' + (document.getElementById('so-input').value || '');">Same Origin Action</button>
  <div id="so-status" role="status">so-idle</div>
  <iframe id="nested-inner-frame" title="Nested Inner Frame" src="/frames/nested-inner" width="580" height="110"></iframe>
</body>
</html>"#
        .to_string()
}

pub fn frames_nested_inner_html() -> String {
    r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <title>Nested Inner Frame</title>
  <style>
    body { font-family: system-ui, sans-serif; margin: 6px; }
    input, button { min-height: 28px; margin: 4px; }
  </style>
</head>
<body>
  <h3>Nested Inner</h3>
  <input id="inner-input" aria-label="Nested Inner Input" type="text" />
  <button id="inner-btn" type="button" onclick="document.getElementById('inner-status').textContent = 'inner-clicked:' + (document.getElementById('inner-input').value || '');">Nested Inner Action</button>
  <div id="inner-status" role="status">inner-idle</div>
</body>
</html>"#
        .to_string()
}

pub fn xo_child_html() -> String {
    r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <title>Cross Origin Child</title>
  <style>
    body { font-family: system-ui, sans-serif; margin: 8px; }
    input, select, button { min-height: 28px; margin: 4px; }
  </style>
</head>
<body>
  <h2>Cross Origin Child</h2>
  <input id="xo-input" aria-label="Cross Origin Input" type="text" />
  <select id="xo-select" aria-label="Cross Origin Select">
    <option value="a">Alpha</option>
    <option value="b">Beta</option>
  </select>
  <button id="xo-btn" type="button" onclick="document.getElementById('xo-status').textContent = 'xo-clicked:' + (document.getElementById('xo-input').value || '') + ':' + (document.getElementById('xo-select').value || '');">Cross Origin Action</button>
  <div id="xo-status" role="status">xo-idle</div>
</body>
</html>"#
        .to_string()
}

pub fn env_probe_html(cross_origin_frame_url: Option<&str>) -> String {
    let xo_iframe = cross_origin_frame_url
        .map(|u| {
            let esc = escape_html(u);
            format!(r#"<iframe id="env-xo-frame" src="{esc}"></iframe>"#)
        })
        .unwrap_or_default();
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <title>Environment Probe</title>
</head>
<body>
  <pre id="main-out"></pre>
  <pre id="frame-out"></pre>
  <pre id="xo-frame-out"></pre>
  <pre id="worker-out"></pre>
  <pre id="mworker-out"></pre>
  <iframe id="env-frame" src="/env-probe-frame"></iframe>
  {xo_iframe}
  <script>
    let webglVendor = null;
    let webglRenderer = null;
    try {{
      const c = document.createElement('canvas');
      const gl = c.getContext('webgl') || c.getContext('experimental-webgl');
      if (gl) {{
        const ext = gl.getExtension('WEBGL_debug_renderer_info');
        if (ext) {{
          webglVendor = gl.getParameter(ext.UNMASKED_VENDOR_WEBGL);
          webglRenderer = gl.getParameter(ext.UNMASKED_RENDERER_WEBGL);
        }}
      }}
    }} catch (e) {{}}
    let audioSampleRate = null;
    let audioBaseLatency = null;
    try {{
      const AC = window.AudioContext || window.webkitAudioContext;
      if (AC) {{
        const ac = new AC();
        audioSampleRate = ac.sampleRate;
        audioBaseLatency = ac.baseLatency;
        ac.close().catch(() => {{}});
      }}
    }} catch (e) {{}}
    let rtcBlocked = false;
    try {{
      new RTCPeerConnection();
    }} catch (e) {{
      rtcBlocked = true;
    }}
    document.getElementById('main-out').textContent = JSON.stringify({{
      userAgent: navigator.userAgent,
      platform: navigator.platform,
      language: navigator.language,
      languages: Array.from(navigator.languages || []),
      hardwareConcurrency: navigator.hardwareConcurrency,
      deviceMemory: navigator.deviceMemory ?? null,
      webdriver: navigator.webdriver,
      userAgentData: navigator.userAgentData ? {{
        platform: navigator.userAgentData.platform,
        mobile: navigator.userAgentData.mobile,
        brands: navigator.userAgentData.brands
      }} : null,
      screenWidth: screen.width,
      screenHeight: screen.height,
      innerWidth: window.innerWidth,
      innerHeight: window.innerHeight,
      devicePixelRatio: window.devicePixelRatio,
      timeZone: Intl.DateTimeFormat().resolvedOptions().timeZone,
      intlLocale: Intl.DateTimeFormat().resolvedOptions().locale,
      tzOffset: new Date().getTimezoneOffset(),
      webglVendor,
      webglRenderer,
      audioSampleRate,
      audioBaseLatency,
      rtcBlocked,
      noMimikyuGlobal: window.__mimikyu === undefined
    }});
    window.addEventListener('message', (ev) => {{
      if (ev.data && ev.data.__kind === 'frame') {{
        document.getElementById('frame-out').textContent = JSON.stringify(ev.data.payload);
      }} else if (ev.data && ev.data.__kind === 'xo-frame') {{
        document.getElementById('xo-frame-out').textContent = JSON.stringify(ev.data.payload);
      }}
    }});
    const w = new Worker('/env-worker.js');
    w.onmessage = (ev) => {{
      document.getElementById('worker-out').textContent = JSON.stringify(ev.data);
    }};
    w.onerror = (err) => {{
      document.getElementById('worker-out').textContent = JSON.stringify({{ error: String(err.message || err) }});
    }};
    const mw = new Worker('/env-worker.mjs', {{ type: 'module' }});
    mw.onmessage = (ev) => {{
      document.getElementById('mworker-out').textContent = JSON.stringify(ev.data);
    }};
    mw.onerror = (err) => {{
      document.getElementById('mworker-out').textContent = JSON.stringify({{ error: String(err.message || err) }});
    }};
  </script>
</body>
</html>"#
    )
}

pub fn env_probe_frame_html(kind: &str) -> String {
    let esc_kind = escape_html(kind);
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <title>Env Probe Frame</title>
</head>
<body>
  <script>
    const payload = {{
      userAgent: navigator.userAgent,
      platform: navigator.platform,
      language: navigator.language,
      languages: Array.from(navigator.languages || []),
      hardwareConcurrency: navigator.hardwareConcurrency,
      deviceMemory: navigator.deviceMemory ?? null,
      webdriver: navigator.webdriver,
      screenWidth: screen.width,
      screenHeight: screen.height,
      timeZone: Intl.DateTimeFormat().resolvedOptions().timeZone,
      noMimikyuGlobal: window.__mimikyu === undefined
    }};
    window.parent.postMessage({{ __kind: '{esc_kind}', payload }}, '*');
  </script>
</body>
</html>"#
    )
}

pub fn env_worker_classic_js() -> &'static str {
    r#"self.postMessage({
  userAgent: self.navigator.userAgent,
  platform: self.navigator.platform,
  language: self.navigator.language,
  languages: Array.from(self.navigator.languages || []),
  hardwareConcurrency: self.navigator.hardwareConcurrency,
  deviceMemory: self.navigator.deviceMemory ?? null,
  timeZone: Intl.DateTimeFormat().resolvedOptions().timeZone,
  tzOffset: new Date().getTimezoneOffset()
});"#
}

pub fn helper_mjs() -> &'static str {
    r#"export const helperTag = 'relative-helper-ok';
export function helperMeta() {
  return {
    helperUrl: import.meta.url,
    platformFromHelper: self.navigator.platform,
    uaFromHelper: self.navigator.userAgent,
    tzFromHelper: Intl.DateTimeFormat().resolvedOptions().timeZone
  };
}"#
}

pub fn env_worker_module_js() -> &'static str {
    r#"import { helperTag, helperMeta } from './helper.mjs';
const payload = {
  userAgent: self.navigator.userAgent,
  platform: self.navigator.platform,
  language: self.navigator.language,
  languages: Array.from(self.navigator.languages || []),
  hardwareConcurrency: self.navigator.hardwareConcurrency,
  deviceMemory: self.navigator.deviceMemory ?? null,
  timeZone: Intl.DateTimeFormat().resolvedOptions().timeZone,
  tzOffset: new Date().getTimezoneOffset(),
  helperTag,
  helperMeta: helperMeta(),
  workerMetaUrl: import.meta.url
};
self.postMessage(payload);
export default payload;"#
}

pub fn geo_page_html() -> String {
    r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <title>Geolocation Fixture</title>
</head>
<body>
  <main>
    <h1>Geolocation Probe</h1>
    <div id="geo-status" role="status">ready</div>
  </main>
</body>
</html>"#
        .to_string()
}
