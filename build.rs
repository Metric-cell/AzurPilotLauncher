//! 编译期资源与权限声明；保留默认 mTLS 发行配置及显式签名更新配置。
use std::{env, fs, path::PathBuf};

use base64::{prelude::BASE64_STANDARD, Engine};

const MTLS_IDENTITY_ENV: &str = "ALAS_LAUNCHER_MTLS_IDENTITY_PEM_B64";
const REQUIRE_MTLS_ENV: &str = "REQUIRE_LAUNCHER_MTLS_IDENTITY";
const LAUNCHER_UPDATE_URL_ENV: &str = "LAUNCHER_UPDATE_URL";
const DEFAULT_LAUNCHER_UPDATE_URL: &str =
    "https://ap-launcher-update.nanoda.work/updata/stable.json";

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
    println!("cargo:rerun-if-env-changed={MTLS_IDENTITY_ENV}");
    println!("cargo:rerun-if-env-changed={REQUIRE_MTLS_ENV}");
    println!("cargo:rerun-if-env-changed={LAUNCHER_UPDATE_URL_ENV}");
    let launcher_update_url = env::var(LAUNCHER_UPDATE_URL_ENV)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_LAUNCHER_UPDATE_URL.to_owned());
    assert!(
        !launcher_update_url.contains(['\r', '\n', '\0']),
        "invalid build configuration {LAUNCHER_UPDATE_URL_ENV}"
    );
    println!("cargo:rustc-env={LAUNCHER_UPDATE_URL_ENV}={launcher_update_url}");
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

    let mtls_identity_target = out_dir.join("launcher_mtls_identity.pem");
    // mTLS 证书解析顺序：环境变量优先，其次仓库根目录的本地证书文件，
    // 都没有时按是否强制（REQUIRE_LAUNCHER_MTLS_IDENTITY）决定成败。
    let mtls_identity = match env::var(MTLS_IDENTITY_ENV) {
        Ok(encoded) if !encoded.trim().is_empty() => Some(
            BASE64_STANDARD
                .decode(encoded.trim().as_bytes())
                // 解码失败说明环境变量内容损坏，立即失败而非静默降级。
                .expect("decode ALAS_LAUNCHER_MTLS_IDENTITY_PEM_B64"),
        ),
        _ => {
            // 未设置环境变量时回退到仓库根目录的"证书.txt"，仅供开发使用。
            let manifest_dir =
                PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
            let local_cert_path = manifest_dir.join("证书.txt");
            println!("cargo:rerun-if-changed={}", local_cert_path.display());
            if local_cert_path.exists() {
                println!(
                    "cargo:warning=ALAS_LAUNCHER_MTLS_IDENTITY_PEM_B64 is not set; using local {}",
                    local_cert_path.display()
                );
                Some(fs::read(&local_cert_path).expect("read local certificate file"))
            } else {
                // 发布构建通过 REQUIRE_LAUNCHER_MTLS_IDENTITY 强制提供证书；
                // 缺失必须失败，绝不允许无证书的构建混入发布渠道。
                if env::var_os(REQUIRE_MTLS_ENV).is_some() {
                    panic!("ALAS_LAUNCHER_MTLS_IDENTITY_PEM_B64 is required but not set");
                }
                println!(
                    "cargo:warning=ALAS_LAUNCHER_MTLS_IDENTITY_PEM_B64 is not set; launcher update client will build without mTLS identity"
                );
                None
            }
        }
    };
    // 无论有无证书都写出文件：空文件仅表示载荷客户端不携带 mTLS 身份。
    match mtls_identity {
        Some(bytes) => {
            fs::write(&mtls_identity_target, bytes).expect("write launcher mTLS identity")
        }
        None => {
            fs::write(&mtls_identity_target, []).expect("write empty launcher mTLS placeholder")
        }
    }
}
