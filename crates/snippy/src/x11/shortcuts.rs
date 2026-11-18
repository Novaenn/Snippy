use super::{X11Error, request};
use snippy_core::config::ShortcutConfig;
use x11rb::{
    connection::Connection,
    protocol::xproto::{ConnectionExt as _, GrabMode, ModMask},
    rust_connection::RustConnection,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Save,
    Picker,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BindingState {
    Bound,
    Disabled,
    Invalid,
    Unavailable,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BindingStatus {
    pub action: Action,
    pub state: BindingState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Shortcut {
    modifiers: u16,
    keysym: u32,
}
impl Shortcut {
    fn parse(input: &str) -> Option<Self> {
        let mut parts = input.split('+').map(str::trim).collect::<Vec<_>>();
        let key = parts.pop()?.to_ascii_lowercase();
        let mut modifiers = 0;
        for name in parts {
            let modifier = match name.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => u16::from(ModMask::CONTROL),
                "alt" => u16::from(ModMask::M1),
                "shift" => u16::from(ModMask::SHIFT),
                "super" | "win" => u16::from(ModMask::M4),
                _ => return None,
            };
            if modifiers & modifier != 0 {
                return None;
            }
            modifiers |= modifier;
        }
        let keysym = match key.as_str() {
            "space" => 0x20,
            "tab" => 0xff09,
            "return" | "enter" => 0xff0d,
            "escape" | "esc" => 0xff1b,
            _ if key.len() == 1 && key.as_bytes()[0].is_ascii_alphanumeric() => {
                u32::from(key.as_bytes()[0])
            }
            _ => {
                let function = key.strip_prefix('f')?.parse::<u32>().ok()?;
                if !(1..=12).contains(&function) {
                    return None;
                }
                0xffbd + function
            }
        };
        Some(Self { modifiers, keysym })
    }
}

struct Grab {
    code: u8,
    modifiers: u16,
    ignored: u16,
    action: Action,
}
pub(super) struct Shortcuts {
    config: ShortcutConfig,
    grabs: Vec<Grab>,
    statuses: Vec<BindingStatus>,
}
impl Shortcuts {
    pub fn new(config: ShortcutConfig) -> Self {
        Self {
            config,
            grabs: Vec::new(),
            statuses: Vec::new(),
        }
    }
    pub fn statuses(&self) -> &[BindingStatus] {
        &self.statuses
    }
    pub fn set_config(&mut self, config: ShortcutConfig) {
        self.config = config;
    }
    pub fn action(&self, code: u8, state: u16) -> Option<Action> {
        self.grabs
            .iter()
            .find(|g| g.code == code && (state & 0xff & !g.ignored) == g.modifiers)
            .map(|g| g.action)
    }
    /// Rebinds from scratch. Every previous grab is released even if one
    /// release fails, and a failed rebind leaves the shortcuts marked
    /// unavailable. Only connection-level failures are returned; an
    /// unsupported keyboard mapping is reported through the statuses.
    pub fn bind(&mut self, connection: &RustConnection, root: u32) -> Result<(), X11Error> {
        let mut result = self.release(connection, root);
        self.statuses.clear();
        if !self.config.enabled {
            self.statuses
                .extend([Action::Save, Action::Picker].map(|action| BindingStatus {
                    action,
                    state: BindingState::Disabled,
                }));
            return result.and(connection.flush().map_err(request));
        }
        match self.grab(connection, root) {
            Ok(()) | Err(X11Error::KeyboardMap) => (),
            Err(error) => result = result.and(Err(error)),
        }
        for action in [Action::Save, Action::Picker] {
            if !self.statuses.iter().any(|s| s.action == action) {
                self.statuses.push(BindingStatus {
                    action,
                    state: BindingState::Unavailable,
                });
            }
        }
        result.and(connection.flush().map_err(request))
    }
    fn release(&mut self, connection: &RustConnection, root: u32) -> Result<(), X11Error> {
        let mut result = Ok(());
        for grab in std::mem::take(&mut self.grabs) {
            for modifiers in variants(grab.modifiers, grab.ignored) {
                result = result.and(ungrab(connection, root, grab.code, modifiers));
            }
        }
        result
    }
    fn grab(&mut self, connection: &RustConnection, root: u32) -> Result<(), X11Error> {
        let setup = connection.setup();
        let count = u8::try_from(u16::from(setup.max_keycode) - u16::from(setup.min_keycode) + 1)
            .map_err(|_| X11Error::KeyboardMap)?;
        let mapping = connection
            .get_keyboard_mapping(setup.min_keycode, count)
            .map_err(request)?
            .reply()
            .map_err(request)?;
        let keyboard = Keyboard::new(
            setup.min_keycode,
            mapping.keysyms_per_keycode,
            &mapping.keysyms,
        )
        .ok_or(X11Error::KeyboardMap)?;
        let modifiers = connection
            .get_modifier_mapping()
            .map_err(request)?
            .reply()
            .map_err(request)?;
        let locks = keyboard.locks(&modifiers.keycodes);
        for (action, text) in [
            (Action::Save, &self.config.save),
            (Action::Picker, &self.config.picker),
        ] {
            let state = match Shortcut::parse(text)
                .and_then(|key| keyboard.keycode(key.keysym).map(|code| (key, code)))
            {
                None => BindingState::Invalid,
                Some((key, code)) => {
                    let ignored = locks & !key.modifiers;
                    if self
                        .grabs
                        .iter()
                        .any(|g| g.code == code && g.modifiers == key.modifiers)
                    {
                        BindingState::Unavailable
                    } else {
                        let mut acquired = Vec::new();
                        let mut failure = None;
                        for mask in variants(key.modifiers, ignored) {
                            match connection
                                .grab_key(
                                    false,
                                    root,
                                    ModMask::from(mask),
                                    code,
                                    GrabMode::ASYNC,
                                    GrabMode::ASYNC,
                                )
                                .map_err(request)
                                .map(|cookie| cookie.check())
                            {
                                Ok(Ok(())) => acquired.push(mask),
                                // The server refused this grab, e.g. another client owns it.
                                Ok(Err(_)) => {
                                    failure = Some(Ok(()));
                                    break;
                                }
                                Err(error) => {
                                    failure = Some(Err(error));
                                    break;
                                }
                            }
                        }
                        match failure {
                            None => {
                                self.grabs.push(Grab {
                                    code,
                                    modifiers: key.modifiers,
                                    ignored,
                                    action,
                                });
                                BindingState::Bound
                            }
                            Some(mut outcome) => {
                                for mask in acquired {
                                    outcome = outcome.and(ungrab(connection, root, code, mask));
                                }
                                self.statuses.push(BindingStatus {
                                    action,
                                    state: BindingState::Unavailable,
                                });
                                outcome?;
                                continue;
                            }
                        }
                    }
                }
            };
            self.statuses.push(BindingStatus { action, state });
        }
        Ok(())
    }
}
fn ungrab(
    connection: &RustConnection,
    root: u32,
    code: u8,
    modifiers: u16,
) -> Result<(), X11Error> {
    connection
        .ungrab_key(code, root, ModMask::from(modifiers))
        .map_err(request)?
        .check()
        .map_err(request)
}
/// A core keyboard mapping snapshot used only to resolve shortcut keycodes.
struct Keyboard<'a> {
    min_keycode: u8,
    width: usize,
    keysyms: &'a [u32],
}
impl<'a> Keyboard<'a> {
    fn new(min_keycode: u8, keysyms_per_keycode: u8, keysyms: &'a [u32]) -> Option<Self> {
        let width = usize::from(keysyms_per_keycode);
        (width > 0).then_some(Self {
            min_keycode,
            width,
            keysyms,
        })
    }
    fn row(&self, code: u8) -> &[u32] {
        let Some(index) = code.checked_sub(self.min_keycode).map(usize::from) else {
            return &[];
        };
        self.keysyms
            .chunks(self.width)
            .nth(index)
            .unwrap_or_default()
    }
    /// Prefers the first layout group's unshifted column, then its shifted
    /// column, so a key from a secondary layout group never wins over the
    /// key that produces the keysym in the primary layout.
    fn keycode(&self, keysym: u32) -> Option<u8> {
        (0..self.width).find_map(|column| {
            self.keysyms
                .chunks(self.width)
                .position(|row| row.get(column) == Some(&keysym))
                .and_then(|index| u8::try_from(usize::from(self.min_keycode) + index).ok())
        })
    }
    /// Modifier bits carrying Caps Lock, Num Lock, or Scroll Lock.
    fn locks(&self, modifier_keycodes: &[u8]) -> u16 {
        let mut locks = u16::from(ModMask::LOCK);
        let per_modifier = modifier_keycodes.len() / 8;
        if per_modifier == 0 {
            return locks;
        }
        for (index, codes) in modifier_keycodes.chunks(per_modifier).take(8).enumerate() {
            if codes
                .iter()
                .any(|&code| code != 0 && self.row(code).iter().any(|s| LOCKS.contains(s)))
            {
                locks |= 1 << index;
            }
        }
        locks
    }
}
const LOCKS: [u32; 2] = [0xff7f, 0xff14];
fn variants(base: u16, ignored: u16) -> Vec<u16> {
    let mut masks = vec![base];
    for bit in 0..8 {
        let bit = 1 << bit;
        if ignored & bit != 0 {
            masks.extend(masks.clone().into_iter().map(|mask| mask | bit));
        }
    }
    masks
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn configurable_keys_reject_ambiguous_or_unsupported_bindings() {
        assert_eq!(
            Shortcut::parse("Ctrl+Alt+G"),
            Shortcut::parse(" alt + Control + g ")
        );
        assert!(Shortcut::parse("Super+Shift+F12").is_some());
        for invalid in ["", "Ctrl+Ctrl+g", "Hyper+g", "Ctrl+", "Ctrl+F13", "Ctrl+gg"] {
            assert_eq!(Shortcut::parse(invalid), None);
        }
    }
    #[test]
    fn locks_are_ignored_but_extra_modifiers_are_not() {
        assert_eq!(variants(4, 18), vec![4, 6, 20, 22]);
        let mut shortcuts = Shortcuts::new(ShortcutConfig::default());
        shortcuts.grabs.push(Grab {
            code: 42,
            modifiers: 12,
            ignored: 18,
            action: Action::Save,
        });
        assert_eq!(shortcuts.action(42, 12 | 18), Some(Action::Save));
        assert_eq!(shortcuts.action(42, 12 | 1), None);
        assert_eq!(shortcuts.action(43, 12), None);
    }
    // Synthetic `us,de` mapping: columns 2-3 are the second layout group,
    // where the physical Y key produces z and the physical Z key produces y.
    const Y: u32 = b'y' as u32;
    const Z: u32 = b'z' as u32;
    const UPPER_Y: u32 = b'Y' as u32;
    const UPPER_Z: u32 = b'Z' as u32;
    const US_DE: [u32; 16] = [
        0x61, 0x41, 0x61, 0x41, // keycode 10: a A a A
        Y, UPPER_Y, Z, UPPER_Z, // keycode 11: physical Y key
        Z, UPPER_Z, Y, UPPER_Y, // keycode 12: physical Z key
        0, 0xff7f, 0, 0xff7f, // keycode 13: Num_Lock only when shifted
    ];
    #[test]
    fn shortcut_keys_prefer_the_primary_layout_group() {
        let keyboard = Keyboard::new(10, 4, &US_DE).unwrap();
        assert_eq!(keyboard.keycode(Z), Some(12));
        assert_eq!(keyboard.keycode(Y), Some(11));
        assert_eq!(keyboard.keycode(UPPER_Z), Some(12));
        assert_eq!(keyboard.keycode(0x61), Some(10));
        assert_eq!(keyboard.keycode(u32::from(b'q')), None);
        // A key reachable only through a secondary group is still bindable.
        let only_secondary = [0x61, 0x41, Z, UPPER_Z];
        assert_eq!(
            Keyboard::new(10, 4, &only_secondary).unwrap().keycode(Z),
            Some(10)
        );
        assert!(Keyboard::new(10, 0, &US_DE).is_none());
    }
    #[test]
    fn lock_modifiers_follow_the_keycodes_in_the_modifier_map() {
        // Keycode 9 also has Num_Lock but is not in the modifier map; the
        // mapped Num_Lock key is keycode 13 on Mod2, Scroll_Lock is on Mod5.
        let mut keysyms = vec![0xff7f, 0, 0, 0];
        keysyms.extend(US_DE);
        keysyms.extend([0xff14, 0, 0, 0]);
        let keyboard = Keyboard::new(9, 4, &keysyms).unwrap();
        let mut map = [0u8; 16];
        map[4] = 12; // Control: an ordinary key
        map[8] = 13; // Mod2: Num_Lock
        map[14] = 14; // Mod5: Scroll_Lock
        let expected = u16::from(ModMask::LOCK) | u16::from(ModMask::M2) | u16::from(ModMask::M5);
        assert_eq!(keyboard.locks(&map), expected);
        assert_eq!(keyboard.locks(&[]), u16::from(ModMask::LOCK));
        assert_eq!(keyboard.row(8), &[] as &[u32]);
        assert_eq!(keyboard.row(200), &[] as &[u32]);
    }
}
