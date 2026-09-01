fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    #[cfg(target_os = "macos")]
    macos::generate();
}

#[cfg(target_os = "macos")]
mod macos {
    use std::env;
    use std::path::PathBuf;

    /// IOKit's USB API is COM-style: every interface is a pointer to a vtable whose method
    /// order comes from a chain of inherited macros. Generating the bindings keeps that
    /// order honest -- a hand-written vtable that is one entry out calls the wrong function.
    const HEADER: &str = r"
        #include <CoreFoundation/CoreFoundation.h>
        #include <IOKit/IOKitLib.h>
        #include <IOKit/IOCFPlugIn.h>
        #include <IOKit/usb/IOUSBLib.h>
    ";

    /// Framework headers only resolve against an SDK root, which cargo does not set.
    fn sdk_path() -> String {
        let output = std::process::Command::new("xcrun")
            .args(["--show-sdk-path"])
            .output()
            .expect("run xcrun to locate the macOS SDK");
        assert!(output.status.success(), "xcrun --show-sdk-path failed");
        String::from_utf8(output.stdout)
            .expect("SDK path is utf-8")
            .trim()
            .to_owned()
    }

    pub fn generate() {
        println!("cargo:rustc-link-lib=framework=IOKit");
        println!("cargo:rustc-link-lib=framework=CoreFoundation");

        let bindings = bindgen::Builder::default()
            .header_contents("usb.h", HEADER)
            .clang_args(["-isysroot", &sdk_path()])
            .allowlist_type("IOUSBDeviceInterface500")
            .allowlist_type("IOUSBInterfaceInterface500")
            .allowlist_type("IOCFPlugInInterface")
            .allowlist_type("IOUSBFindInterfaceRequest")
            .allowlist_type("IOUSBDevRequest")
            .allowlist_type("IOUSBLowLatencyIsocFrame")
            .allowlist_type("IOReturn")
            .allowlist_type("USBLowLatencyBufferType")
            .allowlist_type("io_iterator_t")
            .allowlist_type("io_service_t")
            .allowlist_function("IOCreatePlugInInterfaceForService")
            .allowlist_function("IODestroyPlugInInterface")
            .allowlist_function("IOServiceMatching")
            .allowlist_function("IOServiceGetMatchingServices")
            .allowlist_function("IOIteratorNext")
            .allowlist_function("IOObjectRelease")
            .allowlist_function("IORegistryEntryCreateCFProperty")
            .allowlist_function("CFUUIDGetConstantUUIDWithBytes")
            .allowlist_function("CFUUIDGetUUIDBytes")
            .allowlist_function("CFStringCreateWithCString")
            .allowlist_function("CFStringGetCString")
            .allowlist_var("kCFStringEncodingUTF8")
            .allowlist_var("kCFAllocatorDefault")
            .allowlist_function("CFDictionarySetValue")
            .allowlist_function("CFNumberCreate")
            .allowlist_function("CFRelease")
            .allowlist_function("CFRunLoopGetCurrent")
            .allowlist_function("CFRunLoopAddSource")
            .allowlist_function("CFRunLoopRemoveSource")
            .allowlist_function("CFRunLoopRunInMode")
            .allowlist_var("kCFRunLoopDefaultMode")
            .allowlist_var("kCFAllocatorSystemDefault")
            .allowlist_var("kCFNumberSInt32Type")
            .allowlist_var("kIOReturn.*")
            .allowlist_var("kUSB.*")
            .allowlist_var("kIOUSBFindInterfaceDontCare")
            .derive_default(true)
            .layout_tests(false)
            .generate()
            .expect("generate IOKit USB bindings");

        let out = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR set by cargo"));
        bindings
            .write_to_file(out.join("usb_sys.rs"))
            .expect("write IOKit USB bindings");
    }
}
