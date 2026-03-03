use std::{
    collections::{BTreeSet, HashMap, HashSet, VecDeque},
    fs,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use eframe::egui::{self, Align, Color32, RichText};
use fastchat_core::{
    AppConfig, AppPaths, BacklogRecord, BacklogRetention, BacklogWriter, ChatEvent, ChatFontFamily,
    ChatMessage, ChatStore, ChatStoreStats, ConfigRepository, ConnectionState, FilterEngine,
    GlobalFilterConfig, MessageFragment, RgbColor, StoredChatEntry, UiConfig, WindowConfig,
};
use fastchat_twitch::{
    AnonymousTwitchChatClient, AssetResolver, BadgePresentation, TwitchCdnAssetResolver,
    TwitchChatClient,
};
use lru::LruCache;
use parking_lot::Mutex;
use reqwest::StatusCode;
use serde::Deserialize;
use tokio::{
    runtime::{Handle, Runtime},
    sync::mpsc,
};
use tracing::{error, warn};

pub struct FastChatApp {
    runtime: Runtime,
    twitch_client: AnonymousTwitchChatClient,
    events_tx: mpsc::UnboundedSender<ChatEvent>,
    events_rx: mpsc::UnboundedReceiver<ChatEvent>,
    paths: AppPaths,
    config_repo: ConfigRepository,
    config: AppConfig,
    channel_input: String,
    filter_fields: FilterTextFields,
    filter_engine: FilterEngine,
    chat_store: ChatStore,
    backlog_writer: BacklogWriter,
    asset_cache: AssetCacheHandle,
    row_layout_cache: RowLayoutCache,
    popout_row_layout_cache: RowLayoutCache,
    perf_overlay: PerfOverlayState,
    connection_state: ConnectionState,
    ui_snapshot: UiSnapshot,
    first_frame_auto_connect: bool,
    pending_config_save_since: Option<Instant>,
    last_config_save_error: Option<String>,
    status_message: Option<String>,
    visible_stick_to_bottom: bool,
    jump_to_latest_requested: bool,
    focused_sender_login: Option<String>,
    chat_scroll: ChatScrollState,
    popout_window_open: bool,
    popout_selected_message_id: Option<String>,
    popout_custom_message_input: String,
    popout_custom_message_text: String,
    popout_custom_message_visible: bool,
    available_chat_fonts: Vec<ChatFontFamily>,
    observed_badge_types: BTreeSet<String>,
    events_processed_total: u64,
}

#[derive(Debug, Clone, Default)]
pub struct UiSnapshot {
    pub connection_state: Option<ConnectionState>,
    pub store_stats: ChatStoreStats,
    pub processed_events_total: u64,
    pub last_status: Option<String>,
}

#[derive(Debug, Clone, Copy)]
struct ChatScrollState {
    offset_y: f32,
    max_offset_y: f32,
    at_bottom: bool,
}

impl Default for ChatScrollState {
    fn default() -> Self {
        Self {
            offset_y: 0.0,
            max_offset_y: 0.0,
            at_bottom: true,
        }
    }
}

impl ChatScrollState {
    fn update(&mut self, offset_y: f32, max_offset_y: f32) {
        self.offset_y = offset_y.max(0.0);
        self.max_offset_y = max_offset_y.max(0.0);
        self.at_bottom = self.max_offset_y <= 1.0 || self.offset_y >= self.max_offset_y - 4.0;
    }

    fn should_show_jump_button(&self) -> bool {
        self.max_offset_y > 1.0 && !self.at_bottom
    }

    fn jump_target_offset(&self) -> f32 {
        // Overshoot a bit so egui clamps us to the latest content end.
        (self.max_offset_y + 10_000.0).max(0.0)
    }
}

#[derive(Debug, Clone, Copy)]
struct ChatTypography {
    size: f32,
    family: ChatFontFamily,
}

impl ChatTypography {
    fn from_ui_config(ui: &UiConfig) -> Self {
        Self {
            size: ui.chat_font_size.clamp(10, 36) as f32,
            family: ui.chat_font_family,
        }
    }

    fn from_popout_ui_config(ui: &UiConfig) -> Self {
        Self {
            size: ui.popout_chat_font_size.clamp(10, 240) as f32,
            family: ui.chat_font_family,
        }
    }

    fn egui_family(self) -> egui::FontFamily {
        match self.family {
            ChatFontFamily::Proportional => egui::FontFamily::Proportional,
            ChatFontFamily::Monospace => egui::FontFamily::Monospace,
            custom => egui::FontFamily::Name(custom_font_family_key(custom).into()),
        }
    }

    fn row_height(self) -> f32 {
        (self.emote_height().max(self.size * 1.18) + 4.0)
            .ceil()
            .max(20.0)
    }

    fn badge_text_size(self) -> f32 {
        (self.size * 0.78).max(10.0)
    }

    fn emote_height(self) -> f32 {
        (self.size * 1.35).max(18.0)
    }
}

#[derive(Debug, Clone, Copy)]
struct ChatAppearance {
    background_color: Color32,
    text_color: Color32,
    show_badges: bool,
    show_per_user_name_colors: bool,
    fallback_user_name_color: Color32,
}

impl ChatAppearance {
    fn from_ui_config(ui: &UiConfig) -> Self {
        Self {
            background_color: color32_from_rgb(ui.chat_background_color),
            text_color: color32_from_rgb(ui.chat_text_color),
            show_badges: ui.show_badges,
            show_per_user_name_colors: ui.show_per_user_name_colors,
            fallback_user_name_color: color32_from_rgb(ui.fallback_user_name_color),
        }
    }
}

#[derive(Clone, Copy)]
enum FontFallbackBase {
    Proportional,
    Monospace,
}

struct SystemFontCandidate {
    family: ChatFontFamily,
    #[allow(dead_code)]
    label: &'static str,
    fallback_base: FontFallbackBase,
    paths: &'static [&'static str],
    face_index: usize,
}

#[derive(Debug)]
pub struct RowLayoutCache {
    row_height_cache: LruCache<u64, f32>,
    average_row_height: f32,
}

impl RowLayoutCache {
    pub fn new(capacity: usize) -> Self {
        Self {
            row_height_cache: LruCache::new(std::num::NonZeroUsize::new(capacity.max(16)).unwrap()),
            average_row_height: 0.0,
        }
    }

    pub fn clear(&mut self) {
        self.row_height_cache.clear();
        self.average_row_height = 0.0;
    }

    pub fn note_row(&mut self, key: u64, height: f32) {
        self.row_height_cache.put(key, height);
        let clamped_height = height.max(1.0);
        if self.average_row_height <= 0.0 {
            self.average_row_height = clamped_height;
        } else {
            // Lightweight EMA keeps virtualization stable without rescanning the full buffer.
            self.average_row_height = self.average_row_height * 0.92 + clamped_height * 0.08;
        }
    }

    pub fn estimated_row_height(&self, key: u64, fallback: f32) -> f32 {
        self.row_height_cache
            .peek(&key)
            .copied()
            .unwrap_or(fallback)
    }

    pub fn average_row_height(&self, fallback: f32) -> f32 {
        if self.average_row_height > 0.0 {
            self.average_row_height
        } else {
            fallback
        }
    }
}

#[derive(Clone)]
pub struct AssetCacheHandle {
    inner: Arc<Mutex<AssetCacheInner>>,
}

struct AssetCacheInner {
    resolver: TwitchCdnAssetResolver,
    runtime: Handle,
    http_client: reqwest::Client,
    emote_url_cache: LruCache<String, fastchat_twitch::EmoteAssetUrls>,
    ready_images: LruCache<String, LoadedTexture>,
    pending_urls: HashSet<String>,
    failed_urls: HashSet<String>,
    fetch_results_tx: crossbeam_channel::Sender<AssetPipelineResult>,
    fetch_results_rx: crossbeam_channel::Receiver<AssetPipelineResult>,
    channel_icon_urls: HashMap<String, String>,
    pending_channel_icons: HashSet<String>,
    failed_channel_icons: HashSet<String>,
    channel_icon_results_tx: crossbeam_channel::Sender<ChannelIconResult>,
    channel_icon_results_rx: crossbeam_channel::Receiver<ChannelIconResult>,
    badge_global_index: Option<BadgeIconIndex>,
    badge_channel_indexes: HashMap<String, BadgeIconIndex>,
    pending_badge_global: bool,
    pending_badge_channels: HashSet<String>,
    failed_badge_global: bool,
    failed_badge_channels: HashSet<String>,
    badge_meta_results_tx: crossbeam_channel::Sender<BadgeMetadataResult>,
    badge_meta_results_rx: crossbeam_channel::Receiver<BadgeMetadataResult>,
}

#[derive(Clone)]
struct LoadedTexture {
    texture: egui::TextureHandle,
    size_px: [usize; 2],
}

enum ImageLookup {
    Ready(LoadedTexture),
    Pending,
    Failed,
}

struct AssetPipelineResult {
    url: String,
    decoded: Result<DecodedRgbaImage, String>,
}

struct DecodedRgbaImage {
    size: [usize; 2],
    rgba: Vec<u8>,
}

struct ChannelIconResult {
    channel_login: String,
    icon_url: Result<Option<String>, String>,
}

type BadgeIconIndex = HashMap<(String, String), String>;

enum BadgeMetadataScope {
    Global,
    Channel(String),
}

struct BadgeMetadataResult {
    scope: BadgeMetadataScope,
    decoded: Result<BadgeIconIndex, String>,
}

#[derive(Deserialize)]
struct TwitchBadgeResponse {
    #[serde(default)]
    badge_sets: HashMap<String, TwitchBadgeSet>,
}

#[derive(Deserialize)]
struct TwitchBadgeSet {
    #[serde(default)]
    versions: HashMap<String, TwitchBadgeVersion>,
}

#[derive(Deserialize)]
struct TwitchBadgeVersion {
    image_url_1x: Option<String>,
    image_url_2x: Option<String>,
    image_url_4x: Option<String>,
}

#[derive(Deserialize)]
struct IvrBadgeSet {
    set_id: String,
    #[serde(default)]
    versions: Vec<IvrBadgeVersion>,
}

#[derive(Deserialize)]
struct IvrBadgeVersion {
    id: String,
    image_url_1x: Option<String>,
    image_url_2x: Option<String>,
    image_url_4x: Option<String>,
}

#[derive(Deserialize)]
struct IvrUser {
    logo: Option<String>,
}

impl AssetCacheHandle {
    pub fn new(runtime: Handle) -> Self {
        let http_client = reqwest::Client::builder()
            .user_agent("fastchat/0.1")
            .pool_max_idle_per_host(8)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        let (fetch_results_tx, fetch_results_rx) = crossbeam_channel::unbounded();
        let (channel_icon_results_tx, channel_icon_results_rx) = crossbeam_channel::unbounded();
        let (badge_meta_results_tx, badge_meta_results_rx) = crossbeam_channel::unbounded();

        Self {
            inner: Arc::new(Mutex::new(AssetCacheInner {
                resolver: TwitchCdnAssetResolver,
                runtime,
                http_client,
                emote_url_cache: LruCache::new(std::num::NonZeroUsize::new(4096).unwrap()),
                ready_images: LruCache::new(std::num::NonZeroUsize::new(4096).unwrap()),
                pending_urls: HashSet::new(),
                failed_urls: HashSet::new(),
                fetch_results_tx,
                fetch_results_rx,
                channel_icon_urls: HashMap::new(),
                pending_channel_icons: HashSet::new(),
                failed_channel_icons: HashSet::new(),
                channel_icon_results_tx,
                channel_icon_results_rx,
                badge_global_index: None,
                badge_channel_indexes: HashMap::new(),
                pending_badge_global: false,
                pending_badge_channels: HashSet::new(),
                failed_badge_global: false,
                failed_badge_channels: HashSet::new(),
                badge_meta_results_tx,
                badge_meta_results_rx,
            })),
        }
    }

