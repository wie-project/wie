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

/// Soft payloads below this: `WinApiId` aliases; at/above: unresolved indices.
pub const SOFT_UNRESOLVED_BASE: u16 = 0x8000;

/// `kind=Special` payload: USER32 guest WndProc return trampoline.
pub const SPECIAL_CALLBACK_RETURN: u16 = 0;

/// `kind=Special` payload: SEH / C++ EH cleanup + catch-funclet continuation.
pub const SPECIAL_SEH_CONTINUE: u16 = 1;

/// `kind=Special` payload: static-dependency `DllMain` return trampoline.
///
/// `prepare_dll_main_call` pushes this VA as the return address of every
/// statically-loaded DLL's `DllMain`; the session pump recognizes the stop
/// and verifies the `BOOL` result before dispatching the exe entry.
pub const SPECIAL_DLL_MAIN_RETURN: u16 = 2;

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
    /// `IDirect3DVertexBuffer9` (ABI 6).
    VertexBuffer9,
    /// `IDirect3DIndexBuffer9` (ABI 7).
    IndexBuffer9,
    /// Not one of the D3D9 interfaces (raw byte preserved).
    Unknown(u8),
}

impl D3d9Iface {
    /// The real D3D9 interfaces.
    pub const ALL: [Self; 8] = [
        Self::Direct3D9,
        Self::Device9,
        Self::Texture9,
        Self::Surface9,
        Self::PixelShader9,
        Self::VertexShader9,
        Self::VertexBuffer9,
        Self::IndexBuffer9,
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
            6 => Self::VertexBuffer9,
            7 => Self::IndexBuffer9,
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
            Self::VertexBuffer9 => 6,
            Self::IndexBuffer9 => 7,
            Self::Unknown(v) => v,
        }
    }
}

/// The `kind=Com` iface byte reserved for `IDirectInput8`.
///
/// D3D9 owns 0..=7 (see [`D3d9Iface::ALL`]) and every other byte decodes to
/// `D3d9Iface::Unknown`, so DirectInput takes a disjoint high pair. Picking
/// 240/241 rather than 8/9 keeps the D3D9 decode table — and the
/// `D3d9Iface::Unknown` fallback that catches any other byte — bit-for-bit
/// unchanged, so no D3D9 VA can be re-decoded as DirectInput or vice versa.
const DINPUT8_IFACE_DIRECT_INPUT8: u8 = 240;

/// The `kind=Com` iface byte reserved for `IDirectInputDevice8`.
const DINPUT8_IFACE_DEVICE8: u8 = 241;

/// DirectInput COM interface identity (`kind=Com` high payload byte).
///
/// Same ABI contract as [`D3d9Iface`]: the byte is a real vtable's interface
/// identity as WIE encodes it, so the discriminants must not shift. The
/// variant list is derived from the real vtable layouts in
/// `/opt/homebrew/opt/mingw-w64/toolchain-x86_64/x86_64-w64-mingw32/include/dinput.h`:
/// `IDirectInput8W` (dinput.h:2402-2417) and `IDirectInputDevice8W`
/// (dinput.h:1992-2031). The A and W vtables have identical method order —
/// only the string widths in the signatures differ — so one enum serves both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DInput8Iface {
    /// `IDirectInput8` (the DirectInput8Create-returned object).
    DirectInput8,
    /// `IDirectInputDevice8` (a per-device object from `CreateDevice`).
    DirectInputDevice8,
    /// Not one of the DirectInput8 interfaces (raw byte preserved).
    Unknown(u8),
}

impl DInput8Iface {
    /// The real DirectInput8 interfaces.
    pub const ALL: [Self; 2] = [Self::DirectInput8, Self::DirectInputDevice8];

    /// Decode the ABI iface byte (never fails — unknown bytes stay decodable).
    #[must_use]
    pub const fn from_u8(v: u8) -> Self {
        match v {
            DINPUT8_IFACE_DIRECT_INPUT8 => Self::DirectInput8,
            DINPUT8_IFACE_DEVICE8 => Self::DirectInputDevice8,
            _ => Self::Unknown(v),
        }
    }

    /// The raw ABI iface byte (re-encodes identically).
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        match self {
            Self::DirectInput8 => DINPUT8_IFACE_DIRECT_INPUT8,
            Self::DirectInputDevice8 => DINPUT8_IFACE_DEVICE8,
            Self::Unknown(v) => v,
        }
    }
}

/// Which COM surface a `kind=Com` fake VA belongs to.
///
/// One `FakeVa::Com` carries either a D3D9 or a DirectInput8 interface, so
/// the decoded iface is a two-way sum rather than a flat enum: the D3D9 and
/// DirectInput8 vtable byte spaces are disjoint (see
/// [`DINPUT8_IFACE_DIRECT_INPUT8`]), which is what keeps the decode total.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComIface {
    /// A D3D9 interface (0..=7 plus the `Unknown` fallback).
    D3d9(D3d9Iface),
    /// A DirectInput8 interface (240/241 plus the `Unknown` fallback).
    DInput8(DInput8Iface),
}

