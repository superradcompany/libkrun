use crate::{
    CursorImage, DisplayBackend, DisplayBackendError, DisplayBasicFramebufferVtable,
    DisplayFeatures, DisplayVtable, Rect, ResourceFormat,
};
use log::error;
use std::ffi::c_void;
use std::marker::PhantomData;
use std::ptr;
use std::ptr::{null, null_mut};
use std::slice;

pub trait DisplayBackendNew<T: Sync> {
    fn new(userdata: Option<&T>) -> Self;
}

pub trait DisplayBackendBasicFramebuffer {
    fn configure_scanout(
        &mut self,
        scanout_id: u32,
        display_width: u32,
        display_height: u32,
        width: u32,
        height: u32,
        format: ResourceFormat,
    ) -> Result<(), DisplayBackendError>;

    fn disable_scanout(&mut self, scanout_id: u32) -> Result<(), DisplayBackendError>;

    fn alloc_frame(&mut self, scanout_id: u32) -> Result<(u32, &mut [u8]), DisplayBackendError>;

    fn present_frame(
        &mut self,
        scanout_id: u32,
        frame_id: u32,
        rect: Option<&Rect>,
    ) -> Result<(), DisplayBackendError>;
}

/// Optional cursor plane support, advertised by [`DisplayFeatures::CURSOR`].
///
/// A backend implementing this next to [`DisplayBackendBasicFramebuffer`]
/// composites the guest's cursor itself, which spares the guest a full scanout
/// flush per pointer move. Turn such a backend into a [`DisplayBackend`] with
/// [`IntoDisplayBackendWithCursor::into_display_backend_with_cursor`];
/// [`IntoDisplayBackend::into_display_backend`] keeps working and simply does
/// not advertise the feature.
pub trait DisplayBackendCursor {
    /// Present `image` as the cursor of `scanout_id`, or hide the cursor when
    /// `image` is `None`.
    fn set_cursor(
        &mut self,
        scanout_id: u32,
        image: Option<CursorImage<'_>>,
    ) -> Result<(), DisplayBackendError>;

    /// The guest moved the cursor's hotspot to `(x, y)` in scanout pixels.
    fn move_cursor(&mut self, scanout_id: u32, x: i32, y: i32) -> Result<(), DisplayBackendError>;
}

pub trait IntoDisplayBackend<T: Sync> {
    fn into_display_backend(userdata: Option<&T>) -> DisplayBackend<'_>;
}

/// [`IntoDisplayBackend`] for backends that also implement
/// [`DisplayBackendCursor`].
pub trait IntoDisplayBackendWithCursor<T: Sync> {
    fn into_display_backend_with_cursor(userdata: Option<&T>) -> DisplayBackend<'_>;
}

extern "C" fn create_fn<T: Sync, I: DisplayBackendNew<T>>(
    instance: *mut *mut c_void,
    userdata: *const c_void,
    _reserved: *const c_void,
) -> i32 {
    unsafe {
        assert_ne!(
            instance,
            null_mut(),
            "Pointer to location where to create instance cannot be null"
        );
        let userdata_ref = (userdata as *const T).as_ref();
        *(instance as *mut *mut I) = Box::into_raw(Box::new(I::new(userdata_ref)));
    }
    0
}

extern "C" fn destroy_fn<I>(instance: *mut c_void) -> i32 {
    drop(unsafe { Box::from_raw(instance as *mut I) });
    0
}

fn cast_instance<'a, I>(instance: *mut c_void) -> &'a mut I {
    assert_ne!(instance, null_mut());
    unsafe { &mut *(instance as *mut I) }
}

