pub mod ax_tree;
pub mod diff;
pub mod ref_map;

use serde::{Deserialize, Serialize};

use crate::driver::DriverSession;
use crate::error::Result;

pub use ax_tree::{build_semantic_tree, format_semantic_tree, is_interactive_role, SemanticNode};
pub use diff::{diff_snapshots, DiffEntry, DiffKind, SnapshotDiff};
pub use ref_map::{RefEntry, RefMap, TargetLocator};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SnapshotOptions {
    pub max_depth: Option<usize>,
    pub interactive_only: bool,
    pub diff: bool,
    pub include_bounds: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotOutput {
    pub url: String,
    pub title: String,
    pub tree: Vec<SemanticNode>,
    pub ref_map: RefMap,
    pub text: String,
    pub node_count: usize,
    pub ref_count: usize,
    pub diff: Option<SnapshotDiff>,
}

#[derive(Debug, Clone, Default)]
pub struct SnapshotState {
    pub last_snapshot: Option<SnapshotOutput>,
    pub active_ref_map: RefMap,
}

pub struct SnapshotEngine;

impl SnapshotEngine {
    pub async fn capture(
        session: &dyn DriverSession,
        state: &mut SnapshotState,
        opts: SnapshotOptions,
    ) -> Result<SnapshotOutput> {
        let raw_nodes = session.accessibility_tree().await?;
        let (mut tree, mut ref_map) = build_semantic_tree(&raw_nodes, &opts);

        if opts.include_bounds {
            enrich_missing_bounds(session, &mut tree, &mut ref_map).await;
        }

        let text = format_semantic_tree(&tree);
        let node_count = count_nodes(&tree);
        let ref_count = ref_map.len();
        let url = session.current_url().await.unwrap_or_default();
        let title = session.title().await.unwrap_or_default();

        let mut output = SnapshotOutput {
            url,
            title,
            tree,
            ref_map: ref_map.clone(),
            text,
            node_count,
            ref_count,
            diff: None,
        };

        if opts.diff {
            if let Some(ref prev) = state.last_snapshot {
                output.diff = Some(diff_snapshots(prev, &output));
            } else {
                output.diff = Some(SnapshotDiff::default());
            }
        }

        state.active_ref_map = ref_map;
        state.last_snapshot = Some(output.clone());
        Ok(output)
    }
}

async fn enrich_missing_bounds(
    session: &dyn DriverSession,
    tree: &mut [SemanticNode],
    ref_map: &mut RefMap,
) {
    let missing: Vec<(String, i64, Option<String>)> = ref_map
        .iter()
        .filter_map(|(k, e)| {
            if e.bounds.is_none()
                && let Some(bid) = e.backend_node_id
                && is_interactive_role(&e.role)
            {
                Some((k.clone(), bid, e.frame_id.clone()))
            } else {
                None
            }
        })
        .collect();

    for (ref_id, bid, fid) in missing {
        if let Ok(bbox) = session.resolve_node_box(bid, fid.as_deref()).await {
            if let Some(entry) = ref_map.get_mut(&ref_id) {
                entry.bounds = Some(bbox);
            }
            apply_bounds_to_tree(tree, &ref_id, bbox);
        }
    }
}

fn apply_bounds_to_tree(
    nodes: &mut [SemanticNode],
    ref_id: &str,
    bbox: crate::driver::BoundingBox,
) {
    for n in nodes.iter_mut() {
        if n.ref_id.as_deref() == Some(ref_id) {
            n.bounds = Some(bbox);
        }
        apply_bounds_to_tree(&mut n.children, ref_id, bbox);
    }
}

fn count_nodes(nodes: &[SemanticNode]) -> usize {
    nodes.iter().map(|n| 1 + count_nodes(&n.children)).sum()
}
