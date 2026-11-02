//! Convert one upstream provider into sorted model aliases for an archive block.

use std::collections::BTreeMap;

use serde::ser::SerializeMap;
use serde::{Deserialize, Serialize};

use super::{model_slug, ModelEntry, ModelPricing};

#[derive(Serialize, Deserialize)]
pub(super) struct Provider {
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) api: Option<String>,
    #[serde(default)]
    models: BTreeMap<String, Model>,
}

#[derive(Serialize, Deserialize)]
struct Model {
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    release_date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_updated: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cost: Option<Cost>,
    #[serde(skip_serializing_if = "Option::is_none")]
    limit: Option<Limit>,
    #[serde(skip_serializing_if = "Option::is_none")]
    modalities: Option<Modalities>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<bool>,
}

// Nullable costs must not reject the entire provider. Missing individual
// prices fall back to zero, as they do for the other optional cost fields.
#[derive(Serialize, Deserialize)]
struct Cost {
    #[serde(skip_serializing_if = "Option::is_none")]
    input: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_read: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_write: Option<f64>,
}

#[derive(Serialize, Deserialize)]
struct Limit {
    #[serde(skip_serializing_if = "Option::is_none")]
    context: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output: Option<u32>,
}

#[derive(Serialize, Deserialize)]
struct Modalities {
    #[serde(skip_serializing_if = "Option::is_none")]
    input: Option<Vec<String>>,
}

#[derive(Default)]
pub(super) struct CompiledProvider {
    pub(super) entries: Vec<ModelEntry>,
    pub(super) models: BTreeMap<String, usize>,
}

impl Serialize for CompiledProvider {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(self.models.len()))?;
        for (name, index) in &self.models {
            map.serialize_entry(name, &self.entries[*index])?;
        }
        map.end()
    }
}

pub(super) fn compile_provider(id: &str, provider: Provider) -> CompiledProvider {
    let mut compiled = CompiledProvider {
        entries: Vec::with_capacity(provider.models.len()),
        models: BTreeMap::new(),
    };
    let provider_aliases = [
        Some(id.to_string()),
        provider.name.as_deref().map(model_slug),
    ]
    .into_iter()
    .flatten()
    .filter(|alias| !alias.is_empty())
    .collect::<Vec<_>>();
    let has_provider_alias = provider.models.keys().any(|model| {
        provider_aliases
            .iter()
            .any(|alias| model == alias || model_slug(model) == *alias)
    });
    let mut default_entry: Option<(String, usize)> = None;
    for (model_id, model) in provider.models {
        let pricing = model.cost.and_then(|cost| {
            let input = cost.input.unwrap_or(0.0);
            let output = cost.output.unwrap_or(0.0);
            (input != 0.0 || output != 0.0).then_some(ModelPricing {
                input,
                output,
                cache_read: cost.cache_read.unwrap_or(0.0),
                cache_write: cost.cache_write.unwrap_or(0.0),
            })
        });
        let context_window = model
            .limit
            .as_ref()
            .and_then(|limit| limit.context)
            .filter(|value| *value > 0);
        let output_tokens = model
            .limit
            .as_ref()
            .and_then(|limit| limit.output)
            .filter(|value| *value > 0);
        let supports_reasoning = model.reasoning;
        let input_modalities = model.modalities.and_then(|modalities| {
            let mut values = modalities
                .input?
                .into_iter()
                .map(|value| value.to_ascii_lowercase())
                .filter(|value| !value.is_empty())
                .collect::<Vec<_>>();
            values.sort();
            values.dedup();
            (!values.is_empty()).then_some(values)
        });
        if pricing.is_none()
            && context_window.is_none()
            && output_tokens.is_none()
            && supports_reasoning.is_none()
            && input_modalities.is_none()
        {
            continue;
        }
        let index = compiled.entries.len();
        compiled.entries.push(ModelEntry {
            pricing,
            context_window,
            output_tokens,
            supports_reasoning,
            input_modalities,
        });
        compiled.models.entry(model_id).or_insert(index);
        if let Some(name) = model
            .name
            .as_deref()
            .map(model_slug)
            .filter(|name| !name.is_empty())
        {
            compiled.models.entry(name).or_insert(index);
        }
        let sort = model
            .last_updated
            .or(model.release_date)
            .unwrap_or_default();
        if !sort.is_empty()
            && default_entry
                .as_ref()
                .is_none_or(|(current, _)| sort > *current)
        {
            default_entry = Some((sort, index));
        }
    }
    if !has_provider_alias {
        if let Some((_, index)) = default_entry {
            for alias in provider_aliases {
                compiled.models.entry(alias).or_insert(index);
            }
        }
    }
    compiled
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nullable_costs_and_empty_limits_keep_other_metadata() {
        let provider = serde_json::from_str(
            r#"{"models":{
            "empty":{},
            "kept":{"name":"Display Model", "cost":{"input":null,"output":2,"cache_read":null},
                "limit":{"context":0,"output":null}, "reasoning":false,
                "modalities":{"input":["TEXT","text","","IMAGE"]}}
        }}"#,
        )
        .unwrap();
        let compiled = compile_provider("provider", provider);
        assert_eq!(compiled.entries.len(), 1);
        assert_eq!(compiled.models.len(), 2);
        assert_eq!(compiled.models["kept"], compiled.models["display-model"]);
        let entry = &compiled.entries[0];
        assert_eq!(
            entry.pricing,
            Some(ModelPricing {
                input: 0.0,
                output: 2.0,
                cache_read: 0.0,
                cache_write: 0.0
            })
        );
        assert_eq!(entry.context_window, None);
        assert_eq!(entry.output_tokens, None);
        assert_eq!(entry.supports_reasoning, Some(false));
        assert_eq!(
            entry.input_modalities.as_deref(),
            Some(["image".to_string(), "text".to_string()].as_slice())
        );
    }
}
