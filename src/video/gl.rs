//! A render context is created, used and freed only with the GLArea's context
//! current. Every normal client API call belongs to core.rs's control thread.
use super::core::Core;
use libmpv2_sys as raw;
use std::{
    ffi::{CStr, c_void},
    ptr::{self, NonNull},
    sync::Arc,
};
#[link(name = "EGL")]
unsafe extern "C" {
    fn eglGetProcAddress(name: *const libc::c_char) -> *mut c_void;
}
unsafe extern "C" fn get_proc(_: *mut c_void, name: *const libc::c_char) -> *mut c_void {
    unsafe { eglGetProcAddress(name) }
}
unsafe extern "C" fn update(data: *mut c_void) {
    let send = unsafe { &*data.cast::<async_channel::Sender<()>>() };
    let _ = send.try_send(());
}
type Viewport = unsafe extern "C" fn(i32, i32, i32, i32);
type GetInteger = unsafe extern "C" fn(u32, *mut i32);
pub struct Renderer {
    context: NonNull<raw::mpv_render_context>,
    _core: Arc<Core>,
    _wake: Box<async_channel::Sender<()>>,
    integer: GetInteger,
    viewport: Viewport,
}
impl Renderer {
    pub fn new(
        core: Arc<Core>,
        display: *mut c_void,
        wake: async_channel::Sender<()>,
    ) -> Result<Self, String> {
        let ptr = unsafe { eglGetProcAddress(c"glGetIntegerv".as_ptr()) };
        if ptr.is_null() {
            return Err("OpenGL entry points are unavailable".into());
        }
        // The EGL loader returns a function of this signature for glGetIntegerv.
        let integer = unsafe { std::mem::transmute::<*mut c_void, GetInteger>(ptr) };
        let ptr = unsafe { eglGetProcAddress(c"glViewport".as_ptr()) };
        if ptr.is_null() {
            return Err("OpenGL viewport entry point is unavailable".into());
        }
        let viewport = unsafe { std::mem::transmute::<*mut c_void, Viewport>(ptr) };
        let mut init = raw::mpv_opengl_init_params {
            get_proc_address: Some(get_proc),
            get_proc_address_ctx: ptr::null_mut(),
        };
        let mut params = [
            raw::mpv_render_param {
                type_: raw::mpv_render_param_type_MPV_RENDER_PARAM_API_TYPE,
                data: raw::MPV_RENDER_API_TYPE_OPENGL.as_ptr().cast_mut().cast(),
            },
            raw::mpv_render_param {
                type_: raw::mpv_render_param_type_MPV_RENDER_PARAM_OPENGL_INIT_PARAMS,
                data: (&mut init as *mut raw::mpv_opengl_init_params).cast(),
            },
            raw::mpv_render_param {
                type_: raw::mpv_render_param_type_MPV_RENDER_PARAM_WL_DISPLAY,
                data: display,
            },
            raw::mpv_render_param {
                type_: 0,
                data: ptr::null_mut(),
            },
        ];
        let mut context = ptr::null_mut();
        let result = unsafe {
            raw::mpv_render_context_create(&mut context, core.handle.as_ptr(), params.as_mut_ptr())
        };
        if result < 0 {
            return Err(format!(
                "Cannot create libmpv GL renderer: {}",
                unsafe { CStr::from_ptr(raw::mpv_error_string(result)) }.to_string_lossy()
            ));
        }
        let context = NonNull::new(context).ok_or("libmpv returned no render context")?;
        let mut wake = Box::new(wake);
        unsafe {
            raw::mpv_render_context_set_update_callback(
                context.as_ptr(),
                Some(update),
                (&mut *wake as *mut async_channel::Sender<()>).cast(),
            )
        };
        Ok(Self {
            context,
            _core: core,
            _wake: wake,
            integer,
            viewport,
        })
    }
    pub fn render(&self, width: i32, height: i32) -> Result<bool, String> {
        let flags = unsafe { raw::mpv_render_context_update(self.context.as_ptr()) };
        let mut framebuffer = 0;

        unsafe {
            (self.integer)(0x8ca6, &mut framebuffer);
            (self.viewport)(0, 0, width, height)
        };
        if width <= 0 || height <= 0 {
            return Ok(false);
        }
        let mut fbo = raw::mpv_opengl_fbo {
            fbo: framebuffer,
            w: width,
            h: height,
            internal_format: 0,
        };
        let mut flip = 1i32;
        let mut block = 0i32;
        let mut params = [
            raw::mpv_render_param {
                type_: raw::mpv_render_param_type_MPV_RENDER_PARAM_OPENGL_FBO,
                data: (&mut fbo as *mut raw::mpv_opengl_fbo).cast(),
            },
            raw::mpv_render_param {
                type_: raw::mpv_render_param_type_MPV_RENDER_PARAM_FLIP_Y,
                data: (&mut flip as *mut i32).cast(),
            },
            raw::mpv_render_param {
                type_: raw::mpv_render_param_type_MPV_RENDER_PARAM_BLOCK_FOR_TARGET_TIME,
                data: (&mut block as *mut i32).cast(),
            },
            raw::mpv_render_param {
                type_: 0,
                data: ptr::null_mut(),
            },
        ];
        let result =
            unsafe { raw::mpv_render_context_render(self.context.as_ptr(), params.as_mut_ptr()) };
        if result < 0 {
            return Err(format!(
                "Video rendering failed: {}",
                unsafe { CStr::from_ptr(raw::mpv_error_string(result)) }.to_string_lossy()
            ));
        }
        Ok(flags & raw::mpv_render_update_flag_MPV_RENDER_UPDATE_FRAME as u64 != 0)
    }
}
impl Drop for Renderer {
    fn drop(&mut self) {
        unsafe {
            raw::mpv_render_context_set_update_callback(
                self.context.as_ptr(),
                None,
                ptr::null_mut(),
            );
            raw::mpv_render_context_free(self.context.as_ptr());
        }
    }
}
