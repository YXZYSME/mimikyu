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

    let mut visited = HashSet::new();
    let mut normalized_roots = Vec::new();
    for root in roots {
        let mut built = normalize_subtree(root, None, &by_id, &children_map, &mut visited);
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

fn normalize_subtree<'a>(
    node: &'a AxNode,
    parent_name_if_owned: Option<&str>,
    by_id: &HashMap<&'a str, &'a AxNode>,
    children_map: &HashMap<&'a str, Vec<&'a str>>,
    visited: &mut HashSet<&'a str>,
) -> Vec<SemanticNode> {
    if !visited.insert(node.node_id.as_str()) {
        return Vec::new();
    }

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
    if let Some(cids) = children_map.get(node.node_id.as_str()) {
        for cid in cids {
            if let Some(child_ax) = by_id.get(cid) {
                children.extend(normalize_subtree(
                    child_ax,
                    child_owned_name,
                    by_id,
                    children_map,
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

fn assign_refs_preorder(
    nodes: &mut [SemanticNode],
    next_ref: &mut usize,
    ref_map: &mut RefMap,
) {
    for node in nodes.iter_mut() {
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
}