    pub fn emote_urls(&self, emote_id: &str) -> fastchat_twitch::EmoteAssetUrls {
        let mut inner = self.inner.lock();
        if let Some(found) = inner.emote_url_cache.get(emote_id).cloned() {
            return found;
        }
        let urls = inner.resolver.resolve_emote_urls(emote_id);
        inner.emote_url_cache.put(emote_id.to_owned(), urls.clone());
        urls
    }

    pub fn resolve_badges_for_message(&self, message: &ChatMessage) -> Vec<BadgePresentation> {
        self.ensure_badge_metadata_requested(message.channel_id.as_deref());

        let inner = self.inner.lock();
        let fallbacks = inner.resolver.resolve_badges(&message.badges);
        let global_index = inner.badge_global_index.as_ref();
        let channel_index = message
            .channel_id
            .as_deref()
            .and_then(|id| inner.badge_channel_indexes.get(id));

        message
            .badges
            .iter()
            .zip(fallbacks)
            .map(|(badge, fallback)| {
                let key = (badge.name.clone(), badge.version.clone());
                if let Some(url) = channel_index
                    .and_then(|index| index.get(&key))
                    .or_else(|| global_index.and_then(|index| index.get(&key)))
                {
                    BadgePresentation::IconUrl { url: url.clone() }
                } else {
                    fallback
                }
            })
            .collect()
    }

    pub fn render_channel_icon(
        &self,
        ui: &mut egui::Ui,
        channel_login: &str,
        target_height: f32,
    ) -> bool {
        match self.lookup_or_request_channel_icon(channel_login) {
            Some(url) => self.render_image_from_url(ui, &url, target_height, Some(channel_login)),
            None => false,
        }
    }

    pub fn pump_completed(&self, ctx: &egui::Context) {
        const MAX_BADGE_META_PER_FRAME: usize = 12;
        const MAX_CHANNEL_ICON_META_PER_FRAME: usize = 8;
        const MAX_IMAGE_UPLOADS_PER_FRAME: usize = 20;

        let mut pending_badge_meta_results = Vec::new();
        let mut pending_channel_icon_results = Vec::new();
        let mut pending_results = Vec::new();
        {
            let inner = self.inner.lock();
            while pending_badge_meta_results.len() < MAX_BADGE_META_PER_FRAME {
                let Ok(result) = inner.badge_meta_results_rx.try_recv() else {
                    break;
                };
                pending_badge_meta_results.push(result);
            }
            while pending_channel_icon_results.len() < MAX_CHANNEL_ICON_META_PER_FRAME {
                let Ok(result) = inner.channel_icon_results_rx.try_recv() else {
                    break;
                };
                pending_channel_icon_results.push(result);
            }
            while pending_results.len() < MAX_IMAGE_UPLOADS_PER_FRAME {
                let Ok(result) = inner.fetch_results_rx.try_recv() else {
                    break;
                };
                pending_results.push(result);
            }
        }

        if pending_badge_meta_results.is_empty()
            && pending_channel_icon_results.is_empty()
            && pending_results.is_empty()
        {
            return;
        }

        let mut inner = self.inner.lock();
        for result in pending_badge_meta_results {
            match result.scope {
                BadgeMetadataScope::Global => {
                    inner.pending_badge_global = false;
                    match result.decoded {
                        Ok(index) => {
                            inner.badge_global_index = Some(index);
                            inner.failed_badge_global = false;
                        }
                        Err(err) => {
                            inner.failed_badge_global = true;
                            warn!(error = %err, "global badge metadata fetch failed");
                        }
                    }
                }
                BadgeMetadataScope::Channel(channel_id) => {
                    inner.pending_badge_channels.remove(&channel_id);
                    match result.decoded {
                        Ok(index) => {
                            inner
                                .badge_channel_indexes
                                .insert(channel_id.clone(), index);
                            inner.failed_badge_channels.remove(&channel_id);
                        }
                        Err(err) => {
                            inner.failed_badge_channels.insert(channel_id.clone());
                            warn!(channel_id = %channel_id, error = %err, "channel badge metadata fetch failed");
                        }
                    }
                }
            }
        }
        for result in pending_channel_icon_results {
            inner.pending_channel_icons.remove(&result.channel_login);
            match result.icon_url {
                Ok(Some(url)) => {
                    inner
                        .channel_icon_urls
                        .insert(result.channel_login.clone(), url);
                    inner.failed_channel_icons.remove(&result.channel_login);
                }
                Ok(None) => {
                    inner.failed_channel_icons.insert(result.channel_login);
                }
                Err(err) => {
                    inner
                        .failed_channel_icons
                        .insert(result.channel_login.clone());
                    warn!(
                        channel = %result.channel_login,
                        error = %err,
                        "channel icon metadata fetch failed"
                    );
                }
            }
        }
        for result in pending_results {
            inner.pending_urls.remove(&result.url);
            match result.decoded {
                Ok(decoded) => {
                    let color_image =
                        egui::ColorImage::from_rgba_unmultiplied(decoded.size, &decoded.rgba);
                    let texture = ctx.load_texture(
                        format!("asset:{}", result.url),
                        color_image,
                        egui::TextureOptions::LINEAR,
                    );
                    inner.ready_images.put(
                        result.url.clone(),
                        LoadedTexture {
                            texture,
                            size_px: decoded.size,
                        },
                    );
                    inner.failed_urls.remove(&result.url);
                }
                Err(err) => {
                    inner.failed_urls.insert(result.url.clone());
                    warn!(url = %result.url, error = %err, "asset fetch/decode failed");
                }
            }
        }

        ctx.request_repaint();
    }

    fn render_badge_presentation(
        &self,
        ui: &mut egui::Ui,
        badge: &BadgePresentation,
        typography: ChatTypography,
    ) {
        match badge {
            BadgePresentation::IconUrl { url } => {
                let target_height = (typography.badge_text_size() + 4.0).max(14.0);
                if !self.render_image_from_url(ui, url, target_height, None) {
                    ui.label(RichText::new("[badge]").small().color(Color32::GRAY))
                        .on_hover_text(url);
                }
            }
            BadgePresentation::TextPill { label, color } => {
                ui.label(
                    RichText::new(label)
                        .size(typography.badge_text_size())
                        .family(typography.egui_family())
                        .background_color(color32_from_rgb(*color))
                        .color(Color32::WHITE),
                );
            }
        }
    }

    fn render_emote(
        &self,
        ui: &mut egui::Ui,
        emote_id: &str,
        code: &str,
        typography: ChatTypography,
        placeholder_text_color: Color32,
    ) {
        let urls = self.emote_urls(emote_id);
        let target_height = typography.emote_height();
        if self.render_image_from_url(ui, &urls.static_url, target_height, Some(code)) {
            return;
        }

        ui.label(
            RichText::new(format!(":{code}:"))
                .size(typography.size)
                .family(typography.egui_family())
                .color(placeholder_text_color),
        )
        .on_hover_text(urls.static_url);
    }

    fn ensure_badge_metadata_requested(&self, channel_id: Option<&str>) {
        self.ensure_badge_metadata_scope(BadgeMetadataScope::Global);
        if let Some(channel_id) = channel_id.filter(|id| !id.is_empty()) {
            self.ensure_badge_metadata_scope(BadgeMetadataScope::Channel(channel_id.to_owned()));
        }
    }

    fn ensure_badge_metadata_scope(&self, scope: BadgeMetadataScope) {
        let spawn_task = {
            let mut inner = self.inner.lock();
            match &scope {
                BadgeMetadataScope::Global => {
                    if inner.badge_global_index.is_some()
                        || inner.pending_badge_global
                        || inner.failed_badge_global
                    {
                        return;
                    }
                    inner.pending_badge_global = true;
                }
                BadgeMetadataScope::Channel(channel_id) => {
                    if inner.badge_channel_indexes.contains_key(channel_id)
                        || inner.pending_badge_channels.contains(channel_id)
                        || inner.failed_badge_channels.contains(channel_id)
                    {
                        return;
                    }
                    inner.pending_badge_channels.insert(channel_id.clone());
                }
            }
            Some((
                inner.runtime.clone(),
                inner.http_client.clone(),
                inner.badge_meta_results_tx.clone(),
                scope,
            ))
        };

        if let Some((runtime, http_client, results_tx, scope)) = spawn_task {
            runtime.spawn(async move {
                let decoded = fetch_badge_metadata(&http_client, &scope)
                    .await
                    .map_err(|err| format!("{err:#}"));
                let _ = results_tx.send(BadgeMetadataResult { scope, decoded });
            });
        }
    }

    fn render_image_from_url(
        &self,
        ui: &mut egui::Ui,
        url: &str,
        target_height: f32,
        hover_label: Option<&str>,
    ) -> bool {
        match self.lookup_or_request_image(url) {
            ImageLookup::Ready(loaded) => {
                let response = paint_texture_scaled(ui, &loaded, target_height);
                if let Some(label) = hover_label {
                    response.on_hover_text(label);
                }
                true
            }
            ImageLookup::Pending | ImageLookup::Failed => false,
        }
    }

    fn lookup_or_request_image(&self, url: &str) -> ImageLookup {
        if url.trim().is_empty() {
            return ImageLookup::Failed;
        }

        let mut spawn_task = None;
        let lookup = {
            let mut inner = self.inner.lock();
            if let Some(existing) = inner.ready_images.get(url).cloned() {
                ImageLookup::Ready(existing)
            } else if inner.pending_urls.contains(url) {
                ImageLookup::Pending
            } else if inner.failed_urls.contains(url) {
                ImageLookup::Failed
            } else {
                inner.pending_urls.insert(url.to_owned());
                spawn_task = Some((
                    inner.runtime.clone(),
                    inner.http_client.clone(),
                    inner.fetch_results_tx.clone(),
                    url.to_owned(),
                ));
                ImageLookup::Pending
            }
        };

        if let Some((runtime, http_client, results_tx, url)) = spawn_task {
            runtime.spawn(async move {
                let decoded = fetch_and_decode_image(&http_client, &url)
                    .await
                    .map_err(|err| format!("{err:#}"));
                let _ = results_tx.send(AssetPipelineResult { url, decoded });
            });
        }

        lookup
    }

    fn lookup_or_request_channel_icon(&self, channel_login: &str) -> Option<String> {
        let normalized = normalize_channel_login_for_display(channel_login)?;
        let spawn_task = {
            let mut inner = self.inner.lock();
            if let Some(url) = inner.channel_icon_urls.get(&normalized) {
                return Some(url.clone());
            }
            if inner.pending_channel_icons.contains(&normalized)
                || inner.failed_channel_icons.contains(&normalized)
            {
                return None;
            }
            inner.pending_channel_icons.insert(normalized.clone());
            Some((
                inner.runtime.clone(),
                inner.http_client.clone(),
                inner.channel_icon_results_tx.clone(),
                normalized,
            ))
        };

        if let Some((runtime, http_client, results_tx, channel_login)) = spawn_task {
            runtime.spawn(async move {
                let icon_url = fetch_channel_icon_url(&http_client, &channel_login)
                    .await
                    .map_err(|err| format!("{err:#}"));
                let _ = results_tx.send(ChannelIconResult {
                    channel_login,
                    icon_url,
                });
            });
        }

        None
    }
}

