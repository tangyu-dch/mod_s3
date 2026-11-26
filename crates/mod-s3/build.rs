fn main() {
    // FreeSWITCH 符号（switch_*、SWITCH_*）由宿主进程在 dlopen 时提供。
    // macOS 的 ld 默认拒绝未定义符号；dynamic_lookup 是可加载模块的标准
    // 处理方式，在其他平台上无副作用。
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-arg=-Wl,-undefined,dynamic_lookup");
    }
}
