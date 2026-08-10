// OpenGL GL 1.1 software-renderer micro-test for WIE.
//
// Exercises opengl32.dll end-to-end: register + create a window, GetDC,
// wglChoosePixelFormat + wglSetPixelFormat, wglCreateContext +
// wglMakeCurrent, then a REAL fixed-function draw sequence on WM_PAINT:
// glClearColor + glClear, a red immediate-mode quad, a textured checkerboard
// quad (glGenTextures/glBindTexture/glTexImage2D + GL_MODULATE), a client
// vertex-array triangle (glVertexPointer + glDrawArrays), the same triangle
// through a GL_ARRAY_BUFFER VBO (with the pointer obtained via
// wglGetProcAddress — proving the soft-VA dispatch), a lit quad
// (GL_LIGHTING + GL_LIGHT0 with a positional light and per-vertex normals,
// GL_SMOOTH) whose brightness must vary across the face, and a display list
// (glNewList/glEndList + glCallList) drawn twice. Each frame self-verifies
// with glReadPixels; after a few rendered frames (self-test mode) the window
// quits with 0.
//
// Exit code 0 only if every WGL call succeeded AND every pixel assertion
// held; distinct non-zero codes name the first failed stage (100-108 WGL,
// 110-115 stage-1 pixels, 120-125 stage-2 pixels). Interactive runs (no
// WIE_SELFTEST) keep the window open.
//
// wglChoosePixelFormat/wglSetPixelFormat/wglDescribePixelFormat/
// wglGetPixelFormat/wglSwapBuffers are exported by opengl32.dll (mingw's
// libopengl32.a provides the thunks) but absent from wingdi.h, which declares
// only the GDI names (ChoosePixelFormat/SetPixelFormat/...). The prototypes
// are declared here so the PE imports the opengl32.dll names WIE dispatches.
// GL_GLEXT_PROTOTYPES exposes the GL 1.5 buffer-object entry points.

#include <windows.h>
#include <GL/gl.h>

#define WINDOW_W    640
#define WINDOW_H    480
#define TIMER_TICKS_MIN 3
#define TIMER_ID        1

// wgl* prototypes not declared by wingdi.h (see the header comment).
WINGDIAPI int WINAPI wglChoosePixelFormat(HDC hdc, const PIXELFORMATDESCRIPTOR *ppfd);
WINGDIAPI WINBOOL WINAPI wglSetPixelFormat(HDC hdc, int format, const PIXELFORMATDESCRIPTOR *ppfd);
WINGDIAPI int WINAPI wglDescribePixelFormat(HDC hdc, int iPixelFormat, UINT nBytes, const PIXELFORMATDESCRIPTOR *ppfd);
WINGDIAPI UINT WINAPI wglGetPixelFormat(HDC hdc);
WINGDIAPI WINBOOL WINAPI wglSwapBuffers(HDC hdc);

// GL 1.5 buffer-object constants are not in mingw's GL 1.1 gl.h; the entry
// points themselves are resolved through wglGetProcAddress (see setup_vbo).
#ifndef GL_ARRAY_BUFFER
#define GL_ARRAY_BUFFER 0x8892
#define GL_ELEMENT_ARRAY_BUFFER 0x8893
#define GL_STATIC_DRAW 0x88E4

// GLSL shader-object/program constants + entry points (GL 2.0 — declared
// here since mingw's gl.h stops at GL 1.1; WIE dispatches the opengl32.dll
// exports by name). GLenum/GLuint/GLint/GLsizei come from gl.h; only
// GLchar is a GL 2.0 addition.
#define GL_VERTEX_SHADER 0x8B31
#define GL_FRAGMENT_SHADER 0x8B30
#define GL_COMPILE_STATUS 0x8B81
#define GL_LINK_STATUS 0x8B82
#define GL_INFO_LOG_LENGTH 0x8B84
#ifndef GLchar
typedef char GLchar;
#endif
typedef GLuint (WINAPI *glCreateShader_t)(GLenum type);
typedef void (WINAPI *glShaderSource_t)(GLuint shader, GLsizei count, const GLchar **string, const GLint *length);
typedef void (WINAPI *glCompileShader_t)(GLuint shader);
typedef GLuint (WINAPI *glCreateProgram_t)(void);
typedef void (WINAPI *glAttachShader_t)(GLuint program, GLuint shader);
typedef void (WINAPI *glLinkProgram_t)(GLuint program);
typedef void (WINAPI *glUseProgram_t)(GLuint program);
typedef void (WINAPI *glGetShaderiv_t)(GLuint shader, GLenum pname, GLint *params);
typedef void (WINAPI *glGetProgramiv_t)(GLuint program, GLenum pname, GLint *params);
typedef void (WINAPI *glGetShaderInfoLog_t)(GLuint shader, GLsizei bufSize, GLsizei *length, GLchar *infoLog);
typedef GLint (WINAPI *glGetUniformLocation_t)(GLuint program, const GLchar *name);
typedef void (WINAPI *glUniform1i_t)(GLint location, GLint v0);
typedef void (WINAPI *glUniformMatrix4fv_t)(GLint location, GLsizei count, GLboolean transpose, const GLfloat *value);
typedef void (WINAPI *glGetUniformfv_t)(GLuint program, GLint location, GLfloat *params);
#endif

