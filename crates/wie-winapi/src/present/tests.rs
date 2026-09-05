//! Tests for the present channel (split out of `mod.rs` for the
//! file-size policy; test files are exempt).

#[cfg(test)]
use super::{ContentRev, DEFAULT_BACKGROUND_COLOR, PresentState, SurfaceFrame};
use crate::gdi32::IRect;
use crate::handles::Hwnd;
use std::sync::Arc;

#[test]
fn deferred_publishes_coalesce_per_dispatch() {
    let mut state = PresentState::new();
    let hwnd = Hwnd::from(7);
    state.ensure_surface(hwnd, 200, 100);

    // Two writes in one dispatch (e.g. BitBlt + window-DC TextOut) each
    // defer a publish for the same HWND.
    state.publish_deferred(hwnd);
    state.publish_deferred(hwnd);

    // Nothing is published until the drain.
    assert!(state.published.is_empty());
    assert_eq!(state.pending_publishes.len(), 1);

    let published = state.drain_pending_publishes();
    assert_eq!(
        published, 1,
        "N deferred writes in one dispatch → 1 publish"
    );
    assert!(state.pending_publishes.is_empty());

    let _frame = state.published.get(&hwnd).expect("frame published");
    // The publish moved the painted buffer into the Arc; the surface is
    // empty and will be handed back by the next ensure_surface (B1).
    assert!(
        state
            .surfaces
            .get(&hwnd)
            .is_some_and(|s| s.pixels.is_empty())
    );
}

#[test]
fn deferred_publish_of_two_hwnds_emits_one_frame_each() {
    let mut state = PresentState::new();
    let a = Hwnd::from(1);
    let b = Hwnd::from(2);
    state.ensure_surface(a, 100, 100);
    state.ensure_surface(b, 100, 100);
    state.publish_deferred(a);
    state.publish_deferred(b);
    assert_eq!(state.drain_pending_publishes(), 2);
    assert!(state.published.contains_key(&a));
    assert!(state.published.contains_key(&b));
}

#[test]
fn request_host_sync_fires_the_stored_wake() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let mut state = PresentState::new();
    let fired = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&fired);
    state.wake = Some(Box::new(move || {
        flag.store(true, Ordering::SeqCst);
    }));

    state.request_host_sync();

    assert!(
        fired.load(Ordering::SeqCst),
        "request_host_sync must fire the same stored wake the publish path fires"
    );
}

#[test]
fn request_host_sync_without_wake_is_a_no_op() {
    // No wake installed (headless runs): the call must be a silent no-op.
    PresentState::new().request_host_sync();
}

/// Registering a top-level stacks it on TOP of the z-order (the last
/// element) and bumps BOTH revisions — the window-set fingerprint the
/// presenter's reconcile keys on, and the z-order fingerprint.
#[test]
fn register_top_level_stacks_top_and_bumps_both_revs() {
    let mut state = PresentState::new();
    let a = Hwnd::from(1);
    let b = Hwnd::from(2);

    state.register_top_level(a);
    assert_eq!(state.windows_rev, 1);
    assert_eq!(state.z_rev, 1);
    assert_eq!(
        state.z_order,
        vec![a],
        "the first top-level is the only row"
    );

    state.register_top_level(b);
    assert_eq!(state.windows_rev, 2);
    assert_eq!(state.z_rev, 2);
    assert_eq!(
        state.z_order,
        vec![a, b],
        "creation order, newest on top (back-to-front)"
    );
}

/// Unregistering a top-level removes it from the z-order and bumps the
/// window-set revision unconditionally (the host must drop the stale
/// window even if the destroy raced a create that never registered).
#[test]
fn unregister_top_level_removes_from_z_order() {
    let mut state = PresentState::new();
    let a = Hwnd::from(1);
    let b = Hwnd::from(2);
    let c = Hwnd::from(3);
    state.register_top_level(a);
    state.register_top_level(b);
    state.register_top_level(c);
    let rev_before = state.z_rev;

    state.unregister_top_level(b);

    assert_eq!(state.z_order, vec![a, c], "the destroyed window is gone");
    assert_eq!(state.windows_rev, 4);
    assert_eq!(
        state.z_rev,
        rev_before + 1,
        "a tracked top-level destroy bumps the z-order revision"
    );
}

