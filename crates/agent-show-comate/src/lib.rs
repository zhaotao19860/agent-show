use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use agent_show_core::{
    AgentAdapter, AgentKind, AssistantTurn, ConversationLog, CoreError, Interaction, PromptSummary,
    Result, SessionDetail, SessionEvent, SessionMeta, SessionStatus, ToolCall, TurnItem,
    TurnToolCall, TurnUsage, UserMessageKind,
};
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tokio::sync::mpsc;

const ACTIVE_WINDOW_SECS: i64 = 300;

pub struct ComateAdapter {
    store_dir: PathBuf,
    sessions_path: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ComateSessionMetaRaw {
    session_uuid: String,
    title: Option<String>,
    ctime: Option<i64>,
    utime: Option<i64>,
    workspace_directory: Option<String>,
    summary: Option<String>,
    session_state: Option<String>,
}

impl ComateAdapter {
    pub fn new() -> Result<Self> {
        let root = std::env::var("COMATE_STATE_DIR")
            .map(PathBuf::from)
            .ok()
            .or_else(|| dirs::home_dir().map(|h| h.join(".comate-engine")))
            .ok_or_else(|| CoreError::NotFound("comate state dir".into()))?;
        Self::with_root(root)
    }

    pub fn with_root(root: PathBuf) -> Result<Self> {
        let store_dir = root.join("store");
        let sessions_path = store_dir.join("chat_sessions");
        if !sessions_path.is_file() {
            return Err(CoreError::NotFound(format!(
                "comate sessions not found: {}",
                sessions_path.display()
            )));
        }
        Ok(Self {
            store_dir,
            sessions_path,
        })
    }

    fn detail_path(&self, session_id: &str) -> PathBuf {
        self.store_dir.join(format!("chat_session_{session_id}"))
    }
}

#[async_trait]
impl AgentAdapter for ComateAdapter {
    async fn list_sessions(&self) -> Result<Vec<SessionMeta>> {
        let sessions_path = self.sessions_path.clone();
        let store_dir = self.store_dir.clone();
        tokio::task::spawn_blocking(move || list_sessions_from_files(&sessions_path, &store_dir))
            .await
            .map_err(|e| CoreError::Other(e.to_string()))?
    }

    async fn get_detail(&self, session_id: &str) -> Result<SessionDetail> {
        let path = self.detail_path(session_id);
        let id = session_id.to_string();
        tokio::task::spawn_blocking(move || {
            let value = read_json_file(&path).map_err(|_| CoreError::NotFound(id))?;
            Ok(parse_detail(&value))
        })
        .await
        .map_err(|e| CoreError::Other(e.to_string()))?
    }

    async fn get_conversation(&self, session_id: &str) -> Result<Option<ConversationLog>> {
        let path = self.detail_path(session_id);
        let id = session_id.to_string();
        tokio::task::spawn_blocking(move || {
            let value = read_json_file(&path).map_err(|_| CoreError::NotFound(id))?;
            Ok(Some(parse_conversation(&value)))
        })
        .await
        .map_err(|e| CoreError::Other(e.to_string()))?
    }

    async fn watch(&self, tx: mpsc::Sender<SessionEvent>) -> Result<()> {
        let sessions_path = self.sessions_path.clone();
        let store_dir = self.store_dir.clone();
        tokio::spawn(async move {
            let mut last_sessions_mtime: Option<std::time::SystemTime> = None;
            let mut last_detail_mtime: Option<std::time::SystemTime> = None;
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                let sessions_mtime = std::fs::metadata(&sessions_path)
                    .and_then(|m| m.modified())
                    .ok();
                let detail_mtime = newest_detail_mtime(&store_dir);
                if sessions_mtime != last_sessions_mtime || detail_mtime != last_detail_mtime {
                    last_sessions_mtime = sessions_mtime;
                    last_detail_mtime = detail_mtime;
                    if tx.send(SessionEvent::SessionListChanged).await.is_err() {
                        break;
                    }
                }
            }
        });
        Ok(())
    }

