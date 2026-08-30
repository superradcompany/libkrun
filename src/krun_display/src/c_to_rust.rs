use crate::{
    CursorImage, DisplayBackendBasicFramebuffer, DisplayBackendCursor, DisplayBackendError,
    DisplayBasicFramebufferVtable, DisplayFeatures, DisplayVtable, Rect, ResourceFormat, header,
};
use log::{error, warn};
use static_assertions::assert_not_impl_any;
use std::ffi::c_void;
use std::marker::PhantomData;
use std::ptr::{null, null_mut, slice_from_raw_parts_mut};

#[macro_export]
macro_rules! into_rust_result {
    ($expr:expr) => {
        into_rust_result!($expr,
            0 => Ok(()),
            code @ 0.. => {
                log::warn!("{}: Unknown OK result code: {code}", stringify!($expr));
                Ok(())
            }
        )
    };
    ($expr:expr, $($pat:pat $(if $pat_guard:expr)? => $pat_expr:expr),+ ) => {
        match $expr {
            $($pat $(if $pat_guard)? => $pat_expr,)+
            -1 => Err(DisplayBackendError::InternalError),
            -3 => Err(DisplayBackendError::InvalidScanoutId),
            -4 => Err(DisplayBackendError::InvalidParam),
            code @ i32::MIN.. => {
                log::warn!("{}: Unknown error result code: {code}", stringify!($expr));
                Err(DisplayBackendError::InternalError)
            }
        }
    };
}

macro_rules! method_call {
    ($self:ident.$method:ident($($args:expr),*) ) => {
        unsafe {
            $self.vtable.$method
                .ok_or(DisplayBackendError::MethodNotSupported)?( $self.instance, $($args),* )
        }
    };
}

pub struct DisplayBackendInstance {
    instance: *mut c_void,
    vtable: DisplayBasicFramebufferVtable,
    /// The features the backend advertised; optional methods are only called
    /// when their bit is set, even if the pointer happens to be non-NULL.
    features: DisplayFeatures,
}

// By design the struct is !Send and !Sync to allow for the implementation to safely assume that
// the methods are always called on the GPU worker thread
assert_not_impl_any!(DisplayBackendInstance: Sync, Send);

impl Drop for DisplayBackendInstance {
    fn drop(&mut self) {
        let Some(destroy_fn) = self.vtable.destroy else {
            return;
        };

        if let Err(e) = into_rust_result!(unsafe { destroy_fn(self.instance) }) {
            error!("Failed to destroy krun_gtk_display instance: {e}");
        }
    }
}

impl DisplayBackendBasicFramebuffer for DisplayBackendInstance {
    fn configure_scanout(
        &mut self,
        scanout_id: u32,
        display_width: u32,
        display_height: u32,
        width: u32,
        height: u32,
        format: ResourceFormat,
    ) -> Result<(), DisplayBackendError> {
        into_rust_result!(method_call! {
        self.configure_scanout(
            scanout_id,
            display_width,
            display_height,
            width,
            height,
            format as u32
        )
        })
    }

    fn disable_scanout(&mut self, scanout_id: u32) -> Result<(), DisplayBackendError> {
        into_rust_result! {
            method_call! {
                self.disable_scanout(scanout_id)
            }
        }
    }

    // Soundness note: this method has to take &mut self in order for the lifetime of the returned
    // slice to be tied to self. This way the returned slice cannot stay borrowed once another method
    // on self is called.
    fn alloc_frame(&mut self, scanout_id: u32) -> Result<(u32, &mut [u8]), DisplayBackendError> {
        let mut buffer: *mut u8 = null_mut();
        let mut buffer_len: usize = 0;
        let frame_id = into_rust_result! {
            method_call! {
                self.alloc_frame(scanout_id, &raw mut buffer, &raw mut buffer_len)
            },
            result @ 0.. => Ok(result as u32)
        }?;

        assert_ne!(buffer, null_mut());
        assert_ne!(buffer_len, 0);
        // SAFETY: We have obtained the buffer and buffer_len from the krun_gtk_display impl. Because
        //         the alloc_frame_fn return an error we assume they should be valid.
        let buffer = unsafe {
            slice_from_raw_parts_mut(buffer, buffer_len)
                .as_mut()
                .unwrap()
        };
        Ok((frame_id, buffer))
    }

    fn present_frame(
        &mut self,
        scanout_id: u32,
        frame_id: u32,
        rect: Option<&Rect>,
    ) -> Result<(), DisplayBackendError> {
        into_rust_result! {
            method_call!{
                self.present_frame(scanout_id, frame_id, rect.map(|r| r as *const _).unwrap_or(null()))
            }
        }
    }
}

impl DisplayBackendCursor for DisplayBackendInstance {
    fn set_cursor(
        &mut self,
        scanout_id: u32,
        image: Option<CursorImage<'_>>,
    ) -> Result<(), DisplayBackendError> {
        if !self.features.contains(DisplayFeatures::CURSOR) {
            return Err(DisplayBackendError::MethodNotSupported);
        }
        // A zero-sized image with a NULL pointer is how the ABI hides the cursor.
        let (width, height, format, data, hot_x, hot_y) = match image {
            Some(image) => (
                image.width,
                image.height,
                image.format as u32,
                image.data,
                image.hot_x,
                image.hot_y,
            ),
            None => (0, 0, ResourceFormat::BGRA as u32, &[][..], 0, 0),
        };
        let ptr = if data.is_empty() {
            null()
        } else {
            data.as_ptr()
        };
        into_rust_result! {
            method_call! {
                self.set_cursor(scanout_id, width, height, format, ptr, data.len(), hot_x, hot_y)
            }
        }
    }

