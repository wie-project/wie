//! Scripted input driver: drives a GUI guest with a line-based input script.
//!
//! Acceptance automation needs to type text, press keys, and activate menu
//! items without a human at the keyboard (`run_micro` does not inject
//! `WIE_SELFTEST` and real guests like notepad have no self-test mode). This
//! module parses a small script and posts the same guest-visible messages the
//! winit keyboard path produces (WM_KEYDOWN/WM_CHAR/WM_COMMAND), on a timed
//! schedule, from a background thread.
//!
//! Script format (blank lines and `#` comments are skipped):
//!   sleep <ms>                  wait before the next step
//!   key <vk> [shift|ctrl]       press+release a VK (hex `0x..` or decimal)
//!   type <text>                 type text (WM_CHAR per char, with key down/up)
//!   menu <id>                   post WM_COMMAND with the item id (hex or decimal)

use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use wie_runtime::GuestHandle;

use super::input;

/// One parsed script line.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ScriptStep {
    /// Wait `ms` before the next step.
    Sleep(u64),
    /// Press and release one virtual key, holding Shift/Ctrl around it.
    Key { vk: u16, shift: bool, ctrl: bool },
    /// Type text: one WM_CHAR per character.
    Type(String),
    /// Post WM_COMMAND with the menu item id in wParam's low word.
    Menu(u32),
    /// Left-click at logical client (x, y): hit-test via `window_at`, then
    /// post WM_LBUTTONDOWN + WM_LBUTTONUP (the winit MouseInput path).
    Click { x: u32, y: u32 },
    /// Dump the current published owner frame to a BMP (BMP-script oracle for
    /// real-window rendering regression checks).
    Snapshot(String),
}

/// Message-posting sink the script interpreter drives. [`GuestHandle`] is the
/// real implementation; tests use a recording fake to assert the exact
/// message sequence the guest will observe.
pub(crate) trait InputSink {
    /// Update one virtual key's pressed state in the guest keyboard-state
    /// table (see [`GuestHandle::set_key_state`]).
    fn set_key_state(&self, vk: u16, pressed: bool);
    /// Enqueue one Win32 message to `hwnd` (see [`GuestHandle::post_message`]).
    fn post_message(&self, hwnd: u64, msg: u32, wparam: u64, lparam: u64);
}

impl InputSink for GuestHandle {
    fn set_key_state(&self, vk: u16, pressed: bool) {
        self.set_key_state(vk, pressed);
    }

    fn post_message(&self, hwnd: u64, msg: u32, wparam: u64, lparam: u64) {
        self.post_message(hwnd, msg, wparam, lparam);
    }
}

/// Parse a script document into steps.
///
/// Lines are processed one at a time; blank lines and `#` comments are
/// skipped. A bad line fails the whole parse so a typo'd acceptance script
/// errors before the guest starts.
pub(crate) fn parse_script(text: &str) -> Result<Vec<ScriptStep>> {
    let mut steps = Vec::new();
    for (index, raw) in text.lines().enumerate() {
        let line = strip_inline_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        let what = || format!("input script line {} ({raw:?})", index + 1);
        let (cmd, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
        // `rest` keeps the separator whitespace from split_once; trim only the
        // leading part so `type` text preserves interior spaces.
        let rest = rest.trim_start();
        match cmd {
            "sleep" => {
                let ms = parse_u64(rest).with_context(what)?;
                steps.push(ScriptStep::Sleep(ms));
            }
            "key" => {
                let mut parts = rest.split_whitespace();
                let vk_raw = parts.next().with_context(what)?;
                let vk = parse_vk(vk_raw).with_context(what)?;
                let mut shift = false;
                let mut ctrl = false;
                for modifier in parts {
                    match modifier {
                        "shift" => shift = true,
                        "ctrl" => ctrl = true,
                        other => {
                            bail!(
                                "{}: unknown key modifier {other:?} (expected shift|ctrl)",
                                what()
                            )
                        }
                    }
                }
                steps.push(ScriptStep::Key { vk, shift, ctrl });
            }
            "type" => {
                // `rest` is everything after the command and its separating
                // whitespace; interior spaces are preserved as typed text.
                steps.push(ScriptStep::Type(rest.to_owned()));
            }
            "menu" => {
                let id = parse_u64(rest).with_context(what)?;
                let id = u32::try_from(id)
                    .map_err(|_| anyhow!("{}: menu id {id} out of range", what()))?;
                steps.push(ScriptStep::Menu(id));
            }
            "click" => {
                let mut parts = rest.split_whitespace();
                let x = parse_u64(parts.next().with_context(what)?).with_context(what)?;
                let y = parse_u64(parts.next().with_context(what)?).with_context(what)?;
                steps.push(ScriptStep::Click {
                    x: u32::try_from(x)
                        .map_err(|_| anyhow!("{}: click x {x} out of range", what()))?,
                    y: u32::try_from(y)
                        .map_err(|_| anyhow!("{}: click y {y} out of range", what()))?,
                });
            }
            "snapshot" => {
                steps.push(ScriptStep::Snapshot(rest.to_owned()));
            }
            other => {
                bail!(
                    "{}: unknown command {other:?} (expected sleep|key|type|menu|click|snapshot)",
                    what()
                );
            }
        }
    }
    Ok(steps)
}

/// Read and parse a script file, erroring with the path on I/O failure.
pub(crate) fn read_script(path: &Path) -> Result<Vec<ScriptStep>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read input script: {}", path.display()))?;
    parse_script(&text)
}

