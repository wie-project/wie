use super::config::GuestStubConfig;
use crate::asm_utils::patch_rel8;

/// Encoding context for the guest-stub machine-code bodies.
///
/// Receiver policy: the encoders share the [`GuestStubConfig`] VA table and
/// the output buffer they append to, so they live as methods on a
/// [`StubCtx`] instead of free functions that each thread `buf`/`config`
/// through their signatures. The config carries the callee VAs the bodies
/// embed (modal-loop callees, FLS table, cwd blob); genuinely per-call VAs
/// (the resolved `CreateDialogParamA/W` choice) stay as method parameters.
pub(crate) struct StubCtx<'a> {
    buf: &'a mut Vec<u8>,
    config: &'a GuestStubConfig,
}

impl<'a> StubCtx<'a> {
    /// Creates a context that appends encoded bytes into `buf` with the VA
    /// config in scope.
    pub(crate) fn new(buf: &'a mut Vec<u8>, config: &'a GuestStubConfig) -> Self {
        Self { buf, config }
    }

    /// `_initterm` / `_initterm_e` body — iterate function-pointer range and call.
    ///
    /// Win64: `RCX=first`, `RDX=last` (half-open). Each entry is `void (*)()` or
    /// `int (*)()`; NULL entries are skipped. For `_initterm_e`, a non-zero return
    /// aborts the loop and is returned to the caller.
    pub(super) fn encode_initterm(&mut self, check_status: bool) {
        // push rbx; push rsi
        // mov rbx, rcx          ; cur
        // mov rsi, rdx          ; end
        // .loop:
        //   cmp rbx, rsi
        //   jae .done
        //   mov rax, qword ptr [rbx]
        //   add rbx, 8
        //   test rax, rax
        //   jz .loop
        //   sub rsp, 0x28
        //   call rax
        //   add rsp, 0x28
        //   ; if check_status: test eax,eax; jnz .fail_ret
        //   jmp .loop
        // .done:
        //   xor eax, eax
        //   pop rsi; pop rbx; ret
        // .fail_ret: (initterm_e only)
        //   pop rsi; pop rbx; ret   ; eax already holds status
        self.buf.reserve(64);
        self.buf.extend_from_slice(&[0x53, 0x56]); // push rbx; push rsi
        self.buf.extend_from_slice(&[0x48, 0x89, 0xcb]); // mov rbx, rcx
        self.buf.extend_from_slice(&[0x48, 0x89, 0xd6]); // mov rsi, rdx
        let loop_at = self.buf.len();
        self.buf.extend_from_slice(&[0x48, 0x39, 0xf3]); // cmp rbx, rsi
        let jae_imm = self.buf.len() + 1;
        self.buf.extend_from_slice(&[0x73, 0x00]); // jae .done (patch)
        self.buf.extend_from_slice(&[0x48, 0x8b, 0x03]); // mov rax, [rbx]
        self.buf.extend_from_slice(&[0x48, 0x83, 0xc3, 0x08]); // add rbx, 8
        self.buf.extend_from_slice(&[0x48, 0x85, 0xc0]); // test rax, rax
        let jz_imm = self.buf.len() + 1;
        self.buf.extend_from_slice(&[0x74, 0x00]); // jz .loop (patch)
        self.buf.extend_from_slice(&[0x48, 0x83, 0xec, 0x28]); // sub rsp, 0x28
        self.buf.extend_from_slice(&[0xff, 0xd0]); // call rax
        self.buf.extend_from_slice(&[0x48, 0x83, 0xc4, 0x28]); // add rsp, 0x28
        if check_status {
            self.buf.extend_from_slice(&[0x85, 0xc0]); // test eax, eax
            let jnz_imm = self.buf.len() + 1;
            self.buf.extend_from_slice(&[0x75, 0x00]); // jnz .fail_ret (patch)
            // jmp .loop
            let jmp_imm = self.buf.len() + 1;
            self.buf.extend_from_slice(&[0xeb, 0x00]);
            let done_at = self.buf.len();
            self.buf.extend_from_slice(&[0x31, 0xc0]); // xor eax, eax
            self.buf.extend_from_slice(&[0x5e, 0x5b, 0xc3]); // pop rsi; pop rbx; ret
            let fail_at = self.buf.len();
            self.buf.extend_from_slice(&[0x5e, 0x5b, 0xc3]); // pop rsi; pop rbx; ret (keep eax)
            patch_rel8(self.buf, jae_imm, jae_imm + 1, done_at);
            patch_rel8(self.buf, jz_imm, jz_imm + 1, loop_at);
            patch_rel8(self.buf, jnz_imm, jnz_imm + 1, fail_at);
            patch_rel8(self.buf, jmp_imm, jmp_imm + 1, loop_at);
        } else {
            // jmp .loop
            let jmp_imm = self.buf.len() + 1;
            self.buf.extend_from_slice(&[0xeb, 0x00]);
            let done_at = self.buf.len();
            self.buf.extend_from_slice(&[0x31, 0xc0]); // xor eax, eax
            self.buf.extend_from_slice(&[0x5e, 0x5b, 0xc3]); // pop rsi; pop rbx; ret
            patch_rel8(self.buf, jae_imm, jae_imm + 1, done_at);
            patch_rel8(self.buf, jz_imm, jz_imm + 1, loop_at);
            patch_rel8(self.buf, jmp_imm, jmp_imm + 1, loop_at);
        }
    }

