// ── GLSL ES 1.00 software interpreter: language layer ───────────────────
//
// The lexer, recursive-descent parser, AST, static typechecker, and the
// shader/program object store (`glCreateShader` … `glLinkProgram`). The
// interpreter lives in [`super::glsl_exec`].
//
// Documented subset (the honest boundary): types float/int/bool/vec2..4/
// mat2..4/sampler2D; qualifiers attribute/varying/uniform (precision
// qualifiers parsed and ignored); constructors, arithmetic + comparison +
// logical operators, swizzle reads, assignment; if/else, `for` with a
// compile-time-constant bound ONLY (dynamic bounds → link error), return,
// blocks; user functions (recursion → link error); built-ins
// gl_Position/gl_Vertex/gl_Normal/gl_Color/gl_MultiTexCoord0/
// gl_ModelViewProjectionMatrix (vertex), gl_FragColor/gl_FragCoord/
// texture2D (fragment). Custom attribute names, arrays, structs, and
// swizzle assignment are documented-missing (link/compile error with a clear
// log).

use ahash::{HashMap, HashMapExt};

use super::*;

/// `GL_VERTEX_SHADER` (gl.h).
pub(crate) const GL_VERTEX_SHADER: u32 = 0x8B31;
/// `GL_FRAGMENT_SHADER`.
pub(crate) const GL_FRAGMENT_SHADER: u32 = 0x8B30;
/// `GL_COMPILE_STATUS`.
pub(crate) const GL_COMPILE_STATUS: u32 = 0x8B81;
/// `GL_LINK_STATUS`.
pub(crate) const GL_LINK_STATUS: u32 = 0x8B82;
/// `GL_VALIDATE_STATUS`.
pub(crate) const GL_VALIDATE_STATUS: u32 = 0x8B83;
/// `GL_INFO_LOG_LENGTH`.
pub(crate) const GL_INFO_LOG_LENGTH: u32 = 0x8B84;
/// `GL_ATTACHED_SHADERS`.
pub(crate) const GL_ATTACHED_SHADERS: u32 = 0x8B85;
/// `GL_ACTIVE_UNIFORMS`.
pub(crate) const GL_ACTIVE_UNIFORMS: u32 = 0x8B86;
/// `GL_ACTIVE_ATTRIBUTES`.
pub(crate) const GL_ACTIVE_ATTRIBUTES: u32 = 0x8B89;
/// `GL_SHADER_SOURCE_LENGTH`.
pub(crate) const GL_SHADER_SOURCE_LENGTH: u32 = 0x8B88;
/// `GL_TEXTURE0` — the first texture-unit enum.
pub(crate) const GL_TEXTURE0: u32 = 0x84C0;

/// Maximum varying vec4 slots (link assigns each varying one slot; the
/// per-vertex/per-fragment data is fixed-size so the rasterizer can carry it).
pub(crate) const MAX_VARYINGS: usize = 8;
/// Floats per vertex carried through the rasterizer for varying
/// interpolation (MAX_VARYINGS × 4).
pub(crate) const MAX_VARYING_FLOATS: usize = MAX_VARYINGS * 4;

/// One runtime value. Scalars, vectors, matrices, bools, and sampler
/// handles (a texture unit index).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum GlslVal {
    /// Scalar float.
    F32(f32),
    /// 2-vector.
    V2([f32; 2]),
    /// 3-vector.
    V3([f32; 3]),
    /// 4-vector.
    V4([f32; 4]),
    /// 2×2 matrix (column-major).
    Mat2([f32; 4]),
    /// 3×3 matrix (column-major).
    Mat3([f32; 9]),
    /// 4×4 matrix (column-major).
    Mat4([f32; 16]),
    /// Boolean.
    Bool(bool),
    /// A sampler's texture-unit index.
    Sampler(u32),
}

impl GlslVal {
    /// The number of float components (0 for bool/sampler, N for matN).
    #[must_use]
    pub(crate) fn float_comps(self) -> u32 {
        match self {
            GlslVal::F32(_) => 1,
            GlslVal::V2(_) => 2,
            GlslVal::V3(_) => 3,
            GlslVal::V4(_) => 4,
            GlslVal::Mat2(_) => 4,
            GlslVal::Mat3(_) => 9,
            GlslVal::Mat4(_) => 16,
            GlslVal::Bool(_) | GlslVal::Sampler(_) => 0,
        }
    }
}

/// Base scalar type of a GLSL value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BaseType {
    Float,
    Int,
    Bool,
    Sampler,
}

/// Static GLSL type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GlslType {
    Scalar(BaseType),
    /// `Base::Float/Int/Bool` vector of `comps` components.
    Vec(BaseType, u32),
    /// N×N float matrix.
    Mat(u32),
}

impl GlslType {
    /// True for scalar (non-sampler) types — the operand a vec/mat broadcasts
    /// against in GLSL componentwise arithmetic.
    #[must_use]
    fn is_scalar(self) -> bool {
        matches!(
            self,
            GlslType::Scalar(BaseType::Float | BaseType::Int | BaseType::Bool)
        )
    }

    /// The number of float-equivalent components a constructor argument of
    /// this type contributes (scalar = 1, vecN = N, matN = N²).
    #[must_use]
    fn components(self) -> usize {
        match self {
            GlslType::Scalar(_) => 1,
            GlslType::Vec(_, n) => usize::try_from(n).unwrap_or(0),
            GlslType::Mat(n) => {
                let n = usize::try_from(n).unwrap_or(0);
                n * n
            }
        }
    }
}

/// Look up a declared type name (`float`, `vec2`, `mat3`, `sampler2D`, ...).
#[must_use]
fn parse_type_name(name: &str) -> Option<GlslType> {
    match name {
        "float" => Some(GlslType::Scalar(BaseType::Float)),
        "int" => Some(GlslType::Scalar(BaseType::Int)),
        "bool" => Some(GlslType::Scalar(BaseType::Bool)),
        "sampler2D" => Some(GlslType::Scalar(BaseType::Sampler)),
        "vec2" => Some(GlslType::Vec(BaseType::Float, 2)),
        "vec3" => Some(GlslType::Vec(BaseType::Float, 3)),
        "vec4" => Some(GlslType::Vec(BaseType::Float, 4)),
        "ivec2" => Some(GlslType::Vec(BaseType::Int, 2)),
        "ivec3" => Some(GlslType::Vec(BaseType::Int, 3)),
        "ivec4" => Some(GlslType::Vec(BaseType::Int, 4)),
        "bvec2" => Some(GlslType::Vec(BaseType::Bool, 2)),
        "bvec3" => Some(GlslType::Vec(BaseType::Bool, 3)),
        "bvec4" => Some(GlslType::Vec(BaseType::Bool, 4)),
        "mat2" => Some(GlslType::Mat(2)),
        "mat3" => Some(GlslType::Mat(3)),
        "mat4" => Some(GlslType::Mat(4)),
        _ => None,
    }
}

// ── Tokens ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Ident(String),
    Float(f32),
    Int(i32),
    Sym(String),
    Eof,
}

