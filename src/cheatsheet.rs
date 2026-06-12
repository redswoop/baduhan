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
        e("Ctrl+Shift+T", "new tab"),
        e("Ctrl+Shift+1\u{2026}9", "new tab with profile N"),
        e("Alt+1\u{2026}9", "go to tab N (9 = last)"),
        e("Ctrl+1\u{2026}9", "go to tab N (9 = last)"),
        e("Ctrl+Tab", "next tab"),
        e("Ctrl+Shift+Tab", "previous tab"),
        e("Ctrl+Shift+PgUp/PgDn", "reorder tab"),
        e("Ctrl+Shift+M", "detach tab into new window"),
    ],
};

pub const PANES: Section = Section {
    title: "Panes",
    entries: &[
        e("Ctrl+Shift+D", "split right"),
        e("Ctrl+Shift+E", "split down"),
        e("Ctrl+Shift+B", "split with browser pane"),
        e("Ctrl+Shift+W", "close pane"),
        e("Ctrl+Alt+\u{2190}\u{2191}\u{2193}\u{2192}", "focus pane in direction"),
        e("Ctrl+Shift+Enter", "zoom pane (toggle)"),
    ],
};

pub const TERMINAL: Section = Section {
    title: "Terminal",
    entries: &[
        e("Ctrl+Shift+C / V", "copy / paste"),
        e("Ctrl+Shift+F", "search scrollback"),
        e("Ctrl+Shift+P", "command palette"),
        e("Ctrl+Shift+\u{2191}/\u{2193}", "previous / next prompt"),
        e("Ctrl+Shift+Space", "quick select (hints)"),
        e("Ctrl+Click", "open URL under cursor"),
        e("Shift+PgUp/PgDn", "scrollback paging"),
        e("Ctrl+= / - / 0", "font bigger / smaller / reset"),
    ],
};

pub const WINDOW: Section = Section {
    title: "Window & app",
    entries: &[
        e("Ctrl+Shift+N", "new window"),
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
