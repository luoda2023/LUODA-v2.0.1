#[cfg(windows)]
fn build_windows() {
    // Link libsodium for Windows
    if let Ok(sodium_lib_dir) = std::env::var("SODIUM_LIB_DIR") {
        println!("cargo:rustc-link-search=native={}", sodium_lib_dir);
    }
    // MSVC: vcpkg installs sodium.lib => link "sodium"
    // GNU/MinGW: vcpkg installs libsodium.a => link "libsodium"
    #[cfg(target_env = "msvc")]
    println!("cargo:rustc-link-lib=libsodium");
    #[cfg(not(target_env = "msvc"))]
    println!("cargo:rustc-link-lib=libsodium");

    let file = "src/platform/windows.cc";
    let file2 = "src/platform/windows_delete_test_cert.cc";
    cc::Build::new().file(file).file(file2).compile("windows");
    println!("cargo:rustc-link-lib=WtsApi32");
    println!("cargo:rerun-if-changed={}", file);
    println!("cargo:rerun-if-changed={}", file2);
}

#[cfg(target_os = "macos")]
fn build_mac() {
    let file = "src/platform/macos.mm";
    let mut b = cc::Build::new();
    if let Ok(os_version::OsVersion::MacOS(v)) = os_version::detect() {
        let v = v.version;
        if v.contains("10.14") {
            b.flag("-DNO_InputMonitoringAuthStatus=1");
        }
    }
    b.flag("-std=c++17").file(file).compile("macos");
    println!("cargo:rerun-if-changed={}", file);
}

/// Inject the build timestamp consumed by `src/version.rs` (`BUILD_DATE`).
///
/// Previously this was a literal in `src/version.rs` written by
/// `hbb_common::gen_version()`, which nothing ever called — so it went stale the
/// moment a release was cut from a tree where nobody remembered to edit it.
fn build_date() {
    println!("cargo:rerun-if-changed=build.rs");
    let stamp = match std::env::var("SOURCE_DATE_EPOCH") {
        // Reproducible builds: honour the epoch if the CI sets one.
        Ok(epoch) => match epoch.parse::<i64>() {
            Ok(secs) => chrono::DateTime::from_timestamp(secs, 0)
                .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
                .unwrap_or_else(|| "unknown".to_owned()),
            Err(_) => "unknown".to_owned(),
        },
        Err(_) => chrono::Local::now().format("%Y-%m-%d %H:%M").to_string(),
    };
    println!("cargo:rustc-env=LDESK_BUILD_DATE={stamp}");
}

fn main() {
    build_date();
    #[cfg(windows)]
    build_windows();
    #[cfg(target_os = "macos")]
    build_mac();
}

