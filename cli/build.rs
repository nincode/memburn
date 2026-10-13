use std::{env, path::PathBuf, process::Command};

fn main() {
    println!("cargo::rerun-if-changed=memburn.ico");
    println!("cargo::rerun-if-env-changed=WINDRES");
    let target = |key| env::var(key).unwrap_or_default();
    if target("CARGO_CFG_TARGET_OS") != "windows" || target("CARGO_CFG_TARGET_ENV") != "gnu" {
        return;
    }
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let ico = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap()).join("memburn.ico");
    let rc = out.join("memburn.rc");
    std::fs::write(&rc, format!("1 ICON \"{}\"\n", ico.display().to_string().replace('\\', "/"))).unwrap();
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
