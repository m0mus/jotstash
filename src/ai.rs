//! AI integration. One provider implementation (`OpenAiCompatProvider`) covers
//! both OpenAI and Ollama, since Ollama exposes an OpenAI-compatible
//! `/v1/chat/completions` endpoint. Differences are configuration-only:
//! base URL and whether an `Authorization: Bearer` header is sent.

use std::time::Duration;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone)]
pub enum AiError {
    NoApiKey(String),
    Http(String),
    Api { status: u16, body: String },
    Parse(String),
}

impl std::fmt::Display for AiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AiError::NoApiKey(env) => write!(f, "API key env var {env} not set"),
            AiError::Http(msg) => write!(f, "network error: {msg}"),
            AiError::Api { status, body } => write!(f, "API error {status}: {body}"),
            AiError::Parse(msg) => write!(f, "response parse error: {msg}"),
        }
    }
}

pub trait AiProvider: Send + Sync {
    fn complete(&self, prompt: &str, input: &str) -> Result<String, AiError>;
}

#[derive(Debug)]
pub struct OpenAiCompatProvider {
    pub base_url: String,
    pub model: String,
    pub api_key: Option<String>,
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<ChatMessage<'a>>,
    stream: bool,
}

#[derive(Serialize)]
struct ChatMessage<'a> {
    role: &'a str,
    content: String,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Deserialize)]
struct ChatChoice {
    message: ChatChoiceMessage,
}

#[derive(Deserialize)]
struct ChatChoiceMessage {
    content: String,
}

impl AiProvider for OpenAiCompatProvider {
    fn complete(&self, prompt: &str, input: &str) -> Result<String, AiError> {
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let body = ChatRequest {
            model: &self.model,
            messages: vec![ChatMessage {
                role: "user",
                content: format!("{prompt}\n\n{input}"),
            }],
            stream: false,
        };

        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .map_err(|e| AiError::Http(e.to_string()))?;

        let mut req = client.post(&url).json(&body);
        if let Some(key) = self.api_key.as_deref() {
            req = req.bearer_auth(key);
        }

        let resp = req.send().map_err(|e| AiError::Http(e.to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().unwrap_or_default();
            return Err(AiError::Api {
                status: status.as_u16(),
                body,
            });
        }

        let parsed: ChatResponse = resp.json().map_err(|e| AiError::Parse(e.to_string()))?;
        parsed
            .choices
            .into_iter()
            .next()
            .map(|c| c.message.content)
            .ok_or_else(|| AiError::Parse("no choices in response".into()))
    }
}

/// Build a provider from `AiConfig`, reading the API key from the configured
/// env var (or `None` for local providers like Ollama).
pub fn provider_from_config(cfg: &crate::config::AiConfig) -> Result<OpenAiCompatProvider, AiError> {
    let api_key = match cfg.api_key_env.as_deref() {
        None => None,
        Some(env_name) => {
            let val = std::env::var(env_name).unwrap_or_default();
            if val.is_empty() {
                return Err(AiError::NoApiKey(env_name.to_string()));
            }
            Some(val)
        }
    };
    Ok(OpenAiCompatProvider {
        base_url: cfg.base_url.clone(),
        model: cfg.model.clone(),
        api_key,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_from_config_with_key() {
        let cfg = crate::config::AiConfig {
            provider: "openai".into(),
            model: "gpt-4o-mini".into(),
            base_url: "https://api.openai.com/v1".into(),
            api_key_env: Some("NOTESAPP_TEST_AI_KEY".into()),
        };
        std::env::set_var("NOTESAPP_TEST_AI_KEY", "test-key-123");
        let p = provider_from_config(&cfg).unwrap();
        assert_eq!(p.api_key.as_deref(), Some("test-key-123"));
        assert_eq!(p.model, "gpt-4o-mini");
        std::env::remove_var("NOTESAPP_TEST_AI_KEY");
    }

    #[test]
    fn provider_from_config_missing_key() {
        let cfg = crate::config::AiConfig {
            provider: "openai".into(),
            model: "gpt-4o-mini".into(),
            base_url: "https://api.openai.com/v1".into(),
            api_key_env: Some("NOTESAPP_TEST_AI_NONE".into()),
        };
        std::env::remove_var("NOTESAPP_TEST_AI_NONE");
        let err = provider_from_config(&cfg).unwrap_err();
        matches!(err, AiError::NoApiKey(_));
    }

    #[test]
    fn provider_from_config_no_env_var() {
        // Ollama-style: api_key_env is None → no key required.
        let cfg = crate::config::AiConfig {
            provider: "ollama".into(),
            model: "llama3.2".into(),
            base_url: "http://localhost:11434/v1".into(),
            api_key_env: None,
        };
        let p = provider_from_config(&cfg).unwrap();
        assert!(p.api_key.is_none());
    }

    #[test]
    fn error_display() {
        let e = AiError::NoApiKey("OPENAI_API_KEY".into());
        assert_eq!(e.to_string(), "API key env var OPENAI_API_KEY not set");

        let e = AiError::Api {
            status: 401,
            body: "unauthorized".into(),
        };
        assert_eq!(e.to_string(), "API error 401: unauthorized");
    }
}
