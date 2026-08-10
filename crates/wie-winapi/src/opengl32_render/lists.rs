// ── Display lists (GL 1.1) ──────────────────────────────────────────────
//
// `glNewList(id, GL_COMPILE | GL_COMPILE_AND_EXECUTE)` opens a capture: every
// compiled command (vertices, matrices, state changes, DrawArrays,
// DrawElements, light/material, ...) is RECORDED as a [`ListOp`] instead of
// (GL_COMPILE) or in addition to (GL_COMPILE_AND_EXECUTE) executing.
// `glCallList` replays the recorded ops through the same pipeline fns the
// handlers use; nested calls are bounded (depth 64 → `GL_STACK_OVERFLOW`).
//
// Pointer semantics: `glVertexPointer` and friends are NOT compiled (they
// execute immediately — GL rule), so a list re-dereferences the array state
// current at CALL time; `glDrawElements` records the INDEX POINTER VALUE at
// record time and re-reads through it at replay (documented choice — the
// indices are dereferenced when the list executes). Buffer-object data
// (`glBufferData`) is copied at record time and is therefore already current
// when the list replays.

use super::arrays::{GL_INT, GL_UNSIGNED_INT, GL_UNSIGNED_SHORT, GuestRead};
use super::*;

/// `GL_COMPILE` (list mode — record only).
pub(crate) const GL_COMPILE: u32 = 0x1300;
/// `GL_COMPILE_AND_EXECUTE` (list mode — record + execute).
pub(crate) const GL_COMPILE_AND_EXECUTE: u32 = 0x1302;
/// `GL_STACK_OVERFLOW` (nested list depth exceeded).
pub(crate) const GL_STACK_OVERFLOW: u32 = 0x0503;
/// `GL_STACK_UNDERFLOW`.
#[allow(dead_code)]
pub(crate) const GL_STACK_UNDERFLOW: u32 = 0x0504;

/// Maximum nested `glCallList` depth (GL guarantees ≥ 64).
const MAX_LIST_DEPTH: u32 = 64;

/// One recorded display-list command.
#[derive(Debug, Clone)]
pub(crate) enum ListOp {
    Begin(u32),
    End,
    Vertex(f32, f32, f32, f32),
    Color(f32, f32, f32, f32),
    ColorUb(u8, u8, u8, u8),
    TexCoord(f32, f32, f32, f32),
    Normal(f32, f32, f32),
    MatrixMode(u32),
    LoadIdentity,
    LoadMatrix(Mat4),
    MultMatrix(Mat4),
    Ortho(f32, f32, f32, f32, f32, f32),
    Frustum(f32, f32, f32, f32, f32, f32),
    Translate(f32, f32, f32),
    Rotate(f32, f32, f32, f32),
    Scale(f32, f32, f32),
    PushMatrix,
    PopMatrix,
    Viewport(i32, i32, i32, i32),
    DepthFunc(u32),
    DepthMask(u32),
    Enable(u32),
    Disable(u32),
    BlendFunc(u32, u32),
    PolygonMode(u32, u32),
    ShadeModel(u32),
    BindTexture(u32),
    TexEnv(u32),
    LightFv {
        light: u32,
        pname: u32,
        params: [f32; 4],
    },
    LightModelFv {
        pname: u32,
        params: [f32; 4],
    },
    MaterialFv {
        face: u32,
        pname: u32,
        params: [f32; 4],
    },
    MaterialF {
        face: u32,
        pname: u32,
        param: f32,
    },
    DrawArrays {
        mode: u32,
        first: u32,
        count: u32,
    },
    DrawElements {
        mode: u32,
        count: u32,
        index_type: u32,
        indices_va: u64,
    },
    CallList(u32),
}

/// A compiled display list.
#[derive(Debug, Clone)]
pub(crate) struct ListObject {
    /// GL list id (never 0).
    pub(crate) id: u32,
    /// The recorded ops, replayed in order by `glCallList`.
    pub(crate) ops: Vec<ListOp>,
}

/// An open `glNewList` capture.
#[derive(Debug)]
pub(crate) struct ListCompileState {
    /// The list being compiled.
    pub(crate) id: u32,
    /// `GL_COMPILE` (record only) or `GL_COMPILE_AND_EXECUTE`.
    pub(crate) mode: u32,
    /// Ops recorded so far.
    pub(crate) ops: Vec<ListOp>,
}

