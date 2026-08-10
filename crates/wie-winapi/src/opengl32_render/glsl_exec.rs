// ── GLSL ES 1.00 software interpreter (tree-walker) ─────────────────────
//
// Runs a linked vertex shader per-vertex (attributes in, gl_Position +
// varying slots out) and a linked fragment shader per-pixel (interpolated
// varyings + gl_FragCoord + texture2D in, gl_FragColor out). The value model
// is [`GlslVal`]; locals live in a scope stack allocated per invocation
// (simple + correct; heavy shaders are slow — documented).

use ahash::HashMap;

use super::TextureObject;
use super::glsl::{
    BinOp, Expr, FunctionDef, GL_FRAGMENT_SHADER, GL_VERTEX_SHADER, GlslVal, LinkedFragmentShader,
    LinkedVertexShader, MAX_VARYING_FLOATS, Stmt, UnOp,
};
use super::*;

/// Program-side execution context (uniforms + their name index).
pub(crate) struct ProgramExec<'a> {
    pub uniforms: &'a [GlslVal],
    pub uniform_index: &'a HashMap<String, usize>,
}

/// Per-vertex VS inputs (the client-array / current-value path).
pub(crate) struct VsInputs {
    pub pos: [f32; 4],
    pub color: [f32; 4],
    pub normal: [f32; 3],
    pub texcoord: [f32; 4],
    pub mvp: Mat4,
}

/// VS outputs: clip-space position + optional front color + varying slots.
pub(crate) struct VsOutputs {
    pub position: [f32; 4],
    pub front_color: [f32; 4],
    pub varyings: [f32; MAX_VARYING_FLOATS],
    pub wrote_front_color: bool,
}

impl Default for VsOutputs {
    fn default() -> Self {
        Self {
            position: [0.0; 4],
            front_color: [1.0, 1.0, 1.0, 1.0],
            varyings: [0.0; MAX_VARYING_FLOATS],
            wrote_front_color: false,
        }
    }
}

/// Per-fragment FS inputs.
pub(crate) struct FsInputs<'a> {
    pub frag_coord: [f32; 4],
    /// Interpolated varying slots — a borrowed slice of the rasterizer's
    /// per-pixel buffer (zero copy; never materialised on the FS path).
    pub varyings: &'a [f32],
    pub textures: &'a [TextureObject],
    /// Texture id → index map, rebuilt on mutation; lets `texture2d` skip a
    /// per-pixel linear scan over all bound textures.
    pub texture_index: &'a HashMap<u32, usize>,
    pub texture_units: &'a [u32; 2],
}

/// FS output: the fragment color (before depth/blend).
pub(crate) struct FsOutputs {
    pub color: [f32; 4],
}

impl Default for FsOutputs {
    fn default() -> Self {
        Self {
            color: [1.0, 1.0, 1.0, 1.0],
        }
    }
}

/// One scope frame (name → value). Names are borrowed from the parsed AST
/// (`'a`), so scope writes never allocate.
type Scope<'a> = Vec<(&'a str, GlslVal)>;

struct Interp<'a> {
    kind: u32,
    exec: &'a ProgramExec<'a>,
    functions: &'a [FunctionDef],
    varying_slots: &'a HashMap<String, usize>,
    varying_comps: &'a HashMap<String, u32>,
    vs_inputs: Option<VsInputs>,
    fs_inputs: Option<FsInputs<'a>>,
    vs_out: VsOutputs,
    fs_out: FsOutputs,
    /// Flat scope stack: every live frame's `(name, value)` pairs, newest
    /// last. `scope_marks` records each frame's start index, so push/pop is a
    /// `push(len)` / `truncate(mark)` — no per-scope `Vec` allocation.
    scopes: Scope<'a>,
    scope_marks: Vec<usize>,
    call_depth: u32,
    returned: bool,
    return_value: Option<GlslVal>,
}

impl<'a> Interp<'a> {
    fn run_main(&mut self, main: usize) {
        self.push_scope();
        self.exec_fn_body(main, &[]);
        // Ignore a pending return value — main returns void.
    }

    fn push_scope(&mut self) {
        self.scope_marks.push(self.scopes.len());
    }

    fn pop_scope(&mut self) {
        if let Some(start) = self.scope_marks.pop() {
            self.scopes.truncate(start);
        }
    }

