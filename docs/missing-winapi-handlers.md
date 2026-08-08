# Missing WinAPI Handlers

Categorized by DLL. Based on the dispatch table and module structure in `crates/wie-winapi/`.

---

## Recently landed (notepad milestone, 2026-08)

The classic-Win32 depth-of-coverage work (see `docs/notepad-support-plan.md`) landed a large
batch of handlers that earlier sections of this file list as missing. The stale bullets are
superseded by this list:

- **Menus from resources**: `LoadMenuA/W` + `RT_MENU` parsing (wie-pe), `lpszMenuName` at
  RegisterClassEx/CreateWindowEx, menu state (`EnableMenuItem`/`CheckMenuItem`/`CheckMenuRadioItem`)
  mirrored into the macOS top bar (muda `MacMenuBar`), `Get/SetMenuItemInfo`, `ModifyMenu`,
  `RemoveMenu`, `DestroyMenu`, `GetSystemMenu`.
- **Accelerators**: `RT_ACCELERATOR` parsing, `LoadAcceleratorsW`, `TranslateAcceleratorA/W`,
  `DestroyAcceleratorTable` — wired into the guest message loop ahead of `TranslateMessage`.
- **String tables**: `LoadStringA/W` from `RT_STRING`; `RegisterWindowMessageA/W` (per-session
  atom cache, 0xC000+ range).
- **Multiline EDIT**: ES_MULTILINE + full EM_* family (LIMITTEXT/GETLINECOUNT/LINEFROMCHAR/
  LINEINDEX/LINELENGTH/GETLINE/REPLACESEL/SCROLLCARET/GETHANDLE/SETHANDLE/GETMODIFY/SETMODIFY/
  CANUNDO/UNDO/EMPTYUNDOBUFFER/SETTABSTOPS/SELECTIONTYPE/POSFROMCHAR), wrap, wheel + keyboard
  vertical scrolling, mouse click/drag/double-click selection, caret blink, typing-at-bottom
  auto-scroll, `WM_SETFONT`/`WM_GETFONT`, EN_CHANGE/EN_HSCROLL/EN_VSCROLL delivery.
- **EDIT clipboard**: `WM_CUT/COPY/PASTE/CLEAR/UNDO` + single-level undo buffer +
  `IsClipboardFormatAvailable(CF_TEXT)` (host `String` clipboard state; macOS NSPasteboard not
  bridged — YAGNI).
- **Status bar**: `STATUSCLASSNAMEW` built-in class, `CreateStatusWindowA/W`, `SB_SETPARTS`/
  `SB_SETTEXTW`/`SB_GETTEXTW`/`SB_GETTEXTLENGTHW` + ANSI legacy ids, WM_SIZE bottom-anchor,
  painted parts (BTNFACE raised edge, per-part clipped text).
- **Common dialogs**: interactive `GetOpenFileNameW`/`GetSaveFileNameW` (host "FileDialog" window
  under `FileDialogPolicy::Interactive`; `Cancel`/`Accept{path}` policy for headless), `FindTextW`/
  `ReplaceTextW` modeless dialogs over `FINDMSGSTRING` (FR_FINDNEXT/FR_REPLACE/FR_REPLACEALL/
  FR_DIALOGTERM), `GetFileTitleA/W`, `ChooseColorA`.
- **Registry values**: `RegQueryValueExA/W`, `RegSetValueExA/W`, `RegDeleteValueA/W`,
  `RegOpenKeyA/W`, `RegCreateKeyExA/W` — with per-bottle hive persistence
  (`{root}/registry/hive.dat`, write-through, corrupt → fresh profile).
- **Drag & drop**: `DragAcceptFiles`, `DragQueryFileA/W`, `DragQueryPoint`, `DragFinish`,
  `WM_DROPFILES` from the winit file-drop event, `host_path_to_guest` bottle mapping.
- **Misc**: `ShellAboutW` (via the MessageBox bridge), `ShellExecuteW` ("open" → detached
  `wie-cli run <path>`), `GetTimeFormatW`/`GetDateFormatW` (full picture-string tokenizer),
  `LocalLock`/`LocalUnlock`, `SetDlgItemInt`/`GetDlgItemInt`/`SendDlgItemMessageW`, `COLOR_INFOBK`.
- **CRT/startup (UCRT)**: `_initialize_wide_environment`, `_configure_wide_argv`, `GetStartupInfoW`,
  `GetUserDefaultUILanguage`, `__p__wenviron`, `_vsnwprintf`/`_vsnprintf`, `__stdio_common_vfprintf`,
  and the notepad P0 chain (LoadStringW → RegOpenKeyW → CreateFontIndirectW → LoadIconW →
  LoadCursorW → DragAcceptFiles → CreateStatusWindowW → GetFileTitleW → GetWindowTextLengthW →
  GetWindowPlacement/SetWindowPlacement).
