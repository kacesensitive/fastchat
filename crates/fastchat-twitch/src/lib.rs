use std::time::Duration;

use anyhow::Result;
use fastchat_core::{
    BadgeTag, ChatEvent, ChatMessage, ConnectionState, MessageFlags, MessageFragment, MessageKind,
    RgbColor,
};
use tokio::{runtime::Handle, sync::{mpsc, oneshot}, task::JoinHandle};
use tracing::{debug, info, warn};
use twitch_irc::{
    login::StaticLoginCredentials,
    message::{
        Badge, ClearChatAction, ClearChatMessage, ClearMsgMessage, NoticeMessage, PrivmsgMessage,
        ServerMessage, UserNoticeMessage,
    },
    ClientConfig, SecureTCPTransport, TwitchIRCClient,
};

pub trait TwitchChatClient: Send {
    fn connect(&mut self, channel: String, events_tx: mpsc::UnboundedSender<ChatEvent>) -> Result<()>;
    fn disconnect(&mut self);
    fn current_channel(&self) -> Option<&str>;
}

#[derive(Debug, Clone, Copy)]
pub struct ReconnectPolicy {
    pub base_delay: Duration,
    pub max_delay: Duration,
    pub jitter: Duration,
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self {
            base_delay: Duration::from_millis(750),
            max_delay: Duration::from_secs(15),
            jitter: Duration::from_millis(400),
        }
    }
}

impl ReconnectPolicy {
    pub fn backoff_for_attempt(&self, attempt: u32) -> Duration {
        let exp = 2u64.saturating_pow(attempt.min(6));
        let base_ms = self.base_delay.as_millis() as u64;
        let max_ms = self.max_delay.as_millis() as u64;
        let jitter_ms = self.jitter.as_millis() as u64;
        let deterministic_jitter = if jitter_ms == 0 {
            0
        } else {
            (attempt as u64 * 137) % (jitter_ms + 1)
        };
        Duration::from_millis((base_ms.saturating_mul(exp)).min(max_ms).saturating_add(deterministic_jitter))
    }
}

pub struct AnonymousTwitchChatClient {
    runtime: Handle,
    reconnect_policy: ReconnectPolicy,
    task: Option<JoinHandle<()>>,
    shutdown_tx: Option<oneshot::Sender<()>>,
    current_channel: Option<String>,
}

impl AnonymousTwitchChatClient {
    pub fn new(runtime: Handle) -> Self {
        Self {
            runtime,
            reconnect_policy: ReconnectPolicy::default(),
            task: None,
            shutdown_tx: None,
            current_channel: None,
        }
    }

    pub fn with_reconnect_policy(mut self, policy: ReconnectPolicy) -> Self {
        self.reconnect_policy = policy;
        self
    }
}

impl TwitchChatClient for AnonymousTwitchChatClient {
    fn connect(&mut self, channel: String, events_tx: mpsc::UnboundedSender<ChatEvent>) -> Result<()> {
        self.disconnect();

        let normalized_channel = normalize_channel_login(&channel)?;
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let policy = self.reconnect_policy;
        let task_channel = normalized_channel.clone();

        let task = self.runtime.spawn(async move {
            run_connection_loop(task_channel, events_tx, policy, shutdown_rx).await;
        });

        self.shutdown_tx = Some(shutdown_tx);
        self.task = Some(task);
        self.current_channel = Some(normalized_channel);
        Ok(())
    }

    fn disconnect(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
        self.current_channel = None;
    }

    fn current_channel(&self) -> Option<&str> {
        self.current_channel.as_deref()
    }
}

impl Drop for AnonymousTwitchChatClient {
    fn drop(&mut self) {
        self.disconnect();
    }
}

