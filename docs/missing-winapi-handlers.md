# Missing WinAPI Handlers

Categorized by DLL. Based on the dispatch table and module structure in `crates/wie-winapi/`.

---

## DLLs With Zero Coverage

Any call to these returns `bail!("unsupported WinAPI call: {library}!{name}")`:

| DLL | Purpose |
|-----|---------|
| `VERSION.dll` | Version info |
| `MSIMG32.dll` | Image operations (AlphaBlend, etc.) |
| `IMM32.dll` | Input Method Manager |
| `WS2_32.dll` | Winsock / network APIs |
| `WININET.dll` | Internet APIs |
| `URLMON.dll` | URL Moniker |
| `CRYPT32.dll` | Cryptography |
| `IMAGEHLP.dll` | PE image helpers |
| `DBGHELP.dll` | Debug help |
| `SETUPAPI.dll` | Device setup |
| `CFGmgr32.dll` | Configuration manager |
| `MSVCR71.dll` | Older CRT (MSVC 7.1) |
| `MSVCP71.dll` | Older C++ Standard Library |
| `UXTHEME.dll` | Visual styles (has dispatch entries but likely stub) |

---

## SHELL32.dll — Only 3 of ~50+ exports

**Implemented:**
- `SHGetFolderPathW` — functional, maps CSIDL to synthetic bottle paths
- `SHGetPathFromIDListW` — returns FALSE, writes empty string
- `SHBrowseForFolderW` — returns NULL

**Missing:**
- `ShellExecuteA` / `ShellExecuteW`
- `ShellExecuteExA` / `ShellExecuteExW`
- `FindExecutableA` / `FindExecutableW`
- `SHGetFileInfoA` / `SHGetFileInfoW`
- `CommandLineToArgvW`
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

## OLEAUT32.dll — Only BSTR and Variant basics

**Implemented:** Ordinals 2–11, 149 (SysAllocString, SysFreeString, SysStringLen, VariantInit, VariantClear, VariantCopy, etc.)

**Missing (would bail):**
- `VarDiv`, `VarMod`, `VarAdd`, `VarSub`, `VarMul`
- `VarBstrFromI4`, `VarBstrFromR8`, `VarBstrFromDate`
- `VarDateFromBstr`, `VarDateFromI4`, `VarDateFromR8`
- `VarI4FromBstr`, `VarI4FromDate`, `VarI4FromR8`
- `VarR8FromBstr`, `VarR8FromDate`, `VarR8FromI4`
- All `SafeArray*` functions (Create, Destroy, GetDim, GetElemsize, Lock, Unlock, GetUBound, GetLBound, PtrOfIndex, GetElement, PutElement)
- All `SafeArray*Vector` functions
- `CreateErrorInfo`, `CreateInstance`, `GetActiveObject`
- All `CreateTypeLib*`, `LoadTypeLib*`, `LoadRegTypeLib*` (type info)
- `DispGetIDsOfNames`, `DispInvoke`

---

## OLE32.dll — COM stubs only

**Implemented:** CoInitialize, CoInitializeEx, CoUninitialize (all return S_OK)

**Missing / stub:**
- `CoCreateInstance` — returns `REGDB_E_CLASSNOTREG` — no COM servers registered
- `CoGetClassObject` — returns `CLASS_E_CLASSNOTAVAILABLE`
- `CoRegisterClassObject`, `CoRevokeClassObject`
- `CoCreateGuid`
- `CoTaskMemAlloc`, `CoTaskMemFree`, `CoTaskMemRealloc`
- `StringFromGUID2`, `StringFromCLSID`, `CLSIDFromString`, `CLSIDFromProgID`
- `OleInitialize`, `OleUninitialize`
- `GetRunningObjectTable`, `CreateBindCtx`, `MkParseDisplayName`
- All DCOM / marshaling / apartment APIs

---

## D3D9.dll — ~5% of IDirect3DDevice9

**Implemented (of 119 method slots):** SetVertexShader, SetFVF, SetRenderState, SetTextureStageState, SetSamplerState, Release

