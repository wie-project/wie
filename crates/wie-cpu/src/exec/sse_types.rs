//! ABI-stable SSE opcode enums shared between the iced interpreter and the
//! JIT lowering.
//!
//! The `SseIntOp` / `SseShiftOp` / `SseFpUnOp` / `SseFpBinOp` / `SseCvtOp`
//! discriminants are a JIT ABI contract (encoded into `iconst` immediates and
//! passed through `extern "C"` u64 params) — never renumber.

#[derive(Clone, Copy)]
pub(super) enum SseBitOp {
    Xor,
    And,
    Or,
    Andn,
}

#[derive(Clone, Copy)]
pub(super) enum FpOp {
    Add,
    Sub,
    Mul,
    Div,
}

/// ABI-stable packed-integer opcode (encoded into `iconst` immediates by the
/// JIT lowering). Values are part of the JIT ABI — never renumber.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SseIntOp {
    Paddb = 0,
    Paddw = 1,
    Paddd = 2,
    Paddq = 3,
    Psubb = 4,
    Psubw = 5,
    Psubd = 6,
    Psubq = 7,
    Paddsb = 8,
    Paddsw = 9,
    Paddusb = 10,
    Paddusw = 11,
    Psubsb = 12,
    Psubsw = 13,
    Psubusb = 14,
    Psubusw = 15,
    Pmullw = 16,
    Pmulhw = 17,
    Pmulhuw = 18,
    Pmuludq = 19,
    Pmaddwd = 20,
    Pcmpeqb = 21,
    Pcmpeqw = 22,
    Pcmpeqd = 23,
    Pcmpgtb = 24,
    Pcmpgtw = 25,
    Pcmpgtd = 26,
    Packsswb = 27,
    Packssdw = 28,
    Packuswb = 29,
    Punpcklbw = 30,
    Punpcklwd = 31,
    Punpckldq = 32,
    /// High-sub-lane interleave used for the result's high half of `punpckh*`
    /// (the low half reuses the matching `Punpckl*` op over the high halves).
    PunpckHiBw = 33,
    PunpckHiWd = 34,
    PunpckHiDq = 35,
}

impl SseIntOp {
    pub(crate) fn to_abi(self) -> u64 {
        self as u64
    }
}

impl TryFrom<u64> for SseIntOp {
    type Error = ();

    fn try_from(value: u64) -> Result<Self, Self::Error> {
        Ok(match value {
            0 => Self::Paddb,
            1 => Self::Paddw,
            2 => Self::Paddd,
            3 => Self::Paddq,
            4 => Self::Psubb,
            5 => Self::Psubw,
            6 => Self::Psubd,
            7 => Self::Psubq,
            8 => Self::Paddsb,
            9 => Self::Paddsw,
            10 => Self::Paddusb,
            11 => Self::Paddusw,
            12 => Self::Psubsb,
            13 => Self::Psubsw,
            14 => Self::Psubusb,
            15 => Self::Psubusw,
            16 => Self::Pmullw,
            17 => Self::Pmulhw,
            18 => Self::Pmulhuw,
            19 => Self::Pmuludq,
            20 => Self::Pmaddwd,
            21 => Self::Pcmpeqb,
            22 => Self::Pcmpeqw,
            23 => Self::Pcmpeqd,
            24 => Self::Pcmpgtb,
            25 => Self::Pcmpgtw,
            26 => Self::Pcmpgtd,
            27 => Self::Packsswb,
            28 => Self::Packssdw,
            29 => Self::Packuswb,
            30 => Self::Punpcklbw,
            31 => Self::Punpcklwd,
            32 => Self::Punpckldq,
            33 => Self::PunpckHiBw,
            34 => Self::PunpckHiWd,
            35 => Self::PunpckHiDq,
            _ => return Err(()),
        })
    }
}

/// ABI-stable packed-shift opcode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SseShiftOp {
    Psllw = 0,
    Pslld = 1,
    Psllq = 2,
    Psrlw = 3,
    Psrld = 4,
    Psrlq = 5,
    Psraw = 6,
    Psrad = 7,
}

impl SseShiftOp {
    pub(crate) fn to_abi(self) -> u64 {
        self as u64
    }
}

impl TryFrom<u64> for SseShiftOp {
    type Error = ();

