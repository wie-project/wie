//! Dense fake-API virtual addresses: IDs encoded in the guest VA.
//!
//! Layout (relative to [`FAKE_API_BASE`], stride 16 bytes):
//!
//! ```text
//! offset bits:
//!   [21:20] kind   (2 bits)
//!   [19:4]  payload (16 bits)
//!   [3:0]   0 (alignment)
//! ```
//!
//! | kind | payload |
//! |------|---------|
//! | 0 Export | `WinApiId` as u16 |
//! | 1 Com | `(iface << 8) \| method` |
//! | 2 Special | runtime special id |
//! | 3 Soft | `0x0000..0x7FFF` = alias of `WinApiId`; `0x8000..` = unresolved index |

use std::borrow::Cow;

use crate::WinApiId;

/// Guest base of the fake-API hook window (matches runtime layout).
pub const FAKE_API_BASE: u64 = 0x0000_7000_0000_0000;

/// Size of the mapped fake-API window (4 MiB — room for kind/payload encoding).
pub const FAKE_API_SIZE: usize = 0x0040_0000;

const ALIGN_SHIFT: u32 = 4;
const PAYLOAD_BITS: u32 = 16;
const KIND_SHIFT: u32 = ALIGN_SHIFT + PAYLOAD_BITS; // 20
const PAYLOAD_MASK: u64 = (1 << PAYLOAD_BITS) - 1;
const KIND_MASK: u64 = 0b11;

/// Primary WinAPI export (`WinApiId` in payload).
pub const KIND_EXPORT: u8 = 0;
/// COM / vtable method (`iface` high byte, `method` low byte).
pub const KIND_COM: u8 = 1;
/// Runtime special (callback trampoline, …).
pub const KIND_SPECIAL: u8 = 2;
/// Alias of a `WinApiId` (host fallback) or soft/unresolved slot.
pub const KIND_SOFT: u8 = 3;

/// Soft payloads below this are `WinApiId` aliases; at/above are unresolved indices.
pub const SOFT_UNRESOLVED_BASE: u16 = 0x8000;

/// `kind=Special` payload: USER32 guest WndProc return trampoline.
pub const SPECIAL_CALLBACK_RETURN: u16 = 0;

/// `kind=Special` payload: SEH / C++ EH cleanup + catch-funclet continuation.
pub const SPECIAL_SEH_CONTINUE: u16 = 1;

/// COM interface identity (`kind=Com` high payload byte — ABI: guests call
/// through real vtable positions, so the mapping must not shift).
///
/// The variant order is the ABI (0..3, enforced by the round-trip test);
/// `Unknown` keeps a raw byte addressable for VAs whose iface byte is not one
/// of the four D3D9 interfaces (the old decode kept them decodable).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum D3d9Iface {
    /// `IDirect3D9` (ABI 0).
    Direct3D9,
    /// `IDirect3DDevice9` (ABI 1).
    Device9,
    /// `IDirect3DTexture9` (ABI 2).
    Texture9,
    /// `IDirect3DSurface9` (ABI 3).
    Surface9,
    /// `IDirect3DPixelShader9` (ABI 4).
    PixelShader9,
    /// `IDirect3DVertexShader9` (ABI 5).
    VertexShader9,
    /// Not one of the D3D9 interfaces (raw byte preserved).
    Unknown(u8),
}

impl D3d9Iface {
    /// The real D3D9 interfaces.
    pub const ALL: [Self; 6] = [
        Self::Direct3D9,
        Self::Device9,
        Self::Texture9,
        Self::Surface9,
        Self::PixelShader9,
        Self::VertexShader9,
    ];

    /// Decode the ABI iface byte (never fails — unknown bytes stay decodable).
    #[must_use]
    pub const fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::Direct3D9,
            1 => Self::Device9,
            2 => Self::Texture9,
            3 => Self::Surface9,
            4 => Self::PixelShader9,
            5 => Self::VertexShader9,
            _ => Self::Unknown(v),
        }
    }

    /// The raw ABI iface byte (re-encodes identically).
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        match self {
            Self::Direct3D9 => 0,
            Self::Device9 => 1,
            Self::Texture9 => 2,
            Self::Surface9 => 3,
            Self::PixelShader9 => 4,
            Self::VertexShader9 => 5,
            Self::Unknown(v) => v,
        }
    }
}

/// `IDirect3D9` vtable method. The slot is ABI (a real vtable position), so
/// the discriminants must not shift. Unmodeled slots are carried by
/// [`ComMethod::Unknown`].
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direct3D9Method {
    QueryInterface = 0,
    AddRef = 1,
    Release = 2,
    RegisterSoftwareDevice = 3,
    GetAdapterCount = 4,
    GetAdapterIdentifier = 5,
    GetAdapterModeCount = 6,
    EnumAdapterModes = 7,
    GetAdapterDisplayMode = 8,
    CheckDeviceType = 9,
    CheckDeviceFormat = 10,
    CheckDeviceMultiSampleType = 11,
    CheckDepthStencilMatch = 12,
    CheckDeviceFormatConversion = 13,
    GetDeviceCaps = 14,
    GetAdapterMonitor = 15,
    CreateDevice = 16,
}

