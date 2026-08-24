//! GLSL shader/program ABI handlers (glCreateShader … glUniformMatrix4fv,
//! glActiveTexture). Each reads the Win64 register args and delegates to the
//! `render::glsl` object store.

use super::*;
use crate::gdi32::{ArgReg, read_arg};

/// `GLuint glCreateShader(GLenum type)`.
pub(super) fn handle_gl_create_shader(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let kind = low_u32(
        read_arg(engine, ArgReg::Rcx, "glCreateShader")?,
        "glCreateShader kind",
    )?;
    let id = with_current_gl(|c| render::glsl::gl_create_shader(c, kind)).unwrap_or(0);
    ctx.finish(u64::from(id))
}

/// `void glShaderSource(GLuint shader, GLsizei count, const GLchar* const *string, const GLint *length)`.
pub(super) fn handle_gl_shader_source(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let shader = low_u32(
        read_arg(engine, ArgReg::Rcx, "glShaderSource")?,
        "glShaderSource shader",
    )?;
    let count = low_u32(
        read_arg(engine, ArgReg::Rdx, "glShaderSource")?,
        "glShaderSource count",
    )?;
    let strings_va = read_arg(engine, ArgReg::R8, "glShaderSource")?;
    let lengths_va = read_arg(engine, ArgReg::R9, "glShaderSource")?;
    let mut source = String::new();
    for i in 0..count {
        let slot = u64::from(i).saturating_mul(8);
        let str_va =
            crate::guest_memory::read_u64(engine, strings_va.saturating_add(slot)).unwrap_or(0);
        if str_va == 0 {
            continue;
        }
        let len = if lengths_va != 0 {
            let raw =
                crate::guest_memory::read_u32(engine, lengths_va.saturating_add(slot)).unwrap_or(0);
            i32::from_le_bytes(raw.to_le_bytes())
        } else {
            -1
        };
        if len < 0 {
            let s =
                crate::guest_string::read_ansi_lossy(engine, str_va, 1 << 20).unwrap_or_default();
            source.push_str(&s);
        } else {
            let mut bytes = vec![0_u8; usize::try_from(len).unwrap_or(0)];
            if engine.mem_read(str_va, &mut bytes).is_ok() {
                source.push_str(&String::from_utf8_lossy(&bytes));
            }
        }
    }
    with_current_gl(|c| render::glsl::gl_shader_source(c, shader, &source));
    ctx.finish(0)
}

/// `void glCompileShader(GLuint shader)`.
pub(super) fn handle_gl_compile_shader(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let shader = low_u32(
        read_arg(engine, ArgReg::Rcx, "glCompileShader")?,
        "glCompileShader shader",
    )?;
    with_current_gl(|c| render::glsl::gl_compile_shader(c, shader));
    ctx.finish(0)
}

/// `void glGetShaderiv(GLuint shader, GLenum pname, GLint *params)`.
pub(super) fn handle_gl_get_shader_iv(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let shader = low_u32(
        read_arg(engine, ArgReg::Rcx, "glGetShaderiv")?,
        "glGetShaderiv shader",
    )?;
    let pname = low_u32(
        read_arg(engine, ArgReg::Rdx, "glGetShaderiv")?,
        "glGetShaderiv pname",
    )?;
    let params_va = read_arg(engine, ArgReg::R8, "glGetShaderiv")?;
    if params_va != 0 {
        let value = with_current_gl(|c| match pname {
            render::glsl::GL_COMPILE_STATUS => render::glsl::gl_shader_compile_status(c, shader),
            render::glsl::GL_INFO_LOG_LENGTH => {
                let len = render::glsl::gl_shader_info_log(c, shader).len();
                i32::try_from(len.saturating_add(1)).unwrap_or(0)
            }
            render::glsl::GL_SHADER_SOURCE_LENGTH => {
                let len = render::glsl::gl_shader_source_text(c, shader).len();
                i32::try_from(len.saturating_add(1)).unwrap_or(0)
            }
            _ => 0,
        })
        .unwrap_or(0);
        crate::guest_memory::write_i32(engine, params_va, value)?;
    }
    ctx.finish(0)
}

/// `void glGetShaderInfoLog(GLuint shader, GLsizei maxLength, GLsizei *length, GLchar *infoLog)`.
pub(super) fn handle_gl_get_shader_info_log(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let shader = low_u32(
        read_arg(engine, ArgReg::Rcx, "glGetShaderInfoLog")?,
        "glGetShaderInfoLog shader",
    )?;
    let max_len = low_u32(
        read_arg(engine, ArgReg::Rdx, "glGetShaderInfoLog")?,
        "glGetShaderInfoLog max",
    )?;
    let length_va = read_arg(engine, ArgReg::R8, "glGetShaderInfoLog")?;
    let log_va = read_arg(engine, ArgReg::R9, "glGetShaderInfoLog")?;
    let log = with_current_gl(|c| render::glsl::gl_shader_info_log(c, shader)).unwrap_or_default();
    write_info_log(engine, log_va, max_len, length_va, &log)?;
    ctx.finish(0)
}