async fn fetch_and_decode_image(client: &reqwest::Client, url: &str) -> Result<DecodedRgbaImage> {
    let response = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("request failed for {url}"))?;
    let status = response.status();
    if status != StatusCode::OK {
        anyhow::bail!("http {status} for {url}");
    }

    let bytes = response
        .bytes()
        .await
        .with_context(|| format!("failed reading body for {url}"))?;
    let decoded = image::load_from_memory(&bytes)
        .with_context(|| format!("failed decoding image for {url}"))?
        .into_rgba8();
    let (width, height) = decoded.dimensions();
    if width == 0 || height == 0 {
        anyhow::bail!("decoded zero-sized image for {url}");
    }

    Ok(DecodedRgbaImage {
        size: [width as usize, height as usize],
        rgba: decoded.into_raw(),
    })
}

async fn fetch_channel_icon_url(
    client: &reqwest::Client,
    channel_login: &str,
) -> Result<Option<String>> {
    let channel_login = normalize_channel_login_for_display(channel_login)
        .ok_or_else(|| anyhow::anyhow!("channel login is empty"))?;
    let url = format!("https://api.ivr.fi/v2/twitch/user?login={channel_login}");
    let response = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("request failed for {url}"))?;
    let status = response.status();
    if status == StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if status != StatusCode::OK {
        anyhow::bail!("http {status} for {url}");
    }

    let bytes = response
        .bytes()
        .await
        .with_context(|| format!("failed reading body for {url}"))?;
    let users: Vec<IvrUser> =
        serde_json::from_slice(&bytes).with_context(|| format!("invalid JSON from {url}"))?;

    Ok(users
        .into_iter()
        .filter_map(|user| user.logo)
        .find(|logo| !logo.trim().is_empty()))
}

fn paint_texture_scaled(
    ui: &mut egui::Ui,
    loaded: &LoadedTexture,
    target_height: f32,
) -> egui::Response {
    let [width_px, height_px] = loaded.size_px;
    let aspect = if height_px == 0 {
        1.0
    } else {
        width_px as f32 / height_px as f32
    };
    let height = target_height.max(1.0);
    let width = (height * aspect).clamp(height * 0.5, height * 8.0);
    ui.image((loaded.texture.id(), egui::vec2(width, height)))
}

async fn fetch_badge_metadata(
    client: &reqwest::Client,
    scope: &BadgeMetadataScope,
) -> Result<BadgeIconIndex> {
    let urls: Vec<String> = match scope {
        BadgeMetadataScope::Global => vec![
            "https://badges.twitch.tv/v1/badges/global/display".to_owned(),
            "https://api.ivr.fi/v2/twitch/badges/global".to_owned(),
        ],
        BadgeMetadataScope::Channel(channel_id) => vec![
            format!("https://badges.twitch.tv/v1/badges/channels/{channel_id}/display"),
            format!("https://api.ivr.fi/v2/twitch/badges/channel?id={channel_id}"),
        ],
    };
    let mut saw_not_found = false;
    let mut last_err: Option<anyhow::Error> = None;

    for url in urls {
        match fetch_badge_metadata_once(client, &url).await {
            Ok(bytes) => {
                return parse_badge_metadata_json(&bytes)
                    .with_context(|| format!("failed parsing badge metadata JSON for {url}"));
            }
            Err(BadgeMetadataFetchError::NotFound) => {
                saw_not_found = true;
            }
            Err(BadgeMetadataFetchError::Other(err)) => {
                last_err = Some(err.context(format!("badge metadata request failed for {url}")));
            }
        }
    }

    if matches!(scope, BadgeMetadataScope::Channel(_)) && saw_not_found {
        return Ok(BadgeIconIndex::new());
    }

    if let Some(err) = last_err {
        Err(err)
    } else {
        anyhow::bail!("no badge metadata source succeeded")
    }
}

fn parse_badge_metadata_json(bytes: &[u8]) -> Result<BadgeIconIndex> {
    if let Ok(parsed) = serde_json::from_slice::<TwitchBadgeResponse>(bytes) {
        let mut index = BadgeIconIndex::new();
        for (badge_set_name, badge_set) in parsed.badge_sets {
            for (version, meta) in badge_set.versions {
                if let Some(url) = meta
                    .image_url_2x
                    .or(meta.image_url_1x)
                    .or(meta.image_url_4x)
                {
                    index.insert((badge_set_name.clone(), version), url);
                }
            }
        }
        return Ok(index);
    }

    let parsed: Vec<IvrBadgeSet> = serde_json::from_slice(bytes)?;
    let mut index = BadgeIconIndex::new();

    for badge_set in parsed {
        for meta in badge_set.versions {
            if let Some(url) = meta
                .image_url_2x
                .or(meta.image_url_1x)
                .or(meta.image_url_4x)
            {
                index.insert((badge_set.set_id.clone(), meta.id), url);
            }
        }
    }

    Ok(index)
}

enum BadgeMetadataFetchError {
    NotFound,
    Other(anyhow::Error),
}

async fn fetch_badge_metadata_once(
    client: &reqwest::Client,
    url: &str,
) -> std::result::Result<Vec<u8>, BadgeMetadataFetchError> {
    let response = client
        .get(url)
        .send()
        .await
        .map_err(anyhow::Error::from)
        .map_err(BadgeMetadataFetchError::Other)?;
    let status = response.status();
    if status == StatusCode::NOT_FOUND {
        return Err(BadgeMetadataFetchError::NotFound);
    }
    if status != StatusCode::OK {
        return Err(BadgeMetadataFetchError::Other(anyhow::anyhow!(
            "http {status}"
        )));
    }
    response
        .bytes()
        .await
        .map(|b| b.to_vec())
        .map_err(anyhow::Error::from)
        .map_err(BadgeMetadataFetchError::Other)
}

impl std::fmt::Debug for AssetCacheHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AssetCacheHandle").finish_non_exhaustive()
    }
}

#[derive(Debug)]
pub struct PerfOverlayState {
    frame_times_ms: VecDeque<f32>,
    last_frame_started: Option<Instant>,
    last_present_ms: f32,
}

impl PerfOverlayState {
    pub fn new() -> Self {
        Self {
            frame_times_ms: VecDeque::with_capacity(240),
            last_frame_started: None,
            last_present_ms: 0.0,
        }
    }

    pub fn begin_frame(&mut self) {
        let now = Instant::now();
        if let Some(prev) = self.last_frame_started.replace(now) {
            let ms = now.duration_since(prev).as_secs_f32() * 1000.0;
            self.last_present_ms = ms;
            self.frame_times_ms.push_back(ms);
            while self.frame_times_ms.len() > 240 {
                self.frame_times_ms.pop_front();
            }
        }
    }

    pub fn avg_ms(&self) -> f32 {
        if self.frame_times_ms.is_empty() {
            return 0.0;
        }
        self.frame_times_ms.iter().sum::<f32>() / self.frame_times_ms.len() as f32
    }

    pub fn fps(&self) -> f32 {
        let avg_ms = self.avg_ms();
        if avg_ms <= f32::EPSILON {
            0.0
        } else {
            1000.0 / avg_ms
        }
    }

    pub fn last_frame_ms(&self) -> f32 {
        self.last_present_ms
    }
}

impl Default for PerfOverlayState {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Default)]
struct FilterTextFields {
    include_terms: String,
    exclude_terms: String,
    highlight_terms: String,
    hidden_users: String,
}

impl FilterTextFields {
    fn from_config(config: &GlobalFilterConfig) -> Self {
        Self {
            include_terms: join_terms(&config.include_terms),
            exclude_terms: join_terms(&config.exclude_terms),
            highlight_terms: join_terms(&config.highlight_terms),
            hidden_users: join_terms(&config.hidden_users),
        }
    }
}

impl FastChatApp {
    pub fn new(_cc: &eframe::CreationContext<'_>) -> Result<Self> {
        let paths = AppPaths::discover()?;
        let config_repo = ConfigRepository::new(&paths);
        let mut config = config_repo.load_or_default().unwrap_or_else(|err| {
            eprintln!("failed to load config, using defaults: {err:#}");
            AppConfig::default()
        });
        let normalized_animation_config_changed =
            config.ui.enable_message_animations || config.ui.enable_smooth_scroll;
        config.ui.enable_message_animations = false;
        config.ui.enable_smooth_scroll = false;
        let available_chat_fonts = install_chat_fonts(&_cc.egui_ctx, config.ui.allow_system_fonts);
        if !available_chat_fonts.contains(&config.ui.chat_font_family) {
            config.ui.chat_font_family = ChatFontFamily::Proportional;
        }

        let runtime = Runtime::new().context("failed to create tokio runtime")?;
        let twitch_client = AnonymousTwitchChatClient::new(runtime.handle().clone());
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let filter_engine = FilterEngine::new(config.global_filters.clone());
        let chat_store = ChatStore::new(15_000);
        let backlog_writer = BacklogWriter::spawn(&paths, BacklogRetention::default());
        let asset_cache = AssetCacheHandle::new(runtime.handle().clone());
        let channel_input = config.last_channel.clone().unwrap_or_default();
        let filter_fields = FilterTextFields::from_config(&config.global_filters);
        let first_frame_auto_connect =
            config.auto_reconnect_last_channel && !channel_input.is_empty();

        Ok(Self {
            runtime,
            twitch_client,
            events_tx,
            events_rx,
            paths,
            config_repo,
            config,
            channel_input,
            filter_fields,
            filter_engine,
            chat_store,
            backlog_writer,
            asset_cache,
            row_layout_cache: RowLayoutCache::new(4096),
            popout_row_layout_cache: RowLayoutCache::new(4096),
            perf_overlay: PerfOverlayState::new(),
            connection_state: ConnectionState::Disconnected,
            ui_snapshot: UiSnapshot::default(),
            first_frame_auto_connect,
            pending_config_save_since: normalized_animation_config_changed
                .then_some(Instant::now()),
            last_config_save_error: None,
            status_message: None,
            visible_stick_to_bottom: true,
            jump_to_latest_requested: false,
            focused_sender_login: None,
            chat_scroll: ChatScrollState::default(),
            popout_window_open: false,
            popout_selected_message_id: None,
            popout_custom_message_input: String::new(),
            popout_custom_message_text: String::new(),
            popout_custom_message_visible: false,
            available_chat_fonts,
            observed_badge_types: BTreeSet::new(),
            events_processed_total: 0,
        })
    }