    /// Shared `GetMessageA` → `IsDialogMessageA` → `DispatchMessageA` modal-loop
    /// body, appended to the context's buffer after a prologue that stored the
    /// dialog HWND in `[rsp+0x20]` and set up the 0x60-byte stack frame.
    ///
    /// ```text
    /// .loop:
    ///   lea rcx, [rsp+0x28]      ; lpMsg
    ///   xor edx, edx             ; hWnd = NULL — real modal loops pull every
    ///                            ; thread message (the owner's WM_TIMER must
    ///                            ; still be dispatched inside the dialog)
    ///   xor r8d, r8d; xor r9d, r9d
    ///   call GetMessageA
    ///   test eax, eax; jz .quit
    ///   mov rcx, [rsp+0x20]; lea rdx, [rsp+0x28]
    ///   call IsDialogMessageA
    ///   test eax, eax; jnz .loop ; consumed (Tab/Enter/Esc) → keep going
    ///   lea rcx, [rsp+0x28]
    ///   call DispatchMessageA
    ///   jmp .loop
    /// .quit:
    /// mov rax, result_va        ; EndDialog wrote the result here
    /// mov eax, [rax]
    /// ```
    ///
    /// The `.done` epilogue (`add rsp, 0x60; pop rbx; ret`) is appended by the
    /// caller immediately after this returns, so the caller can take `.done` as
    /// `buf.len()` at return time. All three rel8 branches (`.quit`, `.loop` ×2)
    /// target offsets inside the appended bytes, so the patches are
    /// self-contained.
    ///
    /// Returns `(loop_at, quit_at)` — the absolute offsets of `.loop` and `.quit`
    /// in the buffer, for callers that need programmatic branch targets (both
    /// current encoders append the loop purely for its side effect).
    fn append_modal_loop(&mut self) -> (usize, usize) {
        // .loop:
        let loop_at = self.buf.len();
        // lea rcx, [rsp+0x28] ; xor edx, edx ; xor r8d, r8d ; xor r9d, r9d
        self.buf.extend_from_slice(&[0x48, 0x8d, 0x4c, 0x24, 0x28]);
        self.buf.extend_from_slice(&[0x31, 0xd2]);
        self.buf.extend_from_slice(&[0x45, 0x31, 0xc0]);
        self.buf.extend_from_slice(&[0x45, 0x31, 0xc9]);
        // call GetMessageA
        self.buf.extend_from_slice(&[0x48, 0xb8]);
        self.buf
            .extend_from_slice(&self.config.get_message_a_va.to_le_bytes());
        self.buf.extend_from_slice(&[0xff, 0xd0]);
        // test eax, eax ; jz .quit
        self.buf.extend_from_slice(&[0x85, 0xc0]);
        let jz_quit = self.buf.len() + 1;
        self.buf.extend_from_slice(&[0x74, 0x00]);
        // mov rcx, [rsp+0x20] ; lea rdx, [rsp+0x28] ; call IsDialogMessageA
        self.buf.extend_from_slice(&[0x48, 0x8b, 0x4c, 0x24, 0x20]);
        self.buf.extend_from_slice(&[0x48, 0x8d, 0x54, 0x24, 0x28]);
        self.buf.extend_from_slice(&[0x48, 0xb8]);
        self.buf
            .extend_from_slice(&self.config.is_dialog_message_a_va.to_le_bytes());
        self.buf.extend_from_slice(&[0xff, 0xd0]);
        // test eax, eax ; jnz .loop (consumed by IsDialogMessageA)
        self.buf.extend_from_slice(&[0x85, 0xc0]);
        let jnz_loop = self.buf.len() + 1;
        self.buf.extend_from_slice(&[0x75, 0x00]);
        // lea rcx, [rsp+0x28] ; call DispatchMessageA ; jmp .loop
        self.buf.extend_from_slice(&[0x48, 0x8d, 0x4c, 0x24, 0x28]);
        self.buf.extend_from_slice(&[0x48, 0xb8]);
        self.buf
            .extend_from_slice(&self.config.dispatch_message_a_va.to_le_bytes());
        self.buf.extend_from_slice(&[0xff, 0xd0]);
        let jmp_loop = self.buf.len() + 1;
        self.buf.extend_from_slice(&[0xeb, 0x00]);
        // .quit: mov rax, result_va ; mov eax, [rax]
        let quit_at = self.buf.len();
        self.buf.extend_from_slice(&[0x48, 0xb8]);
        self.buf
            .extend_from_slice(&self.config.dialog_result_va.to_le_bytes());
        self.buf.extend_from_slice(&[0x8b, 0x00]);

        patch_rel8(self.buf, jz_quit, jz_quit + 1, quit_at);
        patch_rel8(self.buf, jnz_loop, jnz_loop + 1, loop_at);
        patch_rel8(self.buf, jmp_loop, jmp_loop + 1, loop_at);
        (loop_at, quit_at)
    }

