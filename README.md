# Discord MCP

A local Discord MCP server written in Rust. It follows the same login workflow as the Teams MCP:

1. Run discord_mcp login to open a dedicated visible Chromium profile.
2. Complete Discord's normal login, including MFA if needed.
3. The profile is preserved locally.
4. Run discord_mcp mcp; agent calls reuse that authenticated browser session.

The server does not read passwords, MFA codes, cookies, or Discord API tokens. Discord interaction is through the rendered web UI. UI selectors can change as Discord changes, so inspect is included to make adapter maintenance observable.

## Quick start

~~~bash
cargo build --release
./target/release/discord_mcp login
./target/release/discord_mcp status
./target/release/discord_mcp mcp
~~~

The CLI also supports:

~~~bash
discord_mcp inspect
discord_mcp logout
discord_mcp http
~~~

http serves the same MCP tools at http://127.0.0.1:3032/mcp. It has no built-in authentication, so keep it on loopback unless an authenticated proxy is in front of it.

## MCP client configuration

For a stdio MCP client such as Claude Desktop or another agent host:

~~~json
{
  "mcpServers": {
    "discord": {
      "command": "/Users/lagos/code/discord_mcp/target/release/discord_mcp"
    }
  }
}
~~~

The login command must be run once before the MCP process can use authenticated tools. The profile is stored in the OS application-data directory under `io.discord.discord-mcp/browser-profile` on macOS.

## Tools

- get_discord_overview — bounded snapshot of the visible authenticated page.
- list_friends — visible friend labels with offset/limit pagination, optional case-insensitive `query`, and `sort` (`ui`, `label_asc`, or `label_desc`).
- list_servers — visible server labels with offset/limit pagination, optional case-insensitive `query`, and `sort`.
- list_channels — visible channels, optionally scoped by exact server label, with pagination, query filtering, and sorting.
- list_dms — visible direct and group-message labels with pagination, query filtering, and sorting. Each result includes the stable visible `id` when Discord exposes one.
- get_messages — visible messages for a channel, DM, or friend selected by label or the `channel_id` returned by a list tool, with pagination.
- send_message — preview by default; sends only when confirm is true. Existing drafts are reported and preserved unless `clear_draft=true`.
- reply_message — preview by default; replies to a visible message by message ID or text when confirm is true. Existing drafts are reported and preserved unless `clear_draft=true`.
- delete_message — preview by default; deletes a visible message by message ID or text only when `confirm=true`.
- inspect_discord_page — bounded DOM diagnostics for selector work.

Server and channel names are visible UI references. Message IDs are returned only where the Discord DOM exposes them.
Pagination is offset/limit over rows currently loaded in Discord's rendered UI; large friend, server, or message histories may require additional adapter scrolling as Discord virtualizes them.

For reliable navigation, pass the `id` from `list_dms` or `list_channels` as `channel_id` to `get_messages`, `send_message`, or `reply_message`. Label matching remains supported and is case-insensitive.

## Browser configuration

~~~bash
DISCORD_BROWSER_EXECUTABLE=/path/to/chrome discord_mcp login
DISCORD_HEADFUL=1 discord_mcp status
DISCORD_CDP_URL=ws://127.0.0.1:9222/devtools/browser/... discord_mcp status
DISCORD_WEB_URL=https://discord.com/app discord_mcp login
~~~

DISCORD_CDP_URL attaches to an existing browser and does not create the stored profile. The normal path is the dedicated profile created by login.

## Development

~~~bash
cargo fmt --all -- --check
cargo check
cargo test
cargo build --release
~~~

Sending and deletion are confirmation-gated; sending refuses to overwrite an existing composer draft. The adapter uses the rendered Discord UI and does not use direct Discord API integration.