    async fn activity_hourly(&self, hours: u32) -> Result<Vec<u64>> {
        let sessions_path = self.sessions_path.clone();
        tokio::task::spawn_blocking(move || activity_from_sessions(&sessions_path, hours, None))
            .await
            .map_err(|e| CoreError::Other(e.to_string()))?
    }

    async fn session_activity_hourly(&self, session_id: &str, hours: u32) -> Result<Vec<u64>> {
        let sessions_path = self.sessions_path.clone();
        let id = session_id.to_string();
        tokio::task::spawn_blocking(move || activity_from_sessions(&sessions_path, hours, Some(&id)))
            .await
            .map_err(|e| CoreError::Other(e.to_string()))?
    }
}

fn list_sessions_from_files(sessions_path: &Path, store_dir: &Path) -> Result<Vec<SessionMeta>> {
    let mut by_id = HashMap::new();
    for item in read_sessions(sessions_path).unwrap_or_default() {
        by_id.insert(item.session_uuid.clone(), session_from_index(item, store_dir));
    }

    if let Ok(entries) = std::fs::read_dir(store_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let Some(session_id) = name.strip_prefix("chat_session_") else {
                continue;
            };
            if session_id.is_empty() || session_id == "s" {
                continue;
            }
            if by_id.contains_key(session_id) {
                continue;
            }
            if let Ok(value) = read_json_file(&entry.path()) {
                if let Some(session) = session_from_detail(session_id, &value, entry.path().as_path()) {
                    by_id.insert(session_id.to_string(), session);
                }
            }
        }
    }

    let mut sessions: Vec<_> = by_id.into_values().collect();
    sessions.sort_by(|a, b| b.last_event_at.cmp(&a.last_event_at));
    Ok(sessions)
}

fn session_from_index(item: ComateSessionMetaRaw, store_dir: &Path) -> SessionMeta {
    let now = Utc::now();
    let started_at = millis_to_utc(item.ctime).unwrap_or(now);
    let last_event_at = millis_to_utc(item.utime).unwrap_or(started_at);
    let status = map_status(item.session_state.as_deref(), last_event_at, now);
    let summary = item
        .summary
        .or(item.title)
        .unwrap_or_default()
        .chars()
        .take(80)
        .collect();
    let cwd = item
        .workspace_directory
        .map(PathBuf::from)
        .unwrap_or_default();
    let detail_path = store_dir.join(format!("chat_session_{}", item.session_uuid));
    let model = read_json_file(&detail_path).ok().and_then(|v| first_model(&v));
    SessionMeta {
        id: item.session_uuid,
        agent: AgentKind::Comate,
        cwd,
        repo: None,
        branch: None,
        summary,
        model,
        status,
        pid: None,
        started_at,
        last_event_at,
    }
}

fn session_from_detail(session_id: &str, value: &Value, path: &Path) -> Option<SessionMeta> {
    if is_empty_placeholder(value) {
        return None;
    }

    let metadata = std::fs::metadata(path).ok();
    let modified_at = metadata
        .and_then(|m| m.modified().ok())
        .map(DateTime::<Utc>::from)
        .unwrap_or_else(Utc::now);
    let started_at = value
        .get("ctime")
        .and_then(value_to_time)
        .unwrap_or(modified_at);
    let last_event_at = value
        .get("utime")
        .and_then(value_to_time)
        .unwrap_or(modified_at);
    let title = value.get("title").and_then(|v| v.as_str()).unwrap_or_default();
    let summary_source = value
        .get("summary")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .map(String::from)
        .or_else(|| first_user_text(value))
        .unwrap_or_else(|| title.to_string());
    let summary = summary_source.chars().take(80).collect();
    let cwd = value
        .get("workspaceDirectory")
        .and_then(|v| v.as_str())
        .map(PathBuf::from)
        .unwrap_or_default();
    Some(SessionMeta {
        id: value
            .get("sessionUuid")
            .and_then(|v| v.as_str())
            .unwrap_or(session_id)
            .to_string(),
        agent: AgentKind::Comate,
        cwd,
        repo: None,
        branch: None,
        summary,
        model: first_model(value),
        status: map_status(
            value.get("sessionState").and_then(|v| v.as_str()),
            last_event_at,
            Utc::now(),
        ),
        pid: None,
        started_at,
        last_event_at,
    })
}