/// `glGenLists(range)` — the first free id, or 0 when none is available.
pub(crate) fn gl_gen_lists(ctx: &mut GlCtx, range: u32) -> u32 {
    let mut id = ctx.next_list_id;
    for _ in 0..range {
        if id == 0 {
            return 0;
        }
        if ctx.lists.iter().any(|l| l.id == id) {
            id = id.wrapping_add(1);
            continue;
        }
        return id;
    }
    0
}

/// `glIsList(id)`.
#[must_use]
pub(crate) fn gl_is_list(ctx: &GlCtx, id: u32) -> bool {
    ctx.lists.iter().any(|l| l.id == id)
}

/// `glDeleteLists(id, range)` — remove the range of list objects.
pub(crate) fn gl_delete_lists(ctx: &mut GlCtx, id: u32, range: u32) {
    for i in 0..range {
        let name = id.wrapping_add(i);
        ctx.lists.retain(|l| l.id != name);
        if ctx.list_compile.as_ref().is_some_and(|c| c.id == name) {
            ctx.list_compile = None;
        }
    }
}

/// `glNewList(id, mode)` — open a capture (error when already compiling).
pub(crate) fn gl_new_list(ctx: &mut GlCtx, id: u32, mode: u32) {
    if ctx.list_compile.is_some() {
        ctx.set_error(GL_INVALID_OPERATION);
        return;
    }
    if !matches!(mode, GL_COMPILE | GL_COMPILE_AND_EXECUTE) {
        ctx.set_error(GL_INVALID_ENUM);
        return;
    }
    if id == 0 {
        ctx.set_error(GL_INVALID_VALUE);
        return;
    }
    if ctx.begin_mode.is_some() {
        // A list cannot open inside glBegin/glEnd.
        ctx.set_error(GL_INVALID_OPERATION);
        return;
    }
    ctx.list_compile = Some(ListCompileState {
        id,
        mode,
        ops: Vec::new(),
    });
}

/// `glEndList` — close the capture.
pub(crate) fn gl_end_list(ctx: &mut GlCtx) {
    let Some(compile) = ctx.list_compile.take() else {
        ctx.set_error(GL_INVALID_OPERATION);
        return;
    };
    if let Some(list) = ctx.lists.iter_mut().find(|l| l.id == compile.id) {
        list.ops = compile.ops;
    } else {
        ctx.lists.push(ListObject {
            id: compile.id,
            ops: compile.ops,
        });
    }
}

/// Record `op` while a list is compiling; returns whether the caller should
/// ALSO execute the command (`GL_COMPILE_AND_EXECUTE`, or no capture at all).
#[must_use]
pub(crate) fn gl_capture(ctx: &mut GlCtx, op: ListOp) -> bool {
    match &mut ctx.list_compile {
        Some(compile) => {
            compile.ops.push(op);
            compile.mode == GL_COMPILE_AND_EXECUTE
        }
        None => true,
    }
}

/// `glCallList(id)` — replay a list through the pipeline.
pub(crate) fn gl_call_list(ctx: &mut GlCtx, id: u32, read: GuestRead<'_>) {
    if !gl_capture(ctx, ListOp::CallList(id)) {
        return;
    }
    if ctx.replay_depth >= MAX_LIST_DEPTH {
        ctx.set_error(GL_STACK_OVERFLOW);
        return;
    }
    let Some(list) = ctx.lists.iter().find(|l| l.id == id).cloned() else {
        // GL ignores calls to undefined lists.
        return;
    };
    ctx.replay_depth = ctx.replay_depth.saturating_add(1);
    for op in &list.ops {
        replay_op(ctx, op, read);
    }
    ctx.replay_depth = ctx.replay_depth.saturating_sub(1);
}

/// `glCallLists(n, type, lists)` — replay `n` list ids (byte / short / int).
pub(crate) fn gl_call_lists(
    ctx: &mut GlCtx,
    n: u32,
    list_type: u32,
    lists_va: u64,
    read: GuestRead<'_>,
) {
    let width = match list_type {
        GL_UNSIGNED_BYTE => 1,
        GL_UNSIGNED_SHORT => 2,
        GL_INT | GL_UNSIGNED_INT => 4,
        _ => {
            ctx.set_error(GL_INVALID_ENUM);
            return;
        }
    };
    let count = usize::try_from(n).unwrap_or(0);
    let mut bytes = vec![0_u8; count.saturating_mul(usize::try_from(width).unwrap_or(0))];
    if !read(lists_va, &mut bytes) {
        return;
    }
    for j in 0..count {
        let off = j.saturating_mul(usize::try_from(width).unwrap_or(0));
        let id = match width {
            1 => u32::from(bytes.get(off).copied().unwrap_or(0)),
            2 => u32::from(u16::from_le_bytes(
                bytes
                    .get(off..off.saturating_add(2))
                    .and_then(|s| s.try_into().ok())
                    .unwrap_or([0; 2]),
            )),
            _ => u32::from_le_bytes(
                bytes
                    .get(off..off.saturating_add(4))
                    .and_then(|s| s.try_into().ok())
                    .unwrap_or([0; 4]),
            ),
        };
        gl_call_list(ctx, id, read);
    }
}