**Missing:**
- Drawing: `DrawPrimitive`, `DrawIndexedPrimitive`, `DrawPrimitiveUP`, `DrawIndexedPrimitiveUP`, `DrawRectPatch`, `DrawTriPatch`
- Vertex buffers: `CreateVertexBuffer`, `CreateIndexBuffer`, `Lock`, `Unlock`, `GetVertexBuffer`, `GetIndexBuffer`
- Textures: `SetTexture`, `GetTexture`, `CreateTexture`, `CreateCubeTexture`, `CreateVolumeTexture`
- Surfaces: `CreateOffscreenPlainSurface`, `CreateRenderTarget`, `CreateDepthStencilSurface`, `GetRenderTarget`, `GetDepthStencilSurface`, `GetSurfaceFromRenderTarget`, `UpdateSurface`
- Shaders: `CreateVertexShader`, `CreatePixelShader`, `DeleteVertexShader`, `DeletePixelShader`, `GetVertexShader`, `GetPixelShader`
- State: Most `Get*` state queries, `SetTransform`, `GetTransform`, `SetMaterial`, `GetMaterial`, `SetLight`, `GetLight`, `LightEnable`
- Queries: `CreateQuery`, `Issue`, `GetData`
- Misc: `Present`, `Clear`, `BeginScene`, `EndScene`, `Reset`, `TestCooperativeLevel`, `GetDeviceCaps`, `CreateAdditionalSwapChain`, `GetBackBuffer`, `SetScissorRect`, `SetViewport`, `GetViewport`, `SetClipPlane`, `GetClipPlane`, `SetClipStatus`, `GetClipStatus`
- StretchRect, ColorFill, UpdateTexture, GetFrontBufferData

---

## GDI32.dll — ~60% coverage, rendering is all stubs

**Implemented but stubs (accept input, return success, do nothing):**
- `ExtTextOutW` — returns 1, no text rendering
- `CreateDIBSection` — allocates zeroed pixel buffer, no real GDI
- `BitBlt` — returns 1, no pixel copy
- `StretchBlt` — returns 1, no scaling
- `PatBlt` — returns 1, no pattern blit
- `GetPixel` — returns 0
- `SetPixel` — returns -1 (stub)
- `GetDIBits` — stub
- `SetDIBits` — stub
- `SetDIBitsToDevice` — stub
- `StretchDIBits` — stub

**Missing entirely:**
- `AlphaBlend`, `TransparentBlt`, `GradientFill`
- `CreateEllipticRgn`, `CreateRectRgn`, `CreatePolygonRgn`, `CombineRgn`
- `FrameRgn`, `FillRgn`, `InvertRgn`, `PaintRgn`
- `GetRandomRgn`, `GetRegionData`
- `SetWorldTransform`, `GetWorldTransform`, `ModifyWorldTransform`
- `SetMapMode`, `GetMapMode`
- `SetGraphicsMode`, `GetGraphicsMode`
- `CreateEnhMetaFile`, `CloseEnhMetaFile`, `PlayEnhMetaFile`, `DeleteEnhMetaFile`
- `StartDoc`, `EndDoc`, `StartPage`, `EndPage`, `AbortDoc`
- `GetStockObject` — partial
- `CreateHalftonePalette`, `CreatePalette`, `RealizePalette`, `SelectPalette`

---

## USER32.dll — ~80% coverage, UI is hollow

**Partial / stub implementations:**
- `BeginPaint` / `EndPaint` — fills PAINTSTRUCT with fake HDC, no real painting
- `ScrollWindowEx` / `ScrollDC` — return 1, no actual pixel scroll
- `DefWindowProcA/W` — returns 0 for unhandled messages
- `TrackMouseEvent` — returns 0
- `SetWindowsHookExW` — installs hook record, hooks never invoked
- `CallNextHookEx` — returns 0
- `SetTimer` / `KillTimer` — maintains timer records, timers never fire
- `SendMessageA/W` — returns 0 for most messages
- `LoadImageA/W` — returns fake handles, no image data loaded
- `SetWindowLongA/W` / `GetWindowLongA/W` — manages window data but may miss GWL_\* IDs
- `SetClassLongA/W` / `GetClassLongA/W` — partial
- `AdjustWindowRect` / `AdjustWindowRectEx` — partial
- `ScreenToClient` / `ClientToScreen` — functional for fake window rects
- `BringWindowToTop` — stub
- `SetParent` / `GetParent` — functional but no reparenting behavior
- `ShowWindow` / `ShowWindowAsync` — manages SW_\* flags, no real visibility
- `IsWindowVisible` / `IsWindowEnabled` — returns state from flags
- `InvalidateRect` / `ValidateRect` — manages dirty rect cache but no real paint
- `GetDC` / `ReleaseDC` — returns fake HDC from window cache
- `GetWindowDC` — returns fake HDC for the whole window
- `BeginDeferWindowPos` / `DeferWindowPos` / `EndDeferWindowPos` — window-z-order tracking, no real reposition
- `CreateWindowExA/W` — registers window in hash map with client rect, styles, parent/child links
- `DestroyWindow` — cleans up window hash map entry
- `GetAsyncKeyState` — functional, reads real keyboard
- `PeekMessageA/W` / `GetMessageA/W` / `TranslateMessage` / `DispatchMessageA/W` — message pump works
- `RegisterClassA/W` / `UnregisterClassA/W` — functional class registration
- `MessageBoxA/W` — prints to stderr via tracing
- `GetSystemMetrics` — returns synthetic values (800x600 screen, etc.)

