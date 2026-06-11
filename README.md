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
| VT emulation | `alacritty_terminal` — Alacritty's production emulator: truecolor, 256-color, alt screen, scroll regions, SGR mouse, bracketed paste, OSC titles, synchronized updates (mode 2026), styled/colored underlines, OSC 8 hyperlinks — pinned by a [VT conformance test battery](src/vt_tests.rs) |
| Rendering | Direct2D + DirectWrite (GPU), per-monitor-v2 DPI native |
| Browser panes | WebView2 (Edge) child HWNDs; `SetParentWindow` moves them across windows **without reloading** |
| UI | Custom-drawn tab bar, pane tree, dividers — no framework. Tabs live **in the title bar** (Windows Terminal-style custom frame): empty tab-bar space drags the window, double-click maximizes, custom min/max/close with Win11 snap-layout support |

## Configuration

Settings live at `%APPDATA%\baduhan\settings.json`. On first run baduhan
**imports your Windows Terminal settings** — default font (Nerd Fonts work
out of the box), color scheme, and profiles, including dynamic-source
profiles like Git Bash (resolved through WT fragment files) and WSL distros
(synthesized as `wsl.exe -d <name>`). If Windows Terminal isn't installed,
profiles are auto-detected: PowerShell 7, Windows PowerShell, cmd, Git Bash,
and every registered WSL distro. Delete the file to re-import.

```jsonc
{
  "font_family": "JetBrainsMonoNL Nerd Font",
  "font_size": 13.0,                   // Ctrl+=/− and Ctrl+wheel zoom per tab
  "dim_inactive_panes": 0.22,          // 0.0 disables split dimming
  "scrollback_lines": 10000,           // per pane; alt screen never scrolls
  "browser_debug_port": 9333,          // CDP for Playwright etc.; 0 disables
  "default_profile": "Git Bash",
  "profiles": [
    { "name": "Git Bash", "command": ["C:\\Program Files\\Git\\bin\\bash.exe", "-i", "-l"] },
    { "name": "Ubuntu",   "command": ["wsl.exe", "-d", "Ubuntu", "--cd", "~"] }
  ],
  "scheme": { "foreground": "#CCCCCC", "background": "#0C0C14", "ansi": ["#0C0C0C", "…16 entries"] }
}
```

Unknown font families fall back to Cascadia Mono → Consolas (with a stderr
note) instead of letting DirectWrite silently substitute something
proportional.

## Hotkeys

### Tabs
| Keys | Action |
|---|---|
| `Ctrl+Shift+T` | New tab (default profile) |
| `Ctrl+Shift+1` … `Ctrl+Shift+9` | New tab with profile N |
| Right-click tab bar | Profile menu |
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
| Title bar `✕` | Close that pane (every split has a title bar when a tab has >1 pane) |
| Drag a pane's title bar | Rearrange: drop on another pane's **edge** to split that side, on its **center** to swap places, or on the **tab bar** to give the pane its own tab — with a live drop-zone preview |

### Terminal
| Keys | Action |
|---|---|
| `Ctrl+Shift+C` / `Ctrl+Shift+V` | Copy / paste (bracketed paste aware) |
| Mouse selection | Copy-on-select; double-click selects a word |
| Right-click | Copy selection if any, else paste |
| `Ctrl+Shift+F` | Search scrollback (regex; prefills from selection). `Enter`/`↑` older, `Shift+Enter`/`↓` newer, `Esc` closes |
| `Ctrl+Shift+P` | **Command palette** — fuzzy-search every action |
| `Ctrl+Shift+↑` / `↓` | Jump to previous / next shell prompt (needs OSC 133 marks, see below) |
| `Ctrl+Shift+Space` | **Quick select** — labels every URL/path/hash on screen; type a label to paste it |
| `Ctrl+click` | Open URL under cursor (OSC 8 hyperlinks or plain text) |
| `Shift+PgUp` / `Shift+PgDn` | Scrollback paging |
| `Ctrl+=` / `Ctrl+-` / `Ctrl+0` | Font size bigger / smaller / reset |
| `Ctrl+wheel` | Font zoom |

### Browser panes
| Keys | Action |
|---|---|
| `Ctrl+L` | Focus the URL bar (when a browser pane is active) |
| `F12` | DevTools |
| `Enter` in URL bar | Navigate — bare words search, `localhost:…` gets `http://` |
| Toolbar `✕` | Close the browser pane |

## Scripting (Lua)

`%APPDATA%\baduhan\init.lua` runs at startup (wezterm-style) with a
`baduhan` global:

```lua
-- Override anything from settings.json:
baduhan.config.font_size = 14
baduhan.config.default_profile = "Ubuntu"

-- Add or replace profiles:
baduhan.profile { name = "Logs", command = { "wsl.exe", "-d", "Ubuntu", "-e", "journalctl", "-f" } }

-- Custom keybindings (run before built-ins, so they can shadow them):
baduhan.keybind("ctrl+shift+g", function(win)
  win:browse("github.com")     -- this tab's browser pane (split if needed)
  -- also: win:new_tab("Ubuntu") · win:split("down") · win:font_size(2)
  --       win:send("ls\n")
end)

-- Hooks:
baduhan.on("tab_title", function(t) return "λ " .. t end)
```