/// Unregistering a window never registered still bumps the window-set
/// revision (the host reconcile must run) but leaves the z-order
/// revision alone (nothing re-stacked).
#[test]
fn unregister_unknown_window_bumps_set_rev_only() {
    let mut state = PresentState::new();
    state.register_top_level(Hwnd::from(1));
    let z_rev_before = state.z_rev;

    state.unregister_top_level(Hwnd::from(99));

    assert_eq!(state.windows_rev, 2, "the set rev always bumps");
    assert_eq!(
        state.z_rev, z_rev_before,
        "an untracked destroy cannot re-stack the z-order"
    );
}

/// SetWindowPos HWND_TOP moves a window to the top of the z-order; a
/// window already on top is a no-op that must NOT bump the revision (a
/// spurious bump would re-trigger the host reorder for no change).
#[test]
fn z_order_top_moves_window_to_the_top() {
    let mut state = PresentState::new();
    let a = Hwnd::from(1);
    let b = Hwnd::from(2);
    let c = Hwnd::from(3);
    state.register_top_level(a);
    state.register_top_level(b);
    state.register_top_level(c);
    let rev_before = state.z_rev;

    assert!(state.z_order_to_top(a));
    assert_eq!(state.z_order, vec![b, c, a]);
    assert_eq!(state.z_rev, rev_before + 1);

    // Repeating the same move changes nothing.
    assert!(
        !state.z_order_to_top(a),
        "a window already on top must not bump z_rev"
    );
    assert_eq!(state.z_rev, rev_before + 1);
    assert_eq!(state.z_order, vec![b, c, a]);
}

/// SetWindowPos HWND_BOTTOM moves a window to the back of the z-order.
#[test]
fn z_order_bottom_moves_window_to_the_back() {
    let mut state = PresentState::new();
    let a = Hwnd::from(1);
    let b = Hwnd::from(2);
    let c = Hwnd::from(3);
    state.register_top_level(a);
    state.register_top_level(b);
    state.register_top_level(c);
    let rev_before = state.z_rev;

    assert!(state.z_order_to_bottom(c));
    assert_eq!(state.z_order, vec![c, a, b]);
    assert_eq!(state.z_rev, rev_before + 1);

    // A window already at the back is a no-op.
    assert!(!state.z_order_to_bottom(c));
    assert_eq!(state.z_rev, rev_before + 1);
}

/// Z-order operations never touch the window-SET revision: they reorder
/// existing host windows but change no membership, so the reconcile
/// latch must not fire.
#[test]
fn z_reorder_leaves_windows_rev_untouched() {
    let mut state = PresentState::new();
    let a = Hwnd::from(1);
    let b = Hwnd::from(2);
    state.register_top_level(a);
    state.register_top_level(b);
    let set_rev = state.windows_rev;

    state.z_order_to_top(a);
    state.z_order_to_bottom(b);

    assert_eq!(
        state.windows_rev, set_rev,
        "a SetWindowPos z-change never changes the window SET"
    );
}

/// An empty frame for the headless record slot.
fn empty_frame() -> SurfaceFrame {
    SurfaceFrame {
        width: 0,
        stride: 0,
        height: 0,
        pixels: Arc::new(Vec::new()),
        background_color: DEFAULT_BACKGROUND_COLOR,
        region: None,
    }
}

#[test]
fn published_frame_carries_recorded_background_color() {
    let mut state = PresentState::new();
    let hwnd = Hwnd::from(9);
    state.set_background_color(hwnd, 0x00F0_F0F0); // COLOR_BTNFACE
    state.record = Some(Box::new(empty_frame()));
    state.ensure_surface(hwnd, 16, 16);
    state.publish(hwnd);

    let frame = state.published.get(&hwnd).expect("published frame");
    assert_eq!(
        frame.background_color, 0x00F0_F0F0,
        "the frame must carry the owning window's background color"
    );
    let recorded = state.record.as_ref().expect("recorded frame");
    assert_eq!(
        recorded.background_color, 0x00F0_F0F0,
        "the headless record slot carries the background too"
    );
}

