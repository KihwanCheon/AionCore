use std::collections::HashSet;
use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Duration;

use aionui_api_types::{
    ClaimExternalConversationLaunchResponse, CompleteExternalConversationLaunchResponse,
    CreateExternalConversationLaunchResponse, ExternalConversationLaunchCallbackStatus,
    ExternalConversationLaunchPayload, ExternalConversationLaunchRequest,
};
use async_trait::async_trait;
use chrono::{SecondsFormat, TimeZone, Utc};
use dashmap::DashMap;
use reqwest::Client;
use url::Url;

use super::error::ExternalLaunchError;

const LAUNCH_TTL_MS: i64 = 5 * 60 * 1000;
const MAX_ACTIVE_LAUNCHES: usize = 128;
const MAX_PROMPT_BYTES: usize = 256 * 1024;
const MAX_TITLE_CHARS: usize = 120;
const MAX_WORKSPACE_CHARS: usize = 4096;
const MAX_OPTION_CHARS: usize = 512;
const MAX_LIST_ITEMS: usize = 128;
const TOKEN_BYTES: usize = 32;

#[async_trait]
pub trait ExternalLaunchConversationLookup: Send + Sync {
    async fn exists_for_user(&self, user_id: &str, conversation_id: &str) -> Result<bool, ExternalLaunchError>;
}

type NowFn = dyn Fn() -> i64 + Send + Sync;

#[derive(Clone)]
pub struct ExternalLaunchService {
    callback_client: Client,
    conversations: Arc<dyn ExternalLaunchConversationLookup>,
    now: Arc<NowFn>,
    tickets: Arc<DashMap<String, StoredLaunch>>,
}

#[derive(Clone)]
struct StoredLaunch {
    callback_url: Option<Url>,
    expires_at_ms: i64,
    launch: ExternalConversationLaunchPayload,
    state: LaunchState,
}

#[derive(Clone)]
enum LaunchState {
    Pending,
    Claimed {
        user_id: String,
    },
    Completing {
        user_id: String,
        conversation_id: String,
    },
    Completed {
        user_id: String,
        conversation_id: String,
        callback_status: ExternalConversationLaunchCallbackStatus,
    },
}

impl ExternalLaunchService {
    pub fn new(callback_client: Client, conversations: Arc<dyn ExternalLaunchConversationLookup>) -> Self {
        Self::with_clock(callback_client, conversations, Arc::new(aionui_common::now_ms))
    }

    fn with_clock(
        callback_client: Client,
        conversations: Arc<dyn ExternalLaunchConversationLookup>,
        now: Arc<NowFn>,
    ) -> Self {
        Self {
            callback_client,
            conversations,
            now,
            tickets: Arc::new(DashMap::new()),
        }
    }

    pub fn issue(
        &self,
        request: ExternalConversationLaunchRequest,
    ) -> Result<CreateExternalConversationLaunchResponse, ExternalLaunchError> {
        self.cleanup_expired();
        if self.tickets.len() >= MAX_ACTIVE_LAUNCHES {
            return Err(ExternalLaunchError::CapacityExhausted);
        }

        let (launch, callback_url) = normalize_request(request)?;
        let now = (self.now)();
        let expires_at_ms = now.saturating_add(LAUNCH_TTL_MS);
        let launch_id = loop {
            let candidate = generate_launch_id();
            if !self.tickets.contains_key(&candidate) {
                break candidate;
            }
        };

        self.tickets.insert(
            launch_id.clone(),
            StoredLaunch {
                callback_url,
                expires_at_ms,
                launch,
                state: LaunchState::Pending,
            },
        );

        tracing::info!(
            active_launches = self.tickets.len(),
            "external conversation launch ticket issued"
        );
        Ok(CreateExternalConversationLaunchResponse {
            launch_id,
            expires_at: format_timestamp(expires_at_ms),
        })
    }

