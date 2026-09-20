use std::collections::HashMap;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use aionui_api_types::{MindNProgressConversationLinkResponse, MindNProgressConversationSelectionResponse};
use aionui_db::{IConversationRepository, IMcpServerRepository};
use reqwest::{Client, Method, Url};
use serde::{Deserialize, de::DeserializeOwned};

use super::error::MindNProgressNavigationError;

const MINDNPROGRESS_MCP_NAME: &str = "MindNProgress";
const DEFAULT_MNP_API_URL: &str = "http://127.0.0.1:4176";
const INTEGRATION_TOKEN_FILENAME: &str = "_integration-token";

#[derive(Debug, Deserialize)]
struct MindNProgressErrorResponse {
    error: Option<String>,
    message: Option<String>,
    code: Option<String>,
}

#[derive(Debug, Deserialize)]
struct StdioTransportConfig {
    args: Vec<String>,
    #[serde(default)]
    env: HashMap<String, String>,
}

struct MindNProgressIntegration {
    api_base_url: Url,
    token_file: PathBuf,
}

impl MindNProgressIntegration {
    fn endpoint(&self, conversation_id: &str, select: bool) -> Result<Url, MindNProgressNavigationError> {
        let mut url = self.api_base_url.clone();
        let mut segments = url
            .path_segments_mut()
            .map_err(|_| MindNProgressNavigationError::NotConfigured)?;
        segments.clear();
        segments.extend([
            "api",
            "integrations",
            "aionui",
            "conversations",
            conversation_id,
            "mindnprogress",
        ]);
        if select {
            segments.push("select");
        }
        drop(segments);
        Ok(url)
    }
}

pub struct MindNProgressNavigationService {
    conversation_repo: Arc<dyn IConversationRepository>,
    mcp_server_repo: Arc<dyn IMcpServerRepository>,
    client: Client,
}

impl MindNProgressNavigationService {
    pub fn new(
        conversation_repo: Arc<dyn IConversationRepository>,
        mcp_server_repo: Arc<dyn IMcpServerRepository>,
        client: Client,
    ) -> Self {
        Self {
            conversation_repo,
            mcp_server_repo,
            client,
        }
    }

    pub async fn lookup(
        &self,
        user_id: &str,
        conversation_id: &str,
        selection_device_address: Option<IpAddr>,
    ) -> Result<MindNProgressConversationLinkResponse, MindNProgressNavigationError> {
        self.ensure_conversation_owner(user_id, conversation_id).await?;
        let integration = self.resolve_integration(user_id).await?;
        relay_request(
            &self.client,
            &integration,
            Method::GET,
            conversation_id,
            false,
            selection_device_address,
        )
        .await
    }

    pub async fn select(
        &self,
        user_id: &str,
        conversation_id: &str,
        selection_device_address: Option<IpAddr>,
    ) -> Result<MindNProgressConversationSelectionResponse, MindNProgressNavigationError> {
        self.ensure_conversation_owner(user_id, conversation_id).await?;
        let integration = self.resolve_integration(user_id).await?;
        relay_request(
            &self.client,
            &integration,
            Method::POST,
            conversation_id,
            true,
            selection_device_address,
        )
        .await
    }

    async fn ensure_conversation_owner(
        &self,
        user_id: &str,
        conversation_id: &str,
    ) -> Result<(), MindNProgressNavigationError> {
        let conversation = self
            .conversation_repo
            .get(user_id, conversation_id)
            .await
            .map_err(|_| MindNProgressNavigationError::Storage)?;
        if conversation.is_none() {
            return Err(MindNProgressNavigationError::ConversationNotFound);
        }
        Ok(())
    }

    async fn resolve_integration(
        &self,
        user_id: &str,
    ) -> Result<MindNProgressIntegration, MindNProgressNavigationError> {
        let server = self
            .mcp_server_repo
            .find_by_name(user_id, MINDNPROGRESS_MCP_NAME)
            .await
            .map_err(|_| MindNProgressNavigationError::Storage)?
            .ok_or(MindNProgressNavigationError::NotConfigured)?;

        if server.transport_type != "stdio" {
            return Err(MindNProgressNavigationError::NotConfigured);
        }
        let transport: StdioTransportConfig =
            serde_json::from_str(&server.transport_config).map_err(|_| MindNProgressNavigationError::NotConfigured)?;
        resolve_integration_from_transport(&transport)
    }
}