/// The vtable of a [`DisplayBackendBasicFramebuffer`] implementation. The
/// cursor methods stay NULL; [`IntoDisplayBackendWithCursor`] fills them in.
fn basic_framebuffer_vtable<I: DisplayBackendBasicFramebuffer>() -> DisplayBasicFramebufferVtable {
    extern "C" fn configure_scanout_fn<I: DisplayBackendBasicFramebuffer>(
        instance: *mut c_void,
        scanout_id: u32,
        display_width: u32,
        display_height: u32,
        width: u32,
        height: u32,
        format: u32,
    ) -> i32 {
        let Ok(format) = ResourceFormat::try_from(format) else {
            error!("Unknown display format: {format}");
            return DisplayBackendError::InvalidParam as i32;
        };

        from_rust_result(cast_instance::<I>(instance).configure_scanout(
            scanout_id,
            display_width,
            display_height,
            width,
            height,
            format,
        ))
    }

    extern "C" fn disable_scanout_fb<I: DisplayBackendBasicFramebuffer>(
        instance: *mut c_void,
        scanout_id: u32,
    ) -> i32 {
        from_rust_result(cast_instance::<I>(instance).disable_scanout(scanout_id))
    }

    extern "C" fn alloc_frame<I: DisplayBackendBasicFramebuffer>(
        instance: *mut c_void,
        scanout_id: u32,
        buffer: *mut *mut u8,
        buffer_size: *mut usize,
    ) -> i32 {
        match cast_instance::<I>(instance).alloc_frame(scanout_id) {
            Ok((frame_id, allocated_buffer)) => {
                unsafe {
                    *buffer_size = allocated_buffer.len();
                    *buffer = allocated_buffer.as_mut_ptr();
                }
                frame_id as i32
            }
            Err(e) => e as i32,
        }
    }

    extern "C" fn present_frame<I: DisplayBackendBasicFramebuffer>(
        instance: *mut c_void,
        scanout_id: u32,
        frame_id: u32,
        rect: *const Rect,
    ) -> i32 {
        // SAFETY: The pointer obtained from the bindings should be safe
        let rect: Option<&Rect> = unsafe { ptr_to_option_ref(rect) };
        from_rust_result(cast_instance::<I>(instance).present_frame(scanout_id, frame_id, rect))
    }

    DisplayBasicFramebufferVtable {
        destroy: Some(destroy_fn::<I>),
        configure_scanout: Some(configure_scanout_fn::<I>),
        present_frame: Some(present_frame::<I>),
        alloc_frame: Some(alloc_frame::<I>),
        disable_scanout: Some(disable_scanout_fb::<I>),
        set_cursor: None,
        move_cursor: None,
    }
}

fn userdata_ptr<T: Sync>(userdata: Option<&T>) -> *const c_void {
    userdata.map_or(null(), |t| ptr::from_ref(t) as *const c_void)
}

impl<T: Sync, I: DisplayBackendBasicFramebuffer + DisplayBackendNew<T>> IntoDisplayBackend<T>
    for I
{
    fn into_display_backend(userdata: Option<&T>) -> DisplayBackend<'_> {
        DisplayBackend {
            create_userdata: userdata_ptr(userdata),
            create_userdata_lifetime: PhantomData,
            features: DisplayFeatures::BASIC_FRAMEBUFFER.bits(),
            create_fn: Some(create_fn::<T, I>),
            vtable: DisplayVtable {
                basic_framebuffer: basic_framebuffer_vtable::<I>(),
            },
        }
    }
}

impl<T: Sync, I: DisplayBackendBasicFramebuffer + DisplayBackendCursor + DisplayBackendNew<T>>
    IntoDisplayBackendWithCursor<T> for I
{
    fn into_display_backend_with_cursor(userdata: Option<&T>) -> DisplayBackend<'_> {
        extern "C" fn set_cursor_fn<I: DisplayBackendCursor>(
            instance: *mut c_void,
            scanout_id: u32,
            width: u32,
            height: u32,
            format: u32,
            data: *const u8,
            data_size: usize,
            hot_x: u32,
            hot_y: u32,
        ) -> i32 {
            let backend = cast_instance::<I>(instance);
            // An empty image hides the cursor; `data` may be NULL then.
            if width == 0 || height == 0 {
                return from_rust_result(backend.set_cursor(scanout_id, None));
            }

            let Ok(format) = ResourceFormat::try_from(format) else {
                error!("Unknown cursor format: {format}");
                return DisplayBackendError::InvalidParam as i32;
            };
            let Some(needed) = (width as usize)
                .checked_mul(height as usize)
                .and_then(|pixels| pixels.checked_mul(ResourceFormat::BYTES_PER_PIXEL))
            else {
                error!("Cursor image {width}x{height} is too large");
                return DisplayBackendError::InvalidParam as i32;
            };
            if data.is_null() || data_size < needed {
                error!("Cursor image {width}x{height} needs {needed} bytes, got {data_size}");
                return DisplayBackendError::InvalidParam as i32;
            }

            // SAFETY: the caller passes `data_size` readable bytes at `data`,
            // valid for the duration of this call, and `needed <= data_size`.
            let data = unsafe { slice::from_raw_parts(data, needed) };
            from_rust_result(backend.set_cursor(
                scanout_id,
                Some(CursorImage {
                    width,
                    height,
                    format,
                    data,
                    hot_x,
                    hot_y,
                }),
            ))
        }

        extern "C" fn move_cursor_fn<I: DisplayBackendCursor>(
            instance: *mut c_void,
            scanout_id: u32,
            x: i32,
            y: i32,
        ) -> i32 {
            from_rust_result(cast_instance::<I>(instance).move_cursor(scanout_id, x, y))
        }

        let mut vtable = basic_framebuffer_vtable::<I>();
        vtable.set_cursor = Some(set_cursor_fn::<I>);
        vtable.move_cursor = Some(move_cursor_fn::<I>);

        DisplayBackend {
            create_userdata: userdata_ptr(userdata),
            create_userdata_lifetime: PhantomData,
            features: (DisplayFeatures::BASIC_FRAMEBUFFER | DisplayFeatures::CURSOR).bits(),
            create_fn: Some(create_fn::<T, I>),
            vtable: DisplayVtable {
                basic_framebuffer: vtable,
            },
        }
    }
}