fn read_sessions(path: &Path) -> Result<Vec<ComateSessionMetaRaw>> {
    let raw = std::fs::read_to_string(path).map_err(|e| CoreError::Other(e.to_string()))?;
    serde_json::from_str(&raw).map_err(|e| CoreError::Other(e.to_string()))
}

fn read_json_file(path: &Path) -> std::result::Result<Value, serde_json::Error> {
    let raw = std::fs::read_to_string(path).map_err(serde_json::Error::io)?;
    serde_json::from_str(&raw)
}

fn map_status(state: Option<&str>, last_event_at: DateTime<Utc>, now: DateTime<Utc>) -> SessionStatus {
    match state.unwrap_or_default().to_ascii_lowercase().as_str() {
        "running" | "active" => SessionStatus::Active,
        "completed" | "cancelled" | "failed" => SessionStatus::Closed,
        _ if (now - last_event_at).num_seconds() < ACTIVE_WINDOW_SECS => SessionStatus::Active,
        _ => SessionStatus::Idle,
    }
}

fn millis_to_utc(ms: Option<i64>) -> Option<DateTime<Utc>> {
    let ms = ms?;
    let secs = ms.div_euclid(1000);
    let nsecs = (ms.rem_euclid(1000) * 1_000_000) as u32;
    Utc.timestamp_opt(secs, nsecs).single()
}

fn parse_detail(value: &Value) -> SessionDetail {
    let mut detail = SessionDetail::default();
    let mut seen_skills: Vec<String> = Vec::new();
    let messages = value
        .get("messages")
        .and_then(|m| m.as_array())
        .cloned()
        .unwrap_or_default();

    for message in messages {
        let role = message.get("role").and_then(|r| r.as_str()).unwrap_or("");
        let timestamp = message_time(value, &message);
        match role {
            "user" => {
                detail.user_messages += 1;
                if let Some(text) = message_content_text(&message) {
                    let snippet: String = text.chars().take(120).collect();
                    detail.prompts.push(PromptSummary {
                        id: message_id(&message, detail.prompts.len(), "u"),
                        timestamp: Some(timestamp),
                        snippet,
                        text,
                    });
                }
            }
            "assistant" => {
                detail.assistant_messages += 1;
                detail.turns += 1;
                if let Some(usage) = message.get("tokenUsage") {
                    if let Some(n) = usage.get("contextUsed").and_then(|v| v.as_u64()) {
                        detail.tokens_in = detail.tokens_in.saturating_add(n);
                    }
                }
                collect_elements(&message, timestamp, &mut detail, &mut seen_skills);
            }
            _ => {}
        }
    }
    detail.skills_invoked = seen_skills;
    detail.conversation = Some(parse_conversation(value));
    detail
}

fn parse_conversation(value: &Value) -> ConversationLog {
    let mut log = ConversationLog::default();
    let mut current_interaction: Option<usize> = None;
    let messages = value
        .get("messages")
        .and_then(|m| m.as_array())
        .cloned()
        .unwrap_or_default();

    for message in messages {
        let role = message.get("role").and_then(|r| r.as_str()).unwrap_or("");
        let at = message_time(value, &message);
        match role {
            "user" => {
                let text = message_content_text(&message);
                log.interactions.push(Interaction {
                    interaction_id: message_id(&message, log.interactions.len(), "u"),
                    started_at: at,
                    kind: UserMessageKind::Human,
                    user_message_raw: text,
                    user_message_transformed: None,
                    turns: Vec::new(),
                });
                current_interaction = Some(log.interactions.len() - 1);
                log.version += 1;
            }
            "assistant" => {
                let interaction_idx = match current_interaction {
                    Some(idx) => idx,
                    None => {
                        log.interactions.push(Interaction {
                            interaction_id: format!("synthetic-{}", log.interactions.len()),
                            started_at: at,
                            kind: UserMessageKind::InjectedContext,
                            user_message_raw: None,
                            user_message_transformed: None,
                            turns: Vec::new(),
                        });
                        let idx = log.interactions.len() - 1;
                        current_interaction = Some(idx);
                        idx
                    }
                };
                let mut items = Vec::new();
                let text = assistant_text(&message);
                if !text.is_empty() {
                    items.push(TurnItem::AssistantMessage { at, content: text });
                }
                collect_turn_items(&message, at, &mut items);
                log.interactions[interaction_idx].turns.push(AssistantTurn {
                    turn_id: message_id(&message, log.version as usize, "t"),
                    started_at: at,
                    completed_at: Some(at),
                    items,
                    usage: turn_usage(&message),
                });
                log.version += 1;
            }
            _ => {}
        }
    }
    agent_show_core::recompute_token_summary(&mut log);
    log
}

