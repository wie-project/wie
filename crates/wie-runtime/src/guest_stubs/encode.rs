use crate::asm_utils::patch_rel8;

/// `_initterm` / `_initterm_e` body — iterate function-pointer range and call.
///
/// Win64: `RCX=first`, `RDX=last` (half-open). Each entry is `void (*)()` or
/// `int (*)()`; NULL entries are skipped. For `_initterm_e`, a non-zero return
/// aborts the loop and is returned to the caller.
pub(super) fn encode_initterm(check_status: bool) -> Vec<u8> {
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
    let mut buf = Vec::with_capacity(64);
    buf.extend_from_slice(&[0x53, 0x56]); // push rbx; push rsi
    buf.extend_from_slice(&[0x48, 0x89, 0xcb]); // mov rbx, rcx
    buf.extend_from_slice(&[0x48, 0x89, 0xd6]); // mov rsi, rdx
    let loop_at = buf.len();
    buf.extend_from_slice(&[0x48, 0x39, 0xf3]); // cmp rbx, rsi
    let jae_imm = buf.len() + 1;
    buf.extend_from_slice(&[0x73, 0x00]); // jae .done (patch)
    buf.extend_from_slice(&[0x48, 0x8b, 0x03]); // mov rax, [rbx]
    buf.extend_from_slice(&[0x48, 0x83, 0xc3, 0x08]); // add rbx, 8
    buf.extend_from_slice(&[0x48, 0x85, 0xc0]); // test rax, rax
    let jz_imm = buf.len() + 1;
    buf.extend_from_slice(&[0x74, 0x00]); // jz .loop (patch)
    buf.extend_from_slice(&[0x48, 0x83, 0xec, 0x28]); // sub rsp, 0x28
    buf.extend_from_slice(&[0xff, 0xd0]); // call rax
    buf.extend_from_slice(&[0x48, 0x83, 0xc4, 0x28]); // add rsp, 0x28
    if check_status {
        buf.extend_from_slice(&[0x85, 0xc0]); // test eax, eax
        let jnz_imm = buf.len() + 1;
        buf.extend_from_slice(&[0x75, 0x00]); // jnz .fail_ret (patch)
        // jmp .loop
        let jmp_imm = buf.len() + 1;
        buf.extend_from_slice(&[0xeb, 0x00]);
        let done_at = buf.len();
        buf.extend_from_slice(&[0x31, 0xc0]); // xor eax, eax
        buf.extend_from_slice(&[0x5e, 0x5b, 0xc3]); // pop rsi; pop rbx; ret
        let fail_at = buf.len();
        buf.extend_from_slice(&[0x5e, 0x5b, 0xc3]); // pop rsi; pop rbx; ret (keep eax)
        patch_rel8(&mut buf, jae_imm, jae_imm + 1, done_at);
        patch_rel8(&mut buf, jz_imm, jz_imm + 1, loop_at);
        patch_rel8(&mut buf, jnz_imm, jnz_imm + 1, fail_at);
        patch_rel8(&mut buf, jmp_imm, jmp_imm + 1, loop_at);
    } else {
        // jmp .loop
        let jmp_imm = buf.len() + 1;
        buf.extend_from_slice(&[0xeb, 0x00]);
        let done_at = buf.len();
        buf.extend_from_slice(&[0x31, 0xc0]); // xor eax, eax
        buf.extend_from_slice(&[0x5e, 0x5b, 0xc3]); // pop rsi; pop rbx; ret
        patch_rel8(&mut buf, jae_imm, jae_imm + 1, done_at);
        patch_rel8(&mut buf, jz_imm, jz_imm + 1, loop_at);
        patch_rel8(&mut buf, jmp_imm, jmp_imm + 1, loop_at);
    }
    buf
}