static HINSTANCE g_inst;
static HWND      g_hwnd;
static HDC       g_hdc;
static HGLRC     g_glrc;
static int       g_selftest;
static int       g_timer_count;
static int       g_render_count;

static int selftest_enabled(void) {
    char buf[16];
    DWORD n = GetEnvironmentVariableA("WIE_SELFTEST", buf, sizeof(buf));
    return n == 1 && buf[0] == '1';
}

// The 24-bit double-buffered RGBA descriptor WIE describes.
static void fill_pixel_format(PIXELFORMATDESCRIPTOR *pfd) {
    pfd->nSize        = (WORD)sizeof(*pfd);
    pfd->nVersion     = 1;
    pfd->dwFlags      = PFD_DRAW_TO_WINDOW | PFD_SUPPORT_OPENGL | PFD_DOUBLEBUFFER;
    pfd->iPixelType   = PFD_TYPE_RGBA;
    pfd->cColorBits   = 32;
    pfd->cRedBits     = 0;
    pfd->cRedShift    = 0;
    pfd->cGreenBits   = 0;
    pfd->cGreenShift  = 0;
    pfd->cBlueBits    = 0;
    pfd->cBlueShift   = 0;
    pfd->cAlphaBits   = 0;
    pfd->cAlphaShift  = 0;
    pfd->cAccumBits   = 0;
    pfd->cAccumRedBits   = 0;
    pfd->cAccumGreenBits = 0;
    pfd->cAccumBlueBits  = 0;
    pfd->cAccumAlphaBits = 0;
    pfd->cDepthBits   = 24;
    pfd->cStencilBits = 0;
    pfd->cAuxBuffers  = 0;
    pfd->iLayerType   = 0;
    pfd->bReserved    = 0;
    pfd->dwLayerMask  = 0;
    pfd->dwVisibleMask = 0;
    pfd->dwDamageMask  = 0;
}

// Append the decimal digits of v (0..255) to buf at *i.
static void append_dec(char *buf, int *i, int v) {
    if (v >= 100) {
        buf[(*i)++] = (char)('0' + v / 100);
    }
    if (v >= 10) {
        buf[(*i)++] = (char)('0' + (v / 10) % 10);
    }
    buf[(*i)++] = (char)('0' + v % 10);
}

// Log a failed exact-RGB pixel check through WIE's host trace
// (OutputDebugStringA), then return its exit code — a bare status code
// alone cannot say which check failed or what the readback returned.
static int fail_rgb(int code, const char *label, const unsigned char *px,
                    int er, int eg, int eb) {
    char buf[96];
    int i = 0;
    static const char fail[] = "FAIL ";
    static const char want[] = ": expected RGB(";
    static const char comma[] = ",";
    static const char close[] = ") got RGB(";
    static const char done[] = ")";
    int k;
    for (k = 0; fail[k] != 0 && i < (int)sizeof(buf) - 1; k++) {
        buf[i++] = fail[k];
    }
    for (k = 0; label[k] != 0 && i < (int)sizeof(buf) - 1; k++) {
        buf[i++] = label[k];
    }
    for (k = 0; want[k] != 0 && i < (int)sizeof(buf) - 1; k++) {
        buf[i++] = want[k];
    }
    append_dec(buf, &i, er);
    for (k = 0; comma[k] != 0 && i < (int)sizeof(buf) - 1; k++) {
        buf[i++] = comma[k];
    }
    append_dec(buf, &i, eg);
    for (k = 0; comma[k] != 0 && i < (int)sizeof(buf) - 1; k++) {
        buf[i++] = comma[k];
    }
    append_dec(buf, &i, eb);
    for (k = 0; close[k] != 0 && i < (int)sizeof(buf) - 1; k++) {
        buf[i++] = close[k];
    }
    append_dec(buf, &i, px[0]);
    for (k = 0; comma[k] != 0 && i < (int)sizeof(buf) - 1; k++) {
        buf[i++] = comma[k];
    }
    append_dec(buf, &i, px[1]);
    for (k = 0; comma[k] != 0 && i < (int)sizeof(buf) - 1; k++) {
        buf[i++] = comma[k];
    }
    append_dec(buf, &i, px[2]);
    for (k = 0; done[k] != 0 && i < (int)sizeof(buf) - 1; k++) {
        buf[i++] = done[k];
    }
    buf[i] = 0;
    OutputDebugStringA(buf);
    return code;
}

// Log a named failure (setup stage, relative check) with no pixel payload.
static int fail_msg(int code, const char *label) {
    char buf[80];
    int i = 0;
    static const char fail[] = "FAIL ";
    int k;
    for (k = 0; fail[k] != 0 && i < (int)sizeof(buf) - 1; k++) {
        buf[i++] = fail[k];
    }
    for (k = 0; label[k] != 0 && i < (int)sizeof(buf) - 1; k++) {
        buf[i++] = label[k];
    }
    buf[i] = 0;
    OutputDebugStringA(buf);
    return code;
}

