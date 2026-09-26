use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Upper bound on a custom provider's display name (characters, not bytes).
pub const MAX_PROVIDER_NAME_CHARS: usize = 64;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct ProviderConfig {
    /// Stable identity of this connection profile. Built-in providers use their
    /// preset key ("openai", "deepseek", "qwen", "volcano", "custom");
    /// additional custom providers get a generated `custom-<uuid>` id. Older
    /// configs that predate this field leave it empty and are normalized to
    /// `preset.key_id()` by [`Self::ensure_id`] during `Config::load`. Session
    /// rows, keyring entries and child-agent resolution address a provider by
    /// this id, never by the `preset` template.
    pub id: String,
    /// User-assigned display name for a custom provider. Empty for built-ins and
    /// for legacy profiles, which fall back to the preset label. Names are the
    /// user-facing handle; `id` remains the machine identity.
    pub name: String,
    /// The provider template/family this profile is built from. Drives request
    /// protocol defaults, thinking profiles, selectable models and the built-in
    /// context-window registry. It is no longer the profile's identity.
    pub preset: ProviderPreset,
    pub kind: ProviderKind,
    pub base_url: String,
    pub model: String,
    pub use_previous_response_id: bool,
    pub native_web_search: NativeWebSearch,
    pub context_window_tokens: Option<u64>,
    /// Hard cap on the model's output tokens. When set it is both the output
    /// reservation subtracted from the safe input budget and the per-request
    /// provider limit (`max_output_tokens` on Responses, `max_completion_tokens`
    /// on OpenAI chat, `max_tokens` on other chat providers). `None` lets the
    /// provider pick its default.
    pub max_output_tokens: Option<u32>,
    pub thinking: ThinkingCapability,
    pub thinking_level: ThinkingLevel,
    pub thinking_budget_tokens: Option<u32>,
    /// Models the user explicitly enabled for this provider. Empty means
    /// "unrestricted" (every discovered/registry model is offered). Reserved for
    /// the selectable-model picker: the field round-trips through config and the
    /// protocol DTO, but is not yet enforced when building requests.
    pub enabled_models: Vec<String>,
    /// Max HTTP-level retry attempts before giving up. 0 disables retries.
    pub retry_max_attempts: u32,
    pub retry_initial_backoff_ms: u64,
    pub retry_max_backoff_ms: u64,
    /// Runtime-discovered metadata (provider `/models` or models.dev),
    /// stamped by the engine from the `model_metadata` cache. Never
    /// serialized: it is rebuilt after every restart or provider switch, and
    /// an explicit `context_window_tokens` / `max_output_tokens` always wins
    /// over it (see `model_meta::resolve`).
    #[serde(skip)]
    pub discovered: Option<crate::model_meta::DiscoveredMeta>,
}

/// Runtime model metadata discovery: `GET {base_url}/models` and the
/// models.dev community database. All fetching is event-driven (provider
/// switch, first submit with an unknown window, manual refresh); there is no
/// polling and the process never touches the network at startup.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct ModelMetadataConfig {
    /// Master switch for runtime metadata fetches. `false` keeps the process
    /// fully offline: only the explicit config, the committed models.dev
    /// snapshot, and the built-in registry are consulted.
    pub fetch: bool,
    /// How long a cached fetch stays fresh before a trigger refreshes it.
    pub ttl_hours: u64,
    /// Per-request timeout for metadata fetches.
    pub timeout_ms: u64,
}