async fn run_connection_loop(
    channel: String,
    events_tx: mpsc::UnboundedSender<ChatEvent>,
    reconnect_policy: ReconnectPolicy,
    mut shutdown_rx: oneshot::Receiver<()>,
) {
    let normalizer = TwitchNormalizer;
    let mut attempt: u32 = 0;

    loop {
        let state = if attempt == 0 {
            ConnectionState::Connecting {
                channel: channel.clone(),
            }
        } else {
            ConnectionState::Reconnecting {
                channel: channel.clone(),
                attempt,
            }
        };
        let _ = events_tx.send(ChatEvent::ConnectionState(state));

        let config = ClientConfig::default();
        let (mut incoming_messages, client) =
            TwitchIRCClient::<SecureTCPTransport, StaticLoginCredentials>::new(config);

        match client.join(channel.clone()) {
            Ok(()) => {
                let _ = events_tx.send(ChatEvent::ConnectionState(ConnectionState::Connected {
                    channel: channel.clone(),
                }));
            }
            Err(err) => {
                let _ = events_tx.send(ChatEvent::ConnectionState(ConnectionState::Error {
                    channel: Some(channel.clone()),
                    message: format!("failed to join channel: {err}"),
                }));
                break;
            }
        }

        info!(%channel, attempt, "twitch connection loop started");
        loop {
            tokio::select! {
                _ = &mut shutdown_rx => {
                    debug!(%channel, "twitch connection shutdown requested");
                    let _ = events_tx.send(ChatEvent::ConnectionState(ConnectionState::Disconnected));
                    return;
                }
                maybe_msg = incoming_messages.recv() => {
                    match maybe_msg {
                        Some(server_msg) => {
                            if let Some(event) = normalizer.normalize(server_msg) {
                                let _ = events_tx.send(event);
                            }
                        }
                        None => {
                            break;
                        }
                    }
                }
            }
        }

        attempt = attempt.saturating_add(1);
        let delay = reconnect_policy.backoff_for_attempt(attempt);
        warn!(%channel, attempt, ?delay, "twitch stream ended, reconnecting");
        tokio::select! {
            _ = &mut shutdown_rx => {
                let _ = events_tx.send(ChatEvent::ConnectionState(ConnectionState::Disconnected));
                return;
            }
            _ = tokio::time::sleep(delay) => {}
        }
    }

    let _ = events_tx.send(ChatEvent::ConnectionState(ConnectionState::Disconnected));
}

fn normalize_channel_login(input: &str) -> Result<String> {
    let trimmed = input.trim().trim_start_matches('#').to_lowercase();
    if trimmed.is_empty() {
        anyhow::bail!("channel username is required");
    }
    if !trimmed.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        anyhow::bail!("channel username contains invalid characters");
    }
    Ok(trimmed)
}

#[derive(Debug, Default, Clone, Copy)]
pub struct TwitchNormalizer;

impl TwitchNormalizer {
    pub fn normalize(&self, message: ServerMessage) -> Option<ChatEvent> {
        match message {
            ServerMessage::Privmsg(msg) => Some(ChatEvent::Message(self.privmsg_to_chat(msg))),
            ServerMessage::UserNotice(msg) => Some(ChatEvent::Message(self.usernotice_to_chat(msg))),
            ServerMessage::Notice(msg) => Some(ChatEvent::Message(self.notice_to_chat(msg))),
            ServerMessage::ClearChat(msg) => Some(ChatEvent::Message(self.clearchat_to_chat(msg))),
            ServerMessage::ClearMsg(msg) => Some(ChatEvent::Message(self.clearmsg_to_chat(msg))),
            ServerMessage::Reconnect(_) => Some(ChatEvent::Info {
                channel: None,
                text: "Twitch requested reconnect".to_owned(),
            }),
            _ => None,
        }
    }

    fn privmsg_to_chat(&self, msg: PrivmsgMessage) -> ChatMessage {
        let mut flags = flags_from_badges(&msg.badges);
        flags.has_bits = msg.bits.is_some();
        flags.is_action = msg.is_action;

        ChatMessage {
            id: msg.message_id,
            timestamp: msg.server_timestamp,
            channel_login: msg.channel_login,
            channel_id: Some(msg.channel_id),
            sender_login: msg.sender.login,
            display_name: msg.sender.name,
            name_color: msg.name_color.map(|c| RgbColor { r: c.r, g: c.g, b: c.b }),
            badges: badges_to_tags(&msg.badges),
            fragments: build_fragments(&msg.message_text, &msg.emotes),
            raw_text: msg.message_text,
            kind: if msg.is_action { MessageKind::Action } else { MessageKind::Chat },
            flags,
        }
    }