impl Direct3D9Method {
    /// Total `IDirect3D9` vtable slots (the fake vtable fills every position).
    pub const VTABLE_SLOTS: usize = 17;

    /// Decode a vtable slot; `None` for unmodeled slots.
    #[must_use]
    pub const fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::QueryInterface),
            1 => Some(Self::AddRef),
            2 => Some(Self::Release),
            3 => Some(Self::RegisterSoftwareDevice),
            4 => Some(Self::GetAdapterCount),
            5 => Some(Self::GetAdapterIdentifier),
            6 => Some(Self::GetAdapterModeCount),
            7 => Some(Self::EnumAdapterModes),
            8 => Some(Self::GetAdapterDisplayMode),
            9 => Some(Self::CheckDeviceType),
            10 => Some(Self::CheckDeviceFormat),
            11 => Some(Self::CheckDeviceMultiSampleType),
            12 => Some(Self::CheckDepthStencilMatch),
            13 => Some(Self::CheckDeviceFormatConversion),
            14 => Some(Self::GetDeviceCaps),
            15 => Some(Self::GetAdapterMonitor),
            16 => Some(Self::CreateDevice),
            _ => None,
        }
    }

    /// The raw vtable slot byte (`#[repr(u8)]` — the discriminant IS the
    /// slot, so this cannot drift from the explicit discriminants).
    #[must_use]
    #[allow(clippy::as_conversions)] // repr(u8) discriminant is the ABI slot
    pub const fn slot(self) -> u8 {
        self as u8
    }

    /// Trace name (`IDirect3D9::Xxx`).
    #[must_use]
    pub fn name(self) -> Cow<'static, str> {
        match self {
            Self::QueryInterface => Cow::Borrowed("IDirect3D9::QueryInterface"),
            Self::AddRef => Cow::Borrowed("IDirect3D9::AddRef"),
            Self::Release => Cow::Borrowed("IDirect3D9::Release"),
            Self::RegisterSoftwareDevice => Cow::Borrowed("IDirect3D9::RegisterSoftwareDevice"),
            Self::GetAdapterCount => Cow::Borrowed("IDirect3D9::GetAdapterCount"),
            Self::GetAdapterIdentifier => Cow::Borrowed("IDirect3D9::GetAdapterIdentifier"),
            Self::GetAdapterModeCount => Cow::Borrowed("IDirect3D9::GetAdapterModeCount"),
            Self::EnumAdapterModes => Cow::Borrowed("IDirect3D9::EnumAdapterModes"),
            Self::GetAdapterDisplayMode => Cow::Borrowed("IDirect3D9::GetAdapterDisplayMode"),
            Self::CheckDeviceType => Cow::Borrowed("IDirect3D9::CheckDeviceType"),
            Self::CheckDeviceFormat => Cow::Borrowed("IDirect3D9::CheckDeviceFormat"),
            Self::CheckDeviceMultiSampleType => {
                Cow::Borrowed("IDirect3D9::CheckDeviceMultiSampleType")
            }
            Self::CheckDepthStencilMatch => Cow::Borrowed("IDirect3D9::CheckDepthStencilMatch"),
            Self::CheckDeviceFormatConversion => {
                Cow::Borrowed("IDirect3D9::CheckDeviceFormatConversion")
            }
            Self::GetDeviceCaps => Cow::Borrowed("IDirect3D9::GetDeviceCaps"),
            Self::GetAdapterMonitor => Cow::Borrowed("IDirect3D9::GetAdapterMonitor"),
            Self::CreateDevice => Cow::Borrowed("IDirect3D9::CreateDevice"),
        }
    }
}

/// `IDirect3DDevice9` vtable method (ABI slot positions; unmodeled slots are
/// carried by [`ComMethod::Unknown`]).
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Device9Method {
    Release = 2,
    Present = 17,
    CreateTexture = 23,
    CreateVertexBuffer = 26,
    CreateIndexBuffer = 27,
    CreateDepthStencilSurface = 29,
    SetDepthStencilSurface = 39,
    GetDepthStencilSurface = 40,
    BeginScene = 41,
    EndScene = 42,
    Clear = 43,
    SetTransform = 44,
    SetViewport = 47,
    GetViewport = 48,
    SetRenderState = 57,
    GetRenderState = 58,
    GetTexture = 64,
    SetTexture = 65,
    GetTextureStageState = 66,
    SetTextureStageState = 67,
    GetSamplerState = 68,
    SetSamplerState = 69,
    DrawPrimitive = 81,
    DrawIndexedPrimitive = 82,
    DrawPrimitiveUp = 83,
    DrawIndexedPrimitiveUp = 84,
    SetFvf = 89,
    // ── P5a shader methods (slots verified against d3d9.h) ──────────────
    CreateVertexShader = 91,
    SetVertexShader = 92,
    GetVertexShader = 93,
    SetVertexShaderConstantF = 94,
    GetVertexShaderConstantF = 95,
    SetStreamSource = 100,
    SetIndices = 104,
    CreatePixelShader = 106,
    SetPixelShader = 107,
    GetPixelShader = 108,
    SetPixelShaderConstantF = 109,
    GetPixelShaderConstantF = 110,
}

