// AGPL v3 License

//! Set up the logging callback for FFMPEG to use tracing

#![allow(unsafe_code)]

use ffmpeg::sys;
use std::{
    ffi::c_void,
    mem,
    os::raw::{c_char, c_int},
};
use tracing::Level;
use vsprintf::vsprintf;

/// Register the logging callback with FFMPEG.
pub(crate) fn register_ffmpeg_logger() {
    unsafe {
        sys::av_log_set_callback(Some(log_callback));
    }
}

/// The callback that forwards to tracing.
unsafe extern "C" fn log_callback(
    class: *mut c_void,
    level: c_int,
    format: *const c_char,
    va_list: *mut sys::__va_list_tag,
) {
    // if any functionality panics, abort
    let _bomb = AbortOnDrop;

    // format the message
    let formatted =
        vsprintf(format, va_list).unwrap_or_else(|_| "FFMPEG: unknown error".to_string());

    // match to a tracing level
    match level {
        sys::AV_LOG_ERROR | sys::AV_LOG_FATAL => tracing::error!("{}", formatted,),
        sys::AV_LOG_WARNING => tracing::warn!("{}", formatted),
        sys::AV_LOG_INFO => tracing::info!("{}", formatted,),
        sys::AV_LOG_DEBUG | sys::AV_LOG_VERBOSE => tracing::debug!("{}", formatted),
        sys::AV_LOG_TRACE => tracing::trace!("{}", formatted),
        _ => tracing::error!("unknown level: {}", formatted,),
    };

    mem::forget(_bomb);
}

struct AbortOnDrop;

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        std::process::abort()
    }
}