    fn usernotice_to_chat(&self, msg: UserNoticeMessage) -> ChatMessage {
        let mut flags = flags_from_badges(&msg.badges);
        flags.is_system_notice = true;
        let event_id_lower = msg.event_id.to_lowercase();
        if event_id_lower.contains("sub") {
            flags.is_subscriber = true;
        }
        if event_id_lower.contains("bits") {
            flags.has_bits = true;
        }
        if event_id_lower.contains("reward") || event_id_lower.contains("redeem") {
            flags.is_redeem = true;
        }

        let mut raw_text = msg.system_message.clone();
        if let Some(user_text) = msg.message_text.as_ref() {
            if !user_text.is_empty() {
                raw_text.push_str(" | ");
                raw_text.push_str(user_text);
            }
        }

        ChatMessage {
            id: msg.message_id,
            timestamp: msg.server_timestamp,
            channel_login: msg.channel_login,
            channel_id: Some(msg.channel_id),
            sender_login: msg.sender.login,
            display_name: msg.sender.name,
            name_color: msg.name_color.map(|c| RgbColor { r: c.r, g: c.g, b: c.b }),
            badges: badges_to_tags(&msg.badges),
            fragments: vec![MessageFragment::Text(raw_text.clone())],
            raw_text,
            kind: MessageKind::UserNotice,
            flags,
        }
    }

    fn notice_to_chat(&self, msg: NoticeMessage) -> ChatMessage {
        let channel_login = msg.channel_login.unwrap_or_else(|| "twitch".to_owned());
        let mut message = ChatMessage::new_text(channel_login, "twitch", "Twitch", msg.message_text, MessageKind::Notice);
        message.flags.is_system_notice = true;
        message
    }

    fn clearchat_to_chat(&self, msg: ClearChatMessage) -> ChatMessage {
        let text = match msg.action {
            ClearChatAction::ChatCleared => "Chat was cleared".to_owned(),
            ClearChatAction::UserBanned { user_login, .. } => format!("{user_login} was banned"),
            ClearChatAction::UserTimedOut { user_login, timeout_length, .. } => {
                format!("{user_login} timed out for {}s", timeout_length.as_secs())
            }
        };
        let mut message = ChatMessage::new_text(msg.channel_login, "twitch", "Twitch", text, MessageKind::ClearChat);
        message.channel_id = Some(msg.channel_id);
        message.timestamp = msg.server_timestamp;
        message.flags.is_system_notice = true;
        message
    }

    fn clearmsg_to_chat(&self, msg: ClearMsgMessage) -> ChatMessage {
        let text = format!("Deleted message from {}: {}", msg.sender_login, msg.message_text);
        let mut message = ChatMessage::new_text(msg.channel_login, "twitch", "Twitch", text, MessageKind::ClearMsg);
        message.timestamp = msg.server_timestamp;
        message.flags.is_system_notice = true;
        message
    }
}

fn badges_to_tags(badges: &[Badge]) -> Vec<BadgeTag> {
    badges
        .iter()
        .map(|b| BadgeTag {
            name: b.name.clone(),
            version: b.version.clone(),
        })
        .collect()
}

fn flags_from_badges(badges: &[Badge]) -> MessageFlags {
    let mut flags = MessageFlags::default();
    for badge in badges {
        match badge.name.as_str() {
            "moderator" => flags.is_mod = true,
            "vip" => flags.is_vip = true,
            "subscriber" => flags.is_subscriber = true,
            "bits" => flags.has_bits = true,
            _ => {}
        }
    }
    flags
}