impl Default for ModelMetadataConfig {
    fn default() -> Self {
        Self {
            fetch: true,
            ttl_hours: 24,
            timeout_ms: 5000,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingLevel {
    #[default]
    Auto,
    None,
    Minimal,
    Low,
    Medium,
    High,
    XHigh,
    Max,
    Enabled,
}

impl ThinkingLevel {
    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::None => "none",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
            Self::Enabled => "开启",
        }
    }

    pub fn menu_label(self) -> &'static str {
        match self {
            Self::Auto => "自动",
            Self::None => "关闭",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
            Self::Enabled => "开启",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThinkingProfileKind {
    OpenAi,
    Qwen38,
    Qwen37,
    DeepSeekPro,
    DeepSeekFlash,
    Volcano,
    Compatible,
}

#[derive(Clone, Copy, Debug)]
pub struct ThinkingProfile {
    pub options: &'static [ThinkingLevel],
    pub default: ThinkingLevel,
    pub kind: ThinkingProfileKind,
}

const OPENAI_LEVELS: &[ThinkingLevel] = &[
    ThinkingLevel::Auto,
    ThinkingLevel::None,
    ThinkingLevel::Minimal,
    ThinkingLevel::Low,
    ThinkingLevel::Medium,
    ThinkingLevel::High,
    ThinkingLevel::XHigh,
    ThinkingLevel::Max,
];
const QWEN38_LEVELS: &[ThinkingLevel] = &[
    ThinkingLevel::None,
    ThinkingLevel::Low,
    ThinkingLevel::Medium,
    ThinkingLevel::XHigh,
];
const QWEN37_LEVELS: &[ThinkingLevel] = &[ThinkingLevel::None, ThinkingLevel::Enabled];
const DEEPSEEK_PRO_LEVELS: &[ThinkingLevel] = &[
    ThinkingLevel::Low,
    ThinkingLevel::High,
    ThinkingLevel::XHigh,
    ThinkingLevel::Max,
];
const DEEPSEEK_FLASH_LEVELS: &[ThinkingLevel] = &[
    ThinkingLevel::Low,
    ThinkingLevel::High,
    ThinkingLevel::XHigh,
    ThinkingLevel::Max,
];
const VOLCANO_LEVELS: &[ThinkingLevel] = &[ThinkingLevel::High];
const COMPATIBLE_LEVELS: &[ThinkingLevel] = &[
    ThinkingLevel::Auto,
    ThinkingLevel::None,
    ThinkingLevel::Low,
    ThinkingLevel::Medium,
    ThinkingLevel::High,
    ThinkingLevel::Max,
];

pub fn thinking_profile(preset: ProviderPreset, model: &str) -> ThinkingProfile {
    let model = model
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect::<String>();
    if preset == ProviderPreset::Qwen && model.contains("qwen38") {
        ThinkingProfile {
            options: QWEN38_LEVELS,
            default: ThinkingLevel::XHigh,
            kind: ThinkingProfileKind::Qwen38,
        }
    } else if preset == ProviderPreset::Qwen && model.contains("qwen37") {
        ThinkingProfile {
            options: QWEN37_LEVELS,
            default: ThinkingLevel::Enabled,
            kind: ThinkingProfileKind::Qwen37,
        }
    } else if preset == ProviderPreset::DeepSeek && model.contains("v4pro") {
        ThinkingProfile {
            options: DEEPSEEK_PRO_LEVELS,
            default: ThinkingLevel::High,
            kind: ThinkingProfileKind::DeepSeekPro,
        }
    } else if preset == ProviderPreset::DeepSeek && model.contains("v4flash") {
        ThinkingProfile {
            options: DEEPSEEK_FLASH_LEVELS,
            default: ThinkingLevel::High,
            kind: ThinkingProfileKind::DeepSeekFlash,
        }
    } else if preset == ProviderPreset::Volcano {
        ThinkingProfile {
            options: VOLCANO_LEVELS,
            default: ThinkingLevel::High,
            kind: ThinkingProfileKind::Volcano,
        }
    } else if preset == ProviderPreset::OpenAi {
        ThinkingProfile {
            options: OPENAI_LEVELS,
            default: ThinkingLevel::Auto,
            kind: ThinkingProfileKind::OpenAi,
        }
    } else {
        ThinkingProfile {
            options: COMPATIBLE_LEVELS,
            default: ThinkingLevel::Auto,
            kind: ThinkingProfileKind::Compatible,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingCapability {
    #[default]
    Auto,
    OpenAi,
    DeepSeek,
    Qwen,
    Volcano,
    Compatible,
    Disabled,
}

impl ThinkingCapability {
    pub const ALL: [Self; 7] = [
        Self::Auto,
        Self::OpenAi,
        Self::DeepSeek,
        Self::Qwen,
        Self::Volcano,
        Self::Compatible,
        Self::Disabled,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "自动",
            Self::OpenAi => "OpenAI 摘要",
            Self::DeepSeek => "DeepSeek",
            Self::Qwen => "Qwen",
            Self::Volcano => "火山方舟",
            Self::Compatible => "兼容解析",
            Self::Disabled => "关闭",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum NativeWebSearch {
    #[default]
    Auto,
    Enabled,
    Disabled,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ProviderPreset {
    #[default]
    OpenAi,
    DeepSeek,
    Qwen,
    Volcano,
    Custom,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    ChatCompletions,
    #[default]
    Responses,
}

impl ProviderKind {
    pub const ALL: [Self; 2] = [Self::Responses, Self::ChatCompletions];

    /// Stable wire tag, identical to the serde form, used by the provider
    /// settings DTOs and the set-provider endpoint.
    pub fn wire_tag(self) -> &'static str {
        match self {
            Self::ChatCompletions => "chat_completions",
            Self::Responses => "responses",
        }
    }

    /// Inverse of [`Self::wire_tag`].
    pub fn parse_wire_tag(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.wire_tag() == value)
    }
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            // Intentionally empty: `#[serde(default)]` copies these struct
            // defaults into every missing field, so a hard-coded built-in id
            // here would wrongly stamp `id = "openai"` onto configs whose
            // `preset` says otherwise. `ensure_id()` derives the real id from
            // the parsed preset instead, and `id()` falls back for direct
            // `Default` users that never call it.
            id: String::new(),
            name: String::new(),
            preset: ProviderPreset::OpenAi,
            kind: ProviderKind::Responses,
            base_url: "https://api.openai.com/v1".into(),
            model: "gpt-5-mini".into(),
            use_previous_response_id: false,
            native_web_search: NativeWebSearch::Auto,
            context_window_tokens: None,
            max_output_tokens: None,
            thinking: ThinkingCapability::Auto,
            thinking_level: ThinkingLevel::Auto,
            thinking_budget_tokens: None,
            enabled_models: Vec::new(),
            retry_max_attempts: 3,
            retry_initial_backoff_ms: 500,
            retry_max_backoff_ms: 8000,
            discovered: None,
        }
    }
}

impl ProviderConfig {
    /// Fills in a missing id from the preset key, then trims it. Older configs
    /// (and any profile created before ids existed) are stamped to their preset
    /// key, so a single legacy `custom` profile keeps the "custom" id and its
    /// existing keyring entry.
    pub fn ensure_id(&mut self) {
        let current = std::mem::take(&mut self.id);
        let trimmed = current.trim();
        self.id = if trimmed.is_empty() {
            self.preset.key_id().to_owned()
        } else {
            trimmed.to_owned()
        };
    }

    /// The id used for keyring entries, session rows and cross-provider
    /// resolution. Always non-empty after [`Self::ensure_id`].
    pub fn id(&self) -> &str {
        if self.id.is_empty() {
            self.preset.key_id()
        } else {
            &self.id
        }
    }

    /// Human-facing label: the custom name when set, else the preset label.
    pub fn display_label(&self) -> &str {
        let name = self.name.trim();
        if name.is_empty() {
            self.preset.label()
        } else {
            name
        }
    }

    /// Whether this profile is a user-defined custom connection, which may be
    /// instantiated many times (unlike the one-profile-per-preset built-ins).
    pub fn is_custom(&self) -> bool {
        self.preset == ProviderPreset::Custom
    }

    /// Generates a fresh id for a new custom provider. The `custom-<hex>` shape
    /// keeps generated ids distinguishable from built-in preset keys.
    pub fn new_custom_id() -> String {
        format!("custom-{}", uuid::Uuid::new_v4().simple())
    }

    /// Validates the display name shape when one is set. Empty names are
    /// permitted so legacy unnamed profiles keep loading; callers that require a
    /// name for a *new* custom provider enforce that separately.
    pub fn validate_name(&self) -> Result<()> {
        let name = self.name.trim();
        if name.is_empty() {
            return Ok(());
        }
        if name.chars().count() > MAX_PROVIDER_NAME_CHARS {
            anyhow::bail!("provider name must be at most {MAX_PROVIDER_NAME_CHARS} characters");
        }
        if name.chars().any(|c| c.is_control()) {
            anyhow::bail!("provider name must not contain control characters");
        }
        Ok(())
    }

    pub fn normalize_thinking(&mut self) {
        let profile = thinking_profile(self.preset, &self.model);
        if !profile.options.contains(&self.thinking_level) {
            self.thinking_level = profile.default;
        }
        if profile.kind != ThinkingProfileKind::Qwen37
            || self.thinking_level != ThinkingLevel::Enabled
            || !matches!(
                self.thinking_budget_tokens,
                None | Some(1024 | 4096 | 8192 | 16384 | 32768)
            )
        {
            self.thinking_budget_tokens = None;
        }
    }
    /// The model's context window, resolved through the metadata chain
    /// (`model_meta::resolve`): explicit `context_window_tokens` >
    /// runtime-discovered metadata (provider `/models`, models.dev) > the
    /// built-in registry. Returns `None` for models nothing knows, so no
    /// request is sent against an uncertain default window — an unknown model
    /// must set `context_window_tokens` explicitly.
    pub fn resolved_context_window_tokens(&self) -> Option<u64> {
        crate::model_meta::resolve(self).context_window_tokens
    }

    pub fn validate(&mut self) -> Result<()> {
        self.base_url = self.base_url.trim().trim_end_matches('/').to_owned();
        self.model = self.model.trim().to_owned();
        self.name = self.name.trim().to_owned();
        self.validate_name()?;
        if self.base_url.contains('{') || self.base_url.contains('}') {
            anyhow::bail!("replace placeholders in the provider Base URL");
        }
        let url = url::Url::parse(&self.base_url).context("provider Base URL is invalid")?;
        if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
            anyhow::bail!("provider Base URL must be HTTP or HTTPS with a host");
        }
        if !url.username().is_empty() || url.password().is_some() {
            anyhow::bail!("provider Base URL must not contain credentials");
        }
        if self.model.is_empty() {
            anyhow::bail!("model must not be empty");
        }
        if !self.preset.supports_responses() {
            self.kind = ProviderKind::ChatCompletions;
            self.use_previous_response_id = false;
        }
        if !self.preset.supports_previous_response_id() {
            self.use_previous_response_id = false;
        }
        Ok(())
    }
}

impl ProviderPreset {
    pub const ALL: [Self; 5] = [
        Self::OpenAi,
        Self::DeepSeek,
        Self::Qwen,
        Self::Volcano,
        Self::Custom,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::OpenAi => "OpenAI",
            Self::DeepSeek => "DeepSeek",
            Self::Qwen => "Qwen / Bailian",
            Self::Volcano => "Volcano Ark",
            Self::Custom => "Custom compatible",
        }
    }

    pub fn key_id(self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
            Self::DeepSeek => "deepseek",
            Self::Qwen => "qwen",
            Self::Volcano => "volcano",
            Self::Custom => "custom",
        }
    }

    pub fn defaults(self) -> ProviderConfig {
        let (kind, base_url, model) = match self {
            Self::OpenAi => (
                ProviderKind::Responses,
                "https://api.openai.com/v1",
                "gpt-5-mini",
            ),
            Self::DeepSeek => (
                ProviderKind::Responses,
                "https://api.deepseek.com",
                "deepseek-v4-flash",
            ),
            Self::Qwen => (
                ProviderKind::ChatCompletions,
                "https://{WorkspaceId}.cn-beijing.maas.aliyuncs.com/compatible-mode/v1",
                "qwen3.8-max",
            ),
            Self::Volcano => (
                ProviderKind::ChatCompletions,
                "https://ark.cn-beijing.volces.com/api/v3",
                "doubao-seed-2-1-pro-260628",
            ),
            Self::Custom => (
                ProviderKind::ChatCompletions,
                "https://api.example.com/v1",
                "model-name",
            ),
        };
        let mut config = ProviderConfig {
            id: self.key_id().to_owned(),
            name: String::new(),
            preset: self,
            kind,
            base_url: base_url.into(),
            model: model.into(),
            use_previous_response_id: false,
            native_web_search: NativeWebSearch::Auto,
            context_window_tokens: None,
            max_output_tokens: None,
            thinking: ThinkingCapability::Auto,
            thinking_level: ThinkingLevel::Auto,
            thinking_budget_tokens: None,
            enabled_models: Vec::new(),
            retry_max_attempts: 3,
            retry_initial_backoff_ms: 500,
            retry_max_backoff_ms: 8000,
            discovered: None,
        };
        config.normalize_thinking();
        config
    }

    pub fn supports_responses(self) -> bool {
        matches!(
            self,
            Self::OpenAi | Self::DeepSeek | Self::Qwen | Self::Custom
        )
    }

    pub fn supports_previous_response_id(self) -> bool {
        !matches!(self, Self::DeepSeek)
    }

    /// Candidate models offered by the settings picker. `Custom` returns an
    /// empty list, so its model must be typed manually. Kept separate from the
    /// context-window lookup tables: those serve token estimation, not choice.
    pub fn selectable_models(self) -> &'static [&'static str] {
        match self {
            Self::OpenAi => OPENAI_SELECTABLE_MODELS,
            Self::DeepSeek => DEEPSEEK_SELECTABLE_MODELS,
            Self::Qwen => QWEN_SELECTABLE_MODELS,
            Self::Volcano => VOLCANO_SELECTABLE_MODELS,
            Self::Custom => &[],
        }
    }

    /// Inverse of `key_id`, used to recover a preset from its stored form.
    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|preset| preset.key_id() == value)
    }
}

