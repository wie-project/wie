## 7-Zip console status (`7za`)

WIE emulates a **Windows PE64** standalone console 7-Zip (`7za.exe` from the official **7-Zip Extra** package). That is **not** a native macOS `7za`/`7z` binary and **not** the GUI (`7zFM` / full installer).

- **`real_exes/` is gitignored** — do not expect `7za.exe` in a clean clone; obtain the Windows PE yourself (steps below).
- Guest files use a **bottle**: host `{root}/drive_c/...` ↔ guest `C:\...` (`--root` / `WIE_ROOT`).

### What works (verified with 7-Zip Extra **26.02** `x64/7za.exe`)

| Guest command        | Meaning                          | Default JIT | `WIE_CPU=iced`          |
| -------------------- | -------------------------------- | ----------- | ----------------------- |
| `--help` / `help`    | Usage                            | OK `exit=0` | OK                      |
| `i`                  | Formats / codecs / hashers       | OK          | OK                      |
| `a -mmt1 -bd …`      | Create `.7z` (LZMA2, 1 thread)   | OK          | OK                      |
| `a -mmt2` / `-mmt4`  | Multi-thread create (CRT MT)     | OK          | OK                      |
| `l …`                | List archive                     | OK          | OK                      |
| `x -mmt1 -bd -y -o…` | Extract                          | OK          | OK (SHA matches source) |
| `x -mmt2` / `-mmt4`  | Multi-thread extract + roundtrip | OK          | OK                      |

7za multi-thread paths use **`msvcrt!_beginthreadex`**, events, and semaphores — the same generic WinAPI/CRT surface as the MT micros (not a 7za special case).

Recommended flags: **`-bd`** (no progress), **`-y`** on extract. Raise **`--max-api`** for real tools (`200000`–`500000`); micros keep the low default.

### Obtain Windows `7za.exe` (any Mac)

WIE needs the **x64 PE** from **7-Zip Extra** (standalone console). Root `7za.exe` in that package is **32-bit** — use **`x64/7za.exe`**.

| Path in Extra archive | Arch              | Use with WIE?                  |
| --------------------- | ----------------- | ------------------------------ |
| `7za.exe`             | Windows **x86**   | No                             |
| `x64/7za.exe`         | Windows **x64**   | **Yes** (this is what we run)  |
| `arm64/7za.exe`       | Windows **ARM64** | No (WIE is x86-64 guest today) |

**One-time setup** (run from the WIE repo root):

```bash
./scripts/fetch.sh 7za
```

Homebrew `p7zip` is only a **bootstrap** to unpack the Extra archive; the guest under WIE is still the **Windows** PE.

Without Homebrew: open `7z*-extra.7z` anywhere you can, then copy **`x64/7za.exe`** into this repo’s `real_exes/` (still gitignored).

### Universal test payloads

Generate inputs on the fly (no personal `Downloads/` files, works in CI):

| Payload                | How                        | Purpose                      |
| ---------------------- | -------------------------- | ---------------------------- |
| Tiny text              | `echo '…' > hello.txt`     | Fast create / list / extract |
| Binary blob (~256 KiB) | Python deterministic bytes | LZMA2 + SHA-256 roundtrip    |
| Optional extra         | `cp /any/local/file …`     | Stress only; not required    |

### Universal example (create / list / extract / MT / SHA roundtrip)

One bottle, synthetic payloads only (no personal files). After `real_exes/7za.exe` is present:

```bash
cargo build -p wie-cli --release
CLI=./target/release/wie
PE=real_exes/7za.exe
test -f "$PE" || { echo "missing $PE — install Windows x64 7za (see above)"; exit 1; }

BOTTLE="${TMPDIR:-/tmp}/wie-7za-bottle-$$"
APP="$BOTTLE/drive_c/App"
mkdir -p "$APP"
cp -f "$PE" "$APP/7za.exe"

echo 'hello from wie bottle' > "$APP/hello.txt"
python3 -c "
from pathlib import Path
app = Path(r'''$APP''')
data = bytes((i * 17 + 31) & 0xFF for i in range(256 * 1024))
(app / 'blob.bin').write_bytes(data)
print('blob.bin', len(data))
"

# Help + codec inventory
$CLI run --root "$BOTTLE" --max-api 100000 "$PE" -- --help
$CLI run --root "$BOTTLE" --max-api 100000 "$PE" -- i

# Create: single-thread + multi-thread (CRT _beginthreadex path)
$CLI run --root "$BOTTLE" --max-api 500000 "$PE" -- \
  a -mmt1 -bd 'C:\App\hello.7z' 'C:\App\hello.txt'
$CLI run --root "$BOTTLE" --max-api 500000 "$PE" -- \
  a -mmt2 -bd 'C:\App\blob.7z' 'C:\App\blob.bin'
$CLI run --root "$BOTTLE" --max-api 500000 "$PE" -- \
  a -mmt4 -bd 'C:\App\blob4.7z' 'C:\App\blob.bin'

# List
$CLI run --root "$BOTTLE" --max-api 100000 "$PE" -- l 'C:\App\blob.7z'

# Extract + SHA-256 roundtrip (mmt2)
rm -rf "$APP/out" && mkdir -p "$APP/out"
$CLI run --root "$BOTTLE" --max-api 500000 "$PE" -- \
  x -mmt2 -bd -y -o'C:\App\out' 'C:\App\blob.7z'
SRC=$(shasum -a 256 "$APP/blob.bin" | awk '{print $1}')
OUT=$(shasum -a 256 "$APP/out/blob.bin" | awk '{print $1}')
echo "src=$SRC out=$OUT"
test "$SRC" = "$OUT" && echo "ROUNDTRIP OK" || { echo "ROUNDTRIP FAIL"; exit 1; }

# Backend A/B (optional)
WIE_CPU=iced $CLI run --root "$BOTTLE" --max-api 500000 "$PE" -- \
  a -mmt2 -bd 'C:\App\blob_iced.7z' 'C:\App\blob.bin'
WIE_CPU=jit  $CLI run --root "$BOTTLE" --max-api 500000 "$PE" -- \
  a -mmt2 -bd 'C:\App\blob_jit.7z'  'C:\App\blob.bin'

rm -rf "$BOTTLE"
```

