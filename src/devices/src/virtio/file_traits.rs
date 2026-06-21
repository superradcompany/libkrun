// Copyright 2018 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::fs::File;
use std::io::{Error, ErrorKind, Result};
#[cfg(feature = "blk")]
use std::io::{IoSlice, IoSliceMut};
#[cfg(unix)]
use std::os::unix::io::AsRawFd;

#[cfg(feature = "blk")]
use imago::io_buffers::{IoVector, IoVectorMut};
#[cfg(all(feature = "blk", windows))]
use imago::{FormatReadPlanStep, StorageExt};
use vm_memory::VolatileSlice;

#[cfg(unix)]
use libc::{c_int, c_void, read, readv, size_t, write, writev};

#[cfg(unix)]
use super::bindings::{off64_t, pread64, preadv64, pwrite64, pwritev64};
#[cfg(feature = "blk")]
use super::block::device::DiskProperties;

/// A trait for setting the size of a file.
/// This is equivalent to File's `set_len` method, but
/// wrapped in a trait so that it can be implemented for
/// other types.
pub trait FileSetLen {
    // Set the size of this file.
    // This is the moral equivalent of `ftruncate()`.
    fn set_len(&self, _len: u64) -> Result<()>;
}

impl FileSetLen for File {
    fn set_len(&self, len: u64) -> Result<()> {
        File::set_len(self, len)
    }
}

/// A trait similar to `Read` and `Write`, but uses volatile memory as buffers.
pub trait FileReadWriteVolatile {
    /// Read bytes from this file into the given slice, returning the number of bytes read on
    /// success.
    fn read_volatile(&mut self, slice: VolatileSlice) -> Result<usize>;

    /// Like `read_volatile`, except it reads to a slice of buffers. Data is copied to fill each
    /// buffer in order, with the final buffer written to possibly being only partially filled. This
    /// method must behave as a single call to `read_volatile` with the buffers concatenated would.
    /// The default implementation calls `read_volatile` with either the first nonempty buffer
    /// provided, or returns `Ok(0)` if none exists.
    fn read_vectored_volatile(&mut self, bufs: &[VolatileSlice]) -> Result<usize> {
        bufs.iter()
            .find(|b| !b.is_empty())
            .map(|&b| self.read_volatile(b))
            .unwrap_or(Ok(0))
    }

    /// Reads bytes from this into the given slice until all bytes in the slice are written, or an
    /// error is returned.
    fn read_exact_volatile(&mut self, mut slice: VolatileSlice) -> Result<()> {
        while !slice.is_empty() {
            let bytes_read = self.read_volatile(slice)?;
            if bytes_read == 0 {
                return Err(Error::from(ErrorKind::UnexpectedEof));
            }
            // Will panic if read_volatile read more bytes than we gave it, which would be worthy of
            // a panic.
            slice = slice.offset(bytes_read).unwrap();
        }
        Ok(())
    }

    /// Write bytes from the slice to the given file, returning the number of bytes written on
    /// success.
    fn write_volatile(&mut self, slice: VolatileSlice) -> Result<usize>;

    /// Like `write_volatile`, except that it writes from a slice of buffers. Data is copied from
    /// each buffer in order, with the final buffer read from possibly being only partially
    /// consumed. This method must behave as a call to `write_volatile` with the buffers
    /// concatenated would. The default implementation calls `write_volatile` with either the first
    /// nonempty buffer provided, or returns `Ok(0)` if none exists.
    fn write_vectored_volatile(&mut self, bufs: &[VolatileSlice]) -> Result<usize> {
        bufs.iter()
            .find(|b| !b.is_empty())
            .map(|&b| self.write_volatile(b))
            .unwrap_or(Ok(0))
    }

