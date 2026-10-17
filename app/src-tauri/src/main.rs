// No console window on Windows in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // WebKitGTK's DMA-BUF renderer kills the window at start on some Wayland + NVIDIA setups ("Error 71 (Protocol
    // error) dispatching to Wayland display"). Setting the variable needs `unsafe` in Rust 2024, so Marv runs itself
    // again with it set instead. Set it to 0 yourself to keep the renderer.
    #[cfg(target_os = "linux")]
    if std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_none()
        && let Ok(exe) = std::env::current_exe()
        && let Ok(status) =
            std::process::Command::new(exe).args(std::env::args_os().skip(1)).env("WEBKIT_DISABLE_DMABUF_RENDERER", "1").status()
    {
        std::process::exit(status.code().unwrap_or(1));
    }
    marv_desktop::run();
}