    fn lookup_local(&self, name: &str) -> Option<GlslVal> {
        self.scopes
            .iter()
            .rev()
            .find_map(|(n, v)| (*n == name).then_some(*v))
    }

    fn set_local(&mut self, name: &'a str, value: GlslVal) {
        for (n, v) in self.scopes.iter_mut().rev() {
            if *n == name {
                *v = value;
                return;
            }
        }
        // Not found — declare in the innermost scope (the flat tail).
        self.scopes.push((name, value));
    }

    fn read_var(&self, name: &str) -> GlslVal {
        if let Some(v) = self.lookup_local(name) {
            return v;
        }
        if let Some(idx) = self.exec.uniform_index.get(name) {
            return self
                .exec
                .uniforms
                .get(*idx)
                .copied()
                .unwrap_or(GlslVal::F32(0.0));
        }
        match self.kind {
            GL_VERTEX_SHADER => self.read_vs_builtin(name),
            _ => self.read_fs_builtin(name),
        }
    }

    fn read_vs_builtin(&self, name: &str) -> GlslVal {
        let inputs = self.vs_inputs.as_ref();
        match name {
            "gl_Vertex" => inputs.map_or(GlslVal::V4([0.0; 4]), |i| GlslVal::V4(i.pos)),
            "gl_Color" => inputs.map_or(GlslVal::V4([0.0; 4]), |i| GlslVal::V4(i.color)),
            "gl_Normal" => inputs.map_or(GlslVal::V3([0.0; 3]), |i| GlslVal::V3(i.normal)),
            "gl_MultiTexCoord0" => {
                inputs.map_or(GlslVal::V4([0.0; 4]), |i| GlslVal::V4(i.texcoord))
            }
            "gl_ModelViewProjectionMatrix" => {
                inputs.map_or(GlslVal::Mat4(IDENTITY), |i| GlslVal::Mat4(i.mvp))
            }
            _ => GlslVal::F32(0.0),
        }
    }

    fn read_fs_builtin(&self, name: &str) -> GlslVal {
        // Varyings: the VS wrote `comps` floats at slot*4 in `vs_out.varyings`;
        // the rasterizer interpolated them into `fs_inputs.varyings`. Read
        // only the DECLARED component count so `vec4(v_uv, 0.0, 1.0)` flattens
        // to exactly 4 (a `varying vec2` read as a vec4 would over-flatten and
        // truncate the trailing constructor args).
        if let Some(slot) = self.varying_slots.get(name) {
            let base = slot.saturating_mul(4);
            let Some(inputs) = self.fs_inputs.as_ref() else {
                return GlslVal::V4([0.0; 4]);
            };
            let comps = self
                .varying_comps
                .get(name)
                .copied()
                .map_or(4, |c| c.clamp(1, 4));
            let mut v = [0.0_f32; 4];
            for (i, dst) in v.iter_mut().enumerate() {
                if i < usize::try_from(comps).unwrap_or(0)
                    && let Some(src) = inputs.varyings.get(base.saturating_add(i))
                {
                    *dst = *src;
                }
            }
            return match comps {
                1 => GlslVal::F32(v[0]),
                2 => GlslVal::V2([v[0], v[1]]),
                3 => GlslVal::V3([v[0], v[1], v[2]]),
                _ => GlslVal::V4(v),
            };
        }
        match name {
            "gl_FragCoord" => self
                .fs_inputs
                .as_ref()
                .map_or(GlslVal::V4([0.0; 4]), |i| GlslVal::V4(i.frag_coord)),
            _ => GlslVal::F32(0.0),
        }
    }

    /// Write to a variable (local, uniform-output, or built-in output).
    fn write_var(&mut self, name: &'a str, value: GlslVal) {
        // Locals shadow built-ins.
        if self.lookup_local(name).is_some() {
            self.set_local(name, value);
            return;
        }
        match self.kind {
            GL_VERTEX_SHADER => match name {
                "gl_Position" => self.vs_out.position = as_v4(value),
                "gl_FrontColor" => {
                    self.vs_out.front_color = as_v4(value);
                    self.vs_out.wrote_front_color = true;
                }
                _ => {
                    if let Some(slot) = self.varying_slots.get(name) {
                        let base = slot.saturating_mul(4);
                        let comps = value.float_comps();
                        let v = as_v4(value);
                        for (i, comp) in v.iter().enumerate() {
                            if i < usize::try_from(comps).unwrap_or(0)
                                && let Some(dst) =
                                    self.vs_out.varyings.get_mut(base.saturating_add(i))
                            {
                                *dst = *comp;
                            }
                        }
                    } else {
                        self.set_local(name, value);
                    }
                }
            },
            _ => match name {
                "gl_FragColor" => self.fs_out.color = as_v4(value),
                _ => self.set_local(name, value),
            },
        }
    }

