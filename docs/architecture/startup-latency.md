# Startup latency: what a hello-world costs, and what the "500 ms" really is

Date: 2026-11-25. Branch `feat/dll-coverage`. Binary measured: release build of
`88fe696` (release profile as shipped, `strip = true`). HEAD moved while this was
written (to `b55c92a`, `jit/cache_persist`, another lane); nothing in these numbers
depends on it, because `88fe696` and its successors are content-identical on the
console startup path.

Measurements were taken on a host where other lanes were compiling at the same
time. Load is printed at both ends of every series below. Phase timings are
load-insensitive (they are interval timers inside one process), but absolute
`init_ms` values drift by several ms between series on the same binary, which is
why the before/after comparisons here are **paired and interleaved**, never two
lists taken one after the other.

---

## 0. TL;DR

1. **Read this first: the `stage_wad_payload` fix documented here does not help on
   `wie run`.** The CLI stages the app into the bottle before the session starts, so
   `same_host_dir` is true, the 214-entry scan never runs, and `vfs-wad-stage` is
   0.10 ms on that path — before and after the fix alike. The 1.05 → 0.22 ms win
   applies **only** to runs where the exe lives outside the bottle. Anything quoting
   that win as a CLI-visible improvement is wrong; see §5.
2. **A console hello-world costs 8.79 ms (min) / 9.13 ms (median) of process
   wall**, `wie run micro-exes/out/crt_hello.exe`, n=10 alternating, load
   3.22 / 7.05 / 11.24 at both ends. That is **process** wall, not session wall.
   Under the 20 ms target with room.
3. **The user's "sometimes 500 ms" is cold executable pages, not startup code.**
   The same binary costs **207–513 ms** with cold image pages and **13–19 ms**
   warm. Splitting page-in from code-signature validation is an **estimate**; no
   privileged cache purge was run. See §2.
4. Largest remaining init phase: **`precompile` 0.83–0.95 ms** — one call,
   `engine.precompile_at(image_summary.entry_point_va)` at `init.rs:758`.
   Flagged, not cut: it is `wie-cpu`'s territory, and the code cache that would make
   it cheap writes files but restores nothing (`restored=0` on runs 2 and 3), which
   is the code cache's open question, not startup's. See §6.

---

## 1. The path a user takes: process wall vs session wall

Both numbers for the same command, same binary:

| clock | value | n | load at both ends |
|---|---|---|---|
| process wall (`wie run …crt_hello.exe`) | **8.79 ms min / 9.13 ms median** (warm max 15.9) | 10 | 3.22 / 7.05 / 11.24 |
| session wall (`wall_ms=` in the profile) | 3.86–4.31 ms | 3 | 3.22 / 7.05 / 11.24 |

