#![allow(clippy::disallowed_types)]

use std::net::{IpAddr, SocketAddr};

use aionui_api_types::{
    ApiResponse, MindNProgressConversationLinkResponse, MindNProgressConversationSelectionResponse,
};
use aionui_auth::CurrentUser;
use aionui_common::ApiError;
use axum::extract::{ConnectInfo, Extension, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};

use super::error::MindNProgressNavigationError;
use super::state::MindNProgressNavigationRouterState;

const TRUSTED_CLIENT_ADDRESS_HEADER: &str = "x-aionui-client-address";

pub fn mindnprogress_navigation_routes(state: MindNProgressNavigationRouterState) -> Router {
    Router::new()
        .route(
            "/api/integrations/mindnprogress/conversations/{conversation_id}",
            get(get_link),
        )
        .route(
            "/api/integrations/mindnprogress/conversations/{conversation_id}/select",
            post(select_link),
        )
        .with_state(state)
}

async fn get_link(
    State(state): State<MindNProgressNavigationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(conversation_id): Path<String>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
) -> Result<Json<ApiResponse<MindNProgressConversationLinkResponse>>, ApiError> {
    let selection_device_address = selection_device_address(&headers, Some(peer))?;
    let response = state
        .service
        .lookup(&user.id, &conversation_id, selection_device_address)
        .await
        .map_err(map_navigation_error)?;
    Ok(Json(ApiResponse::ok(response)))
}

async fn select_link(
    State(state): State<MindNProgressNavigationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(conversation_id): Path<String>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
) -> Result<Json<ApiResponse<MindNProgressConversationSelectionResponse>>, ApiError> {
    let selection_device_address = selection_device_address(&headers, Some(peer))?;
    let response = state
        .service
        .select(&user.id, &conversation_id, selection_device_address)
        .await
        .map_err(map_navigation_error)?;
    Ok(Json(ApiResponse::ok(response)))
}

fn normalize_ip(address: IpAddr) -> IpAddr {
    match address {
        IpAddr::V6(address) => address.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(IpAddr::V6(address)),
        address => address,
    }
}

fn selection_device_address(headers: &HeaderMap, peer: Option<SocketAddr>) -> Result<Option<IpAddr>, ApiError> {
    let Some(peer) = peer else {
        return Ok(None);
    };
    let peer_address = normalize_ip(peer.ip());
    if !peer_address.is_loopback() {
        return Ok(Some(peer_address));
    }

    let Some(forwarded_address) = headers.get(TRUSTED_CLIENT_ADDRESS_HEADER) else {
        return Ok(None);
    };
    let forwarded_address = forwarded_address.to_str().map_err(|_| invalid_device_address())?;
    if forwarded_address.contains(',') || forwarded_address.trim() != forwarded_address {
        return Err(invalid_device_address());
    }
    let address = normalize_ip(forwarded_address.parse().map_err(|_| invalid_device_address())?);
    Ok((!address.is_loopback()).then_some(address))
}

fn invalid_device_address() -> ApiError {
    ApiError::coded(
        StatusCode::BAD_REQUEST,
        "MNP_SELECTION_DEVICE_ADDRESS_INVALID",
        "The AionUi client device address is invalid.",
        None,
    )
}

fn map_navigation_error(error: MindNProgressNavigationError) -> ApiError {
    match error {
        MindNProgressNavigationError::ConversationNotFound => ApiError::coded(
            StatusCode::NOT_FOUND,
            "MNP_AIONUI_CONVERSATION_NOT_FOUND",
            "Conversation was not found for the current user.",
            None,
        ),
        MindNProgressNavigationError::Storage => ApiError::coded(
            StatusCode::INTERNAL_SERVER_ERROR,
            "MNP_INTEGRATION_STORAGE_FAILED",
            "MindNProgress integration storage lookup failed.",
            None,
        ),
        MindNProgressNavigationError::NotConfigured => ApiError::coded(
            StatusCode::BAD_GATEWAY,
            "MNP_INTEGRATION_NOT_CONFIGURED",
            "MindNProgress integration is not configured on this device.",
            None,
        ),
        MindNProgressNavigationError::Unavailable => ApiError::coded(
            StatusCode::BAD_GATEWAY,
            "MNP_INTEGRATION_UNAVAILABLE",
            "MindNProgress is not available on this device.",
            None,
        ),
        MindNProgressNavigationError::InvalidResponse => ApiError::coded(
            StatusCode::BAD_GATEWAY,
            "MNP_INTEGRATION_RESPONSE_INVALID",
            "MindNProgress returned an invalid response.",
            None,
        ),
        MindNProgressNavigationError::Upstream { status, code, message } => {
            map_upstream_error(status, code.as_deref(), message)
        }
    }
}

