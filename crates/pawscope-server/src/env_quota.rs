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
    pub quota_snapshots: Option<QuotaSnapshots>,
}

#[derive(Serialize, Default)]
pub struct QuotaSnapshots {
    pub premium: Option<QuotaEntry>,
    pub chat: Option<QuotaEntry>,
    pub completions: Option<QuotaEntry>,
}

#[derive(Serialize)]
pub struct QuotaEntry {
    pub entitlement: u64,
    pub remaining: u64,
    pub percent_remaining: f64,
    pub unlimited: bool,
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
                quota_snapshots: None,
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
                quota_snapshots: None,
            }),
        ),
    }
}

async fn fetch_copilot_usage(token: &str) -> anyhow::Result<CopilotQuota> {
    // Try PupKit's OAuth token first (has better Copilot access), fall back to provided PAT
    let effective_token = try_pupkit_token().unwrap_or_else(|| token.to_string());

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()?;

    // Use PupKit-style headers to get full quota_snapshots
    let resp = client
        .get("https://api.github.com/copilot_internal/user")
        .header("Authorization", format!("token {effective_token}"))
        .header("User-Agent", "GitHubCopilotChat/0.26.7")
        .header("Accept", "application/json")
        .header("editor-plugin-version", "copilot-chat/0.26.7")
        .header("editor-version", "vscode/1.104.3")
        .header("x-github-api-version", "2025-04-01")
        .header(
            "x-vscode-user-agent-library-version",
            "electron-fetch",
        )
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("GitHub API {status}: {body}");
    }

    let raw: serde_json::Value = resp.json().await?;

    let chat_enabled = raw.get("chat_enabled").and_then(|v| v.as_bool()).unwrap_or(false);
    let plan = raw.get("copilot_plan").and_then(|v| v.as_str()).map(String::from);
    let access_sku = raw.get("access_type_sku").and_then(|v| v.as_str()).map(String::from);
    let reset_at = raw.get("quota_reset_date").and_then(|v| v.as_str()).map(String::from);

    // Parse quota_snapshots (available when subscription is active)
    let quota_snapshots = raw.get("quota_snapshots").and_then(|snapshots| {
        let premium = snapshots.get("premium_interactions").and_then(parse_quota_entry);
        let chat = snapshots.get("chat").and_then(parse_quota_entry);
        let completions = snapshots.get("completions").and_then(parse_quota_entry);
        if premium.is_some() || chat.is_some() || completions.is_some() {
            Some(QuotaSnapshots { premium, chat, completions })
        } else {
            None
        }
    });

    // Derive premium_used/limit from quota_snapshots if available
    let (premium_used, premium_limit) = match &quota_snapshots {
        Some(qs) => match &qs.premium {
            Some(p) if !p.unlimited => {
                let used = p.entitlement.saturating_sub(p.remaining);
                (Some(used), Some(p.entitlement))
            }
            _ => (None, None),
        },
        None => (
            raw.get("premium_requests_used").and_then(|v| v.as_u64()),
            raw.get("premium_requests_limit").and_then(|v| v.as_u64()),
        ),
    };

    let alert_level = match (premium_used, premium_limit) {
        (Some(used), Some(limit)) if limit > 0 => {
            let pct = (used as f64 / limit as f64) * 100.0;
            if pct >= 95.0 { "critical".to_string() }
            else if pct >= 80.0 { "warning".to_string() }
            else { "ok".to_string() }
        }
        _ => "ok".to_string(),
    };

    let available = plan.is_some();

    Ok(CopilotQuota {
        available,
        chat_enabled,
        premium_requests_used: premium_used,
        premium_requests_limit: premium_limit,
        alert_level,
        reset_at,
        plan,
        access_sku,
        error: None,
        quota_snapshots,
    })
}

fn try_pupkit_token() -> Option<String> {
    let home = dirs::home_dir()?;
    let path = home.join(".local/share/pupkit/github_token");
    std::fs::read_to_string(&path).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

fn parse_quota_entry(value: &serde_json::Value) -> Option<QuotaEntry> {
    let entitlement = value.get("entitlement").and_then(|v| v.as_u64()).unwrap_or(0);
    let remaining = value.get("remaining").and_then(|v| v.as_u64()).unwrap_or(0);
    let percent_remaining = value.get("percent_remaining").and_then(|v| v.as_f64()).unwrap_or(0.0);
    let unlimited = value.get("unlimited").and_then(|v| v.as_bool()).unwrap_or(false);
    Some(QuotaEntry { entitlement, remaining, percent_remaining, unlimited })
}

// ---------------------------------------------------------------------------
// GET /api/copilot/sessions — count local Copilot session requests
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct CopilotSessions {
    pub total_requests: u64,
    pub requests_24h: u64,
    pub session_count: u32,
}

pub async fn get_copilot_sessions() -> impl IntoResponse {
    let result = tokio::task::spawn_blocking(count_copilot_sessions)
        .await
        .unwrap_or(CopilotSessions {
            total_requests: 0,
            requests_24h: 0,
            session_count: 0,
        });
    Json(result)
}

fn count_copilot_sessions() -> CopilotSessions {
    use std::fs;
    use std::io::{BufRead, BufReader};
    use std::time::{SystemTime, Duration};

    let home = match dirs::home_dir() {
        Some(h) => h,
        None => return CopilotSessions { total_requests: 0, requests_24h: 0, session_count: 0 },
    };
    let session_dir = home.join(".copilot").join("session-state");
    let entries = match fs::read_dir(&session_dir) {
        Ok(e) => e,
        Err(_) => return CopilotSessions { total_requests: 0, requests_24h: 0, session_count: 0 },
    };

    let cutoff_24h = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
        .saturating_sub(86400);

    let mut total_requests: u64 = 0;
    let mut requests_24h: u64 = 0;
    let mut session_count: u32 = 0;

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let events_file = path.join("events.jsonl");
        if !events_file.exists() {
            continue;
        }
        session_count += 1;

        // Check file modification time for 24h filter
        let is_recent = events_file
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() > cutoff_24h)
            .unwrap_or(false);

        let file = match fs::File::open(&events_file) {
            Ok(f) => f,
            Err(_) => continue,
        };
        let reader = BufReader::new(file);
        let mut file_count: u64 = 0;
        for line in reader.lines().flatten() {
            if line.contains("\"assistant.turn_start\"") {
                file_count += 1;
            }
        }
        total_requests += file_count;
        if is_recent {
            requests_24h += file_count;
        }
    }

    CopilotSessions {
        total_requests,
        requests_24h,
        session_count,
    }
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
