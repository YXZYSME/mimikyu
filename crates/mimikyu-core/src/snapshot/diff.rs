use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::snapshot::ax_tree::SemanticNode;
use crate::snapshot::SnapshotOutput;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffKind {
    Added,
    Removed,
    Changed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffEntry {
    pub kind: DiffKind,
    pub ref_id: Option<String>,
    pub role: String,
    pub name: Option<String>,
    pub details: String,
    pub line: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SnapshotDiff {
    pub has_previous: bool,
    pub added: usize,
    pub removed: usize,
    pub changed: usize,
    pub entries: Vec<DiffEntry>,
    pub text: String,
}

#[derive(Debug, Clone)]
struct FlatNode<'a> {
    node: &'a SemanticNode,
}

fn flatten_nodes<'a>(nodes: &'a [SemanticNode], out: &mut Vec<FlatNode<'a>>) {
    for n in nodes {
        out.push(FlatNode { node: n });
        flatten_nodes(&n.children, out);
    }
}

fn format_label(role: &str, name: Option<&str>) -> String {
    match name.filter(|s| !s.is_empty()) {
        Some(n) => format!("{role} \"{}\"", n.replace('"', "\\\"")),
        None => role.to_string(),
    }
}

pub fn diff_snapshots(prev: &SnapshotOutput, curr: &SnapshotOutput) -> SnapshotDiff {
    let mut prev_flat = Vec::new();
    flatten_nodes(&prev.tree, &mut prev_flat);
    let mut curr_flat = Vec::new();
    flatten_nodes(&curr.tree, &mut curr_flat);

    let mut prev_matched = vec![false; prev_flat.len()];
    let mut curr_matched = vec![false; curr_flat.len()];
    let mut pairs: Vec<(usize, usize)> = Vec::new();

    // Pass 1: primary identity match by (frame_id, backend_node_id)
    let mut prev_by_backend: HashMap<(&Option<String>, i64), Vec<usize>> = HashMap::new();
    for (pi, p) in prev_flat.iter().enumerate() {
        if let Some(bid) = p.node.backend_node_id {
            prev_by_backend
                .entry((&p.node.frame_id, bid))
                .or_default()
                .push(pi);
        }
    }
    for (ci, c) in curr_flat.iter().enumerate() {
        if let Some(bid) = c.node.backend_node_id
            && let Some(candidates) = prev_by_backend.get(&(&c.node.frame_id, bid))
        {
            for &pi in candidates {
                if !prev_matched[pi] && prev_flat[pi].node.role == c.node.role {
                    prev_matched[pi] = true;
                    curr_matched[ci] = true;
                    pairs.push((pi, ci));
                    break;
                }
            }
        }
    }

    // Pass 2: secondary semantic match by (role, name, depth) preserving order
    for (ci, c) in curr_flat.iter().enumerate() {
        if curr_matched[ci] {
            continue;
        }
        for (pi, p) in prev_flat.iter().enumerate() {
            if !prev_matched[pi]
                && p.node.role == c.node.role
                && p.node.name == c.node.name
                && p.node.depth == c.node.depth
            {
                prev_matched[pi] = true;
                curr_matched[ci] = true;
                pairs.push((pi, ci));
                break;
            }
        }
    }

    // Pass 3: relaxed fallback by (role, name) when depth shifted
    for (ci, c) in curr_flat.iter().enumerate() {
        if curr_matched[ci] {
            continue;
        }
        for (pi, p) in prev_flat.iter().enumerate() {
            if !prev_matched[pi] && p.node.role == c.node.role && p.node.name == c.node.name {
                prev_matched[pi] = true;
                curr_matched[ci] = true;
                pairs.push((pi, ci));
                break;
            }
        }
    }

    let mut entries = Vec::new();
    let mut removed = 0usize;
    let mut added = 0usize;
    let mut changed = 0usize;

    for (pi, p) in prev_flat.iter().enumerate() {
        if !prev_matched[pi] {
            removed += 1;
            let ref_part = p.node.ref_id.as_deref().unwrap_or("-");
            let lbl = format_label(&p.node.role, p.node.name.as_deref());
            entries.push(DiffEntry {
                kind: DiffKind::Removed,
                ref_id: p.node.ref_id.clone(),
                role: p.node.role.clone(),
                name: p.node.name.clone(),
                details: "removed".to_string(),
                line: format!("- {ref_part} {lbl}"),
            });
        }
    }

    for (ci, c) in curr_flat.iter().enumerate() {
        if !curr_matched[ci] {
            added += 1;
            let ref_part = c.node.ref_id.as_deref().unwrap_or("+");
            let lbl = format_label(&c.node.role, c.node.name.as_deref());
            entries.push(DiffEntry {
                kind: DiffKind::Added,
                ref_id: c.node.ref_id.clone(),
                role: c.node.role.clone(),
                name: c.node.name.clone(),
                details: "added".to_string(),
                line: format!("+ {ref_part} {lbl}"),
            });
        }
    }

    pairs.sort_by_key(|&(_, ci)| ci);
    for (pi, ci) in pairs {
        let p = prev_flat[pi].node;
        let c = curr_flat[ci].node;
        let mut deltas = Vec::new();
        if p.name != c.name {
            deltas.push(format!(
                "name: {:?} -> {:?}",
                p.name.as_deref().unwrap_or(""),
                c.name.as_deref().unwrap_or("")
            ));
        }
        if p.value != c.value {
            deltas.push(format!(
                "value: {:?} -> {:?}",
                p.value.as_deref().unwrap_or(""),
                c.value.as_deref().unwrap_or("")
            ));
        }
        if p.disabled != c.disabled {
            deltas.push(format!("disabled: {} -> {}", p.disabled, c.disabled));
        }
        if p.checked != c.checked {
            deltas.push(format!("checked: {:?} -> {:?}", p.checked, c.checked));
        }
        if p.expanded != c.expanded {
            deltas.push(format!("expanded: {:?} -> {:?}", p.expanded, c.expanded));
        }
        if p.selected != c.selected {
            deltas.push(format!("selected: {:?} -> {:?}", p.selected, c.selected));
        }
        if p.focused != c.focused {
            deltas.push(format!("focused: {} -> {}", p.focused, c.focused));
        }
        if !deltas.is_empty() {
            changed += 1;
            let ref_part = c.ref_id.as_deref().unwrap_or("~");
            let lbl = format_label(&c.role, c.name.as_deref());
            let details = deltas.join(", ");
            entries.push(DiffEntry {
                kind: DiffKind::Changed,
                ref_id: c.ref_id.clone(),
                role: c.role.clone(),
                name: c.name.clone(),
                details: details.clone(),
                line: format!("~ {ref_part} {lbl} ({details})"),
            });
        }
    }

    let text = entries
        .iter()
        .map(|e| e.line.as_str())
        .collect::<Vec<_>>()
        .join("\n");

    SnapshotDiff {
        has_previous: true,
        added,
        removed,
        changed,
        entries,
        text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::ax_tree::build_semantic_tree;
    use crate::snapshot::SnapshotOptions;
    use crate::driver::{AxNode, BoundingBox};

    fn mk_node(id: &str, role: &str, name: Option<&str>, bid: i64) -> AxNode {
        AxNode {
            node_id: id.to_string(),
            parent_id: None,
            child_ids: Vec::new(),
            role: role.to_string(),
            name: name.map(ToString::to_string),
            value: None,
            description: None,
            backend_node_id: Some(bid),
            frame_id: None,
            ignored: false,
            focusable: true,
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
                width: 80.0,
                height: 24.0,
            }),
        }
    }

    #[test]
    fn diff_ignores_ref_shift_when_banner_inserted_and_detects_state_change() {
        let btn1 = mk_node("1", "button", Some("Submit"), 101);
        let mut cb1 = mk_node("2", "checkbox", Some("Agree"), 102);
        cb1.checked = Some(false);
        let (t1, r1) = build_semantic_tree(&[btn1, cb1], &SnapshotOptions::default());
        let prev = SnapshotOutput {
            url: "http://127.0.0.1/".into(),
            title: "T".into(),
            tree: t1,
            ref_map: r1,
            text: String::new(),
            node_count: 2,
            ref_count: 2,
            diff: None,
        };

        let banner = mk_node("0", "alert", Some("Saved"), 100);
        let btn2 = mk_node("1", "button", Some("Submit"), 101);
        let mut cb2 = mk_node("2", "checkbox", Some("Agree"), 102);
        cb2.checked = Some(true);
        let (t2, r2) = build_semantic_tree(&[banner, btn2, cb2], &SnapshotOptions::default());
        let curr = SnapshotOutput {
            url: "http://127.0.0.1/".into(),
            title: "T".into(),
            tree: t2,
            ref_map: r2,
            text: String::new(),
            node_count: 3,
            ref_count: 3,
            diff: None,
        };

        let diff = diff_snapshots(&prev, &curr);
        assert_eq!(diff.added, 1);
        assert_eq!(diff.removed, 0);
        assert_eq!(diff.changed, 1);
        assert!(diff.text.contains("+ @e1 alert \"Saved\""));
        assert!(diff.text.contains("~ @e3 checkbox \"Agree\""));
    }
}