// GetDC → pixel format → context → make current. Returns 0 on success, else
// the exit code naming the failed stage.
static int setup_gl(void) {
    g_hdc = GetDC(g_hwnd);
    if (g_hdc == NULL) {
        return fail_msg(101, "GetDC returned NULL");
    }

    PIXELFORMATDESCRIPTOR pfd;
    fill_pixel_format(&pfd);
    int fmt = wglChoosePixelFormat(g_hdc, &pfd);
    if (fmt == 0) {
        return fail_msg(102, "wglChoosePixelFormat failed");
    }
    if (!wglSetPixelFormat(g_hdc, fmt, &pfd)) {
        return fail_msg(103, "wglSetPixelFormat failed");
    }
    if (wglGetPixelFormat(g_hdc) != (UINT)fmt) {
        return fail_msg(108, "wglGetPixelFormat does not match the chosen format");
    }

    g_glrc = wglCreateContext(g_hdc);
    if (g_glrc == NULL) {
        return fail_msg(104, "wglCreateContext returned NULL");
    }
    if (!wglMakeCurrent(g_hdc, g_glrc)) {
        return fail_msg(105, "wglMakeCurrent failed");
    }
    if (wglGetCurrentContext() != g_glrc || wglGetCurrentDC() != g_hdc) {
        return fail_msg(108, "current context/DC mismatch after wglMakeCurrent");
    }
    return 0;
}

// A 4x4 RGBA checkerboard whose rows (from the GL bottom up) are
// [R G R G], [G B G B], [B W B W], [W R W R] — the vertical asymmetry lets
// glReadPixels prove the GL bottom-up upload flip. W = white.
static void build_checkerboard(unsigned char *pixels) {
    static const unsigned char rows[4][4][3] = {
        {{255, 0, 0}, {0, 255, 0}, {255, 0, 0}, {0, 255, 0}},   // row 0 (GL bottom)
        {{0, 255, 0}, {0, 0, 255}, {0, 255, 0}, {0, 0, 255}},   // row 1
        {{0, 0, 255}, {255, 255, 255}, {0, 0, 255}, {255, 255, 255}}, // row 2
        {{255, 255, 255}, {255, 0, 0}, {255, 255, 255}, {255, 0, 0}}, // row 3 (GL top)
    };
    int y, x;
    for (y = 0; y < 4; y++) {
        for (x = 0; x < 4; x++) {
            int i = (y * 4 + x) * 4;
            pixels[i + 0] = rows[y][x][0];
            pixels[i + 1] = rows[y][x][1];
            pixels[i + 2] = rows[y][x][2];
            pixels[i + 3] = 255;
        }
    }
}

// Upload the checkerboard as texture 1 with NEAREST sampling. Returns 0 on
// success, else the exit code (115 = a GL error flag was raised).
static int setup_texture(void) {
    GLuint tex = 0;
    unsigned char checker[4 * 4 * 4];
    build_checkerboard(checker);
    glGenTextures(1, &tex);
    glBindTexture(GL_TEXTURE_2D, tex);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_REPEAT);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_REPEAT);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA, 4, 4, 0, GL_RGBA, GL_UNSIGNED_BYTE, checker);
    if (glGetError() != GL_NO_ERROR) {
        return fail_msg(115, "GL error after glTexImage2D upload");
    }
    return 0;
}

// Log a readback pixel through WIE's host trace (OutputDebugStringA).
static void debug_pixel(const char *label, const unsigned char *px) {
    static const char hex[] = "0123456789ABCDEF";
    char buf[80];
    int i = 0;
    while (label[i] != 0 && i < 40) {
        buf[i] = label[i];
        i++;
    }
    buf[i++] = '=';
    buf[i++] = '0';
    buf[i++] = 'x';
    buf[i++] = hex[px[0] >> 4];
    buf[i++] = hex[px[0] & 0x0F];
    buf[i++] = hex[px[1] >> 4];
    buf[i++] = hex[px[1] & 0x0F];
    buf[i++] = hex[px[2] >> 4];
    buf[i++] = hex[px[2] & 0x0F];
    buf[i++] = hex[px[3] >> 4];
    buf[i++] = hex[px[3] & 0x0F];
    buf[i] = 0;
    OutputDebugStringA(buf);
}

// Read the pixel at a WORLD-space point, mapped through the current viewport
// (glOrtho(0,640,0,480) → viewport (0,0,cx,cy)). Every self-check reads at a
// world position, so the checks hold at any client size — a resize must not
// change which texel/quad a readback targets (the 640×480-anchored pixel
// coordinates would drift as the viewport scales).
static void read_world_px(int wx, int wy, int cx, int cy, unsigned char *px) {
    int x = (int)(wx * cx / 640.0f);
    int y = (int)(wy * cy / 480.0f);
    glReadPixels(x, y, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, px);
}

// ── Stage-2 fixtures: VBO, display list, function pointers ──────────────