    /// `DialogBoxParamA/W` modal-loop body (out-of-line helper).
    ///
    /// Win64 entry: `RCX=hInstance, RDX=lpTemplateName, R8=hWndParent,
    /// R9=lpDialogFunc, [rsp+0x28]=dwInitParam`.
    ///
    /// ```text
    /// push rbx; sub rsp, 0x60     ; rbx = alignment pad (preserved)
    /// mov rax, [rsp+0x90]        ; dwInitParam (caller's 5th arg)
    /// mov [rsp+0x20], rax        ; 5th arg slot for CreateDialogParam — the
    ///                            ; caller places arg5 at [rsp+0x20] so the
    ///                            ; callee reads it at [rsp+0x28] after the
    ///                            ; call pushes the return address
    /// call CreateDialogParamA/W  ; host: build dialog, WM_INITDIALOG bridge
    /// test rax, rax; jnz .created
    ///   mov rax, -1; jmp .done   ; creation failed → DialogBoxParam returns -1
    /// .created:
    /// mov [rsp+0x20], rax        ; hwnd lives in OUR frame — the WM_INITDIALOG
    ///                            ; guest callback clobbers every register, and
    ///                            ; its frame sits BELOW ours, so the stack slot
    ///                            ; survives
    /// .loop: … .quit:
    /// (see [`Self::append_modal_loop`] — shared with the file-dialog loop)
    /// .done:
    /// add rsp, 0x60; pop rbx; ret
    /// ```
    ///
    /// `[rsp+0x90]` = the caller's 5th arg: entry `rsp` = R0, after `push rbx`
    /// (8) + `sub rsp, 0x60` the frame is at R0-0x68, and the caller's stack arg
    /// sits at `[R0+0x28]` = `[rsp+0x68+0x28]` = `[rsp+0x90]`.
    pub(super) fn encode_dialog_box_param(&mut self, create_dialog_param_va: u64) {
        self.buf.reserve(180);
        // push rbx ; sub rsp, 0x60
        self.buf.extend_from_slice(&[0x53]);
        self.buf.extend_from_slice(&[0x48, 0x83, 0xec, 0x60]);
        // mov rax, [rsp+0x90] ; mov [rsp+0x20], rax
        self.buf
            .extend_from_slice(&[0x48, 0x8b, 0x84, 0x24, 0x90, 0x00, 0x00, 0x00]);
        self.buf.extend_from_slice(&[0x48, 0x89, 0x44, 0x24, 0x20]);
        // call CreateDialogParamA/W
        self.buf.extend_from_slice(&[0x48, 0xb8]);
        self.buf
            .extend_from_slice(&create_dialog_param_va.to_le_bytes());
        self.buf.extend_from_slice(&[0xff, 0xd0]);
        // test rax, rax ; jnz .created
        self.buf.extend_from_slice(&[0x48, 0x85, 0xc0]);
        let jnz_created = self.buf.len() + 1;
        self.buf.extend_from_slice(&[0x75, 0x00]);
        // mov rax, -1 ; jmp .done
        self.buf
            .extend_from_slice(&[0x48, 0xc7, 0xc0, 0xff, 0xff, 0xff, 0xff]);
        let jmp_done = self.buf.len() + 1;
        self.buf.extend_from_slice(&[0xeb, 0x00]);
        // .created: mov [rsp+0x20], rax  (hwnd slot)
        let created_at = self.buf.len();
        self.buf.extend_from_slice(&[0x48, 0x89, 0x44, 0x24, 0x20]);
        // .loop → .quit: shared GetMessage/IsDialogMessage/Dispatch modal loop
        let (_loop_at, _quit_at) = self.append_modal_loop();
        // .done: add rsp, 0x60 ; pop rbx ; ret
        let done_at = self.buf.len();
        self.buf.extend_from_slice(&[0x48, 0x83, 0xc4, 0x60]);
        self.buf.push(0x5b);
        self.buf.push(0xc3);

        patch_rel8(self.buf, jnz_created, jnz_created + 1, created_at);
        patch_rel8(self.buf, jmp_done, jmp_done + 1, done_at);
    }