    /// Write bytes from the slice to the given file until all the bytes from the slice have been
    /// written, or an error is returned.
    fn write_all_volatile(&mut self, mut slice: VolatileSlice) -> Result<()> {
        while !slice.is_empty() {
            let bytes_written = self.write_volatile(slice)?;
            if bytes_written == 0 {
                return Err(Error::from(ErrorKind::WriteZero));
            }
            // Will panic if read_volatile read more bytes than we gave it, which would be worthy of
            // a panic.
            slice = slice.offset(bytes_written).unwrap();
        }
        Ok(())
    }
}

impl<T: FileReadWriteVolatile + ?Sized> FileReadWriteVolatile for &mut T {
    fn read_volatile(&mut self, slice: VolatileSlice) -> Result<usize> {
        (**self).read_volatile(slice)
    }

    fn read_vectored_volatile(&mut self, bufs: &[VolatileSlice]) -> Result<usize> {
        (**self).read_vectored_volatile(bufs)
    }

    fn read_exact_volatile(&mut self, slice: VolatileSlice) -> Result<()> {
        (**self).read_exact_volatile(slice)
    }

    fn write_volatile(&mut self, slice: VolatileSlice) -> Result<usize> {
        (**self).write_volatile(slice)
    }

    fn write_vectored_volatile(&mut self, bufs: &[VolatileSlice]) -> Result<usize> {
        (**self).write_vectored_volatile(bufs)
    }

    fn write_all_volatile(&mut self, slice: VolatileSlice) -> Result<()> {
        (**self).write_all_volatile(slice)
    }
}

/// A trait similar to the unix `ReadExt` and `WriteExt` traits, but for volatile memory.
pub trait FileReadWriteAtVolatile {
    /// Reads bytes from this file at `offset` into the given slice, returning the number of bytes
    /// read on success.
    fn read_at_volatile(&self, slice: VolatileSlice, offset: u64) -> Result<usize>;

    /// Like `read_at_volatile`, except it reads to a slice of buffers. Data is copied to fill each
    /// buffer in order, with the final buffer written to possibly being only partially filled. This
    /// method must behave as a single call to `read_at_volatile` with the buffers concatenated
    /// would. The default implementation calls `read_at_volatile` with either the first nonempty
    /// buffer provided, or returns `Ok(0)` if none exists.
    fn read_vectored_at_volatile(&self, bufs: &[VolatileSlice], offset: u64) -> Result<usize> {
        if let Some(&slice) = bufs.first() {
            self.read_at_volatile(slice, offset)
        } else {
            Ok(0)
        }
    }