// GL 1.5 buffer functions are NOT in opengl32.dll's export table (real
// Windows ships only GL 1.1 + WGL there) — apps resolve them through
// wglGetProcAddress. Doing the same here proves the soft-VA dispatch
// end-to-end: the returned fake VAs must stop and dispatch to the renderer.
typedef void (WINAPI *glBindBuffer_t)(GLenum target, GLuint buffer);
typedef void (WINAPI *glGenBuffers_t)(GLsizei n, GLuint *buffers);
typedef ptrdiff_t GLsizeiptr;
typedef void (WINAPI *glBufferData_t)(GLenum target, GLsizeiptr size, const void *data, GLenum usage);
static glBindBuffer_t pfn_bind_buffer;
static glGenBuffers_t pfn_gen_buffers;
static glBufferData_t pfn_buffer_data;

// GL 2.0 shader-object/program entry points — also resolved through
// wglGetProcAddress (mingw's gl.h stops at GL 1.1).
static glCreateShader_t pfn_create_shader;
static glShaderSource_t pfn_shader_source;
static glCompileShader_t pfn_compile_shader;
static glCreateProgram_t pfn_create_program;
static glAttachShader_t pfn_attach_shader;
static glLinkProgram_t pfn_link_program;
static glUseProgram_t pfn_use_program;
static glGetShaderiv_t pfn_get_shader_iv;
static glGetProgramiv_t pfn_get_program_iv;
static glGetShaderInfoLog_t pfn_get_shader_info_log;
static glGetUniformLocation_t pfn_get_uniform_location;
static glUniform1i_t pfn_uniform_1i;

// The VBO triangle's vertices (GL float pairs), uploaded once.
static const float g_vbo_vertices[6] = {30.0f, 170.0f, 150.0f, 170.0f, 30.0f, 290.0f};
static GLuint g_vbo = 0;
static GLuint g_list = 0;
static GLuint g_prog_gradient = 0;
static GLuint g_prog_tex = 0;

// Create the array-buffer VBO, resolving every entry point through
// wglGetProcAddress. Returns 0 on success, else the exit code (116 = a
// pointer was NULL).
static int setup_vbo(void) {
    pfn_bind_buffer = (glBindBuffer_t)wglGetProcAddress("glBindBuffer");
    pfn_gen_buffers = (glGenBuffers_t)wglGetProcAddress("glGenBuffers");
    pfn_buffer_data = (glBufferData_t)wglGetProcAddress("glBufferData");
    if (pfn_bind_buffer == NULL || pfn_gen_buffers == NULL || pfn_buffer_data == NULL) {
        return fail_msg(116, "wglGetProcAddress returned NULL for a VBO entry point");
    }
    pfn_gen_buffers(1, &g_vbo);
    pfn_bind_buffer(GL_ARRAY_BUFFER, g_vbo);
    pfn_buffer_data(GL_ARRAY_BUFFER, sizeof(g_vbo_vertices), g_vbo_vertices, GL_STATIC_DRAW);
    pfn_bind_buffer(GL_ARRAY_BUFFER, 0);
    if (glGetError() != GL_NO_ERROR) {
        return fail_msg(117, "GL error after VBO setup");
    }
    return 0;
}

// Compile a cyan quad at x∈[320,400], y∈[330,410] (GL coords) into a display
// list; the modelview at call time positions each instance.
static int setup_display_list(void) {
    g_list = glGenLists(1);
    if (g_list == 0) {
        return fail_msg(118, "glGenLists returned 0");
    }
    glNewList(g_list, GL_COMPILE);
    glColor3f(0.0f, 1.0f, 1.0f);
    glBegin(GL_QUADS);
    glVertex2f(320.0f, 330.0f);
    glVertex2f(400.0f, 330.0f);
    glVertex2f(400.0f, 410.0f);
    glVertex2f(320.0f, 410.0f);
    glEnd();
    glEndList();
    if (!glIsList(g_list)) {
        return fail_msg(118, "glIsList false after glEndList");
    }
    if (glGetError() != GL_NO_ERROR) {
        return fail_msg(119, "GL error after display-list compile");
    }
    return 0;
}

// Compile a GLSL shader; 0 on success, else the exit code naming the stage.
static int compile_one(GLenum kind, const GLchar *src, GLuint *out) {
    GLuint sh = pfn_create_shader(kind);
    if (sh == 0) {
        return fail_msg(130, "glCreateShader returned 0");
    }
    pfn_shader_source(sh, 1, &src, NULL);
    pfn_compile_shader(sh);
    GLint ok = 0;
    pfn_get_shader_iv(sh, GL_COMPILE_STATUS, &ok);
    if (!ok) {
        GLchar log[256];
        pfn_get_shader_info_log(sh, sizeof(log), NULL, log);
        debug_pixel("shader_log", (const unsigned char *)log);
        return fail_msg(131, "shader compile failed (see shader_log trace)");
    }
    *out = sh;
    return 0;
}

