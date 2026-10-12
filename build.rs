//! Embeds the Windows icon and manifest.

/// Written by `hindsight icon assets/hindsight.ico`.
const ICON: &str = "assets/hindsight.ico";
/// Must match `gui::ICON_RESOURCE`.
const ICON_ID: &str = "1";
const MANIFEST: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <dependency>
    <dependentAssembly>
      <assemblyIdentity type="win32" name="Microsoft.Windows.Common-Controls" version="6.0.0.0" processorArchitecture="*" publicKeyToken="6595b64144ccf1df" language="*"/>
    </dependentAssembly>
  </dependency>
  <application xmlns="urn:schemas-microsoft-com:asm.v3">
    <windowsSettings>
      <dpiAware xmlns="http://schemas.microsoft.com/SMI/2005/WindowsSettings">true</dpiAware>
    </windowsSettings>
  </application>
</assembly>"#;

fn main() {
    println!("cargo:rerun-if-changed={ICON}");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    winresource::WindowsResource::new()
        .set_manifest(MANIFEST)
        .set("ProductName", env!("CARGO_PKG_NAME"))
        .set(
            "FileDescription",
            "Instant replay, captured and encoded on the GPU",
        )
        .set_icon_with_id(ICON, ICON_ID)
        .compile()
        .expect("embed the Windows resources");
}