#[test]
fn published_frame_defaults_to_white_background() {
    // No erase ever recorded a color: the presenter falls back to
    // COLOR_WINDOW-white (notepad's client, and the headless default).
    let mut state = PresentState::new();
    let hwnd = Hwnd::from(10);
    state.ensure_surface(hwnd, 8, 8);
    state.publish(hwnd);
    let frame = state.published.get(&hwnd).expect("published frame");
    assert_eq!(frame.background_color, 0x00FF_FFFF);
}

#[test]
fn background_filled_frame_has_no_black_pixels() {
    // F2 invariant at the present level: a frame whose surface was
    // erased with its recorded background color (the erase-before-clear
    // guarantee) publishes zero 0x000000 pixels.
    let mut state = PresentState::new();
    let hwnd = Hwnd::from(11);
    let background = 0x00FF_FFFF;
    state.set_background_color(hwnd, background);
    state.ensure_surface(hwnd, 32, 24);
    if let Some(surf) = state.surfaces.get_mut(&hwnd) {
        for px in &mut surf.pixels {
            *px = background;
        }
    }
    state.record = Some(Box::new(empty_frame()));
    state.publish(hwnd);

    let recorded = state.record.as_ref().expect("recorded frame");
    assert!(
        !recorded.pixels.contains(&0x0000_0000),
        "a background-erased frame must contain no unpainted black pixels"
    );
    assert!(
        recorded.pixels.iter().all(|&px| px == 0x00FF_FFFF),
        "every pixel is the erased background"
    );
}

#[test]
fn black_background_is_legitimate_and_recorded() {
    // A window whose class brush is genuinely black (COLOR_WINDOWTEXT)
    // erases to black — that content is legitimate, and the invariant's
    // "outside legitimately black content" clause excludes it. The frame
    // must still record the black background so the presenter clears
    // with it (never the default white-on-black mismatch).
    let mut state = PresentState::new();
    let hwnd = Hwnd::from(12);
    let black = 0x0000_0000;
    state.set_background_color(hwnd, black);
    state.ensure_surface(hwnd, 8, 8);
    if let Some(surf) = state.surfaces.get_mut(&hwnd) {
        for px in &mut surf.pixels {
            *px = black;
        }
    }
    state.publish(hwnd);
    let frame = state.published.get(&hwnd).expect("published frame");
    assert_eq!(frame.background_color, 0x0000_0000);
}

/// A paint that writes two disjoint sub-rects must publish a frame whose
/// region is their union — the GPU uploads exactly the repainted area.
///
/// The accumulator starts from the post-publish reset: a fresh surface is
/// fully dirty (`None`) and a partial write must not narrow it, so the
/// scenario seeds one full publish first, then hands the buffer back (the
/// next paint cycle's `ensure_surface`) before the partial writes.
#[test]
fn publish_reports_the_union_of_partial_writes() {
    let mut state = PresentState::new();
    let hwnd = Hwnd::from(20);
    state.ensure_surface(hwnd, 200, 100);
    state.publish(hwnd);
    state.ensure_surface(hwnd, 200, 100);
    // Two writes in one repaint cycle, both via the write-primitive seam
    // (`mark_dirty` is what fill_rect_surface / the BitBlt / the text
    // band call).
    state.mark_dirty(
        hwnd,
        IRect {
            left: 10,
            top: 20,
            right: 60,
            bottom: 50,
        },
    );
    state.mark_dirty(
        hwnd,
        IRect {
            left: 120,
            top: 70,
            right: 180,
            bottom: 90,
        },
    );
    state.publish(hwnd);

    let frame = state.published.get(&hwnd).expect("published frame");
    assert_eq!(
        frame.region,
        Some(IRect {
            left: 10,
            top: 20,
            right: 180,
            bottom: 90
        }),
        "the frame region is the union of the cycle's writes"
    );
    assert_eq!(frame.stride, 256, "the row pitch is 64-padded (ADR-0001)");
    assert_eq!(
        frame.pixels.len(),
        usize::try_from(frame.stride * frame.height).unwrap_or(0),
        "the pixel bytes still carry the FULL padded frame (region is a hint)"
    );
}

