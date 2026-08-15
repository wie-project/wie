//! Single declaration of the dense WinAPI dispatch surface.
//!
//! `winapi_ids!` below is the one place an API is declared. The macro expands
//! it into three artifacts that used to be maintained by hand in five places:
//!
//! 1. the `#[repr(u16)]` `WinApiId` enum (variants + append-only discriminants),
//! 2. the hot-path `dispatch_winapi_id` match (variant -> handler call),
//! 3. the `(library, export, id)` name rows (name -> id resolution and the
//!    reverse id -> name lookup for trace/profile).
//!
//! Row syntax (one API per line, append-only — never renumber existing rows):
//!
//! ```text
//! {discriminant} {Variant} "{dll}.dll" "{export}" ["{alias}"]... => {handler call};
//! ```
//!
//! * The discriminant is explicit and fixed forever; appends use the next free
//!   number. The two historical holes (109/110) stay empty.
//! * The first `"{export}"` is the canonical name row (`winapi_id_export`
//!   returns it); extra exports emit additional alias rows in the given order.
//!   `User32Getdlgitema` historically lists the bare `getdlgitem` alias first,
//!   so it is the primary row there.
//! * `{handler call}` is the dispatch-arm expression; `ctx` is the
//!   `HandlerContext` parameter of the generated dispatch function (see the
//!   `param ctx;` header line below — the macro substitutes it).

use crate::{
    HandlerContext, WinApiHandlerResult, advapi32, comctl32, comdlg32, d3d9, gdi32, kernel32,
    shell32, user32, uxtheme, version, winmm,
};
use anyhow::Result;
use strum::{EnumCount, FromRepr};

/// Expands the declaration into the enum, the dispatch match, and the name rows.
macro_rules! winapi_ids {
    (
        param $param:ident;
        $(
            $disc:literal $name:ident $lib:literal $($export:literal)+ => $handler:expr;
        )+
    ) => {
        /// Dense handler identifier. Resolved once when building the fake-API
        /// table.
        ///
        /// `FromRepr` replaces the old hand-checked `u16` -> `Self` transmute;
        /// `EnumCount` supplies `COUNT` (the variant count, see
        /// `WINAPI_ID_COUNT` for why that is one less than the discriminant
        /// span).
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, FromRepr, EnumCount)]
        #[repr(u16)]
        pub enum WinApiId {
            $( $name = $disc, )+
        }

        /// Hot-path dispatch: dense `u16` match over the `WinApiId`
        /// discriminant (LLVM jump table) with no string comparison — runs
        /// once per host API stop.
        pub fn dispatch_winapi_id(
            $param: &mut HandlerContext<'_>,
            id: WinApiId,
        ) -> Result<WinApiHandlerResult> {
            match id {
                $( WinApiId::$name => $handler, )+
            }
        }

        /// All `(library, export, id)` name rows, in declaration order.
        ///
        /// The same id may appear on several rows (export aliases); the first
        /// row per id is what the reverse lookup reports.
        pub(super) static WINAPI_NAME_ROWS: &[(&str, &str, WinApiId)] = &[
            $( $( ($lib, $export, WinApiId::$name), )+ )+
        ];
    };
}

/// Compile-time coverage: the declaration must keep emitting exactly the
/// historical surface — 505 variants, 507 discriminants (two holes), 506 name
/// rows (505 variants + the `getdlgitem`/`getdlgitema` alias pair). Dispatch
/// coverage is exhaustive by construction: the match is generated with one arm
/// per declaration row, so the compiler rejects any variant without a handler.
const _: () = {
    assert!(WinApiId::COUNT == 505, "variant count drifted");
    assert!(super::WINAPI_ID_COUNT == 507, "discriminant span drifted");
    assert!(
        WINAPI_NAME_ROWS.len() == 506,
        "name-row count drifted (505 variants + 1 alias)"
    );
};

