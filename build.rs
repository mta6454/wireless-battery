//! Embeds the icon, the application manifest (per-monitor DPI awareness, Common Controls 6) and
//! the version resource into the Windows exe.
//!
//! The version comes from the `WB_VERSION` environment variable when set (CI passes the release
//! tag), else from Cargo.toml.

fn main() {
    println!("cargo:rerun-if-changed=res/app.rc");
    println!("cargo:rerun-if-changed=res/app.ico");
    println!("cargo:rerun-if-changed=res/app.manifest");
    println!("cargo:rerun-if-env-changed=WB_VERSION");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let version = std::env::var("WB_VERSION")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".into()));
    // "1.2.3" or "1.2.3-beta.1": the numeric FILEVERSION takes the leading three numbers.
    let mut nums = version.split(|c: char| !c.is_ascii_digit()).filter(|s| !s.is_empty()).map(|s| s.parse::<u16>().unwrap_or(0));
    let (major, minor, patch) = (nums.next().unwrap_or(0), nums.next().unwrap_or(0), nums.next().unwrap_or(0));

    let macros = [
        format!("VER_MAJOR={major}"),
        format!("VER_MINOR={minor}"),
        format!("VER_PATCH={patch}"),
        format!("VER_STR=\"{version}\""),
    ];
    let result = embed_resource::compile("res/app.rc", macros);
    // Building on Windows must fail loudly if the resources cannot be compiled. When
    // cross-checking from another OS no resource compiler may exist, which is fine.
    if cfg!(windows) {
        result.manifest_required().unwrap();
    } else {
        let _ = result.manifest_optional();
    }
}