/// A dirty rect covering the whole surface normalizes to `None` (full):
/// a full repaint must take the presenter's zero-copy full-frame upload,
/// not a packed full-size region.
#[test]
fn full_surface_dirty_normalizes_to_none() {
    let mut state = PresentState::new();
    let hwnd = Hwnd::from(21);
    state.ensure_surface(hwnd, 64, 32);
    state.mark_dirty(
        hwnd,
        IRect {
            left: 0,
            top: 0,
            right: 64,
            bottom: 32,
        },
    );
    state.publish(hwnd);
    let frame = state.published.get(&hwnd).expect("published frame");
    assert_eq!(
        frame.region, None,
        "a whole-surface repaint is a full frame"
    );
}

/// A publish with NO recorded writes emits a FULL frame (region None): a
/// direct full-surface writer such as the D3D9 Present handler writes the
/// whole surface without marking partial rects, so "nothing recorded"
/// cannot be distinguished from "everything changed" — the hint must
/// never under-report.
#[test]
fn publish_without_recorded_writes_is_full() {
    let mut state = PresentState::new();
    let hwnd = Hwnd::from(22);
    state.ensure_surface(hwnd, 16, 16);
    state.publish(hwnd);
    let first = state.published.get(&hwnd).expect("published frame");
    assert_eq!(first.region, None, "a fresh surface is full");

    // A second publish with no writes (the D3D9 pattern — the surface was
    // written directly, no marks recorded) is full too: the buffer is
    // handed back and published without any `mark_dirty` call.
    state.ensure_surface(hwnd, 16, 16);
    state.publish(hwnd);
    let second = state.published.get(&hwnd).expect("published frame");
    assert_eq!(
        second.region, None,
        "an unmarked publish must fall back to full, never to an empty region"
    );
}

/// `publish` resets the dirty accumulator: the NEXT cycle's writes start
/// a fresh region instead of accumulating the previous one.
#[test]
fn publish_resets_the_dirty_accumulator() {
    let mut state = PresentState::new();
    let hwnd = Hwnd::from(23);
    state.ensure_surface(hwnd, 100, 100);
    state.publish(hwnd);
    assert_eq!(
        state.surfaces.get(&hwnd).expect("surface").dirty,
        Some(IRect::empty()),
        "the dirty accumulator resets to 'nothing' after a publish"
    );
    // The next write produces a region that starts from scratch.
    state.ensure_surface(hwnd, 100, 100);
    state.mark_dirty(
        hwnd,
        IRect {
            left: 40,
            top: 40,
            right: 55,
            bottom: 55,
        },
    );
    state.publish(hwnd);
    let frame = state.published.get(&hwnd).expect("published frame");
    assert_eq!(
        frame.region,
        Some(IRect {
            left: 40,
            top: 40,
            right: 55,
            bottom: 55
        }),
        "the second cycle's region carries only its own write"
    );
}

/// A new surface is dirty `None` (full): its content is unknown until the
/// first paint, so the first publish must always upload everything.
#[test]
fn new_surface_is_dirty_full() {
    let mut state = PresentState::new();
    let hwnd = Hwnd::from(24);
    state.ensure_surface(hwnd, 32, 32);
    assert_eq!(
        state.surfaces.get(&hwnd).expect("surface").dirty,
        None,
        "a freshly created surface is fully dirty"
    );
}

