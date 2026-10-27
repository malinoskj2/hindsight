//! Embeds the Windows icon and manifest, compiles the HDR tone-map shader with the Windows
//! SDK's `fxc.exe`, which is installed alongside the `rc.exe` the resources already need,
//! records the commit, date and compiler of the build for the About tab, and embeds this
//! version's release notes.

use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

#[path = "src/update/changelog.rs"]
mod changelog;

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
/// The value shown for a build fact that could not be determined.
const UNKNOWN: &str = "unknown";
const CHANGELOG: &str = "CHANGELOG.md";
/// The file in `OUT_DIR` that `src/update/notes.rs` includes.
const RELEASE_NOTES: &str = "release-notes.md";

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
    record_build_info();
    embed_release_notes();
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let mut resource = winresource::WindowsResource::new();
    resource
        .set_manifest(MANIFEST)
        .set("ProductName", env!("CARGO_PKG_NAME"))
        // Windows names the app by its description in the taskbar's tray icon settings, Task
        // Manager and the startup apps list.
        .set("FileDescription", env!("CARGO_PKG_NAME"))
        .set("ProductVersion", env!("CARGO_PKG_VERSION"));
    for table::IconEntry { ico, id, .. } in table::ICONS {
        resource.set_icon_with_id(&format!("assets/{ico}"), &id.to_string());
    }
    resource.compile().expect("embed the Windows resources");
    compile_shaders();
}

/// Sets `HINDSIGHT_COMMIT`, `HINDSIGHT_BUILD_DATE` and `HINDSIGHT_RUSTC`. Each is "unknown" when
/// it cannot be determined, so a build without git or outside a checkout still succeeds.
fn record_build_info() {
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");
    // HEAD changes on a checkout, the branch's ref on a commit and the index on staging. A
    // worktree keeps these files outside `.git`, so git is asked for their paths.
    for name in ["HEAD", "index", "packed-refs"] {
        if let Some(path) = git(&["rev-parse", "--git-path", name]) {
            println!("cargo:rerun-if-changed={path}");
        }
    }
    if let Some(branch) = git(&["symbolic-ref", "-q", "HEAD"])
        && let Some(path) = git(&["rev-parse", "--git-path", &branch])
    {
        println!("cargo:rerun-if-changed={path}");
    }
    let commit = git(&["rev-parse", "--short", "HEAD"]).map_or_else(
        || UNKNOWN.to_owned(),
        |hash| {
            // Without the optional lock git leaves the index untouched; refreshing it would rerun
            // this script.
            let dirty = git(&[
                "--no-optional-locks",
                "status",
                "--porcelain",
                "--untracked-files=no",
            ])
            .is_some_and(|changes| !changes.is_empty());
            if dirty { format!("{hash}-dirty") } else { hash }
        },
    );
    println!("cargo:rustc-env=HINDSIGHT_COMMIT={commit}");
    println!("cargo:rustc-env=HINDSIGHT_BUILD_DATE={}", build_date());
    let rustc = env::var_os("RUSTC")
        .and_then(|rustc| output(Command::new(rustc).arg("--version")))
        .unwrap_or_else(|| UNKNOWN.to_owned());
    println!("cargo:rustc-env=HINDSIGHT_RUSTC={rustc}");
}

/// Writes this version's section of the changelog to `OUT_DIR`. A release build without one
/// fails, so every installer carries its notes; any other build embeds no notes instead.
fn embed_release_notes() {
    println!("cargo:rerun-if-changed={CHANGELOG}");
    let version = env!("CARGO_PKG_VERSION");
    let changelog = fs::read_to_string(CHANGELOG).unwrap_or_default();
    let notes = changelog::section(&changelog, version);
    let release = env::var("PROFILE").as_deref() == Ok("release");
    assert!(
        notes.is_some() || !release,
        "{CHANGELOG} has no section for {version}; add `## {version}` with its notes"
    );
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"));
    fs::write(out.join(RELEASE_NOTES), notes.unwrap_or_default())
        .expect("write the release notes to OUT_DIR");
}

/// Returns the trimmed output of a git command run in the package, or `None` when git is
/// missing or fails, as it does outside a checkout.
fn git(args: &[&str]) -> Option<String> {
    output(Command::new("git").args(args))
}

fn output(command: &mut Command) -> Option<String> {
    let output = command.output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Returns the UTC date as `2026-10-14`, taken from `SOURCE_DATE_EPOCH` when it is set so that a
/// build is reproducible.
fn build_date() -> String {
    let seconds = env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|epoch| epoch.trim().parse::<u64>().ok())
        .or_else(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .ok()
                .map(|since| since.as_secs())
        });
    seconds.map_or_else(
        || UNKNOWN.to_owned(),
        |seconds| {
            let (year, month, day) = civil_date(seconds / 86_400);
            format!("{year:04}-{month:02}-{day:02}")
        },
    )
}

/// Days since 1970-01-01 as a Gregorian year, month and day (Howard Hinnant's `civil_from_days`).
fn civil_date(days: u64) -> (u64, u64, u64) {
    let shifted = days + 719_468;
    let era = shifted / 146_097;
    let day_of_era = shifted % 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + u64::from(month <= 2);
    (year, month, day)
}

/// Compiles the shader stages, strict and fully optimized, as `D3DCompile` did at run time
/// previously.
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

/// Returns the path in `FXC` if it is set, otherwise the newest installed SDK's `fxc.exe` for
/// this machine.
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

/// Returns the installed Windows 10+ SDK's root, as recorded in the registry.
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

/// Parses a name such as `10.0.26100.0` into numbers so versions sort numerically. Returns
/// `None` for other directories.
fn sdk_version(name: &str) -> Option<Vec<u32>> {
    name.split('.').map(|part| part.parse().ok()).collect()
}