- **Not winapi handlers**: multi-window winit host + `run --console` are wie-cli/runtime plumbing;
  `SHUFPD` (SSE shuffle) is a wie-cpu iced/JIT instruction (commit b5952b3).

## Notepad trace triage (Task 6.2, 2026-08-04)

`./target/debug/wie trace --max-api 400 real_exes/notepad.exe` now runs the full startup +
first message-loop iterations to a clean `ExitProcess { code: 0 }` — the entire P0.3-era
first-failure chain is resolved. `inspect --winapi-map` still flags 25 imports as TODO; 23 are
genuinely unimplemented (2 are map artifacts: `RegCreateKeyExW` is soft-dispatched in advapi32,
`DialogBoxParamW` is an in-guest stub). Verified against the current dispatch table (2026-08),
the remaining notepad imports that would bail if called:

| Import | Status |
| --- | --- |
| `advapi32!IsTextUnicode` | encoding detect helper |
| `user32!WinHelpW`, `user32!wsprintfW` | minor/rare paths |
| `msvcrt!_wcmdln`, `fgetwc`, `getc`, `vfprintf` | legacy CRT exports not reached by notepad |

Previously-listed gaps that have since landed: `ChooseFontW`/`PageSetupDlgW`/`PrintDlgW`
(native font/page-setup/print panels), the print-path GDI stubs (`AbortDoc`/`EndDoc`/`EndPage`/
`StartDocW`/`StartPage`), `GetTextMetricsW`, `Rectangle`, `SetMapMode`, `InflateRect`,
`SetProcessDefaultLayout`, and the interactive file dialog (native rfd Open/Save bridge —
the earlier "in-guest modal loop does not survive a live run" gap is superseded by it).

---

## DLLs With Zero Coverage

Any call to these returns `bail!("unsupported WinAPI call: {library}!{name}")`:

| DLL | Purpose |
|-----|---------|
| `WININET.dll` | Internet APIs |
| `URLMON.dll` | URL Moniker |

**Recently landed (Tier-1 universal-DLL wave, release 0.2):**

- `WS2_32.dll` — real Winsock: TCP loopback sockets via host `std::net`
  (socket/bind/listen/accept/connect/send/recv/select/getaddrinfo/gethostbyname/…).
  SOCKET table lives in `DllId::Ws2` (`Ws2State`). `ws2_echo` micro round-trips
  "ping"→"pong" through the JIT and iced backends.
- `CRYPT32.dll` — real hashing (SHA-1/SHA-256 via sha1/sha2 crates) + real entropy
  (`/dev/urandom`): acquire/release context, GenRandom, Create/DestroyHash,
  HashData, GetHashParam. `DllId::Crypt32` (`Crypt32State`). `crypt_hash` micro
  verifies both digest vectors. NOTE: mingw links these CryptoAPI exports under
  `advapi32.dll` — `crypt_hash` uses a `.def`/dlltool import lib rebinding them
  to `crypt32.dll` where WIE dispatches them.
- `MSIMG32.dll` — real DIB-to-DIB `AlphaBlend` (per-pixel straight alpha,
  AC_SRC_ALPHA-gated), `TransparentBlt` (color-keyed copy), `GradientFill`
  (H/V rect modes). `gdi_alpha` micro proves blending end-to-end.
- `IMM32.dll` — benign no-ops (IME is a non-goal): ImmGetContext→NULL,
  ImmGetOpenStatus→0, ImmReleaseContext→TRUE, ImmGetCompositionStringW→0.
- `UXTHEME.dll` — visual-style no-ops: OpenThemeData→NULL, CloseThemeData→S_OK,
  IsThemeActive→FALSE, GetWindowTheme→NULL (SetWindowTheme already S_OK).
- `SETUPAPI.dll` + `CFGmgr32.dll` — empty device enumeration: GetClassDevs→fake
  HDEVINFO, EnumDeviceInfo→FALSE + ERROR_NO_MORE_ITEMS, DestroyDeviceInfoList→TRUE,
  CM_Get_Device_ID_List→CR_SUCCESS with empty buffer.
- `DBGHELP.dll` + `IMAGEHLP.dll` — minimal: SymInitialize/SymCleanup→TRUE,
  SymFromAddr→FALSE + ERROR_INVALID_ADDRESS, MapFileAndCheckSum→CHECKSUM_SUCCESS
  with zeroed sums.