/// Resolve the script path: the explicit CLI flag wins; otherwise the
/// `WIE_INPUT_SCRIPT` environment variable (a path to a script file).
pub(crate) fn script_path(cli: Option<&Path>) -> Option<PathBuf> {
    cli.map(Path::to_path_buf)
        .or_else(|| std::env::var_os("WIE_INPUT_SCRIPT").map(PathBuf::from))
}

/// Spawn the driver thread. The thread waits for the guest's first window
/// (up to 30 s), then executes `steps`, posting messages on its own schedule.
///
/// The guest keeps running after the script ends — acceptance scripts must
/// drive the app to quit themselves (e.g. `type q` or `menu 2`).
pub(crate) fn spawn(handle: GuestHandle, steps: Vec<ScriptStep>) -> Result<()> {
    thread::Builder::new()
        .name("wie-input-script".into())
        .spawn(move || {
            let Some(hwnd) = wait_for_window(&handle) else {
                tracing::warn!(
                    target: "wiegui",
                    "input script: guest window never appeared; script skipped"
                );
                return;
            };
            tracing::info!(target: "wiegui", steps = steps.len(), "input script: executing");
            execute_real_steps(&handle, hwnd, &steps);
            tracing::info!(target: "wiegui", "input script: finished");
        })
        .context("spawn input-script thread")?;
    Ok(())
}

/// Execute the steps against the guest, sleeping between timed steps.
pub(crate) fn execute_script<S: InputSink>(sink: &S, hwnd: u64, steps: &[ScriptStep]) {
    for step in steps {
        match step {
            ScriptStep::Sleep(ms) => thread::sleep(Duration::from_millis(*ms)),
            ScriptStep::Key { vk, shift, ctrl } => post_key(sink, hwnd, *vk, *shift, *ctrl),
            ScriptStep::Type(text) => post_type(sink, hwnd, text),
            ScriptStep::Menu(id) => {
                sink.post_message(hwnd, input::WM_COMMAND, u64::from(*id), 0);
            }
            ScriptStep::Click { .. } | ScriptStep::Snapshot(_) => {
                // GuestHandle-specific steps: the recording fake cannot
                // hit-test or read frames, so `spawn` drives them through
                // `execute_real_steps` instead; this arm keeps the shared
                // interpreter total for every step kind.
                let _ = (sink, hwnd);
            }
        }
    }
}

