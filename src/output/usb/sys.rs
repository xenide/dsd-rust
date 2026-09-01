//! Raw IOKit USB bindings, generated at build time from the macOS SDK headers.

#![allow(
    non_upper_case_globals,
    non_camel_case_types,
    non_snake_case,
    dead_code,
    clippy::all,
    clippy::pedantic
)]

include!(concat!(env!("OUT_DIR"), "/usb_sys.rs"));

/// `IOReturn.h` builds its codes from macros bindgen cannot evaluate:
/// `iokit_common_err(x)` is `err_system(0x38) | err_sub(0) | x`, so `0xE0000000 | x`.
pub const kIOReturnUnderrun: IOReturn = 0xE000_02E7_u32 as IOReturn;

/// UUID byte values copied from `IOKit.framework/Headers/usb/IOUSBLib.h` and
/// `IOKit.framework/Headers/IOCFPlugIn.h`.
pub mod uuid {
    use super::{CFUUIDGetConstantUUIDWithBytes, CFUUIDRef, kCFAllocatorSystemDefault};

    macro_rules! uuid {
        ($name:ident, $allocator:expr, [$($byte:expr),+ $(,)?]) => {
            pub fn $name() -> CFUUIDRef {
                // SAFETY: constant UUIDs are interned by CoreFoundation and never freed.
                unsafe { CFUUIDGetConstantUUIDWithBytes($allocator, $($byte),+) }
            }
        };
    }

    uuid!(
        device_user_client_type,
        std::ptr::null(),
        [
            0x9d, 0xc7, 0xb7, 0x80, 0x9e, 0xc0, 0x11, 0xD4, 0xa5, 0x4f, 0x00, 0x0a, 0x27, 0x05,
            0x28, 0x61,
        ]
    );

    uuid!(
        interface_user_client_type,
        std::ptr::null(),
        [
            0x2d, 0x97, 0x86, 0xc6, 0x9e, 0xf3, 0x11, 0xD4, 0xad, 0x51, 0x00, 0x0a, 0x27, 0x05,
            0x28, 0x61,
        ]
    );

    uuid!(
        plugin_interface,
        std::ptr::null(),
        [
            0xC2, 0x44, 0xE8, 0x58, 0x10, 0x9C, 0x11, 0xD4, 0x91, 0xD4, 0x00, 0x50, 0xE4, 0xC6,
            0x42, 0x6F,
        ]
    );

    // `kCFAllocatorSystemDefault` is a CoreFoundation constant, valid for the process
    // lifetime; the macro already reads it inside its own unsafe block.
    uuid!(
        device_interface_500,
        kCFAllocatorSystemDefault,
        [
            0xA3, 0x3C, 0xF0, 0x47, 0x4B, 0x5B, 0x48, 0xE2, 0xB5, 0x7D, 0x02, 0x07, 0xFC, 0xEA,
            0xE1, 0x3B,
        ]
    );

    uuid!(
        usb_interface_500,
        kCFAllocatorSystemDefault,
        [
            0x6C, 0x0D, 0x38, 0xC3, 0xB0, 0x93, 0x4E, 0xA7, 0x80, 0x9B, 0x09, 0xFB, 0x5D, 0xDD,
            0xAC, 0x16,
        ]
    );
}