// Link a VS+FS pair into a program; 0 on success, else the exit code.
static int link_program(GLuint vs, GLuint fs, GLuint *prog) {
    GLuint p = pfn_create_program();
    if (p == 0) {
        return fail_msg(132, "glCreateProgram returned 0");
    }
    pfn_attach_shader(p, vs);
    pfn_attach_shader(p, fs);
    pfn_link_program(p);
    GLint ok = 0;
    pfn_get_program_iv(p, GL_LINK_STATUS, &ok);
    if (!ok) {
        return fail_msg(133, "glLinkProgram failed (link status false)");
    }
    *prog = p;
    return 0;
}

// Compile the two GLSL programs: a v_uv gradient (VS maps the quad's window
// coords to uv, FS colors by it) and a texture2D sampler. 0 on success, else
// the exit code (130-134).
static int setup_shaders(void) {
    pfn_create_shader = (glCreateShader_t)wglGetProcAddress("glCreateShader");
    pfn_shader_source = (glShaderSource_t)wglGetProcAddress("glShaderSource");
    pfn_compile_shader = (glCompileShader_t)wglGetProcAddress("glCompileShader");
    pfn_create_program = (glCreateProgram_t)wglGetProcAddress("glCreateProgram");
    pfn_attach_shader = (glAttachShader_t)wglGetProcAddress("glAttachShader");
    pfn_link_program = (glLinkProgram_t)wglGetProcAddress("glLinkProgram");
    pfn_use_program = (glUseProgram_t)wglGetProcAddress("glUseProgram");
    pfn_get_shader_iv = (glGetShaderiv_t)wglGetProcAddress("glGetShaderiv");
    pfn_get_program_iv = (glGetProgramiv_t)wglGetProcAddress("glGetProgramiv");
    pfn_get_shader_info_log = (glGetShaderInfoLog_t)wglGetProcAddress("glGetShaderInfoLog");
    pfn_get_uniform_location = (glGetUniformLocation_t)wglGetProcAddress("glGetUniformLocation");
    pfn_uniform_1i = (glUniform1i_t)wglGetProcAddress("glUniform1i");
    if (pfn_create_shader == NULL || pfn_shader_source == NULL || pfn_compile_shader == NULL
        || pfn_create_program == NULL || pfn_attach_shader == NULL || pfn_link_program == NULL
        || pfn_use_program == NULL || pfn_get_shader_iv == NULL || pfn_get_program_iv == NULL
        || pfn_get_shader_info_log == NULL || pfn_get_uniform_location == NULL
        || pfn_uniform_1i == NULL) {
        return fail_msg(134, "wglGetProcAddress returned NULL for a GLSL entry point");
    }

    // Gradient program: uv = (window xy - quad origin) / quad size.
    static const GLchar *grad_vs =
        "attribute vec4 gl_Vertex;"
        "varying vec2 v_uv;"
        "void main() {"
        "  v_uv = vec2((gl_Vertex.x - 220.0) * 0.005, (gl_Vertex.y - 20.0) * 0.005);"
        "  gl_Position = gl_ModelViewProjectionMatrix * gl_Vertex;"
        "}";
    static const GLchar *grad_fs =
        "varying vec2 v_uv;"
        "void main() {"
        "  gl_FragColor = vec4(v_uv, 0.0, 1.0);"
        "}";
    GLuint gvs = 0, gfs = 0;
    int rc = compile_one(GL_VERTEX_SHADER, grad_vs, &gvs);
    if (rc != 0) {
        return rc;
    }
    rc = compile_one(GL_FRAGMENT_SHADER, grad_fs, &gfs);
    if (rc != 0) {
        return rc;
    }
    rc = link_program(gvs, gfs, &g_prog_gradient);
    if (rc != 0) {
        return rc;
    }

    // Texture program: sample the checkerboard (unit 0) at a fixed uv.
    static const GLchar *tex_vs =
        "attribute vec4 gl_Vertex;"
        "void main() {"
        "  gl_Position = gl_ModelViewProjectionMatrix * gl_Vertex;"
        "}";
    static const GLchar *tex_fs =
        "uniform sampler2D u_tex;"
        "void main() {"
        "  gl_FragColor = texture2D(u_tex, vec2(0.125, 0.125));"
        "}";
    GLuint tvs = 0, tfs = 0;
    rc = compile_one(GL_VERTEX_SHADER, tex_vs, &tvs);
    if (rc != 0) {
        return rc;
    }
    rc = compile_one(GL_FRAGMENT_SHADER, tex_fs, &tfs);
    if (rc != 0) {
        return rc;
    }
    rc = link_program(tvs, tfs, &g_prog_tex);
    if (rc != 0) {
        return rc;
    }
    // Bind the sampler to texture unit 0 (the checkerboard).
    GLint loc = pfn_get_uniform_location(g_prog_tex, "u_tex");
    if (loc < 0) {
        return fail_msg(134, "glGetUniformLocation(u_tex) not found");
    }
    pfn_use_program(g_prog_tex);
    pfn_uniform_1i(loc, 0);
    pfn_use_program(0);
    return 0;
}