Script errors are reported to stderr and never take the terminal down.

## Drag & drop

Drop files from Explorer onto a pane:

| Gesture | Action |
|---|---|
| Drop | Paste the path, quoted for the pane's shell (`"C:\…"` for pwsh/cmd, `'C:/…'` for git-bash, `'/mnt/c/…'` for WSL) |
| `Ctrl`+drop | **Copy** the files into the shell's *current* directory |
| `Shift`+drop | **Move** them there (undo-able, Explorer semantics) |
| Drop on a browser pane | Open the file |

The target directory follows your `cd`s: pwsh/cmd report it via the process
itself; bash/zsh/WSL shells report it via the OSC 7 escape (one line of
shell config — see below). Without OSC 7, copy/move gracefully degrade to
path-pasting rather than guessing. WSL directories resolve through
`\\wsl$\<distro>\…`, so Ctrl+drop can copy straight into Linux.

```bash
# .bashrc / .zshrc — shell integration: cwd tracking (OSC 7, for drag&drop
# copy/move) and prompt marks (OSC 133, for Ctrl+Shift+↑/↓ prompt jumping):
PROMPT_COMMAND='printf "\e]7;file://%s%s\e\\\e]133;A\e\\" "$HOSTNAME" "$PWD"'  # bash
precmd() { printf '\e]7;file://%s%s\e\\\e]133;A\e\\' "$HOST" "$PWD" }          # zsh
```

## The dev browser

The browser pane is built to sit next to your dev server — and to be driven
from the shell and from test tooling.

**From any shell pane** (the env vars flow into WSL too, via WSLENV):

```bash
"$BADUHAN_EXE" browse localhost:5173   # load a URL in this tab's browser
                                       # (creates a browser split if needed)
"$BADUHAN_EXE" reload                  # reload it — bind it to a file watcher
"$BADUHAN_EXE" devtools                # pop DevTools
"$BADUHAN_EXE" cdp                     # print the CDP endpoint URL
alias bb='"$BADUHAN_EXE" browse'       # taste
```

**From Playwright / puppeteer / claude** — the embedded browser exposes the
Chrome DevTools Protocol on `browser_debug_port` (default 9333, `0`
disables; also exported as `$BADUHAN_CDP`):

```js
const { chromium } = require('playwright-core');
const browser = await chromium.connectOverCDP(process.env.BADUHAN_CDP);
const page = browser.contexts()[0].pages()[0];
await page.goto('http://localhost:5173');
await page.screenshot({ path: 'shot.png' });
```

No browser download needed — Playwright attaches to the pane you're looking
at, so you watch your tests drive the page live inside the terminal.

### Windows
| Keys | Action |
|---|---|
| `Ctrl+Shift+N` | New window |
| `` Ctrl+` `` | **Quake mode** — global hotkey toggles a full-width dropdown terminal (config: `quake_hotkey`) |

## Inline images

`baduhan view <img>` (any WIC format: PNG/JPEG/GIF/BMP/WebP) renders the
picture inline, scrolling with the scrollback. iTerm2's own `imgcat`
script works unmodified — both the single-sequence `File=` form and the
modern multipart `MultipartFile=`/`FilePart=`/`FileEnd` protocol are
implemented. `baduhan view` also reads stdin: `curl -s …/cat.png | baduhan view`.

## Session restore

Closing baduhan saves windows, tabs, split trees, per-tab zoom, shell
cwds (via OSC 7 / process inspection), and browser URLs to
`session.json`; launching restores it all, respawning each pane's profile
in its old directory. `"restore_session": false` disables.

## Build

```
cargo build --release
cargo test
```

Requires Rust 1.88+ and Windows 10 1809+ (for ConPTY). The browser pane needs
the WebView2 runtime, which ships with Windows 11 and Edge.

## Architecture notes

- All windows share one UI thread. Tabs own their panes, so moving a tab
  between windows is a data-structure move plus HWND reparenting for browser
  panes. Terminal scrollback, processes, and state survive the move untouched.
- Each PTY has a reader thread feeding the VT parser into the shared `Term`
  under a fair mutex, coalescing repaints via `PostMessage` with a dirty flag.
  A separate thread waits on the child process for exit detection (ConPTY's
  output pipe deliberately does not EOF when the child exits).
- The renderer batches grid cells into style runs and draws with
  `DrawTextLayout` + color-font support, so emoji render in color. Underline
  styles get real geometry: sine-wave undercurls, dotted/dashed runs, and
  SGR 58 underline colors.
- Focus reporting (mode 1004) tracks the focused *pane*, not just the window:
  switching splits sends `CSI I`/`CSI O` to the panes gaining/losing focus.

### Debug hooks (debug builds)

- `WM_APP+9` posted to a window dumps a pixel-faithful frame to
  `%TEMP%\term-frame.png` (PrintWindow can't capture D2D swapchains; this
  renders the same frame into a WIC bitmap instead).
- `WM_APP+10` wparam=N drives actions programmatically: 1 split-right,
  2 split-down, 3 new-tab, 4 browser-split, 5 next-tab, 6 zoom, 7 close-pane,
  8/9 focus right/left, 10 detach-tab, 11 font+, 12 new-tab-with-profile
  (lparam = profile index), 13 active-pane-to-new-tab.

## License

MIT
