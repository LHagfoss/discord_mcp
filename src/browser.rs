use std::{
    env,
    io::{self, Write},
    path::PathBuf,
    sync::OnceLock,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use chromiumoxide::{
    Browser, browser::BrowserConfig, cdp::browser_protocol::input::InsertTextParams,
};
use directories::ProjectDirs;
use futures_util::StreamExt;
use serde::Serialize;
use tokio::time::{sleep, timeout};

use crate::data::{
    DeleteMessageResult, ItemsResult, MessagesResult, PageSnapshot, SendMessageResult, UiItem,
    VisibleMessage,
};

const DEFAULT_DISCORD_URL: &str = "https://discord.com/app";
const LOGIN_WAIT: Duration = Duration::from_secs(10 * 60);
const MAX_VISIBLE_TEXT_CHARS: usize = 20_000;
const MAX_ITEMS: usize = 200;
const MAX_MESSAGES: usize = 100;
const MAX_MESSAGE_CHARS: usize = 2_000;

static PROFILE_LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

struct BrowserSession {
    browser: Browser,
    handler_task: tokio::task::JoinHandle<()>,
    page: chromiumoxide::Page,
    owns_browser: bool,
    _profile_guard: tokio::sync::MutexGuard<'static, ()>,
}

fn profile_lock() -> &'static tokio::sync::Mutex<()> {
    PROFILE_LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

fn app_dirs() -> Result<ProjectDirs> {
    ProjectDirs::from("io", "discord", "discord-mcp")
        .context("could not determine an OS application-data directory")
}

fn profile_dir() -> Result<PathBuf> {
    Ok(app_dirs()?.data_dir().join("browser-profile"))
}

fn discord_url() -> String {
    env::var("DISCORD_WEB_URL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_DISCORD_URL.to_string())
}

fn print_json<T: Serialize>(value: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

fn browser_config(headful: bool) -> Result<BrowserConfig> {
    let profile = profile_dir()?;
    std::fs::create_dir_all(&profile)
        .with_context(|| format!("could not create browser profile at {}", profile.display()))?;

    let mut builder = BrowserConfig::builder()
        .user_data_dir(profile)
        .port(0)
        .window_size(1440, 1000);
    if let Ok(executable) = env::var("DISCORD_BROWSER_EXECUTABLE") {
        builder = builder.chrome_executable(executable);
    }
    let headful = headful
        || env::var("DISCORD_HEADFUL")
            .map(|value| {
                !matches!(
                    value.trim().to_ascii_lowercase().as_str(),
                    "0" | "false" | "no"
                )
            })
            .unwrap_or(false);
    if headful {
        builder = builder.with_head();
    }
    builder
        .build()
        .map_err(|error| anyhow::anyhow!("could not build Chromium configuration: {error}"))
}

async fn launch(headful: bool) -> Result<(Browser, chromiumoxide::Handler)> {
    if let Ok(cdp_url) = env::var("DISCORD_CDP_URL") {
        if cdp_url.trim().is_empty() {
            bail!("DISCORD_CDP_URL is set but empty");
        }
        tracing::info!(%cdp_url, "attaching to an existing browser over CDP");
        return Browser::connect(cdp_url).await.map_err(Into::into);
    }

    Browser::launch(browser_config(headful)?).await.map_err(|error| {
        anyhow::anyhow!(
            "could not launch Chromium: {error}. Install Chrome/Chromium or set DISCORD_CDP_URL to an existing browser WebSocket URL"
        )
    })
}

fn is_retryable_page_context_error(error: &anyhow::Error) -> bool {
    let message = error.to_string().to_lowercase();
    message.contains("cannot find context with specified id")
        || message.contains("execution context was destroyed")
        || message.contains("cannot find execution context")
}

fn truncate(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

fn is_authenticated_url(url: &str) -> bool {
    let Ok(parsed) = url::Url::parse(url) else {
        return false;
    };
    let Some(host) = parsed.host_str() else {
        return false;
    };
    let path = parsed.path().to_ascii_lowercase();
    matches!(
        host,
        "discord.com" | "www.discord.com" | "discordapp.com" | "www.discordapp.com"
    ) && path != "/login"
        && (path.starts_with("/channels/") || path == "/app" || path == "/app/")
}

fn looks_like_login_page(text: &str) -> bool {
    let text = text.to_lowercase();
    text.contains("welcome back!")
        || text.contains("email or phone number")
        || text.contains("forgot your password")
}

fn looks_like_loading_page(text: &str) -> bool {
    let text = text.to_lowercase();
    text.contains("loading") || text.contains("connection problems?")
}

fn is_authenticated_snapshot(
    url: &str,
    app_ready: bool,
    login_input: bool,
    visible_text: &str,
) -> bool {
    is_authenticated_url(url)
        && app_ready
        && !login_input
        && !looks_like_login_page(visible_text)
        && !looks_like_loading_page(visible_text)
}

async fn page_snapshot(page: &chromiumoxide::Page) -> Result<PageSnapshot> {
    for attempt in 0..40 {
        let snapshot = async {
            let page_title = page.get_title().await?.unwrap_or_default();
            let url = page.url().await?.unwrap_or_default();
            let state: serde_json::Value = page
                .evaluate(
                    r#"() => {
                        const text = (document.body?.innerText || '').trim();
                        const loginInput = document.querySelector('input[name="email"], input[name="password"], input[autocomplete="username"], input[autocomplete="current-password"], input[aria-label*="email" i], input[aria-label*="password" i]');
                        const appMount = document.querySelector('#app-mount');
                        const hasDiscordUi = !!document.querySelector(
                            '[data-list-id="guildsnav"], [data-list-id="private-channels"], [data-list-item-id^="channels___"], [id^="chat-messages-"], [data-list-id^="channels"]'
                        );
                        const loading = /\bloading\b|connection problems\?/i.test(text);
                        const appReady = !!appMount && appMount.childElementCount > 0 && hasDiscordUi && !loading;
                        return { visible_text: text, login_input: !!loginInput, app_ready: appReady };
                    }"#,
                )
                .await?
                .into_value()?;
            let visible_text = truncate(
                state
                    .get("visible_text")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default(),
                MAX_VISIBLE_TEXT_CHARS,
            );
            let login_input = state
                .get("login_input")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            let app_ready = state
                .get("app_ready")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            Ok::<PageSnapshot, anyhow::Error>(PageSnapshot {
                authenticated: is_authenticated_snapshot(
                    &url,
                    app_ready,
                    login_input,
                    &visible_text,
                ),
                page_title,
                url,
                visible_text,
            })
        }
        .await;

        match snapshot {
            Ok(snapshot) => return Ok(snapshot),
            Err(error) if is_retryable_page_context_error(&error) && attempt < 39 => {
                sleep(Duration::from_millis(250)).await;
            }
            Err(error) => return Err(error),
        }
    }
    bail!("the Discord page did not expose a stable browser execution context")
}

async fn wait_for_discord(page: &chromiumoxide::Page) -> Result<PageSnapshot> {
    let mut snapshot = page_snapshot(page).await?;
    for _ in 0..40 {
        if snapshot.authenticated {
            return Ok(snapshot);
        }
        sleep(Duration::from_millis(500)).await;
        snapshot = page_snapshot(page).await?;
    }
    Ok(snapshot)
}

async fn open_discord(headful: bool) -> Result<BrowserSession> {
    let profile_guard = profile_lock().lock().await;
    let owns_browser = env::var_os("DISCORD_CDP_URL").is_none();
    let (mut browser, mut handler) = launch(headful).await?;
    let handler_task = tokio::spawn(async move {
        while let Some(event) = handler.next().await {
            if let Err(error) = event {
                tracing::debug!(%error, "browser handler stopped");
                break;
            }
        }
    });

    let page = if owns_browser {
        browser.new_page(discord_url()).await?
    } else {
        browser.fetch_targets().await?;
        sleep(Duration::from_millis(250)).await;
        let mut pages = browser.pages().await?;
        let mut discord_page = None;
        for candidate in pages.iter().cloned() {
            if let Ok(Some(url)) = candidate.url().await {
                if is_authenticated_url(&url) || url.contains("discord.com") {
                    discord_page = Some(candidate);
                    break;
                }
            }
        }
        discord_page
            .or_else(|| pages.pop())
            .context("connected browser has no open pages")?
    };
    Ok(BrowserSession {
        browser,
        handler_task,
        page,
        owns_browser,
        _profile_guard: profile_guard,
    })
}

async fn finish(mut session: BrowserSession) -> Result<()> {
    let result = if session.owns_browser {
        match session.browser.close().await {
            Ok(_) => session
                .browser
                .wait()
                .await
                .map(|_| ())
                .map_err(anyhow::Error::from),
            Err(error) => Err(error.into()),
        }
    } else {
        Ok(())
    };
    session.handler_task.abort();
    result.map_err(Into::into)
}

async fn prepared_page() -> Result<BrowserSession> {
    let session = open_discord(false).await?;
    if session.owns_browser {
        session.page.goto(discord_url()).await?;
    }
    let snapshot = match wait_for_discord(&session.page).await {
        Ok(snapshot) => snapshot,
        Err(error) => {
            finish(session).await?;
            return Err(error);
        }
    };
    if !snapshot.authenticated {
        finish(session).await?;
        bail!("Discord is not authenticated; run 'discord_mcp login' first")
    }
    Ok(session)
}

async fn click_server(page: &chromiumoxide::Page, server_name: &str) -> Result<bool> {
    click_named(page, server_name, "server").await
}

async fn click_named(page: &chromiumoxide::Page, label: &str, kind: &str) -> Result<bool> {
    let label = serde_json::to_string(label)?;
    let kind = serde_json::to_string(kind)?;
    let marked: bool = page
        .evaluate(format!(
        r#"() => {{
            const wanted = {label};
            const kind = {kind};
            const normalize = value => (value || '').replace(/\s+/g, ' ').trim();
            const matching = value => {{
                const text = normalize(value).toLocaleLowerCase();
                const target = normalize(wanted).toLocaleLowerCase();
                return text === target || text.startsWith(target + ' ');
            }};
            const selector = kind === 'server'
                ? '[data-list-id="guildsnav"] [data-list-item-id], [data-list-id="guildsnav"] [role="button"], nav[aria-label*="server" i] [role="button"]'
                : kind === 'home'
                ? '[data-list-item-id*="home" i], [data-list-item-id*="friends" i], [aria-label*="friends" i], [role="link"], [role="button"]'
                : '[data-list-item-id^="channels___"], [data-list-item-id^="private-channels-"], [data-list-id="private-channels"] [role="listitem"], [data-list-id="private-channels"] [role="link"], main [role="listitem"], [role="treeitem"], [role="link"]';
            const candidates = Array.from(document.querySelectorAll(selector));
            const valuesFor = element => [element.getAttribute('aria-label'), element.getAttribute('title'), element.innerText, element.textContent];
            const wantedNormalized = normalize(wanted).toLocaleLowerCase();
            const exact = candidates.find(element => valuesFor(element).some(value => normalize(value).toLocaleLowerCase() === wantedNormalized));
            const target = exact || candidates.find(element => {{
                return valuesFor(element).some(matching);
            }});
            document.querySelectorAll('[data-discord-mcp-click-target]').forEach(element => element.removeAttribute('data-discord-mcp-click-target'));
            if (!target) return false;
            target.setAttribute('data-discord-mcp-click-target', 'true');
            return true;
        }}"#
        ))
        .await?
        .into_value()?;
    if !marked {
        return Ok(false);
    }
    let target = page
        .find_element("[data-discord-mcp-click-target]")
        .await
        .context("could not locate the visible Discord item selected for clicking")?;
    target.click().await?;
    page.evaluate(
        r#"() => document.querySelectorAll('[data-discord-mcp-click-target]').forEach(element => element.removeAttribute('data-discord-mcp-click-target'))"#,
    )
    .await?;
    Ok(true)
}

async fn click_item_id(page: &chromiumoxide::Page, item_id: &str) -> Result<bool> {
    let item_id = serde_json::to_string(item_id)?;
    let marked: bool = page
        .evaluate(format!(
            r#"() => {{
                const wanted = {item_id};
                const target = Array.from(document.querySelectorAll('[data-list-item-id]'))
                    .find(element => element.getAttribute('data-list-item-id') === wanted);
                document.querySelectorAll('[data-discord-mcp-click-target]').forEach(element => element.removeAttribute('data-discord-mcp-click-target'));
                if (!target) return false;
                target.setAttribute('data-discord-mcp-click-target', 'true');
                return true;
            }}"#
        ))
        .await?
        .into_value()?;
    if !marked {
        return Ok(false);
    }
    let target = page
        .find_element("[data-discord-mcp-click-target]")
        .await
        .context("could not locate the visible Discord item selected by ID")?;
    target.click().await?;
    page.evaluate(
        r#"() => document.querySelectorAll('[data-discord-mcp-click-target]').forEach(element => element.removeAttribute('data-discord-mcp-click-target'))"#,
    )
    .await?;
    Ok(true)
}