fn collect_elements(
    message: &Value,
    timestamp: DateTime<Utc>,
    detail: &mut SessionDetail,
    seen_skills: &mut Vec<String>,
) {
    if let Some(elements) = message.get("elements").and_then(|e| e.as_array()) {
        for element in elements {
            walk_element(element, timestamp, detail, seen_skills);
        }
    }
}

fn walk_element(
    element: &Value,
    timestamp: DateTime<Utc>,
    detail: &mut SessionDetail,
    seen_skills: &mut Vec<String>,
) {
    let element_type = element.get("type").and_then(|t| t.as_str()).unwrap_or("");
    if element_type == "TOOL" || element.get("toolName").is_some() {
        let name = tool_name(element);
        *detail.tools_used.entry(name.clone()).or_default() += 1;
        let success = element
            .get("toolState")
            .and_then(|s| s.as_str())
            .map(|s| matches!(s, "executed" | "success" | "completed"));
        let args_summary = element.get("params").map(truncate_json);
        let result_snippet = element.get("result").map(truncate_json);
        detail.tool_calls.push(ToolCall {
            name: name.clone(),
            timestamp,
            args_summary,
            result_snippet,
            success,
        });
        if name == "skill" {
            let skill = extract_skill_name(element).unwrap_or_else(|| "skill".to_string());
            if !seen_skills.contains(&skill) {
                seen_skills.push(skill);
            }
        }
    }
    if let Some(children) = element.get("children").and_then(|c| c.as_array()) {
        for child in children {
            walk_element(child, timestamp, detail, seen_skills);
        }
    }
}

fn collect_turn_items(message: &Value, at: DateTime<Utc>, items: &mut Vec<TurnItem>) {
    if let Some(elements) = message.get("elements").and_then(|e| e.as_array()) {
        for element in elements {
            walk_turn_element(element, at, items);
        }
    }
}

fn walk_turn_element(element: &Value, at: DateTime<Utc>, items: &mut Vec<TurnItem>) {
    let element_type = element.get("type").and_then(|t| t.as_str()).unwrap_or("");
    if element_type == "TOOL" || element.get("toolName").is_some() {
        items.push(TurnItem::Tool(TurnToolCall {
            call_id: element
                .get("id")
                .and_then(|id| id.as_str())
                .unwrap_or_default()
                .to_string(),
            name: tool_name(element),
            at,
            args_summary: element.get("params").map(truncate_json),
            result_snippet: element.get("result").map(truncate_json),
            success: element
                .get("toolState")
                .and_then(|s| s.as_str())
                .map(|s| matches!(s, "executed" | "success" | "completed")),
        }));
    }
    if let Some(children) = element.get("children").and_then(|c| c.as_array()) {
        for child in children {
            walk_turn_element(child, at, items);
        }
    }
}

fn message_time(session: &Value, message: &Value) -> DateTime<Utc> {
    message
        .get("ctime")
        .or_else(|| message.get("timestamp"))
        .and_then(value_to_time)
        .or_else(|| session.get("ctime").and_then(value_to_time))
        .unwrap_or_else(Utc::now)
}

fn value_to_time(value: &Value) -> Option<DateTime<Utc>> {
    if let Some(ms) = value.as_i64() {
        return millis_to_utc(Some(ms));
    }
    value
        .as_str()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&Utc))
}

fn message_id(message: &Value, idx: usize, prefix: &str) -> String {
    message
        .get("id")
        .and_then(|id| id.as_str())
        .map(String::from)
        .unwrap_or_else(|| format!("{prefix}-{idx}"))
}