fn map_upstream_error(status: u16, code: Option<&str>, message: String) -> ApiError {
    match (status, code) {
        (400, Some("MNP_AI_CONVERSATION_ID_INVALID")) => {
            ApiError::coded(StatusCode::BAD_REQUEST, "MNP_AI_CONVERSATION_ID_INVALID", message, None)
        }
        (400, Some("MNP_SELECTION_ACCOUNT_REQUIRED")) => {
            ApiError::coded(StatusCode::BAD_REQUEST, "MNP_SELECTION_ACCOUNT_REQUIRED", message, None)
        }
        (400, Some("MNP_SELECTION_DEVICE_ADDRESS_INVALID")) => ApiError::coded(
            StatusCode::BAD_REQUEST,
            "MNP_SELECTION_DEVICE_ADDRESS_INVALID",
            message,
            None,
        ),
        (400, Some("MNP_SELECTION_DEVICE_REQUIRED")) => {
            ApiError::coded(StatusCode::BAD_REQUEST, "MNP_SELECTION_DEVICE_REQUIRED", message, None)
        }
        (403, Some("MNP_LOCAL_SELECTION_REQUIRED")) => {
            ApiError::coded(StatusCode::FORBIDDEN, "MNP_LOCAL_SELECTION_REQUIRED", message, None)
        }
        (403, Some("MNP_SELECTION_ACCOUNT_UNAVAILABLE")) => ApiError::coded(
            StatusCode::FORBIDDEN,
            "MNP_SELECTION_ACCOUNT_UNAVAILABLE",
            message,
            None,
        ),
        (403, Some("MNP_SELECTION_ACCOUNT_MISMATCH")) => {
            ApiError::coded(StatusCode::FORBIDDEN, "MNP_SELECTION_ACCOUNT_MISMATCH", message, None)
        }
        (404, Some("MNP_AI_CONVERSATION_NOT_FOUND")) => {
            ApiError::coded(StatusCode::NOT_FOUND, "MNP_AI_CONVERSATION_NOT_FOUND", message, None)
        }
        (409, Some("MNP_LOCAL_VIEW_NOT_CONNECTED")) => {
            ApiError::coded(StatusCode::CONFLICT, "MNP_LOCAL_VIEW_NOT_CONNECTED", message, None)
        }
        (409, Some("MNP_MATCHING_VIEW_NOT_CONNECTED")) => {
            ApiError::coded(StatusCode::CONFLICT, "MNP_MATCHING_VIEW_NOT_CONNECTED", message, None)
        }
        (401, _) => ApiError::coded(StatusCode::BAD_GATEWAY, "MNP_INTEGRATION_AUTH_FAILED", message, None),
        _ => ApiError::coded(StatusCode::BAD_GATEWAY, "MNP_INTEGRATION_UPSTREAM_ERROR", message, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upstream_selection_errors_keep_the_mnp_code_and_message() {
        let cases = [
            (400, "MNP_SELECTION_ACCOUNT_REQUIRED"),
            (400, "MNP_SELECTION_DEVICE_ADDRESS_INVALID"),
            (400, "MNP_SELECTION_DEVICE_REQUIRED"),
            (403, "MNP_LOCAL_SELECTION_REQUIRED"),
            (403, "MNP_SELECTION_ACCOUNT_UNAVAILABLE"),
            (403, "MNP_SELECTION_ACCOUNT_MISMATCH"),
            (404, "MNP_AI_CONVERSATION_NOT_FOUND"),
            (409, "MNP_LOCAL_VIEW_NOT_CONNECTED"),
            (409, "MNP_MATCHING_VIEW_NOT_CONNECTED"),
        ];
        for (status, code) in cases {
            let error = map_upstream_error(status, Some(code), "server message".to_owned());
            assert_eq!(error.status_code().as_u16(), status);
            assert_eq!(error.error_code(), code);
            assert_eq!(error.public_message(), "server message");
        }
    }

    #[test]
    fn direct_remote_peer_ignores_spoofed_internal_address_header() {
        let mut headers = HeaderMap::new();
        headers.insert(TRUSTED_CLIENT_ADDRESS_HEADER, "203.0.113.10".parse().unwrap());
        let peer = SocketAddr::from(([10, 77, 15, 55], 43210));

        assert_eq!(selection_device_address(&headers, Some(peer)).unwrap(), Some(peer.ip()));
    }

    #[test]
    fn loopback_web_host_supplies_the_observed_remote_device_address() {
        let mut headers = HeaderMap::new();
        headers.insert(TRUSTED_CLIENT_ADDRESS_HEADER, "::ffff:10.77.15.55".parse().unwrap());
        let peer = SocketAddr::from(([127, 0, 0, 1], 43210));

        assert_eq!(
            selection_device_address(&headers, Some(peer)).unwrap(),
            Some("10.77.15.55".parse().unwrap())
        );
    }

    #[test]
    fn local_browser_address_is_omitted() {
        let mut headers = HeaderMap::new();
        headers.insert(TRUSTED_CLIENT_ADDRESS_HEADER, "127.0.0.1".parse().unwrap());
        let peer = SocketAddr::from(([127, 0, 0, 1], 43210));

        assert_eq!(selection_device_address(&headers, Some(peer)).unwrap(), None);
    }

    #[test]
    fn loopback_web_host_rejects_forwarding_chains() {
        let mut headers = HeaderMap::new();
        headers.insert(TRUSTED_CLIENT_ADDRESS_HEADER, "10.77.15.55, 127.0.0.1".parse().unwrap());
        let peer = SocketAddr::from(([127, 0, 0, 1], 43210));

        let error = selection_device_address(&headers, Some(peer)).unwrap_err();
        assert_eq!(error.status_code(), StatusCode::BAD_REQUEST);
        assert_eq!(error.error_code(), "MNP_SELECTION_DEVICE_ADDRESS_INVALID");
    }
}
