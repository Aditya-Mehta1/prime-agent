//! Model subsystem: resolver and Prime Inference private models.

pub(crate) mod custom;
pub(crate) mod prime_inference;
pub(crate) mod resolver;

pub use custom::{
    apply_model_override, load_custom_models, merge_compat, parse_models_config,
    strip_json_comments, validate_config, CustomModelsResult, ModelOverride, ModelsConfig,
    ProviderOverride,
};
pub use prime_inference::{
    is_private_prime_inference_model, is_private_prime_inference_model_id,
    private_prime_inference_models, PRIME_INFERENCE_BASE_URL,
};
pub use resolver::{
    build_fallback_model, find_exact_model_reference_match, find_preferred_default_model,
    resolve_cli_model, resolve_model_scope_from_models, ResolveCliModelResult, ScopedModel,
    PRIME_INFERENCE_DEFAULT_MODEL_ID,
};
