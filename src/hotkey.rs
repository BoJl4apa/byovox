//! Hotkey events, the chord grammar, and the trait every hotkey backend implements.
//!
//! `hotkey.key` is one key name or a chord (`ControlLeft+ShiftLeft+Z`): `parse_chord` reads
//! it, `ChordTracker` turns a stream of key downs and ups into `HotkeyEvent`s plus the verdict
//! on whether the keystroke reaches the focused window. Both are platform-neutral — a backend
//! maps its own key codes onto `ChordKey` and does what `Step` says.

use std::fmt;
use std::sync::mpsc::Sender;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HotkeyEvent {
    Pressed,
    Released,
    Toggle,
    Cancel,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HotkeyMode {
    Hold,
    Toggle,
}

impl HotkeyMode {
    pub fn parse(s: &str) -> Option<HotkeyMode> {
        match s {
            "hold" => Some(HotkeyMode::Hold),
            "toggle" => Some(HotkeyMode::Toggle),
            _ => None,
        }
    }
}

/// W3C UI Events `code` names byovox accepts. Backends map these to their own codes.
pub const KEY_NAMES: &[&str] = &[
    "ControlLeft",
    "ControlRight",
    "AltLeft",
    "AltRight",
    "ShiftLeft",
    "ShiftRight",
    "MetaLeft",
    "MetaRight",
    "CapsLock",
    "ScrollLock",
    "Pause",
    "Insert",
    "Escape",
    "F13",
    "F14",
    "F15",
    "F16",
    "F17",
    "F18",
    "F19",
    "F20",
    "F21",
    "F22",
    "F23",
    "F24",
];

pub fn validate_key_name(name: &str) -> Result<(), String> {
    if KEY_NAMES.contains(&name) {
        Ok(())
    } else {
        Err(format!(
            "unknown key `{name}`; accepted: {}",
            KEY_NAMES.join(", ")
        ))
    }
}

/// The names a chord may end on but nothing may use alone: `A`-`Z` and `0`-`9`. Documented
/// in `docs/config.example.toml` as those two ranges rather than as thirty-six names, which
/// is why the drift test below proves the ranges are exactly what is accepted.
fn is_trigger_only(name: &str) -> bool {
    matches!(name.as_bytes(), [b] if b.is_ascii_uppercase() || b.is_ascii_digit())
}

/// The rejection for a name no chord element may be, with the advice `validate_key_name`
/// cannot give: letters and digits are legal, but only in the trigger position.
fn unknown_chord_key(name: &str) -> String {
    format!(
        "unknown key `{name}`; accepted: {}, or A-Z / 0-9 as a chord's trigger",
        KEY_NAMES.join(", ")
    )
}

/// A hotkey as configured: modifiers that must all be held, and the key pressed on top of
/// them. A single name is a chord with no modifiers — the bare hotkey byovox has always had.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chord {
    pub modifiers: Vec<String>,
    pub trigger: String,
}

impl fmt::Display for Chord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for m in &self.modifiers {
            write!(f, "{m}+")?;
        }
        f.write_str(&self.trigger)
    }
}

/// `hotkey.key`: one key name, or `M1+M2+...+T` — every element but the last is a modifier
/// that must be held, the last is the trigger. Whitespace around an element is ignored, so
/// `ControlLeft + Z` reads the same as `ControlLeft+Z`.
///
/// A letter or digit is a trigger only. As a bare hotkey it would swallow every one of them
/// typed on the desktop, and refusing it here is cheaper than the user discovering that.
pub fn parse_chord(s: &str) -> Result<Chord, String> {
    let mut names: Vec<&str> = Vec::new();
    for raw in s.split('+') {
        let name = raw.trim();
        if name.is_empty() {
            return Err(format!(
                "empty element in `{s}`; expected a key name, or Modifier+...+Key"
            ));
        }
        if names.contains(&name) {
            return Err(format!("`{name}` appears twice in `{s}`"));
        }
        names.push(name);
    }
    // `split` yields at least one element however empty the input, so this cannot fail.
    let trigger = names.pop().expect("split yields at least one element");
    if !KEY_NAMES.contains(&trigger) {
        if !is_trigger_only(trigger) {
            return Err(unknown_chord_key(trigger));
        }
        if names.is_empty() {
            return Err(format!(
                "`{trigger}` needs a modifier: a bare letter or digit as the hotkey would \
                 swallow every one you type (e.g. `ControlLeft+ShiftLeft+{trigger}`)"
            ));
        }
    }
    for m in &names {
        if !KEY_NAMES.contains(m) {
            return Err(unknown_chord_key(m));
        }
    }
    Ok(Chord {
        modifiers: names.into_iter().map(String::from).collect(),
        trigger: trigger.to_string(),
    })
}