// Draw a real fixed-function frame and self-verify it via glReadPixels, then
// swap. Returns 0 on success, else the exit code naming the failed check.
static int render_frame(void) {
    RECT client;
    GetClientRect(g_hwnd, &client);
    int cx = client.right - client.left;
    int cy = client.bottom - client.top;

    glViewport(0, 0, cx, cy);
    glMatrixMode(GL_PROJECTION);
    glLoadIdentity();
    glOrtho(0.0f, 640.0f, 0.0f, 480.0f, -1.0f, 1.0f);
    glMatrixMode(GL_MODELVIEW);
    glLoadIdentity();

    glClearColor(0.1f, 0.1f, 0.4f, 1.0f);
    glClear(GL_COLOR_BUFFER_BIT | GL_DEPTH_BUFFER_BIT);

    // Red quad centered at (320, 240) — the viewport center on any client
    // size, so the readback always lands on it.
    glColor3f(1.0f, 0.0f, 0.0f);
    glBegin(GL_QUADS);
    glVertex2f(240.0f, 180.0f);
    glVertex2f(400.0f, 180.0f);
    glVertex2f(400.0f, 300.0f);
    glVertex2f(240.0f, 300.0f);
    glEnd();

    // Textured checkerboard quad at x∈[430,600], y∈[60,220] (GL coords).
    glEnable(GL_TEXTURE_2D);
    glBindTexture(GL_TEXTURE_2D, 1);
    glColor3f(1.0f, 1.0f, 1.0f);   // GL_MODULATE passes the texel color through
    glBegin(GL_QUADS);
    glTexCoord2f(0.0f, 0.0f);
    glVertex2f(430.0f, 60.0f);
    glTexCoord2f(1.0f, 0.0f);
    glVertex2f(600.0f, 60.0f);
    glTexCoord2f(1.0f, 1.0f);
    glVertex2f(600.0f, 220.0f);
    glTexCoord2f(0.0f, 1.0f);
    glVertex2f(430.0f, 220.0f);
    glEnd();
    glDisable(GL_TEXTURE_2D);

    // ── Stage 2: client arrays, VBO, lighting, display lists ────────────

    // (a) Client vertex-array triangle (bottom-left), current color yellow.
    {
        static const float verts[6] = {30.0f, 30.0f, 150.0f, 30.0f, 30.0f, 150.0f};
        glEnableClientState(GL_VERTEX_ARRAY);
        glVertexPointer(2, GL_FLOAT, 0, verts);
        glColor3f(1.0f, 1.0f, 0.0f);
        glDrawArrays(GL_TRIANGLES, 0, 3);
        glDisableClientState(GL_VERTEX_ARRAY);
    }

    // (b) The same shape through a GL_ARRAY_BUFFER VBO (magenta), bound via
    // the wglGetProcAddress-resolved function pointer.
    {
        pfn_bind_buffer(GL_ARRAY_BUFFER, g_vbo);
        glEnableClientState(GL_VERTEX_ARRAY);
        glVertexPointer(2, GL_FLOAT, 0, NULL);   // offset 0 into the VBO
        glColor3f(1.0f, 0.0f, 1.0f);
        glDrawArrays(GL_TRIANGLES, 0, 3);
        glDisableClientState(GL_VERTEX_ARRAY);
        pfn_bind_buffer(GL_ARRAY_BUFFER, 0);
    }

    // (c) Lit quad at x∈[30,200], y∈[310,450] with a positional light above
    // its top-right corner — the face must brighten toward the light.
    {
        static const GLfloat light_pos[4] = {210.0f, 460.0f, 80.0f, 1.0f};
        static const GLfloat white[4] = {1.0f, 1.0f, 1.0f, 1.0f};
        glEnable(GL_LIGHTING);
        glEnable(GL_LIGHT0);
        glLightfv(GL_LIGHT0, GL_POSITION, light_pos);
        glMaterialfv(GL_FRONT_AND_BACK, GL_DIFFUSE, white);
        glNormal3f(0.0f, 0.0f, 1.0f);
        glBegin(GL_QUADS);
        glVertex2f(30.0f, 310.0f);
        glVertex2f(200.0f, 310.0f);
        glVertex2f(200.0f, 450.0f);
        glVertex2f(30.0f, 450.0f);
        glEnd();
        glDisable(GL_LIGHTING);
        glDisable(GL_LIGHT0);
    }

    // (d) Display list: a cyan quad drawn twice at different modelview
    // offsets (the list is compiled once, positioned by the caller).
    glLoadIdentity();
    glCallList(g_list);                  // x∈[320,400], y∈[330,410]
    glTranslatef(-90.0f, 0.0f, 0.0f);
    glCallList(g_list);                  // x∈[230,310], y∈[330,410]
    glLoadIdentity();

    // ── Stage 3: GLSL shaders ───────────────────────────────────────────

    // (a) A v_uv-gradient quad at x∈[220,420], y∈[20,220] (GL coords): the VS
    // maps the quad to uv (0..1), the FS colors by the interpolated varying.
    // Self-verified at two points: the bottom-left corner is dark (uv≈(0,0)),
    // the top-right bright.
    if (g_prog_gradient) {
        pfn_use_program(g_prog_gradient);
        glBegin(GL_QUADS);
        glVertex2f(220.0f, 20.0f);
        glVertex2f(420.0f, 20.0f);
        glVertex2f(420.0f, 220.0f);
        glVertex2f(220.0f, 220.0f);
        glEnd();
        pfn_use_program(0);
    }

    // (b) A texture2D quad at x∈[430,600], y∈[300,460]: the FS samples the
    // checkerboard at a fixed uv — (0.125,0.125) is the bottom-left texel
    // (red). The checkerboard texture (id 1) is still bound to unit 0.
    if (g_prog_tex) {
        pfn_use_program(g_prog_tex);
        glBegin(GL_QUADS);
        glVertex2f(430.0f, 300.0f);
        glVertex2f(600.0f, 300.0f);
        glVertex2f(600.0f, 460.0f);
        glVertex2f(430.0f, 460.0f);
        glEnd();
        pfn_use_program(0);
    }

    glFlush();

    // ── Self-verify the rendered backbuffer (WIE implements glReadPixels) ──
    unsigned char px[4];

    glReadPixels(cx / 2, cy / 2, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, px); // quad center
    debug_pixel("quad_center", px);
    if (px[0] != 255 || px[1] != 0 || px[2] != 0) {
        return fail_rgb(110, "quad_center", px, 255, 0, 0);
    }

    glReadPixels(2, 2, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, px); // corner clear
    debug_pixel("corner", px);
    if (px[0] != 26 || px[1] != 26 || px[2] != 102) {
        return fail_rgb(111, "corner", px, 26, 26, 102);
    }

    read_world_px(451, 80, cx, cy, px); // texel (0,0) red
    debug_pixel("texel00", px);
    if (px[0] != 255 || px[1] != 0 || px[2] != 0) {
        return fail_rgb(112, "texel00", px, 255, 0, 0);
    }

    read_world_px(493, 80, cx, cy, px); // texel (1,0) green
    debug_pixel("texel10", px);
    if (px[0] != 0 || px[1] != 255 || px[2] != 0) {
        return fail_rgb(113, "texel10", px, 0, 255, 0);
    }

    read_world_px(451, 200, cx, cy, px); // texel (0,3) white
    debug_pixel("texel03", px);
    if (px[0] != 255 || px[1] != 255 || px[2] != 255) {
        return fail_rgb(114, "texel03", px, 255, 255, 255);
    }

    // ── Stage-2 readbacks ────────────────────────────────────────────────

    read_world_px(60, 60, cx, cy, px); // array triangle
    debug_pixel("array_tri", px);
    if (px[0] != 255 || px[1] != 255 || px[2] != 0) {
        return fail_rgb(120, "array_tri", px, 255, 255, 0);
    }

    read_world_px(60, 200, cx, cy, px); // VBO triangle
    debug_pixel("vbo_tri", px);
    if (px[0] != 255 || px[1] != 0 || px[2] != 255) {
        return fail_rgb(121, "vbo_tri", px, 255, 0, 255);
    }

    read_world_px(180, 430, cx, cy, px); // lit quad near light
    debug_pixel("lit_near", px);
    int bright = px[0];
    read_world_px(50, 330, cx, cy, px);  // lit quad far from light
    debug_pixel("lit_far", px);
    int dim = px[0];
    if (bright < dim + 40) {
        return fail_msg(122, "lit_quad: brightness gradient too flat");
    }

    read_world_px(380, 370, cx, cy, px); // list instance 1
    debug_pixel("list1", px);
    if (px[0] != 0 || px[1] != 255 || px[2] != 255) {
        return fail_rgb(123, "list1", px, 0, 255, 255);
    }

    read_world_px(270, 370, cx, cy, px); // list instance 2
    debug_pixel("list2", px);
    if (px[0] != 0 || px[1] != 255 || px[2] != 255) {
        return fail_rgb(124, "list2", px, 0, 255, 255);
    }

    read_world_px(210, 370, cx, cy, px); // gap between instances
    debug_pixel("list_gap", px);
    if (px[0] != 26 || px[1] != 26 || px[2] != 102) {
        return fail_rgb(125, "list_gap", px, 26, 26, 102);
    }

    // ── Stage-3 readbacks (GLSL shaders) ────────────────────────────────

    // (a) Gradient quad at x∈[220,420], y∈[20,220]: bottom-left is dark
    // (uv≈(0,0)), top-right is bright yellow (uv≈(1,1)).
    read_world_px(230, 30, cx, cy, px); // grad dark corner
    debug_pixel("grad_dark", px);
    int grad_dim = px[0];
    read_world_px(410, 210, cx, cy, px); // grad bright corner
    debug_pixel("grad_bright", px);
    int grad_bright = px[0];
    if (grad_bright < grad_dim + 100 || px[1] < 200) {
        return fail_msg(135, "gradient: uv slope too flat");
    }

    // (b) Texture2D quad at x∈[430,600], y∈[300,460]: the FS samples the
    // checkerboard at uv (0.125,0.125) → the bottom-left texel (red).
    read_world_px(515, 380, cx, cy, px);
    debug_pixel("shader_tex", px);
    if (px[0] != 255 || px[1] != 0 || px[2] != 0) {
        return fail_rgb(136, "shader_tex", px, 255, 0, 0);
    }

    if (!wglSwapBuffers(g_hdc)) {
        return 106;
    }
    // Self-test quit: leave after a few successful rendered frames. The CLI
    // GUI driver does not always deliver the third WM_TIMER wake, so the
    // paint-count is the authoritative quit condition; the WM_TIMER arm stays
    // for interactive pacing.
    if (g_selftest) {
        g_render_count++;
        if (g_render_count >= TIMER_TICKS_MIN) {
            ExitProcess(0);
        }
    }
    return 0;
}

