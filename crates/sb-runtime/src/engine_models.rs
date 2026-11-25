use std::collections::{BTreeMap, HashMap};
use std::sync::RwLock;

use sb_core::{Config, EngineModelNames, ExecutionProfile, RouteConfig, RouteMatch};
use sb_store::{EngineModelListing, StateStore};
use sha2::{Digest, Sha256};

use crate::{Engine, Snapshot};

type Listings = HashMap<String, HashMap<String, EngineModelListing>>;

#[derive(Default)]
pub(crate) struct EngineModels {
    listings: RwLock<Listings>,
}

impl EngineModels {
    pub(crate) fn restore(&self, store: &dyn StateStore) -> sb_store::Result<()> {
        let restored = store.engine_model_listings()?;
        let mut listings = self
            .listings
            .write()
            .unwrap_or_else(|error| error.into_inner());
        for listing in restored {
            listings
                .entry(listing.lane.clone())
                .or_default()
                .insert(listing.endpoint_key.clone(), listing);
        }
        Ok(())
    }

    fn listing_for<'a>(
        listings: &'a Listings,
        lane: &EngineModelLane,
    ) -> Option<&'a EngineModelListing> {
        listings.get(&lane.name)?.get(&lane.endpoint_key)
    }

    pub(crate) fn route(&self, snap: &Snapshot, name: &str) -> Option<EngineModelRoute> {
        if snap.engine_model_lanes.is_empty() {
            return None;
        }
        let listings = self
            .listings
            .read()
            .unwrap_or_else(|error| error.into_inner());
        if let Some((lane, model)) = name.split_once('/') {
            if let Some(lane) = snap
                .engine_model_lanes
                .iter()
                .find(|entry| entry.name == lane)
            {
                let listing = Self::listing_for(&listings, lane)?;
                return listing_has_model(listing, model)
                    .then(|| EngineModelRoute::single(name.to_string(), listing.seen_at_ms));
            }
        }
        if !allows_bare(&snap.config, name) {
            return None;
        }
        let mut route = EngineModelRoute::default();
        for lane in snap
            .engine_model_lanes
            .iter()
            .filter(|lane| lane.names == EngineModelNames::Bare)
        {
            if let Some(listing) = Self::listing_for(&listings, lane) {
                if listing_has_model(listing, name) {
                    route.add(format!("{}/{name}", lane.name), listing.seen_at_ms);
                }
            }
        }
        (!route.targets.is_empty()).then_some(route)
    }

    pub(crate) fn missing(&self, snap: &Snapshot, target: &str) -> bool {
        let Some((provider, model)) = target.split_once('/') else {
            return false;
        };
        let Some(lane) = snap
            .engine_model_lanes
            .iter()
            .find(|entry| entry.name == provider)
        else {
            return false;
        };
        let listings = self
            .listings
            .read()
            .unwrap_or_else(|error| error.into_inner());
        EngineModels::listing_for(&listings, lane)
            .is_some_and(|listing| !listing_has_model(listing, model))
    }

    fn routes(&self, snap: &Snapshot) -> BTreeMap<String, EngineModelRoute> {
        let listings = self
            .listings
            .read()
            .unwrap_or_else(|error| error.into_inner());
        let mut routes = BTreeMap::<String, EngineModelRoute>::new();
        for lane in &snap.engine_model_lanes {
            let Some(listing) = Self::listing_for(&listings, lane) else {
                continue;
            };
            for model in &listing.model_ids {
                let target = format!("{}/{model}", lane.name);
                if lane.names == EngineModelNames::Bare && allows_bare(&snap.config, model) {
                    routes
                        .entry(model.clone())
                        .or_default()
                        .add(target.clone(), listing.seen_at_ms);
                }
                routes.insert(
                    target.clone(),
                    EngineModelRoute::single(target, listing.seen_at_ms),
                );
            }
        }
        routes
    }
}

#[derive(Clone)]
pub(crate) struct EngineModelLane {
    name: String,
    base: String,
    endpoint_key: String,
    names: EngineModelNames,
}