/// Which member of the chord a key event belongs to. A backend maps its own key codes to
/// this, so the tracker never sees a platform type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChordKey {
    /// Index into `Chord::modifiers`.
    Modifier(usize),
    Trigger,
}

/// What a backend does with one key event: the event to publish, if any, and whether to
/// swallow the keystroke instead of letting it reach the focused window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Step {
    pub event: Option<HotkeyEvent>,
    pub swallow: bool,
}

/// The chord's state machine: key downs and ups in, `HotkeyEvent`s and swallow verdicts out.
/// Pure — no platform types and no clock — so every rule it enforces is a unit test rather
/// than something only a live keyboard could show.
///
/// Hold/toggle is not its business: it reports that the chord went down and came up, and
/// `pipeline` decides what a press means.
pub struct ChordTracker {
    /// One flag per modifier, in the chord's order.
    mods_down: Vec<bool>,
    /// The trigger went down as part of a satisfied chord and has not come up yet, so every
    /// repeat and the eventual up belong to us — even after a modifier was released, or a
    /// held `ControlLeft+ShiftLeft+Z` would leak `zzzz` into the window the moment Shift
    /// came up.
    holding: bool,
    /// A `Pressed` is outstanding: exactly one `Released` follows it.
    latched: bool,
}

impl ChordTracker {
    pub fn new(chord: &Chord) -> ChordTracker {
        ChordTracker {
            mods_down: vec![false; chord.modifiers.len()],
            holding: false,
            latched: false,
        }
    }

    /// A hotkey with no modifiers is passed through: `ControlRight` has always reached the
    /// window as well as byovox, and swallowing a lone `Insert` or `F13` would be a change
    /// nobody asked for. Swallowing the trigger is what a chord is for.
    fn swallows(&self) -> bool {
        !self.mods_down.is_empty()
    }

    /// True when the next trigger down would fire the chord on the strength of `mods_down`
    /// alone. A backend calls it to decide whether this one keystroke is worth checking its
    /// flags against the OS before swallowing it: the flags are built from events, and an
    /// event that never arrives leaves one stuck. False for a hotkey with no modifiers —
    /// there is nothing there to go stale, and nothing gets swallowed either.
    pub fn would_fire(&self) -> bool {
        !self.holding && !self.mods_down.is_empty() && self.mods_down.iter().all(|d| *d)
    }

