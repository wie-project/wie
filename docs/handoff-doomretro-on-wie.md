# Handoff: Doom Retro on WIE — remaining crash (startup `s_VERSION` parse)

Date: 2026-08-21. Repo: `/Users/yanis/Programming/wie` ("WIE" — a Windows emulator, JIT Cranelift + iced-x86 fallback, runs real Windows PE binaries incl. SDL2.dll as guest code; macOS host via winit/wgpu). Release binary: `./target/release/wie` (rebuild with `cargo build --release`, ~1–2 min). Shell: macOS fish. Doom Retro is `real_exes/doomretro/` (doomretro.exe v6.3, real SDL2.dll, doomretro.wad, freedoom1/2.wad); a bottle `doomretro` exists under `~/Library/Application Support/WIE/bottles/` with the WADs staged at `.../drive_c/Program Files/DoomRetro/`.

This document is the complete context for a FRESH agent session to finish the bug. Everything below is verified — do not re-derive it.

---

## 1. Goal

Make Doom Retro boot past its startup handshake under WIE. The current blocker is a WIE-side mis-execution of Doom Retro's config/DEHACKED parser that makes the guest's fatal version check fire (or fault) — NOT a wad problem.

## 2. Repro commands and current behavior

### 2a. Headless micro run (deterministic, fast-ish)
```fish
WIE_RUNTIME_PROFILE=1 timeout 100 ./target/release/wie run --max-api 20000000 \
  --bottle doomretro --app-dir real_exes/doomretro \
  real_exes/doomretro/doomretro.exe -iwad freedoom2.wad
```
Current: after a slow bounded emulated busy-loop (~40 no-hook slices, RIP `0x14012D3E4`), the guest prints EXACTLY:
```
The wrong version of C:\Program Files\DoomRetro\doomretro.wad was found.
```
then exits `-1` (`Error: run_micro: exit=4294967295`). That message is Doom Retro's fatal `I_Error` from `d_main.c:2863`.

### 2b. GUI windowed run
```fish
env RUST_LOG='info,cranelift_jit=off,cranelift_codegen=off,wiegui=debug,wie_runtime=debug' \
  WIE_RUNTIME_PROFILE=1 timeout 120 ./target/release/wie run \
  --bottle doomretro --app-dir real_exes/doomretro --gui \
  real_exes/doomretro/doomretro.exe -iwad freedoom2.wad
```
Current: boots in ~38 s, creates ~5 guest windows, publishes `wiegui: Frame event`s (present path works), then hits an **unhandled ACCESS_VIOLATION** `exc=0xc0000005`, `addr=0, access=read, size=8`, guest RIP `0x14012B951` (`doomretro.exe +0x12b951`), instruction `48 8b 00` = `mov rax,[rax]` with `rax=0`.

### The two symptoms share one root cause
The fatal `I_Error` (2a) fires because under WIE Doom Retro parses the DEHACKED lump and ends up with `s_VERSION != "DOOM Retro v6.3"`. The GUI NULL fault (2b) is the same parser path: its char-class table is NULL, so depending on the path it either crashes (fault) or corrupts the parse (`I_Error`).

## 3. Verified facts (do NOT re-derive; trust these)