/// Execute steps against a live [`GuestHandle`], including the
/// `GuestHandle`-only `click` and `snapshot` steps, interleaved with the
/// shared steps.
fn execute_real_steps(handle: &GuestHandle, hwnd: u64, steps: &[ScriptStep]) {
    for step in steps {
        match step {
            ScriptStep::Sleep(ms) => thread::sleep(Duration::from_millis(*ms)),
            ScriptStep::Key { vk, shift, ctrl } => post_key(handle, hwnd, *vk, *shift, *ctrl),
            ScriptStep::Type(text) => post_type(handle, hwnd, text),
            ScriptStep::Menu(id) => {
                handle.post_message(hwnd, input::WM_COMMAND, u64::from(*id), 0);
            }
            ScriptStep::Click { x, y } => {
                let (x, y) = (
                    i32::try_from(*x).unwrap_or(0),
                    i32::try_from(*y).unwrap_or(0),
                );
                let Some((target, rx, ry)) = handle.window_at(x, y) else {
                    tracing::warn!(target: "wiegui", "click: no window at ({x},{y})");
                    continue;
                };
                let lparam = u64::from((ry << 16) | rx);
                handle.post_message_at(
                    target,
                    input::WM_LBUTTONDOWN,
                    u64::from(input::MK_LBUTTON),
                    lparam,
                    i32::try_from(rx).unwrap_or(0),
                    i32::try_from(ry).unwrap_or(0),
                );
                handle.post_message_at(
                    target,
                    input::WM_LBUTTONUP,
                    0,
                    lparam,
                    i32::try_from(rx).unwrap_or(0),
                    i32::try_from(ry).unwrap_or(0),
                );
                tracing::info!(target: "wiegui", "click ({x},{y}) -> hwnd {target}");
            }
            ScriptStep::Snapshot(path) => {
                let Some(owner) = handle.first_guest_window_handle() else {
                    continue;
                };
                let Some(frame) = handle.take_frame(owner) else {
                    tracing::warn!(target: "wiegui", "snapshot: no frame");
                    continue;
                };
                let file = std::fs::File::create(path).expect("snapshot file");
                let mut writer = std::io::BufWriter::new(file);
                crate::bmp::write_bmp(&mut writer, frame.width, frame.height, &frame.pixels)
                    .expect("snapshot bmp");
                tracing::info!(target: "wiegui", "snapshot written to {path}");
            }
        }
    }
}

/// Post a full key press/release for `vk`, with optional Shift/Ctrl held.
///
/// Windows delivers modifier key-downs before the main key (Shift first) and
/// releases them in reverse order after the main key-up; the key-state table
/// updates bracket the matching messages so GetKeyState/GetAsyncKeyState see
/// the held modifier while the main key is down. No WM_CHAR is posted — `key`
/// is a raw key press, `type` produces characters.
fn post_key<S: InputSink>(sink: &S, hwnd: u64, vk: u16, shift: bool, ctrl: bool) {
    const VK_SHIFT: u16 = 0x10;
    const VK_CONTROL: u16 = 0x11;
    if shift {
        sink.set_key_state(VK_SHIFT, true);
        sink.post_message(hwnd, input::WM_KEYDOWN, u64::from(VK_SHIFT), 0);
    }
    if ctrl {
        sink.set_key_state(VK_CONTROL, true);
        sink.post_message(hwnd, input::WM_KEYDOWN, u64::from(VK_CONTROL), 0);
    }
    sink.set_key_state(vk, true);
    sink.post_message(hwnd, input::WM_KEYDOWN, u64::from(vk), 0);
    sink.set_key_state(vk, false);
    sink.post_message(hwnd, input::WM_KEYUP, u64::from(vk), 0);
    if ctrl {
        sink.post_message(hwnd, input::WM_KEYUP, u64::from(VK_CONTROL), 0);
        sink.set_key_state(VK_CONTROL, false);
    }
    if shift {
        sink.post_message(hwnd, input::WM_KEYUP, u64::from(VK_SHIFT), 0);
        sink.set_key_state(VK_SHIFT, false);
    }
}

/// Type `text`: one WM_CHAR per character, bracketed by WM_KEYDOWN/WM_KEYUP
/// with the char's base VK — the same shape the winit keyboard path posts
/// (KEYDOWN → CHAR → KEYUP per key). Characters without a VK (e.g. UTF-8
/// beyond Latin-1) post WM_CHAR alone.
fn post_type<S: InputSink>(sink: &S, hwnd: u64, text: &str) {
    for c in text.chars() {
        let vk = vk_from_char(c);
        if vk != 0 {
            sink.set_key_state(vk, true);
            sink.post_message(hwnd, input::WM_KEYDOWN, u64::from(vk), 0);
        }
        sink.post_message(hwnd, input::WM_CHAR, u64::from(u32::from(c)), 0);
        if vk != 0 {
            sink.set_key_state(vk, false);
            sink.post_message(hwnd, input::WM_KEYUP, u64::from(vk), 0);
        }
    }
}

/// Best-effort VK for a typed character (0 when none applies — the char is
/// then posted as WM_CHAR only).
fn vk_from_char(c: char) -> u16 {
    match c {
        'a'..='z' | 'A'..='Z' => {
            // VK_A..VK_Z are contiguous starting at 0x41; letters share one
            // VK regardless of case (the WM_CHAR carries the case).
            u16::try_from(u32::from(c) & 0x1F).map_or(0, |v| v.wrapping_add(0x40))
        }
        '0'..='9' => u16::try_from(u32::from(c) & 0x0F).map_or(0, |v| v.wrapping_add(0x30)),
        ' ' => 0x20,
        '\t' => 0x09,
        '\n' | '\r' => 0x0D,
        ';' => 0xBA,
        '=' => 0xBB,
        ',' => 0xBC,
        '-' => 0xBD,
        '.' => 0xBE,
        '/' => 0xBF,
        '`' => 0xC0,
        '[' => 0xDB,
        '\\' => 0xDC,
        ']' => 0xDD,
        '\'' => 0xDE,
        _ => 0,
    }
}