    /// `armed` is the daemon's Enable/Disable, and it gates exactly one transition: the
    /// `Pressed` that latches a chord. A disabled daemon must not swallow a keystroke it is
    /// not going to act on — but a chord already latched when Disable landed still finishes,
    /// repeats and up included, or the window would get a `Z` up whose down it never saw.
    ///
    /// Gating anything else would be worse than not gating at all: skipping the tracker while
    /// disabled would leave a modifier released in that window stuck down forever.
    pub fn feed(&mut self, key: ChordKey, down: bool, armed: bool) -> Step {
        let pass = Step {
            event: None,
            swallow: false,
        };
        match key {
            ChordKey::Modifier(i) => {
                let Some(slot) = self.mods_down.get_mut(i) else {
                    // The backend maps key codes from this same chord, so this is
                    // unreachable; a hook procedure must not panic (the unwind would abort
                    // the process), so a release build passes the key through instead.
                    debug_assert!(false, "modifier {i} is not a member of this chord");
                    return pass;
                };
                *slot = down;
                if !down && self.latched {
                    // The combination stopped being held, so the hold is over whichever key
                    // left first. `holding` stays set: the trigger's own up is still ours.
                    self.latched = false;
                    return Step {
                        event: Some(HotkeyEvent::Released),
                        swallow: false,
                    };
                }
                // Modifiers are never swallowed — the window would be left holding a Ctrl
                // or Shift that never comes up.
                pass
            }
            ChordKey::Trigger if down => {
                if self.holding {
                    // Auto-repeat: one press is one dictation.
                    return Step {
                        event: None,
                        swallow: self.swallows(),
                    };
                }
                if !armed || !self.mods_down.iter().all(|d| *d) {
                    // The chord is not held, or the daemon was told to stop listening:
                    // either way this keystroke is the user's, untouched.
                    return pass;
                }
                self.holding = true;
                self.latched = true;
                Step {
                    event: Some(HotkeyEvent::Pressed),
                    swallow: self.swallows(),
                }
            }
            ChordKey::Trigger => {
                if !self.holding {
                    // A key already down when the daemon started, a second keyboard's key-up
                    // or another process clearing a stuck key: a `Released` with no `Pressed`
                    // before it would end a dictation that never began.
                    return pass;
                }
                self.holding = false;
                let event = if self.latched {
                    self.latched = false;
                    Some(HotkeyEvent::Released)
                } else {
                    // A modifier already ended the hold; this up is swallowed all the same.
                    None
                };
                Step {
                    event,
                    swallow: self.swallows(),
                }
            }
        }
    }
}

/// A backend runs on its own thread and pushes events until the sender drops.
pub trait Hotkey: Send {
    fn run(self: Box<Self>, tx: Sender<HotkeyEvent>);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::EXAMPLE;

    /// grok-2: `docs/config.example.toml` is the reference a user reads before setting
    /// `hotkey.key`, and its list had drifted — `Insert`, `Escape`, `AltLeft`, `MetaLeft` and
    /// `MetaRight` were accepted by `validate_key_name` but named nowhere a reader looks.
    /// `check` prints the full list too, but only after a wrong guess has been refused.
    ///
    /// The thirty-six trigger-only names are documented as the two ranges `A-Z` and `0-9`,
    /// not one by one — so this asserts the ranges are in the file *and* that they are the
    /// truth: every name inside them parses as a trigger and nothing outside them does.
    #[test]
    fn every_accepted_key_name_is_documented_in_the_example() {
        let missing: Vec<&str> = KEY_NAMES
            .iter()
            .copied()
            .filter(|name| !EXAMPLE.contains(name))
            .collect();
        assert!(
            missing.is_empty(),
            "accepted by hotkey::KEY_NAMES, absent from docs/config.example.toml: {missing:?}"
        );

        for range in ["A-Z", "0-9"] {
            assert!(
                EXAMPLE.contains(range),
                "trigger range {range} absent from docs/config.example.toml"
            );
        }
        for c in ('A'..='Z').chain('0'..='9') {
            assert!(
                parse_chord(&format!("ControlLeft+{c}")).is_ok(),
                "`{c}` is documented by a range but not accepted"
            );
        }
        for c in ['a', 'z', '!', 'ß'] {
            assert!(
                parse_chord(&format!("ControlLeft+{c}")).is_err(),
                "`{c}` is outside the documented ranges but accepted"
            );
        }
    }

    #[test]
    fn a_single_name_parses_as_a_chord_with_no_modifiers() {
        let c = parse_chord("ControlRight").unwrap();
        assert!(c.modifiers.is_empty());
        assert_eq!(c.trigger, "ControlRight");
        assert_eq!(c.to_string(), "ControlRight");
    }

