use crate::AppState;
use axum::{Json, extract::State, http::StatusCode, response::IntoResponse};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// GET /api/env — local environment info
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct EnvInfo {
    pub ip: String,
    pub country: String,
    pub city: String,
    pub proxy: Option<String>,
    pub os: String,
    pub hostname: String,
}

pub async fn get_env() -> impl IntoResponse {
    let proxy = std::env::var("https_proxy")
        .or_else(|_| std::env::var("HTTPS_PROXY"))
        .or_else(|_| std::env::var("http_proxy"))
        .or_else(|_| std::env::var("HTTP_PROXY"))
        .or_else(|_| std::env::var("ALL_PROXY"))
        .ok();

    let os = std::env::consts::OS.to_string();
    let hostname = hostname::get()
        .map(|h| h.to_string_lossy().to_string())
        .unwrap_or_else(|_| "unknown".into());

    // Fetch public IP from ipinfo.io (fast, no auth needed)
    let (ip, country, city) = match fetch_ip_info().await {
        Ok(info) => (info.ip, info.country, info.city),
        Err(_) => ("unknown".into(), "".into(), "".into()),
    };

    Json(EnvInfo {
        ip,
        country,
        city,
        proxy,
        os,
        hostname,
    })
}

#[derive(Deserialize)]
struct IpInfoResponse {
    #[serde(default)]
    ip: String,
    #[serde(default)]
    country: String,
    #[serde(default)]
    city: String,
}

async fn fetch_ip_info() -> anyhow::Result<IpInfoResponse> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()?;
    let resp = client
        .get("https://ipinfo.io/json")
        .header("Accept", "application/json")
        .send()
        .await?
        .json::<IpInfoResponse>()
        .await?;
    Ok(resp)
}

// ---------------------------------------------------------------------------
// GET /api/copilot/quota — GitHub Copilot usage quota
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct CopilotQuota {
    pub available: bool,
    pub chat_enabled: bool,
    pub premium_requests_used: Option<u64>,
    pub premium_requests_limit: Option<u64>,
    pub alert_level: String, // "ok" | "warning" | "critical"
    pub reset_at: Option<String>,
    pub plan: Option<String>,
    pub error: Option<String>,
}

pub async fn get_copilot_quota(State(s): State<AppState>) -> impl IntoResponse {
    let auth = s.auth.snapshot().await;
    if auth.github_token.is_empty() {
        return (
            StatusCode::OK,
            Json(CopilotQuota {
                available: false,
                chat_enabled: false,
                premium_requests_used: None,
                premium_requests_limit: None,
                alert_level: "ok".into(),
                reset_at: None,
                plan: None,
                error: Some("Not logged in".into()),
            }),
        );
    }

    match fetch_copilot_usage(&auth.github_token).await {
        Ok(quota) => (StatusCode::OK, Json(quota)),
        Err(e) => (
            StatusCode::OK,
            Json(CopilotQuota {
                available: false,
                chat_enabled: false,
                premium_requests_used: None,
                premium_requests_limit: None,
                alert_level: "ok".into(),
                reset_at: None,
                plan: None,
                error: Some(format!("Failed to fetch: {e}")),
            }),
        ),
    }
}

#[derive(Deserialize, Debug)]
struct CopilotApiResponse {
    #[serde(default)]
    copilot_chat: Option<String>,
    #[serde(default)]
    copilot_ide_chat: Option<String>,
    #[serde(default)]
    premium_requests_used: Option<u64>,
    #[serde(default)]
    premium_requests_limit: Option<u64>,
    #[serde(default)]
    next_cycle_start_date: Option<String>,
    #[serde(default)]
    plan_type: Option<String>,
}

async fn fetch_copilot_usage(token: &str) -> anyhow::Result<CopilotQuota> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()?;

    let resp = client
        .get("https://api.github.com/copilot_internal/v2/token")
        .header("Authorization", format!("token {token}"))
        .header("User-Agent", "Pawscope/1.0")
        .header("Accept", "application/json")
        .send()
        .await?;

    if !resp.status().is_success() {
        // Try the billing endpoint instead
        return fetch_copilot_billing(token).await;
    }

    // The token endpoint may not have usage info; try billing
    fetch_copilot_billing(token).await
}

async fn fetch_copilot_billing(token: &str) -> anyhow::Result<CopilotQuota> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()?;

    // Try the user copilot endpoint
    let resp = client
        .get("https://api.github.com/user/copilot")
        .header("Authorization", format!("token {token}"))
        .header("User-Agent", "Pawscope/1.0")
        .header("Accept", "application/json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("GitHub API {status}: {body}");
    }

    let data: CopilotApiResponse = resp.json().await?;

    let chat_enabled = data
        .copilot_chat
        .as_deref()
        .map(|v| v == "enabled")
        .unwrap_or(false)
        || data
            .copilot_ide_chat
            .as_deref()
            .map(|v| v == "enabled")
            .unwrap_or(false);

    let used = data.premium_requests_used.unwrap_or(0);
    let limit = data.premium_requests_limit.unwrap_or(0);

    let alert_level = if limit == 0 {
        "ok".to_string()
    } else {
        let pct = (used as f64 / limit as f64) * 100.0;
        if pct >= 95.0 {
            "critical".to_string()
        } else if pct >= 80.0 {
            "warning".to_string()
        } else {
            "ok".to_string()
        }
    };

    Ok(CopilotQuota {
        available: true,
        chat_enabled,
        premium_requests_used: data.premium_requests_used,
        premium_requests_limit: data.premium_requests_limit,
        alert_level,
        reset_at: data.next_cycle_start_date,
        plan: data.plan_type,
        error: None,
    })
}