    fn poll_events(&mut self) {
        const EVENT_POLL_BUDGET_PER_FRAME: u32 = 1200;
        let mut processed_this_frame = 0u32;
        loop {
            match self.events_rx.try_recv() {
                Ok(event) => {
                    processed_this_frame += 1;
                    self.events_processed_total = self.events_processed_total.saturating_add(1);
                    self.handle_event(event);
                    if processed_this_frame >= EVENT_POLL_BUDGET_PER_FRAME {
                        warn!("event poll budget hit in one frame");
                        break;
                    }
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => break,
            }
        }
        self.ui_snapshot = UiSnapshot {
            connection_state: Some(self.connection_state.clone()),
            store_stats: self.chat_store.stats(),
            processed_events_total: self.events_processed_total,
            last_status: self.status_message.clone(),
        };
    }

    fn handle_event(&mut self, event: ChatEvent) {
        match event {
            ChatEvent::Message(message) => {
                self.observe_badge_types(&message);
                self.backlog_writer
                    .append(BacklogRecord::from_message(message.clone()));
                self.chat_store.push(message, &self.filter_engine);
            }
            ChatEvent::ConnectionState(state) => {
                self.connection_state = state.clone();
                self.status_message = Some(match &state {
                    ConnectionState::Disconnected => "Disconnected".to_owned(),
                    ConnectionState::Connecting { channel } => {
                        format!("Connecting to #{channel}...")
                    }
                    ConnectionState::Connected { channel } => format!("Connected to #{channel}"),
                    ConnectionState::Reconnecting { channel, attempt } => {
                        format!("Reconnecting to #{channel} (attempt {attempt})")
                    }
                    ConnectionState::Error { channel, message } => match channel {
                        Some(channel) => format!("Connection error for #{channel}: {message}"),
                        None => format!("Connection error: {message}"),
                    },
                });
            }
            ChatEvent::Info { channel, text } => {
                self.status_message = Some(match channel {
                    Some(c) => format!("[{c}] {text}"),
                    None => text,
                });
            }
            ChatEvent::Error { channel, text } => {
                self.status_message = Some(match channel {
                    Some(c) => format!("[{c}] {text}"),
                    None => text,
                });
            }
        }
    }

    fn connect_current_channel(&mut self) {
        let requested = self.channel_input.trim().to_owned();
        if requested.is_empty() {
            self.status_message = Some("channel username is required".to_owned());
            return;
        }

        match self
            .twitch_client
            .connect(requested.clone(), self.events_tx.clone())
        {
            Ok(()) => {
                self.config.last_channel = Some(requested);
                self.mark_config_dirty();
                self.status_message = Some("Connecting...".to_owned());
            }
            Err(err) => {
                self.status_message = Some(format!("connect failed: {err:#}"));
            }
        }
    }

    fn mark_config_dirty(&mut self) {
        self.pending_config_save_since = Some(Instant::now());
    }

    fn maybe_save_config(&mut self) {
        let Some(changed_at) = self.pending_config_save_since else {
            return;
        };
        if changed_at.elapsed() < Duration::from_millis(300) {
            return;
        }
        match self.config_repo.save(&self.config) {
            Ok(()) => {
                self.pending_config_save_since = None;
                self.last_config_save_error = None;
            }
            Err(err) => {
                self.last_config_save_error = Some(format!("{err:#}"));
                self.pending_config_save_since = Some(Instant::now());
            }
        }
    }

    fn apply_filter_fields_to_config(&mut self) {
        self.config.global_filters.include_terms = parse_terms(&self.filter_fields.include_terms);
        self.config.global_filters.exclude_terms = parse_terms(&self.filter_fields.exclude_terms);
        self.config.global_filters.highlight_terms =
            parse_terms(&self.filter_fields.highlight_terms);
        self.config.global_filters.hidden_users = parse_terms(&self.filter_fields.hidden_users);
        self.filter_engine
            .set_config(self.config.global_filters.clone());
        self.chat_store.recompute_filters(&self.filter_engine);
        self.row_layout_cache.clear();
        self.mark_config_dirty();
    }

    fn toggle_focused_sender_login(&mut self, sender_login: String) {
        if self.focused_sender_login.as_deref() == Some(sender_login.as_str()) {
            self.focused_sender_login = None;
        } else {
            self.focused_sender_login = Some(sender_login);
        }
        self.popout_selected_message_id = None;
    }

    fn observe_badge_types(&mut self, message: &ChatMessage) {
        for badge in &message.badges {
            if let Some(normalized) = normalize_badge_type(&badge.name) {
                self.observed_badge_types.insert(normalized);
            }
        }
    }

    fn badge_type_filter_options(&self) -> Vec<String> {
        let mut badge_types = self.observed_badge_types.clone();
        for badge_type in &self.config.global_filters.hidden_badge_types {
            if let Some(normalized) = normalize_badge_type(badge_type) {
                badge_types.insert(normalized);
            }
        }
        badge_types.into_iter().collect()
    }

    fn set_badge_type_hidden(&mut self, badge_type: &str, hidden: bool) {
        let Some(normalized) = normalize_badge_type(badge_type) else {
            return;
        };

        let mut next = BTreeSet::new();
        for existing in &self.config.global_filters.hidden_badge_types {
            if let Some(existing_norm) = normalize_badge_type(existing) {
                if existing_norm != normalized {
                    next.insert(existing_norm);
                }
            }
        }
        if hidden {
            next.insert(normalized);
        }
        self.config.global_filters.hidden_badge_types = next.into_iter().collect();
    }

    fn set_popout_window_open(&mut self, ctx: &egui::Context, open: bool) {
        if self.popout_window_open == open {
            return;
        }

        self.popout_window_open = open;
        if !open {
            ctx.send_viewport_cmd_to(popout_viewport_id(), egui::ViewportCommand::Close);
        }
    }

    fn sync_main_window_geometry(&mut self, ctx: &egui::Context) {
        let viewport_info = ctx.input(|i| i.viewport().clone());
        if apply_viewport_info_to_window_config(&viewport_info, &mut self.config.window) {
            self.mark_config_dirty();
        }
    }

    fn toggle_popout_message_selection(&mut self, message_id: String) {
        if self.popout_selected_message_id.as_deref() == Some(message_id.as_str()) {
            self.popout_selected_message_id = None;
        } else {
            self.popout_selected_message_id = Some(message_id);
        }
    }

    fn set_sidebar_open(&mut self, open: bool) {
        if self.config.ui.filters_panel_open != open {
            self.config.ui.filters_panel_open = open;
            self.mark_config_dirty();
        }
    }

    fn toggle_sidebar(&mut self) {
        let open = self.config.ui.filters_panel_open;
        self.set_sidebar_open(!open);
    }

    fn render_sidebar_tools(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.heading("Controls");
        ui.add_space(6.0);

        ui.collapsing("Connection", |ui| {
            ui.label(connection_state_label(&self.connection_state));
            if let Some(status) = &self.status_message {
                ui.label(RichText::new(status).small().color(Color32::GRAY));
            }
            ui.horizontal(|ui| {
                ui.label("Channel");
                let response = ui.add(
                    egui::TextEdit::singleline(&mut self.channel_input)
                        .hint_text("twitch username")
                        .desired_width(170.0),
                );
                if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    self.connect_current_channel();
                }
            });

            if let Some(channel_login) =
                channel_login_for_icon(&self.connection_state, &self.channel_input)
            {
                ui.horizontal(|ui| {
                    let icon_size = 22.0_f32;
                    if !self
                        .asset_cache
                        .render_channel_icon(ui, &channel_login, icon_size)
                    {
                        render_channel_icon_placeholder(ui, icon_size);
                    }
                    ui.label(
                        RichText::new(format!("#{channel_login}"))
                            .small()
                            .color(Color32::LIGHT_GRAY),
                    );
                });
            }

            let button_label = if matches!(self.connection_state, ConnectionState::Connected { .. })
            {
                "Reconnect"
            } else {
                "Connect"
            };
            if ui.button(button_label).clicked() {
                self.connect_current_channel();
            }
        });

        ui.collapsing("Typography", |ui| {
            let mut typography_changed = false;
            let system_fonts_changed = ui
                .checkbox(&mut self.config.ui.allow_system_fonts, "Use system fonts")
                .on_hover_text("When off, Fast Chat only uses built-in fonts.")
                .changed();
            if system_fonts_changed {
                self.available_chat_fonts =
                    install_chat_fonts(ctx, self.config.ui.allow_system_fonts);
                if !self
                    .available_chat_fonts
                    .contains(&self.config.ui.chat_font_family)
                {
                    self.config.ui.chat_font_family = ChatFontFamily::Proportional;
                }
                typography_changed = true;
            }
            ui.horizontal(|ui| {
                ui.label("Font");
                egui::ComboBox::from_id_salt("chat_font_family_selector")
                    .selected_text(chat_font_family_label(self.config.ui.chat_font_family))
                    .width(140.0)
                    .show_ui(ui, |ui| {
                        for family in self.available_chat_fonts.iter().copied() {
                            typography_changed |= ui
                                .selectable_value(
                                    &mut self.config.ui.chat_font_family,
                                    family,
                                    chat_font_family_label(family),
                                )
                                .changed();
                        }
                    });
            });
            ui.horizontal(|ui| {
                ui.label("Main size");
                typography_changed |= ui
                    .add(
                        egui::DragValue::new(&mut self.config.ui.chat_font_size)
                            .range(10..=36)
                            .speed(0.2),
                    )
                    .changed();
            });

            if typography_changed {
                self.row_layout_cache.clear();
                self.popout_row_layout_cache.clear();
                self.mark_config_dirty();
            }
        });

        ui.collapsing("Popout", |ui| {
            let popout_label = if self.popout_window_open {
                "Close Popout"
            } else {
                "Open Popout"
            };
            if ui.button(popout_label).clicked() {
                self.set_popout_window_open(ctx, !self.popout_window_open);
            }

            let popout_size_changed = ui
                .horizontal(|ui| {
                    ui.label("Font size");
                    ui.add(
                        egui::DragValue::new(&mut self.config.ui.popout_chat_font_size)
                            .range(10..=240)
                            .speed(0.5),
                    )
                    .changed()
                })
                .inner;
            if popout_size_changed {
                self.popout_row_layout_cache.clear();
                self.mark_config_dirty();
            }

            ui.separator();
            ui.label("Custom message");
            let custom_message_changed = ui
                .add(
                    egui::TextEdit::singleline(&mut self.popout_custom_message_input)
                        .hint_text("message shown in popout overlay")
                        .desired_width(f32::INFINITY),
                )
                .changed();

            let show_label = if self.popout_custom_message_visible {
                "Unshow"
            } else {
                "Show"
            };
            if ui.button(show_label).clicked() {
                if self.popout_custom_message_visible {
                    self.popout_custom_message_visible = false;
                } else {
                    let trimmed = self.popout_custom_message_input.trim();
                    if trimmed.is_empty() {
                        self.status_message =
                            Some("Custom popout message cannot be empty.".to_owned());
                    } else {
                        self.popout_custom_message_text = trimmed.to_owned();
                        self.popout_custom_message_visible = true;
                        self.set_popout_window_open(ctx, true);
                    }
                }
            }
            if custom_message_changed && self.popout_custom_message_visible {
                let trimmed = self.popout_custom_message_input.trim();
                if !trimmed.is_empty() {
                    self.popout_custom_message_text = trimmed.to_owned();
                }
            }
        });

        ui.collapsing("Runtime", |ui| {
            let perf_changed = ui
                .checkbox(&mut self.config.ui.show_perf_overlay, "Show perf overlay")
                .changed();
            if perf_changed {
                self.mark_config_dirty();
            }
        });
    }