const OPENAI_SELECTABLE_MODELS: &[&str] = &[
    "gpt-5-mini",
    "gpt-5",
    "gpt-5.6-sol",
    "gpt-5.6-terra",
    "gpt-5.6-luna",
    "o1",
    "o3",
    "o4-mini",
    "gpt-4o",
    "gpt-4.1",
    "o1-mini",
    "o1-preview",
];

const DEEPSEEK_SELECTABLE_MODELS: &[&str] = &[
    "deepseek-v4-flash",
    "deepseek-v4-pro",
    "deepseek-chat",
    "deepseek-reasoner",
];

const QWEN_SELECTABLE_MODELS: &[&str] = &[
    "qwen3.8-max",
    "qwen3.7-max",
    "qwen-plus",
    "qwen-max",
    "qwen-turbo",
    "qwen-long",
];

const VOLCANO_SELECTABLE_MODELS: &[&str] =
    &["doubao-seed-2-1-pro-260628", "deepseek-v4-flash", "glm-5.2"];

#[derive(Clone, Copy)]
struct ModelRule {
    model: &'static str,
    context_window_tokens: u64,
    /// Documented output cap when publicly known; `None` leaves the reserve
    /// to explicit config or discovered metadata.
    max_output_tokens: Option<u32>,
}

const OPENAI_EXACT_MODELS: &[ModelRule] = &[
    ModelRule {
        model: "o1-mini",
        context_window_tokens: 128_000,
        max_output_tokens: Some(65_536),
    },
    ModelRule {
        model: "o1-preview",
        context_window_tokens: 128_000,
        max_output_tokens: Some(65_536),
    },
    ModelRule {
        model: "o1",
        context_window_tokens: 200_000,
        max_output_tokens: Some(100_000),
    },
    ModelRule {
        model: "o3",
        context_window_tokens: 200_000,
        max_output_tokens: Some(100_000),
    },
    ModelRule {
        model: "o4-mini",
        context_window_tokens: 200_000,
        max_output_tokens: Some(100_000),
    },
    ModelRule {
        model: "gpt-5.6-sol",
        context_window_tokens: 1_050_000,
        max_output_tokens: None,
    },
    ModelRule {
        model: "gpt-5.6-terra",
        context_window_tokens: 1_050_000,
        max_output_tokens: None,
    },
    ModelRule {
        model: "gpt-5.6-luna",
        context_window_tokens: 1_050_000,
        max_output_tokens: None,
    },
];