    /// `GetOpenFileName`/`GetSaveFileName` modal-loop body (out-of-line helper).
    ///
    /// The comdlg32 handler builds the file dialog and stores its HWND in the
    /// callback-entry `RCX`; the body runs the shared modal message loop
    /// [`Self::append_modal_loop`] — `GetMessageA` → `IsDialogMessageA` →
    /// `DispatchMessageA` until `WM_QUIT` (posted by `EndDialog`), then returns
    /// the dialog-result slot (the `GetOpenFileName` TRUE/FALSE the guest sees).
    ///
    /// ```text
    /// push rbx; sub rsp, 0x60
    /// mov [rsp+0x20], rcx        ; dialog hwnd from the callback frame
    /// .loop: … .quit:            ; append_modal_loop (shared with the dialog box)
    /// .done:
    /// add rsp, 0x60; pop rbx; ret
    /// ```
    pub(crate) fn encode_file_dialog_loop(&mut self) {
        self.buf.reserve(170);
        // push rbx ; sub rsp, 0x60
        self.buf.extend_from_slice(&[0x53]);
        self.buf.extend_from_slice(&[0x48, 0x83, 0xec, 0x60]);
        // mov [rsp+0x20], rcx (dialog hwnd from the callback frame)
        self.buf.extend_from_slice(&[0x48, 0x89, 0x4c, 0x24, 0x20]);
        // .loop → .quit: shared GetMessage/IsDialogMessage/Dispatch modal loop
        let (_loop_at, _quit_at) = self.append_modal_loop();
        // .done: add rsp, 0x60 ; pop rbx ; ret (fall-through after .quit)
        self.buf.extend_from_slice(&[0x48, 0x83, 0xc4, 0x60]);
        self.buf.push(0x5b);
        self.buf.push(0xc3);
    }
}

