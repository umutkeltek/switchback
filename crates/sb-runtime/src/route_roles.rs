//! Route roles at run time: the store's role documents over the config's
//! routes (see [`sb_core::RouteRoleDoc`]).
//!
//! The registry lives on the [`crate::Engine`], beside the model settings:
//! it is state, not config, so a config reload or publish never replaces a
//! role. Routing reads it at request time, so a write takes effect on the
//! next request without a restart; a request already routed keeps the
//! targets it resolved.

use std::borrow::Cow;
use std::sync::Arc;

use sb_core::{config_route_for_role, Config, PreferAwakeOverride, RouteConfig, RouteRoleDoc};
use sb_store::{ModelSettingsChange, StateStore};

use crate::model_settings::{SettingsActor, SettingsWriteError};
use crate::revisioned::{DocEntry, DocFamily, DocRegistry, DocWrite, RevisionedDoc};

/// One role's current document.
pub type RoleEntry = DocEntry<RouteRoleDoc>;

impl RevisionedDoc for RouteRoleDoc {
    const FAMILY: DocFamily = DocFamily::RouteRoles;

    fn check(&self) -> Result<(), String> {
        self.validate().map_err(|e| e.0)
    }

    fn changed(before: &Self, after: &Self) -> Vec<String> {
        let mut changed = Vec::new();
        if before.targets != after.targets {
            changed.push("targets".to_string());
        }
        if before.pin_order != after.pin_order {
            changed.push("pin_order".to_string());
        }
        if before.prefer_awake != after.prefer_awake {
            changed.push("prefer_awake".to_string());
        }
        if before.locked != after.locked {
            changed.push("locked".to_string());
        }
        if before.lock_reason != after.lock_reason {
            changed.push("lock_reason".to_string());
        }
        changed
    }
}

/// A store role answered a request: which and at what revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleUse {
    pub name: String,
    pub revision: u64,
}

impl RoleUse {
    /// The decision's reason line.
    pub fn reason(&self) -> String {
        format!(
            "role `{}`: targets from the role store (revision {})",
            self.name, self.revision
        )
    }
}

/// The route a name resolves to and, when a store role made it, which.
pub struct ExactRoute<'a> {
    pub route: Cow<'a, RouteConfig>,
    pub role: Option<RoleUse>,
    /// The config's own route of this name (a store role over it can fall
    /// back to it when none of its targets is known any more).
    pub config_route: Option<&'a RouteConfig>,
}

/// The role documents of every route name, revisioned.
#[derive(Default)]
pub struct RouteRoles {
    docs: DocRegistry<RouteRoleDoc>,
}

impl RouteRoles {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_history_keep(&self, keep: usize) {
        self.docs.set_history_keep(keep);
    }

    pub fn history_keep(&self) -> usize {
        self.docs.history_keep()
    }

    /// Attach the state store and load every stored role.
    pub fn attach_store(&self, store: Arc<dyn StateStore>) {
        self.docs.attach_store(store);
    }

    /// Whether writes are durable.
    pub fn durable(&self) -> bool {
        self.docs.durable()
    }

    pub fn get(&self, name: &str) -> Option<RoleEntry> {
        self.docs.get(name)
    }

    pub fn revision(&self, name: &str) -> u64 {
        self.docs.revision(name)
    }

    pub fn snapshot_entries(&self) -> Result<Vec<(String, RoleEntry)>, SettingsWriteError> {
        self.docs.snapshot_entries()
    }

    pub fn all(&self) -> Vec<(String, RoleEntry)> {
        self.docs.all()
    }

    /// The role's changes, newest first.
    pub fn history(&self, name: &str, limit: usize) -> Result<Vec<ModelSettingsChange>, String> {
        self.docs.history(name, limit)
    }

    /// Revision `to`'s document (0 = none: the config's route).
    pub fn revision_doc(&self, name: &str, to: u64) -> Result<RouteRoleDoc, SettingsWriteError> {
        self.docs.revision_doc(name, to)
    }

    /// The document a role overrides with, when it does.
    fn overriding(&self, name: &str) -> Option<RoleEntry> {
        self.get(name).filter(|entry| entry.doc.overrides())
    }