const OPENAI_PREFIX_MODELS: &[ModelRule] = &[
    ModelRule {
        model: "o1-mini",
        context_window_tokens: 128_000,
        max_output_tokens: Some(65_536),
    },
    ModelRule {
        model: "o1-preview",
        context_window_tokens: 128_000,
        max_output_tokens: Some(65_536),
    },
    ModelRule {
        model: "o1",
        context_window_tokens: 200_000,
        max_output_tokens: Some(100_000),
    },
    ModelRule {
        model: "o3",
        context_window_tokens: 200_000,
        max_output_tokens: Some(100_000),
    },
    ModelRule {
        model: "o4-mini",
        context_window_tokens: 200_000,
        max_output_tokens: Some(100_000),
    },
    ModelRule {
        model: "gpt-5.6-sol",
        context_window_tokens: 1_050_000,
        max_output_tokens: None,
    },
    ModelRule {
        model: "gpt-5.6-terra",
        context_window_tokens: 1_050_000,
        max_output_tokens: None,
    },
    ModelRule {
        model: "gpt-5.6-luna",
        context_window_tokens: 1_050_000,
        max_output_tokens: None,
    },
    ModelRule {
        model: "gpt-4.1",
        context_window_tokens: 1_047_576,
        max_output_tokens: Some(32_768),
    },
    ModelRule {
        model: "gpt-4o",
        context_window_tokens: 128_000,
        max_output_tokens: Some(16_384),
    },
    ModelRule {
        model: "gpt-5",
        context_window_tokens: 400_000,
        max_output_tokens: Some(128_000),
    },
];