/// File-dialog proc stub body (out-of-line helper).
///
/// The comdlg32 handler builds the file-dialog window with this address as its
/// `dialog_proc`, so every non-`WM_PAINT` message bridges here with the WndProc
/// convention (`rcx`=hwnd, `rdx`=message, `r8`=wParam, `r9`=lParam):
///
/// ```text
/// mov eax, edx               ; message
/// cmp eax, 0x0010 (WM_CLOSE); je .cancel
/// cmp eax, 0x0111 (WM_COMMAND); jne .zero
/// mov eax, r8d; and eax, 0xFFFF   ; control id (low word of wParam)
/// cmp eax, 1 (IDOK); je .ok
/// cmp eax, 2 (IDCANCEL); je .cancel
/// cmp eax, strikeout_id; je .strikeout
/// cmp eax, underline_id; je .underline
/// jmp .zero
/// .cancel: xor edx, edx; jmp .close
/// .ok: mov edx, 1
/// .strikeout: mov edx, strikeout_id; jmp .close
/// .underline: mov edx, underline_id
/// .close: sub rsp, 0x28; call EndDialog(hwnd=rcx, result=rdx); add rsp, 0x28
/// .zero: xor eax, eax; ret
/// ```
///
/// The stub is shared by the file dialog and the font dialog (`ChooseFontW`).
/// The `EndDialog` handler performs the `OPENFILENAME` write-back for the file
/// dialog and the `CHOOSEFONTW`/`LOGFONTW` write-back for the font dialog.
/// The font dialog's Strikeout/Underline effects buttons carry the sentinel
/// ids `wie_winapi::comdlg32::FONT_DLG_*_ID`; the stub ends the dialog with
/// those ids as the result, and the `EndDialog` handler turns them into
/// checkbox toggles (the dialog stays open) instead of closing. Any other
/// message (or unknown id) is ignored (`0`), matching DefDlgProc's
/// pass-through.
pub(crate) fn encode_file_dialog_proc(end_dialog_va: u64) -> Vec<u8> {
    let strikeout_id = u32::try_from(wie_winapi::comdlg32::FONT_DLG_STRIKEOUT_ID).unwrap_or(0);
    let underline_id = u32::try_from(wie_winapi::comdlg32::FONT_DLG_UNDERLINE_ID).unwrap_or(0);
    let mut buf = Vec::with_capacity(140);
    // mov eax, edx (message)
    buf.extend_from_slice(&[0x89, 0xd0]);
    // cmp eax, 0x0010 (WM_CLOSE); je .cancel
    buf.extend_from_slice(&[0x3d, 0x10, 0x00, 0x00, 0x00]);
    let je_cancel = buf.len() + 1;
    buf.extend_from_slice(&[0x74, 0x00]);
    // cmp eax, 0x0111 (WM_COMMAND); jne .zero
    buf.extend_from_slice(&[0x3d, 0x11, 0x01, 0x00, 0x00]);
    let jne_zero = buf.len() + 1;
    buf.extend_from_slice(&[0x75, 0x00]);
    // mov eax, r8d ; and eax, 0xFFFF (control id)
    buf.extend_from_slice(&[0x44, 0x89, 0xc0]);
    buf.extend_from_slice(&[0x25, 0xff, 0xff, 0x00, 0x00]);
    // cmp eax, 1 (IDOK); je .ok
    buf.extend_from_slice(&[0x83, 0xf8, 0x01]);
    let je_ok = buf.len() + 1;
    buf.extend_from_slice(&[0x74, 0x00]);
    // cmp eax, 2 (IDCANCEL); je .cancel
    buf.extend_from_slice(&[0x83, 0xf8, 0x02]);
    let je_cancel_id = buf.len() + 1;
    buf.extend_from_slice(&[0x74, 0x00]);
    // cmp eax, strikeout_id; je .strikeout
    buf.extend_from_slice(&[0x3d]);
    buf.extend_from_slice(&strikeout_id.to_le_bytes());
    let je_strikeout = buf.len() + 1;
    buf.extend_from_slice(&[0x74, 0x00]);
    // cmp eax, underline_id; je .underline
    buf.extend_from_slice(&[0x3d]);
    buf.extend_from_slice(&underline_id.to_le_bytes());
    let je_underline = buf.len() + 1;
    buf.extend_from_slice(&[0x74, 0x00]);
    // jmp .zero (unknown id)
    let jmp_zero = buf.len() + 1;
    buf.extend_from_slice(&[0xeb, 0x00]);
    // .cancel: xor edx, edx ; jmp .close
    let cancel_at = buf.len();
    buf.extend_from_slice(&[0x31, 0xd2]);
    let jmp_close = buf.len() + 1;
    buf.extend_from_slice(&[0xeb, 0x00]);
    // .ok: mov edx, 1 ; jmp .close
    //
    // The `jmp` is load-bearing: without it the OK branch falls through into
    // the Strikeout branch below, `mov edx, strikeout_id` overwrites the IDOK
    // result, and `EndDialog` receives the strikeout sentinel — the font
    // dialog's OK then toggles the Strikeout checkbox instead of closing
    // (ghost-modal: the first File→Exit after "OK" is eaten by the still-open
    // modal loop).
    let ok_at = buf.len();
    buf.extend_from_slice(&[0xba, 0x01, 0x00, 0x00, 0x00]);
    let jmp_ok_close = buf.len() + 1;
    buf.extend_from_slice(&[0xeb, 0x00]);
    // .strikeout: mov edx, strikeout_id ; jmp .close
    let strikeout_at = buf.len();
    buf.extend_from_slice(&[0xba]);
    buf.extend_from_slice(&strikeout_id.to_le_bytes());
    let jmp_strikeout_close = buf.len() + 1;
    buf.extend_from_slice(&[0xeb, 0x00]);
    // .underline: mov edx, underline_id (falls through into .close)
    let underline_at = buf.len();
    buf.extend_from_slice(&[0xba]);
    buf.extend_from_slice(&underline_id.to_le_bytes());
    // .close: sub rsp, 0x28 ; mov rax, end_dialog_va ; call rax ; add rsp, 0x28
    let close_at = buf.len();
    buf.extend_from_slice(&[0x48, 0x83, 0xec, 0x28]);
    buf.extend_from_slice(&[0x48, 0xb8]);
    buf.extend_from_slice(&end_dialog_va.to_le_bytes());
    buf.extend_from_slice(&[0xff, 0xd0]);
    buf.extend_from_slice(&[0x48, 0x83, 0xc4, 0x28]);
    // .zero: xor eax, eax ; ret
    let zero_at = buf.len();
    buf.extend_from_slice(&[0x31, 0xc0]);
    buf.push(0xc3);

    patch_rel8(&mut buf, je_cancel, je_cancel + 1, cancel_at);
    patch_rel8(&mut buf, jne_zero, jne_zero + 1, zero_at);
    patch_rel8(&mut buf, je_ok, je_ok + 1, ok_at);
    patch_rel8(&mut buf, je_cancel_id, je_cancel_id + 1, cancel_at);
    patch_rel8(&mut buf, je_strikeout, je_strikeout + 1, strikeout_at);
    patch_rel8(&mut buf, je_underline, je_underline + 1, underline_at);
    patch_rel8(&mut buf, jmp_zero, jmp_zero + 1, zero_at);
    patch_rel8(&mut buf, jmp_close, jmp_close + 1, close_at);
    patch_rel8(&mut buf, jmp_ok_close, jmp_ok_close + 1, close_at);
    patch_rel8(
        &mut buf,
        jmp_strikeout_close,
        jmp_strikeout_close + 1,
        close_at,
    );
    buf
}