impl ComIface {
    /// Decode the `kind=Com` high payload byte.
    ///
    /// Total: the two reserved DirectInput8 bytes are matched first, and
    /// *everything else* falls through to the D3D9 decoder — so a D3D9
    /// `Unknown` byte can never be mistaken for DirectInput.
    #[must_use]
    pub const fn from_byte(byte: u8) -> Self {
        if byte == DINPUT8_IFACE_DIRECT_INPUT8 {
            Self::DInput8(DInput8Iface::DirectInput8)
        } else if byte == DINPUT8_IFACE_DEVICE8 {
            Self::DInput8(DInput8Iface::DirectInputDevice8)
        } else {
            Self::D3d9(D3d9Iface::from_u8(byte))
        }
    }

    /// The raw `kind=Com` high payload byte (re-encodes identically).
    #[must_use]
    pub const fn as_byte(self) -> u8 {
        match self {
            Self::D3d9(iface) => iface.as_u8(),
            Self::DInput8(iface) => iface.as_u8(),
        }
    }

    /// The DLL name runtime dispatch resolves this interface's methods under.
    #[must_use]
    pub const fn library(self) -> &'static str {
        match self {
            Self::D3d9(_) => "D3D9.dll",
            Self::DInput8(_) => "dinput8.dll",
        }
    }

    /// Decode a vtable slot for this interface.
    #[must_use]
    pub const fn decode_method(self, slot: u8) -> ComMethod {
        match self {
            Self::D3d9(D3d9Iface::Direct3D9) => match Direct3D9Method::from_u8(slot) {
                Some(m) => ComMethod::Direct3D9(m),
                None => ComMethod::Unknown(slot),
            },
            Self::D3d9(D3d9Iface::Device9) => match Device9Method::from_u8(slot) {
                Some(m) => ComMethod::Device9(m),
                None => ComMethod::Unknown(slot),
            },
            Self::D3d9(D3d9Iface::Texture9) => match Texture9Method::from_u8(slot) {
                Some(m) => ComMethod::Texture9(m),
                None => ComMethod::Unknown(slot),
            },
            Self::D3d9(D3d9Iface::Surface9) => match Surface9Method::from_u8(slot) {
                Some(m) => ComMethod::Surface9(m),
                None => ComMethod::Unknown(slot),
            },
            Self::D3d9(D3d9Iface::PixelShader9) => match PixelShader9Method::from_u8(slot) {
                Some(m) => ComMethod::PixelShader9(m),
                None => ComMethod::Unknown(slot),
            },
            Self::D3d9(D3d9Iface::VertexShader9) => match VertexShader9Method::from_u8(slot) {
                Some(m) => ComMethod::VertexShader9(m),
                None => ComMethod::Unknown(slot),
            },
            Self::D3d9(D3d9Iface::VertexBuffer9) => match VertexBuffer9Method::from_u8(slot) {
                Some(m) => ComMethod::VertexBuffer9(m),
                None => ComMethod::Unknown(slot),
            },
            Self::D3d9(D3d9Iface::IndexBuffer9) => match IndexBuffer9Method::from_u8(slot) {
                Some(m) => ComMethod::IndexBuffer9(m),
                None => ComMethod::Unknown(slot),
            },
            Self::D3d9(D3d9Iface::Unknown(_)) => ComMethod::Unknown(slot),
            Self::DInput8(DInput8Iface::DirectInput8) => match DirectInput8Method::from_u8(slot) {
                Some(m) => ComMethod::DirectInput8(m),
                None => ComMethod::Unknown(slot),
            },
            Self::DInput8(DInput8Iface::DirectInputDevice8) => {
                match DirectInputDevice8Method::from_u8(slot) {
                    Some(m) => ComMethod::DirectInputDevice8(m),
                    None => ComMethod::Unknown(slot),
                }
            }
            Self::DInput8(DInput8Iface::Unknown(_)) => ComMethod::Unknown(slot),
        }
    }
}

/// `IDirectInput8` vtable method (the `DirectInput8Create` object).
///
/// The slot is ABI — a real vtable position in `IDirectInput8W`
/// (`dinput.h:2402-2417`, whose slot order is fixed by the `IUnknown` base
/// plus the `IDirectInput` → `IDirectInput2` → `IDirectInput7` →
/// `IDirectInput8` chain), so the discriminants must not shift. Unmodeled
/// slots are carried by [`ComMethod::Unknown`].
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectInput8Method {
    QueryInterface = 0,
    AddRef = 1,
    Release = 2,
    CreateDevice = 3,
    EnumDevices = 4,
    GetDeviceStatus = 5,
    RunControlPanel = 6,
    Initialize = 7,
    FindDevice = 8,
    EnumDevicesBySemantics = 9,
    ConfigureDevices = 10,
}

impl DirectInput8Method {
    /// Total `IDirectInput8` vtable slots (the fake vtable fills every position).
    pub const VTABLE_SLOTS: usize = 11;

