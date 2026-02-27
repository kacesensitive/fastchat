use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct AppConfig {
    pub last_channel: Option<String>,
    pub global_filters: GlobalFilterConfig,
    pub ui: UiConfig,
    pub window: WindowConfig,
    pub popout_window: WindowConfig,
    pub auto_reconnect_last_channel: bool,
    pub auth_mode: AuthMode,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            last_channel: None,
            global_filters: GlobalFilterConfig::default(),
            ui: UiConfig::default(),
            window: WindowConfig::default(),
            popout_window: WindowConfig::default_popout(),
            auto_reconnect_last_channel: true,
            auth_mode: AuthMode::Anonymous,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum AuthMode {
    Anonymous,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct WindowConfig {
    pub width: f32,
    pub height: f32,
    pub pos_x: Option<f32>,
    pub pos_y: Option<f32>,
    pub maximized: bool,
}

impl Default for WindowConfig {
    fn default() -> Self {
        Self {
            width: 1400.0,
            height: 900.0,
            pos_x: None,
            pos_y: None,
            maximized: false,
        }
    }
}

impl WindowConfig {
    pub fn default_popout() -> Self {
        Self {
            width: 920.0,
            height: 720.0,
            pos_x: None,
            pos_y: None,
            maximized: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct UiConfig {
    pub show_perf_overlay: bool,
    pub filters_panel_open: bool,
    pub enable_message_animations: bool,
    pub show_badges: bool,
    pub chat_background_color: RgbColor,
    pub chat_text_color: RgbColor,
    pub show_per_user_name_colors: bool,
    pub fallback_user_name_color: RgbColor,
    pub chat_font_size: u16,
    pub popout_chat_font_size: u16,
    pub chat_font_family: ChatFontFamily,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            show_perf_overlay: false,
            filters_panel_open: true,
            enable_message_animations: true,
            show_badges: true,
            chat_background_color: RgbColor::from_rgb(0x15, 0x15, 0x18),
            chat_text_color: RgbColor::from_rgb(0xF2, 0xF2, 0xF2),
            show_per_user_name_colors: true,
            fallback_user_name_color: RgbColor::from_rgb(0xD8, 0xD8, 0xD8),
            chat_font_size: 15,
            popout_chat_font_size: 28,
            chat_font_family: ChatFontFamily::Proportional,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ChatFontFamily {
    Proportional,
    Monospace,
    DejaVuSans,
    DejaVuSerif,
    DejaVuSansMono,
    LiberationSans,
    LiberationSerif,
    LiberationMono,
    NotoSans,
    NotoSerif,
    NotoSansMono,
    JetBrainsMono,
    FiraCode,
    Menlo,
    SegoeUi,
    Consolas,
    Georgia,
    Arial,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct GlobalFilterConfig {
    pub include_terms: Vec<String>,
    pub exclude_terms: Vec<String>,
    pub highlight_terms: Vec<String>,
    pub hidden_users: Vec<String>,
    pub hidden_badge_types: Vec<String>,
    pub min_message_len: u16,
    pub visibility: MessageVisibilityToggles,
}

impl Default for GlobalFilterConfig {
    fn default() -> Self {
        Self {
            include_terms: Vec::new(),
            exclude_terms: Vec::new(),
            highlight_terms: Vec::new(),
            hidden_users: Vec::new(),
            hidden_badge_types: Vec::new(),
            min_message_len: 0,
            visibility: MessageVisibilityToggles::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct MessageVisibilityToggles {
    pub show_mod_messages: bool,
    pub show_vip_messages: bool,
    pub show_subscriber_messages: bool,
    pub show_non_subscriber_messages: bool,
    pub show_cheers: bool,
    pub show_redeems: bool,
    pub show_system_notices: bool,
}

impl Default for MessageVisibilityToggles {
    fn default() -> Self {
        Self {
            show_mod_messages: true,
            show_vip_messages: true,
            show_subscriber_messages: true,
            show_non_subscriber_messages: true,
            show_cheers: true,
            show_redeems: true,
            show_system_notices: true,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct RgbColor {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl RgbColor {
    pub const fn from_rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BadgeTag {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum MessageFragment {
    Text(String),
    Emote {
        emote_id: String,
        code: String,
        animated_preferred: bool,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum MessageKind {
    Chat,
    Action,
    Notice,
    UserNotice,
    ClearChat,
    ClearMsg,
    System,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct MessageFlags {
    pub is_mod: bool,
    pub is_vip: bool,
    pub is_subscriber: bool,
    pub has_bits: bool,
    pub is_redeem: bool,
    pub is_system_notice: bool,
    pub is_deleted: bool,
    pub is_action: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChatMessage {
    pub id: String,
    pub timestamp: DateTime<Utc>,
    pub channel_login: String,
    pub channel_id: Option<String>,
    pub sender_login: String,
    pub display_name: String,
    pub name_color: Option<RgbColor>,
    pub badges: Vec<BadgeTag>,
    pub fragments: Vec<MessageFragment>,
    pub raw_text: String,
    pub kind: MessageKind,
    pub flags: MessageFlags,
}

impl ChatMessage {
    pub fn new_text(
        channel_login: impl Into<String>,
        sender_login: impl Into<String>,
        display_name: impl Into<String>,
        text: impl Into<String>,
        kind: MessageKind,
    ) -> Self {
        let raw_text = text.into();
        Self {
            id: Uuid::new_v4().to_string(),
            timestamp: Utc::now(),
            channel_login: channel_login.into(),
            channel_id: None,
            sender_login: sender_login.into(),
            display_name: display_name.into(),
            name_color: None,
            badges: Vec::new(),
            fragments: vec![MessageFragment::Text(raw_text.clone())],
            raw_text,
            kind,
            flags: MessageFlags {
                is_system_notice: matches!(kind, MessageKind::Notice | MessageKind::UserNotice | MessageKind::ClearChat | MessageKind::ClearMsg | MessageKind::System),
                ..Default::default()
            },
        }
    }

    pub fn canonical_text_lowercase(&self) -> String {
        self.raw_text.to_lowercase()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ChatEvent {
    Message(ChatMessage),
    ConnectionState(ConnectionState),
    Info { channel: Option<String>, text: String },
    Error { channel: Option<String>, text: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ConnectionState {
    Disconnected,
    Connecting { channel: String },
    Connected { channel: String },
    Reconnecting { channel: String, attempt: u32 },
    Error { channel: Option<String>, message: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FilterDecision {
    pub visible: bool,
    pub highlighted: bool,
    pub drop_reason: Option<FilterDropReason>,
}

impl Default for FilterDecision {
    fn default() -> Self {
        Self {
            visible: true,
            highlighted: false,
            drop_reason: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum FilterDropReason {
    HiddenUser,
    BadgeTypeHidden,
    TooShort,
    ExcludedKeyword,
    MissingIncludedKeyword,
    ModHidden,
    VipHidden,
    SubscriberHidden,
    NonSubscriberHidden,
    BitsHidden,
    RedeemHidden,
    SystemHidden,
}