#[derive(Debug, serde::Deserialize)]
struct EvaluatedItems {
    items: Vec<UiItem>,
    total: usize,
}

async fn evaluate_items(page: &chromiumoxide::Page, kind: &str) -> Result<EvaluatedItems> {
    let kind = serde_json::to_string(kind)?;
    page.evaluate(format!(
        r#"() => {{
            const kind = {kind};
            const normalize = value => (value || '').replace(/\s+/g, ' ').trim();
            const seen = new Set();
            const result = [];
            const add = (element, label, secondary = null, itemType = null) => {{
                label = normalize(label);
                if (!label || label.length > 160 || seen.has(label)) return;
                const stableElement = element?.closest('[data-list-item-id]') || element;
                const id = stableElement?.getAttribute('data-list-item-id') || element?.getAttribute('data-id') || element?.id || null;
                seen.add(label);
                result.push({{ label, id, secondary_label: normalize(secondary) || null, item_type: itemType || null }});
            }};
            let selector;
            if (kind === 'servers') selector = '[data-list-id="guildsnav"] [data-list-item-id], [data-list-id="guildsnav"] [role="button"], nav[aria-label*="server" i] [role="button"]';
            if (kind === 'channels') selector = '[data-list-item-id^="channels___"], [data-list-id^="channels"] [role="treeitem"], [data-list-id^="channels"] [role="link"], [aria-label*="channel" i][role="link"]';
            if (kind === 'dms') selector = '[data-list-id="private-channels"] [role="listitem"], [data-list-id="private-channels"] [role="link"], [data-list-item-id^="private-channels-"]';
            if (kind === 'friends') selector = '[data-list-item-id^="friends___"], [data-list-item-id^="people___"], [class*="peopleListItem" i], main [role="listitem"]';
            document.querySelectorAll(selector || '').forEach(element => {{
                const rawLabel = element.getAttribute('aria-label') || element.getAttribute('title') || element.innerText || element.textContent;
                const secondary = element.getAttribute('data-list-item-id') || element.getAttribute('role');
                const raw = kind === 'friends'
                    ? (rawLabel || '').split(/\n+/).map(value => normalize(value)).find(Boolean) || ''
                    : normalize(rawLabel);
                if (!raw) return;
                if (kind === 'servers' && (element.getAttribute('data-list-item-id') === 'guildsnav___home' || /add a server|explore discoverable|download apps|^direct messages$/i.test(raw))) return;
                if (kind === 'channels' && /^voice channels?$|^text channels?$/i.test(raw)) return;
                if (kind === 'dms' && /^(friends|message requests|nitro home|shop|quests|find or start a conversation)(?:\s+\d+)?$/i.test(raw)) return;
                const cleaned = kind === 'channels'
                    ? raw.replace(/\s+\((?:text|voice) channel\)$/i, '').replace(/\s+\(category\)$/i, '').trim()
                    : kind === 'dms'
                    ? raw.replace(/\s+\((?:direct|group) message\).*$/i, '').replace(/,\s*(?:pinned|online|idle|do not disturb|offline).*$/i, '').trim()
                    : raw;
                const channelType = kind === 'channels'
                    ? /category/i.test(raw) ? 'category' : /voice/i.test(raw) ? 'voice' : 'text'
                    : null;
                add(element, cleaned, secondary, kind === 'channels' ? channelType : kind === 'dms' ? 'dm' : kind === 'friends' ? 'friend' : 'server');
            }});
            return {{ items: result.slice(0, {MAX_ITEMS}), total: result.length }};
        }}"#
    ))
    .await?
    .into_value()
    .map_err(Into::into)
}

