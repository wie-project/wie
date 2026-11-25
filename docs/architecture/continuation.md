# Continuation: where the campaign stands, and the next slice for each open item

Date: 2026-11-25. Branch `feat/dll-coverage`. HEAD at the time of writing:
`99d11e3`. This file exists so the next session starts from measurements rather
than from a conversation. Everything below is either a number that was measured
or an instruction that names the file and the acceptance test.

**Read `docs/RUNBOOK.md` before touching a knob**, and `cpu-and-memory.md` before
touching `crates/wie-cpu/src/mem/`. Both carry correctness records that were paid
for in lost rounds.

---

## 1. Status of the five campaign items

| item | status | the number that closes it |
|---|---|---|
| (a) startup latency | **met, on the process clock** | `wie run micro-exes/out/crt_hello.exe` **8.79 ms min / 9.13 ms median** of process wall, n=10 alternating. Session is 3.86-4.31 ms of that 9.13; the rest is dyld + clap + teardown. |
| (a) the "sometimes 500 ms" | **attributed, irreducible** | The same binary with **cold image pages** costs **207-282 ms** median (n=5); warm, 13-19 ms. 10 853 856-byte image + 14 dylibs. Page-in vs code-signature split is an **estimate** (no privileged purge was run). |
| (b) app identity | **done** (`88f8a6c`) | Title observed through real winit on `notepad.exe`; Dock NSImage pixels asserted 48×48/96×96; name fallback (FileDescription → ProductName → exe stem) tested on two real binaries. **Rendered Dock tile explicitly not verified.** |
| (c) flag cost on the blit | **closed on evidence** | Current blit iteration is 239 host instructions with 35 flags (14.6 %), PF 0/iteration, pack 0/iteration, probe guard 3 per access / 6 per iteration. AF deferral rejected (239/245/251), OF/CF carrier negative EV. The attributed share is ~21 % of emitted code and ~**37 % per iteration of the scaled-blit block** — never quote a raw 38 %. |
| (d) tier-0 emitter | **closed as a refutation** | Cranelift measured at **374 µs median/block** (185 min, p90 588, n=1001), so a hand emitter is ≤1.5× on 16.3 % of blocks. No emitter written, and that is the finding. |
| (e) WinAPI fast paths | **closed as unprofitable in this workload** | API boundary is **0.1 % of wall** (90 ms of handler time against 72.7 s of emulation). Records landed instead. |

---

## 2. Open items, in priority order

### 2.1 The `mt_contention` fault class — mostly fixed, one residue

**Fixed at `420c395`** (`mem/rw.rs`): the data path now re-resolves across a
mapping-generation change, with `check_access` re-run *per attempt* so a
protection change can never become a success. Acceptance measured, n=30 per arm
on the sharp mask `mt_contention.exe 4 64 40`:

    before (pre-fix rw.rs)   20 / 30 runs with faults, 22 faults, 5 nonzero rc
    after                    0 / 30, twice; and 0 / 30 on `4 64 c0` and on defaults

**Still open, four things, none of them understood:**

1. **The new churn test has two distinct flake causes, and only one of them is
   tuning.** `mem::tests::a_peer_churn_of_another_region_never_faults_a_stable_access`
   fails intermittently (seen by the `flags_ssa` lane as pass/fail/fail/pass across
   identical isolated runs, and by the author in the gate worktree). Measured, from
   the failure strings themselves:

   * `read: mem_read generation changed at 0x30000000 (concurrent map/free, 3 attempts)`
     — **attempt exhaustion, curable.** Raising `DATA_ACCESS_ATTEMPTS` from 3 to 8
     was tried for the experiment and reverted; it is one-line with the arithmetic in
     the doc comment rather than a guess.
   * `read: mem_read unmapped 0x30000000` — **no generation change in the string at
     all**, i.e. the refusal came from a resolve that saw the map unmoved. **No
     attempt count can cure that**, and it is the incoherent-resolve /
     `ArenaSet::arenas` shape. This is the one that matters.

   Which of the two appears depends on scheduling, so one assertion conflating them
   makes a green run unreadable. **Split the sensor before trusting it:** one counter
   for `generation changed` refusals (must be zero at the justified bound), a second
   for refusals with the generation unmoved. Then measure the failure fraction at the
   parent over n=20 and report it as a fraction. The earlier "1.7 % per attempt" is
   **inferred from one sample** and should be treated as a hypothesis, not a rate.
2. **One `rc=1` with no fault line** in the `4 64 c0` arm (rep 14 of 30). A
   different class from the faults; unattributed.
2. **`ArenaSet::arenas`** (`crates/wie-cpu/src/mem/arena.rs`) is a plain
   `Vec<MmapArena>` whose readers (`find_va`, `read`, `write_ptr`,
   `page_data_ptr`) use `&self` while VM mutators rebuild it under `&mut self` —
   the same shape as the `PageMap` tear fixed at `7aba5e7`. **Do not lock it
   blind.** The `PageMap` fix was justified by a test that failed at HEAD
   (`debug_assert_eq!` firing with `left: 131072 / right: 131072`); write that
   test for `ArenaSet` first, and if it does not fail, stop — a tear nobody can
   show is not worth 300 lines of guard plumbing.
