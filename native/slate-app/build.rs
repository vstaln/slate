use std::{env, fs, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-env-changed=SLATE_CONPTY_DIR");
    // `slate_browser` = this platform can host the wry webview AND the
    // `browser` feature asked for it. webview.rs's cfg gates hang off this
    // one name instead of repeating the platform pair.
    let os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if matches!(os.as_str(), "linux" | "freebsd") && env::var_os("CARGO_FEATURE_BROWSER").is_some()
    {
        println!("cargo:rustc-cfg=slate_browser");
    }
    println!("cargo:rustc-check-cfg=cfg(slate_browser)");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let architecture = match env::var("CARGO_CFG_TARGET_ARCH").unwrap().as_str() {
        "x86_64" => "win10-x64",
        "aarch64" => "win10-arm64",
        other => panic!("Unsupported Windows architecture: {other}"),
    };
    let source = env::var_os("SLATE_CONPTY_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest.join("../vendor/conpty").join(architecture));
    let output = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let destination = output.ancestors().nth(3).expect("Cargo profile directory");
    for name in ["conpty.dll", "OpenConsole.exe"] {
        let source = source.join(name);
        println!("cargo:rerun-if-changed={}", source.display());
        let bytes = fs::read(&source).unwrap_or_else(|error| {
            panic!(
                "Cannot read {}: {error}. Set SLATE_CONPTY_DIR to the directory holding conpty.dll and OpenConsole.exe.",
                source.display()
            )
        });
        let target = destination.join(name);
        if fs::read(&target).ok().as_deref() != Some(bytes.as_slice()) {
            fs::write(&target, bytes)
                .unwrap_or_else(|error| panic!("Cannot write {}: {error}", target.display()));
        }
    }
}