    fn move_cursor(&mut self, scanout_id: u32, x: i32, y: i32) -> Result<(), DisplayBackendError> {
        if !self.features.contains(DisplayFeatures::CURSOR) {
            return Err(DisplayBackendError::MethodNotSupported);
        }
        into_rust_result! {
            method_call! {
                self.move_cursor(scanout_id, x, y)
            }
        }
    }
}

#[derive(Copy, Clone)]
#[repr(C)]
pub struct DisplayBackend<'userdata> {
    pub features: u64,
    pub create_userdata: *const c_void,
    pub create_userdata_lifetime: PhantomData<&'userdata c_void>,
    pub create_fn: header::krun_display_create_fn,
    pub vtable: DisplayVtable,
}

/// `krun_display_basic_framebuffer_vtable` as it was originally published,
/// before any method was appended to it.
///
/// This is a frozen snapshot of the ABI and must never gain a field: it is
/// what [`MIN_BACKEND_SIZE`] measures, so that a caller built against the
/// oldest header is still accepted.
#[repr(C)]
#[derive(Copy, Clone)]
pub(crate) struct OriginalBasicFramebufferVtable {
    pub(crate) destroy: header::krun_display_destroy_fn,
    pub(crate) disable_scanout: header::krun_display_disable_scanout_fn,
    pub(crate) configure_scanout: header::krun_display_configure_scanout_fn,
    pub(crate) alloc_frame: header::krun_display_alloc_frame_fn,
    pub(crate) present_frame: header::krun_display_present_frame_fn,
}

/// The smallest `krun_display_backend` `krun_set_display_backend` accepts:
/// the size the struct had before any method was appended to its vtable.
///
/// Methods are only ever appended, so a caller built against an older
/// `libkrun_display.h` passes a shorter struct whose fields are all at the
/// offsets this version expects. Copying those bytes into a zeroed struct
/// leaves the methods it does not carry NULL and their feature bits unset,
/// which [`DisplayBackend::verify`] reads as "not supported".
///
/// This measures the fields ahead of the vtable as they are now, which is
/// sound only because they have not moved since the struct was published —
/// reordering or inserting one would break the ABI outright, not just this.
pub const MIN_BACKEND_SIZE: usize = std::mem::offset_of!(DisplayBackend<'static>, vtable)
    + size_of::<OriginalBasicFramebufferVtable>();

// The minimum only makes sense while the vtable has grown past its original
// size; nothing below is load-bearing if these are ever equal.
const _: () = assert!(MIN_BACKEND_SIZE < size_of::<DisplayBackend<'static>>());

impl DisplayBackend<'_> {
    /// Create a DisplayBackendInstance, the caller is responsible for only calling this on a
    /// properly constructed DisplayBackend struct.
    pub fn create_instance(&self) -> Result<DisplayBackendInstance, DisplayBackendError> {
        let mut instance = null_mut();
        if let Some(create_fn) = self.create_fn {
            into_rust_result!(unsafe {
                create_fn(&raw mut instance, self.create_userdata, null())
            })?;
        }
        assert!(self.verify());

        Ok(DisplayBackendInstance {
            instance,
            // SAFETY: we have checked the feature flags, so basic_framebuffer should be populated
            vtable: unsafe { self.vtable.basic_framebuffer },
            features: DisplayFeatures::from_bits_truncate(self.features),
        })
    }

    /// Whether the backend struct is usable: every feature it advertises has
    /// its methods filled in.
    ///
    /// Methods are only ever appended to the vtable, so the existing fields keep
    /// their offsets and a struct that stops short of the cursor methods is
    /// accepted here: those fields are NULL and their feature bits unset, which
    /// is exactly the "backend does not support this" case.
    ///
    /// This is the half of the compatibility story that lives in the bindings.
    /// The other half is `krun_set_display_backend`, which accepts any
    /// `backend_size` down to [`MIN_BACKEND_SIZE`] and copies only that many
    /// bytes into a zeroed struct before calling this.
    pub fn verify(&self) -> bool {
        let features = DisplayFeatures::from_bits_retain(self.features);

        // This requirement might change in the future when we add support for alternatives to this
        if !features.contains(DisplayFeatures::BASIC_FRAMEBUFFER) {
            error!("This version of libkrun requires BASIC_FRAMEBUFFER feature");
            return false;
        }

        for feature in features {
            if feature.contains(DisplayFeatures::BASIC_FRAMEBUFFER) {
                // SAFETY: We have checked the feature flag is enabled, so we should be able to
                // access the union field.
                if unsafe {
                    self.vtable.basic_framebuffer.disable_scanout.is_none()
                        || self.vtable.basic_framebuffer.configure_scanout.is_none()
                        || self.vtable.basic_framebuffer.alloc_frame.is_none()
                        || self.vtable.basic_framebuffer.present_frame.is_none()
                } {
                    error!("Missing required methods for BASIC_FRAMEBUFFER");
                    return false;
                }
            } else if feature.contains(DisplayFeatures::CURSOR) {
                // SAFETY: BASIC_FRAMEBUFFER is required above, so the union
                // field is the one that is populated.
                if unsafe {
                    self.vtable.basic_framebuffer.set_cursor.is_none()
                        || self.vtable.basic_framebuffer.move_cursor.is_none()
                } {
                    error!("Missing required methods for CURSOR");
                    return false;
                }
            } else {
                warn!("Unknown display features ({feature:x}) will be ignored")
            }
        }
        true
    }
}

unsafe impl Send for DisplayBackend<'_> {}
