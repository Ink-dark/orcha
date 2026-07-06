//! IM 触发来源鉴权与白名单（M7）。
//!
//! Gateway 收到 Adapter 转发来的 Trigger / AuthCheck 时，用 [`Authenticator`]
//! 校验 `TriggerSource` 是否在配置的白名单内。白名单匹配规则：
//! - 群聊（group 非空）：按 (platform, group) 匹配，群内任何人 @Orcha 都允许。
//! - 私聊（group 为空）：按 (platform, user) 匹配，仅白名单用户私聊允许。
//! - 平台必须匹配。
//! - 大小写敏感（飞书 open_id / chat_id 是定长字符串，无大小写歧义）。

use crate::protocol::TriggerSource;
use serde::{Deserialize, Serialize};

/// 一条白名单规则。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WhitelistEntry {
    pub platform: String,
    /// 私聊白名单：填 user，group 留空。
    /// 群聊白名单：填 group，user 可留空（群内任何人允许）。
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub group: Option<String>,
}

/// 鉴权结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthResult {
    Allowed,
    Denied { reason: DenyReason },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DenyReason {
    PlatformNotMatched,
    UserNotInWhitelist,
    GroupNotInWhitelist,
    EmptyWhitelist,
}

impl AuthResult {
    pub fn allowed(&self) -> bool {
        matches!(self, AuthResult::Allowed)
    }
    pub fn reason_str(&self) -> Option<&'static str> {
        match self {
            AuthResult::Allowed => None,
            AuthResult::Denied { reason } => Some(match reason {
                DenyReason::PlatformNotMatched => "platform 不在白名单",
                DenyReason::UserNotInWhitelist => "user 不在白名单",
                DenyReason::GroupNotInWhitelist => "group 不在白名单",
                DenyReason::EmptyWhitelist => "白名单为空",
            }),
        }
    }
}

/// 鉴权器：用预编译的白名单做 O(n) 匹配（n 通常 <10，无需 hashmap）。
#[derive(Debug, Clone, Default)]
pub struct Authenticator {
    entries: Vec<WhitelistEntry>,
}

impl Authenticator {
    pub fn new(entries: Vec<WhitelistEntry>) -> Self {
        Self { entries }
    }

    /// 校验 TriggerSource。
    /// - 白名单为空 → Denied(EmptyWhitelist)
    /// - group 非空 → 找 (platform, group) 匹配，匹配则 Allowed，否则 Denied(GroupNotInWhitelist)
    /// - group 为空 → 找 (platform, user) 匹配
    pub fn check(&self, source: &TriggerSource) -> AuthResult {
        // 1. 白名单为空：直接拒绝（避免误以为"任何人都能用"）
        if self.entries.is_empty() {
            return AuthResult::Denied {
                reason: DenyReason::EmptyWhitelist,
            };
        }

        // 2. 平台不在任何白名单条目里：拒绝（避免配置写错平台时被绕过）
        let platform_matched = self.entries.iter().any(|e| e.platform == source.platform);
        if !platform_matched {
            return AuthResult::Denied {
                reason: DenyReason::PlatformNotMatched,
            };
        }

        // 3. 群聊：按 (platform, group) 匹配；私聊：按 (platform, user) 匹配
        if let Some(group) = &source.group {
            let found = self.entries.iter().any(|e| {
                e.platform == source.platform && e.group.as_deref() == Some(group.as_str())
            });
            if found {
                AuthResult::Allowed
            } else {
                AuthResult::Denied {
                    reason: DenyReason::GroupNotInWhitelist,
                }
            }
        } else {
            let found = self.entries.iter().any(|e| {
                e.platform == source.platform && e.user.as_deref() == Some(source.user.as_str())
            });
            if found {
                AuthResult::Allowed
            } else {
                AuthResult::Denied {
                    reason: DenyReason::UserNotInWhitelist,
                }
            }
        }
    }