const DEEPSEEK_EXACT_MODELS: &[ModelRule] = &[
    ModelRule {
        model: "deepseek-chat",
        context_window_tokens: 128_000,
        max_output_tokens: Some(8_192),
    },
    ModelRule {
        model: "deepseek-reasoner",
        context_window_tokens: 128_000,
        max_output_tokens: Some(32_768),
    },
    ModelRule {
        model: "deepseek-v4-pro",
        context_window_tokens: 1_000_000,
        max_output_tokens: None,
    },
    ModelRule {
        model: "deepseek-v4-flash",
        context_window_tokens: 1_000_000,
        max_output_tokens: None,
    },
];

const DEEPSEEK_PREFIX_MODELS: &[ModelRule] = &[
    ModelRule {
        model: "deepseek-r1",
        context_window_tokens: 128_000,
        max_output_tokens: Some(32_768),
    },
    ModelRule {
        model: "deepseek-v3",
        context_window_tokens: 128_000,
        max_output_tokens: Some(8_192),
    },
    ModelRule {
        model: "deepseek-v4-pro",
        context_window_tokens: 1_000_000,
        max_output_tokens: None,
    },
    ModelRule {
        model: "deepseek-v4-flash",
        context_window_tokens: 1_000_000,
        max_output_tokens: None,
    },
];