    fn exec_fn_body(&mut self, index: usize, args: &[GlslVal]) {
        let Some(f) = self.functions.get(index) else {
            return;
        };
        self.call_depth = self.call_depth.saturating_add(1);
        self.push_scope();
        for (p, a) in f.params.iter().zip(args) {
            self.set_local(&p.name, *a);
        }
        for stmt in &f.body {
            if self.returned {
                break;
            }
            self.exec_stmt(stmt);
        }
        self.pop_scope();
        self.call_depth = self.call_depth.saturating_sub(1);
    }

    fn find_function(&self, name: &str) -> Option<usize> {
        self.functions.iter().position(|f| f.name == name)
    }

    fn exec_stmt(&mut self, s: &'a Stmt) {
        match s {
            Stmt::Block(body) => {
                self.push_scope();
                for stmt in body {
                    if self.returned {
                        break;
                    }
                    self.exec_stmt(stmt);
                }
                self.pop_scope();
            }
            Stmt::Decl(d) => {
                let value = d
                    .init
                    .as_ref()
                    .map_or_else(|| default_val(&d.type_name), |e| self.eval(e));
                self.set_local(&d.name, value);
            }
            Stmt::Expr(e) => {
                let _ = self.eval(e);
            }
            Stmt::If { cond, then, else_ } => {
                let c = self.eval(cond);
                if truthy(c) {
                    self.exec_stmt(then);
                } else if let Some(e) = else_ {
                    self.exec_stmt(e);
                }
            }
            Stmt::For {
                init,
                cond,
                step,
                body,
            } => {
                let init_val = init
                    .init
                    .as_ref()
                    .map_or_else(|| default_val(&init.type_name), |e| self.eval(e));
                self.push_scope();
                self.set_local(&init.name, init_val);
                let mut guard = 0_u32;
                while truthy(self.eval(cond)) {
                    self.exec_stmt(body);
                    if self.returned {
                        break;
                    }
                    let _ = self.eval(step);
                    guard = guard.saturating_add(1);
                    if guard > 1_000_000 {
                        break; // safety net; constant bounds are link-enforced
                    }
                }
                self.pop_scope();
            }
            Stmt::Return(e) => {
                self.return_value = e.as_ref().map(|e| self.eval(e));
                self.returned = true;
            }
        }
    }

    fn eval(&mut self, e: &'a Expr) -> GlslVal {
        match e {
            Expr::FloatLit(v) => GlslVal::F32(*v),
            Expr::IntLit(v) => GlslVal::F32(*v as f32),
            Expr::BoolLit(b) => GlslVal::Bool(*b),
            Expr::Var(name) => self.read_var(name),
            Expr::Assign { name, value } => {
                let v = self.eval(value);
                self.write_var(name, v);
                v
            }
            Expr::Bin { op, left, right } => {
                // Short-circuit the logical operators.
                match op {
                    BinOp::And => {
                        if !truthy(self.eval(left)) {
                            return GlslVal::Bool(false);
                        }
                        return GlslVal::Bool(truthy(self.eval(right)));
                    }
                    BinOp::Or => {
                        if truthy(self.eval(left)) {
                            return GlslVal::Bool(true);
                        }
                        return GlslVal::Bool(truthy(self.eval(right)));
                    }
                    _ => {}
                }
                let l = self.eval(left);
                let r = self.eval(right);
                bin_eval(*op, l, r)
            }
            Expr::Un { op, e } => {
                let v = self.eval(e);
                match op {
                    UnOp::Neg => val_neg(v),
                    UnOp::Not => match v {
                        GlslVal::Bool(b) => GlslVal::Bool(!b),
                        other => other,
                    },
                }
            }
            Expr::Ternary { cond, yes, no } => {
                if truthy(self.eval(cond)) {
                    self.eval(yes)
                } else {
                    self.eval(no)
                }
            }
            Expr::Call { name, args } => {
                let values: Vec<GlslVal> = args.iter().map(|a| self.eval(a)).collect();
                self.call_function(name, &values)
            }
            Expr::Constructor { type_name, args } => {
                let values: Vec<GlslVal> = args.iter().map(|a| self.eval(a)).collect();
                construct(type_name, &values)
            }
            Expr::Swizzle { base, mask } => {
                let v = self.eval(base);
                swizzle(v, mask)
            }
        }
    }