/// Wait up to 30 s for the guest to create its first window, polling the
/// window tree every 10 ms. Returns `None` on timeout so the caller can skip
/// the script instead of posting into a void.
fn wait_for_window(handle: &GuestHandle) -> Option<u64> {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if let Some(hwnd) = handle.first_guest_window_handle() {
            return Some(hwnd);
        }
        thread::sleep(Duration::from_millis(10));
    }
    None
}

/// Strip an inline `#` comment — a `#` that starts a whitespace-delimited
/// token or the line itself. A `#` glued to a word (e.g. `a#b`) stays in the
/// text so `type` can type it.
fn strip_inline_comment(line: &str) -> &str {
    let Some(idx) = line.find('#') else {
        return line;
    };
    let starts_token = idx == 0 || line[..idx].ends_with(char::is_whitespace);
    if starts_token { &line[..idx] } else { line }
}

/// Parse an unsigned integer, accepting hex (`0x..`) or decimal.
fn parse_u64(s: &str) -> Result<u64> {
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u64::from_str_radix(hex, 16).map_err(|e| anyhow!("invalid hex number {s:?}: {e}"))
    } else {
        s.parse::<u64>()
            .map_err(|e| anyhow!("invalid number {s:?}: {e}"))
    }
}

/// Parse a VK code (hex `0x41` or decimal `65`), range-checked to 8 bits.
fn parse_vk(s: &str) -> Result<u16> {
    let value = parse_u64(s)?;
    u8::try_from(value)
        .map(u16::from)
        .map_err(|_| anyhow!("VK code {value} out of range (0..=255)"))
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    /// Recording sink: captures every key-state write and posted message.
    #[derive(Debug, Default)]
    struct RecordingSink {
        events: std::cell::RefCell<Vec<Recorded>>,
    }

    #[derive(Debug, PartialEq, Clone)]
    enum Recorded {
        KeyState(u16, bool),
        Post(u32, u64),
    }

    impl InputSink for RecordingSink {
        fn set_key_state(&self, vk: u16, pressed: bool) {
            self.events
                .borrow_mut()
                .push(Recorded::KeyState(vk, pressed));
        }

        fn post_message(&self, _hwnd: u64, msg: u32, wparam: u64, _lparam: u64) {
            self.events.borrow_mut().push(Recorded::Post(msg, wparam));
        }
    }

    impl RecordingSink {
        fn events(&self) -> Vec<Recorded> {
            self.events.borrow().clone()
        }
    }

    #[test]
    fn parse_full_script_all_commands_and_comments() {
        let script = "\
# drive the demo: type, apply, exit
sleep 500
key 0x41 ctrl     # ctrl+a
key 65
type hello world
menu 2

type q
";
        let steps = parse_script(script).expect("parse");
        assert_eq!(
            steps,
            vec![
                ScriptStep::Sleep(500),
                ScriptStep::Key {
                    vk: 0x41,
                    shift: false,
                    ctrl: true
                },
                ScriptStep::Key {
                    vk: 0x41,
                    shift: false,
                    ctrl: false
                },
                ScriptStep::Type("hello world".to_owned()),
                ScriptStep::Menu(2),
                ScriptStep::Type("q".to_owned()),
            ]
        );
    }

    #[test]
    fn parse_type_preserves_interior_spaces() {
        let steps = parse_script("type  hello   world \n").expect("parse");
        assert_eq!(steps, vec![ScriptStep::Type("hello   world".to_owned())]);
    }

    #[test]
    fn parse_hex_and_decimal_menu_ids() {
        let steps = parse_script("menu 0x10\nmenu 16\n").expect("parse");
        assert_eq!(steps, vec![ScriptStep::Menu(0x10), ScriptStep::Menu(16)]);
    }

    #[test]
    fn parse_click_and_snapshot_commands() {
        let steps = parse_script("click 438 218\nsnapshot /tmp/frame.bmp\n").expect("parse");
        assert_eq!(
            steps,
            vec![
                ScriptStep::Click { x: 438, y: 218 },
                ScriptStep::Snapshot("/tmp/frame.bmp".to_owned()),
            ]
        );
    }

    #[test]
    fn parse_rejects_bad_click_lines() {
        for bad in ["click\n", "click 3\n", "click a b\n", "click -1 5\n"] {
            assert!(
                parse_script(bad).is_err(),
                "line {bad:?} should fail to parse"
            );
        }
    }

    #[test]
    fn parse_rejects_bad_lines() {
        for bad in [
            "bogus 3\n",
            "sleep\n",
            "sleep abc\n",
            "key\n",
            "key 0x1000\n",
            "key 0x41 alt\n",
            "menu 0x100000000\n",
        ] {
            assert!(
                parse_script(bad).is_err(),
                "line {bad:?} should fail to parse"
            );
        }
    }

    #[test]
    fn parse_empty_script_is_ok() {
        assert_eq!(parse_script("# nothing\n\n").expect("parse"), vec![]);
    }

    #[test]
    fn execute_key_with_ctrl_posts_modifier_bracketed_sequence() {
        let sink = RecordingSink::default();
        execute_script(
            &sink,
            0x1234,
            &[ScriptStep::Key {
                vk: 0x41,
                shift: false,
                ctrl: true,
            }],
        );
        // Shift-free ctrl+A: ctrl down, A down, A up, ctrl up — key state
        // writes bracket the matching messages.
        assert_eq!(
            sink.events(),
            vec![
                Recorded::KeyState(0x11, true),
                Recorded::Post(input::WM_KEYDOWN, 0x11),
                Recorded::KeyState(0x41, true),
                Recorded::Post(input::WM_KEYDOWN, 0x41),
                Recorded::KeyState(0x41, false),
                Recorded::Post(input::WM_KEYUP, 0x41),
                Recorded::Post(input::WM_KEYUP, 0x11),
                Recorded::KeyState(0x11, false),
            ]
        );
    }

    #[test]
    fn execute_key_with_shift_presses_shift_first_releases_last() {
        let sink = RecordingSink::default();
        execute_script(
            &sink,
            0x1234,
            &[ScriptStep::Key {
                vk: 0x41,
                shift: true,
                ctrl: false,
            }],
        );
        let events = sink.events();
        assert_eq!(events.first(), Some(&Recorded::KeyState(0x10, true)));
        assert_eq!(events.last(), Some(&Recorded::KeyState(0x10, false)));
        assert!(events.contains(&Recorded::Post(input::WM_KEYDOWN, 0x41)));
        assert!(events.contains(&Recorded::Post(input::WM_KEYUP, 0x41)));
    }

    #[test]
    fn execute_type_posts_keydown_char_keyup_per_char() {
        let sink = RecordingSink::default();
        execute_script(&sink, 0x1234, &[ScriptStep::Type("hi".to_owned())]);
        assert_eq!(
            sink.events(),
            vec![
                Recorded::KeyState(0x48, true),
                Recorded::Post(input::WM_KEYDOWN, 0x48),
                Recorded::Post(input::WM_CHAR, 0x68),
                Recorded::KeyState(0x48, false),
                Recorded::Post(input::WM_KEYUP, 0x48),
                Recorded::KeyState(0x49, true),
                Recorded::Post(input::WM_KEYDOWN, 0x49),
                Recorded::Post(input::WM_CHAR, 0x69),
                Recorded::KeyState(0x49, false),
                Recorded::Post(input::WM_KEYUP, 0x49),
            ]
        );
    }

    #[test]
    fn execute_menu_posts_wm_command() {
        let sink = RecordingSink::default();
        execute_script(&sink, 0x1234, &[ScriptStep::Menu(13)]);
        assert_eq!(sink.events(), vec![Recorded::Post(input::WM_COMMAND, 13)]);
    }

    #[test]
    fn execute_plain_key_posts_keydown_keyup_only() {
        let sink = RecordingSink::default();
        execute_script(
            &sink,
            0x1234,
            &[ScriptStep::Key {
                vk: 0x0D,
                shift: false,
                ctrl: false,
            }],
        );
        // Enter: no WM_CHAR — `key` posts a raw key, `type` produces chars.
        assert_eq!(
            sink.events(),
            vec![
                Recorded::KeyState(0x0D, true),
                Recorded::Post(input::WM_KEYDOWN, 0x0D),
                Recorded::KeyState(0x0D, false),
                Recorded::Post(input::WM_KEYUP, 0x0D),
            ]
        );
    }
}
