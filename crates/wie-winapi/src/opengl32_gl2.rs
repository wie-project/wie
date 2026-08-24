//! Stage-2 `gl*` handlers: vertex arrays + VBOs, display lists, and
//! fixed-function lighting. Each handler reads the Win64 register args and
//! delegates to the pipeline (`render::*`); the display-list capture lives
//! inside the pipeline fns, so the handlers are uniform with the stage-1 set.

use super::*;
use crate::gdi32::{ArgReg, read_arg};

/// `void glEnableClientState(GLenum array)`.
pub(super) fn handle_gl_enable_client_state(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let array = low_u32(
        read_arg(engine, ArgReg::Rcx, "glEnableClientState")?,
        "glEnableClientState array",
    )?;
    with_current_gl(|c| render::arrays::client_state(c, array, true));
    ctx.finish(0)
}

/// `void glDisableClientState(GLenum array)`.
pub(super) fn handle_gl_disable_client_state(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let array = low_u32(
        read_arg(engine, ArgReg::Rcx, "glDisableClientState")?,
        "glDisableClientState array",
    )?;
    with_current_gl(|c| render::arrays::client_state(c, array, false));
    ctx.finish(0)
}

/// Shared body for the four `gl*Pointer` setters (same register layout).
fn pointer_setter(
    ctx: &mut HandlerContext<'_>,
    name: &str,
    apply: impl FnOnce(&mut render::GlCtx, u32, u32, u32, u64),
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let size = low_u32(read_arg(engine, ArgReg::Rcx, name)?, "pointer size")?;
    let type_ = low_u32(read_arg(engine, ArgReg::Rdx, name)?, "pointer type")?;
    let stride = low_u32(read_arg(engine, ArgReg::R8, name)?, "pointer stride")?;
    let pointer = read_arg(engine, ArgReg::R9, name)?;
    with_current_gl(|c| apply(c, size, type_, stride, pointer));
    ctx.finish(0)
}

/// `void glVertexPointer(GLint size, GLenum type, GLsizei stride, const void *ptr)`.
pub(super) fn handle_gl_vertex_pointer(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    pointer_setter(ctx, "glVertexPointer", render::arrays::gl_vertex_pointer)
}

/// `void glColorPointer(GLint size, GLenum type, GLsizei stride, const void *ptr)`.
pub(super) fn handle_gl_color_pointer(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    pointer_setter(ctx, "glColorPointer", render::arrays::gl_color_pointer)
}

/// `void glTexCoordPointer(GLint size, GLenum type, GLsizei stride, const void *ptr)`.
pub(super) fn handle_gl_texcoord_pointer(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    pointer_setter(
        ctx,
        "glTexCoordPointer",
        render::arrays::gl_texcoord_pointer,
    )
}

/// `void glNormalPointer(GLenum type, GLsizei stride, const void *ptr)`.
pub(super) fn handle_gl_normal_pointer(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    pointer_setter(
        ctx,
        "glNormalPointer",
        |c, _size, type_, stride, pointer| {
            render::arrays::gl_normal_pointer(c, type_, stride, pointer)
        },
    )
}

/// `void glDrawArrays(GLenum mode, GLint first, GLsizei count)`.
pub(super) fn handle_gl_draw_arrays(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let mode = low_u32(
        read_arg(engine, ArgReg::Rcx, "glDrawArrays")?,
        "glDrawArrays mode",
    )?;
    let first = low_u32(
        read_arg(engine, ArgReg::Rdx, "glDrawArrays")?,
        "glDrawArrays first",
    )?;
    let count = low_u32(
        read_arg(engine, ArgReg::R8, "glDrawArrays")?,
        "glDrawArrays count",
    )?;
    with_current_gl(|c| {
        render::arrays::gl_draw_arrays(c, mode, first, count, &mut |va, buf| {
            engine.mem_read(va, buf).is_ok()
        })
    });
    ctx.finish(0)
}