    #[test]
    fn a_chord_keeps_its_modifiers_in_order_and_renders_as_written() {
        let c = parse_chord("ControlLeft+ShiftLeft+Z").unwrap();
        assert_eq!(c.modifiers, ["ControlLeft", "ShiftLeft"]);
        assert_eq!(c.trigger, "Z");
        assert_eq!(c.to_string(), "ControlLeft+ShiftLeft+Z");
        // Padding is not a typo worth refusing, and refusing it would say `unknown key
        // `ControlLeft `` — a trailing space nobody can see in the message.
        assert_eq!(
            parse_chord(" ControlLeft + Z ").unwrap().to_string(),
            "ControlLeft+Z"
        );
        // A modifier need not be a modifier key: any accepted name may be held.
        assert_eq!(parse_chord("CapsLock+F13").unwrap().modifiers, ["CapsLock"]);
    }

    /// A bare letter or digit as the hotkey would swallow every one the user types, so it is
    /// refused where they can still read why — not discovered at the keyboard.
    #[test]
    fn a_letter_or_digit_alone_is_refused_and_says_what_to_do() {
        for bare in ["Z", "7"] {
            let e = parse_chord(bare).unwrap_err();
            assert!(e.contains("needs a modifier"), "{bare}: {e}");
            assert!(e.contains("ControlLeft+ShiftLeft+"), "{bare}: {e}");
        }
        assert!(parse_chord("ControlLeft+7").is_ok());
    }

    #[test]
    fn an_unknown_name_is_refused_wherever_it_sits() {
        for bad in ["Nope", "Nope+Z", "ControlLeft+nope", "ControlLeft+z"] {
            let e = parse_chord(bad).unwrap_err();
            assert!(e.starts_with("unknown key `"), "{bad}: {e}");
            // The accepted list is what turns the rejection into an answer.
            assert!(e.contains("ControlRight"), "{bad}: {e}");
        }
    }

    /// Both halves of a doubled key would map to one physical key: the chord could never be
    /// satisfied, or the trigger would be its own modifier.
    #[test]
    fn the_same_key_twice_is_refused() {
        for bad in ["ControlLeft+ControlLeft+Z", "ControlLeft+Z+Z"] {
            let e = parse_chord(bad).unwrap_err();
            assert!(e.contains("appears twice"), "{bad}: {e}");
        }
    }

    #[test]
    fn an_empty_element_is_refused() {
        for bad in ["ControlLeft++Z", "+Z", "ControlLeft+", "", "+"] {
            let e = parse_chord(bad).unwrap_err();
            assert!(e.starts_with("empty element in `"), "{bad:?}: {e}");
        }
    }

    const CTRL: ChordKey = ChordKey::Modifier(0);
    const SHIFT: ChordKey = ChordKey::Modifier(1);
    const TRIGGER: ChordKey = ChordKey::Trigger;

    fn tracker(key: &str) -> ChordTracker {
        ChordTracker::new(&parse_chord(key).unwrap())
    }

    /// `(event, swallow)` — the whole of one `Step`, in the order the rules read. Armed,
    /// which is the daemon's normal state; `feed_disarmed` is the tray's Disable.
    fn feed(t: &mut ChordTracker, key: ChordKey, down: bool) -> (Option<HotkeyEvent>, bool) {
        let s = t.feed(key, down, true);
        (s.event, s.swallow)
    }

    fn feed_disarmed(
        t: &mut ChordTracker,
        key: ChordKey,
        down: bool,
    ) -> (Option<HotkeyEvent>, bool) {
        let s = t.feed(key, down, false);
        (s.event, s.swallow)
    }