    /// Decode a vtable slot; `None` for unmodeled slots.
    #[must_use]
    pub const fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::QueryInterface),
            1 => Some(Self::AddRef),
            2 => Some(Self::Release),
            3 => Some(Self::CreateDevice),
            4 => Some(Self::EnumDevices),
            5 => Some(Self::GetDeviceStatus),
            6 => Some(Self::RunControlPanel),
            7 => Some(Self::Initialize),
            8 => Some(Self::FindDevice),
            9 => Some(Self::EnumDevicesBySemantics),
            10 => Some(Self::ConfigureDevices),
            _ => None,
        }
    }

    /// The raw vtable slot byte (`#[repr(u8)]` — the discriminant IS the slot).
    #[must_use]
    pub const fn slot(self) -> u8 {
        self as u8
    }

    /// Trace name (`IDirectInput8::Xxx`).
    #[must_use]
    pub fn name(self) -> Cow<'static, str> {
        match self {
            Self::QueryInterface => Cow::Borrowed("IDirectInput8::QueryInterface"),
            Self::AddRef => Cow::Borrowed("IDirectInput8::AddRef"),
            Self::Release => Cow::Borrowed("IDirectInput8::Release"),
            Self::CreateDevice => Cow::Borrowed("IDirectInput8::CreateDevice"),
            Self::EnumDevices => Cow::Borrowed("IDirectInput8::EnumDevices"),
            Self::GetDeviceStatus => Cow::Borrowed("IDirectInput8::GetDeviceStatus"),
            Self::RunControlPanel => Cow::Borrowed("IDirectInput8::RunControlPanel"),
            Self::Initialize => Cow::Borrowed("IDirectInput8::Initialize"),
            Self::FindDevice => Cow::Borrowed("IDirectInput8::FindDevice"),
            Self::EnumDevicesBySemantics => Cow::Borrowed("IDirectInput8::EnumDevicesBySemantics"),
            Self::ConfigureDevices => Cow::Borrowed("IDirectInput8::ConfigureDevices"),
        }
    }
}

/// `IDirectInputDevice8` vtable method (a per-device object).
///
/// The slot is ABI — a real vtable position in `IDirectInputDevice8W`
/// (`dinput.h:1992-2031`), whose order is fixed by the
/// `IDirectInputDeviceA/W` → `…2` → `…7` → `…8` inheritance chain. Note
/// that the DirectInput8 interface has **no** `GetCapabilities` on
/// `IDirectInput8` itself (that is a device-interface method, slot 3), and no
/// `SendDeviceChangeAck` at all: the DX-era ack call is `SendDeviceData`
/// (slot 26) in the `IDirectInputDevice2` block.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectInputDevice8Method {
    QueryInterface = 0,
    AddRef = 1,
    Release = 2,
    GetCapabilities = 3,
    EnumObjects = 4,
    GetProperty = 5,
    SetProperty = 6,
    Acquire = 7,
    Unacquire = 8,
    GetDeviceState = 9,
    GetDeviceData = 10,
    SetDataFormat = 11,
    SetEventNotification = 12,
    SetCooperativeLevel = 13,
    GetObjectInfo = 14,
    GetDeviceInfo = 15,
    RunControlPanel = 16,
    Initialize = 17,
    CreateEffect = 18,
    EnumEffects = 19,
    GetEffectInfo = 20,
    GetForceFeedbackState = 21,
    SendForceFeedbackCommand = 22,
    EnumCreatedEffectObjects = 23,
    Escape = 24,
    Poll = 25,
    SendDeviceData = 26,
    EnumEffectsInFile = 27,
    WriteEffectToFile = 28,
    BuildActionMap = 29,
    SetActionMap = 30,
    GetImageInfo = 31,
}

impl DirectInputDevice8Method {
    /// Total `IDirectInputDevice8` vtable slots (the fake vtable fills every
    /// position).
    pub const VTABLE_SLOTS: usize = 32;