fn build_fragments(
    raw_text: &str,
    emotes: &[twitch_irc::message::Emote],
) -> Vec<MessageFragment> {
    if emotes.is_empty() {
        return vec![MessageFragment::Text(raw_text.to_owned())];
    }

    let chars: Vec<char> = raw_text.chars().collect();
    let mut sorted = emotes.to_vec();
    sorted.sort_by_key(|e| e.char_range.start);

    let mut fragments = Vec::new();
    let mut cursor = 0usize;

    for emote in sorted {
        let start = emote.char_range.start.min(chars.len());
        let end = emote.char_range.end.min(chars.len());
        if start > cursor {
            let chunk: String = chars[cursor..start].iter().collect();
            if !chunk.is_empty() {
                fragments.push(MessageFragment::Text(chunk));
            }
        }
        fragments.push(MessageFragment::Emote {
            emote_id: emote.id,
            code: emote.code,
            animated_preferred: true,
        });
        cursor = cursor.max(end);
    }

    if cursor < chars.len() {
        let chunk: String = chars[cursor..].iter().collect();
        if !chunk.is_empty() {
            fragments.push(MessageFragment::Text(chunk));
        }
    }

    if fragments.is_empty() {
        fragments.push(MessageFragment::Text(raw_text.to_owned()));
    }

    fragments
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BadgePresentation {
    IconUrl { url: String },
    TextPill { label: String, color: RgbColor },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmoteAssetUrls {
    pub static_url: String,
    pub animated_url: Option<String>,
}

pub trait AssetResolver: Send + Sync {
    fn resolve_emote_urls(&self, emote_id: &str) -> EmoteAssetUrls;
    fn resolve_badges(&self, badges: &[BadgeTag]) -> Vec<BadgePresentation>;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct TwitchCdnAssetResolver;

impl AssetResolver for TwitchCdnAssetResolver {
    fn resolve_emote_urls(&self, emote_id: &str) -> EmoteAssetUrls {
        let base = format!("https://static-cdn.jtvnw.net/emoticons/v2/{emote_id}/default/dark");
        EmoteAssetUrls {
            static_url: format!("{base}/3.0"),
            animated_url: Some(format!("{base}/animated/3.0")),
        }
    }

    fn resolve_badges(&self, badges: &[BadgeTag]) -> Vec<BadgePresentation> {
        badges.iter().map(text_pill_badge).collect()
    }
}

fn text_pill_badge(badge: &BadgeTag) -> BadgePresentation {
    let (label, color) = match badge.name.as_str() {
        "moderator" => ("MOD", RgbColor::from_rgb(0x00, 0xAD, 0x03)),
        "vip" => ("VIP", RgbColor::from_rgb(0xD2, 0x69, 0xFF)),
        "subscriber" => ("SUB", RgbColor::from_rgb(0x1F, 0xB9, 0x4F)),
        "bits" => ("BITS", RgbColor::from_rgb(0x91, 0x4B, 0xFF)),
        "broadcaster" => ("LIVE", RgbColor::from_rgb(0xE9, 0x19, 0x16)),
        _ => (badge.name.as_str(), RgbColor::from_rgb(0x55, 0x55, 0x55)),
    };
    BadgePresentation::TextPill {
        label: label.to_owned(),
        color,
    }
}

#[cfg(test)]
mod tests {
    use super::{normalize_channel_login, TwitchCdnAssetResolver, TwitchNormalizer};
    use crate::AssetResolver;

    #[test]
    fn normalizes_channel_name() {
        assert_eq!(normalize_channel_login("#SodaPoppin").unwrap(), "sodapoppin");
        assert!(normalize_channel_login("bad-name!").is_err());
    }

    #[test]
    fn emote_urls_include_animated_variant() {
        let resolver = TwitchCdnAssetResolver;
        let urls = resolver.resolve_emote_urls("25");
        assert!(urls.static_url.contains("/25/"));
        assert!(urls.animated_url.unwrap().contains("animated"));
    }

    #[test]
    fn normalizer_exists() {
        let _ = TwitchNormalizer;
    }
}