/// Hand-written GLSL ES tokenizer (identifiers, numbers, operators).
fn lex(source: &str) -> Result<Vec<Tok>, String> {
    let chars: Vec<char> = source.chars().collect();
    let mut out = Vec::new();
    let mut i = 0_usize;
    while i < chars.len() {
        let c = chars.get(i).copied().unwrap_or('\0');
        if c.is_whitespace() {
            i = i.saturating_add(1);
            continue;
        }
        if c == '/' && chars.get(i.saturating_add(1)).copied() == Some('/') {
            while i < chars.len() && chars.get(i).copied().unwrap_or('\0') != '\n' {
                i = i.saturating_add(1);
            }
            continue;
        }
        if c == '/' && chars.get(i.saturating_add(1)).copied() == Some('*') {
            let start = i;
            i = i.saturating_add(2);
            let mut closed = false;
            while i.saturating_add(1) < chars.len() {
                if chars.get(i).copied() == Some('*')
                    && chars.get(i.saturating_add(1)).copied() == Some('/')
                {
                    i = i.saturating_add(2);
                    closed = true;
                    break;
                }
                i = i.saturating_add(1);
            }
            if !closed {
                return Err(format!(
                    "line {}: unterminated block comment",
                    line_of(chars.as_slice(), start)
                ));
            }
            continue;
        }
        if c.is_ascii_alphabetic() || c == '_' {
            let start = i;
            while i < chars.len()
                && (chars
                    .get(i)
                    .copied()
                    .unwrap_or('\0')
                    .is_ascii_alphanumeric()
                    || chars.get(i).copied().unwrap_or('\0') == '_')
            {
                i = i.saturating_add(1);
            }
            let text: String = chars.get(start..i).unwrap_or(&[]).iter().collect();
            out.push(Tok::Ident(text));
            continue;
        }
        if c.is_ascii_digit()
            || (c == '.'
                && chars
                    .get(i.saturating_add(1))
                    .is_some_and(|d| d.is_ascii_digit()))
        {
            let start = i;
            while i < chars.len() && chars.get(i).copied().unwrap_or('\0').is_ascii_digit() {
                i = i.saturating_add(1);
            }
            let mut is_float = false;
            if chars.get(i).copied() == Some('.') {
                is_float = true;
                i = i.saturating_add(1);
                while i < chars.len() && chars.get(i).copied().unwrap_or('\0').is_ascii_digit() {
                    i = i.saturating_add(1);
                }
            }
            if chars.get(i).copied() == Some('e') || chars.get(i).copied() == Some('E') {
                is_float = true;
                i = i.saturating_add(1);
                if chars.get(i).copied() == Some('+') || chars.get(i).copied() == Some('-') {
                    i = i.saturating_add(1);
                }
                while i < chars.len() && chars.get(i).copied().unwrap_or('\0').is_ascii_digit() {
                    i = i.saturating_add(1);
                }
            }
            let text: String = chars.get(start..i).unwrap_or(&[]).iter().collect();
            if is_float {
                let v = text.parse::<f32>().map_err(|_| {
                    format!(
                        "line {}: bad number {text}",
                        line_of(chars.as_slice(), start)
                    )
                })?;
                out.push(Tok::Float(v));
            } else {
                let v = text.parse::<i32>().map_err(|_| {
                    format!(
                        "line {}: bad number {text}",
                        line_of(chars.as_slice(), start)
                    )
                })?;
                out.push(Tok::Int(v));
            }
            continue;
        }
        // Multi-char operators.
        let two: String = chars
            .get(i..i.saturating_add(2).min(chars.len()))
            .unwrap_or(&[])
            .iter()
            .collect();
        let sym = match two.as_str() {
            "==" | "!=" | "<=" | ">=" | "&&" | "||" | "+=" | "-=" | "*=" | "/=" => {
                i = i.saturating_add(2);
                two
            }
            _ => {
                let one = c.to_string();
                i = i.saturating_add(1);
                one
            }
        };
        out.push(Tok::Sym(sym));
    }
    out.push(Tok::Eof);
    Ok(out)
}

fn line_of(chars: &[char], index: usize) -> usize {
    chars
        .get(..index)
        .unwrap_or(&[])
        .iter()
        .filter(|c| **c == '\n')
        .count()
        .saturating_add(1)
}