const QWEN_EXACT_MODELS: &[ModelRule] = &[
    ModelRule {
        model: "qwen-max",
        context_window_tokens: 32_768,
        max_output_tokens: Some(8_192),
    },
    ModelRule {
        model: "qwen-plus",
        context_window_tokens: 131_072,
        max_output_tokens: Some(8_192),
    },
    ModelRule {
        model: "qwen-turbo",
        context_window_tokens: 1_000_000,
        max_output_tokens: Some(8_192),
    },
    ModelRule {
        model: "qwen-long",
        context_window_tokens: 1_000_000,
        max_output_tokens: None,
    },
    ModelRule {
        model: "qwen3.8-max",
        context_window_tokens: 1_000_000,
        max_output_tokens: None,
    },
    ModelRule {
        model: "qwen3.7-max",
        context_window_tokens: 1_000_000,
        max_output_tokens: None,
    },
    ModelRule {
        model: "qwen3.7-plus",
        context_window_tokens: 1_000_000,
        max_output_tokens: None,
    },
    ModelRule {
        model: "qwen3.7-flash",
        context_window_tokens: 1_000_000,
        max_output_tokens: None,
    },
];

const QWEN_PREFIX_MODELS: &[ModelRule] = &[
    ModelRule {
        model: "qwen-max",
        context_window_tokens: 32_768,
        max_output_tokens: Some(8_192),
    },
    ModelRule {
        model: "qwen-plus",
        context_window_tokens: 131_072,
        max_output_tokens: Some(8_192),
    },
    ModelRule {
        model: "qwen-turbo",
        context_window_tokens: 1_000_000,
        max_output_tokens: Some(8_192),
    },
    ModelRule {
        model: "qwen-long",
        context_window_tokens: 1_000_000,
        max_output_tokens: None,
    },
    ModelRule {
        model: "qwen3.8-max",
        context_window_tokens: 1_000_000,
        max_output_tokens: None,
    },
    ModelRule {
        model: "qwen3.7-max",
        context_window_tokens: 1_000_000,
        max_output_tokens: None,
    },
    ModelRule {
        model: "qwen3.7-plus",
        context_window_tokens: 1_000_000,
        max_output_tokens: None,
    },
    ModelRule {
        model: "qwen3.7-flash",
        context_window_tokens: 1_000_000,
        max_output_tokens: None,
    },
    ModelRule {
        model: "qwen2.5",
        context_window_tokens: 131_072,
        max_output_tokens: Some(8_192),
    },
    ModelRule {
        model: "qwen3",
        context_window_tokens: 131_072,
        max_output_tokens: Some(32_768),
    },
];

