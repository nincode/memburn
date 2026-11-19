use std::{env, path::PathBuf, process::Command};

/// Per-monitor DPI aware (the dashboard is drawn at physical pixels); no console of its own when started without one
/// (Windows 11 24H2 and later, ignored before).
const MANIFEST: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3">
    <security><requestedPrivileges><requestedExecutionLevel level="asInvoker" uiAccess="false"/></requestedPrivileges></security>
  </trustInfo>
  <compatibility xmlns="urn:schemas-microsoft-com:compatibility.v1">
    <application><supportedOS Id="{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}"/></application>
  </compatibility>
  <application xmlns="urn:schemas-microsoft-com:asm.v3">
    <windowsSettings>
      <dpiAware xmlns="http://schemas.microsoft.com/SMI/2005/WindowsSettings">true/pm</dpiAware>
      <dpiAwareness xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">PerMonitorV2</dpiAwareness>
      <consoleAllocationPolicy xmlns="http://schemas.microsoft.com/SMI/2024/WindowsSettings">detached</consoleAllocationPolicy>
    </windowsSettings>
  </application>
</assembly>
"#;

fn main() {
    println!("cargo::rerun-if-changed=memburn.ico");
    println!("cargo::rerun-if-env-changed=WINDRES");
    let target = |key| env::var(key).unwrap_or_default();
    if target("CARGO_CFG_TARGET_OS") != "windows" || target("CARGO_CFG_TARGET_ENV") != "gnu" {
        return;
    }
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let ico = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap()).join("memburn.ico");
    let manifest = out.join("memburn.manifest");
    std::fs::write(&manifest, MANIFEST).unwrap();
    let path = |p: &PathBuf| p.display().to_string().replace('\\', "/");
    let rc = out.join("memburn.rc");
    std::fs::write(&rc, format!("1 ICON \"{}\"\n1 24 \"{}\"\n", path(&ico), path(&manifest))).unwrap();
    let res = out.join("memburn-res.o");
    let windres = env::var("WINDRES")
        .unwrap_or_else(|_| if cfg!(windows) { "windres" } else { "x86_64-w64-mingw32-windres" }.into());
    let status = Command::new(&windres)
        .arg(&rc)
        .args(["-O", "coff", "-o"])
        .arg(&res)
        .status()
        .unwrap_or_else(|e| panic!("{windres}: {e}"));
    assert!(status.success(), "{windres} failed on {}", rc.display());
    println!("cargo::rustc-link-arg-bins={}", res.display());
}