    pub fn current(&self, name: &str) -> Result<Option<RoleEntry>, SettingsWriteError> {
        self.docs.current(name)
    }

    pub fn lock_refusal(name: &str, current: &RouteRoleDoc) -> Option<String> {
        if !current.locked {
            return None;
        }
        let why = current
            .lock_reason
            .as_deref()
            .map(|reason| format!(" ({reason})"))
            .unwrap_or_default();
        Some(format!(
            "role `{name}` locked{why}: unlock it first with `sb ai role unlock {name}` (nothing was written)"
        ))
    }

    pub fn replace(
        &self,
        name: &str,
        doc: RouteRoleDoc,
        expected: u64,
        actor: &SettingsActor,
        action: &str,
        unlock: bool,
    ) -> Result<DocWrite<RouteRoleDoc>, SettingsWriteError> {
        let before = self.current(name)?;
        if let Some(entry) = before.as_ref().filter(|entry| entry.doc.equivalent(&doc)) {
            return Ok(DocWrite {
                entry: entry.clone(),
                unchanged: true,
            });
        }
        {
            let current = before.as_ref().map_or(0, |entry| entry.revision);
            if expected != current {
                return Err(SettingsWriteError::Conflict { expected, current });
            }
            if !unlock {
                if let Some(why) = before
                    .as_ref()
                    .and_then(|entry| Self::lock_refusal(name, &entry.doc))
                {
                    return Err(SettingsWriteError::Locked(why));
                }
            }
        }
        self.docs.write(name, doc, expected, actor, action)
    }

    /// Set a role's targets (and, when given, `pin_order` and
    /// `prefer_awake`; `None` keeps the config route's). Refused while the
    /// role is locked.
    pub fn set(
        &self,
        name: &str,
        targets: Vec<String>,
        pin_order: Option<bool>,
        prefer_awake: Option<PreferAwakeOverride>,
        expected: u64,
        actor: &SettingsActor,
    ) -> Result<DocWrite<RouteRoleDoc>, SettingsWriteError> {
        let current = self
            .current(name)?
            .map(|entry| (*entry.doc).clone())
            .unwrap_or_default();
        let doc = RouteRoleDoc {
            targets,
            pin_order,
            prefer_awake,
            ..current
        };
        self.replace(name, doc, expected, actor, "set", false)
    }

    /// Write revision `to`'s targets again as a new revision (0 = no store
    /// targets: the config's route answers). The lock is not part of a
    /// revert; a locked role refuses it.
    pub fn revert(
        &self,
        name: &str,
        to: u64,
        expected: u64,
        actor: &SettingsActor,
    ) -> Result<DocWrite<RouteRoleDoc>, SettingsWriteError> {
        let current = self
            .current(name)?
            .map(|entry| (*entry.doc).clone())
            .unwrap_or_default();
        let old = self.revision_doc(name, to)?;
        let doc = RouteRoleDoc {
            targets: old.targets,
            pin_order: old.pin_order,
            prefer_awake: old.prefer_awake,
            locked: current.locked,
            lock_reason: current.lock_reason,
        };
        self.replace(name, doc, expected, actor, &format!("revert:{to}"), false)
    }

    /// Lock a role (its targets stay as they are) with an optional reason.
    pub fn lock(
        &self,
        name: &str,
        reason: Option<String>,
        expected: u64,
        actor: &SettingsActor,
    ) -> Result<DocWrite<RouteRoleDoc>, SettingsWriteError> {
        let current = self
            .current(name)?
            .map(|entry| (*entry.doc).clone())
            .unwrap_or_default();
        let doc = RouteRoleDoc {
            locked: true,
            lock_reason: reason.or(current.lock_reason.clone()),
            ..current
        };
        self.docs.write(name, doc, expected, actor, "lock")
    }

    /// Take a role's lock off.
    pub fn unlock(
        &self,
        name: &str,
        expected: u64,
        actor: &SettingsActor,
    ) -> Result<DocWrite<RouteRoleDoc>, SettingsWriteError> {
        let current = self
            .current(name)?
            .map(|entry| (*entry.doc).clone())
            .unwrap_or_default();
        let doc = RouteRoleDoc {
            locked: false,
            lock_reason: None,
            ..current
        };
        self.docs.write(name, doc, expected, actor, "unlock")
    }