    fn call_function(&mut self, name: &str, args: &[GlslVal]) -> GlslVal {
        if name == "texture2D" {
            return self.texture2d(args);
        }
        let Some(index) = self.find_function(name) else {
            return GlslVal::F32(0.0); // defensive; the typecheck rejects unknown calls
        };
        if self.call_depth >= 64 {
            return GlslVal::F32(0.0); // recursion is link-banned; guard anyway
        }
        // Save the return state, run, restore.
        let saved_returned = self.returned;
        let saved_value = self.return_value;
        self.returned = false;
        self.return_value = None;
        self.exec_fn_body(index, args);
        let result = self.return_value.take().unwrap_or(GlslVal::F32(0.0));
        self.returned = saved_returned;
        self.return_value = saved_value;
        result
    }

    fn texture2d(&mut self, args: &[GlslVal]) -> GlslVal {
        let (Some(GlslVal::Sampler(unit)), Some(GlslVal::V2(uv))) = (args.first(), args.get(1))
        else {
            return GlslVal::V4([0.0; 4]);
        };
        let inputs = match &self.fs_inputs {
            Some(i) => i,
            None => return GlslVal::V4([0.0; 4]),
        };
        let unit = usize::try_from(*unit).unwrap_or(usize::MAX);
        let name = inputs.texture_units.get(unit).copied().unwrap_or(0);
        let Some(idx) = inputs.texture_index.get(&name) else {
            return GlslVal::V4([0.0, 0.0, 0.0, 1.0]);
        };
        let Some(tex) = inputs.textures.get(*idx).filter(|t| !t.pixels.is_empty()) else {
            return GlslVal::V4([0.0, 0.0, 0.0, 1.0]);
        };
        let linear = tex.mag_filter == 0x2601; // GL_LINEAR
        let rgba = super::sample::sample_texel(tex, uv[0], uv[1], linear);
        GlslVal::V4(rgba)
    }
}

/// Run a linked vertex shader on one vertex.
pub(crate) fn run_vertex_shader(
    exec: &ProgramExec<'_>,
    vs: &LinkedVertexShader,
    inputs: VsInputs,
) -> VsOutputs {
    let mut interp = Interp {
        kind: GL_VERTEX_SHADER,
        exec,
        functions: &vs.functions,
        varying_slots: &vs.varying_slots,
        varying_comps: &vs.varying_comps,
        vs_inputs: Some(inputs),
        fs_inputs: None,
        vs_out: VsOutputs::default(),
        fs_out: FsOutputs::default(),
        scopes: Vec::new(),
        scope_marks: Vec::new(),
        call_depth: 0,
        returned: false,
        return_value: None,
    };
    interp.run_main(vs.main);
    interp.vs_out
}

/// Run a linked fragment shader on one pixel.
pub(crate) fn run_fragment_shader(
    exec: &ProgramExec<'_>,
    fs: &LinkedFragmentShader,
    inputs: FsInputs<'_>,
) -> FsOutputs {
    let mut interp = Interp {
        kind: GL_FRAGMENT_SHADER,
        exec,
        functions: &fs.functions,
        varying_slots: &fs.varying_slots,
        varying_comps: &fs.varying_comps,
        vs_inputs: None,
        fs_inputs: Some(inputs),
        vs_out: VsOutputs::default(),
        fs_out: FsOutputs::default(),
        scopes: Vec::new(),
        scope_marks: Vec::new(),
        call_depth: 0,
        returned: false,
        return_value: None,
    };
    interp.run_main(fs.main);
    interp.fs_out
}

