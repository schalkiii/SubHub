fn main() {
    // 构建产物里的 WebUI 是“构建期快照”：Tauri 的 frontendDist 资源，以及
    // server 通过 include_bytes! 内嵌的发行版兜底副本。显式监听 webui，
    // 确保重新构建时这两份快照都刷新。开发运行时 server 仍优先读磁盘
    // webui/，因此改前端只需重启进程，不必重建 exe。
    println!("cargo:rerun-if-changed=../webui");
    println!("cargo:rerun-if-changed=../webui/index.html");
    println!("cargo:rerun-if-changed=../webui/app.js");
    println!("cargo:rerun-if-changed=../webui/style.css");
    tauri_build::build()
}
