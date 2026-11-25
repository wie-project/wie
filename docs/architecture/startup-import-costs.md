# Startup import costs, and the load threshold below which DOOM Retro never renders

Two facts, both measured, both worth having in one place because both have
already cost somebody a wrong conclusion.

The first is about **measurement method**: on a loaded host DOOM Retro does not
reach its frame loop at all, so a run that finishes without a `frames_published`
line is not a slow run — it is a startup-only run, and every per-frame number
derived from it is meaningless.

The second is a **startup-only import profile** of the DOOM Retro guest. It is
not the per-frame census (that one is still not taken); it is what a run that
never renders still pays, and two of its entries are large enough to matter to
anyone working on startup latency.

## 1. A loaded host suppresses the frame loop entirely

Two runs, same command, different binaries, both windows `--gui`, both bounded
with `timeout -k 5 -s INT`:

| run | binary | window | host load (start → end) | `frames_published` | `gdi32!BitBlt` | retired instructions |
| --- | --- | --- | --- | --- | --- | --- |
| A | HEAD-era release build | 100 s | **18.8 → 12.5** | **absent (0)** | 1 | 14.2 B |
| B | frozen baseline `0cdf6fe` (`/tmp/wie-head`) | 60 s | **10.4** | **absent (0)** | 1 | 12.1 B |

Run B is the control that makes the point: the *frozen baseline* fails exactly
like the working tree, so the failure travels with host load, not with our code.
For contrast, the same HEAD binary published **2 424 and 2 480 frames** in
100 s windows taken at load ~2.3, and ~767-1 980 frames at load 8-12.

Practical rules that follow, and the reason this is a doc and not a note:

- **A run with `gdi32!BitBlt = 1` and no `frames_published` line is startup-only.**
  Do not rank per-frame costs from it, do not quote it as a slow frame rate, and
  do not compare it with a run that rendered.
- **`frames_published` is the discriminator**, not wall time and not instruction
  count: run A retired *more* instructions (14.2 B) than a rendering run does per
  second, because the guest spins while blocked rather than idling.
- A borderline sample is worth nothing here: unloaded the same command renders
  24 fps, loaded it renders zero. There is no useful middle.

### Caveat for any future census

Both binaries above are **older than HEAD**: the release binary used for run A
was built at 23:54 while HEAD `9d9df37` ("JIT tier-0: executable code memory")
committed at 00:31, so run A describes the tree *before* tier-0. Any import
census must record the binary's build time against `git log -1 --date`, because a
ranking taken from a stale binary silently describes code nobody is working on.

## 2. Startup-only import costs (DOOM Retro)

From run A's `top exports by handler time`, ranked by total time. All of it is
one-shot startup work — asset load, DC/DIB setup, module probing.

| import | calls | total | µs/call | note |
| --- | --- | --- | --- | --- |
| `kernel32!CreateFileW` | 34 | **49.20 ms** | **1 447** | 1.4 ms per open; the largest single startup item |
| `kernel32!ReadFile` | 3 872 | 43.72 ms | 11.3 | WAD/data streaming, 43 ms in aggregate |
| `gdi32!CreateDIBSection` | 2 | **9.59 ms** | **4 795** | 4.8 ms per call, two calls |
| `kernel32!FindFirstFileA` | 5 | 7.76 ms | 1 552 | directory scan in the probe path |
| `kernel32!CloseHandle` | 7 | 7.58 ms | 1 083 | |
| `kernel32!SetFilePointerEx` | 4 058 | 5.51 ms | 1.36 | |
| `kernel32!LeaveCriticalSection` | 15 360 | 5.17 ms | 0.34 | |
| `kernel32!EnterCriticalSection` | 15 360 | 3.79 ms | 0.25 | |
| `kernel32!GetProcAddress` | 24 | 3.36 ms | 140 | |
| `kernel32!LoadLibraryExW` | 12 | 1.73 ms | 144 | |
| `kernel32!GetEnvironmentVariableA` | 251 | 1.51 ms | 6.0 | |
| `kernel32!LoadLibraryW` | 19 | 1.40 ms | **73.7** | **first-time miss path** — see below |

### Two actionable entries for startup latency

`CreateFileW` at **1.4 ms per call over 34 calls (49 ms)** and `CreateDIBSection`
at **4.8 ms per call over 2 calls (9.6 ms)** dominate startup. Neither needs a
frame loop to investigate, and together they are ~59 ms of a startup that the
guest spends before it draws anything.

`CreateFileW` is the one to take first: 34 opens is not a plausible count for a
guest that loads nine files, so either the guest retries, or our open path is
doing work per call that could be done once (the same shape as the `LoadLibrary`
finding: 15 618 calls, all misses, 93 % of the cost in rebuilding a search list).

### `LoadLibraryW` here is *not* a regression

73.7 µs/call over 19 calls is the **first-time miss path**: the memoised search
list (`dll_loader::search_directories_cached`) only removes the cost of
*repeated* resolutions, and the attributed win was on 5 954 repeated misses
(`search` 68.91 → 9.83 µs/call, `search_dirs` 64.17 → 0.10 µs/call). Nineteen
first-time misses paying the full search is expected and does not contradict it.

### Caveats on the table

- **Host was loaded** (load 18.8 → 12.5), so the absolute microseconds are
  inflated relative to a quiet host and are not comparable with numbers taken
  elsewhere. The *ranking* within this one run is what is being reported.
- **One run, one binary**, and the binary predates HEAD `9d9df37` by ~37 minutes.
- It is **startup only**: 0 frames published, so nothing here is per-frame.
