//! Filterable provider/model list for the TUI model picker.
//!
//! The catalog is built from config and built-in fallbacks only. It never
//! calls the live `/models` endpoint, so the picker opens offline.

use huginn_config::{ConfigFile, Provider};

/// One provider/model row in the picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelEntry {
    /// Provider name (`grok`, `openrouter`, …).
    pub provider: String,
    /// Model id on that provider.
    pub model: String,
    /// Display label `{provider} · {model}`.
    pub label: String,
}

impl ModelEntry {
    fn new(provider: &str, model: String) -> Self {
        let label = format!("{provider} · {model}");
        Self {
            provider: provider.to_string(),
            model,
            label,
        }
    }
}

/// Filterable list of every known provider's models.
///
/// `selected` is an index into the filtered view. Filtering keeps the
/// highlighted provider and model when that row still matches, and otherwise
/// selects the first match. [`ModelPicker::move_up`] / [`ModelPicker::move_down`]
/// wrap around the filtered rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelPicker {
    entries: Vec<ModelEntry>,
    query: String,
    selected: usize,
}

impl ModelPicker {
    /// Current filter text.
    pub fn query(&self) -> &str {
        &self.query
    }

    /// Index of the highlighted row in the filtered view.
    pub fn selected(&self) -> usize {
        self.selected
    }

    /// Append one character to the filter.
    pub fn push_char(&mut self, c: char) {
        let keep = self.highlighted_id();
        self.query.push(c);
        self.retarget(keep);
    }

    /// Delete the last filter character, if any.
    pub fn backspace(&mut self) {
        let keep = self.highlighted_id();
        self.query.pop();
        self.retarget(keep);
    }

    /// Rows that match the current filter, in catalog order.
    pub fn filtered(&self) -> Vec<&ModelEntry> {
        let query = self.query.to_lowercase();
        self.entries
            .iter()
            .filter(|entry| entry_matches(entry, &query))
            .collect()
    }

    /// Move the highlight to the previous filtered row, wrapping.
    pub fn move_up(&mut self) {
        let n = self.filtered().len();
        if n == 0 {
            self.selected = 0;
            return;
        }
        self.selected = if self.selected == 0 {
            n - 1
        } else {
            self.selected - 1
        };
    }

    /// Move the highlight to the next filtered row, wrapping.
    pub fn move_down(&mut self) {
        let n = self.filtered().len();
        if n == 0 {
            self.selected = 0;
            return;
        }
        self.selected = (self.selected + 1) % n;
    }

    /// The highlighted row, or `None` when the filter matches nothing.
    pub fn confirm(&self) -> Option<ModelEntry> {
        self.filtered()
            .get(self.selected)
            .map(|entry| (*entry).clone())
    }

    fn highlighted_id(&self) -> Option<(String, String)> {
        self.filtered()
            .get(self.selected)
            .map(|entry| (entry.provider.clone(), entry.model.clone()))
    }

    /// Keep `keep` highlighted when it still matches. A dropped row, or a
    /// filter that matches nothing, selects the first row (index 0).
    fn retarget(&mut self, keep: Option<(String, String)>) {
        let filtered = self.filtered();
        if filtered.is_empty() {
            self.selected = 0;
            return;
        }
        if let Some((provider, model)) = keep {
            if let Some(index) = filtered
                .iter()
                .position(|entry| entry.provider == provider && entry.model == model)
            {
                self.selected = index;
                return;
            }
        }
        self.selected = 0;
    }
}

fn entry_matches(entry: &ModelEntry, query_lower: &str) -> bool {
    if query_lower.is_empty() {
        return true;
    }
    entry.provider.to_lowercase().contains(query_lower)
        || entry.model.to_lowercase().contains(query_lower)
        || entry.label.to_lowercase().contains(query_lower)
}

fn provider_default_model(cfg: &ConfigFile, name: &str) -> Option<String> {
    if let Some(configured) = cfg.providers.get(name) {
        if let Some(model) = configured.default_model.as_deref() {
            let model = model.trim();
            if !model.is_empty() {
                return Some(model.to_string());
            }
        }
    }
    builtin_default_model(name)
}

/// Default model id from [`Provider::builtin`], without building `grok`.
///
/// `Provider::builtin("grok")` fills client headers, which runs `grok --version`
/// inside `block_in_place`. That stalls the first open and panics on a
/// current-thread runtime. The id itself is the builtin literal.
fn builtin_default_model(name: &str) -> Option<String> {
    if name == "grok" {
        return Some("grok-4.7".to_string());
    }
    Provider::builtin(name).map(|provider| provider.default_model)
}