/// `DialogBoxParamA/W` modal-loop body (out-of-line helper).
///
/// Win64 entry: `RCX=hInstance, RDX=lpTemplateName, R8=hWndParent,
/// R9=lpDialogFunc, [rsp+0x28]=dwInitParam`.
///
/// ```text
/// push rbx; sub rsp, 0x60     ; rbx = alignment pad (preserved)
/// mov rax, [rsp+0x90]        ; dwInitParam (caller's 5th arg)
/// mov [rsp+0x28], rax        ; 5th arg slot for CreateDialogParam
/// call CreateDialogParamA/W  ; host: build dialog, WM_INITDIALOG bridge
/// test rax, rax; jnz .created
///   mov rax, -1; jmp .done   ; creation failed → DialogBoxParam returns -1
/// .created:
/// mov [rsp+0x20], rax        ; hwnd lives in OUR frame — the WM_INITDIALOG
///                            ; guest callback clobbers every register, and
///                            ; its frame sits BELOW ours, so the stack slot
///                            ; survives
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
/// mov rax, dialog_result_va  ; EndDialog wrote the result here
/// mov eax, [rax]
/// .done:
/// add rsp, 0x60; pop rbx; ret
/// ```
///
/// `[rsp+0x90]` = the caller's 5th arg: entry `rsp` = R0, after `push rbx`
/// (8) + `sub rsp, 0x60` the frame is at R0-0x68, and the caller's stack arg
/// sits at `[R0+0x28]` = `[rsp+0x68+0x28]` = `[rsp+0x90]`.
pub(super) fn encode_dialog_box_param(
    create_dialog_param_va: u64,
    get_message_va: u64,
    is_dialog_message_va: u64,
    dispatch_message_va: u64,
    dialog_result_va: u64,
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(180);
    // push rbx ; sub rsp, 0x60
    buf.extend_from_slice(&[0x53]);
    buf.extend_from_slice(&[0x48, 0x83, 0xec, 0x60]);
    // mov rax, [rsp+0x90] ; mov [rsp+0x28], rax
    buf.extend_from_slice(&[0x48, 0x8b, 0x84, 0x24, 0x90, 0x00, 0x00, 0x00]);
    buf.extend_from_slice(&[0x48, 0x89, 0x44, 0x24, 0x28]);
    // call CreateDialogParamA/W
    buf.extend_from_slice(&[0x48, 0xb8]);
    buf.extend_from_slice(&create_dialog_param_va.to_le_bytes());
    buf.extend_from_slice(&[0xff, 0xd0]);
    // test rax, rax ; jnz .created
    buf.extend_from_slice(&[0x48, 0x85, 0xc0]);
    let jnz_created = buf.len() + 1;
    buf.extend_from_slice(&[0x75, 0x00]);
    // mov rax, -1 ; jmp .done
    buf.extend_from_slice(&[0x48, 0xc7, 0xc0, 0xff, 0xff, 0xff, 0xff]);
    let jmp_done = buf.len() + 1;
    buf.extend_from_slice(&[0xeb, 0x00]);
    // .created: mov [rsp+0x20], rax  (hwnd slot)
    let created_at = buf.len();
    buf.extend_from_slice(&[0x48, 0x89, 0x44, 0x24, 0x20]);
    // .loop:
    let loop_at = buf.len();
    // lea rcx, [rsp+0x28] ; xor edx, edx ; xor r8d, r8d ; xor r9d, r9d
    buf.extend_from_slice(&[0x48, 0x8d, 0x4c, 0x24, 0x28]);
    buf.extend_from_slice(&[0x31, 0xd2]);
    buf.extend_from_slice(&[0x45, 0x31, 0xc0]);
    buf.extend_from_slice(&[0x45, 0x31, 0xc9]);
    // call GetMessageA
    buf.extend_from_slice(&[0x48, 0xb8]);
    buf.extend_from_slice(&get_message_va.to_le_bytes());
    buf.extend_from_slice(&[0xff, 0xd0]);
    // test eax, eax ; jz .quit
    buf.extend_from_slice(&[0x85, 0xc0]);
    let jz_quit = buf.len() + 1;
    buf.extend_from_slice(&[0x74, 0x00]);
    // mov rcx, [rsp+0x20] ; lea rdx, [rsp+0x28] ; call IsDialogMessageA
    buf.extend_from_slice(&[0x48, 0x8b, 0x4c, 0x24, 0x20]);
    buf.extend_from_slice(&[0x48, 0x8d, 0x54, 0x24, 0x28]);
    buf.extend_from_slice(&[0x48, 0xb8]);
    buf.extend_from_slice(&is_dialog_message_va.to_le_bytes());
    buf.extend_from_slice(&[0xff, 0xd0]);
    // test eax, eax ; jnz .loop (consumed)
    buf.extend_from_slice(&[0x85, 0xc0]);
    let jnz_loop = buf.len() + 1;
    buf.extend_from_slice(&[0x75, 0x00]);
    // lea rcx, [rsp+0x28] ; call DispatchMessageA ; jmp .loop
    buf.extend_from_slice(&[0x48, 0x8d, 0x4c, 0x24, 0x28]);
    buf.extend_from_slice(&[0x48, 0xb8]);
    buf.extend_from_slice(&dispatch_message_va.to_le_bytes());
    buf.extend_from_slice(&[0xff, 0xd0]);
    let jmp_loop = buf.len() + 1;
    buf.extend_from_slice(&[0xeb, 0x00]);
    // .quit: mov rax, dialog_result_va ; mov eax, [rax]
    let quit_at = buf.len();
    buf.extend_from_slice(&[0x48, 0xb8]);
    buf.extend_from_slice(&dialog_result_va.to_le_bytes());
    buf.extend_from_slice(&[0x8b, 0x00]);
    // .done: add rsp, 0x60 ; pop rbx ; ret
    let done_at = buf.len();
    buf.extend_from_slice(&[0x48, 0x83, 0xc4, 0x60]);
    buf.push(0x5b);
    buf.push(0xc3);

    patch_rel8(&mut buf, jnz_created, jnz_created + 1, created_at);
    patch_rel8(&mut buf, jmp_done, jmp_done + 1, done_at);
    patch_rel8(&mut buf, jz_quit, jz_quit + 1, quit_at);
    patch_rel8(&mut buf, jnz_loop, jnz_loop + 1, loop_at);
    patch_rel8(&mut buf, jmp_loop, jmp_loop + 1, loop_at);
    buf
}

