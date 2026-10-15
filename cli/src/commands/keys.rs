//! Shared key bindings for Oak's full-screen viewers (`oak log`, `oak diff`).
//!
//! Both viewers are pagers at heart, and readers arrive with one of three sets
//! of muscle memory. Rather than pick a winner, every motion answers to all
//! three dialects at once:
//!
//! * **Arrows** — `↑ ↓`, `PgUp`/`PgDn`, `Home`/`End`.
//! * **less / vi** — `k j`, `f b` (page), `d u` (half page), `g G`, and
//!   **`Space` to advance a page**, the binding that makes a pager feel like a
//!   pager.
//! * **Emacs** — `C-p`/`C-n`, `C-v`/`M-v` (page), `M-<`/`M->` (buffer ends),
//!   `C-s` (search), `C-g` (cancel).
//!
//! Nothing here collides: the three dialects disagree on spelling, not on
//! meaning, and the one genuine clash (`Space` already toggles a directory in
//! `oak diff`'s file tree) is resolved per-pane by [`SpaceKey`].
//!
//! Keeping the table in one module means the two viewers can't drift apart,
//! and the footer/overlay help is generated from the same source as the
//! matching, so the documented keys are the implemented ones.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// A movement request, resolved from a key press. The viewer decides what a
/// "line" is (a commit row, a diff line) and how big a page is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Nav {
    LineUp,
    LineDown,
    PageUp,
    PageDown,
    HalfUp,
    HalfDown,
    Top,
    Bottom,
}

/// What `Space` means in the pane being handled.
///
/// In a pager pane it advances a page (less). In `oak diff`'s file tree it is
/// already the expand/collapse toggle, and stealing it would cost more than
/// the less-compatibility gains.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpaceKey {
    /// `Space` pages forward, `b` pages back — the `less` contract.
    Pages,
    /// `Space` belongs to the pane; don't claim it here.
    Reserved,
}

/// Map a key press to a [`Nav`], or `None` if it isn't a motion key.
///
/// Callers match their own pane-specific keys first (`Enter`, `q`, …) and fall
/// through to this for anything that moves a cursor or a viewport.
pub fn nav(ev: &KeyEvent, space: SpaceKey) -> Option<Nav> {
    let ctrl = ev.modifiers.contains(KeyModifiers::CONTROL);
    let alt = ev.modifiers.contains(KeyModifiers::ALT);

    match ev.code {
        // Arrows and the navigation cluster.
        KeyCode::Up => Some(Nav::LineUp),
        KeyCode::Down => Some(Nav::LineDown),
        KeyCode::PageUp => Some(Nav::PageUp),
        KeyCode::PageDown => Some(Nav::PageDown),
        KeyCode::Home => Some(Nav::Top),
        KeyCode::End => Some(Nav::Bottom),

        // Emacs meta motions. `M-<` / `M->` are the buffer ends; `M-v` pages
        // back (its `C-v` counterpart is in the control block below).
        KeyCode::Char('<') if alt => Some(Nav::Top),
        KeyCode::Char('>') if alt => Some(Nav::Bottom),
        KeyCode::Char('v') if alt => Some(Nav::PageUp),

        // Control motions. `C-n`/`C-p` are emacs; `C-f`/`C-b`/`C-d`/`C-u` are
        // shared between vi and less and predate this module.
        KeyCode::Char('n') if ctrl => Some(Nav::LineDown),
        KeyCode::Char('p') if ctrl => Some(Nav::LineUp),
        KeyCode::Char('v') if ctrl => Some(Nav::PageDown),
        KeyCode::Char('f') if ctrl => Some(Nav::PageDown),
        KeyCode::Char('b') if ctrl => Some(Nav::PageUp),
        KeyCode::Char('d') if ctrl => Some(Nav::HalfDown),
        KeyCode::Char('u') if ctrl => Some(Nav::HalfUp),

        // Bare letters: vi/less. Guarded on "no modifier" so a stray
        // `C-g`-style chord never reads as a motion.
        _ if ctrl || alt => None,
        KeyCode::Char('k') => Some(Nav::LineUp),
        KeyCode::Char('j') => Some(Nav::LineDown),
        KeyCode::Char('f') => Some(Nav::PageDown),
        KeyCode::Char('b') => Some(Nav::PageUp),
        KeyCode::Char('d') => Some(Nav::HalfDown),
        KeyCode::Char('u') => Some(Nav::HalfUp),
        KeyCode::Char('g') => Some(Nav::Top),
        KeyCode::Char('G') => Some(Nav::Bottom),
        KeyCode::Char(' ') if space == SpaceKey::Pages => Some(Nav::PageDown),

        _ => None,
    }
}

