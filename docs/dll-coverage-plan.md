# Universal DLL Coverage Plan — release 0.2

**Version**: 0.1
**Status**: Draft
**Goal**: every DLL a mainstream Windows app links — Qt-class, games, installers,
network tools — resolves its imports and behaves correctly enough to run.

## Current state (measured, 2026-08)

Dispatch-table export counts per DLL:

| DLL | Exports in dispatch | Notes |
| --- | ---: | --- |
| user32 | 188 | ~80% covered, many stubs |
| kernel32 | 139 | ~90% covered |
| d3d9 | 81 | software renderer, 51/119 device methods |
| gdi32 | 37 | real text/print/blit; specialty missing |
| version | 18 | done (RNotepad dependency) |
| comdlg32 | 13 | native panels |
| advapi32 | 12 | registry core + landed extras |
| comctl32 | 9 | status bar + 4 ImageList |
| shell32 | 7 | drag-drop + folder paths |
| winmm | 1 | timeGetTime only |
| uxtheme | 1 | SetWindowTheme no-op |

Zero-coverage DLLs (any call bails): `MSIMG32`, `IMM32`, `WS2_32`, `WININET`,
`URLMON`, `CRYPT32`, `IMAGEHLP`, `DBGHELP`, `SETUPAPI`, `CFGMGR32`,
`MSVCR71`, `MSVCP71`.

API sets (`api-ms-win-*`) and `ntdll` already resolve into the kernel32/ucrt
tables — the api-set forwarding is in place.

## Tier 1 — near-universal, zero coverage today

These are linked by the widest set of programs (Qt, games, installers,
network tools). Each gets a micro-exe verification.

| DLL | Export families to implement | Host strategy | Verify with |
| --- | --- | --- | --- |
| `WS2_32` | WSAStartup/WSACleanup, socket, bind, listen, accept, connect, send, recv, closesocket, select, getaddrinfo/freeaddrinfo, gethostbyname, inet_addr/ntoa, htons/ntohs, getsockname, setsockopt | real host sockets via a small FFI bridge (or loopback std::net) | `ws2_echo` — guest TCP echo client/server over loopback |
| `CRYPT32` | CryptAcquireContext, CryptGenRandom, CryptCreateHash, CryptHashData, CryptDeriveKey, CryptEncrypt/Decrypt | host hashing (sha256) + real RNG; symmetric ops via host | `crypt_hash` — hash + random micro |
| `MSIMG32` | AlphaBlend, TransparentBlt, GradientFill | gdi32 blit layer (32-bpp blend already exists in blit.rs) | `gdi_alpha` — alpha-blended quad |
| `IMM32` | ImmGetContext, ImmGetCompositionString, ImmReleaseContext, ImmGetOpenStatus | no-op returns (IME is a non-goal) | `ime_noop` — calls them, asserts no crash |
| `UXTHEME` | SetWindowTheme, OpenThemeData, CloseThemeData, IsThemeActive | S_OK no-ops | notepad runs (already calls SetWindowTheme) |
| `SETUPAPI` + `CFGMGR32` | SetupDiGetClassDevs, SetupDiEnumDeviceInfo, SetupDiGetDeviceInstanceId, CM_Get_Device_ID_List | empty device list (no host devices) | `setupapi_list` — installer-style enumeration returns empty |
| `DBGHELP` + `IMAGEHLP` | SymInitialize, SymCleanup, SymFromAddr, StackWalk64, MapFileAndCheckSum | minimal: init no-op, lookups fail gracefully | `dbghelp_stub` — init + fail path |
| `MSVCR71` / `MSVCP71` | legacy CRT entry points | forward to the ucrt/msvcrt handlers where names match | old mingw binaries load |

**Milestone M1**: ✅ LANDED (2026-08) — all Tier-1 families implemented with
micro-exes green under JIT and iced: WS2_32 (real loopback TCP), CRYPT32
(real SHA-1/SHA-256 + /dev/urandom), MSIMG32 (real AlphaBlend/TransparentBlt/
GradientFill), IMM32, UXTHEME, SETUPAPI/CFGMGR32, DBGHELP/IMAGEHLP,
MSVCR71/MSVCP71 forwarding. Zero-coverage table now holds only WININET/URLMON.

## Tier 2 — universal but partial today

| DLL | Gap to close | Verify with |
| --- | --- | --- |
| `OLE32` + `OLEAUT32` | CoCreateInstance with registered classes, CoRegisterClassObject/Revoke, CoTaskMemAlloc family, StringFromCLSID/CLSIDFromString, SafeArray* full family, DispGetIDsOfNames/DispInvoke | `ole_com` — CoCreateGuid + SafeArray round-trip |
| `SHELL32` | SHGetFileInfo, SHGetSpecialFolderPath, ShellExecuteEx, known-folder CSIDLs | notepad + `shell_folders` |
| `ADVAPI32` | RegDeleteKey, RegFlushKey, RegSaveKey; security stubs return access-denied-free success | notepad registry round-trip |
| `USER32` | EnumWindows/EnumChildWindows, FindWindow, caret family (CreateCaret etc.), DrawIcon, GetSystemMetrics breadth | `user32_enum` |
| `GDI32` | GetDIBits/SetDIBits, regions (CreateRectRgn/CombineRgn), font enumeration (EnumFontFamiliesEx), SetPixel | `gdi_regions` |
| `COMCTL32` | ImageList rest, toolbar, listview, treeview, progress bar | `comctl_toolbar` |
| `WINMM` | waveOut*, timeSetEvent | `winmm_audio` — waveOut stubs (no host audio) |

**Milestone M2**: Tier 2 complete.

## Tier 3 — infrastructure breadth

- `ntdll` Nt* surface used by modern toolchains (NtQueryInformationProcess,
  NtQuerySystemInformation, NtClose aliases).
- Api-set completeness check: build a small modern-SDK PE and assert every
  `api-ms-win-*` import resolves.
- Legacy CRT forwarders (MSVCR100/120/140 into ucrt) so pre-UCRT binaries link.

**Milestone M3**: Tier 3 complete.

## Cross-cutting

- **Import census**: extend `wie inspect --winapi-map` to emit a per-DLL
  coverage report; CI asserts the zero-coverage count never regresses.
- **Per-DLL test seam**: one micro-exe per DLL family, added to the
  micro-suite; a DLL family is "done" only when its micro-exe passes under
  both JIT and `WIE_CPU=iced`.
- **File-size cap**: new DLL modules must stay under the 1500-line cap;
  split into per-family submodules from the start (the `kernel32/file_io/`
  precedent).
- **Docs**: each landed DLL updates `docs/missing-winapi-handlers.md` (move
  exports from Missing to Implemented) in the same commit.

## Definition of done (release 0.2)

1. Zero-coverage table has only explicitly-deferred entries (WININET/URLMON).
2. Every Tier-1/2 DLL has a passing micro-exe under both CPU backends.
3. Qt-class smoke: a minimal Qt app links and reaches its message loop.
4. Full gate green: `./scripts/check.sh`, clippy `-D warnings`, fmt, sizes.
