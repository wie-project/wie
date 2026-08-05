// P3/P4b/P5a + L1/L2/L3 D3D9 software-render micro-test for WIE.
//
// Exercises the software-render slice (roadmap B6 + P4b + P5a caps + the
// vs_2_0 L1 vertex stage + the L2 buffer objects + the L3 state surface):
// Direct3DCreate9 → GetDeviceCaps (honest P5a caps: ps_2_0 AND vs_2_0
// reported) → CreateDevice
// → Clear → BeginScene → DrawPrimitiveUP (XYZ|DIFFUSE gradient triangle) →
// the L2 buffer-form DrawIndexedPrimitive (CreateVertexBuffer → Lock → fill →
// Unlock → SetStreamSource → CreateIndexBuffer → Lock → fill → Unlock →
// SetIndices → GetStreamSource/GetIndices round-trip → DrawIndexedPrimitive —
// the same cyan triangle the pre-L2 exe drew via DrawIndexedPrimitiveUP) → a
// TEXTURED quad (CreateTexture → GetSurfaceLevel → LockRect → UnlockRect →
// SetTexture → DrawPrimitiveUP with XYZ|DIFFUSE|TEX1) → EndScene → Present.
// The frame is rendered from WM_PAINT; Present publishes it through the same
// PresentState surface pipeline GDI BitBlt uses.
//
// The L1 additions:
// - A vs_2_0 shader (embedded hand-encoded bytecode) transforms a textured
//   quad by the orthographic projection's constant columns; the VS output
//   (oPos/oD0/oT0) drives the same viewport transform + fragment stage as
//   the FFP path.
// - A w-skewed quad under a perspective-shear projection matrix: the
//   per-vertex clip w varies, so the perspective-correct uv interpolation
//   (this lane) picks a different texel at the quad's center than the old
//   affine interpolation would — the D3D9_RESTING_FRAME_HASH gate + the
//   host-side pixel test prove the w plumbing.
//
// Self-test (WIE_SELFTEST=1): every D3D9 call's HRESULT is checked, a
// SetViewport/GetViewport round-trip is verified, the L3 state surface is
// verified (the D3DERR_INVALIDCALL validation, the raw-value GetRenderState
// round-trip of an unmodeled state, the fog/alpha/scissor state round-trips,
// and the GetTransform / MultiplyTransform / D3DTS_TEXTURE0 round-trips),
// and after TIMER_TICKS WM_TIMER ticks (each invalidating → repaint →
// represent) the window quits with 0. Distinct non-zero codes (101-185)
// report the first stage that did not run. Interactive runs (no
// WIE_SELFTEST) keep the window open — the timer drives nothing and the
// window quits only on 'q' / close.
//
// The resting frame is deterministic: red clear + two triangles + a textured
// quad whose 2x2 checkerboard (red/green/blue/white) fills the screen region
// x∈[240,310], y∈[10,110] + a VS-driven textured quad at x∈[220,290],
// y∈[150,220] + the w-skewed quad at x∈[136,196], y∈[141,215] + the L3
// fragment-stage strip at x∈[100,220], y∈[220,235] (the alpha-tested quad,
// the fogged quad, and the scissor-clipped quad). The CI test samples pixels
// (clear red outside the geometry, triangle colors, quad texels, the
// perspective-correct w-skewed center, the L3 strip colors) and gates a
// resting-frame hash.

#define COBJMACROS
#include <windows.h>
#include <d3d9.h>

#define BACKBUFFER_W    320
#define BACKBUFFER_H    240

#define TIMER_TICKS_MIN 3
#define TIMER_ID        1

static HINSTANCE g_inst;
static HWND      g_hwnd;
static IDirect3D9       *g_d3d;
static IDirect3DDevice9 *g_device;
static IDirect3DTexture9 *g_tex;
static IDirect3DSurface9 *g_depth;
static IDirect3DVertexShader9 *g_vs;
static D3DMATRIX g_ortho;      // the baseline orthographic projection
static int g_selftest;
static int g_timer_count;

// ── the L1 vs_2_0 shader (hand-encoded bytecode) ────────────────────────
//
// vs_2_0: dcl v0/v5/v6 → mov oT0, v5 / mov oD0, v6 → transform by the
// projection columns c0..c3 (row-vector: oPos = dp4 per output channel):
//   dcl v0; dcl v5; dcl v6;
//   mov oT0, v5; mov oD0, v6;
//   dp4 oPos.x, v0, c0; dp4 oPos.y, v0, c1;
//   dp4 oPos.z, v0, c2; dp4 oPos.w, v0, c3;
//   end
static const DWORD g_vs_bytecode[] = {
    0xFFFE0200,        // vs_2_0 version token
    0x0000001F,        // dcl
    0x10000000,        // v0
    0x0000001F,        // dcl
    0x10000005,        // v5
    0x0000001F,        // dcl
    0x10000006,        // v6
    0x00000001,        // mov
    0x600F0000,        // oT0 (TEXCRDOUT reg 0, writemask all)
    0x10E40005,        // v5 (INPUT reg 5, NOSWIZZLE)
    0x00000001,        // mov
    0x500F0000,        // oD0 (ATTROUT reg 0, writemask all)
    0x10E40006,        // v6 (INPUT reg 6, NOSWIZZLE)
    0x00000009,        // dp4
    0x40010000,        // oPos.x (RASTOUT reg 0, .x writemask)
    0x10E40000,        // v0 (INPUT reg 0, NOSWIZZLE)
    0x20E40000,        // c0 (CONST reg 0, NOSWIZZLE)
    0x00000009,        // dp4
    0x40020000,        // oPos.y
    0x10E40000,        // v0
    0x20E40001,        // c1
    0x00000009,        // dp4
    0x40040000,        // oPos.z
    0x10E40000,        // v0
    0x20E40002,        // c2
    0x00000009,        // dp4
    0x40080000,        // oPos.w
    0x10E40000,        // v0
    0x20E40003,        // c3
    0x0000FFFF,        // end
};

static int selftest_enabled(void) {
    char buf[16];
    DWORD n = GetEnvironmentVariableA("WIE_SELFTEST", buf, sizeof(buf));
    return n == 1 && buf[0] == '1';
}

