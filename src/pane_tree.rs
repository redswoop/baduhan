//! Recursive split tree for panes inside a tab, with iTerm2-style arbitrary
//! nesting. Layout math is pure; rendering/input live in window.rs.

pub type PaneId = u64;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Dir {
    Row, // children laid out left→right (a "vertical split" visually)
    Col, // children stacked top→bottom
}

#[derive(Clone, Debug)]
pub enum Node {
    Leaf(PaneId),
    Split { dir: Dir, fracs: Vec<f32>, kids: Vec<Node> },
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RectF {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl RectF {
    pub fn contains(&self, px: f32, py: f32) -> bool {
        px >= self.x && px < self.x + self.w && py >= self.y && py < self.y + self.h
    }
    pub fn center(&self) -> (f32, f32) {
        (self.x + self.w / 2.0, self.y + self.h / 2.0)
    }
}

/// A draggable divider between two siblings of a split.
#[derive(Clone, Debug)]
pub struct Divider {
    pub rect: RectF,
    /// Path of child indices from the root to the split node.
    pub path: Vec<usize>,
    /// Divider sits between kids[index] and kids[index + 1].
    pub index: usize,
    pub dir: Dir,
}

pub const GAP: f32 = 4.0;

#[derive(Default)]
pub struct Layout {
    pub panes: Vec<(PaneId, RectF)>,
    pub dividers: Vec<Divider>,
}

impl Layout {
    pub fn rect_of(&self, id: PaneId) -> Option<RectF> {
        self.panes.iter().find(|(p, _)| *p == id).map(|(_, r)| *r)
    }
}

pub fn layout(root: &Node, area: RectF, zoomed: Option<PaneId>) -> Layout {
    let mut out = Layout::default();
    if let Some(z) = zoomed
        && collect_leaves(root).contains(&z) {
            out.panes.push((z, area));
            return out;
        }
    walk(root, area, &mut Vec::new(), &mut out);
    out
}

fn walk(node: &Node, area: RectF, path: &mut Vec<usize>, out: &mut Layout) {
    match node {
        Node::Leaf(id) => out.panes.push((*id, area)),
        Node::Split { dir, fracs, kids } => {
            let n = kids.len();
            let gaps = GAP * (n.saturating_sub(1)) as f32;
            let total = match dir {
                Dir::Row => (area.w - gaps).max(0.0),
                Dir::Col => (area.h - gaps).max(0.0),
            };
            let mut pos = match dir {
                Dir::Row => area.x,
                Dir::Col => area.y,
            };
            for (i, kid) in kids.iter().enumerate() {
                let extent = total * fracs[i];
                let r = match dir {
                    Dir::Row => RectF { x: pos, y: area.y, w: extent, h: area.h },
                    Dir::Col => RectF { x: area.x, y: pos, w: area.w, h: extent },
                };
                path.push(i);
                walk(kid, r, path, out);
                path.pop();
                pos += extent;
                if i + 1 < n {
                    let d = match dir {
                        Dir::Row => RectF { x: pos, y: area.y, w: GAP, h: area.h },
                        Dir::Col => RectF { x: area.x, y: pos, w: area.w, h: GAP },
                    };
                    out.dividers.push(Divider {
                        rect: d,
                        path: path.clone(),
                        index: i,
                        dir: *dir,
                    });
                    pos += GAP;
                }
            }
        },
    }
}

pub fn collect_leaves(node: &Node) -> Vec<PaneId> {
    let mut v = Vec::new();
    fn rec(n: &Node, v: &mut Vec<PaneId>) {
        match n {
            Node::Leaf(id) => v.push(*id),
            Node::Split { kids, .. } => kids.iter().for_each(|k| rec(k, v)),
        }
    }
    rec(node, &mut v);
    v
}

/// Split the leaf `target`, placing `new_id` after it in direction `dir`.
pub fn split(root: &mut Node, target: PaneId, dir: Dir, new_id: PaneId) -> bool {
    fn rec(node: &mut Node, target: PaneId, dir: Dir, new_id: PaneId) -> bool {
        // If this is a split in the same direction containing the target leaf
        // directly, insert a sibling instead of nesting (iTerm2 behavior).
        if let Node::Split { dir: d, fracs, kids } = node {
            if *d == dir
                && let Some(i) = kids.iter().position(
                    |k| matches!(k, Node::Leaf(id) if *id == target),
                ) {
                    let half = fracs[i] / 2.0;
                    fracs[i] = half;
                    fracs.insert(i + 1, half);
                    kids.insert(i + 1, Node::Leaf(new_id));
                    return true;
                }
            if let Node::Split { kids, .. } = node {
                for kid in kids.iter_mut() {
                    if rec(kid, target, dir, new_id) {
                        return true;
                    }
                }
            }
            return false;
        }
        if matches!(node, Node::Leaf(id) if *id == target) {
            let old = std::mem::replace(node, Node::Leaf(0));
            *node = Node::Split {
                dir,
                fracs: vec![0.5, 0.5],
                kids: vec![old, Node::Leaf(new_id)],
            };
            return true;
        }
        false
    }
    rec(root, target, dir, new_id)
}

/// Remove a leaf, collapsing single-child splits. Returns false if `target`
/// was the last leaf (caller should close the tab).
pub fn remove(root: &mut Node, target: PaneId) -> bool {
    if matches!(root, Node::Leaf(id) if *id == target) {
        return false;
    }
    fn rec(node: &mut Node, target: PaneId) -> bool {
        if let Node::Split { fracs, kids, .. } = node {
            if let Some(i) =
                kids.iter().position(|k| matches!(k, Node::Leaf(id) if *id == target))
            {
                kids.remove(i);
                let removed = fracs.remove(i);
                // Redistribute the freed space proportionally.
                let rest: f32 = fracs.iter().sum();
                if rest > 0.0 {
                    for f in fracs.iter_mut() {
                        *f += removed * (*f / rest);
                    }
                }
                if kids.len() == 1 {
                    *node = kids.pop().unwrap();
                }
                return true;
            }
            for kid in kids.iter_mut() {
                if rec(kid, target) {
                    return true;
                }
            }
            // A nested split may have collapsed to a leaf; nothing to do here.
        }
        false
    }
    rec(root, target)
}

/// Drag the divider at `path`/`index` by `delta` fraction of the split's extent.
pub fn drag_divider(root: &mut Node, path: &[usize], index: usize, delta_frac: f32) {
    let mut node = root;
    for &i in path {
        match node {
            Node::Split { kids, .. } if i < kids.len() => node = &mut kids[i],
            _ => return,
        }
    }
    if let Node::Split { fracs, .. } = node
        && index + 1 < fracs.len() {
            const MIN: f32 = 0.07;
            let pair = fracs[index] + fracs[index + 1];
            let a = (fracs[index] + delta_frac).clamp(MIN, pair - MIN);
            fracs[index] = a;
            fracs[index + 1] = pair - a;
        }
}

/// Find the nearest pane in `dir` from `from`, by rect geometry.
pub fn neighbor(lay: &Layout, from: PaneId, dx: i32, dy: i32) -> Option<PaneId> {
    let fr = lay.rect_of(from)?;
    let (fcx, fcy) = fr.center();
    lay.panes
        .iter()
        .filter(|(id, _)| *id != from)
        .filter(|(_, r)| {
            let (cx, cy) = r.center();
            if dx > 0 {
                cx > fr.x + fr.w - 1.0
            } else if dx < 0 {
                cx < fr.x + 1.0
            } else if dy > 0 {
                cy > fr.y + fr.h - 1.0
            } else {
                cy < fr.y + 1.0
            }
        })
        .min_by(|(_, a), (_, b)| {
            let da = dist(fcx, fcy, a);
            let db = dist(fcx, fcy, b);
            da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|(id, _)| *id)
}

fn dist(x: f32, y: f32, r: &RectF) -> f32 {
    let (cx, cy) = r.center();
    (cx - x).powi(2) + (cy - y).powi(2)
}

#[cfg(test)]
mod tests {
    use super::*;

    const AREA: RectF = RectF { x: 0.0, y: 0.0, w: 1000.0, h: 600.0 };

    #[test]
    fn split_leaf_creates_two_panes() {
        let mut root = Node::Leaf(1);
        assert!(split(&mut root, 1, Dir::Row, 2));
        let lay = layout(&root, AREA, None);
        assert_eq!(lay.panes.len(), 2);
        assert_eq!(lay.dividers.len(), 1);
        // Side by side, equal-ish halves.
        let r1 = lay.rect_of(1).unwrap();
        let r2 = lay.rect_of(2).unwrap();
        assert!(r1.x < r2.x);
        assert!((r1.w - r2.w).abs() < 1.0);
        assert_eq!(r1.h, AREA.h);
    }

    #[test]
    fn same_direction_split_inserts_sibling_not_nested() {
        let mut root = Node::Leaf(1);
        split(&mut root, 1, Dir::Row, 2);
        split(&mut root, 2, Dir::Row, 3);
        // One flat split with 3 kids, not a nested tree.
        match &root {
            Node::Split { kids, dir, .. } => {
                assert_eq!(*dir, Dir::Row);
                assert_eq!(kids.len(), 3);
            },
            _ => panic!("expected split"),
        }
        assert_eq!(collect_leaves(&root), vec![1, 2, 3]);
    }

    #[test]
    fn cross_direction_split_nests() {
        let mut root = Node::Leaf(1);
        split(&mut root, 1, Dir::Row, 2);
        split(&mut root, 2, Dir::Col, 3);
        let lay = layout(&root, AREA, None);
        let r2 = lay.rect_of(2).unwrap();
        let r3 = lay.rect_of(3).unwrap();
        // 3 is stacked below 2 in the right half.
        assert_eq!(r2.x, r3.x);
        assert!(r3.y > r2.y);
    }

    #[test]
    fn remove_collapses_single_child_split() {
        let mut root = Node::Leaf(1);
        split(&mut root, 1, Dir::Row, 2);
        split(&mut root, 2, Dir::Col, 3);
        assert!(remove(&mut root, 3));
        // Back to a flat 2-leaf row.
        match &root {
            Node::Split { kids, .. } => assert_eq!(kids.len(), 2),
            _ => panic!("expected split"),
        }
        assert!(remove(&mut root, 2));
        assert!(matches!(root, Node::Leaf(1)));
        // Last leaf refuses removal — caller closes the tab instead.
        assert!(!remove(&mut root, 1));
    }

    #[test]
    fn removed_space_is_redistributed() {
        let mut root = Node::Leaf(1);
        split(&mut root, 1, Dir::Row, 2);
        split(&mut root, 2, Dir::Row, 3);
        remove(&mut root, 2);
        if let Node::Split { fracs, .. } = &root {
            let sum: f32 = fracs.iter().sum();
            assert!((sum - 1.0).abs() < 1e-4);
        } else {
            panic!("expected split");
        }
    }

    #[test]
    fn zoom_shows_only_zoomed_pane() {
        let mut root = Node::Leaf(1);
        split(&mut root, 1, Dir::Row, 2);
        let lay = layout(&root, AREA, Some(2));
        assert_eq!(lay.panes.len(), 1);
        assert_eq!(lay.panes[0].0, 2);
        assert_eq!(lay.panes[0].1.w, AREA.w);
    }

    #[test]
    fn divider_drag_clamps_to_minimum() {
        let mut root = Node::Leaf(1);
        split(&mut root, 1, Dir::Row, 2);
        drag_divider(&mut root, &[], 0, -10.0); // absurd drag left
        if let Node::Split { fracs, .. } = &root {
            assert!(fracs[0] >= 0.05);
            assert!((fracs[0] + fracs[1] - 1.0).abs() < 1e-4);
        } else {
            panic!("expected split");
        }
    }

    #[test]
    fn neighbor_finds_pane_in_direction() {
        let mut root = Node::Leaf(1);
        split(&mut root, 1, Dir::Row, 2);
        split(&mut root, 2, Dir::Col, 3);
        let lay = layout(&root, AREA, None);
        assert_eq!(neighbor(&lay, 1, 1, 0), Some(2));
        assert_eq!(neighbor(&lay, 2, 0, 1), Some(3));
        assert_eq!(neighbor(&lay, 3, -1, 0), Some(1));
        assert_eq!(neighbor(&lay, 1, -1, 0), None);
    }

    #[test]
    fn layout_rects_do_not_overlap() {
        let mut root = Node::Leaf(1);
        split(&mut root, 1, Dir::Row, 2);
        split(&mut root, 2, Dir::Col, 3);
        split(&mut root, 1, Dir::Col, 4);
        let lay = layout(&root, AREA, None);
        for (i, (_, a)) in lay.panes.iter().enumerate() {
            for (_, b) in lay.panes.iter().skip(i + 1) {
                let overlap_x = a.x < b.x + b.w && b.x < a.x + a.w;
                let overlap_y = a.y < b.y + b.h && b.y < a.y + a.h;
                assert!(!(overlap_x && overlap_y), "panes overlap: {a:?} {b:?}");
            }
        }
    }
}