    /// Decode a vtable slot; `None` for unmodeled slots.
    #[must_use]
    pub const fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::QueryInterface),
            1 => Some(Self::AddRef),
            2 => Some(Self::Release),
            3 => Some(Self::GetCapabilities),
            4 => Some(Self::EnumObjects),
            5 => Some(Self::GetProperty),
            6 => Some(Self::SetProperty),
            7 => Some(Self::Acquire),
            8 => Some(Self::Unacquire),
            9 => Some(Self::GetDeviceState),
            10 => Some(Self::GetDeviceData),
            11 => Some(Self::SetDataFormat),
            12 => Some(Self::SetEventNotification),
            13 => Some(Self::SetCooperativeLevel),
            14 => Some(Self::GetObjectInfo),
            15 => Some(Self::GetDeviceInfo),
            16 => Some(Self::RunControlPanel),
            17 => Some(Self::Initialize),
            18 => Some(Self::CreateEffect),
            19 => Some(Self::EnumEffects),
            20 => Some(Self::GetEffectInfo),
            21 => Some(Self::GetForceFeedbackState),
            22 => Some(Self::SendForceFeedbackCommand),
            23 => Some(Self::EnumCreatedEffectObjects),
            24 => Some(Self::Escape),
            25 => Some(Self::Poll),
            26 => Some(Self::SendDeviceData),
            27 => Some(Self::EnumEffectsInFile),
            28 => Some(Self::WriteEffectToFile),
            29 => Some(Self::BuildActionMap),
            30 => Some(Self::SetActionMap),
            31 => Some(Self::GetImageInfo),
            _ => None,
        }
    }

    /// The raw vtable slot byte (`#[repr(u8)]` — the discriminant IS the slot).
    #[must_use]
    pub const fn slot(self) -> u8 {
        self as u8
    }

    /// Trace name (`IDirectInputDevice8::Xxx`).
    #[must_use]
    pub fn name(self) -> Cow<'static, str> {
        match self {
            Self::QueryInterface => Cow::Borrowed("IDirectInputDevice8::QueryInterface"),
            Self::AddRef => Cow::Borrowed("IDirectInputDevice8::AddRef"),
            Self::Release => Cow::Borrowed("IDirectInputDevice8::Release"),
            Self::GetCapabilities => Cow::Borrowed("IDirectInputDevice8::GetCapabilities"),
            Self::EnumObjects => Cow::Borrowed("IDirectInputDevice8::EnumObjects"),
            Self::GetProperty => Cow::Borrowed("IDirectInputDevice8::GetProperty"),
            Self::SetProperty => Cow::Borrowed("IDirectInputDevice8::SetProperty"),
            Self::Acquire => Cow::Borrowed("IDirectInputDevice8::Acquire"),
            Self::Unacquire => Cow::Borrowed("IDirectInputDevice8::Unacquire"),
            Self::GetDeviceState => Cow::Borrowed("IDirectInputDevice8::GetDeviceState"),
            Self::GetDeviceData => Cow::Borrowed("IDirectInputDevice8::GetDeviceData"),
            Self::SetDataFormat => Cow::Borrowed("IDirectInputDevice8::SetDataFormat"),
            Self::SetEventNotification => {
                Cow::Borrowed("IDirectInputDevice8::SetEventNotification")
            }
            Self::SetCooperativeLevel => Cow::Borrowed("IDirectInputDevice8::SetCooperativeLevel"),
            Self::GetObjectInfo => Cow::Borrowed("IDirectInputDevice8::GetObjectInfo"),
            Self::GetDeviceInfo => Cow::Borrowed("IDirectInputDevice8::GetDeviceInfo"),
            Self::RunControlPanel => Cow::Borrowed("IDirectInputDevice8::RunControlPanel"),
            Self::Initialize => Cow::Borrowed("IDirectInputDevice8::Initialize"),
            Self::CreateEffect => Cow::Borrowed("IDirectInputDevice8::CreateEffect"),
            Self::EnumEffects => Cow::Borrowed("IDirectInputDevice8::EnumEffects"),
            Self::GetEffectInfo => Cow::Borrowed("IDirectInputDevice8::GetEffectInfo"),
            Self::GetForceFeedbackState => {
                Cow::Borrowed("IDirectInputDevice8::GetForceFeedbackState")
            }
            Self::SendForceFeedbackCommand => {
                Cow::Borrowed("IDirectInputDevice8::SendForceFeedbackCommand")
            }
            Self::EnumCreatedEffectObjects => {
                Cow::Borrowed("IDirectInputDevice8::EnumCreatedEffectObjects")
            }
            Self::Escape => Cow::Borrowed("IDirectInputDevice8::Escape"),
            Self::Poll => Cow::Borrowed("IDirectInputDevice8::Poll"),
            Self::SendDeviceData => Cow::Borrowed("IDirectInputDevice8::SendDeviceData"),
            Self::EnumEffectsInFile => Cow::Borrowed("IDirectInputDevice8::EnumEffectsInFile"),
            Self::WriteEffectToFile => Cow::Borrowed("IDirectInputDevice8::WriteEffectToFile"),
            Self::BuildActionMap => Cow::Borrowed("IDirectInputDevice8::BuildActionMap"),
            Self::SetActionMap => Cow::Borrowed("IDirectInputDevice8::SetActionMap"),
            Self::GetImageInfo => Cow::Borrowed("IDirectInputDevice8::GetImageInfo"),
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
    CreateRenderTarget = 28,
    CreateDepthStencilSurface = 29,
    SetRenderTarget = 37,
    GetRenderTarget = 38,
    SetDepthStencilSurface = 39,
    GetDepthStencilSurface = 40,
    BeginScene = 41,
    EndScene = 42,
    Clear = 43,
    SetTransform = 44,
    GetTransform = 45,
    MultiplyTransform = 46,
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
    SetScissorRect = 75,
    DrawPrimitive = 81,
    DrawIndexedPrimitive = 82,
    DrawPrimitiveUp = 83,
    DrawIndexedPrimitiveUp = 84,
    SetFvf = 89,
    // ── shader methods (slots verified against d3d9.h) ─────────────────
    CreateVertexShader = 91,
    SetVertexShader = 92,
    GetVertexShader = 93,
    SetVertexShaderConstantF = 94,
    GetVertexShaderConstantF = 95,
    SetVertexShaderConstantI = 96,
    GetVertexShaderConstantI = 97,
    SetVertexShaderConstantB = 98,
    GetVertexShaderConstantB = 99,
    SetStreamSource = 100,
    GetStreamSource = 101,
    SetIndices = 104,
    GetIndices = 105,
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
            28 => Some(Self::CreateRenderTarget),
            29 => Some(Self::CreateDepthStencilSurface),
            37 => Some(Self::SetRenderTarget),
            38 => Some(Self::GetRenderTarget),
            39 => Some(Self::SetDepthStencilSurface),
            40 => Some(Self::GetDepthStencilSurface),
            41 => Some(Self::BeginScene),
            42 => Some(Self::EndScene),
            43 => Some(Self::Clear),
            44 => Some(Self::SetTransform),
            45 => Some(Self::GetTransform),
            46 => Some(Self::MultiplyTransform),
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
            96 => Some(Self::SetVertexShaderConstantI),
            97 => Some(Self::GetVertexShaderConstantI),
            98 => Some(Self::SetVertexShaderConstantB),
            99 => Some(Self::GetVertexShaderConstantB),
            100 => Some(Self::SetStreamSource),
            101 => Some(Self::GetStreamSource),
            104 => Some(Self::SetIndices),
            105 => Some(Self::GetIndices),
            106 => Some(Self::CreatePixelShader),
            107 => Some(Self::SetPixelShader),
            108 => Some(Self::GetPixelShader),
            109 => Some(Self::SetPixelShaderConstantF),
            110 => Some(Self::GetPixelShaderConstantF),
            75 => Some(Self::SetScissorRect),
            _ => None,
        }
    }

    /// The raw vtable slot byte (`#[repr(u8)]` — the discriminant IS the
    /// slot, so this cannot drift from the explicit discriminants).
    #[must_use]
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
            Self::CreateRenderTarget => Cow::Borrowed("IDirect3DDevice9::CreateRenderTarget"),
            Self::CreateVertexBuffer => Cow::Borrowed("IDirect3DDevice9::CreateVertexBuffer"),
            Self::CreateIndexBuffer => Cow::Borrowed("IDirect3DDevice9::CreateIndexBuffer"),
            Self::CreateDepthStencilSurface => {
                Cow::Borrowed("IDirect3DDevice9::CreateDepthStencilSurface")
            }
            Self::SetRenderTarget => Cow::Borrowed("IDirect3DDevice9::SetRenderTarget"),
            Self::GetRenderTarget => Cow::Borrowed("IDirect3DDevice9::GetRenderTarget"),
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
            Self::GetTransform => Cow::Borrowed("IDirect3DDevice9::GetTransform"),
            Self::MultiplyTransform => Cow::Borrowed("IDirect3DDevice9::MultiplyTransform"),
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
            Self::SetVertexShaderConstantI => {
                Cow::Borrowed("IDirect3DDevice9::SetVertexShaderConstantI")
            }
            Self::GetVertexShaderConstantI => {
                Cow::Borrowed("IDirect3DDevice9::GetVertexShaderConstantI")
            }
            Self::SetVertexShaderConstantB => {
                Cow::Borrowed("IDirect3DDevice9::SetVertexShaderConstantB")
            }
            Self::GetVertexShaderConstantB => {
                Cow::Borrowed("IDirect3DDevice9::GetVertexShaderConstantB")
            }
            Self::SetStreamSource => Cow::Borrowed("IDirect3DDevice9::SetStreamSource"),
            Self::GetStreamSource => Cow::Borrowed("IDirect3DDevice9::GetStreamSource"),
            Self::GetIndices => Cow::Borrowed("IDirect3DDevice9::GetIndices"),
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
            Self::SetScissorRect => Cow::Borrowed("IDirect3DDevice9::SetScissorRect"),
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
/// the vtable is the 3-slot COM trio (only Release has a handler).
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
/// shader (only Release has a handler).
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