/// `void glDeleteShader(GLuint shader)`.
pub(super) fn handle_gl_delete_shader(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let shader = low_u32(
        read_arg(engine, ArgReg::Rcx, "glDeleteShader")?,
        "glDeleteShader shader",
    )?;
    with_current_gl(|c| render::glsl::gl_delete_shader(c, shader));
    ctx.finish(0)
}

/// `GLboolean glIsShader(GLuint shader)`.
pub(super) fn handle_gl_is_shader(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let shader = low_u32(
        read_arg(engine, ArgReg::Rcx, "glIsShader")?,
        "glIsShader shader",
    )?;
    let present = with_current_gl(|c| render::glsl::gl_is_shader(c, shader)).unwrap_or(false);
    ctx.finish(u64::from(present))
}

/// `GLuint glCreateProgram(void)`.
pub(super) fn handle_gl_create_program(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let id = with_current_gl(render::glsl::gl_create_program).unwrap_or(0);
    ctx.finish(u64::from(id))
}

/// `void glAttachShader(GLuint program, GLuint shader)`.
pub(super) fn handle_gl_attach_shader(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let program = low_u32(
        read_arg(engine, ArgReg::Rcx, "glAttachShader")?,
        "glAttachShader program",
    )?;
    let shader = low_u32(
        read_arg(engine, ArgReg::Rdx, "glAttachShader")?,
        "glAttachShader shader",
    )?;
    with_current_gl(|c| render::glsl::gl_attach_shader(c, program, shader));
    ctx.finish(0)
}

/// `void glDetachShader(GLuint program, GLuint shader)`.
pub(super) fn handle_gl_detach_shader(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let program = low_u32(
        read_arg(engine, ArgReg::Rcx, "glDetachShader")?,
        "glDetachShader program",
    )?;
    let shader = low_u32(
        read_arg(engine, ArgReg::Rdx, "glDetachShader")?,
        "glDetachShader shader",
    )?;
    with_current_gl(|c| render::glsl::gl_detach_shader(c, program, shader));
    ctx.finish(0)
}

/// `void glLinkProgram(GLuint program)`.
pub(super) fn handle_gl_link_program(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let program = low_u32(
        read_arg(engine, ArgReg::Rcx, "glLinkProgram")?,
        "glLinkProgram program",
    )?;
    with_current_gl(|c| render::glsl::gl_link_program(c, program));
    ctx.finish(0)
}

/// `void glUseProgram(GLuint program)`.
pub(super) fn handle_gl_use_program(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let program = low_u32(
        read_arg(engine, ArgReg::Rcx, "glUseProgram")?,
        "glUseProgram program",
    )?;
    with_current_gl(|c| render::glsl::gl_use_program(c, program));
    ctx.finish(0)
}

/// `void glGetProgramiv(GLuint program, GLenum pname, GLint *params)`.
pub(super) fn handle_gl_get_program_iv(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let program = low_u32(
        read_arg(engine, ArgReg::Rcx, "glGetProgramiv")?,
        "glGetProgramiv program",
    )?;
    let pname = low_u32(
        read_arg(engine, ArgReg::Rdx, "glGetProgramiv")?,
        "glGetProgramiv pname",
    )?;
    let params_va = read_arg(engine, ArgReg::R8, "glGetProgramiv")?;
    if params_va != 0 {
        let value = with_current_gl(|c| match pname {
            render::glsl::GL_LINK_STATUS | render::glsl::GL_VALIDATE_STATUS => {
                render::glsl::gl_program_link_status(c, program)
            }
            render::glsl::GL_INFO_LOG_LENGTH => {
                let len = render::glsl::gl_program_info_log(c, program).len();
                i32::try_from(len.saturating_add(1)).unwrap_or(0)
            }
            render::glsl::GL_ATTACHED_SHADERS => {
                render::glsl::gl_program_attached_count(c, program)
            }
            render::glsl::GL_ACTIVE_UNIFORMS => {
                render::glsl::gl_program_active_uniforms(c, program)
            }
            render::glsl::GL_ACTIVE_ATTRIBUTES => {
                render::glsl::gl_program_active_attributes(c, program)
            }
            _ => 0,
        })
        .unwrap_or(0);
        crate::guest_memory::write_i32(engine, params_va, value)?;
    }
    ctx.finish(0)
}