    /// Reads bytes from this file at `offset` into the given slice until all bytes in the slice are
    /// read, or an error is returned.
    fn read_exact_at_volatile(&self, mut slice: VolatileSlice, mut offset: u64) -> Result<()> {
        while !slice.is_empty() {
            match self.read_at_volatile(slice, offset) {
                Ok(0) => return Err(Error::from(ErrorKind::UnexpectedEof)),
                Ok(n) => {
                    slice = slice.offset(n).unwrap();
                    offset = offset.checked_add(n as u64).unwrap();
                }
                Err(ref e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// Writes bytes from this file at `offset` into the given slice, returning the number of bytes
    /// written on success.
    fn write_at_volatile(&self, slice: VolatileSlice, offset: u64) -> Result<usize>;

    /// Like `write_at_at_volatile`, except that it writes from a slice of buffers. Data is copied
    /// from each buffer in order, with the final buffer read from possibly being only partially
    /// consumed. This method must behave as a call to `write_at_volatile` with the buffers
    /// concatenated would. The default implementation calls `write_at_volatile` with either the
    /// first nonempty buffer provided, or returns `Ok(0)` if none exists.
    fn write_vectored_at_volatile(&self, bufs: &[VolatileSlice], offset: u64) -> Result<usize> {
        if let Some(&slice) = bufs.first() {
            self.write_at_volatile(slice, offset)
        } else {
            Ok(0)
        }
    }

    /// Writes bytes from this file at `offset` into the given slice until all bytes in the slice
    /// are written, or an error is returned.
    fn write_all_at_volatile(&self, mut slice: VolatileSlice, mut offset: u64) -> Result<()> {
        while !slice.is_empty() {
            match self.write_at_volatile(slice, offset) {
                Ok(0) => return Err(Error::from(ErrorKind::WriteZero)),
                Ok(n) => {
                    slice = slice.offset(n).unwrap();
                    offset = offset.checked_add(n as u64).unwrap();
                }
                Err(ref e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}

impl<T: FileReadWriteAtVolatile + ?Sized> FileReadWriteAtVolatile for &T {
    fn read_at_volatile(&self, slice: VolatileSlice, offset: u64) -> Result<usize> {
        (**self).read_at_volatile(slice, offset)
    }

    fn read_vectored_at_volatile(&self, bufs: &[VolatileSlice], offset: u64) -> Result<usize> {
        (**self).read_vectored_at_volatile(bufs, offset)
    }

    fn read_exact_at_volatile(&self, slice: VolatileSlice, offset: u64) -> Result<()> {
        (**self).read_exact_at_volatile(slice, offset)
    }

    fn write_at_volatile(&self, slice: VolatileSlice, offset: u64) -> Result<usize> {
        (**self).write_at_volatile(slice, offset)
    }

    fn write_vectored_at_volatile(&self, bufs: &[VolatileSlice], offset: u64) -> Result<usize> {
        (**self).write_vectored_at_volatile(bufs, offset)
    }

    fn write_all_at_volatile(&self, slice: VolatileSlice, offset: u64) -> Result<()> {
        (**self).write_all_at_volatile(slice, offset)
    }
}

#[cfg(unix)]
macro_rules! volatile_impl {
    ($ty:ty) => {
        impl FileReadWriteVolatile for $ty {
            fn read_volatile(&mut self, slice: VolatileSlice) -> Result<usize> {
                // Safe because only bytes inside the slice are accessed and the kernel is expected
                // to handle arbitrary memory for I/O.
                let ret = unsafe {
                    read(
                        self.as_raw_fd(),
                        slice.ptr_guard_mut().as_ptr() as *mut c_void,
                        slice.len(),
                    )
                };
                if ret >= 0 {
                    Ok(ret as usize)
                } else {
                    Err(Error::last_os_error())
                }
            }

            fn read_vectored_volatile(&mut self, bufs: &[VolatileSlice]) -> Result<usize> {
                let iovecs: Vec<libc::iovec> = bufs
                    .iter()
                    .map(|s| libc::iovec {
                        iov_base: s.ptr_guard_mut().as_ptr() as *mut c_void,
                        iov_len: s.len() as size_t,
                    })
                    .collect();

                if iovecs.is_empty() {
                    return Ok(0);
                }

                // Safe because only bytes inside the buffers are accessed and the kernel is
                // expected to handle arbitrary memory for I/O.
                let ret = unsafe { readv(self.as_raw_fd(), &iovecs[0], iovecs.len() as c_int) };
                if ret >= 0 {
                    Ok(ret as usize)
                } else {
                    Err(Error::last_os_error())
                }
            }

            fn write_volatile(&mut self, slice: VolatileSlice) -> Result<usize> {
                // Safe because only bytes inside the slice are accessed and the kernel is expected
                // to handle arbitrary memory for I/O.
                let ret = unsafe {
                    write(
                        self.as_raw_fd(),
                        slice.ptr_guard().as_ptr() as *const c_void,
                        slice.len(),
                    )
                };
                if ret >= 0 {
                    Ok(ret as usize)
                } else {
                    Err(Error::last_os_error())
                }
            }

            fn write_vectored_volatile(&mut self, bufs: &[VolatileSlice]) -> Result<usize> {
                let iovecs: Vec<libc::iovec> = bufs
                    .iter()
                    .map(|s| libc::iovec {
                        iov_base: s.ptr_guard_mut().as_ptr() as *mut c_void,
                        iov_len: s.len() as size_t,
                    })
                    .collect();

                if iovecs.is_empty() {
                    return Ok(0);
                }

                // Safe because only bytes inside the buffers are accessed and the kernel is
                // expected to handle arbitrary memory for I/O.
                let ret = unsafe { writev(self.as_raw_fd(), &iovecs[0], iovecs.len() as c_int) };
                if ret >= 0 {
                    Ok(ret as usize)
                } else {
                    Err(Error::last_os_error())
                }
            }
        }

        impl FileReadWriteAtVolatile for $ty {
            fn read_at_volatile(&self, slice: VolatileSlice, offset: u64) -> Result<usize> {
                // Safe because only bytes inside the slice are accessed and the kernel is expected
                // to handle arbitrary memory for I/O.
                let ret = unsafe {
                    pread64(
                        self.as_raw_fd(),
                        slice.ptr_guard_mut().as_ptr() as *mut c_void,
                        slice.len(),
                        offset as off64_t,
                    )
                };

                if ret >= 0 {
                    Ok(ret as usize)
                } else {
                    Err(Error::last_os_error())
                }
            }

            fn read_vectored_at_volatile(
                &self,
                bufs: &[VolatileSlice],
                offset: u64,
            ) -> Result<usize> {
                let iovecs: Vec<libc::iovec> = bufs
                    .iter()
                    .map(|s| libc::iovec {
                        iov_base: s.ptr_guard_mut().as_ptr() as *mut c_void,
                        iov_len: s.len() as size_t,
                    })
                    .collect();

                if iovecs.is_empty() {
                    return Ok(0);
                }

                // Safe because only bytes inside the buffers are accessed and the kernel is
                // expected to handle arbitrary memory for I/O.
                let ret = unsafe {
                    preadv64(
                        self.as_raw_fd(),
                        &iovecs[0],
                        iovecs.len() as c_int,
                        offset as off64_t,
                    )
                };
                if ret >= 0 {
                    Ok(ret as usize)
                } else {
                    Err(Error::last_os_error())
                }
            }

            fn write_at_volatile(&self, slice: VolatileSlice, offset: u64) -> Result<usize> {
                // Safe because only bytes inside the slice are accessed and the kernel is expected
                // to handle arbitrary memory for I/O.
                let ret = unsafe {
                    pwrite64(
                        self.as_raw_fd(),
                        slice.ptr_guard().as_ptr() as *const c_void,
                        slice.len(),
                        offset as off64_t,
                    )
                };

                if ret >= 0 {
                    Ok(ret as usize)
                } else {
                    Err(Error::last_os_error())
                }
            }

            fn write_vectored_at_volatile(
                &self,
                bufs: &[VolatileSlice],
                offset: u64,
            ) -> Result<usize> {
                let iovecs: Vec<libc::iovec> = bufs
                    .iter()
                    .map(|s| libc::iovec {
                        iov_base: s.ptr_guard_mut().as_ptr() as *mut c_void,
                        iov_len: s.len() as size_t,
                    })
                    .collect();

                if iovecs.is_empty() {
                    return Ok(0);
                }

                // Safe because only bytes inside the buffers are accessed and the kernel is
                // expected to handle arbitrary memory for I/O.
                let ret = unsafe {
                    pwritev64(
                        self.as_raw_fd(),
                        &iovecs[0],
                        iovecs.len() as c_int,
                        offset as off64_t,
                    )
                };
                if ret >= 0 {
                    Ok(ret as usize)
                } else {
                    Err(Error::last_os_error())
                }
            }
        }
    };
}

#[cfg(unix)]
volatile_impl!(File);

#[cfg(feature = "blk")]
impl FileReadWriteAtVolatile for DiskProperties {
    fn read_at_volatile(&self, slice: VolatileSlice, offset: u64) -> Result<usize> {
        self.read_vectored_at_volatile(&[slice], offset)
    }

    fn read_vectored_at_volatile(&self, bufs: &[VolatileSlice], offset: u64) -> Result<usize> {
        if bufs.is_empty() {
            return Ok(0);
        }

        #[cfg(windows)]
        if let Some(result) = self.windows_raw_read_vectored_at_volatile(bufs, offset) {
            return result;
        }

        #[cfg(windows)]
        if let Some(result) = self.windows_formatted_read_vectored_at_volatile(bufs, offset)? {
            return Ok(result);
        }

        let ptr_guards = bufs
            .iter()
            .map(|slice| slice.ptr_guard_mut())
            .collect::<Vec<_>>();
        let buffers = ptr_guards
            .iter()
            .map(|guard| {
                let slice = if guard.len() == 0 {
                    &mut []
                } else {
                    unsafe { std::slice::from_raw_parts_mut(guard.as_ptr(), guard.len()) }
                };
                IoSliceMut::new(slice)
            })
            .collect::<Vec<_>>();
        let iovec = IoVectorMut::from(buffers);
        let full_length = iovec
            .len()
            .try_into()
            .map_err(|e| Error::new(ErrorKind::InvalidData, e))?;
        self.file.lock().unwrap().readv(iovec, offset)?;
        Ok(full_length)
    }

    fn write_at_volatile(&self, slice: VolatileSlice, offset: u64) -> Result<usize> {
        self.write_vectored_at_volatile(&[slice], offset)
    }

    fn write_vectored_at_volatile(&self, bufs: &[VolatileSlice], offset: u64) -> Result<usize> {
        if bufs.is_empty() {
            return Ok(0);
        }

        #[cfg(windows)]
        if let Some(result) = self.windows_raw_write_vectored_at_volatile(bufs, offset) {
            return result;
        }

        let ptr_guards = bufs
            .iter()
            .map(|slice| slice.ptr_guard())
            .collect::<Vec<_>>();
        let buffers = ptr_guards
            .iter()
            .map(|guard| {
                let slice = if guard.len() == 0 {
                    &[]
                } else {
                    unsafe { std::slice::from_raw_parts(guard.as_ptr(), guard.len()) }
                };
                IoSlice::new(slice)
            })
            .collect::<Vec<_>>();
        let iovec = IoVector::from(buffers);
        let full_length = iovec
            .len()
            .try_into()
            .map_err(|e| Error::new(ErrorKind::InvalidData, e))?;
        self.file.lock().unwrap().writev(iovec, offset)?;
        Ok(full_length)
    }
}

#[cfg(all(feature = "blk", windows))]
impl DiskProperties {
    fn windows_formatted_read_vectored_at_volatile(
        &self,
        bufs: &[VolatileSlice],
        offset: u64,
    ) -> Result<Option<usize>> {
        let full_length = volatile_slices_len(bufs)?;
        if full_length == 0 {
            return Ok(Some(0));
        }

        let file = self.file.lock().unwrap();
        let plan = file.plan_read(offset, full_length)?;
        if plan
            .steps()
            .iter()
            .any(|step| !planned_read_step_supported(step))
        {
            return Ok(None);
        }

        let ptr_guards = bufs
            .iter()
            .map(|slice| slice.ptr_guard_mut())
            .collect::<Vec<_>>();
        let buffers = ptr_guards
            .iter()
            .map(|guard| {
                let slice = if guard.len() == 0 {
                    &mut []
                } else {
                    unsafe { std::slice::from_raw_parts_mut(guard.as_ptr(), guard.len()) }
                };
                IoSliceMut::new(slice)
            })
            .collect::<Vec<_>>();
        let mut remaining = IoVectorMut::from(buffers);

        for step in plan.steps() {
            let Some(len) = planned_read_step_len(step) else {
                return Ok(None);
            };
            let (mut chunk, rest) = remaining.split_at(len);
            remaining = rest;

            match step {
                FormatReadPlanStep::Raw {
                    storage,
                    offset: storage_offset,
                    ..
                } => self
                    .windows_formatted_read_runtime
                    .block_on(storage.readv(chunk, *storage_offset))?,
                FormatReadPlanStep::Zero { .. } | FormatReadPlanStep::Eof { .. } => {
                    chunk.fill(0);
                }
                _ => return Ok(None),
            }
        }

        Ok(Some(
            full_length
                .try_into()
                .map_err(|e| Error::new(ErrorKind::InvalidData, e))?,
        ))
    }
}

#[cfg(all(feature = "blk", windows))]
fn volatile_slices_len(bufs: &[VolatileSlice]) -> Result<u64> {
    bufs.iter().try_fold(0u64, |total, slice| {
        total
            .checked_add(slice.len() as u64)
            .ok_or_else(|| Error::new(ErrorKind::InvalidData, "volatile slice length overflow"))
    })
}

#[cfg(all(feature = "blk", windows))]
fn planned_read_step_supported(step: &FormatReadPlanStep<'_, Box<dyn imago::DynStorage>>) -> bool {
    matches!(
        step,
        FormatReadPlanStep::Raw { .. }
            | FormatReadPlanStep::Zero { .. }
            | FormatReadPlanStep::Eof { .. }
    )
}

#[cfg(all(feature = "blk", windows))]
fn planned_read_step_len(step: &FormatReadPlanStep<'_, Box<dyn imago::DynStorage>>) -> Option<u64> {
    match step {
        FormatReadPlanStep::Raw { len, .. }
        | FormatReadPlanStep::Zero { len, .. }
        | FormatReadPlanStep::Eof { len, .. } => Some(*len),
        _ => None,
    }
}

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[cfg(all(test, feature = "blk", windows))]
mod tests {
    use std::io::Write;
    use std::sync::{Arc, Mutex};
    use std::time::{SystemTime, UNIX_EPOCH};

    use imago::{raw::Raw, DynStorage, SyncFormatAccess};
    use vm_memory::{Bytes, GuestAddress, GuestMemoryBackend, GuestMemoryMmap};

    use crate::virtio::block::device::CacheType;

    use super::*;

    #[test]
    fn windows_formatted_read_plan_handles_raw_and_eof_ranges() {
        let path = temp_image_path("planned-raw-eof");
        let payload = (0..512).map(|byte| (byte % 251) as u8).collect::<Vec<_>>();

        let mut file = File::create(&path).unwrap();
        file.write_all(&payload).unwrap();
        file.set_len(payload.len() as u64).unwrap();
        drop(file);

        let raw = Raw::<Box<dyn DynStorage>>::open_path_sync(&path, true).unwrap();
        let disk_image = Arc::new(Mutex::new(SyncFormatAccess::new(raw).unwrap()));
        let disk = DiskProperties::new(disk_image, Vec::new(), CacheType::Unsafe).unwrap();

        let mem: GuestMemoryMmap<()> =
            GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x1000)]).unwrap();
        let target = mem.get_slice(GuestAddress(0x100), 1024).unwrap();

        assert_eq!(
            disk.windows_formatted_read_vectored_at_volatile(&[target], 0)
                .unwrap(),
            Some(1024)
        );

        let mut actual = vec![0xff; 1024];
        mem.read_slice(&mut actual, GuestAddress(0x100)).unwrap();
        assert_eq!(&actual[..payload.len()], &payload);
        assert_eq!(&actual[payload.len()..], vec![0; 512]);

        std::fs::remove_file(path).unwrap();
    }

    fn temp_image_path(test_name: &str) -> std::path::PathBuf {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();

        std::env::temp_dir().join(format!(
            "libkrun-formatted-block-{test_name}-{}-{timestamp}.img",
            std::process::id()
        ))
    }
}
