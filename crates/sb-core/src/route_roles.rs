//! Route roles: which targets answer a public name (`default`, `fast`,
//! `vision`, ...), kept in the settings store instead of the config file.
//!
//! A role document overrides the ordered `targets` of the config's route of
//! the same name (and, when it sets them, `pin_order` and `prefer_awake`).
//! Every other route key (`match`, `require`, `settings_profile`,
//! `optimize`) stays the config's. A document with no targets overrides
//! nothing: the config's route answers, as it does for a name with no
//! document at all. A role whose name has no config route is a route of its
//! own (`match.model` = its name).
//!
//! The store keeps each name's documents as revisions with a history, like
//! the per-model settings ([`crate::ModelSettingsDoc`]); writing, history
//! and revert live in `sb-runtime`.

use serde::{Deserialize, Serialize};

use crate::{Config, PreferAwakeOverride, ProviderKind, RouteConfig};

/// Longest role name accepted.
const MAX_ROLE_NAME: usize = 64;
/// Most targets one role may list.
const MAX_ROLE_TARGETS: usize = 16;
/// Longest reason a lock may carry (characters).
const MAX_LOCK_REASON: usize = 200;

/// The store document of one role (route name).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RouteRoleDoc {
    /// Ordered targets (`provider/model`): the first is the first choice,
    /// the rest are fallbacks. Empty = no override (the config's route).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub targets: Vec<String>,
    /// Keep the declared order (see [`RouteConfig::pin_order`]). Absent =
    /// the config route's, or `true` for a role with no config route.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pin_order: Option<bool>,
    /// Wake-aware ordering (see [`RouteConfig::prefer_awake`]). Absent =
    /// the config route's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefer_awake: Option<PreferAwakeOverride>,
    /// No write changes the role's targets while it is locked.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub locked: bool,
    /// Why the role is locked: shown beside it and in every refusal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lock_reason: Option<String>,
}

/// The shape of a role document or name is not valid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleError(pub String);

impl std::fmt::Display for RoleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for RoleError {}

impl RouteRoleDoc {
    /// The document overrides the config: it lists targets.
    pub fn overrides(&self) -> bool {
        !self.targets.is_empty()
    }

    /// The document's own checks (shape only). Whether each target exists
    /// is the gateway's question: it knows the lanes and their models.
    pub fn validate(&self) -> Result<(), RoleError> {
        if self.targets.len() > MAX_ROLE_TARGETS {
            return Err(RoleError(format!(
                "at most {MAX_ROLE_TARGETS} targets per role"
            )));
        }
        for (i, target) in self.targets.iter().enumerate() {
            if let Some(problem) = target_shape_problem(target) {
                return Err(RoleError(format!("target `{target}` {problem}")));
            }
            if self.targets[..i].contains(target) {
                return Err(RoleError(format!("target `{target}` is listed twice")));
            }
        }
        if !self.overrides() && (self.pin_order.is_some() || self.prefer_awake.is_some()) {
            return Err(RoleError(
                "pin_order and prefer_awake belong to a role with targets".to_string(),
            ));
        }
        match &self.lock_reason {
            Some(_) if !self.locked => {
                return Err(RoleError("lock_reason needs locked: true".to_string()))
            }
            Some(reason)
                if reason.trim().is_empty()
                    || reason.chars().count() > MAX_LOCK_REASON
                    || reason.chars().any(char::is_control) =>
            {
                return Err(RoleError(format!(
                    "the lock reason must be one line of 1 to {MAX_LOCK_REASON} characters"
                )))
            }
            _ => {}
        }
        Ok(())
    }

    /// The route this document makes of `base` (the config's route of the
    /// same name, `None` when the config has none): its targets, and
    /// `pin_order` / `prefer_awake` when set; everything else is `base`'s.
    /// `None` when the document overrides nothing.
    pub fn route_over(&self, name: &str, base: Option<&RouteConfig>) -> Option<RouteConfig> {
        if !self.overrides() {
            return None;
        }
        let mut route = match base {
            Some(base) => base.clone(),
            None => RouteConfig {
                name: name.to_string(),
                match_: crate::RouteMatch {
                    model: Some(name.to_string()),
                },
                require: crate::RouteRequire::default(),
                targets: Vec::new(),
                pin_order: true,
                prefer_awake: None,
                settings_profile: None,
                optimize: None,
            },
        };
        route.targets = self.targets.clone();
        if let Some(pin_order) = self.pin_order {
            route.pin_order = pin_order;
        }
        if let Some(prefer_awake) = self.prefer_awake {
            route.prefer_awake = Some(prefer_awake);
        }
        Some(route)
    }
}

/// Why `target` is not a `provider/model` id, if it is not.
pub fn target_shape_problem(target: &str) -> Option<&'static str> {
    let Some((provider, model)) = target.split_once('/') else {
        return Some("is not provider/model");
    };
    if provider.is_empty() || model.is_empty() {
        return Some("is not provider/model");
    }
    if target.len() > 256 || target.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Some("has spaces, control characters or is too long");
    }
    None
}

/// The config route a role name stands for: the route whose `match.model`
/// is the name, else the route whose `name` is (the `*` route is never a
/// role).
pub fn config_route_for_role<'a>(config: &'a Config, name: &str) -> Option<&'a RouteConfig> {
    config
        .routes
        .iter()
        .find(|route| route.match_.model.as_deref() == Some(name))
        .or_else(|| {
            config
                .routes
                .iter()
                .find(|route| route.name == name && route.match_.model.as_deref() != Some("*"))
        })
}

