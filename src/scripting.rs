//! Lua scripting (wezterm-style): %APPDATA%\baduhan\init.lua runs at startup
//! with a `baduhan` global. It can override config, add profiles, register
//! keybindings, and hook tab-title formatting.
//!
//! ```lua
//! baduhan.config.font_size = 14
//! baduhan.profile { name = "Logs", command = { "wsl.exe", "-d", "Ubuntu" } }
//! baduhan.keybind("ctrl+shift+g", function(win)
//!   win:browse("github.com")
//! end)
//! baduhan.on("tab_title", function(t) return "• " .. t end)
//! ```
//!
//! Keybind callbacks receive a `win` whose methods *enqueue* actions
//! (new_tab, split, browse, font_size, send); the window executes them after
//! the callback returns — Lua never re-enters window state.

use std::cell::RefCell;
use std::rc::Rc;

use mlua::{Function, Lua, RegistryKey, Table, Value};

use crate::config::Config;
use crate::keys::Mods;
use crate::pane_tree::Dir;

/// What a Lua keybind asked the window to do.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    NewTab(Option<String>),
    Split(Dir),
    Browse(String),
    FontDelta(f32),
    SendText(String),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct KeySpec {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub vk: u16,
}

pub fn parse_keyspec(s: &str) -> Option<KeySpec> {
    let mut spec = KeySpec { ctrl: false, shift: false, alt: false, vk: 0 };
    let parts: Vec<&str> = s.split('+').map(str::trim).collect();
    let (mods, key) = parts.split_at(parts.len().checked_sub(1)?);
    for m in mods {
        match m.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => spec.ctrl = true,
            "shift" => spec.shift = true,
            "alt" => spec.alt = true,
            _ => return None,
        }
    }
    let k = key.first()?.to_ascii_lowercase();
    spec.vk = match k.as_str() {
        "enter" | "return" => 0x0D,
        "tab" => 0x09,
        "space" => 0x20,
        "escape" | "esc" => 0x1B,
        "up" => 0x26,
        "down" => 0x28,
        "left" => 0x25,
        "right" => 0x27,
        "home" => 0x24,
        "end" => 0x23,
        "pgup" | "pageup" => 0x21,
        "pgdn" | "pagedown" => 0x22,
        "insert" => 0x2D,
        "delete" => 0x2E,
        "backspace" => 0x08,
        "`" | "backtick" | "grave" => 0xC0, // VK_OEM_3
        "-" | "minus" => 0xBD,
        "=" | "plus" | "equals" => 0xBB,
        _ => {
            let b = k.as_bytes();
            if b.len() == 1 && b[0].is_ascii_alphanumeric() {
                b[0].to_ascii_uppercase() as u16
            } else if let Some(n) = k.strip_prefix('f').and_then(|n| n.parse::<u16>().ok()) {
                if (1..=24).contains(&n) {
                    0x70 + n - 1 // VK_F1..
                } else {
                    return None;
                }
            } else {
                return None;
            }
        },
    };
    Some(spec)
}

struct Engine {
    lua: Lua,
    binds: Vec<(KeySpec, RegistryKey)>,
    title_hook: Option<RegistryKey>,
}

thread_local! {
    static ENGINE: RefCell<Option<Engine>> = const { RefCell::new(None) };
}

/// Load init.lua (if present) and apply its config/profile changes.
pub fn init(cfg: &mut Config) {
    let path = Config::path().parent().map(|d| d.join("init.lua"));
    let Some(path) = path else { return };
    let Ok(src) = std::fs::read_to_string(&path) else { return };
    if let Err(e) = init_from_source(cfg, &src) {
        eprintln!("init.lua error ({}): {e}", path.display());
    }
}