async fn result_items(
    session: &BrowserSession,
    kind: &str,
    offset: usize,
    limit: usize,
    query: Option<&str>,
    sort: Option<&str>,
    mut warnings: Vec<String>,
) -> Result<ItemsResult> {
    let evaluated = evaluate_items(&session.page, kind).await?;
    let query = query.map(str::trim).filter(|value| !value.is_empty());
    let query_lower = query.map(str::to_lowercase);
    let sort = sort.unwrap_or("ui").trim().to_ascii_lowercase();
    let mut filtered = evaluated
        .items
        .into_iter()
        .filter(|item| {
            query_lower
                .as_ref()
                .is_none_or(|query| item.label.to_lowercase().contains(query))
        })
        .collect::<Vec<_>>();
    match sort.as_str() {
        "ui" | "" => {}
        "label_asc" => filtered.sort_by_cached_key(|item| item.label.to_lowercase()),
        "label_desc" => filtered.sort_by_cached_key(|item| std::cmp::Reverse(item.label.to_lowercase())),
        other => warnings.push(format!(
            "Unknown sort {other:?}; supported values are ui, label_asc, and label_desc. Using ui order."
        )),
    }
    let filtered_total = filtered.len();
    let items = filtered
        .into_iter()
        .skip(offset)
        .take(limit)
        .collect::<Vec<_>>();
    if evaluated.total == 0 {
        warnings.push(format!(
            "The current Discord UI did not expose {kind} in the visible DOM; use inspect_discord_page to capture the current page shape."
        ));
    }
    if evaluated.total > MAX_ITEMS {
        warnings.push(format!(
            "Discord exposed only the first {MAX_ITEMS} {kind} in the current DOM; pagination is limited to loaded visible UI."
        ));
    }
    let has_more = offset + items.len() < filtered_total;
    Ok(ItemsResult {
        page_url: session.page.url().await?.unwrap_or_default(),
        offset,
        limit,
        items,
        has_more,
        warnings,
    })
}

fn page_limit(value: u32) -> usize {
    value.clamp(1, MAX_ITEMS as u32) as usize
}

pub async fn visible_snapshot() -> Result<PageSnapshot> {
    let session = prepared_page().await?;
    let result = page_snapshot(&session.page).await;
    finish(session).await?;
    result
}

pub async fn list_servers(
    offset: u32,
    limit: u32,
    query: Option<&str>,
    sort: Option<&str>,
) -> Result<ItemsResult> {
    let session = prepared_page().await?;
    let result = result_items(
        &session,
        "servers",
        offset as usize,
        page_limit(limit),
        query,
        sort,
        Vec::new(),
    )
    .await;
    finish(session).await?;
    result
}