/// How far a [`Nav`] moves, given the pane's visible height in rows.
///
/// `less` pages by the window, not by a fixed 20 lines, so the viewers pass
/// the height they last rendered at. Full pages keep two rows of overlap so
/// the reader has an anchor across the jump; half pages are exactly half.
/// Returns `None` for the absolute motions ([`Nav::Top`] / [`Nav::Bottom`]),
/// which the caller handles directly.
pub fn delta(nav: Nav, page_rows: usize) -> Option<i32> {
    let page = page_rows.saturating_sub(2).max(1).min(i32::MAX as usize) as i32;
    let half = (page_rows / 2).max(1).min(i32::MAX as usize) as i32;
    match nav {
        Nav::LineUp => Some(-1),
        Nav::LineDown => Some(1),
        Nav::PageUp => Some(-page),
        Nav::PageDown => Some(page),
        Nav::HalfUp => Some(-half),
        Nav::HalfDown => Some(half),
        Nav::Top | Nav::Bottom => None,
    }
}

/// `?` — open the key-binding overlay. `F1` too, for the reader who reaches
/// for it.
pub fn is_help(ev: &KeyEvent) -> bool {
    matches!(ev.code, KeyCode::Char('?') | KeyCode::F(1))
        && !ev.modifiers.contains(KeyModifiers::CONTROL)
}

/// Start a search: `/` (vi/less) or `C-s` (emacs incremental search).
pub fn is_search(ev: &KeyEvent) -> bool {
    let ctrl = ev.modifiers.contains(KeyModifiers::CONTROL);
    match ev.code {
        KeyCode::Char('/') if !ctrl => true,
        KeyCode::Char('s') if ctrl => true,
        _ => false,
    }
}

/// Back out of the current mode: `Esc` or emacs `C-g`.
pub fn is_cancel(ev: &KeyEvent) -> bool {
    match ev.code {
        KeyCode::Esc => true,
        KeyCode::Char('g') if ev.modifiers.contains(KeyModifiers::CONTROL) => true,
        _ => false,
    }
}

/// The character a key press contributes to a text field, if any.
///
/// Chorded keys (`C-g`, `M-v`) must not land in the search box as bare
/// letters — that's what made `C-g` type a `g` instead of cancelling.
pub fn text_input(ev: &KeyEvent) -> Option<char> {
    if ev
        .modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
    {
        return None;
    }
    match ev.code {
        KeyCode::Char(c) => Some(c),
        _ => None,
    }
}

/// Rows of the `?` overlay: `(keys, meaning)`. Rendered by both viewers so the
/// documented bindings are, by construction, the implemented ones.
pub const HELP_ROWS: &[(&str, &str)] = &[
    ("↑ ↓  ·  k j  ·  ^P ^N", "move a line"),
    ("PgUp PgDn  ·  b Space  ·  ^B ^F  ·  M-v ^V", "page"),
    ("^U ^D  ·  u d", "half page"),
    ("Home End  ·  g G  ·  M-< M->", "top / bottom"),
    ("/  ·  ^S", "search"),
    ("Esc  ·  ^G", "cancel / back"),
    ("Tab  ·  ← →  ·  h l", "switch pane"),
    ("?  ·  F1", "this help"),
    ("q", "quit"),
];