impl Device9Method {
    /// Total `IDirect3DDevice9` vtable slots.
    pub const VTABLE_SLOTS: usize = 119;

    /// Decode a vtable slot; `None` for unmodeled slots.
    #[must_use]
    pub const fn from_u8(v: u8) -> Option<Self> {
        match v {
            2 => Some(Self::Release),
            17 => Some(Self::Present),
            23 => Some(Self::CreateTexture),
            26 => Some(Self::CreateVertexBuffer),
            27 => Some(Self::CreateIndexBuffer),
            29 => Some(Self::CreateDepthStencilSurface),
            39 => Some(Self::SetDepthStencilSurface),
            40 => Some(Self::GetDepthStencilSurface),
            41 => Some(Self::BeginScene),
            42 => Some(Self::EndScene),
            43 => Some(Self::Clear),
            44 => Some(Self::SetTransform),
            47 => Some(Self::SetViewport),
            48 => Some(Self::GetViewport),
            57 => Some(Self::SetRenderState),
            58 => Some(Self::GetRenderState),
            64 => Some(Self::GetTexture),
            65 => Some(Self::SetTexture),
            66 => Some(Self::GetTextureStageState),
            67 => Some(Self::SetTextureStageState),
            68 => Some(Self::GetSamplerState),
            69 => Some(Self::SetSamplerState),
            81 => Some(Self::DrawPrimitive),
            82 => Some(Self::DrawIndexedPrimitive),
            83 => Some(Self::DrawPrimitiveUp),
            84 => Some(Self::DrawIndexedPrimitiveUp),
            89 => Some(Self::SetFvf),
            91 => Some(Self::CreateVertexShader),
            92 => Some(Self::SetVertexShader),
            93 => Some(Self::GetVertexShader),
            94 => Some(Self::SetVertexShaderConstantF),
            95 => Some(Self::GetVertexShaderConstantF),
            100 => Some(Self::SetStreamSource),
            104 => Some(Self::SetIndices),
            106 => Some(Self::CreatePixelShader),
            107 => Some(Self::SetPixelShader),
            108 => Some(Self::GetPixelShader),
            109 => Some(Self::SetPixelShaderConstantF),
            110 => Some(Self::GetPixelShaderConstantF),
            _ => None,
        }
    }

    /// The raw vtable slot byte (`#[repr(u8)]` — the discriminant IS the
    /// slot, so this cannot drift from the explicit discriminants).
    #[must_use]
    #[allow(clippy::as_conversions)] // repr(u8) discriminant is the ABI slot
    pub const fn slot(self) -> u8 {
        self as u8
    }

