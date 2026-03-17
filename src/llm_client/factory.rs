use std::sync::Arc;

use super::legacy::GeminiClient;
use super::openai_compat::OpenAiCompatClient;
use super::policy::estimate_context_window;
use super::protocol::{GeminiPlatform, LlmClient};

pub fn create_llm_client(
    provider: &str,
    model: Option<String>,
    platform_override: Option<String>,
    config: &crate::config::AppConfig,
) -> Result<Arc<dyn LlmClient>, String> {
    if let Some(prov_config) = config.get_provider(provider) {
        tracing::info!("Initializing provider '{}' from config", provider);
        match prov_config.type_name.as_str() {
            "openai_compat" | "aliyun" => {
                let raw_api_key = if let Some(env_var) = &prov_config.api_key_env {
                    std::env::var(env_var).or_else(|_| {
                        prov_config.api_key.clone().ok_or_else(|| {
                            format!("API key not found in env var '{}' or config", env_var)
                        })
                    })?
                } else {
                    prov_config
                        .api_key
                        .clone()
                        .ok_or_else(|| "API key must be provided in config".to_string())?
                };
                let api_key = raw_api_key.trim().to_string();

                let base_url = prov_config
                    .base_url
                    .clone()
                    .ok_or_else(|| "base_url required for openai_compat".to_string())?;
                let model_final = model
                    .or(prov_config.model.clone())
                    .unwrap_or_else(|| "gpt-3.5-turbo".to_string());

                let context_window = prov_config
                    .context_window
                    .unwrap_or_else(|| estimate_context_window(&model_final));

                Ok(Arc::new(OpenAiCompatClient::new_with_window(
                    api_key,
                    base_url,
                    model_final,
                    provider.to_string(),
                    context_window,
                    prov_config.reasoning_effort.clone(),
                )))
            }
            "gemini" => {
                let raw_api_key = if let Some(env_var) = &prov_config.api_key_env {
                    std::env::var(env_var).or_else(|_| {
                        prov_config.api_key.clone().ok_or_else(|| {
                            format!("API key not found in env var '{}' or config", env_var)
                        })
                    })?
                } else {
                    prov_config
                        .api_key
                        .clone()
                        .ok_or_else(|| "API key must be provided in config".to_string())?
                };
                let api_key = raw_api_key.trim().to_string();
                let model_final = model.or(prov_config.model.clone());
                let model_str = model_final
                    .clone()
                    .unwrap_or_else(|| "gemini-3.1-pro-preview".to_string());
                let context_window = prov_config
                    .context_window
                    .unwrap_or_else(|| estimate_context_window(&model_str));

                let platform = match platform_override.as_deref() {
                    Some("vertex") => GeminiPlatform::Vertex,
                    Some("gen") => GeminiPlatform::Gen,
                    _ => match prov_config.platform.as_deref() {
                        Some("gen") => GeminiPlatform::Gen,
                        _ => GeminiPlatform::Vertex,
                    },
                };

                Ok(Arc::new(GeminiClient::new_with_platform_and_window(
                    api_key,
                    model_final,
                    context_window,
                    provider.to_string(),
                    platform,
                )))
            }
            _ => Err(format!("Unknown provider type '{}'", prov_config.type_name)),
        }
    } else {
        match provider {
            "aliyun" => {
                let api_key = std::env::var("DASHSCOPE_API_KEY")
                    .map(|s| s.trim().to_string())
                    .map_err(|_| "DASHSCOPE_API_KEY must be set for aliyun provider")?;
                let model_final = model.unwrap_or_else(|| "qwen-plus".to_string());
                tracing::info!("Using Aliyun provider with model: {}", model_final);
                let context_window = estimate_context_window(&model_final);
                Ok(Arc::new(OpenAiCompatClient::new_with_window(
                    api_key,
                    "https://coding.dashscope.aliyuncs.com/v1/chat/completions".to_string(),
                    model_final,
                    "aliyun".to_string(),
                    context_window,
                    None,
                )))
            }
            "gemini" | _ => {
                let api_key = std::env::var("GEMINI_API_KEY")
                    .map(|s| s.trim().to_string())
                    .map_err(|_| "GEMINI_API_KEY must be set for gemini provider")?;
                tracing::info!(
                    "Using Gemini provider with model: {:?}",
                    model.as_deref().unwrap_or("default")
                );
                let model_str = model
                    .clone()
                    .unwrap_or_else(|| "gemini-3.1-pro-preview".to_string());
                let context_window = estimate_context_window(&model_str);
                let platform = match platform_override.as_deref() {
                    Some("gen") => GeminiPlatform::Gen,
                    _ => GeminiPlatform::Vertex,
                };
                Ok(Arc::new(GeminiClient::new_with_platform_and_window(
                    api_key,
                    model,
                    context_window,
                    provider.to_string(),
                    platform,
                )))
            }
        }
    }
}
