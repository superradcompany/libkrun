//! Source-only clock fixture; restore must use the real WHP DLL directly.

use std::{ffi::c_void, sync::OnceLock};

use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryA};

unsafe fn symbol(name: &[u8]) -> *const c_void {
    static LIB: OnceLock<usize> = OnceLock::new();
    let h = *LIB.get_or_init(|| LoadLibraryA(b"WinHvPlatformNative.dll\0".as_ptr()) as usize);
    GetProcAddress(h as _, name.as_ptr()).expect("native WHP symbol") as *const c_void
}

fn frequency() -> u64 {
    std::env::var("TEST_SOURCE_TSC_HZ")
        .unwrap()
        .parse()
        .unwrap()
}

#[no_mangle]
pub unsafe extern "system" fn WHvGetCapability(
    code: u32,
    out: *mut c_void,
    size: u32,
    written: *mut u32,
) -> i32 {
    let f: unsafe extern "system" fn(u32, *mut c_void, u32, *mut u32) -> i32 =
        std::mem::transmute(symbol(b"WHvGetCapability\0"));
    let hr = f(code, out, size, written);
    if hr >= 0 && code == 0x1004 && size >= 8 {
        *(out as *mut u64) = frequency();
    }
    hr
}

#[no_mangle]
pub unsafe extern "system" fn WHvSetupPartition(partition: isize) -> i32 {
    let set: unsafe extern "system" fn(isize, u32, *const c_void, u32) -> i32 =
        std::mem::transmute(symbol(b"WHvSetPartitionProperty\0"));
    let hz = frequency();
    let hr = set(partition, 0x1007, (&hz as *const u64).cast(), 8);
    if hr < 0 {
        return hr;
    }
    let setup: unsafe extern "system" fn(isize) -> i32 =
        std::mem::transmute(symbol(b"WHvSetupPartition\0"));
    let hr = setup(partition);
    if hr < 0 {
        return hr;
    }
    let get: unsafe extern "system" fn(isize, u32, *mut c_void, u32, *mut u32) -> i32 =
        std::mem::transmute(symbol(b"WHvGetPartitionProperty\0"));
    let mut actual = 0_u64;
    let mut written = 0;
    let hr = get(
        partition,
        0x1007,
        (&mut actual as *mut u64).cast(),
        8,
        &mut written,
    );
    if hr < 0 {
        return hr;
    }
    eprintln!("source partition clock: {actual} Hz");
    if written != 8 || actual != hz {
        return 0x80004005_u32 as i32;
    }
    0
}