/// `IDirect3DVertexBuffer9` vtable method (ABI slot positions; unmodeled
/// slots are carried by [`ComMethod::Unknown`]).
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VertexBuffer9Method {
    QueryInterface = 0,
    AddRef = 1,
    Release = 2,
    Lock = 11,
    Unlock = 12,
    GetDesc = 13,
}

impl VertexBuffer9Method {
    /// Total `IDirect3DVertexBuffer9` vtable slots (0..13).
    pub const VTABLE_SLOTS: usize = 14;

    /// Decode a vtable slot; `None` for unmodeled slots.
    #[must_use]
    pub const fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::QueryInterface),
            1 => Some(Self::AddRef),
            2 => Some(Self::Release),
            11 => Some(Self::Lock),
            12 => Some(Self::Unlock),
            13 => Some(Self::GetDesc),
            _ => None,
        }
    }

    /// The raw vtable slot byte (`#[repr(u8)]` — the discriminant IS the
    /// slot, so this cannot drift from the explicit discriminants).
    #[must_use]
    pub const fn slot(self) -> u8 {
        self as u8
    }

    /// Trace name (`IDirect3DVertexBuffer9::Xxx`).
    #[must_use]
    pub fn name(self) -> Cow<'static, str> {
        match self {
            Self::QueryInterface => Cow::Borrowed("IDirect3DVertexBuffer9::QueryInterface"),
            Self::AddRef => Cow::Borrowed("IDirect3DVertexBuffer9::AddRef"),
            Self::Release => Cow::Borrowed("IDirect3DVertexBuffer9::Release"),
            Self::Lock => Cow::Borrowed("IDirect3DVertexBuffer9::Lock"),
            Self::Unlock => Cow::Borrowed("IDirect3DVertexBuffer9::Unlock"),
            Self::GetDesc => Cow::Borrowed("IDirect3DVertexBuffer9::GetDesc"),
        }
    }
}