pub async fn list_channels(
    server_name: Option<&str>,
    offset: u32,
    limit: u32,
    query: Option<&str>,
    sort: Option<&str>,
) -> Result<ItemsResult> {
    let session = prepared_page().await?;
    let result = async {
        let mut warnings = Vec::new();
        if let Some(server_name) = server_name.filter(|value| !value.trim().is_empty()) {
            if !click_server(&session.page, server_name).await? {
                warnings.push(format!(
                    "Could not find an exact visible server label {server_name:?}; channels were read from the current page."
                ));
            } else {
                sleep(Duration::from_millis(500)).await;
            }
        }
        result_items(
            &session,
            "channels",
            offset as usize,
            page_limit(limit),
            query,
            sort,
            warnings,
        )
        .await
    }
    .await;
    finish(session).await?;
    result
}

pub async fn list_dms(
    offset: u32,
    limit: u32,
    query: Option<&str>,
    sort: Option<&str>,
) -> Result<ItemsResult> {
    let session = prepared_page().await?;
    let result = result_items(
        &session,
        "dms",
        offset as usize,
        page_limit(limit),
        query,
        sort,
        Vec::new(),
    )
    .await;
    finish(session).await?;
    result
}

pub async fn list_friends(
    offset: u32,
    limit: u32,
    query: Option<&str>,
    sort: Option<&str>,
) -> Result<ItemsResult> {
    let session = prepared_page().await?;
    let result = async {
        let mut warnings = Vec::new();
        if !click_named(&session.page, "Friends", "home").await? {
            warnings.push(
                "Could not find the Friends view; friend rows were read from the current page."
                    .to_string(),
            );
        } else {
            sleep(Duration::from_millis(700)).await;
        }
        result_items(
            &session,
            "friends",
            offset as usize,
            page_limit(limit),
            query,
            sort,
            warnings,
        )
        .await
    }
    .await;
    finish(session).await?;
    result
}

async fn select_channel(
    page: &chromiumoxide::Page,
    channel_name: &str,
    channel_id: Option<&str>,
    server_name: Option<&str>,
) -> Result<()> {
    if let Some(server_name) = server_name.filter(|value| !value.trim().is_empty()) {
        if !click_server(page, server_name).await? {
            bail!("Could not find an exact visible server label {server_name:?}");
        }
        sleep(Duration::from_millis(500)).await;
    }
    if let Some(channel_id) = channel_id.map(str::trim).filter(|value| !value.is_empty()) {
        if !click_item_id(page, channel_id).await? {
            bail!("Could not find the visible Discord channel or DM ID {channel_id:?}");
        }
        sleep(Duration::from_millis(700)).await;
        return Ok(());
    }
    if !click_named(page, channel_name, "channel").await? {
        if click_named(page, "Friends", "home").await? {
            sleep(Duration::from_millis(500)).await;
            if !click_named(page, channel_name, "channel").await? {
                bail!(
                    "Could not find an exact visible Discord channel, DM, or friend label {channel_name:?}"
                );
            }
        } else {
            bail!(
                "Could not find an exact visible Discord channel, DM, or friend label {channel_name:?}"
            );
        }
    }
    sleep(Duration::from_millis(700)).await;
    Ok(())
}

#[derive(Debug, serde::Deserialize)]
struct EvaluatedMessages {
    messages: Vec<VisibleMessage>,
    total: usize,
}

async fn evaluate_messages(page: &chromiumoxide::Page) -> Result<EvaluatedMessages> {
    page.evaluate(format!(
        r#"() => {{
            const normalize = value => (value || '').replace(/\s+/g, ' ').trim();
            const seen = new Set();
            const result = [];
            const candidates = Array.from(document.querySelectorAll('[id^="chat-messages-"], li[class*="message" i], [class*="messageListItem" i]'));
            candidates.forEach(element => {{
                const container = element.closest('[id^="chat-messages-"]') || element;
                const contentNode = container.querySelector('[id^="message-content-"], [class*="markup" i], [class*="messageContent" i], [data-slate-string], [class*="textContainer" i]');
                const rawText = normalize(container.innerText || container.textContent);
                let text = normalize(contentNode?.innerText || contentNode?.textContent);
                if (!text) {{
                    const metadataMarker = /\s+—\s+\d{{1,2}}:\d{{2}}\s+\w+,\s+\w+\s+\d{{1,2}},\s+\d{{4}}\s+at\s+\d{{1,2}}:\d{{2}}(?:\s*)$/;
                    text = metadataMarker.test(rawText)
                        ? '[non-text message]'
                        : rawText.replace(/\s+Server Tag:.*$/i, '').trim();
                }}
                text = text
                    .replace(/\s+(?:\d+\s+)?Add Reaction$/i, '')
                    .replace(/\s+(?:Monday|Tuesday|Wednesday|Thursday|Friday|Saturday|Sunday),\s+\w+\s+\d{{1,2}},\s+\d{{4}}\s+at\s+\d{{1,2}}:\d{{2}}$/i, '')
                    .trim();
                if (!text || text.length > 10000) return;
                const id = container.getAttribute('id') || element.getAttribute('data-message-id') || element.getAttribute('data-id');
                const key = id || text;
                if (seen.has(key)) return;
                seen.add(key);
                const authorNode = container.querySelector('[class*="username" i], h3, [class*="headerText" i], [role="button"][aria-label*="message" i]');
                const timeNode = container.querySelector('time');
                const author = normalize(authorNode?.getAttribute('aria-label') || authorNode?.innerText)
                    .replace(/\s+Server Tag:.*$/i, '')
                    .replace(/\s+—\s+\d{{1,2}}:\d{{2}}.*$/, '')
                    .trim();
                if (author && (text === author || text.startsWith(author + ' '))) text = '[non-text message]';
                result.push({{
                    text,
                    message_id: id || null,
                    author: author || null,
                    timestamp: timeNode?.getAttribute('datetime') || normalize(timeNode?.innerText) || null
                }});
            }});
            return {{ messages: result.slice(-{MAX_MESSAGES}), total: result.length }};
        }}"#
    ))
    .await?
    .into_value()
    .map_err(Into::into)
}