    fn render_filters_panel(&mut self, ctx: &egui::Context) {
        egui::SidePanel::left("filters_panel")
            .resizable(true)
            .default_width(320.0)
            .show_animated(ctx, self.config.ui.filters_panel_open, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        self.render_sidebar_tools(ui, ctx);
                        ui.separator();
                        ui.heading("Filters");
                        ui.add_space(6.0);

                        ui.horizontal(|ui| {
                            ui.label("Include");
                            if ui
                                .add(
                                    egui::TextEdit::singleline(
                                        &mut self.filter_fields.include_terms,
                                    )
                                    .hint_text("comma-separated"),
                                )
                                .changed()
                            {
                                self.apply_filter_fields_to_config();
                            }
                        });
                        ui.horizontal(|ui| {
                            ui.label("Exclude");
                            if ui
                                .add(
                                    egui::TextEdit::singleline(
                                        &mut self.filter_fields.exclude_terms,
                                    )
                                    .hint_text("comma-separated"),
                                )
                                .changed()
                            {
                                self.apply_filter_fields_to_config();
                            }
                        });
                        ui.horizontal(|ui| {
                            ui.label("Highlight");
                            if ui
                                .add(
                                    egui::TextEdit::singleline(
                                        &mut self.filter_fields.highlight_terms,
                                    )
                                    .hint_text("comma-separated"),
                                )
                                .changed()
                            {
                                self.apply_filter_fields_to_config();
                            }
                        });
                        ui.horizontal(|ui| {
                            ui.label("Hidden users");
                            if ui
                                .add(
                                    egui::TextEdit::singleline(
                                        &mut self.filter_fields.hidden_users,
                                    )
                                    .hint_text("comma-separated"),
                                )
                                .changed()
                            {
                                self.apply_filter_fields_to_config();
                            }
                        });

                        ui.separator();

                        let mut filter_logic_changed = false;
                        let mut appearance_changed = false;
                        filter_logic_changed |= ui
                            .horizontal(|ui| {
                                ui.label("Min length");
                                ui.add(
                                    egui::DragValue::new(
                                        &mut self.config.global_filters.min_message_len,
                                    )
                                    .range(0..=500),
                                )
                                .changed()
                            })
                            .inner;

                        ui.collapsing("Message visibility", |ui| {
                            let v = &mut self.config.global_filters.visibility;
                            filter_logic_changed |=
                                ui.checkbox(&mut v.show_mod_messages, "Show mods").changed();
                            filter_logic_changed |=
                                ui.checkbox(&mut v.show_vip_messages, "Show VIPs").changed();
                            filter_logic_changed |= ui
                                .checkbox(&mut v.show_subscriber_messages, "Show subscribers")
                                .changed();
                            filter_logic_changed |= ui
                                .checkbox(
                                    &mut v.show_non_subscriber_messages,
                                    "Show non-subscribers",
                                )
                                .changed();
                            filter_logic_changed |= ui
                                .checkbox(&mut v.show_cheers, "Show cheers/bits")
                                .changed();
                            filter_logic_changed |= ui
                                .checkbox(&mut v.show_redeems, "Show redeems/highlights")
                                .changed();
                            filter_logic_changed |= ui
                                .checkbox(&mut v.show_system_notices, "Show system notices")
                                .changed();
                        });

                        let badge_type_filters = self.badge_type_filter_options();
                        ui.collapsing("Badge types", |ui| {
                            if badge_type_filters.is_empty() {
                                ui.label(
                                    RichText::new("No badge types seen yet.").color(Color32::GRAY),
                                );
                                return;
                            }

                            ui.label(
                                RichText::new("Hide messages from users with selected badges.")
                                    .small()
                                    .color(Color32::GRAY),
                            );
                            let hidden_badges: HashSet<String> = self
                                .config
                                .global_filters
                                .hidden_badge_types
                                .iter()
                                .filter_map(|value| normalize_badge_type(value))
                                .collect();

                            for badge_type in badge_type_filters {
                                let mut show_badge = !hidden_badges.contains(badge_type.as_str());
                                let label =
                                    format!("Show {}", format_badge_type_label(&badge_type));
                                if ui.checkbox(&mut show_badge, label).changed() {
                                    self.set_badge_type_hidden(&badge_type, !show_badge);
                                    filter_logic_changed = true;
                                }
                            }
                        });

                        ui.collapsing("Appearance", |ui| {
                            appearance_changed |= ui
                                .checkbox(&mut self.config.ui.show_badges, "Show badges")
                                .changed();
                            appearance_changed |= ui
                                .checkbox(
                                    &mut self.config.ui.show_per_user_name_colors,
                                    "Use per-user name colors",
                                )
                                .changed();

                            if !self.config.ui.show_per_user_name_colors {
                                appearance_changed |= ui
                                    .horizontal(|ui| {
                                        ui.label("Username color");
                                        let mut color = color32_from_rgb(
                                            self.config.ui.fallback_user_name_color,
                                        );
                                        let changed = egui::color_picker::color_edit_button_srgba(
                                            ui,
                                            &mut color,
                                            egui::color_picker::Alpha::Opaque,
                                        )
                                        .changed();
                                        if changed {
                                            self.config.ui.fallback_user_name_color =
                                                rgb_from_color32(color);
                                        }
                                        changed
                                    })
                                    .inner;
                            }

                            appearance_changed |= ui
                                .horizontal(|ui| {
                                    ui.label("Text color");
                                    let mut color =
                                        color32_from_rgb(self.config.ui.chat_text_color);
                                    let changed = egui::color_picker::color_edit_button_srgba(
                                        ui,
                                        &mut color,
                                        egui::color_picker::Alpha::Opaque,
                                    )
                                    .changed();
                                    if changed {
                                        self.config.ui.chat_text_color = rgb_from_color32(color);
                                    }
                                    changed
                                })
                                .inner;

                            appearance_changed |= ui
                                .horizontal(|ui| {
                                    ui.label("Background");
                                    let mut color =
                                        color32_from_rgb(self.config.ui.chat_background_color);
                                    let changed = egui::color_picker::color_edit_button_srgba(
                                        ui,
                                        &mut color,
                                        egui::color_picker::Alpha::Opaque,
                                    )
                                    .changed();
                                    if changed {
                                        self.config.ui.chat_background_color =
                                            rgb_from_color32(color);
                                    }
                                    changed
                                })
                                .inner;
                        });

                        if filter_logic_changed {
                            self.filter_engine
                                .set_config(self.config.global_filters.clone());
                            self.chat_store.recompute_filters(&self.filter_engine);
                            self.mark_config_dirty();
                        }
                        if appearance_changed {
                            self.mark_config_dirty();
                        }

                        ui.separator();
                        if ui.button("Clear visible view").clicked() {
                            self.chat_store.clear_visible_view();
                        }
                        if ui.button("Reset filters").clicked() {
                            self.config.global_filters = GlobalFilterConfig::default();
                            self.filter_fields =
                                FilterTextFields::from_config(&self.config.global_filters);
                            self.filter_engine
                                .set_config(self.config.global_filters.clone());
                            self.chat_store.recompute_filters(&self.filter_engine);
                            self.mark_config_dirty();
                        }
                    });
            });
    }

    fn render_chat(&mut self, ctx: &egui::Context) {
        const MANUAL_SCROLL_BREAK_THRESHOLD: f32 = 18.0;
        let focused_sender_login = self.focused_sender_login.clone();
        let chat_typography = ChatTypography::from_ui_config(&self.config.ui);
        let chat_appearance = ChatAppearance::from_ui_config(&self.config.ui);
        let previous_offset_y = self.chat_scroll.offset_y;
        let previous_max_scroll_y = self.chat_scroll.max_offset_y;
        let row_height_fallback = chat_typography.row_height();
        let estimated_row_height = self
            .row_layout_cache
            .average_row_height(row_height_fallback)
            .max(row_height_fallback);
        let visible_count = if focused_sender_login.is_some() {
            self.chat_store
                .visible_entries()
                .filter(|entry| matches_focused_sender(entry, focused_sender_login.as_deref()))
                .count()
        } else {
            self.chat_store.visible_len()
        };
        let bottom_padding_px = 18.0_f32;
        egui::CentralPanel::default()
            .frame(egui::Frame::default().fill(chat_appearance.background_color))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if ui
                        .button(if self.config.ui.filters_panel_open {
                            "Close Sidebar"
                        } else {
                            "Open Sidebar"
                        })
                        .clicked()
                    {
                        self.toggle_sidebar();
                    }
                    ui.checkbox(&mut self.visible_stick_to_bottom, "Stick to bottom");
                    if !self.visible_stick_to_bottom && self.chat_scroll.should_show_jump_button() {
                        if ui.button("Jump to latest").clicked() {
                            self.visible_stick_to_bottom = true;
                            self.jump_to_latest_requested = true;
                        }
                    }
                    if let Some(sender_login) = focused_sender_login.as_deref() {
                        ui.separator();
                        ui.label(format!("Only @{sender_login}"));
                        if ui.button("Show all users").clicked() {
                            self.toggle_focused_sender_login(sender_login.to_owned());
                        }
                    }
                    if let Some(err) = &self.last_config_save_error {
                        ui.colored_label(Color32::RED, format!("Config save error: {err}"));
                    }
                });
                ui.separator();

                if visible_count == 0 {
                    ui.add_space(12.0);
                    let placeholder = match &self.connection_state {
                        ConnectionState::Disconnected => {
                            "Enter a Twitch channel username and press Connect.".to_owned()
                        }
                        ConnectionState::Connecting { .. } => {
                            "Connecting to Twitch chat...".to_owned()
                        }
                        ConnectionState::Connected { channel: _ } => {
                            if let Some(sender_login) = focused_sender_login.as_deref() {
                                if self.chat_store.is_empty() {
                                    "Connected. Waiting for messages...".to_owned()
                                } else {
                                    format!("No visible messages from @{sender_login}.")
                                }
                            } else if self.chat_store.is_empty() {
                                // Connected but no messages have arrived yet (quiet/offline/slow channel).
                                // Keep this explicit so the UI doesn't look frozen.
                                "Connected. Waiting for messages...".to_owned()
                            } else {
                                "No messages match the current filters.".to_owned()
                            }
                        }
                        ConnectionState::Reconnecting { .. } => {
                            "Connection lost. Reconnecting...".to_owned()
                        }
                        ConnectionState::Error { .. } => {
                            "Connection failed. Check the channel name and try again.".to_owned()
                        }
                    };
                    ui.label(RichText::new(placeholder).color(Color32::GRAY));
                }
                let mut scroll_area = egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .stick_to_bottom(self.visible_stick_to_bottom);
                if self.jump_to_latest_requested {
                    scroll_area =
                        scroll_area.vertical_scroll_offset(self.chat_scroll.jump_target_offset());
                }
                let mut clicked_message_id: Option<String> = None;
                let mut clicked_sender_login: Option<String> = None;
                let selected_message_id = self.popout_selected_message_id.clone();
                let scroll_output = scroll_area.show_viewport(ui, |ui, viewport| {
                    let virtual_range = approximate_virtual_range(
                        viewport,
                        visible_count,
                        estimated_row_height,
                        10,
                        self.visible_stick_to_bottom,
                    );

                    if virtual_range.top_spacer > 0.0 {
                        ui.add_space(virtual_range.top_spacer);
                    }

                    for entry in collect_virtual_entries(
                        &self.chat_store,
                        focused_sender_login.as_deref(),
                        visible_count,
                        virtual_range.start_idx,
                        virtual_range.take_count,
                    ) {
                        let interaction = render_chat_row(
                            ui,
                            entry,
                            &self.asset_cache,
                            &mut self.row_layout_cache,
                            chat_typography,
                            chat_appearance,
                            selected_message_id.as_deref() == Some(entry.message.id.as_str()),
                            true,
                        );
                        if let Some(sender_login) = interaction.clicked_sender_login {
                            clicked_sender_login = Some(sender_login);
                        } else if interaction.row_clicked {
                            clicked_message_id = Some(entry.message.id.clone());
                        }
                    }

                    if virtual_range.bottom_spacer > 0.0 {
                        ui.add_space(virtual_range.bottom_spacer);
                    }
                    ui.add_space(bottom_padding_px);
                });
                let max_scroll_y =
                    (scroll_output.content_size.y - scroll_output.inner_rect.height()).max(0.0);
                let current_offset = scroll_output.state.offset.y.max(0.0).min(max_scroll_y);
                if self.visible_stick_to_bottom && !self.jump_to_latest_requested {
                    let content_shrank =
                        max_scroll_y + MANUAL_SCROLL_BREAK_THRESHOLD < previous_max_scroll_y;
                    let user_scrolled_up =
                        current_offset + MANUAL_SCROLL_BREAK_THRESHOLD < previous_offset_y;
                    let moved_away_from_bottom =
                        current_offset + MANUAL_SCROLL_BREAK_THRESHOLD < max_scroll_y;
                    if !content_shrank && user_scrolled_up && moved_away_from_bottom {
                        self.visible_stick_to_bottom = false;
                    }
                }
                self.chat_scroll.update(current_offset, max_scroll_y);
                self.jump_to_latest_requested = false;
                if let Some(sender_login) = clicked_sender_login {
                    self.toggle_focused_sender_login(sender_login);
                } else if let Some(clicked_id) = clicked_message_id {
                    self.toggle_popout_message_selection(clicked_id);
                }
            });
    }

    fn render_popout_chat_viewport(&mut self, ctx: &egui::Context) {
        if !self.popout_window_open {
            return;
        }

        let selected_message_id = self.popout_selected_message_id.clone();
        let mut clicked_sender_login: Option<String> = None;
        let chat_typography = ChatTypography::from_popout_ui_config(&self.config.ui);
        let chat_appearance = ChatAppearance::from_ui_config(&self.config.ui);
        let mut viewport_builder = egui::ViewportBuilder::default()
            .with_title("Fast Chat - Popout")
            .with_inner_size([
                self.config.popout_window.width.max(320.0),
                self.config.popout_window.height.max(240.0),
            ])
            .with_min_inner_size([320.0, 240.0]);
        if let (Some(x), Some(y)) = (
            self.config.popout_window.pos_x,
            self.config.popout_window.pos_y,
        ) {
            viewport_builder = viewport_builder.with_position(egui::pos2(x, y));
        }
        if self.config.popout_window.maximized {
            viewport_builder = viewport_builder.with_maximized(true);
        }

        ctx.show_viewport_immediate(
            popout_viewport_id(),
            viewport_builder,
            |popout_ctx, viewport_class| {
                let viewport_info = popout_ctx.input(|i| i.viewport().clone());
                if apply_viewport_info_to_window_config(
                    &viewport_info,
                    &mut self.config.popout_window,
                ) {
                    self.mark_config_dirty();
                }
                if popout_ctx.input(|i| i.viewport().close_requested()) {
                    self.popout_window_open = false;
                    return;
                }

                match viewport_class {
                    egui::ViewportClass::Embedded => {
                        let mut open = self.popout_window_open;
                        egui::Window::new("Popout Chat")
                            .id(egui::Id::new("fastchat_popout_embedded_window"))
                            .open(&mut open)
                            .resizable(true)
                            .show(popout_ctx, |ui| {
                                self.render_popout_chat_contents(
                                    ui,
                                    selected_message_id.as_deref(),
                                    chat_typography,
                                    chat_appearance,
                                    &mut clicked_sender_login,
                                );
                            });
                        self.popout_window_open = open;
                    }
                    _ => {
                        egui::CentralPanel::default()
                            .frame(egui::Frame::default().fill(chat_appearance.background_color))
                            .show(popout_ctx, |ui| {
                                self.render_popout_chat_contents(
                                    ui,
                                    selected_message_id.as_deref(),
                                    chat_typography,
                                    chat_appearance,
                                    &mut clicked_sender_login,
                                );
                            });
                    }
                }
            },
        );
        if let Some(sender_login) = clicked_sender_login {
            self.toggle_focused_sender_login(sender_login);
        }
    }

    fn render_popout_chat_contents(
        &mut self,
        ui: &mut egui::Ui,
        selected_message_id: Option<&str>,
        chat_typography: ChatTypography,
        chat_appearance: ChatAppearance,
        clicked_sender_login: &mut Option<String>,
    ) {
        let bottom_padding_px = 18.0_f32;
        let row_height_fallback = chat_typography.row_height();
        let estimated_row_height = self
            .popout_row_layout_cache
            .average_row_height(row_height_fallback)
            .max(row_height_fallback);
        let selection_active = selected_message_id.is_some();
        let focused_sender_login = self.focused_sender_login.clone();
        let selected_entry = selected_message_id.and_then(|selected_id| {
            self.chat_store
                .visible_entries()
                .filter(|entry| matches_focused_sender(entry, focused_sender_login.as_deref()))
                .find(|entry| entry.message.id == selected_id)
        });
        let visible_count = if selected_entry.is_some() {
            1
        } else if selection_active {
            0
        } else {
            if focused_sender_login.is_some() {
                self.chat_store
                    .visible_entries()
                    .filter(|entry| matches_focused_sender(entry, focused_sender_login.as_deref()))
                    .count()
            } else {
                self.chat_store.visible_len()
            }
        };
        let show_custom_overlay = self.popout_custom_message_visible
            && !self.popout_custom_message_text.trim().is_empty();

        if visible_count == 0 {
            ui.add_space(12.0);
            let placeholder = if selection_active {
                "Selected message is no longer in the visible buffer. Click a message in the main chat.".to_owned()
            } else {
                match &self.connection_state {
                    ConnectionState::Disconnected => {
                        "Enter a Twitch channel username and press Connect.".to_owned()
                    }
                    ConnectionState::Connecting { .. } => "Connecting to Twitch chat...".to_owned(),
                    ConnectionState::Connected { channel: _ } => {
                        if let Some(sender_login) = focused_sender_login.as_deref() {
                            if self.chat_store.is_empty() {
                                "Connected. Waiting for messages...".to_owned()
                            } else {
                                format!("No visible messages from @{sender_login}.")
                            }
                        } else if self.chat_store.is_empty() {
                            "Connected. Waiting for messages...".to_owned()
                        } else {
                            "No messages match the current filters.".to_owned()
                        }
                    }
                    ConnectionState::Reconnecting { .. } => {
                        "Connection lost. Reconnecting...".to_owned()
                    }
                    ConnectionState::Error { .. } => {
                        "Connection failed. Check the channel name and try again.".to_owned()
                    }
                }
            };
            ui.label(RichText::new(placeholder).color(Color32::GRAY));
        }

        ui.scope(|ui| {
            if show_custom_overlay {
                ui.multiply_opacity(0.35);
            }
            let _scroll_output = egui::ScrollArea::vertical()
                .stick_to_bottom(true)
                .auto_shrink([false, false])
                .show_viewport(ui, |ui, viewport| {
                    if let Some(entry) = selected_entry {
                        let interaction = render_chat_row(
                            ui,
                            entry,
                            &self.asset_cache,
                            &mut self.popout_row_layout_cache,
                            chat_typography,
                            chat_appearance,
                            false,
                            false,
                        );
                        if let Some(sender_login) = interaction.clicked_sender_login {
                            *clicked_sender_login = Some(sender_login);
                        }
                        ui.add_space(bottom_padding_px);
                        return;
                    }

                    let virtual_range = approximate_virtual_range(
                        viewport,
                        visible_count,
                        estimated_row_height,
                        10,
                        true,
                    );

                    if virtual_range.top_spacer > 0.0 {
                        ui.add_space(virtual_range.top_spacer);
                    }
                    for entry in collect_virtual_entries(
                        &self.chat_store,
                        focused_sender_login.as_deref(),
                        visible_count,
                        virtual_range.start_idx,
                        virtual_range.take_count,
                    ) {
                        let interaction = render_chat_row(
                            ui,
                            entry,
                            &self.asset_cache,
                            &mut self.popout_row_layout_cache,
                            chat_typography,
                            chat_appearance,
                            false,
                            false,
                        );
                        if let Some(sender_login) = interaction.clicked_sender_login {
                            *clicked_sender_login = Some(sender_login);
                        }
                    }
                    if virtual_range.bottom_spacer > 0.0 {
                        ui.add_space(virtual_range.bottom_spacer);
                    }
                    ui.add_space(bottom_padding_px);
                });
        });

        if show_custom_overlay {
            render_popout_custom_message_overlay(
                ui,
                &self.popout_custom_message_text,
                chat_typography,
            );
        }
    }

    fn render_bottom_status(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::bottom("status_bar").show(ctx, |ui| {
            ui.horizontal_wrapped(|ui| {
                let stats = self.ui_snapshot.store_stats;
                ui.label(format!("Visible: {}", stats.visible_messages));
                ui.separator();
                ui.label(format!(
                    "In-memory: {}/{}",
                    stats.total_messages, stats.capacity
                ));
                ui.separator();
                ui.label(format!(
                    "Processed events: {}",
                    self.ui_snapshot.processed_events_total
                ));
                ui.separator();
                ui.label(format!("Disk log dir: {}", self.paths.logs_dir.display()));
                if let Some(status) = &self.ui_snapshot.last_status {
                    ui.separator();
                    ui.label(status);
                }
            });
        });
    }

    fn render_perf_overlay(&mut self, ctx: &egui::Context) {
        if !self.config.ui.show_perf_overlay {
            return;
        }
        egui::Window::new("Perf")
            .default_pos(egui::pos2(20.0, 80.0))
            .fixed_size(egui::vec2(260.0, 140.0))
            .resizable(false)
            .collapsible(false)
            .show(ctx, |ui| {
                ui.monospace(format!("FPS:        {:>7.1}", self.perf_overlay.fps()));
                ui.monospace(format!(
                    "Avg frame:  {:>7.2} ms",
                    self.perf_overlay.avg_ms()
                ));
                ui.monospace(format!(
                    "Last frame: {:>7.2} ms",
                    self.perf_overlay.last_frame_ms()
                ));
                ui.monospace(format!(
                    "Visible:    {:>7}",
                    self.ui_snapshot.store_stats.visible_messages
                ));
                ui.monospace(format!(
                    "Total:      {:>7}",
                    self.ui_snapshot.store_stats.total_messages
                ));
            });
    }
}