/// The default value for a declared type (uninitialized locals are 0).
#[must_use]
fn default_val(type_name: &str) -> GlslVal {
    match type_name {
        "float" | "int" => GlslVal::F32(0.0),
        "bool" => GlslVal::Bool(false),
        "sampler2D" => GlslVal::Sampler(0),
        "vec2" => GlslVal::V2([0.0; 2]),
        "vec3" => GlslVal::V3([0.0; 3]),
        "vec4" => GlslVal::V4([0.0; 4]),
        "mat2" => GlslVal::Mat2([0.0; 4]),
        "mat3" => GlslVal::Mat3([0.0; 9]),
        "mat4" => GlslVal::Mat4([0.0; 16]),
        _ => GlslVal::F32(0.0),
    }
}

#[must_use]
fn truthy(v: GlslVal) -> bool {
    matches!(v, GlslVal::Bool(true))
}

/// Extract the float components of a value as a 4-vector (padded).
#[must_use]
fn as_v4(v: GlslVal) -> [f32; 4] {
    match v {
        GlslVal::F32(a) => [a, 0.0, 0.0, 1.0],
        GlslVal::V2(a) => [a[0], a[1], 0.0, 1.0],
        GlslVal::V3(a) => [a[0], a[1], a[2], 1.0],
        GlslVal::V4(a) => a,
        GlslVal::Bool(b) => [f32::from(b), 0.0, 0.0, 1.0],
        _ => [0.0; 4],
    }
}

#[must_use]
fn val_neg(v: GlslVal) -> GlslVal {
    match v {
        GlslVal::F32(a) => GlslVal::F32(-a),
        GlslVal::V2(a) => GlslVal::V2([-a[0], -a[1]]),
        GlslVal::V3(a) => GlslVal::V3([-a[0], -a[1], -a[2]]),
        GlslVal::V4(a) => GlslVal::V4([-a[0], -a[1], -a[2], -a[3]]),
        other => other,
    }
}

