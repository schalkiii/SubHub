#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::net::TcpStream;
use std::time::{Duration, Instant};

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
            let url = tauri::WebviewUrl::External(window_url.parse().unwrap());
            tauri::WebviewWindowBuilder::new(app, "main", url)
                .title("SubHub")
                .inner_size(1280.0, 820.0)
                .resizable(true)
                .build()?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running SubHub");
}