**CLI shape:**

```text
wie-cli run --root <bottle> --max-api N real_exes/7za.exe -- <7za-args...>
```

**Minimal smoke (text only, single-thread):**

```bash
cargo build -p wie-cli --release
test -f real_exes/7za.exe || { echo "install x64 7za.exe first"; exit 1; }
B=$(mktemp -d) && mkdir -p "$B/drive_c/App" && \
  cp real_exes/7za.exe "$B/drive_c/App/" && \
  echo hi > "$B/drive_c/App/hello.txt" && \
  ./target/release/wie run --root "$B" --max-api 200000 real_exes/7za.exe -- \
    a -mmt1 -bd 'C:\App\h.7z' 'C:\App\hello.txt' && \
  ./target/release/wie run --root "$B" --max-api 200000 real_exes/7za.exe -- \
    x -mmt1 -bd -y -o'C:\App\out' 'C:\App\h.7z' && \
  cat "$B/drive_c/App/out/hello.txt" && rm -rf "$B"
```

### Implementation notes (why real tools work)

1. **`SBB` flags** — MSVC COM `QueryInterface` uses `cmp` + `sbb r,r` + `sbb r,-1`. Wrong CF when `src+CF` overflowed the operand width picked the wrong interface → null call.
2. **`VirtualAlloc(NULL, size, MEM_COMMIT)`** — treated as **RESERVE|COMMIT** (Windows/Wine-compatible) for LZMA2 buffers.
3. **JIT dual_super GPR writeback** — only store live GPRs on block exit (do not zero callee-saved).
4. **Default `WIE_JIT_SUPER=loop`** — non-loop stack super can host-fault on some real tools (`7za a`); self-loop super keeps `long_loop` fast. Opt in with `WIE_JIT_SUPER=all` only when bisecting.
5. **CRT/WinAPI MT** — `_beginthreadex`, semaphores, events, `WaitForMultipleObjects`, save-before-switch on the shared engine (see Multithreading above).
6. **`WIN32_FIND_DATA` layout** — sizes at 28/32, `cFileName` at **44** (not BY_HANDLE offsets); wrong names made `7za a` recurse forever on empty path components.

### Not claimed yet

- GUI 7-Zip / `7zFM` / full installer PE.
- Password / crypto, solid multi-file update, or every format beyond default `.7z` LZMA2.

### Heavy / multi‑GiB `7za a` (local stress)

Small and medium create/list/extract (README universal example, including ~140 KiB payloads) **work** under default JIT. Prefer compact method switches where noted below.

**Fixed (was a false “heap too small” failure):** `WIN32_FIND_DATA{A,W}` used the wrong field offsets (`cFileName` at 48 instead of **44**, sizes mixed up with `BY_HANDLE_FILE_INFORMATION`). Every FindFirst/Next name looked empty → 7za recursive scan looped, burned millions of tiny `malloc`s, then `std::bad_alloc` / `_CxxThrowException`. Correct layout + `FILETIME` fill; scan is now O(entries), not OOM.

**Still open / caveats**

| Topic                          | Notes                                                                                                                                                                                                                                                                          |
| ------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| Multi‑GiB tree via `--drive-d` | Not claimed end-to-end yet. Raise `--max-api` and optionally `WIE_PROCESS_HEAP_MB`; use `WIE_RUNTIME_PROFILE=1` / `WIE_JIT_CHAIN=0` / `WIE_CPU=iced` to bisect.                                                                                                                |
| C++ EH                         | MSVC `_CxxThrowException` → host FuncInfo (exact IP→state, ThrowInfo type match, `dispCatchObj`, UnwindMap guest actions, catch **funclet CALL** + RAX continuation). `-md=64k` prints error and **exits cleanly** (no host fault). Mingw LSDA / `cpp_*` micros still partial. |

**Example stress command** (local only; not CI):

```bash
B="/tmp/w7z_heavy_$$" && mkdir -p "$B/drive_c/App"
time env WIE_RUNTIME_PROFILE=1 ./target/release/wie run \
  --root "$B" \
  --drive-d "/path/to/large_tree" \
  --max-api 20000000 \
  real_exes/7za.exe -- \
  a -mmt4 -mx=1 -md64k -bd 'C:\App\huge_cache.7z' 'D:\'
rm -rf "$B"
```