**Missing entirely:**
- `CreateAcceleratorTableA/W`, `DestroyAcceleratorTable`, `TranslateAcceleratorA/W`
- `DialogBoxParamA/W`, `CreateDialogParamA/W`, `EndDialog`, `DefDlgProcA/W`
- `IsDialogMessageA/W`
- `GetDlgItem`, `GetDlgItemInt`, `GetDlgItemTextA/W`, `SetDlgItemInt`, `SetDlgItemTextA/W`
- `CheckDlgButton`, `CheckRadioButton`, `IsDlgButtonChecked`
- `MapDialogRect`
- `SetActiveWindow`, `GetActiveWindow`, `SetFocus`, `GetFocus`
- `GetCapture`, `SetCapture`, `ReleaseCapture`
- `SendDlgItemMessageA/W`
- `GetUpdateRect`, `GetUpdateRgn`, `ExcludeUpdateRgn`
- `RedrawWindow`
- `UpdateWindow`
- `SetWindowRgn`, `GetWindowRgn`
- `GetWindowPlacement`, `SetWindowPlacement`
- `ArrangeIconicWindows`
- `SetSysColors`, `GetSysColor`, `SetSysColorsTemp`, `GetSysColorBrush`
- `DrawIcon`, `DrawIconEx`, `DrawTextA/W`, `DrawTextExA/W`, `TabbedTextOutA/W`
- `FillRect`, `DrawEdge`, `DrawFrameControl`, `DrawCaption`
- `FrameRect`, `InvertRect`
- `GetMenu`, `SetMenu`, `GetSubMenu`, `GetMenuItemInfoA/W`, `GetMenuStringA/W`
- `AppendMenuA/W`, `InsertMenuItemA/W`, `DeleteMenu`, `DestroyMenu`, `CreateMenu`, `CreatePopupMenu`, `TrackPopupMenu`
- `EnableMenuItem`, `CheckMenuItem`, `CheckMenuRadioItem`
- `DrawMenuBar`
- `WindowFromPoint`, `ChildWindowFromPoint`, `ChildWindowFromPointEx`
- `FindWindowA/W`, `FindWindowExA/W`
- `EnumWindows`, `EnumChildWindows`, `EnumThreadWindows`
- `GetDesktopWindow`
- `SetForegroundWindow`, `GetForegroundWindow`
- `FlashWindow`, `FlashWindowEx`
- `OpenIcon`, `CloseWindow`
- `LockWindowUpdate`
- `CreateCaret`, `ShowCaret`, `HideCaret`, `SetCaretPos`, `GetCaretPos`, `DestroyCaret`
- `GetCursorPos`, `SetCursorPos`, `SetCursor`, `GetCursor`, `ShowCursor`, `LoadCursorFromFileA/W`
- `ClipCursor`, `GetClipCursor`
- `MoveWindow`, `GetWindowRect` (has implementation), `SetWindowPos` (has implementation)
- `CascadeWindows`, `TileWindows`

---

## ADVAPI32.dll — ~40% coverage, registry is minimal

**Registry missing:**
- `RegDeleteKeyA` / `RegDeleteKeyW`
- `RegEnumKeyExA` / `RegEnumKeyExW`
- `RegEnumValueA` / `RegEnumValueW`
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
- `OpenThreadToken` / `OpenProcessToken` (has implementation)
- `AdjustTokenPrivileges` (has implementation)
- `LookupPrivilegeValueW` (has implementation)

---

## KERNEL32.dll — ~90% coverage, mostly complete