    /// Trace name (`IDirect3DDevice9::Xxx`).
    #[must_use]
    pub fn name(self) -> Cow<'static, str> {
        match self {
            Self::Release => Cow::Borrowed("IDirect3DDevice9::Release"),
            Self::Present => Cow::Borrowed("IDirect3DDevice9::Present"),
            Self::CreateTexture => Cow::Borrowed("IDirect3DDevice9::CreateTexture"),
            Self::CreateVertexBuffer => Cow::Borrowed("IDirect3DDevice9::CreateVertexBuffer"),
            Self::CreateIndexBuffer => Cow::Borrowed("IDirect3DDevice9::CreateIndexBuffer"),
            Self::CreateDepthStencilSurface => {
                Cow::Borrowed("IDirect3DDevice9::CreateDepthStencilSurface")
            }
            Self::SetDepthStencilSurface => {
                Cow::Borrowed("IDirect3DDevice9::SetDepthStencilSurface")
            }
            Self::GetDepthStencilSurface => {
                Cow::Borrowed("IDirect3DDevice9::GetDepthStencilSurface")
            }
            Self::BeginScene => Cow::Borrowed("IDirect3DDevice9::BeginScene"),
            Self::EndScene => Cow::Borrowed("IDirect3DDevice9::EndScene"),
            Self::Clear => Cow::Borrowed("IDirect3DDevice9::Clear"),
            Self::SetTransform => Cow::Borrowed("IDirect3DDevice9::SetTransform"),
            Self::SetViewport => Cow::Borrowed("IDirect3DDevice9::SetViewport"),
            Self::GetViewport => Cow::Borrowed("IDirect3DDevice9::GetViewport"),
            Self::SetRenderState => Cow::Borrowed("IDirect3DDevice9::SetRenderState"),
            Self::GetRenderState => Cow::Borrowed("IDirect3DDevice9::GetRenderState"),
            Self::GetTexture => Cow::Borrowed("IDirect3DDevice9::GetTexture"),
            Self::SetTexture => Cow::Borrowed("IDirect3DDevice9::SetTexture"),
            Self::GetTextureStageState => Cow::Borrowed("IDirect3DDevice9::GetTextureStageState"),
            Self::SetTextureStageState => Cow::Borrowed("IDirect3DDevice9::SetTextureStageState"),
            Self::GetSamplerState => Cow::Borrowed("IDirect3DDevice9::GetSamplerState"),
            Self::SetSamplerState => Cow::Borrowed("IDirect3DDevice9::SetSamplerState"),
            Self::DrawPrimitive => Cow::Borrowed("IDirect3DDevice9::DrawPrimitive"),
            Self::DrawIndexedPrimitive => Cow::Borrowed("IDirect3DDevice9::DrawIndexedPrimitive"),
            Self::DrawPrimitiveUp => Cow::Borrowed("IDirect3DDevice9::DrawPrimitiveUP"),
            Self::DrawIndexedPrimitiveUp => {
                Cow::Borrowed("IDirect3DDevice9::DrawIndexedPrimitiveUP")
            }
            Self::SetFvf => Cow::Borrowed("IDirect3DDevice9::SetFVF"),
            Self::CreateVertexShader => Cow::Borrowed("IDirect3DDevice9::CreateVertexShader"),
            Self::SetVertexShader => Cow::Borrowed("IDirect3DDevice9::SetVertexShader"),
            Self::GetVertexShader => Cow::Borrowed("IDirect3DDevice9::GetVertexShader"),
            Self::SetVertexShaderConstantF => {
                Cow::Borrowed("IDirect3DDevice9::SetVertexShaderConstantF")
            }
            Self::GetVertexShaderConstantF => {
                Cow::Borrowed("IDirect3DDevice9::GetVertexShaderConstantF")
            }
            Self::SetStreamSource => Cow::Borrowed("IDirect3DDevice9::SetStreamSource"),
            Self::SetIndices => Cow::Borrowed("IDirect3DDevice9::SetIndices"),
            Self::CreatePixelShader => Cow::Borrowed("IDirect3DDevice9::CreatePixelShader"),
            Self::SetPixelShader => Cow::Borrowed("IDirect3DDevice9::SetPixelShader"),
            Self::GetPixelShader => Cow::Borrowed("IDirect3DDevice9::GetPixelShader"),
            Self::SetPixelShaderConstantF => {
                Cow::Borrowed("IDirect3DDevice9::SetPixelShaderConstantF")
            }
            Self::GetPixelShaderConstantF => {
                Cow::Borrowed("IDirect3DDevice9::GetPixelShaderConstantF")
            }
        }
    }
}

/// `IDirect3DTexture9` vtable method (ABI slot positions; unmodeled slots are
/// carried by [`ComMethod::Unknown`]).
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Texture9Method {
    Release = 2,
    GetLevelCount = 13,
    GetLevelDesc = 17,
    GetSurfaceLevel = 18,
    LockRect = 19,
    UnlockRect = 20,
}

impl Texture9Method {
    /// Total `IDirect3DTexture9` vtable slots.
    pub const VTABLE_SLOTS: usize = 22;

    /// Decode a vtable slot; `None` for unmodeled slots.
    #[must_use]
    pub const fn from_u8(v: u8) -> Option<Self> {
        match v {
            2 => Some(Self::Release),
            13 => Some(Self::GetLevelCount),
            17 => Some(Self::GetLevelDesc),
            18 => Some(Self::GetSurfaceLevel),
            19 => Some(Self::LockRect),
            20 => Some(Self::UnlockRect),
            _ => None,
        }
    }

    /// The raw vtable slot byte (`#[repr(u8)]` — the discriminant IS the
    /// slot, so this cannot drift from the explicit discriminants).
    #[must_use]
    #[allow(clippy::as_conversions)] // repr(u8) discriminant is the ABI slot
    pub const fn slot(self) -> u8 {
        self as u8
    }

    /// Trace name (`IDirect3DTexture9::Xxx`).
    #[must_use]
    pub fn name(self) -> Cow<'static, str> {
        match self {
            Self::Release => Cow::Borrowed("IDirect3DTexture9::Release"),
            Self::GetLevelCount => Cow::Borrowed("IDirect3DTexture9::GetLevelCount"),
            Self::GetLevelDesc => Cow::Borrowed("IDirect3DTexture9::GetLevelDesc"),
            Self::GetSurfaceLevel => Cow::Borrowed("IDirect3DTexture9::GetSurfaceLevel"),
            Self::LockRect => Cow::Borrowed("IDirect3DTexture9::LockRect"),
            Self::UnlockRect => Cow::Borrowed("IDirect3DTexture9::UnlockRect"),
        }
    }
}

/// `IDirect3DSurface9` vtable method (ABI slot positions; unmodeled slots are
/// carried by [`ComMethod::Unknown`]).
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Surface9Method {
    Release = 2,
    GetDesc = 12,
    LockRect = 13,
    UnlockRect = 14,
}

impl Surface9Method {
    /// Total `IDirect3DSurface9` vtable slots.
    pub const VTABLE_SLOTS: usize = 18;

    /// Decode a vtable slot; `None` for unmodeled slots.
    #[must_use]
    pub const fn from_u8(v: u8) -> Option<Self> {
        match v {
            2 => Some(Self::Release),
            12 => Some(Self::GetDesc),
            13 => Some(Self::LockRect),
            14 => Some(Self::UnlockRect),
            _ => None,
        }
    }

