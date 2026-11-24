# Who decides a guest write: the protection table, the fast paths, and the host mapping

Date: measured on Apple M3, macOS 26.5.2 (25F84), arm64, `hw.pagesize` = 16384,
at commit `cae55c1` + the change this document describes. Scope: `crates/wie-cpu/src/mem/`.
Numbers here are from `cargo nextest run -p wie-cpu` in this session, not from a
prior report.

## The question

`VirtualProtect(tail, 32, PAGE_EXECUTE_READWRITE)` returned `Ok`, `mem_read` on the
page worked, and a write on the same page was refused. Three layers can refuse a
write and they were asked in this order: the page-protection lookup
(`PageMap::check_access`), the *derived* write authority handed to compiled code
(`PageTlbEntry::allow_w`, `RegionPinInfo::allow_w`, `GuestMemory::host_span`), and
the host `mprotect` on the arena's mmap.

## What each layer actually did (measured before the fix)

A matrix probe over `map RW / RX / ERW / RO / NONE → protect ERW`, writing 32 bytes
at the tail of a 64 KiB mapping, plus the image-tail shape (`map_image`, whole
image `PAGE_NOACCESS`, one section `PAGE_EXECUTE_READ`, then the tail to RWX):

| probe | result |
|---|---|
| `write` after every protect-to-ERW case | **Ok** |
| `read` on the same page | **Ok** |
| `page_tlb_entry(page).allow_w` | **false** |
| `host_span(page, 32, write=true)` | **None** |
| `mem_fill(page, ..)` / `mem_copy(..)` | **false** |
| `host_slice_mut(page, ..)` | **None** |

So the protection lookup was **not** the layer that refused: it honoured
`PAGE_EXECUTE_READWRITE` and `write` succeeded. The allocation's `RwxPerms` was
**not** consulted at access time either — no access path reads
`VadNode::allocation_protect`; only `VirtualQuery` reports it.

The refusal came from `PageProtect::allow_write_for` — the W-on-X rule,
`allows_write() && !allows_execute()` — which the TLB, the region pins and the
bulk/host-pointer paths all used as if it were the page's *permission*. It is not:
it exists because a store through a raw host pointer cannot run the
code-invalidation drain that self-modifying guest code needs. Reading it as a
permission made an RWX page "writable by `write`, not writable by anything else",
so the same protection produced two different answers depending on which path the
access took.

A second, independent fault was in the host layer, and it was worse than a
refusal — it was a process kill:

```
VirtualAlloc(0x1_0000, RESERVE|COMMIT, PAGE_READWRITE)   ok
VirtualProtect(0x4000, PAGE_READONLY)                    ok   (host frame → PROT_READ)
VirtualFree(+0x1000, MEM_DECOMMIT)                       SIGBUS (signal 10)
```

`MEM_DECOMMIT` zeros the guest bytes by storing through the *host* mapping, so
decommitting a page inside a frame an earlier protect had tightened stores into
`PROT_READ` host pages. `sync_host_protect` swallowed the `mprotect` error
(`let _ = ...`) and was not called at all from `va_commit_only`, `va_decommit`,
`va_reserve_only` or `va_reserve_and_commit` — four paths on which the host
mapping could drift tighter than the protection table it is supposed to mirror.

## The order now

1. **The current protection is the only access authority.**
   `PageProtect::allows(kind)` decides, and `PageMap::check_access` is its only
   gate, so `read` / `write` / `fetch_into` honour exactly what the guest last
   set. `PAGE_EXECUTE_READWRITE` means writable, end of argument.
2. **The fast-pointer paths ask a different question and may say no.**
   `PageProtect::allows_soft_store` (renamed from `allow_write_for`) answers "may
   a store land through a raw host pointer?", and executable pages say no. That
   answer is a **capability miss, not a permission denial**: every consumer
   (`wie_jit_store`'s TLB probe, the region pins, `host_span`, `mem_copy`,
   `mem_fill`, `host_slice_mut`) falls back to `GuestMemory::write`, which runs
   the code-invalidation drain and succeeds. Keeping SMC invalidation inside
   `write` is why the capability gate cannot simply be widened to match the
   permission.
3. **The host mapping is a cache of (1) and never tighter than it.**
   `sync_host_protect` is called from every mutation path (`map`, `map_image`,
   `VirtualProtect`, reserve, commit, decommit) and derives each 16 KiB frame's
   prot from the guest pages inside it. Because `write` stores through that
   mapping, a frame that **must** be writable and cannot be made writable fails
   the protect (which then undoes its own page-map change and reports the error)
   instead of admitting the protect and dying later on the first store. A failed
   *tighten* is deliberately not an error: SPC still denies the guest, and a
   frame left at RW only loses defence-in-depth. Frames the arena cannot express
   at host granularity are never tightened either — both directions pass the same
   alignment tests — so they are still at the `mmap` default RW.
4. **`MEM_DECOMMIT` relaxes before it zeroes.** The span is marked reserved
   first, then the host frames are widened (a reserved page answers RW in
   `host_prot_for_frame`), and only then are the bytes zeroed through the host
   mapping.

Nothing here asks the host for `PROT_EXEC`: guest execute is soft-translated, so
the macOS W^X constraints in [`code-memory-on-darwin.md`](code-memory-on-darwin.md)
govern Cranelift's own code pages, not guest arenas, and `mprotect` to
`PROT_READ|PROT_WRITE` on our own anonymous mapping is always permitted.

## Generation and TLB invalidation (unchanged, and load-bearing)

Every protection change bumps `GuestMemory::generation()` — `mem_map`,
`mem_map_image`, `VirtualProtect`, `VirtualAlloc` reserve/commit, `VirtualFree`
decommit/release — including a `VirtualProtect` that sets the protection already
in force. `jit/cpu_engine.rs` mirrors the value into `JitShared::mem_gen` and
calls `invalidate_tlb` for the same calls; this fix **does not move any bump**,
and the undo path in `virtual_protect` bumps again so a refused protect still
invalidates. `every_protection_change_bumps_the_generation` and
`engine_memory_generation_follows_protection_changes` pin it, because the JIT
probe folds `page_key ^ mem_gen` into its slot tag and a stale tag there is a
memory-safety bug rather than a slow path.

## What proves the three failures are gone

| failure | now |
|---|---|
| protect-to-RWX then write | `write` succeeds; `mem_fill` / `mem_copy` / `host_span(write)` still refuse, and that is asserted as a *capability* in `protected_writable_page_writes_but_offers_no_fast_pointer` |
| `RW → RX` then a write | faults with `mem_write permission denied …`, read and fetch still work (`protect_to_rx_denies_write_with_permission_error_and_keeps_fetch`) |
| decommit inside a tightened frame | no fault; decommit, recomit and write all succeed (`decommit_inside_a_readonly_host_frame_does_not_fault` — this test SIGBUSed before the fix) |
| a compiled block writing through a page re-protected after compilation | the store faults with `ACCESS_VIOLATION`, write type, exact address, and stores nothing; restoring RW makes the same store land again (`guest_store_observes_protection_change_jit`, which also asserts `cache_hits >= 1` so the fault provably came from a compiled block) |

`WIE_MPROTECT=0` still disables the host layer entirely; the gate's off-values
(`0`, `false`, `off`, case-insensitive) are pinned by
`host_mprotect_switch_is_off_only_for_explicit_off_values`, and with it unset the
host mapping stays at the `mmap` default RW for the arena's whole life.
