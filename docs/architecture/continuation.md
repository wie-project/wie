# Continuation: where the campaign stands, and the next slice for each open item

Date: 2026-11-25. Branch `feat/dll-coverage`. HEAD at the time of writing:
**`66a5fe6`**, pushed, working tree clean. Since the first draft of this file
(`99d11e3`) the session landed the startup-latency record (`2afc82f`), the guest-I/O
arena mirror removal (`7ffc1f6`), the `PageMap::runs` lock (`7aba5e7`), four docs
records (`4b72ae7`), the probe counter and cache-before-compile path (`e809d3b`), the
fault-record RUNBOOK correction (`ca91223`), the bounded re-resolve in `mem/rw.rs`
(`420c395`), the profile-line probe count (`99d11e3`), this record (`c577135`,
`c2ad72f`), and the two slices §2.3 and §2.5 describe (`399161c`, `66a5fe6`).

This file exists so the next session starts from measurements rather than from a
conversation. Everything below is either a number that was measured or an
instruction that names the file and the acceptance test.

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
`EventObject::wait`'s timeout branch returned `true` whenever `wait_timeout`
returned spuriously (`if timed_out { false } else { true }`, never re-reading
`signaled`) — a `WAIT_OBJECT_0` the guest did not earn. That was a real bug and it
is fixed (`66a5fe6`, deadline loop + three tests), but **it was never the cause of
this fault class** — do not read that commit as re-opening the hypothesis. The
faults are invalid *guest* accesses (§2.5), not missed wakes.

### 2.3 Block-parameter materialisation — the largest remaining emission lever

`docs/architecture/prologue-epilogue-census.md` **corrects** a figure three
slices were aimed at. The 275-per-exit does not reproduce: 174/165/172 of the
337 is *cold* probe-miss code charged to every exit by an attribution whose own
wording said "once per exit *or on a probe miss*". Executed entry+exit is
**197.6**, and over `hops/entries = 955 156 362 / 127 786 895 = 7.47` that is
**26.2 host instructions per iteration (11 %)**, not 45 (21 %).