/// A compact one-line footer naming the motions, for panes that scroll.
pub const PAGER_FOOTER_HINT: &str =
    " ↑↓/jk/^P^N move · Space/PgDn/^V page · ^U^D half · g/G top/bottom · ? keys · q quit";

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn alt(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::ALT)
    }

    /// The three dialects must agree on meaning: whichever spelling a reader
    /// arrives with, the motion is the same one.
    #[test]
    fn vi_emacs_and_arrow_dialects_map_to_the_same_motions() {
        let space = SpaceKey::Pages;
        for ev in [
            plain('k'),
            ctrl('p'),
            KeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
        ] {
            assert_eq!(nav(&ev, space), Some(Nav::LineUp), "{ev:?}");
        }
        for ev in [
            plain('j'),
            ctrl('n'),
            KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
        ] {
            assert_eq!(nav(&ev, space), Some(Nav::LineDown), "{ev:?}");
        }
        for ev in [
            plain('f'),
            plain(' '),
            ctrl('f'),
            ctrl('v'),
            KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE),
        ] {
            assert_eq!(nav(&ev, space), Some(Nav::PageDown), "{ev:?}");
        }
        for ev in [
            plain('b'),
            ctrl('b'),
            alt('v'),
            KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE),
        ] {
            assert_eq!(nav(&ev, space), Some(Nav::PageUp), "{ev:?}");
        }
        for ev in [
            plain('g'),
            alt('<'),
            KeyEvent::new(KeyCode::Home, KeyModifiers::NONE),
        ] {
            assert_eq!(nav(&ev, space), Some(Nav::Top), "{ev:?}");
        }
        for ev in [
            plain('G'),
            alt('>'),
            KeyEvent::new(KeyCode::End, KeyModifiers::NONE),
        ] {
            assert_eq!(nav(&ev, space), Some(Nav::Bottom), "{ev:?}");
        }
        assert_eq!(nav(&ctrl('d'), space), Some(Nav::HalfDown));
        assert_eq!(nav(&plain('d'), space), Some(Nav::HalfDown));
        assert_eq!(nav(&ctrl('u'), space), Some(Nav::HalfUp));
        assert_eq!(nav(&plain('u'), space), Some(Nav::HalfUp));
    }

    /// `Space` pages in a pager pane but stays the tree's own toggle in
    /// `oak diff`'s file list.
    #[test]
    fn space_pages_only_where_the_pane_offers_it() {
        assert_eq!(nav(&plain(' '), SpaceKey::Pages), Some(Nav::PageDown));
        assert_eq!(nav(&plain(' '), SpaceKey::Reserved), None);
    }

    /// A chord must never be mistaken for the bare letter that spells it —
    /// `C-g` cancels, and does not read as `g` (top) or type a `g`.
    #[test]
    fn chords_are_not_confused_with_the_bare_letters_they_contain() {
        assert_eq!(nav(&ctrl('g'), SpaceKey::Pages), None);
        assert!(is_cancel(&ctrl('g')));
        assert_eq!(text_input(&ctrl('g')), None);
        assert_eq!(text_input(&alt('v')), None);
        assert_eq!(text_input(&plain('g')), Some('g'));

        assert!(is_search(&plain('/')));
        assert!(is_search(&ctrl('s')));
        assert!(!is_search(&plain('s')));

        assert!(is_help(&plain('?')));
        assert!(!is_help(&ctrl('?')));
    }

    /// Paging follows the window like `less`, with a two-row overlap so the
    /// reader keeps an anchor across the jump. Tiny panes still move.
    #[test]
    fn page_size_tracks_the_window_and_never_stalls() {
        assert_eq!(delta(Nav::PageDown, 40), Some(38));
        assert_eq!(delta(Nav::PageUp, 40), Some(-38));
        assert_eq!(delta(Nav::HalfDown, 40), Some(20));
        assert_eq!(delta(Nav::HalfUp, 40), Some(-20));
        assert_eq!(delta(Nav::LineDown, 40), Some(1));
        // A one-row pane (or a zero-height one mid-resize) still advances.
        assert_eq!(delta(Nav::PageDown, 1), Some(1));
        assert_eq!(delta(Nav::HalfDown, 1), Some(1));
        assert_eq!(delta(Nav::PageDown, 0), Some(1));
        // Absolute motions have no delta — the caller jumps directly.
        assert_eq!(delta(Nav::Top, 40), None);
        assert_eq!(delta(Nav::Bottom, 40), None);
    }
}
