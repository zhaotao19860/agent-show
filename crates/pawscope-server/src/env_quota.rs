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
    pub access_sku: Option<String>,
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
                access_sku: None,
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
                access_sku: None,
                error: Some(format!("Failed to fetch: {e}")),
            }),
        ),
    }
}

#[derive(Deserialize, Debug)]
struct CopilotInternalResponse {
    #[serde(default)]
    copilot_plan: Option<String>,
    #[serde(default)]
    chat_enabled: Option<bool>,
    #[serde(default)]
    access_type_sku: Option<String>,
    #[serde(default)]
    premium_requests_used: Option<u64>,
    #[serde(default)]
    premium_requests_limit: Option<u64>,
    #[serde(default)]
    next_cycle_start_date: Option<String>,
}

async fn fetch_copilot_usage(token: &str) -> anyhow::Result<CopilotQuota> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()?;

    // Use the copilot_internal/user endpoint (works with standard PATs)
    let resp = client
        .get("https://api.github.com/copilot_internal/user")
        .header("Authorization", format!("token {token}"))
        .header("User-Agent", "Pawscope/1.0")
        .header("Accept", "application/json")
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("GitHub API {status}: {body}");
    }

    let data: CopilotInternalResponse = resp.json().await?;

    let chat_enabled = data.chat_enabled.unwrap_or(false);
    let plan = data.copilot_plan.clone();
    let access_sku = data.access_type_sku.clone();

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

    let available = plan.is_some();

    Ok(CopilotQuota {
        available,
        chat_enabled,
        premium_requests_used: data.premium_requests_used,
        premium_requests_limit: data.premium_requests_limit,
        alert_level,
        reset_at: data.next_cycle_start_date,
        plan,
        access_sku,
        error: None,
    })
}

// ---------------------------------------------------------------------------
// GET /api/usage/providers — AI provider usage aggregation from sessions
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct ProviderUsage {
    pub providers: Vec<ProviderStats>,
    pub total_tokens_in: u64,
    pub total_tokens_out: u64,
    pub total_sessions: u32,
}

#[derive(Serialize)]
pub struct ProviderStats {
    pub name: String,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub sessions: u32,
    pub models: Vec<String>,
}

fn model_to_provider(model: &str) -> &str {
    let m = model.to_lowercase();
    if m.contains("claude") || m.contains("anthropic") || m.contains("sonnet") || m.contains("opus") || m.contains("haiku") {
        "Claude"
    } else if m.contains("codex") {
        "Codex"
    } else if m.contains("gpt") || m.contains("o1") || m.contains("o3") || m.contains("o4") {
        "GPT"
    } else if m.contains("gemini") {
        "Gemini"
    } else if m.contains("deepseek") {
        "DeepSeek"
    } else {
        "Other"
    }
}

pub async fn get_provider_usage(State(s): State<AppState>) -> impl IntoResponse {
    let sessions = match s.adapter.list_sessions().await {
        Ok(v) => v,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };

    use std::collections::{HashMap, HashSet};
    struct Accum {
        tokens_in: u64,
        tokens_out: u64,
        sessions: u32,
        models: HashSet<String>,
    }

    let mut map: HashMap<&str, Accum> = HashMap::new();
    let mut total_in: u64 = 0;
    let mut total_out: u64 = 0;

    let mut handles = Vec::with_capacity(sessions.len());
    for sess in &sessions {
        let adapter = s.adapter.clone();
        let id = sess.id.clone();
        let model = sess.model.clone();
        handles.push(tokio::spawn(async move {
            let detail = adapter.get_detail(&id).await.ok();
            (model, detail)
        }));
    }

    let mut results: Vec<(Option<String>, Option<pawscope_core::SessionDetail>)> = Vec::new();
    for h in handles {
        if let Ok(r) = h.await {
            results.push(r);
        }
    }

    for (model_opt, detail_opt) in &results {
        let model_str = model_opt.as_deref().unwrap_or("unknown");
        let provider = model_to_provider(model_str);
        let entry = map.entry(provider).or_insert_with(|| Accum {
            tokens_in: 0,
            tokens_out: 0,
            sessions: 0,
            models: HashSet::new(),
        });
        entry.sessions += 1;
        entry.models.insert(model_str.to_string());
        if let Some(d) = detail_opt {
            entry.tokens_in += d.tokens_in;
            entry.tokens_out += d.tokens_out;
            total_in += d.tokens_in;
            total_out += d.tokens_out;
        }
    }

    let mut providers: Vec<ProviderStats> = map
        .into_iter()
        .map(|(name, a)| ProviderStats {
            name: name.to_string(),
            tokens_in: a.tokens_in,
            tokens_out: a.tokens_out,
            sessions: a.sessions,
            models: a.models.into_iter().collect(),
        })
        .collect();
    providers.sort_by(|a, b| (b.tokens_in + b.tokens_out).cmp(&(a.tokens_in + a.tokens_out)));

    Json(ProviderUsage {
        providers,
        total_tokens_in: total_in,
        total_tokens_out: total_out,
        total_sessions: results.len() as u32,
    })
    .into_response()
}