fn push_unique(models: &mut Vec<String>, model: &str) {
    let model = model.trim();
    if model.is_empty() || models.iter().any(|existing| existing == model) {
        return;
    }
    models.push(model.to_string());
}

/// Build a picker from configured and fallback models.
///
/// Providers follow [`huginn_config::known_provider_names`] (sorted name).
/// Within a provider, rows keep curated or fallback order. A configured
/// `default_model`, or the built-in default when config has none, is appended
/// when it is not already listed. The row matching `current_provider` and
/// `current_model` starts selected; otherwise the first row is selected.
pub fn catalog(cfg: &ConfigFile, current_provider: &str, current_model: &str) -> ModelPicker {
    let mut entries = Vec::new();
    for name in huginn_config::known_provider_names(cfg) {
        let mut models = match huginn_config::curated_models(cfg, &name) {
            Some(models) => models,
            None => huginn_config::fallback_models(&name),
        };
        if let Some(default_model) = provider_default_model(cfg, &name) {
            push_unique(&mut models, &default_model);
        }
        let mut seen = Vec::new();
        for model in models {
            let model = model.trim().to_string();
            if model.is_empty() || seen.contains(&model) {
                continue;
            }
            seen.push(model.clone());
            entries.push(ModelEntry::new(&name, model));
        }
    }
    let selected = entries
        .iter()
        .position(|entry| entry.provider == current_provider && entry.model == current_model)
        .unwrap_or(0);
    ModelPicker {
        entries,
        query: String::new(),
        selected,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use huginn_config::ProviderConfig;

    fn entry(provider: &str, model: &str) -> ModelEntry {
        ModelEntry::new(provider, model.to_string())
    }

    fn sample() -> ModelPicker {
        ModelPicker {
            entries: vec![
                entry("grok", "grok-4.7"),
                entry("grok", "grok-4.5"),
                entry("ollama", "qwen3.8:latest"),
            ],
            query: String::new(),
            selected: 0,
        }
    }

    fn models_for(picker: &ModelPicker, provider: &str) -> Vec<String> {
        picker
            .entries
            .iter()
            .filter(|entry| entry.provider == provider)
            .map(|entry| entry.model.clone())
            .collect()
    }

    #[test]
    fn filter_matches_provider_model_and_label_case_insensitively() {
        let mut picker = sample();
        for c in "GROK".chars() {
            picker.push_char(c);
        }
        let providers: Vec<_> = picker
            .filtered()
            .iter()
            .map(|entry| entry.provider.as_str())
            .collect();
        assert_eq!(providers, vec!["grok", "grok"]);

        picker = sample();
        for c in "4.5".chars() {
            picker.push_char(c);
        }
        assert_eq!(
            picker
                .filtered()
                .iter()
                .map(|entry| entry.model.as_str())
                .collect::<Vec<_>>(),
            vec!["grok-4.5"]
        );

        picker = sample();
        for c in "Ollama · QWEN".chars() {
            picker.push_char(c);
        }
        assert_eq!(picker.filtered().len(), 1);
        assert_eq!(picker.filtered()[0].model, "qwen3.8:latest");

        picker = sample();
        picker.push_char('·');
        assert_eq!(picker.filtered().len(), 3);
    }

    #[test]
    fn move_up_and_down_wrap() {
        let mut picker = sample();
        assert_eq!(picker.selected(), 0);
        picker.move_up();
        assert_eq!(picker.selected(), 2);
        picker.move_up();
        assert_eq!(picker.selected(), 1);
        picker.move_down();
        assert_eq!(picker.selected(), 2);
        picker.move_down();
        assert_eq!(picker.selected(), 0);

        let mut single = ModelPicker {
            entries: vec![entry("grok", "grok-4.7")],
            query: String::new(),
            selected: 0,
        };
        single.move_up();
        single.move_down();
        assert_eq!(single.selected(), 0);
    }

    #[test]
    fn confirm_returns_selected_or_none_when_empty() {
        let mut picker = sample();
        picker.move_down();
        let chosen = picker.confirm().expect("row");
        assert_eq!(chosen.provider, "grok");
        assert_eq!(chosen.model, "grok-4.5");
        assert_eq!(chosen.label, "grok · grok-4.5");

        for c in "no-such-model".chars() {
            picker.push_char(c);
        }
        picker.move_up();
        picker.move_down();
        assert!(picker.confirm().is_none());
        assert_eq!(picker.selected(), 0);
        assert!(picker.query().contains("no-such"));
        picker.backspace();
        assert!(!picker.query().ends_with('l'));
    }

    #[test]
    fn selected_stays_in_range_as_the_filter_changes() {
        let mut picker = sample();
        picker.move_down();
        assert_eq!(picker.selected(), 1);
        for c in "grok".chars() {
            picker.push_char(c);
        }
        assert_eq!(picker.filtered().len(), 2);
        assert_eq!(picker.selected(), 1);
        assert_eq!(picker.confirm().unwrap().model, "grok-4.5");

        for c in "zzz".chars() {
            picker.push_char(c);
        }
        assert!(picker.filtered().is_empty());
        assert_eq!(picker.selected(), 0);
        assert!(picker.confirm().is_none());
    }

    #[test]
    fn filter_follows_the_highlighted_row_when_earlier_rows_drop() {
        let mut picker = ModelPicker {
            entries: vec![
                entry("ollama", "gemma"),
                entry("grok", "grok-4.7"),
                entry("openrouter", "x-ai/grok-4.7"),
            ],
            query: String::new(),
            selected: 1,
        };
        for c in "grok".chars() {
            picker.push_char(c);
        }
        let chosen = picker.confirm().expect("highlighted row still matches");
        assert_eq!(chosen.provider, "grok");
        assert_eq!(chosen.model, "grok-4.7");

        while !picker.query().is_empty() {
            picker.backspace();
        }
        assert_eq!(picker.confirm().expect("row").model, "grok-4.7");

        // The highlighted row drops out. Select the first row that remains.
        for c in "x-ai".chars() {
            picker.push_char(c);
        }
        let chosen = picker.confirm().expect("openrouter still matches");
        assert_eq!(chosen.provider, "openrouter");
        assert_eq!(chosen.model, "x-ai/grok-4.7");
    }

    #[test]
    fn catalog_keeps_provider_name_order_and_curated_model_order() {
        let mut cfg = ConfigFile::default();
        cfg.providers.insert(
            "grok".into(),
            ProviderConfig {
                default_model: Some("grok-custom".into()),
                models: vec![
                    "grok-4.7".into(),
                    "grok-4.7".into(),
                    "  ".into(),
                    "grok-4.5".into(),
                ],
                ..Default::default()
            },
        );
        cfg.providers.insert(
            "openrouter".into(),
            ProviderConfig {
                default_model: Some("custom-or".into()),
                models: vec!["z-model".into(), "a-model".into()],
                ..Default::default()
            },
        );

        let picker = catalog(&cfg, "openrouter", "a-model");
        for entry in &picker.entries {
            assert_eq!(entry.label, format!("{} · {}", entry.provider, entry.model));
        }
        assert_eq!(
            models_for(&picker, "grok"),
            vec![
                "grok-4.7".to_string(),
                "grok-4.5".to_string(),
                "grok-custom".to_string()
            ]
        );
        assert_eq!(
            models_for(&picker, "openrouter"),
            vec![
                "z-model".to_string(),
                "a-model".to_string(),
                "custom-or".to_string()
            ]
        );
        assert_eq!(
            models_for(&picker, "ollama"),
            huginn_config::fallback_models("ollama")
        );
        assert_eq!(
            models_for(&picker, "opencode-go"),
            huginn_config::fallback_models("opencode-go")
        );

        let mut providers = Vec::new();
        for entry in &picker.entries {
            if providers.last() != Some(&entry.provider) {
                providers.push(entry.provider.clone());
            }
        }
        assert_eq!(providers, huginn_config::known_provider_names(&cfg));

        let chosen = picker.confirm().expect("current row");
        assert_eq!(chosen.provider, "openrouter");
        assert_eq!(chosen.model, "a-model");

        let missing = catalog(&cfg, "openrouter", "missing");
        assert_eq!(missing.selected(), 0);
    }

    #[test]
    fn builtin_default_models_match_provider_presets() {
        for name in ["ollama", "openrouter", "opencode-go", "grok"] {
            let builtin = Provider::builtin(name).expect(name);
            assert_eq!(
                builtin_default_model(name).as_deref(),
                Some(builtin.default_model.as_str()),
                "{name}"
            );
        }
        assert!(builtin_default_model("custom").is_none());
    }
}
