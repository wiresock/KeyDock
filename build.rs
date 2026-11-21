//! Embeds the application icon, the tray-state icons, the application
//! manifest (common controls v6, per-monitor v2 DPI awareness) and the version
//! resource into keydock.exe. Does nothing for non-Windows targets.

use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let out = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    let root = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let version = env::var("CARGO_PKG_VERSION").expect("CARGO_PKG_VERSION");
    let mut nums: Vec<u16> = version
        .split(['.', '-', '+'])
        .take(3)
        .map(|p| p.parse().unwrap_or(0))
        .collect();
    nums.resize(4, 0);
    let dotted = format!("{}.{}.{}.{}", nums[0], nums[1], nums[2], nums[3]);
    let commas = format!("{},{},{},{}", nums[0], nums[1], nums[2], nums[3]);

    let manifest = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <assemblyIdentity type="win32" name="KeyDock" version="{dotted}" processorArchitecture="*"/>
  <description>KeyDock</description>
  <dependency>
    <dependentAssembly>
      <assemblyIdentity type="win32" name="Microsoft.Windows.Common-Controls" version="6.0.0.0"
        processorArchitecture="*" publicKeyToken="6595b64144ccf1df" language="*"/>
    </dependentAssembly>
  </dependency>
  <application xmlns="urn:schemas-microsoft-com:asm.v3">
    <windowsSettings>
      <dpiAware xmlns="http://schemas.microsoft.com/SMI/2005/WindowsSettings">true/pm</dpiAware>
      <dpiAwareness xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">PerMonitorV2, PerMonitor</dpiAwareness>
    </windowsSettings>
  </application>
  <compatibility xmlns="urn:schemas-microsoft-com:compatibility.v1">
    <application>
      <supportedOS Id="{{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}}"/>
    </application>
  </compatibility>
  <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3">
    <security>
      <requestedPrivileges>
        <requestedExecutionLevel level="asInvoker" uiAccess="false"/>
      </requestedPrivileges>
    </security>
  </trustInfo>
</assembly>
"#
    );
    let manifest_path = out.join("keydock.manifest");
    fs::write(&manifest_path, manifest).expect("write manifest");

    let p = |pb: PathBuf| pb.display().to_string().replace('\\', "/");
    let icon = |name: &str| p(root.join("assets").join(name));
    // Numeric resource types/IDs so that no Windows SDK headers are needed:
    // 24 = RT_MANIFEST, 1 = CREATEPROCESS_MANIFEST_RESOURCE_ID.
    // Icon IDs must match `IDI_*` in src/win/window.rs.
    let rc = format!(
        r#"1 ICON "{app}"
2 ICON "{docked}"
3 ICON "{laptop}"
4 ICON "{paused}"
1 24 "{manifest}"

1 VERSIONINFO
FILEVERSION {commas}
PRODUCTVERSION {commas}
FILEFLAGSMASK 0x3F
FILEFLAGS 0x0
FILEOS 0x40004
FILETYPE 0x1
FILESUBTYPE 0x0
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904B0"
    BEGIN
      VALUE "FileDescription", "KeyDock - keyboard layouts that follow your keyboard\0"
      VALUE "FileVersion", "{version}\0"
      VALUE "InternalName", "keydock\0"
      VALUE "OriginalFilename", "keydock.exe\0"
      VALUE "ProductName", "KeyDock\0"
      VALUE "ProductVersion", "{version}\0"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#,
        app = icon("keydock.ico"),
        docked = icon("docked.ico"),
        laptop = icon("laptop.ico"),
        paused = icon("paused.ico"),
        manifest = p(manifest_path.clone()),
    );
    let rc_path = out.join("keydock.rc");
    fs::write(&rc_path, rc).expect("write rc");
    embed_resource::compile(&rc_path, embed_resource::NONE)
        .manifest_required()
        .expect("compiling resources failed");
}
