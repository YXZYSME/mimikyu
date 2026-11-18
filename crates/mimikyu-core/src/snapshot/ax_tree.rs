use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::driver::{AxNode, BoundingBox};
use crate::snapshot::ref_map::{RefEntry, RefMap};
use crate::snapshot::SnapshotOptions;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SemanticNode {
    pub ref_id: Option<String>,
    pub depth: usize,
    pub role: String,
    pub name: Option<String>,
    pub value: Option<String>,
    pub frame_id: Option<String>,
    pub backend_node_id: Option<i64>,
    pub bounds: Option<BoundingBox>,
    pub css_fallback: Option<String>,
    pub disabled: bool,
    pub focused: bool,
    pub focusable: bool,
    pub checked: Option<bool>,
    pub expanded: Option<bool>,
    pub selected: Option<bool>,
    pub required: bool,
    pub level: Option<u32>,
    pub children: Vec<SemanticNode>,
}

pub fn is_interactive_role(role: &str) -> bool {
    matches!(
        role.to_ascii_lowercase().as_str(),
        "button"
            | "link"
            | "textbox"
            | "searchbox"
            | "combobox"
            | "listbox"
            | "option"
            | "checkbox"
            | "radio"
            | "switch"
            | "slider"
            | "spinbutton"
            | "tab"
            | "menuitem"
            | "menuitemcheckbox"
            | "menuitemradio"
            | "treeitem"
    )
}

fn is_decorative_or_layout_role(role: &str) -> bool {
    matches!(
        role.to_ascii_lowercase().as_str(),
        "none" | "presentation" | "inlinetextbox" | "linebreak"
    )
}

fn is_transparent_wrapper(node: &AxNode) -> bool {
    let r = node.role.to_ascii_lowercase();
    if is_decorative_or_layout_role(&r) {
        return true;
    }
    if node.ignored {
        return true;
    }
    if matches!(r.as_str(), "generic" | "section" | "div" | "group" | "paragraph")
        && node.name.as_deref().map(str::trim).unwrap_or("").is_empty()
        && node.value.as_deref().map(str::trim).unwrap_or("").is_empty()
        && !node.focusable
        && !node.focused
        && !node.disabled
        && node.checked.is_none()
        && node.expanded.is_none()
        && node.selected.is_none()
    {
        return true;
    }
    false
}

fn is_static_text_role(role: &str) -> bool {
    matches!(
        role.to_ascii_lowercase().as_str(),
        "statictext" | "text" | "inlinetextbox"
    )
}

fn parent_owns_text(role: &str) -> bool {
    matches!(
        role.to_ascii_lowercase().as_str(),
        "button"
            | "link"
            | "heading"
            | "menuitem"
            | "menuitemcheckbox"
            | "menuitemradio"
            | "tab"
            | "option"
            | "checkbox"
            | "radio"
            | "switch"
            | "label"
            | "columnheader"
            | "rowheader"
            | "cell"
            | "listitem"
    )
}

pub fn build_semantic_tree(
    raw_nodes: &[AxNode],
    opts: &SnapshotOptions,
) -> (Vec<SemanticNode>, RefMap) {
    if raw_nodes.is_empty() {
        return (Vec::new(), RefMap::new());
    }

    let by_id: HashMap<&str, &AxNode> = raw_nodes
        .iter()
        .map(|n| (n.node_id.as_str(), n))
        .collect();

    let mut children_map: HashMap<&str, Vec<&str>> = HashMap::new();
    for n in raw_nodes {
        children_map
            .entry(n.node_id.as_str())
            .or_default()
            .extend(n.child_ids.iter().map(String::as_str));
    }
    // Also attach nodes via parent_id if parent's child_ids omitted them.
    for n in raw_nodes {
        if let Some(ref pid) = n.parent_id
            && by_id.contains_key(pid.as_str())
        {
            let list = children_map.entry(pid.as_str()).or_default();
            if !list.contains(&n.node_id.as_str()) {
                list.push(n.node_id.as_str());
            }
        }
    }

    let roots: Vec<&AxNode> = raw_nodes
        .iter()
        .filter(|n| {
            n.parent_id
                .as_deref()
                .is_none_or(|pid| !by_id.contains_key(pid))
        })
        .collect();

    let ctx = NormalizeCtx {
        by_id: &by_id,
        children_map: &children_map,
        max_depth: opts.max_depth,
    };
    let mut visited = HashSet::new();
    let mut normalized_roots = Vec::new();
    for root in roots {
        let mut built = normalize_subtree(root, None, true, 0, &ctx, &mut visited);
        normalized_roots.append(&mut built);
    }

    // Unwrap top-level RootWebArea so page landmarks/elements start at depth 0.
    let mut unwrapped = Vec::new();
    for node in normalized_roots {
        if node.role.eq_ignore_ascii_case("RootWebArea") {
            unwrapped.extend(node.children);
        } else {
            unwrapped.push(node);
        }
    }

    let mut final_tree = if opts.interactive_only {
        let mut interactive = Vec::new();
        collect_interactive(unwrapped, &mut interactive);
        interactive
    } else {
        unwrapped
    };

    assign_depths_and_prune_depth(&mut final_tree, 0, opts.max_depth);

    let mut ref_map = RefMap::new();
    let mut next_ref = 1usize;
    assign_refs_preorder(&mut final_tree, &mut next_ref, &mut ref_map);

    (final_tree, ref_map)
}