/// The hand-back keeps the composite accumulating across publishes, and
/// in the steady state it is zero-ALLOC, not zero-copy: the presenter
/// channel pins a clone of every published frame (the host takes it
/// without the big lock, at any time), so the published Arc can no longer
/// be unwrapped on the next paint. The recycled allocation is the
/// DISPLACED channel slot: publishing frame N+1 frees the channel's
/// frame-N clone, which lands in the spare pool and comes back as the
/// next paint base — the very first cycle still pays one clone.
#[test]
fn hand_back_moves_the_published_buffer_zero_copy() {
    let mut state = PresentState::new();
    let hwnd = Hwnd::from(30);
    state.ensure_surface(hwnd, 64, 32);
    // Paint a recognizable pattern, then publish: the buffer leaves the
    // surface and lands in the published Arc plus the channel's slot
    // clone (a refcount bump, not a copy).
    if let Some(surf) = state.surfaces.get_mut(&hwnd) {
        for (i, px) in surf.pixels.iter_mut().enumerate() {
            *px = u32::try_from(i % 7 + 1).unwrap_or(0);
        }
    }
    state.publish(hwnd);
    let published_ptr = state
        .published
        .get(&hwnd)
        .expect("published frame")
        .pixels
        .as_ptr();

    // First paint cycle: the channel pins its clone, so `try_unwrap`
    // fails and no spare exists yet — the fallback CLONES (one alloc,
    // first cycle only). The composite still accumulates.
    state.ensure_surface(hwnd, 64, 32);
    assert_eq!(state.hand_back_unwrap, 0, "the channel pins every frame");
    assert_eq!(
        state.hand_back_clone, 1,
        "no spare yet: one first-cycle clone"
    );
    let surface = state.surfaces.get(&hwnd).expect("surface");
    assert!(
        surface.pixels.iter().any(|&px| px != 0),
        "the reclaimed buffer carries the previously painted content"
    );
    // The published map is empty until the repaint republishes: a
    // presenter take in this window sees None and skips (the old content
    // is being replaced); the next publish re-adds before waking it.
    assert!(
        !state.published.contains_key(&hwnd),
        "the reclaimed entry is gone until the repaint republishes"
    );

    // Republish: the clone flows through a fresh Arc, and the channel's
    // displaced slot (the FIRST frame, still holding the original
    // allocation) lands in the spare pool.
    state.publish(hwnd);
    let republished = state.published.get(&hwnd).expect("republished frame");
    assert_ne!(
        republished.pixels.as_ptr(),
        published_ptr,
        "the republish wraps the cloned buffer in a new Arc"
    );

    // Second paint cycle: `try_unwrap` fails again (the channel pins the
    // new frame), but the displaced first-frame buffer is now a spare —
    // the hand-back takes it with NO clone and NO allocation, and the
    // original published allocation comes back as the paint base.
    state.ensure_surface(hwnd, 64, 32);
    assert_eq!(
        state.hand_back_clone, 1,
        "steady-state hand-backs recycle spares"
    );
    let surface = state.surfaces.get(&hwnd).expect("surface");
    assert_eq!(
        surface.pixels.as_ptr(),
        published_ptr,
        "the displaced channel slot's allocation returns as the paint base"
    );
    assert!(
        surface.pixels.iter().any(|&px| px != 0),
        "the spare carries the painted content (the composite keeps accumulating)"
    );
}

/// Every publish wraps its buffer in a FRESH Arc allocation: the
/// presenter's `Arc::ptr_eq` present-skip compares consecutive takes, and
/// a repaint must never be mistaken for an unchanged frame. Both Arcs are
/// kept alive here so the comparison is deterministic (a freed Arc's
/// address can be reused by the allocator, so comparing against one would
/// be flaky).
#[test]
fn each_publish_wraps_the_buffer_in_a_fresh_arc() {
    let mut state = PresentState::new();
    let hwnd = Hwnd::from(32);
    state.ensure_surface(hwnd, 64, 32);
    state.publish(hwnd);
    // The presenter's take_frame clone: holds the first frame's Arc alive
    // while the second publish runs (it also forces the clone fallback on
    // the reclaim below, which this test does not assert on).
    let first: Arc<Vec<u32>> = Arc::clone(&state.published.get(&hwnd).expect("first frame").pixels);

    state.ensure_surface(hwnd, 64, 32);
    state.publish(hwnd);
    let second = state
        .published
        .get(&hwnd)
        .expect("second frame")
        .pixels
        .clone();
    assert!(
        !Arc::ptr_eq(&first, &second),
        "a republish always allocates a new Arc — the ptr_eq present-skip never misfires"
    );
}

