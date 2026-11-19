use serde::{Deserialize, Serialize};

use crate::driver::DriverSession;
use crate::error::Result;
use crate::extract::tables::{normalize_table_grid, RawTable};
use crate::extract::ExtractOptions;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DocBlock {
    Heading {
        level: u8,
        text: String,
    },
    Paragraph {
        text: String,
    },
    List {
        ordered: bool,
        items: Vec<ListItemBlock>,
    },
    CodeBlock {
        language: Option<String>,
        code: String,
    },
    Blockquote {
        text: String,
    },
    Table {
        headers: Vec<String>,
        rows: Vec<Vec<String>>,
    },
    HorizontalRule,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListItemBlock {
    pub text: String,
    pub depth: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarkdownResult {
    pub url: String,
    pub title: String,
    pub markdown: String,
    pub char_count: usize,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextResult {
    pub url: String,
    pub title: String,
    pub text: String,
    pub char_count: usize,
    pub truncated: bool,
}

#[derive(Debug, Clone, Deserialize)]
struct JsExtractPayload {
    #[serde(default)]
    blocks: Vec<JsRawBlock>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum JsRawBlock {
    Heading {
        level: u8,
        text: String,
    },
    Paragraph {
        text: String,
    },
    List {
        ordered: bool,
        items: Vec<ListItemBlock>,
    },
    CodeBlock {
        language: Option<String>,
        code: String,
    },
    Blockquote {
        text: String,
    },
    Table {
        raw: RawTable,
    },
    HorizontalRule,
}

impl JsRawBlock {
    fn into_doc_block(self) -> Option<DocBlock> {
        match self {
            JsRawBlock::Heading { level, text } => {
                let t = collapse_ws(&text);
                if t.is_empty() {
                    None
                } else {
                    Some(DocBlock::Heading {
                        level: level.clamp(1, 6),
                        text: t,
                    })
                }
            }
            JsRawBlock::Paragraph { text } => {
                let t = collapse_ws(&text);
                if t.is_empty() {
                    None
                } else {
                    Some(DocBlock::Paragraph { text: t })
                }
            }
            JsRawBlock::List { ordered, items } => {
                let cleaned: Vec<ListItemBlock> = items
                    .into_iter()
                    .filter_map(|it| {
                        let t = collapse_ws(&it.text);
                        if t.is_empty() {
                            None
                        } else {
                            Some(ListItemBlock {
                                text: t,
                                depth: it.depth,
                            })
                        }
                    })
                    .collect();
                if cleaned.is_empty() {
                    None
                } else {
                    Some(DocBlock::List {
                        ordered,
                        items: cleaned,
                    })
                }
            }
            JsRawBlock::CodeBlock { language, code } => {
                let trimmed = code.trim_matches('\n').to_string();
                if trimmed.trim().is_empty() {
                    None
                } else {
                    Some(DocBlock::CodeBlock {
                        language: language.filter(|l| !l.trim().is_empty()),
                        code: trimmed,
                    })
                }
            }
            JsRawBlock::Blockquote { text } => {
                let t = collapse_ws(&text);
                if t.is_empty() {
                    None
                } else {
                    Some(DocBlock::Blockquote { text: t })
                }
            }
            JsRawBlock::Table { raw } => {
                let norm = normalize_table_grid(&raw, 0, None);
                if norm.headers.is_empty() && norm.rows.is_empty() {
                    None
                } else {
                    Some(DocBlock::Table {
                        headers: norm.headers,
                        rows: norm.rows,
                    })
                }
            }
            JsRawBlock::HorizontalRule => Some(DocBlock::HorizontalRule),
        }
    }
}

pub fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn truncate_chars(s: &str, max_chars: Option<usize>) -> (String, bool) {
    let Some(limit) = max_chars else {
        return (s.to_string(), false);
    };
    if limit == 0 {
        return (String::new(), !s.is_empty());
    }
    let mut indices = s.char_indices();
    if let Some((byte_idx, _)) = indices.nth(limit) {
        (s[..byte_idx].to_string(), true)
    } else {
        (s.to_string(), false)
    }
}

pub fn render_markdown(blocks: &[DocBlock]) -> String {
    let mut parts = Vec::new();
    for block in blocks {
        match block {
            DocBlock::Heading { level, text } => {
                let hashes = "#".repeat(usize::from((*level).clamp(1, 6)));
                parts.push(format!("{hashes} {text}"));
            }
            DocBlock::Paragraph { text } => {
                parts.push(text.clone());
            }
            DocBlock::List { ordered, items } => {
                let mut lines = Vec::new();
                let mut counters: std::collections::HashMap<usize, usize> =
                    std::collections::HashMap::new();
                for item in items {
                    let indent = "  ".repeat(item.depth.min(6));
                    if *ordered {
                        counters.retain(|&d, _| d <= item.depth);
                        let count = counters.entry(item.depth).or_insert(0);
                        *count += 1;
                        lines.push(format!("{indent}{count}. {}", item.text));
                    } else {
                        lines.push(format!("{indent}- {}", item.text));
                    }
                }
                parts.push(lines.join("\n"));
            }
            DocBlock::CodeBlock { language, code } => {
                let lang = language.as_deref().unwrap_or("");
                parts.push(format!("```{lang}\n{code}\n```"));
            }
            DocBlock::Blockquote { text } => {
                let quoted = text
                    .lines()
                    .map(|l| format!("> {l}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                parts.push(quoted);
            }
            DocBlock::Table { headers, rows } => {
                let cols = headers
                    .len()
                    .max(rows.iter().map(Vec::len).max().unwrap_or(0));
                if cols == 0 {
                    continue;
                }
                let esc = |s: &str| s.replace('|', "\\|");
                let mut lines = Vec::new();
                let hdr_cells: Vec<String> = (0..cols)
                    .map(|i| esc(headers.get(i).map(String::as_str).unwrap_or("")))
                    .collect();
                lines.push(format!("| {} |", hdr_cells.join(" | ")));
                let sep = vec!["---"; cols].join(" | ");
                lines.push(format!("| {sep} |"));
                for row in rows {
                    let r_cells: Vec<String> = (0..cols)
                        .map(|i| esc(row.get(i).map(String::as_str).unwrap_or("")))
                        .collect();
                    lines.push(format!("| {} |", r_cells.join(" | ")));
                }
                parts.push(lines.join("\n"));
            }
            DocBlock::HorizontalRule => {
                parts.push("---".to_string());
            }
        }
    }
    parts.join("\n\n")
}

pub fn render_text(blocks: &[DocBlock]) -> String {
    let mut lines = Vec::new();
    for block in blocks {
        match block {
            DocBlock::Heading { text, .. }
            | DocBlock::Paragraph { text }
            | DocBlock::Blockquote { text } => {
                lines.push(text.clone());
            }
            DocBlock::List { items, .. } => {
                for item in items {
                    lines.push(item.text.clone());
                }
            }
            DocBlock::CodeBlock { code, .. } => {
                lines.push(code.clone());
            }
            DocBlock::Table { headers, rows } => {
                if !headers.is_empty() {
                    lines.push(headers.join("\t"));
                }
                for row in rows {
                    lines.push(row.join("\t"));
                }
            }
            DocBlock::HorizontalRule => {}
        }
    }
    lines.join("\n")
}

const JS_DOC_WALKER_HELPERS: &str = r#"
const isVisible = (el) => {
    if (!el || el.nodeType !== 1) return false;
    const tag = el.tagName;
    if (tag === 'SCRIPT' || tag === 'STYLE' || tag === 'NOSCRIPT' || tag === 'TEMPLATE') return false;
    if (el.hasAttribute && (el.hasAttribute('hidden') || el.getAttribute('aria-hidden') === 'true')) return false;
    try {
        const win = (el.ownerDocument && el.ownerDocument.defaultView) || window;
        const cs = win.getComputedStyle(el);
        if (cs.display === 'none' || cs.visibility === 'hidden') return false;
    } catch {}
    return true;
};

const childNodesWithSlotsAndShadow = (node, includeShadow) => {
    const out = [];
    if (node.nodeType === 1 && node.tagName === 'SLOT' && typeof node.assignedNodes === 'function') {
        const assigned = node.assignedNodes({ flatten: true });
        if (assigned && assigned.length) return assigned;
    }
    if (node.childNodes) {
        for (const ch of node.childNodes) out.push(ch);
    }
    if (includeShadow && node.nodeType === 1 && node.shadowRoot) {
        for (const ch of node.shadowRoot.childNodes) out.push(ch);
    }
    return out;
};

const inlineText = (node, includeShadow, forMarkdown) => {
    if (!node) return '';
    if (node.nodeType === 3) return node.nodeValue || '';
    if (node.nodeType !== 1) return '';
    const tag = node.tagName;
    if (tag === 'SVG') {
        const title = node.querySelector('title');
        return title ? (title.textContent || '').trim() : '';
    }
    if (!isVisible(node)) return '';
    if (tag === 'BR') return ' ';
    if (tag === 'IMG') return (node.getAttribute('alt') || '').trim();
    if (tag === 'CODE' && forMarkdown && (!node.parentElement || node.parentElement.tagName !== 'PRE')) {
        const t = (node.textContent || '').trim();
        return t ? '`' + t + '`' : '';
    }
    if (tag === 'A' && forMarkdown) {
        const label = childNodesWithSlotsAndShadow(node, includeShadow)
            .map(c => inlineText(c, includeShadow, false))
            .join('').replace(/\s+/g, ' ').trim();
        const href = node.getAttribute('href') || '';
        if (label && href) return `[${label}](${href})`;
        return label || href;
    }
    return childNodesWithSlotsAndShadow(node, includeShadow)
        .map(c => inlineText(c, includeShadow, forMarkdown))
        .join('');
};

const collectList = (listEl, depth, includeShadow, forMarkdown, out) => {
    for (const ch of childNodesWithSlotsAndShadow(listEl, includeShadow)) {
        if (ch.nodeType !== 1) continue;
        if (ch.tagName === 'LI' && isVisible(ch)) {
            let ownParts = [];
            const nestedLists = [];
            for (const sub of childNodesWithSlotsAndShadow(ch, includeShadow)) {
                if (sub.nodeType === 1 && (sub.tagName === 'UL' || sub.tagName === 'OL')) {
                    nestedLists.push(sub);
                } else {
                    ownParts.push(inlineText(sub, includeShadow, forMarkdown));
                }
            }
            const txt = ownParts.join('').replace(/\s+/g, ' ').trim();
            if (txt) out.push({ text: txt, depth });
            for (const nl of nestedLists) collectList(nl, depth + 1, includeShadow, forMarkdown, out);
        }
    }
};

const collectTable = (tableEl, includeShadow) => {
    const capEl = tableEl.querySelector(':scope > caption');
    const caption = capEl ? (capEl.textContent || '').trim() : null;
    const theadRows = [];
    const bodyRows = [];
    const allTrs = Array.from(tableEl.querySelectorAll('tr')).filter(tr => tr.closest('table') === tableEl);
    for (const tr of allTrs) {
        if (!isVisible(tr)) continue;
        const cells = Array.from(tr.children).filter(c => (c.tagName === 'TH' || c.tagName === 'TD') && isVisible(c));
        const rowCells = cells.map(c => ({
            text: inlineText(c, includeShadow, false).replace(/\s+/g, ' ').trim(),
            is_th: c.tagName === 'TH',
            colspan: Math.max(1, Number(c.getAttribute('colspan')) || 1),
            rowspan: Math.max(1, Number(c.getAttribute('rowspan')) || 1),
        }));
        if (!rowCells.length) continue;
        if (tr.parentElement && tr.parentElement.tagName === 'THEAD') {
            theadRows.push(rowCells);
        } else {
            bodyRows.push(rowCells);
        }
    }
    return {
        id: tableEl.id || null,
        caption: caption || null,
        thead_rows: theadRows,
        body_rows: bodyRows,
    };
};

const BLOCK_TAGS = new Set([
    'H1','H2','H3','H4','H5','H6','P','UL','OL','PRE','BLOCKQUOTE','TABLE','HR',
    'DIV','SECTION','ARTICLE','MAIN','HEADER','FOOTER','NAV','ASIDE','DETAILS','FIGURE','FIGCAPTION','FORM'
]);

const collectBlocks = (root, includeShadow, forMarkdown, blocks) => {
    if (!root) return;
    let inlineAcc = '';
    const flushInline = () => {
        const t = inlineAcc.replace(/\s+/g, ' ').trim();
        inlineAcc = '';
        if (t) blocks.push({ kind: 'paragraph', text: t });
    };
    const walkChildren = childNodesWithSlotsAndShadow(root, includeShadow);
    for (const node of walkChildren) {
        if (node.nodeType === 3) {
            inlineAcc += (node.nodeValue || '');
            continue;
        }
        if (node.nodeType !== 1) continue;
        const tag = node.tagName;
        if (!isVisible(node) || tag === 'SVG') continue;

        if (/^H([1-6])$/.test(tag)) {
            flushInline();
            const t = inlineText(node, includeShadow, forMarkdown).replace(/\s+/g, ' ').trim();
            if (t) blocks.push({ kind: 'heading', level: Number(tag.slice(1)), text: t });
            continue;
        }
        if (tag === 'P' || tag === 'FIGCAPTION') {
            flushInline();
            const t = inlineText(node, includeShadow, forMarkdown).replace(/\s+/g, ' ').trim();
            if (t) blocks.push({ kind: 'paragraph', text: t });
            continue;
        }
        if (tag === 'UL' || tag === 'OL') {
            flushInline();
            const items = [];
            collectList(node, 0, includeShadow, forMarkdown, items);
            if (items.length) blocks.push({ kind: 'list', ordered: tag === 'OL', items });
            continue;
        }
        if (tag === 'PRE') {
            flushInline();
            const codeEl = node.querySelector('code');
            let lang = null;
            if (codeEl && codeEl.className) {
                const m = String(codeEl.className).match(/language-([a-zA-Z0-9_-]+)/);
                if (m) lang = m[1];
            }
            const code = (codeEl ? codeEl.textContent : node.textContent) || '';
            blocks.push({ kind: 'code_block', language: lang, code });
            continue;
        }
        if (tag === 'BLOCKQUOTE') {
            flushInline();
            const t = inlineText(node, includeShadow, forMarkdown).replace(/\s+/g, ' ').trim();
            if (t) blocks.push({ kind: 'blockquote', text: t });
            continue;
        }
        if (tag === 'TABLE') {
            flushInline();
            blocks.push({ kind: 'table', raw: collectTable(node, includeShadow) });
            continue;
        }
        if (tag === 'HR') {
            flushInline();
            blocks.push({ kind: 'horizontal_rule' });
            continue;
        }
        const hasBlockDescendant = Array.from(node.children || []).some(c => BLOCK_TAGS.has(c.tagName))
            || (includeShadow && node.shadowRoot);
        if (BLOCK_TAGS.has(tag) || hasBlockDescendant) {
            flushInline();
            collectBlocks(node, includeShadow, forMarkdown, blocks);
        } else {
            inlineAcc += inlineText(node, includeShadow, forMarkdown);
        }
    }
    flushInline();
};
"#;

async fn extract_blocks_in_frame(
    session: &dyn DriverSession,
    opts: &ExtractOptions,
    frame_id: Option<&str>,
    for_markdown: bool,
) -> Result<Vec<DocBlock>> {
    let sel_json = serde_json::to_string(&opts.selector)?;
    let include_shadow = opts.include_shadow_dom;
    let main_only = opts.main_only;
    let js = format!(
        r#"(() => {{
            {JS_DOC_WALKER_HELPERS}
            const sel = {sel_json};
            let root = sel ? document.querySelector(sel) : null;
            if (!sel) {{
                root = ({main_only_flag} ? (document.querySelector('main, [role="main"], article') || document.body) : document.body) || document.documentElement;
            }}
            if (!root) return {{ blocks: [] }};
            const blocks = [];
            collectBlocks(root, {include_shadow}, {for_markdown}, blocks);
            return {{ blocks }};
        }})()"#,
        main_only_flag = main_only,
    );
    let val = session.eval_isolated_in_frame(&js, frame_id).await?;
    let payload: JsExtractPayload = serde_json::from_value(val).unwrap_or(JsExtractPayload {
        blocks: Vec::new(),
    });
    let mut blocks: Vec<DocBlock> = payload
        .blocks
        .into_iter()
        .filter_map(JsRawBlock::into_doc_block)
        .collect();

    if opts.include_shadow_dom && session.capabilities().supports_closed_shadow_dom {
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
        let closed_hosts = session
            .closed_shadow_hosts(shadow_fid)
            .await
            .unwrap_or_default();
        let closed_fn = format!(
            r#"function(forMd, sel, mainOnly) {{
                {JS_DOC_WALKER_HELPERS}
                const isSubtreeVisible = (startEl) => {{
                    let cur = startEl;
                    while (cur) {{
                        if (cur.nodeType === 1) {{
                            if (!isVisible(cur)) return false;
                            cur = cur.parentElement || (cur.getRootNode ? cur.getRootNode().host : null);
                        }} else if (cur.nodeType === 11) {{
                            cur = cur.host || null;
                        }} else {{
                            cur = null;
                        }}
                    }}
                    return true;
                }};
                const isInsideScope = (startEl, scopeRoot) => {{
                    if (!scopeRoot) return false;
                    let cur = startEl;
                    while (cur) {{
                        if (cur === scopeRoot) return true;
                        if (cur.nodeType === 1) {{
                            cur = cur.parentElement || (cur.getRootNode ? cur.getRootNode().host : null);
                        }} else if (cur.nodeType === 11) {{
                            cur = cur.host || null;
                        }} else {{
                            cur = null;
                        }}
                    }}
                    return false;
                }};
                if (!this || !this.host || !isSubtreeVisible(this.host)) return {{ blocks: [] }};
                let scopeRoot = sel ? document.querySelector(sel) : null;
                if (!sel) {{
                    scopeRoot = (mainOnly ? (document.querySelector('main, [role="main"], article') || document.body) : document.body) || document.documentElement;
                }}
                if (!isInsideScope(this.host, scopeRoot)) return {{ blocks: [] }};
                const blocks = [];
                collectBlocks(this, true, forMd, blocks);
                return {{ blocks }};
            }}"#
        );
        for sr_bid in closed_hosts {
            if let Ok(sr_val) = session
                .call_function_on_node(
                    sr_bid,
                    frame_id,
                    &closed_fn,
                    vec![
                        serde_json::json!(for_markdown),
                        serde_json::json!(opts.selector),
                        serde_json::json!(opts.main_only),
                    ],
                )
                .await
                && let Ok(sr_payload) = serde_json::from_value::<JsExtractPayload>(sr_val)
            {
                blocks.extend(
                    sr_payload
                        .blocks
                        .into_iter()
                        .filter_map(JsRawBlock::into_doc_block),
                );
            }
        }
    }

    Ok(blocks)
}

async fn extract_all_blocks(
    session: &dyn DriverSession,
    opts: &ExtractOptions,
    for_markdown: bool,
) -> Result<Vec<DocBlock>> {
    let mut blocks =
        extract_blocks_in_frame(session, opts, opts.frame_id.as_deref(), for_markdown).await?;
    if opts.include_frames && opts.frame_id.is_none() {
        let frames = session.list_frames().await.unwrap_or_default();
        for f in frames {
            if f.is_main {
                continue;
            }
            if let Ok(sub_blocks) =
                extract_blocks_in_frame(session, opts, Some(&f.frame_id), for_markdown).await
            {
                blocks.extend(sub_blocks);
            }
        }
    }
    Ok(blocks)
}

pub async fn extract_markdown(
    session: &dyn DriverSession,
    opts: &ExtractOptions,
) -> Result<MarkdownResult> {
    let url = session.current_url().await.unwrap_or_default();
    let title = session.title().await.unwrap_or_default();
    let blocks = extract_all_blocks(session, opts, true).await?;
    let raw_md = render_markdown(&blocks);
    let (markdown, truncated) = truncate_chars(&raw_md, opts.max_chars);
    let char_count = markdown.chars().count();
    Ok(MarkdownResult {
        url,
        title,
        markdown,
        char_count,
        truncated,
    })
}

pub async fn extract_text(
    session: &dyn DriverSession,
    opts: &ExtractOptions,
) -> Result<TextResult> {
    let url = session.current_url().await.unwrap_or_default();
    let title = session.title().await.unwrap_or_default();
    let blocks = extract_all_blocks(session, opts, false).await?;
    let raw_text = render_text(&blocks);
    let (text, truncated) = truncate_chars(&raw_text, opts.max_chars);
    let char_count = text.chars().count();
    Ok(TextResult {
        url,
        title,
        text,
        char_count,
        truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_markdown_and_text_handle_blocks_and_utf8_truncation() {
        let blocks = vec![
            DocBlock::Heading {
                level: 1,
                text: "Title 🚀".into(),
            },
            DocBlock::Paragraph {
                text: "Hello [world](https://example.com)".into(),
            },
            DocBlock::List {
                ordered: true,
                items: vec![
                    ListItemBlock {
                        text: "First".into(),
                        depth: 0,
                    },
                    ListItemBlock {
                        text: "Nested".into(),
                        depth: 1,
                    },
                ],
            },
            DocBlock::CodeBlock {
                language: Some("rust".into()),
                code: "fn main() {}".into(),
            },
            DocBlock::Blockquote {
                text: "Quote".into(),
            },
            DocBlock::Table {
                headers: vec!["A".into(), "B|C".into()],
                rows: vec![vec!["1".into(), "2".into()]],
            },
        ];

        let md = render_markdown(&blocks);
        assert!(md.contains("# Title 🚀"));
        assert!(md.contains("1. First"));
        assert!(md.contains("  1. Nested"));
        assert!(md.contains("```rust\nfn main() {}\n```"));
        assert!(md.contains("> Quote"));
        assert!(md.contains("| A | B\\|C |"));

        let txt = render_text(&blocks);
        assert!(txt.contains("Title 🚀"));
        assert!(txt.contains("fn main() {}"));

        let (trunc, did) = truncate_chars("a🚀b漢字", Some(3));
        assert!(did);
        assert_eq!(trunc, "a🚀b");
    }
}