struct NormalizeCtx<'a> {
    by_id: &'a HashMap<&'a str, &'a AxNode>,
    children_map: &'a HashMap<&'a str, Vec<&'a str>>,
    max_depth: Option<usize>,
}

fn normalize_subtree<'a>(
    node: &'a AxNode,
    parent_name_if_owned: Option<&str>,
    at_root: bool,
    raw_depth: usize,
    ctx: &NormalizeCtx<'a>,
    visited: &mut HashSet<&'a str>,
) -> Vec<SemanticNode> {
    if let Some(limit) = ctx.max_depth
        && raw_depth > limit
    {
        return Vec::new();
    }
    if !visited.insert(node.node_id.as_str()) {
        return Vec::new();
    }

    let is_top_level_root_web_area = at_root && node.role.eq_ignore_ascii_case("RootWebArea");
    let child_raw_depth = if is_top_level_root_web_area {
        raw_depth
    } else {
        raw_depth + 1
    };

    let trimmed_name = node
        .name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string);

    let child_owned_name = if parent_owns_text(&node.role) {
        trimmed_name.as_deref()
    } else {
        None
    };

    let mut children = Vec::new();
    if let Some(cids) = ctx.children_map.get(node.node_id.as_str()) {
        for cid in cids {
            if let Some(child_ax) = ctx.by_id.get(cid) {
                children.extend(normalize_subtree(
                    child_ax,
                    child_owned_name,
                    false,
                    child_raw_depth,
                    ctx,
                    visited,
                ));
            }
        }
    }

    // Drop duplicate or empty StaticText nodes.
    if is_static_text_role(&node.role) {
        let txt = trimmed_name.as_deref().unwrap_or("");
        if txt.is_empty() {
            return children;
        }
        if let Some(p_name) = parent_name_if_owned
            && txt == p_name
        {
            return children;
        }
    }

    // Prune zero-rect invisible nodes AFTER lifting any visible children.
    if let Some(b) = node.bounds
        && !b.is_visible()
    {
        return children;
    }

    // Re-parent children of ignored / decorative / empty layout wrapper nodes.
    if is_transparent_wrapper(node) {
        return children;
    }

    // Collapse redundant single-child generic wrapper if it carries no identity/state.
    if node.role.eq_ignore_ascii_case("generic")
        && trimmed_name.is_none()
        && node.value.is_none()
        && !node.focusable
        && !node.focused
        && !node.disabled
        && node.checked.is_none()
        && node.expanded.is_none()
        && node.selected.is_none()
        && children.len() == 1
    {
        return children;
    }

    vec![SemanticNode {
        ref_id: None,
        depth: 0,
        role: node.role.clone(),
        name: trimmed_name,
        value: node.value.clone(),
        frame_id: node.frame_id.clone(),
        backend_node_id: node.backend_node_id,
        bounds: node.bounds,
        css_fallback: None,
        disabled: node.disabled,
        focused: node.focused,
        focusable: node.focusable,
        checked: node.checked,
        expanded: node.expanded,
        selected: node.selected,
        required: node.required,
        level: node.level,
        children,
    }]
}

fn collect_interactive(nodes: Vec<SemanticNode>, out: &mut Vec<SemanticNode>) {
    for mut node in nodes {
        let children = std::mem::take(&mut node.children);
        let keep = (is_interactive_role(&node.role) || node.focusable)
            && !node.role.eq_ignore_ascii_case("RootWebArea")
            && !node.role.eq_ignore_ascii_case("Iframe");
        if keep {
            out.push(node);
        }
        collect_interactive(children, out);
    }
}