async fn relay_request<T: DeserializeOwned>(
    client: &Client,
    integration: &MindNProgressIntegration,
    method: Method,
    conversation_id: &str,
    select: bool,
    selection_device_address: Option<IpAddr>,
) -> Result<T, MindNProgressNavigationError> {
    let token = tokio::fs::read_to_string(&integration.token_file)
        .await
        .map_err(|_| MindNProgressNavigationError::NotConfigured)?;
    let token = token.trim();
    if token.is_empty() {
        return Err(MindNProgressNavigationError::NotConfigured);
    }

    let mut request = client
        .request(method, integration.endpoint(conversation_id, select)?)
        .bearer_auth(token);
    if let Some(address) = selection_device_address {
        request = request.header("x-mnp-selection-device-address", address.to_string());
    }
    let response = request
        .send()
        .await
        .map_err(|_| MindNProgressNavigationError::Unavailable)?;
    let status = response.status();
    if status.is_success() {
        return response
            .json::<T>()
            .await
            .map_err(|_| MindNProgressNavigationError::InvalidResponse);
    }

    let error = response
        .json::<MindNProgressErrorResponse>()
        .await
        .unwrap_or(MindNProgressErrorResponse {
            error: None,
            message: None,
            code: None,
        });
    Err(MindNProgressNavigationError::Upstream {
        status: status.as_u16(),
        code: error.code,
        message: error
            .error
            .or(error.message)
            .unwrap_or_else(|| "MindNProgress request failed.".to_owned()),
    })
}

fn resolve_integration_from_transport(
    transport: &StdioTransportConfig,
) -> Result<MindNProgressIntegration, MindNProgressNavigationError> {
    let entry = transport
        .args
        .iter()
        .map(Path::new)
        .find(|candidate| {
            candidate.is_absolute()
                && candidate.file_name().and_then(|name| name.to_str()) == Some("server.mjs")
                && candidate
                    .parent()
                    .and_then(Path::file_name)
                    .and_then(|name| name.to_str())
                    == Some("mcp")
        })
        .ok_or(MindNProgressNavigationError::NotConfigured)?;
    let project_root = entry
        .parent()
        .and_then(Path::parent)
        .ok_or(MindNProgressNavigationError::NotConfigured)?;

    let token_file_setting = backend_setting(transport, "MNP_TOKEN_FILE");
    let data_dir_setting = backend_setting(transport, "MNP_DATA_DIR");
    let token_file = token_file_setting
        .as_deref()
        .map(PathBuf::from)
        .filter(|path| {
            path.is_absolute() && path.file_name().and_then(|name| name.to_str()) == Some(INTEGRATION_TOKEN_FILENAME)
        })
        .or_else(|| {
            data_dir_setting
                .as_deref()
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .map(|path| path.join(INTEGRATION_TOKEN_FILENAME))
        })
        .unwrap_or_else(|| {
            project_root
                .join("server")
                .join("data")
                .join(INTEGRATION_TOKEN_FILENAME)
        });

    let api_url_setting = backend_setting(transport, "MNP_API_URL");
    let configured_url = api_url_setting.as_deref().unwrap_or(DEFAULT_MNP_API_URL);
    let api_base_url = normalize_loopback_api_url(configured_url)?;
    Ok(MindNProgressIntegration {
        api_base_url,
        token_file,
    })
}

fn backend_setting(transport: &StdioTransportConfig, name: &str) -> Option<String> {
    transport
        .env
        .get(name)
        .cloned()
        .or_else(|| std::env::var(name).ok())
        .filter(|value| !value.trim().is_empty())
}

