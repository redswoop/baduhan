//! Keyboard-shortcut overlay (Ctrl+Shift+/): every built-in binding, grouped,
//! plus the user's init.lua keybinds. Pure data here; the overlay UI lives in
//! window.rs.

pub struct Entry {
    pub keys: &'static str,
    pub action: &'static str,
}

pub struct Section {
    pub title: &'static str,
    pub entries: &'static [Entry],
}

const fn e(keys: &'static str, action: &'static str) -> Entry {
    Entry { keys, action }
}

pub const TABS: Section = Section {
    title: "Tabs",
    entries: &[
        e("Alt+T / Ctrl+Shift+T", "new tab (same profile)"),
        e("Alt+Shift+T", "new tab, pick profile"),
        e("Ctrl+Shift+1\u{2026}9", "new tab with profile N"),
        e("Alt+1\u{2026}9", "go to tab N (9 = last)"),
        e("Ctrl+1\u{2026}9", "go to tab N (9 = last)"),
        e("Alt+Shift+] / Ctrl+Tab", "next tab"),
        e("Alt+Shift+[ / Ctrl+Shift+Tab", "previous tab"),
        e("Ctrl+Shift+PgUp/PgDn", "reorder tab"),
        e("Ctrl+Shift+M", "detach tab into new window"),
    ],
};

pub const PANES: Section = Section {
    title: "Panes",
    entries: &[
        e("Alt+D / Ctrl+Shift+D", "split right (same profile)"),
        e("Alt+Shift+D / Ctrl+Shift+E", "split down"),
        e("Ctrl+Shift+B", "split with browser pane"),
        e("Alt+W / Ctrl+Shift+W", "close pane"),
        e("Alt+[ / Alt+]", "previous / next pane"),
        e(
            "Alt+\u{2190}\u{2191}\u{2193}\u{2192} / Ctrl+Alt+\u{2190}\u{2191}\u{2193}\u{2192}",
            "focus pane in direction",
        ),
        e("Ctrl+Shift+Enter", "zoom pane (toggle)"),
    ],
};

pub const TERMINAL: Section = Section {
    title: "Terminal",
    entries: &[
        e("Ctrl+Shift+C / Ctrl+Insert", "copy (select copies too)"),
        e("Ctrl+Shift+V / Shift+Insert", "paste (right/middle-click too)"),
        e("Ctrl+Shift+F", "search scrollback"),
        e("Ctrl+Shift+P", "command palette"),
        e("Ctrl+Shift+\u{2191}/\u{2193}", "previous / next prompt"),
        e("Ctrl+Shift+Space", "quick select (hints)"),
        e("Ctrl+Click", "open URL under cursor"),
        e("2x / 3x click", "select word, link / line"),
        e("Shift+Click", "extend selection"),
        e("Shift+PgUp/PgDn", "scrollback paging"),
        e("Ctrl+= / - / 0", "font bigger / smaller / reset"),
        e("Shift+Enter", "newline in Claude Code etc."),
    ],
};

pub const WINDOW: Section = Section {
    title: "Window & app",
    entries: &[
        e("Alt+N / Ctrl+Shift+N", "new window"),
        e("Ctrl+`", "quake mode (global)"),
        e("Ctrl+,", "open settings.json"),
        e("Ctrl+Shift+S", "next theme"),
        e("Ctrl+L", "URL bar (browser pane)"),
        e("F12", "DevTools (browser pane)"),
        e("Ctrl+Shift+/", "this overlay"),
    ],
};

/// Section layout for the overlay: two columns.
pub const COLUMNS: [&[&Section]; 2] = [&[&TABS, &PANES], &[&TERMINAL, &WINDOW]];

/// True when `chord` is an Alt binding fully released to the shell via the
/// `alt_passthrough` setting, so the overlay should hide its row. Alias rows
/// ("Alt+T / Ctrl+Shift+T") stay — the Ctrl+Shift chord still works; pair
/// rows ("Alt+[ / Alt+]") hide only when both halves are released.
pub fn released(chord: &str, passthrough: &[String]) -> bool {
    let k = chord.to_ascii_lowercase();
    let Some(rest) = k.strip_prefix("alt+") else { return false };
    let single =
        |s: &str| passthrough.iter().any(|p| p.trim().eq_ignore_ascii_case(s));
    match rest.split_once(" / ") {
        Some((a, b)) => match b.strip_prefix("alt+") {
            Some(b) => single(a) && single(b),
            None => false, // non-Alt alias keeps the row alive
        },
        None => single(rest),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passthrough_hides_only_fully_released_rows() {
        let p: Vec<String> = ["d", "shift+t", "[", "]"].map(String::from).into();
        assert!(released("Alt+Shift+T", &p));
        assert!(released("Alt+[ / Alt+]", &p));
        // Alias rows survive: the Ctrl+Shift chord is still bound.
        assert!(!released("Alt+D / Ctrl+Shift+D", &p));
        assert!(!released("Ctrl+Shift+B", &p));
        assert!(!released("Alt+W / Ctrl+Shift+W", &p));
        assert!(!released("Alt+1\u{2026}9", &p));
    }
}
