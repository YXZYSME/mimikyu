use serde::{Deserialize, Serialize};

use crate::driver::DriverSession;
use crate::error::Result;
use crate::extract::markdown::collapse_ws;
use crate::extract::ExtractOptions;

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct RawCell {
    pub text: String,
    pub is_th: bool,
    #[serde(default = "default_span")]
    pub colspan: usize,
    #[serde(default = "default_span")]
    pub rowspan: usize,
}

fn default_span() -> usize {
    1
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct RawTable {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub caption: Option<String>,
    #[serde(default)]
    pub thead_rows: Vec<Vec<RawCell>>,
    #[serde(default)]
    pub body_rows: Vec<Vec<RawCell>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtractedTable {
    pub index: usize,
    pub id: Option<String>,
    pub caption: Option<String>,
    pub headers: Vec<String>,
    pub rows: Vec<Vec<String>>,
    pub row_count: usize,
    pub frame_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableExtractResult {
    pub url: String,
    pub tables: Vec<ExtractedTable>,
}

pub(crate) fn normalize_table_grid(
    raw: &RawTable,
    index: usize,
    frame_id: Option<String>,
) -> ExtractedTable {
    let mut all_rows: Vec<(bool, Vec<RawCell>)> = Vec::new();
    for r in &raw.thead_rows {
        all_rows.push((true, r.clone()));
    }
    for r in &raw.body_rows {
        all_rows.push((false, r.clone()));
    }

    let mut grid: Vec<Vec<Option<String>>> = Vec::new();
    let mut is_header_row: Vec<bool> = Vec::new();

    for (r_idx, (in_thead, cells)) in all_rows.iter().enumerate() {
        while grid.len() <= r_idx {
            grid.push(Vec::new());
        }
        let all_th = !cells.is_empty() && cells.iter().all(|c| c.is_th);
        is_header_row.push(*in_thead || (r_idx == 0 && raw.thead_rows.is_empty() && all_th));

        let mut col = 0usize;
        for cell in cells {
            while grid[r_idx].get(col).is_some_and(Option::is_some) {
                col += 1;
            }
            let txt = collapse_ws(&cell.text);
            let cs = cell.colspan.clamp(1, 64);
            let rs = cell.rowspan.clamp(1, 256);

            for dr in 0..rs {
                let target_r = r_idx + dr;
                while grid.len() <= target_r {
                    grid.push(Vec::new());
                }
                while grid[target_r].len() < col + cs {
                    grid[target_r].push(None);
                }
                for dc in 0..cs {
                    if grid[target_r][col + dc].is_none() {
                        grid[target_r][col + dc] = Some(txt.clone());
                    }
                }
            }
            col += cs;
        }
    }

    let max_cols = grid.iter().map(Vec::len).max().unwrap_or(0);
    let mut headers = Vec::new();
    let mut rows = Vec::new();

    for (r_idx, row_opt) in grid.into_iter().enumerate() {
        let mut norm_row: Vec<String> = (0..max_cols)
            .map(|c| row_opt.get(c).and_then(|o| o.clone()).unwrap_or_default())
            .collect();
        if norm_row.iter().all(|c| c.is_empty()) {
            continue;
        }
        let marked_header = is_header_row.get(r_idx).copied().unwrap_or(false);
        if marked_header && headers.is_empty() {
            headers = norm_row;
        } else {
            if marked_header && !headers.is_empty() {
                // Multi-row thead: merge non-empty subheaders into headers
                for (i, h) in headers.iter_mut().enumerate() {
                    let sub = norm_row.get(i).map(String::as_str).unwrap_or("");
                    if !sub.is_empty() && sub != h {
                        if h.is_empty() {
                            *h = sub.to_string();
                        } else {
                            *h = format!("{h} / {sub}");
                        }
                    }
                }
            } else {
                norm_row.resize(max_cols, String::new());
                rows.push(norm_row);
            }
        }
    }

    let row_count = rows.len();
    ExtractedTable {
        index,
        id: raw.id.clone().filter(|s| !s.is_empty()),
        caption: raw
            .caption
            .as_deref()
            .map(collapse_ws)
            .filter(|s| !s.is_empty()),
        headers,
        rows,
        row_count,
        frame_id,
    }
}

const JS_COLLECT_TABLES: &str = r#"
const isVisible = (el) => {
    if (!el || el.nodeType !== 1) return false;
    if (el.hasAttribute && (el.hasAttribute('hidden') || el.getAttribute('aria-hidden') === 'true')) return false;
    try {
        const win = (el.ownerDocument && el.ownerDocument.defaultView) || window;
        const cs = win.getComputedStyle(el);
        if (cs.display === 'none' || cs.visibility === 'hidden') return false;
    } catch {}
    return true;
};

const extractTablesFromRoot = (root, includeShadow) => {
    const tables = [];
    const seen = new Set();
    const visit = (node) => {
        if (!node) return;
        if (node.nodeType === 1) {
            if (!isVisible(node)) return;
            if (node.tagName === 'TABLE' && !seen.has(node)) {
                seen.add(node);
                const capEl = node.querySelector(':scope > caption');
                const thead_rows = [];
                const body_rows = [];
                const trs = Array.from(node.querySelectorAll('tr')).filter(tr => tr.closest('table') === node);
                for (const tr of trs) {
                    if (!isVisible(tr)) continue;
                    const cells = Array.from(tr.children).filter(c => (c.tagName === 'TH' || c.tagName === 'TD') && isVisible(c));
                    const mapped = cells.map(c => ({
                        text: (c.innerText || c.textContent || '').trim(),
                        is_th: c.tagName === 'TH',
                        colspan: Math.max(1, Number(c.getAttribute('colspan')) || 1),
                        rowspan: Math.max(1, Number(c.getAttribute('rowspan')) || 1),
                    }));
                    if (!mapped.length) continue;
                    if (tr.parentElement && tr.parentElement.tagName === 'THEAD') thead_rows.push(mapped);
                    else body_rows.push(mapped);
                }
                tables.push({
                    id: node.id || null,
                    caption: capEl ? (capEl.textContent || '').trim() : null,
                    thead_rows,
                    body_rows,
                });
            } else if ((node.getAttribute('role') === 'table' || node.getAttribute('role') === 'grid') && node.tagName !== 'TABLE' && !seen.has(node)) {
                seen.add(node);
                const thead_rows = [];
                const body_rows = [];
                const rows = Array.from(node.querySelectorAll('[role="row"]'));
                for (const r of rows) {
                    if (!isVisible(r)) continue;
                    const cells = Array.from(r.querySelectorAll('[role="columnheader"],[role="rowheader"],[role="cell"],[role="gridcell"]'));
                    const mapped = cells.map(c => ({
                        text: (c.innerText || c.textContent || '').trim(),
                        is_th: c.getAttribute('role') === 'columnheader',
                        colspan: 1,
                        rowspan: 1,
                    }));
                    if (!mapped.length) continue;
                    if (mapped.every(c => c.is_th) && thead_rows.length === 0 && body_rows.length === 0) thead_rows.push(mapped);
                    else body_rows.push(mapped);
                }
                tables.push({
                    id: node.id || null,
                    caption: node.getAttribute('aria-label') || null,
                    thead_rows,
                    body_rows,
                });
            }
            if (includeShadow && node.shadowRoot) visit(node.shadowRoot);
        }
        if (node.childNodes) {
            for (const ch of node.childNodes) visit(ch);
        }
    };
    visit(root);
    return tables;
};
"#;

async fn extract_tables_in_frame(
    session: &dyn DriverSession,
    opts: &ExtractOptions,
    frame_id: Option<&str>,
    start_idx: usize,
) -> Result<Vec<ExtractedTable>> {
    let sel_json = serde_json::to_string(&opts.selector)?;
    let include_shadow = opts.include_shadow_dom;
    let js = format!(
        r#"(() => {{
            {JS_COLLECT_TABLES}
            const sel = {sel_json};
            const root = sel ? document.querySelector(sel) : (document.body || document.documentElement);
            if (!root) return [];
            return extractTablesFromRoot(root, {include_shadow});
        }})()"#
    );
    let val = session.eval_isolated_in_frame(&js, frame_id).await?;
    let raws: Vec<RawTable> = serde_json::from_value(val).unwrap_or_default();
    let mut out = Vec::new();
    for raw in raws {
        let t = normalize_table_grid(&raw, start_idx + out.len(), frame_id.map(ToString::to_string));
        if !t.headers.is_empty() || !t.rows.is_empty() {
            out.push(t);
        }
    }

    if opts.include_shadow_dom
        && session.capabilities().supports_closed_shadow_dom
        && opts.selector.is_none()
    {
        let owned_main_fid;
        let shadow_fid = match frame_id {
            Some(f) => Some(f),
            None => {
                owned_main_fid = session
                    .list_frames()
                    .await
                    .ok()
                    .and_then(|fs| fs.into_iter().find(|f| f.is_main).map(|f| f.frame_id));
                owned_main_fid.as_deref()
            }
        };
        let hosts = session
            .closed_shadow_hosts(shadow_fid)
            .await
            .unwrap_or_default();
        let fn_decl = format!(
            r#"function() {{
                {JS_COLLECT_TABLES}
                return extractTablesFromRoot(this, true);
            }}"#
        );
        for bid in hosts {
            if let Ok(sr_val) = session
                .call_function_on_node(bid, frame_id, &fn_decl, vec![])
                .await
                && let Ok(sr_raws) = serde_json::from_value::<Vec<RawTable>>(sr_val)
            {
                for raw in sr_raws {
                    let t = normalize_table_grid(
                        &raw,
                        start_idx + out.len(),
                        frame_id.map(ToString::to_string),
                    );
                    if !t.headers.is_empty() || !t.rows.is_empty() {
                        out.push(t);
                    }
                }
            }
        }
    }

    Ok(out)
}