    /// The chord's own rule: the trigger fires only on top of every modifier, and the window
    /// never sees it. Modifiers pass through — swallowing a Shift up leaves the window
    /// holding a Shift forever.
    #[test]
    fn the_trigger_fires_on_top_of_the_modifiers_and_is_swallowed() {
        let mut t = tracker("ControlLeft+ShiftLeft+Z");
        assert_eq!(feed(&mut t, CTRL, true), (None, false));
        assert_eq!(feed(&mut t, SHIFT, true), (None, false));
        assert_eq!(
            feed(&mut t, TRIGGER, true),
            (Some(HotkeyEvent::Pressed), true)
        );
        assert_eq!(
            feed(&mut t, TRIGGER, false),
            (Some(HotkeyEvent::Released), true)
        );
        assert_eq!(feed(&mut t, SHIFT, false), (None, false));
        assert_eq!(feed(&mut t, CTRL, false), (None, false));
    }

    /// One press is one dictation: holding the chord repeats the trigger at the typematic
    /// rate, and every repeat is swallowed rather than typed.
    #[test]
    fn auto_repeat_of_the_trigger_reports_once_and_types_nothing() {
        let mut t = tracker("ControlLeft+Z");
        feed(&mut t, CTRL, true);
        assert_eq!(
            feed(&mut t, TRIGGER, true),
            (Some(HotkeyEvent::Pressed), true)
        );
        for _ in 0..5 {
            assert_eq!(feed(&mut t, TRIGGER, true), (None, true));
        }
        assert_eq!(
            feed(&mut t, TRIGGER, false),
            (Some(HotkeyEvent::Released), true)
        );
    }

    /// The trigger is a letter, so the common case is the user typing it: without the
    /// modifiers it must reach the window untouched and start nothing.
    #[test]
    fn the_trigger_without_its_modifiers_types() {
        let mut t = tracker("ControlLeft+ShiftLeft+Z");
        for _ in 0..3 {
            assert_eq!(feed(&mut t, TRIGGER, true), (None, false));
            assert_eq!(feed(&mut t, TRIGGER, false), (None, false));
        }
        // One modifier short is still typing.
        feed(&mut t, CTRL, true);
        assert_eq!(feed(&mut t, TRIGGER, true), (None, false));
        assert_eq!(feed(&mut t, TRIGGER, false), (None, false));
    }

    /// Fingers leave a chord in whatever order they like. Releasing a modifier first ends
    /// the hold — but the trigger is still physically down, so its repeats and its up stay
    /// ours, or `zzzz` lands in the window.
    #[test]
    fn a_modifier_released_first_ends_the_hold_and_the_trigger_stays_swallowed() {
        let mut t = tracker("ControlLeft+ShiftLeft+Z");
        feed(&mut t, CTRL, true);
        feed(&mut t, SHIFT, true);
        assert_eq!(
            feed(&mut t, TRIGGER, true),
            (Some(HotkeyEvent::Pressed), true)
        );
        assert_eq!(
            feed(&mut t, SHIFT, false),
            (Some(HotkeyEvent::Released), false)
        );
        assert_eq!(feed(&mut t, TRIGGER, true), (None, true), "a repeat");
        assert_eq!(feed(&mut t, CTRL, false), (None, false));
        assert_eq!(
            feed(&mut t, TRIGGER, false),
            (None, true),
            "swallowed, and no second Released"
        );
    }

    /// Still held after a modifier came and went: nothing new starts until the trigger has
    /// actually been let go and the chord pressed again.
    #[test]
    fn re_pressing_a_modifier_over_a_held_trigger_starts_nothing() {
        let mut t = tracker("ControlLeft+Z");
        feed(&mut t, CTRL, true);
        assert_eq!(
            feed(&mut t, TRIGGER, true),
            (Some(HotkeyEvent::Pressed), true)
        );
        assert_eq!(
            feed(&mut t, CTRL, false),
            (Some(HotkeyEvent::Released), false)
        );
        assert_eq!(feed(&mut t, CTRL, true), (None, false));
        assert_eq!(
            feed(&mut t, TRIGGER, true),
            (None, true),
            "no second Pressed"
        );
        assert_eq!(feed(&mut t, TRIGGER, false), (None, true));
        // A full release, and the next press is a dictation again.
        assert_eq!(
            feed(&mut t, TRIGGER, true),
            (Some(HotkeyEvent::Pressed), true)
        );
    }