pub async fn visible_messages(
    channel_name: &str,
    channel_id: Option<&str>,
    server_name: Option<&str>,
    offset: u32,
    limit: u32,
) -> Result<MessagesResult> {
    let channel_name = channel_name.trim();
    if channel_name.is_empty() {
        bail!("channel_name is required");
    }
    let offset = offset as usize;
    let limit = limit.clamp(1, MAX_MESSAGES as u32) as usize;
    let session = prepared_page().await?;
    let result = async {
        select_channel(&session.page, channel_name, channel_id, server_name).await?;
        let evaluated = evaluate_messages(&session.page).await?;
        let end = evaluated.total.saturating_sub(offset);
        let start = end.saturating_sub(limit);
        let loaded_start = evaluated.total.saturating_sub(evaluated.messages.len());
        let slice_start = start
            .saturating_sub(loaded_start)
            .min(evaluated.messages.len());
        let slice_end = end
            .saturating_sub(loaded_start)
            .min(evaluated.messages.len())
            .max(slice_start);
        let messages = evaluated
            .messages
            .into_iter()
            .skip(slice_start)
            .take(slice_end.saturating_sub(slice_start))
            .collect::<Vec<_>>();
        let mut warnings = Vec::new();
        if messages.is_empty() {
            warnings.push("The current Discord UI did not expose message elements in the visible DOM; use inspect_discord_page to update the adapter selectors.".to_string());
        }
        if evaluated.total > MAX_MESSAGES {
            warnings.push(format!(
                "Discord exposed only the latest {MAX_MESSAGES} messages in the current DOM; older pagination requires scrolling the chat UI."
            ));
        }
        Ok(MessagesResult {
            page_url: session.page.url().await?.unwrap_or_default(),
            channel: channel_name.to_string(),
            offset,
            limit,
            has_more: offset + messages.len() < evaluated.total,
            messages,
            warnings,
        })
    }
    .await;
    finish(session).await?;
    result
}

const COMPOSER_SELECTOR: &str = r#"[role="textbox"][contenteditable="true"]:not([aria-label*="Search" i]), textarea, div[contenteditable="true"][data-slate-editor="true"]:not([role="combobox"]):not([aria-label*="Search" i])"#;
const SEND_SELECTOR: &str = r#"button[aria-label*="Send" i], button[type="submit"], [role="button"][aria-label*="Send" i], [data-testid*="send" i], [class*="sendButton" i]"#;

async fn clear_composer(page: &chromiumoxide::Page) -> Result<()> {
    let cleared: bool = page
        .evaluate(format!(
            r#"() => {{
                const box = document.querySelector({composer:?});
                if (!box) return false;
                const meaningfulText = value => (value || '').replace(/[\u200B-\u200D\uFEFF]/g, '').trim();
                box.focus();
                if (box.isContentEditable) {{
                    document.execCommand('selectAll', false, null);
                    document.execCommand('delete', false, null);
                    if (meaningfulText(box.innerText || box.textContent)) {{
                        box.innerHTML = '';
                        box.dispatchEvent(new InputEvent('input', {{ bubbles: true, inputType: 'deleteContentBackward' }}));
                    }}
                }} else {{
                    const prototype = Object.getPrototypeOf(box);
                    const setter = Object.getOwnPropertyDescriptor(prototype, 'value')?.set;
                    if (setter) setter.call(box, '');
                    else box.value = '';
                    box.dispatchEvent(new Event('input', {{ bubbles: true }}));
                    box.dispatchEvent(new Event('change', {{ bubbles: true }}));
                }}
                return !meaningfulText(box.innerText || box.value || box.textContent);
            }}"#,
            composer = COMPOSER_SELECTOR,
        ))
        .await?
        .into_value()?;
    if !cleared {
        bail!("Discord did not expose a composer that could be cleared");
    }
    sleep(Duration::from_millis(150)).await;
    Ok(())
}

fn validate_message(message: &str) -> Result<()> {
    if message.trim().is_empty() {
        bail!("message is required");
    }
    if message.chars().count() > MAX_MESSAGE_CHARS {
        bail!("message is limited to {MAX_MESSAGE_CHARS} characters");
    }
    Ok(())
}

async fn send_from_composer(
    page: &chromiumoxide::Page,
    channel_name: &str,
    message: &str,
    reply_to: Option<String>,
    clear_draft: bool,
) -> Result<SendMessageResult> {
    let state: serde_json::Value = page
        .evaluate(format!(
            r#"() => {{
                const box = document.querySelector({composer:?});
                const meaningfulText = value => (value || '').replace(/[\u200B-\u200D\uFEFF]/g, '').trim();
                const rawText = box?.innerText || box?.value || box?.textContent || '';
                return {{ found: !!box, text: meaningfulText(rawText) }};
            }}"#,
            composer = COMPOSER_SELECTOR,
        ))
        .await?
        .into_value()?;
    if !state
        .get("found")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
    {
        bail!("The current Discord UI did not expose a message composer");
    }
    let draft = state
        .get("text")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .trim();
    if !draft.is_empty() && !clear_draft {
        bail!(
            "The message composer already contains a draft: {:?}. The draft was preserved; retry with clear_draft=true to clear it before sending.",
            truncate(draft, MAX_MESSAGE_CHARS)
        );
    }
    if !draft.is_empty() {
        clear_composer(page).await?;
    }

    let composer = page
        .find_element(COMPOSER_SELECTOR)
        .await
        .context("could not locate the Discord message composer")?;
    composer.click().await?;
    page.execute(InsertTextParams::new(message.to_string()))
        .await?;
    sleep(Duration::from_millis(200)).await;

    let typed: serde_json::Value = page
        .evaluate(format!(
            r#"() => document.querySelector({composer:?})?.innerText || document.querySelector({composer:?})?.value || document.querySelector({composer:?})?.textContent || ''"#,
            composer = COMPOSER_SELECTOR,
        ))
        .await?
        .into_value()?;
    let typed_text = typed.as_str().unwrap_or_default();
    if !typed_text.contains(message) {
        bail!("Discord did not expose the typed message exactly; refusing to click Send");
    }

    let send_state: serde_json::Value = page
        .evaluate(format!(
            r#"() => {{
                const box = document.querySelector({composer:?});
                const scope = box?.closest('form') || box?.parentElement?.parentElement?.parentElement || document;
                const visible = element => {{
                    const style = getComputedStyle(element);
                    return style.display !== 'none' && style.visibility !== 'hidden' && !element.disabled;
                }};
                const label = element => [
                    element.getAttribute('aria-label'),
                    element.getAttribute('title'),
                    element.getAttribute('data-testid'),
                    element.className
                ].filter(Boolean).join(' ');
                const candidates = Array.from(scope.querySelectorAll({send:?})).filter(visible);
                const button = candidates.find(element => /^(send|send message|send a message)$/i.test(label(element)))
                    || candidates.find(element => /send message|sendbutton/i.test(label(element)));
                if (!button) return {{
                    clicked: false,
                    candidates: candidates.map(element => ({{
                        tag: element.tagName,
                        aria_label: element.getAttribute('aria-label'),
                        title: element.getAttribute('title'),
                        data_testid: element.getAttribute('data-testid'),
                        class_name: element.className
                    }}))
                }};
                button.click();
                return {{ clicked: true, candidates: [] }};
            }}"#,
            composer = COMPOSER_SELECTOR,
            send = SEND_SELECTOR,
        ))
        .await?
        .into_value()?;
    let clicked = send_state
        .get("clicked")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    if !clicked {
        composer.press_key("Enter").await.context(
            "could not locate the Discord Send control or submit the message composer with Enter",
        )?;
    }
    sleep(Duration::from_millis(700)).await;
    Ok(SendMessageResult {
        channel: channel_name.to_string(),
        message: message.to_string(),
        reply_to,
        sent: true,
        confirmation_required: false,
        page_url: page.url().await?.unwrap_or_default(),
        warnings: vec![
            "Discord UI send was clicked; verify delivery if the channel is slow to update."
                .to_string(),
        ],
    })
}

