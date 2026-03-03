use std::collections::HashSet;

use aho_corasick::{AhoCorasick, AhoCorasickBuilder};

use crate::model::{
    ChatMessage, FilterDecision, FilterDropReason, GlobalFilterConfig, MessageKind,
    MessageVisibilityToggles,
};

#[derive(Debug)]
pub struct FilterEngine {
    config: GlobalFilterConfig,
    hidden_users: HashSet<String>,
    hidden_badge_types: HashSet<String>,
    include_matcher: Option<AhoCorasick>,
    exclude_matcher: Option<AhoCorasick>,
    highlight_matcher: Option<AhoCorasick>,
}

impl FilterEngine {
    pub fn new(config: GlobalFilterConfig) -> Self {
        let mut engine = Self {
            config,
            hidden_users: HashSet::new(),
            hidden_badge_types: HashSet::new(),
            include_matcher: None,
            exclude_matcher: None,
            highlight_matcher: None,
        };
        engine.rebuild();
        engine
    }

    pub fn config(&self) -> &GlobalFilterConfig {
        &self.config
    }

    pub fn set_config(&mut self, config: GlobalFilterConfig) {
        self.config = config;
        self.rebuild();
    }

    pub fn evaluate(&self, message: &ChatMessage) -> FilterDecision {
        let mut decision = FilterDecision {
            visible: true,
            highlighted: false,
            drop_reason: None,
        };

        if !self.visibility_allows(message) {
            return self.drop_for_visibility(message);
        }

        if self
            .hidden_users
            .contains(&message.sender_login.to_lowercase())
        {
            return FilterDecision {
                visible: false,
                highlighted: false,
                drop_reason: Some(FilterDropReason::HiddenUser),
            };
        }

        if self.has_hidden_badge_type(message) {
            return FilterDecision {
                visible: false,
                highlighted: false,
                drop_reason: Some(FilterDropReason::BadgeTypeHidden),
            };
        }

        let msg_len = message.raw_text.chars().count() as u16;
        if msg_len < self.config.min_message_len {
            return FilterDecision {
                visible: false,
                highlighted: false,
                drop_reason: Some(FilterDropReason::TooShort),
            };
        }

        let lowered = message.canonical_text_lowercase();

        if let Some(matcher) = &self.exclude_matcher {
            if matcher.is_match(&lowered) {
                return FilterDecision {
                    visible: false,
                    highlighted: false,
                    drop_reason: Some(FilterDropReason::ExcludedKeyword),
                };
            }
        }

        if let Some(matcher) = &self.include_matcher {
            if !matcher.is_match(&lowered) {
                return FilterDecision {
                    visible: false,
                    highlighted: false,
                    drop_reason: Some(FilterDropReason::MissingIncludedKeyword),
                };
            }
        }

        if let Some(matcher) = &self.highlight_matcher {
            decision.highlighted = matcher.is_match(&lowered);
        }

        decision
    }

    fn rebuild(&mut self) {
        self.hidden_users = self
            .config
            .hidden_users
            .iter()
            .map(|s| s.trim().to_lowercase())
            .filter(|s| !s.is_empty())
            .collect();
        self.hidden_badge_types = self
            .config
            .hidden_badge_types
            .iter()
            .map(|s| s.trim().to_lowercase())
            .filter(|s| !s.is_empty())
            .collect();

        self.include_matcher = Self::compile_terms(&self.config.include_terms);
        self.exclude_matcher = Self::compile_terms(&self.config.exclude_terms);
        self.highlight_matcher = Self::compile_terms(&self.config.highlight_terms);
    }

    fn has_hidden_badge_type(&self, message: &ChatMessage) -> bool {
        if self.hidden_badge_types.is_empty() || message.badges.is_empty() {
            return false;
        }
        message.badges.iter().any(|badge| {
            let name = badge.name.trim();
            if name.is_empty() {
                return false;
            }
            self.hidden_badge_types.contains(name)
                || self.hidden_badge_types.contains(&name.to_lowercase())
        })
    }

    fn compile_terms(terms: &[String]) -> Option<AhoCorasick> {
        let cleaned: Vec<String> = terms
            .iter()
            .map(|s| s.trim().to_lowercase())
            .filter(|s| !s.is_empty())
            .collect();
        if cleaned.is_empty() {
            return None;
        }
        AhoCorasickBuilder::new()
            .ascii_case_insensitive(true)
            .build(cleaned)
            .ok()
    }

    fn visibility_allows(&self, message: &ChatMessage) -> bool {
        let v = &self.config.visibility;
        if message.flags.is_mod && !v.show_mod_messages {
            return false;
        }
        if message.flags.is_vip && !v.show_vip_messages {
            return false;
        }
        if message.flags.is_subscriber && !v.show_subscriber_messages {
            return false;
        }
        if is_regular_user_chat(message.kind)
            && !message.flags.is_subscriber
            && !v.show_non_subscriber_messages
        {
            return false;
        }
        if message.flags.has_bits && !v.show_cheers {
            return false;
        }
        if message.flags.is_redeem && !v.show_redeems {
            return false;
        }
        if is_system_like(message.kind) && !v.show_system_notices {
            return false;
        }
        true
    }