/// `IDirect3DIndexBuffer9` vtable method (ABI slot positions; unmodeled
/// slots are carried by [`ComMethod::Unknown`]).
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexBuffer9Method {
    QueryInterface = 0,
    AddRef = 1,
    Release = 2,
    Lock = 11,
    Unlock = 12,
    GetDesc = 13,
}

impl IndexBuffer9Method {
    /// Total `IDirect3DIndexBuffer9` vtable slots (0..13).
    pub const VTABLE_SLOTS: usize = 14;

    /// Decode a vtable slot; `None` for unmodeled slots.
    #[must_use]
    pub const fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::QueryInterface),
            1 => Some(Self::AddRef),
            2 => Some(Self::Release),
            11 => Some(Self::Lock),
            12 => Some(Self::Unlock),
            13 => Some(Self::GetDesc),
            _ => None,
        }
    }

    /// The raw vtable slot byte (`#[repr(u8)]` — the discriminant IS the
    /// slot, so this cannot drift from the explicit discriminants).
    #[must_use]
    pub const fn slot(self) -> u8 {
        self as u8
    }

    /// Trace name (`IDirect3DIndexBuffer9::Xxx`).
    #[must_use]
    pub fn name(self) -> Cow<'static, str> {
        match self {
            Self::QueryInterface => Cow::Borrowed("IDirect3DIndexBuffer9::QueryInterface"),
            Self::AddRef => Cow::Borrowed("IDirect3DIndexBuffer9::AddRef"),
            Self::Release => Cow::Borrowed("IDirect3DIndexBuffer9::Release"),
            Self::Lock => Cow::Borrowed("IDirect3DIndexBuffer9::Lock"),
            Self::Unlock => Cow::Borrowed("IDirect3DIndexBuffer9::Unlock"),
            Self::GetDesc => Cow::Borrowed("IDirect3DIndexBuffer9::GetDesc"),
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
    VertexBuffer9(VertexBuffer9Method),
    IndexBuffer9(IndexBuffer9Method),
    /// An `IDirectInput8` vtable slot.
    DirectInput8(DirectInput8Method),
    /// An `IDirectInputDevice8` vtable slot.
    DirectInputDevice8(DirectInputDevice8Method),
    /// Unmodeled slot (raw byte preserved).
    Unknown(u8),
}