    /// The store key of a role name in this config: the config route's
    /// name when the config routes the name, else the name itself.
    pub fn key_for<'a>(config: &'a Config, name: &'a str) -> &'a str {
        config_route_for_role(config, name)
            .map(|route| route.name.as_str())
            .unwrap_or(name)
    }

    /// The route an exact model name resolves to: the config's route whose
    /// `match.model` is the name, with the store role of its name over it;
    /// else a store role of that name with no config route. `None` when
    /// neither exists (combos, targets and the `*` route come after).
    pub fn exact_route<'a>(&self, config: &'a Config, model: &str) -> Option<ExactRoute<'a>> {
        if let Some(base) = config.exact_route_for(model) {
            if base.match_.model.as_deref() != Some("*") {
                if let Some(entry) = self.overriding(&base.name) {
                    if let Some(route) = entry.doc.route_over(&base.name, Some(base)) {
                        return Some(ExactRoute {
                            route: Cow::Owned(route),
                            role: Some(RoleUse {
                                name: base.name.clone(),
                                revision: entry.revision,
                            }),
                            config_route: Some(base),
                        });
                    }
                }
            }
            return Some(ExactRoute {
                route: Cow::Borrowed(base),
                role: None,
                config_route: Some(base),
            });
        }
        // A role of its own: only a name no config route answers to.
        if config_route_for_role(config, model).is_some() {
            return None;
        }
        let entry = self.overriding(model)?;
        let route = entry.doc.route_over(model, None)?;
        Some(ExactRoute {
            route: Cow::Owned(route),
            role: Some(RoleUse {
                name: model.to_string(),
                revision: entry.revision,
            }),
            config_route: None,
        })
    }

    /// The config with every store role applied: each overridden route
    /// carries the role's targets, and each role with no config route is
    /// added as a route of its own. For views (`/v1/models`, the settings
    /// of a name), never for the config file or its revisions.
    pub fn effective_config(&self, config: &Config) -> Config {
        let roles: Vec<(String, RoleEntry)> = self
            .all()
            .into_iter()
            .filter(|(_, entry)| entry.doc.overrides())
            .collect();
        if roles.is_empty() {
            return config.clone();
        }
        let mut effective = config.clone();
        for route in effective.routes.iter_mut() {
            if route.match_.model.as_deref() == Some("*") {
                continue;
            }
            if let Some((_, entry)) = roles.iter().find(|(name, _)| *name == route.name) {
                if let Some(over) = entry.doc.route_over(&route.name, Some(route)) {
                    *route = over;
                }
            }
        }
        // Roles of their own go before a `*` route (order matters only to
        // `match.model: "*"`, which is looked for separately anyway).
        for (name, entry) in &roles {
            if config_route_for_role(config, name).is_some() {
                continue;
            }
            if let Some(route) = entry.doc.route_over(name, None) {
                effective.routes.push(route);
            }
        }
        effective
    }
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
routes:
  - name: fast
    match: { model: fast }
    pin_order: true
    targets: ["text/a", "mac/b"]
  - name: catchall
    match: { model: "*" }
    targets: ["text/a"]