unsafe fn ptr_to_option_ref<'a, T>(x: *const T) -> Option<&'a T> {
    if x.is_null() {
        None
    } else {
        // SAFETY: this method is unsafe, up to the caller to be sure
        unsafe { Some(&*x) }
    }
}

fn from_rust_result(result: Result<(), DisplayBackendError>) -> i32 {
    match result {
        Ok(()) => 0,
        Err(e) => e as i32,
    }
}

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DisplayBackendInstance, c_to_rust::OriginalBasicFramebufferVtable};
    use std::cell::RefCell;

    /// What [`TestBackend`] recorded, so a call can be checked after it has
    /// gone through the C vtable and back.
    #[derive(Debug, PartialEq, Eq)]
    enum Call {
        SetCursor {
            scanout_id: u32,
            image: Option<(u32, u32, ResourceFormat, Vec<u8>, u32, u32)>,
        },
        MoveCursor {
            scanout_id: u32,
            x: i32,
            y: i32,
        },
    }

    thread_local! {
        static CALLS: RefCell<Vec<Call>> = const { RefCell::new(Vec::new()) };
    }

    fn take_calls() -> Vec<Call> {
        CALLS.with(|calls| std::mem::take(&mut *calls.borrow_mut()))
    }

    struct TestBackend;

    impl DisplayBackendNew<()> for TestBackend {
        fn new(_userdata: Option<&()>) -> Self {
            Self
        }
    }

    impl DisplayBackendBasicFramebuffer for TestBackend {
        fn configure_scanout(
            &mut self,
            _scanout_id: u32,
            _display_width: u32,
            _display_height: u32,
            _width: u32,
            _height: u32,
            _format: ResourceFormat,
        ) -> Result<(), DisplayBackendError> {
            Ok(())
        }

        fn disable_scanout(&mut self, _scanout_id: u32) -> Result<(), DisplayBackendError> {
            Ok(())
        }

        fn alloc_frame(
            &mut self,
            _scanout_id: u32,
        ) -> Result<(u32, &mut [u8]), DisplayBackendError> {
            Err(DisplayBackendError::OutOfBuffers)
        }

        fn present_frame(
            &mut self,
            _scanout_id: u32,
            _frame_id: u32,
            _rect: Option<&Rect>,
        ) -> Result<(), DisplayBackendError> {
            Ok(())
        }
    }

    impl DisplayBackendCursor for TestBackend {
        fn set_cursor(
            &mut self,
            scanout_id: u32,
            image: Option<CursorImage<'_>>,
        ) -> Result<(), DisplayBackendError> {
            let image = image.map(|i| {
                (
                    i.width,
                    i.height,
                    i.format,
                    i.data.to_vec(),
                    i.hot_x,
                    i.hot_y,
                )
            });
            CALLS.with(|calls| {
                calls
                    .borrow_mut()
                    .push(Call::SetCursor { scanout_id, image })
            });
            Ok(())
        }

        fn move_cursor(
            &mut self,
            scanout_id: u32,
            x: i32,
            y: i32,
        ) -> Result<(), DisplayBackendError> {
            CALLS.with(|calls| {
                calls
                    .borrow_mut()
                    .push(Call::MoveCursor { scanout_id, x, y })
            });
            Ok(())
        }
    }

    /// A backend struct as `krun_set_display_backend` reconstructs it from a
    /// caller that only knows the old, shorter vtable: the bytes it did pass
    /// copied into a zeroed struct, the rest left NULL.
    fn as_old_caller(backend: &DisplayBackend<'static>) -> DisplayBackend<'static> {
        // SAFETY: BASIC_FRAMEBUFFER is set on everything this helper is given.
        let full = unsafe { backend.vtable.basic_framebuffer };
        let old = OriginalBasicFramebufferVtable {
            destroy: full.destroy,
            disable_scanout: full.disable_scanout,
            configure_scanout: full.configure_scanout,
            alloc_frame: full.alloc_frame,
            present_frame: full.present_frame,
        };
        assert!(
            size_of::<OriginalBasicFramebufferVtable>()
                < size_of::<DisplayBasicFramebufferVtable>()
        );

        let mut truncated: DisplayBasicFramebufferVtable = unsafe { std::mem::zeroed() };
        // SAFETY: both are `#[repr(C)]` with the same prefix layout, and only
        // the `size_of::<OldVtable>()` bytes the old caller passed are copied.
        unsafe {
            std::ptr::copy_nonoverlapping(
                (&raw const old).cast::<u8>(),
                (&raw mut truncated).cast::<u8>(),
                size_of::<OriginalBasicFramebufferVtable>(),
            );
        }
        DisplayBackend {
            features: DisplayFeatures::BASIC_FRAMEBUFFER.bits(),
            vtable: DisplayVtable {
                basic_framebuffer: truncated,
            },
            ..*backend
        }
    }

    fn instance(backend: &DisplayBackend<'static>) -> DisplayBackendInstance {
        backend.create_instance().expect("create instance")
    }

    #[test]
    fn cursor_calls_round_trip_through_the_c_vtable() {
        let backend = TestBackend::into_display_backend_with_cursor(None);
        assert_eq!(
            backend.features,
            (DisplayFeatures::BASIC_FRAMEBUFFER | DisplayFeatures::CURSOR).bits()
        );
        assert!(backend.verify());

        let mut instance = instance(&backend);
        let data: Vec<u8> = (0..2u32 * 2 * 4).map(|i| i as u8).collect();
        instance
            .set_cursor(
                1,
                Some(CursorImage {
                    width: 2,
                    height: 2,
                    format: ResourceFormat::BGRA,
                    data: &data,
                    hot_x: 1,
                    hot_y: 0,
                }),
            )
            .unwrap();
        instance.set_cursor(1, None).unwrap();
        instance.move_cursor(1, 7, -3).unwrap();

        assert_eq!(
            take_calls(),
            vec![
                Call::SetCursor {
                    scanout_id: 1,
                    image: Some((2, 2, ResourceFormat::BGRA, data, 1, 0)),
                },
                Call::SetCursor {
                    scanout_id: 1,
                    image: None,
                },
                Call::MoveCursor {
                    scanout_id: 1,
                    x: 7,
                    y: -3,
                },
            ]
        );
    }

    #[test]
    fn a_backend_without_cursor_support_keeps_working() {
        let backend = TestBackend::into_display_backend(None);
        assert_eq!(backend.features, DisplayFeatures::BASIC_FRAMEBUFFER.bits());
        assert!(backend.verify());
        // SAFETY: BASIC_FRAMEBUFFER is set.
        assert!(unsafe { backend.vtable.basic_framebuffer.set_cursor.is_none() });

        let mut instance = instance(&backend);
        assert!(matches!(
            instance.set_cursor(0, None),
            Err(DisplayBackendError::MethodNotSupported)
        ));
        assert!(matches!(
            instance.move_cursor(0, 1, 1),
            Err(DisplayBackendError::MethodNotSupported)
        ));
        assert_eq!(take_calls(), vec![]);
    }

    #[test]
    fn an_old_size_struct_is_accepted_and_simply_lacks_the_feature() {
        let backend = as_old_caller(&TestBackend::into_display_backend_with_cursor(None));
        assert!(backend.verify());

        let mut instance = instance(&backend);
        assert!(matches!(
            instance.set_cursor(0, None),
            Err(DisplayBackendError::MethodNotSupported)
        ));
        assert_eq!(take_calls(), vec![]);
    }

    #[test]
    fn the_cursor_feature_without_its_methods_is_rejected() {
        let mut backend = TestBackend::into_display_backend(None);
        backend.features |= DisplayFeatures::CURSOR.bits();
        assert!(!backend.verify());
    }
}