fn message_content_text(message: &Value) -> Option<String> {
    message
        .get("content")
        .and_then(|c| c.as_str())
        .or_else(|| message.pointer("/payload/rawMessage").and_then(|v| v.as_str()))
        .or_else(|| message.pointer("/payload/query").and_then(|v| v.as_str()))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
}

fn assistant_text(message: &Value) -> String {
    let mut parts = Vec::new();
    if let Some(elements) = message.get("elements").and_then(|e| e.as_array()) {
        for element in elements {
            collect_text(element, &mut parts);
        }
    }
    parts.join("\n")
}

fn collect_text(element: &Value, parts: &mut Vec<String>) {
    if element.get("type").and_then(|t| t.as_str()) == Some("TEXT") {
        if let Some(content) = element.get("content").and_then(|c| c.as_str()) {
            if !content.trim().is_empty() {
                parts.push(content.to_string());
            }
        }
    }
    if let Some(children) = element.get("children").and_then(|c| c.as_array()) {
        for child in children {
            collect_text(child, parts);
        }
    }
}

fn tool_name(element: &Value) -> String {
    element
        .get("toolName")
        .and_then(|n| n.as_str())
        .or_else(|| element.get("name").and_then(|n| n.as_str()))
        .unwrap_or("tool")
        .to_string()
}

fn extract_skill_name(element: &Value) -> Option<String> {
    let params = element.get("params")?;
    params
        .get("name")
        .or_else(|| params.get("skill"))
        .or_else(|| params.get("skillName"))
        .or_else(|| params.get("path"))
        .and_then(|v| v.as_str())
        .map(String::from)
}

fn is_empty_placeholder(value: &Value) -> bool {
    let messages_empty = value
        .get("messages")
        .and_then(|v| v.as_array())
        .map(|messages| messages.is_empty())
        .unwrap_or(true);
    let title_empty = value
        .get("title")
        .and_then(|v| v.as_str())
        .map(|title| title.trim().is_empty())
        .unwrap_or(true);
    let workspace_empty = value
        .get("workspaceDirectory")
        .and_then(|v| v.as_str())
        .map(|workspace| workspace.trim().is_empty())
        .unwrap_or(true);

    messages_empty && title_empty && workspace_empty
}

fn first_model(value: &Value) -> Option<String> {
    let messages = value.get("messages")?.as_array()?;
    for message in messages {
        if let Some(model) = message
            .pointer("/payload/model/displayName")
            .or_else(|| message.pointer("/payload/model/modelId"))
            .and_then(|v| v.as_str())
        {
            return Some(model.to_string());
        }
    }
    None
}

fn first_user_text(value: &Value) -> Option<String> {
    let messages = value.get("messages")?.as_array()?;
    for message in messages {
        if message.get("role").and_then(|v| v.as_str()) == Some("user") {
            if let Some(text) = message_content_text(message) {
                return Some(text);
            }
        }
    }
    None
}

fn turn_usage(message: &Value) -> Option<TurnUsage> {
    let usage = message.get("tokenUsage")?;
    let model = message
        .pointer("/payload/model/displayName")
        .or_else(|| message.pointer("/payload/model/modelId"))
        .and_then(|v| v.as_str())
        .unwrap_or("comate")
        .to_string();
    let mut turn_usage = TurnUsage {
        model,
        input_tokens: usage.get("contextUsed").and_then(|v| v.as_u64()),
        output_tokens: None,
        cache_read_tokens: None,
        cache_write_tokens: None,
        cost_usd: None,
    };
    turn_usage.cost_usd = agent_show_core::pricing::compute_cost(&turn_usage);
    Some(turn_usage)
}

fn truncate_json(value: &Value) -> String {
    let raw = match value {
        Value::String(s) => s.clone(),
        other => serde_json::to_string(other).unwrap_or_default(),
    };
    if raw.chars().count() <= 600 {
        raw
    } else {
        let mut out: String = raw.chars().take(600).collect();
        out.push('…');
        out
    }
}