impl<'a> StubCtx<'a> {
    pub(super) fn encode_fls_get(&mut self, max_slots: u32) {
        self.buf.reserve(32);
        self.buf.extend_from_slice(&[0x48, 0x81, 0xf9]);
        self.buf.extend_from_slice(&max_slots.to_le_bytes());
        let jae_imm = self.buf.len() + 1;
        self.buf.extend_from_slice(&[0x73, 0x00]);
        self.buf.extend_from_slice(&[0x48, 0xb8]);
        self.buf
            .extend_from_slice(&self.config.fls_table_va.to_le_bytes());
        self.buf.extend_from_slice(&[0x48, 0x8b, 0x04, 0xc8]);
        self.buf.push(0xc3);
        let zero_at = self.buf.len();
        self.buf.extend_from_slice(&[0x31, 0xc0, 0xc3]);
        patch_rel8(self.buf, jae_imm, jae_imm + 1, zero_at);
    }

    pub(super) fn encode_fls_set(&mut self, max_slots: u32) {
        self.buf.reserve(40);
        self.buf.extend_from_slice(&[0x48, 0x81, 0xf9]);
        self.buf.extend_from_slice(&max_slots.to_le_bytes());
        let jae_imm = self.buf.len() + 1;
        self.buf.extend_from_slice(&[0x73, 0x00]);
        self.buf.extend_from_slice(&[0x48, 0xb8]);
        self.buf
            .extend_from_slice(&self.config.fls_table_va.to_le_bytes());
        self.buf.extend_from_slice(&[0x48, 0x89, 0x14, 0xc8]);
        self.buf.extend_from_slice(&[0xb8, 0x01, 0x00, 0x00, 0x00]);
        self.buf.push(0xc3);
        let fail_at = self.buf.len();
        self.buf.extend_from_slice(&[0x31, 0xc0, 0xc3]);
        patch_rel8(self.buf, jae_imm, jae_imm + 1, fail_at);
    }
}