impl eframe::App for FastChatApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let _runtime_guard = self.runtime.enter();
        self.perf_overlay.begin_frame();
        self.asset_cache.pump_completed(ctx);
        self.poll_events();

        if self.first_frame_auto_connect {
            self.first_frame_auto_connect = false;
            self.connect_current_channel();
        }

        self.render_filters_panel(ctx);
        self.render_chat(ctx);
        self.render_popout_chat_viewport(ctx);
        self.render_bottom_status(ctx);
        self.render_perf_overlay(ctx);

        self.sync_main_window_geometry(ctx);
        self.maybe_save_config();
        ctx.request_repaint_after(Duration::from_millis(16));
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.twitch_client.disconnect();
        self.backlog_writer.flush();
        if let Err(err) = self.config_repo.save(&self.config) {
            error!(?err, "failed to save config on exit");
        }
    }
}

#[derive(Default)]
struct ChatRowInteraction {
    row_clicked: bool,
    clicked_sender_login: Option<String>,
}

fn render_chat_row(
    ui: &mut egui::Ui,
    entry: &StoredChatEntry,
    assets: &AssetCacheHandle,
    row_cache: &mut RowLayoutCache,
    typography: ChatTypography,
    appearance: ChatAppearance,
    is_selected: bool,
    selection_click_enabled: bool,
) -> ChatRowInteraction {
    let bg = if is_selected {
        Color32::from_rgb(0x1A, 0x3F, 0x66)
    } else if entry.filter.highlighted {
        Color32::from_rgb(0x2C, 0x2A, 0x10)
    } else {
        Color32::TRANSPARENT
    };
    let min_row_height = typography.row_height();
    const ROW_LEFT_PADDING: f32 = 6.0;
    let mut clicked_sender_login = None;
    let mut message_click_start_x = None;

    let row = egui::Frame::NONE.fill(bg).show(ui, |ui| {
        ui.set_min_height(min_row_height);
        ui.add_space(ROW_LEFT_PADDING);
        ui.horizontal_wrapped(|ui| {
            if appearance.show_badges {
                for badge in assets.resolve_badges_for_message(&entry.message) {
                    assets.render_badge_presentation(ui, &badge, typography);
                }
            }

            let name_text = if entry.message.flags.is_deleted {
                format!("{} (deleted)", entry.message.display_name)
            } else {
                entry.message.display_name.clone()
            };
            let mut username = RichText::new(name_text)
                .strong()
                .size(typography.size)
                .family(typography.egui_family());
            if appearance.show_per_user_name_colors {
                if let Some(color) = entry.message.name_color {
                    username = username.color(color32_from_rgb(color));
                } else {
                    username = username.color(appearance.fallback_user_name_color);
                }
            } else {
                username = username.color(appearance.fallback_user_name_color);
            }
            let username_response = ui
                .add(egui::Label::new(username).sense(egui::Sense::click()))
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .on_hover_text(format!(
                    "Show only messages from @{}",
                    entry.message.sender_login
                ));
            if username_response.clicked() {
                clicked_sender_login = Some(entry.message.sender_login.clone());
            }
            ui.label(
                RichText::new(":")
                    .size(typography.size)
                    .family(typography.egui_family())
                    .color(appearance.text_color),
            );
            message_click_start_x = Some(ui.cursor().min.x);

            render_fragments(ui, &entry.message, assets, typography, appearance);
        });
    });
    let rendered_height = row.response.rect.height();
    let row_rect = row.response.rect;
    row.response.on_hover_text(&entry.message.raw_text);
    row_cache.note_row(stable_row_key(entry), rendered_height);

    if let Some(sender_login) = clicked_sender_login {
        return ChatRowInteraction {
            row_clicked: false,
            clicked_sender_login: Some(sender_login),
        };
    }

    if !selection_click_enabled {
        return ChatRowInteraction::default();
    }

    let message_click_start_x = message_click_start_x
        .unwrap_or(row_rect.min.x)
        .clamp(row_rect.min.x, row_rect.max.x);
    let click_rect = egui::Rect::from_min_max(
        egui::pos2(message_click_start_x, row_rect.min.y),
        row_rect.max,
    );
    if click_rect.width() <= 0.0 {
        return ChatRowInteraction::default();
    }

    let click_id = ui.id().with("chat_row_click").with(stable_row_key(entry));
    let click_response = ui.interact(click_rect, click_id, egui::Sense::click());
    ChatRowInteraction {
        row_clicked: click_response.clicked(),
        clicked_sender_login: None,
    }
}

