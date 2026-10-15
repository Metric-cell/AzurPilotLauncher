//! 编译期资源与权限声明；发布信任根仅来自明确的构建配置。
use std::{env, fs, path::PathBuf};

fn main() {
    let windows = tauri_build::WindowsAttributes::new().app_manifest(
        r#"
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <dependency>
    <dependentAssembly>
      <assemblyIdentity
        type="win32"
        name="Microsoft.Windows.Common-Controls"
        version="6.0.0.0"
        processorArchitecture="*"
        publicKeyToken="6595b64144ccf1df"
        language="*"
      />
    </dependentAssembly>
  </dependency>
  <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3">
    <security>
      <requestedPrivileges>
        <requestedExecutionLevel level="asInvoker" uiAccess="false" />
      </requestedPrivileges>
    </security>
  </trustInfo>
</assembly>
"#,
    );
    const COMMANDS: &[&str] = &[
        "open_project_repository",
        "save_as",
        "splash_ready",
        "download_today_gui_log",
        "download_today_launcher_log",
        "retry_backend_connection",
        "window_hide",
        "window_minimize",
        "window_toggle_maximize",
        "window_close",
        "get_launcher_preferences",
        "set_launcher_close_behavior",
        "window_choose_close_behavior",
        "window_exit_application",
        "window_start_dragging",
        "window_is_maximized",
    ];
    let attributes = tauri_build::Attributes::new()
        .windows_attributes(windows)
        .app_manifest(tauri_build::AppManifest::new().commands(COMMANDS));
    tauri_build::try_build(attributes).expect("failed to run tauri build script");
    println!("cargo:rerun-if-changed=icons/");
    for (name, default) in [
        ("AZURPILOT_UPDATE_MANIFEST_URL", ""),
        ("AZURPILOT_UPDATE_PUBLIC_KEY", ""),
        ("AZURPILOT_UPDATE_PAYLOAD_ORIGINS", ""),
        ("AZURPILOT_UPDATE_METADATA_ORIGINS", ""),
        ("AZURPILOT_UPDATE_CHANNEL", "stable"),
        ("AZURPILOT_UPDATE_MIN_SEQUENCE", "0"),
    ] {
        println!("cargo:rerun-if-env-changed={name}");
        let value = env::var(name).unwrap_or_else(|_| default.to_owned());
        assert!(
            !value.contains(['\r', '\n', '\0']),
            "invalid build configuration {name}"
        );
        println!("cargo:rustc-env={name}={value}");
    }
    println!("cargo:rerun-if-env-changed=ALAS_BOOTSTRAP_UV");
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR"));
    let target = out_dir.join("bootstrap_uv.bin");
    if let Some(source) = env::var_os("ALAS_BOOTSTRAP_UV") {
        fs::copy(&source, &target).expect("copy ALAS_BOOTSTRAP_UV");
        println!("cargo:rerun-if-changed={}", PathBuf::from(source).display());
    } else {
        fs::write(target, []).expect("write bootstrap uv placeholder");
        println!("cargo:warning=ALAS_BOOTSTRAP_UV is not set; launcher will use PATH uv for local builds");
    }
}