**The classification is done** (the same doc's §8-§10, committed with the fix):
of the 52.6 materialisation instructions per entry+exit, **68 % (35.8) are dead on
entry** — the block loads a GPR it never reads, because the self-loop path filled
`live_eff` with `true` instead of carrying the real live-in set. 24 % (12.4) are
genuinely loop-carried, 4 % are pure spill→reload pairs, 2 % a flag word, 2 %
read-but-never-written.

The fix is **`399161c`** — delete `live_eff.fill(true)` for self-loops, and drop
`|| self_loop` from the shared exit block's GPR store predicate so a self-loop
writes back exactly what it loaded. Measured acceptance in the census's own unit:
**52.6 → 14.9 host instructions per entry+exit (−72 %, −5.0 per iteration), latch
frame traffic 15.6 → 4.5 per iteration, executed loop body 244.7 → 203.3
(−16.9 %)**, with same-block CLIF opcode counts identical for every guest-visible
op (`icmp` 50/50, `band`, `bxor`, `ishl`, `ushr`, `isub`, `select`, `imul`, `call`,
`uextend`, `bor`, `brif`, `return`) and only `iconst`/`iadd`/`load`/`store`
falling. It gated clean on its own tree: `clippy --workspace --all-targets`, `fmt`,
`check --workspace --all-targets`, `wie-cpu` **544 release** (543 of 544 dev, the
one failure the churn sensor in §2.5), and three new dual-engine tests that compare
the whole register file, RFLAGS and RIP against iced.

What that leaves on the table is the other 32 % of the 52.6 — the genuinely
loop-carried 12.4 plus 4 % of pure spill→reload — which is now visible per block
rather than buried under the dead 35.8, and is the next thing to look at with the
self-loop path no longer hiding it.

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

### 2.5 Landed, and what only looks done

The two slices that were dirty at `c577135` are committed, each gated on its own
tree with its own `CARGO_TARGET_DIR`:

* **`399161c`** — `flags_ssa`. A self-loop's header carries only the GPRs the body
  reads; the exit stores back only those it loaded. Measured over the three SDL2
  blocks: **52.6 → 14.9 host instructions per entry+exit (−72 %, −5.0 per
  iteration)**, latch frame traffic 15.6 → 4.5, executed loop body 244.7 → 203.3.
  CLIF opcode counts identical for every guest-visible op.
* **`66a5fe6`** — `perf`. `EventObject::wait` is a deadline loop that re-checks
  `signaled` (a wake is not a signal), plus per-run worker-fault bookkeeping and a
  `RuntimeStop` when `ExitProcess{code:0}` would have masked a dead worker.

Nothing is left uncommitted as of `66a5fe6`; `git status` is clean. Gate counts on
those trees: `wie-cpu` **544 release / 543 of 544 dev**, `wie-winapi` **1401**,
`wie-runtime` **191**, clippy clean, fmt clean. `trace.rs` and `mt_runtime.rs` had
to travel together — with `trace.rs` left out, clippy reports
`reset_worker_faults is never used`, which is how the pairing was discovered rather
than assumed.

**The `RuntimeStop` branch has no witness yet.** `mt_contention.exe 4 64 40` is
10/10 exit 0 with no fault line, and 6 further runs under `WIE_JIT_MEM=slow`
produced **0 fault lines**, so the branch that turns a faulting run into a failure
has never run for real. It is verified by construction and by the suites staying
green. The next slice should be a guest that faults a worker on purpose, so the
behaviour has a test that fails when the branch is deleted — the acceptance for
`perf`'s work, still owed. The earlier evidence for the mechanism it defends stands
and is worth keeping: on n=20 at `4 64 40`, 14 runs carried a verdict, **21 of 21
fault lines printed `reprobe=ok`** (page mapped at the fault site), dominant class
the return-address push of a `call` (`rip=0x140001373`, write, size 8, `value=0x140001376`),
14 of 21 — and **12 of those 14 faulting runs exited 0**, the exit-code blindness
this branch closes.

**The churn sensor now has a measured failure rate instead of a reputation.**
`mem::tests::a_peer_churn_of_another_region_never_faults_a_stable_access` passes
4 of 8 isolated runs on the pre-slice tree (`c577135`) and 2 of 8 on `399161c`,
Fisher p ≈ 0.31 — so it fails on its own account, not because of nearby edits. It is
one assertion over **two independent causes**, and the failure string distinguishes
them:

```
read: mem_read generation changed at 0x30000000 (concurrent map/free, 3 attempts)
read: mem_read unmapped 0x30000000
```

The first is attempt exhaustion: the bound is tuning, and 3→8 was tried and
reverted. The second string carries **no generation change at all**, so no attempt
count can cure it — that is the incoherent-resolve / `ArenaSet` shape, and it is the
one worth a slice. Observed failure this session: `6 of 200000 accesses faulted`,
`read: mem_read unmapped 0x30000000`.

### 2.6 The order to take them in

Each line names the item, the file it lives in, and the thing that has to be true
before it can be called done. No item is done until its acceptance has been run.

1. **A guest that faults a worker on purpose** — new file under `micro-exes/src`,
   plus a harness case. Acceptance: the run reports non-zero and names the fault,
   and the test fails if `trace.rs`'s `RuntimeStop` branch is deleted. This is the
   only owed acceptance from `66a5fe6` (§2.5).
2. **The churn sensor's two causes, split** — `crates/wie-cpu/src/mem/tests.rs`.
   Acceptance: two counters instead of one assertion, and the failure fraction at
   the parent stated as a fraction over n=20. Then re-point the `generation unmoved`
   counter at `ArenaSet` (§2.5).
3. **`ArenaSet::arenas` under the same tear as `PageMap`** —
   `crates/wie-cpu/src/mem/arena.rs`. **Do not lock blind**: the test that fails at
   HEAD first, as `pagemap.rs:202` did, or stop and record why.
4. **`regions=` beside `jit: compiles`** — `pipeline.rs`, then the remaining
   instrumentation gaps in §2.4. Cheap, and it makes the region path's frame effect
   measurable rather than inferred.
5. **The remaining 32 % of block-parameter materialisation** — `lower/mod.rs`. It is
   visible now that the dead 68 % is gone; the census's unit (host instructions per
   entry+exit) is the measurement (§2.3).

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

# the arm that used to fault oftenest; use it to try to make the RuntimeStop
# branch fire, because 4 64 40 no longer faults at all (10/10 clean, 0 fault lines)
WIE_JIT_MEM=slow ./target/release/wie run micro-exes/out/mt_contention.exe 4 64 40 < /dev/null

# console startup, process wall and phases
WIE_RUNTIME_PROFILE=1 ./target/release/wie run micro-exes/out/crt_hello.exe

# the code cache, cold then warm against one cache dir
WIE_JIT_CODE_CACHE=1 WIE_CACHE_DIR=/tmp/cc ./target/release/wie run micro-exes/out/crt_hello.exe
```

**The base binary is gone.** `/tmp/wie-head/target/release/wie` (commit `0cdf6fe`) was
deleted during the end-of-session cleanup, so a frame A/B has to recreate it first:

```sh
git worktree add --detach /tmp/wie-head 0cdf6fe
(cd /tmp/wie-head && CARGO_TARGET_DIR=/tmp/wie-head/target cargo build --release -p wie-cli)
```

**Frame A/Bs were off for the whole session** — five lanes compiling means no
20-second quiet gap exists. The last valid paired figure stands at **3.67× median
(2424/2480 vs 593/742) at `4183676`** and must never be presented as current for
a later HEAD.
