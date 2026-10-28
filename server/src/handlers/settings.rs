//! 应用设置的读取与写入（含导入 / 导出）。
//!
//! 设置分散存放在 `AppState` 的各独立锁中，关键项再由 `db::Db` 的 meta 表持久化。
use crate::LockExt;
use crate::*;
use axum::{
    extract::State,
    http::{header, HeaderValue},
    response::IntoResponse,
    Json,
};
/// Read global runtime settings.
/// 构造当前全局设置的响应。供 GET /api/settings、配置导出等复用。
pub(crate) fn build_settings_resp(state: &AppState) -> SettingsResp {
    SettingsResp {
        use_proxy: *state.use_proxy.lock_ok(),
        auto_refresh_sec: *state.auto_refresh_sec.lock_ok(),
        node_health_check_sec: *state.node_health_check_sec.lock_ok(),
        default_fetch_proxy: state.default_fetch_proxy.lock_ok().clone(),
        top_n: *state.top_n.lock_ok(),
        engine_bin: state.engine_bin.lock_ok().clone(),
        remove_after_fails: *state.remove_after_fails.lock_ok(),
        remove_sub_after_days: *state.remove_sub_after_days.lock_ok(),
        bind_addr: state.bind_addr.lock_ok().clone(),
        external_host: state.external_host.lock_ok().clone(),
        github_user: state.github_user.lock_ok().clone(),
        has_github_token: state
            .github_token
            .lock_ok()
            .as_ref()
            .map(|t| !t.trim().is_empty())
            .unwrap_or(false),
        lan_ip: state.lan_ip.clone(),
    }
}

pub(crate) async fn get_settings(State(state): State<AppState>) -> Json<SettingsResp> {
    Json(build_settings_resp(&state))
}

/// Patch global runtime settings (persisted to the SQLite `meta` table).
pub(crate) fn apply_settings(state: &AppState, req: SettingsReq) -> SettingsResp {
    if let Some(v) = req.use_proxy {
        *state.use_proxy.lock_ok() = v;
        if let Some(db) = &state.db {
            db.meta_set("use_proxy", if v { "1" } else { "0" });
        }
    }

    if let Some(sec) = req.auto_refresh_sec {
        let sec = clamp_schedule(sec);
        *state.auto_refresh_sec.lock_ok() = sec;

        if let Some(db) = &state.db {
            db.meta_set("auto_refresh_sec", &sec.to_string());
        }
    }

    if let Some(sec) = req.node_health_check_sec {
        let sec = clamp_schedule(sec);
        *state.node_health_check_sec.lock_ok() = sec;

        if let Some(db) = &state.db {
            db.meta_set("node_health_check_sec", &sec.to_string());
        }
    }

    // default pull-proxy: empty string clears it (None), otherwise stored as-is
    if let Some(p) = &req.default_fetch_proxy {
        let val = p.trim();
        let next = if val.is_empty() {
            None
        } else {
            Some(val.to_string())
        };
        *state.default_fetch_proxy.lock_ok() = next.clone();
        if let Some(db) = &state.db {
            db.meta_set("default_fetch_proxy", if next.is_some() { val } else { "" });
        }
    }

    // Top-N export size: persisted so it survives restarts.
    // 上限与其他数值字段一样做 sanity clamp（0 = 不限量，仍允许）。
    if let Some(n) = req.top_n {
        let n = if n > 100_000 { 100_000 } else { n };
        *state.top_n.lock_ok() = n;
        if let Some(db) = &state.db {
            db.meta_set("top_n", &n.to_string());
        }
    }

    // External engine binary: empty string clears it (engine disabled).
    if let Some(b) = &req.engine_bin {
        let val = b.trim();
        let next = if val.is_empty() {
            None
        } else {
            Some(val.to_string())
        };
        *state.engine_bin.lock_ok() = next.clone();
        if let Some(db) = &state.db {
            db.meta_set("engine_bin", if next.is_some() { val } else { "" });
        }
    }

    // Auto-remove threshold: 0 disables auto-removal (the safe default).
    if let Some(n) = req.remove_after_fails {
        let n = n.min(1000); // sanity cap; 1000 consecutive failures is absurd
        *state.remove_after_fails.lock_ok() = n;
        if let Some(db) = &state.db {
            db.meta_set("remove_after_fails", &n.to_string());
        }
    }

    if let Some(n) = req.remove_sub_after_days {
        let n = n.min(3650); // 十年上限，防止误填巨大值
        *state.remove_sub_after_days.lock_ok() = n;
        if let Some(db) = &state.db {
            db.meta_set("remove_sub_after_days", &n.to_string());
        }
    }

    // 外部访问主机：空字符串清除（回退到本机 IP / localhost）。
    if let Some(h) = &req.external_host {
        let val = h.trim();
        let next = if val.is_empty() {
            None
        } else {
            Some(val.to_string())
        };
        *state.external_host.lock_ok() = next.clone();
        if let Some(db) = &state.db {
            db.meta_set("external_host", if next.is_some() { val } else { "" });
        }
    }

    // 绑定地址：仅记录，需重启生效（重启时由 SUBHUB_BIND / 持久化值决定监听网卡）。
    if let Some(b) = &req.bind_addr {
        let val = b.trim();
        if !val.is_empty() {
            *state.bind_addr.lock_ok() = val.to_string();
            if let Some(db) = &state.db {
                db.meta_set("bind_addr", val);
            }
        }
    }

    // GitHub 账号：空字符串清除。
    if let Some(u) = &req.github_user {
        let val = u.trim();
        let next = if val.is_empty() {
            None
        } else {
            Some(val.to_string())
        };
        *state.github_user.lock_ok() = next.clone();
        if let Some(db) = &state.db {
            db.meta_set("github_user", if next.is_some() { val } else { "" });
        }
    }

    // GitHub token：空字符串清除。出于安全，get_settings 只回传「是否已配置」。
    if let Some(t) = &req.github_token {
        let val = t.trim();
        let next = if val.is_empty() {
            None
        } else {
            Some(val.to_string())
        };
        *state.github_token.lock_ok() = next.clone();
        if let Some(db) = &state.db {
            db.meta_set("github_token", if next.is_some() { val } else { "" });
        }
    }

    build_settings_resp(state)
}

/// Patch global runtime settings (persisted to the SQLite `meta` table).
pub(crate) async fn set_settings(
    State(state): State<AppState>,
    Json(req): Json<SettingsReq>,
) -> Json<SettingsResp> {
    Json(apply_settings(&state, req))
}

/// GET /api/settings/export — 导出完整配置为 JSON 文件（供备份 / 迁移到另一台机器）。
/// 出于安全，响应不包含 GitHub token 明文（与 get_settings 一致，只回传「是否已配置」）。
pub(crate) async fn export_settings(State(state): State<AppState>) -> impl IntoResponse {
    let resp = build_settings_resp(&state);
    let body = serde_json::to_string_pretty(&resp).unwrap_or_default();
    (
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            ),
            (
                header::CONTENT_DISPOSITION,
                HeaderValue::from_static("attachment; filename=\"subhub-settings.json\""),
            ),
        ],
        body,
    )
}

/// POST /api/settings/import — 从导出文件恢复配置（部分更新：只应用提供的字段，
/// 未提供的字段保持原值）。接受与 set_settings 相同的请求体。安全提示：导入文件里
/// 若含 github_token 会被写入；请仅供可信环境使用。
pub(crate) async fn import_settings(
    State(state): State<AppState>,
    Json(req): Json<SettingsReq>,
) -> Json<SettingsResp> {
    Json(apply_settings(&state, req))
}
