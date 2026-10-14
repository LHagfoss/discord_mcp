use std::sync::Arc;

use anyhow::Result;
use rmcp::{
    ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, Implementation, ServerCapabilities, ServerConfig},
    schemars::JsonSchema,
    tool, tool_handler, tool_router,
    transport::stdio,
};
use serde::{Deserialize, Serialize};

use crate::browser;

/// Env var that exposes developer diagnostics such as `inspect_discord_page`.
const DEBUG_TOOLS_ENV: &str = "DISCORD_MCP_DEBUG_TOOLS";
const DEBUG_TOOLS: &[&str] = &["inspect_discord_page"];

const INSTRUCTIONS: &str = "\
Discord via the user's logged-in web UI (persistent local browser profile). \
Servers, channels, DMs and friends are identified by exact visible UI labels (or a stable channel_id when \
returned); list them first and reuse values verbatim. List tools and get_messages page with offset/limit.\n\
Writes (send_message, reply_message, delete_message) act as the user on their real account:\n\
- Call without confirm first for a preview (no browser opened); set confirm=true only when the user clearly \
asked for that exact action.\n\
- Existing composer drafts are preserved unless clear_draft=true; only set it when the user asked.\n\
- For replies/deletes prefer message_id from get_messages; message_text is a fallback matcher.\n\
- delete_message is irreversible.\n\
Errors are returned as tool results with isError=true; report them instead of retrying blindly.";

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct DiscordServer {
    tool_router: ToolRouter<Self>,
    _marker: Arc<()>,
}

impl Default for DiscordServer {
    fn default() -> Self {
        Self::with_debug_tools(debug_tools_enabled())
    }
}

fn debug_tools_enabled() -> bool {
    std::env::var(DEBUG_TOOLS_ENV)
        .map(|value| matches!(value.trim(), "1" | "true" | "yes"))
        .unwrap_or(false)
}