    #[test]
    fn the_chord_pressed_twice_is_two_dictations() {
        let mut t = tracker("ControlLeft+ShiftLeft+Z");
        for _ in 0..2 {
            feed(&mut t, CTRL, true);
            feed(&mut t, SHIFT, true);
            assert_eq!(
                feed(&mut t, TRIGGER, true),
                (Some(HotkeyEvent::Pressed), true)
            );
            assert_eq!(
                feed(&mut t, TRIGGER, false),
                (Some(HotkeyEvent::Released), true)
            );
            feed(&mut t, SHIFT, false);
            feed(&mut t, CTRL, false);
        }
    }

    /// The default hotkey is one bare key, and the tracker has to leave that exactly as it
    /// was: press on the first down, release on the up, auto-repeat ignored, and nothing
    /// swallowed — `ControlRight` still reaches the window.
    #[test]
    fn a_hotkey_with_no_modifiers_behaves_as_a_bare_key_always_has() {
        let mut t = tracker("ControlRight");
        assert_eq!(
            feed(&mut t, TRIGGER, true),
            (Some(HotkeyEvent::Pressed), false)
        );
        assert_eq!(feed(&mut t, TRIGGER, true), (None, false), "auto-repeat");
        assert_eq!(
            feed(&mut t, TRIGGER, false),
            (Some(HotkeyEvent::Released), false)
        );
        // An up with no press before it — a key held when the daemon started, or another
        // process clearing a stuck key — ends a dictation that never began.
        assert_eq!(feed(&mut t, TRIGGER, false), (None, false));
    }

    /// The flags are built from events, and an event can simply never arrive: hold Ctrl+Shift
    /// into a UAC prompt and the ups go to that desktop, not to the hook. Both flags then say
    /// "held" with nothing held, and the next plain `z` would satisfy the chord and be eaten.
    /// `would_fire` is what lets the backend catch that — it asks the OS before swallowing
    /// anything, and re-feeding the truth disarms the stale flags.
    #[test]
    fn a_resync_clears_stale_modifiers_before_they_can_eat_the_trigger() {
        let mut t = tracker("ControlLeft+ShiftLeft+Z");
        feed(&mut t, CTRL, true);
        assert!(!t.would_fire(), "one modifier short");
        feed(&mut t, SHIFT, true);
        assert!(
            t.would_fire(),
            "the backend checks the OS at exactly this point"
        );

        // What the backend re-feeds when `GetAsyncKeyState` says neither is really held.
        assert_eq!(feed(&mut t, CTRL, false), (None, false));
        assert_eq!(feed(&mut t, SHIFT, false), (None, false));
        assert!(!t.would_fire());
        assert_eq!(
            feed(&mut t, TRIGGER, true),
            (None, false),
            "a plain z types"
        );
        assert_eq!(feed(&mut t, TRIGGER, false), (None, false));

        // And a resync that confirms the flags leaves the chord working.
        feed(&mut t, CTRL, true);
        feed(&mut t, SHIFT, true);
        assert_eq!(feed(&mut t, CTRL, true), (None, false), "still held");
        assert_eq!(feed(&mut t, SHIFT, true), (None, false));
        assert_eq!(
            feed(&mut t, TRIGGER, true),
            (Some(HotkeyEvent::Pressed), true)
        );
    }

    /// The two states in which the backend must *not* read the keyboard: a chord already ours
    /// (its repeats are ours whatever is held now), and a hotkey with no modifiers, which has
    /// no flags to go stale and swallows nothing anyway.
    #[test]
    fn a_held_chord_and_a_bare_key_never_ask_for_a_resync() {
        let mut t = tracker("ControlLeft+Z");
        feed(&mut t, CTRL, true);
        feed(&mut t, TRIGGER, true);
        assert!(!t.would_fire(), "already holding");

        let mut bare = tracker("ControlRight");
        assert!(!bare.would_fire());
        feed(&mut bare, TRIGGER, true);
        assert!(!bare.would_fire());
    }

