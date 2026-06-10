//! Session save/restore: window geometry, tabs, split trees, profiles,
//! cwds, and browser URLs survive a restart. Saved to
//! %APPDATA%\baduhan\session.json whenever a window starts closing.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::pane_tree::{Dir, Node, PaneId};

#[derive(Serialize, Deserialize, Default)]
pub struct Session {
    pub windows: Vec<WindowState>,
}

#[derive(Serialize, Deserialize)]
pub struct WindowState {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    pub active: usize,
    pub tabs: Vec<TabState>,
}

#[derive(Serialize, Deserialize)]
pub struct TabState {
    pub font_size: f32,
    /// Index of the focused pane in leaf order.
    pub active_leaf: usize,
    pub tree: NodeState,
}

#[derive(Serialize, Deserialize)]
pub enum NodeState {
    Leaf(LeafState),
    Split { row: bool, fracs: Vec<f32>, kids: Vec<NodeState> },
}

#[derive(Serialize, Deserialize)]
pub struct LeafState {
    pub kind: PaneType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

#[derive(Serialize, Deserialize, PartialEq, Clone, Copy, Debug)]
pub enum PaneType {
    Term,
    Browser,
}

pub fn path() -> PathBuf {
    crate::config::Config::path().with_file_name("session.json")
}

pub fn save(session: &Session) {
    if session.windows.iter().all(|w| w.tabs.is_empty()) {
        return;
    }
    if let Ok(json) = serde_json::to_string_pretty(session) {
        let p = path();
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(p, json);
    }
}

pub fn load() -> Option<Session> {
    let text = std::fs::read_to_string(path()).ok()?;
    serde_json::from_str(&text).ok()
}

/// Serialize a live pane tree: leaves carry enough to respawn them.
pub fn snapshot_node(node: &Node, leaf_state: &impl Fn(PaneId) -> LeafState) -> NodeState {
    match node {
        Node::Leaf(id) => NodeState::Leaf(leaf_state(*id)),
        Node::Split { dir, fracs, kids } => NodeState::Split {
            row: *dir == Dir::Row,
            fracs: fracs.clone(),
            kids: kids.iter().map(|k| snapshot_node(k, leaf_state)).collect(),
        },
    }
}

/// Rebuild a Node tree, calling `spawn` per leaf to allocate a new pane id.
/// Leaves whose spawn fails are dropped; a tab with no live leaves returns
/// None.
pub fn rebuild_node(
    state: &NodeState,
    spawn: &mut impl FnMut(&LeafState) -> Option<PaneId>,
) -> Option<Node> {
    match state {
        NodeState::Leaf(leaf) => spawn(leaf).map(Node::Leaf),
        NodeState::Split { row, fracs, kids } => {
            let mut new_kids = Vec::new();
            let mut new_fracs = Vec::new();
            for (k, f) in kids.iter().zip(fracs) {
                if let Some(n) = rebuild_node(k, spawn) {
                    new_kids.push(n);
                    new_fracs.push(*f);
                }
            }
            match new_kids.len() {
                0 => None,
                1 => Some(new_kids.pop().unwrap()),
                _ => {
                    // Renormalize after any dropped leaves.
                    let sum: f32 = new_fracs.iter().sum();
                    if sum > 0.0 {
                        for f in &mut new_fracs {
                            *f /= sum;
                        }
                    }
                    Some(Node::Split {
                        dir: if *row { Dir::Row } else { Dir::Col },
                        fracs: new_fracs,
                        kids: new_kids,
                    })
                },
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pane_tree::collect_leaves;

    #[test]
    fn snapshot_and_rebuild_roundtrip() {
        // Row [1, Col [2, 3]]
        let tree = Node::Split {
            dir: Dir::Row,
            fracs: vec![0.6, 0.4],
            kids: vec![
                Node::Leaf(1),
                Node::Split {
                    dir: Dir::Col,
                    fracs: vec![0.5, 0.5],
                    kids: vec![Node::Leaf(2), Node::Leaf(3)],
                },
            ],
        };
        let snap = snapshot_node(&tree, &|id| LeafState {
            kind: PaneType::Term,
            profile: Some(format!("p{id}")),
            cwd: None,
            url: None,
        });
        let mut next = 100;
        let rebuilt = rebuild_node(&snap, &mut |leaf| {
            assert_eq!(leaf.kind, PaneType::Term);
            next += 1;
            Some(next)
        })
        .unwrap();
        assert_eq!(collect_leaves(&rebuilt).len(), 3);
        let json = serde_json::to_string(&snap).unwrap();
        let back: NodeState = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, NodeState::Split { row: true, .. }));
    }

    #[test]
    fn rebuild_drops_failed_leaves_and_collapses() {
        let snap = NodeState::Split {
            row: true,
            fracs: vec![0.5, 0.5],
            kids: vec![
                NodeState::Leaf(LeafState {
                    kind: PaneType::Term,
                    profile: Some("dead".into()),
                    cwd: None,
                    url: None,
                }),
                NodeState::Leaf(LeafState {
                    kind: PaneType::Term,
                    profile: Some("alive".into()),
                    cwd: None,
                    url: None,
                }),
            ],
        };
        let rebuilt = rebuild_node(&snap, &mut |leaf| {
            (leaf.profile.as_deref() == Some("alive")).then_some(7)
        })
        .unwrap();
        assert!(matches!(rebuilt, Node::Leaf(7)));
    }
}