/// Dispatch one recorded op to the matching pipeline fn.
fn replay_op(ctx: &mut GlCtx, op: &ListOp, read: GuestRead<'_>) {
    use super::arrays as a;
    match op {
        ListOp::Begin(mode) => gl_begin(ctx, *mode),
        ListOp::End => gl_end(ctx),
        ListOp::Vertex(x, y, z, w) => gl_vertex(ctx, *x, *y, *z, *w),
        ListOp::Color(r, g, b, a) => gl_color(ctx, *r, *g, *b, *a),
        ListOp::ColorUb(r, g, b, a) => gl_color_ub(ctx, *r, *g, *b, *a),
        ListOp::TexCoord(s, t, r, q) => gl_tex_coord(ctx, *s, *t, *r, *q),
        ListOp::Normal(x, y, z) => gl_normal(ctx, *x, *y, *z),
        ListOp::MatrixMode(mode) => gl_matrix_mode(ctx, *mode),
        ListOp::LoadIdentity => gl_load_identity(ctx),
        ListOp::LoadMatrix(m) => gl_load_matrix(ctx, m),
        ListOp::MultMatrix(m) => gl_mult_matrix(ctx, m),
        ListOp::Ortho(l1, r, b, t, n, f) => gl_ortho(ctx, *l1, *r, *b, *t, *n, *f),
        ListOp::Frustum(l1, r, b, t, n, f) => gl_frustum(ctx, *l1, *r, *b, *t, *n, *f),
        ListOp::Translate(x, y, z) => gl_translate(ctx, *x, *y, *z),
        ListOp::Rotate(a1, x, y, z) => gl_rotate(ctx, *a1, *x, *y, *z),
        ListOp::Scale(x, y, z) => gl_scale(ctx, *x, *y, *z),
        ListOp::PushMatrix => gl_push_matrix(ctx),
        ListOp::PopMatrix => gl_pop_matrix(ctx),
        ListOp::Viewport(x, y, w, h) => gl_viewport(ctx, *x, *y, *w, *h),
        ListOp::DepthFunc(func) => gl_depth_func(ctx, *func),
        ListOp::DepthMask(flag) => gl_depth_mask(ctx, *flag),
        ListOp::Enable(cap) => gl_enable(ctx, *cap),
        ListOp::Disable(cap) => gl_disable(ctx, *cap),
        ListOp::BlendFunc(src, dst) => gl_blend_func(ctx, *src, *dst),
        ListOp::PolygonMode(face, mode) => gl_polygon_mode(ctx, *face, *mode),
        ListOp::ShadeModel(mode) => gl_shade_model(ctx, *mode),
        ListOp::BindTexture(name) => gl_bind_texture(ctx, GL_TEXTURE_2D, *name),
        ListOp::TexEnv(param) => gl_tex_env_i(ctx, GL_TEXTURE_ENV, GL_TEXTURE_ENV_MODE, *param),
        ListOp::LightFv {
            light,
            pname,
            params,
        } => ctx.light_fv(*light, *pname, params),
        ListOp::LightModelFv { pname, params } => ctx.light_model_fv(*pname, params),
        ListOp::MaterialFv {
            face,
            pname,
            params,
        } => ctx.material_fv(*face, *pname, params),
        ListOp::MaterialF { face, pname, param } => ctx.material_f(*face, *pname, *param),
        ListOp::DrawArrays { mode, first, count } => {
            a::gl_draw_arrays(ctx, *mode, *first, *count, read)
        }
        ListOp::DrawElements {
            mode,
            count,
            index_type,
            indices_va,
        } => a::gl_draw_elements(ctx, *mode, *count, *index_type, *indices_va, read),
        ListOp::CallList(id) => gl_call_list(ctx, *id, read),
    }
}
