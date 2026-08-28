//! Reusable reference data borrowed from mature competitors.
//!
//! - Speed-test / alive / unlock targets are lifted from **BestSub**
//!   (`internal/core/check/checker/{alive,speed,tiktok}.go`): the alive probe
//!   is `gstatic generate_204` (expect 204), bandwidth uses cloudflare's
//!   `__down`/`__up` endpoints, TikTok unlock uses tiktok.com.
//! - Outbound geo-IP channels are lifted from **BestSub**
//!   (`internal/modules/country/channel/*`): a prioritized list of free
//!   geo-IP services used to detect a proxy's exit country.
//! - Streaming-unlock services are modelled on BestSub's `checker/tiktok.go`
//!   (parse the `"region":` JSON field) plus the well-established community
//!   media-unlock heuristics (Netflix / Disney+ / YouTube Premium / ChatGPT).
// 原 BestSub 用 gstatic generate_204，但「节点出口 → 测速目标」链路中该域常被
// 干扰/不可达（国内代理环境尤甚），实测 HTTP 存活探测全失败。改用数据中心普遍
// 可达的主机（Hetzner 主页，返回 200），让延迟探测真正生效。
/// HTTP "is the proxy alive / what is its latency" probe.
// HTTP "is the proxy alive / what is its latency" probe.

/// 原用 Hetzner 主页作存活目标，但本环境（及部分国内网络）Hetzner 被封锁/污染，
/// 实测 `http=000` 完全不可达，导致存活探测全失败、节点被误判不可用（TCP 握手
/// 能过就标「可用」的假绿，正是此前「SubHub 绿但 clash 报死」的同类问题）。
/// 改用数据中心普遍可达的 OVH 分片作存活目标。
pub const ALIVE_TARGET: &str = "https://proof.ovh.net/files/1Mb.dat";
pub const ALIVE_EXPECT_CODE: u16 = 200;

/// Bandwidth test endpoints. Each entry is tried in order; the first that yields
/// bytes wins. On failure we fall through to the next, smaller / different-host
/// shard to maximize the chance of getting a *real* measurement instead of a
/// silent `None`.
///
/// 原 BestSub 采用 cloudflare `__down`。但在「节点出口 → 测速目标」链路中该域常被
/// 干扰/不可达（Rust 侧实测所有节点均 `error sending request`），导致引擎带宽探测
/// 全失败、速度列空白——而 clash 用另一套目标能正常测速。这里改为跨多主机
/// （Hetzner / OVH / Tele2，均为数据中心常规可达点）的分片列表：先 10MB 档（快节点
/// 取真实带宽），再 1MB 档兜底（慢节点也能在带宽超时内下完）。(上传端点未接入，
/// 见 `core/src/speedtest.rs`。)
/// Bandwidth test endpoints, tried in order; first that yields bytes wins.
///
/// 1MB 档放前面：慢节点也能在带宽超时（默认 30s）内下完，给出真实测速，而不是
/// 10MB 档在慢节点上 30s 下不完 → 超时失败 → 静默回退 TCP 假测量（节点显示绿色
/// 但从未被真实代理校验过）。10MB 档留给快节点取更准的带宽；Hetzner 放最后兜底
/// （本环境实测不可达，但部分网络仍可达）。
pub const SPEED_DOWNLOAD_URLS: &[&str] = &[
    "https://proof.ovh.net/files/1Mb.dat",
    "https://speedtest.tele2.net/1MB.zip",
    "https://proof.ovh.net/files/10Mb.dat",
    "https://speedtest.tele2.net/10MB.zip",
    "https://speed.hetzner.de/1MB.bin",
];
/// Backward-compatible alias pointing at the primary (largest) shard.
pub const SPEED_DOWNLOAD_URL: &str = SPEED_DOWNLOAD_URLS[0];

/// Outbound geo-IP channels (BestSub `internal/modules/country/channel/*`).
/// Each channel is tried in order; the first that returns a 2-letter country
/// code wins. `keys` are the JSON fields to inspect (empty = parse cloudflare
/// `cdn-cgi/trace` text, which uses `loc=XX`).
pub struct GeoChannel {
    pub name: &'static str,
    pub url: &'static str,
    pub keys: &'static [&'static str],
}

pub const GEO_CHANNELS: &[GeoChannel] = &[
    GeoChannel {
        name: "cloudflare-trace",
        url: "https://cloudflare.com/cdn-cgi/trace",
        keys: &[],
    },
    GeoChannel {
        name: "cloudflare-meta",
        url: "https://speed.cloudflare.com/meta",
        keys: &["country"],
    },
    GeoChannel {
        name: "freeipapi",
        url: "https://free.freeipapi.com/api/json",
        keys: &["country_code"],
    },
    GeoChannel {
        name: "ip.sb",
        url: "https://api.ip.sb/geoip",
        keys: &["countryCode"],
    },
    GeoChannel {
        name: "ipapi.co",
        url: "https://ipapi.co/json",
        keys: &["country_code"],
    },
    GeoChannel {
        name: "myip",
        url: "https://api.myip.com",
        keys: &["country"],
    },
    GeoChannel {
        name: "reallyfreegeoip",
        url: "https://reallyfreegeoip.org/json",
        keys: &["country_code"],
    },
];

