//! Ghostex: what marks a window for redraw, counted per drawn frame, for a diagnostic the app turns
//! on (TEMPORARY, remove once the Windows redraw investigation of 2026-11-19 is done).
//!
//! Off by default. While off, every hook below costs one relaxed atomic load. While on, each call
//! that can make a window draw again (`Context::notify`, `App::notify`, `Window::refresh`,
//! `Window::request_animation_frame`, `App::refresh_windows`) is recorded with its caller's source
//! location (and, for `Context::notify`, the view's type), and every drawn frame is counted once
//! for each distinct cause that was pending on its window. [`take_report`] hands the counts back
//! and starts over.

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};
use std::panic::Location;
use std::sync::atomic::{AtomicBool, Ordering};

static ENABLED: AtomicBool = AtomicBool::new(false);

#[derive(Default)]
struct State {
    /// The cause the call being made right now belongs to, set by the public entry points so the
    /// internal invalidation they lead to can be attributed.
    current: Option<String>,
    /// Causes recorded against each window since its last draw, by the window's invalidator.
    pending: HashMap<usize, BTreeSet<String>>,
    /// The cause of a pending `App::refresh_windows`, applied when its effect runs.
    refresh_windows: Option<String>,
    frames: u64,
    frames_by_window: HashMap<usize, u64>,
    frames_by_cause: HashMap<String, u64>,
    calls_by_cause: HashMap<String, u64>,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
}

/// Turns the census on or off. Turning it off drops what was counted.
pub fn set_enabled(enabled: bool) {
    if ENABLED.swap(enabled, Ordering::Relaxed) && !enabled {
        STATE.with_borrow_mut(|state| *state = State::default());
    }
}

/// Whether the census is counting.
#[inline]
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// What was counted since the last report.
#[derive(Debug, Default)]
pub struct RedrawReport {
    /// Frames drawn, across every window.
    pub frames: u64,
    /// Frames drawn by each window (an opaque per-window number).
    pub frames_by_window: Vec<(usize, u64)>,
    /// For each cause, how many drawn frames it was pending for, most first.
    pub frames_by_cause: Vec<(String, u64)>,
    /// For each cause, how many calls were made, whether or not a window drew because of them.
    pub calls_by_cause: Vec<(String, u64)>,
}

/// Hands back what was counted and starts over; `None` while the census is off.
pub fn take_report() -> Option<RedrawReport> {
    if !enabled() {
        return None;
    }
    STATE.with_borrow_mut(|state| {
        let sorted = |map: HashMap<String, u64>| {
            let mut list: Vec<(String, u64)> = map.into_iter().collect();
            list.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            list
        };
        let mut frames_by_window: Vec<(usize, u64)> = std::mem::take(&mut state.frames_by_window)
            .into_iter()
            .collect();
        frames_by_window.sort_by(|a, b| b.1.cmp(&a.1));
        Some(RedrawReport {
            frames: std::mem::take(&mut state.frames),
            frames_by_window,
            frames_by_cause: sorted(std::mem::take(&mut state.frames_by_cause)),
            calls_by_cause: sorted(std::mem::take(&mut state.calls_by_cause)),
        })
    })
}

fn location(location: &Location<'_>) -> String {
    // Trimmed to the part of the path under the repository, so a line names the file plainly.
    let file = location.file().replace('\\', "/");
    let file = file
        .rfind("/src/")
        .and_then(|at| file[..at].rfind('/').map(|crate_at| &file[crate_at + 1..]))
        .unwrap_or(&file);
    format!("{file}:{}", location.line())
}

/// The cause for a call at `caller`, built only while the census is on.
pub(crate) fn cause(kind: &str, view: Option<&str>, caller: &Location<'_>) -> Option<String> {
    enabled().then(|| match view {
        Some(view) => format!("{kind} {view} @ {}", location(caller)),
        None => format!("{kind} @ {}", location(caller)),
    })
}

/// Runs `call` with `cause` as the current cause, unless one is already set (an outer entry point's
/// cause wins: `Context::notify` leads to `App::notify`).
pub(crate) fn with_cause<R>(cause: Option<String>, call: impl FnOnce() -> R) -> R {
    let Some(cause) = cause else {
        return call();
    };
    let set = STATE.with_borrow_mut(|state| {
        if state.current.is_some() {
            return false;
        }
        *state.calls_by_cause.entry(cause.clone()).or_default() += 1;
        state.current = Some(cause);
        true
    });
    let result = call();
    if set {
        STATE.with_borrow_mut(|state| state.current = None);
    }
    result
}

/// A window was marked dirty: the current cause is pending on it.
pub(crate) fn note_window_dirty(window: usize) {
    if !enabled() {
        return;
    }
    STATE.with_borrow_mut(|state| {
        let cause = state
            .current
            .clone()
            .unwrap_or_else(|| "unattributed".to_string());
        state.pending.entry(window).or_default().insert(cause);
    });
}

/// `App::refresh_windows` was called; its effect runs later ([`take_refresh_windows_cause`]).
pub(crate) fn note_refresh_windows(cause: Option<String>) {
    let Some(cause) = cause else {
        return;
    };
    STATE.with_borrow_mut(|state| {
        *state.calls_by_cause.entry(cause.clone()).or_default() += 1;
        state.refresh_windows.get_or_insert(cause);
    });
}

/// The cause of the `App::refresh_windows` whose effect is being applied now.
pub(crate) fn take_refresh_windows_cause() -> Option<String> {
    if !enabled() {
        return None;
    }
    STATE.with_borrow_mut(|state| state.refresh_windows.take())
}

/// A window drew a frame: one frame for each cause pending on it.
pub(crate) fn note_draw(window: usize) {
    if !enabled() {
        return;
    }
    STATE.with_borrow_mut(|state| {
        state.frames += 1;
        *state.frames_by_window.entry(window).or_default() += 1;
        let mut causes = state.pending.remove(&window).unwrap_or_default();
        if causes.is_empty() {
            causes.insert("no recorded cause (platform or first frame)".to_string());
        }
        for cause in causes {
            *state.frames_by_cause.entry(cause).or_default() += 1;
        }
    });
}