impl DiscordServer {
    fn with_debug_tools(debug_tools: bool) -> Self {
        let mut tool_router = Self::tool_router();
        if !debug_tools {
            for name in DEBUG_TOOLS {
                tool_router.remove_route(name);
            }
        }
        Self {
            tool_router,
            _marker: Arc::new(()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ChannelRequest {
    /// Optional exact visible server label. Omit for the current server/channel view.
    #[serde(default)]
    pub server_name: Option<String>,
    /// Number of entries to skip. Pagination is over currently visible UI rows.
    #[serde(default)]
    pub offset: Option<u32>,
    /// Number of entries to return. Defaults to 20 and is capped at 200.
    #[serde(default)]
    pub limit: Option<u32>,
    /// Optional case-insensitive substring filter applied to visible labels.
    #[serde(default)]
    pub query: Option<String>,
    /// Optional ordering: `ui` (default), `label_asc`, or `label_desc`.
    #[serde(default)]
    pub sort: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PaginationRequest {
    /// Number of entries to skip. Pagination is over currently visible UI rows.
    #[serde(default)]
    pub offset: Option<u32>,
    /// Number of entries to return. Defaults to 20 and is capped at 200.
    #[serde(default)]
    pub limit: Option<u32>,
    /// Optional case-insensitive substring filter applied to visible labels.
    #[serde(default)]
    pub query: Option<String>,
    /// Optional ordering: `ui` (default), `label_asc`, or `label_desc`.
    #[serde(default)]
    pub sort: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct MessagesRequest {
    /// Exact visible channel, DM, or group DM label.
    pub channel_name: String,
    /// Stable visible DOM ID returned by list_dms or list_channels. Preferred when available.
    #[serde(default)]
    pub channel_id: Option<String>,
    /// Optional exact visible server label when channel_name is a server channel.
    #[serde(default)]
    pub server_name: Option<String>,
    /// Number of messages to skip from the newest visible messages.
    #[serde(default)]
    pub offset: Option<u32>,
    /// Number of visible messages to return. Defaults to 20 and is capped at 100.
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SendMessageRequest {
    /// Exact visible channel, DM, or group DM label.
    pub channel_name: String,
    /// Stable visible DOM ID returned by list_dms or list_channels. Preferred when available.
    #[serde(default)]
    pub channel_id: Option<String>,
    /// Optional exact visible server label when channel_name is a server channel.
    #[serde(default)]
    pub server_name: Option<String>,
    /// Message text. Discord's visible composer limit is 2,000 characters.
    pub message: String,
    /// Required to perform the external send. Omit or set false for a preview.
    #[serde(default)]
    pub confirm: bool,
    /// When true, clear an existing composer draft before typing and sending.
    #[serde(default)]
    pub clear_draft: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ReplyMessageRequest {
    /// Exact visible channel, DM, or group DM label.
    pub channel_name: String,
    /// Stable visible DOM ID returned by list_dms or list_channels. Preferred when available.
    #[serde(default)]
    pub channel_id: Option<String>,
    /// Optional exact visible server label when channel_name is a server channel.
    #[serde(default)]
    pub server_name: Option<String>,
    /// Message ID returned by get_messages when Discord exposes one.
    #[serde(default)]
    pub message_id: Option<String>,
    /// Exact or distinctive visible message text, used when no message ID is available.
    #[serde(default)]
    pub message_text: Option<String>,
    /// Reply text. Discord's visible composer limit is 2,000 characters.
    pub message: String,
    /// Required to perform the external send. Omit or set false for a preview.
    #[serde(default)]
    pub confirm: bool,
    /// When true, clear an existing composer draft before typing and sending the reply.
    #[serde(default)]
    pub clear_draft: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DeleteMessageRequest {
    /// Exact visible channel, DM, or group DM label.
    pub channel_name: String,
    /// Stable visible DOM ID returned by list_dms or list_channels. Preferred when available.
    #[serde(default)]
    pub channel_id: Option<String>,
    /// Optional exact visible server label when channel_name is a server channel.
    #[serde(default)]
    pub server_name: Option<String>,
    /// Message ID returned by get_messages when Discord exposes one.
    #[serde(default)]
    pub message_id: Option<String>,
    /// Exact or distinctive visible message text, used when no message ID is available.
    #[serde(default)]
    pub message_text: Option<String>,
    /// Required to perform the destructive delete. Omit or set false for a preview.
    #[serde(default)]
    pub confirm: bool,
}

fn json_success<T: Serialize>(value: &T) -> CallToolResult {
    match serde_json::to_string_pretty(value) {
        Ok(text) => CallToolResult::success(vec![rmcp::model::ContentBlock::text(text)]),
        Err(error) => json_error(format!("could not serialize tool result: {error}")),
    }
}

fn json_error(error: impl std::fmt::Display) -> CallToolResult {
    CallToolResult::error(vec![rmcp::model::ContentBlock::text(error.to_string())])
}

#[tool_router]
impl DiscordServer {
    /// Return a bounded snapshot of the authenticated Discord page.
    #[tool(
        name = "get_discord_overview",
        description = "Read the current visible Discord page. Read-only; visible text is bounded and no credentials are returned.",
        annotations(read_only_hint = true)
    )]
    async fn get_discord_overview(&self) -> CallToolResult {
        match browser::visible_snapshot().await {
            Ok(value) => json_success(&value),
            Err(error) => json_error(format!("Could not read Discord overview: {error:#}")),
        }
    }

    /// List visible Discord servers.
    #[tool(
        name = "list_servers",
        description = "List visible Discord server labels with offset/limit pagination, optional case-insensitive query filtering, and label_asc/label_desc sorting. Results are UI labels, not API IDs. Read-only.",
        annotations(read_only_hint = true)
    )]
    async fn list_servers(
        &self,
        Parameters(request): Parameters<PaginationRequest>,
    ) -> CallToolResult {
        match browser::list_servers(
            request.offset.unwrap_or(0),
            request.limit.unwrap_or(20),
            request.query.as_deref(),
            request.sort.as_deref(),
        )
        .await
        {
            Ok(value) => json_success(&value),
            Err(error) => json_error(format!("Could not list Discord servers: {error:#}")),
        }
    }

    /// List visible Discord friends.
    #[tool(
        name = "list_friends",
        description = "Open the visible Discord Friends view and list friend labels with offset/limit pagination, optional case-insensitive query filtering, and label_asc/label_desc sorting. Results are UI labels, not API IDs. Read-only.",
        annotations(read_only_hint = true)
    )]
    async fn list_friends(
        &self,
        Parameters(request): Parameters<PaginationRequest>,
    ) -> CallToolResult {
        match browser::list_friends(
            request.offset.unwrap_or(0),
            request.limit.unwrap_or(20),
            request.query.as_deref(),
            request.sort.as_deref(),
        )
        .await
        {
            Ok(value) => json_success(&value),
            Err(error) => json_error(format!("Could not list Discord friends: {error:#}")),
        }
    }

    /// List visible text and voice channels under a server.
    #[tool(
        name = "list_channels",
        description = "List visible Discord text and voice channels with offset/limit pagination, optional query filtering and label sorting, optionally after selecting an exact visible server label. Read-only; labels are UI references, not invented API IDs.",
        annotations(read_only_hint = true)
    )]
    async fn list_channels(
        &self,
        Parameters(request): Parameters<ChannelRequest>,
    ) -> CallToolResult {
        match browser::list_channels(
            request.server_name.as_deref(),
            request.offset.unwrap_or(0),
            request.limit.unwrap_or(20),
            request.query.as_deref(),
            request.sort.as_deref(),
        )
        .await
        {
            Ok(value) => json_success(&value),
            Err(error) => json_error(format!("Could not list Discord channels: {error:#}")),
        }
    }

    /// List visible direct and group messages.
    #[tool(
        name = "list_dms",
        description = "List visible Discord direct-message and group-message labels with offset/limit pagination, optional case-insensitive query filtering, and label_asc/label_desc sorting. Read-only.",
        annotations(read_only_hint = true)
    )]
    async fn list_dms(&self, Parameters(request): Parameters<PaginationRequest>) -> CallToolResult {
        match browser::list_dms(
            request.offset.unwrap_or(0),
            request.limit.unwrap_or(20),
            request.query.as_deref(),
            request.sort.as_deref(),
        )
        .await
        {
            Ok(value) => json_success(&value),
            Err(error) => json_error(format!("Could not list Discord DMs: {error:#}")),
        }
    }

    /// Read messages currently visible in a channel or DM.
    #[tool(
        name = "get_messages",
        description = "Open a visible Discord channel, DM, or friend by label or stable channel_id and read visible messages with offset/limit pagination. This uses the rendered UI only and returns message IDs when Discord exposes them.",
        annotations(read_only_hint = true)
    )]
    async fn get_messages(
        &self,
        Parameters(request): Parameters<MessagesRequest>,
    ) -> CallToolResult {
        let limit = request.limit.unwrap_or(20).clamp(1, 100);
        match browser::visible_messages(
            &request.channel_name,
            request.channel_id.as_deref(),
            request.server_name.as_deref(),
            request.offset.unwrap_or(0),
            limit,
        )
        .await
        {
            Ok(value) => json_success(&value),
            Err(error) => json_error(format!("Could not read Discord messages: {error:#}")),
        }
    }

    /// Preview or send a message.
    #[tool(
        name = "send_message",
        description = "Send (or preview, if confirm is false) one message to a Discord channel or DM by visible label or channel_id. Writes as the user.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false
        )
    )]
    async fn send_message(
        &self,
        Parameters(request): Parameters<SendMessageRequest>,
    ) -> CallToolResult {
        match browser::send_message(
            &request.channel_name,
            request.channel_id.as_deref(),
            request.server_name.as_deref(),
            &request.message,
            request.confirm,
            request.clear_draft,
        )
        .await
        {
            Ok(value) => json_success(&value),
            Err(error) => json_error(format!("Could not send Discord message: {error:#}")),
        }
    }

    /// Preview or send a reply to a visible message.
    #[tool(
        name = "reply_message",
        description = "Reply (or preview, if confirm is false) to a visible Discord message, by message_id (preferred) or message_text. Writes as the user.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false
        )
    )]
    async fn reply_message(
        &self,
        Parameters(request): Parameters<ReplyMessageRequest>,
    ) -> CallToolResult {
        match browser::reply_message(
            &request.channel_name,
            request.channel_id.as_deref(),
            request.server_name.as_deref(),
            request.message_id.as_deref(),
            request.message_text.as_deref(),
            &request.message,
            request.confirm,
            request.clear_draft,
        )
        .await
        {
            Ok(value) => json_success(&value),
            Err(error) => json_error(format!("Could not reply in Discord: {error:#}")),
        }
    }

    /// Preview or delete a visible message.
    #[tool(
        name = "delete_message",
        description = "Delete (or preview, if confirm is false) one of the user's visible Discord messages, by message_id (preferred) or message_text. Irreversible.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false
        )
    )]
    async fn delete_message(
        &self,
        Parameters(request): Parameters<DeleteMessageRequest>,
    ) -> CallToolResult {
        match browser::delete_message(
            &request.channel_name,
            request.channel_id.as_deref(),
            request.server_name.as_deref(),
            request.message_id.as_deref(),
            request.message_text.as_deref(),
            request.confirm,
        )
        .await
        {
            Ok(value) => json_success(&value),
            Err(error) => json_error(format!("Could not delete Discord message: {error:#}")),
        }
    }

    /// Return low-level normalized page data for adapter diagnostics.
    #[tool(
        name = "inspect_discord_page",
        description = "Developer diagnostic: visible Discord page text and DOM data. Only exposed when DISCORD_MCP_DEBUG_TOOLS=1.",
        annotations(read_only_hint = true)
    )]
    async fn inspect_discord_page(&self) -> CallToolResult {
        match browser::inspect_value().await {
            Ok(value) => json_success(&value),
            Err(error) => json_error(format!("Could not inspect Discord page: {error:#}")),
        }
    }
}