fn assign_depths_and_prune_depth(
    nodes: &mut Vec<SemanticNode>,
    depth: usize,
    max_depth: Option<usize>,
) {
    if let Some(limit) = max_depth
        && depth > limit
    {
        nodes.clear();
        return;
    }
    for node in nodes.iter_mut() {
        node.depth = depth;
        if let Some(limit) = max_depth
            && depth >= limit
        {
            node.children.clear();
        } else {
            assign_depths_and_prune_depth(&mut node.children, depth + 1, max_depth);
        }
    }
}

fn is_ref_eligible(node: &SemanticNode) -> bool {
    if node.focusable || is_interactive_role(&node.role) {
        return true;
    }
    !matches!(
        node.role.to_ascii_lowercase().as_str(),
        "statictext"
            | "text"
            | "inlinetextbox"
            | "linebreak"
            | "labeltext"
            | "label"
            | "legend"
            | "menulistpopup"
    )
}

fn assign_refs_preorder(
    nodes: &mut [SemanticNode],
    next_ref: &mut usize,
    ref_map: &mut RefMap,
) {
    for node in nodes.iter_mut() {
        if is_ref_eligible(node) {
            let ref_id = format!("@e{}", *next_ref);
            *next_ref += 1;
            node.ref_id = Some(ref_id.clone());
            ref_map.insert(RefEntry {
                ref_id,
                role: node.role.clone(),
                name: node.name.clone(),
                value: node.value.clone(),
                frame_id: node.frame_id.clone(),
                backend_node_id: node.backend_node_id,
                bounds: node.bounds,
                css_fallback: node.css_fallback.clone(),
                disabled: node.disabled,
                focused: node.focused,
                checked: node.checked,
                expanded: node.expanded,
                selected: node.selected,
            });
        }
        assign_refs_preorder(&mut node.children, next_ref, ref_map);
    }
}

pub fn format_semantic_tree(nodes: &[SemanticNode]) -> String {
    let mut lines = Vec::new();
    format_nodes_recursive(nodes, &mut lines);
    lines.join("\n")
}

fn format_nodes_recursive(nodes: &[SemanticNode], lines: &mut Vec<String>) {
    for node in nodes {
        let indent = "  ".repeat(node.depth);
        let mut line = String::new();
        line.push_str(&indent);
        if let Some(ref rid) = node.ref_id {
            line.push_str(rid);
            line.push(' ');
        }
        line.push_str(&node.role);
        if let Some(ref name) = node.name
            && !name.is_empty()
        {
            line.push(' ');
            line.push_str(&format_quoted(name));
        }
        if let Some(ref val) = node.value
            && !val.is_empty()
        {
            line.push_str(" value=");
            line.push_str(&format_quoted(val));
        }
        let mut attrs = Vec::new();
        if node.disabled {
            attrs.push("disabled".to_string());
        }
        if node.focused {
            attrs.push("focused".to_string());
        }
        if node.required {
            attrs.push("required".to_string());
        }
        if let Some(c) = node.checked {
            attrs.push(format!("checked={c}"));
        }
        if let Some(e) = node.expanded {
            attrs.push(format!("expanded={e}"));
        }
        if let Some(s) = node.selected {
            attrs.push(format!("selected={s}"));
        }
        if let Some(l) = node.level {
            attrs.push(format!("level={l}"));
        }
        if !attrs.is_empty() {
            line.push_str(" [");
            line.push_str(&attrs.join(", "));
            line.push(']');
        }
        lines.push(line);
        format_nodes_recursive(&node.children, lines);
    }
}

