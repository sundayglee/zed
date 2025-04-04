mod clipboard;
mod direct_write;
mod dispatcher;
mod display;
mod events;
mod platform;
mod system_settings;
mod util;
mod window;
mod wrapper;

pub(crate) use clipboard::*;
pub(crate) use direct_write::*;
pub(crate) use dispatcher::*;
pub(crate) use display::*;
pub(crate) use events::*;
pub(crate) use platform::*;
pub(crate) use system_settings::*;
pub(crate) use util::*;
pub(crate) use window::*;
pub(crate) use wrapper::*;

pub(crate) use windows::Win32::Foundation::HWND;

pub(crate) type PlatformScreenCaptureFrame = scap::frame::Frame;
