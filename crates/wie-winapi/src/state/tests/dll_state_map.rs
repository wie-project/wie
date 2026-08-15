//! `DllStateMap` slot-mapping invariants: the single authoritative
//! [`DllId`] → slot mapping (`dll_slot`) covers every variant exactly once,
//! every index addresses the `DllId::COUNT`-sized array, and state round-trips
//! through each slot.

use super::*;

/// Minimal per-slot state used to probe [`DllStateMap`] round-trips.
#[derive(Debug, Default, PartialEq, Eq)]
struct SlotProbe(u64);

/// Every [`DllId`] maps to a unique in-range slot index with a non-empty
/// label, and the slot round-trips: a fresh slot reads back as uninitialised,
/// `get_or_init` allocates a default, writes survive, and `get` reads them
/// back.
#[test]
fn dll_slot_round_trip_covers_every_variant() {
    let mut map = DllStateMap::new();
    let mut seen_indices = [false; DllId::COUNT];

    for id in DllId::iter() {
        let (index, label) = dll_slot(id);
        // The index must address the COUNT-sized array…
        assert!(
            index < DllId::COUNT,
            "{label}: slot index {index} out of range (COUNT = {})",
            DllId::COUNT
        );
        // …and no two variants may share one.
        let seen = seen_indices
            .get_mut(index)
            .expect("slot index already checked in range");
        assert!(!*seen, "{label}: duplicate slot index {index}");
        *seen = true;
        // Debug labels must be present for every slot.
        assert!(!label.is_empty(), "{id:?}: empty slot label");

        // Fresh slot: uninitialised, then a default on first access.
        assert_eq!(
            map.get::<SlotProbe>(id),
            None,
            "{label}: fresh slot read as Some"
        );
        assert!(
            map.try_get_or_init::<SlotProbe>(id).is_ok(),
            "{label}: fallible init reported an error"
        );
        let probe = map.get_or_init::<SlotProbe>(id);
        assert_eq!(*probe, SlotProbe(0), "{label}: default value");
        // Writes survive through the map.
        probe.0 = u64::try_from(index).expect("slot index fits u64");
        assert_eq!(
            map.get::<SlotProbe>(id),
            Some(&SlotProbe(
                u64::try_from(index).expect("slot index fits u64")
            )),
            "{label}: write/read round-trip"
        );
    }

    // The mapping is a bijection onto 0..COUNT: every slot was visited.
    assert!(
        seen_indices.iter().all(|seen| *seen),
        "mapping does not cover every slot index"
    );
    // Loaded slots appear in Debug output under their own label — this pins
    // the `Winhttp` slot (index 14), which the old `slot_of` left as "?".
    let debug = format!("{map:?}");
    for (_, label) in DllId::iter().map(dll_slot) {
        assert!(
            debug.contains(label),
            "Debug output missing {label}: {debug}"
        );
    }
}