#[tool_handler(router = self.tool_router)]
impl rmcp::ServerHandler for DiscordServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(
                Implementation::new(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"))
                    .with_title("Discord"),
            )
            .with_instructions(INSTRUCTIONS)
    }
}

pub async fn serve() -> Result<()> {
    let server = DiscordServer::default();
    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::DiscordServer;
    use rmcp::ServerHandler;

    #[test]
    fn debug_tools_hidden_by_default() {
        let hidden = DiscordServer::with_debug_tools(false);
        assert!(!hidden.tool_router.has_route("inspect_discord_page"));
        assert!(hidden.tool_router.has_route("send_message"));
        assert!(
            DiscordServer::with_debug_tools(true)
                .tool_router
                .has_route("inspect_discord_page")
        );
    }

    #[test]
    fn server_info_names_server_and_has_instructions() {
        let info = DiscordServer::with_debug_tools(false).get_info();
        assert_eq!(info.server_info.name, env!("CARGO_PKG_NAME"));
        assert!(info.instructions.unwrap_or_default().contains("confirm"));
    }

    #[test]
    fn tool_descriptions_stay_short() {
        for tool in DiscordServer::tool_router().list_all() {
            let len = tool.description.as_deref().unwrap_or_default().len();
            assert!(len <= 250, "{} description is {len} chars", tool.name);
        }
    }

    #[test]
    fn tools_advertise_read_only_hints() {
        let tools = DiscordServer::tool_router().list_all();
        assert_eq!(
            tools.len(),
            10,
            "expected 10 Discord tools, got {}",
            tools.len()
        );
        for tool in &tools {
            let read_only = tool.annotations.as_ref().and_then(|a| a.read_only_hint);
            assert_eq!(
                read_only,
                Some(!matches!(
                    tool.name.as_ref(),
                    "send_message" | "reply_message" | "delete_message"
                )),
                "unexpected read-only hint for {}",
                tool.name
            );
        }
    }
}