#[rustfmt::skip]
winapi_ids! {
    param ctx;

    0 Kernel32Getversionexa "kernel32.dll" "getversionexa" => kernel32::handle_get_version_ex_a(ctx);
    1 Kernel32Getmodulehandlea "kernel32.dll" "getmodulehandlea" => kernel32::handle_get_module_handle_a(ctx);
    2 Kernel32Getcommandlinea "kernel32.dll" "getcommandlinea" => kernel32::handle_get_command_line_a(ctx);
    3 Kernel32Getcommandlinew "kernel32.dll" "getcommandlinew" => kernel32::handle_get_command_line_w(ctx);
    4 Kernel32Getstartupinfoa "kernel32.dll" "getstartupinfoa" => kernel32::handle_get_startup_info_a(ctx);
    5 Kernel32Getprocessheap "kernel32.dll" "getprocessheap" => kernel32::handle_get_process_heap(ctx);
    6 Kernel32Getsystemtimeasfiletime "kernel32.dll" "getsystemtimeasfiletime" => kernel32::handle_get_system_time_as_file_time(ctx);
    7 Kernel32Getcurrentprocessid "kernel32.dll" "getcurrentprocessid" => kernel32::handle_get_current_process_id(ctx);
    8 Kernel32Getcurrentthreadid "kernel32.dll" "getcurrentthreadid" => kernel32::handle_get_current_thread_id(ctx);
    9 Kernel32Gettickcount "kernel32.dll" "gettickcount" => kernel32::handle_get_tick_count(ctx);
    10 Kernel32Queryperformancecounter "kernel32.dll" "queryperformancecounter" => kernel32::handle_query_performance_counter(ctx);
    11 Kernel32Heapalloc "kernel32.dll" "heapalloc" => kernel32::handle_heap_alloc(ctx);
    12 Kernel32Heapfree "kernel32.dll" "heapfree" => kernel32::handle_heap_free(ctx);
    13 Kernel32Heaprealloc "kernel32.dll" "heaprealloc" => kernel32::handle_heap_realloc(ctx);
    14 Kernel32Heapcreate "kernel32.dll" "heapcreate" => kernel32::handle_heap_create(ctx);
    15 Kernel32Heapsetinformation "kernel32.dll" "heapsetinformation" => kernel32::handle_heap_set_information(ctx);
    16 Kernel32Initializecriticalsection "kernel32.dll" "initializecriticalsection" => kernel32::handle_initialize_critical_section(ctx);
    17 Kernel32Entercriticalsection "kernel32.dll" "entercriticalsection" => kernel32::handle_enter_critical_section(ctx);
    18 Kernel32Leavecriticalsection "kernel32.dll" "leavecriticalsection" => kernel32::handle_leave_critical_section(ctx);
    19 Kernel32Deletecriticalsection "kernel32.dll" "deletecriticalsection" => kernel32::handle_delete_critical_section(ctx);
    20 Kernel32Flsalloc "kernel32.dll" "flsalloc" => kernel32::handle_fls_alloc(ctx);
    21 Kernel32Flsfree "kernel32.dll" "flsfree" => kernel32::handle_fls_free(ctx);
    22 Kernel32Flssetvalue "kernel32.dll" "flssetvalue" => kernel32::handle_fls_set_value(ctx);
    23 Kernel32Flsgetvalue "kernel32.dll" "flsgetvalue" => kernel32::handle_fls_get_value(ctx);
    24 Kernel32Getstdhandle "kernel32.dll" "getstdhandle" => kernel32::handle_get_std_handle(ctx);
    25 Kernel32Getfiletype "kernel32.dll" "getfiletype" => kernel32::handle_get_file_type(ctx);
    26 Kernel32Sethandlecount "kernel32.dll" "sethandlecount" => kernel32::handle_set_handle_count(ctx);
    27 Kernel32Getenvironmentstringsw "kernel32.dll" "getenvironmentstringsw" => kernel32::handle_get_environment_strings_w(ctx);
    28 Kernel32Freeenvironmentstringsw "kernel32.dll" "freeenvironmentstringsw" => kernel32::handle_free_environment_strings_w(ctx);
    29 Kernel32Widechartomultibyte "kernel32.dll" "widechartomultibyte" => kernel32::handle_wide_char_to_multi_byte(ctx);
    30 Kernel32Getlasterror "kernel32.dll" "getlasterror" => kernel32::handle_get_last_error(ctx);
    31 Kernel32Setlasterror "kernel32.dll" "setlasterror" => kernel32::handle_set_last_error(ctx);
    32 Kernel32Getacp "kernel32.dll" "getacp" => kernel32::handle_get_acp(ctx);
    33 Kernel32Getoemcp "kernel32.dll" "getoemcp" => kernel32::handle_get_oem_cp(ctx);
    34 Kernel32Getcpinfo "kernel32.dll" "getcpinfo" => kernel32::handle_get_cp_info(ctx);
    35 Kernel32Isvalidcodepage "kernel32.dll" "isvalidcodepage" => kernel32::handle_is_valid_code_page(ctx);
    36 Kernel32Getstringtypew "kernel32.dll" "getstringtypew" => kernel32::handle_get_string_type_w(ctx);
    37 Kernel32Multibytetowidechar "kernel32.dll" "multibytetowidechar" => kernel32::handle_multi_byte_to_wide_char(ctx);
    38 Kernel32Lcmapstringw "kernel32.dll" "lcmapstringw" => kernel32::handle_lc_map_string_w(ctx);
    39 Kernel32Getmodulefilenamea "kernel32.dll" "getmodulefilenamea" => kernel32::handle_get_module_file_name_a(ctx);
    40 Kernel32Getmodulefilenamew "kernel32.dll" "getmodulefilenamew" => kernel32::handle_get_module_file_name_w(ctx);
    41 Kernel32Setunhandledexceptionfilter "kernel32.dll" "setunhandledexceptionfilter" => kernel32::handle_set_unhandled_exception_filter(ctx);
    42 Kernel32Heapsize "kernel32.dll" "heapsize" => kernel32::handle_heap_size(ctx);
    43 Advapi32Regcreatekeyexa "advapi32.dll" "regcreatekeyexa" => advapi32::handle_reg_create_key_ex_a(ctx);
    44 Advapi32Regopenkeyexa "advapi32.dll" "regopenkeyexa" => advapi32::handle_reg_open_key_ex_a(ctx);
    45 Advapi32Regqueryvalueexa "advapi32.dll" "regqueryvalueexa" => advapi32::handle_reg_query_value_ex_a(ctx);
    46 Advapi32Regqueryvalueexw "advapi32.dll" "regqueryvalueexw" => advapi32::handle_reg_query_value_ex_w(ctx);
    47 Advapi32Regsetvalueexa "advapi32.dll" "regsetvalueexa" => advapi32::handle_reg_set_value_ex_a(ctx);
    48 Advapi32Regsetvalueexw "advapi32.dll" "regsetvalueexw" => advapi32::handle_reg_set_value_ex_w(ctx);
    49 Advapi32Regdeletevaluea "advapi32.dll" "regdeletevaluea" => advapi32::handle_reg_delete_value_a(ctx);
    50 Advapi32Regclosekey "advapi32.dll" "regclosekey" => advapi32::handle_reg_close_key(ctx);
    51 Advapi32Initializesecuritydescriptor "advapi32.dll" "initializesecuritydescriptor" => advapi32::handle_initialize_security_descriptor(ctx);
    52 Advapi32Setsecuritydescriptordacl "advapi32.dll" "setsecuritydescriptordacl" => advapi32::handle_set_security_descriptor_dacl(ctx);
    53 Kernel32Loadlibrarya "kernel32.dll" "loadlibrarya" => kernel32::handle_load_library_a(ctx);
    54 Kernel32Loadlibraryw "kernel32.dll" "loadlibraryw" => kernel32::handle_load_library_w(ctx);
    55 Kernel32Freelibrary "kernel32.dll" "freelibrary" => kernel32::handle_free_library(ctx);
    56 Kernel32Getprocaddress "kernel32.dll" "getprocaddress" => kernel32::handle_get_proc_address(ctx);
    57 Kernel32Getfileattributesa "kernel32.dll" "getfileattributesa" => kernel32::handle_get_file_attributes_a(ctx);
    58 Kernel32Getfileattributesw "kernel32.dll" "getfileattributesw" => kernel32::handle_get_file_attributes_w(ctx);
    59 Kernel32Findfirstfilew "kernel32.dll" "findfirstfilew" => kernel32::handle_find_first_file_w(ctx);
    60 Kernel32Findfirstfilea "kernel32.dll" "findfirstfilea" => kernel32::handle_find_first_file_a(ctx);
    61 Kernel32Findnextfilew "kernel32.dll" "findnextfilew" => kernel32::handle_find_next_file_w(ctx);
    62 Kernel32Findnextfilea "kernel32.dll" "findnextfilea" => kernel32::handle_find_next_file_a(ctx);
    63 Kernel32Findclose "kernel32.dll" "findclose" => kernel32::handle_find_close(ctx);
    64 User32Getasynckeystate "user32.dll" "getasynckeystate" => user32::handle_get_async_key_state(ctx);
    65 User32Peekmessagea "user32.dll" "peekmessagea" => user32::handle_peek_message_a(ctx);
    66 Kernel32Loadlibraryexa "kernel32.dll" "loadlibraryexa" => kernel32::handle_load_library_ex_a(ctx);
    67 Kernel32Loadlibraryexw "kernel32.dll" "loadlibraryexw" => kernel32::handle_load_library_ex_w(ctx);
    68 Kernel32Findresourcea "kernel32.dll" "findresourcea" => kernel32::handle_find_resource_a(ctx);
    69 Kernel32Loadresource "kernel32.dll" "loadresource" => kernel32::handle_load_resource(ctx);
    70 Kernel32Lockresource "kernel32.dll" "lockresource" => kernel32::handle_lock_resource(ctx);
    71 Kernel32Sizeofresource "kernel32.dll" "sizeofresource" => kernel32::handle_sizeof_resource(ctx);
    72 Kernel32Getsystemdefaultlangid "kernel32.dll" "getsystemdefaultlangid" => kernel32::handle_get_system_default_lang_id(ctx);
    73 Kernel32Getuserdefaultlangid "kernel32.dll" "getuserdefaultlangid" => kernel32::handle_get_user_default_lang_id(ctx);
    74 Kernel32Globalmemorystatus "kernel32.dll" "globalmemorystatus" => kernel32::handle_global_memory_status(ctx);
    75 Kernel32Getlocaltime "kernel32.dll" "getlocaltime" => kernel32::handle_get_local_time(ctx);
    76 User32Loadicona "user32.dll" "loadicona" => user32::handle_load_icon_a(ctx);
    77 User32Loadcursora "user32.dll" "loadcursora" => user32::handle_load_cursor_a(ctx);
    78 User32Registerclassexw "user32.dll" "registerclassexw" => user32::handle_register_class_ex_w(ctx);
    79 User32Registerclassexa "user32.dll" "registerclassexa" => user32::handle_register_class_ex_a(ctx);
    80 Kernel32Createfilew "kernel32.dll" "createfilew" => kernel32::handle_create_file_w(ctx);
    81 Kernel32Createfilea "kernel32.dll" "createfilea" => kernel32::handle_create_file_a(ctx);
    82 Kernel32Closehandle "kernel32.dll" "closehandle" => kernel32::handle_close_handle(ctx);
    83 User32Messageboxw "user32.dll" "messageboxw" => user32::handle_message_box_w(ctx);
    84 User32Messageboxa "user32.dll" "messageboxa" => user32::handle_message_box_a(ctx);
    85 Kernel32Getfileinformationbyhandle "kernel32.dll" "getfileinformationbyhandle" => kernel32::handle_get_file_information_by_handle(ctx);
    86 Kernel32Filetimetolocalfiletime "kernel32.dll" "filetimetolocalfiletime" => kernel32::handle_file_time_to_local_file_time(ctx);
    87 Kernel32Filetimetosystemtime "kernel32.dll" "filetimetosystemtime" => kernel32::handle_file_time_to_system_time(ctx);
    88 Kernel32Gettimezoneinformation "kernel32.dll" "gettimezoneinformation" => kernel32::handle_get_time_zone_information(ctx);
    89 Kernel32Getfiletime "kernel32.dll" "getfiletime" => kernel32::handle_get_file_time(ctx);
    90 Kernel32Setfilepointer "kernel32.dll" "setfilepointer" => kernel32::handle_set_file_pointer(ctx);
    91 Kernel32Getfilesize "kernel32.dll" "getfilesize" => kernel32::handle_get_file_size(ctx);
    92 Kernel32Encodepointer "kernel32.dll" "encodepointer" => kernel32::handle_encode_pointer(ctx);
    93 Kernel32Decodepointer "kernel32.dll" "decodepointer" => kernel32::handle_decode_pointer(ctx);
    94 Kernel32Initializecriticalsectionandspincount "kernel32.dll" "initializecriticalsectionandspincount" => kernel32::handle_initialize_critical_section_and_spin_count(ctx);
    95 User32Setprocessdpiaware "user32.dll" "setprocessdpiaware" => user32::handle_set_process_dpi_aware(ctx);
    96 User32Trackmouseevent "user32.dll" "trackmouseevent" => user32::handle_track_mouse_event(ctx);
    97 Comctl32Dllgetversion "comctl32.dll" "dllgetversion" => comctl32::handle_dll_get_version(ctx);
    98 Kernel32Readfile "kernel32.dll" "readfile" => kernel32::handle_read_file(ctx);
    99 Kernel32Writefile "kernel32.dll" "writefile" => kernel32::handle_write_file(ctx);
    100 User32Getcursorpos "user32.dll" "getcursorpos" => user32::handle_get_cursor_pos(ctx);
    101 User32Getsystemmetrics "user32.dll" "getsystemmetrics" => user32::handle_get_system_metrics(ctx);
    102 User32Monitorfromwindow "user32.dll" "monitorfromwindow" => user32::handle_monitor_from_window(ctx);
    103 User32Getmonitorinfoa "user32.dll" "getmonitorinfoa" => user32::handle_get_monitor_info_a(ctx);
    104 User32Getmonitorinfow "user32.dll" "getmonitorinfow" => user32::handle_get_monitor_info_w(ctx);
    105 User32Enumdisplaymonitors "user32.dll" "enumdisplaymonitors" => user32::handle_enum_display_monitors(ctx);
    106 User32Enumdisplaydevicesa "user32.dll" "enumdisplaydevicesa" => user32::handle_enum_display_devices_a(ctx);
    107 User32Enumdisplaydevicesw "user32.dll" "enumdisplaydevicesw" => user32::handle_enum_display_devices_w(ctx);
    108 User32Monitorfrompoint "user32.dll" "monitorfrompoint" => user32::handle_monitor_from_point(ctx);
    111 Comctl32Ordinal17 "comctl32.dll" "ordinal 17" => comctl32::handle_init_common_controls(ctx);
    112 User32Getwindowrect "user32.dll" "getwindowrect" => user32::handle_get_window_rect(ctx);
    113 User32Getdpiforwindow "user32.dll" "getdpiforwindow" => user32::handle_get_dpi_for_window(ctx);
    114 User32Postmessagea "user32.dll" "postmessagea" => user32::handle_post_message_a(ctx);
    115 User32Getsystemmetricsfordpi "user32.dll" "getsystemmetricsfordpi" => user32::handle_get_system_metrics_for_dpi(ctx);
    116 User32Adjustwindowrectexfordpi "user32.dll" "adjustwindowrectexfordpi" => user32::handle_adjust_window_rect_ex_for_dpi(ctx);
    117 User32Setwindowpos "user32.dll" "setwindowpos" => user32::handle_set_window_pos(ctx);
    118 User32Setscrollinfo "user32.dll" "setscrollinfo" => user32::handle_set_scroll_info(ctx);
    119 User32Scrollwindowex "user32.dll" "scrollwindowex" => user32::handle_scroll_window_ex(ctx);
    120 User32Scrolldc "user32.dll" "scrolldc" => user32::handle_scroll_dc(ctx);
    121 User32Beginpaint "user32.dll" "beginpaint" => user32::handle_begin_paint(ctx);
    122 User32Endpaint "user32.dll" "endpaint" => user32::handle_end_paint(ctx);
    123 User32Clipcursor "user32.dll" "clipcursor" => user32::handle_clip_cursor(ctx);
    124 User32Getclipcursor "user32.dll" "getclipcursor" => user32::handle_get_clip_cursor(ctx);
    125 User32Callmsgfiltera "user32.dll" "callmsgfiltera" => user32::handle_call_msg_filter(ctx, "CallMsgFilterA");
    126 User32Callmsgfilterw "user32.dll" "callmsgfilterw" => user32::handle_call_msg_filter(ctx, "CallMsgFilterW");
    127 User32Getdc "user32.dll" "getdc" => user32::handle_get_dc(ctx);
    128 User32Sendmessagea "user32.dll" "sendmessagea" => user32::handle_send_message_a(ctx);
    129 User32Sendmessagew "user32.dll" "sendmessagew" => user32::handle_send_message_w(ctx);
    130 Comdlg32Getopenfilenamea "comdlg32.dll" "getopenfilenamea" => comdlg32::handle_get_open_file_name_a(ctx);
    131 Comdlg32Getopenfilenamew "comdlg32.dll" "getopenfilenamew" => comdlg32::handle_get_open_file_name_w(ctx);
    132 Comdlg32Getsavefilenamea "comdlg32.dll" "getsavefilenamea" => comdlg32::handle_get_save_file_name_a(ctx);
    133 Comdlg32Getsavefilenamew "comdlg32.dll" "getsavefilenamew" => comdlg32::handle_get_save_file_name_w(ctx);
    134 Comdlg32Commdlgextendederror "comdlg32.dll" "commdlgextendederror" => comdlg32::handle_comm_dlg_extended_error(ctx);
    135 Comdlg32Choosecolora "comdlg32.dll" "choosecolora" => comdlg32::handle_choose_color_a(ctx);
    136 Gdi32Selectobject "gdi32.dll" "selectobject" => gdi32::handle_select_object(ctx);
    137 Gdi32Gettextextentpoint32a "gdi32.dll" "gettextextentpoint32a" => gdi32::handle_get_text_extent_point_32_a(ctx);
    138 Gdi32Gettextextentpoint32w "gdi32.dll" "gettextextentpoint32w" => gdi32::handle_get_text_extent_point_32_w(ctx);
    139 Gdi32Exttextoutw "gdi32.dll" "exttextoutw" => gdi32::handle_ext_text_out_w(ctx);
    140 User32Releasedc "user32.dll" "releasedc" => user32::handle_release_dc(ctx);
    141 Kernel32Getcurrentdirectoryw "kernel32.dll" "getcurrentdirectoryw" => kernel32::handle_get_current_directory_w(ctx);
    142 Kernel32Setcurrentdirectoryw "kernel32.dll" "setcurrentdirectoryw" => kernel32::handle_set_current_directory_w(ctx);
    143 User32Loadimagea "user32.dll" "loadimagea" => user32::handle_load_image_a(ctx);
    144 User32Loadimagew "user32.dll" "loadimagew" => user32::handle_load_image_w(ctx);
    145 Comctl32Initcommoncontrolsex "comctl32.dll" "initcommoncontrolsex" => comctl32::handle_init_common_controls_ex(ctx);
    146 UxthemeSetwindowtheme "uxtheme.dll" "setwindowtheme" => uxtheme::handle_set_window_theme(ctx);
    147 User32Setwindowlongptrw "user32.dll" "setwindowlongptrw" => user32::handle_set_window_long_ptr_w(ctx);
    148 User32Getwindowlongptra "user32.dll" "getwindowlongptra" => user32::handle_get_window_long_ptr_a(ctx);
    149 User32Getwindowlongptrw "user32.dll" "getwindowlongptrw" => user32::handle_get_window_long_ptr_w(ctx);
    150 Gdi32Getobjecta "gdi32.dll" "getobjecta" => gdi32::handle_get_object_a(ctx);
    151 Comctl32ImagelistCreate "comctl32.dll" "imagelist_create" => comctl32::handle_image_list_create(ctx);
    152 Gdi32Createcompatibledc "gdi32.dll" "createcompatibledc" => gdi32::handle_create_compatible_dc(ctx);
    153 Gdi32Createdibsection "gdi32.dll" "createdibsection" => gdi32::handle_create_dib_section(ctx);
    154 Gdi32Createcompatiblebitmap "gdi32.dll" "createcompatiblebitmap" => gdi32::handle_create_compatible_bitmap(ctx);
    155 Gdi32Getdevicecaps "gdi32.dll" "getdevicecaps" => gdi32::handle_get_device_caps(ctx);
    156 Gdi32Createfonta "gdi32.dll" "createfonta" => gdi32::handle_create_font_a(ctx);
    157 Gdi32Createfontw "gdi32.dll" "createfontw" => gdi32::handle_create_font_w(ctx);
    158 Gdi32Createfontindirecta "gdi32.dll" "createfontindirecta" => gdi32::handle_create_font_indirect_a(ctx);
    159 Gdi32Gettextmetricsa "gdi32.dll" "gettextmetricsa" => gdi32::handle_get_text_metrics_a(ctx);
    160 Gdi32Settextcolor "gdi32.dll" "settextcolor" => gdi32::handle_set_text_color(ctx);
    161 Gdi32Setbkcolor "gdi32.dll" "setbkcolor" => gdi32::handle_set_bk_color(ctx);
    162 Gdi32Setbkmode "gdi32.dll" "setbkmode" => gdi32::handle_set_bk_mode(ctx);
    163 Gdi32Textouta "gdi32.dll" "textouta" => gdi32::handle_text_out_a(ctx);
    164 Gdi32Bitblt "gdi32.dll" "bitblt" => gdi32::handle_bit_blt(ctx);
    165 Gdi32Stretchblt "gdi32.dll" "stretchblt" => gdi32::handle_stretch_blt(ctx);
    166 Gdi32Patblt "gdi32.dll" "patblt" => gdi32::handle_pat_blt(ctx);
    167 Gdi32Getpixel "gdi32.dll" "getpixel" => gdi32::handle_get_pixel(ctx);
    168 Gdi32Deletedc "gdi32.dll" "deletedc" => gdi32::handle_delete_dc(ctx);
    169 Comctl32ImagelistAddmasked "comctl32.dll" "imagelist_addmasked" => comctl32::handle_image_list_add_masked(ctx);
    170 Comctl32ImagelistSetbkcolor "comctl32.dll" "imagelist_setbkcolor" => comctl32::handle_image_list_set_bk_color(ctx);
    171 Comctl32ImagelistDestroy "comctl32.dll" "imagelist_destroy" => comctl32::handle_image_list_destroy(ctx);
    172 Gdi32Deleteobject "gdi32.dll" "deleteobject" => gdi32::handle_delete_object(ctx);
    173 User32Destroyicon "user32.dll" "destroyicon" => user32::handle_destroy_icon(ctx);
    174 User32Iswindow "user32.dll" "iswindow" => user32::handle_is_window(ctx);
    175 User32Iswindowvisible "user32.dll" "iswindowvisible" => user32::handle_is_window_visible(ctx);
    176 User32Iswindowenabled "user32.dll" "iswindowenabled" => user32::handle_is_window_enabled(ctx);
    177 User32Getparent "user32.dll" "getparent" => user32::handle_get_parent(ctx);
    178 User32Getactivewindow "user32.dll" "getactivewindow" => user32::handle_get_active_window(ctx);
    179 User32Getforegroundwindow "user32.dll" "getforegroundwindow" => user32::handle_get_foreground_window(ctx);
    180 User32Showwindow "user32.dll" "showwindow" => user32::handle_show_window(ctx);
    181 User32Enablewindow "user32.dll" "enablewindow" => user32::handle_enable_window(ctx);
    182 User32Setforegroundwindow "user32.dll" "setforegroundwindow" => user32::handle_set_foreground_window(ctx);
    183 User32Setactivewindow "user32.dll" "setactivewindow" => user32::handle_set_active_window(ctx);
    184 User32Setfocus "user32.dll" "setfocus" => user32::handle_set_focus(ctx);
    185 User32Getfocus "user32.dll" "getfocus" => user32::handle_get_focus(ctx);
    186 User32Setcapture "user32.dll" "setcapture" => user32::handle_set_capture(ctx);
    187 User32Getcapture "user32.dll" "getcapture" => user32::handle_get_capture(ctx);
    188 User32Releasecapture "user32.dll" "releasecapture" => user32::handle_release_capture(ctx);
    189 User32Setcursor "user32.dll" "setcursor" => user32::handle_set_cursor(ctx);
    190 User32Updatewindow "user32.dll" "updatewindow" => user32::handle_update_window(ctx);
    191 User32Invalidaterect "user32.dll" "invalidaterect" => user32::handle_invalidate_rect(ctx);
    192 User32Redrawwindow "user32.dll" "redrawwindow" => user32::handle_redraw_window(ctx);
    193 User32Setwindowtexta "user32.dll" "setwindowtexta" => user32::handle_set_window_text_a(ctx);
    194 User32Setwindowtextw "user32.dll" "setwindowtextw" => user32::handle_set_window_text_w(ctx);
    195 User32Getwindowtexta "user32.dll" "getwindowtexta" => user32::handle_get_window_text_a(ctx);
    196 User32Getwindowtextw "user32.dll" "getwindowtextw" => user32::handle_get_window_text_w(ctx);
    197 User32Getclientrect "user32.dll" "getclientrect" => user32::handle_get_client_rect(ctx);
    198 User32Movewindow "user32.dll" "movewindow" => user32::handle_move_window(ctx);
    199 User32Screentoclient "user32.dll" "screentoclient" => user32::handle_screen_to_client(ctx);
    200 User32Clienttoscreen "user32.dll" "clienttoscreen" => user32::handle_client_to_screen(ctx);
    201 User32Getdesktopwindow "user32.dll" "getdesktopwindow" => user32::handle_get_desktop_window(ctx);
    202 User32Getsyscolor "user32.dll" "getsyscolor" => user32::handle_get_sys_color(ctx);
    203 User32Getsyscolorbrush "user32.dll" "getsyscolorbrush" => user32::handle_get_sys_color_brush(ctx);
    204 User32Getdialogbaseunits "user32.dll" "getdialogbaseunits" => user32::handle_get_dialog_base_units(ctx);
    205 User32Setrect "user32.dll" "setrect" => user32::handle_set_rect(ctx);
    206 User32Isiconic "user32.dll" "isiconic" => user32::handle_is_iconic(ctx);
    207 User32Iszoomed "user32.dll" "iszoomed" => user32::handle_is_zoomed(ctx);
    208 User32Getwindowthreadprocessid "user32.dll" "getwindowthreadprocessid" => user32::handle_get_window_thread_process_id(ctx);
    209 User32Getdlgctrlid "user32.dll" "getdlgctrlid" => user32::handle_get_dlg_ctrl_id(ctx);
    210 Kernel32Getcurrentprocess "kernel32.dll" "getcurrentprocess" => kernel32::handle_get_current_process(ctx);
    211 Kernel32Sleep "kernel32.dll" "sleep" => kernel32::handle_sleep(ctx);
    212 WinmmTimegettime "winmm.dll" "timegettime" => winmm::handle_time_get_time(ctx);
    213 Kernel32Localalloc "kernel32.dll" "localalloc" => kernel32::handle_local_alloc(ctx);
    214 Kernel32Localfree "kernel32.dll" "localfree" => kernel32::handle_local_free(ctx);
    215 Kernel32Globalalloc "kernel32.dll" "globalalloc" => kernel32::handle_global_alloc(ctx);
    216 Kernel32Globalfree "kernel32.dll" "globalfree" => kernel32::handle_global_free(ctx);
    217 Kernel32Globallock "kernel32.dll" "globallock" => kernel32::handle_global_lock(ctx);
    218 Kernel32Globalunlock "kernel32.dll" "globalunlock" => kernel32::handle_global_unlock(ctx);
    219 Kernel32Globalsize "kernel32.dll" "globalsize" => kernel32::handle_global_size(ctx);
    220 Kernel32Muldiv "kernel32.dll" "muldiv" => kernel32::handle_mul_div(ctx);
    221 User32Getcursor "user32.dll" "getcursor" => user32::handle_get_cursor(ctx);
    222 User32Ischild "user32.dll" "ischild" => user32::handle_is_child(ctx);
    223 User32Getwindow "user32.dll" "getwindow" => user32::handle_get_window(ctx);
    224 User32Setkeyboardstate "user32.dll" "setkeyboardstate" => user32::handle_set_keyboard_state(ctx);
    225 User32Getkeyboardstate "user32.dll" "getkeyboardstate" => user32::handle_get_keyboard_state(ctx);
    226 User32Getkeystate "user32.dll" "getkeystate" => user32::handle_get_key_state(ctx);
    227 User32Mapvirtualkeya "user32.dll" "mapvirtualkeya" => user32::handle_map_virtual_key_a(ctx);
    228 User32Setwindowlongptra "user32.dll" "setwindowlongptra" => user32::handle_set_window_long_ptr_a(ctx);
    229 User32Settimer "user32.dll" "settimer" => user32::handle_set_timer(ctx);
    230 User32Killtimer "user32.dll" "killtimer" => user32::handle_kill_timer(ctx);
    231 User32Adjustwindowrectex "user32.dll" "adjustwindowrectex" => user32::handle_adjust_window_rect_ex(ctx);
    232 Kernel32Globaladdatoma "kernel32.dll" "globaladdatoma" => kernel32::handle_global_add_atom_a(ctx);
    233 Kernel32Globaldeleteatom "kernel32.dll" "globaldeleteatom" => kernel32::handle_global_delete_atom(ctx);
    234 User32Setwindowshookexw "user32.dll" "setwindowshookexw" => user32::handle_set_windows_hook_ex_w(ctx);
    235 User32Unhookwindowshookex "user32.dll" "unhookwindowshookex" => user32::handle_unhook_windows_hook_ex(ctx);
    236 User32Callnexthookex "user32.dll" "callnexthookex" => user32::handle_call_next_hook_ex(ctx);
    237 Kernel32Getfullpathnamew "kernel32.dll" "getfullpathnamew" => kernel32::handle_get_full_path_name_w(ctx);
    238 D3d9Direct3dcreate9 "d3d9.dll" "direct3dcreate9" => d3d9::handle_direct3d_create9(ctx);
    239 D3d9Idirect3d9Getadaptercount "d3d9.dll" "idirect3d9::getadaptercount" => d3d9::handle_get_adapter_count(ctx);
    240 D3d9Idirect3d9Getadaptermonitor "d3d9.dll" "idirect3d9::getadaptermonitor" => d3d9::handle_get_adapter_monitor(ctx);
    241 D3d9Idirect3d9Getdevicecaps "d3d9.dll" "idirect3d9::getdevicecaps" => d3d9::handle_get_device_caps(ctx);
    242 D3d9Idirect3d9Getadapterdisplaymode "d3d9.dll" "idirect3d9::getadapterdisplaymode" => d3d9::handle_get_adapter_display_mode(ctx);
    243 D3d9Idirect3d9Createdevice "d3d9.dll" "idirect3d9::createdevice" => d3d9::handle_create_device(ctx);
    244 D3d9Idirect3ddevice9Setvertexshader "d3d9.dll" "idirect3ddevice9::setvertexshader" => d3d9::handle_set_vertex_shader(ctx);
    245 D3d9Idirect3ddevice9Setfvf "d3d9.dll" "idirect3ddevice9::setfvf" => d3d9::handle_set_fvf(ctx);
    246 D3d9Idirect3ddevice9Setrenderstate "d3d9.dll" "idirect3ddevice9::setrenderstate" => d3d9::handle_set_render_state(ctx);
    247 D3d9Idirect3ddevice9Settexturestagestate "d3d9.dll" "idirect3ddevice9::settexturestagestate" => d3d9::handle_set_texture_stage_state(ctx);
    248 D3d9Idirect3ddevice9Setsamplerstate "d3d9.dll" "idirect3ddevice9::setsamplerstate" => d3d9::handle_set_sampler_state(ctx);
    249 User32Enablemenuitem "user32.dll" "enablemenuitem" => user32::handle_enable_menu_item(ctx);
    250 User32Checkmenuitem "user32.dll" "checkmenuitem" => user32::handle_check_menu_item(ctx);
    251 User32Getmessagea "user32.dll" "getmessagea" => user32::handle_get_message_a(ctx);
    252 User32Translatemessage "user32.dll" "translatemessage" => user32::handle_translate_message(ctx);
    253 User32Defwindowproca "user32.dll" "defwindowproca" => user32::handle_def_window_proc_a(ctx);
    254 User32Defwindowprocw "user32.dll" "defwindowprocw" => user32::handle_def_window_proc_w(ctx);
    255 User32Defframeproca "user32.dll" "defframeproca" => user32::handle_def_frame_proc_a(ctx);
    256 User32Defframeprocw "user32.dll" "defframeprocw" => user32::handle_def_frame_proc_w(ctx);
    257 User32Defmdichildproca "user32.dll" "defmdichildproca" => user32::handle_def_mdi_child_proc_a(ctx);
    258 User32Defmdichildprocw "user32.dll" "defmdichildprocw" => user32::handle_def_mdi_child_proc_w(ctx);
    259 User32Createmenu "user32.dll" "createmenu" => user32::handle_create_menu(ctx);
    260 User32Createpopupmenu "user32.dll" "createpopupmenu" => user32::handle_create_popup_menu(ctx);
    261 User32Appendmenua "user32.dll" "appendmenua" => user32::handle_append_menu_a(ctx);
    262 User32Appendmenuw "user32.dll" "appendmenuw" => user32::handle_append_menu_w(ctx);
    263 User32Setmenu "user32.dll" "setmenu" => user32::handle_set_menu(ctx);
    264 User32Destroymenu "user32.dll" "destroymenu" => user32::handle_destroy_menu(ctx);
    265 User32Removemenu "user32.dll" "removemenu" => user32::handle_remove_menu(ctx);
    266 User32Deletemenu "user32.dll" "deletemenu" => user32::handle_delete_menu(ctx);
    267 User32Modifymenua "user32.dll" "modifymenua" => user32::handle_modify_menu_a(ctx);
    268 User32Modifymenuw "user32.dll" "modifymenuw" => user32::handle_modify_menu_w(ctx);
    269 User32Getsystemmenu "user32.dll" "getsystemmenu" => user32::handle_get_system_menu(ctx);
    270 User32Trackpopupmenu "user32.dll" "trackpopupmenu" => user32::handle_track_popup_menu(ctx);
    271 User32Getmenuiteminfoa "user32.dll" "getmenuiteminfoa" => user32::handle_get_menu_item_info_a(ctx);
    272 User32Getmenuiteminfow "user32.dll" "getmenuiteminfow" => user32::handle_get_menu_item_info_w(ctx);
    273 User32Setmenuiteminfoa "user32.dll" "setmenuiteminfoa" => user32::handle_set_menu_item_info_a(ctx);
    274 User32Setmenuiteminfow "user32.dll" "setmenuiteminfow" => user32::handle_set_menu_item_info_w(ctx);
    275 User32Checkmenuradioitem "user32.dll" "checkmenuradioitem" => user32::handle_check_menu_radio_item(ctx);
    276 User32Dispatchmessagea "user32.dll" "dispatchmessagea" => user32::handle_dispatch_message_a(ctx);
    277 D3d9Idirect3ddevice9Release "d3d9.dll" "idirect3ddevice9::release" => d3d9::handle_device_release(ctx);
    278 D3d9Idirect3d9Release "d3d9.dll" "idirect3d9::release" => d3d9::handle_direct3d9_release(ctx);
    279 Kernel32Getfullpathnamea "kernel32.dll" "getfullpathnamea" => kernel32::handle_get_full_path_name_a(ctx);
    280 Kernel32Getcurrentdirectorya "kernel32.dll" "getcurrentdirectorya" => kernel32::handle_get_current_directory_a(ctx);
    281 Kernel32Setcurrentdirectorya "kernel32.dll" "setcurrentdirectorya" => kernel32::handle_set_current_directory_a(ctx);
    282 Kernel32Createdirectoryw "kernel32.dll" "createdirectoryw" => kernel32::handle_create_directory_w(ctx);
    283 Kernel32Createdirectorya "kernel32.dll" "createdirectorya" => kernel32::handle_create_directory_a(ctx);
    284 Kernel32Removefirectoryw "kernel32.dll" "removedirectoryw" => kernel32::handle_remove_directory_w(ctx);
    285 Kernel32Removefirectorya "kernel32.dll" "removedirectorya" => kernel32::handle_remove_directory_a(ctx);
    286 Kernel32Deletefilew "kernel32.dll" "deletefilew" => kernel32::handle_delete_file_w(ctx);
    287 Kernel32Deletefilea "kernel32.dll" "deletefilea" => kernel32::handle_delete_file_a(ctx);
    288 Kernel32Movefilew "kernel32.dll" "movefilew" => kernel32::handle_move_file_w(ctx);
    289 Kernel32Movefilea "kernel32.dll" "movefilea" => kernel32::handle_move_file_a(ctx);
    290 Kernel32Gettemppathw "kernel32.dll" "gettemppathw" => kernel32::handle_get_temp_path_w(ctx);
    291 Kernel32Gettemppatha "kernel32.dll" "gettemppatha" => kernel32::handle_get_temp_path_a(ctx);
    292 Kernel32Gettempfilenamew "kernel32.dll" "gettempfilenamew" => kernel32::handle_get_temp_file_name_w(ctx);
    293 Kernel32Gettempfilenamea "kernel32.dll" "gettempfilenamea" => kernel32::handle_get_temp_file_name_a(ctx);
    294 Kernel32Getdrivetypew "kernel32.dll" "getdrivetypew" => kernel32::handle_get_drive_type_w(ctx);
    295 Kernel32Getdrivetypea "kernel32.dll" "getdrivetypea" => kernel32::handle_get_drive_type_a(ctx);
    296 Kernel32Getlogicaldrives "kernel32.dll" "getlogicaldrives" => kernel32::handle_get_logical_drives(ctx);
    297 Kernel32Getsystemdirectoryw "kernel32.dll" "getsystemdirectoryw" => kernel32::handle_get_system_directory_w(ctx);
    298 Kernel32Getsystemdirectorya "kernel32.dll" "getsystemdirectorya" => kernel32::handle_get_system_directory_a(ctx);
    299 Kernel32Getwindowsdirectoryw "kernel32.dll" "getwindowsdirectoryw" => kernel32::handle_get_windows_directory_w(ctx);
    300 Kernel32Getwindowsdirectorya "kernel32.dll" "getwindowsdirectorya" => kernel32::handle_get_windows_directory_a(ctx);
    301 Kernel32Getfilesizeex "kernel32.dll" "getfilesizeex" => kernel32::handle_get_file_size_ex(ctx);
    302 Kernel32Setfilepointerex "kernel32.dll" "setfilepointerex" => kernel32::handle_set_file_pointer_ex(ctx);
    303 Kernel32Setendoffile "kernel32.dll" "setendoffile" => kernel32::handle_set_end_of_file(ctx);
    304 Kernel32Flushfilebuffers "kernel32.dll" "flushfilebuffers" => kernel32::handle_flush_file_buffers(ctx);
    305 User32Getmenu "user32.dll" "getmenu" => user32::handle_get_menu(ctx);
    306 Gdi32Getstockobject "gdi32.dll" "getstockobject" => gdi32::handle_get_stock_object(ctx);
    307 Kernel32Writeconsolew "kernel32.dll" "writeconsolew" => kernel32::console::handle_write_console_w(ctx);
    308 Kernel32Writeconsolea "kernel32.dll" "writeconsolea" => kernel32::console::handle_write_console_a(ctx);
    309 Kernel32Readconsolew "kernel32.dll" "readconsolew" => kernel32::console::handle_read_console_w(ctx);
    310 Kernel32Readconsolea "kernel32.dll" "readconsolea" => kernel32::console::handle_read_console_a(ctx);
    311 Kernel32Getconsolemode "kernel32.dll" "getconsolemode" => kernel32::console::handle_get_console_mode(ctx);
    312 Kernel32Setconsolemode "kernel32.dll" "setconsolemode" => kernel32::console::handle_set_console_mode(ctx);
    313 Kernel32Writeconsoleoutputw "kernel32.dll" "writeconsoleoutputw" => kernel32::console_cells::handle_write_console_output_w(ctx);
    314 Kernel32Fillconsoleoutputcharacterw "kernel32.dll" "fillconsoleoutputcharacterw" => kernel32::console_cells::handle_fill_console_output_character_w(ctx);
    315 Kernel32Setconsolecursorposition "kernel32.dll" "setconsolecursorposition" => kernel32::console_cells::handle_set_console_cursor_position(ctx);
    316 Kernel32Setconsoletextattribute "kernel32.dll" "setconsoletextattribute" => kernel32::console_cells::handle_set_console_text_attribute(ctx);
    317 Kernel32Gettickcount64 "kernel32.dll" "gettickcount64" => kernel32::misc::handle_get_tick_count_64(ctx);
    318 Kernel32Getenvironmentvariablew "kernel32.dll" "getenvironmentvariablew" => kernel32::environment::handle_get_environment_variable_w(ctx);
    319 Kernel32Setenvironmentvariablew "kernel32.dll" "setenvironmentvariablew" => kernel32::environment::handle_set_environment_variable_w(ctx);
    320 User32Createwindowexa "user32.dll" "createwindowexa" => user32::handle_create_window_ex_a(ctx);
    321 User32Createwindowexw "user32.dll" "createwindowexw" => user32::handle_create_window_ex_w(ctx);
    322 User32Destroywindow "user32.dll" "destroywindow" => user32::handle_destroy_window(ctx);
    323 User32Postquitmessage "user32.dll" "postquitmessage" => user32::handle_post_quit_message(ctx);
    324 User32Getmessagew "user32.dll" "getmessagew" => user32::handle_get_message_w(ctx);
    325 User32Peekmessagew "user32.dll" "peekmessagew" => user32::handle_peek_message_w(ctx);
    326 User32Dispatchmessagew "user32.dll" "dispatchmessagew" => user32::handle_dispatch_message_w(ctx);
    327 User32Postmessagew "user32.dll" "postmessagew" => user32::handle_post_message_w(ctx);
    328 User32Registerclassa "user32.dll" "registerclassa" => user32::handle_register_class_a(ctx);
    329 User32Registerclassw "user32.dll" "registerclassw" => user32::handle_register_class_w(ctx);
    330 User32Unregisterclassa "user32.dll" "unregisterclassa" => user32::handle_unregister_class_a(ctx);
    331 User32Unregisterclassw "user32.dll" "unregisterclassw" => user32::handle_unregister_class_w(ctx);
    332 User32Validateirect "user32.dll" "validaterect" => user32::handle_validate_rect(ctx);
    333 User32Setwindowlonga "user32.dll" "setwindowlonga" => user32::handle_set_window_long_a(ctx);
    334 User32Setwindowlongw "user32.dll" "setwindowlongw" => user32::handle_set_window_long_w(ctx);
    335 User32Getwindowdc "user32.dll" "getwindowdc" => user32::handle_get_window_dc(ctx);
    336 User32Getclassnamea "user32.dll" "getclassnamea" => user32::handle_get_class_name_a(ctx);
    337 User32Getclassnamew "user32.dll" "getclassnamew" => user32::handle_get_class_name_w(ctx);
    338 User32Getclasslongptra "user32.dll" "getclasslongptra" => user32::handle_get_class_long_ptr_a(ctx);
    339 User32Getclasslongptrw "user32.dll" "getclasslongptrw" => user32::handle_get_class_long_ptr_w(ctx);
    340 User32Setclasslongptra "user32.dll" "setclasslongptra" => user32::handle_set_class_long_ptr_a(ctx);
    341 User32Setclasslongptrw "user32.dll" "setclasslongptrw" => user32::handle_set_class_long_ptr_w(ctx);
    342 User32Drawmenubar "user32.dll" "drawmenubar" => user32::handle_draw_menu_bar(ctx);
    343 User32Getmenustate "user32.dll" "getmenustate" => user32::handle_get_menu_state(ctx);
    344 Gdi32Createsolidbrush "gdi32.dll" "createsolidbrush" => gdi32::handle_create_solid_brush(ctx);
    345 User32Fillrect "user32.dll" "fillrect" => gdi32::handle_fill_rect(ctx);
    346 Gdi32Createpen "gdi32.dll" "createpen" => gdi32::handle_create_pen(ctx);
    347 Gdi32Textoutw "gdi32.dll" "textoutw" => gdi32::handle_text_out_w(ctx);
    348 Gdi32Drawtexta "user32.dll" "drawtexta" => gdi32::handle_draw_text_a(ctx);
    349 Gdi32Drawtextw "user32.dll" "drawtextw" => gdi32::handle_draw_text_w(ctx);
    350 User32Createdialogparama "user32.dll" "createdialogparama" => user32::handle_create_dialog_param_a(ctx);
    351 User32Createdialogparamw "user32.dll" "createdialogparamw" => user32::handle_create_dialog_param_w(ctx);
    352 User32Isdialogmessagea "user32.dll" "isdialogmessagea" => user32::handle_is_dialog_message_a(ctx);
    353 User32Isdialogmessagew "user32.dll" "isdialogmessagew" => user32::handle_is_dialog_message_w(ctx);
    354 User32Enddialog "user32.dll" "enddialog" => user32::handle_end_dialog(ctx);
    355 User32Getdlgitema "user32.dll" "getdlgitem" "getdlgitema" => user32::handle_get_dlg_item_a(ctx);
    356 User32Getdlgitemw "user32.dll" "getdlgitemw" => user32::handle_get_dlg_item_w(ctx);
    357 User32Getdlgitemtexta "user32.dll" "getdlgitemtexta" => user32::handle_get_dlg_item_text_a(ctx);
    358 User32Getdlgitemtextw "user32.dll" "getdlgitemtextw" => user32::handle_get_dlg_item_text_w(ctx);
    359 User32Setdlgitemtexta "user32.dll" "setdlgitemtexta" => user32::handle_set_dlg_item_text_a(ctx);
    360 User32Setdlgitemtextw "user32.dll" "setdlgitemtextw" => user32::handle_set_dlg_item_text_w(ctx);
    361 User32Defdlgproca "user32.dll" "defdlgproca" => user32::handle_def_dlg_proc_a(ctx);
    362 User32Defdlgprocw "user32.dll" "defdlgprocw" => user32::handle_def_dlg_proc_w(ctx);
    363 D3d9Idirect3ddevice9Present "d3d9.dll" "idirect3ddevice9::present" => d3d9::handle_present(ctx);
    364 D3d9Idirect3ddevice9Beginscene "d3d9.dll" "idirect3ddevice9::beginscene" => d3d9::handle_begin_scene(ctx);
    365 D3d9Idirect3ddevice9Endscene "d3d9.dll" "idirect3ddevice9::endscene" => d3d9::handle_end_scene(ctx);
    366 D3d9Idirect3ddevice9Clear "d3d9.dll" "idirect3ddevice9::clear" => d3d9::handle_clear(ctx);
    367 D3d9Idirect3ddevice9Settransform "d3d9.dll" "idirect3ddevice9::settransform" => d3d9::handle_set_transform(ctx);
    368 D3d9Idirect3ddevice9Setviewport "d3d9.dll" "idirect3ddevice9::setviewport" => d3d9::handle_set_viewport(ctx);
    369 D3d9Idirect3ddevice9Getviewport "d3d9.dll" "idirect3ddevice9::getviewport" => d3d9::handle_get_viewport(ctx);
    370 D3d9Idirect3ddevice9Drawprimitive "d3d9.dll" "idirect3ddevice9::drawprimitive" => d3d9::handle_draw_primitive(ctx);
    371 D3d9Idirect3ddevice9Drawindexedprimitive "d3d9.dll" "idirect3ddevice9::drawindexedprimitive" => d3d9::handle_draw_indexed_primitive(ctx);
    372 D3d9Idirect3ddevice9Drawprimitiveup "d3d9.dll" "idirect3ddevice9::drawprimitiveup" => d3d9::handle_draw_primitive_up(ctx);
    373 D3d9Idirect3ddevice9Drawindexedprimitiveup "d3d9.dll" "idirect3ddevice9::drawindexedprimitiveup" => d3d9::handle_draw_indexed_primitive_up(ctx);
    374 D3d9Idirect3ddevice9Setstreamsource "d3d9.dll" "idirect3ddevice9::setstreamsource" => d3d9::handle_set_stream_source(ctx);
    375 D3d9Idirect3ddevice9Setindices "d3d9.dll" "idirect3ddevice9::setindices" => d3d9::handle_set_indices(ctx);
    376 D3d9Idirect3ddevice9Createvertexbuffer "d3d9.dll" "idirect3ddevice9::createvertexbuffer" => d3d9::handle_create_vertex_buffer(ctx);
    377 D3d9Idirect3ddevice9Createindexbuffer "d3d9.dll" "idirect3ddevice9::createindexbuffer" => d3d9::handle_create_index_buffer(ctx);
    378 D3d9Idirect3ddevice9Createtexture "d3d9.dll" "idirect3ddevice9::createtexture" => d3d9::handle_create_texture(ctx);
    379 D3d9Idirect3ddevice9Settexture "d3d9.dll" "idirect3ddevice9::settexture" => d3d9::handle_set_texture(ctx);
    380 D3d9Idirect3ddevice9Gettexture "d3d9.dll" "idirect3ddevice9::gettexture" => d3d9::handle_get_texture(ctx);
    381 D3d9Idirect3ddevice9Gettexturestagestate "d3d9.dll" "idirect3ddevice9::gettexturestagestate" => d3d9::handle_get_texture_stage_state(ctx);
    382 D3d9Idirect3ddevice9Getsamplerstate "d3d9.dll" "idirect3ddevice9::getsamplerstate" => d3d9::handle_get_sampler_state(ctx);
    383 D3d9Idirect3dtexture9Getlevelcount "d3d9.dll" "idirect3dtexture9::getlevelcount" => d3d9::handle_texture_get_level_count(ctx);
    384 D3d9Idirect3dtexture9Getsurfacelevel "d3d9.dll" "idirect3dtexture9::getsurfacelevel" => d3d9::handle_texture_get_surface_level(ctx);
    385 D3d9Idirect3dtexture9Lockrect "d3d9.dll" "idirect3dtexture9::lockrect" => d3d9::handle_texture_lock_rect(ctx);
    386 D3d9Idirect3dtexture9Unlockrect "d3d9.dll" "idirect3dtexture9::unlockrect" => d3d9::handle_texture_unlock_rect(ctx);
    387 D3d9Idirect3dtexture9Release "d3d9.dll" "idirect3dtexture9::release" => d3d9::handle_texture_release(ctx);
    388 D3d9Idirect3dsurface9Lockrect "d3d9.dll" "idirect3dsurface9::lockrect" => d3d9::handle_surface_lock_rect(ctx);
    389 D3d9Idirect3dsurface9Unlockrect "d3d9.dll" "idirect3dsurface9::unlockrect" => d3d9::handle_surface_unlock_rect(ctx);
    390 D3d9Idirect3dsurface9Release "d3d9.dll" "idirect3dsurface9::release" => d3d9::handle_surface_release(ctx);
    391 D3d9Idirect3ddevice9Createdepthstencilsurface "d3d9.dll" "idirect3ddevice9::createdepthstencilsurface" => d3d9::handle_create_depth_stencil_surface(ctx);
    392 D3d9Idirect3ddevice9Setdepthstencilsurface "d3d9.dll" "idirect3ddevice9::setdepthstencilsurface" => d3d9::handle_set_depth_stencil_surface(ctx);
    393 D3d9Idirect3ddevice9Getdepthstencilsurface "d3d9.dll" "idirect3ddevice9::getdepthstencilsurface" => d3d9::handle_get_depth_stencil_surface(ctx);
    394 D3d9Idirect3ddevice9Getrenderstate "d3d9.dll" "idirect3ddevice9::getrenderstate" => d3d9::handle_get_render_state(ctx);
    395 D3d9Idirect3ddevice9Createvertexshader "d3d9.dll" "idirect3ddevice9::createvertexshader" => d3d9::handle_create_vertex_shader(ctx);
    396 D3d9Idirect3ddevice9Getvertexshader "d3d9.dll" "idirect3ddevice9::getvertexshader" => d3d9::handle_get_vertex_shader(ctx);
    397 D3d9Idirect3ddevice9Setvertexshaderconstantf "d3d9.dll" "idirect3ddevice9::setvertexshaderconstantf" => d3d9::handle_set_vertex_shader_constant_f(ctx);
    398 D3d9Idirect3ddevice9Getvertexshaderconstantf "d3d9.dll" "idirect3ddevice9::getvertexshaderconstantf" => d3d9::handle_get_vertex_shader_constant_f(ctx);
    399 D3d9Idirect3ddevice9Createpixelshader "d3d9.dll" "idirect3ddevice9::createpixelshader" => d3d9::handle_create_pixel_shader(ctx);
    400 D3d9Idirect3ddevice9Setpixelshader "d3d9.dll" "idirect3ddevice9::setpixelshader" => d3d9::handle_set_pixel_shader(ctx);
    401 D3d9Idirect3ddevice9Getpixelshader "d3d9.dll" "idirect3ddevice9::getpixelshader" => d3d9::handle_get_pixel_shader(ctx);
    402 D3d9Idirect3ddevice9Setpixelshaderconstantf "d3d9.dll" "idirect3ddevice9::setpixelshaderconstantf" => d3d9::handle_set_pixel_shader_constant_f(ctx);
    403 D3d9Idirect3ddevice9Getpixelshaderconstantf "d3d9.dll" "idirect3ddevice9::getpixelshaderconstantf" => d3d9::handle_get_pixel_shader_constant_f(ctx);
    404 D3d9Idirect3dpixelshader9Release "d3d9.dll" "idirect3dpixelshader9::release" => d3d9::handle_pixel_shader_release(ctx);
    405 D3d9Idirect3dvertexshader9Release "d3d9.dll" "idirect3dvertexshader9::release" => d3d9::handle_vertex_shader_release(ctx);
    406 Kernel32Getstartupinfow "kernel32.dll" "getstartupinfow" => kernel32::handle_get_startup_info_w(ctx);
    407 Kernel32Getuserdefaultuilanguage "kernel32.dll" "getuserdefaultuilanguage" => kernel32::handle_get_user_default_ui_language(ctx);
    408 User32Registerwindowmessagea "user32.dll" "registerwindowmessagea" => user32::handle_register_window_message_a(ctx);
    409 User32Registerwindowmessagew "user32.dll" "registerwindowmessagew" => user32::handle_register_window_message_w(ctx);
    410 User32Loadstringa "user32.dll" "loadstringa" => user32::handle_load_string_a(ctx);
    411 User32Loadstringw "user32.dll" "loadstringw" => user32::handle_load_string_w(ctx);
    412 Advapi32Regopenkeya "advapi32.dll" "regopenkeya" => advapi32::handle_reg_open_key_a(ctx);
    413 Advapi32Regopenkeyw "advapi32.dll" "regopenkeyw" => advapi32::handle_reg_open_key_w(ctx);
    414 Gdi32Createfontindirectw "gdi32.dll" "createfontindirectw" => gdi32::handle_create_font_indirect_w(ctx);
    415 User32Loadiconw "user32.dll" "loadiconw" => user32::handle_load_icon_w(ctx);
    416 User32Loadcursorw "user32.dll" "loadcursorw" => user32::handle_load_cursor_w(ctx);
    417 Shell32Dragacceptfiles "shell32.dll" "dragacceptfiles" => shell32::handle_drag_accept_files(ctx);
    418 Comctl32Createstatuswindowa "comctl32.dll" "createstatuswindowa" => comctl32::handle_create_status_window_a(ctx);
    419 Comctl32Createstatuswindoww "comctl32.dll" "createstatuswindoww" => comctl32::handle_create_status_window_w(ctx);
    420 Comdlg32Getfiletitlea "comdlg32.dll" "getfiletitlea" => comdlg32::handle_get_file_title_a(ctx);
    421 Comdlg32Getfiletitlew "comdlg32.dll" "getfiletitlew" => comdlg32::handle_get_file_title_w(ctx);
    422 User32Getwindowtextlengtha "user32.dll" "getwindowtextlengtha" => user32::handle_get_window_text_length_a(ctx);
    423 User32Getwindowtextlengthw "user32.dll" "getwindowtextlengthw" => user32::handle_get_window_text_length_w(ctx);
    424 User32Getwindowplacement "user32.dll" "getwindowplacement" => user32::handle_get_window_placement(ctx);
    425 User32Setwindowplacement "user32.dll" "setwindowplacement" => user32::handle_set_window_placement(ctx);
    426 User32Loadacceleratorsa "user32.dll" "loadacceleratorsa" => user32::handle_load_accelerators_a(ctx);
    427 User32Loadacceleratorsw "user32.dll" "loadacceleratorsw" => user32::handle_load_accelerators_w(ctx);
    428 User32Translateacceleratora "user32.dll" "translateacceleratora" => user32::handle_translate_accelerator_a(ctx);
    429 User32Translateacceleratorw "user32.dll" "translateacceleratorw" => user32::handle_translate_accelerator_w(ctx);
    430 User32Destroyacceleratortable "user32.dll" "destroyacceleratortable" => user32::handle_destroy_accelerator_table(ctx);
    431 User32Loadmenua "user32.dll" "loadmenua" => user32::handle_load_menu_a(ctx);
    432 User32Loadmenuw "user32.dll" "loadmenuw" => user32::handle_load_menu_w(ctx);
    433 User32Isclipboardformatavailable "user32.dll" "isclipboardformatavailable" => crate::clipboard::handle_is_clipboard_format_available(ctx);
    434 Shell32Dragqueryfilew "shell32.dll" "dragqueryfilew" => user32::handle_drag_query_file_w(ctx);
    435 Shell32Dragqueryfilea "shell32.dll" "dragqueryfilea" => user32::handle_drag_query_file_a(ctx);
    436 Shell32Dragquerypoint "shell32.dll" "dragquerypoint" => user32::handle_drag_query_point(ctx);
    437 Shell32Dragfinish "shell32.dll" "dragfinish" => user32::handle_drag_finish(ctx);
    438 Kernel32Locallock "kernel32.dll" "locallock" => kernel32::handle_local_lock(ctx);
    439 Kernel32Localunlock "kernel32.dll" "localunlock" => kernel32::handle_local_unlock(ctx);
    440 Kernel32Gettimeformatw "kernel32.dll" "gettimeformatw" => kernel32::handle_get_time_format_w(ctx);
    441 Kernel32Getdateformatw "kernel32.dll" "getdateformatw" => kernel32::handle_get_date_format_w(ctx);
    442 User32Getdlgitemint "user32.dll" "getdlgitemint" => user32::handle_get_dlg_item_int(ctx);
    443 User32Setdlgitemint "user32.dll" "setdlgitemint" => user32::handle_set_dlg_item_int(ctx);
    444 User32Senddlgitemmessagew "user32.dll" "senddlgitemmessagew" => user32::handle_send_dlg_item_message_w(ctx);
    445 Shell32Shellaboutw "shell32.dll" "shellaboutw" => shell32::handle_shell_about_w(ctx);
    446 Shell32Shellexecutew "shell32.dll" "shellexecutew" => shell32::handle_shell_execute_w(ctx);
    447 User32Callwindowproca "user32.dll" "callwindowproca" => user32::handle_call_window_proc_a(ctx);
    448 User32Callwindowprocw "user32.dll" "callwindowprocw" => user32::handle_call_window_proc_w(ctx);
    449 Comdlg32Findtextw "comdlg32.dll" "findtextw" => comdlg32::handle_find_text_w(ctx);
    450 Comdlg32Replacetextw "comdlg32.dll" "replacetextw" => comdlg32::handle_replace_text_w(ctx);
    451 Comdlg32Choosefontw "comdlg32.dll" "choosefontw" => comdlg32::handle_choose_font_w(ctx);
    452 Comdlg32Printdlgw "comdlg32.dll" "printdlgw" => comdlg32::handle_print_dlg_w(ctx);
    453 Comdlg32Pagesetupdlgw "comdlg32.dll" "pagesetupdlgw" => comdlg32::handle_page_setup_dlg_w(ctx);
    454 Gdi32Startdocw "gdi32.dll" "startdocw" => gdi32::handle_start_doc_w(ctx);
    455 Gdi32Startpage "gdi32.dll" "startpage" => gdi32::handle_start_page(ctx);
    456 Gdi32Endpage "gdi32.dll" "endpage" => gdi32::handle_end_page(ctx);
    457 Gdi32Enddoc "gdi32.dll" "enddoc" => gdi32::handle_end_doc(ctx);
    458 Gdi32Abortdoc "gdi32.dll" "abortdoc" => gdi32::handle_abort_doc(ctx);
    459 Gdi32Createdcw "gdi32.dll" "createdcw" => gdi32::handle_create_dc_w(ctx);
    460 Gdi32Gettextmetricsw "gdi32.dll" "gettextmetricsw" => gdi32::handle_get_text_metrics_w(ctx);
    461 Gdi32Setmapmode "gdi32.dll" "setmapmode" => gdi32::handle_set_map_mode(ctx);
    462 Gdi32Rectangle "gdi32.dll" "rectangle" => gdi32::handle_rectangle(ctx);
    463 User32Inflaterect "user32.dll" "inflaterect" => user32::handle_inflate_rect(ctx);
    464 D3d9Idirect3dvertexbuffer9Queryinterface "d3d9.dll" "idirect3dvertexbuffer9::queryinterface" => d3d9::handle_vertex_buffer_query_interface(ctx);
    465 D3d9Idirect3dvertexbuffer9Addref "d3d9.dll" "idirect3dvertexbuffer9::addref" => d3d9::handle_vertex_buffer_add_ref(ctx);
    466 D3d9Idirect3dvertexbuffer9Release "d3d9.dll" "idirect3dvertexbuffer9::release" => d3d9::handle_vertex_buffer_release(ctx);
    467 D3d9Idirect3dvertexbuffer9Lock "d3d9.dll" "idirect3dvertexbuffer9::lock" => d3d9::handle_vertex_buffer_lock(ctx);
    468 D3d9Idirect3dvertexbuffer9Unlock "d3d9.dll" "idirect3dvertexbuffer9::unlock" => d3d9::handle_vertex_buffer_unlock(ctx);
    469 D3d9Idirect3dvertexbuffer9Getdesc "d3d9.dll" "idirect3dvertexbuffer9::getdesc" => d3d9::handle_vertex_buffer_get_desc(ctx);
    470 D3d9Idirect3dindexbuffer9Queryinterface "d3d9.dll" "idirect3dindexbuffer9::queryinterface" => d3d9::handle_index_buffer_query_interface(ctx);
    471 D3d9Idirect3dindexbuffer9Addref "d3d9.dll" "idirect3dindexbuffer9::addref" => d3d9::handle_index_buffer_add_ref(ctx);
    472 D3d9Idirect3dindexbuffer9Release "d3d9.dll" "idirect3dindexbuffer9::release" => d3d9::handle_index_buffer_release(ctx);
    473 D3d9Idirect3dindexbuffer9Lock "d3d9.dll" "idirect3dindexbuffer9::lock" => d3d9::handle_index_buffer_lock(ctx);
    474 D3d9Idirect3dindexbuffer9Unlock "d3d9.dll" "idirect3dindexbuffer9::unlock" => d3d9::handle_index_buffer_unlock(ctx);
    475 D3d9Idirect3dindexbuffer9Getdesc "d3d9.dll" "idirect3dindexbuffer9::getdesc" => d3d9::handle_index_buffer_get_desc(ctx);
    476 D3d9Idirect3ddevice9Getstreamsource "d3d9.dll" "idirect3ddevice9::getstreamsource" => d3d9::handle_get_stream_source(ctx);
    477 D3d9Idirect3ddevice9Getindices "d3d9.dll" "idirect3ddevice9::getindices" => d3d9::handle_get_indices(ctx);
    478 VersionGetfileversioninfosizew "version.dll" "getfileversioninfosizew" => version::handle_get_file_version_info_size_w(ctx);
    479 VersionGetfileversioninfosizea "version.dll" "getfileversioninfosizea" => version::handle_get_file_version_info_size_a(ctx);
    480 VersionGetfileversioninfosizeexw "version.dll" "getfileversioninfosizeexw" => version::handle_get_file_version_info_size_ex_w(ctx);
    481 VersionGetfileversioninfosizeexa "version.dll" "getfileversioninfosizeexa" => version::handle_get_file_version_info_size_ex_a(ctx);
    482 VersionGetfileversioninfow "version.dll" "getfileversioninfow" => version::handle_get_file_version_info_w(ctx);
    483 VersionGetfileversioninfoa "version.dll" "getfileversioninfoa" => version::handle_get_file_version_info_a(ctx);
    484 VersionGetfileversioninfoexw "version.dll" "getfileversioninfoexw" => version::handle_get_file_version_info_ex_w(ctx);
    485 VersionGetfileversioninfoexa "version.dll" "getfileversioninfoexa" => version::handle_get_file_version_info_ex_a(ctx);
    486 VersionVerqueryvaluew "version.dll" "verqueryvaluew" => version::handle_ver_query_value_w(ctx);
    487 VersionVerqueryvaluea "version.dll" "verqueryvaluea" => version::handle_ver_query_value_a(ctx);
    488 VersionGetfileversioninfobyhandlew "version.dll" "getfileversioninfobyhandlew" => version::handle_get_file_version_info_by_handle_w(ctx);
    489 VersionGetfileversioninfobyhandlea "version.dll" "getfileversioninfobyhandlea" => version::handle_get_file_version_info_by_handle_a(ctx);
    490 VersionVerlanguagenamew "version.dll" "verlanguagenamew" => version::handle_ver_language_name_w(ctx);
    491 VersionVerlanguagenamea "version.dll" "verlanguagenamea" => version::handle_ver_language_name_a(ctx);
    492 VersionVerfindfilew "version.dll" "verfindfilew" => version::handle_ver_find_file_w(ctx);
    493 VersionVerfindfilea "version.dll" "verfindfilea" => version::handle_ver_find_file_a(ctx);
    494 VersionVerinstallfilew "version.dll" "verinstallfilew" => version::handle_ver_install_file_w(ctx);
    495 VersionVerinstallfilea "version.dll" "verinstallfilea" => version::handle_ver_install_file_a(ctx);
    496 D3d9Idirect3ddevice9Gettransform "d3d9.dll" "idirect3ddevice9::gettransform" => d3d9::handle_get_transform(ctx);
    497 D3d9Idirect3ddevice9Multiplytransform "d3d9.dll" "idirect3ddevice9::multiplytransform" => d3d9::handle_multiply_transform(ctx);
    498 D3d9Idirect3ddevice9Setscissorrect "d3d9.dll" "idirect3ddevice9::setscissorrect" => d3d9::handle_set_scissor_rect(ctx);
    499 D3d9Idirect3ddevice9Setvertexshaderconstanti "d3d9.dll" "idirect3ddevice9::setvertexshaderconstanti" => d3d9::handle_set_vertex_shader_constant_i(ctx);
    500 D3d9Idirect3ddevice9Getvertexshaderconstanti "d3d9.dll" "idirect3ddevice9::getvertexshaderconstanti" => d3d9::handle_get_vertex_shader_constant_i(ctx);
    501 D3d9Idirect3ddevice9Setvertexshaderconstantb "d3d9.dll" "idirect3ddevice9::setvertexshaderconstantb" => d3d9::handle_set_vertex_shader_constant_b(ctx);
    502 D3d9Idirect3ddevice9Getvertexshaderconstantb "d3d9.dll" "idirect3ddevice9::getvertexshaderconstantb" => d3d9::handle_get_vertex_shader_constant_b(ctx);
    503 D3d9Idirect3ddevice9Createrendertarget "d3d9.dll" "idirect3ddevice9::createrendertarget" => d3d9::handle_create_render_target(ctx);
    504 D3d9Idirect3ddevice9Setrendertarget "d3d9.dll" "idirect3ddevice9::setrendertarget" => d3d9::handle_set_render_target(ctx);
    505 D3d9Idirect3ddevice9Getrendertarget "d3d9.dll" "idirect3ddevice9::getrendertarget" => d3d9::handle_get_render_target(ctx);
    506 D3d9Idirect3dsurface9Getdesc "d3d9.dll" "idirect3dsurface9::getdesc" => d3d9::handle_surface_get_desc(ctx);
}
