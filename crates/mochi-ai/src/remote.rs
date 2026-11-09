use crate::{
    redact_secrets, AiProvider, AiRequest, AiResponse, CompletionFuture, ProviderConfig,
    ProviderError, ProviderErrorCode as Code, ProviderKind,
};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::{json, Value};
use std::time::Duration;

#[derive(Clone)]
pub struct RemoteOptions {
    pub timeout: Duration,
    pub retry_delays: Vec<Duration>,
}
impl Default for RemoteOptions {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(60),
            retry_delays: vec![Duration::from_secs(1), Duration::from_secs(3)],
        }
    }
}

// Neither the client nor its configuration is Debug: keys and gateway headers stay private.
pub struct RemoteProvider {
    config: ProviderConfig,
    api_key: String,
    client: reqwest::Client,
    options: RemoteOptions,
}
impl RemoteProvider {
    pub fn new(config: ProviderConfig, api_key: String) -> Result<Self, ProviderError> {
        Self::with_options(config, api_key, RemoteOptions::default())
    }
    pub fn with_options(
        config: ProviderConfig,
        api_key: String,
        options: RemoteOptions,
    ) -> Result<Self, ProviderError> {
        if config.kind == ProviderKind::Mock {
            return Err(ProviderError::new(
                Code::BadResponse,
                "Use MockProvider for MOCK",
            ));
        }
        let _ = rustls::crypto::ring::default_provider().install_default();
        let client = reqwest::Client::builder().build().map_err(|_| {
            ProviderError::new(Code::Network, "Could not create the provider HTTP client")
        })?;
        Ok(Self {
            config,
            api_key,
            client,
            options,
        })
    }
    fn clean(&self, text: &str) -> String {
        redact_secrets(
            text,
            std::iter::once(self.api_key.as_str()).chain(
                self.config
                    .extra_headers
                    .iter()
                    .flat_map(|h| h.values().map(String::as_str)),
            ),
        )
    }
    fn protocol(&self, request: &AiRequest) -> Result<(String, HeaderMap, Value), ProviderError> {
        let mut headers = HeaderMap::new();
        for (k, v) in self.config.extra_headers.iter().flat_map(|h| h.iter()) {
            let name = HeaderName::from_bytes(k.as_bytes()).map_err(|_| {
                ProviderError::new(
                    Code::Network,
                    "Could not reach the provider: Invalid header name",
                )
            })?;
            let mut value = HeaderValue::from_str(v).map_err(|_| {
                ProviderError::new(
                    Code::Network,
                    "Could not reach the provider: Invalid header value",
                )
            })?;
            value.set_sensitive(true);
            headers.insert(name, value);
        }
        headers.insert("content-type", HeaderValue::from_static("application/json"));
        let (default, path, body) = match self.config.kind {
            ProviderKind::Openai
            | ProviderKind::OpenaiCompatible
            | ProviderKind::Deepseek
            | ProviderKind::Opencode => {
                let default = match self.config.kind {
                    ProviderKind::Deepseek => "https://api.deepseek.com",
                    ProviderKind::Opencode => "https://opencode.ai/inference/openai/v1",
                    _ => "https://api.openai.com/v1",
                };
                let mut body = json!({"model":request.model,"messages":[{"role":"system","content":request.system},{"role":"user","content":request.user}]});
                if matches!(
                    self.config.kind,
                    ProviderKind::Openai | ProviderKind::Deepseek
                ) {
                    body["response_format"] = json!({"type":"json_object"});
                }
                self.auth(
                    &mut headers,
                    "authorization",
                    &format!("Bearer {}", self.api_key),
                )?;
                (default, "/chat/completions".into(), body)
            }
            ProviderKind::Anthropic => {
                self.auth(&mut headers, "x-api-key", &self.api_key)?;
                headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
                (
                    "https://api.anthropic.com",
                    "/v1/messages".into(),
                    json!({"model":request.model,"max_tokens":2048,"system":request.system,"messages":[{"role":"user","content":request.user}]}),
                )
            }
            ProviderKind::Gemini => {
                self.auth(&mut headers, "x-goog-api-key", &self.api_key)?;
                // encodeURIComponent preserves only these punctuation bytes in addition to ASCII alphanumerics.
                let mut model = String::new();
                for byte in request.model.bytes() {
                    if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
                        model.push(char::from(byte));
                    } else {
                        use std::fmt::Write;
                        write!(model, "%{byte:02X}").unwrap();
                    }
                }
                (
                    "https://generativelanguage.googleapis.com",
                    format!("/v1beta/models/{model}:generateContent"),
                    json!({"systemInstruction":{"parts":[{"text":request.system}]},"contents":[{"role":"user","parts":[{"text":request.user}]}],"generationConfig":{"responseMimeType":"application/json"}}),
                )
            }
            ProviderKind::Mock => {
                return Err(ProviderError::new(
                    Code::BadResponse,
                    "Use MockProvider for MOCK",
                ))
            }
        };
        Ok((
            format!(
                "{}{path}",
                self.config
                    .base_url
                    .as_deref()
                    .unwrap_or(default)
                    .trim_end_matches('/')
            ),
            headers,
            body,
        ))
    }
    fn auth(
        &self,
        headers: &mut HeaderMap,
        name: &'static str,
        value: &str,
    ) -> Result<(), ProviderError> {
        let mut value = HeaderValue::from_str(value).map_err(|_| {
            ProviderError::new(
                Code::Network,
                "Could not reach the provider: Invalid header value",
            )
        })?;
        value.set_sensitive(true);
        headers.insert(name, value);
        Ok(())
    }
    async fn post_once(
        &self,
        url: &str,
        headers: &HeaderMap,
        body: &Value,
    ) -> Result<Value, ProviderError> {
        let operation = async {
            let response = self
                .client
                .post(url)
                .headers(headers.clone())
                .body(body.to_string())
                .send()
                .await
                .map_err(|e| {
                    ProviderError::new(
                        Code::Network,
                        self.clean(&format!("Could not reach the provider: {e}")),
                    )
                })?;
            let status = response.status();
            if !status.is_success() {
                let text = self.clean(&response.text().await.unwrap_or_default());
                // Bound UTF-16 units only after redaction: a cut through a credential must not expose its prefix.
                let bounded =
                    String::from_utf16_lossy(&text.encode_utf16().take(400).collect::<Vec<_>>());
                let code = match status.as_u16() {
                    401 | 403 => Code::Auth,
                    429 => Code::RateLimit,
                    500.. => Code::Server,
                    _ => Code::Http,
                };
                return Err(ProviderError::new(
                    code,
                    format!("Provider returned HTTP {}: {}", status.as_u16(), bounded),
                ));
            }
            let bytes = response.bytes().await.map_err(|_| {
                ProviderError::new(
                    Code::BadResponse,
                    "Provider returned a body that is not JSON",
                )
            })?;
            serde_json::from_slice(&bytes).map_err(|_| {
                ProviderError::new(
                    Code::BadResponse,
                    "Provider returned a body that is not JSON",
                )
            })
        };
        tokio::time::timeout(self.options.timeout, operation)
            .await
            .unwrap_or_else(|_| {
                Err(ProviderError::new(
                    Code::Timeout,
                    "The model did not answer in time",
                ))
            })
    }
    async fn run(&self, request: &AiRequest) -> Result<AiResponse, ProviderError> {
        let (url, headers, body) = self.protocol(request)?;
        let mut attempt = 0;
        let json = loop {
            match self.post_once(&url, &headers, &body).await {
                Ok(json) => break json,
                Err(mut e) => {
                    if matches!(e.code, Code::Network | Code::RateLimit | Code::Server) {
                        if let Some(delay) = self.options.retry_delays.get(attempt) {
                            tokio::time::sleep(*delay).await;
                            attempt += 1;
                            continue;
                        }
                    }
                    if attempt > 0 {
                        e.message
                            .push_str(&format!(" (after {} attempts)", attempt + 1));
                    }
                    return Err(e);
                }
            }
        };
        let (text, input_tokens, output_tokens, message) = match self.config.kind {
            ProviderKind::Anthropic => (
                join_text(json["content"].as_array(), true),
                json["usage"]["input_tokens"].as_f64(),
                json["usage"]["output_tokens"].as_f64(),
                "No text block in the response",
            ),
            ProviderKind::Gemini => (
                join_text(json["candidates"][0]["content"]["parts"].as_array(), false),
                json["usageMetadata"]["promptTokenCount"].as_f64(),
                json["usageMetadata"]["candidatesTokenCount"].as_f64(),
                "No candidate text in the response",
            ),
            _ => (
                json["choices"][0]["message"]["content"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
                json["usage"]["prompt_tokens"].as_f64(),
                json["usage"]["completion_tokens"].as_f64(),
                "No message content in the response",
            ),
        };
        let empty = if matches!(
            self.config.kind,
            ProviderKind::Anthropic | ProviderKind::Gemini
        ) {
            text.is_empty()
        } else {
            mochi_domain::jsnum::js_trim(&text).is_empty()
        };
        if empty {
            return Err(ProviderError::new(Code::BadResponse, message));
        }
        Ok(AiResponse {
            text,
            input_tokens,
            output_tokens,
        })
    }
}
fn join_text(parts: Option<&Vec<Value>>, anthropic: bool) -> String {
    parts
        .into_iter()
        .flatten()
        .filter(|p| !anthropic || p["type"] == "text")
        .filter_map(|p| p["text"].as_str())
        .collect()
}
impl AiProvider for RemoteProvider {
    fn kind(&self) -> ProviderKind {
        self.config.kind
    }
    fn complete<'a>(&'a self, request: &'a AiRequest) -> CompletionFuture<'a> {
        Box::pin(self.run(request))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn default_endpoints_match_node_without_network() {
        let request = AiRequest {
            model: "model".into(),
            system: String::new(),
            user: String::new(),
            context: None,
        };
        for (kind, expected) in [
            (
                ProviderKind::Openai,
                "https://api.openai.com/v1/chat/completions",
            ),
            (
                ProviderKind::OpenaiCompatible,
                "https://api.openai.com/v1/chat/completions",
            ),
            (
                ProviderKind::Deepseek,
                "https://api.deepseek.com/chat/completions",
            ),
            (
                ProviderKind::Opencode,
                "https://opencode.ai/inference/openai/v1/chat/completions",
            ),
            (
                ProviderKind::Anthropic,
                "https://api.anthropic.com/v1/messages",
            ),
            (
                ProviderKind::Gemini,
                "https://generativelanguage.googleapis.com/v1beta/models/model:generateContent",
            ),
        ] {
            let config = ProviderConfig {
                id: "p".into(),
                name: "test".into(),
                kind,
                base_url: None,
                default_model: "model".into(),
                input_price_per_m_tok: None,
                output_price_per_m_tok: None,
                extra_headers: None,
                created_at: 0,
            };
            let provider = RemoteProvider::new(config, "synthetic-key".into()).unwrap();
            assert_eq!(provider.protocol(&request).unwrap().0, expected);
        }
    }
}
