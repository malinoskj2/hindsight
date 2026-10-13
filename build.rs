//! Embeds the Windows icon and manifest.

#[path = "src/sys/icon/table.rs"]
mod table;

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
    for (_, _, ico, _) in table::ICONS {
        println!("cargo:rerun-if-changed=assets/{ico}");
    }
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let mut resource = winresource::WindowsResource::new();
    resource
        .set_manifest(MANIFEST)
        .set("ProductName", env!("CARGO_PKG_NAME"))
        .set(
            "FileDescription",
            "The lowest-overhead instant replay, captured and encoded on the GPU",
        )
        .set("ProductVersion", env!("CARGO_PKG_VERSION"));
    for (_, _, ico, id) in table::ICONS {
        resource.set_icon_with_id(&format!("assets/{ico}"), &id.to_string());
    }
    resource.compile().expect("embed the Windows resources");
}