- `MSVCR71.dll` + `MSVCP71.dll` — legacy CRT forwarded into the existing
  `dispatch_ucrt` (added to `is_ucrt_library`); data imports (`_iob`, `_fmode`,
  `_acmdln`, `__initenv`) were already covered.

---

## SHELL32.dll — 12 exports

**Implemented:**
- `SHGetFolderPathW` — functional, maps CSIDL to synthetic bottle paths
- `SHGetPathFromIDListW` — returns FALSE, writes empty string
- `SHBrowseForFolderW` — returns NULL
- `CommandLineToArgvW` — full argv parse, heap-allocated result + strings
  (`handle_command_line_to_argv_w`, soft-dispatch `dispatch_shell32`)
- `SHAddToRecentDocs` — records via debug trace, no-op (RNotepad calls it on
  every open/save; `handle_sh_add_to_recent_docs`)
- `DragAcceptFiles`, `DragQueryFileA/W`, `DragQueryPoint`, `DragFinish` + `WM_DROPFILES` (see "Recently landed")
- `ShellAboutW` (MessageBox bridge), `ShellExecuteW` ("open" → detached `wie-cli run`)

**Missing:**
- `ShellExecuteA`
- `ShellExecuteExA` / `ShellExecuteExW`
- `FindExecutableA` / `FindExecutableW`
- `SHGetFileInfoA` / `SHGetFileInfoW`
- `SHGetSpecialFolderPathA` / `SHGetSpecialFolderPathW`
- `SHCreateDirectoryExA` / `SHCreateDirectoryExW`
- `SHEmptyRecycleBinA` / `SHEmptyRecycleBinW`
- `SHQueryRecycleBinA` / `SHQueryRecycleBinW`
- `SHParseDisplayName`
- `ILCreateFromPath`
- `SHChangeNotify`
- `SHGetDesktopFolder`
- All other shell namespace / PIDL / icon / known folder APIs

---

## OLEAUT32.dll — BSTR / Variant core + arithmetic

**Implemented:** Ordinals 2–11, 149, 150 (SysAllocString/Len/ByteLen, SysReallocString/Len,
SysFreeString, SysStringLen, VariantInit/Clear/Copy/CopyInd), plus the variant arithmetic and
conversion family in `dispatch_oleaut32` (`oleaut32.rs`): `VarAdd/Sub/Mul/Div/Mod` (shared
`handle_var_math`), `VarBstrFromI4/R4/R8/Date`, `VarDateFromBstr/I4/R8`, `VarI4FromBstr/R8`,
`VarR4FromBstr`, `VarR8FromBstr/I4`.

**Missing (would bail):**
- `VarI4FromDate`, `VarR8FromDate`
- `VarBoolFromStr`, `VarI2FromStr`, `VarI4FromStr`, `VarR8FromStr`, `VarDateFromStr`, `VarBstrFromBool`
- All `SafeArray*` functions (Create, Destroy, GetDim, GetElemsize, Lock, Unlock, GetUBound, GetLBound, PtrOfIndex, GetElement, PutElement)
- All `SafeArray*Vector` functions
- `CreateErrorInfo`, `CreateInstance`, `GetActiveObject`
- All `CreateTypeLib*`, `LoadTypeLib*`, `LoadRegTypeLib*` (type info)
- `DispGetIDsOfNames`, `DispInvoke`

---

## OLE32.dll — COM stubs only

**Implemented:** CoInitialize, CoInitializeEx, CoUninitialize (all return S_OK),
CoCreateInstance (returns `REGDB_E_CLASSNOTREG` — no COM servers registered)

**Missing (would bail):**
- `CoGetClassObject`
- `CoRegisterClassObject`, `CoRevokeClassObject`
- `CoCreateGuid`
- `CoTaskMemAlloc`, `CoTaskMemFree`, `CoTaskMemRealloc`
- `StringFromGUID2`, `StringFromCLSID`, `CLSIDFromString`, `CLSIDFromProgID`
- `OleInitialize`, `OleUninitialize`
- `GetRunningObjectTable`, `CreateBindCtx`, `MkParseDisplayName`
- All DCOM / marshaling / apartment APIs

---

## D3D9.dll — ~51 of 119 IDirect3DDevice9 methods (L1–L7 landed)

**Implemented** (all backed by the `d3d9_render/` software rasterizer — triangle/
line/point rasterization, VS/PS programs, texture sampling, depth/blend state,
region-limited present; 51 `IDirect3DDevice9` methods + the `IDirect3D9` core
(6) + texture/surface/vertex-buffer/index-buffer/shader objects (23)):

