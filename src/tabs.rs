//! Tabs own a pane tree plus the panes themselves, so a whole tab can be
//! handed from one window to another (drag between windows / tear-out).

use std::collections::HashMap;

use crate::browser_pane::BrowserPane;
use crate::pane_tree::{self, Dir, Layout, Node, PaneId, RectF};
use crate::renderer::FontSet;
use crate::term_pane::TermPane;

pub enum PaneKind {
    Term(TermPane),
    Browser(BrowserPane),
}

pub struct Pane {
    pub id: PaneId,
    pub kind: PaneKind,
}

impl Pane {
    pub fn title(&self) -> String {
        match &self.kind {
            PaneKind::Term(t) => t.title(),
            PaneKind::Browser(b) => b.title(),
        }
    }
}

pub struct Tab {
    pub root: Node,
    pub panes: HashMap<PaneId, Pane>,
    pub active: PaneId,
    pub zoomed: Option<PaneId>,
    /// Per-tab font zoom. `fonts` is None until the size diverges from the
    /// window default; it travels with the tab across windows.
    pub font_size: f32,
    pub fonts: Option<FontSet>,
}

impl Tab {
    pub fn single(pane: Pane, font_size: f32) -> Tab {
        let pid = pane.id;
        let mut panes = HashMap::new();
        panes.insert(pid, pane);
        Tab {
            root: Node::Leaf(pid),
            panes,
            active: pid,
            zoomed: None,
            font_size,
            fonts: None,
        }
    }

    pub fn title(&self) -> String {
        let t = self.panes.get(&self.active).map(|p| p.title()).unwrap_or_default();
        if t.is_empty() {
            "Terminal".into()
        } else {
            t
        }
    }

    pub fn layout(&self, area: RectF) -> Layout {
        pane_tree::layout(&self.root, area, self.zoomed)
    }

    pub fn split(&mut self, dir: Dir, new_pane: Pane) {
        let target = self.active;
        let new_id = new_pane.id;
        if pane_tree::split(&mut self.root, target, dir, new_id) {
            self.panes.insert(new_id, new_pane);
            self.active = new_id;
            self.zoomed = None;
        }
    }

    /// Remove a pane. Returns the removed pane; `None` means it was the last
    /// one and the tab itself should be closed instead.
    pub fn remove(&mut self, id: PaneId) -> Option<Pane> {
        if !pane_tree::remove(&mut self.root, id) {
            return None;
        }
        let pane = self.panes.remove(&id);
        if self.zoomed == Some(id) {
            self.zoomed = None;
        }
        if self.active == id {
            self.active = *pane_tree::collect_leaves(&self.root).first().unwrap_or(&0);
        }
        pane
    }

    pub fn pane(&self, id: PaneId) -> Option<&Pane> {
        self.panes.get(&id)
    }

    pub fn pane_mut(&mut self, id: PaneId) -> Option<&mut Pane> {
        self.panes.get_mut(&id)
    }

    pub fn active_term(&self) -> Option<&TermPane> {
        match &self.panes.get(&self.active)?.kind {
            PaneKind::Term(t) => Some(t),
            _ => None,
        }
    }

    pub fn active_browser_mut(&mut self) -> Option<&mut BrowserPane> {
        match &mut self.panes.get_mut(&self.active)?.kind {
            PaneKind::Browser(b) => Some(b),
            _ => None,
        }
    }

    pub fn find_pane_by_id(&self, id: PaneId) -> bool {
        self.panes.contains_key(&id)
    }
}