/// The fallback CLONES when the host still holds the published Arc (the
/// presenter's take_frame clone, or the app's last-presented keep-alive):
/// the scratch buffer is a fresh allocation carrying the same pixels, so
/// the composite still accumulates — at the cost of one copy.
#[test]
fn hand_back_clones_when_the_host_still_holds_the_frame() {
    let mut state = PresentState::new();
    let hwnd = Hwnd::from(31);
    state.ensure_surface(hwnd, 64, 32);
    if let Some(surf) = state.surfaces.get_mut(&hwnd) {
        for (i, px) in surf.pixels.iter_mut().enumerate() {
            *px = u32::try_from(i % 7 + 1).unwrap_or(0);
        }
    }
    state.publish(hwnd);
    // The presenter's take_frame clones the SurfaceFrame, bumping the
    // pixel Arc's refcount — that clone (or the keep-alive) is exactly the
    // documented blocker that forces the fallback.
    let held: Arc<Vec<u32>> = Arc::clone(&state.published.get(&hwnd).expect("published").pixels);
    let published_ptr = state
        .published
        .get(&hwnd)
        .expect("published")
        .pixels
        .as_ptr();

    state.ensure_surface(hwnd, 64, 32);
    assert_eq!(state.hand_back_unwrap, 0);
    assert_eq!(state.hand_back_clone, 1);
    let surface = state.surfaces.get(&hwnd).expect("surface");
    assert_ne!(
        surface.pixels.as_ptr(),
        published_ptr,
        "the clone fallback allocates a fresh buffer"
    );
    assert_eq!(
        surface.pixels, *held,
        "the cloned buffer carries the same pixels (the composite keeps accumulating)"
    );
}

/// A stale top-level (content revision ahead of its last published
/// revision) is republished by `reconcile_and_publish`; the last-published
/// revision catches up, so the same window is then skipped.
#[test]
fn reconcile_publishes_stale_top_levels_and_advances_their_rev() {
    let mut state = PresentState::new();
    let hwnd = Hwnd::from(40);
    state.ensure_surface(hwnd, 32, 16);
    // Paint recognizable content so the reconcile's republish carries it.
    if let Some(surf) = state.surfaces.get_mut(&hwnd) {
        for px in &mut surf.pixels {
            *px = 0x00FF_FFFF;
        }
    }
    state.revisions.content_rev.insert(hwnd, ContentRev(3));
    state
        .revisions
        .last_published_rev
        .insert(hwnd, ContentRev(2));

    assert_eq!(
        state.reconcile_and_publish(),
        1,
        "the stale top-level is republished once"
    );
    assert!(state.published.contains_key(&hwnd));
    assert_eq!(
        state.revisions.last_published_rev.get(&hwnd),
        Some(&ContentRev(3)),
        "the last-published revision catches up to the content revision"
    );

    assert_eq!(
        state.reconcile_and_publish(),
        0,
        "a caught-up window is skipped until the next mutation"
    );
}

/// A window with no `content_rev` entry was never mutated — the reconcile
/// diff iterates the mutation set, not the window registry.
#[test]
fn reconcile_ignores_windows_without_a_content_revision() {
    let mut state = PresentState::new();
    let hwnd = Hwnd::from(41);
    state.ensure_surface(hwnd, 8, 8);
    state.publish(hwnd);
    assert_eq!(state.reconcile_and_publish(), 0);
}

/// A stale top-level with NO surface still catches up: the publish is a
/// no-op, but the revision must not stay stale forever — an idle loop
/// would otherwise republish it on every boundary.
#[test]
fn reconcile_catches_up_revisions_even_without_a_surface() {
    let mut state = PresentState::new();
    let hwnd = Hwnd::from(42);
    state.revisions.content_rev.insert(hwnd, ContentRev(1));

    assert_eq!(state.reconcile_and_publish(), 1);
    assert!(
        !state.published.contains_key(&hwnd),
        "no surface means nothing to publish"
    );
    assert_eq!(
        state.revisions.last_published_rev.get(&hwnd),
        Some(&ContentRev(1))
    );
    assert_eq!(state.reconcile_and_publish(), 0);
}

/// `unregister_top_level` drops a destroyed window's revision
/// bookkeeping: it must not stay stale (an idle reconcile would
/// republish its ghost surface) or leak its entries.
#[test]
fn unregister_top_level_drops_the_revision_bookkeeping() {
    let mut state = PresentState::new();
    let hwnd = Hwnd::from(43);
    state.revisions.content_rev.insert(hwnd, ContentRev(5));
    state
        .revisions
        .last_published_rev
        .insert(hwnd, ContentRev(5));

    state.unregister_top_level(hwnd);

    assert!(!state.revisions.content_rev.contains_key(&hwnd));
    assert!(!state.revisions.last_published_rev.contains_key(&hwnd));
}
