pub const KRUN_INPUT_ERR_INTERNAL: i32 = -1;
pub const KRUN_INPUT_ERR_EAGAIN: i32 = -2;
pub const KRUN_INPUT_ERR_METHOD_UNSUPPORTED: i32 = -3;
pub const KRUN_INPUT_ERR_INVALID_PARAM: i32 = -4;

pub const KRUN_INPUT_CONFIG_FEATURE_QUERY: u32 = 1;
pub const KRUN_INPUT_EVENT_PROVIDER_FEATURE_QUEUE: u32 = 1;

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct krun_input_event {
    pub type_: u16,
    pub code: u16,
    pub value: u32,
}

pub type krun_input_create_fn = Option<
    unsafe extern "C" fn(
        instance: *mut *mut core::ffi::c_void,
        userdata: *const core::ffi::c_void,
        reserved: *const core::ffi::c_void,
    ) -> i32,
>;

pub type krun_input_destroy_fn =
    Option<unsafe extern "C" fn(instance: *mut core::ffi::c_void) -> i32>;

pub type krun_input_get_ready_efd_fn =
    Option<unsafe extern "C" fn(instance: *mut core::ffi::c_void) -> i32>;

pub type krun_input_next_event_fn = Option<
    unsafe extern "C" fn(
        instance: *mut core::ffi::c_void,
        out_event: *mut krun_input_event,
    ) -> i32,
>;

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct krun_input_event_provider_vtable {
    pub destroy: krun_input_destroy_fn,
    pub get_ready_efd: krun_input_get_ready_efd_fn,
    pub next_event: krun_input_next_event_fn,
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct krun_input_device_ids {
    pub bustype: u16,
    pub vendor: u16,
    pub product: u16,
    pub version: u16,
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct krun_input_absinfo {
    pub min: u32,
    pub max: u32,
    pub fuzz: u32,
    pub flat: u32,
    pub res: u32,
}

pub type krun_input_query_device_name_fn = Option<
    unsafe extern "C" fn(
        instance: *mut core::ffi::c_void,
        name_buf: *mut u8,
        name_buf_len: usize,
    ) -> i32,
>;
pub type krun_input_query_serial_name_fn = krun_input_query_device_name_fn;
pub type krun_input_query_device_ids_fn = Option<
    unsafe extern "C" fn(
        instance: *mut core::ffi::c_void,
        ids: *mut krun_input_device_ids,
    ) -> i32,
>;
pub type krun_input_query_event_capabilities_fn = Option<
    unsafe extern "C" fn(
        instance: *mut core::ffi::c_void,
        event_type: u8,
        bitmap_buf: *mut u8,
        bitmap_buf_len: usize,
    ) -> i32,
>;
pub type krun_input_query_abs_info_fn = Option<
    unsafe extern "C" fn(
        instance: *mut core::ffi::c_void,
        abs_axis: u8,
        abs_info: *mut krun_input_absinfo,
    ) -> i32,
>;
pub type krun_input_query_properties_fn = Option<
    unsafe extern "C" fn(
        instance: *mut core::ffi::c_void,
        bitmap_buf: *mut u8,
        bitmap_buf_len: usize,
    ) -> i32,
>;

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct krun_input_config_vtable {
    pub destroy: krun_input_destroy_fn,
    pub query_device_name: krun_input_query_device_name_fn,
    pub query_serial_name: krun_input_query_serial_name_fn,
    pub query_device_ids: krun_input_query_device_ids_fn,
    pub query_event_capabilities: krun_input_query_event_capabilities_fn,
    pub query_abs_info: krun_input_query_abs_info_fn,
    pub query_properties: krun_input_query_properties_fn,
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct krun_input_config {
    pub features: u64,
    pub create_userdata: *mut core::ffi::c_void,
    pub create: krun_input_create_fn,
    pub vtable: krun_input_config_vtable,
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct krun_input_event_provider {
    pub features: u64,
    pub create_userdata: *mut core::ffi::c_void,
    pub create: krun_input_create_fn,
    pub vtable: krun_input_event_provider_vtable,
}