/// Componentwise binary op for matching-shape numeric values.
#[must_use]
fn bin_eval(op: BinOp, l: GlslVal, r: GlslVal) -> GlslVal {
    use BinOp::*;
    let f = |a: f32, b: f32| -> f32 {
        match op {
            Add => a + b,
            Sub => a - b,
            Mul => a * b,
            Div => {
                if b == 0.0 {
                    0.0
                } else {
                    a / b
                }
            }
            Lt => f32::from(a < b),
            Gt => f32::from(a > b),
            Le => f32::from(a <= b),
            Ge => f32::from(a >= b),
            Eq => f32::from(a == b),
            Ne => f32::from(a != b),
            And | Or => 0.0,
        }
    };
    let comp_bool = |a: f32, b: f32| -> bool {
        match op {
            Lt => a < b,
            Gt => a > b,
            Le => a <= b,
            Ge => a >= b,
            Eq => a == b,
            Ne => a != b,
            _ => false,
        }
    };
    // Scalar comparisons → bool.
    if matches!(op, Lt | Gt | Le | Ge | Eq | Ne) {
        return match (l, r) {
            (GlslVal::F32(a), GlslVal::F32(b)) => GlslVal::Bool(comp_bool(a, b)),
            (GlslVal::Bool(a), GlslVal::Bool(b)) => match op {
                Eq => GlslVal::Bool(a == b),
                _ => GlslVal::Bool(a != b),
            },
            (GlslVal::V2(a), GlslVal::V2(b)) => GlslVal::V2([
                f32::from(comp_bool(a[0], b[0])),
                f32::from(comp_bool(a[1], b[1])),
            ]),
            (GlslVal::V3(a), GlslVal::V3(b)) => GlslVal::V3([
                f32::from(comp_bool(a[0], b[0])),
                f32::from(comp_bool(a[1], b[1])),
                f32::from(comp_bool(a[2], b[2])),
            ]),
            (GlslVal::V4(a), GlslVal::V4(b)) => GlslVal::V4([
                f32::from(comp_bool(a[0], b[0])),
                f32::from(comp_bool(a[1], b[1])),
                f32::from(comp_bool(a[2], b[2])),
                f32::from(comp_bool(a[3], b[3])),
            ]),
            _ => GlslVal::Bool(false),
        };
    }
    match (l, r) {
        (GlslVal::F32(a), GlslVal::F32(b)) => GlslVal::F32(f(a, b)),
        (GlslVal::V2(a), GlslVal::V2(b)) => GlslVal::V2([f(a[0], b[0]), f(a[1], b[1])]),
        (GlslVal::V3(a), GlslVal::V3(b)) => {
            GlslVal::V3([f(a[0], b[0]), f(a[1], b[1]), f(a[2], b[2])])
        }
        (GlslVal::V4(a), GlslVal::V4(b)) => {
            GlslVal::V4([f(a[0], b[0]), f(a[1], b[1]), f(a[2], b[2]), f(a[3], b[3])])
        }
        // Scalar × vector / vector × scalar (and scalar / vector is invalid —
        // treat as componentwise for robustness).
        (GlslVal::F32(a), GlslVal::V2(b)) => GlslVal::V2([f(a, b[0]), f(a, b[1])]),
        (GlslVal::V2(a), GlslVal::F32(b)) => GlslVal::V2([f(a[0], b), f(a[1], b)]),
        (GlslVal::F32(a), GlslVal::V3(b)) => GlslVal::V3([f(a, b[0]), f(a, b[1]), f(a, b[2])]),
        (GlslVal::V3(a), GlslVal::F32(b)) => GlslVal::V3([f(a[0], b), f(a[1], b), f(a[2], b)]),
        (GlslVal::F32(a), GlslVal::V4(b)) => {
            GlslVal::V4([f(a, b[0]), f(a, b[1]), f(a, b[2]), f(a, b[3])])
        }
        (GlslVal::V4(a), GlslVal::F32(b)) => {
            GlslVal::V4([f(a[0], b), f(a[1], b), f(a[2], b), f(a[3], b)])
        }
        // Matrices.
        (GlslVal::Mat2(a), GlslVal::Mat2(b)) => GlslVal::Mat2(copy4(mat_mul::<2>(&a, &b))),
        (GlslVal::Mat3(a), GlslVal::Mat3(b)) => GlslVal::Mat3(copy9(mat_mul::<3>(&a, &b))),
        (GlslVal::Mat4(a), GlslVal::Mat4(b)) => GlslVal::Mat4(copy16(mat_mul::<4>(&a, &b))),
        (GlslVal::Mat2(a), GlslVal::V2(b)) => GlslVal::V2(mat_vec::<2>(&a, &b)),
        (GlslVal::V2(a), GlslVal::Mat2(b)) => GlslVal::V2(vec_mat::<2>(&a, &b)),
        (GlslVal::Mat3(a), GlslVal::V3(b)) => GlslVal::V3(mat_vec::<3>(&a, &b)),
        (GlslVal::V3(a), GlslVal::Mat3(b)) => GlslVal::V3(vec_mat::<3>(&a, &b)),
        (GlslVal::Mat4(a), GlslVal::V4(b)) => GlslVal::V4(mat_vec::<4>(&a, &b)),
        (GlslVal::V4(a), GlslVal::Mat4(b)) => GlslVal::V4(vec_mat::<4>(&a, &b)),
        (GlslVal::F32(a), GlslVal::Mat2(b)) => GlslVal::Mat2(copy4(scale_mat::<2>(&b, a))),
        (GlslVal::F32(a), GlslVal::Mat3(b)) => GlslVal::Mat3(copy9(scale_mat::<3>(&b, a))),
        (GlslVal::F32(a), GlslVal::Mat4(b)) => GlslVal::Mat4(copy16(scale_mat::<4>(&b, a))),
        (GlslVal::Mat2(a), GlslVal::F32(b)) => GlslVal::Mat2(copy4(scale_mat::<2>(&a, b))),
        (GlslVal::Mat3(a), GlslVal::F32(b)) => GlslVal::Mat3(copy9(scale_mat::<3>(&a, b))),
        (GlslVal::Mat4(a), GlslVal::F32(b)) => GlslVal::Mat4(copy16(scale_mat::<4>(&a, b))),
        (GlslVal::Bool(a), GlslVal::Bool(b)) => match op {
            Eq => GlslVal::Bool(a == b),
            _ => GlslVal::Bool(a != b),
        },
        _ => GlslVal::F32(0.0),
    }
}

/// Column-major N×N matrix × vector.
#[must_use]
fn mat_vec<const N: usize>(m: &[f32], v: &[f32]) -> [f32; N] {
    let mut out = [0.0_f32; N];
    for j in 0..N {
        let mut sum = 0.0_f32;
        for k in 0..N {
            let mkj = m.get(k * N + j).copied().unwrap_or(0.0);
            let vk = v.get(k).copied().unwrap_or(0.0);
            sum += mkj * vk;
        }
        if let Some(slot) = out.get_mut(j) {
            *slot = sum;
        }
    }
    out
}