pub async fn send_message(
    channel_name: &str,
    channel_id: Option<&str>,
    server_name: Option<&str>,
    message: &str,
    confirm: bool,
    clear_draft: bool,
) -> Result<SendMessageResult> {
    let channel_name = channel_name.trim();
    if channel_name.is_empty() {
        bail!("channel_name is required");
    }
    validate_message(message)?;
    if !confirm {
        return Ok(SendMessageResult {
            channel: channel_name.to_string(),
            message: message.to_string(),
            reply_to: None,
            sent: false,
            confirmation_required: true,
            page_url: discord_url(),
            warnings: vec![
                "Preview only: pass confirm=true to open the channel and send this message."
                    .to_string(),
            ],
        });
    }

    let session = prepared_page().await?;
    let result = async {
        select_channel(&session.page, channel_name, channel_id, server_name).await?;
        send_from_composer(&session.page, channel_name, message, None, clear_draft).await
    }
    .await;
    finish(session).await?;
    result
}

async fn start_reply(
    page: &chromiumoxide::Page,
    message_id: Option<&str>,
    message_text: Option<&str>,
) -> Result<String> {
    let message_id = message_id.map(str::trim).filter(|value| !value.is_empty());
    let message_text = message_text
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if message_id.is_none() && message_text.is_none() {
        bail!("message_id or message_text is required to reply");
    }
    let id_json = serde_json::to_string(message_id.unwrap_or_default())?;
    let text_json = serde_json::to_string(message_text.unwrap_or_default())?;
    let state: serde_json::Value = page
        .evaluate(format!(
            r#"() => {{
                const wantedId = {id_json};
                const wantedText = {text_json};
                const normalize = value => (value || '').replace(/\s+/g, ' ').trim();
                const candidates = Array.from(document.querySelectorAll('[id^="chat-messages-"], [data-message-id], li[class*="message" i], [class*="messageListItem" i]'));
                const target = candidates.find(element => {{
                    const container = element.closest('[id^="chat-messages-"], [data-message-id]') || element;
                    const id = container.getAttribute('data-message-id') || container.id || '';
                    const text = normalize(container.innerText || container.textContent);
                    return (wantedId && (id === wantedId || element.getAttribute('data-message-id') === wantedId))
                        || (!wantedId && wantedText && (text === wantedText || text.includes(wantedText)));
                }});
                if (!target) return {{ found: false, id: null }};
                const container = target.closest('[id^="chat-messages-"], [data-message-id]') || target;
                document.querySelectorAll('[data-discord-mcp-reply-target]').forEach(element => element.removeAttribute('data-discord-mcp-reply-target'));
                const hoverTarget = container.querySelector('[id^="message-content-"], [class*="markup" i], [class*="messageContent" i]') || container;
                hoverTarget.setAttribute('data-discord-mcp-reply-target', 'true');
                container.scrollIntoView({{ block: 'center' }});
                return {{ found: true, id: container.getAttribute('data-message-id') || container.id || null }};
            }}"#,
            id_json = id_json,
            text_json = text_json,
        ))
        .await?
        .into_value()?;
    if !state
        .get("found")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
    {
        bail!("Could not find the requested visible message to reply to");
    }
    // Discord renders the action toolbar in response to the message's hover
    // handlers. A synthetic context menu is not equivalent: it is ignored by
    // some Discord builds and can leave the browser waiting for a menu that
    // never opens. Dispatch the same hover events that the live UI receives,
    // then click the rendered Reply control inside this message only.
    page.evaluate(
        r#"() => {
            const target = document.querySelector('[data-discord-mcp-reply-target]');
            if (!target) return false;
            const container = target.closest('[id^="chat-messages-"], [data-message-id]') || target;
            const rect = target.getBoundingClientRect();
            const eventOptions = {
                bubbles: true,
                cancelable: true,
                view: window,
                clientX: rect.left + Math.min(16, rect.width / 2),
                clientY: rect.top + Math.min(10, rect.height / 2),
                relatedTarget: null,
                shiftKey: true
            };
            for (const element of [container, target]) {
                for (const type of ['pointerover', 'mouseover', 'mouseenter']) {
                    element.dispatchEvent(new MouseEvent(type, eventOptions));
                }
            }
            return true;
        }"#,
    )
    .await?
    .into_value::<bool>()?;
    sleep(Duration::from_millis(500)).await;

    let reply_state: serde_json::Value = page
        .evaluate(
            r#"() => {
                const target = document.querySelector('[data-discord-mcp-reply-target]');
                if (!target) return false;
                const container = target.closest('[id^="chat-messages-"], [data-message-id]') || target;
                const visible = element => {
                    const style = getComputedStyle(element);
                    const label = element.getAttribute('aria-label') || element.getAttribute('title') || element.innerText || '';
                    return /reply/i.test(label) && style.display !== 'none' && style.visibility !== 'hidden';
                };
                const button = Array.from(container.querySelectorAll('[aria-label="Reply"], [title="Reply"], button')).find(visible);
                target.removeAttribute('data-discord-mcp-reply-target');
                if (!button) return false;
                button.click();
                return true;
            }"#,
        )
        .await?
        .into_value()?;
    if !reply_state.as_bool().unwrap_or(false) {
        bail!("Discord did not expose a visible Reply control for that message");
    }
    Ok(state
        .get("id")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .or(message_id)
        .or(message_text)
        .unwrap_or("visible message")
        .to_string())
}