fn format_quoted(s: &str) -> String {
    let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_ax(id: &str, parent: Option<&str>, role: &str, name: Option<&str>) -> AxNode {
        AxNode {
            node_id: id.to_string(),
            parent_id: parent.map(ToString::to_string),
            child_ids: Vec::new(),
            role: role.to_string(),
            name: name.map(ToString::to_string),
            value: None,
            description: None,
            backend_node_id: id.parse::<i64>().ok(),
            frame_id: None,
            ignored: false,
            focusable: is_interactive_role(role),
            focused: false,
            disabled: false,
            checked: None,
            expanded: None,
            selected: None,
            required: false,
            level: None,
            bounds: Some(BoundingBox {
                x: 0.0,
                y: 0.0,
                width: 100.0,
                height: 30.0,
            }),
        }
    }

    #[test]
    fn reparents_ignored_and_display_contents_and_drops_duplicate_static_text() {
        let mut root = mk_ax("1", None, "RootWebArea", Some("Page"));
        root.child_ids = vec!["2".into()];
        let mut ignored_wrap = mk_ax("2", Some("1"), "generic", None);
        ignored_wrap.ignored = true;
        ignored_wrap.bounds = Some(BoundingBox {
            x: 0.0,
            y: 0.0,
            width: 0.0,
            height: 0.0,
        });
        ignored_wrap.child_ids = vec!["3".into(), "4".into()];
        let mut btn = mk_ax("3", Some("2"), "button", Some("Submit"));
        btn.child_ids = vec!["5".into()];
        let dup_txt = mk_ax("5", Some("3"), "StaticText", Some("Submit"));
        let mut hidden_div = mk_ax("4", Some("2"), "button", Some("Ghost"));
        hidden_div.bounds = Some(BoundingBox {
            x: 0.0,
            y: 0.0,
            width: 0.0,
            height: 0.0,
        });

        let (tree, refs) = build_semantic_tree(
            &[root, ignored_wrap, btn, dup_txt, hidden_div],
            &SnapshotOptions::default(),
        );
        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0].role, "button");
        assert_eq!(tree[0].name.as_deref(), Some("Submit"));
        assert!(tree[0].children.is_empty());
        assert_eq!(refs.len(), 1);
        assert_eq!(refs.get("@e1").unwrap().backend_node_id, Some(3));
    }

    #[test]
    fn interactive_only_and_max_depth_filter_and_number_sequentially() {
        let mut root = mk_ax("1", None, "RootWebArea", None);
        root.child_ids = vec!["2".into(), "3".into()];
        let mut nav = mk_ax("2", Some("1"), "navigation", Some("Nav"));
        nav.child_ids = vec!["4".into()];
        let mut link = mk_ax("4", Some("2"), "link", Some("Home"));
        link.child_ids = vec!["5".into()];
        let nested_btn = mk_ax("5", Some("4"), "button", Some("Inner"));
        let h1 = mk_ax("3", Some("1"), "heading", Some("Welcome"));

        let raw = vec![root, nav, link, nested_btn, h1];

        let (depth0, _) = build_semantic_tree(
            &raw,
            &SnapshotOptions {
                max_depth: Some(0),
                ..Default::default()
            },
        );
        assert_eq!(depth0.len(), 2);
        assert!(depth0[0].children.is_empty());

        let (inter, refs) = build_semantic_tree(
            &raw,
            &SnapshotOptions {
                interactive_only: true,
                ..Default::default()
            },
        );
        assert_eq!(inter.len(), 2);
        assert_eq!(inter[0].ref_id.as_deref(), Some("@e1"));
        assert_eq!(inter[0].name.as_deref(), Some("Home"));
        assert_eq!(inter[1].ref_id.as_deref(), Some("@e2"));
        assert_eq!(inter[1].name.as_deref(), Some("Inner"));
        assert_eq!(refs.len(), 2);
    }

    #[test]
    fn skips_refs_for_non_focusable_label_and_static_text_while_preserving_controls() {
        let mut root = mk_ax("1", None, "RootWebArea", None);
        root.child_ids = vec![
            "2".into(),
            "4".into(),
            "8".into(),
            "10".into(),
            "13".into(),
        ];

        // LabelText -> StaticText "Role" followed by combobox "Role" -> MenuListPopup -> option "Admin"
        let mut lbl_role = mk_ax("2", Some("1"), "LabelText", None);
        lbl_role.child_ids = vec!["3".into()];
        let txt_role = mk_ax("3", Some("2"), "StaticText", Some("Role"));

        let mut combo = mk_ax("4", Some("1"), "combobox", Some("Role"));
        combo.child_ids = vec!["5".into()];
        let mut popup = mk_ax("5", Some("4"), "MenuListPopup", None);
        popup.child_ids = vec!["6".into()];
        let opt = mk_ax("6", Some("5"), "option", Some("Admin"));

        // LabelText -> StaticText "Upload File" followed by button "Upload File"
        let mut lbl_upload = mk_ax("8", Some("1"), "LabelText", None);
        lbl_upload.child_ids = vec!["9".into()];
        let txt_upload = mk_ax("9", Some("8"), "StaticText", Some("Upload File"));
        let btn_upload = mk_ax("10", Some("1"), "button", Some("Upload File"));

        // Legend -> StaticText "Tier"
        let mut legend = mk_ax("13", Some("1"), "Legend", None);
        legend.child_ids = vec!["14".into()];
        let txt_tier = mk_ax("14", Some("13"), "StaticText", Some("Tier"));

        let (tree, refs) = build_semantic_tree(
            &[
                root, lbl_role, txt_role, combo, popup, opt, lbl_upload, txt_upload, btn_upload,
                legend, txt_tier,
            ],
            &SnapshotOptions::default(),
        );

        assert_eq!(tree.len(), 5);
        // LabelText & StaticText stay in tree for readable text/diff, without @eN refs
        assert_eq!(tree[0].role, "LabelText");
        assert_eq!(tree[0].ref_id, None);
        assert_eq!(tree[0].children[0].role, "StaticText");
        assert_eq!(tree[0].children[0].ref_id, None);

        // combobox "Role" gets @e1, MenuListPopup gets None, option "Admin" gets @e2
        assert_eq!(tree[1].role, "combobox");
        assert_eq!(tree[1].ref_id.as_deref(), Some("@e1"));
        assert_eq!(tree[1].children[0].role, "MenuListPopup");
        assert_eq!(tree[1].children[0].ref_id, None);
        assert_eq!(tree[1].children[0].children[0].role, "option");
        assert_eq!(tree[1].children[0].children[0].ref_id.as_deref(), Some("@e2"));

        // Upload LabelText/StaticText get None; button "Upload File" gets @e3
        assert_eq!(tree[2].ref_id, None);
        assert_eq!(tree[2].children[0].ref_id, None);
        assert_eq!(tree[3].role, "button");
        assert_eq!(tree[3].ref_id.as_deref(), Some("@e3"));

        // Legend/StaticText get None
        assert_eq!(tree[4].role, "Legend");
        assert_eq!(tree[4].ref_id, None);
        assert_eq!(tree[4].children[0].ref_id, None);

        assert_eq!(refs.len(), 3);
        assert_eq!(refs.get("@e1").unwrap().role, "combobox");
        assert_eq!(refs.get("@e2").unwrap().role, "option");
        assert_eq!(refs.get("@e3").unwrap().role, "button");

        let formatted = format_semantic_tree(&tree);
        assert!(formatted.contains("StaticText \"Role\""));
        assert!(formatted.contains("@e1 combobox \"Role\""));
        assert!(formatted.contains("StaticText \"Upload File\""));
        assert!(formatted.contains("@e3 button \"Upload File\""));
    }

    #[test]
    fn preserves_focusable_single_child_generic_and_assigns_ref() {
        let mut root = mk_ax("1", None, "RootWebArea", None);
        root.child_ids = vec!["2".into(), "4".into(), "6".into(), "8".into()];

        // <span tabindex="0">Tab Span</span> -> focusable generic with single StaticText child
        let mut tab_span = mk_ax("2", Some("1"), "generic", None);
        tab_span.focusable = true;
        tab_span.child_ids = vec!["3".into()];
        let txt_span = mk_ax("3", Some("2"), "StaticText", Some("Tab Span"));

        // <div tabindex="0">Tab Div</div> -> focusable generic with single StaticText child
        let mut tab_div = mk_ax("4", Some("1"), "generic", None);
        tab_div.focusable = true;
        tab_div.child_ids = vec!["5".into()];
        let txt_div = mk_ax("5", Some("4"), "StaticText", Some("Tab Div"));

        // Plain non-focusable single-child generic wrapper around a button -> still collapses
        let mut plain_wrap = mk_ax("6", Some("1"), "generic", None);
        plain_wrap.child_ids = vec!["7".into()];
        let wrapped_btn = mk_ax("7", Some("6"), "button", Some("WrappedBtn"));

        // Disabled single-child generic with StaticText -> preserved in full snapshot
        let mut disabled_div = mk_ax("8", Some("1"), "generic", None);
        disabled_div.disabled = true;
        disabled_div.child_ids = vec!["9".into()];
        let txt_disabled = mk_ax("9", Some("8"), "StaticText", Some("DisabledDiv"));

        let raw = vec![
            root,
            tab_span,
            txt_span,
            tab_div,
            txt_div,
            plain_wrap,
            wrapped_btn,
            disabled_div,
            txt_disabled,
        ];

        let (tree, refs) = build_semantic_tree(&raw, &SnapshotOptions::default());
        assert_eq!(tree.len(), 4);

        assert_eq!(tree[0].role, "generic");
        assert!(tree[0].focusable);
        assert_eq!(tree[0].ref_id.as_deref(), Some("@e1"));
        assert_eq!(tree[0].children.len(), 1);
        assert_eq!(tree[0].children[0].role, "StaticText");
        assert_eq!(tree[0].children[0].name.as_deref(), Some("Tab Span"));
        assert_eq!(tree[0].children[0].ref_id, None);

        assert_eq!(tree[1].role, "generic");
        assert!(tree[1].focusable);
        assert_eq!(tree[1].ref_id.as_deref(), Some("@e2"));
        assert_eq!(tree[1].children.len(), 1);
        assert_eq!(tree[1].children[0].role, "StaticText");
        assert_eq!(tree[1].children[0].name.as_deref(), Some("Tab Div"));
        assert_eq!(tree[1].children[0].ref_id, None);

        assert_eq!(tree[2].role, "button");
        assert_eq!(tree[2].name.as_deref(), Some("WrappedBtn"));
        assert_eq!(tree[2].ref_id.as_deref(), Some("@e3"));

        assert_eq!(tree[3].role, "generic");
        assert!(tree[3].disabled);
        assert_eq!(tree[3].ref_id.as_deref(), Some("@e4"));
        assert_eq!(tree[3].children[0].role, "StaticText");
        assert_eq!(tree[3].children[0].ref_id, None);

        assert_eq!(refs.len(), 4);
        assert_eq!(refs.get("@e1").unwrap().backend_node_id, Some(2));
        assert_eq!(refs.get("@e2").unwrap().backend_node_id, Some(4));

        // --interactive-only keeps focusable generic nodes and button, omits StaticText & non-focusable disabled generic
        let (inter, inter_refs) = build_semantic_tree(
            &raw,
            &SnapshotOptions {
                interactive_only: true,
                ..Default::default()
            },
        );
        assert_eq!(inter.len(), 3);
        assert_eq!(inter[0].role, "generic");
        assert_eq!(inter[0].ref_id.as_deref(), Some("@e1"));
        assert_eq!(inter[1].role, "generic");
        assert_eq!(inter[1].ref_id.as_deref(), Some("@e2"));
        assert_eq!(inter[2].role, "button");
        assert_eq!(inter[2].name.as_deref(), Some("WrappedBtn"));
        assert_eq!(inter[2].ref_id.as_deref(), Some("@e3"));
        assert_eq!(inter_refs.len(), 3);
    }

    #[test]
    fn max_depth_prunes_deeply_nested_transparent_wrappers_regardless_of_ignored_flag() {
        for ignored_wrappers in [false, true] {
            let mut raw = vec![mk_ax("root", None, "RootWebArea", None)];
            raw[0].child_ids = vec!["d1".into()];

            for i in 1..=12usize {
                let id = format!("d{i}");
                let pid = if i == 1 {
                    "root".to_string()
                } else {
                    format!("d{}", i - 1)
                };
                let cid = if i == 12 {
                    "btn".to_string()
                } else {
                    format!("d{}", i + 1)
                };
                let mut div = mk_ax(&id, Some(&pid), "generic", None);
                div.ignored = ignored_wrappers;
                div.child_ids = vec![cid];
                raw.push(div);
            }
            raw.push(mk_ax("btn", Some("d12"), "button", Some("DeepTarget")));

            // Full snapshot (max_depth: None) collapses transparent wrappers and keeps DeepTarget
            let (full_tree, full_refs) = build_semantic_tree(&raw, &SnapshotOptions::default());
            assert_eq!(full_tree.len(), 1);
            assert_eq!(full_tree[0].role, "button");
            assert_eq!(full_tree[0].name.as_deref(), Some("DeepTarget"));
            assert_eq!(full_refs.len(), 1);

            // max_depth: Some(1) prunes wrappers beyond raw depth 1 before collapsing
            let (shallow_tree, shallow_refs) = build_semantic_tree(
                &raw,
                &SnapshotOptions {
                    max_depth: Some(1),
                    ..Default::default()
                },
            );
            assert!(
                shallow_tree.is_empty(),
                "ignored={ignored_wrappers}: expected DeepTarget pruned at max_depth=1, got {shallow_tree:?}"
            );
            assert!(shallow_refs.is_empty());

            // interactive_only: true + max_depth: Some(1) must also respect max_depth
            let (shallow_inter, shallow_inter_refs) = build_semantic_tree(
                &raw,
                &SnapshotOptions {
                    max_depth: Some(1),
                    interactive_only: true,
                    ..Default::default()
                },
            );
            assert!(shallow_inter.is_empty());
            assert!(shallow_inter_refs.is_empty());
        }
    }
}