static LRESULT CALLBACK WndProc(HWND hwnd, UINT msg, WPARAM wParam, LPARAM lParam) {
    switch (msg) {
    case WM_PAINT: {
        PAINTSTRUCT ps;
        BeginPaint(hwnd, &ps);
        {
            int rc = render_frame();
            if (rc != 0) {
                ExitProcess(rc);
            }
        }
        EndPaint(hwnd, &ps);
        return 0;
    }

    case WM_TIMER:
        if (!g_selftest) {
            return 0;   // interactive: the timer drives nothing
        }
        g_timer_count++;
        InvalidateRect(hwnd, NULL, FALSE);
        if (g_timer_count >= TIMER_TICKS_MIN) {
            PostQuitMessage(0);
        }
        return 0;

    case WM_CHAR:
        if (wParam == 'q' || wParam == 'Q') {
            DestroyWindow(hwnd);
        }
        return 0;

    case WM_DESTROY:
        if (g_glrc) {
            wglMakeCurrent(NULL, NULL);
            if (!wglDeleteContext(g_glrc)) {
                OutputDebugStringA("FAIL wglDeleteContext failed");
                PostQuitMessage(107);
                return 0;
            }
            g_glrc = NULL;
        }
        if (g_hdc) {
            ReleaseDC(hwnd, g_hdc);
            g_hdc = NULL;
        }
        if (g_timer_count < TIMER_TICKS_MIN) {
            OutputDebugStringA("FAIL window destroyed before enough frames rendered");
            PostQuitMessage(120);
            return 0;
        }
        PostQuitMessage(0);
        return 0;
    }

    return DefWindowProcA(hwnd, msg, wParam, lParam);
}

