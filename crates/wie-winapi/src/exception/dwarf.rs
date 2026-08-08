// ── DWARF EH pointer encodings (Itanium C++ ABI / GCC dwarf2.h) ────────

/// `DW_EH_PE_*` application / format bits used in LSDA headers.
pub(super) mod dw_eh_pe {
    pub(in crate::exception) const OMIT: u8 = 0xff;
    pub(in crate::exception) const ABSPTR: u8 = 0x00;
    pub(in crate::exception) const ULEB128: u8 = 0x01;
    pub(in crate::exception) const UDATA2: u8 = 0x02;
    pub(in crate::exception) const UDATA4: u8 = 0x03;
    pub(in crate::exception) const UDATA8: u8 = 0x04;
    pub(in crate::exception) const SLEB128: u8 = 0x09;
    pub(in crate::exception) const SDATA2: u8 = 0x0a;
    pub(in crate::exception) const SDATA4: u8 = 0x0b;
    pub(in crate::exception) const SDATA8: u8 = 0x0c;
    pub(in crate::exception) const PCREL: u8 = 0x10;
    pub(in crate::exception) const TEXTREL: u8 = 0x20;
    pub(in crate::exception) const DATAREL: u8 = 0x30;
    pub(in crate::exception) const FUNCREL: u8 = 0x40;
    pub(in crate::exception) const ALIGNED: u8 = 0x50;
    pub(in crate::exception) const INDIRECT: u8 = 0x80;
}

/// Result of host-side Itanium LSDA call-site + action matching.
#[derive(Debug, Clone, Copy)]
pub struct LandingPadMatch {
    /// Absolute guest VA of the landing pad (or cleanup).
    pub landing_pad: u64,
    /// 1-based action table index from the call-site entry (`0` = no action).
    pub action_index: u64,
    /// Value loaded into RDX at landing-pad entry (handler switch / type filter).
    pub switch_value: i64,
}