- **The version check**: `d_main.c:2863` — `if (!M_StringCompare(s_VERSION, DOOMRETRO_NAMEANDVERSIONSTRING)) I_Error("The wrong version of %s was found.", resourcewad)`, where `resourcewad` is the guest path to `doomretro.wad` and `DOOMRETRO_NAMEANDVERSIONSTRING` = `"DOOM Retro v6.3"` (the exe's other `"DOOM Retro v6.3."` / `"DOOM Retro v6.3..."` strings are GUI messages — see below — not the constant). `s_VERSION` comes from the wad's DEHACKED `[STRINGS] VERSION = ...` line, parsed by Doom Retro's DEHACKED loader.
- **`doomretro.wad` is VALID and correct — do NOT patch it.** Directory entry format is `{DWORD filepos; DWORD size; char name[8]}` (note: filepos FIRST, name LAST — earlier parses that assumed name-first produced garbage). lump[0] = `DEHACKED` at pos `0x0c` (offset 12), size `0x603b` (24635), content starts `[STRINGS]\r\nVERSION = DOOM Retro v6.3\r\nD_DEVSTR = Development mode ON...` (hexdump confirmed: file begins `50 57 41 44 | ed 01 00 00 | 90 1a 7b 00 | [STRINGS]...`). numlumps=493, dir@`0x7b1a90`, dir_end == file size (`0x7b3960`) — self-consistent.
- **The exe constant matches**: `strings real_exes/doomretro/doomretro.exe | grep 'DOOM Retro v6'` → `DOOM Retro v6.3` (the real const; `v6.3.` / `v6.3...` are message strings like `"The release notes for DOOM Retro v6.3..."` and `"DOOM Retro v6.3... is incompatible with E1M10"`). So on real Windows the check passes; under WIE it fails ⇒ WIE-side bug.
- No symbols / PDB for the exe (checked). Use `/opt/homebrew/opt/llvm/bin/llvm-objdump --triple=x86_64-pc-windows-msvc -d --start-address=X --stop-address=Y real_exes/doomretro/doomretro.exe`.
- Doom Retro is a fork of Chocolate Doom (source: github.com/bradharding/doomretro; note current master is v6.4, the shipped build is v6.3 — read the v6.3 tag / git history if consulting source).

## 4. The precise crash mechanism (parser NULL table)

From the 2b fault dump (RuntimeStop diagnostic; extended regs captured):
- Faulting function `doomretro.exe 0x14012B90C`:
  ```
  14012b946: 49 8b 00            movq  (%r8), %rax        ; rax = [r8]  (r8 = &obj+0x18)
  14012b949: 81 fd 00 01 00 00    cmpl  $0x100, %ebp
  14012b94f: 77 0b                ja    0x14012b95c
  14012b951: 48 8b 00             movq  (%rax), %rax      ; FAULT: rax==0 → read [0]
  14012b954: 0f b7 04 78          movzwl (%rax,%rdi,2), %eax   ; u16 char-class lookup
  ...
  ```
- Caller loop (`0x140127658`–`0x14012769F`) reads input bytes from a stream structure (`end@0x8`, `pos@0x10`) and per char calls `0x14012B90C` with `rcx=char, rdx=8, r8=&obj+0x18` (the `lea 0x18(%r13), %r8` at `0x140127692`).
- Fault-time registers: `r8=0x207fde08`, `[0x207fde08]=0` ⇒ **the object `obj = r13 = r8-0x18 = 0x207fddf0` is a STACK object of the caller and its `+0x18` member (the u16 char-class table pointer) is 0 — it was never written.** On real Windows that member holds a `.rdata` table address set during the object's construction. Under WIE the construction/init did not complete.
- Consequence: fix = find the constructing/store instruction that should write a `0x140000000+…` address into `obj+0x18` of this stack object, determine which WIE execution/loader behavior prevents it (prime suspects: a mis-emulated instruction in Doom Retro's early arg/config/DEHACKED parsing, or a PE `.data`/`.rdata` section content/relocation issue), fix that WIE mechanism, regression-test, verify both repros.

## 5. How to root-cause it (recommended next steps for the fresh agent)

1. Reproduce 2a and 2b to confirm current behavior (yes, the fault is deterministic in 2b).
2. Get a fuller register dump if needed: the RuntimeStop diagnostic in `crates/wie-runtime/src/session/mod.rs` `invalid_memory_diagnostic` (~line 517) reads `rax/rcx/rdx/r8/r9` (engine getters in `crates/wie-cpu/src/lib.rs` ~438-476; `read_rbx/read_r12/read_rsp` exist; `read_r13/r14/r15/rdi/rsi/rbp` do NOT — adding getters means touching all `CpuEngine` impls: iced, jit, tests). The fallback is a tagged (unique, e.g. `[DRX]`) temporary log at `crates/wie-runtime/src/quantum.rs` ~:455 (the unhandled-fault arm) printing `exception_code`, `invalid_memory.address`, `engine.read_rip()` — that prints the faulting RIP reliably (the earlier hand-captured RIP came from this trick).
3. **Write-watchpoint (the decisive step):** stack addresses are deterministic in WIE (0x207fdd20 etc. were identical across runs). Add a temporary tagged check in the guest write path that logs any store targeting `obj+0x18 = 0x207fde08` (or the object range `0x207fddf0..0x207fde20`): store RIP + value. Guest stores from compiled JIT code bypass `engine.mem_write` — hook the iced interpreter's store handler and/or the JIT store slow path (`crates/wie-cpu/src/jit/lower/mem.rs`, and `crates/wie-cpu/src/iced_cpu.rs` mem store). Simplest robust variant: run the repro with `WIE_CPU=iced` (pure interpreter; slow but the parse happens early) so ALL stores funnel through the iced store code, and log the tagged line there. If NO store ever hits `+0x18` before the fault ⇒ constructor/call flow never reached the init (a WIE instruction/call bug upstream, e.g. an early parse or `memcpy/memset` mis-result). If a store DOES hit it with a wrong value (0, or a truncated constant) ⇒ WIE produced that value (image data / relocation / instruction).
4. Wherever the wrong value comes from, fix the WIE mechanism minimally and add a regression test at a correct seam (PE-load/relocation or interpreter-op test as appropriate; existing test patterns in `crates/wie-winapi/src/state/tests/`, `crates/wie-runtime/tests/micro_*`).
5. Success criteria:
   - 2a: the guest must NOT print `The wrong version of ...doomretro.wad was found.` — report what it does instead (it may then exit via another `I_Error` — capture that message too and iterate).
   - 2b: no ACCESS_VIOLATION at `0x14012B951`; ideally it proceeds to a rendered frame (host window + `take_frame` evidence), otherwise report the next stage honestly.
6. Cleanup: remove ALL tags (`[DRX]` etc.) — grep them; `cargo build --release`; full `cargo test` MUST pass (the micro-exe suite is behavioral and must stay green).

## 6. Constraints & context for the fresh agent

- **Do NOT touch** (other work may be in flight / already landed there): gdi32 `enumerate.rs`/`font_system.rs`/text-metric/textout, user32 `display.rs`/`blit`/`dib`, `dispatch_table/decl.rs`+`names/mod.rs`, `guest_io_host.rs`, `kernel32/file_io/*`, CLI (`wie-cli`), JIT config/pipeline hotness changes. Scope your fix to the loader/runtime/interpreter/image-load area — wherever the root cause lands.
- The working tree has many uncommitted changes from prior fixes (see `git status`). Do not revert them. Notable landed fixes (all `cargo test`-green at their time): `bottle run` unified with `run --bottle` (shared `run_entry`), WAD self-truncation fix (`stage_wad_payload` `same_host_dir`, session/init.rs), 11 missing WinAPI handlers, buffered-file mirror dirty-flag fix (guest_io_host/file_io), DEVMODE offsets + software-render present wiring (`SetDIBitsToDevice`/`StretchDIBits` → present lane), JIT eager-compile for long one-shot blocks (`WIE_JIT_EAGER_BLOCK_INSNS`, default 48), `EnumDisplayMonitors` guest-callback bridge, font-enumeration zero-copy `FontRef` metrics (EnumFontFamiliesExW 2474→132 ms), text-engine zero-copy first-load (~15.9→8.6 ms), and the eager `%format!` removal in the callback log (pump.rs).
- Release build is REQUIRED for meaningful repro timing (debug is ~5–10× slower and can time out before the fault).
- The background-subagent channel has been unreliable for long tasks (repeated lost/empty reports). Do the work directly with small, self-reverted instrumentation, and ALWAYS write a final report to disk (e.g. append a section here or `docs/handoff-doomretro-on-wie.md`).