pub async fn extract_tables(
    session: &dyn DriverSession,
    opts: &ExtractOptions,
) -> Result<TableExtractResult> {
    let url = session.current_url().await.unwrap_or_default();
    let mut tables =
        extract_tables_in_frame(session, opts, opts.frame_id.as_deref(), 0).await?;
    if opts.include_frames && opts.frame_id.is_none() {
        let frames = session.list_frames().await.unwrap_or_default();
        for f in frames {
            if f.is_main {
                continue;
            }
            if let Ok(sub) =
                extract_tables_in_frame(session, opts, Some(&f.frame_id), tables.len()).await
            {
                tables.extend(sub);
            }
        }
    }
    Ok(TableExtractResult { url, tables })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_table_grid_expands_colspan_rowspan_and_promotes_th_row() {
        let raw = RawTable {
            id: Some("t1".into()),
            caption: Some(" Pricing ".into()),
            thead_rows: vec![],
            body_rows: vec![
                vec![
                    RawCell {
                        text: "Plan".into(),
                        is_th: true,
                        colspan: 1,
                        rowspan: 1,
                    },
                    RawCell {
                        text: "Metrics".into(),
                        is_th: true,
                        colspan: 2,
                        rowspan: 1,
                    },
                ],
                vec![
                    RawCell {
                        text: "Pro".into(),
                        is_th: false,
                        colspan: 1,
                        rowspan: 2,
                    },
                    RawCell {
                        text: "CPU".into(),
                        is_th: false,
                        colspan: 1,
                        rowspan: 1,
                    },
                    RawCell {
                        text: "4".into(),
                        is_th: false,
                        colspan: 1,
                        rowspan: 1,
                    },
                ],
                vec![
                    RawCell {
                        text: "RAM".into(),
                        is_th: false,
                        colspan: 1,
                        rowspan: 1,
                    },
                    RawCell {
                        text: "16G".into(),
                        is_th: false,
                        colspan: 1,
                        rowspan: 1,
                    },
                ],
            ],
        };

        let table = normalize_table_grid(&raw, 0, None);
        assert_eq!(table.headers, vec!["Plan", "Metrics", "Metrics"]);
        assert_eq!(
            table.rows,
            vec![
                vec!["Pro", "CPU", "4"],
                vec!["Pro", "RAM", "16G"],
            ]
        );
        assert_eq!(table.row_count, 2);
        assert_eq!(table.caption.as_deref(), Some("Pricing"));
    }
}