// Create the 2x2 checkerboard texture (red/green/blue/white) and bind it to
// stage 0. Returns 0 on success, else the exit code naming the failed stage.
static int setup_texture(void) {
    HRESULT hr = IDirect3DDevice9_CreateTexture(
        g_device, 2, 2, 1, 0, D3DFMT_A8R8G8B8, D3DPOOL_MANAGED, &g_tex, NULL);
    if (FAILED(hr) || g_tex == NULL) {
        return 130;
    }
    IDirect3DSurface9 *surf = NULL;
    hr = IDirect3DTexture9_GetSurfaceLevel(g_tex, 0, &surf);
    if (FAILED(hr) || surf == NULL) {
        return 131;
    }
    D3DLOCKED_RECT lr;
    hr = IDirect3DSurface9_LockRect(surf, &lr, NULL, 0);
    if (FAILED(hr) || lr.pBits == NULL || lr.Pitch < 8) {
        return 132;
    }
    DWORD *bits = (DWORD *)lr.pBits;
    bits[0] = D3DCOLOR_XRGB(255, 0, 0);   // red    (top-left)
    bits[1] = D3DCOLOR_XRGB(0, 255, 0);   // green  (top-right)
    bits[2] = D3DCOLOR_XRGB(0, 0, 255);   // blue   (bottom-left)
    bits[3] = D3DCOLOR_XRGB(255, 255, 255); // white (bottom-right)
    hr = IDirect3DSurface9_UnlockRect(surf);
    if (FAILED(hr)) {
        return 133;
    }
    IDirect3DSurface9_Release(surf);
    // SetTexture takes an IDirect3DBaseTexture9* — the concrete texture is a
    // distinct C struct in the mingw headers, so cast to the base type.
    hr = IDirect3DDevice9_SetTexture(g_device, 0, (IDirect3DBaseTexture9 *)g_tex);
    if (FAILED(hr)) {
        return 134;
    }
    return 0;
}

// Draw one XYZRHW quad (two triangles) with the current FVF/render state.
// Returns 0 on success, else the exit code naming the failed stage.
static int draw_rhw_quad(float x0, float y0, float x1, float y1, float z, DWORD color) {
    struct RhwQuadVertex { float x, y, z, rhw; DWORD color; };
    struct RhwQuadVertex q[4];
    q[0].x = x0; q[0].y = y0; q[0].z = z; q[0].rhw = 1.0f; q[0].color = color;
    q[1].x = x1; q[1].y = y0; q[1].z = z; q[1].rhw = 1.0f; q[1].color = color;
    q[2].x = x0; q[2].y = y1; q[2].z = z; q[2].rhw = 1.0f; q[2].color = color;
    q[3].x = x1; q[3].y = y1; q[3].z = z; q[3].rhw = 1.0f; q[3].color = color;
    struct RhwQuadVertex verts[6];
    verts[0] = q[0]; verts[1] = q[1]; verts[2] = q[2];
    verts[3] = q[1]; verts[4] = q[3]; verts[5] = q[2];
    HRESULT hr = IDirect3DDevice9_DrawPrimitiveUP(
        g_device, D3DPT_TRIANGLELIST, 2, verts, (UINT)sizeof(verts[0]));
    return FAILED(hr) ? 183 : 0;
}

// Bit-exact matrix compare (no libc in this -nostdlib micro-exe).
static int matrix_equal(const D3DMATRIX *a, const D3DMATRIX *b) {
    const unsigned char *pa = (const unsigned char *)a;
    const unsigned char *pb = (const unsigned char *)b;
    for (int i = 0; i < (int)sizeof(D3DMATRIX); i++) {
        if (pa[i] != pb[i]) {
            return 0;
        }
    }
    return 1;
}

