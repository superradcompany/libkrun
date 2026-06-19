// Copyright 2026 Super Rad Company.
// SPDX-License-Identifier: Apache-2.0

//! Windows eventfd-style wake primitive.
//!
//! Windows does not have file descriptors or Linux `eventfd`. This type preserves the small API
//! surface libkrun uses for wake notifications while exposing the underlying waitable handle to
//! Windows event loops.

use std::io;
use std::os::windows::io::{AsRawHandle, RawHandle};
use std::sync::Arc;

use windows_sys::Win32::Foundation::{
    CloseHandle, HANDLE, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::System::Threading::{
    CreateEventW, ResetEvent, SetEvent, WaitForSingleObject,
};

pub const EFD_NONBLOCK: i32 = 1;
pub const EFD_SEMAPHORE: i32 = 2;

const ZERO_TIMEOUT_MS: u32 = 0;

#[derive(Debug)]
pub struct EventFd {
    inner: Arc<EventHandle>,
}

#[derive(Debug)]
struct EventHandle {
    handle: HANDLE,
}

impl EventFd {
    pub fn new(_flag: i32) -> io::Result<Self> {
        let handle = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }

        Ok(Self {
            inner: Arc::new(EventHandle { handle }),
        })
    }

    pub fn write(&self, _v: u64) -> io::Result<()> {
        if unsafe { SetEvent(self.inner.handle) } == 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(())
    }

    pub fn read(&self) -> io::Result<u64> {
        match unsafe { WaitForSingleObject(self.inner.handle, ZERO_TIMEOUT_MS) } {
            WAIT_OBJECT_0 => {
                if unsafe { ResetEvent(self.inner.handle) } == 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(1)
            }
            WAIT_TIMEOUT => Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "event is not signaled",
            )),
            WAIT_FAILED => Err(io::Error::last_os_error()),
            _ => Err(io::Error::last_os_error()),
        }
    }

    pub fn try_clone(&self) -> io::Result<EventFd> {
        Ok(Self {
            inner: self.inner.clone(),
        })
    }

    pub fn get_write_handle(&self) -> RawHandle {
        self.inner.handle as RawHandle
    }
}

impl AsRawHandle for EventFd {
    fn as_raw_handle(&self) -> RawHandle {
        self.inner.handle as RawHandle
    }
}

impl Drop for EventHandle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.handle);
        }
    }
}

unsafe impl Send for EventFd {}
unsafe impl Sync for EventFd {}