/// `void glGetProgramInfoLog(GLuint program, GLsizei maxLength, GLsizei *length, GLchar *infoLog)`.
pub(super) fn handle_gl_get_program_info_log(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let program = low_u32(
        read_arg(engine, ArgReg::Rcx, "glGetProgramInfoLog")?,
        "glGetProgramInfoLog program",
    )?;
    let max_len = low_u32(
        read_arg(engine, ArgReg::Rdx, "glGetProgramInfoLog")?,
        "glGetProgramInfoLog max",
    )?;
    let length_va = read_arg(engine, ArgReg::R8, "glGetProgramInfoLog")?;
    let log_va = read_arg(engine, ArgReg::R9, "glGetProgramInfoLog")?;
    let log =
        with_current_gl(|c| render::glsl::gl_program_info_log(c, program)).unwrap_or_default();
    write_info_log(engine, log_va, max_len, length_va, &log)?;
    ctx.finish(0)
}

/// `void glDeleteProgram(GLuint program)`.
pub(super) fn handle_gl_delete_program(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let program = low_u32(
        read_arg(engine, ArgReg::Rcx, "glDeleteProgram")?,
        "glDeleteProgram program",
    )?;
    with_current_gl(|c| render::glsl::gl_delete_program(c, program));
    ctx.finish(0)
}

/// `GLboolean glIsProgram(GLuint program)`.
pub(super) fn handle_gl_is_program(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let program = low_u32(
        read_arg(engine, ArgReg::Rcx, "glIsProgram")?,
        "glIsProgram program",
    )?;
    let present = with_current_gl(|c| render::glsl::gl_is_program(c, program)).unwrap_or(false);
    ctx.finish(u64::from(present))
}

/// `void glValidateProgram(GLuint program)`.
pub(super) fn handle_gl_validate_program(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let program = low_u32(
        read_arg(engine, ArgReg::Rcx, "glValidateProgram")?,
        "glValidateProgram program",
    )?;
    with_current_gl(|c| render::glsl::gl_validate_program(c, program));
    ctx.finish(0)
}

/// `GLint glGetUniformLocation(GLuint program, const GLchar *name)`.
pub(super) fn handle_gl_get_uniform_location(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let program = low_u32(
        read_arg(engine, ArgReg::Rcx, "glGetUniformLocation")?,
        "glGetUniformLocation program",
    )?;
    let name_va = read_arg(engine, ArgReg::Rdx, "glGetUniformLocation")?;
    let name = crate::guest_string::read_ansi_lossy(engine, name_va, 256).unwrap_or_default();
    let location =
        with_current_gl(|c| render::glsl::gl_get_uniform_location(c, program, &name)).unwrap_or(-1);
    let bits = u32::from_ne_bytes(location.to_ne_bytes());
    ctx.finish(u64::from(bits))
}

/// `void glUniform1f(GLint location, GLfloat v0)`.
pub(super) fn handle_gl_uniform1f(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let location = low_i32(read_arg(engine, ArgReg::Rcx, "glUniform1f")?);
    let v = read_xmm_f32(engine, 1).context("failed to read XMM1 for glUniform1f")?;
    with_current_gl(|c| render::glsl::gl_uniform_set(c, location, render::glsl::GlslVal::F32(v)));
    ctx.finish(0)
}

/// `void glUniform2f(GLint location, GLfloat v0, GLfloat v1)`.
pub(super) fn handle_gl_uniform2f(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let location = low_i32(read_arg(engine, ArgReg::Rcx, "glUniform2f")?);
    let a = read_xmm_f32(engine, 1).context("failed to read XMM1 for glUniform2f")?;
    let b = read_xmm_f32(engine, 2).context("failed to read XMM2 for glUniform2f")?;
    with_current_gl(|c| {
        render::glsl::gl_uniform_set(c, location, render::glsl::GlslVal::V2([a, b]))
    });
    ctx.finish(0)
}

/// `void glUniform3f(GLint location, GLfloat v0, GLfloat v1, GLfloat v2)`.
pub(super) fn handle_gl_uniform3f(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let location = low_i32(read_arg(engine, ArgReg::Rcx, "glUniform3f")?);
    let a = read_xmm_f32(engine, 1).context("failed to read XMM1 for glUniform3f")?;
    let b = read_xmm_f32(engine, 2).context("failed to read XMM2 for glUniform3f")?;
    let c = read_xmm_f32(engine, 3).context("failed to read XMM3 for glUniform3f")?;
    with_current_gl(|gl_ctx| {
        render::glsl::gl_uniform_set(gl_ctx, location, render::glsl::GlslVal::V3([a, b, c]))
    });
    ctx.finish(0)
}

