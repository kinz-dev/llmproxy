//! Keeps every configured provider/model pair inside that provider's catalog.

use std::collections::HashMap;
use std::time::Duration;

use toml_edit::{DocumentMut, Item, value};
use tracing::warn;

use super::{AdminState, Persist, apply_config_change};
use crate::{Config, ModelAlias};

const INTERVAL: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, PartialEq)]
pub(super) enum Fix {
    DefaultModel(String),
    TriageModel(String),
    Target(String, String),
    Alias(String, String),
    Rule(usize, String),
    CloudOverflow(String),
    Fallback(String, usize, String),
}

/// Fixes for references whose model the provider's non-empty catalog lacks.
pub(super) fn plan(config: &Config, catalogs: &HashMap<String, Vec<String>>) -> Vec<Fix> {
    let replacement = |provider: &str, model: &str| {
        let ids = catalogs.get(provider).filter(|ids| !ids.is_empty())?;
        (!ids.iter().any(|id| id == model)).then(|| ids[0].clone())
    };
    let routed = |route: &str| {
        let (provider, model) = route.split_once('/')?;
        replacement(provider, model).map(|model| format!("{provider}/{model}"))
    };
    let mut fixes = Vec::new();
    if let Some(model) = config.default_model.as_deref()
        && let Some(new) = replacement(&config.default_provider, model)
    {
        fixes.push(Fix::DefaultModel(new));
    }
    if !config.interceptor.model.is_empty()
        && let Some(new) = replacement(&config.interceptor.provider, &config.interceptor.model)
    {
        fixes.push(Fix::TriageModel(new));
    }
    for (name, target) in &config.integrated.targets {
        if let Some(new) = replacement(&target.provider, &target.model) {
            fixes.push(Fix::Target(name.clone(), new));
        }
    }
    for (name, alias) in &config.model_aliases {
        let (provider, model) = match alias {
            ModelAlias::Model(model) => (config.default_provider.as_str(), model.as_str()),
            ModelAlias::Route { provider, model } => (provider.as_str(), model.as_str()),
        };
        if let Some(new) = replacement(provider, model) {
            fixes.push(Fix::Alias(name.clone(), new));
        }
    }
    for (index, rule) in config.routing_rules.iter().enumerate() {
        if let Some(new) = routed(&rule.route) {
            fixes.push(Fix::Rule(index, new));
        }
    }
    if let Some(new) = config.sharing.cloud_overflow.as_deref().and_then(routed) {
        fixes.push(Fix::CloudOverflow(new));
    }
    let upstreams = std::iter::once((&config.default_provider, &config.upstream))
        .chain(config.providers.iter());
    for (name, upstream) in upstreams {
        for (index, entry) in upstream.fallback.iter().enumerate() {
            if let Some(new) = routed(entry) {
                fixes.push(Fix::Fallback(name.clone(), index, new));
            }
        }
    }
    fixes
}

pub(super) fn apply(config: &mut Config, fixes: &[Fix]) {
    for fix in fixes {
        match fix {
            Fix::DefaultModel(model) => config.default_model = Some(model.clone()),
            Fix::TriageModel(model) => config.interceptor.model = model.clone(),
            Fix::Target(name, model) => {
                if let Some(target) = config.integrated.targets.get_mut(name) {
                    target.model = model.clone();
                }
            }
            Fix::Alias(name, model) => match config.model_aliases.get_mut(name) {
                Some(ModelAlias::Model(current)) => *current = model.clone(),
                Some(ModelAlias::Route { model: current, .. }) => *current = model.clone(),
                None => {}
            },
            Fix::Rule(index, route) => {
                if let Some(rule) = config.routing_rules.get_mut(*index) {
                    rule.route = route.clone();
                }
            }
            Fix::CloudOverflow(route) => config.sharing.cloud_overflow = Some(route.clone()),
            Fix::Fallback(provider, index, entry) => {
                let upstream = if *provider == config.default_provider
                    && !config.providers.contains_key(provider)
                {
                    Some(&mut config.upstream)
                } else {
                    config.providers.get_mut(provider)
                };
                if let Some(slot) = upstream.and_then(|up| up.fallback.get_mut(*index)) {
                    *slot = entry.clone();
                }
            }
        }
    }
}

pub(super) fn write(doc: &mut DocumentMut, fixes: &[Fix], default_provider: &str) {
    for fix in fixes {
        match fix {
            Fix::DefaultModel(model) => doc["default_model"] = value(model.as_str()),
            Fix::TriageModel(model) => {
                if let Some(table) = doc.get_mut("interceptor") {
                    table["model"] = value(model.as_str());
                }
            }
            Fix::Target(name, model) => {
                if let Some(target) = doc
                    .get_mut("integrated")
                    .and_then(|item| item.get_mut("targets"))
                    .and_then(|item| item.get_mut(name))
                {
                    target["model"] = value(model.as_str());
                }
            }
            Fix::Alias(name, model) => {
                if let Some(alias) = doc.get_mut("model_aliases").and_then(|a| a.get_mut(name)) {
                    if alias.is_str() {
                        *alias = value(model.as_str());
                    } else {
                        alias["model"] = value(model.as_str());
                    }
                }
            }
            Fix::Rule(index, route) => {
                if let Some(rule) = doc
                    .get_mut("routing_rules")
                    .and_then(Item::as_array_of_tables_mut)
                    .and_then(|rules| rules.get_mut(*index))
                {
                    rule["route"] = value(route.as_str());
                }
            }
            Fix::CloudOverflow(route) => {
                if let Some(sharing) = doc.get_mut("sharing") {
                    sharing["cloud_overflow"] = value(route.as_str());
                }
            }
            Fix::Fallback(provider, index, entry) => {
                let in_providers = doc.get("providers").and_then(|p| p.get(provider)).is_some();
                let table = if in_providers {
                    doc.get_mut("providers").and_then(|p| p.get_mut(provider))
                } else if provider == default_provider {
                    doc.get_mut("upstream")
                } else {
                    None
                };
                if let Some(slot) = table
                    .and_then(|table| table.get_mut("fallback"))
                    .and_then(Item::as_array_mut)
                    .and_then(|fallback| fallback.get_mut(*index))
                {
                    *slot = entry.as_str().into();
                }
            }
        }
    }
}

pub(super) fn spawn(state: AdminState) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(INTERVAL).await;
            let snapshot = state.shared.snapshot();
            let catalogs: HashMap<String, Vec<String>> = snapshot
                .providers
                .iter()
                .map(|(name, provider)| (name.clone(), provider.model_cache.cached_ids().0))
                .collect();
            let fixes = plan(&snapshot.config, &catalogs);
            if fixes.is_empty() {
                continue;
            }
            warn!(fixes = ?fixes, "configured models missing from provider catalogs; using each provider's first model");
            let (doc_fixes, default_provider) =
                (fixes.clone(), snapshot.config.default_provider.clone());
            apply_config_change(
                &state,
                move |config| {
                    apply(config, &fixes);
                    Ok(())
                },
                Persist::Edit(Box::new(move |doc| {
                    write(doc, &doc_fixes, &default_provider);
                    Ok(())
                })),
            )
            .await;
        }
    });
}
