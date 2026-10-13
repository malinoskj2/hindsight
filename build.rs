//! Embeds the Windows icon and manifest.

/// Written by `hindsight icon assets`, one per theme; the ids must match `sys::icon::resource`.
const ICONS: [(&str, &str); 4] = [
    ("assets/hindsight.ico", "1"),
    ("assets/hindsight-ember.ico", "2"),
    ("assets/hindsight-selene.ico", "3"),
    ("assets/hindsight-nova.ico", "4"),
];
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
    for (icon, _) in ICONS {
        println!("cargo:rerun-if-changed={icon}");
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
    for (icon, id) in ICONS {
        resource.set_icon_with_id(icon, id);
    }
    resource.compile().expect("embed the Windows resources");
}