    pub fn claim(
        &self,
        user_id: &str,
        launch_id: &str,
    ) -> Result<ClaimExternalConversationLaunchResponse, ExternalLaunchError> {
        let mut ticket = self
            .tickets
            .get_mut(launch_id)
            .ok_or(ExternalLaunchError::NotFoundOrExpired)?;
        if ticket.expires_at_ms <= (self.now)() {
            drop(ticket);
            self.tickets.remove(launch_id);
            return Err(ExternalLaunchError::NotFoundOrExpired);
        }

        match &ticket.state {
            LaunchState::Pending => {
                ticket.state = LaunchState::Claimed {
                    user_id: user_id.to_owned(),
                };
                tracing::info!("external conversation launch ticket claimed");
                Ok(ClaimExternalConversationLaunchResponse {
                    launch: ticket.launch.clone(),
                    expires_at: format_timestamp(ticket.expires_at_ms),
                })
            }
            LaunchState::Claimed { user_id: owner }
            | LaunchState::Completing { user_id: owner, .. }
            | LaunchState::Completed { user_id: owner, .. }
                if owner != user_id =>
            {
                Err(ExternalLaunchError::Forbidden)
            }
            _ => Err(ExternalLaunchError::AlreadyClaimed),
        }
    }

    pub async fn complete(
        &self,
        user_id: &str,
        launch_id: &str,
        conversation_id: &str,
    ) -> Result<CompleteExternalConversationLaunchResponse, ExternalLaunchError> {
        let callback_url = {
            let mut ticket = self
                .tickets
                .get_mut(launch_id)
                .ok_or(ExternalLaunchError::NotFoundOrExpired)?;
            if ticket.expires_at_ms <= (self.now)() {
                drop(ticket);
                self.tickets.remove(launch_id);
                return Err(ExternalLaunchError::NotFoundOrExpired);
            }

            match &ticket.state {
                LaunchState::Pending => return Err(ExternalLaunchError::NotClaimed),
                LaunchState::Claimed { user_id: owner } if owner != user_id => {
                    return Err(ExternalLaunchError::Forbidden);
                }
                LaunchState::Claimed { .. } => {
                    ticket.state = LaunchState::Completing {
                        user_id: user_id.to_owned(),
                        conversation_id: conversation_id.to_owned(),
                    };
                    ticket.callback_url.clone()
                }
                LaunchState::Completing { user_id: owner, .. } if owner != user_id => {
                    return Err(ExternalLaunchError::Forbidden);
                }
                LaunchState::Completing { .. } => return Err(ExternalLaunchError::CompletionInProgress),
                LaunchState::Completed { user_id: owner, .. } if owner != user_id => {
                    return Err(ExternalLaunchError::Forbidden);
                }
                LaunchState::Completed {
                    conversation_id: completed_id,
                    ..
                } if completed_id != conversation_id => {
                    return Err(ExternalLaunchError::ConversationMismatch);
                }
                LaunchState::Completed {
                    callback_status: ExternalConversationLaunchCallbackStatus::Pending,
                    ..
                } => {
                    ticket.state = LaunchState::Completing {
                        user_id: user_id.to_owned(),
                        conversation_id: conversation_id.to_owned(),
                    };
                    ticket.callback_url.clone()
                }
                LaunchState::Completed { callback_status, .. } => {
                    return Ok(CompleteExternalConversationLaunchResponse {
                        callback_status: *callback_status,
                    });
                }
            }
        };

        let exists = match self.conversations.exists_for_user(user_id, conversation_id).await {
            Ok(exists) => exists,
            Err(error) => {
                self.restore_claimed_state(launch_id, user_id, conversation_id);
                return Err(error);
            }
        };
        if !exists {
            self.restore_claimed_state(launch_id, user_id, conversation_id);
            return Err(ExternalLaunchError::ConversationNotFound);
        }

        let callback_status = match callback_url {
            None => ExternalConversationLaunchCallbackStatus::NotRequired,
            Some(url) => self.deliver_callback(url, conversation_id).await,
        };

        if let Some(mut ticket) = self.tickets.get_mut(launch_id)
            && matches!(
                &ticket.state,
                LaunchState::Completing {
                    user_id: owner,
                    conversation_id: active_id,
                } if owner == user_id && active_id == conversation_id
            )
        {
            ticket.state = LaunchState::Completed {
                user_id: user_id.to_owned(),
                conversation_id: conversation_id.to_owned(),
                callback_status,
            };
        }

        Ok(CompleteExternalConversationLaunchResponse { callback_status })
    }