/// Why `name` cannot be a role (`None` = it can): a role is a plain public
/// name, never an internal id (a `provider/model` target, a provider, a
/// built-in execution profile, a combo, a media workflow name, the `*`
/// route). A name the config already routes is a role (its document
/// overrides that route).
pub fn role_name_problem(config: &Config, name: &str) -> Option<String> {
    let plain = !name.is_empty()
        && name.len() <= MAX_ROLE_NAME
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric());
    if !plain {
        return Some(format!(
            "`{name}` is not a role name: 1 to {MAX_ROLE_NAME} letters, digits, `-`, `_` or `.`, \
             starting with a letter or digit (a `provider/model` id is a target, not a role)"
        ));
    }
    if config_route_for_role(config, name).is_some() {
        return None;
    }
    if config.providers.iter().any(|provider| provider.id == name) {
        return Some(format!("`{name}` is a provider id, not a role"));
    }
    if crate::ExecutionProfile::from_model(name).is_some() {
        return Some(format!(
            "`{name}` is a built-in execution profile, not a role"
        ));
    }
    if config.combos.contains_key(name) {
        return Some(format!("`{name}` is a combo, not a role"));
    }
    let workflow = config
        .providers
        .iter()
        .any(|provider| match &provider.kind {
            ProviderKind::ComfyUi { workflows, .. } => workflows.iter().any(|workflow| {
                workflow.model_id.as_deref() == Some(name)
                    || workflow.aliases.iter().any(|alias| alias == name)
            }),
            _ => false,
        });
    if workflow {
        return Some(format!("`{name}` is a media workflow name, not a role"));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        Config::from_yaml(
            r#"
providers:
  - id: text
    type: mock
  - id: mac
    type: mock
combos:
  mix:
    models: ["text/a", "mac/b"]
routes:
  - name: fast
    match: { model: fast }
    pin_order: true
    prefer_awake: { enabled: true }
    settings_profile: quick
    targets: ["text/a", "mac/b"]
  - name: catchall
    match: { model: "*" }
    targets: ["text/a"]
"#,
        )
        .expect("config")
    }

    #[test]
    fn a_document_overrides_targets_and_keeps_the_other_route_keys() {
        let cfg = config();
        let base = config_route_for_role(&cfg, "fast");
        let doc = RouteRoleDoc {
            targets: vec!["mac/b".to_string()],
            ..Default::default()
        };
        let route = doc.route_over("fast", base).expect("overrides");
        assert_eq!(route.targets, vec!["mac/b"]);
        assert!(route.pin_order);
        assert_eq!(route.settings_profile.as_deref(), Some("quick"));
        assert!(route.prefer_awake.is_some());
        assert_eq!(route.match_.model.as_deref(), Some("fast"));
        // No targets: nothing overridden.
        assert!(RouteRoleDoc::default().route_over("fast", base).is_none());
    }

    #[test]
    fn a_role_without_a_config_route_is_a_pinned_route_of_its_own() {
        let doc = RouteRoleDoc {
            targets: vec!["text/a".to_string()],
            ..Default::default()
        };
        let route = doc.route_over("long", None).expect("overrides");
        assert_eq!(route.name, "long");
        assert_eq!(route.match_.model.as_deref(), Some("long"));
        assert!(route.pin_order);
    }

    #[test]
    fn shape_checks() {
        let bad = |doc: RouteRoleDoc| doc.validate().unwrap_err().0;
        assert!(bad(RouteRoleDoc {
            targets: vec!["nolane".into()],
            ..Default::default()
        })
        .contains("not provider/model"));
        assert!(bad(RouteRoleDoc {
            targets: vec!["text/a".into(), "text/a".into()],
            ..Default::default()
        })
        .contains("twice"));
        assert!(bad(RouteRoleDoc {
            lock_reason: Some("why".into()),
            ..Default::default()
        })
        .contains("needs locked"));
        assert!(bad(RouteRoleDoc {
            locked: true,
            lock_reason: Some("two\nlines".into()),
            ..Default::default()
        })
        .contains("one line"));
        assert!(bad(RouteRoleDoc {
            pin_order: Some(false),
            ..Default::default()
        })
        .contains("with targets"));
        RouteRoleDoc {
            targets: vec!["text/a".into()],
            locked: true,
            lock_reason: Some("night job uses it".into()),
            ..Default::default()
        }
        .validate()
        .expect("valid");
    }

    #[test]
    fn a_role_is_never_an_internal_id() {
        let cfg = config();
        assert_eq!(role_name_problem(&cfg, "fast"), None, "a config route");
        assert_eq!(role_name_problem(&cfg, "long"), None, "a new name");
        for (name, word) in [
            ("text/a", "not a role name"),
            ("text", "provider id"),
            ("mix", "combo"),
            ("auto", "execution profile"),
            ("*", "not a role name"),
            ("", "not a role name"),
            ("-x", "not a role name"),
        ] {
            let problem = role_name_problem(&cfg, name).unwrap_or_default();
            assert!(problem.contains(word), "{name}: {problem}");
        }
        // The `*` route is not a role by its name either.
        assert!(config_route_for_role(&cfg, "catchall").is_none());
    }
}