    fn try_from(value: u64) -> Result<Self, Self::Error> {
        Ok(match value {
            0 => Self::Psllw,
            1 => Self::Pslld,
            2 => Self::Psllq,
            3 => Self::Psrlw,
            4 => Self::Psrld,
            5 => Self::Psrlq,
            6 => Self::Psraw,
            7 => Self::Psrad,
            _ => return Err(()),
        })
    }
}

/// ABI-stable packed/scalar FP unary opcode (sqrt family).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SseFpUnOp {
    /// 2 f32 lanes per half.
    Sqrtps = 0,
    /// 1 f64 lane per half.
    Sqrtpd = 1,
    /// Scalar low f32 lane.
    Sqrtss = 2,
    /// Scalar low f64 lane.
    Sqrtsd = 3,
}

impl SseFpUnOp {
    pub(crate) fn to_abi(self) -> u64 {
        self as u64
    }
}

impl TryFrom<u64> for SseFpUnOp {
    type Error = ();

    fn try_from(value: u64) -> Result<Self, Self::Error> {
        Ok(match value {
            0 => Self::Sqrtps,
            1 => Self::Sqrtpd,
            2 => Self::Sqrtss,
            3 => Self::Sqrtsd,
            _ => return Err(()),
        })
    }
}

/// ABI-stable FP min/max opcode (scalar + packed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SseFpBinOp {
    /// 2 f32 lanes per half.
    Minps = 0,
    Maxps = 1,
    /// 1 f64 lane per half.
    Minpd = 2,
    Maxpd = 3,
    /// Scalar low f32 lane.
    Minss = 4,
    Maxss = 5,
    /// Scalar low f64 lane.
    Minsd = 6,
    Maxsd = 7,
}

impl SseFpBinOp {
    pub(crate) fn to_abi(self) -> u64 {
        self as u64
    }
}

impl TryFrom<u64> for SseFpBinOp {
    type Error = ();

    fn try_from(value: u64) -> Result<Self, Self::Error> {
        Ok(match value {
            0 => Self::Minps,
            1 => Self::Maxps,
            2 => Self::Minpd,
            3 => Self::Maxpd,
            4 => Self::Minss,
            5 => Self::Maxss,
            6 => Self::Minsd,
            7 => Self::Maxsd,
            _ => return Err(()),
        })
    }
}

/// ABI-stable convert opcode (GPR↔FP and packed FP↔int).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SseCvtOp {
    Cvtsi2ss32 = 0,
    Cvtsi2ss64 = 1,
    Cvtsi2sd32 = 2,
    Cvtsi2sd64 = 3,
    Cvttss2si32 = 4,
    Cvtss2si32 = 5,
    Cvttss2si64 = 6,
    Cvtss2si64 = 7,
    Cvttsd2si32 = 8,
    Cvtsd2si32 = 9,
    Cvttsd2si64 = 10,
    Cvtsd2si64 = 11,
    /// 2 f32 lanes → 2 i32 lanes (round-nearest).
    Cvtps2dq = 12,
    /// 2 i32 lanes → 2 f32 lanes.
    Cvtdq2ps = 13,
    /// 2 f32 lanes → 2 i32 lanes (truncate).
    Cvttps2dq = 14,
}

impl SseCvtOp {
    pub(crate) fn to_abi(self) -> u64 {
        self as u64
    }
}

impl TryFrom<u64> for SseCvtOp {
    type Error = ();

    fn try_from(value: u64) -> Result<Self, Self::Error> {
        Ok(match value {
            0 => Self::Cvtsi2ss32,
            1 => Self::Cvtsi2ss64,
            2 => Self::Cvtsi2sd32,
            3 => Self::Cvtsi2sd64,
            4 => Self::Cvttss2si32,
            5 => Self::Cvtss2si32,
            6 => Self::Cvttss2si64,
            7 => Self::Cvtss2si64,
            8 => Self::Cvttsd2si32,
            9 => Self::Cvtsd2si32,
            10 => Self::Cvttsd2si64,
            11 => Self::Cvtsd2si64,
            12 => Self::Cvtps2dq,
            13 => Self::Cvtdq2ps,
            14 => Self::Cvttps2dq,
            _ => return Err(()),
        })
    }
}