- `Direct3DCreate9`; `IDirect3D9`: `GetAdapterCount`, `GetAdapterMonitor`,
  `GetDeviceCaps`, `GetAdapterDisplayMode`, `CreateDevice`, `Release`
- Drawing: `DrawPrimitive`, `DrawIndexedPrimitive`, `DrawPrimitiveUP`,
  `DrawIndexedPrimitiveUP`; `BeginScene`/`EndScene`/`Present`/`Clear`
- Vertex/index buffers: `CreateVertexBuffer`, `CreateIndexBuffer`, `SetStreamSource`/
  `GetStreamSource`, `SetIndices`/`GetIndices`; `IDirect3DVertexBuffer9`/`IndexBuffer9`:
  `QueryInterface`, `AddRef`, `Release`, `Lock`, `Unlock`, `GetDesc`
- Textures: `CreateTexture`, `SetTexture`/`GetTexture`; `IDirect3DTexture9`:
  `GetLevelCount`, `GetSurfaceLevel`, `LockRect`, `UnlockRect`, `Release`
- Render targets / depth stencil: `CreateRenderTarget`, `SetRenderTarget`/
  `GetRenderTarget`, `CreateDepthStencilSurface`, `SetDepthStencilSurface`/
  `GetDepthStencilSurface`; `IDirect3DSurface9`: `LockRect`, `UnlockRect`,
  `GetDesc`, `Release`
- Shaders: `CreateVertexShader`/`GetVertexShader`/`SetVertexShader`,
  `CreatePixelShader`/`GetPixelShader`/`SetPixelShader`, the full
  `Set/GetVertexShaderConstant{F,I,B}` + `Set/GetPixelShaderConstantF` set,
  `IDirect3DVertexShader9`/`PixelShader9` `Release`
- State: `SetRenderState`/`GetRenderState`, `SetTextureStageState`/
  `GetTextureStageState`, `SetSamplerState`/`GetSamplerState`, `SetFVF`,
  `SetTransform`/`GetTransform`/`MultiplyTransform`, `SetViewport`/`GetViewport`,
  `SetScissorRect`

**Remaining gaps:**
- Cube/volume textures: `CreateCubeTexture`, `CreateVolumeTexture`
- Surfaces: `CreateOffscreenPlainSurface`, `GetBackBuffer`, `GetFrontBufferData`,
  `GetSurfaceFromRenderTarget`, `UpdateSurface`, `UpdateTexture`, `StretchRect`,
  `ColorFill`
- Shader lifecycle: `DeleteVertexShader`, `DeletePixelShader`
- Materials/lighting: `SetMaterial`/`GetMaterial`, `SetLight`/`GetLight`, `LightEnable`
- Clipping: `SetClipPlane`/`GetClipPlane`, `SetClipStatus`/`GetClipStatus`
- Queries: `CreateQuery`, `Issue`, `GetData`
- Swap chain / device lifecycle: `CreateAdditionalSwapChain`, `Reset`,
  `TestCooperativeLevel`, `DrawRectPatch`/`DrawTriPatch`
- Most remaining `Get*` state queries not listed above

---

## GDI32.dll — 38 exports, real text/print/blit

**Implemented (real, not stubs):**
- **Text**: `TextOutA/W`, `ExtTextOutW`, `DrawTextA/W` — real glyph rasterization
  with macOS system fonts (glyph-coverage blending, OPAQUE/TRANSPARENT modes,
  fake-bold double-draw, strikeout/underline) into the DC backing store
  (`gdi32/text.rs`); `GetTextExtentPoint32A/W`, `GetTextMetricsA/W` (font metrics)
- **Print**: `CreateDCW`, `StartDocW`, `StartPage`, `EndPage`, `EndDoc`, `AbortDoc` —
  real print-DC jobs at 300 DPI; completed pages are written as
  `page-{n}.bmp` under `WIE_PRINT_TO` (`gdi32/print.rs`)
