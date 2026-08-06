//! Phase-0 fake-handle disjointness: dynamically allocated handle bases never overlap the FAKE handle ranges.
use super::*;

// ── Phase 0: handle disjointness ─────────────────────────────────
//
// Ensures dynamically-allocated handle bases seeded in WindowState do not
// overlap the FAKE handle ranges used by USER32/GDI32 stubs.
//
// FAKE handles live in ranges:
//   USER32: 0x6600_0000..0x6601_xxxx
//   GDI32:  0x6800_0000..0x6800_xxxx
//
// Seeded bases live in:
//   window_handle:  0x6610_0000
//   menu_handle:    0x6620_0000
//   hook_handle:    0x6630_0000
//   atoms:          0xC000
//   timer_id:       1
//   (future GDI DC: 0x6810_0000, bitmap: 0x6820_0000)

#[test]
fn test_fake_handle_disjointness() {
    let ws = WindowState::default();

    // USER32 window handle range (0x6610_0000+) must not overlap FAKE range (0x6600_xxxx)
    assert!(
        ws.next_window_handle.as_u64() >= 0x0000_0000_6610_0000,
        "window handle base collides with FAKE range"
    );

    // Menu/hook bases
    assert!(
        ws.next_menu_handle.as_u64() >= 0x0000_0000_6620_0000,
        "menu handle base collides with FAKE range"
    );
    assert!(
        ws.next_windows_hook_handle.as_u64() >= 0x0000_0000_6630_0000,
        "hook handle base collides with FAKE range"
    );

    // Atoms in user-atom range (0xC000-0xFFFF)
    assert!(
        ws.next_window_class_atom >= 0xC000,
        "class atom base collides with FAKE range"
    );
    assert!(
        ws.next_global_atom >= 0xC000,
        "global atom base collides with FAKE range"
    );

    // Timer ID is trivially disjoint
    assert_eq!(ws.next_timer_id, 1, "timer ID must be 1");
}