    fn restore_claimed_state(&self, launch_id: &str, user_id: &str, conversation_id: &str) {
        if let Some(mut ticket) = self.tickets.get_mut(launch_id)
            && matches!(
                &ticket.state,
                LaunchState::Completing {
                    user_id: owner,
                    conversation_id: active_id,
                } if owner == user_id && active_id == conversation_id
            )
        {
            ticket.state = LaunchState::Claimed {
                user_id: user_id.to_owned(),
            };
        }
    }

    async fn deliver_callback(&self, url: Url, conversation_id: &str) -> ExternalConversationLaunchCallbackStatus {
        let response = self
            .callback_client
            .post(url)
            .json(&serde_json::json!({ "conversationId": conversation_id }))
            .send()
            .await;
        match response {
            Ok(response) if response.status().is_success() => ExternalConversationLaunchCallbackStatus::Delivered,
            _ => {
                tracing::warn!("external conversation launch completion callback is pending retry");
                ExternalConversationLaunchCallbackStatus::Pending
            }
        }
    }

    fn cleanup_expired(&self) {
        let now = (self.now)();
        self.tickets.retain(|_, ticket| ticket.expires_at_ms > now);
    }
}

pub fn build_external_launch_callback_client() -> Client {
    Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(5))
        .build()
        .expect("static external launch callback client configuration must be valid")
}

fn generate_launch_id() -> String {
    let mut bytes = [0u8; TOKEN_BYTES];
    getrandom::getrandom(&mut bytes).expect("OS entropy source unavailable");
    let mut token = String::with_capacity(TOKEN_BYTES * 2);
    for byte in bytes {
        let _ = write!(token, "{byte:02x}");
    }
    token
}

