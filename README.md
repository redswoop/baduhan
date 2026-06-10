# baduhan

**baduhan** (պատուհան — Armenian for *window*) is a fast, native terminal
emulator for Windows with iTerm2-style tabs, arbitrary splits, and embedded
browser panes for web development. Single ~1 MB exe, no runtime.

![CI](https://github.com/redswoop/baduhan/actions/workflows/ci.yml/badge.svg)

## Why

Windows Terminal is fine. iTerm2 is better. baduhan chases the parts of iTerm2
that matter for real work — splits you can carve up arbitrarily, tabs you can
drag between windows without killing the shell, and a browser pane sitting next
to your dev server — in a small, fast, native package.

## Stack

| Layer | Implementation |
|---|---|
| Language | Rust — single native exe, no runtime/VM |
| PTY | ConPTY via `portable-pty`, one pseudo-console per pane |
| Process management | Win32 **job object** per pane (`KILL_ON_JOB_CLOSE`) — closing a pane reliably kills the entire child process tree |
| VT emulation | `alacritty_terminal` — Alacritty's production emulator: truecolor, 256-color, alt screen, scroll regions, SGR mouse, bracketed paste, OSC titles |
| Rendering | Direct2D + DirectWrite (GPU), per-monitor-v2 DPI native |
| Browser panes | WebView2 (Edge) child HWNDs; `SetParentWindow` moves them across windows **without reloading** |
| UI | Custom-drawn tab bar, pane tree, dividers — no framework |

## Hotkeys

### Tabs
| Keys | Action |
|---|---|
| `Ctrl+Shift+T` | New tab |
| `Ctrl+Tab` / `Ctrl+Shift+Tab` | Next / previous tab |
| `Ctrl+1` … `Ctrl+8` | Jump to tab N |
| `Ctrl+9` | Jump to last tab |
| `Ctrl+Shift+PgUp` / `PgDn` | Reorder tab left / right |
| `Ctrl+Shift+M` | Detach tab into a new window |
| Middle-click tab | Close tab |
| Drag tab | Reorder; drop on another window's tab bar to move it there; drop anywhere else to tear out a new window |

### Panes / splits
| Keys | Action |
|---|---|
| `Ctrl+Shift+D` | Split right (side-by-side) |
| `Ctrl+Shift+E` | Split down (stacked) |
| `Ctrl+Shift+B` | Split with a **browser pane** |
| `Ctrl+Shift+W` | Close pane (closes tab when last) |
| `Ctrl+Alt+←↑↓→` | Focus pane in direction |
| `Ctrl+Shift+Enter` | Zoom/restore pane (maximize within tab) |
| Drag divider | Resize splits |

### Terminal
| Keys | Action |
|---|---|
| `Ctrl+Shift+C` / `Ctrl+Shift+V` | Copy / paste (bracketed paste aware) |
| Mouse selection | Copy-on-select; double-click selects a word |
| Right-click | Copy selection if any, else paste |
| `Shift+PgUp` / `Shift+PgDn` | Scrollback paging |
| `Ctrl+=` / `Ctrl+-` / `Ctrl+0` | Font size bigger / smaller / reset |

### Browser panes
| Keys | Action |
|---|---|
| `Ctrl+L` | Focus the URL bar (when a browser pane is active) |
| `F12` | DevTools |
| `Enter` in URL bar | Navigate — bare words search, `localhost:…` gets `http://` |

### Windows
| Keys | Action |
|---|---|
| `Ctrl+Shift+N` | New window |

## Build

```
cargo build --release
cargo test
```

Requires Rust 1.88+ and Windows 10 1809+ (for ConPTY). The browser pane needs
the WebView2 runtime, which ships with Windows 11 and Edge.

Default shell: `pwsh.exe` from PATH, falling back to Windows PowerShell.

## Architecture notes

- All windows share one UI thread. Tabs own their panes, so moving a tab
  between windows is a data-structure move plus HWND reparenting for browser
  panes. Terminal scrollback, processes, and state survive the move untouched.
- Each PTY has a reader thread feeding the VT parser into the shared `Term`
  under a fair mutex, coalescing repaints via `PostMessage` with a dirty flag.
  A separate thread waits on the child process for exit detection (ConPTY's
  output pipe deliberately does not EOF when the child exits).
- The renderer batches grid cells into style runs and draws with
  `DrawTextLayout` + color-font support, so emoji render in color.

### Debug hooks (debug builds)

- `WM_APP+9` posted to a window dumps a pixel-faithful frame to
  `%TEMP%\term-frame.png` (PrintWindow can't capture D2D swapchains; this
  renders the same frame into a WIC bitmap instead).
- `WM_APP+10` wparam=N drives actions programmatically: 1 split-right,
  2 split-down, 3 new-tab, 4 browser-split, 5 next-tab, 6 zoom, 7 close-pane,
  8/9 focus right/left, 10 detach-tab, 11 font+.

## License

MIT
