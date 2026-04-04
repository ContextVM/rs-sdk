//! Conversion boundary between internal JSON-RPC messages and rmcp message types.
//!
//! These helpers intentionally convert via serde JSON to preserve wire-level
//! compatibility and avoid fragile hand-mapping between evolving type systems.

use crate::core::types::JsonRpcMessage;
use crate::util::logger;

/// Convert internal JSON-RPC message into rmcp server RX message.
///
/// Role mapping:
/// - RoleServer RX receives client-originated messages.
pub fn internal_to_rmcp_server_rx(
    msg: &JsonRpcMessage,
) -> Option<rmcp::service::RxJsonRpcMessage<rmcp::RoleServer>> {
    let direction = "internal_to_rmcp_server_rx";
    let target = "contextvm_sdk::rmcp_transport::convert";
    let value = match serde_json::to_value(msg) {
        Ok(value) => value,
        Err(error) => {
            logger::error_with_target(
                target,
                format!(
                    "{direction}: failed to serialize message into intermediate JSON: {error}"
                ),
            );
            return None;
        }
    };

    match serde_json::from_value(value.clone()) {
        Ok(parsed) => Some(parsed),
        Err(error) => {
            logger::error_with_target(
                target,
                format!(
                    "{direction}: failed to parse converted JSON payload: {error}; payload={value:?}"
                ),
            );
            None
        }
    }
}

/// Convert internal JSON-RPC message into rmcp client RX message.
///
/// Role mapping:
/// - RoleClient RX receives server-originated messages.
pub fn internal_to_rmcp_client_rx(
    msg: &JsonRpcMessage,
) -> Option<rmcp::service::RxJsonRpcMessage<rmcp::RoleClient>> {
    let direction = "internal_to_rmcp_client_rx";
    let target = "contextvm_sdk::rmcp_transport::convert";
    let value = match serde_json::to_value(msg) {
        Ok(value) => value,
        Err(error) => {
            logger::error_with_target(
                target,
                format!(
                    "{direction}: failed to serialize message into intermediate JSON: {error}"
                ),
            );
            return None;
        }
    };

    match serde_json::from_value(value.clone()) {
        Ok(parsed) => Some(parsed),
        Err(error) => {
            logger::error_with_target(
                target,
                format!(
                    "{direction}: failed to parse converted JSON payload: {error}; payload={value:?}"
                ),
            );
            None
        }
    }
}

/// Convert rmcp server TX message back into internal JSON-RPC.
pub fn rmcp_server_tx_to_internal(
    msg: rmcp::service::TxJsonRpcMessage<rmcp::RoleServer>,
) -> Option<JsonRpcMessage> {
    let direction = "rmcp_server_tx_to_internal";
    let target = "contextvm_sdk::rmcp_transport::convert";
    let value = match serde_json::to_value(msg) {
        Ok(value) => value,
        Err(error) => {
            logger::error_with_target(
                target,
                format!(
                    "{direction}: failed to serialize message into intermediate JSON: {error}"
                ),
            );
            return None;
        }
    };

    match serde_json::from_value(value.clone()) {
        Ok(parsed) => Some(parsed),
        Err(error) => {
            logger::error_with_target(
                target,
                format!(
                    "{direction}: failed to parse converted JSON payload: {error}; payload={value:?}"
                ),
            );
            None
        }
    }
}

/// Convert rmcp client TX message back into internal JSON-RPC.
pub fn rmcp_client_tx_to_internal(
    msg: rmcp::service::TxJsonRpcMessage<rmcp::RoleClient>,
) -> Option<JsonRpcMessage> {
    let direction = "rmcp_client_tx_to_internal";
    let target = "contextvm_sdk::rmcp_transport::convert";
    let value = match serde_json::to_value(msg) {
        Ok(value) => value,
        Err(error) => {
            logger::error_with_target(
                target,
                format!(
                    "{direction}: failed to serialize message into intermediate JSON: {error}"
                ),
            );
            return None;
        }
    };

    match serde_json::from_value(value.clone()) {
        Ok(parsed) => Some(parsed),
        Err(error) => {
            logger::error_with_target(
                target,
                format!(
                    "{direction}: failed to parse converted JSON payload: {error}; payload={value:?}"
                ),
            );
            None
        }
    }
}