    /// The raw vtable slot byte (`#[repr(u8)]` — the discriminant IS the
    /// slot, so this cannot drift from the explicit discriminants).
    #[must_use]
    #[allow(clippy::as_conversions)] // repr(u8) discriminant is the ABI slot
    pub const fn slot(self) -> u8 {
        self as u8
    }

    /// Trace name (`IDirect3DSurface9::Xxx`).
    #[must_use]
    pub fn name(self) -> Cow<'static, str> {
        match self {
            Self::Release => Cow::Borrowed("IDirect3DSurface9::Release"),
            Self::GetDesc => Cow::Borrowed("IDirect3DSurface9::GetDesc"),
            Self::LockRect => Cow::Borrowed("IDirect3DSurface9::LockRect"),
            Self::UnlockRect => Cow::Borrowed("IDirect3DSurface9::UnlockRect"),
        }
    }
}

/// `IDirect3DPixelShader9` vtable method. The interface is IUnknown-only, so
/// the vtable is the 3-slot COM trio (P5a: only Release has a handler).
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelShader9Method {
    QueryInterface = 0,
    AddRef = 1,
    Release = 2,
}

impl PixelShader9Method {
    /// Total `IDirect3DPixelShader9` vtable slots.
    pub const VTABLE_SLOTS: usize = 3;

    /// Decode a vtable slot; `None` for unmodeled slots.
    #[must_use]
    pub const fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::QueryInterface),
            1 => Some(Self::AddRef),
            2 => Some(Self::Release),
            _ => None,
        }
    }

    /// The raw vtable slot byte (`#[repr(u8)]` — the discriminant IS the
    /// slot, so this cannot drift from the explicit discriminants).
    #[must_use]
    #[allow(clippy::as_conversions)] // repr(u8) discriminant is the ABI slot
    pub const fn slot(self) -> u8 {
        self as u8
    }

    /// Trace name (`IDirect3DPixelShader9::Xxx`).
    #[must_use]
    pub fn name(self) -> Cow<'static, str> {
        match self {
            Self::QueryInterface => Cow::Borrowed("IDirect3DPixelShader9::QueryInterface"),
            Self::AddRef => Cow::Borrowed("IDirect3DPixelShader9::AddRef"),
            Self::Release => Cow::Borrowed("IDirect3DPixelShader9::Release"),
        }
    }
}

/// `IDirect3DVertexShader9` vtable method. IUnknown-only, like the pixel
/// shader (P5a: only Release has a handler).
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VertexShader9Method {
    QueryInterface = 0,
    AddRef = 1,
    Release = 2,
}

impl VertexShader9Method {
    /// Total `IDirect3DVertexShader9` vtable slots.
    pub const VTABLE_SLOTS: usize = 3;

    /// Decode a vtable slot; `None` for unmodeled slots.
    #[must_use]
    pub const fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::QueryInterface),
            1 => Some(Self::AddRef),
            2 => Some(Self::Release),
            _ => None,
        }
    }

    /// The raw vtable slot byte (`#[repr(u8)]` — the discriminant IS the
    /// slot, so this cannot drift from the explicit discriminants).
    #[must_use]
    #[allow(clippy::as_conversions)] // repr(u8) discriminant is the ABI slot
    pub const fn slot(self) -> u8 {
        self as u8
    }

    /// Trace name (`IDirect3DVertexShader9::Xxx`).
    #[must_use]
    pub fn name(self) -> Cow<'static, str> {
        match self {
            Self::QueryInterface => Cow::Borrowed("IDirect3DVertexShader9::QueryInterface"),
            Self::AddRef => Cow::Borrowed("IDirect3DVertexShader9::AddRef"),
            Self::Release => Cow::Borrowed("IDirect3DVertexShader9::Release"),
        }
    }
}

/// A COM vtable method decoded against its interface (the `kind=Com` low
/// payload byte).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComMethod {
    Direct3D9(Direct3D9Method),
    Device9(Device9Method),
    Texture9(Texture9Method),
    Surface9(Surface9Method),
    PixelShader9(PixelShader9Method),
    VertexShader9(VertexShader9Method),
    /// Unmodeled slot (raw byte preserved).
    Unknown(u8),
}

impl ComMethod {
    /// Decode a method slot for `iface` (never fails — unknown slots fall
    /// back to [`Self::Unknown`], preserving the raw byte).
    #[must_use]
    pub const fn decode(iface: D3d9Iface, slot: u8) -> Self {
        match iface {
            D3d9Iface::Direct3D9 => match Direct3D9Method::from_u8(slot) {
                Some(m) => Self::Direct3D9(m),
                None => Self::Unknown(slot),
            },
            D3d9Iface::Device9 => match Device9Method::from_u8(slot) {
                Some(m) => Self::Device9(m),
                None => Self::Unknown(slot),
            },
            D3d9Iface::Texture9 => match Texture9Method::from_u8(slot) {
                Some(m) => Self::Texture9(m),
                None => Self::Unknown(slot),
            },
            D3d9Iface::Surface9 => match Surface9Method::from_u8(slot) {
                Some(m) => Self::Surface9(m),
                None => Self::Unknown(slot),
            },
            D3d9Iface::PixelShader9 => match PixelShader9Method::from_u8(slot) {
                Some(m) => Self::PixelShader9(m),
                None => Self::Unknown(slot),
            },
            D3d9Iface::VertexShader9 => match VertexShader9Method::from_u8(slot) {
                Some(m) => Self::VertexShader9(m),
                None => Self::Unknown(slot),
            },
            D3d9Iface::Unknown(_) => Self::Unknown(slot),
        }
    }