impl ComMethod {
    /// Decode a method slot for `iface` (never fails — unknown slots fall
    /// back to [`Self::Unknown`], preserving the raw byte).
    #[must_use]
    pub const fn decode(iface: ComIface, slot: u8) -> Self {
        iface.decode_method(slot)
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
            Self::VertexBuffer9(m) => m.slot(),
            Self::IndexBuffer9(m) => m.slot(),
            Self::DirectInput8(m) => m.slot(),
            Self::DirectInputDevice8(m) => m.slot(),
            Self::Unknown(v) => v,
        }
    }

    /// Trace name. Known methods carry their interface prefix; unknown slots
    /// keep the legacy `IDirect3D*::SlotNNN` string (which also drives the
    /// name-table lookup — an unknown name resolves to no handler, as before);
    /// unknown interfaces keep the legacy `Com{iface}::Method{slot}` string.
    #[must_use]
    pub fn name(self, iface: ComIface) -> Cow<'static, str> {
        match self {
            Self::Direct3D9(m) => m.name(),
            Self::Device9(m) => m.name(),
            Self::Texture9(m) => m.name(),
            Self::Surface9(m) => m.name(),
            Self::PixelShader9(m) => m.name(),
            Self::VertexShader9(m) => m.name(),
            Self::VertexBuffer9(m) => m.name(),
            Self::IndexBuffer9(m) => m.name(),
            Self::DirectInput8(m) => m.name(),
            Self::DirectInputDevice8(m) => m.name(),
            Self::Unknown(v) => match iface {
                ComIface::D3d9(D3d9Iface::Direct3D9) => {
                    Cow::Owned(format!("IDirect3D9::Slot{v:03}"))
                }
                ComIface::D3d9(D3d9Iface::Device9) => {
                    Cow::Owned(format!("IDirect3DDevice9::Slot{v:03}"))
                }
                ComIface::D3d9(D3d9Iface::Texture9) => {
                    Cow::Owned(format!("IDirect3DTexture9::Slot{v:03}"))
                }
                ComIface::D3d9(D3d9Iface::Surface9) => {
                    Cow::Owned(format!("IDirect3DSurface9::Slot{v:03}"))
                }
                ComIface::D3d9(D3d9Iface::PixelShader9) => {
                    Cow::Owned(format!("IDirect3DPixelShader9::Slot{v:03}"))
                }
                ComIface::D3d9(D3d9Iface::VertexShader9) => {
                    Cow::Owned(format!("IDirect3DVertexShader9::Slot{v:03}"))
                }
                ComIface::D3d9(D3d9Iface::VertexBuffer9) => {
                    Cow::Owned(format!("IDirect3DVertexBuffer9::Slot{v:03}"))
                }
                ComIface::D3d9(D3d9Iface::IndexBuffer9) => {
                    Cow::Owned(format!("IDirect3DIndexBuffer9::Slot{v:03}"))
                }
                ComIface::D3d9(D3d9Iface::Unknown(raw)) => {
                    Cow::Owned(format!("Com{}::Method{v}", raw))
                }
                ComIface::DInput8(DInput8Iface::DirectInput8) => {
                    Cow::Owned(format!("IDirectInput8::Slot{v:03}"))
                }
                ComIface::DInput8(DInput8Iface::DirectInputDevice8) => {
                    Cow::Owned(format!("IDirectInputDevice8::Slot{v:03}"))
                }
                ComIface::DInput8(DInput8Iface::Unknown(raw)) => {
                    Cow::Owned(format!("Com{}::Method{v}", raw))
                }
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
    Com { iface: ComIface, method: ComMethod },
    /// Runtime special.
    Special(u16),
}

/// Pack `kind` + `payload` into a guest fake VA (the single encoding
/// implementation; every public entry point below delegates to it).
#[must_use]
const fn encode_parts(kind: u8, payload: u16) -> u64 {
    FAKE_API_BASE | ((kind as u64) << KIND_SHIFT) | ((payload as u64) << ALIGN_SHIFT)
}

impl FakeVa {
    /// Encode this decoded fake-API address back into its guest VA.
    ///
    /// The single `FakeVa` ↔ `u64` encoder: the kind-specific free functions
    /// (`encode_export`, …) are thin wrappers over this method so every kind
    /// shares one packing implementation.
    #[must_use]
    pub const fn encode(self) -> u64 {
        match self {
            Self::Export(id) => encode_parts(KIND_EXPORT, id.to_u16()),
            Self::Alias(id) => encode_parts(KIND_SOFT, id.to_u16()),
            Self::Unresolved(index) => {
                encode_parts(KIND_SOFT, SOFT_UNRESOLVED_BASE | (index & 0x7fff))
            }
            Self::Com { iface, method } => encode_parts(
                KIND_COM,
                ((iface.as_byte() as u16) << 8) | (method.slot() as u16),
            ),
            Self::Special(id) => encode_parts(KIND_SPECIAL, id),
        }
    }
}

/// Encode a primary export address for `id`.
#[must_use]
pub const fn encode_export(id: WinApiId) -> u64 {
    FakeVa::Export(id).encode()
}

/// Encode a host-fallback alias that dispatches the same `id`.
#[must_use]
pub const fn encode_alias(id: WinApiId) -> u64 {
    FakeVa::Alias(id).encode()
}

/// Encode a soft/unresolved slot (`index` must be `< 0x8000`).
#[must_use]
pub const fn encode_unresolved(index: u16) -> u64 {
    FakeVa::Unresolved(index).encode()
}

/// Encode a COM method address for a D3D9 interface.
#[must_use]
pub const fn encode_com(iface: D3d9Iface, method: u8) -> u64 {
    encode_com_for(ComIface::D3d9(iface), method)
}

/// Encode a COM method address for a DirectInput8 interface.
#[must_use]
pub const fn encode_com_dinput8(iface: DInput8Iface, method: u8) -> u64 {
    encode_com_for(ComIface::DInput8(iface), method)
}

/// Encode a COM method address for an already-summed [`ComIface`].
#[must_use]
pub const fn encode_com_for(iface: ComIface, method: u8) -> u64 {
    FakeVa::Com {
        iface,
        method: ComMethod::decode(iface, method),
    }
    .encode()
}

/// Encode a runtime special address.
#[must_use]
pub const fn encode_special(id: u16) -> u64 {
    FakeVa::Special(id).encode()
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

/// Static-dependency `DllMain` return trampoline VA (inside the fake-API
/// window; the stop bitmap covers it, so the session pump intercepts the
/// return from every statically-loaded DLL's `DllMain`).
#[must_use]
pub const fn dll_main_return_trampoline_va() -> u64 {
    encode_special(SPECIAL_DLL_MAIN_RETURN)
}

/// Decode a guest VA into a [`FakeVa`], if it lies in the fake-API window.
#[must_use]
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
            let iface = ComIface::from_byte((payload >> 8) as u8);
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
                iface: ComIface::D3d9(D3d9Iface::Device9),
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
                iface: ComIface::D3d9(D3d9Iface::Direct3D9),
                method: ComMethod::Direct3D9(Direct3D9Method::CreateDevice)
            })
        );
        let encoded = encode_com(D3d9Iface::Texture9, 20);
        assert_eq!(
            decode(encoded),
            Some(FakeVa::Com {
                iface: ComIface::D3d9(D3d9Iface::Texture9),
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
                iface: ComIface::D3d9(D3d9Iface::Unknown(9)),
                method: ComMethod::Unknown(3)
            })
        );
        // Unknown slot on a known iface: the raw slot survives
        // (legacy "IDirect3DDevice9::SlotNNN" path).
        let va = encode_com(D3d9Iface::Device9, 250);
        assert_eq!(
            decode(va),
            Some(FakeVa::Com {
                iface: ComIface::D3d9(D3d9Iface::Device9),
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
                let method = ComMethod::decode(ComIface::D3d9(iface), slot);
                assert_eq!(method.slot(), slot, "slot must round-trip for {iface:?}");
            }
        }
        // Unknown iface bytes keep their slot byte too.
        for slot in 0..=u8::MAX {
            let method = ComMethod::decode(ComIface::D3d9(D3d9Iface::Unknown(9)), slot);
            assert_eq!(method.slot(), slot);
        }
        assert_eq!(D3d9Iface::Unknown(9).as_u8(), 9);
        // Iface bytes 4 and 5 decode to the shader interfaces; 6/7 to the
        // vertex/index buffer interfaces (the L2 buffer-object additions).
        assert_eq!(D3d9Iface::from_u8(4), D3d9Iface::PixelShader9);
        assert_eq!(D3d9Iface::from_u8(5), D3d9Iface::VertexShader9);
        assert_eq!(D3d9Iface::from_u8(6), D3d9Iface::VertexBuffer9);
        assert_eq!(D3d9Iface::from_u8(7), D3d9Iface::IndexBuffer9);
    }

    #[test]
    fn iface_and_method_slots_are_the_abi_mapping() {
        // The ABI mapping is explicit and locked: iface bytes 0..7 decode to
        // the eight interfaces in order, and every modeled method slot is its
        // real vtable position.
        assert_eq!(D3d9Iface::from_u8(0), D3d9Iface::Direct3D9);
        assert_eq!(D3d9Iface::from_u8(1), D3d9Iface::Device9);
        assert_eq!(D3d9Iface::from_u8(2), D3d9Iface::Texture9);
        assert_eq!(D3d9Iface::from_u8(3), D3d9Iface::Surface9);
        assert_eq!(D3d9Iface::from_u8(4), D3d9Iface::PixelShader9);
        assert_eq!(D3d9Iface::from_u8(5), D3d9Iface::VertexShader9);
        assert_eq!(D3d9Iface::from_u8(6), D3d9Iface::VertexBuffer9);
        assert_eq!(D3d9Iface::from_u8(7), D3d9Iface::IndexBuffer9);
        assert_eq!(D3d9Iface::Direct3D9.as_u8(), 0);
        assert_eq!(D3d9Iface::Device9.as_u8(), 1);
        assert_eq!(D3d9Iface::Texture9.as_u8(), 2);
        assert_eq!(D3d9Iface::Surface9.as_u8(), 3);
        assert_eq!(D3d9Iface::PixelShader9.as_u8(), 4);
        assert_eq!(D3d9Iface::VertexShader9.as_u8(), 5);
        assert_eq!(D3d9Iface::VertexBuffer9.as_u8(), 6);
        assert_eq!(D3d9Iface::IndexBuffer9.as_u8(), 7);

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
        // The L2 buffer-form getters (slots verified against d3d9.h).
        assert_eq!(
            Device9Method::from_u8(101),
            Some(Device9Method::GetStreamSource)
        );
        assert_eq!(Device9Method::from_u8(105), Some(Device9Method::GetIndices));
        assert_eq!(Device9Method::GetStreamSource.slot(), 101);
        assert_eq!(Device9Method::GetIndices.slot(), 105);
        // Device shader methods (slots verified against d3d9.h).
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

        // Vertex/index buffer methods (slots verified against d3d9.h: the
        // IUnknown trio 0..2, Lock 11, Unlock 12, GetDesc 13).
        assert_eq!(
            VertexBuffer9Method::from_u8(11),
            Some(VertexBuffer9Method::Lock)
        );
        assert_eq!(
            VertexBuffer9Method::from_u8(13),
            Some(VertexBuffer9Method::GetDesc)
        );
        assert_eq!(VertexBuffer9Method::Lock.slot(), 11);
        assert_eq!(VertexBuffer9Method::GetDesc.slot(), 13);
        assert_eq!(VertexBuffer9Method::VTABLE_SLOTS, 14);
        assert_eq!(
            IndexBuffer9Method::from_u8(12),
            Some(IndexBuffer9Method::Unlock)
        );
        assert_eq!(IndexBuffer9Method::Unlock.slot(), 12);
        assert_eq!(IndexBuffer9Method::VTABLE_SLOTS, 14);

        // Unmodeled slots decode to None.
        assert_eq!(Direct3D9Method::from_u8(200), None);
        assert_eq!(Device9Method::from_u8(119), None);
    }

    #[test]
    fn com_name_matches_legacy_dispatch_strings() {
        // The trace names are load-bearing: they drive the D3D9 name-table
        // lookup, so they must match the pre-refactor strings exactly.
        assert_eq!(
            ComMethod::Direct3D9(Direct3D9Method::GetDeviceCaps)
                .name(ComIface::D3d9(D3d9Iface::Direct3D9)),
            "IDirect3D9::GetDeviceCaps"
        );
        assert_eq!(
            ComMethod::Device9(Device9Method::SetRenderState)
                .name(ComIface::D3d9(D3d9Iface::Device9)),
            "IDirect3DDevice9::SetRenderState"
        );
        assert_eq!(
            ComMethod::Unknown(250).name(ComIface::D3d9(D3d9Iface::Device9)),
            "IDirect3DDevice9::Slot250"
        );
        assert_eq!(
            ComMethod::Unknown(3).name(ComIface::D3d9(D3d9Iface::Unknown(5))),
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