// Render one deterministic frame into the D3D9 backbuffer and present it.
// Returns 0 on success, else the exit code naming the failed stage.
static int render_frame(void) {
    // Start untextured — setup_texture may have left stage 0 bound, and the
    // triangles below have no TEX1 (a bound stage would modulate them with
    // the uv (0,0) texel).
    if (FAILED(IDirect3DDevice9_SetTexture(g_device, 0, NULL))) {
        return 134;
    }
    // Deterministic blend/depth baseline for the P4c quads.
    if (FAILED(IDirect3DDevice9_SetRenderState(g_device, D3DRS_ALPHABLENDENABLE, FALSE)) ||
        FAILED(IDirect3DDevice9_SetRenderState(g_device, D3DRS_ZENABLE, FALSE))) {
        return 137;
    }
    // D3DCOLOR_XRGB(200, 0, 0) — pure red clear, no alpha. Clear the depth
    // buffer to the far plane (z=1.0) so the P4c depth quads test against it.
    if (FAILED(IDirect3DDevice9_Clear(g_device, 0, NULL,
                                      D3DCLEAR_TARGET | D3DCLEAR_ZBUFFER,
                                      D3DCOLOR_XRGB(200, 0, 0), 1.0f, 0))) {
        return 106;
    }
    if (FAILED(IDirect3DDevice9_BeginScene(g_device))) {
        return 105;
    }
    // XYZ | DIFFUSE, 16-byte vertices. The orthographic projection maps
    // x in [-160,160], y in [-120,120] to the full 320x240 backbuffer.
    if (FAILED(IDirect3DDevice9_SetFVF(g_device, D3DFVF_XYZ | D3DFVF_DIFFUSE))) {
        return 104;
    }
    struct Vertex { float x, y, z; DWORD color; };
    struct Vertex tri[3];
    tri[0].x = -120.0f; tri[0].y = -80.0f; tri[0].z = 0.0f;
    tri[0].color = D3DCOLOR_XRGB(255, 0, 0);   // red
    tri[1].x = 120.0f; tri[1].y = -80.0f; tri[1].z = 0.0f;
    tri[1].color = D3DCOLOR_XRGB(0, 255, 0);   // green
    tri[2].x = 0.0f; tri[2].y = 80.0f; tri[2].z = 0.0f;
    tri[2].color = D3DCOLOR_XRGB(0, 0, 255);   // blue
    if (FAILED(IDirect3DDevice9_DrawPrimitiveUP(g_device, D3DPT_TRIANGLELIST,
                                                1, tri, (UINT)sizeof(tri[0])))) {
        return 107;
    }
    // Indexed path (L2 buffer form): a second triangle (solid cyan) below the
    // first via CreateVertexBuffer → Lock → fill → Unlock → SetStreamSource
    // → CreateIndexBuffer → Lock → fill → Unlock → SetIndices →
    // DrawIndexedPrimitive. The geometry is identical to the pre-L2
    // DrawIndexedPrimitiveUP call, so the resting-frame pixels do not change;
    // the GetStreamSource/GetIndices round-trip asserts the getters.
    struct Vertex indexed[3];
    indexed[0].x = -60.0f; indexed[0].y = 100.0f; indexed[0].z = 0.0f;
    indexed[0].color = D3DCOLOR_XRGB(0, 255, 255);
    indexed[1].x = 60.0f; indexed[1].y = 100.0f; indexed[1].z = 0.0f;
    indexed[1].color = D3DCOLOR_XRGB(0, 255, 255);
    indexed[2].x = 0.0f; indexed[2].y = 40.0f; indexed[2].z = 0.0f;
    indexed[2].color = D3DCOLOR_XRGB(0, 255, 255);
    {
        IDirect3DVertexBuffer9 *vb = NULL;
        IDirect3DIndexBuffer9 *ib = NULL;
        HRESULT hr = IDirect3DDevice9_CreateVertexBuffer(
            g_device, (UINT)sizeof(indexed), D3DUSAGE_WRITEONLY,
            D3DFVF_XYZ | D3DFVF_DIFFUSE, D3DPOOL_MANAGED, &vb, NULL);
        if (FAILED(hr) || vb == NULL) {
            return 156;
        }
        void *vdata = NULL;
        hr = IDirect3DVertexBuffer9_Lock(vb, 0, 0, &vdata, 0);
        if (FAILED(hr) || vdata == NULL) {
            return 157;
        }
        // The guest fills the locked block through ordinary memory writes
        // (struct assignment — no libc in this -nostdlib micro-exe).
        {
            struct Vertex *vd = (struct Vertex *)vdata;
            vd[0] = indexed[0];
            vd[1] = indexed[1];
            vd[2] = indexed[2];
        }
        if (FAILED(IDirect3DVertexBuffer9_Unlock(vb))) {
            return 158;
        }
        hr = IDirect3DDevice9_SetStreamSource(g_device, 0, vb, 0,
                                              (UINT)sizeof(indexed[0]));
        if (FAILED(hr)) {
            return 159;
        }
        hr = IDirect3DDevice9_CreateIndexBuffer(
            g_device, (UINT)(3 * sizeof(unsigned short)), D3DUSAGE_WRITEONLY,
            D3DFMT_INDEX16, D3DPOOL_MANAGED, &ib, NULL);
        if (FAILED(hr) || ib == NULL) {
            return 160;
        }
        void *idata = NULL;
        hr = IDirect3DIndexBuffer9_Lock(ib, 0, 0, &idata, 0);
        if (FAILED(hr) || idata == NULL) {
            return 161;
        }
        {
            unsigned short *id = (unsigned short *)idata;
            id[0] = 0;
            id[1] = 1;
            id[2] = 2;
        }
        if (FAILED(IDirect3DIndexBuffer9_Unlock(ib))) {
            return 162;
        }
        if (FAILED(IDirect3DDevice9_SetIndices(g_device, ib))) {
            return 163;
        }
        {
            // The L2 round-trip getters: GetStreamSource must return the bound
            // buffer with the exact offset/stride, GetIndices the index buffer.
            IDirect3DVertexBuffer9 *vb2 = NULL;
            UINT off = 0xDEADu;
            UINT stride = 0xDEADu;
            if (FAILED(IDirect3DDevice9_GetStreamSource(
                           g_device, 0, &vb2, &off, &stride)) ||
                vb2 != vb || off != 0 || stride != (UINT)sizeof(indexed[0])) {
                return 164;
            }
            IDirect3DIndexBuffer9 *ib2 = NULL;
            if (FAILED(IDirect3DDevice9_GetIndices(g_device, &ib2)) || ib2 != ib) {
                return 165;
            }
        }
        if (FAILED(IDirect3DDevice9_DrawIndexedPrimitive(
                       g_device, D3DPT_TRIANGLELIST, 0, 0, 3, 0, 1))) {
            return 166;
        }
        // Unbind + release the per-frame buffers (device-owned COM objects).
        IDirect3DDevice9_SetStreamSource(g_device, 0, NULL, 0, 0);
        IDirect3DDevice9_SetIndices(g_device, NULL);
        IDirect3DVertexBuffer9_Release(vb);
        IDirect3DIndexBuffer9_Release(ib);
    }
    // Textured quad (XYZ|DIFFUSE|TEX1, 24-byte vertices) covering the screen
    // region x∈[240,310], y∈[10,110] with the full uv range. World coords:
    // screen (240,10) = world (80,110), screen (310,110) = world (150,10).
    // The texture is bound only for this draw so the triangles above stay
    // untextured (a bound stage samples uv (0,0) for TEX1-less vertices).
    if (FAILED(IDirect3DDevice9_SetTexture(g_device, 0, (IDirect3DBaseTexture9 *)g_tex))) {
        return 134;
    }
    if (FAILED(IDirect3DDevice9_SetFVF(g_device,
                                       D3DFVF_XYZ | D3DFVF_DIFFUSE | D3DFVF_TEX1))) {
        return 135;
    }
    struct TexVertex { float x, y, z; DWORD color; float u, v; };
    struct TexVertex quad[4];
    quad[0].x = 80.0f;  quad[0].y = 110.0f; quad[0].z = 0.0f;
    quad[0].color = D3DCOLOR_XRGB(255, 255, 255);
    quad[0].u = 0.0f; quad[0].v = 0.0f;
    quad[1].x = 150.0f; quad[1].y = 110.0f; quad[1].z = 0.0f;
    quad[1].color = D3DCOLOR_XRGB(255, 255, 255);
    quad[1].u = 1.0f; quad[1].v = 0.0f;
    quad[2].x = 80.0f; quad[2].y = 10.0f; quad[2].z = 0.0f;
    quad[2].color = D3DCOLOR_XRGB(255, 255, 255);
    quad[2].u = 0.0f; quad[2].v = 1.0f;
    quad[3].x = 150.0f; quad[3].y = 10.0f; quad[3].z = 0.0f;
    quad[3].color = D3DCOLOR_XRGB(255, 255, 255);
    quad[3].u = 1.0f; quad[3].v = 1.0f;
    {
        struct TexVertex verts[6];
        verts[0] = quad[0]; verts[1] = quad[1]; verts[2] = quad[2];
        verts[3] = quad[1]; verts[4] = quad[3]; verts[5] = quad[2];
        if (FAILED(IDirect3DDevice9_DrawPrimitiveUP(
                       g_device, D3DPT_TRIANGLELIST, 2, verts,
                       (UINT)sizeof(verts[0])))) {
            return 136;
        }
    }
    // Unbind so the next frame's triangles start untextured.
    if (FAILED(IDirect3DDevice9_SetTexture(g_device, 0, NULL))) {
        return 134;
    }
    // ── L1 vertex-shader quad (vs_2_0 execution) ─────────────────────────
    // A textured quad at screen x∈[220,290], y∈[150,220] (world x∈[60,130],
    // y∈[-100,-30]) transformed by the VS shader: the constant registers
    // c0..c3 hold the orthographic projection's columns and the shader
    // computes oPos = v0·M channel by channel. oT0 (uv) and oD0 (diffuse)
    // pass through, proving the VS output feeds the viewport transform +
    // fragment stage like the FFP path.
    {
        float vs_c[16];
        vs_c[0]  = g_ortho._11; vs_c[1]  = g_ortho._21;
        vs_c[2]  = g_ortho._31; vs_c[3]  = g_ortho._41;
        vs_c[4]  = g_ortho._12; vs_c[5]  = g_ortho._22;
        vs_c[6]  = g_ortho._32; vs_c[7]  = g_ortho._42;
        vs_c[8]  = g_ortho._13; vs_c[9]  = g_ortho._23;
        vs_c[10] = g_ortho._33; vs_c[11] = g_ortho._43;
        vs_c[12] = g_ortho._14; vs_c[13] = g_ortho._24;
        vs_c[14] = g_ortho._34; vs_c[15] = g_ortho._44;
        if (FAILED(IDirect3DDevice9_SetVertexShaderConstantF(
                       g_device, 0, vs_c, 4))) {
            return 151;
        }
        if (FAILED(IDirect3DDevice9_SetVertexShader(g_device, g_vs))) {
            return 152;
        }
        if (FAILED(IDirect3DDevice9_SetTexture(g_device, 0,
                                               (IDirect3DBaseTexture9 *)g_tex))) {
            return 134;
        }
        struct TexVertex vsq[4];
        vsq[0].x = 60.0f;  vsq[0].y = -100.0f; vsq[0].z = 0.0f;
        vsq[0].color = D3DCOLOR_XRGB(255, 255, 255);
        vsq[0].u = 0.0f; vsq[0].v = 0.0f;
        vsq[1].x = 130.0f; vsq[1].y = -100.0f; vsq[1].z = 0.0f;
        vsq[1].color = D3DCOLOR_XRGB(255, 255, 255);
        vsq[1].u = 1.0f; vsq[1].v = 0.0f;
        vsq[2].x = 60.0f; vsq[2].y = -30.0f; vsq[2].z = 0.0f;
        vsq[2].color = D3DCOLOR_XRGB(255, 255, 255);
        vsq[2].u = 0.0f; vsq[2].v = 1.0f;
        vsq[3].x = 130.0f; vsq[3].y = -30.0f; vsq[3].z = 0.0f;
        vsq[3].color = D3DCOLOR_XRGB(255, 255, 255);
        vsq[3].u = 1.0f; vsq[3].v = 1.0f;
        struct TexVertex vsverts[6];
        vsverts[0] = vsq[0]; vsverts[1] = vsq[1]; vsverts[2] = vsq[2];
        vsverts[3] = vsq[1]; vsverts[4] = vsq[3]; vsverts[5] = vsq[2];
        if (FAILED(IDirect3DDevice9_DrawPrimitiveUP(
                       g_device, D3DPT_TRIANGLELIST, 2, vsverts,
                       (UINT)sizeof(vsverts[0])))) {
            return 153;
        }
        if (FAILED(IDirect3DDevice9_SetVertexShader(g_device, NULL)) ||
            FAILED(IDirect3DDevice9_SetTexture(g_device, 0, NULL))) {
            return 152;
        }
    }
    // ── L1 w-skewed quad (perspective-correct interpolation) ─────────────
    // The projection matrix is the ortho PLUS a w-shear (`_24 = 0.002`, so
    // clip w = 1 + 0.002·y varies 0.84..0.96 across the quad). The quad
    // covers screen x∈[136,196], y∈[141,215]; its center pixel (165,175)
    // samples texel (0,0) RED under perspective-correct uv (≈(0.499,0.499))
    // but texel (1,0) GREEN under the old affine interpolation
    // (≈(0.532,0.466)) — the host renderer test + the resting-frame hash
    // gate the difference.
    {
        D3DMATRIX wskew = g_ortho;
        wskew._24 = 0.002f;
        if (FAILED(IDirect3DDevice9_SetTransform(
                       g_device, D3DTS_PROJECTION, &wskew))) {
            return 154;
        }
        if (FAILED(IDirect3DDevice9_SetTexture(g_device, 0,
                                               (IDirect3DBaseTexture9 *)g_tex))) {
            return 134;
        }
        struct TexVertex wsq[4];
        wsq[0].x = -20.0f; wsq[0].y = -20.0f; wsq[0].z = 0.0f;
        wsq[0].color = D3DCOLOR_XRGB(255, 255, 255);
        wsq[0].u = 0.0f; wsq[0].v = 0.0f;
        wsq[1].x = 30.0f; wsq[1].y = -20.0f; wsq[1].z = 0.0f;
        wsq[1].color = D3DCOLOR_XRGB(255, 255, 255);
        wsq[1].u = 1.0f; wsq[1].v = 0.0f;
        wsq[2].x = -20.0f; wsq[2].y = -80.0f; wsq[2].z = 0.0f;
        wsq[2].color = D3DCOLOR_XRGB(255, 255, 255);
        wsq[2].u = 0.0f; wsq[2].v = 1.0f;
        wsq[3].x = 30.0f; wsq[3].y = -80.0f; wsq[3].z = 0.0f;
        wsq[3].color = D3DCOLOR_XRGB(255, 255, 255);
        wsq[3].u = 1.0f; wsq[3].v = 1.0f;
        struct TexVertex wsverts[6];
        wsverts[0] = wsq[0]; wsverts[1] = wsq[1]; wsverts[2] = wsq[2];
        wsverts[3] = wsq[1]; wsverts[4] = wsq[3]; wsverts[5] = wsq[2];
        if (FAILED(IDirect3DDevice9_DrawPrimitiveUP(
                       g_device, D3DPT_TRIANGLELIST, 2, wsverts,
                       (UINT)sizeof(wsverts[0])))) {
            return 155;
        }
        if (FAILED(IDirect3DDevice9_SetTransform(
                       g_device, D3DTS_PROJECTION, &g_ortho)) ||
            FAILED(IDirect3DDevice9_SetTexture(g_device, 0, NULL))) {
            return 154;
        }
    }
    // ── P4c blend: an opaque red quad, then a half-alpha blue quad over its
    // left half. Blend factors SRCALPHA/INVSRCALPHA, ADD.
    // Screen region: x∈[10,90], y∈[10,60]; the blue half is x∈[10,50].
    if (FAILED(IDirect3DDevice9_SetFVF(g_device, D3DFVF_XYZ | D3DFVF_DIFFUSE))) {
        return 138;
    }
    {
        struct Vertex red_q[4];
        red_q[0].x = -150.0f; red_q[0].y = 110.0f; red_q[0].z = 0.0f;
        red_q[0].color = D3DCOLOR_ARGB(255, 255, 0, 0);
        red_q[1].x = -70.0f;  red_q[1].y = 110.0f; red_q[1].z = 0.0f;
        red_q[1].color = D3DCOLOR_ARGB(255, 255, 0, 0);
        red_q[2].x = -150.0f; red_q[2].y = 60.0f; red_q[2].z = 0.0f;
        red_q[2].color = D3DCOLOR_ARGB(255, 255, 0, 0);
        red_q[3].x = -70.0f;  red_q[3].y = 60.0f; red_q[3].z = 0.0f;
        red_q[3].color = D3DCOLOR_ARGB(255, 255, 0, 0);
        struct Vertex verts[6];
        verts[0] = red_q[0]; verts[1] = red_q[1]; verts[2] = red_q[2];
        verts[3] = red_q[1]; verts[4] = red_q[3]; verts[5] = red_q[2];
        if (FAILED(IDirect3DDevice9_DrawPrimitiveUP(
                       g_device, D3DPT_TRIANGLELIST, 2, verts,
                       (UINT)sizeof(verts[0])))) {
            return 138;
        }
    }
    if (FAILED(IDirect3DDevice9_SetRenderState(g_device, D3DRS_ALPHABLENDENABLE, TRUE)) ||
        FAILED(IDirect3DDevice9_SetRenderState(g_device, D3DRS_SRCBLEND, D3DBLEND_SRCALPHA)) ||
        FAILED(IDirect3DDevice9_SetRenderState(g_device, D3DRS_DESTBLEND, D3DBLEND_INVSRCALPHA)) ||
        FAILED(IDirect3DDevice9_SetRenderState(g_device, D3DRS_BLENDOP, D3DBLENDOP_ADD))) {
        return 139;
    }
    {
        struct Vertex blue_q[4];
        blue_q[0].x = -150.0f; blue_q[0].y = 110.0f; blue_q[0].z = 0.0f;
        blue_q[0].color = D3DCOLOR_ARGB(128, 0, 0, 255);   // alpha 0x80 blue
        blue_q[1].x = -110.0f; blue_q[1].y = 110.0f; blue_q[1].z = 0.0f;
        blue_q[1].color = D3DCOLOR_ARGB(128, 0, 0, 255);
        blue_q[2].x = -150.0f; blue_q[2].y = 60.0f; blue_q[2].z = 0.0f;
        blue_q[2].color = D3DCOLOR_ARGB(128, 0, 0, 255);
        blue_q[3].x = -110.0f; blue_q[3].y = 60.0f; blue_q[3].z = 0.0f;
        blue_q[3].color = D3DCOLOR_ARGB(128, 0, 0, 255);
        struct Vertex verts[6];
        verts[0] = blue_q[0]; verts[1] = blue_q[1]; verts[2] = blue_q[2];
        verts[3] = blue_q[1]; verts[4] = blue_q[3]; verts[5] = blue_q[2];
        if (FAILED(IDirect3DDevice9_DrawPrimitiveUP(
                       g_device, D3DPT_TRIANGLELIST, 2, verts,
                       (UINT)sizeof(verts[0])))) {
            return 140;
        }
    }
    // ── P4c depth: two overlapping XYZRHW quads (near z=0.1 white, far
    // z=0.9 magenta). ZENABLE TRUE + LESSEQUAL + ZWRITEENABLE. Screen region
    // x∈[10,90], y∈[90,150]; the far quad's left half (x∈[10,50]) is covered
    // by the near quad and must lose the depth test there.
    if (FAILED(IDirect3DDevice9_SetRenderState(g_device, D3DRS_ALPHABLENDENABLE, FALSE)) ||
        FAILED(IDirect3DDevice9_SetRenderState(g_device, D3DRS_ZENABLE, D3DZB_TRUE)) ||
        FAILED(IDirect3DDevice9_SetRenderState(g_device, D3DRS_ZFUNC, D3DCMP_LESSEQUAL)) ||
        FAILED(IDirect3DDevice9_SetRenderState(g_device, D3DRS_ZWRITEENABLE, TRUE))) {
        return 141;
    }
    if (FAILED(IDirect3DDevice9_SetFVF(g_device,
                                       D3DFVF_XYZRHW | D3DFVF_DIFFUSE))) {
        return 142;
    }
    struct RhwVertex { float x, y, z, rhw; DWORD color; };
    {
        // Near quad: right half only, white — wins the overlap with the far.
        struct RhwVertex near_q[4];
        near_q[0].x = 50.0f;  near_q[0].y = 90.0f;  near_q[0].z = 0.1f; near_q[0].rhw = 1.0f;
        near_q[0].color = D3DCOLOR_XRGB(255, 255, 255);
        near_q[1].x = 90.0f;  near_q[1].y = 90.0f;  near_q[1].z = 0.1f; near_q[1].rhw = 1.0f;
        near_q[1].color = D3DCOLOR_XRGB(255, 255, 255);
        near_q[2].x = 50.0f;  near_q[2].y = 150.0f; near_q[2].z = 0.1f; near_q[2].rhw = 1.0f;
        near_q[2].color = D3DCOLOR_XRGB(255, 255, 255);
        near_q[3].x = 90.0f;  near_q[3].y = 150.0f; near_q[3].z = 0.1f; near_q[3].rhw = 1.0f;
        near_q[3].color = D3DCOLOR_XRGB(255, 255, 255);
        struct RhwVertex verts[6];
        verts[0] = near_q[0]; verts[1] = near_q[1]; verts[2] = near_q[2];
        verts[3] = near_q[1]; verts[4] = near_q[3]; verts[5] = near_q[2];
        if (FAILED(IDirect3DDevice9_DrawPrimitiveUP(
                       g_device, D3DPT_TRIANGLELIST, 2, verts,
                       (UINT)sizeof(verts[0])))) {
            return 143;
        }
    }
    {
        // Far quad: full region, magenta — must lose the near overlap.
        struct RhwVertex far_q[4];
        far_q[0].x = 10.0f;  far_q[0].y = 90.0f;  far_q[0].z = 0.9f; far_q[0].rhw = 1.0f;
        far_q[0].color = D3DCOLOR_XRGB(255, 0, 255);
        far_q[1].x = 90.0f;  far_q[1].y = 90.0f;  far_q[1].z = 0.9f; far_q[1].rhw = 1.0f;
        far_q[1].color = D3DCOLOR_XRGB(255, 0, 255);
        far_q[2].x = 10.0f;  far_q[2].y = 150.0f; far_q[2].z = 0.9f; far_q[2].rhw = 1.0f;
        far_q[2].color = D3DCOLOR_XRGB(255, 0, 255);
        far_q[3].x = 90.0f;  far_q[3].y = 150.0f; far_q[3].z = 0.9f; far_q[3].rhw = 1.0f;
        far_q[3].color = D3DCOLOR_XRGB(255, 0, 255);
        struct RhwVertex verts[6];
        verts[0] = far_q[0]; verts[1] = far_q[1]; verts[2] = far_q[2];
        verts[3] = far_q[1]; verts[4] = far_q[3]; verts[5] = far_q[2];
        if (FAILED(IDirect3DDevice9_DrawPrimitiveUP(
                       g_device, D3DPT_TRIANGLELIST, 2, verts,
                       (UINT)sizeof(verts[0])))) {
            return 144;
        }
    }
    // ── L3 fragment stages: alpha test, fog, scissor ────────────────────
    // A deterministic strip at y∈[220,235], x∈[100,220] — clear of every
    // other draw (the w-skew quad ends at y≈215, the gradient triangle at
    // y=200):
    // - Alpha-test quad at x∈[100,150]: the left half's alpha 0x20 fails
    //   ALPHAFUNC GREATER 0x40 and is clipped (clear red shows), the right
    //   half's 0x80 passes and draws white.
    // - Fog quad at x∈[150,190], z=0.5: red diffuse under blue LINEAR fog
    //   over [0.25, 0.75] → f=0.5 → (128, 0, 128).
    // - Scissor quad at x∈[190,220] clipped to x∈[190,206) by SetScissorRect:
    //   the clipped right part shows clear red.
    if (FAILED(IDirect3DDevice9_SetRenderState(g_device, D3DRS_ALPHABLENDENABLE, FALSE)) ||
        FAILED(IDirect3DDevice9_SetRenderState(g_device, D3DRS_ZENABLE, FALSE))) {
        return 180;
    }
    if (FAILED(IDirect3DDevice9_SetFVF(g_device, D3DFVF_XYZRHW | D3DFVF_DIFFUSE))) {
        return 181;
    }
    if (FAILED(IDirect3DDevice9_SetRenderState(g_device, D3DRS_ALPHATESTENABLE, TRUE)) ||
        FAILED(IDirect3DDevice9_SetRenderState(g_device, D3DRS_ALPHAFUNC, D3DCMP_GREATER)) ||
        FAILED(IDirect3DDevice9_SetRenderState(g_device, D3DRS_ALPHAREF, 0x40))) {
        return 182;
    }
    {
        int rc = draw_rhw_quad(100.0f, 220.0f, 125.0f, 235.0f, 0.5f,
                               D3DCOLOR_ARGB(0x20, 255, 255, 255));
        if (rc != 0) {
            return rc;
        }
        rc = draw_rhw_quad(125.0f, 220.0f, 150.0f, 235.0f, 0.5f,
                           D3DCOLOR_ARGB(0x80, 255, 255, 255));
        if (rc != 0) {
            return rc;
        }
    }
    {
        union { float f; DWORD d; } fog_start = { 0.25f };
        union { float f; DWORD d; } fog_end = { 0.75f };
        if (FAILED(IDirect3DDevice9_SetRenderState(g_device, D3DRS_ALPHATESTENABLE, FALSE)) ||
            FAILED(IDirect3DDevice9_SetRenderState(g_device, D3DRS_FOGENABLE, TRUE)) ||
            FAILED(IDirect3DDevice9_SetRenderState(g_device, D3DRS_FOGCOLOR,
                                                   D3DCOLOR_XRGB(0, 0, 255))) ||
            FAILED(IDirect3DDevice9_SetRenderState(g_device, D3DRS_FOGSTART, fog_start.d)) ||
            FAILED(IDirect3DDevice9_SetRenderState(g_device, D3DRS_FOGEND, fog_end.d)) ||
            FAILED(IDirect3DDevice9_SetRenderState(g_device, D3DRS_FOGTABLEMODE, D3DFOG_LINEAR))) {
            return 184;
        }
        int rc = draw_rhw_quad(150.0f, 220.0f, 190.0f, 235.0f, 0.5f,
                               D3DCOLOR_XRGB(255, 0, 0));
        if (rc != 0) {
            return rc;
        }
    }
    {
        RECT scissor;
        scissor.left = 190; scissor.top = 215; scissor.right = 206; scissor.bottom = 240;
        if (FAILED(IDirect3DDevice9_SetRenderState(g_device, D3DRS_FOGENABLE, FALSE)) ||
            FAILED(IDirect3DDevice9_SetRenderState(g_device, D3DRS_SCISSORTESTENABLE, TRUE)) ||
            FAILED(IDirect3DDevice9_SetScissorRect(g_device, &scissor))) {
            return 185;
        }
        int rc = draw_rhw_quad(190.0f, 220.0f, 220.0f, 235.0f, 0.5f,
                               D3DCOLOR_XRGB(0, 255, 0));
        if (rc != 0) {
            return rc;
        }
        if (FAILED(IDirect3DDevice9_SetRenderState(g_device, D3DRS_SCISSORTESTENABLE, FALSE))) {
            return 185;
        }
    }
    if (FAILED(IDirect3DDevice9_EndScene(g_device))) {
        return 108;
    }
    if (FAILED(IDirect3DDevice9_Present(g_device, NULL, NULL, NULL, NULL))) {
        return 109;
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
            return 0;   // interactive: nothing drives the frame loop
        }
        g_timer_count++;
        // Repaint → re-render → re-present (exercises repeated presents).
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
        if (g_device) {
            IDirect3DDevice9_Release(g_device);
            g_device = NULL;
        }
        if (g_d3d) {
            IDirect3D9_Release(g_d3d);
            g_d3d = NULL;
        }
        if (g_tex) {
            IDirect3DTexture9_Release(g_tex);
            g_tex = NULL;
        }
        if (g_depth) {
            IDirect3DSurface9_Release(g_depth);
            g_depth = NULL;
        }
        if (g_vs) {
            IDirect3DVertexShader9_Release(g_vs);
            g_vs = NULL;
        }
        if (g_timer_count < TIMER_TICKS_MIN) {
            PostQuitMessage(120);
        }
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
    wc.lpszClassName = "GuiD3D9Class";
    wc.hIconSm       = NULL;

    if (RegisterClassExA(&wc) == 0) {
        ExitProcess(100);
    }

    g_hwnd = CreateWindowExA(
        0, "GuiD3D9Class", "WIE GUI D3D9",
        WS_OVERLAPPEDWINDOW,
        CW_USEDEFAULT, CW_USEDEFAULT, BACKBUFFER_W, BACKBUFFER_H,
        NULL, NULL, g_inst, NULL);
    if (g_hwnd == NULL) {
        ExitProcess(101);
    }

    // P3 pipeline setup.
    g_d3d = Direct3DCreate9(D3D_SDK_VERSION);
    if (g_d3d == NULL) {
        ExitProcess(102);
    }
    {
        // P5a caps honesty: the ps_2_0 interpreter AND the vs_2_0 vertex
        // stage are implemented, so the caps report D3DPS_VERSION(2,0),
        // D3DVS_VERSION(2,0), PixelShader1xMaxValue 1.0, and
        // MaxVertexShaderConst 256 (the implemented vs_2_0 constant file).
        D3DCAPS9 caps;
        if (FAILED(IDirect3D9_GetDeviceCaps(g_d3d, 0, D3DDEVTYPE_HAL, &caps))) {
            ExitProcess(103);
        }
        if (caps.VertexShaderVersion != D3DVS_VERSION(2, 0)) {
            ExitProcess(104);
        }
        if (caps.PixelShaderVersion != D3DPS_VERSION(2, 0)) {
            ExitProcess(104);
        }
        if (caps.MaxVertexShaderConst != 256) {
            ExitProcess(104);
        }
        if (caps.PixelShader1xMaxValue != 1.0f) {
            ExitProcess(104);
        }
    }
    {
        D3DPRESENT_PARAMETERS d3dpp = { 0 };
        d3dpp.BackBufferWidth  = BACKBUFFER_W;
        d3dpp.BackBufferHeight = BACKBUFFER_H;
        d3dpp.BackBufferFormat = D3DFMT_X8R8G8B8;
        d3dpp.BackBufferCount  = 1;
        d3dpp.MultiSampleType  = D3DMULTISAMPLE_NONE;
        d3dpp.SwapEffect       = D3DSWAPEFFECT_DISCARD;
        d3dpp.hDeviceWindow    = g_hwnd;
        d3dpp.Windowed         = TRUE;
        d3dpp.PresentationInterval = 0;

        HRESULT hr = IDirect3D9_CreateDevice(
            g_d3d, 0, D3DDEVTYPE_HAL, g_hwnd,
            D3DCREATE_SOFTWARE_VERTEXPROCESSING, &d3dpp, &g_device);
        if (FAILED(hr) || g_device == NULL) {
            ExitProcess(105);
        }
    }
    {
        // Create the vs_2_0 shader (embedded bytecode above).
        HRESULT hr = IDirect3DDevice9_CreateVertexShader(g_device,
                                                         g_vs_bytecode, &g_vs);
        if (FAILED(hr) || g_vs == NULL) {
            ExitProcess(150);
        }
    }
    {
        // Orthographic projection: world x ∈ [-160,160] → NDC [-1,1],
        // world y ∈ [-120,120] → NDC [-1,1] (y-up world, y-down screen).
        // Stored in g_ortho: the VS quad's constant columns and the w-skewed
        // quad's restore both read it.
        D3DMATRIX proj;
        proj._11 = 2.0f / BACKBUFFER_W; proj._12 = 0.0f; proj._13 = 0.0f; proj._14 = 0.0f;
        proj._21 = 0.0f; proj._22 = 2.0f / BACKBUFFER_H; proj._23 = 0.0f; proj._24 = 0.0f;
        proj._31 = 0.0f; proj._32 = 0.0f; proj._33 = 1.0f; proj._34 = 0.0f;
        proj._41 = 0.0f; proj._42 = 0.0f; proj._43 = 0.0f; proj._44 = 1.0f;
        g_ortho = proj;
        if (FAILED(IDirect3DDevice9_SetTransform(g_device, D3DTS_PROJECTION, &proj))) {
            ExitProcess(110);
        }
    }
    {
        // Viewport round-trip: the default is the full backbuffer.
        D3DVIEWPORT9 vp;
        if (FAILED(IDirect3DDevice9_GetViewport(g_device, &vp))) {
            ExitProcess(121);
        }
        if (vp.X != 0 || vp.Y != 0 || vp.Width != BACKBUFFER_W ||
            vp.Height != BACKBUFFER_H || vp.MinZ != 0.0f || vp.MaxZ != 1.0f) {
            ExitProcess(121);
        }
    }
    {
        // ── L3 state-surface selftest ──
        // Validation: an out-of-range render-state value is the honest
        // D3DERR_INVALIDCALL (0x8876086C), never a silent accept.
        HRESULT hr = IDirect3DDevice9_SetRenderState(g_device, D3DRS_SRCBLEND, 0xDEAD);
        if (hr != D3DERR_INVALIDCALL) {
            ExitProcess(170);
        }
        // Raw-value round-trip: an unmodeled state keeps its last-set value.
        hr = IDirect3DDevice9_SetRenderState(g_device, D3DRS_CULLMODE, 2);
        if (FAILED(hr)) {
            ExitProcess(171);
        }
        DWORD cull = 0;
        hr = IDirect3DDevice9_GetRenderState(g_device, D3DRS_CULLMODE, &cull);
        if (FAILED(hr) || cull != 2) {
            ExitProcess(172);
        }
        // The L3 fragment states round-trip (fog color bit-exact).
        union { float f; DWORD d; } fog_start = { 0.25f };
        union { float f; DWORD d; } fog_end = { 0.75f };
        if (FAILED(IDirect3DDevice9_SetRenderState(g_device, D3DRS_FOGENABLE, TRUE)) ||
            FAILED(IDirect3DDevice9_SetRenderState(g_device, D3DRS_FOGCOLOR,
                                                   D3DCOLOR_XRGB(0, 0, 255))) ||
            FAILED(IDirect3DDevice9_SetRenderState(g_device, D3DRS_FOGSTART, fog_start.d)) ||
            FAILED(IDirect3DDevice9_SetRenderState(g_device, D3DRS_FOGEND, fog_end.d)) ||
            FAILED(IDirect3DDevice9_SetRenderState(g_device, D3DRS_FOGTABLEMODE, D3DFOG_LINEAR)) ||
            FAILED(IDirect3DDevice9_SetRenderState(g_device, D3DRS_ALPHATESTENABLE, TRUE)) ||
            FAILED(IDirect3DDevice9_SetRenderState(g_device, D3DRS_ALPHAFUNC, D3DCMP_GREATER)) ||
            FAILED(IDirect3DDevice9_SetRenderState(g_device, D3DRS_ALPHAREF, 0x40)) ||
            FAILED(IDirect3DDevice9_SetRenderState(g_device, D3DRS_SCISSORTESTENABLE, TRUE))) {
            ExitProcess(173);
        }
        DWORD fog_en = 0, fog_col = 0, alpha_en = 0, scissor_en = 0;
        DWORD fog_start_out = 0, fog_end_out = 0, alpha_func = 0, alpha_ref = 0;
        if (FAILED(IDirect3DDevice9_GetRenderState(g_device, D3DRS_FOGENABLE, &fog_en)) ||
            FAILED(IDirect3DDevice9_GetRenderState(g_device, D3DRS_FOGCOLOR, &fog_col)) ||
            FAILED(IDirect3DDevice9_GetRenderState(g_device, D3DRS_FOGSTART, &fog_start_out)) ||
            FAILED(IDirect3DDevice9_GetRenderState(g_device, D3DRS_FOGEND, &fog_end_out)) ||
            FAILED(IDirect3DDevice9_GetRenderState(g_device, D3DRS_ALPHATESTENABLE, &alpha_en)) ||
            FAILED(IDirect3DDevice9_GetRenderState(g_device, D3DRS_ALPHAFUNC, &alpha_func)) ||
            FAILED(IDirect3DDevice9_GetRenderState(g_device, D3DRS_ALPHAREF, &alpha_ref)) ||
            FAILED(IDirect3DDevice9_GetRenderState(g_device, D3DRS_SCISSORTESTENABLE, &scissor_en))) {
            ExitProcess(174);
        }
        if (fog_en != TRUE || fog_col != D3DCOLOR_XRGB(0, 0, 255) ||
            fog_start_out != fog_start.d || fog_end_out != fog_end.d ||
            alpha_en != TRUE || alpha_func != D3DCMP_GREATER || alpha_ref != 0x40 ||
            scissor_en != TRUE) {
            ExitProcess(175);
        }
        // Reset the fragment gates so the resting frame is unaffected.
        if (FAILED(IDirect3DDevice9_SetRenderState(g_device, D3DRS_FOGENABLE, FALSE)) ||
            FAILED(IDirect3DDevice9_SetRenderState(g_device, D3DRS_ALPHATESTENABLE, FALSE)) ||
            FAILED(IDirect3DDevice9_SetRenderState(g_device, D3DRS_SCISSORTESTENABLE, FALSE))) {
            ExitProcess(176);
        }
    }
    {
        // ── L3 transforms selftest ──
        // MultiplyTransform concatenates in D3D9's row-vector convention
        // (current × M); the world starts at identity, so the stored world
        // must become M exactly. GetTransform round-trips bit-exact.
        D3DMATRIX m;
        m._11 = 2.0f; m._12 = 0.0f; m._13 = 0.0f; m._14 = 0.0f;
        m._21 = 0.0f; m._22 = 3.0f; m._23 = 0.0f; m._24 = 0.0f;
        m._31 = 0.0f; m._32 = 0.0f; m._33 = 0.5f; m._34 = 0.0f;
        m._41 = 0.0f; m._42 = 0.0f; m._43 = 0.0f; m._44 = 1.0f;
        HRESULT hr = IDirect3DDevice9_MultiplyTransform(g_device, D3DTS_WORLD, &m);
        if (FAILED(hr)) {
            ExitProcess(177);
        }
        D3DMATRIX got;
        hr = IDirect3DDevice9_GetTransform(g_device, D3DTS_WORLD, &got);
        if (FAILED(hr) || !matrix_equal(&got, &m)) {
            ExitProcess(178);
        }
        // Restore the identity world so the frame's transforms are unchanged.
        D3DMATRIX id;
        id._11 = 1.0f; id._12 = 0.0f; id._13 = 0.0f; id._14 = 0.0f;
        id._21 = 0.0f; id._22 = 1.0f; id._23 = 0.0f; id._24 = 0.0f;
        id._31 = 0.0f; id._32 = 0.0f; id._33 = 1.0f; id._34 = 0.0f;
        id._41 = 0.0f; id._42 = 0.0f; id._43 = 0.0f; id._44 = 1.0f;
        hr = IDirect3DDevice9_SetTransform(g_device, D3DTS_WORLD, &id);
        if (FAILED(hr)) {
            ExitProcess(179);
        }
        // The projection matrix (set at startup) round-trips exactly.
        hr = IDirect3DDevice9_GetTransform(g_device, D3DTS_PROJECTION, &got);
        if (FAILED(hr) || !matrix_equal(&got, &g_ortho)) {
            ExitProcess(180);
        }
        // D3DTS_TEXTURE0 stores + round-trips (the texgen is a later slice).
        hr = IDirect3DDevice9_SetTransform(g_device, D3DTS_TEXTURE0, &m);
        if (FAILED(hr)) {
            ExitProcess(181);
        }
        hr = IDirect3DDevice9_GetTransform(g_device, D3DTS_TEXTURE0, &got);
        if (FAILED(hr) || !matrix_equal(&got, &m)) {
            ExitProcess(182);
        }
    }
    {
        // P4b: the 2x2 checkerboard texture + stage-0 binding.
        int rc = setup_texture();
        if (rc != 0) {
            ExitProcess(rc);
        }
    }
    {
        // P4c: a backbuffer-sized depth-stencil surface, bound and cleared to
        // the far plane each frame (D3DCLEAR_ZBUFFER in render_frame).
        HRESULT hr = IDirect3DDevice9_CreateDepthStencilSurface(
            g_device, BACKBUFFER_W, BACKBUFFER_H, D3DFMT_D16,
            D3DMULTISAMPLE_NONE, 0, FALSE, &g_depth, NULL);
        if (FAILED(hr) || g_depth == NULL) {
            ExitProcess(145);
        }
        hr = IDirect3DDevice9_SetDepthStencilSurface(g_device, g_depth);
        if (FAILED(hr)) {
            ExitProcess(146);
        }
    }

    if (SetTimer(g_hwnd, TIMER_ID, 50, NULL) == 0) {
        ExitProcess(122);
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