/// `void glDrawElements(GLenum mode, GLsizei count, GLenum type, const void *indices)`.
pub(super) fn handle_gl_draw_elements(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let mode = low_u32(
        read_arg(engine, ArgReg::Rcx, "glDrawElements")?,
        "glDrawElements mode",
    )?;
    let count = low_u32(
        read_arg(engine, ArgReg::Rdx, "glDrawElements")?,
        "glDrawElements count",
    )?;
    let index_type = low_u32(
        read_arg(engine, ArgReg::R8, "glDrawElements")?,
        "glDrawElements type",
    )?;
    let indices_va = read_arg(engine, ArgReg::R9, "glDrawElements")?;
    with_current_gl(|c| {
        render::arrays::gl_draw_elements(c, mode, count, index_type, indices_va, &mut |va, buf| {
            engine.mem_read(va, buf).is_ok()
        })
    });
    ctx.finish(0)
}

// ── Buffer objects (GL 1.5) ─────────────────────────────────────────────

/// `void glGenBuffers(GLsizei n, GLuint *buffers)`.
pub(super) fn handle_gl_gen_buffers(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let count = low_u32(
        read_arg(engine, ArgReg::Rcx, "glGenBuffers")?,
        "glGenBuffers count",
    )?;
    let buffers_va = read_arg(engine, ArgReg::Rdx, "glGenBuffers")?;
    let names = with_current_gl(|c| render::arrays::gl_gen_buffers(c, count)).unwrap_or_default();
    if buffers_va != 0 {
        for (i, name) in names.iter().enumerate() {
            let offset = u64::try_from(i.saturating_mul(4)).unwrap_or(0);
            crate::guest_memory::write_u32(engine, buffers_va.wrapping_add(offset), *name)?;
        }
    }
    ctx.finish(0)
}

/// `void glDeleteBuffers(GLsizei n, const GLuint *buffers)`.
pub(super) fn handle_gl_delete_buffers(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let count = low_u32(
        read_arg(engine, ArgReg::Rcx, "glDeleteBuffers")?,
        "glDeleteBuffers count",
    )?;
    let buffers_va = read_arg(engine, ArgReg::Rdx, "glDeleteBuffers")?;
    let mut names = Vec::new();
    let mut bytes = vec![0_u8; usize::try_from(count.saturating_mul(4)).unwrap_or(0)];
    if buffers_va != 0 && engine.mem_read(buffers_va, &mut bytes).is_ok() {
        for chunk in bytes.chunks(4) {
            let raw: [u8; 4] = chunk.try_into().unwrap_or([0; 4]);
            names.push(u32::from_le_bytes(raw));
        }
    }
    with_current_gl(|c| render::arrays::gl_delete_buffers(c, &names));
    ctx.finish(0)
}

/// `GLboolean glIsBuffer(GLuint id)`.
pub(super) fn handle_gl_is_buffer(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let id = low_u32(
        read_arg(engine, ArgReg::Rcx, "glIsBuffer")?,
        "glIsBuffer id",
    )?;
    let present = with_current_gl(|c| render::arrays::gl_is_buffer(c, id)).unwrap_or(false);
    ctx.finish(u64::from(present))
}

/// `void glBindBuffer(GLenum target, GLuint buffer)`.
pub(super) fn handle_gl_bind_buffer(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let target = low_u32(
        read_arg(engine, ArgReg::Rcx, "glBindBuffer")?,
        "glBindBuffer target",
    )?;
    let id = low_u32(
        read_arg(engine, ArgReg::Rdx, "glBindBuffer")?,
        "glBindBuffer id",
    )?;
    with_current_gl(|c| render::arrays::gl_bind_buffer(c, target, id));
    ctx.finish(0)
}

/// `void glBufferData(GLenum target, GLsizeiptr size, const void *data, GLenum usage)`.
pub(super) fn handle_gl_buffer_data(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let target = low_u32(
        read_arg(engine, ArgReg::Rcx, "glBufferData")?,
        "glBufferData target",
    )?;
    let size = low_u32(
        read_arg(engine, ArgReg::Rdx, "glBufferData")?,
        "glBufferData size",
    )?;
    let data_va = read_arg(engine, ArgReg::R8, "glBufferData")?;
    let usage = low_u32(
        read_arg(engine, ArgReg::R9, "glBufferData")?,
        "glBufferData usage",
    )?;
    with_current_gl(|c| {
        render::arrays::gl_buffer_data(c, target, size, data_va, usage, &mut |va, buf| {
            engine.mem_read(va, buf).is_ok()
        })
    });
    ctx.finish(0)
}

