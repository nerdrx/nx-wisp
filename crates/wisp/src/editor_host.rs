//! `nx-wisp edit` — mounts the rig editor in a normal window.
//!
//! Follows `wisp_editor::MOUNT_CONTRACT` step for step. The editor owns no
//! window, no clock and no event loop by design, so this is the whole of the
//! glue: build it, pump the compositor, forward input, draw.

use std::path::Path;
use std::time::Instant;

use wisp_shell::Keysym;
use wisp_editor::{Editor, Key, SelectMode, Tool};
use wisp_paint::{Point, Rect};
use wisp_shell::EditorWindow;

/// Map a keysym to an editor key. Returns `None` for keys the editor has no
/// opinion about, which is most of them.
fn map_key(sym: Keysym, ctrl: bool, shift: bool) -> Option<Key> {
    Some(match sym {
        Keysym::z | Keysym::Z if ctrl && shift => Key::Redo,
        Keysym::z | Keysym::Z if ctrl => Key::Undo,
        Keysym::y | Keysym::Y if ctrl => Key::Redo,
        Keysym::s | Keysym::S if ctrl => Key::Save,
        Keysym::Delete | Keysym::BackSpace => Key::Delete,
        Keysym::k | Keysym::K => Key::Keyframe,
        Keysym::space => Key::PlayPause,
        Keysym::o | Keysym::O => Key::ToggleOnion,
        Keysym::g | Keysym::G => Key::ToggleGraph,
        Keysym::Right => Key::NextFrame,
        Keysym::Left => Key::PrevFrame,
        Keysym::Escape => Key::Escape,
        Keysym::f | Keysym::F => Key::Fit,
        Keysym::_1 => Key::Tool(Tool::Select),
        Keysym::_2 => Key::Tool(Tool::Pen),
        Keysym::_3 => Key::Tool(Tool::Erase),
        Keysym::_4 => Key::Tool(Tool::Bone),
        Keysym::_5 => Key::Tool(Tool::Weight),
        Keysym::_6 => Key::Tool(Tool::Ik),
        Keysym::_7 => Key::Tool(Tool::Puppet),
        Keysym::_8 => Key::Tool(Tool::Pan),
        _ => return None,
    })
}

/// Run the editor until the window closes. Returns a process exit code.
pub fn run(path: Option<&Path>) -> i32 {
    let mut editor = match path {
        Some(p) => match Editor::open(p) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("Could not open {} — {e}", p.display());
                return 1;
            }
        },
        None => match Editor::default_skin() {
            Ok(e) => e,
            Err(e) => {
                eprintln!("Could not load the built-in skin — {e}");
                return 1;
            }
        },
    };

    let title = match path {
        Some(p) => format!("NX Wisp — {}", p.display()),
        None => "NX Wisp — the built-in skin".to_string(),
    };
    let mut win = match EditorWindow::new(&title, 1440, 900) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("The editor needs a window and could not get one: {e}");
            eprintln!("`nx-wisp doctor` will say why.");
            return 2;
        }
    };

    // Nothing may be drawn before the compositor has told us how big we are.
    let deadline = Instant::now() + std::time::Duration::from_secs(3);
    while !win.is_configured() && Instant::now() < deadline {
        win.block();
    }
    if !win.is_configured() {
        eprintln!("The compositor never configured the editor window.");
        return 3;
    }

    let mut last = Instant::now();
    let mut down_at: Option<Point> = None;

    loop {
        let tick = win.pump();
        if tick.closed {
            break;
        }

        let at = tick.pointer.map(|(x, y)| Point { x, y });
        if let Some(p) = at {
            if tick.press {
                down_at = Some(p);
                let mode = if tick.shift { SelectMode::Add } else { SelectMode::Replace };
                editor.pointer_down(p, mode);
            } else if tick.release {
                editor.pointer_up(p);
                down_at = None;
            } else {
                editor.pointer_move(p);
            }
        }
        if tick.scroll != 0.0 {
            if let Some(p) = at.or(down_at) {
                editor.wheel(p, -tick.scroll);
            }
        }
        for sym in &tick.keys {
            if let Some(k) = map_key(*sym, tick.ctrl, tick.shift) {
                editor.key(k);
            }
        }

        let now = Instant::now();
        let dt_ms = (now - last).as_secs_f32() * 1000.0;
        last = now;
        editor.tick(dt_ms.min(100.0));

        let (w, h) = win.size();
        let bounds = Rect::from_size(w as f32, h as f32);
        // Chrome clicks are resolved against the panels built LAST frame, which
        // is the frame the operator was actually looking at when they clicked.
        let click = at.filter(|_| tick.press);
        win.draw(|painter, text, scene| {
            let mut panels = editor.build_panels(bounds, text);
            if let Some(p) = click {
                if let Some(action) = panels.click(p, true) {
                    editor.perform(action);
                    // Rebuild so the click's effect is visible this frame
                    // rather than one frame late.
                    panels = editor.build_panels(bounds, text);
                }
            }
            panels.ui.paint(painter, text, scene);
            let mut sink = wisp_editor::Live::new(painter, text);
            editor.draw_canvas(&mut sink, scene);
        });
    }

    if editor.dirty() {
        eprintln!("Closed with unsaved changes — nothing was written.");
    }
    0
}