pub(super) fn encode_load_u32_table(table_va: u64, max_index: u32) -> Vec<u8> {
    // cmp rcx, max ; jae .zero
    // mov rax, table ; mov eax, [rax+rcx*4] ; ret
    // .zero: xor eax,eax ; ret
    let mut buf = Vec::with_capacity(32);
    buf.extend_from_slice(&[0x48, 0x81, 0xf9]);
    buf.extend_from_slice(&max_index.to_le_bytes());
    let jae_imm = buf.len() + 1;
    buf.extend_from_slice(&[0x73, 0x00]);
    buf.extend_from_slice(&[0x48, 0xb8]);
    buf.extend_from_slice(&table_va.to_le_bytes());
    buf.extend_from_slice(&[0x8b, 0x04, 0x88]); // mov eax, [rax+rcx*4]
    buf.push(0xc3);
    let zero_at = buf.len();
    buf.extend_from_slice(&[0x31, 0xc0, 0xc3]);
    patch_rel8(&mut buf, jae_imm, jae_imm + 1, zero_at);
    buf
}

/// `GetSystemTimeAsFileTime` / `QueryPerformanceCounter` body: copy one u64
/// clock-table slot through the guest pointer in RCX (out-of-line helper).
///
/// ```text
/// mov rax, slot_va       ; mov rdx, [rax]  ; load the table slot
/// test rcx, rcx          ; NULL pointer → skip the store (host semantics)
/// jz  .skip
/// mov [rcx], rdx
/// .skip: (mov eax, 1 ;) ret     ; ret_one → QPC/QPF returns TRUE
/// ```
pub(super) fn encode_copy_u64_to_ptr(slot_va: u64, ret_one: bool) -> Vec<u8> {
    let mut buf = Vec::with_capacity(32);
    buf.extend_from_slice(&[0x48, 0xb8]); // mov rax, imm64
    buf.extend_from_slice(&slot_va.to_le_bytes());
    buf.extend_from_slice(&[0x48, 0x8b, 0x10]); // mov rdx, [rax]
    buf.extend_from_slice(&[0x48, 0x85, 0xc9]); // test rcx, rcx
    let jz_imm = buf.len() + 1;
    buf.extend_from_slice(&[0x74, 0x00]); // jz .skip (patched)
    buf.extend_from_slice(&[0x48, 0x89, 0x11]); // mov [rcx], rdx
    let skip_at = buf.len();
    if ret_one {
        buf.extend_from_slice(&[0xb8, 0x01, 0x00, 0x00, 0x00]); // mov eax, 1
    }
    buf.push(0xc3);
    patch_rel8(&mut buf, jz_imm, jz_imm + 1, skip_at);
    buf
}

