fn main() {
    // WebUI 改动后必须触发重新构建：否则 cargo 不会重跑 build script，
    // `tauri_build::build()` 会使用缓存的旧前端，重新编译并运行后界面
    // 依旧是旧版（表现为修改 webui 无效，进度条等 UI 修复“不生效”）。
    // 显式监听 webui 目录与关键文件，保证前端被重新嵌入 exe。
    println!("cargo:rerun-if-changed=../webui");
    println!("cargo:rerun-if-changed=../webui/index.html");
    println!("cargo:rerun-if-changed=../webui/app.js");
    println!("cargo:rerun-if-changed=../webui/style.css");
    tauri_build::build()
}