    /// The raw method slot byte.
    #[must_use]
    pub const fn slot(self) -> u8 {
        match self {
            Self::Direct3D9(m) => m.slot(),
            Self::Device9(m) => m.slot(),
            Self::Texture9(m) => m.slot(),
            Self::Surface9(m) => m.slot(),
            Self::PixelShader9(m) => m.slot(),
            Self::VertexShader9(m) => m.slot(),
            Self::Unknown(v) => v,
        }
    }

    /// Trace name. Known methods carry their interface prefix; unknown slots
    /// keep the legacy `IDirect3D*::SlotNNN` string (which also drives the
    /// name-table lookup — an unknown name resolves to no handler, as before);
    /// unknown interfaces keep the legacy `Com{iface}::Method{slot}` string.
    #[must_use]
    pub fn name(self, iface: D3d9Iface) -> Cow<'static, str> {
        match self {
            Self::Direct3D9(m) => m.name(),
            Self::Device9(m) => m.name(),
            Self::Texture9(m) => m.name(),
            Self::Surface9(m) => m.name(),
            Self::PixelShader9(m) => m.name(),
            Self::VertexShader9(m) => m.name(),
            Self::Unknown(v) => match iface {
                D3d9Iface::Direct3D9 => Cow::Owned(format!("IDirect3D9::Slot{v:03}")),
                D3d9Iface::Device9 => Cow::Owned(format!("IDirect3DDevice9::Slot{v:03}")),
                D3d9Iface::Texture9 => Cow::Owned(format!("IDirect3DTexture9::Slot{v:03}")),
                D3d9Iface::Surface9 => Cow::Owned(format!("IDirect3DSurface9::Slot{v:03}")),
                D3d9Iface::PixelShader9 => Cow::Owned(format!("IDirect3DPixelShader9::Slot{v:03}")),
                D3d9Iface::VertexShader9 => {
                    Cow::Owned(format!("IDirect3DVertexShader9::Slot{v:03}"))
                }
                D3d9Iface::Unknown(_) => Cow::Owned(format!("Com{}::Method{v}", iface.as_u8())),
            },
        }
    }
}

/// Decoded fake-API address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FakeVa {
    /// Dense handler id.
    Export(WinApiId),
    /// Host-fallback / alias VA that dispatches the same `WinApiId`.
    Alias(WinApiId),
    /// Import without a dense id (UCRT, ordinals, …); index into soft table.
    Unresolved(u16),
    /// COM vtable slot.
    Com { iface: D3d9Iface, method: ComMethod },
    /// Runtime special.
    Special(u16),
}

/// Pack `kind` + `payload` into a guest fake VA.
#[must_use]
#[allow(clippy::as_conversions)] // const fn — From is not const-stable yet
pub const fn encode(kind: u8, payload: u16) -> u64 {
    FAKE_API_BASE | ((kind as u64) << KIND_SHIFT) | ((payload as u64) << ALIGN_SHIFT)
}

/// Encode a primary export address for `id`.
#[must_use]
pub const fn encode_export(id: WinApiId) -> u64 {
    encode(KIND_EXPORT, id.to_u16())
}

/// Encode a host-fallback alias that dispatches the same `id`.
#[must_use]
pub const fn encode_alias(id: WinApiId) -> u64 {
    encode(KIND_SOFT, id.to_u16())
}

/// Encode a soft/unresolved slot (`index` must be `< 0x8000`).
#[must_use]
pub const fn encode_unresolved(index: u16) -> u64 {
    encode(KIND_SOFT, SOFT_UNRESOLVED_BASE | (index & 0x7fff))
}

/// Encode a COM method address.
#[must_use]
#[allow(clippy::as_conversions)] // const fn — From is not const-stable yet
pub const fn encode_com(iface: D3d9Iface, method: u8) -> u64 {
    encode(KIND_COM, ((iface.as_u8() as u16) << 8) | (method as u16))
}

/// Encode a runtime special address.
#[must_use]
pub const fn encode_special(id: u16) -> u64 {
    encode(KIND_SPECIAL, id)
}

/// Callback-return trampoline VA (inside the fake-API window).
#[must_use]
pub const fn callback_return_trampoline_va() -> u64 {
    encode_special(SPECIAL_CALLBACK_RETURN)
}

/// SEH continuation trampoline VA (UnwindMap actions / MSVC catch funclets).
#[must_use]
pub const fn seh_continue_trampoline_va() -> u64 {
    encode_special(SPECIAL_SEH_CONTINUE)
}