pub fn init_from_source(cfg: &mut Config, src: &str) -> mlua::Result<()> {
    let lua = Lua::new();
    let raw_binds: Rc<RefCell<Vec<(String, RegistryKey)>>> = Rc::new(RefCell::new(Vec::new()));
    let raw_hooks: Rc<RefCell<Vec<(String, RegistryKey)>>> = Rc::new(RefCell::new(Vec::new()));
    let raw_profiles: Rc<RefCell<Vec<crate::config::Profile>>> = Rc::new(RefCell::new(Vec::new()));

    {
        let baduhan = lua.create_table()?;
        baduhan.set("config", lua.create_table()?)?;

        let binds = raw_binds.clone();
        baduhan.set(
            "keybind",
            lua.create_function(move |lua, (spec, f): (String, Function)| {
                let key = lua.create_registry_value(f)?;
                binds.borrow_mut().push((spec, key));
                Ok(())
            })?,
        )?;

        let hooks = raw_hooks.clone();
        baduhan.set(
            "on",
            lua.create_function(move |lua, (event, f): (String, Function)| {
                let key = lua.create_registry_value(f)?;
                hooks.borrow_mut().push((event, key));
                Ok(())
            })?,
        )?;

        let profiles = raw_profiles.clone();
        baduhan.set(
            "profile",
            lua.create_function(move |_, t: Table| {
                let name: String = t.get("name")?;
                let command: Vec<String> = t.get("command")?;
                let cwd: Option<String> = t.get("cwd").ok();
                profiles.borrow_mut().push(crate::config::Profile { name, command, cwd });
                Ok(())
            })?,
        )?;

        lua.globals().set("baduhan", baduhan)?;
    }

    lua.load(src).set_name("init.lua").exec()?;

    // Read back config overrides.
    let baduhan: Table = lua.globals().get("baduhan")?;
    let lcfg: Table = baduhan.get("config")?;
    if let Ok(v) = lcfg.get::<String>("font_family") {
        cfg.font_family = v;
    }
    if let Ok(v) = lcfg.get::<f32>("font_size") {
        cfg.font_size = v;
    }
    if let Ok(v) = lcfg.get::<f32>("dim_inactive_panes") {
        cfg.dim_inactive_panes = v;
    }
    if let Ok(v) = lcfg.get::<usize>("scrollback_lines") {
        cfg.scrollback_lines = v;
    }
    if let Ok(v) = lcfg.get::<u16>("browser_debug_port") {
        cfg.browser_debug_port = v;
    }
    if let Ok(v) = lcfg.get::<String>("default_profile") {
        cfg.default_profile = v;
    }
    // Lua profiles replace same-named JSON ones, else append.
    for p in raw_profiles.borrow().iter() {
        match cfg.profiles.iter_mut().find(|e| e.name == p.name) {
            Some(slot) => *slot = p.clone(),
            None => cfg.profiles.push(p.clone()),
        }
    }

    let mut binds = Vec::new();
    for (spec, key) in raw_binds.borrow_mut().drain(..) {
        match parse_keyspec(&spec) {
            Some(k) => binds.push((k, key)),
            None => eprintln!("init.lua: bad keybind spec '{spec}'"),
        }
    }
    let title_hook = raw_hooks
        .borrow_mut()
        .drain(..)
        .find(|(e, _)| e == "tab_title")
        .map(|(_, k)| k);

    ENGINE.with(|e| *e.borrow_mut() = Some(Engine { lua, binds, title_hook }));
    Ok(())
}

/// Build the `win` object whose methods enqueue actions.
fn make_win(lua: &Lua, queue: Rc<RefCell<Vec<Action>>>) -> mlua::Result<Table> {
    let win = lua.create_table()?;
    let q = queue.clone();
    win.set(
        "new_tab",
        lua.create_function(move |_, (_this, profile): (Value, Option<String>)| {
            q.borrow_mut().push(Action::NewTab(profile));
            Ok(())
        })?,
    )?;
    let q = queue.clone();
    win.set(
        "split",
        lua.create_function(move |_, (_this, dir): (Value, Option<String>)| {
            let d = match dir.as_deref() {
                Some("down") | Some("below") => Dir::Col,
                _ => Dir::Row,
            };
            q.borrow_mut().push(Action::Split(d));
            Ok(())
        })?,
    )?;
    let q = queue.clone();
    win.set(
        "browse",
        lua.create_function(move |_, (_this, url): (Value, String)| {
            q.borrow_mut().push(Action::Browse(url));
            Ok(())
        })?,
    )?;
    let q = queue.clone();
    win.set(
        "font_size",
        lua.create_function(move |_, (_this, delta): (Value, f32)| {
            q.borrow_mut().push(Action::FontDelta(delta));
            Ok(())
        })?,
    )?;
    let q = queue;
    win.set(
        "send",
        lua.create_function(move |_, (_this, text): (Value, String)| {
            q.borrow_mut().push(Action::SendText(text));
            Ok(())
        })?,
    )?;
    Ok(win)
}

