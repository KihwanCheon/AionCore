use std::path::Path;
use std::time::Duration;

use reqwest::Client;
use serde::Deserialize;

use super::service::{IntegrationToken, MindNProgressIntegration, normalize_loopback_api_url};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RunnerRelayDescriptor {
    schema_version: u32,
    base_url: String,
    token: String,
    instance_id: String,
    pid: u32,
}

fn is_url_safe_identifier(value: &str, min: usize, max: usize) -> bool {
    (min..=max).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn parse_descriptor(contents: &str) -> Option<MindNProgressIntegration> {
    let descriptor: RunnerRelayDescriptor = serde_json::from_str(contents).ok()?;
    if descriptor.schema_version != 1
        || descriptor.pid == 0
        || !is_url_safe_identifier(&descriptor.instance_id, 16, 100)
        || !descriptor
            .token
            .strip_prefix("mnprl_")
            .is_some_and(|token| is_url_safe_identifier(token, 40, 100))
    {
        return None;
    }
    Some(MindNProgressIntegration {
        api_base_url: normalize_loopback_api_url(&descriptor.base_url).ok()?,
        token: IntegrationToken::Runner(descriptor.token),
    })
}

/// Discover the same user-local relay used by the MnP MCP client. Resolve before
/// sending a selection so a lost response never replays the operation elsewhere.
pub(super) async fn prefer_local_or_runner(
    client: &Client,
    local: MindNProgressIntegration,
    descriptor_file: &Path,
) -> MindNProgressIntegration {
    // Read every time: the sidecar rotates its port and local token on restart.
    let runner = match tokio::fs::read_to_string(descriptor_file).await {
        Ok(contents) => parse_descriptor(&contents),
        Err(_) => None,
    };
    let Some(runner) = runner else { return local };
    let health_url = local.api_base_url.join("api/health").expect("validated base URL");
    let local_available = client
        .get(health_url)
        .timeout(Duration::from_millis(500))
        .send()
        .await
        .is_ok_and(|response| response.status().is_success());
    if local_available { local } else { runner }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use reqwest::{Method, Url};
    use serde_json::{Value, json};
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::external_launch::mindnprogress::error::MindNProgressNavigationError;
    use crate::external_launch::mindnprogress::service::relay_request;

    struct DescriptorFile(PathBuf);

    impl DescriptorFile {
        fn new(value: &Value) -> Self {
            let unique = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
            let path = std::env::temp_dir().join(format!("aioncore-mnp-relay-{}-{unique}.json", std::process::id()));
            std::fs::write(&path, value.to_string()).unwrap();
            Self(path)
        }
    }

    impl Drop for DescriptorFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn descriptor(base_url: &str) -> Value {
        json!({
            "schemaVersion": 1,
            "baseUrl": base_url,
            "token": format!("mnprl_{}", "a".repeat(43)),
            "instanceId": "runner-instance-123456",
            "pid": std::process::id(),
        })
    }

    fn local(base_url: &str) -> MindNProgressIntegration {
        MindNProgressIntegration {
            api_base_url: Url::parse(base_url).unwrap(),
            // A sub-machine does not need a main-machine integration token.
            token: IntegrationToken::File(PathBuf::from("missing-local-integration-token")),
        }
    }

    fn client() -> Client {
        Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap()
    }

    #[test]
    fn descriptors_reject_external_destinations_and_invalid_credentials() {
        for (key, value) in [
            ("schemaVersion", json!(2)),
            ("baseUrl", json!("http://10.78.12.223:4176")),
            ("baseUrl", json!("https://127.0.0.1:4176")),
            ("baseUrl", json!("http://user:secret@127.0.0.1:4176")),
            ("baseUrl", json!("http://127.0.0.1:4176/api")),
            ("baseUrl", json!("http://127.0.0.1:4176/?token=secret")),
            ("baseUrl", json!("http://127.0.0.1:4176/#fragment")),
            ("token", json!("machine-token-must-not-be-used")),
            ("instanceId", json!("short")),
            ("pid", json!(0)),
        ] {
            let mut value_to_parse = descriptor("http://127.0.0.1:12345");
            value_to_parse[key] = value;
            assert!(
                parse_descriptor(&value_to_parse.to_string()).is_none(),
                "accepted invalid {key}"
            );
        }
        assert!(parse_descriptor("not-json").is_none());
    }

    #[tokio::test]
    async fn healthy_local_mnp_takes_precedence_over_a_runner_descriptor() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/health"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;
        let file = DescriptorFile::new(&descriptor("http://127.0.0.1:12345"));
        let resolved = prefer_local_or_runner(&client(), local(&server.uri()), &file.0).await;
        assert_eq!(resolved.api_base_url.as_str().trim_end_matches('/'), server.uri());
        assert!(matches!(resolved.token, IntegrationToken::File(_)));
    }

    #[tokio::test]
    async fn malformed_or_missing_descriptors_preserve_the_local_path_without_a_probe() {
        let server = MockServer::start().await;
        let file = DescriptorFile::new(&json!({"schemaVersion": 2}));
        let resolved = prefer_local_or_runner(&client(), local(&server.uri()), &file.0).await;
        assert!(matches!(resolved.token, IntegrationToken::File(_)));
        std::fs::remove_file(&file.0).unwrap();
        let resolved = prefer_local_or_runner(&client(), local(&server.uri()), &file.0).await;
        assert!(matches!(resolved.token, IntegrationToken::File(_)));
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn unavailable_local_mnp_uses_runner_for_scoped_lookup_and_single_selection() {
        let local_server = MockServer::start().await;
        let relay = MockServer::start().await;
        let value = descriptor(&relay.uri());
        let file = DescriptorFile::new(&value);
        for (verb, suffix) in [("GET", ""), ("POST", "/select")] {
            Mock::given(method(verb))
                .and(path(format!(
                    "/api/integrations/aionui/conversations/remote-chat/mindnprogress{suffix}"
                )))
                .and(header(
                    "authorization",
                    format!("Bearer {}", value["token"].as_str().unwrap()),
                ))
                .and(header("x-mnp-ai-conversation-id", "remote-chat"))
                .and(header("x-mnp-selection-device-address", "10.78.12.223"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
                .expect(1)
                .mount(&relay)
                .await;
            let client = client();
            let resolved = prefer_local_or_runner(&client, local(&local_server.uri()), &file.0).await;
            let result = relay_request::<Value>(
                &client,
                &resolved,
                Method::from_bytes(verb.as_bytes()).unwrap(),
                "remote-chat",
                !suffix.is_empty(),
                Some("10.78.12.223".parse().unwrap()),
            )
            .await
            .unwrap();
            assert_eq!(result["ok"], true);
        }
        assert!(
            local_server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .all(|request| request.url.path() == "/api/health")
        );
        assert!(
            relay
                .received_requests()
                .await
                .unwrap()
                .iter()
                .all(|request| !request.headers.contains_key("x-mnp-ai-editor-id"))
        );
    }

    #[tokio::test]
    async fn runner_restart_refreshes_the_port_and_token_on_the_next_lookup() {
        let local_server = MockServer::start().await;
        let first = MockServer::start().await;
        let second = MockServer::start().await;
        let file = DescriptorFile::new(&descriptor(&first.uri()));
        let client = client();
        let resolved = prefer_local_or_runner(&client, local(&local_server.uri()), &file.0).await;
        assert_eq!(resolved.api_base_url.as_str().trim_end_matches('/'), first.uri());
        let mut next = descriptor(&second.uri());
        next["token"] = json!(format!("mnprl_{}", "b".repeat(43)));
        std::fs::write(&file.0, next.to_string()).unwrap();
        let resolved = prefer_local_or_runner(&client, local(&local_server.uri()), &file.0).await;
        assert_eq!(resolved.api_base_url.as_str().trim_end_matches('/'), second.uri());
        assert!(matches!(resolved.token, IntegrationToken::Runner(token) if token == next["token"].as_str().unwrap()));
    }

    #[tokio::test]
    async fn selection_failure_is_returned_without_replaying_on_another_destination() {
        let relay = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(
                "/api/integrations/aionui/conversations/remote-chat/mindnprogress/select",
            ))
            .respond_with(ResponseTemplate::new(403).set_body_json(json!({
                "code": "MNP_RUNNER_MCP_CONVERSATION_DENIED", "error": "denied"
            })))
            .expect(1)
            .mount(&relay)
            .await;
        let resolved = parse_descriptor(&descriptor(&relay.uri()).to_string()).unwrap();
        let error = relay_request::<Value>(&client(), &resolved, Method::POST, "remote-chat", true, None)
            .await
            .unwrap_err();
        assert!(
            matches!(error, MindNProgressNavigationError::Upstream { status: 403, code: Some(code), .. }
            if code == "MNP_RUNNER_MCP_CONVERSATION_DENIED")
        );
    }
}