fn newest_detail_mtime(store_dir: &Path) -> Option<std::time::SystemTime> {
    let mut newest = None;
    let entries = std::fs::read_dir(store_dir).ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with("chat_session_") || name == "chat_sessions" {
            continue;
        }
        if let Ok(modified) = entry.metadata().and_then(|m| m.modified()) {
            if newest.map(|t| modified > t).unwrap_or(true) {
                newest = Some(modified);
            }
        }
    }
    newest
}

fn activity_from_sessions(
    sessions_path: &Path,
    hours: u32,
    session_id: Option<&str>,
) -> Result<Vec<u64>> {
    let hours_usize = hours as usize;
    if hours_usize == 0 {
        return Ok(Vec::new());
    }
    let mut buckets = vec![0u64; hours_usize];
    let now = Utc::now();
    let bucket_start = now - chrono::Duration::hours(hours_usize as i64);
    for item in read_sessions(sessions_path)? {
        if session_id.map(|id| id != item.session_uuid).unwrap_or(false) {
            continue;
        }
        let Some(ts) = millis_to_utc(item.utime.or(item.ctime)) else {
            continue;
        };
        let offset = (ts - bucket_start).num_hours();
        if offset >= 0 && (offset as usize) < hours_usize {
            buckets[offset as usize] += 1;
        }
    }
    Ok(buckets)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_root() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("store");
        std::fs::create_dir(&store).unwrap();
        std::fs::write(
            store.join("chat_sessions"),
            r#"[{"sessionUuid":"s1","title":"Test","ctime":1700000000000,"utime":1700000005000,"workspaceDirectory":"/tmp/p","summary":"Hello","sessionState":"completed"}]"#,
        )
        .unwrap();
        std::fs::write(
            store.join("chat_session_s1"),
            r#"{"sessionUuid":"s1","ctime":1700000000000,"messages":[{"id":"u1","role":"user","status":"success","content":"Hi","payload":{"model":{"displayName":"GPT-5.5"}}},{"id":"a1","role":"assistant","status":"success","tokenUsage":{"contextUsed":123},"elements":[{"children":[{"type":"TEXT","content":"Hello"},{"type":"TOOL","toolName":"skill","toolState":"executed","params":{"name":"demo"},"result":{"ok":true}}]}]}]}"#,
        )
        .unwrap();
        std::fs::write(
            store.join("chat_session_s2"),
            r#"{"sessionUuid":"s2","title":"Only detail","ctime":1700000010000,"utime":1700000020000,"workspaceDirectory":"/tmp/p","messages":[{"id":"u2","role":"user","content":"From detail"}]}"#,
        )
        .unwrap();
        std::fs::write(
            store.join("chat_session_empty"),
            r#"{"messages":[],"title":"","workspaceDirectory":"","sessionUuid":"empty"}"#,
        )
        .unwrap();
        dir
    }

    #[tokio::test]
    async fn list_sessions_reads_comate_store() {
        let dir = fixture_root();
        let adapter = ComateAdapter::with_root(dir.path().to_path_buf()).unwrap();
        let sessions = adapter.list_sessions().await.unwrap();
        assert_eq!(sessions.len(), 2);
        let indexed = sessions.iter().find(|s| s.id == "s1").unwrap();
        assert_eq!(indexed.agent, AgentKind::Comate);
        assert_eq!(indexed.summary, "Hello");
        assert_eq!(indexed.model.as_deref(), Some("GPT-5.5"));
        let detail_only = sessions.iter().find(|s| s.id == "s2").unwrap();
        assert_eq!(detail_only.summary, "From detail");
        assert!(sessions.iter().all(|s| s.id != "empty"));
    }

    #[tokio::test]
    async fn detail_counts_messages_tools_and_skills() {
        let dir = fixture_root();
        let adapter = ComateAdapter::with_root(dir.path().to_path_buf()).unwrap();
        let detail = adapter.get_detail("s1").await.unwrap();
        assert_eq!(detail.user_messages, 1);
        assert_eq!(detail.assistant_messages, 1);
        assert_eq!(detail.tools_used.get("skill"), Some(&1));
        assert_eq!(detail.skills_invoked, vec!["demo"]);
        assert_eq!(detail.tokens_in, 123);
        assert!(detail.conversation.is_some());
    }
}