/// `GetOpenFileName`/`GetSaveFileName` modal-loop body (out-of-line helper).
///
/// The comdlg32 handler builds the file dialog and stores its HWND in the
/// callback-entry `RCX`; the body runs the same modal message loop as
/// [`encode_dialog_box_param`] — `GetMessageA` → `IsDialogMessageA` →
/// `DispatchMessageA` until `WM_QUIT` (posted by `EndDialog`), then returns
/// the dialog-result slot (the `GetOpenFileName` TRUE/FALSE the guest sees).
///
/// ```text
/// push rbx; sub rsp, 0x60
/// mov [rsp+0x20], rcx        ; dialog hwnd from the callback frame
/// .loop:
///   lea rcx, [rsp+0x28]      ; lpMsg
///   xor edx, edx; xor r8d, r8d; xor r9d, r9d
///   call GetMessageA
///   test eax, eax; jz .quit
///   mov rcx, [rsp+0x20]; lea rdx, [rsp+0x28]
///   call IsDialogMessageA
///   test eax, eax; jnz .loop ; consumed (Tab/Enter/Esc) → keep going
///   lea rcx, [rsp+0x28]
///   call DispatchMessageA
///   jmp .loop
/// .quit:
/// mov rax, dialog_result_va  ; EndDialog wrote the result here
/// mov eax, [rax]
/// .done:
/// add rsp, 0x60; pop rbx; ret
/// ```
pub(crate) fn encode_file_dialog_loop(
    get_message_va: u64,
    is_dialog_message_va: u64,
    dispatch_message_va: u64,
    dialog_result_va: u64,
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(170);
    // push rbx ; sub rsp, 0x60
    buf.extend_from_slice(&[0x53]);
    buf.extend_from_slice(&[0x48, 0x83, 0xec, 0x60]);
    // mov [rsp+0x20], rcx (dialog hwnd from the callback frame)
    buf.extend_from_slice(&[0x48, 0x89, 0x4c, 0x24, 0x20]);
    // .loop:
    let loop_at = buf.len();
    // lea rcx, [rsp+0x28] ; xor edx, edx ; xor r8d, r8d ; xor r9d, r9d
    buf.extend_from_slice(&[0x48, 0x8d, 0x4c, 0x24, 0x28]);
    buf.extend_from_slice(&[0x31, 0xd2]);
    buf.extend_from_slice(&[0x45, 0x31, 0xc0]);
    buf.extend_from_slice(&[0x45, 0x31, 0xc9]);
    // call GetMessageA
    buf.extend_from_slice(&[0x48, 0xb8]);
    buf.extend_from_slice(&get_message_va.to_le_bytes());
    buf.extend_from_slice(&[0xff, 0xd0]);
    // test eax, eax ; jz .quit
    buf.extend_from_slice(&[0x85, 0xc0]);
    let jz_quit = buf.len() + 1;
    buf.extend_from_slice(&[0x74, 0x00]);
    // mov rcx, [rsp+0x20] ; lea rdx, [rsp+0x28] ; call IsDialogMessageA
    buf.extend_from_slice(&[0x48, 0x8b, 0x4c, 0x24, 0x20]);
    buf.extend_from_slice(&[0x48, 0x8d, 0x54, 0x24, 0x28]);
    buf.extend_from_slice(&[0x48, 0xb8]);
    buf.extend_from_slice(&is_dialog_message_va.to_le_bytes());
    buf.extend_from_slice(&[0xff, 0xd0]);
    // test eax, eax ; jnz .loop (consumed)
    buf.extend_from_slice(&[0x85, 0xc0]);
    let jnz_loop = buf.len() + 1;
    buf.extend_from_slice(&[0x75, 0x00]);
    // lea rcx, [rsp+0x28] ; call DispatchMessageA ; jmp .loop
    buf.extend_from_slice(&[0x48, 0x8d, 0x4c, 0x24, 0x28]);
    buf.extend_from_slice(&[0x48, 0xb8]);
    buf.extend_from_slice(&dispatch_message_va.to_le_bytes());
    buf.extend_from_slice(&[0xff, 0xd0]);
    let jmp_loop = buf.len() + 1;
    buf.extend_from_slice(&[0xeb, 0x00]);
    // .quit: mov rax, dialog_result_va ; mov eax, [rax]
    let quit_at = buf.len();
    buf.extend_from_slice(&[0x48, 0xb8]);
    buf.extend_from_slice(&dialog_result_va.to_le_bytes());
    buf.extend_from_slice(&[0x8b, 0x00]);
    // .done: add rsp, 0x60 ; pop rbx ; ret (fall-through after .quit)
    buf.extend_from_slice(&[0x48, 0x83, 0xc4, 0x60]);
    buf.push(0x5b);
    buf.push(0xc3);

    patch_rel8(&mut buf, jz_quit, jz_quit + 1, quit_at);
    patch_rel8(&mut buf, jnz_loop, jnz_loop + 1, loop_at);
    patch_rel8(&mut buf, jmp_loop, jmp_loop + 1, loop_at);
    buf
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
    // .ok: mov edx, 1
    let ok_at = buf.len();
    buf.extend_from_slice(&[0xba, 0x01, 0x00, 0x00, 0x00]);
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
    patch_rel8(
        &mut buf,
        jmp_strikeout_close,
        jmp_strikeout_close + 1,
        close_at,
    );
    buf
}

