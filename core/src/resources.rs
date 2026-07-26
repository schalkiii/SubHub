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
/// HTTP "is the proxy alive / what is its latency" probe (BestSub `alive.go`).
pub const ALIVE_TARGET: &str = "https://www.gstatic.com/generate_204";
pub const ALIVE_EXPECT_CODE: u16 = 204;

/// Bandwidth test endpoint (BestSub `speed.go`). Used for the download-speed
/// half of the speed test. (The upload endpoint is intentionally not wired up
/// — see `core/src/speedtest.rs`, which only measures download throughput.)
pub const SPEED_DOWNLOAD_URL: &str = "https://speed.cloudflare.com/__down?bytes=104857600";

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
                    return UnlockResult { status: "blocked".into(), region: None };
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
                UnlockResult { status: "blocked".into(), region: None }
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
                    UnlockResult { status: "blocked".into(), region: None }
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
                    UnlockResult { status: "blocked".into(), region: None }
                }
            }
            UnlockDetect::YouTube => {
                if status == 200 && body.to_lowercase().contains("youtube premium") {
                    UnlockResult {
                        status: "unlocked".into(),
                        region: extract_html_lang_region(body),
                    }
                } else {
                    UnlockResult { status: "blocked".into(), region: None }
                }
            }
            UnlockDetect::Body { block } => {
                let low = body.to_lowercase();
                let blocked = block.iter().any(|b| low.contains(&b.to_lowercase()));
                if status == 200 && !blocked {
                    UnlockResult { status: "unlocked".into(), region: None }
                } else {
                    UnlockResult { status: "blocked".into(), region: None }
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
            block: &["not available in your country", "your country is not supported"],
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