fn normalize_loopback_api_url(value: &str) -> Result<Url, MindNProgressNavigationError> {
    let configured = Url::parse(value).map_err(|_| MindNProgressNavigationError::NotConfigured)?;
    let loopback_host = matches!(configured.host_str(), Some("127.0.0.1" | "localhost" | "::1"));
    if configured.scheme() != "http"
        || !loopback_host
        || !configured.username().is_empty()
        || configured.password().is_some()
        || configured.query().is_some()
        || configured.fragment().is_some()
        || configured.path() != "/"
    {
        return Err(MindNProgressNavigationError::NotConfigured);
    }
    let port = configured
        .port_or_known_default()
        .ok_or(MindNProgressNavigationError::NotConfigured)?;
    Url::parse(&format!("http://127.0.0.1:{port}/")).map_err(|_| MindNProgressNavigationError::NotConfigured)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::time::{SystemTime, UNIX_EPOCH};

    use aionui_api_types::{ApiResponse, MindNProgressTarget};
    use axum::Json;
    use axum::Router;
    use axum::extract::Request;
    use axum::response::IntoResponse;
    use axum::routing::post;

    use super::*;

    #[test]
    fn loopback_url_is_rebuilt_with_literal_ipv4_loopback() {
        let url = normalize_loopback_api_url("http://localhost:4321").unwrap();
        assert_eq!(url.as_str(), "http://127.0.0.1:4321/");
    }

    #[test]
    fn lan_and_public_urls_are_rejected() {
        assert!(normalize_loopback_api_url("http://192.168.0.10:4176").is_err());
        assert!(normalize_loopback_api_url("https://example.com:4176").is_err());
    }

    #[test]
    fn token_path_and_api_port_come_from_backend_only_configuration() {
        let root = if cfg!(windows) {
            r"C:\MindNProgress"
        } else {
            "/opt/MindNProgress"
        };
        let entry = Path::new(root).join("mcp").join("server.mjs");
        let token_file = Path::new(root).join("private").join(INTEGRATION_TOKEN_FILENAME);
        let transport = StdioTransportConfig {
            args: vec![entry.to_string_lossy().into_owned()],
            env: HashMap::from([
                ("MNP_API_URL".to_owned(), "http://127.0.0.1:4321".to_owned()),
                ("MNP_TOKEN_FILE".to_owned(), token_file.to_string_lossy().into_owned()),
            ]),
        };

        let resolved = resolve_integration_from_transport(&transport).unwrap();
        assert_eq!(resolved.api_base_url.as_str(), "http://127.0.0.1:4321/");
        assert_eq!(resolved.token_file, token_file);
    }

    #[test]
    fn browser_response_shape_never_contains_the_integration_token() {
        let response = MindNProgressConversationLinkResponse {
            conversation_id: "conversation-1".to_owned(),
            exists: true,
            target: Some(MindNProgressTarget {
                map_id: "map-1".to_owned(),
                document_title: "Document".to_owned(),
                card_id: "card-1".to_owned(),
                card_title: "Card".to_owned(),
                archived: false,
            }),
            selection_available: false,
            matching_view_count: 0,
            local_selection_available: false,
            local_view_count: 0,
            message: "linked".to_owned(),
        };
        let json = serde_json::to_string(&ApiResponse::ok(response)).unwrap();
        assert!(!json.contains("integration-token"));
        assert!(!json.contains("Authorization"));
    }

    #[tokio::test]
    async fn relay_sends_the_token_only_to_the_fixed_loopback_endpoint() {
        let observed_authorization = Arc::new(Mutex::new(None::<String>));
        let observed_device_address = Arc::new(Mutex::new(None::<String>));
        let observed_editor_id = Arc::new(Mutex::new(None::<String>));
        let observed_authorization_for_route = observed_authorization.clone();
        let observed_device_address_for_route = observed_device_address.clone();
        let observed_editor_id_for_route = observed_editor_id.clone();
        let app = Router::new().route(
            "/api/integrations/aionui/conversations/{conversation_id}/mindnprogress/select",
            post(move |request: Request| {
                let observed_authorization = observed_authorization_for_route.clone();
                let observed_device_address = observed_device_address_for_route.clone();
                let observed_editor_id = observed_editor_id_for_route.clone();
                async move {
                    *observed_authorization.lock().unwrap() = request
                        .headers()
                        .get("authorization")
                        .and_then(|value| value.to_str().ok())
                        .map(str::to_owned);
                    *observed_device_address.lock().unwrap() = request
                        .headers()
                        .get("x-mnp-selection-device-address")
                        .and_then(|value| value.to_str().ok())
                        .map(str::to_owned);
                    *observed_editor_id.lock().unwrap() = request
                        .headers()
                        .get("x-mnp-ai-editor-id")
                        .and_then(|value| value.to_str().ok())
                        .map(str::to_owned);
                    Json(serde_json::json!({
                        "selected": true,
                        "conversationId": "conversation-1",
                        "target": {
                            "mapId": "map-1",
                            "documentTitle": "Document",
                            "cardId": "card-1",
                            "cardTitle": "Card",
                            "archived": false
                        },
                        "deliveredClientCount": 1,
                        "requestedAt": "2026-09-15T00:00:00.000Z",
                        "message": "selected"
                    }))
                    .into_response()
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let unique = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let token_file = std::env::temp_dir().join(format!("aionui-mnp-token-{}-{unique}", std::process::id()));
        std::fs::write(&token_file, "secret-token\n").unwrap();
        let integration = MindNProgressIntegration {
            api_base_url: Url::parse(&format!("http://127.0.0.1:{port}/")).unwrap(),
            token_file: token_file.clone(),
        };
        let client = Client::builder().no_proxy().build().unwrap();

        let response = relay_request::<MindNProgressConversationSelectionResponse>(
            &client,
            &integration,
            Method::POST,
            "conversation-1",
            true,
            Some("10.77.15.55".parse().unwrap()),
        )
        .await
        .unwrap();

        assert!(response.selected);
        assert_eq!(
            observed_authorization.lock().unwrap().as_deref(),
            Some("Bearer secret-token")
        );
        assert_eq!(observed_device_address.lock().unwrap().as_deref(), Some("10.77.15.55"));
        assert_eq!(observed_editor_id.lock().unwrap().as_deref(), None);
        std::fs::remove_file(token_file).unwrap();
        server.abort();
    }
}