#[cfg(all(test, feature = "rmcp"))]
mod tests {
    use super::*;
    use crate::core::types::{JsonRpcRequest, JsonRpcResponse};

    #[test]
    fn test_internal_request_to_rmcp_server_rx_ping() {
        let internal = JsonRpcMessage::Request(JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: serde_json::json!(1),
            method: "ping".to_string(),
            params: None,
        });

        let rmcp_msg = internal_to_rmcp_server_rx(&internal)
            .expect("expected conversion to rmcp server rx message");
        let value = serde_json::to_value(rmcp_msg).expect("serialize rmcp message to JSON");

        assert_eq!(value.get("method"), Some(&serde_json::json!("ping")));
        assert_eq!(value.get("id"), Some(&serde_json::json!(1)));
    }

    #[test]
    fn test_internal_response_to_rmcp_client_rx_empty_result() {
        let internal = JsonRpcMessage::Response(JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: serde_json::json!(42),
            result: serde_json::json!({}),
        });

        let rmcp_msg = internal_to_rmcp_client_rx(&internal)
            .expect("expected conversion to rmcp client rx message");
        let value = serde_json::to_value(rmcp_msg).expect("serialize rmcp message to JSON");

        assert_eq!(value.get("id"), Some(&serde_json::json!(42)));
        assert_eq!(value.get("result"), Some(&serde_json::json!({})));
    }

    #[test]
    fn test_rmcp_server_tx_to_internal_response() {
        let rmcp_msg = rmcp::model::ServerJsonRpcMessage::response(
            rmcp::model::ServerResult::empty(()),
            rmcp::model::RequestId::Number(7),
        );

        let internal = rmcp_server_tx_to_internal(rmcp_msg)
            .expect("expected conversion from rmcp server tx to internal JSON-RPC");

        match internal {
            JsonRpcMessage::Response(resp) => {
                assert_eq!(resp.id, serde_json::json!(7));
                assert_eq!(resp.result, serde_json::json!({}));
            }
            other => panic!("expected internal response, got {other:?}"),
        }
    }

    #[test]
    fn test_rmcp_client_tx_to_internal_response() {
        let rmcp_msg = rmcp::model::ClientJsonRpcMessage::response(
            rmcp::model::ClientResult::empty(()),
            rmcp::model::RequestId::Number(9),
        );

        let internal = rmcp_client_tx_to_internal(rmcp_msg)
            .expect("expected conversion from rmcp client tx to internal JSON-RPC");

        match internal {
            JsonRpcMessage::Response(resp) => {
                assert_eq!(resp.id, serde_json::json!(9));
                assert_eq!(resp.result, serde_json::json!({}));
            }
            other => panic!("expected internal response, got {other:?}"),
        }
    }

    #[test]
    fn test_server_rx_roundtrip_preserves_wire_shape() {
        let internal = JsonRpcMessage::Request(JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: serde_json::json!("abc"),
            method: "ping".to_string(),
            params: None,
        });

        let rmcp_msg = internal_to_rmcp_server_rx(&internal)
            .expect("expected conversion to rmcp server rx message");
        let value = serde_json::to_value(rmcp_msg).expect("serialize rmcp message to JSON");
        let roundtrip_internal: JsonRpcMessage =
            serde_json::from_value(value).expect("deserialize back to internal JSON-RPC");

        match roundtrip_internal {
            JsonRpcMessage::Request(req) => {
                assert_eq!(req.id, serde_json::json!("abc"));
                assert_eq!(req.method, "ping");
            }
            other => panic!("expected internal request, got {other:?}"),
        }
    }
}