void entry(void) {
    g_inst = GetModuleHandleA(NULL);
    g_selftest = selftest_enabled();

    WNDCLASSEXA wc;
    wc.cbSize        = sizeof(WNDCLASSEXA);
    wc.style         = CS_HREDRAW | CS_VREDRAW;
    wc.lpfnWndProc   = WndProc;
    wc.cbClsExtra    = 0;
    wc.cbWndExtra    = 0;
    wc.hInstance     = g_inst;
    wc.hIcon         = NULL;
    wc.hCursor       = NULL;
    wc.hbrBackground = (HBRUSH)(COLOR_WINDOW + 1);
    wc.lpszMenuName  = NULL;
    wc.lpszClassName = "GlQuadClass";
    wc.hIconSm       = NULL;

    if (RegisterClassExA(&wc) == 0) {
        OutputDebugStringA("FAIL RegisterClassExA failed");
        ExitProcess(100);
    }

    g_hwnd = CreateWindowExA(
        0, "GlQuadClass", "WIE GL Quad",
        WS_OVERLAPPEDWINDOW,
        CW_USEDEFAULT, CW_USEDEFAULT, WINDOW_W, WINDOW_H,
        NULL, NULL, g_inst, NULL);
    if (g_hwnd == NULL) {
        OutputDebugStringA("FAIL CreateWindowExA returned NULL");
        ExitProcess(101);
    }

    {
        int rc = setup_gl();
        if (rc != 0) {
            ExitProcess(rc);
        }
    }
    {
        int rc = setup_texture();
        if (rc != 0) {
            ExitProcess(rc);
        }
    }
    {
        int rc = setup_vbo();
        if (rc != 0) {
            ExitProcess(rc);
        }
    }
    {
        int rc = setup_display_list();
        if (rc != 0) {
            ExitProcess(rc);
        }
    }
    {
        int rc = setup_shaders();
        if (rc != 0) {
            ExitProcess(rc);
        }
    }

    if (SetTimer(g_hwnd, TIMER_ID, 50, NULL) == 0) {
        OutputDebugStringA("FAIL SetTimer failed");
        ExitProcess(108);
    }

    ShowWindow(g_hwnd, SW_SHOW);

    int exit_code = 0;
    MSG msg;
    while (GetMessageA(&msg, NULL, 0, 0)) {
        TranslateMessage(&msg);
        DispatchMessageA(&msg);
    }
    exit_code = (int)msg.wParam;

    ExitProcess(exit_code);
}
