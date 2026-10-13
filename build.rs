//! Embeds the Windows icon and manifest, and compiles the HDR tone-map shader with the Windows
//! SDK's `fxc.exe`, which sits beside the `rc.exe` the resources already need.

use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

#[path = "src/sys/icon/table.rs"]
#[expect(
    dead_code,
    reason = "the build embeds the icons and never reads the logos"
)]
mod table;

const SHADER: &str = "src/gpu/capture/tonemap.hlsl";
/// Entry point, profile and output file of each stage `tonemap.rs` includes.
const SHADER_STAGES: [(&str, &str, &str); 2] = [
    ("fullscreen", "vs_5_0", "tonemap.vs.cso"),
    ("tonemap", "ps_5_0", "tonemap.ps.cso"),
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
    for table::IconEntry { ico, .. } in table::ICONS {
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
    for table::IconEntry { ico, id, .. } in table::ICONS {
        resource.set_icon_with_id(&format!("assets/{ico}"), &id.to_string());
    }
    resource.compile().expect("embed the Windows resources");
    compile_shaders();
}

/// Strict and fully optimized, as `D3DCompile` did at run time before.
fn compile_shaders() {
    println!("cargo:rerun-if-changed={SHADER}");
    println!("cargo:rerun-if-env-changed=FXC");
    let fxc = find_fxc().expect(
        "find fxc.exe in the Windows SDK; install the SDK or set FXC to the path of fxc.exe",
    );
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"));
    for (entry, profile, file) in SHADER_STAGES {
        let status = Command::new(&fxc)
            .args(["/nologo", "/T", profile, "/E", entry, "/O3", "/Ges", "/Fo"])
            .arg(out.join(file))
            .arg(SHADER)
            .status()
            .unwrap_or_else(|error| panic!("run {}: {error}", fxc.display()));
        assert!(
            status.success(),
            "fxc.exe failed to compile {entry} in {SHADER}"
        );
    }
}

/// `FXC` if set, else the newest SDK's `fxc.exe` for this machine.
fn find_fxc() -> Option<PathBuf> {
    if let Some(path) = env::var_os("FXC") {
        return Some(path.into());
    }
    let root =
        kits_root().unwrap_or_else(|| PathBuf::from(r"C:\Program Files (x86)\Windows Kits\10"));
    let host = if cfg!(target_arch = "aarch64") {
        "arm64"
    } else if cfg!(target_arch = "x86") {
        "x86"
    } else {
        "x64"
    };
    fs::read_dir(root.join("bin"))
        .ok()?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let version = sdk_version(&entry.file_name().to_string_lossy())?;
            let fxc = entry.path().join(host).join("fxc.exe");
            fxc.is_file().then_some((version, fxc))
        })
        .max()
        .map(|(_, fxc)| fxc)
}

/// The installed Windows 10+ SDK's root, as the registry records it.
fn kits_root() -> Option<PathBuf> {
    let output = Command::new("reg")
        .args([
            "query",
            r"HKLM\SOFTWARE\Microsoft\Windows Kits\Installed Roots",
            "/v",
            "KitsRoot10",
            "/reg:32",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let (_, root) = text.lines().find_map(|line| line.split_once("REG_SZ"))?;
    let root = Path::new(root.trim());
    root.is_dir().then(|| root.to_path_buf())
}

/// `10.0.26100.0` as numbers, so versions sort numerically; `None` for other directories.
fn sdk_version(name: &str) -> Option<Vec<u32>> {
    name.split('.').map(|part| part.parse().ok()).collect()
}