/// Column-major N×N matrix × row vector (v·M).
#[must_use]
fn vec_mat<const N: usize>(v: &[f32], m: &[f32]) -> [f32; N] {
    let mut out = [0.0_f32; N];
    for j in 0..N {
        let mut sum = 0.0_f32;
        for k in 0..N {
            let mjk = m.get(j * N + k).copied().unwrap_or(0.0);
            let vk = v.get(k).copied().unwrap_or(0.0);
            sum += vk * mjk;
        }
        if let Some(slot) = out.get_mut(j) {
            *slot = sum;
        }
    }
    out
}

/// Column-major N×N matrix product (returns a plain Vec — the callers
/// copy into their fixed-size matrix arrays).
#[must_use]
fn mat_mul<const N: usize>(a: &[f32], b: &[f32]) -> Vec<f32> {
    let mut out = vec![0.0_f32; N * N];
    for i in 0..N {
        for j in 0..N {
            let mut sum = 0.0_f32;
            for k in 0..N {
                let a_ik = a.get(k * N + i).copied().unwrap_or(0.0);
                let b_kj = b.get(j * N + k).copied().unwrap_or(0.0);
                sum += a_ik * b_kj;
            }
            if let Some(slot) = out.get_mut(j * N + i) {
                *slot = sum;
            }
        }
    }
    out
}

/// Column-major N×N matrix scaled by a scalar.
#[must_use]
fn scale_mat<const N: usize>(m: &[f32], s: f32) -> Vec<f32> {
    let mut out = vec![0.0_f32; N * N];
    for (i, slot) in out.iter_mut().enumerate() {
        *slot = m.get(i).copied().unwrap_or(0.0) * s;
    }
    out
}

/// Swizzle a value (`xyzw`/`rgba`/`stpq` masks, 1..4 components).
#[must_use]
fn swizzle(v: GlslVal, mask: &str) -> GlslVal {
    let src: [f32; 4] = as_v4(v);
    let idx = |c: char| -> usize {
        match c {
            'x' | 'r' | 's' => 0,
            'y' | 'g' | 't' => 1,
            'z' | 'b' | 'p' => 2,
            _ => 3,
        }
    };
    let comps = mask.len().min(4);
    let pick = |i: usize| {
        src.get(idx(mask.chars().nth(i).unwrap_or('x')))
            .copied()
            .unwrap_or(0.0)
    };
    match comps {
        1 => GlslVal::F32(pick(0)),
        2 => GlslVal::V2([pick(0), pick(1)]),
        3 => GlslVal::V3([pick(0), pick(1), pick(2)]),
        _ => GlslVal::V4([pick(0), pick(1), pick(2), pick(3)]),
    }
}

/// Evaluate a `vecN/matN` constructor from the evaluated args.
#[must_use]
fn construct(type_name: &str, args: &[GlslVal]) -> GlslVal {
    if let Some(n) = type_name
        .strip_prefix("vec")
        .and_then(|s| s.parse::<u32>().ok())
    {
        return construct_vec(n, args);
    }
    if let Some(n) = type_name
        .strip_prefix("mat")
        .and_then(|s| s.parse::<u32>().ok())
    {
        return construct_mat(n, args);
    }
    match type_name {
        "float" => args.first().map_or(GlslVal::F32(0.0), |a| scalar_float(*a)),
        "int" => args.first().map_or(GlslVal::F32(0.0), |a| scalar_float(*a)),
        "bool" => GlslVal::Bool(args.first().is_some_and(|a| truthy(*a))),
        _ => GlslVal::F32(0.0),
    }
}

/// Flatten the args into float components (scalars + vector comps).
fn flatten(args: &[GlslVal]) -> Vec<f32> {
    let mut out = Vec::new();
    for a in args {
        match a {
            GlslVal::F32(x) => out.push(*x),
            GlslVal::V2(x) => out.extend_from_slice(x),
            GlslVal::V3(x) => out.extend_from_slice(x),
            GlslVal::V4(x) => out.extend_from_slice(x),
            GlslVal::Bool(b) => out.push(f32::from(*b)),
            _ => {}
        }
    }
    out
}