pub async fn reply_message(
    channel_name: &str,
    channel_id: Option<&str>,
    server_name: Option<&str>,
    message_id: Option<&str>,
    message_text: Option<&str>,
    message: &str,
    confirm: bool,
    clear_draft: bool,
) -> Result<SendMessageResult> {
    let channel_name = channel_name.trim();
    if channel_name.is_empty() {
        bail!("channel_name is required");
    }
    validate_message(message)?;
    let target = message_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .or_else(|| {
            message_text
                .map(str::trim)
                .filter(|value| !value.is_empty())
        })
        .context("message_id or message_text is required to reply")?;
    if !confirm {
        return Ok(SendMessageResult {
            channel: channel_name.to_string(),
            message: message.to_string(),
            reply_to: Some(target.to_string()),
            sent: false,
            confirmation_required: true,
            page_url: discord_url(),
            warnings: vec![
                "Preview only: pass confirm=true to open the channel, select the message, and send the reply."
                    .to_string(),
            ],
        });
    }

    let session = prepared_page().await?;
    let result = async {
        select_channel(&session.page, channel_name, channel_id, server_name).await?;
        let reply_to = start_reply(&session.page, message_id, message_text).await?;
        sleep(Duration::from_millis(250)).await;
        send_from_composer(
            &session.page,
            channel_name,
            message,
            Some(reply_to),
            clear_draft,
        )
        .await
    }
    .await;
    finish(session).await?;
    result
}

async fn start_delete(
    page: &chromiumoxide::Page,
    message_id: Option<&str>,
    message_text: Option<&str>,
) -> Result<String> {
    let message_id = message_id.map(str::trim).filter(|value| !value.is_empty());
    let message_text = message_text
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if message_id.is_none() && message_text.is_none() {
        bail!("message_id or message_text is required to delete");
    }
    let id_json = serde_json::to_string(message_id.unwrap_or_default())?;
    let text_json = serde_json::to_string(message_text.unwrap_or_default())?;
    let state: serde_json::Value = page
        .evaluate(format!(
            r#"() => {{
                const wantedId = {id_json};
                const wantedText = {text_json};
                const normalize = value => (value || '').replace(/\s+/g, ' ').trim();
                const candidates = Array.from(document.querySelectorAll('[id^="chat-messages-"], [data-message-id], li[class*="message" i], [class*="messageListItem" i]'));
                const target = candidates.find(element => {{
                    const container = element.closest('[id^="chat-messages-"], [data-message-id]') || element;
                    const id = container.getAttribute('data-message-id') || container.id || '';
                    const text = normalize(container.innerText || container.textContent);
                    return (wantedId && (id === wantedId || element.getAttribute('data-message-id') === wantedId))
                        || (!wantedId && wantedText && (text === wantedText || text.includes(wantedText)));
                }});
                if (!target) return {{ found: false, id: null }};
                const container = target.closest('[id^="chat-messages-"], [data-message-id]') || target;
                document.querySelectorAll('[data-discord-mcp-delete-target]').forEach(element => element.removeAttribute('data-discord-mcp-delete-target'));
                const hoverTarget = container.querySelector('[id^="message-content-"], [class*="markup" i], [class*="messageContent" i]') || container;
                hoverTarget.setAttribute('data-discord-mcp-delete-target', 'true');
                container.scrollIntoView({{ block: 'center' }});
                return {{ found: true, id: container.getAttribute('data-message-id') || container.id || null }};
            }}"#,
            id_json = id_json,
            text_json = text_json,
        ))
        .await?
        .into_value()?;
    if !state
        .get("found")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
    {
        bail!("Could not find the requested visible message to delete");
    }

    page.evaluate(
        r#"() => {
            const target = document.querySelector('[data-discord-mcp-delete-target]');
            if (!target) return false;
            const container = target.closest('[id^="chat-messages-"], [data-message-id]') || target;
            const rect = target.getBoundingClientRect();
            const eventOptions = {
                bubbles: true,
                cancelable: true,
                view: window,
                clientX: rect.left + Math.min(16, rect.width / 2),
                clientY: rect.top + Math.min(10, rect.height / 2),
                relatedTarget: null,
                shiftKey: true
            };
            for (const element of [container, target]) {
                for (const type of ['pointerover', 'mouseover', 'mouseenter']) {
                    element.dispatchEvent(new MouseEvent(type, eventOptions));
                }
            }
            return true;
        }"#,
    )
    .await?
    .into_value::<bool>()?;
    sleep(Duration::from_millis(500)).await;

    let more_clicked: bool = page
        .evaluate(
            r#"() => {
                const target = document.querySelector('[data-discord-mcp-delete-target]');
                if (!target) return false;
                const container = target.closest('[id^="chat-messages-"], [data-message-id]') || target;
                const visible = element => {
                    const style = getComputedStyle(element);
                    return style.display !== 'none' && style.visibility !== 'hidden' && !element.disabled;
                };
                const label = element => (
                    element.getAttribute('aria-label')
                    || element.getAttribute('title')
                    || element.getAttribute('data-testid')
                    || element.innerText
                    || element.textContent
                    || ''
                ).replace(/\s+/g, ' ').trim();
                const button = Array.from(container.querySelectorAll('button, [role="button"]'))
                    .filter(visible)
                    .find(element => /^(more|more options|message actions)$/i.test(label(element)) || /more/i.test(label(element)));
                if (!button) return false;
                button.click();
                return true;
            }"#,
        )
        .await?
        .into_value()?;
    if !more_clicked {
        bail!("Discord did not expose a visible More actions control for that message");
    }
    sleep(Duration::from_millis(350)).await;

    let delete_clicked: bool = page
        .evaluate(
            r#"() => {
                const visible = element => {
                    const style = getComputedStyle(element);
                    return style.display !== 'none' && style.visibility !== 'hidden' && !element.disabled;
                };
                const label = element => (
                    element.getAttribute('aria-label')
                    || element.getAttribute('title')
                    || element.innerText
                    || element.textContent
                    || ''
                ).replace(/\s+/g, ' ').trim();
                const menu = Array.from(document.querySelectorAll('[role="menu"]')).filter(visible).at(-1);
                const scope = menu || document;
                const item = Array.from(scope.querySelectorAll('button, [role="menuitem"], [role="option"], [data-menu-item]'))
                    .filter(visible)
                    .find(element => /^delete(?: message)?$/i.test(label(element)));
                if (!item) return false;
                item.click();
                return true;
            }"#,
        )
        .await?
        .into_value()?;
    if !delete_clicked {
        bail!("Discord did not expose a visible Delete message action");
    }
    sleep(Duration::from_millis(350)).await;

    let confirmation_clicked: bool = page
        .evaluate(
            r#"() => {
                const visible = element => {
                    const style = getComputedStyle(element);
                    return style.display !== 'none' && style.visibility !== 'hidden' && !element.disabled;
                };
                const label = element => (
                    element.getAttribute('aria-label')
                    || element.getAttribute('title')
                    || element.innerText
                    || element.textContent
                    || ''
                ).replace(/\s+/g, ' ').trim();
                const dialogs = Array.from(document.querySelectorAll('[role="dialog"]')).filter(visible);
                const dialog = dialogs.at(-1);
                if (!dialog) return false;
                const button = Array.from(dialog.querySelectorAll('button, [role="button"]'))
                    .filter(visible)
                    .find(element => /^delete$/i.test(label(element)));
                if (!button) return false;
                button.click();
                return true;
            }"#,
        )
        .await?
        .into_value()?;
    if !confirmation_clicked {
        bail!("Discord did not expose the Delete confirmation control");
    }
    sleep(Duration::from_millis(700)).await;

    Ok(state
        .get("id")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .or(message_id)
        .or(message_text)
        .unwrap_or("visible message")
        .to_string())
}