fn render_fragments(
    ui: &mut egui::Ui,
    message: &ChatMessage,
    assets: &AssetCacheHandle,
    typography: ChatTypography,
    appearance: ChatAppearance,
) {
    if message.fragments.is_empty() {
        ui.add(
            egui::Label::new(
                RichText::new(&message.raw_text)
                    .size(typography.size)
                    .family(typography.egui_family())
                    .color(appearance.text_color),
            )
            .wrap(),
        );
        return;
    }

    for fragment in &message.fragments {
        match fragment {
            MessageFragment::Text(text) => {
                let color = appearance.text_color;
                ui.add(
                    egui::Label::new(
                        RichText::new(text)
                            .size(typography.size)
                            .family(typography.egui_family())
                            .color(color),
                    )
                    .wrap(),
                );
            }
            MessageFragment::Emote {
                emote_id,
                code,
                animated_preferred: _,
            } => {
                assets.render_emote(ui, emote_id, code, typography, appearance.text_color);
            }
        }
    }
}

fn render_popout_custom_message_overlay(
    ui: &mut egui::Ui,
    message: &str,
    typography: ChatTypography,
) {
    let overlay_rect = ui.max_rect();
    ui.painter().rect_filled(
        overlay_rect,
        0.0,
        Color32::from_rgba_unmultiplied(10, 10, 14, 180),
    );
    ui.painter().rect_filled(
        overlay_rect,
        0.0,
        Color32::from_rgba_unmultiplied(230, 236, 255, 18),
    );

    // Consume pointer input so the blurred chat beneath doesn't interact while the message is shown.
    let block_id = ui.id().with("popout_custom_message_overlay_block");
    let _ = ui.interact(overlay_rect, block_id, egui::Sense::click_and_drag());

    let overlay_id = egui::Id::new("popout_custom_message_overlay_area");
    egui::Area::new(overlay_id)
        .order(egui::Order::Foreground)
        .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
        .interactable(false)
        .show(ui.ctx(), |ui| {
            let panel_width = (overlay_rect.width() * 0.74).clamp(320.0, 1200.0);
            let message_size = (typography.size * 1.35).clamp(18.0, 96.0);
            egui::Frame::new()
                .fill(Color32::from_rgba_unmultiplied(16, 18, 26, 238))
                .stroke(egui::Stroke::new(
                    1.0,
                    Color32::from_rgba_unmultiplied(192, 214, 255, 196),
                ))
                .corner_radius(egui::CornerRadius::same(14))
                .inner_margin(egui::Margin::same(18))
                .show(ui, |ui| {
                    ui.set_width(panel_width);
                    ui.with_layout(egui::Layout::top_down(Align::Center), |ui| {
                        ui.add(
                            egui::Label::new(
                                RichText::new(message)
                                    .strong()
                                    .size(message_size)
                                    .family(typography.egui_family())
                                    .color(Color32::WHITE),
                            )
                            .wrap(),
                        );
                    });
                });
        });
}

#[derive(Clone, Copy)]
struct ApproxVirtualRange {
    start_idx: usize,
    take_count: usize,
    top_spacer: f32,
    bottom_spacer: f32,
}

fn approximate_virtual_range(
    viewport: egui::Rect,
    visible_count: usize,
    estimated_row_height: f32,
    overscan_rows: usize,
    anchor_to_bottom: bool,
) -> ApproxVirtualRange {
    if visible_count == 0 {
        return ApproxVirtualRange {
            start_idx: 0,
            take_count: 0,
            top_spacer: 0.0,
            bottom_spacer: 0.0,
        };
    }

    let row_height = estimated_row_height.max(1.0);
    if anchor_to_bottom {
        let visible_rows = (viewport.height() / row_height).ceil().max(0.0) as usize;
        let take_count = visible_rows
            .saturating_add(overscan_rows.saturating_mul(2))
            .max(1)
            .min(visible_count);
        let start_idx = visible_count.saturating_sub(take_count);
        return ApproxVirtualRange {
            start_idx,
            take_count,
            top_spacer: start_idx as f32 * row_height,
            bottom_spacer: 0.0,
        };
    }

    let first_visible = (viewport.min.y / row_height).floor().max(0.0) as usize;
    let last_visible_exclusive = (viewport.max.y / row_height).ceil().max(0.0) as usize;
    let start_idx = first_visible
        .saturating_sub(overscan_rows)
        .min(visible_count);
    let end_idx = last_visible_exclusive
        .saturating_add(overscan_rows)
        .min(visible_count);
    ApproxVirtualRange {
        start_idx,
        take_count: end_idx.saturating_sub(start_idx),
        top_spacer: start_idx as f32 * row_height,
        bottom_spacer: visible_count.saturating_sub(end_idx) as f32 * row_height,
    }
}

fn stable_row_key(entry: &StoredChatEntry) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    entry.message.id.hash(&mut hasher);
    hasher.finish()
}

fn matches_focused_sender(entry: &StoredChatEntry, focused_sender_login: Option<&str>) -> bool {
    match focused_sender_login {
        Some(sender_login) => entry.message.sender_login == sender_login,
        None => true,
    }
}

fn collect_virtual_entries<'a>(
    store: &'a ChatStore,
    focused_sender_login: Option<&str>,
    visible_count: usize,
    start_idx: usize,
    take_count: usize,
) -> Vec<&'a StoredChatEntry> {
    if take_count == 0 || start_idx >= visible_count {
        return Vec::new();
    }

    let end_idx = start_idx.saturating_add(take_count).min(visible_count);
    let render_count = end_idx.saturating_sub(start_idx);
    let iter = store
        .visible_entries()
        .filter(|entry| matches_focused_sender(entry, focused_sender_login));

    if start_idx > visible_count / 2 {
        let tail_skip = visible_count.saturating_sub(end_idx);
        let mut entries: Vec<_> = iter.rev().skip(tail_skip).take(render_count).collect();
        entries.reverse();
        entries
    } else {
        iter.skip(start_idx).take(render_count).collect()
    }
}

fn connection_state_label(state: &ConnectionState) -> String {
    match state {
        ConnectionState::Disconnected => "Disconnected".to_owned(),
        ConnectionState::Connecting { channel } => format!("Connecting #{channel}"),
        ConnectionState::Connected { channel } => format!("Live #{channel}"),
        ConnectionState::Reconnecting { channel, attempt } => {
            format!("Reconnecting #{channel} (attempt {attempt})")
        }
        ConnectionState::Error { channel, message } => match channel {
            Some(channel) => format!("Error #{channel}: {message}"),
            None => format!("Error: {message}"),
        },
    }
}

fn channel_login_for_icon(state: &ConnectionState, channel_input: &str) -> Option<String> {
    let from_state = match state {
        ConnectionState::Connecting { channel }
        | ConnectionState::Connected { channel }
        | ConnectionState::Reconnecting { channel, .. } => Some(channel.as_str()),
        ConnectionState::Error {
            channel: Some(channel),
            ..
        } => Some(channel.as_str()),
        ConnectionState::Disconnected | ConnectionState::Error { channel: None, .. } => None,
    };
    from_state
        .and_then(normalize_channel_login_for_display)
        .or_else(|| normalize_channel_login_for_display(channel_input))
}

fn normalize_channel_login_for_display(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    let normalized = trimmed.trim_start_matches('#').to_ascii_lowercase();
    if normalized.is_empty() {
        None
    } else {
        Some(normalized)
    }
}

fn render_channel_icon_placeholder(ui: &mut egui::Ui, size: f32) {
    let size = size.max(12.0);
    let (rect, response) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover());
    let radius = size * 0.5;
    ui.painter()
        .circle_filled(rect.center(), radius, Color32::from_rgb(44, 46, 52));
    ui.painter().circle_stroke(
        rect.center(),
        radius - 0.5,
        egui::Stroke::new(1.0, Color32::from_rgb(70, 74, 83)),
    );
    response.on_hover_text("Channel icon");
}

fn popout_viewport_id() -> egui::ViewportId {
    egui::ViewportId::from_hash_of("fastchat_popout_chat")
}

fn apply_viewport_info_to_window_config(
    viewport_info: &egui::ViewportInfo,
    window_config: &mut WindowConfig,
) -> bool {
    let mut changed = false;

    if let Some(inner_rect) = viewport_info.inner_rect {
        changed |= update_f32(&mut window_config.width, inner_rect.width().max(120.0), 0.5);
        changed |= update_f32(
            &mut window_config.height,
            inner_rect.height().max(120.0),
            0.5,
        );
    }

    if let Some(outer_rect) = viewport_info.outer_rect {
        changed |= update_opt_f32(&mut window_config.pos_x, Some(outer_rect.min.x), 0.5);
        changed |= update_opt_f32(&mut window_config.pos_y, Some(outer_rect.min.y), 0.5);
    }

    if let Some(maximized) = viewport_info.maximized {
        if window_config.maximized != maximized {
            window_config.maximized = maximized;
            changed = true;
        }
    }

    changed
}