pub(super) fn encode_fls_get(table_va: u64, max_slots: u32) -> Vec<u8> {
    let mut buf = Vec::with_capacity(32);
    buf.extend_from_slice(&[0x48, 0x81, 0xf9]);
    buf.extend_from_slice(&max_slots.to_le_bytes());
    let jae_imm = buf.len() + 1;
    buf.extend_from_slice(&[0x73, 0x00]);
    buf.extend_from_slice(&[0x48, 0xb8]);
    buf.extend_from_slice(&table_va.to_le_bytes());
    buf.extend_from_slice(&[0x48, 0x8b, 0x04, 0xc8]);
    buf.push(0xc3);
    let zero_at = buf.len();
    buf.extend_from_slice(&[0x31, 0xc0, 0xc3]);
    patch_rel8(&mut buf, jae_imm, jae_imm + 1, zero_at);
    buf
}

pub(super) fn encode_fls_set(table_va: u64, max_slots: u32) -> Vec<u8> {
    let mut buf = Vec::with_capacity(40);
    buf.extend_from_slice(&[0x48, 0x81, 0xf9]);
    buf.extend_from_slice(&max_slots.to_le_bytes());
    let jae_imm = buf.len() + 1;
    buf.extend_from_slice(&[0x73, 0x00]);
    buf.extend_from_slice(&[0x48, 0xb8]);
    buf.extend_from_slice(&table_va.to_le_bytes());
    buf.extend_from_slice(&[0x48, 0x89, 0x14, 0xc8]);
    buf.extend_from_slice(&[0xb8, 0x01, 0x00, 0x00, 0x00]);
    buf.push(0xc3);
    let fail_at = buf.len();
    buf.extend_from_slice(&[0x31, 0xc0, 0xc3]);
    patch_rel8(&mut buf, jae_imm, jae_imm + 1, fail_at);
    buf
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

/// `GetCurrentDirectoryW` per Microsoft Learn:
/// - success: return chars written **excluding** NUL
/// - buffer too small / size query: return required size **including** NUL
/// - size query: `nBufferLength == 0` and `lpBuffer == NULL` (we also treat
///   null buffer as size query, matching common app patterns and host)
pub(super) fn encode_get_current_directory_w(cwd_blob_va: u64) -> Vec<u8> {
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
    let mut buf = Vec::with_capacity(80);
    buf.extend_from_slice(&[0x49, 0xb8]); // mov r8, imm64
    buf.extend_from_slice(&cwd_blob_va.to_le_bytes());
    buf.extend_from_slice(&[0x41, 0x8b, 0x00]); // mov eax, [r8]
    buf.extend_from_slice(&[0x44, 0x8d, 0x48, 0x01]); // lea r9d, [rax+1]
    buf.extend_from_slice(&[0x48, 0x85, 0xd2]); // test rdx, rdx
    let jz1 = buf.len() + 1;
    buf.extend_from_slice(&[0x74, 0x00]); // jz .need_size
    buf.extend_from_slice(&[0x48, 0x85, 0xc9]); // test rcx, rcx
    let jz2 = buf.len() + 1;
    buf.extend_from_slice(&[0x74, 0x00]);
    buf.extend_from_slice(&[0x48, 0x39, 0xc1]); // cmp rcx, rax
    let jbe = buf.len() + 1;
    buf.extend_from_slice(&[0x76, 0x00]); // jbe .need_size
    // preserve RSI/RDI (Win64 non-volatiles)
    buf.extend_from_slice(&[0x56, 0x57]); // push rsi, rdi
    buf.extend_from_slice(&[0x49, 0x8d, 0x70, 0x04]); // lea rsi, [r8+4]
    buf.extend_from_slice(&[0x48, 0x89, 0xd7]); // mov rdi, rdx
    buf.extend_from_slice(&[0x8d, 0x48, 0x01]); // lea ecx, [rax+1]
    buf.extend_from_slice(&[0xf3, 0xa5]); // rep movsw
    buf.extend_from_slice(&[0x5f, 0x5e]); // pop rdi, rsi
    // restore eax = char_count (destroyed by lea ecx if we only used eax — eax intact)
    buf.push(0xc3);
    let need_size = buf.len();
    buf.extend_from_slice(&[0x44, 0x89, 0xc8]); // mov eax, r9d
    buf.push(0xc3);
    patch_rel8(&mut buf, jz1, jz1 + 1, need_size);
    patch_rel8(&mut buf, jz2, jz2 + 1, need_size);
    patch_rel8(&mut buf, jbe, jbe + 1, need_size);
    buf
}
