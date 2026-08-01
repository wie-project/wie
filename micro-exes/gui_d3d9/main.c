// P3 D3D9 software-render micro-test for WIE.
//
// Exercises the software-render slice 1 (roadmap B6): Direct3DCreate9 →
// GetDeviceCaps (must honestly report NO vertex/pixel shaders) → CreateDevice
// → Clear → BeginScene → DrawPrimitiveUP (XYZ|DIFFUSE gradient triangle) →
// DrawIndexedPrimitiveUP (solid indexed triangle) → EndScene → Present.
// The frame is rendered from WM_PAINT; Present publishes it through the same
// PresentState surface pipeline GDI BitBlt uses.
//
// Self-test (WIE_SELFTEST=1): every D3D9 call's HRESULT is checked, a
// SetViewport/GetViewport round-trip is verified, and after TIMER_TICKS
// WM_TIMER ticks (each invalidating → repaint → represent) the window quits
// with 0. Distinct non-zero codes (101-121) report the first stage that did
// not run. Interactive runs (no WIE_SELFTEST) keep the window open — the
// timer drives nothing and the window quits only on 'q' / close.
//
// The resting frame is deterministic: red clear + two triangles. The CI test
// samples pixels (clear red outside the triangles, blended/diffuse colors
// inside) and asserts the exit code.

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
static int g_selftest;
static int g_timer_count;

static int selftest_enabled(void) {
    char buf[16];
    DWORD n = GetEnvironmentVariableA("WIE_SELFTEST", buf, sizeof(buf));
    return n == 1 && buf[0] == '1';
}

// Render one deterministic frame into the D3D9 backbuffer and present it.
// Returns 0 on success, else the exit code naming the failed stage.
static int render_frame(void) {
    // D3DCOLOR_XRGB(200, 0, 0) — pure red clear, no alpha.
    if (FAILED(IDirect3DDevice9_Clear(g_device, 0, NULL,
                                      D3DCLEAR_TARGET,
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
    // Indexed path: a second triangle (solid cyan) below the first via
    // DrawIndexedPrimitiveUP with 16-bit indices.
    struct Vertex indexed[3];
    indexed[0].x = -60.0f; indexed[0].y = 100.0f; indexed[0].z = 0.0f;
    indexed[0].color = D3DCOLOR_XRGB(0, 255, 255);
    indexed[1].x = 60.0f; indexed[1].y = 100.0f; indexed[1].z = 0.0f;
    indexed[1].color = D3DCOLOR_XRGB(0, 255, 255);
    indexed[2].x = 0.0f; indexed[2].y = 40.0f; indexed[2].z = 0.0f;
    indexed[2].color = D3DCOLOR_XRGB(0, 255, 255);
    {
        unsigned short indices[3] = { 0, 1, 2 };
        if (FAILED(IDirect3DDevice9_DrawIndexedPrimitiveUP(
                       g_device, D3DPT_TRIANGLELIST, 0, 3, 1, indices,
                       D3DFMT_INDEX16, indexed, (UINT)sizeof(indexed[0])))) {
            return 111;
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
        // P3 caps honesty (B6c): no vertex shader, no pixel shader — the
        // game must take the fixed-function path we actually implement.
        D3DCAPS9 caps;
        if (FAILED(IDirect3D9_GetDeviceCaps(g_d3d, 0, D3DDEVTYPE_HAL, &caps))) {
            ExitProcess(103);
        }
        if (caps.VertexShaderVersion != 0 || caps.PixelShaderVersion != 0) {
            ExitProcess(104);
        }
        // MaxVertexShaderConst / PixelShader1xMaxValue must also be zero.
        if (caps.MaxVertexShaderConst != 0 || caps.PixelShader1xMaxValue != 0.0f) {
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
        // Orthographic projection: world x ∈ [-160,160] → NDC [-1,1],
        // world y ∈ [-120,120] → NDC [-1,1] (y-up world, y-down screen).
        D3DMATRIX proj;
        proj._11 = 2.0f / BACKBUFFER_W; proj._12 = 0.0f; proj._13 = 0.0f; proj._14 = 0.0f;
        proj._21 = 0.0f; proj._22 = 2.0f / BACKBUFFER_H; proj._23 = 0.0f; proj._24 = 0.0f;
        proj._31 = 0.0f; proj._32 = 0.0f; proj._33 = 1.0f; proj._34 = 0.0f;
        proj._41 = 0.0f; proj._42 = 0.0f; proj._43 = 0.0f; proj._44 = 1.0f;
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