    /// Told to stop listening, byovox must also stop *taking*: a chord pressed while the tray
    /// says Disable types its trigger and starts nothing. Swallowing a keystroke a disabled
    /// daemon is going to drop anyway would destroy it for nothing.
    #[test]
    fn a_disarmed_chord_types_its_trigger_and_starts_nothing() {
        let mut t = tracker("ControlLeft+ShiftLeft+Z");
        assert_eq!(feed_disarmed(&mut t, CTRL, true), (None, false));
        assert_eq!(feed_disarmed(&mut t, SHIFT, true), (None, false));
        assert_eq!(feed_disarmed(&mut t, TRIGGER, true), (None, false));
        assert_eq!(
            feed_disarmed(&mut t, TRIGGER, true),
            (None, false),
            "repeat"
        );
        assert_eq!(feed_disarmed(&mut t, TRIGGER, false), (None, false));
        assert_eq!(feed_disarmed(&mut t, SHIFT, false), (None, false));
        assert_eq!(feed_disarmed(&mut t, CTRL, false), (None, false));
    }

    /// Disable landing mid-hold does not abandon the chord halfway: its repeats and its up
    /// are still ours, or the window would get a `Z` up whose down it never saw. Only the
    /// *next* press is refused — and re-enabling arms it again.
    #[test]
    fn a_chord_latched_before_disable_still_finishes_its_swallow() {
        let mut t = tracker("ControlLeft+ShiftLeft+Z");
        feed(&mut t, CTRL, true);
        feed(&mut t, SHIFT, true);
        assert_eq!(
            feed(&mut t, TRIGGER, true),
            (Some(HotkeyEvent::Pressed), true)
        );
        // Disable lands here.
        assert_eq!(feed_disarmed(&mut t, TRIGGER, true), (None, true), "repeat");
        assert_eq!(
            feed_disarmed(&mut t, TRIGGER, false),
            (Some(HotkeyEvent::Released), true),
            "one Released, and the up is still swallowed"
        );
        // Still disabled: the next press is the user's.
        assert_eq!(feed_disarmed(&mut t, TRIGGER, true), (None, false));
        assert_eq!(feed_disarmed(&mut t, TRIGGER, false), (None, false));
        // Enable, and the chord works again.
        assert_eq!(
            feed(&mut t, TRIGGER, true),
            (Some(HotkeyEvent::Pressed), true)
        );
        assert_eq!(
            feed(&mut t, TRIGGER, false),
            (Some(HotkeyEvent::Released), true)
        );
    }

    /// The modifiers are tracked whatever the arming. Skipping the tracker while disabled
    /// would leave a modifier that was released in that window stuck down for good, and the
    /// next bare trigger would then fire the chord and be eaten.
    #[test]
    fn a_modifier_released_while_disarmed_still_clears() {
        let mut t = tracker("ControlLeft+Z");
        feed(&mut t, CTRL, true);
        assert_eq!(feed_disarmed(&mut t, CTRL, false), (None, false));
        // Armed again, with nothing held: a plain trigger types.
        assert_eq!(feed(&mut t, TRIGGER, true), (None, false));
        assert_eq!(feed(&mut t, TRIGGER, false), (None, false));
    }

    /// Same guard for a chord: the daemon cannot have seen the down that came before it.
    #[test]
    fn a_trigger_up_with_no_press_before_it_reports_nothing() {
        let mut t = tracker("ControlLeft+Z");
        assert_eq!(feed(&mut t, CTRL, true), (None, false));
        assert_eq!(feed(&mut t, TRIGGER, false), (None, false));
        assert_eq!(feed(&mut t, CTRL, false), (None, false));
    }
}