/// Pull a 2-letter country code out of a geo-channel response body.
/// Tries the declared JSON keys, then falls back to cloudflare-trace's
/// `loc=XX` text format.
pub fn extract_country(body: &str) -> Option<String> {
    // JSON path
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(body) {
        for key in ["country", "country_code", "countryCode"] {
            if let Some(s) = v.get(key).and_then(|x| x.as_str()) {
                let s = s.trim().to_uppercase();
                if s.len() == 2 && s.chars().all(|c| c.is_ascii_alphabetic()) {
                    return Some(s);
                }
            }
        }
    }
    // cloudflare cdn-cgi/trace: `loc=US`
    for line in body.lines() {
        if let Some(rest) = line.strip_prefix("loc=") {
            let s = rest.trim().to_uppercase();
            if s.len() == 2 && s.chars().all(|c| c.is_ascii_alphabetic()) {
                return Some(s);
            }
        }
    }
    None
}

// ----------------------- streaming-unlock detection -----------------------

use crate::model::UnlockResult;

/// How a streaming service's HTTP response is classified into an unlock state.
/// Modeled on BestSub's `checker/tiktok.go` `detectTikTok` (JSON region parse)
/// and extended with the standard community media-unlock heuristics.
pub enum UnlockDetect {
    /// BestSub `tiktok.go`: GET `region/get/`, parse JSON `"region"`.
    TikTok,
    /// Netflix: GET a title page; blocked when the page says it is not
    /// available in your country, otherwise unlocked (region from `<html lang>`).
    Netflix,
    /// Disney+: blocked when the page says unavailable in your region.
    Disney,
    /// YouTube Premium: unlocked when the premium page loads (`<html lang>` region).
    YouTube,
    /// Generic: 200 + body does NOT contain any `block` string => unlocked.
    Body { block: &'static [&'static str] },
}

impl UnlockDetect {
    /// Classify an HTTP response into an unlock state.
    pub fn classify(&self, status: u16, body: &str) -> UnlockResult {
        match self {
            UnlockDetect::TikTok => {
                if status != 200 {
                    return UnlockResult {
                        status: "blocked".into(),
                        region: None,
                    };
                }
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(body) {
                    if let Some(r) = v.get("region").and_then(|x| x.as_str()) {
                        if !r.is_empty() {
                            return UnlockResult {
                                status: "unlocked".into(),
                                region: Some(r.trim().to_uppercase()),
                            };
                        }
                    }
                }
                UnlockResult {
                    status: "blocked".into(),
                    region: None,
                }
            }
            UnlockDetect::Netflix => {
                let blocked = body.contains("is not available in your country")
                    || body.contains("Not Available in Your Country");
                if status == 200 && !blocked {
                    UnlockResult {
                        status: "unlocked".into(),
                        region: extract_html_lang_region(body),
                    }
                } else {
                    UnlockResult {
                        status: "blocked".into(),
                        region: None,
                    }
                }
            }
            UnlockDetect::Disney => {
                let low = body.to_lowercase();
                let blocked = low.contains("is not available in your country")
                    || low.contains("not available in your region")
                    || low.contains("disney+ is not available");
                if status == 200 && !blocked && low.contains("disney+") {
                    UnlockResult {
                        status: "unlocked".into(),
                        region: extract_html_lang_region(body),
                    }
                } else {
                    UnlockResult {
                        status: "blocked".into(),
                        region: None,
                    }
                }
            }
            UnlockDetect::YouTube => {
                if status == 200 && body.to_lowercase().contains("youtube premium") {
                    UnlockResult {
                        status: "unlocked".into(),
                        region: extract_html_lang_region(body),
                    }
                } else {
                    UnlockResult {
                        status: "blocked".into(),
                        region: None,
                    }
                }
            }
            UnlockDetect::Body { block } => {
                let low = body.to_lowercase();
                let blocked = block.iter().any(|b| low.contains(&b.to_lowercase()));
                if status == 200 && !blocked {
                    UnlockResult {
                        status: "unlocked".into(),
                        region: None,
                    }
                } else {
                    UnlockResult {
                        status: "blocked".into(),
                        region: None,
                    }
                }
            }
        }
    }
}

/// A streaming service to probe for unlock status.
pub struct StreamService {
    pub id: &'static str,
    pub name: &'static str,
    pub url: &'static str,
    pub detect: UnlockDetect,
}