fn format_timestamp(timestamp_ms: i64) -> String {
    Utc.timestamp_millis_opt(timestamp_ms)
        .single()
        .expect("external launch timestamp must be representable")
        .to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn normalize_request(
    request: ExternalConversationLaunchRequest,
) -> Result<(ExternalConversationLaunchPayload, Option<Url>), ExternalLaunchError> {
    let agent_id = normalize_required(request.agent_id, MAX_OPTION_CHARS)?;
    if request.prompt.trim().is_empty() || request.prompt.len() > MAX_PROMPT_BYTES {
        return Err(ExternalLaunchError::InvalidPayload);
    }

    let callback_url = request.completion_url.map(validate_callback_url).transpose()?;
    let title = request.title.and_then(|value| {
        let normalized = value.split_whitespace().collect::<Vec<_>>().join(" ");
        (!normalized.is_empty()).then(|| normalized.chars().take(MAX_TITLE_CHARS).collect())
    });

    Ok((
        ExternalConversationLaunchPayload {
            agent_id,
            title,
            prompt: request.prompt,
            model_id: normalize_optional(request.model_id, MAX_OPTION_CHARS)?,
            provider_id: normalize_optional(request.provider_id, MAX_OPTION_CHARS)?,
            mode: normalize_optional(request.mode, MAX_OPTION_CHARS)?,
            thought_level: normalize_optional(request.thought_level, MAX_OPTION_CHARS)?,
            enabled_skill_ids: normalize_list(request.enabled_skill_ids)?,
            disabled_builtin_skill_ids: normalize_list(request.disabled_builtin_skill_ids)?,
            mcp_ids: normalize_list(request.mcp_ids)?,
            workspace: normalize_optional(request.workspace, MAX_WORKSPACE_CHARS)?,
            auto_send: request.auto_send,
        },
        callback_url,
    ))
}

fn normalize_required(value: String, max_chars: usize) -> Result<String, ExternalLaunchError> {
    let normalized = value.trim();
    if normalized.is_empty() || normalized.chars().count() > max_chars {
        return Err(ExternalLaunchError::InvalidPayload);
    }
    Ok(normalized.to_owned())
}

fn normalize_optional(value: Option<String>, max_chars: usize) -> Result<Option<String>, ExternalLaunchError> {
    value.map(|value| normalize_required(value, max_chars)).transpose()
}

fn normalize_list(values: Option<Vec<String>>) -> Result<Option<Vec<String>>, ExternalLaunchError> {
    let Some(values) = values else {
        return Ok(None);
    };
    if values.len() > MAX_LIST_ITEMS {
        return Err(ExternalLaunchError::InvalidPayload);
    }

    let mut seen = HashSet::new();
    let mut normalized = Vec::with_capacity(values.len());
    for value in values {
        let value = normalize_required(value, MAX_OPTION_CHARS)?;
        if seen.insert(value.clone()) {
            normalized.push(value);
        }
    }
    Ok(Some(normalized))
}

fn validate_callback_url(value: String) -> Result<Url, ExternalLaunchError> {
    let url = Url::parse(&value).map_err(|_| ExternalLaunchError::InvalidCallbackUrl)?;
    let host_allowed = matches!(url.host_str(), Some("127.0.0.1" | "::1"));
    let path_allowed = url.path().starts_with("/api/integrations/aionui/");
    if url.scheme() != "http"
        || !host_allowed
        || !path_allowed
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || url.query().is_some()
    {
        return Err(ExternalLaunchError::InvalidCallbackUrl);
    }
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
    use wiremock::matchers::{body_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    struct FakeConversationLookup {
        exists: AtomicBool,
    }

    struct FailingConversationLookup;

    #[async_trait]
    impl ExternalLaunchConversationLookup for FakeConversationLookup {
        async fn exists_for_user(&self, _user_id: &str, _conversation_id: &str) -> Result<bool, ExternalLaunchError> {
            Ok(self.exists.load(Ordering::SeqCst))
        }
    }

    #[async_trait]
    impl ExternalLaunchConversationLookup for FailingConversationLookup {
        async fn exists_for_user(&self, _user_id: &str, _conversation_id: &str) -> Result<bool, ExternalLaunchError> {
            Err(ExternalLaunchError::Storage)
        }
    }

    fn request(callback_url: Option<String>) -> ExternalConversationLaunchRequest {
        ExternalConversationLaunchRequest {
            agent_id: " codex ".to_owned(),
            completion_url: callback_url,
            title: Some(" Review   card ".to_owned()),
            prompt: "Review this card".to_owned(),
            model_id: Some("gpt-5.6-sol".to_owned()),
            provider_id: None,
            mode: Some("agent".to_owned()),
            thought_level: Some("high".to_owned()),
            enabled_skill_ids: Some(vec!["skill-a".to_owned(), "skill-a".to_owned()]),
            disabled_builtin_skill_ids: None,
            mcp_ids: Some(vec!["mcp-a".to_owned()]),
            workspace: Some("D:/workspace".to_owned()),
            auto_send: true,
        }
    }

    fn service(now: Arc<AtomicI64>, exists: bool) -> ExternalLaunchService {
        ExternalLaunchService::with_clock(
            build_external_launch_callback_client(),
            Arc::new(FakeConversationLookup {
                exists: AtomicBool::new(exists),
            }),
            Arc::new(move || now.load(Ordering::SeqCst)),
        )
    }

    #[test]
    fn issue_normalizes_payload_without_exposing_callback() {
        let service = service(Arc::new(AtomicI64::new(1_000)), true);
        let issued = service
            .issue(request(Some(
                "http://127.0.0.1:4176/api/integrations/aionui/launch/token/conversation".to_owned(),
            )))
            .unwrap();
        let claimed = service.claim("user-a", &issued.launch_id).unwrap();

        assert_eq!(claimed.launch.agent_id, "codex");
        assert_eq!(claimed.launch.title.as_deref(), Some("Review card"));
        assert_eq!(claimed.launch.enabled_skill_ids, Some(vec!["skill-a".to_owned()]));
        assert_eq!(claimed.expires_at, "1970-01-01T00:05:01.000Z");
    }

    #[test]
    fn claim_is_one_time_and_user_scoped() {
        let service = service(Arc::new(AtomicI64::new(1_000)), true);
        let issued = service.issue(request(None)).unwrap();
        service.claim("user-a", &issued.launch_id).unwrap();

        assert_eq!(
            service.claim("user-a", &issued.launch_id).unwrap_err(),
            ExternalLaunchError::AlreadyClaimed
        );
        assert_eq!(
            service.claim("user-b", &issued.launch_id).unwrap_err(),
            ExternalLaunchError::Forbidden
        );
    }

    #[test]
    fn expired_ticket_cannot_be_claimed() {
        let now = Arc::new(AtomicI64::new(1_000));
        let service = service(now.clone(), true);
        let issued = service.issue(request(None)).unwrap();
        now.store(1_000 + LAUNCH_TTL_MS, Ordering::SeqCst);

        assert_eq!(
            service.claim("user-a", &issued.launch_id).unwrap_err(),
            ExternalLaunchError::NotFoundOrExpired
        );
    }

    #[tokio::test]
    async fn complete_is_idempotent_for_the_same_conversation() {
        let service = service(Arc::new(AtomicI64::new(1_000)), true);
        let issued = service.issue(request(None)).unwrap();
        service.claim("user-a", &issued.launch_id).unwrap();

        let first = service.complete("user-a", &issued.launch_id, "conv-1").await.unwrap();
        let second = service.complete("user-a", &issued.launch_id, "conv-1").await.unwrap();

        assert_eq!(
            first.callback_status,
            ExternalConversationLaunchCallbackStatus::NotRequired
        );
        assert_eq!(second, first);
        assert_eq!(
            service
                .complete("user-a", &issued.launch_id, "conv-2")
                .await
                .unwrap_err(),
            ExternalLaunchError::ConversationMismatch
        );
    }

    #[tokio::test]
    async fn missing_conversation_restores_claim_for_retry() {
        let service = service(Arc::new(AtomicI64::new(1_000)), false);
        let issued = service.issue(request(None)).unwrap();
        service.claim("user-a", &issued.launch_id).unwrap();

        assert_eq!(
            service
                .complete("user-a", &issued.launch_id, "missing")
                .await
                .unwrap_err(),
            ExternalLaunchError::ConversationNotFound
        );
        assert_eq!(
            service
                .complete("user-b", &issued.launch_id, "missing")
                .await
                .unwrap_err(),
            ExternalLaunchError::Forbidden
        );
    }

    #[tokio::test]
    async fn lookup_failure_restores_claim_for_retry() {
        let service = ExternalLaunchService::with_clock(
            build_external_launch_callback_client(),
            Arc::new(FailingConversationLookup),
            Arc::new(|| 1_000),
        );
        let issued = service.issue(request(None)).unwrap();
        service.claim("user-a", &issued.launch_id).unwrap();

        for _ in 0..2 {
            assert_eq!(
                service
                    .complete("user-a", &issued.launch_id, "conv-1")
                    .await
                    .unwrap_err(),
                ExternalLaunchError::Storage
            );
        }
    }

    #[test]
    fn callback_rejects_non_loopback_and_redirectable_hosts() {
        let service = service(Arc::new(AtomicI64::new(1_000)), true);
        assert_eq!(
            service
                .issue(request(Some(
                    "https://example.com/api/integrations/aionui/x".to_owned()
                )))
                .unwrap_err(),
            ExternalLaunchError::InvalidCallbackUrl
        );
        assert_eq!(
            service
                .issue(request(Some(
                    "http://localhost:4176/api/integrations/aionui/x".to_owned()
                )))
                .unwrap_err(),
            ExternalLaunchError::InvalidCallbackUrl
        );
    }

    #[tokio::test]
    async fn completion_callback_is_delivered_by_the_server() {
        let callback_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/integrations/aionui/launches/token/conversation"))
            .and(body_json(serde_json::json!({ "conversationId": "conv-1" })))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&callback_server)
            .await;
        let service = service(Arc::new(AtomicI64::new(1_000)), true);
        let issued = service
            .issue(request(Some(format!(
                "{}/api/integrations/aionui/launches/token/conversation",
                callback_server.uri()
            ))))
            .unwrap();
        service.claim("user-a", &issued.launch_id).unwrap();

        let completed = service.complete("user-a", &issued.launch_id, "conv-1").await.unwrap();

        assert_eq!(
            completed.callback_status,
            ExternalConversationLaunchCallbackStatus::Delivered
        );
    }
}