#[must_use]
fn construct_vec(n: u32, args: &[GlslVal]) -> GlslVal {
    let comps = flatten(args);
    let n = usize::try_from(n).unwrap_or(0);
    let mut v = [0.0_f32; 4];
    for i in 0..n {
        if let Some(slot) = v.get_mut(i) {
            *slot = comps.get(i).copied().unwrap_or(0.0);
        }
    }
    match n {
        2 => GlslVal::V2([v[0], v[1]]),
        3 => GlslVal::V3([v[0], v[1], v[2]]),
        _ => GlslVal::V4(v),
    }
}

#[must_use]
fn construct_mat(n: u32, args: &[GlslVal]) -> GlslVal {
    let n = usize::try_from(n).unwrap_or(0);
    // Diagonal from a single scalar.
    if args.len() == 1
        && let Some(GlslVal::F32(s)) = args.first()
    {
        let mut m = vec![0.0_f32; n * n];
        for i in 0..n {
            if let Some(slot) = m.get_mut(i * n + i) {
                *slot = *s;
            }
        }
        return mat_val(n, &m);
    }
    // Embed a (n-1) matrix.
    if args.len() == 1
        && let Some(GlslVal::Mat2(m)) = args.first()
        && n == 3
    {
        let mut out = vec![0.0_f32; 9];
        for c in 0..2 {
            for r in 0..2 {
                if let Some(slot) = out.get_mut(c * 3 + r) {
                    *slot = m.get(c * 2 + r).copied().unwrap_or(0.0);
                }
            }
        }
        if let Some(slot) = out.get_mut(8) {
            *slot = 1.0;
        }
        return mat_val(3, &out);
    }
    if args.len() == 1
        && let Some(GlslVal::Mat3(m)) = args.first()
        && n == 4
    {
        let mut out = vec![0.0_f32; 16];
        for c in 0..3 {
            for r in 0..3 {
                if let Some(slot) = out.get_mut(c * 4 + r) {
                    *slot = m.get(c * 3 + r).copied().unwrap_or(0.0);
                }
            }
        }
        if let Some(slot) = out.get_mut(15) {
            *slot = 1.0;
        }
        return mat_val(4, &out);
    }
    // N² floats → column-major fill.
    let comps = flatten(args);
    let mut m = vec![0.0_f32; n * n];
    for i in 0..n * n {
        if let Some(slot) = m.get_mut(i) {
            *slot = comps.get(i).copied().unwrap_or(0.0);
        }
    }
    mat_val(n, &m)
}

fn mat_val(n: usize, m: &[f32]) -> GlslVal {
    match n {
        2 => {
            let mut out = [0.0_f32; 4];
            out.copy_from_slice(m.get(..4).unwrap_or(&[]));
            GlslVal::Mat2(out)
        }
        3 => {
            let mut out = [0.0_f32; 9];
            out.copy_from_slice(m.get(..9).unwrap_or(&[]));
            GlslVal::Mat3(out)
        }
        _ => {
            let mut out = [0.0_f32; 16];
            out.copy_from_slice(m.get(..16).unwrap_or(&[]));
            GlslVal::Mat4(out)
        }
    }
}

fn scalar_float(v: GlslVal) -> GlslVal {
    match v {
        GlslVal::F32(x) => GlslVal::F32(x),
        GlslVal::Bool(b) => GlslVal::F32(f32::from(b)),
        GlslVal::V4(a) => GlslVal::F32(a[0]),
        GlslVal::V3(a) => GlslVal::F32(a[0]),
        GlslVal::V2(a) => GlslVal::F32(a[0]),
        _ => GlslVal::F32(0.0),
    }
}

fn copy4(v: Vec<f32>) -> [f32; 4] {
    let mut out = [0.0_f32; 4];
    out.copy_from_slice(v.get(..4).unwrap_or(&[]));
    out
}

fn copy9(v: Vec<f32>) -> [f32; 9] {
    let mut out = [0.0_f32; 9];
    out.copy_from_slice(v.get(..9).unwrap_or(&[]));
    out
}

fn copy16(v: Vec<f32>) -> [f32; 16] {
    let mut out = [0.0_f32; 16];
    out.copy_from_slice(v.get(..16).unwrap_or(&[]));
    out
}