pub async fn delete_message(
    channel_name: &str,
    channel_id: Option<&str>,
    server_name: Option<&str>,
    message_id: Option<&str>,
    message_text: Option<&str>,
    confirm: bool,
) -> Result<DeleteMessageResult> {
    let channel_name = channel_name.trim();
    if channel_name.is_empty() {
        bail!("channel_name is required");
    }
    let target = message_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .or_else(|| {
            message_text
                .map(str::trim)
                .filter(|value| !value.is_empty())
        })
        .context("message_id or message_text is required to delete")?;
    if !confirm {
        return Ok(DeleteMessageResult {
            channel: channel_name.to_string(),
            message: target.to_string(),
            deleted: false,
            confirmation_required: true,
            page_url: discord_url(),
            warnings: vec![
                "Preview only: pass confirm=true to open the channel, select the message, and delete it."
                    .to_string(),
            ],
        });
    }

    let session = prepared_page().await?;
    let result = async {
        select_channel(&session.page, channel_name, channel_id, server_name).await?;
        let deleted_message = start_delete(&session.page, message_id, message_text).await?;
        Ok::<DeleteMessageResult, anyhow::Error>(DeleteMessageResult {
            channel: channel_name.to_string(),
            message: deleted_message,
            deleted: true,
            confirmation_required: false,
            page_url: session.page.url().await?.unwrap_or_default(),
            warnings: vec![
                "Discord UI deletion was confirmed; verify the message list if the channel is slow to update."
                    .to_string(),
            ],
        })
    }
    .await;
    finish(session).await?;
    result
}

pub async fn login() -> Result<()> {
    let profile = profile_dir()?;
    println!("Opening Discord in a dedicated browser profile.");
    println!("Profile: {}", profile.display());
    println!(
        "Complete the normal Discord login in the browser window; no credentials are read by this CLI."
    );

    let session = open_discord(true).await?;
    let result = timeout(LOGIN_WAIT, async {
        loop {
            let snapshot = page_snapshot(&session.page).await?;
            if snapshot.authenticated {
                return Ok::<PageSnapshot, anyhow::Error>(snapshot);
            }
            sleep(Duration::from_secs(2)).await;
        }
    })
    .await;
    let output = match result {
        Ok(Ok(snapshot)) => {
            println!("Discord login detected.");
            print_json(&snapshot)
        }
        Ok(Err(error)) => Err(error),
        Err(_) => bail!("timed out waiting for Discord login; the browser profile was preserved"),
    };
    finish(session).await?;
    output
}

pub async fn status() -> Result<()> {
    print_json(&visible_snapshot().await?)
}

pub async fn inspect_value() -> Result<serde_json::Value> {
    let session = open_discord(false).await?;
    let result: Result<serde_json::Value> = async {
        let snapshot = wait_for_discord(&session.page).await?;
        let diagnostics: serde_json::Value = session
            .page
            .evaluate(
                r#"() => ({
                    html_chars: document.documentElement?.outerHTML?.length || 0,
                    body_text: (document.body?.innerText || '').slice(0, 20000),
                    iframes: Array.from(document.querySelectorAll('iframe')).map(frame => ({src: frame.src || null, title: frame.title || null})),
                    elements: Array.from(document.querySelectorAll('[role], [aria-label], [data-list-id], [data-list-item-id], [id^="chat-messages-"]')).slice(0, 600).map(element => ({
                        tag: element.tagName.toLowerCase(),
                        role: element.getAttribute('role'),
                        aria_label: element.getAttribute('aria-label'),
                        data_list_id: element.getAttribute('data-list-id'),
                        data_list_item_id: element.getAttribute('data-list-item-id'),
                        id: element.id || null,
                        text: (element.innerText || element.textContent || '').replace(/\s+/g, ' ').trim().slice(0, 240)
                    }))
                })"#,
            )
            .await?
            .into_value()?;
        Ok(serde_json::json!({ "snapshot": snapshot, "dom": diagnostics }))
    }
    .await;
    finish(session).await?;
    result
}

pub async fn inspect() -> Result<()> {
    print_json(&inspect_value().await?)
}

pub fn logout() -> Result<()> {
    let profile = profile_dir()?;
    if !profile.exists() {
        println!("No local Discord browser profile exists.");
        return Ok(());
    }
    print!(
        "Remove the local Discord browser profile at {}? [y/N] ",
        profile.display()
    );
    io::stdout().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    if answer.trim().eq_ignore_ascii_case("y") {
        std::fs::remove_dir_all(&profile)?;
        println!("Local Discord browser profile removed.");
    } else {
        println!("Kept the local Discord browser profile.");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{is_authenticated_snapshot, is_authenticated_url, validate_message};

    #[test]
    fn authentication_requires_a_rendered_app() {
        assert!(!is_authenticated_snapshot(
            "https://discord.com/app",
            false,
            false,
            ""
        ));
        assert!(!is_authenticated_snapshot(
            "https://discord.com/login",
            true,
            true,
            "Welcome back!"
        ));
        assert!(!is_authenticated_snapshot(
            "https://discord.com/app",
            true,
            false,
            "Loading\nConnection problems?"
        ));
        assert!(is_authenticated_snapshot(
            "https://discord.com/channels/123/456",
            true,
            false,
            "General"
        ));
    }

    #[test]
    fn authentication_requires_an_exact_discord_host() {
        assert!(!is_authenticated_url(
            "https://discord.com.evil.example/app"
        ));
        assert!(is_authenticated_url("https://discord.com/app"));
    }

    #[test]
    fn message_validation_accepts_unicode_and_newlines() {
        assert!(validate_message("når, så, på 😀\nsecond line").is_ok());
    }

    #[test]
    fn message_validation_rejects_empty_text() {
        assert!(validate_message("   ").is_err());
    }
}