/// Decode a guest VA into a [`FakeVa`], if it lies in the fake-API window.
#[must_use]
#[allow(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::arithmetic_side_effects
)] // bitfield decode: checked window, then truncating payload/kind extracts
pub fn decode(va: u64) -> Option<FakeVa> {
    if va < FAKE_API_BASE {
        return None;
    }
    let off = va - FAKE_API_BASE;
    let window = FAKE_API_SIZE as u64;
    if off >= window {
        return None;
    }
    // Require 16-byte alignment (IAT / stub stride).
    if off & ((1 << ALIGN_SHIFT) - 1) != 0 {
        return None;
    }

    let payload = ((off >> ALIGN_SHIFT) & PAYLOAD_MASK) as u16;
    let kind = ((off >> KIND_SHIFT) & KIND_MASK) as u8;

    match kind {
        KIND_EXPORT => {
            let id = WinApiId::from_u16(payload)?;
            Some(FakeVa::Export(id))
        }
        KIND_COM => {
            let iface = D3d9Iface::from_u8((payload >> 8) as u8);
            let method = (payload & 0xFF) as u8;
            Some(FakeVa::Com {
                iface,
                method: ComMethod::decode(iface, method),
            })
        }
        KIND_SPECIAL => Some(FakeVa::Special(payload)),
        KIND_SOFT => {
            if payload < SOFT_UNRESOLVED_BASE {
                let id = WinApiId::from_u16(payload)?;
                Some(FakeVa::Alias(id))
            } else {
                Some(FakeVa::Unresolved(
                    payload.wrapping_sub(SOFT_UNRESOLVED_BASE),
                ))
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[allow(clippy::as_conversions)] // test: FAKE_API_SIZE usize → u64 window bound
    fn export_round_trip() {
        let id = WinApiId::Kernel32Getlasterror;
        let va = encode_export(id);
        assert_eq!(decode(va), Some(FakeVa::Export(id)));
        assert!(va >= FAKE_API_BASE);
        assert!((va - FAKE_API_BASE) < FAKE_API_SIZE as u64);
        assert_eq!(va & 0xf, 0);
    }

    #[test]
    fn alias_and_unresolved_distinct() {
        let id = WinApiId::Kernel32Readfile;
        let a = encode_alias(id);
        let u = encode_unresolved(3);
        assert_ne!(a, u);
        assert_eq!(decode(a), Some(FakeVa::Alias(id)));
        assert_eq!(decode(u), Some(FakeVa::Unresolved(3)));
    }

    #[test]
    fn com_and_special() {
        let va = encode_com(D3d9Iface::Device9, 57);
        assert_eq!(
            decode(va),
            Some(FakeVa::Com {
                iface: D3d9Iface::Device9,
                method: ComMethod::Device9(Device9Method::SetRenderState)
            })
        );
        let cb = callback_return_trampoline_va();
        assert_eq!(decode(cb), Some(FakeVa::Special(SPECIAL_CALLBACK_RETURN)));
    }

    #[test]
    fn com_typed_decode_round_trip() {
        // Known iface + known slot decodes to the typed method and back.
        let va = encode_com(D3d9Iface::Direct3D9, 16);
        assert_eq!(
            decode(va),
            Some(FakeVa::Com {
                iface: D3d9Iface::Direct3D9,
                method: ComMethod::Direct3D9(Direct3D9Method::CreateDevice)
            })
        );
        let encoded = encode_com(D3d9Iface::Texture9, 20);
        assert_eq!(
            decode(encoded),
            Some(FakeVa::Com {
                iface: D3d9Iface::Texture9,
                method: ComMethod::Texture9(Texture9Method::UnlockRect)
            })
        );
    }

    #[test]
    fn com_unknown_iface_and_slot_fall_back() {
        // Unknown iface byte: the raw bytes survive (legacy "Com{x}::Method{y}").
        // Byte 9 is outside the six D3D9 interfaces (0..5).
        let va = encode_com(D3d9Iface::Unknown(9), 3);
        assert_eq!(
            decode(va),
            Some(FakeVa::Com {
                iface: D3d9Iface::Unknown(9),
                method: ComMethod::Unknown(3)
            })
        );
        // Unknown slot on a known iface: the raw slot survives
        // (legacy "IDirect3DDevice9::SlotNNN" path).
        let va = encode_com(D3d9Iface::Device9, 250);
        assert_eq!(
            decode(va),
            Some(FakeVa::Com {
                iface: D3d9Iface::Device9,
                method: ComMethod::Unknown(250)
            })
        );
    }

    #[test]
    fn every_slot_round_trips_through_decode() {
        // The slot byte is ABI — every byte must survive a decode(encode())
        // round-trip for every interface, known or not.
        for iface in D3d9Iface::ALL {
            for slot in 0..=u8::MAX {
                let method = ComMethod::decode(iface, slot);
                assert_eq!(method.slot(), slot, "slot must round-trip for {iface:?}");
            }
        }
        // Unknown iface bytes keep their slot byte too.
        for slot in 0..=u8::MAX {
            let method = ComMethod::decode(D3d9Iface::Unknown(9), slot);
            assert_eq!(method.slot(), slot);
        }
        assert_eq!(D3d9Iface::Unknown(9).as_u8(), 9);
        // P5a iface bytes 4 and 5 decode to the shader interfaces.
        assert_eq!(D3d9Iface::from_u8(4), D3d9Iface::PixelShader9);
        assert_eq!(D3d9Iface::from_u8(5), D3d9Iface::VertexShader9);
    }

    #[test]
    fn iface_and_method_slots_are_the_abi_mapping() {
        // The ABI mapping is explicit and locked: iface bytes 0..5 decode to
        // the six interfaces in order, and every modeled method slot is its
        // real vtable position.
        assert_eq!(D3d9Iface::from_u8(0), D3d9Iface::Direct3D9);
        assert_eq!(D3d9Iface::from_u8(1), D3d9Iface::Device9);
        assert_eq!(D3d9Iface::from_u8(2), D3d9Iface::Texture9);
        assert_eq!(D3d9Iface::from_u8(3), D3d9Iface::Surface9);
        assert_eq!(D3d9Iface::from_u8(4), D3d9Iface::PixelShader9);
        assert_eq!(D3d9Iface::from_u8(5), D3d9Iface::VertexShader9);
        assert_eq!(D3d9Iface::Direct3D9.as_u8(), 0);
        assert_eq!(D3d9Iface::Device9.as_u8(), 1);
        assert_eq!(D3d9Iface::Texture9.as_u8(), 2);
        assert_eq!(D3d9Iface::Surface9.as_u8(), 3);
        assert_eq!(D3d9Iface::PixelShader9.as_u8(), 4);
        assert_eq!(D3d9Iface::VertexShader9.as_u8(), 5);

        assert_eq!(Direct3D9Method::from_u8(2), Some(Direct3D9Method::Release));
        assert_eq!(
            Direct3D9Method::from_u8(16),
            Some(Direct3D9Method::CreateDevice)
        );
        assert_eq!(Direct3D9Method::Release.slot(), 2);
        assert_eq!(Direct3D9Method::CreateDevice.slot(), 16);

        assert_eq!(Device9Method::from_u8(17), Some(Device9Method::Present));
        assert_eq!(
            Device9Method::from_u8(57),
            Some(Device9Method::SetRenderState)
        );
        assert_eq!(Device9Method::from_u8(104), Some(Device9Method::SetIndices));
        assert_eq!(Device9Method::Present.slot(), 17);
        assert_eq!(Device9Method::SetRenderState.slot(), 57);
        assert_eq!(Device9Method::SetIndices.slot(), 104);
        // P5a device shader methods (slots verified against d3d9.h).
        assert_eq!(
            Device9Method::from_u8(91),
            Some(Device9Method::CreateVertexShader)
        );
        assert_eq!(
            Device9Method::from_u8(106),
            Some(Device9Method::CreatePixelShader)
        );
        assert_eq!(
            Device9Method::from_u8(109),
            Some(Device9Method::SetPixelShaderConstantF)
        );
        assert_eq!(Device9Method::CreateVertexShader.slot(), 91);
        assert_eq!(Device9Method::CreatePixelShader.slot(), 106);
        assert_eq!(Device9Method::SetPixelShaderConstantF.slot(), 109);

        assert_eq!(
            Texture9Method::from_u8(18),
            Some(Texture9Method::GetSurfaceLevel)
        );
        assert_eq!(Texture9Method::GetSurfaceLevel.slot(), 18);
        assert_eq!(Surface9Method::from_u8(13), Some(Surface9Method::LockRect));
        assert_eq!(Surface9Method::LockRect.slot(), 13);

        // Unmodeled slots decode to None.
        assert_eq!(Direct3D9Method::from_u8(200), None);
        assert_eq!(Device9Method::from_u8(119), None);
    }

    #[test]
    fn com_name_matches_legacy_dispatch_strings() {
        // The trace names are load-bearing: they drive the D3D9 name-table
        // lookup, so they must match the pre-refactor strings exactly.
        assert_eq!(
            ComMethod::Direct3D9(Direct3D9Method::GetDeviceCaps).name(D3d9Iface::Direct3D9),
            "IDirect3D9::GetDeviceCaps"
        );
        assert_eq!(
            ComMethod::Device9(Device9Method::SetRenderState).name(D3d9Iface::Device9),
            "IDirect3DDevice9::SetRenderState"
        );
        assert_eq!(
            ComMethod::Unknown(250).name(D3d9Iface::Device9),
            "IDirect3DDevice9::Slot250"
        );
        assert_eq!(
            ComMethod::Unknown(3).name(D3d9Iface::Unknown(5)),
            "Com5::Method3"
        );
    }

    #[test]
    #[allow(clippy::as_conversions)] // test: FAKE_API_SIZE usize → u64 window bound
    fn rejects_outside_window() {
        assert!(decode(0x140_000_000).is_none());
        assert!(decode(FAKE_API_BASE + FAKE_API_SIZE as u64).is_none());
    }
}
