pub const KRUN_DISPLAY_ERR_INTERNAL: i32 = -1;
pub const KRUN_DISPLAY_ERR_METHOD_UNSUPPORTED: i32 = -2;
pub const KRUN_DISPLAY_ERR_INVALID_SCANOUT_ID: i32 = -3;
pub const KRUN_DISPLAY_ERR_INVALID_PARAM: i32 = -4;
pub const KRUN_DISPLAY_ERR_OUT_OF_BUFFERS: i32 = -5;

pub const KRUN_DISPLAY_FORMAT_B8G8R8A8_UNORM: u32 = 1;
pub const KRUN_DISPLAY_FORMAT_B8G8R8X8_UNORM: u32 = 2;
pub const KRUN_DISPLAY_FORMAT_A8R8G8B8_UNORM: u32 = 3;
pub const KRUN_DISPLAY_FORMAT_X8R8G8B8_UNORM: u32 = 4;
pub const KRUN_DISPLAY_FORMAT_R8G8B8A8_UNORM: u32 = 67;
pub const KRUN_DISPLAY_FORMAT_X8B8G8R8_UNORM: u32 = 68;
pub const KRUN_DISPLAY_FORMAT_A8B8G8R8_UNORM: u32 = 121;
pub const KRUN_DISPLAY_FORMAT_R8G8B8X8_UNORM: u32 = 134;

pub const KRUN_DISPLAY_FEATURE_BASIC_FRAMEBUFFER: u32 = 1;
pub const KRUN_DISPLAY_FEATURE_CURSOR: u32 = 2;

pub type krun_display_create_fn = Option<
    unsafe extern "C" fn(
        instance: *mut *mut core::ffi::c_void,
        userdata: *const core::ffi::c_void,
        reserved: *const core::ffi::c_void,
    ) -> i32,
>;
pub type krun_display_destroy_fn =
    Option<unsafe extern "C" fn(instance: *mut core::ffi::c_void) -> i32>;
pub type krun_display_configure_scanout_fn = Option<
    unsafe extern "C" fn(
        instance: *mut core::ffi::c_void,
        scanout_id: u32,
        display_width: u32,
        display_height: u32,
        width: u32,
        height: u32,
        format: u32,
    ) -> i32,
>;
pub type krun_display_disable_scanout_fn =
    Option<unsafe extern "C" fn(instance: *mut core::ffi::c_void, scanout_id: u32) -> i32>;
pub type krun_display_alloc_frame_fn = Option<
    unsafe extern "C" fn(
        instance: *mut core::ffi::c_void,
        scanout_id: u32,
        buffer: *mut *mut u8,
        buffer_size: *mut usize,
    ) -> i32,
>;

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct krun_rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

pub type krun_display_present_frame_fn = Option<
    unsafe extern "C" fn(
        instance: *mut core::ffi::c_void,
        scanout_id: u32,
        frame_id: u32,
        damage_area: *const krun_rect,
    ) -> i32,
>;

pub type krun_display_set_cursor_fn = Option<
    unsafe extern "C" fn(
        instance: *mut core::ffi::c_void,
        scanout_id: u32,
        width: u32,
        height: u32,
        format: u32,
        data: *const u8,
        data_size: usize,
        hot_x: u32,
        hot_y: u32,
    ) -> i32,
>;
pub type krun_display_move_cursor_fn = Option<
    unsafe extern "C" fn(
        instance: *mut core::ffi::c_void,
        scanout_id: u32,
        x: i32,
        y: i32,
    ) -> i32,
>;

#[repr(C)]
#[derive(Copy, Clone)]
pub struct krun_display_basic_framebuffer_vtable {
    pub destroy: krun_display_destroy_fn,
    pub disable_scanout: krun_display_disable_scanout_fn,
    pub configure_scanout: krun_display_configure_scanout_fn,
    pub alloc_frame: krun_display_alloc_frame_fn,
    pub present_frame: krun_display_present_frame_fn,
    pub set_cursor: krun_display_set_cursor_fn,
    pub move_cursor: krun_display_move_cursor_fn,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub union krun_display_vtable {
    pub basic_framebuffer: krun_display_basic_framebuffer_vtable,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct krun_display_backend {
    pub features: u64,
    pub create_userdata: *mut core::ffi::c_void,
    pub create: krun_display_create_fn,
    pub vtable: krun_display_vtable,
}