const VOLCANO_PREFIX_MODELS: &[ModelRule] = &[
    ModelRule {
        model: "doubao-seed",
        context_window_tokens: 256_000,
        max_output_tokens: None,
    },
    ModelRule {
        model: "deepseek-v4-flash",
        context_window_tokens: 1_000_000,
        max_output_tokens: None,
    },
    ModelRule {
        model: "glm-5.2",
        context_window_tokens: 1_000_000,
        max_output_tokens: None,
    },
    ModelRule {
        model: "deepseek-v4-pro",
        context_window_tokens: 200_000,
        max_output_tokens: None,
    },
    ModelRule {
        model: "glm-4.7",
        context_window_tokens: 200_000,
        max_output_tokens: None,
    },
    ModelRule {
        model: "minimax-m2.7",
        context_window_tokens: 200_000,
        max_output_tokens: None,
    },
    ModelRule {
        model: "minimax-m2.5",
        context_window_tokens: 200_000,
        max_output_tokens: None,
    },
    ModelRule {
        model: "doubao-seed-2.0-pro",
        context_window_tokens: 256_000,
        max_output_tokens: None,
    },
    ModelRule {
        model: "doubao-seed-2.0-code",
        context_window_tokens: 256_000,
        max_output_tokens: None,
    },
    ModelRule {
        model: "doubao-seed-2.0-lite",
        context_window_tokens: 256_000,
        max_output_tokens: None,
    },
    ModelRule {
        model: "kimi-k2.6",
        context_window_tokens: 256_000,
        max_output_tokens: None,
    },
    ModelRule {
        model: "kimi-k2.5",
        context_window_tokens: 256_000,
        max_output_tokens: None,
    },
    ModelRule {
        model: "doubao-1-5-pro-32k",
        context_window_tokens: 32_000,
        max_output_tokens: None,
    },
    ModelRule {
        model: "doubao-1-5-lite-32k",
        context_window_tokens: 32_000,
        max_output_tokens: None,
    },
    ModelRule {
        model: "doubao-1-5-pro-128k",
        context_window_tokens: 128_000,
        max_output_tokens: None,
    },
    ModelRule {
        model: "doubao-1-5-lite-128k",
        context_window_tokens: 128_000,
        max_output_tokens: None,
    },
    ModelRule {
        model: "doubao-1-5-pro-256k",
        context_window_tokens: 256_000,
        max_output_tokens: None,
    },
    ModelRule {
        model: "doubao-1-5-lite-256k",
        context_window_tokens: 256_000,
        max_output_tokens: None,
    },
    ModelRule {
        model: "doubao-1-6-pro-32k",
        context_window_tokens: 32_000,
        max_output_tokens: None,
    },
    ModelRule {
        model: "doubao-1-6-lite-32k",
        context_window_tokens: 32_000,
        max_output_tokens: None,
    },
    ModelRule {
        model: "doubao-1-6-pro-128k",
        context_window_tokens: 128_000,
        max_output_tokens: None,
    },
    ModelRule {
        model: "doubao-1-6-lite-128k",
        context_window_tokens: 128_000,
        max_output_tokens: None,
    },
    ModelRule {
        model: "doubao-1-6-pro-256k",
        context_window_tokens: 256_000,
        max_output_tokens: None,
    },
    ModelRule {
        model: "doubao-1-6-lite-256k",
        context_window_tokens: 256_000,
        max_output_tokens: None,
    },
    ModelRule {
        model: "doubao-pro-32k",
        context_window_tokens: 32_000,
        max_output_tokens: None,
    },
    ModelRule {
        model: "doubao-lite-32k",
        context_window_tokens: 32_000,
        max_output_tokens: None,
    },
    ModelRule {
        model: "doubao-pro-128k",
        context_window_tokens: 128_000,
        max_output_tokens: None,
    },
    ModelRule {
        model: "doubao-lite-128k",
        context_window_tokens: 128_000,
        max_output_tokens: None,
    },
    ModelRule {
        model: "doubao-pro-256k",
        context_window_tokens: 256_000,
        max_output_tokens: None,
    },
    ModelRule {
        model: "doubao-lite-256k",
        context_window_tokens: 256_000,
        max_output_tokens: None,
    },
];