/// `void glUniform4f(GLint location, GLfloat v0, GLfloat v1, GLfloat v2, GLfloat v3)`.
pub(super) fn handle_gl_uniform4f(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let location = low_i32(read_arg(engine, ArgReg::Rcx, "glUniform4f")?);
    let a = read_xmm_f32(engine, 1).context("failed to read XMM1 for glUniform4f")?;
    let b = read_xmm_f32(engine, 2).context("failed to read XMM2 for glUniform4f")?;
    let c = read_xmm_f32(engine, 3).context("failed to read XMM3 for glUniform4f")?;
    // The 4th float rides the stack (XMM3 holds the 3rd).
    let d_raw = crate::d3d9::read_stack_argument(engine, 0x28, "glUniform4f v3")?;
    let d = f32::from_bits(u32::try_from(d_raw & u64::from(u32::MAX)).unwrap_or(0));
    with_current_gl(|gl_ctx| {
        render::glsl::gl_uniform_set(gl_ctx, location, render::glsl::GlslVal::V4([a, b, c, d]))
    });
    ctx.finish(0)
}

/// `void glUniform1i(GLint location, GLint v0)` — the sampler → unit path.
pub(super) fn handle_gl_uniform1i(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let location = low_i32(read_arg(engine, ArgReg::Rcx, "glUniform1i")?);
    let v = low_i32(read_arg(engine, ArgReg::Rdx, "glUniform1i")?);
    with_current_gl(|c| {
        render::glsl::gl_uniform_set(
            c,
            location,
            render::glsl::GlslVal::Sampler(u32::try_from(v).unwrap_or(0)),
        )
    });
    ctx.finish(0)
}

/// `void glUniformMatrix4fv(GLint location, GLsizei count, GLboolean transpose, const GLfloat *value)`.
pub(super) fn handle_gl_uniform_matrix4fv(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let location = low_i32(read_arg(engine, ArgReg::Rcx, "glUniformMatrix4fv")?);
    let count = low_u32(
        read_arg(engine, ArgReg::Rdx, "glUniformMatrix4fv")?,
        "count",
    )?;
    let transpose = read_arg(engine, ArgReg::R8, "glUniformMatrix4fv")? != 0;
    let value_va = read_arg(engine, ArgReg::R9, "glUniformMatrix4fv")?;
    let mut bytes = [0_u8; 64];
    if value_va != 0 && engine.mem_read(value_va, &mut bytes).is_ok() {
        let mut m = [0.0_f32; 16];
        for (i, slot) in m.iter_mut().enumerate() {
            *slot = read_f32_from(&bytes, i.saturating_mul(4));
        }
        // The GL column-major array is consumed directly; the transpose flag
        // (a transposed upload) swaps the storage.
        let m = if transpose {
            let mut t = [0.0_f32; 16];
            for r in 0..4 {
                for c in 0..4 {
                    if let (Some(dst), Some(src)) = (t.get_mut(c * 4 + r), m.get(r * 4 + c)) {
                        *dst = *src;
                    }
                }
            }
            t
        } else {
            m
        };
        with_current_gl(|c| {
            render::glsl::gl_uniform_set(c, location, render::glsl::GlslVal::Mat4(m))
        });
    }
    let _ = count;
    ctx.finish(0)
}

/// `void glActiveTexture(GLenum texture)` — selects the texture unit that
/// `glBindTexture` and the sampler uniforms target.
pub(super) fn handle_gl_active_texture(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let texture = low_u32(
        read_arg(engine, ArgReg::Rcx, "glActiveTexture")?,
        "glActiveTexture unit",
    )?;
    with_current_gl(|c| {
        let unit = texture.saturating_sub(render::glsl::GL_TEXTURE0);
        if unit < 2 {
            c.active_texture_unit = unit;
        }
    });
    ctx.finish(0)
}

/// Write a NUL-terminated info log (bounded by `max_len`).
fn write_info_log(
    engine: &mut dyn wie_cpu::CpuEngine,
    log_va: u64,
    max_len: u32,
    length_va: u64,
    log: &str,
) -> Result<()> {
    let mut bytes: Vec<u8> = log.as_bytes().to_vec();
    bytes.push(0);
    if log_va != 0 {
        let room = usize::try_from(max_len).unwrap_or(0);
        let n = bytes.len().min(room);
        engine.mem_write(log_va, bytes.get(..n).unwrap_or(&[]))?;
    }
    if length_va != 0 {
        let written = i32::try_from(bytes.len()).unwrap_or(0);
        crate::guest_memory::write_i32(engine, length_va, written)?;
    }
    Ok(())
}