So ~4 ms is session and ~5 ms is outside it: dyld + clap `Command` tree + teardown,
attributed in an earlier slice (clap construction 3.71 ms of a `--version` run, and
clap lives in `wie-cli`, which is not this lane's). Earlier in this campaign the
same command measured 23.35 ms process against 13.36 ms session; the delta has
closed from both ends since.

For reference, the failure mode this replaced: `wall_ms` included `emu_ms` mostly
spent with the guest thread parked on background compiles (`bg_stalls` was 104 in
the first profile taken). It is now `bg_stalls=0 bg_stall_us=0` on every run below.

---

## 2. The "sometimes 500 ms": cold image pages

Same command, same guest, same working directory; the only difference is whether
the executable's pages are in the page cache. "Cold" = a **fresh copy of the same
binary** run once, so its inode has no cached pages. n=5 per arm, load
3.35 → 4.21, binary = `/tmp/wie-cand` (build of `adb4ed2` + this slice, sha256
`ca05242e…`).

| condition | process wall, 5 runs (ms) | min | median |
|---|---|---|---|
| warm binary | 18.6, 17.1, 63.5, 15.0, 13.3 | 13.31 | 17.11 |
| fresh copy each run | 297.5, 207.6, 513.2, 282.7, 207.3 | **207.34** | **282.75** |

The same shape appears as the first run of otherwise-warm series: 337.9 ms for one
of ten `wie run`s of `88fe696`, 343.7 ms for one of ten of the pre-fix binary, and
0.28 s in a five-run `/usr/bin/time` loop.

**What this means for the complaint.** "Sometimes wie takes 500 ms" is that row,
and no amount of init-phase work moves it: init is ~2.8 ms of the 9 ms warm path,
and 0 of the ~200–500 ms cold path. The image is 10,853,856 bytes with 14 linked
dylibs.

**Estimate, not a measurement:** the split between page-in and macOS code-signature
validation of a newly-copied image is an **estimate**. Separating them requires
purging the page cache (`purge`, root) which was not run.

`/usr/bin/time -p` cannot resolve this: its resolution is 10 ms, so a warm run
prints `real 0.01`. All process-wall figures here come from a monotonic-clock
harness around the same spawn.

---

## 3. The phase table, `wie run`, `crt_hello`

`WIE_RUNTIME_PROFILE=1 wie run micro-exes/out/crt_hello.exe`, three consecutive
runs, load 3.22 / 7.05 / 11.24. Values are min–max across the three; the profile
line prints one run.

| phase | ms (min–max of 3) | what it covers |
|---|---|---|
| `pe-read-parse` | 0.18–0.20 | read the PE, parse headers/sections/imports |
| `backend-open` | 0.07 | CPU backend construction |
| `image-load+protects` | 0.38–0.43 | map image, apply section protections |
| `stub-planting+accelerators` | 0.12–0.14 | plant fake-API stubs, install runtime hooks |
| `precompile` | 0.89–0.93 | **one** call: compile the PE entry block (§6) |
| `stack-map` | 0.01 | guest stack |
| `teb+crt+env-addrs` | 0.01 | TEB, CRT pointers, env block addresses |
| `vfs-env+config` | 0.00 | `VolumeConfig::from_parts` + env knobs |
| `vfs-identity` | 0.00 | cmdline/identity strings (pure formatting) |
| `vfs-host-to-guest` | 0.05–0.07 | `host_path_to_guest` → `normalize_host_path` → `canonicalize_host_target` |
| `vfs-wad-stage` | 0.10–0.12 | `stage_wad_payload` (alias branch only on this path, §5) |
| `identity-strings+crt-argv` | 0.00 | argv formatting into guest memory |
| `env-block` | 0.00 | env block copy |
| `heap+resource-maps` | 0.74–0.82 | guest heap + resource maps |
| `regions-registered` | 0.00–0.01 | region table |
| `regions+env` | 0.00 | region/env coupling |
| `winapi-state` | 0.01 | WinAPI shared state |
| `seed-skeleton` | 0.06–0.07 | seed the run skeleton |
| `winapi-state+session` | 0.11–0.12 | session construction |
| **sum** | **2.78–2.92** (`init_ms` 2.78–2.92) | |

Sample report lines, verbatim (run 2 of 3):

```
wall_ms=3.86  cpu_user_ms=3.30  cpu_sys_ms=0.72  cpu%≈104.0
emu_ms=1.02 (97.1%)  handler_ms=0.03 (2.8%)  resolve_ms=0.00 (0.1%)  total_accounted_ms=1.05  init_ms=2.78
init_phases: pe-read-parse=0.18 backend-open=0.07 image-load+protects=0.38 stub-planting+accelerators=0.14 precompile=0.89 stack-map=0.01 teb+crt+env-addrs=0.01 vfs-env+config=0.00 vfs-identity=0.00 vfs-host-to-guest=0.05 vfs-wad-stage=0.10 identity-strings+crt-argv=0.00 env-block=0.00 heap+resource-maps=0.74 regions-registered=0.01 regions+env=0.00 winapi-state=0.01 seed-skeleton=0.06 winapi-state+session=0.12 | sum_ms=2.78
jit: insns=14 iced=458 compiles=1 skip=0 bg_compiles=0 bg_stalls=0 bg_stall_us=0 bg_fallback=0 cache_hits=1 load=1 store=1
```

**Provenance.** The `init_phases:` line is one report line in
`crates/wie-runtime/src/session/profile.rs` fed by `phase(...)` marks placed by
hand in `crates/wie-runtime/src/session/init.rs`; both are in the tree (`88fe696`
and earlier). The four `vfs-*` rows were split out of a single
`identity+vfs-resolve` phase that was never attributed below its own total.

The `vfs-*` **micro-path** costs are lower on `wie run` than the numbers they were
derived from, because that path takes a different branch (§5). The figures the
split was measured on, under the `microh` runtime harness (`/tmp/wie-h`, a
throwaway crate outside the tree) with `bottle_root = None` and the exe **outside**
the bottle, n=7, load 9.84 → 9.84, of `init_ms` 7.12 / 7.55:

| sub-stage | min / median (ms) | share of the phase |
|---|---|---|
| `vfs-env+config` | 0.00 / 0.00 | <1 % |
| `vfs-identity` | 0.01 / 0.01 | <1 % |
| `vfs-host-to-guest` | 0.10 / 0.12 | 9 % |
| `vfs-wad-stage` | **1.05 / 1.21** | **91 %** |

---

## 4. `stage_wad_payload`: what the 91 % was, and the fix

`stage_wad_payload` (`crates/wie-runtime/src/session/init.rs`) copies `.wad` game
data from the host exe's directory into the guest-visible directory and
materializes the `freedoom1.wad → DOOM1.WAD` / `freedoom2.wad → DOOM2.WAD` aliases.
To find the `.wad` files it walked `read_dir(host_exe_dir)` and called
`src.is_file()` — a full `stat` — on **every** entry.

Measured composition, directory `micro-exes/out` (214 regular files, **0** `.wad`,
0 symlinks, 0 subdirs), python `os` calls, min of 3:

| operation | cost |
|---|---|
| `readdir` (214 entries) | 0.246 ms |
| 214 × `is_file()` (`stat`) | 0.735 ms |
| one `stat` | 3.3 µs |
| **total** | **~0.98 ms** = the whole 1.05 ms sub-stage |

So: not one syscall per bottle lookup, and not ~30 unavoidable stats — one stat per
**directory entry**, on a directory that contains no WAD at all, for a program that
never opens one.

**The fix** (18 lines, `init.rs`): take the entry type from `DirEntry::file_type()`,
which answers from the `d_type` in the dirent and costs no syscall on APFS, and keep
the follow-`stat` only for symlinks — the lone case where `is_file` (follows) and
`d_type` (does not) disagree. The set of files copied is unchanged. Committed as
`88fe696`; `init.rs` content sha256
`a2c2717463c172cdb4bdecf4d9c750f475dc50866b62637183cc2ebd70a6cbf8`.

### Paired before/after

Both arms rebuilt from the same worktree, run **interleaved** (alternating, so
drift hits both), n=10 pairs, load 17.68 → 17.68:

| | before min / median | after min / median | paired delta |
|---|---|---|---|
| `vfs-wad-stage` | 2.03 / 2.14 ms | 0.71 / 0.86 ms | **−1.30 ms median, 10/10 pairs faster** (worst −1.09, best −1.77) |
| `init_ms` | 12.41 / 13.25 ms | 11.15 / 11.98 ms | −1.37 ms median, 8/10 pairs faster |
| every other phase | — | — | within ±0.08 ms (noise) |

A second interleaved series (n=10, load 13.74 → 15.13) agreed: `vfs-wad-stage`
2.14 → 0.83 ms median, vfs subtotal 2.60 → 1.46 ms, no other phase outside ±0.08.

On a quiet host the same phase is smaller in absolute terms (n=5, load
9.08 → 9.08): `vfs-wad-stage` 0.22 / 0.24 ms, vfs subtotal 0.29 / 0.33 ms,
`init_ms` 3.26 / 4.97 ms. The phase is ~5 % of init on `wie run` (§5) and was
15–20 % of it on the micro path before the fix.

### Byte-identical staging proof

Direct behavioural check, both arms, `WIE_ROOT=/tmp/wadbottle`, a copy of
`crt_hello.exe` beside a fake `freedoom1.wad` (content
`IWAD-fake-payload-1234567890`) and a `readme.txt`:

| file in the bottle `drive_c` | before | after |
|---|---|---|
| `freedoom1.wad` | sha `a90dc2df…` | sha `a90dc2df…` |
| `DOOM1.WAD` (alias) | sha `a90dc2df…` | sha `a90dc2df…` |
| `readme.txt` | not copied | not copied |

Identical set, identical content, and non-WAD entries are still skipped. The
`same_host_dir` case-variant guard (which prevents copying a WAD onto itself and
truncating it to zero) is untouched by the change.

Unit tests: `cargo test --release -p wie-runtime --lib -- session::init` → 10 passed,
0 failed, including `stage_wad_payload_copies_wads_with_freedoom_aliases` and
`stage_wad_payload_does_not_self_copy_wads_across_case_variants`. `cargo fmt -p
wie-runtime -- --check` clean; `cargo clippy --release -p wie-runtime --all-targets`
exits 0 with no `wie-runtime` warnings (the single warning in that run is `wie-cpu`'s
`needless_return` in `set_fault`, another lane's).

---

## 5. The caveat that matters most: the win is not on the `wie run` path

`wie run` stages the application folder into the bottle **before** the session
starts. When the session then computes `stage_wad_payload`'s destination, the host
exe directory and the guest-visible directory are the **same physical directory**,
`same_host_dir` returns true, the function takes the alias-only branch, and the
214-entry scan never executes. `vfs-wad-stage` is 0.10–0.12 ms there (§3), before
and after the fix alike.

End-to-end proof, n=10 alternating, plain `wie run micro-exes/out/crt_hello.exe`,
load 3.22 / 7.05 / 11.24 both ends:

| series | binary | min | median |
|---|---|---|---|
| A (load 3.22 / 7.05 / 11.24) | `88fe696` (with the fix) | 8.79 ms | 9.13 ms |
| A (load 3.22 / 7.05 / 11.24) | `adb4ed2` + the fix (earlier build) | 8.76 ms | 9.05 ms |
| B — load 3.36 / 7.98 / 13.07 (different, busier series; not comparable row-to-row) | `adb4ed2` (pre-fix) | 9.40 ms | 10.03 ms |

Series A is one interleaved run, so its two rows are comparable: paired median
delta +0.07 ms, i.e. `0d50c22` (a `wie-winapi` change) did nothing here. The
fix's end-to-end contribution **on this path is not measurable** and should not be
claimed as such — series B's pre-fix figure is quoted only to show the same ~9–10 ms
landscape, not as a before/after. Its value is confined to runs whose exe sits
outside the bottle (the micro path, harness runs, and any future non-staging launch
path), where it is −1.30 ms paired (§4).

---

## 6. What is left, and the `precompile` question

Ranked, from §3 (`wie run`, `crt_hello`):

| what | cost | owner / status |
|---|---|---|
| `precompile` | **0.83–0.95 ms** (one cold first-run read 1.62) | `wie-cpu` — flagged, not cut |
| `heap+resource-maps` | 0.74–0.82 ms | hand-off: lazy-mapping the 512 MiB shadow heap; scales with mapped size |
| outside the session (dyld, clap, teardown) | ~5 ms of a 9.13 ms process | clap already handed to `wie-cli`; dyld is the floor |
| `image-load+protects` | 0.38–0.43 ms | not investigated below the phase |
| everything else | ≤0.20 ms each | — |

`precompile` is now literally one call — `engine.precompile_at(image_summary.entry_point_va)`
at `init.rs:758` — after the earlier slice removed the loop that precompiled every
planted guest stub. It compiles the PE entry block before the guest runs. How much of
its 0.83–0.95 ms is first-use Cranelift backend setup versus the compile itself is
**not measured** — that split needs a mark inside `precompile_at`, which is
`wie-cpu`'s file; the split is inferred from where the call sits, not observed.

**Is it worth a slice? Not yet, and the reason is narrower than "the cache is inert".**

The mechanism that would make it cheap is JIT code-cache persistence. Three runs
against one `WIE_CACHE_DIR`, as originally taken:

| run (same `WIE_CACHE_DIR`) | `jit_code_cache` | `precompile` |
|---|---|---|
| 1 (cold) | `restored=0 refused=0 unpersistable=0 hit_rate=0%` | 1.62 ms |
| 2 | `restored=0 refused=0 unpersistable=0 hit_rate=0%` | 0.86 ms |
| 3 | `restored=0 refused=0 unpersistable=0 hit_rate=0%` | 0.83 ms |

**Those zeros are the knob-off line, not a measurement of misses.** The report line
is emitted unconditionally, and `WIE_JIT_CODE_CACHE` is opt-in (`1`/`true`/`on`/`yes`;
anything else, including unset, is off) — `diag.rs`'s
`code_cache_line_reports_zeros_without_the_knob` asserts exactly this string. So the
table above says "the feature was off", which is why `precompile` does not move.

**And the files in that directory are not machine code.** `$WIE_CACHE_DIR/jit/*.bin`
(two files, 8 KB by `du` — 63 and 62 bytes of content) is the *metadata ledger*,
`PersistentJitCache`, which by design stores no code: it answers "this block earned an
eager compile", never "here is the code". The code cache writes into a different
directory, `$WIE_CACHE_DIR/jit/code/<key>.code`, and only with the knob on.

**With the knob on, the entry block is still compiled every run — because it is never
offered to the cache.** Measured on `crt_hello` with a fresh cache dir and
`WIE_JIT_CODE_CACHE=1`: run 1 writes `jit/code/<key>.code` (4 298 B, and since
`compiles=1` that blob *is* the entry block), and run 2 still reports `restored=0`.
The reason is on the path, not in the counters: `precompile_at` →
`try_compile` → `decode` → `try_compile_region` → `try_compile_from_kind` has **no
code-cache probe anywhere on it**, and the only probe site in the crate is
`pipeline.rs`'s `restore_or_probe`, inside the dispatch **miss** branch. The
precompiled entry block is `Ready` from init, so dispatch returns before any probe.
Control on the same binary and knob: `7za i` restores 51 then 52 with
`refused=95/32`, so the blob format, validation and counters all work — the entry
block is simply never asked.

**The real open question is therefore not "is one block worth a cache hit" but "the
entry block is never offered to the cache, and nothing counts whether an offer was
made".** `refused=0 unpersistable=0` is consistent with both "nothing to restore" and
"nobody looked", which is the gap a `probes=` counter would close. Whether replaying
a persisted 4 298-byte blob beats compiling it is **unmeasured** — there is no timing
on the restore path, and the counters cannot supply one.

Also worth knowing before anyone reads `restored=0` as a defect: the knob is off by
default for a *correctness* reason (replaying captured host code), so a win here
exists only for users who set it. It is not a reason to flip the default for a
0.9 ms phase.

**The one measurement that would change this answer:** a run where
`jit_code_cache restored>0` *and* `precompile` falls below ~0.5 ms — i.e. first offer
the entry block to the cache, then re-measure. If restore cannot be made to pay for
this shape, the remaining lever is Cranelift backend setup cost inside
`precompile_at` — `wie-cpu`'s file, and the setup-vs-compile split would need a mark
inside it to be measured.

No new knobs were added for any of this; everything is measured from a normal run
under `WIE_RUNTIME_PROFILE=1`.

---

## 7. Reproducing

```sh
# process wall. `/usr/bin/time -p` CANNOT resolve this (10 ms resolution, so a warm
# run prints `real 0.01`) — the figures above come from a monotonic-clock wrapper
# around the same spawn, which was a throwaway in /tmp and is not in the tree. Wrap
# the spawn yourself, and discard the first run: it is the cold-page case of §2.
for i in $(seq 1 10); do ./wrap-monotonic target/release/wie run micro-exes/out/crt_hello.exe; done

# session + phases
WIE_RUNTIME_PROFILE=1 ./target/release/wie run micro-exes/out/crt_hello.exe 2>&1 | grep -E 'wall_ms|init_phases'

# the cold-page case: a fresh copy per run has cold image pages
cp target/release/wie /tmp/wie-cold && /usr/bin/time -p /tmp/wie-cold run micro-exes/out/crt_hello.exe
```

For phase comparisons, rebuild both arms from one worktree and **interleave** them;
two sequential lists on this host differ by more than the effects being measured.