/// Built-in registry window lookup (the L4 tier of `model_meta::resolve`).
pub(crate) fn known_context_window(preset: ProviderPreset, model: &str) -> Option<u64> {
    known_model_rule(preset, model).map(|rule| rule.context_window_tokens)
}

/// Built-in registry output-cap lookup (L4 of `model_meta::resolve`).
pub(crate) fn known_max_output(preset: ProviderPreset, model: &str) -> Option<u32> {
    known_model_rule(preset, model).and_then(|rule| rule.max_output_tokens)
}

fn known_model_rule(preset: ProviderPreset, model: &str) -> Option<ModelRule> {
    let model = model.trim().to_ascii_lowercase();
    match preset {
        ProviderPreset::OpenAi => {
            lookup_model_rule(&model, OPENAI_EXACT_MODELS, OPENAI_PREFIX_MODELS)
        }
        ProviderPreset::DeepSeek => {
            lookup_model_rule(&model, DEEPSEEK_EXACT_MODELS, DEEPSEEK_PREFIX_MODELS)
        }
        ProviderPreset::Qwen => lookup_model_rule(&model, QWEN_EXACT_MODELS, QWEN_PREFIX_MODELS),
        ProviderPreset::Volcano => lookup_model_rule(&model, &[], VOLCANO_PREFIX_MODELS),
        ProviderPreset::Custom => {
            lookup_model_rule(&model, OPENAI_EXACT_MODELS, OPENAI_PREFIX_MODELS)
                .or_else(|| {
                    lookup_model_rule(&model, DEEPSEEK_EXACT_MODELS, DEEPSEEK_PREFIX_MODELS)
                })
                .or_else(|| lookup_model_rule(&model, QWEN_EXACT_MODELS, QWEN_PREFIX_MODELS))
                .or_else(|| lookup_model_rule(&model, &[], VOLCANO_PREFIX_MODELS))
        }
    }
    .copied()
}

fn lookup_model_rule<'a>(
    model: &str,
    exact: &'a [ModelRule],
    prefixes: &'a [ModelRule],
) -> Option<&'a ModelRule> {
    exact.iter().find(|rule| model == rule.model).or_else(|| {
        prefixes
            .iter()
            .filter(|rule| model_family_matches(model, rule.model))
            .max_by_key(|rule| rule.model.len())
    })
}

fn model_family_matches(model: &str, family: &str) -> bool {
    model == family
        || model
            .strip_prefix(family)
            .is_some_and(|suffix| suffix.starts_with(['-', '.', ':']))
}

impl ProviderKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::ChatCompletions => "Chat Completions",
            Self::Responses => "Responses",
        }
    }
}
