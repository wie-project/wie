//! Revision-driven pull-based repaint: the request_paint / reconcile latch, child-to-top resolution, and the published-frame drain.
use super::*;

// ---------------------------------------------------------------------------
// Revision-driven pull-based repaint: the request_paint / reconcile latch.
// ---------------------------------------------------------------------------

/// `request_paint` bumps the top-level content revision of a KNOWN window —
/// the push half of the latch. A fresh window has no entry; each call
/// advances it.
#[test]
fn request_paint_bumps_the_top_level_content_revision() {
    let mut state = default_winapi_state();
    let hwnd = crate::handles::Hwnd::from(0x6610_0201_u64);
    state.window_state().windows.push(WindowRecord {
        handle: hwnd,
        window_proc: 0x7000_0000,
        width: 100,
        height: 50,
        visible: true,
        ..Default::default()
    });

    assert!(
        !state.present().revisions.content_rev.contains_key(&hwnd),
        "a fresh window has no content revision yet"
    );

    present::PresentState::request_paint(&mut state, hwnd.as_u64());
    assert_eq!(
        state.present().revisions.content_rev.get(&hwnd),
        Some(&present::ContentRev(1))
    );

    present::PresentState::request_paint(&mut state, hwnd.as_u64());
    assert_eq!(
        state.present().revisions.content_rev.get(&hwnd),
        Some(&present::ContentRev(2)),
        "each mutation advances the revision"
    );
}

/// A child control's mutation resolves to its top-level ancestor — the
/// surface that actually composites it.
#[test]
fn request_paint_resolves_child_controls_to_the_top_level() {
    let mut state = default_winapi_state();
    let top = crate::handles::Hwnd::from(0x6610_0301_u64);
    let child = crate::handles::Hwnd::from(0x6610_0302_u64);
    state.window_state().windows.push(WindowRecord {
        handle: top,
        window_proc: 0x7000_0000,
        width: 200,
        height: 100,
        visible: true,
        ..Default::default()
    });
    state.window_state().windows.push(WindowRecord {
        handle: child,
        window_proc: 0x7000_0000,
        parent_handle: top,
        x: 10,
        y: 20,
        width: 80,
        height: 30,
        visible: true,
        ..Default::default()
    });

    present::PresentState::request_paint(&mut state, child.as_u64());

    assert_eq!(
        state.present().revisions.content_rev.get(&top),
        Some(&present::ContentRev(1)),
        "the child's mutation marks the top-level's revision"
    );
    assert!(
        !state.present().revisions.content_rev.contains_key(&child),
        "children have no revision of their own"
    );
}

/// Unknown windows are silently ignored — no revision entry is created.
#[test]
fn request_paint_unknown_window_is_a_no_op() {
    let mut state = default_winapi_state();
    present::PresentState::request_paint(&mut state, 0x7777_0001_u64);
    assert!(
        state.present().revisions.content_rev.is_empty(),
        "an unknown hwnd must not create a revision entry"
    );
}

/// `request_paint` fires the same stored wake the publish path fires, so the
/// host presenter pulls the repainted frame as soon as it is published.
#[test]
fn request_paint_fires_the_stored_wake() {
    let mut state = default_winapi_state();
    let hwnd = crate::handles::Hwnd::from(0x6610_0401_u64);
    state.window_state().windows.push(WindowRecord {
        handle: hwnd,
        window_proc: 0x7000_0000,
        width: 50,
        height: 25,
        visible: true,
        ..Default::default()
    });
    let fired = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = Arc::clone(&fired);
    state.present().wake = Some(Box::new(move || {
        flag.store(true, std::sync::atomic::Ordering::SeqCst);
    }));

    present::PresentState::request_paint(&mut state, hwnd.as_u64());

    assert!(
        fired.load(std::sync::atomic::Ordering::SeqCst),
        "a visible mutation must wake the host presenter"
    );
}

/// The full latch loop: a painted mutation is drained and reconciled in one
/// idle pass — the frame reaches the published map and the revisions catch
/// up, so a second reconcile publishes nothing.
#[test]
fn mutation_reconcile_publishes_the_fresh_frame_once() {
    let mut state = default_winapi_state();
    let hwnd = crate::handles::Hwnd::from(0x6610_0501_u64);
    state.window_state().windows.push(WindowRecord {
        handle: hwnd,
        window_proc: 0x7000_0000,
        width: 120,
        height: 60,
        visible: true,
        ..Default::default()
    });

    // A visible mutation: the dirty-region mark (what the control seams do)
    // plus the revision bump.
    present::PresentState::request_paint(&mut state, hwnd.as_u64());
    gdi32::fill_rect_surface(&mut state, hwnd, 120, 60, 0, 0, 120, 60, 0x00FF_FFFF);
    state.present().drain_pending_publishes();
    assert_eq!(
        state.present().reconcile_and_publish(),
        1,
        "the stale top-level is reconciled"
    );
    assert!(
        state.present().published.contains_key(&hwnd),
        "the mutation's frame is published"
    );
    assert_eq!(
        state.present().reconcile_and_publish(),
        0,
        "a caught-up window is skipped until the next mutation"
    );
}