    /// 校验 AuthCheck 风格的 (user, group) 对（不一定带 platform）。
    /// 这里用 platform=白名单任一匹配项的简化策略，主要给 Adapter 启动时询问用。
    pub fn check_user_group(&self, user: &str, group: Option<&str>) -> AuthResult {
        // 1. 白名单为空：直接拒绝
        if self.entries.is_empty() {
            return AuthResult::Denied {
                reason: DenyReason::EmptyWhitelist,
            };
        }

        // 2. 群聊路径：按 group 匹配（platform 缺省，任一匹配即可）
        if let Some(group) = group {
            let found = self
                .entries
                .iter()
                .any(|e| e.group.as_deref() == Some(group));
            if found {
                AuthResult::Allowed
            } else {
                AuthResult::Denied {
                    reason: DenyReason::GroupNotInWhitelist,
                }
            }
        } else {
            // 3. 私聊路径：按 user 匹配
            let found = self.entries.iter().any(|e| e.user.as_deref() == Some(user));
            if found {
                AuthResult::Allowed
            } else {
                AuthResult::Denied {
                    reason: DenyReason::UserNotInWhitelist,
                }
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试辅助：构造 TriggerSource（raw 留空，鉴权不读 raw）。
    fn src(platform: &str, user: &str, group: Option<&str>) -> TriggerSource {
        TriggerSource {
            platform: platform.to_string(),
            user: user.to_string(),
            group: group.map(|s| s.to_string()),
            raw: String::new(),
        }
    }

    fn feishu_user(u: &str) -> WhitelistEntry {
        WhitelistEntry {
            platform: "feishu".into(),
            user: Some(u.into()),
            group: None,
        }
    }

    fn feishu_group(g: &str) -> WhitelistEntry {
        WhitelistEntry {
            platform: "feishu".into(),
            user: None,
            group: Some(g.into()),
        }
    }

    #[test]
    fn empty_whitelist_denied() {
        // 1. 空白名单 → Denied(EmptyWhitelist)
        let auth = Authenticator::new(vec![]);
        assert_eq!(
            auth.check(&src("feishu", "ou_x", Some("oc_y"))),
            AuthResult::Denied {
                reason: DenyReason::EmptyWhitelist
            }
        );
        assert!(auth.is_empty());
        assert_eq!(auth.len(), 0);
    }

    #[test]
    fn group_match_allowed() {
        // 2. 群聊命中 → Allowed
        let auth = Authenticator::new(vec![feishu_group("oc_y")]);
        let r = auth.check(&src("feishu", "any_user", Some("oc_y")));
        assert_eq!(r, AuthResult::Allowed);
        assert!(r.allowed());
        assert_eq!(r.reason_str(), None);
    }

    #[test]
    fn group_not_match_denied() {
        // 3. 群聊未命中 → Denied(GroupNotInWhitelist)
        let auth = Authenticator::new(vec![feishu_group("oc_y")]);
        let r = auth.check(&src("feishu", "any_user", Some("oc_other")));
        assert_eq!(
            r,
            AuthResult::Denied {
                reason: DenyReason::GroupNotInWhitelist
            }
        );
        assert_eq!(r.reason_str(), Some("group 不在白名单"));
    }

    #[test]
    fn user_match_allowed() {
        // 4. 私聊命中 → Allowed
        let auth = Authenticator::new(vec![feishu_user("ou_x")]);
        let r = auth.check(&src("feishu", "ou_x", None));
        assert_eq!(r, AuthResult::Allowed);
    }

    #[test]
    fn user_not_match_denied() {
        // 5. 私聊未命中 → Denied(UserNotInWhitelist)
        let auth = Authenticator::new(vec![feishu_user("ou_x")]);
        let r = auth.check(&src("feishu", "ou_other", None));
        assert_eq!(
            r,
            AuthResult::Denied {
                reason: DenyReason::UserNotInWhitelist
            }
        );
        assert_eq!(r.reason_str(), Some("user 不在白名单"));
    }

    #[test]
    fn platform_not_matched_denied() {
        // 6. 平台不匹配 → Denied(PlatformNotMatched)
        let auth = Authenticator::new(vec![feishu_user("ou_x")]);
        let r = auth.check(&src("qq", "ou_x", None));
        assert_eq!(
            r,
            AuthResult::Denied {
                reason: DenyReason::PlatformNotMatched
            }
        );
        assert_eq!(r.reason_str(), Some("platform 不在白名单"));
    }

    #[test]
    fn group_match_ignores_user() {
        // 7. 群聊时 user 字段被忽略（群内任何人允许）
        let auth = Authenticator::new(vec![feishu_group("oc_y")]);
        // 群内各种 user 都应该允许
        assert_eq!(
            auth.check(&src("feishu", "user_a", Some("oc_y"))),
            AuthResult::Allowed
        );
        assert_eq!(
            auth.check(&src("feishu", "user_b", Some("oc_y"))),
            AuthResult::Allowed
        );
        assert_eq!(
            auth.check(&src("feishu", "", Some("oc_y"))),
            AuthResult::Allowed
        );
    }

    #[test]
    fn check_user_group_paths() {
        // 8. check_user_group 路径（不带 platform 的简化匹配）
        let auth = Authenticator::new(vec![feishu_user("ou_x"), feishu_group("oc_y")]);
        // 群聊命中
        assert_eq!(
            auth.check_user_group("any", Some("oc_y")),
            AuthResult::Allowed
        );
        // 私聊命中
        assert_eq!(auth.check_user_group("ou_x", None), AuthResult::Allowed);
        // 私聊未命中
        assert_eq!(
            auth.check_user_group("ou_other", None),
            AuthResult::Denied {
                reason: DenyReason::UserNotInWhitelist
            }
        );
        // 群聊未命中
        assert_eq!(
            auth.check_user_group("any", Some("oc_other")),
            AuthResult::Denied {
                reason: DenyReason::GroupNotInWhitelist
            }
        );
        // 空白名单
        let empty = Authenticator::new(vec![]);
        assert_eq!(
            empty.check_user_group("any", None),
            AuthResult::Denied {
                reason: DenyReason::EmptyWhitelist
            }
        );
    }

    #[test]
    fn mixed_whitelist_entries() {
        // 9. 多条白名单混合（群聊规则 + 私聊规则并存，跨平台）
        let auth = Authenticator::new(vec![
            feishu_user("ou_admin"),
            feishu_group("oc_team"),
            WhitelistEntry {
                platform: "qq".into(),
                user: Some("qq_admin".into()),
                group: None,
            },
        ]);
        assert_eq!(auth.len(), 3);
        // feishu 群聊命中
        assert_eq!(
            auth.check(&src("feishu", "u1", Some("oc_team"))),
            AuthResult::Allowed
        );
        // feishu 私聊 admin 命中
        assert_eq!(
            auth.check(&src("feishu", "ou_admin", None)),
            AuthResult::Allowed
        );
        // feishu 私聊非 admin → user 不在白名单
        assert_eq!(
            auth.check(&src("feishu", "other", None)),
            AuthResult::Denied {
                reason: DenyReason::UserNotInWhitelist
            }
        );
        // qq 私聊 admin 命中
        assert_eq!(
            auth.check(&src("qq", "qq_admin", None)),
            AuthResult::Allowed
        );
        // slack 平台完全不在白名单
        assert_eq!(
            auth.check(&src("slack", "x", None)),
            AuthResult::Denied {
                reason: DenyReason::PlatformNotMatched
            }
        );
    }

    #[test]
    fn auth_result_helpers() {
        // 辅助方法 allowed() / reason_str() 边界覆盖
        assert!(AuthResult::Allowed.allowed());
        assert!(!AuthResult::Denied {
            reason: DenyReason::EmptyWhitelist
        }
        .allowed());
        assert_eq!(AuthResult::Allowed.reason_str(), None);
        // 4 种 DenyReason 都能映射到文案
        for reason in [
            DenyReason::PlatformNotMatched,
            DenyReason::UserNotInWhitelist,
            DenyReason::GroupNotInWhitelist,
            DenyReason::EmptyWhitelist,
        ] {
            assert!(AuthResult::Denied { reason }.reason_str().is_some());
        }
    }
}