/// `void glBufferSubData(GLenum target, GLintptr offset, GLsizeiptr size, const void *data)`.
pub(super) fn handle_gl_buffer_sub_data(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let target = low_u32(
        read_arg(engine, ArgReg::Rcx, "glBufferSubData")?,
        "glBufferSubData target",
    )?;
    let offset = low_u32(
        read_arg(engine, ArgReg::Rdx, "glBufferSubData")?,
        "glBufferSubData offset",
    )?;
    let size = low_u32(
        read_arg(engine, ArgReg::R8, "glBufferSubData")?,
        "glBufferSubData size",
    )?;
    let data_va = read_arg(engine, ArgReg::R9, "glBufferSubData")?;
    with_current_gl(|c| {
        render::arrays::gl_buffer_sub_data(c, target, offset, size, data_va, &mut |va, buf| {
            engine.mem_read(va, buf).is_ok()
        })
    });
    ctx.finish(0)
}

// ── Display lists ───────────────────────────────────────────────────────

/// `GLuint glGenLists(GLsizei range)`.
pub(super) fn handle_gl_gen_lists(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let range = low_u32(
        read_arg(engine, ArgReg::Rcx, "glGenLists")?,
        "glGenLists range",
    )?;
    let id = with_current_gl(|c| render::lists::gl_gen_lists(c, range)).unwrap_or(0);
    ctx.finish(u64::from(id))
}

/// `GLboolean glIsList(GLuint id)`.
pub(super) fn handle_gl_is_list(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let id = low_u32(read_arg(engine, ArgReg::Rcx, "glIsList")?, "glIsList id")?;
    let present = with_current_gl(|c| render::lists::gl_is_list(c, id)).unwrap_or(false);
    ctx.finish(u64::from(present))
}

/// `void glDeleteLists(GLuint id, GLsizei range)`.
pub(super) fn handle_gl_delete_lists(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let id = low_u32(
        read_arg(engine, ArgReg::Rcx, "glDeleteLists")?,
        "glDeleteLists id",
    )?;
    let range = low_u32(
        read_arg(engine, ArgReg::Rdx, "glDeleteLists")?,
        "glDeleteLists range",
    )?;
    with_current_gl(|c| render::lists::gl_delete_lists(c, id, range));
    ctx.finish(0)
}

/// `void glNewList(GLuint id, GLenum mode)`.
pub(super) fn handle_gl_new_list(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let id = low_u32(read_arg(engine, ArgReg::Rcx, "glNewList")?, "glNewList id")?;
    let mode = low_u32(
        read_arg(engine, ArgReg::Rdx, "glNewList")?,
        "glNewList mode",
    )?;
    with_current_gl(|c| render::lists::gl_new_list(c, id, mode));
    ctx.finish(0)
}

/// `void glEndList(void)`.
pub(super) fn handle_gl_end_list(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    with_current_gl(render::lists::gl_end_list);
    ctx.finish(0)
}

/// `void glCallList(GLuint id)`.
pub(super) fn handle_gl_call_list(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let id = low_u32(
        read_arg(engine, ArgReg::Rcx, "glCallList")?,
        "glCallList id",
    )?;
    with_current_gl(|c| {
        render::lists::gl_call_list(c, id, &mut |va, buf| engine.mem_read(va, buf).is_ok())
    });
    ctx.finish(0)
}

/// `void glCallLists(GLsizei n, GLenum type, const void *lists)`.
pub(super) fn handle_gl_call_lists(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let n = low_u32(
        read_arg(engine, ArgReg::Rcx, "glCallLists")?,
        "glCallLists n",
    )?;
    let list_type = low_u32(
        read_arg(engine, ArgReg::Rdx, "glCallLists")?,
        "glCallLists type",
    )?;
    let lists_va = read_arg(engine, ArgReg::R8, "glCallLists")?;
    with_current_gl(|c| {
        render::lists::gl_call_lists(c, n, list_type, lists_va, &mut |va, buf| {
            engine.mem_read(va, buf).is_ok()
        })
    });
    ctx.finish(0)
}

// ── Lighting ────────────────────────────────────────────────────────────

