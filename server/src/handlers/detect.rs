//! 流媒体解锁检测：逐个节点拉起引擎，探测各流媒体服务的解锁状态与解锁地区。
use crate::*;
use crate::{apply_mode, engine_bin_of, flatten_dedup, now_ms, persist_all, LockExt};
use axum::{extract::State, Json};
use subhub_core::{Proxy, ProxyUnlock};
pub(crate) async fn unlock_detect(
    State(state): State<AppState>,

    Json(req): Json<SpeedTestReq>,
) -> Json<Vec<serde_json::Value>> {
    let timeout_ms = req.timeout_ms.unwrap_or(8000);

    let mut proxies: Vec<Proxy> = {
        let guard = state.store.lock_ok();

        flatten_dedup(&guard, None)
    };
    let mode = req.mode.clone().unwrap_or_default();
    if !mode.is_empty() {
        proxies = apply_mode(proxies, &mode);
    }

    let unlocks: Vec<ProxyUnlock> = if let Some(bin) = engine_bin_of(&state) {
        engine::detect_unlock(&proxies, &bin, timeout_ms, proxies.len()).await
    } else {
        vec![ProxyUnlock::default(); proxies.len()]
    };

    let now = now_ms();

    // Index results by fingerprint once (O(N)) instead of a linear `.find()`

    // per stored proxy (was O(N²)).

    let unlock_by_fp: std::collections::HashMap<String, ProxyUnlock> = proxies
        .iter()
        .zip(unlocks.iter())
        .map(|(p, u)| (p.fingerprint(), u.clone()))
        .collect();

    {
        let mut guard = state.store.lock_ok();

        for sub in guard.iter_mut() {
            for p in sub.proxies.iter_mut() {
                if let Some(u) = unlock_by_fp.get(&p.fingerprint()) {
                    p.unlock = Some(u.clone());

                    p.last_tested_at = Some(now);
                }
            }
        }
    }

    persist_all(&state);

    Json(
        proxies
            .iter()
            .zip(unlocks.iter())
            .map(|(p, u)| {
                serde_json::json!({

                    "name": p.name,

                    "server": p.server,

                    "unlock": u.summary(),

                })
            })
            .collect(),
    )
}