// ── AST ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Lt,
    Gt,
    Le,
    Ge,
    Eq,
    Ne,
    And,
    Or,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum UnOp {
    Neg,
    Not,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Expr {
    FloatLit(f32),
    IntLit(i32),
    BoolLit(bool),
    /// Variable read (local / built-in / uniform / varying).
    Var(String),
    /// Assignment to a variable name.
    Assign {
        name: String,
        value: Box<Expr>,
    },
    Bin {
        op: BinOp,
        left: Box<Expr>,
        right: Box<Expr>,
    },
    Un {
        op: UnOp,
        e: Box<Expr>,
    },
    Ternary {
        cond: Box<Expr>,
        yes: Box<Expr>,
        no: Box<Expr>,
    },
    Call {
        name: String,
        args: Vec<Expr>,
    },
    /// A `vecN/matN` constructor call (`vec4(...)`), or a user function call.
    Constructor {
        type_name: String,
        args: Vec<Expr>,
    },
    /// Swizzle read (`v.yxw`).
    Swizzle {
        base: Box<Expr>,
        mask: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Qualifier {
    None,
    Attribute,
    Varying,
    Uniform,
    Const,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct VarDecl {
    pub qualifier: Qualifier,
    pub type_name: String,
    pub name: String,
    pub init: Option<Expr>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Stmt {
    Decl(VarDecl),
    Expr(Expr),
    If {
        cond: Expr,
        then: Box<Stmt>,
        else_: Option<Box<Stmt>>,
    },
    For {
        init: VarDecl,
        cond: Expr,
        step: Expr,
        body: Box<Stmt>,
    },
    Return(Option<Expr>),
    Block(Vec<Stmt>),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct FunctionDef {
    pub name: String,
    pub params: Vec<VarDecl>,
    pub return_type: String,
    pub body: Vec<Stmt>,
}

/// A parsed + typechecked shader (the compile output).
#[derive(Debug, Clone)]
pub(crate) struct ParsedShader {
    pub functions: Vec<FunctionDef>,
    /// Index into `functions` of `main()`.
    pub main: usize,
    /// Uniform declaration names (in order).
    pub uniforms: Vec<String>,
    /// Varying declaration names (in order).
    pub varyings: Vec<String>,
    /// Varying name → component count (from the declared type).
    pub varying_comps: HashMap<String, u32>,
    /// Attribute declaration names (in order).
    pub attributes: Vec<String>,
}

// ── Parser ──────────────────────────────────────────────────────────────

struct Parser {
    toks: Vec<Tok>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> &Tok {
        self.toks.get(self.pos).unwrap_or(&Tok::Eof)
    }

    fn next(&mut self) -> Tok {
        let t = self.toks.get(self.pos).cloned().unwrap_or(Tok::Eof);
        self.pos = self.pos.saturating_add(1);
        t
    }

    fn expect_sym(&mut self, sym: &str) -> Result<(), String> {
        if self.peek() == &Tok::Sym(sym.to_owned()) {
            self.next();
            Ok(())
        } else {
            Err(format!(
                "line {}: expected '{sym}', found {:?}",
                self.line_of_peek(),
                self.peek()
            ))
        }
    }

    fn line_of_peek(&self) -> usize {
        // The parser tracks an absolute offset; approximate with the token
        // index (the lexer already reported precise line numbers for lex
        // errors; parse errors report the token position).
        self.pos.saturating_add(1)
    }

    fn eat_sym(&mut self, sym: &str) -> bool {
        if self.peek() == &Tok::Sym(sym.to_owned()) {
            self.next();
            true
        } else {
            false
        }
    }

    /// Parse one top-level declaration: a function or a global variable
    /// (uniform/varying/attribute/const or plain global).
    fn parse_top(
        &mut self,
        out: &mut Vec<FunctionDef>,
        globals: &mut Vec<VarDecl>,
    ) -> Result<(), String> {
        let qual = self.parse_qualifier();
        let type_name = self.expect_ident("type")?;
        let name = self.expect_ident("identifier")?;
        // Function?
        if self.peek() == &Tok::Sym("(".to_owned()) {
            self.next();
            let mut params = Vec::new();
            if self.peek() != &Tok::Sym(")".to_owned()) {
                loop {
                    let pqual = self.parse_qualifier();
                    let ptype = self.expect_ident("parameter type")?;
                    let pname = self.expect_ident("parameter name")?;
                    params.push(VarDecl {
                        qualifier: pqual,
                        type_name: ptype,
                        name: pname,
                        init: None,
                    });
                    if !self.eat_sym(",") {
                        break;
                    }
                }
            }
            self.expect_sym(")")?;
            if self.eat_sym(";") {
                // A function prototype without a body — accepted and ignored
                // (calls to it fail at typecheck as unknown).
                return Ok(());
            }
            self.expect_sym("{")?;
            let body = self.parse_block_until_rbrace()?;
            self.expect_sym("}")?;
            out.push(FunctionDef {
                name,
                params,
                return_type: type_name,
                body,
            });
            return Ok(());
        }
        // Global variable declaration (with optional init).
        let init = if self.eat_sym("=") {
            Some(self.parse_assign()?)
        } else {
            None
        };
        self.expect_sym(";")?;
        globals.push(VarDecl {
            qualifier: qual,
            type_name,
            name,
            init,
        });
        Ok(())
    }

    fn parse_qualifier(&mut self) -> Qualifier {
        let name = match self.peek() {
            Tok::Ident(n) => n.clone(),
            _ => return Qualifier::None,
        };
        let q = match name.as_str() {
            "attribute" => Qualifier::Attribute,
            "varying" => Qualifier::Varying,
            "uniform" => Qualifier::Uniform,
            "const" => Qualifier::Const,
            "highp" | "mediump" | "lowp" => {
                // Precision qualifier — parsed and ignored.
                self.next();
                return Qualifier::None;
            }
            _ => return Qualifier::None,
        };
        self.next();
        q
    }

    fn expect_ident(&mut self, what: &str) -> Result<String, String> {
        match self.next() {
            Tok::Ident(n) => Ok(n),
            other => Err(format!(
                "line {}: expected {what}, found {other:?}",
                self.pos
            )),
        }
    }

    fn parse_block_until_rbrace(&mut self) -> Result<Vec<Stmt>, String> {
        let mut stmts = Vec::new();
        while self.peek() != &Tok::Sym("}".to_owned()) {
            if self.peek() == &Tok::Eof {
                return Err("line {self.pos}: unexpected end of shader (missing '}')".to_owned());
            }
            stmts.push(self.parse_stmt()?);
        }
        Ok(stmts)
    }

    fn parse_stmt(&mut self) -> Result<Stmt, String> {
        match self.peek() {
            Tok::Sym(s) if s == "{" => {
                self.next();
                let body = self.parse_block_until_rbrace()?;
                self.expect_sym("}")?;
                Ok(Stmt::Block(body))
            }
            Tok::Sym(s) if s == ";" => {
                self.next();
                Ok(Stmt::Block(Vec::new()))
            }
            Tok::Ident(kw) => match kw.as_str() {
                "if" => {
                    self.next();
                    self.expect_sym("(")?;
                    let cond = self.parse_assign()?;
                    self.expect_sym(")")?;
                    let then = Box::new(self.parse_stmt()?);
                    let else_ = if self.eat_sym("else") {
                        Some(Box::new(self.parse_stmt()?))
                    } else {
                        None
                    };
                    Ok(Stmt::If { cond, then, else_ })
                }
                "for" => {
                    self.next();
                    self.expect_sym("(")?;
                    let init = self.parse_var_decl()?;
                    self.expect_sym(";")?;
                    let cond = self.parse_assign()?;
                    self.expect_sym(";")?;
                    let step = self.parse_assign()?;
                    self.expect_sym(")")?;
                    let body = Box::new(self.parse_stmt()?);
                    Ok(Stmt::For {
                        init,
                        cond,
                        step,
                        body,
                    })
                }
                "return" => {
                    self.next();
                    if self.eat_sym(";") {
                        Ok(Stmt::Return(None))
                    } else {
                        let e = self.parse_assign()?;
                        self.expect_sym(";")?;
                        Ok(Stmt::Return(Some(e)))
                    }
                }
                _ => {
                    if is_type_keyword(kw) {
                        let d = self.parse_var_decl()?;
                        self.expect_sym(";")?;
                        Ok(Stmt::Decl(d))
                    } else {
                        // A bare qualifier before a type (e.g. `uniform float
                        // x;` inside a block) or an expression statement.
                        let saved = self.pos;
                        let qual = self.parse_qualifier();
                        if qual != Qualifier::None && self.peek_ident_is_type() {
                            let d = self.parse_var_decl_with_qual(qual)?;
                            self.expect_sym(";")?;
                            Ok(Stmt::Decl(d))
                        } else {
                            self.pos = saved;
                            let e = self.parse_assign()?;
                            self.expect_sym(";")?;
                            Ok(Stmt::Expr(e))
                        }
                    }
                }
            },
            _ => {
                let e = self.parse_assign()?;
                self.expect_sym(";")?;
                Ok(Stmt::Expr(e))
            }
        }
    }

    fn peek_ident_is_type(&self) -> bool {
        matches!(self.peek(), Tok::Ident(n) if is_type_keyword(n))
    }

    fn parse_var_decl(&mut self) -> Result<VarDecl, String> {
        let qual = self.parse_qualifier();
        self.parse_var_decl_with_qual(qual)
    }

    fn parse_var_decl_with_qual(&mut self, qual: Qualifier) -> Result<VarDecl, String> {
        let type_name = self.expect_ident("declaration type")?;
        if parse_type_name(&type_name).is_none() {
            return Err(format!("line {}: unknown type '{type_name}'", self.pos));
        }
        let name = self.expect_ident("declaration name")?;
        let init = if self.eat_sym("=") {
            Some(self.parse_assign()?)
        } else {
            None
        };
        Ok(VarDecl {
            qualifier: qual,
            type_name,
            name,
            init,
        })
    }

    // Expression grammar: assign → ternary → or → and → equality →
    // relational → additive → multiplicative → unary → postfix → primary.

    fn parse_assign(&mut self) -> Result<Expr, String> {
        let left = self.parse_ternary()?;
        if let Expr::Var(name) = &left {
            let name = name.clone();
            let op = match self.peek() {
                Tok::Sym(s) if s == "=" || s == "+=" || s == "-=" || s == "*=" || s == "/=" => {
                    let s = s.clone();
                    self.next();
                    Some(s)
                }
                _ => None,
            };
            if let Some(op) = op {
                let value = self.parse_assign()?;
                let value = if op == "=" {
                    value
                } else {
                    let bin = match op.as_str() {
                        "+=" => BinOp::Add,
                        "-=" => BinOp::Sub,
                        "*=" => BinOp::Mul,
                        _ => BinOp::Div,
                    };
                    Expr::Bin {
                        op: bin,
                        left: Box::new(Expr::Var(name.clone())),
                        right: Box::new(value),
                    }
                };
                return Ok(Expr::Assign {
                    name,
                    value: Box::new(value),
                });
            }
        }
        Ok(left)
    }

    fn parse_ternary(&mut self) -> Result<Expr, String> {
        let cond = self.parse_or()?;
        if self.eat_sym("?") {
            let yes = self.parse_assign()?;
            self.expect_sym(":")?;
            let no = self.parse_assign()?;
            Ok(Expr::Ternary {
                cond: Box::new(cond),
                yes: Box::new(yes),
                no: Box::new(no),
            })
        } else {
            Ok(cond)
        }
    }

    fn parse_or(&mut self) -> Result<Expr, String> {
        let mut left = self.parse_and()?;
        while self.eat_sym("||") {
            let right = self.parse_and()?;
            left = Expr::Bin {
                op: BinOp::Or,
                left: Box::new(left),
                right: Box::new(right),
            };
        }
        Ok(left)
    }

    fn parse_and(&mut self) -> Result<Expr, String> {
        let mut left = self.parse_equality()?;
        while self.eat_sym("&&") {
            let right = self.parse_equality()?;
            left = Expr::Bin {
                op: BinOp::And,
                left: Box::new(left),
                right: Box::new(right),
            };
        }
        Ok(left)
    }

    fn parse_equality(&mut self) -> Result<Expr, String> {
        let mut left = self.parse_relational()?;
        loop {
            let op = match self.peek() {
                Tok::Sym(s) if s == "==" => BinOp::Eq,
                Tok::Sym(s) if s == "!=" => BinOp::Ne,
                _ => break,
            };
            self.next();
            let right = self.parse_relational()?;
            left = Expr::Bin {
                op,
                left: Box::new(left),
                right: Box::new(right),
            };
        }
        Ok(left)
    }

    fn parse_relational(&mut self) -> Result<Expr, String> {
        let mut left = self.parse_additive()?;
        loop {
            let op = match self.peek() {
                Tok::Sym(s) if s == "<" => BinOp::Lt,
                Tok::Sym(s) if s == ">" => BinOp::Gt,
                Tok::Sym(s) if s == "<=" => BinOp::Le,
                Tok::Sym(s) if s == ">=" => BinOp::Ge,
                _ => break,
            };
            self.next();
            let right = self.parse_additive()?;
            left = Expr::Bin {
                op,
                left: Box::new(left),
                right: Box::new(right),
            };
        }
        Ok(left)
    }

    fn parse_additive(&mut self) -> Result<Expr, String> {
        let mut left = self.parse_multiplicative()?;
        loop {
            let op = match self.peek() {
                Tok::Sym(s) if s == "+" => BinOp::Add,
                Tok::Sym(s) if s == "-" => BinOp::Sub,
                _ => break,
            };
            self.next();
            let right = self.parse_multiplicative()?;
            left = Expr::Bin {
                op,
                left: Box::new(left),
                right: Box::new(right),
            };
        }
        Ok(left)
    }

    fn parse_multiplicative(&mut self) -> Result<Expr, String> {
        let mut left = self.parse_unary()?;
        loop {
            let op = match self.peek() {
                Tok::Sym(s) if s == "*" => BinOp::Mul,
                Tok::Sym(s) if s == "/" => BinOp::Div,
                _ => break,
            };
            self.next();
            let right = self.parse_unary()?;
            left = Expr::Bin {
                op,
                left: Box::new(left),
                right: Box::new(right),
            };
        }
        Ok(left)
    }

    fn parse_unary(&mut self) -> Result<Expr, String> {
        if self.eat_sym("-") {
            let e = self.parse_unary()?;
            Ok(Expr::Un {
                op: UnOp::Neg,
                e: Box::new(e),
            })
        } else if self.eat_sym("!") {
            let e = self.parse_unary()?;
            Ok(Expr::Un {
                op: UnOp::Not,
                e: Box::new(e),
            })
        } else {
            self.parse_postfix()
        }
    }

    fn parse_postfix(&mut self) -> Result<Expr, String> {
        let mut e = self.parse_primary()?;
        while self.peek() == &Tok::Sym(".".to_owned()) {
            self.next();
            let mask = self.expect_ident("swizzle component")?;
            e = Expr::Swizzle {
                base: Box::new(e),
                mask,
            };
        }
        Ok(e)
    }

    fn parse_primary(&mut self) -> Result<Expr, String> {
        match self.next() {
            Tok::Float(v) => Ok(Expr::FloatLit(v)),
            Tok::Int(v) => Ok(Expr::IntLit(v)),
            Tok::Ident(name) => {
                if name == "true" {
                    Ok(Expr::BoolLit(true))
                } else if name == "false" {
                    Ok(Expr::BoolLit(false))
                } else if self.peek() == &Tok::Sym("(".to_owned()) {
                    self.next();
                    if parse_type_name(&name).is_some() {
                        // Constructor.
                        let mut args = Vec::new();
                        if self.peek() != &Tok::Sym(")".to_owned()) {
                            loop {
                                args.push(self.parse_assign()?);
                                if !self.eat_sym(",") {
                                    break;
                                }
                            }
                        }
                        self.expect_sym(")")?;
                        Ok(Expr::Constructor {
                            type_name: name,
                            args,
                        })
                    } else {
                        // User function call.
                        let mut args = Vec::new();
                        if self.peek() != &Tok::Sym(")".to_owned()) {
                            loop {
                                args.push(self.parse_assign()?);
                                if !self.eat_sym(",") {
                                    break;
                                }
                            }
                        }
                        self.expect_sym(")")?;
                        Ok(Expr::Call { name, args })
                    }
                } else {
                    Ok(Expr::Var(name))
                }
            }
            Tok::Sym(s) if s == "(" => {
                let e = self.parse_assign()?;
                self.expect_sym(")")?;
                Ok(e)
            }
            other => Err(format!("line {}: unexpected token {other:?}", self.pos)),
        }
    }
}

/// A function prototype (`void foo();`) — accepted and ignored; a call to a
/// body-less function fails the typecheck as unknown.
fn is_type_keyword(name: &str) -> bool {
    parse_type_name(name).is_some()
}

// ── Static typecheck ────────────────────────────────────────────────────

struct TypeChecker<'a> {
    parsed: &'a ParsedShader,
    func_types: HashMap<String, (Vec<GlslType>, GlslType)>,
    /// Uniform / varying / attribute global names and their types.
    globals: HashMap<String, GlslType>,
    /// The shader kind (vertex writes gl_Position; fragment gl_FragColor).
    kind: u32,
}

impl<'a> TypeChecker<'a> {
    fn typecheck(&mut self) -> Result<(), String> {
        for (i, f) in self.parsed.functions.iter().enumerate() {
            if i == self.parsed.main {
                continue;
            }
            let ret = parse_type_name(&f.return_type).ok_or_else(|| {
                format!(
                    "function '{}' has unknown return type '{}'",
                    f.name, f.return_type
                )
            })?;
            let params = f
                .params
                .iter()
                .map(|p| {
                    parse_type_name(&p.type_name).ok_or_else(|| {
                        format!("parameter '{}' has unknown type '{}'", p.name, p.type_name)
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            self.func_types.insert(f.name.clone(), (params, ret));
        }
        let main = self
            .parsed
            .functions
            .get(self.parsed.main)
            .ok_or("shader has no main()")?;
        if main.name != "main" || main.return_type != "void" {
            return Err("main() must return void".to_owned());
        }
        let mut scopes = Vec::new();
        scopes.push(HashMap::<String, GlslType>::new());
        self.check_stmts(&main.body, &mut scopes, "void")?;
        Ok(())
    }

    fn check_stmts(
        &mut self,
        stmts: &[Stmt],
        scopes: &mut Vec<HashMap<String, GlslType>>,
        ret: &str,
    ) -> Result<(), String> {
        for s in stmts {
            self.check_stmt(s, scopes, ret)?;
        }
        Ok(())
    }

    fn check_stmt(
        &mut self,
        s: &Stmt,
        scopes: &mut Vec<HashMap<String, GlslType>>,
        ret: &str,
    ) -> Result<(), String> {
        match s {
            Stmt::Block(body) => {
                scopes.push(HashMap::new());
                let r = self.check_stmts(body, scopes, ret);
                scopes.pop();
                r
            }
            Stmt::Decl(d) => {
                let t = parse_type_name(&d.type_name)
                    .ok_or_else(|| format!("unknown type '{}'", d.type_name))?;
                if let Some(init) = &d.init {
                    let it = self.check_expr(init, scopes)?;
                    if !assignable(t, it) {
                        return Err(format!(
                            "cannot initialize '{}' ({} ) with {}",
                            d.name,
                            type_name_str(t),
                            type_name_str(it)
                        ));
                    }
                }
                let slot = scopes.last_mut().ok_or("no scope")?;
                slot.insert(d.name.clone(), t);
                Ok(())
            }
            Stmt::Expr(e) => {
                self.check_expr(e, scopes)?;
                Ok(())
            }
            Stmt::If { cond, then, else_ } => {
                let ct = self.check_expr(cond, scopes)?;
                if ct != GlslType::Scalar(BaseType::Bool) {
                    return Err("if condition must be bool".to_owned());
                }
                self.check_stmt(then, scopes, ret)?;
                if let Some(e) = else_ {
                    self.check_stmt(e, scopes, ret)?;
                }
                Ok(())
            }
            Stmt::For {
                init,
                cond,
                step,
                body,
            } => {
                scopes.push(HashMap::new());
                let it = parse_type_name(&init.type_name)
                    .ok_or_else(|| format!("unknown type '{}'", init.type_name))?;
                if let Some(i) = &init.init {
                    let iit = self.check_expr(i, scopes)?;
                    if !assignable(it, iit) {
                        return Err("for-init type mismatch".to_owned());
                    }
                }
                let slot = scopes.last_mut().ok_or("no scope")?;
                slot.insert(init.name.clone(), it);
                let ct = self.check_expr(cond, scopes)?;
                if ct != GlslType::Scalar(BaseType::Bool) {
                    return Err("for condition must be bool".to_owned());
                }
                self.check_expr(step, scopes)?;
                let r = self.check_stmt(body, scopes, ret);
                scopes.pop();
                r
            }
            Stmt::Return(e) => {
                let rt = parse_type_name(ret).ok_or("unknown return type")?;
                if let Some(e) = e {
                    let et = self.check_expr(e, scopes)?;
                    if !assignable(rt, et) {
                        return Err(format!(
                            "return type mismatch: {} vs {}",
                            type_name_str(rt),
                            type_name_str(et)
                        ));
                    }
                } else if rt != GlslType::Scalar(BaseType::Float) || ret != "void" {
                    // `return;` only valid in void.
                    if ret != "void" {
                        return Err("return; in non-void function".to_owned());
                    }
                }
                Ok(())
            }
        }
    }

    fn check_expr(
        &mut self,
        e: &Expr,
        scopes: &mut Vec<HashMap<String, GlslType>>,
    ) -> Result<GlslType, String> {
        match e {
            Expr::FloatLit(_) => Ok(GlslType::Scalar(BaseType::Float)),
            Expr::IntLit(_) => Ok(GlslType::Scalar(BaseType::Int)),
            Expr::BoolLit(_) => Ok(GlslType::Scalar(BaseType::Bool)),
            Expr::Var(name) => {
                if let Some(t) = self.resolve_var(name, scopes) {
                    return Ok(t);
                }
                Err(format!("unknown identifier '{name}'"))
            }
            Expr::Assign { name, value } => {
                let t = self
                    .resolve_var(name, scopes)
                    .ok_or_else(|| format!("unknown identifier '{name}'"))?;
                let vt = self.check_expr(value, scopes)?;
                if !assignable(t, vt) {
                    return Err(format!(
                        "cannot assign {} to {}",
                        type_name_str(vt),
                        type_name_str(t)
                    ));
                }
                self.check_builtin_write(name, &t)?;
                Ok(t)
            }
            Expr::Bin { op, left, right } => {
                let lt = self.check_expr(left, scopes)?;
                let rt = self.check_expr(right, scopes)?;
                self.check_bin(*op, lt, rt)
            }
            Expr::Un { op, e } => {
                let t = self.check_expr(e, scopes)?;
                match op {
                    UnOp::Neg => {
                        if matches!(
                            t,
                            GlslType::Scalar(BaseType::Float) | GlslType::Scalar(BaseType::Int)
                        ) || matches!(t, GlslType::Vec(_, _))
                        {
                            Ok(t)
                        } else {
                            Err("unary - on non-numeric".to_owned())
                        }
                    }
                    UnOp::Not => {
                        if t == GlslType::Scalar(BaseType::Bool)
                            || matches!(t, GlslType::Vec(BaseType::Bool, _))
                        {
                            Ok(t)
                        } else {
                            Err("unary ! on non-bool".to_owned())
                        }
                    }
                }
            }
            Expr::Ternary { cond, yes, no } => {
                let ct = self.check_expr(cond, scopes)?;
                if ct != GlslType::Scalar(BaseType::Bool) {
                    return Err("ternary condition must be bool".to_owned());
                }
                let yt = self.check_expr(yes, scopes)?;
                let nt = self.check_expr(no, scopes)?;
                if yt != nt {
                    return Err("ternary arms must have the same type".to_owned());
                }
                Ok(yt)
            }
            Expr::Call { name, args } => {
                // Builtin texture sampler: texture2D(sampler2D, vec2) → vec4.
                if name == "texture2D" {
                    if args.len() != 2 {
                        return Err("texture2D expects (sampler2D, vec2)".to_owned());
                    }
                    let st = self.check_expr(&args[0], scopes)?;
                    let ut = self.check_expr(&args[1], scopes)?;
                    if st != GlslType::Scalar(BaseType::Sampler) {
                        return Err("texture2D first arg must be a sampler2D".to_owned());
                    }
                    if ut != GlslType::Vec(BaseType::Float, 2) {
                        return Err("texture2D second arg must be a vec2".to_owned());
                    }
                    return Ok(GlslType::Vec(BaseType::Float, 4));
                }
                let (params, ret) = self
                    .func_types
                    .get(name)
                    .cloned()
                    .ok_or_else(|| format!("unknown function '{name}'"))?;
                if params.len() != args.len() {
                    return Err(format!(
                        "function '{name}' expects {} args, got {}",
                        params.len(),
                        args.len()
                    ));
                }
                for (p, a) in params.iter().zip(args) {
                    let at = self.check_expr(a, scopes)?;
                    if !assignable(*p, at) {
                        return Err(format!("argument type mismatch in '{name}'"));
                    }
                }
                Ok(ret)
            }
            Expr::Constructor { type_name, args } => {
                let t = parse_type_name(type_name)
                    .ok_or_else(|| format!("unknown constructor type '{type_name}'"))?;
                let mut arg_types = Vec::with_capacity(args.len());
                for a in args {
                    arg_types.push(self.check_expr(a, scopes)?);
                }
                self.check_constructor(t, &arg_types)?;
                Ok(t)
            }
            Expr::Swizzle { base, mask } => {
                let bt = self.check_expr(base, scopes)?;
                self.check_swizzle(bt, mask)?;
                let comps = u32::try_from(mask.len()).unwrap_or(0);
                Ok(GlslType::Vec(BaseType::Float, comps))
            }
        }
    }

    fn resolve_var(&self, name: &str, scopes: &[HashMap<String, GlslType>]) -> Option<GlslType> {
        // Local scopes (innermost first).
        for scope in scopes.iter().rev() {
            if let Some(t) = scope.get(name) {
                return Some(*t);
            }
        }
        if let Some(t) = self.globals.get(name) {
            return Some(*t);
        }
        // Built-in attribute/input/output names.
        match name {
            "gl_Vertex" => Some(GlslType::Vec(BaseType::Float, 4)),
            "gl_Color" => Some(GlslType::Vec(BaseType::Float, 4)),
            "gl_Normal" => Some(GlslType::Vec(BaseType::Float, 3)),
            "gl_MultiTexCoord0" => Some(GlslType::Vec(BaseType::Float, 4)),
            "gl_ModelViewProjectionMatrix" => Some(GlslType::Mat(4)),
            "gl_Position" if self.kind == GL_VERTEX_SHADER => {
                Some(GlslType::Vec(BaseType::Float, 4))
            }
            "gl_FrontColor" if self.kind == GL_VERTEX_SHADER => {
                Some(GlslType::Vec(BaseType::Float, 4))
            }
            "gl_FragColor" if self.kind == GL_FRAGMENT_SHADER => {
                Some(GlslType::Vec(BaseType::Float, 4))
            }
            "gl_FragCoord" if self.kind == GL_FRAGMENT_SHADER => {
                Some(GlslType::Vec(BaseType::Float, 4))
            }
            _ => None,
        }
    }

    fn check_builtin_write(&self, name: &str, t: &GlslType) -> Result<(), String> {
        let expect = |what: &str, ok: bool| {
            if ok {
                Ok(())
            } else {
                Err(format!("{what} must be a vec4"))
            }
        };
        match name {
            "gl_Position" => expect("gl_Position", *t == GlslType::Vec(BaseType::Float, 4)),
            "gl_FragColor" => expect("gl_FragColor", *t == GlslType::Vec(BaseType::Float, 4)),
            _ => Ok(()),
        }
    }

    fn check_bin(&self, op: BinOp, l: GlslType, r: GlslType) -> Result<GlslType, String> {
        use BinOp::*;
        match op {
            And | Or => {
                if l == GlslType::Scalar(BaseType::Bool) && r == GlslType::Scalar(BaseType::Bool) {
                    Ok(GlslType::Scalar(BaseType::Bool))
                } else {
                    Err("&& / || require bool operands".to_owned())
                }
            }
            Lt | Gt | Le | Ge => {
                if self.numeric(l) && l == r {
                    Ok(GlslType::Scalar(BaseType::Bool))
                } else {
                    Err("relational comparison requires matching numeric operands".to_owned())
                }
            }
            Eq | Ne => {
                if l == r {
                    Ok(GlslType::Scalar(BaseType::Bool))
                } else {
                    Err("== / != require matching operands".to_owned())
                }
            }
            Add | Sub | Div => {
                if self.numeric(l) && self.numeric(r) && (l == r || l.is_scalar() || r.is_scalar())
                {
                    // GLSL broadcast: scalar op vec/mat applies componentwise;
                    // the result takes the non-scalar side's type.
                    Ok(if l.is_scalar() { r } else { l })
                } else {
                    Err("arithmetic requires matching numeric operands".to_owned())
                }
            }
            Mul => {
                // scalar×any, vec×vec componentwise, mat×vec → vec, vec×mat → vec,
                // mat×mat → mat.
                if l == r {
                    Ok(l)
                } else if l.is_scalar() {
                    Ok(r)
                } else if r.is_scalar() {
                    Ok(l)
                } else {
                    let size = |t: GlslType| match t {
                        GlslType::Mat(n) | GlslType::Vec(BaseType::Float, n) => Some(n),
                        _ => None,
                    };
                    if let (Some(n), Some(m)) = (size(l), size(r))
                        && n == m
                    {
                        // mat×vec and vec×mat both yield the vec (GLSL).
                        match (l, r) {
                            (GlslType::Mat(_), GlslType::Mat(_)) => Ok(l),
                            (GlslType::Mat(_), GlslType::Vec(_, _)) => Ok(r),
                            (GlslType::Vec(_, _), GlslType::Mat(_)) => Ok(l),
                            _ => Err("incompatible * operands".to_owned()),
                        }
                    } else {
                        Err("incompatible * operands".to_owned())
                    }
                }
            }
        }
    }

    fn numeric(&self, t: GlslType) -> bool {
        matches!(
            t,
            GlslType::Scalar(BaseType::Float) | GlslType::Scalar(BaseType::Int)
        ) || matches!(
            t,
            GlslType::Vec(BaseType::Float, _) | GlslType::Vec(BaseType::Int, _)
        ) || matches!(t, GlslType::Mat(_))
    }

    fn check_constructor(&self, t: GlslType, arg_types: &[GlslType]) -> Result<(), String> {
        // GLSL constructors accept any mix of scalars/vectors whose total
        // component count equals the target: vec4(vec2, f, f) = 2+1+1 = 4,
        // vec4(f, f, f, f) = 4, mat4(vec4×4) = 16, vec4(1.0) broadcast = 1.
        let argc = arg_types.len();
        let sum: usize = arg_types.iter().map(|at| at.components()).sum();
        let ok = match t {
            GlslType::Scalar(BaseType::Float) => argc == 1,
            GlslType::Scalar(BaseType::Int) | GlslType::Scalar(BaseType::Bool) => argc == 1,
            GlslType::Vec(_, n) => {
                let n = usize::try_from(n).unwrap_or(0);
                // One scalar (broadcast) or one vector of equal size (subset
                // copy); otherwise the component sum must equal N.
                (argc == 1 && sum == 1) || sum == n
            }
            GlslType::Mat(n) => {
                let n = usize::try_from(n).unwrap_or(0);
                (argc == 1 && sum == 1) || sum == n * n
            }
            GlslType::Scalar(BaseType::Sampler) => false,
        };
        if ok {
            Ok(())
        } else {
            Err(format!(
                "invalid constructor arity for {}",
                type_name_str(t)
            ))
        }
    }

    fn check_swizzle(&self, base: GlslType, mask: &str) -> Result<(), String> {
        let n = match base {
            GlslType::Vec(_, n) => n,
            GlslType::Scalar(BaseType::Float) => 1,
            _ => {
                return Err("swizzle on non-vector".to_owned());
            }
        };
        if mask.is_empty() || mask.len() > 4 {
            return Err("swizzle mask length must be 1..4".to_owned());
        }
        let valid = match n {
            1 => "xrastpq",
            2 => "xyrastpq",
            _ => "xyzwrgbastpq",
        };
        for c in mask.chars() {
            if !valid.contains(c) {
                return Err(format!(
                    "invalid swizzle component '{c}' for a {n}-component vector"
                ));
            }
        }
        Ok(())
    }
}

fn assignable(target: GlslType, value: GlslType) -> bool {
    // int → float widening is the only implicit conversion.
    if target == value {
        return true;
    }
    target == GlslType::Scalar(BaseType::Float) && value == GlslType::Scalar(BaseType::Int)
}

fn type_name_str(t: GlslType) -> String {
    match t {
        GlslType::Scalar(BaseType::Float) => "float".to_owned(),
        GlslType::Scalar(BaseType::Int) => "int".to_owned(),
        GlslType::Scalar(BaseType::Bool) => "bool".to_owned(),
        GlslType::Scalar(BaseType::Sampler) => "sampler2D".to_owned(),
        GlslType::Vec(BaseType::Float, n) => format!("vec{n}"),
        GlslType::Vec(BaseType::Int, n) => format!("ivec{n}"),
        GlslType::Vec(BaseType::Sampler, _) => "sampler2D".to_owned(),
        GlslType::Vec(BaseType::Bool, n) => format!("bvec{n}"),
        GlslType::Mat(n) => format!("mat{n}"),
    }
}

// ── Shader / program object store ───────────────────────────────────────

/// A `glCreateShader` object.
#[derive(Debug)]
pub(crate) struct ShaderObject {
    pub id: u32,
    pub kind: u32,
    pub source: String,
    pub compiled: bool,
    pub info_log: String,
    pub parsed: Option<ParsedShader>,
}

/// A `glCreateProgram` object.
#[derive(Debug)]
pub(crate) struct ProgramObject {
    pub id: u32,
    pub attached: Vec<u32>,
    pub linked: bool,
    pub info_log: String,
    /// Linked vertex stage (None = fixed function).
    pub vs: Option<LinkedVertexShader>,
    /// Linked fragment stage (None = fixed function).
    pub fs: Option<LinkedFragmentShader>,
    /// Uniform values indexed by location (set by glUniform*).
    pub uniforms: Vec<GlslVal>,
    /// Uniform name → location.
    pub uniform_index: HashMap<String, usize>,
    pub active_attributes: usize,
}

/// The linked + validated vertex stage.
#[derive(Debug)]
pub(crate) struct LinkedVertexShader {
    pub functions: Vec<FunctionDef>,
    pub main: usize,
    /// Varying name → slot (0..MAX_VARYINGS).
    pub varying_slots: HashMap<String, usize>,
    /// Varying name → component count (from the declared type).
    pub varying_comps: HashMap<String, u32>,
}

/// The linked + validated fragment stage.
#[derive(Debug)]
pub(crate) struct LinkedFragmentShader {
    pub functions: Vec<FunctionDef>,
    pub main: usize,
    pub varying_slots: HashMap<String, usize>,
    /// Varying name → component count (from the declared type).
    pub varying_comps: HashMap<String, u32>,
}

/// `glCreateShader(kind)` — a new shader object id (0 on failure).
pub(crate) fn gl_create_shader(ctx: &mut GlCtx, kind: u32) -> u32 {
    if !matches!(kind, GL_VERTEX_SHADER | GL_FRAGMENT_SHADER) {
        ctx.set_error(GL_INVALID_ENUM);
        return 0;
    }
    let mut id = ctx.next_shader_id;
    while id == 0 || ctx.shaders.iter().any(|s| s.id == id) {
        id = id.wrapping_add(1);
    }
    ctx.next_shader_id = id.wrapping_add(1);
    ctx.shaders.push(ShaderObject {
        id,
        kind,
        source: String::new(),
        compiled: false,
        info_log: String::new(),
        parsed: None,
    });
    id
}

/// `glShaderSource(id, source)` — replace the source (marks it un-compiled).
pub(crate) fn gl_shader_source(ctx: &mut GlCtx, id: u32, source: &str) {
    let Some(shader) = ctx.shaders.iter_mut().find(|s| s.id == id) else {
        return; // no such shader: GL ignores the call
    };
    shader.source = source.to_owned();
    shader.compiled = false;
    shader.parsed = None;
}

/// `glCompileShader(id)` — lex + parse + typecheck; sets the status + log.
pub(crate) fn gl_compile_shader(ctx: &mut GlCtx, id: u32) {
    let Some(shader) = ctx.shaders.iter_mut().find(|s| s.id == id) else {
        return;
    };
    match parse_shader(&shader.source, shader.kind) {
        Ok(parsed) => {
            shader.parsed = Some(parsed);
            shader.compiled = true;
            shader.info_log.clear();
        }
        Err(err) => {
            shader.parsed = None;
            shader.compiled = false;
            shader.info_log = err;
        }
    }
}

/// `glDeleteShader(id)`.
pub(crate) fn gl_delete_shader(ctx: &mut GlCtx, id: u32) {
    ctx.shaders.retain(|s| s.id != id);
}

/// `glIsShader(id)`.
#[must_use]
pub(crate) fn gl_is_shader(ctx: &GlCtx, id: u32) -> bool {
    ctx.shaders.iter().any(|s| s.id == id)
}

/// `glCreateProgram()` — a new program object id (0 on failure).
pub(crate) fn gl_create_program(ctx: &mut GlCtx) -> u32 {
    let mut id = ctx.next_program_id;
    while id == 0 || ctx.programs.iter().any(|p| p.id == id) {
        id = id.wrapping_add(1);
    }
    ctx.next_program_id = id.wrapping_add(1);
    ctx.programs.push(ProgramObject {
        id,
        attached: Vec::new(),
        linked: false,
        info_log: String::new(),
        vs: None,
        fs: None,
        uniforms: Vec::new(),
        uniform_index: HashMap::new(),
        active_attributes: 0,
    });
    id
}

/// `glAttachShader(program, shader)`.
pub(crate) fn gl_attach_shader(ctx: &mut GlCtx, program: u32, shader: u32) {
    let attached_ok = ctx.shaders.iter().any(|s| s.id == shader);
    let Some(prog) = ctx.programs.iter_mut().find(|p| p.id == program) else {
        return;
    };
    if !attached_ok {
        return;
    }
    if !prog.attached.contains(&shader) {
        prog.attached.push(shader);
        prog.linked = false;
    }
}

/// `glDetachShader(program, shader)`.
pub(crate) fn gl_detach_shader(ctx: &mut GlCtx, program: u32, shader: u32) {
    let Some(prog) = ctx.programs.iter_mut().find(|p| p.id == program) else {
        return;
    };
    prog.attached.retain(|s| *s != shader);
    prog.linked = false;
}

/// `glLinkProgram(program)` — resolve attributes/uniforms across both
/// stages, validate (main per stage, constant loop bounds, built-in
/// attributes only, matching varyings), assign varying slots.
pub(crate) fn gl_link_program(ctx: &mut GlCtx, program: u32) {
    let program_id = program;
    let Some(prog) = ctx.programs.iter_mut().find(|p| p.id == program_id) else {
        return;
    };
    let attached: Vec<u32> = prog.attached.clone();
    let vs_src = find_attached(&ctx.shaders, &attached, GL_VERTEX_SHADER);
    let fs_src = find_attached(&ctx.shaders, &attached, GL_FRAGMENT_SHADER);
    match link_stages(&ctx.shaders, &attached, vs_src, fs_src) {
        Ok((vs, fs, uniforms, uniform_index, attribute_count)) => {
            prog.vs = vs;
            prog.fs = fs;
            prog.uniforms = uniforms;
            prog.uniform_index = uniform_index;
            prog.active_attributes = attribute_count;
            prog.linked = true;
            prog.info_log.clear();
        }
        Err(err) => {
            prog.linked = false;
            prog.info_log = err;
        }
    }
}

fn find_attached(shaders: &[ShaderObject], attached: &[u32], kind: u32) -> Option<usize> {
    attached.iter().find_map(|id| {
        shaders
            .iter()
            .position(|s| s.id == *id && s.kind == kind && s.compiled && s.parsed.is_some())
    })
}

/// The linked program stages + the uniform table a `ProgramObject` needs.
///
/// `(vs, fs, uniform_values, uniform_index, attribute_count)` — kept as a
/// tuple so the caller can destructure without naming an intermediate type.
type LinkedStages = (
    Option<LinkedVertexShader>,
    Option<LinkedFragmentShader>,
    Vec<GlslVal>,
    HashMap<String, usize>,
    usize,
);

fn link_stages(
    shaders: &[ShaderObject],
    _attached: &[u32],
    vs_src: Option<usize>,
    fs_src: Option<usize>,
) -> Result<LinkedStages, String> {
    let mut uniforms: Vec<GlslVal> = Vec::new();
    let mut uniform_index = HashMap::new();
    let mut attribute_count = 0_usize;

    let vs = if let Some(idx) = vs_src {
        let parsed = shaders
            .get(idx)
            .and_then(|s| s.parsed.clone())
            .ok_or("vertex shader not compiled")?;
        validate_loop_bounds(&parsed)?;
        for name in &parsed.attributes {
            if !matches!(
                name.as_str(),
                "gl_Vertex" | "gl_Color" | "gl_Normal" | "gl_MultiTexCoord0"
            ) {
                return Err(format!(
                    "link: custom attribute '{name}' is not supported (only gl_Vertex/gl_Color/gl_Normal/gl_MultiTexCoord0)"
                ));
            }
        }
        attribute_count = parsed.attributes.len();
        let mut varying_slots = HashMap::new();
        for (i, v) in parsed.varyings.iter().enumerate() {
            if i >= MAX_VARYINGS {
                return Err(format!("link: more than {MAX_VARYINGS} varyings"));
            }
            varying_slots.insert(v.clone(), i);
        }
        for name in &parsed.uniforms {
            register_uniform(&mut uniforms, &mut uniform_index, name.clone(), 0.0);
        }
        Some(LinkedVertexShader {
            functions: parsed.functions.clone(),
            main: parsed.main,
            varying_slots,
            varying_comps: parsed.varying_comps.clone(),
        })
    } else {
        None
    };

    let fs = if let Some(idx) = fs_src {
        let parsed = shaders
            .get(idx)
            .and_then(|s| s.parsed.clone())
            .ok_or("fragment shader not compiled")?;
        validate_loop_bounds(&parsed)?;
        let mut varying_slots = HashMap::new();
        for (i, v) in parsed.varyings.iter().enumerate() {
            if i >= MAX_VARYINGS {
                return Err(format!("link: more than {MAX_VARYINGS} varyings"));
            }
            varying_slots.insert(v.clone(), i);
        }
        // FS varyings must exist in the VS (when a VS is present).
        if let Some(vs) = &vs {
            for v in &parsed.varyings {
                if !vs.varying_slots.contains_key(v) {
                    return Err(format!(
                        "link: fragment varying '{v}' not declared in the vertex shader"
                    ));
                }
            }
        }
        for name in &parsed.uniforms {
            register_uniform(&mut uniforms, &mut uniform_index, name.clone(), 0.0);
        }
        Some(LinkedFragmentShader {
            functions: parsed.functions.clone(),
            main: parsed.main,
            varying_slots,
            varying_comps: parsed.varying_comps.clone(),
        })
    } else {
        None
    };

    // The program must have at least one stage (a program with neither is
    // a link error).
    if vs.is_none() && fs.is_none() {
        return Err("link: program has no shaders attached".to_owned());
    }
    Ok((vs, fs, uniforms, uniform_index, attribute_count))
}

fn register_uniform(
    uniforms: &mut Vec<GlslVal>,
    uniform_index: &mut HashMap<String, usize>,
    name: String,
    default: f32,
) {
    if uniform_index.contains_key(&name) {
        return;
    }
    let location = uniforms.len();
    uniforms.push(GlslVal::F32(default));
    uniform_index.insert(name, location);
}

/// Scan the AST for `for` loops whose bound is not a compile-time constant —
/// the documented subset only supports constant-bounded loops.
fn validate_loop_bounds(parsed: &ParsedShader) -> Result<(), String> {
    for f in &parsed.functions {
        check_loop_bounds_stmt(&f.body)?;
    }
    Ok(())
}

fn check_loop_bounds_stmt(stmts: &[Stmt]) -> Result<(), String> {
    for s in stmts {
        match s {
            Stmt::For { cond, body, .. } => {
                if !is_constant_expr(cond) {
                    return Err("link: for-loop bound must be a compile-time constant (dynamic loops are not supported)".to_owned());
                }
                match &**body {
                    Stmt::Block(b) => check_loop_bounds_stmt(b)?,
                    other => check_loop_bounds_stmt(std::slice::from_ref(other))?,
                }
            }
            Stmt::If { then, else_, .. } => {
                match &**then {
                    Stmt::Block(b) => check_loop_bounds_stmt(b)?,
                    other => check_loop_bounds_stmt(std::slice::from_ref(other))?,
                }
                if let Some(e) = else_ {
                    match &**e {
                        Stmt::Block(b) => check_loop_bounds_stmt(b)?,
                        other => check_loop_bounds_stmt(std::slice::from_ref(other))?,
                    }
                }
            }
            Stmt::Block(body) => check_loop_bounds_stmt(body)?,
            _ => {}
        }
    }
    Ok(())
}

/// A compile-time-constant expression (literals, simple arithmetic).
fn is_constant_expr(e: &Expr) -> bool {
    match e {
        Expr::FloatLit(_) | Expr::IntLit(_) | Expr::BoolLit(_) => true,
        Expr::Un { e, .. } => is_constant_expr(e),
        Expr::Bin { left, right, .. } => is_constant_expr(left) && is_constant_expr(right),
        _ => false,
    }
}

/// `glUseProgram(program)` — 0 restores fixed function.
pub(crate) fn gl_use_program(ctx: &mut GlCtx, program: u32) {
    if program != 0 && !ctx.programs.iter().any(|p| p.id == program) {
        ctx.set_error(GL_INVALID_VALUE);
        return;
    }
    ctx.program_active = program;
    // Cache the index so the per-pixel fragment stage avoids a linear scan.
    ctx.active_program_idx = if program == 0 {
        None
    } else {
        ctx.programs.iter().position(|p| p.id == program)
    };
}

/// `glDeleteProgram(program)` — unbinds when active.
pub(crate) fn gl_delete_program(ctx: &mut GlCtx, program: u32) {
    if ctx.program_active == program {
        ctx.program_active = 0;
        ctx.active_program_idx = None;
    }
    ctx.programs.retain(|p| p.id != program);
}

/// `glIsProgram(id)`.
#[must_use]
pub(crate) fn gl_is_program(ctx: &GlCtx, id: u32) -> bool {
    ctx.programs.iter().any(|p| p.id == id)
}

/// `glValidateProgram` — re-run the link validation, set VALIDATE_STATUS.
pub(crate) fn gl_validate_program(ctx: &mut GlCtx, program: u32) {
    gl_link_program(ctx, program);
    // The link result IS the validate result (the link step performs the
    // same checks); a failed link leaves a log + linked=false.
}

/// `glGetUniformLocation(program, name)` — the location or -1.
pub(crate) fn gl_get_uniform_location(ctx: &GlCtx, program: u32, name: &str) -> i32 {
    let Some(prog) = ctx.programs.iter().find(|p| p.id == program) else {
        return -1;
    };
    i32::try_from(prog.uniform_index.get(name).copied().unwrap_or(usize::MAX)).unwrap_or(-1)
}

/// `glUniform*(location, value)` — set a uniform on the ACTIVE program.
pub(crate) fn gl_uniform_set(ctx: &mut GlCtx, location: i32, value: GlslVal) {
    let program = ctx.program_active;
    let Some(prog) = ctx.programs.iter_mut().find(|p| p.id == program) else {
        return;
    };
    if location < 0 {
        return;
    }
    let idx = usize::try_from(location).unwrap_or(usize::MAX);
    if idx < prog.uniforms.len() {
        prog.uniforms[idx] = value;
    }
}

/// Query helpers for the glGet* handlers.
#[must_use]
pub(crate) fn gl_shader_info_log(ctx: &GlCtx, id: u32) -> String {
    ctx.shaders
        .iter()
        .find(|s| s.id == id)
        .map_or_else(String::new, |s| s.info_log.clone())
}

#[must_use]
pub(crate) fn gl_shader_source_text(ctx: &GlCtx, id: u32) -> String {
    ctx.shaders
        .iter()
        .find(|s| s.id == id)
        .map_or_else(String::new, |s| s.source.clone())
}

#[must_use]
pub(crate) fn gl_shader_compile_status(ctx: &GlCtx, id: u32) -> i32 {
    i32::from(
        ctx.shaders
            .iter()
            .find(|s| s.id == id)
            .is_some_and(|s| s.compiled),
    )
}

#[must_use]
pub(crate) fn gl_program_info_log(ctx: &GlCtx, id: u32) -> String {
    ctx.programs
        .iter()
        .find(|p| p.id == id)
        .map_or_else(String::new, |p| p.info_log.clone())
}

#[must_use]
pub(crate) fn gl_program_link_status(ctx: &GlCtx, id: u32) -> i32 {
    i32::from(
        ctx.programs
            .iter()
            .find(|p| p.id == id)
            .is_some_and(|p| p.linked),
    )
}

#[must_use]
pub(crate) fn gl_program_attached_count(ctx: &GlCtx, id: u32) -> i32 {
    ctx.programs
        .iter()
        .find(|p| p.id == id)
        .map_or(0, |p| i32::try_from(p.attached.len()).unwrap_or(0))
}

#[must_use]
pub(crate) fn gl_program_active_uniforms(ctx: &GlCtx, id: u32) -> i32 {
    ctx.programs
        .iter()
        .find(|p| p.id == id)
        .map_or(0, |p| i32::try_from(p.uniforms.len()).unwrap_or(0))
}

#[must_use]
pub(crate) fn gl_program_active_attributes(ctx: &GlCtx, id: u32) -> i32 {
    ctx.programs
        .iter()
        .find(|p| p.id == id)
        .map_or(0, |p| i32::try_from(p.active_attributes).unwrap_or(0))
}

// ── Shader parse entry point ────────────────────────────────────────────

fn parse_shader(source: &str, kind: u32) -> Result<ParsedShader, String> {
    let toks = lex(source)?;
    let mut parser = Parser { toks, pos: 0 };
    let mut functions = Vec::new();
    let mut globals = Vec::new();
    while parser.peek() != &Tok::Eof {
        parser.parse_top(&mut functions, &mut globals)?;
    }
    let mut uniforms = Vec::new();
    let mut varyings = Vec::new();
    let mut varying_comps = HashMap::new();
    let mut attributes = Vec::new();
    for g in &globals {
        match g.qualifier {
            Qualifier::Uniform => uniforms.push(g.name.clone()),
            Qualifier::Varying => {
                varyings.push(g.name.clone());
                if let Some(t) = parse_type_name(&g.type_name) {
                    varying_comps
                        .insert(g.name.clone(), u32::try_from(t.components()).unwrap_or(0));
                }
            }
            Qualifier::Attribute => attributes.push(g.name.clone()),
            _ => {}
        }
    }
    let main = functions
        .iter()
        .position(|f| f.name == "main")
        .ok_or("shader has no main() function".to_owned())?;
    let parsed = ParsedShader {
        functions,
        main,
        uniforms,
        varyings,
        varying_comps,
        attributes,
    };
    // Typecheck with the global symbol table.
    let mut globals_table = HashMap::new();
    for g in &globals {
        let t = parse_type_name(&g.type_name)
            .ok_or_else(|| format!("unknown global type '{}'", g.type_name))?;
        globals_table.insert(g.name.clone(), t);
    }
    let mut checker = TypeChecker {
        parsed: &parsed,
        func_types: HashMap::new(),
        globals: globals_table,
        kind,
    };
    checker.typecheck()?;
    Ok(parsed)
}