/// Shared body for the fv light/material setters (4 floats at `params`).
fn fv_setter(
    ctx: &mut HandlerContext<'_>,
    name: &str,
    apply: impl FnOnce(&mut render::GlCtx, u32, u32, &[f32]),
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let a = low_u32(read_arg(engine, ArgReg::Rcx, name)?, "first enum")?;
    let pname = low_u32(read_arg(engine, ArgReg::Rdx, name)?, "pname")?;
    let params_va = read_arg(engine, ArgReg::R8, name)?;
    let mut bytes = [0_u8; 16];
    if params_va != 0 && engine.mem_read(params_va, &mut bytes).is_ok() {
        let params = [
            read_f32_from(&bytes, 0),
            read_f32_from(&bytes, 4),
            read_f32_from(&bytes, 8),
            read_f32_from(&bytes, 12),
        ];
        with_current_gl(|c| apply(c, a, pname, &params));
    }
    ctx.finish(0)
}

/// `void glLightfv(GLenum light, GLenum pname, const GLfloat *params)`.
pub(super) fn handle_gl_light_fv(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    fv_setter(ctx, "glLightfv", |c, light, pname, params| {
        c.light_fv(light, pname, params)
    })
}

/// `void glLightModelfv(GLenum pname, const GLfloat *params)`.
pub(super) fn handle_gl_light_model_fv(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let pname = low_u32(
        read_arg(engine, ArgReg::Rcx, "glLightModelfv")?,
        "glLightModelfv pname",
    )?;
    let params_va = read_arg(engine, ArgReg::Rdx, "glLightModelfv")?;
    let mut bytes = [0_u8; 16];
    if params_va != 0 && engine.mem_read(params_va, &mut bytes).is_ok() {
        let params = [
            read_f32_from(&bytes, 0),
            read_f32_from(&bytes, 4),
            read_f32_from(&bytes, 8),
            read_f32_from(&bytes, 12),
        ];
        with_current_gl(|c| c.light_model_fv(pname, &params));
    }
    ctx.finish(0)
}

/// `void glMaterialfv(GLenum face, GLenum pname, const GLfloat *params)`.
pub(super) fn handle_gl_material_fv(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    fv_setter(ctx, "glMaterialfv", |c, face, pname, params| {
        c.material_fv(face, pname, params)
    })
}

/// `void glMaterialf(GLenum face, GLenum pname, GLfloat param)`.
pub(super) fn handle_gl_material_f(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let face = low_u32(
        read_arg(engine, ArgReg::Rcx, "glMaterialf")?,
        "glMaterialf face",
    )?;
    let pname = low_u32(
        read_arg(engine, ArgReg::Rdx, "glMaterialf")?,
        "glMaterialf pname",
    )?;
    let param = read_xmm_f32(engine, 2).context("failed to read XMM2 for glMaterialf")?;
    with_current_gl(|c| c.material_f(face, pname, param));
    ctx.finish(0)
}

/// `void glShadeModel(GLenum mode)`.
pub(super) fn handle_gl_shade_model(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let mode = low_u32(
        read_arg(engine, ArgReg::Rcx, "glShadeModel")?,
        "glShadeModel mode",
    )?;
    with_current_gl(|c| render::gl_shade_model(c, mode));
    ctx.finish(0)
}

/// `void glNormal3f(GLfloat x, GLfloat y, GLfloat z)`.
pub(super) fn handle_gl_normal3f(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let x = read_xmm_f32(engine, 0).context("failed to read XMM0 for glNormal3f")?;
    let y = read_xmm_f32(engine, 1).context("failed to read XMM1 for glNormal3f")?;
    let z = read_xmm_f32(engine, 2).context("failed to read XMM2 for glNormal3f")?;
    with_current_gl(|c| render::gl_normal(c, x, y, z));
    ctx.finish(0)
}

/// `void glNormal3fv(const GLfloat *v)`.
pub(super) fn handle_gl_normal3fv(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let va = read_arg(engine, ArgReg::Rcx, "glNormal3fv")?;
    let mut bytes = [0_u8; 12];
    if engine.mem_read(va, &mut bytes).is_ok() {
        let x = read_f32_from(&bytes, 0);
        let y = read_f32_from(&bytes, 4);
        let z = read_f32_from(&bytes, 8);
        with_current_gl(|c| render::gl_normal(c, x, y, z));
    }
    ctx.finish(0)
}