    fn drop_for_visibility(&self, message: &ChatMessage) -> FilterDecision {
        let v: &MessageVisibilityToggles = &self.config.visibility;
        let drop_reason = if message.flags.is_mod && !v.show_mod_messages {
            Some(FilterDropReason::ModHidden)
        } else if message.flags.is_vip && !v.show_vip_messages {
            Some(FilterDropReason::VipHidden)
        } else if message.flags.is_subscriber && !v.show_subscriber_messages {
            Some(FilterDropReason::SubscriberHidden)
        } else if is_regular_user_chat(message.kind)
            && !message.flags.is_subscriber
            && !v.show_non_subscriber_messages
        {
            Some(FilterDropReason::NonSubscriberHidden)
        } else if message.flags.has_bits && !v.show_cheers {
            Some(FilterDropReason::BitsHidden)
        } else if message.flags.is_redeem && !v.show_redeems {
            Some(FilterDropReason::RedeemHidden)
        } else if is_system_like(message.kind) && !v.show_system_notices {
            Some(FilterDropReason::SystemHidden)
        } else {
            None
        };

        FilterDecision {
            visible: false,
            highlighted: false,
            drop_reason,
        }
    }
}

fn is_system_like(kind: MessageKind) -> bool {
    matches!(
        kind,
        MessageKind::Notice
            | MessageKind::UserNotice
            | MessageKind::ClearChat
            | MessageKind::ClearMsg
            | MessageKind::System
    )
}

fn is_regular_user_chat(kind: MessageKind) -> bool {
    matches!(kind, MessageKind::Chat | MessageKind::Action)
}

#[cfg(test)]
mod tests {
    use crate::filter::FilterEngine;
    use crate::model::{
        BadgeTag, ChatMessage, FilterDropReason, GlobalFilterConfig, MessageKind,
        MessageVisibilityToggles,
    };

    #[test]
    fn exclude_precedes_include() {
        let cfg = GlobalFilterConfig {
            include_terms: vec!["hello".into()],
            exclude_terms: vec!["bad".into()],
            ..Default::default()
        };
        let engine = FilterEngine::new(cfg);
        let msg = ChatMessage::new_text("chan", "user", "User", "hello bad", MessageKind::Chat);
        let decision = engine.evaluate(&msg);
        assert!(!decision.visible);
        assert_eq!(
            decision.drop_reason,
            Some(FilterDropReason::ExcludedKeyword)
        );
    }

    #[test]
    fn moderation_toggle_hides_mod_messages() {
        let cfg = GlobalFilterConfig {
            visibility: MessageVisibilityToggles {
                show_mod_messages: false,
                ..Default::default()
            },
            ..Default::default()
        };
        let engine = FilterEngine::new(cfg);
        let mut msg = ChatMessage::new_text("chan", "moduser", "Mod", "hi", MessageKind::Chat);
        msg.flags.is_mod = true;
        let decision = engine.evaluate(&msg);
        assert!(!decision.visible);
        assert_eq!(decision.drop_reason, Some(FilterDropReason::ModHidden));
    }

    #[test]
    fn non_subscriber_toggle_hides_non_sub_chat_only() {
        let cfg = GlobalFilterConfig {
            visibility: MessageVisibilityToggles {
                show_non_subscriber_messages: false,
                ..Default::default()
            },
            ..Default::default()
        };
        let engine = FilterEngine::new(cfg);

        let non_sub_chat =
            ChatMessage::new_text("chan", "viewer", "Viewer", "hello", MessageKind::Chat);
        let decision = engine.evaluate(&non_sub_chat);
        assert!(!decision.visible);
        assert_eq!(
            decision.drop_reason,
            Some(FilterDropReason::NonSubscriberHidden)
        );

        let system_msg =
            ChatMessage::new_text("chan", "twitch", "Twitch", "notice", MessageKind::Notice);
        let system_decision = engine.evaluate(&system_msg);
        assert!(system_decision.visible);
    }

    #[test]
    fn hidden_badge_type_hides_matching_messages() {
        let cfg = GlobalFilterConfig {
            hidden_badge_types: vec!["vip".into()],
            ..Default::default()
        };
        let engine = FilterEngine::new(cfg);

        let mut msg = ChatMessage::new_text("chan", "viewer", "Viewer", "hello", MessageKind::Chat);
        msg.badges.push(BadgeTag {
            name: "vip".into(),
            version: "1".into(),
        });

        let decision = engine.evaluate(&msg);
        assert!(!decision.visible);
        assert_eq!(
            decision.drop_reason,
            Some(FilterDropReason::BadgeTypeHidden)
        );
    }
}