impl EngineModelLane {
    pub(crate) fn from_config(config: &Config) -> Vec<Self> {
        config
            .providers
            .iter()
            .filter_map(|provider| {
                let lane = config
                    .local_executors
                    .iter()
                    .find(|lane| lane.name == provider.id)?;
                let names = lane.engine.as_ref()?.model_names;
                if names == EngineModelNames::Disabled {
                    return None;
                }
                let base = config.engine_base_url(lane)?;
                let endpoint_key = format!("{:x}", Sha256::digest(base.as_bytes()));
                Some(Self {
                    name: lane.name.clone(),
                    base: base.to_string(),
                    endpoint_key,
                    names,
                })
            })
            .collect()
    }
}

#[derive(Debug, Clone, Default)]
pub struct EngineModelRoute {
    pub targets: Vec<String>,
    pub target_seen_at_ms: BTreeMap<String, i64>,
}

impl EngineModelRoute {
    fn single(target: String, seen_at_ms: i64) -> Self {
        let mut route = Self::default();
        route.add(target, seen_at_ms);
        route
    }

    fn add(&mut self, target: String, seen_at_ms: i64) {
        self.target_seen_at_ms.insert(target.clone(), seen_at_ms);
        self.targets.push(target);
    }

    pub fn route_config(&self, name: &str) -> RouteConfig {
        RouteConfig {
            name: name.to_string(),
            match_: RouteMatch {
                model: Some(name.to_string()),
            },
            require: Default::default(),
            targets: self.targets.clone(),
            pin_order: true,
            prefer_awake: None,
            settings_profile: None,
            optimize: None,
        }
    }
}

fn allows_bare(config: &Config, name: &str) -> bool {
    name != "*"
        && ExecutionProfile::from_model(name).is_none()
        && !config.combos.contains_key(name)
        && !config.providers.iter().any(|provider| {
            name == provider.id
                || name
                    .split_once('/')
                    .is_some_and(|(prefix, _)| prefix == provider.id)
        })
        && config.resolve_workflow(name).is_none()
}

fn listing_has_model(listing: &EngineModelListing, model: &str) -> bool {
    listing
        .model_ids
        .binary_search_by(|id| id.as_str().cmp(model))
        .is_ok()
}

impl Engine {
    pub fn record_engine_models(
        &self,
        snap: &Snapshot,
        lane: &str,
        base: &str,
        seen_at_ms: i64,
        mut model_ids: Vec<String>,
    ) {
        let Some(lane) = snap
            .engine_model_lanes
            .iter()
            .find(|entry| entry.name == lane && entry.base == base)
        else {
            return;
        };
        model_ids.sort_unstable();
        model_ids.dedup();
        let listing = EngineModelListing {
            lane: lane.name.clone(),
            endpoint_key: lane.endpoint_key.clone(),
            model_ids,
            seen_at_ms,
        };
        if let Some(store) = &self.store {
            if let Err(error) = store.record_engine_model_listing(&listing) {
                tracing::warn!(lane = %lane.name, %error, "engine model listing could not be saved");
            }
        }
        let mut listings = self
            .engine_models
            .listings
            .write()
            .unwrap_or_else(|error| error.into_inner());
        let entries = listings.entry(lane.name.clone()).or_default();
        if entries
            .get(&lane.endpoint_key)
            .is_some_and(|previous| previous.seen_at_ms > seen_at_ms)
        {
            return;
        }
        entries.insert(lane.endpoint_key.clone(), listing);
    }

    pub fn engine_model_listing(&self, snap: &Snapshot, lane: &str) -> Option<EngineModelListing> {
        let lane = snap
            .engine_model_lanes
            .iter()
            .find(|entry| entry.name == lane)?;
        let listings = self
            .engine_models
            .listings
            .read()
            .unwrap_or_else(|error| error.into_inner());
        EngineModels::listing_for(&listings, lane).cloned()
    }

    pub fn engine_model_missing(&self, snap: &Snapshot, target: &str) -> bool {
        self.engine_models.missing(snap, target)
    }

    pub fn engine_model_route(&self, snap: &Snapshot, name: &str) -> Option<EngineModelRoute> {
        self.engine_models.route(snap, name)
    }

    pub fn engine_model_routes(&self, snap: &Snapshot) -> BTreeMap<String, EngineModelRoute> {
        self.engine_models.routes(snap)
    }
}