"#,
        )
        .expect("config")
    }

    fn actor() -> SettingsActor {
        SettingsActor {
            actor: "gateway key".to_string(),
            role: "admin".to_string(),
            source: "test".to_string(),
        }
    }

    #[test]
    fn a_store_role_overrides_the_config_route_and_revert_zero_gives_it_back() {
        let cfg = config();
        let roles = RouteRoles::new();
        let exact = roles.exact_route(&cfg, "fast").expect("route");
        assert_eq!(exact.route.targets, vec!["text/a", "mac/b"]);
        assert!(exact.role.is_none());

        roles
            .set("fast", vec!["mac/b".into()], None, None, 0, &actor())
            .expect("set");
        let exact = roles.exact_route(&cfg, "fast").expect("route");
        assert_eq!(exact.route.targets, vec!["mac/b"]);
        assert!(exact.route.pin_order, "kept from the config");
        assert_eq!(
            exact.role,
            Some(RoleUse {
                name: "fast".into(),
                revision: 1
            })
        );

        roles.revert("fast", 0, 1, &actor()).expect("revert");
        let exact = roles.exact_route(&cfg, "fast").expect("route");
        assert_eq!(exact.route.targets, vec!["text/a", "mac/b"]);
        assert!(exact.role.is_none());
        assert_eq!(roles.revision("fast"), 2, "the revert is a revision");
        let history = roles.history("fast", 10).unwrap();
        assert_eq!(
            history
                .iter()
                .map(|c| c.action.as_str())
                .collect::<Vec<_>>(),
            vec!["revert:0", "set"]
        );
    }

    #[test]
    fn a_locked_role_refuses_set_and_revert_with_its_reason() {
        let roles = RouteRoles::new();
        roles
            .set("fast", vec!["mac/b".into()], None, None, 0, &actor())
            .unwrap();
        roles
            .lock("fast", Some("night job uses it".into()), 1, &actor())
            .unwrap();
        let refused = roles
            .set("fast", vec!["text/a".into()], None, None, 2, &actor())
            .unwrap_err();
        assert!(
            matches!(&refused, SettingsWriteError::Locked(m) if m.contains("night job uses it")),
            "{refused}"
        );
        assert!(matches!(
            roles.revert("fast", 0, 2, &actor()),
            Err(SettingsWriteError::Locked(_))
        ));
        assert_eq!(roles.get("fast").unwrap().doc.targets, vec!["mac/b"]);
        roles.unlock("fast", 2, &actor()).unwrap();
        roles
            .set("fast", vec!["text/a".into()], None, None, 3, &actor())
            .unwrap();
    }

    #[test]
    fn a_stale_revision_is_a_conflict() {
        let roles = RouteRoles::new();
        roles
            .set("fast", vec!["mac/b".into()], None, None, 0, &actor())
            .unwrap();
        assert!(matches!(
            roles.set("fast", vec!["text/a".into()], None, None, 0, &actor()),
            Err(SettingsWriteError::Conflict {
                expected: 0,
                current: 1
            })
        ));
    }

    #[test]
    fn a_role_of_its_own_routes_and_shows_in_the_effective_config() {
        let cfg = config();
        let roles = RouteRoles::new();
        assert!(roles.exact_route(&cfg, "long").is_none());
        roles
            .set("long", vec!["text/a".into()], None, None, 0, &actor())
            .unwrap();
        let exact = roles.exact_route(&cfg, "long").expect("route");
        assert_eq!(exact.route.name, "long");
        assert!(exact.config_route.is_none());
        roles
            .set("fast", vec!["mac/b".into()], Some(false), None, 0, &actor())
            .unwrap();
        let effective = roles.effective_config(&cfg);
        let fast = effective.exact_route_for("fast").unwrap();
        assert_eq!(fast.targets, vec!["mac/b"]);
        assert!(!fast.pin_order);
        assert_eq!(
            effective.exact_route_for("long").unwrap().targets,
            vec!["text/a"]
        );
        // The `*` route is untouched and the config itself unchanged.
        assert_eq!(effective.wildcard_route().unwrap().targets, vec!["text/a"]);
        assert_eq!(cfg.exact_route_for("fast").unwrap().targets.len(), 2);
    }

    #[test]
    fn roles_survive_a_new_registry_over_the_same_store() {
        let store: Arc<dyn StateStore> = Arc::new(sb_store::SqliteStore::in_memory().unwrap());
        let roles = RouteRoles::new();
        roles.attach_store(store.clone());
        roles
            .set("fast", vec!["mac/b".into()], None, None, 0, &actor())
            .unwrap();
        let again = RouteRoles::new();
        again.attach_store(store);
        assert!(again.durable());
        assert_eq!(again.get("fast").unwrap().doc.targets, vec!["mac/b"]);
        assert_eq!(again.history("fast", 10).unwrap().len(), 1);
    }
}