**Missing / stub:**
- `DuplicateHandle` — returns 0 / last_error=0
- `GetThreadPriority` — returns 0
- `DeviceIoControl` — returns FALSE (comment: "unsupported")
- `FindFirstStreamW` / `FindNextStreamW` — extra dispatch, basic
- `GetCompressedFileSizeA/W`
- `GetVolumeInformationA/W`
- `GetDiskFreeSpaceA/W` / `GetDiskFreeSpaceExA/W`
- `SetEndOfFile`
- `SetFileValidData`
- `LockFile` / `UnlockFile`
- `GetFileAttributesExA/W`
- `BackupRead` / `BackupSeek` / `BackupWrite`
- `ReadFileScatter` / `WriteFileGather`
- `GetLongPathNameA/W` / `GetShortPathNameA/W`
- `GetTempFileNameA/W` / `GetTempPathA/W` / `SetTempPathA`
- `GetWindowsDirectoryA/W` / `GetSystemDirectoryA/W` / `GetSystemWindowsDirectoryA/W`
- `GetComputerNameA/W` / `GetComputerNameExA/W`
- `GetUserNameA/W` / `GetUserNameExA/W`
- `GetUserProfileDirectoryA/W`
- `SHGetFolderPath` equivalents — may rely on SHELL32
- `OpenThread`, `OpenProcess` (OpenProcess has implementation)
- `CreateJobObjectA/W`, `AssignProcessToJobObject`, `SetInformationJobObject`, `QueryInformationJobObject`
- `CreateNamedPipeA/W`, `ConnectNamedPipe`, `DisconnectNamedPipe`, `CallNamedPipe`, `WaitNamedPipe`
- `CreateFileMappingA/W` (has basic implementation for anonymous), `OpenFileMappingA/W`, `MapViewOfFile`, `UnmapViewOfFile`, `FlushViewOfFile`
- SetErrorMode, SetThreadErrorMode
- `DebugBreak`, `IsDebuggerPresent`, `OutputDebugStringA/W`
- `QueryFullProcessImageNameA/W`
- `GetProcessTimes`, `GetThreadTimes`, `GetSystemTimes`
- `GetProcessIoCounters`
- `GetProcessMemoryInfo` (PSAPI)
- `EnumProcessModules` (PSAPI)
- `TerminateProcess` / `TerminateThread`
- `SuspendThread` / `ResumeThread`
- SignalObjectAndWait
- `SetThreadAffinityMask`, `SetThreadIdealProcessor`
- `SetProcessPriorityBoost`, `GetProcessPriorityBoost`
- `SetThreadPriorityBoost`, `GetThreadPriorityBoost`
- `CreateSemaphoreExA/W`

---

## COMCTL32.dll — ~60% coverage mostly ImageList

**Implemented:**
- Basic ImageList functions (Create, Destroy, GetImageCount, SetImageCount, Add, ReplaceIcon, Remove, GetIconSize, SetIconSize, GetIcon, SetOverlayImage, BeginDrag, EndDrag, DragEnter, DragLeave, DragMove, GetDragImage, SetDragCursorImage)

**Missing:**
- `InitCommonControls` / `InitCommonControlsEx` (may be stubs)
- All toolbar APIs: `CreateToolbarEx`, `CommandBar_*`, `Toolbar_Set*`
- All status bar APIs: `CreateStatusWindow`, `SB_SetText`, `SB_GetText`, `SB_SetParts`
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
- `DLLGetVersion`

---

## UCRT.dll — ~80% coverage

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
- `_time64`, `_localtime64_s`, `_gmtime64_s`, `_mktime64`, `_difftime64`
- `_strftime_l`, `_wcsftime_l`, `_date`, `_strdate`, `_time`, `_strtime`, `_tzset`
- `_beginthread`, `_beginthreadex` (has implementation), `_endthread`, `_endthreadex` (has implementation)
- `_execute_onexit_table`, `_initialize_onexit_table`, `_register_onexit_function`
- `_CrtDbgReport` (debug CRT)
- `_controlfp`, `_control87`, `_statusfp`, `_clearfp`
- `_fpieee_flt`
- `_set_invalid_parameter_handler`, `_set_thread_local_invalid_parameter_handler`
- `_seh_filter_dll`, `_seh_filter_exe`
- `_set_se_translator`
- `_set_app_type`

---

## GDI32.dll — additional specialty missing

Beyond the render stubs above:
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
- `SetBkColor`, `SetBkMode`, `SetTextColor`, `SetTextAlign`, `SetTextCharacterExtra`, `SetTextJustification`
- `GetBkColor`, `GetBkMode`, `GetTextColor`, `GetTextAlign`, `GetTextCharacterExtra`, `GetTextMetrics`
- `SetArcDirection`, `GetArcDirection`
- `SetMiterLimit`, `GetMiterLimit`
- `SetPolyFillMode`, `GetPolyFillMode`
- `SetROP2`, `GetROP2`
- `SetStretchBltMode`, `GetStretchBltMode`
- `SetBrushOrgEx`, `GetBrushOrgEx`
- `SetPixelFormat`, `DescribePixelFormat`, `ChoosePixelFormat`, `GetPixelFormat`, `SwapBuffers` (OpenGL interop)