impl<'a> StubCtx<'a> {
    /// `GetCurrentDirectoryW` per Microsoft Learn:
    /// - success: return chars written **excluding** NUL
    /// - buffer too small / size query: return required size **including** NUL
    /// - size query: `nBufferLength == 0` and `lpBuffer == NULL` (we also treat
    ///   null buffer as size query, matching common app patterns and host)
    pub(super) fn encode_get_current_directory_w(&mut self) {
        // Layout: [u32 char_count][u16 path...][u16 0]
        // RCX = nBufferLength, RDX = lpBuffer
        //
        // mov r8, cwd_blob
        // mov eax, [r8]              ; char_count
        // lea r9d, [eax+1]           ; required_with_nul
        // test rdx, rdx
        // jz .need_size
        // test rcx, rcx
        // jz .need_size
        // cmp rcx, rax               ; need length > char_count (room for NUL)
        // jbe .need_size
        // ; copy (char_count+1) UTF-16 units to [rdx]
        // push rsi / rdi
        // lea rsi, [r8+4]
        // mov rdi, rdx
        // lea ecx, [eax+1]
        // rep movsw
        // pop rdi / rsi
        // ; eax still char_count (rep uses ecx only)
        // ret
        // .need_size: mov eax, r9d ; ret
        self.buf.reserve(80);
        self.buf.extend_from_slice(&[0x49, 0xb8]); // mov r8, imm64
        self.buf
            .extend_from_slice(&self.config.cwd_blob_va.to_le_bytes());
        self.buf.extend_from_slice(&[0x41, 0x8b, 0x00]); // mov eax, [r8]
        self.buf.extend_from_slice(&[0x44, 0x8d, 0x48, 0x01]); // lea r9d, [rax+1]
        self.buf.extend_from_slice(&[0x48, 0x85, 0xd2]); // test rdx, rdx
        let jz1 = self.buf.len() + 1;
        self.buf.extend_from_slice(&[0x74, 0x00]); // jz .need_size
        self.buf.extend_from_slice(&[0x48, 0x85, 0xc9]); // test rcx, rcx
        let jz2 = self.buf.len() + 1;
        self.buf.extend_from_slice(&[0x74, 0x00]);
        self.buf.extend_from_slice(&[0x48, 0x39, 0xc1]); // cmp rcx, rax
        let jbe = self.buf.len() + 1;
        self.buf.extend_from_slice(&[0x76, 0x00]); // jbe .need_size
        // preserve RSI/RDI (Win64 non-volatiles)
        self.buf.extend_from_slice(&[0x56, 0x57]); // push rsi, rdi
        self.buf.extend_from_slice(&[0x49, 0x8d, 0x70, 0x04]); // lea rsi, [r8+4]
        self.buf.extend_from_slice(&[0x48, 0x89, 0xd7]); // mov rdi, rdx
        self.buf.extend_from_slice(&[0x8d, 0x48, 0x01]); // lea ecx, [rax+1]
        self.buf.extend_from_slice(&[0xf3, 0xa5]); // rep movsw
        self.buf.extend_from_slice(&[0x5f, 0x5e]); // pop rdi, rsi
        // restore eax = char_count (destroyed by lea ecx if we only used eax — eax intact)
        self.buf.push(0xc3);
        let need_size = self.buf.len();
        self.buf.extend_from_slice(&[0x44, 0x89, 0xc8]); // mov eax, r9d
        self.buf.push(0xc3);
        patch_rel8(self.buf, jz1, jz1 + 1, need_size);
        patch_rel8(self.buf, jz2, jz2 + 1, need_size);
        patch_rel8(self.buf, jbe, jbe + 1, need_size);
    }
}