- **Blits**: `BitBlt` (real 32-bpp SRCCOPY blit to window surfaces),
  `PatBlt` (fills with the DC's selected brush color), `FillRect`
  (`gdi32/blit.rs` — `FillRect` is dispatched from the `user32.dll` name rows)
- **Objects/state**: `CreateSolidBrush`, `CreatePen`, `CreateFontA/W`,
  `CreateFontIndirectA/W`, `CreateCompatibleBitmap`, `CreateCompatibleDC`,
  `CreateDIBSection` (allocates a pixel buffer selectable into DCs),
  `SelectObject`, `GetObjectA`, `GetStockObject` (7 brushes + 3 pens +
  SYSTEM_FONT + DEFAULT_PALETTE; unknown → 0), `SetBkColor`/`SetBkMode`/
  `SetTextColor`, `SetMapMode`, `Rectangle`, `DeleteDC` (drops print jobs),
  `DeleteObject`
- `GetDeviceCaps` — partial: ~30 screen indices + print-DC geometry, unknown → 0

**Stubs (accept input, return success, do nothing):**
- `StretchBlt` — returns 1, no scaling
- `GetPixel` — returns `FAKE_PIXEL_COLOR` (0)

**Missing entirely:**
- `AlphaBlend`, `TransparentBlt`, `GradientFill`
- `SetPixel`, `GetDIBits`, `SetDIBits`, `SetDIBitsToDevice`, `StretchDIBits`
- `CreateEllipticRgn`, `CreateRectRgn`, `CreatePolygonRgn`, `CombineRgn`
- `FrameRgn`, `FillRgn`, `InvertRgn`, `PaintRgn`
- `GetRandomRgn`, `GetRegionData`
- `SetWorldTransform`, `GetWorldTransform`, `ModifyWorldTransform`
- `GetMapMode`, `SetGraphicsMode`, `GetGraphicsMode`
- `CreateEnhMetaFile`, `CloseEnhMetaFile`, `PlayEnhMetaFile`, `DeleteEnhMetaFile`
- `CreateHalftonePalette`, `CreatePalette`, `RealizePalette`, `SelectPalette`

---

## USER32.dll — ~80% coverage, windows are painted

**Partial / stub implementations:**
- `BeginPaint` / `EndPaint` — PAINTSTRUCT + surface-backed HDC, repaint loop drives WM_PAINT
- `ScrollWindowEx` / `ScrollDC` — return 1, no actual pixel scroll
- `DefWindowProcA/W` — returns 0 for unhandled messages
- `TrackMouseEvent` — returns 0
- `SetWindowsHookExW` — installs hook record, hooks never invoked
- `CallNextHookEx` — returns 0
- `SetTimer` / `KillTimer` — real deadline-tracked timer records, WM_TIMER delivered via the synthetic-message pump
- `SendMessageA/W` — synchronous dispatch into the target WndProc (WM_ERASEBKGND / WM_MDICREATE handled host-side)
- `LoadImageA/W` — returns fake handles, no image data loaded
- `SetWindowLongA/W` / `GetWindowLongA/W` — manages window data but may miss GWL_\* IDs
- `SetClassLongA/W` / `GetClassLongA/W` — partial
- `AdjustWindowRect` / `AdjustWindowRectEx` — partial
- `ScreenToClient` / `ClientToScreen` — functional for fake window rects
- `BringWindowToTop` — stub
- `SetParent` / `GetParent` — functional but no reparenting behavior
- `ShowWindow` / `ShowWindowAsync` — manages SW_\* flags, no real visibility
- `IsWindowVisible` / `IsWindowEnabled` — returns state from flags
- `InvalidateRect` / `ValidateRect` — real dirty-rect accumulation; invalidations bump the content revision so the idle reconcile republishes the window surface
- `GetDC` / `ReleaseDC` — surface-backed HDCs (getdc paints to the window surface)
- `GetWindowDC` — returns fake HDC for the whole window
- `BeginDeferWindowPos` / `DeferWindowPos` / `EndDeferWindowPos` — window-z-order tracking, no real reposition
- `CreateWindowExA/W` — registers window in hash map with client rect, styles, parent/child links
- `DestroyWindow` — cleans up window hash map entry
- `GetAsyncKeyState` — functional, reads real keyboard
- `PeekMessageA/W` / `GetMessageA/W` / `TranslateMessage` / `DispatchMessageA/W` — message pump works
- `RegisterClassA/W` / `UnregisterClassA/W` — functional class registration
- `MessageBoxA/W` — prints to stderr via tracing
- `GetSystemMetrics` — returns synthetic values (800x600 screen, etc.)

**Missing entirely (superseded items — accelerators, dialogs, focus/capture, menu APIs,
window placement, clipboard, SetTimer/WM_TIMER, drag-drop, SetWindowText-family, the
find/replace + Go To + font dialogs, status-bar paint, the EDIT caret + EM_SETHANDLE —
are landed; see "Recently landed"):**
- `CreateAcceleratorTableA/W` (runtime-built tables; resource tables + TranslateAccelerator are landed)
- `MapDialogRect`
- `CheckDlgButton`, `CheckRadioButton`, `IsDlgButtonChecked`
- `GetUpdateRect`, `GetUpdateRgn`, `ExcludeUpdateRgn`
- `SetWindowRgn`, `GetWindowRgn`
- `ArrangeIconicWindows`
- `SetSysColors`, `SetSysColorsTemp`
- `DrawIcon`, `DrawIconEx`, `DrawTextExA/W`, `TabbedTextOutA/W`
- `DrawEdge`, `DrawFrameControl`, `DrawCaption`
- `FrameRect`, `InvertRect`
- `WindowFromPoint`, `ChildWindowFromPoint`, `ChildWindowFromPointEx`
- `FindWindowA/W`, `FindWindowExA/W`
- `EnumWindows`, `EnumChildWindows`, `EnumThreadWindows`
- `FlashWindow`, `FlashWindowEx`
- `OpenIcon`, `CloseWindow`
- `LockWindowUpdate`
- `CreateCaret`, `ShowCaret`, `HideCaret`, `SetCaretPos`, `GetCaretPos`, `DestroyCaret`
  (the EDIT control's caret blink is internal to the control; these Win32 caret APIs are
  not dispatched)
- `SetCursorPos`, `ShowCursor`, `LoadCursorFromFileA/W`
  (`GetCursorPos`, `SetCursor`, `GetCursor`, `ClipCursor`, `GetClipCursor` landed)
- `CascadeWindows`, `TileWindows`

**Landed since this list was written** (now in the dense user32 table):
`RedrawWindow`, `GetDesktopWindow`, `SetForegroundWindow`/`GetForegroundWindow`,
`GetSysColorBrush`, `DrawMenuBar`, `FillRect` (gdi32 `handle_fill_rect`),
`GetCursorPos`, `SetCursor`, `GetCursor`, `ClipCursor`, `GetClipCursor`.

---

## ADVAPI32.dll — ~40% coverage, registry is minimal

**Registry missing (value/key storage landed — RegQueryValueExA/W, RegSetValueExA/W,
RegDeleteValueA/W, RegOpenKeyA/W, RegOpenKeyExA/W, RegCreateKeyExA/W, RegCloseKey +
per-bottle hive persistence; enumeration landed — RegEnumKeyExA/W, RegEnumValueA/W in
`dispatch_advapi32_extra`; see "Recently landed"):**
- `RegDeleteKeyA` / `RegDeleteKeyW`
- `RegLoadKeyA` / `RegLoadKeyW`
- `RegUnLoadKeyA` / `RegUnLoadKeyW`
- `RegConnectRegistryA` / `RegConnectRegistryW`
- `RegSaveKeyA` / `RegSaveKeyW`
- `RegReplaceKeyA` / `RegReplaceKeyW`
- `RegSetKeySecurity` / `RegGetKeySecurity`
- `RegGetKeyName`
- `RegCopyTree`
- `RegDeleteTree`
- `RegDisablePredefinedCache`
- `RegFlushKey`

**Security missing:**
- `AccessCheck`
- `AllocateAndInitializeSid`
- `CheckTokenMembership`
- `CreatePrivateObjectSecurity`
- `DestroyPrivateObjectSecurity`
- `DuplicateToken` / `DuplicateTokenEx`
- `GetNamedSecurityInfoA` / `GetNamedSecurityInfoW`
- `SetNamedSecurityInfoA` / `SetNamedSecurityInfoW`
- `GetSecurityInfo` / `SetSecurityInfo`
- `ImpersonateSelf` / `RevertToSelf`
- `ImpersonateLoggedOnUser` / `RevertToSelf`
- `IsValidSid` / `EqualSid` / `CopySid` / `GetLengthSid`
- `LookupAccountNameA/W`, `LookupAccountSidA/W`
- `OpenThreadToken` (`OpenProcessToken` landed — fake token handle)
- `OpenProcessToken`, `AdjustTokenPrivileges`, `LookupPrivilegeValueA/W` (have implementations)

**Landed security/file-security:** `InitializeSecurityDescriptor`,
`SetSecurityDescriptorDacl`, `GetFileSecurityA/W`, `SetFileSecurityA/W`.

---

## KERNEL32.dll — ~90% coverage, mostly complete

**Landed since this list was written** (verified against `dispatch_kernel32_extra`
+ `kernel32/file_io/*`): `DuplicateHandle` (real handle duplication), `GetThreadPriority`
(validates the handle, returns THREAD_PRIORITY_NORMAL), `GetCompressedFileSizeA/W`,
`GetVolumeInformationA/W`, `GetDiskFreeSpaceW`/`GetDiskFreeSpaceExW`, `SetEndOfFile`,
`SetFileValidData`, `LockFile`/`UnlockFile`, `GetFileAttributesExA/W`, `BackupRead`/
`BackupSeek`/`BackupWrite`, `GetLongPathNameA/W`/`GetShortPathNameA/W`,
`GetTempPathA/W`/`GetTempFileNameA/W`, `GetWindowsDirectoryA/W`/`GetSystemDirectoryA/W`,
`GetComputerNameA/W`/`GetComputerNameExA/W`, `GetUserNameA/W`, `GetUserProfileDirectoryA/W`,
`OpenThread`, `CreateJobObjectA/W`/`AssignProcessToJobObject`, `CreateFileMappingW`/
`OpenFileMappingA/W`/`MapViewOfFile`/`UnmapViewOfFile`, `SetErrorMode`/`SetThreadErrorMode`,
`DebugBreak`/`IsDebuggerPresent`/`OutputDebugStringA/W`, `QueryFullProcessImageNameA/W`,
`GetProcessTimes`, `TerminateProcess`/`TerminateThread`, `SuspendThread`/`ResumeThread`,
`SignalObjectAndWait`, `SetThreadAffinityMask`, `CreateSemaphoreA/W`+`ReleaseSemaphore`
(see also `kernel32/misc/identity.rs`, `process_thread.rs`, `memory.rs`, `file_io/vol.rs`).

**Missing / stub:**
- `DeviceIoControl` — returns FALSE, `ERROR_INVALID_FUNCTION` (comment: "unsupported")
- `FindFirstStreamW` / `FindNextStreamW` — extra dispatch, basic (always `ERROR_HANDLE_EOF`)
- `GetDiskFreeSpaceA` / `GetDiskFreeSpaceExA` (W forms landed)
- `ReadFileScatter` / `WriteFileGather`
- `SetTempPathA`
- `GetSystemWindowsDirectoryA/W`
- `GetUserNameExA/W`
- `OpenProcess`
- `SetInformationJobObject` / `QueryInformationJobObject`
- `CreateNamedPipeA/W`, `ConnectNamedPipe`, `DisconnectNamedPipe`, `CallNamedPipe`, `WaitNamedPipe`
- `CreateFileMappingA` (W landed), `FlushViewOfFile`
- `GetThreadTimes`, `GetSystemTimes`
- `GetProcessIoCounters`
- `GetProcessMemoryInfo` (PSAPI), `EnumProcessModules` (PSAPI)
- `SetThreadIdealProcessor`
- `SetProcessPriorityBoost` / `GetProcessPriorityBoost`
- `SetThreadPriorityBoost` / `GetThreadPriorityBoost`
- `CreateSemaphoreExA/W` (base CreateSemaphoreA/W landed)
- `SHGetFolderPath` equivalents — may rely on SHELL32 (SHGetFolderPathW is implemented there)

---

## COMCTL32.dll — status bar + a few ImageList functions

**Implemented:**
- `DLLGetVersion`, `InitCommonControls` (ordinal 17), `InitCommonControlsEx`
- ImageList: `ImageList_Create`, `ImageList_AddMasked`, `ImageList_SetBkColor`, `ImageList_Destroy`
- Status bar: `CreateStatusWindowA/W`, `STATUSCLASSNAMEW` class, `SB_SETPARTS`/`SB_SETTEXTW`/`SB_GETTEXTW`/`SB_GETTEXTLENGTHW` (see "Recently landed")

**Missing:**
- Rest of the ImageList family: `ImageList_Add`, `ImageList_ReplaceIcon`, `ImageList_Remove`,
  `ImageList_GetImageCount`, `ImageList_SetImageCount`, `ImageList_GetIconSize`,
  `ImageList_SetIconSize`, `ImageList_GetIcon`, `ImageList_SetOverlayImage`, `ImageList_BeginDrag`,
  `ImageList_EndDrag`, `ImageList_DragEnter`, `ImageList_DragLeave`, `ImageList_DragMove`,
  `ImageList_GetDragImage`, `ImageList_SetDragCursorImage`
- All toolbar APIs: `CreateToolbarEx`, `CommandBar_*`, `Toolbar_Set*`
- All listview APIs: `ListView_Set*`, `ListView_Get*`, `ListView_InsertColumn`, `ListView_InsertItem`
- All treeview APIs: `TreeView_InsertItem`, `TreeView_DeleteItem`, `TreeView_Expand`, `TreeView_SelectItem`
- All tab control APIs: `TabCtrl_InsertItem`, `TabCtrl_DeleteItem`, `TabCtrl_SetCurSel`, `TabCtrl_GetCurSel`
- All up-down control APIs: `CreateUpDownControl`, `UDM_SetRange`, `UDM_GetRange`, `UDM_SetPos`, `UDM_GetPos`
- All progress bar APIs: `PBM_SetRange`, `PBM_SetPos`, `PBM_DeltaPos`, `PBM_SetMarquee`
- All trackbar APIs: `TBM_SetRange`, `TBM_SetPos`, `TBM_GetPos`, `TBM_SetTic`, `TBM_SetLineSize`, `TBM_SetPageSize`
- All header APIs: `Header_SetItem`, `Header_GetItem`, `Header_InsertItem`, `Header_DeleteItem`
- All rebar APIs
- All property sheet APIs: `PropertySheet`, `CreatePropertySheetPage`, `DestroyPropertySheetPage`
- All task dialog APIs: `TaskDialog`, `TaskDialogIndirect`

---

## UCRT.dll — ~80% coverage

**Landed since this list was written** (verified against `ucrt/mod.rs` dispatch):
`iswctype` (wide-char ctype mask test, `ucrt/string.rs` — RNotepad's whole-word Find
depends on it), `_set_app_type`/`__set_app_type`, `_time64`, `_localtime64`,
`_set_invalid_parameter_handler`, `__p__environ`/`__p__wenviron` (the environ
accessors; the `_environ`/`_wenviron` data symbols themselves are still not
exported), `_initterm`/`_initterm_e`.

**Likely missing (would be called by larger apps):**
- `_findfirst`, `_findnext`, `_findclose` (may use Win32 FindFirstFile instead)
- `_stat`, `_fstat`, `_wstat`
- `_access`, `_waccess`
- `_chdir`, `_wchdir`, `_mkdir`, `_wmkdir`, `_rmdir`, `_wrmdir`
- `_unlink`, `_wunlink`, `_remove`, `_wremove`
- `_rename`, `_wrename`
- `_getcwd`, `_wgetcwd`, `_getdcwd`, `_wgetdcwd`
- `_fullpath`, `_wfullpath`
- `_splitpath`, `_wsplitpath`, `_makepath`, `_wmakepath`
- `_dupenv_s`, `_wdupenv_s`, `_environ`, `_wenviron`
- `_localtime64_s`, `_gmtime64_s`, `_mktime64`, `_difftime64` (`_time64`/`_localtime64` landed)
- `_strftime_l`, `_wcsftime_l`, `_date`, `_strdate`, `_time`, `_strtime`, `_tzset`
- `_beginthread`, `_endthread` (`_beginthreadex`/`_endthreadex` have implementation)
- `_execute_onexit_table`, `_initialize_onexit_table`, `_register_onexit_function`
- `_CrtDbgReport` (debug CRT)
- `_controlfp`, `_control87`, `_statusfp`, `_clearfp`
- `_fpieee_flt`
- `_set_thread_local_invalid_parameter_handler` (`_set_invalid_parameter_handler` landed)
- `_seh_filter_dll`, `_seh_filter_exe`
- `_set_se_translator`

---

## GDI32.dll — additional specialty missing

Beyond the implemented set above (`TextOutA/W`, `ExtTextOutW`, `DrawTextA/W`,
`GetTextMetricsA/W`, `SetBkColor`/`SetBkMode`/`SetTextColor`, print path, `BitBlt`,
`PatBlt`, `FillRect`, object/state management):
- `GetDeviceCaps` — partial (only some indexes)
- `GetGlyphOutlineA/W` — no real font outline
- `GetCharABCWidthsA/W`, `GetCharWidthA/W`, `GetCharWidth32A/W`
- `GetOutlineTextMetricsA/W`
- `GetKerningPairsA/W`
- `CreateScalableFontResourceA/W`
- `AddFontResourceA/W`, `AddFontMemResourceEx`, `RemoveFontResourceA/W`
- `EnumFontsA/W`, `EnumFontFamiliesA/W`, `EnumFontFamiliesExA/W`
- `GetFontLanguageInfo`, `GetFontUnicodeRanges`
- `GetGlyphIndicesA/W`
- `GetRasterizerCaps`
- `GetSystemPaletteEntries`, `GetSystemPaletteUse`, `SetSystemPaletteUse`
- `AnimatePalette`
- `ResizePalette`
- `SetTextAlign`, `SetTextCharacterExtra`, `SetTextJustification`
- `GetBkColor`, `GetBkMode`, `GetTextColor`, `GetTextAlign`, `GetTextCharacterExtra`
- `SetArcDirection`, `GetArcDirection`
- `SetMiterLimit`, `GetMiterLimit`
- `SetPolyFillMode`, `GetPolyFillMode`
- `SetROP2`, `GetROP2`
- `SetStretchBltMode`, `GetStretchBltMode`
- `SetBrushOrgEx`, `GetBrushOrgEx`
- `SetPixelFormat`, `DescribePixelFormat`, `ChoosePixelFormat`, `GetPixelFormat`, `SwapBuffers` (OpenGL interop)
