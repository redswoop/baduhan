//! Command palette (Ctrl+Shift+P): every action, fuzzy-filterable.
//! Pure data + filtering here; the overlay UI lives in window.rs.

use crate::config::Profile;
use crate::pane_tree::Dir;

#[derive(Clone, PartialEq, Debug)]
pub enum PaletteAction {
    Theme(String),
    ThemeNext,
    NewTabProfile(usize),
    Split(Dir),
    BrowserSplit,
    ClosePane,
    Zoom,
    DetachTab,
    PaneToNewTab,
    NewWindow,
    FontBigger,
    FontSmaller,
    FontReset,
    Search,
    MoveTabLeft,
    MoveTabRight,
    PromptPrev,
    PromptNext,
    Hints,
    OpenSettings,
    OpenInitLua,
}

pub struct Item {
    pub label: String,
    pub hint: &'static str,
    pub action: PaletteAction,
}

pub fn items(profiles: &[Profile], themes: &[String]) -> Vec<Item> {
    let mut v = Vec::new();
    let it = |label: &str, hint: &'static str, action: PaletteAction| Item {
        label: label.to_string(),
        hint,
        action,
    };
    for (i, p) in profiles.iter().enumerate() {
        let hint = if i < 9 { PROFILE_HINTS[i] } else { "" };
        v.push(Item {
            label: format!("New Tab: {}", p.name),
            hint,
            action: PaletteAction::NewTabProfile(i),
        });
    }
    v.push(it("Split Right", "Ctrl+Shift+D", PaletteAction::Split(Dir::Row)));
    v.push(it("Split Down", "Ctrl+Shift+E", PaletteAction::Split(Dir::Col)));
    v.push(it("Split: Browser Pane", "Ctrl+Shift+B", PaletteAction::BrowserSplit));
    v.push(it("Close Pane", "Ctrl+Shift+W", PaletteAction::ClosePane));
    v.push(it("Zoom Pane (toggle)", "Ctrl+Shift+Enter", PaletteAction::Zoom));
    v.push(it("Detach Tab to New Window", "Ctrl+Shift+M", PaletteAction::DetachTab));
    v.push(it("Move Pane to New Tab", "", PaletteAction::PaneToNewTab));
    v.push(it("New Window", "Ctrl+Shift+N", PaletteAction::NewWindow));
    v.push(it("Font: Bigger", "Ctrl+=", PaletteAction::FontBigger));
    v.push(it("Font: Smaller", "Ctrl+-", PaletteAction::FontSmaller));
    v.push(it("Font: Reset", "Ctrl+0", PaletteAction::FontReset));
    v.push(it("Search Scrollback", "Ctrl+Shift+F", PaletteAction::Search));
    v.push(it("Move Tab Left", "Ctrl+Shift+PgUp", PaletteAction::MoveTabLeft));
    v.push(it("Move Tab Right", "Ctrl+Shift+PgDn", PaletteAction::MoveTabRight));
    v.push(it("Jump to Previous Prompt", "Ctrl+Shift+Up", PaletteAction::PromptPrev));
    v.push(it("Jump to Next Prompt", "Ctrl+Shift+Down", PaletteAction::PromptNext));
    v.push(it("Quick Select (hints)", "Ctrl+Shift+Space", PaletteAction::Hints));
    v.push(it("Settings: Open settings.json", "Ctrl+,", PaletteAction::OpenSettings));
    v.push(it("Settings: Open init.lua (Lua scripting)", "", PaletteAction::OpenInitLua));
    v.push(it("Theme: Next", "Ctrl+Shift+S", PaletteAction::ThemeNext));
    for t in themes {
        v.push(Item {
            label: format!("Theme: {t}"),
            hint: "",
            action: PaletteAction::Theme(t.clone()),
        });
    }
    v
}

const PROFILE_HINTS: [&str; 9] = [
    "Ctrl+Shift+1",
    "Ctrl+Shift+2",
    "Ctrl+Shift+3",
    "Ctrl+Shift+4",
    "Ctrl+Shift+5",
    "Ctrl+Shift+6",
    "Ctrl+Shift+7",
    "Ctrl+Shift+8",
    "Ctrl+Shift+9",
];

/// Case-insensitive subsequence match; lower score = better (compact, early).
fn score(label: &str, query: &str) -> Option<u32> {
    if query.is_empty() {
        return Some(0);
    }
    let label: Vec<char> = label.to_lowercase().chars().collect();
    let query: Vec<char> = query.to_lowercase().chars().collect();
    let mut qi = 0;
    let mut first = None;
    let mut last = 0;
    for (i, c) in label.iter().enumerate() {
        if qi < query.len() && *c == query[qi] {
            if first.is_none() {
                first = Some(i);
            }
            last = i;
            qi += 1;
        }
    }
    if qi < query.len() {
        return None;
    }
    let first = first.unwrap_or(0);
    let spread = (last - first) as u32;
    Some(spread * 4 + first as u32)
}

/// Indices into `items`, best match first.
pub fn filter(items: &[Item], query: &str) -> Vec<usize> {
    let mut scored: Vec<(u32, usize)> = items
        .iter()
        .enumerate()
        .filter_map(|(i, it)| score(&it.label, query).map(|s| (s, i)))
        .collect();
    scored.sort_by_key(|(s, i)| (*s, *i));
    scored.into_iter().map(|(_, i)| i).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk(labels: &[&str]) -> Vec<Item> {
        labels
            .iter()
            .map(|l| Item { label: l.to_string(), hint: "", action: PaletteAction::Zoom })
            .collect()
    }

    #[test]
    fn empty_query_keeps_order() {
        let items = mk(&["Alpha", "Beta"]);
        assert_eq!(filter(&items, ""), vec![0, 1]);
    }

    #[test]
    fn subsequence_and_ranking() {
        let items = mk(&["Split Right", "Split Down", "Search Scrollback"]);
        // "spd" matches "Split Down" tightly, also "Split ... " others loosely.
        let f = filter(&items, "spd");
        assert_eq!(f[0], 1);
        // Compact contiguous match wins over scattered.
        let items = mk(&["Move Tab Left", "Font: Smaller"]);
        assert_eq!(filter(&items, "left")[0], 0);
        // No match filtered out.
        assert!(filter(&mk(&["Zoom"]), "xyz").is_empty());
    }

    #[test]
    fn case_insensitive() {
        let items = mk(&["New Window"]);
        assert_eq!(filter(&items, "NEWWIN"), vec![0]);
    }
}