/// Streaming services probed per proxy. Borrowed/configured from BestSub's
/// unlock model (`checker/tiktok.go`) + common community checks.
pub const STREAM_SERVICES: &[StreamService] = &[
    StreamService {
        id: "tiktok",
        name: "TikTok",
        url: "https://www.tiktok.com/api/passport/web/region/get/",
        detect: UnlockDetect::TikTok,
    },
    StreamService {
        id: "netflix",
        name: "Netflix",
        url: "https://www.netflix.com/title/81215567",
        detect: UnlockDetect::Netflix,
    },
    StreamService {
        id: "disney",
        name: "Disney+",
        url: "https://www.disneyplus.com/",
        detect: UnlockDetect::Disney,
    },
    StreamService {
        id: "youtube",
        name: "YouTube Premium",
        url: "https://www.youtube.com/premium",
        detect: UnlockDetect::YouTube,
    },
    StreamService {
        id: "chatgpt",
        name: "ChatGPT",
        url: "https://chat.openai.com/",
        detect: UnlockDetect::Body {
            block: &[
                "not available in your country",
                "your country is not supported",
            ],
        },
    },
];

/// Pull a 2-letter region from an HTML `<html lang="xx-YY">` attribute.
fn extract_html_lang_region(html: &str) -> Option<String> {
    let i = html.find("lang=\"")?;
    let s = i + 6;
    let e = html[s..].find('"')? + s;
    let lang = &html[s..e]; // e.g. "en-US"
    let region = lang.split('-').nth(1)?;
    let r = region.to_uppercase();
    if r.len() == 2 && r.chars().all(|c| c.is_ascii_alphabetic()) {
        Some(r)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_country_from_json_keys() {
        assert_eq!(extract_country(r#"{"country":"US"}"#), Some("US".into()));
        assert_eq!(
            extract_country(r#"{"country_code":"JP"}"#),
            Some("JP".into())
        );
        assert_eq!(
            extract_country(r#"{"countryCode":"DE"}"#),
            Some("DE".into())
        );
    }

    #[test]
    fn extract_country_from_cloudflare_trace() {
        let body = "fl=...\nip=1.2.3.4\nloc=FR\nts=...\n";
        assert_eq!(extract_country(body), Some("FR".into()));
    }

    #[test]
    fn extract_country_rejects_non_two_letter() {
        // 非 2 字母 ISO 代码（如 "USA"）必须被拒绝，避免误报。
        assert_eq!(extract_country(r#"{"country":"USA"}"#), None);
        assert_eq!(extract_country("garbage"), None);
    }

    #[test]
    fn html_lang_region_parsed() {
        assert_eq!(
            extract_html_lang_region(r#"<html lang="en-US">"#),
            Some("US".into())
        );
        assert_eq!(
            extract_html_lang_region(r#"<html lang="zh-CN">"#),
            Some("CN".into())
        );
        assert_eq!(extract_html_lang_region("<html>"), None);
    }

    #[test]
    fn tiktok_unlock_classify() {
        let ok = UnlockDetect::TikTok.classify(200, r#"{"region":"US"}"#);
        assert_eq!(ok.status, "unlocked");
        assert_eq!(ok.region, Some("US".into()));
        // 缺 region 字段视为 blocked
        assert_eq!(UnlockDetect::TikTok.classify(200, "{}").status, "blocked");
        // 非 200 视为 blocked
        assert_eq!(
            UnlockDetect::TikTok
                .classify(403, r#"{"region":"US"}"#)
                .status,
            "blocked"
        );
    }

    #[test]
    fn netflix_unlock_classify() {
        let ok = UnlockDetect::Netflix.classify(200, "<html lang=\"en-US\">Netflix");
        assert_eq!(ok.status, "unlocked");
        let blocked =
            UnlockDetect::Netflix.classify(200, "This title is not available in your country");
        assert_eq!(blocked.status, "blocked");
    }

    #[test]
    fn disney_unlock_classify() {
        let ok = UnlockDetect::Disney.classify(200, "<html lang=\"en-US\">disney+ content");
        assert_eq!(ok.status, "unlocked");
        let blocked = UnlockDetect::Disney.classify(200, "Disney+ is not available in your region");
        assert_eq!(blocked.status, "blocked");
    }

    #[test]
    fn youtube_unlock_classify() {
        let ok = UnlockDetect::YouTube.classify(200, "Welcome to YouTube Premium");
        assert_eq!(ok.status, "unlocked");
        let blocked = UnlockDetect::YouTube.classify(200, "some page");
        assert_eq!(blocked.status, "blocked");
    }

    #[test]
    fn body_unlock_classify() {
        let detect = UnlockDetect::Body {
            block: &["not available in your country"],
        };
        assert_eq!(detect.classify(200, "welcome").status, "unlocked");
        assert_eq!(
            detect.classify(200, "not available in your country").status,
            "blocked"
        );
        // 非 200 一律 blocked
        assert_eq!(detect.classify(404, "welcome").status, "blocked");
    }
}