/// Dispatch a key chord to a Lua binding; returns the queued actions, or
/// None when no binding matches.
pub fn handle_key(vk: u16, mods: &Mods) -> Option<Vec<Action>> {
    ENGINE.with(|e| {
        let e = e.borrow();
        let eng = e.as_ref()?;
        let hit = eng.binds.iter().find(|(k, _)| {
            k.vk == vk && k.ctrl == mods.ctrl && k.shift == mods.shift && k.alt == mods.alt
        })?;
        let f: Function = eng.lua.registry_value(&hit.1).ok()?;
        let queue: Rc<RefCell<Vec<Action>>> = Rc::new(RefCell::new(Vec::new()));
        match make_win(&eng.lua, queue.clone()) {
            Ok(win) => {
                if let Err(err) = f.call::<()>(win) {
                    eprintln!("init.lua keybind error: {err}");
                }
            },
            Err(err) => eprintln!("init.lua: {err}"),
        }
        let actions = queue.borrow().clone();
        Some(actions)
    })
}

/// Run the tab_title hook, if any.
pub fn format_title(title: &str) -> Option<String> {
    ENGINE.with(|e| {
        let e = e.borrow();
        let eng = e.as_ref()?;
        let key = eng.title_hook.as_ref()?;
        let f: Function = eng.lua.registry_value(key).ok()?;
        match f.call::<Option<String>>(title.to_string()) {
            Ok(s) => s,
            Err(err) => {
                eprintln!("init.lua tab_title error: {err}");
                None
            },
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const CTRL_SHIFT: Mods = Mods { shift: true, ctrl: true, alt: false };

    #[test]
    fn keyspec_parsing() {
        assert_eq!(
            parse_keyspec("ctrl+shift+g"),
            Some(KeySpec { ctrl: true, shift: true, alt: false, vk: b'G' as u16 })
        );
        assert_eq!(
            parse_keyspec("alt+f5"),
            Some(KeySpec { ctrl: false, shift: false, alt: true, vk: 0x74 })
        );
        assert_eq!(
            parse_keyspec("ctrl+enter"),
            Some(KeySpec { ctrl: true, shift: false, alt: false, vk: 0x0D })
        );
        assert_eq!(parse_keyspec("ctrl+banana"), None);
        assert_eq!(parse_keyspec(""), None);
    }

    #[test]
    fn lua_config_profiles_keybinds_and_title_hook() {
        let mut cfg = Config::default();
        cfg.profiles.push(crate::config::Profile {
            name: "Base".into(),
            command: vec!["cmd.exe".into()],
            cwd: None,
        });
        init_from_source(
            &mut cfg,
            r#"
            baduhan.config.font_size = 15.5
            baduhan.config.default_profile = "Logs"
            baduhan.profile { name = "Logs", command = { "wsl.exe", "-d", "Ubuntu" } }
            baduhan.profile { name = "Base", command = { "pwsh.exe" } }
            baduhan.keybind("ctrl+shift+g", function(win)
              win:browse("github.com")
              win:font_size(2)
              win:send("ls\n")
              win:new_tab("Logs")
              win:split("down")
            end)
            baduhan.on("tab_title", function(t) return "* " .. t end)
            "#,
        )
        .expect("lua init");

        assert_eq!(cfg.font_size, 15.5);
        assert_eq!(cfg.default_profile, "Logs");
        assert_eq!(cfg.profiles.len(), 2); // Base replaced, Logs appended
        assert_eq!(cfg.profiles[0].command, vec!["pwsh.exe"]);

        let actions = handle_key(b'G' as u16, &CTRL_SHIFT).expect("binding fires");
        assert_eq!(
            actions,
            vec![
                Action::Browse("github.com".into()),
                Action::FontDelta(2.0),
                Action::SendText("ls\n".into()),
                Action::NewTab(Some("Logs".into())),
                Action::Split(Dir::Col),
            ]
        );
        // Non-matching chord falls through to built-ins.
        assert!(handle_key(b'G' as u16, &Mods { shift: false, ctrl: true, alt: false }).is_none());

        assert_eq!(format_title("bash").as_deref(), Some("* bash"));
    }
}
