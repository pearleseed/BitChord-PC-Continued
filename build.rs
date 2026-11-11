// Embed the app icon into the Windows executable (Explorer, taskbar, shortcuts).
fn main() {
    println!("cargo:rerun-if-changed=assets/icon/icon.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        winresource::WindowsResource::new()
            .set_icon("assets/icon/icon.ico")
            .set_manifest(MANIFEST)
            .compile()
            .expect("failed to embed Windows icon");
    }
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rerun-if-changed=src/native/macos/window_chrome.m");
        println!("cargo:rerun-if-changed=src/native/macos/lock_screen.m");
        println!("cargo:rustc-link-lib=framework=AppKit");
        println!("cargo:rustc-link-lib=framework=CoreGraphics");
        println!("cargo:rustc-link-lib=framework=CoreText");
        cc::Build::new()
            .files([
                "src/native/macos/window_chrome.m",
                "src/native/macos/lock_screen.m",
            ])
            .flag("-fobjc-arc")
            .compile("bitchord_window_chrome");
    }
}

// The segment heap returns freed memory to the OS far more readily than the default
// NT heap, which keeps the resident footprint down after bursts (images, JSON).
const MANIFEST: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <application xmlns="urn:schemas-microsoft-com:asm.v3">
    <windowsSettings>
      <heapType xmlns="http://schemas.microsoft.com/SMI/2020/WindowsSettings">SegmentHeap</heapType>
    </windowsSettings>
  </application>
</assembly>
"#;
