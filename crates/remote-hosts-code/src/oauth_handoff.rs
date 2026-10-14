//! OAuth step-up signaling only. Never creates grants, tokens, retries or jobs.
use crate::{auth::Principal, tools};
use rmcp::{
    RoleServer, Service,
    model::{
        CallToolResult, ClientNotification, ClientRequest, ContentBlock, CustomResult, ErrorData,
        Meta, ServerInfo, ServerResult,
    },
    service::{NotificationContext, RequestContext},
};
use serde_json::{Value, json};

pub(crate) fn scope_challenge(
    p: &Principal,
    tool: &str,
    public_url: &str,
    request_id: &str,
) -> Option<CallToolResult> {
    tools::scope(tool)?;
    if tools::required_scopes(tool)
        .iter()
        .all(|r| p.scopes.iter().any(|s| s == r))
    {
        return None;
    }
    // Include existing scopes so re-consent does not silently discard working access.
    let mut requested = p.scopes.clone();
    for scope in tools::required_scopes(tool) {
        if !requested.iter().any(|s| s == scope) {
            requested.push(scope.into());
        }
    }
    let challenge = format!(
        "Bearer error=\"insufficient_scope\", error_description=\"Additional permission is required for this tool\", resource_metadata=\"{public_url}/.well-known/oauth-protected-resource\", scope=\"{}\"",
        requested.join(" "),
    );
    let value = json!({"error":{"code":"insufficient_scope",
        "message":"Use the existing secure client connection to approve the required scopes."},
        "required_scopes":tools::required_scopes(tool), "request_id":request_id,
        "automatic_replay":false, "execution_started":false});
    let mut result = CallToolResult::error(vec![ContentBlock::text(value.to_string())]);
    result.structured_content = Some(value);
    result.meta = Some(Meta(
        json!({"mcp/www_authenticate":[challenge]})
            .as_object()?
            .clone(),
    ));
    Some(result)
}

/// rmcp 2.1 retains _meta but drops nonstandard top-level Tool fields.
/// Project its static mirrored securitySchemes immediately before wire serialization,
/// using CustomResult rather than forking the SDK or buffering transport bodies.
pub(crate) struct Wire<S>(pub S);
impl<S: Service<RoleServer>> Service<RoleServer> for Wire<S> {
    async fn handle_request(
        &self,
        request: ClientRequest,
        context: RequestContext<RoleServer>,
    ) -> Result<ServerResult, ErrorData> {
        let result = self.0.handle_request(request, context).await?;
        if let ServerResult::ListToolsResult(list) = result {
            let mut value = serde_json::to_value(list)
                .map_err(|_| ErrorData::internal_error("catalog serialization failed", None))?;
            if let Some(tools) = value.get_mut("tools").and_then(Value::as_array_mut) {
                for tool in tools {
                    if let Some(schemes) = tool.pointer("/_meta/securitySchemes").cloned() {
                        tool["securitySchemes"] = schemes;
                    }
                }
            }
            Ok(ServerResult::CustomResult(CustomResult::new(value)))
        } else {
            Ok(result)
        }
    }

    async fn handle_notification(
        &self,
        notification: ClientNotification,
        context: NotificationContext<RoleServer>,
    ) -> Result<(), ErrorData> {
        self.0.handle_notification(notification, context).await
    }

    fn get_info(&self) -> ServerInfo {
        self.0.get_info()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::{ServerHandler, ServiceExt, model::*};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    struct Catalog;
    impl ServerHandler for Catalog {
        fn get_info(&self) -> ServerInfo {
            let mut info = ServerInfo::default();
            info.capabilities = ServerCapabilities::builder().enable_tools().build();
            info
        }
        async fn list_tools(
            &self,
            _: Option<PaginatedRequestParams>,
            _: RequestContext<RoleServer>,
        ) -> Result<ListToolsResult, ErrorData> {
            Ok(ListToolsResult {
                tools: tools::catalog(),
                ..Default::default()
            })
        }
        async fn call_tool(
            &self,
            _: CallToolRequestParams,
            _: RequestContext<RoleServer>,
        ) -> Result<CallToolResult, ErrorData> {
            Ok(scope_challenge(
                &Principal {
                    owner: "fixture".into(),
                    scopes: vec!["code:read".into()],
                },
                "desktop_worker",
                "https://fixture.example",
                "req_stdio",
            )
            .unwrap())
        }
    }

    #[tokio::test]
    async fn stdio_wire_preserves_top_level_schemes_and_authorization_challenge() {
        let (server_io, client_io) = tokio::io::duplex(128 * 1024);
        let server = tokio::spawn(async move {
            let (reader, writer) = tokio::io::split(server_io);
            Wire(Catalog)
                .serve((reader, writer))
                .await
                .unwrap()
                .waiting()
                .await
                .unwrap();
        });
        let (reader, mut writer) = tokio::io::split(client_io);
        let mut reader = BufReader::new(reader);
        writer.write_all((json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
            "protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"fixture","version":"1"}}}).to_string()+"\n").as_bytes()).await.unwrap();
        let mut line = String::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            reader.read_line(&mut line),
        )
        .await
        .unwrap()
        .unwrap();
        let init: Value = serde_json::from_str(&line).unwrap();
        assert!(init.get("result").is_some(), "{init}");
        writer
            .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
            .await
            .unwrap();
        writer
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\",\"params\":{}}\n")
            .await
            .unwrap();
        line.clear();
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            reader.read_line(&mut line),
        )
        .await
        .unwrap()
        .unwrap();
        let listed: Value = serde_json::from_str(&line).unwrap();
        let tool = listed["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == "desktop_worker")
            .unwrap();
        assert_eq!(tool["securitySchemes"], tool["_meta"]["securitySchemes"]);
        assert_eq!(
            tool["securitySchemes"][0]["scopes"],
            json!(["desktop:observe", "desktop:input"])
        );
        writer.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"tools/call\",\"params\":{\"name\":\"desktop_worker\",\"arguments\":{}}}\n").await.unwrap();
        line.clear();
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            reader.read_line(&mut line),
        )
        .await
        .unwrap()
        .unwrap();
        let called: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(called["result"]["isError"], true);
        assert_eq!(
            called["result"]["_meta"]["mcp/www_authenticate"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        drop(writer);
        drop(reader);
        tokio::time::timeout(std::time::Duration::from_secs(5), server)
            .await
            .unwrap()
            .unwrap();
    }
}