fn update_f32(slot: &mut f32, next: f32, epsilon: f32) -> bool {
    if (*slot - next).abs() > epsilon {
        *slot = next;
        true
    } else {
        false
    }
}

fn update_opt_f32(slot: &mut Option<f32>, next: Option<f32>, epsilon: f32) -> bool {
    match (*slot, next) {
        (Some(current), Some(next_value)) if (current - next_value).abs() <= epsilon => false,
        (None, None) => false,
        _ => {
            *slot = next;
            true
        }
    }
}

fn install_chat_fonts(ctx: &egui::Context, allow_system_fonts: bool) -> Vec<ChatFontFamily> {
    let mut available = vec![ChatFontFamily::Proportional, ChatFontFamily::Monospace];
    let mut fonts = egui::FontDefinitions::default();

    if allow_system_fonts {
        for candidate in system_font_candidates() {
            if let Some(path) = candidate
                .paths
                .iter()
                .map(PathBuf::from)
                .find(|p| p.exists() && p.is_file())
            {
                match fs::read(&path) {
                    Ok(bytes) => {
                        let mut data = egui::FontData::from_owned(bytes);
                        data.index = candidate.face_index as u32;
                        let key = custom_font_family_key(candidate.family).to_owned();
                        fonts.font_data.insert(key.clone(), Arc::new(data));

                        let mut chain = vec![key.clone()];
                        let base_list = match candidate.fallback_base {
                            FontFallbackBase::Proportional => fonts
                                .families
                                .get(&egui::FontFamily::Proportional)
                                .cloned()
                                .unwrap_or_default(),
                            FontFallbackBase::Monospace => fonts
                                .families
                                .get(&egui::FontFamily::Monospace)
                                .cloned()
                                .unwrap_or_default(),
                        };
                        chain.extend(base_list);
                        fonts
                            .families
                            .insert(egui::FontFamily::Name(key.into()), chain);

                        available.push(candidate.family);
                    }
                    Err(err) => {
                        warn!(path = %path.display(), ?err, "failed to read font file");
                    }
                }
            }
        }
    }

    // Always apply font definitions so toggling system fonts off resets to pure built-in fonts.
    ctx.set_fonts(fonts);

    available
}

fn system_font_candidates() -> &'static [SystemFontCandidate] {
    const CANDIDATES: &[SystemFontCandidate] = &[
        SystemFontCandidate {
            family: ChatFontFamily::DejaVuSans,
            label: "DejaVu Sans",
            fallback_base: FontFallbackBase::Proportional,
            paths: &[
                "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
                "/usr/local/share/fonts/DejaVuSans.ttf",
            ],
            face_index: 0,
        },
        SystemFontCandidate {
            family: ChatFontFamily::DejaVuSerif,
            label: "DejaVu Serif",
            fallback_base: FontFallbackBase::Proportional,
            paths: &[
                "/usr/share/fonts/truetype/dejavu/DejaVuSerif.ttf",
                "/usr/local/share/fonts/DejaVuSerif.ttf",
            ],
            face_index: 0,
        },
        SystemFontCandidate {
            family: ChatFontFamily::DejaVuSansMono,
            label: "DejaVu Sans Mono",
            fallback_base: FontFallbackBase::Monospace,
            paths: &[
                "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
                "/usr/local/share/fonts/DejaVuSansMono.ttf",
            ],
            face_index: 0,
        },
        SystemFontCandidate {
            family: ChatFontFamily::LiberationSans,
            label: "Liberation Sans",
            fallback_base: FontFallbackBase::Proportional,
            paths: &[
                "/usr/share/fonts/truetype/liberation2/LiberationSans-Regular.ttf",
                "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
            ],
            face_index: 0,
        },
        SystemFontCandidate {
            family: ChatFontFamily::LiberationSerif,
            label: "Liberation Serif",
            fallback_base: FontFallbackBase::Proportional,
            paths: &[
                "/usr/share/fonts/truetype/liberation2/LiberationSerif-Regular.ttf",
                "/usr/share/fonts/truetype/liberation/LiberationSerif-Regular.ttf",
            ],
            face_index: 0,
        },
        SystemFontCandidate {
            family: ChatFontFamily::LiberationMono,
            label: "Liberation Mono",
            fallback_base: FontFallbackBase::Monospace,
            paths: &[
                "/usr/share/fonts/truetype/liberation2/LiberationMono-Regular.ttf",
                "/usr/share/fonts/truetype/liberation/LiberationMono-Regular.ttf",
            ],
            face_index: 0,
        },
        SystemFontCandidate {
            family: ChatFontFamily::NotoSans,
            label: "Noto Sans",
            fallback_base: FontFallbackBase::Proportional,
            paths: &[
                "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf",
                "/usr/share/fonts/noto/NotoSans-Regular.ttf",
            ],
            face_index: 0,
        },
        SystemFontCandidate {
            family: ChatFontFamily::NotoSerif,
            label: "Noto Serif",
            fallback_base: FontFallbackBase::Proportional,
            paths: &[
                "/usr/share/fonts/truetype/noto/NotoSerif-Regular.ttf",
                "/usr/share/fonts/noto/NotoSerif-Regular.ttf",
            ],
            face_index: 0,
        },
        SystemFontCandidate {
            family: ChatFontFamily::NotoSansMono,
            label: "Noto Sans Mono",
            fallback_base: FontFallbackBase::Monospace,
            paths: &[
                "/usr/share/fonts/truetype/noto/NotoSansMono-Regular.ttf",
                "/usr/share/fonts/noto/NotoSansMono-Regular.ttf",
            ],
            face_index: 0,
        },
        SystemFontCandidate {
            family: ChatFontFamily::JetBrainsMono,
            label: "JetBrains Mono",
            fallback_base: FontFallbackBase::Monospace,
            paths: &[
                "/usr/share/fonts/truetype/jetbrains-mono/JetBrainsMono-Regular.ttf",
                "/Library/Fonts/JetBrainsMono-Regular.ttf",
                "C:\\Windows\\Fonts\\JetBrainsMono-Regular.ttf",
            ],
            face_index: 0,
        },
        SystemFontCandidate {
            family: ChatFontFamily::FiraCode,
            label: "Fira Code",
            fallback_base: FontFallbackBase::Monospace,
            paths: &[
                "/usr/share/fonts/truetype/firacode/FiraCode-Regular.ttf",
                "/Library/Fonts/FiraCode-Regular.ttf",
                "C:\\Windows\\Fonts\\FiraCode-Regular.ttf",
            ],
            face_index: 0,
        },
        SystemFontCandidate {
            family: ChatFontFamily::Menlo,
            label: "Menlo",
            fallback_base: FontFallbackBase::Monospace,
            paths: &["/System/Library/Fonts/Menlo.ttc"],
            face_index: 0,
        },
        SystemFontCandidate {
            family: ChatFontFamily::SegoeUi,
            label: "Segoe UI",
            fallback_base: FontFallbackBase::Proportional,
            paths: &["C:\\Windows\\Fonts\\segoeui.ttf"],
            face_index: 0,
        },
        SystemFontCandidate {
            family: ChatFontFamily::Consolas,
            label: "Consolas",
            fallback_base: FontFallbackBase::Monospace,
            paths: &["C:\\Windows\\Fonts\\consola.ttf"],
            face_index: 0,
        },
        SystemFontCandidate {
            family: ChatFontFamily::Georgia,
            label: "Georgia",
            fallback_base: FontFallbackBase::Proportional,
            paths: &[
                "/System/Library/Fonts/Supplemental/Georgia.ttf",
                "/Library/Fonts/Georgia.ttf",
                "C:\\Windows\\Fonts\\georgia.ttf",
            ],
            face_index: 0,
        },
        SystemFontCandidate {
            family: ChatFontFamily::Arial,
            label: "Arial",
            fallback_base: FontFallbackBase::Proportional,
            paths: &[
                "/System/Library/Fonts/Supplemental/Arial.ttf",
                "/Library/Fonts/Arial.ttf",
                "C:\\Windows\\Fonts\\arial.ttf",
            ],
            face_index: 0,
        },
    ];
    CANDIDATES
}

fn custom_font_family_key(family: ChatFontFamily) -> &'static str {
    match family {
        ChatFontFamily::DejaVuSans => "fastchat-font-dejavu-sans",
        ChatFontFamily::DejaVuSerif => "fastchat-font-dejavu-serif",
        ChatFontFamily::DejaVuSansMono => "fastchat-font-dejavu-sans-mono",
        ChatFontFamily::LiberationSans => "fastchat-font-liberation-sans",
        ChatFontFamily::LiberationSerif => "fastchat-font-liberation-serif",
        ChatFontFamily::LiberationMono => "fastchat-font-liberation-mono",
        ChatFontFamily::NotoSans => "fastchat-font-noto-sans",
        ChatFontFamily::NotoSerif => "fastchat-font-noto-serif",
        ChatFontFamily::NotoSansMono => "fastchat-font-noto-sans-mono",
        ChatFontFamily::JetBrainsMono => "fastchat-font-jetbrains-mono",
        ChatFontFamily::FiraCode => "fastchat-font-fira-code",
        ChatFontFamily::Menlo => "fastchat-font-menlo",
        ChatFontFamily::SegoeUi => "fastchat-font-segoe-ui",
        ChatFontFamily::Consolas => "fastchat-font-consolas",
        ChatFontFamily::Georgia => "fastchat-font-georgia",
        ChatFontFamily::Arial => "fastchat-font-arial",
        ChatFontFamily::Proportional | ChatFontFamily::Monospace => "fastchat-font-default",
    }
}

fn chat_font_family_label(family: ChatFontFamily) -> &'static str {
    match family {
        ChatFontFamily::Proportional => "Proportional",
        ChatFontFamily::Monospace => "Monospace",
        ChatFontFamily::DejaVuSans => "DejaVu Sans",
        ChatFontFamily::DejaVuSerif => "DejaVu Serif",
        ChatFontFamily::DejaVuSansMono => "DejaVu Sans Mono",
        ChatFontFamily::LiberationSans => "Liberation Sans",
        ChatFontFamily::LiberationSerif => "Liberation Serif",
        ChatFontFamily::LiberationMono => "Liberation Mono",
        ChatFontFamily::NotoSans => "Noto Sans",
        ChatFontFamily::NotoSerif => "Noto Serif",
        ChatFontFamily::NotoSansMono => "Noto Sans Mono",
        ChatFontFamily::JetBrainsMono => "JetBrains Mono",
        ChatFontFamily::FiraCode => "Fira Code",
        ChatFontFamily::Menlo => "Menlo",
        ChatFontFamily::SegoeUi => "Segoe UI",
        ChatFontFamily::Consolas => "Consolas",
        ChatFontFamily::Georgia => "Georgia",
        ChatFontFamily::Arial => "Arial",
    }
}

fn normalize_badge_type(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_lowercase())
    }
}

fn format_badge_type_label(badge_type: &str) -> String {
    badge_type
        .split('_')
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => format!("{}{}", first.to_ascii_uppercase(), chars.as_str()),
                None => String::new(),
            }
        })
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn join_terms(values: &[String]) -> String {
    values.join(", ")
}

fn parse_terms(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn color32_from_rgb(color: RgbColor) -> Color32 {
    Color32::from_rgb(color.r, color.g, color.b)
}

fn rgb_from_color32(color: Color32) -> RgbColor {
    let [r, g, b, _a] = color.to_array();
    RgbColor::from_rgb(r, g, b)
}