3. **A silent dead worker used to look like success.** On the sharp arm all ten
   failing runs exited **0**: the verdict killed a worker, the guest never
   noticed. So on this class a clean exit code is **not** evidence of a clean
   run — pass means the `wiegui` verdict line is absent. `perf`'s change makes a
   dead worker fail the run non-zero with the fault named; that is the fix for
   the *reporting*, and it changes what "green" means for every suite.

### 2.2 The lost-wake hypothesis: refuted, do not re-open

`mt_contention`'s 5 s timeout is a **symptom**. In a failing run there is exactly
one `SetEvent` (the primary's own `g_start`) and no worker ever pings, because
two workers are already dead. `EventObject::set` holds the lock across
mutate-and-notify and the registry token travels through a buffered channel, so
there is no gap to close. The one real defect found there is in passing:
`EventObject::wait`'s timeout branch returns `true` when `wait_timeout` returns
spuriously (`sync_obj.rs`, `if timed_out { false } else { true }` without
re-checking `signaled`) — a `WAIT_OBJECT_0` the guest did not earn. That is
`perf`'s, and it wants a four-line loop plus a test.

### 2.3 Block-parameter materialisation — the largest remaining emission lever

`docs/architecture/prologue-epilogue-census.md` **corrects** a figure three
slices were aimed at. The 275-per-exit does not reproduce: 174/165/172 of the
337 is *cold* probe-miss code charged to every exit by an attribution whose own
wording said "once per exit *or on a probe miss*". Executed entry+exit is
**197.6**, and over `hops/entries = 955 156 362 / 127 786 895 = 7.47` that is
**26.2 host instructions per iteration (11 %)**, not 45 (21 %).

**The classification is done** (the same doc's §8-§10, uncommitted at handover):
of the 52.6 materialisation instructions per entry+exit, **68 % (35.8) are dead on
entry** — the block loads a GPR it never reads, because the self-loop path filled
`live_eff` with `true` instead of carrying the real live-in set. 24 % (12.4) are
genuinely loop-carried, 4 % are pure spill→reload pairs, 2 % a flag word, 2 %
read-but-never-written.

A two-hunk change already implements the fix and is **uncommitted**: delete
`live_eff.fill(true)` for self-loops, and drop `|| self_loop` from the shared exit
block's GPR store predicate so a self-loop writes back exactly what it loaded.
Measured acceptance in the census's own unit: **52.6 → 14.9 host instructions per
entry+exit (−72 %, −5.0 per iteration), latch frame traffic 15.6 → 4.5 per
iteration, executed loop body 244.7 → 203.3 (−16.9 %)**, with same-block CLIF
opcode counts identical for every guest-visible op (`icmp` 50/50, `band`, `bxor`,
`ishl`, `ushr`, `isub`, `select`, `imul`, `call`, `uextend`, `bor`, `brif`,
`return`) and only `iconst`/`iadd`/`load`/`store` falling. It reports 541/541 dev
and the release CLI linking clean, and **owes `clippy` and `fmt`** — that is the
first gate to run before committing it.

### 2.4 Instrumentation gaps that are still gaps

* **`jit: compiles` lumps blocks and regions** (85 = 1 block + 84 regions in a
  DOOM pre-render burst), so a reader must reconstruct the split from the log;
  and `jit_eager_sites: region_fold=` reads 0 against 84 regions because it
  counts the eager-fold site, not regions compiled. Needs a `regions=` field
  beside `compiles=`, in `pipeline.rs`.
* **The iced fallback's per-instruction cost is unquantified** — it is 0.21 % of
  render-phase instructions and no counter says what one costs. Bounded at ~2 %
  of host work, which is **not asserted** anywhere.
* **Inline probe-guard executions have no counter.** Only probe *misses* reach a
  helper, so any per-instruction guard cost comes from the static census (15-16
  host instructions per memory operand), not from a runtime count.
* **`nomod` in `unpersistable_silent` is implemented and not test-triggered** —
  it needs an engine whose tier module was never created, which the install path
  cannot produce without manufacturing engine state. Recorded as a claim.

### 2.5 Uncommitted work in the tree at the time of writing

The tree was handed over dirty on purpose rather than reverted. Per-file sha256,
so the next session can tell whether the file moved:

```
3e64ab9d…  crates/wie-cpu/src/jit/lower/mod.rs                  flags_ssa  (the two-hunk fix, §2.3)
53595e01…  crates/wie-cpu/src/jit/tests/mod.rs                  flags_ssa  (one `mod` line)
38843ab5…  crates/wie-cpu/src/jit/tests/param_liveness_tests.rs flags_ssa  (new, 3 dual-engine tests)
76cdfedb…  docs/architecture/prologue-epilogue-census.md        flags_ssa  (§8-§10: the breakdown, the invariants, the acceptance)
f30aa180…  crates/wie-runtime/src/mt_runtime.rs                 perf       (loud fault + 2 fields)
941da142…  crates/wie-winapi/src/sync_obj.rs                    perf       (wrong wake)
7237bdde…  crates/wie-winapi/src/kernel32/sync.rs               perf       (WIE_MT_DEBUG SetEvent trace)
3027a3e3…  crates/wie-runtime/src/session/profile.rs            perf       (access_type surface)
53ec03ff…  crates/wie-runtime/src/trace.rs                      perf
```

None of it is gated. Gate it in a worktree of `99d11e3` with its own
`CARGO_TARGET_DIR` before committing, and expect `nextest -p wie-cpu` to be
**538 dev / 540 release** depending on which lane's tests are present — the
count moves with the tree, so state it rather than assuming it.

**Read this before committing `perf`'s set.** Four of the five files are
instrumentation and one is a behaviour change:

* `access=`, `value=` and `reprobe=` on the worker-fault line, plus an
  `mt_worker_faults=<n> first: <detail>` profile line emitted only when a fault
  happened. Measured on `4 64 40` n=20: 14 runs carried at least one verdict, 21
  fault lines, and **21/21 printed `reprobe=ok`, 0 `reprobe=err`** — the page was
  mapped at the fault site every time, which is the mechanism this whole class
  rests on (a live page reported unmapped), now measured at the fault point rather
  than inferred. Dominant class: the return-address push of a `call`
  (`rip=0x140001373`, write, size 8, worker stack, `value=0x140001376`), 14 of 21.
  **12 of those 14 faulting runs exited 0**, which is the exit-code blindness
  described in §2.1.
* `EventObject::wait`'s timeout branch is now a deadline loop that re-checks
  `signaled`, so a spurious condvar wake cannot return `WAIT_OBJECT_0`. Three new
  tests, and they were proven to fail against the old code by temporarily
  reintroducing the wrong-wake (`left: Some(true), right: None`) and restoring the
  file byte-identically. `nextest -p wie-winapi` **1401 passed** with it.
* **`crates/wie-runtime/src/trace.rs` is the least verified thing in the tree.**
  It turns `ExitProcess { code: 0 }` *plus* a recorded worker fault into
  `RuntimeStop("<detail>; ExitProcess would have reported success")`. That is the
  right behaviour and it changes what a micro run reports, but it is **unmeasured**:
  no `-p wie-runtime` suite run and no `mt_contention 4 64 40` n=10 acceptance was
  taken after it. If an existing runtime test asserts exit 0 on a faulting guest,
  that test is the first thing to reconcile.

---

## 3. Discipline that was paid for

* **One gate per worktree, one `CARGO_TARGET_DIR` per worktree.** A shared target
  dir once silently reused another worktree's artifacts and a commit went in
  whose every cargo invocation died on `E0599`.
* **A gate worktree must have fixtures.** `wie-runtime`'s suite **fails by
  design** without `micro-exes/out` and `real_exes` populated (`tests/common/mod.rs`
  says so): 45 GUI tests failed in a worktree where they were absent, which is
  how the requirement was confirmed. `rsync -a --ignore-existing
  micro-exes/out/ <worktree>/micro-exes/out/` plus a `real_exes` symlink.
* **Lanes do not run git.** One lane committed its own one-line doc fix; it was
  correct, and kept, but an ungated commit is only safe by luck about its parent.
* **Quote the counter, not the clock.** Three provenance errors in this campaign
  were clock-derived (a capture time derived from `wall_ms`; a 44 s vs 131 s
  comparison across builds; a "20 s lag" that was 0.11 s when observed). If you
  did not observe the event, it is not a number.
* **A test that has never failed has demonstrated nothing.** Both fixes in the
  `mem/` layer were accepted on a toggle that fails at the parent commit, and
  each toggle is quoted verbatim in its commit.
* **Read the hunk after any scripted edit.** A regex once deleted `set_fault` and
  turned every fault into a silent success; another scripted edit corrupted a
  file header. Both cost a round.

---

## 4. Reproduction

```sh
# the harness that found the torn read, and the arena tear candidate
cargo nextest run -p wie-cpu -E 'test(a_peer_free_another_region)' --no-capture
cargo nextest run -p wie-cpu -E 'test(a_peer_churn_of_another_region)' --no-capture

# the fault class, sharp arm first (it fails ~2x as often as the full mask)
./target/release/wie run micro-exes/out/mt_contention.exe 4 64 40 < /dev/null
./target/release/wie run micro-exes/out/mt_contention.exe 4 64 c0 < /dev/null

# console startup, process wall and phases
WIE_RUNTIME_PROFILE=1 ./target/release/wie run micro-exes/out/crt_hello.exe

# the code cache, cold then warm against one cache dir
WIE_JIT_CODE_CACHE=1 WIE_CACHE_DIR=/tmp/cc ./target/release/wie run micro-exes/out/crt_hello.exe
```

**Frame A/Bs were off for the whole session** — five lanes compiling means no
20-second quiet gap exists. The last valid paired figure stands at **3.67× median
(2424/2480 vs 593/742) at `4183676`** and must never be presented as current for
a later HEAD.
