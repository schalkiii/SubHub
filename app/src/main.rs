#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::net::TcpStream;
use std::time::{Duration, Instant};

use tauri::{
    Manager,
    menu::{MenuBuilder, MenuItemBuilder},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    WindowEvent,
};

fn main() {
    // The embedded server resolves SUBHUB_PORT (default 3005) for itself.
    // Resolve it here too and export it back into the environment so the Tauri
    // window and the server always agree on the same port — previously the
    // window URL was hard-coded to :3005 in tauri.conf.json, so setting
    // SUBHUB_PORT made the app navigate to a dead port.
    let port: u16 = std::env::var("SUBHUB_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(3005);
    std::env::set_var("SUBHUB_PORT", port.to_string());

    // Start the axum backend (API + WebUI host) in a background OS thread
    // with its own tokio runtime.
    std::thread::spawn(|| {
        subhub_server::run_blocking();
    });

    // Wait for the server to actually be listening before showing the window,
    // so the first `/api/health` (and every other call) never races a
    // not-yet-bound socket. Previously the window loaded immediately and the
    // initial health check always failed on launch.
    let addr = format!("127.0.0.1:{port}");
    let socket_addr = addr.parse::<std::net::SocketAddr>().expect("invalid listen addr");
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut ready = false;
    while Instant::now() < deadline {
        if TcpStream::connect_timeout(&socket_addr, Duration::from_millis(200)).is_ok() {
            ready = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    if !ready {
        eprintln!("SubHub: server did not come up on {addr} within 10s; window may be offline");
    }

    let window_url = format!("http://127.0.0.1:{port}");
    tauri::Builder::default()
        .setup(move |app| {
            // 显式关联应用默认图标，确保 Windows 任务栏图标正常显示
            // （否则部分环境下任务栏会回退为默认/空白图标）。
            let default_icon = app.default_window_icon().cloned();
            let mut builder = tauri::WebviewWindowBuilder::new(
                app,
                "main",
                tauri::WebviewUrl::External(window_url.parse().unwrap()),
            )
            .title("SubHub")
            .inner_size(1280.0, 820.0)
            .resizable(true);
            if let Some(icon) = default_icon {
                builder = builder.icon(icon)?;
            }
            builder.build()?;

            // 系统托盘：左键单击恢复窗口；右键菜单含「显示」「退出」。
            // 应用窗口默认存在，关闭主窗口时仅隐藏到托盘（见 on_window_event）。
            let show_item = MenuItemBuilder::with_id("show", "显示 SubHub").build(app)?;
            let quit_item = MenuItemBuilder::with_id("quit", "退出").build(app)?;
            let menu = MenuBuilder::new(app)
                .items(&[&show_item, &quit_item])
                .build()?;

            let _tray = TrayIconBuilder::new()
                .icon(app.default_window_icon().unwrap().clone())
                .menu(&menu)
                .on_menu_event(|app, event| match event.id().as_ref() {
                    "show" => {
                        if let Some(window) = app.get_webview_window("main") {
                            let _ = window.unminimize();
                            let _ = window.show();
                            let _ = window.set_focus();
                        }
                    }
                    "quit" => {
                        app.exit(0);
                    }
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        let app = tray.app_handle();
                        if let Some(window) = app.get_webview_window("main") {
                            let _ = window.unminimize();
                            let _ = window.show();
                            let _ = window.set_focus();
                        }
                    }
                })
                .build(app)?;

            Ok(())
        })
        // 点右上角关闭：阻止真正退出，仅隐藏窗口到托盘。
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running SubHub");
}
