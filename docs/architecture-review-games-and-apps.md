# Architecture review — "any game at high FPS, any app of any kind"

Status: analysis · 2026-09-02 · branch `feat/dll-coverage` @ `19a656a`
Question: what must change architecturally so WIE can (a) emulate any game at high FPS and (b) emulate any app of any kind, given the observed limitation that **JIT execution and rendering do not overlap**.

Companion docs: `jit-architecture-review.md` (throughput math), `perf-plan-2.md` (round-2 goals), `adr/0001`, `adr/0002`, `dll-coverage-plan.md`.

---

## 0. TL;DR

The "JIT vs rendering" limitation is not a JIT problem and not a rendering problem. It is a **concurrency architecture problem**: every pixel-producing operation (GDI paint, D3D9 software rasterization, `Present`, publish) runs synchronously **inside a WinAPI handler, on the guest emu thread, under the single process-wide `Arc<Mutex<WinApiState>>`** — the same lock every guest thread takes twice per API stop and the host UI thread takes once per frame. Rendering therefore *pauses emulation by construction*, and emulation *pauses rendering*; there is no path where they overlap.

On top of that sit three independent ceilings:

1. **The JIT is ~6× off target and its accepted fix was never built.** ADR-0002 (direct block-to-block register hand-off, SSA rflags) is marked *Accepted, default on* — in code the `WIE_JIT_DIRECT_REGS` gate is parsed, defaults on, and **has zero call sites**; no x19–x28 mapping exists in `jit/lower/*`; every block boundary and every chain hop still pays the full 16-GPR + 16-XMM `JitCtx` round trip (`pipeline.rs:1181–1328`, `emit.rs:206–237`). Same story for ADR-0001's zero-copy pitch (`WIE_SURFACE_PAD` and the 64-px pad: zero hits in the workspace; `upload_source` allocates a fresh pack buffer per frame, `present_wgpu.rs:669`).
2. **The ISA coverage cliff is fatal, not degrading.** One unimplemented mnemonic (SDL2 or a game's hand-coded SSE3/SSSE3/SSE4/AVX, or x87 in a legacy CRT) kills the session (`exec/mod.rs:654–657`). There is no x87 at all.
3. **The game-subsystem surface is total absence, not gaps:** no audio backend anywhere (dsound/xaudio2 are not even name-recognized → load failure), no D3D11/DXGI, no XInput/DirectInput/RawInput, D3D9 at 51/119 device methods on a host-CPU software rasterizer, OpenGL fragment shading via a per-pixel tree-walking interpreter.

Everything below is evidence-backed and grouped into painpoints, then three target-architecture decisions with options, then a sequenced roadmap.

---

## 1. Why JIT and rendering cannot overlap today (the core finding)

The chain, verified in code:

1. **All rendering is handler-side.** GDI `BitBlt`/fills write into `PresentState` surfaces and D3D9 `DrawPrimitive*` synchronously rasterizes into the host backbuffer — no command queue, no render thread, `EndScene` flushes nothing (`d3d9/device.rs:500-503`, rasterization inside `draw.rs:20-169` → `raster.rs:452`).
2. **All handlers run under the big lock.** The quantum machine takes `Arc<Mutex<WinApiState>>` around activation and dispatch (≥2 acquisitions per API stop, `quantum.rs:469-494, 579-582`), and `PresentState`, `GdiState`, `D3D9State`, `Window` records, `SyncState`, VFS, registry, clipboard all live inside that one mutex (`state/mod.rs:389-418`). The heap shard and the message queue are the only two escapes.
3. **The UI thread joins the queue.** `take_frame`, window snapshots, and key-state injection all lock the same mutex from the winit main thread (`session/mod.rs:346-376`, `session/window.rs:346-356`). A long `DrawPrimitive` or a 4 MB clone-fallback stalls macOS rendering of the *host* window.
4. **No pacing anywhere on the emu side.** D3D9 `Present` returns immediately by design ("vsync frame pacing is deferred to P4", `device.rs:311-314`), every `Present` is a full-frame copy + publish + wake under the lock, and one `WieEvent::Frame` per guest frame is sent with explicit no-coalescing (`app.rs:741-762, 974-981`). A guest at 1000 FPS floods the main thread with 1000 lock acquisitions + take_frames per second.
5. **Secondary couplings.** Host-side guest writes take the process-wide `JitShared::mem` **write** lock (`jit/cpu_engine.rs:28-33`) contending with every other thread's block decode and reads; compilation serializes on the single Cranelift `JITModule` mutex (`shared.rs:253, 803`) so the boot-time compile burst (Doom Retro: ~38 s) cannot parallelize; and `host_slice` returns raw arena pointers whose no-unmap safety argument holds only because the WinAPI mutex happens to serialize dispatch (`cpu_engine.rs:47-78`) — a fragile invariant, not a designed one.

So when a game runs its loop — render thread blasting D3D9 state changes and draws, main thread in USER32, audio thread polling — **frame throughput equals big-mutex throughput**, and the software rasterizer consumes emu-thread cycles the JIT needed.

### Docs-vs-code drift (important)

Two accepted ADRs and the status doc describe mitigations that are **not in the code**:

| Claim | Where | Reality |
|---|---|---|
| ADR-0002 direct register ABI, "default on" | `config.rs:275-278, 408-413` | gate parsed, `direct_regs_enabled()` dead code (no call sites), no x19–x28 mapping in `jit/lower/*` |
| ADR-0002 SSA rflags | `lower/insn.rs:89-…`, `lower/flags.rs:152-258` | flags are lazy but packed-u64; every consume re-unpacks bits |
| ADR-0001 64-px padded width → always zero-copy upload | `present/mod.rs:292, 352-362` | unpadded; `WIE_SURFACE_PAD` does not exist (0 grep hits) |
| ADR-0001 reused `WgpuPresenter` upload scratch | `present_wgpu.rs:669` | fresh `vec![0_u8; h*padded]` per region/unaligned upload |
| status.md "steady-state GDI/D3D9 frames are zero-alloc/zero-copy" | — | true only for widths ≡ 0 mod 64 and when the spare-buffer round trip doesn't miss |

Any perf plan built on the current docs starts from a false baseline. **First action: a drift audit of every ADR/status "✅ landed" claim.**

---

## 2. Painpoint catalog

Severity legend: 🔴 blocks the goal outright · 🟠 major ceiling · 🟡 needed for "any app".

### A. Execution & serialization architecture

**A1 🔴 One big mutex over everything WinAPI.** `Arc<Mutex<WinApiState>>` covers kernel threads/sync, window records, GDI, D3D9, present surfaces, VFS, registry, clipboard, WS2, etc. (`state/mod.rs:389-418`). Every API stop from every guest thread, every paint, every `Present`, and the UI thread's `take_frame` serialize on it. One window's rasterize blocks every other guest thread and the host UI (head-of-line blocking is structural, not incidental). Change direction: shard — extract `Present`, `Gdi`, `D3D9`, and `Window` into separately-locked domains exactly the way `GuestHeap` and `MessageQueue` were already extracted, keeping one lock only for `KernelState` (threads/sync) (see Decision B).

**A2 🔴 The single global `active` TID register.** `ThreadState.active` is one process-global "who is dispatching" slot; every dispatch must re-activate under the lock or CS ownership/TLS/waits corrupt (`runtime.md:124`; the 7za `-mmt2` deadlock class). Any sharding must replace this with per-domain or per-thread context (handler contexts should carry their `GuestThread` by value/Arc, not read a global).

**A3 🔴 Software rasterization on the emu thread.** `DrawPrimitive*` rasterizes synchronously under the big lock; per-draw vertex-stream `.to_vec()` + index clone are extra copies (`d3d9/draw.rs:117-122, 141`). No render thread, no command capture. This is the single biggest reason a game cannot hit FPS: the rasterizer's wall time is subtracted directly from emulation time. Change direction: command capture + render thread first, GPU offload second (Decision A).

**A4 🟠 No frame pacing / no D3D9 publish coalescing.** Guest FPS unbounded; one wake per Present; full-frame copy per Present under the lock (`device.rs:315-356`, `present/mod.rs:476-604`). Change direction: latest-wins frame slot + dirty-union + wake coalescing (the GDI path already does this via `pending_publishes`; D3D9 deliberately bypasses it, `present/mod.rs:473-475`); plus an optional emu-side Present throttle (sleep-until-vblank estimate) so guest FPS caps near swap rate and the main thread stops drowning.

**A5 🟠 Cooperative scheduling only.** 20 M-insn quanta, `yield_now` on pure-compute, no time-based preemption, no priorities, affinity stubbed (`memory.rs:148`, `mt_runtime.rs:690-693`, `process_thread.rs:185-192`). A compute-heavy guest thread can starve peers until budget exhaustion; a spinning render thread cannot be deprioritized. Change direction: time-sliced quanta (wall-clock budget in addition to insn budget) + Win32 priority mapped to host thread priorities (QoS classes on macOS).

**A6 🟡 Poll-based waits.** `pthread_cond_wait` parks re-enter every 1 ms; `WaitFor*INFINITE` polls in 50 ms slices; CS parks 1 ms (`quantum.rs:781-788, 809-870`). With render+audio+main threads this is N busy-waking host threads and ms-granularity wake latency. The `WakeHub` inbox design (`wake.rs`) is good but only wired to the message queue. Change direction: extend per-object `WaiterRegistry` signaling to kernel objects, CS queues, and pthread waits — event-driven wait resolution end-to-end.

**A7 🟡 One process-wide message queue; cross-thread `SendMessage` runs inline.** No thread affinity in `GetMessage` — a worker can steal the main thread's messages (`user32/message/mod.rs:383-465`); `SendMessage` to another thread's window executes the target WndProc on the caller (`:238-260`). Qt-class apps rely on per-thread queues + marshaling; several frameworks create UI-adjacent threads with their own message loops. Change direction: per-thread queues (queue created per thread, `GetThreadMessage` filters by TID) + inter-thread send marshaling with a reply mailbox (this also gives `SendMessageTimeout` semantics).

**A8 🟡 Timers are pump-bound.** WINMM `timeSetEvent` callbacks fire only on the primary pump, at most one per quantum (`pump.rs:612-660, 813-858`); no host timer thread. A compute-saturated primary delays all guest timers (multimedia timers are exactly what old games use for pacing/audio). Change direction: host timer thread posting into the (now per-thread) queues via the wake hub.

### B. CPU core / JIT

**B1 🔴 ADR-0002 is unbuilt — every block boundary pays the `JitCtx` round trip.** Full 16 GPR + 16 XMM copy-in and copy-out per block entry/exit (`pipeline.rs:1181-1328`) and dirty-GPR flush + reload at every chain hop (`emit.rs:206-237`); chain depth cap 48 forces dispatcher re-entry (`lower/mod.rs:441`). The accepted fix (fixed native ABI, x19–x28, SSA rflags) is designed but absent. This is the highest-ROI CPU item and it is already specced (see Decision C).

**B2 🔴 ISA coverage cliff is fatal.** Unimplemented mnemonic = session stop (`exec/mod.rs:654-657`). Missing for games/apps: x87 entirely (only fninit/fnclex no-ops; `fnstcw`/`fldcw`/`_ftol` in legacy CRT startup will kill a session), SSE3/SSSE3/SSE4.1/4.2 (ptest, pextr*/pinsr*, pmin*/pmax*, pblend*, palignr, pmulld, pcmpeqq, crc32, popcnt, string ops), all AVX/AVX2, 64-bit `div/idiv` and 8/16-bit ops in JIT (iced-only today). Also integer-SSE lowering is two scalar helper calls per 128-bit op (`lower/sse.rs:20`) — only FP lanes actually vectorize. Change direction: (1) policy change — unknown mnemonics degrade to a generic iced fallback that raises a "slow path" flag instead of dying, so coverage gaps cost time, not the session; (2) implement the histogram-driven missing set (batch per `WIE_JIT_OPCODE_HISTO`); (3) minimal x87 (register stack emulation over the existing xmm/fpr storage, or translate x87 FP to SSE at decode — the 32-bit-era DLL payload case is real per status.md).

**B3 🟠 Compilation cannot parallelize.** One `cranelift_jit::JITModule` behind `JitShared::engine: Mutex` (`shared.rs:253, 803`); K workers overlap execution with compile, not compile with compile. Boot bursts (cold compile ~0.5-3 ms/block, `cache_persist.rs:27`) dominate real-app startup. The persistent ledger (commit `19a656a`) is metadata-only; the tier-0 "baseline" emitter is a stub delegating to the same lowering (`baseline.rs:1-11`). Change direction: per-worker `JITModule` + runtime-resolved chain table (perf-plan-2 G3 spike) or batched multi-block module commits; that is what actually cuts the 38 s Doom Retro boot.

**B4 🟠 Cross-thread memory-lock coupling.** Every host-side guest write takes `JitShared::mem` write lock (`cpu_engine.rs:28-33`); block decode, `host_slice`, `mem_read` on all other threads wait. `host_slice`'s returned raw pointer relies on the WinAPI mutex to prevent concurrent unmapping (`cpu_engine.rs:57-58`) — a blit racing another thread's `VirtualFree` is a correctness cliff. Change direction: arena refcounting/epoch-protection for raw slices (pin the arena for the borrow's lifetime), and narrow the write lock (per-arena RwLock or a seqlock-style generation) once handlers stop running under the big lock.

**B5 🟡 Epoch machinery complexity.** Five interacting generations (`cache_epoch`, `invalidate_gen`, `mem_gen`, `pins_gen`, sticky/tlb gens) with stale-chain bugs already in history (`eebe3d0`, `e385a31`). Not a perf item per se, but every later change (B1, B3, A3) multiplies through this surface — consolidate before/while building on it.

### C. Render / present path

**C1 🔴 Present/Gdi/D3D9/PresentState all inside the big mutex** — see A1/A3; the render-specific shard is the first one to extract because it is self-contained (surfaces, published frames, spare pool, dirty unions, wake).

**C2 🔴 ADR-0001 completion missing.** No 64-px pitch padding (zero-copy upload only for widths ≡ 0 mod 64 — most window sizes pay a full-frame pack copy on the **main thread**, `present_wgpu.rs:648-685`); no reused upload scratch (fresh alloc per region upload, `:669`). At 60 fps on a 1512-px window that is ~2.4 MB/frame avoidable main-thread memcpy. Small, mechanical, high value.

**C3 🟠 Clone fallback under the lock.** `ensure_surface`'s `Arc::try_unwrap` failure path does a full `to_vec()` (up to 4 MB) on the emu thread under the big mutex after resize/first-frame/any missed spare recycle (`present/mod.rs:330-350`). The zero-copy protocol depends on subtle three-component refcount choreography (`last_uploaded`, `last_presented_pixels`, `spare_buffers`). Change direction: make the frame slot a double-buffered `ArcSwap`-style slot with deterministic hand-back (or move presentation wholesale out of the mutex per C1 and make the pool presenter-owned).

**C4 🟡 GDI DIB path still copies per call.** `SetDIBitsToDevice`/`StretchDIBits` read the guest buffer row-by-row via `mem_read` into a per-call `vec![0u8; stride]` with no `host_slice` fast path (`gdi32/dib.rs:369-390`). Games that flip frames through GDI (Doom-style ports pre-SDL) pay this per frame.

### D. WinAPI surface & coverage

**D1 🔴 Missing import = death.** A call through an unresolved soft slot ends in `bail!("unsupported WinAPI call: …")` (`dispatch_table/mod.rs:192`) → worker `ExitThread(1)` / session stop. There is no soft-failure policy for the long tail; with 510 dense APIs + string extras against real user32/kernel32's thousands of exports, *any* new app is one rare call away from a hard stop. "Any app of any kind" requires policy inversion: default = warn-once + documented benign return (per-API-class), hard stop only for APIs on a known-dangerous list. Keep the `inspect --winapi-map` census as the coverage gate, add a runtime "first-call" telemetry so real apps populate the gap list automatically.

**D2 🔴 Game subsystems absent, not partial.** Audio: no host audio backend anywhere; `dsound.dll` is not in `is_winapi_library` → treated as a guest DLL → load fails; waveOut is a no-op ack (`winmm.rs:339-455`). Input: no XInput/DirectInput/RawInput. D3D11/DXGI: nothing. Games are blocked before performance even matters. Sequencing: audio first (a game without sound is unshippable, and winmm/waveOut + DirectSound primary-buffer semantics cover the retro-game corpus), then raw input/XInput, then D3D9 completeness (Reset/TestCooperativeLevel/materials/lights/queries/VS3-PS3), then D3D11.

**D3 🟡 COM breadth.** `CoCreateInstance` returns `REGDB_E_CLASSNOTREG` for unregistered CLSIDs — no real COM servers (shell links, shell views, DirectX objects-for-config). Needed for installers and many frameworks; low risk to defer until apps demand specific CLSIDs.

**D4 🟡 Scheduler/timing fidelity.** `timeBeginPeriod` accepted but not honored in wait resolution; `SetThreadAffinityMask`/priority stubs (see A5); QPC is solid.

### E. Observability

**E1 🟠 No end-to-end FPS/pacing harness.** The nightly bench measures only JIT hot paths (`nightly-bench.yml:20-21`); lock waits and `wake→present` P95 exist only as manual `WIE_RUNTIME_PROFILE` captures, and the ADR-0001 validation harness (P95 < 16.6 ms under 40 s capture) has never been attached to CI. Every wave below should land with a committed baseline (`docs/baselines/` protocol already exists).

---

## 3. The three architecture decisions

### Decision A — How rendering leaves the emu thread

**Option A1: Command capture + dedicated render thread (software raster kept).**
WinAPI draw handlers (GDI blits/fills, D3D9 Draw*/state changes) stop rasterizing; they append commands + state deltas into a per-device triple-buffered command stream (SPSC, latest-wins per Present). A dedicated host render thread drains the stream, owns the backbuffer and the publish path, and publishes frames that the (already separate) wgpu presenter uploads. Present = "commit stream + bump frame slot".
- ✅ Directly removes A3/A1(render part)/C1 from the emu thread; software renderer stays the correctness oracle (repo's stated strategy); benefits GDI and D3D9 at once; moderate, incremental effort.
- ❌ Still CPU-bound — raster wall time now competes for cores instead of stealing emu-thread time; needs state-diff capture discipline (Doom-era engines churn render state per draw); adds one frame of latency (acceptable at 60 fps).

**Option A2: GPU offload — translate D3D9 to wgpu/Metal.**
Draw calls map to wgpu render pipelines; VS/PS 2.0-3.0 shader tokens translate (hand-written → naga WGSL or MSL); textures stream on Lock/UnlockRect; the software renderer remains as the per-draw fallback and test oracle.
- ✅ The only path to genuinely high FPS on modern titles; removes the CPU raster cost entirely; matches the long-run plan in status.md ("GPU offload — software renderer stays the correctness oracle").
- ❌ Large, risky: shader translation fidelity, state-group caching (wgpu pipeline creation is expensive — must cache per state-diff), texture round-trips through guest memory, the honesty-bar semantics (`D3DERR_INVALIDCALL`) must survive. Not incremental.

**Recommendation: A1 now, A2 as the follow-on.** A1 is the architectural unlock both paths need anyway (the command stream A1 introduces is exactly the interface A2 consumes). Sequenced this way, A2's risk is confined to "how commands execute", not "how commands are captured".

### Decision B — How to break the big mutex

**Option B1: Incremental sharding.** Extract domains in game-hotness order: `Present` (self-contained; also frees the UI thread), then `D3D9` + `Gdi`, then `Window`/USER32, leaving `KernelState` (threads/sync) as the last shared lock. Each shard takes its own lock; cross-domain operations (present z-order needs window hierarchy) are resolved by snapshotting under the source lock and applying under the target (or by moving the z-order cache into Present, which is where it already conceptually lives).
- ✅ Proven pattern in-repo (heap shard, message-queue split); reversible; each extraction is independently shippable and measurable via the existing `LockWaitStats` instrumentation.
- ❌ The global `active` TID register (A2) must be fixed first or during the first extraction; cross-domain invariants need explicit ownership decisions; risk of lock-order bugs (document and enforce a lock hierarchy).

**Option B2: Actor/dispatcher model.** All WinAPI state owned by one dispatcher thread; guest threads submit requests and park on replies.
- ✅ Removes lock contention by construction; natural marshaling point for cross-thread SendMessage and COM.
- ❌ Turns every API stop into a cross-thread message (latency up on the exact path that is already hot — 74k+ stops in a boot); a near-total rewrite of the runtime's dispatch model; kills the "no lock across run_until_stop" invariant in the other direction (the dispatcher becomes the bottleneck — it is one thread).

**Recommendation: B1.** The measured data says the lock is held too long, not that coordination itself is the problem; sharding attacks hold time directly. Revisit B2 only if per-domain locks still serialize (e.g. one global Window lock remains contended after sharding).

### Decision C — The JIT throughput path

**Option C1: Finish ADR-0002 as specced.** Fixed native block ABI (x19–x28 = guest GPRs, x14 = rflags carrier), chain via `b`/`br` keeping live regs in place, spill only on dispatcher miss/SMC/host stop; rflags as SSA i1 booleans consumed directly.
- ✅ 2-3× on the dominant fixed cost; already designed, reviewed, and accepted; reversible via the existing gate; enables bigger compilation units (traces) to compound.
- ❌ Touches every lowering's prologue/epilogue and the trampolines; the epoch machinery (B5) must be respected — a chain hop that no longer round-trips the ctx must still observe invalidations (the per-hop `invalidate_gen` check already exists, `emit.rs:171-197`, and survives).

**Option C2: Multi-module parallel compile.** Per-worker `JITModule` + runtime-resolved chain slots (G3 spike), batched module commits.
- ✅ Attacks boot wall time (the visible "any app" pain: 38 s Doom Retro boot), independent of C1.
- ❌ Wider SMC/invalidation surface; more code-cache memory; needs the cross-module invalidation test before any default flip (perf-plan-2 risk note).

**Option C3: Bigger compilation units / trace compilation.**
- ✅ Amortizes B1-style overheads further; data-driven via the G5 chaining counters.
- ❌ Orthogonal; only worth its complexity after C1 lands (C1 changes what a boundary costs, which changes the trace-vs-block calculus).

**Recommendation: all three, in that order.** C1 (throughput) → C2 (boot) → C3 (polish). Do not start C3 before C1.

---

## 4. Sequenced roadmap

Ordered by dependency, each wave independently shippable with a committed baseline.

**Wave 0 — truth (days).**
- Drift audit: every ADR/status "landed" claim vs code; fix the docs or file the gap (ADR-0001, ADR-0002 confirmed false-positive as of this review).
- Attach an end-to-end FPS harness to nightly: a D3D9 game-loop micro (exists: `gui_d3d9.exe`) reporting guest FPS, `wake→present` P95, big-lock wait guest/presenter, emu-vs-render time split. Everything later gates on this.

**Wave 1 — unbreak the render coupling (the "JIT and rendering at the same time" fix).**
1. Extract `Present` out of `WinApiState` (own lock, presenter-owned pool; `take_frame` stops touching the big mutex) — unblocks the UI thread immediately (C1, C3).
2. Complete ADR-0001: 64-px pitch pad + reused upload scratch (C2).
3. D3D9 latest-wins publish coalescing + optional emu-side Present pacing (A4).
4. GDI DIB `host_slice` fast path (C4).
- Acceptance: on the D3D9 loop micro, presenter-side lock wait ≈ 0; steady-state zero main-thread pack copies at arbitrary widths; guest FPS no longer drops when the window is animating.

**Wave 2 — rendering leaves the emu thread (Decision A1).**
Command capture for D3D9 draws + GDI blits/fills into a triple-buffered stream; dedicated render thread executes + publishes. Per-draw `.to_vec()` eliminated (stream writes borrow from guest memory under the arena pin).
- Acceptance: emu thread spends ≤1 ms/frame on D3D9 handlers at 1080p mid-scene; render thread utilization visible in the profile; frame pacing holds 60/60.

**Wave 3 — JIT throughput (Decision C1, then C2).**
1. ADR-0002 direct-register ABI + SSA rflags (B1).
2. Multi-module parallel compile + cross-module invalidation test (B3).
3. A `JitShared::mem` write-lock narrowing + arena-pinned raw slices (B4) — before or with Wave 2's borrow discipline.
- Acceptance: `long_loop` ≤ 0.10 s (ADR-0002's own target); Doom Retro boot ≤ 10 s; no stale-chain regressions in the micro matrix.

**Wave 4 — correctness for "any" binary (B2, D1).**
1. Degrade-not-die: unknown mnemonic → iced fallback + slow-path counter (policy change), never a session kill for memory-safe mnemonics.
2. Histogram-driven mnemonic batches: SSE3/SSSE3/SSE4.1/4.2, 64-bit div/idiv in JIT, minimal x87 (fld/fstp/fadd/fmul/fdiv/fcom/fnstcw/fldcw + `_ftol` via SSE).
3. Missing-import soft-failure policy with first-call telemetry (D1).
- Acceptance: the real-exe corpus (7za, notepad, doomretro, +3 new apps) launches with zero `unsupported`/`unimplemented` terminations; gap telemetry feeds `missing-winapi-handlers.md` automatically.

**Wave 5 — the systems a game needs (D2, A5-A8).**
1. Audio: host CoreAudio sink + winmm waveOut real + DirectSound (primary/secondary buffers, no hardware acceleration semantics) — retro corpus covered.
2. Raw input + XInput; `GetKeyState`/`GetAsyncKeyState` vkey table already exists to build on.
3. Per-thread message queues + marshaled `SendMessage` (A7).
4. Event-driven waits via `WaiterRegistry` for kernel objects/CS/pthread (A6); host timer thread for multimedia timers (A8).
5. Time-sliced quanta + Win32 priority → macOS QoS mapping (A5).
- Acceptance: a render+audio+main-thread game shows < 1% poll-wake CPU and sub-ms wake latency; audio continuous under load; input latency < 1 frame.

**Wave 6 — GPU offload for D3D9 (Decision A2) and D3D11/DXGI.** Consume the Wave-2 command stream with wgpu pipelines + shader translation; software renderer demoted to per-draw fallback/oracle. Then start D3D11/DXGI on the same architecture.

**Non-goal to re-decide:** "any app of any kind" currently collides with two declared non-goals: 32-bit PE (WoW64) and full Windows compatibility. If "any app" includes 32-bit installers/launchers/legacy games, that is a scope decision to make explicitly — the JIT's register model and the PE loader are both affected; everything above is invariant to that decision.

---

## 5. Key files touched per painpoint

| Painpoint | Files |
|---|---|
| A1/C1 present shard | `wie-winapi/src/present/mod.rs`, `state/mod.rs`, `wie-runtime/src/session/{window,mod}.rs`, `mt_runtime.rs` |
| A2 active register | `wie-winapi/src/kernel32/thread.rs`, `state/mod.rs`, all handler call sites reading `current_tid()` |
| A3/Wave 2 command capture | `wie-winapi/src/d3d9/{device,draw}.rs`, `d3d9_render/*`, `gdi32/{blit,dib}.rs`, new `render_thread` module in `wie-cli` |
| A4 pacing/coalescing | `d3d9/device.rs`, `present/mod.rs`, `wie-cli/src/gui/app.rs` |
| B1 ADR-0002 | `wie-cpu/src/jit/{pipeline,engine,lower/*,emit,config}.rs`, `trampolines.rs` |
| B3 multi-module | `jit/shared.rs`, `jit/config.rs`, `cache_persist.rs` |
| B2 ISA | `jit/block.rs`, `jit/lower/sse*.rs`, `exec/mod.rs`, `exec/sse.rs`, new `exec/x87.rs` |
| B4 mem lock | `jit/cpu_engine.rs`, `mem/{mod,rw,mmap_arena,arena}.rs` |
| C2 ADR-0001 | `present/mod.rs`, `wie-cli/src/gui/present_wgpu.rs` |
| D1 soft-fail | `dispatch_table/mod.rs`, `hooks.rs`, `session/pump.rs` |
| A6/A7/A8 | `wake.rs`, `sync_obj.rs`, `user32/message/*`, `winmm.rs`, `session/pump.rs` |
