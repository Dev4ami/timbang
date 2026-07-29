//! One job: send text, get text back.
//!
//! This module is deliberately stupid (§2). It knows nothing about debate,
//! phases, sides, or whether an answer was any good. It retries networks, never
//! opinions: there is no `BadOutput` variant in [`LlmError`], so "ask again,
//! that was weak" cannot be expressed here at all. That logic belongs to
//! `phase`, and §5 is explicit that the two must not share a code path.
//!
//! The only module allowed to touch [`ApiKey`] (§6).

use std::time::Duration;

use serde::Deserialize;

use crate::config::{ApiKey, ModelId};

/// What came back, along with everything needed to judge whether to trust it.
#[derive(Debug, Clone)]
pub struct Completion {
    pub text: String,
    /// The `model` field from the response body, verbatim. Not compared,
    /// normalised, or defaulted here — `llm` reports, `phase` judges.
    pub model_reported: Option<String>,
    pub finish_reason: Option<String>,
    pub prompt_tokens: Option<u32>,
    pub completion_tokens: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct Message {
    pub role: &'static str,
    pub content: String,
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Message { role: "system", content: content.into() }
    }
    pub fn user(content: impl Into<String>) -> Self {
        Message { role: "user", content: content.into() }
    }
}

/// Cloneable: `reqwest::Client` shares one connection pool across clones, and
/// the key is behind an `Arc` so the web server can hand a client to each
/// background debate task without re-reading or copying the secret. `Arc<ApiKey>`
/// keeps §6's rule intact — the key is still only reachable through this module.
#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    base_url: String,
    key: std::sync::Arc<ApiKey>,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Written by hand rather than derived: a derived Debug here would be
        // fine today only because ApiKey redacts itself, and that is too much to
        // rest on.
        f.debug_struct("Client")
            .field("base_url", &self.base_url)
            .field("key", &"[redacted]")
            .finish()
    }
}

impl Client {
    pub fn new(base_url: impl Into<String>, key: ApiKey) -> Result<Self, LlmError> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(300))
            .build()
            .map_err(|e| LlmError::Jaringan(e.to_string()))?;
        Ok(Client {
            http,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            key: std::sync::Arc::new(key),
        })
    }

    /// Sends one completion request, retrying only for reasons that have nothing
    /// to do with the answer's content.
    pub async fn kirim(
        &self,
        model: &ModelId,
        messages: &[Message],
        temperature: f32,
        max_tokens: u32,
    ) -> Result<Completion, LlmError> {
        let body = serde_json::json!({
            "model": model.as_str(),
            // Must be explicit: this router streams by default and only returns
            // plain JSON when the field is written out. Discovered in Tahap 0.
            "stream": false,
            "temperature": temperature,
            "max_tokens": max_tokens,
            "messages": messages.iter().map(|m| serde_json::json!({
                "role": m.role,
                "content": m.content,
            })).collect::<Vec<_>>(),
        });

        let url = format!("{}/v1/chat/completions", self.base_url);

        // Three attempts for transport-level trouble (§5). Anything decided by
        // the response body's *content* is not retried here.
        const MAX_PERCOBAAN: u32 = 3;
        let mut percobaan = 0u32;

        loop {
            percobaan += 1;

            let resp = self
                .http
                .post(&url)
                .header("Authorization", format!("Bearer {}", self.key.expose()))
                // Mandatory (§5). The router's automatic compression targets tool
                // output and probably never touches debate text — but "probably"
                // is not enough when the failure mode is arguments silently
                // shrinking and an evening lost debugging a prompt that was fine.
                .header("X-9Router-Token-Saver", "off")
                .json(&body)
                .send()
                .await;

            let resp = match resp {
                Ok(r) => r,
                Err(e) => {
                    if percobaan >= MAX_PERCOBAAN {
                        return Err(LlmError::Jaringan(e.to_string()));
                    }
                    tokio::time::sleep(backoff(percobaan)).await;
                    continue;
                }
            };

            let status = resp.status();
            let retry_after = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok());
            let text = resp.text().await.unwrap_or_default();

            match status.as_u16() {
                200 => return parse_completion(&text),

                // Config problems, not transient ones. Retrying wastes time and
                // hides the real cause (§5).
                401 | 403 => return Err(LlmError::Auth(potong(&text))),
                400 | 404 | 422 => {
                    return Err(LlmError::ModelDitolak {
                        model: model.as_str().to_string(),
                        detail: potong(&text),
                    });
                }
                402 => {
                    return Err(LlmError::KreditHabis {
                        model: model.as_str().to_string(),
                        detail: potong(&text),
                    });
                }

                // Upstream capacity. Honour retry-after when the server sends it;
                // fall back to backoff when it does not.
                429 | 503 => {
                    if percobaan >= MAX_PERCOBAAN {
                        return Err(LlmError::TidakTersedia {
                            status: status.as_u16(),
                            detail: potong(&text),
                        });
                    }
                    let tunggu = retry_after
                        .map(Duration::from_secs)
                        .unwrap_or_else(|| backoff(percobaan));
                    tokio::time::sleep(tunggu).await;
                    continue;
                }

                _ => {
                    if percobaan >= MAX_PERCOBAAN {
                        return Err(LlmError::Http {
                            status: status.as_u16(),
                            detail: potong(&text),
                        });
                    }
                    tokio::time::sleep(backoff(percobaan)).await;
                    continue;
                }
            }
        }
    }

    /// `GET /api/health` → `{"ok":true}`. Used by the connection-test button.
    pub async fn health(&self) -> Result<bool, LlmError> {
        let url = format!("{}/api/health", self.base_url);
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| LlmError::Jaringan(e.to_string()))?;
        let v: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| LlmError::Format(e.to_string()))?;
        Ok(v["ok"].as_bool().unwrap_or(false))
    }

    /// `GET /v1/models` → the catalogue, as `(id, owned_by)` pairs.
    pub async fn list_models(&self) -> Result<Vec<(String, String)>, LlmError> {
        let url = format!("{}/v1/models", self.base_url);
        let resp = self
            .http
            .get(&url)
            .header("Authorization", format!("Bearer {}", self.key.expose()))
            .send()
            .await
            .map_err(|e| LlmError::Jaringan(e.to_string()))?;
        let v: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| LlmError::Format(e.to_string()))?;
        Ok(v["data"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| {
                        Some((
                            m["id"].as_str()?.to_string(),
                            m["owned_by"].as_str().unwrap_or("").to_string(),
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    /// `POST /v1/audio/speech` → raw audio bytes (the router returns WAV).
    ///
    /// Read-aloud for one turn (aksesibilitas). `model_path` carries the voice as
    /// its final segment (see [`crate::config::Tts::model_path`]); the body is the
    /// minimal `{model, input}` the router accepts. Returns the audio verbatim —
    /// this module stays stupid (§2): it does not cache, transcode, or know what a
    /// "turn" is. Caching is the caller's job, on disk.
    ///
    /// Same key rule (§6) and same transport-retry taxonomy (§5) as [`kirim`];
    /// content-level failures (a bad voice, an unknown model) fail fast as
    /// `ModelDitolak` rather than retrying, for the same reason completions do.
    ///
    /// [`kirim`]: Client::kirim
    pub async fn tts(&self, model_path: &str, input: &str) -> Result<Vec<u8>, LlmError> {
        let body = serde_json::json!({ "model": model_path, "input": input });
        let url = format!("{}/v1/audio/speech", self.base_url);

        const MAX_PERCOBAAN: u32 = 3;
        let mut percobaan = 0u32;

        loop {
            percobaan += 1;

            let resp = self
                .http
                .post(&url)
                .header("Authorization", format!("Bearer {}", self.key.expose()))
                // Mandatory (§5): the router's compression targets tool output,
                // and there is no reason to risk it touching an audio stream.
                .header("X-9Router-Token-Saver", "off")
                .json(&body)
                .send()
                .await;

            let resp = match resp {
                Ok(r) => r,
                Err(e) => {
                    if percobaan >= MAX_PERCOBAAN {
                        return Err(LlmError::Jaringan(e.to_string()));
                    }
                    tokio::time::sleep(backoff(percobaan)).await;
                    continue;
                }
            };

            let status = resp.status();
            let retry_after = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok());

            match status.as_u16() {
                // Success is binary, not JSON — read bytes, not text.
                200 => {
                    let bytes = resp
                        .bytes()
                        .await
                        .map_err(|e| LlmError::Jaringan(e.to_string()))?;
                    if bytes.is_empty() {
                        return Err(LlmError::Format("audio kosong dari router".into()));
                    }
                    return Ok(bytes.to_vec());
                }

                // Config problems, not transient ones (§5).
                401 | 403 => return Err(LlmError::Auth(potong(&resp.text().await.unwrap_or_default()))),
                400 | 404 | 422 => {
                    return Err(LlmError::ModelDitolak {
                        model: model_path.to_string(),
                        detail: potong(&resp.text().await.unwrap_or_default()),
                    });
                }
                402 => {
                    return Err(LlmError::KreditHabis {
                        model: model_path.to_string(),
                        detail: potong(&resp.text().await.unwrap_or_default()),
                    });
                }

                429 | 503 => {
                    if percobaan >= MAX_PERCOBAAN {
                        return Err(LlmError::TidakTersedia {
                            status: status.as_u16(),
                            detail: potong(&resp.text().await.unwrap_or_default()),
                        });
                    }
                    let tunggu = retry_after
                        .map(Duration::from_secs)
                        .unwrap_or_else(|| backoff(percobaan));
                    tokio::time::sleep(tunggu).await;
                    continue;
                }

                _ => {
                    if percobaan >= MAX_PERCOBAAN {
                        return Err(LlmError::Http {
                            status: status.as_u16(),
                            detail: potong(&resp.text().await.unwrap_or_default()),
                        });
                    }
                    tokio::time::sleep(backoff(percobaan)).await;
                    continue;
                }
            }
        }
    }
}

#[derive(Deserialize)]
struct RespBody {
    model: Option<String>,
    choices: Vec<Choice>,
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct Choice {
    message: Msg,
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct Msg {
    content: Option<String>,
}

#[derive(Deserialize)]
struct Usage {
    prompt_tokens: Option<u32>,
    completion_tokens: Option<u32>,
}

fn parse_completion(text: &str) -> Result<Completion, LlmError> {
    let body: RespBody =
        serde_json::from_str(text).map_err(|e| LlmError::Format(format!("{e}: {}", potong(text))))?;

    let choice = body
        .choices
        .into_iter()
        .next()
        .ok_or_else(|| LlmError::Format("respons tanpa choices".into()))?;

    Ok(Completion {
        text: choice.message.content.unwrap_or_default(),
        model_reported: body.model,
        finish_reason: choice.finish_reason,
        prompt_tokens: body.usage.as_ref().and_then(|u| u.prompt_tokens),
        completion_tokens: body.usage.as_ref().and_then(|u| u.completion_tokens),
    })
}

fn backoff(percobaan: u32) -> Duration {
    Duration::from_secs(2u64.saturating_pow(percobaan))
}

/// Error bodies can be long; only the first part is useful in a message.
fn potong(s: &str) -> String {
    let s = s.trim();
    if s.chars().count() <= 300 {
        s.to_string()
    } else {
        s.chars().take(300).collect::<String>() + "…"
    }
}

/// Transport and configuration failures — and nothing else.
///
/// There is deliberately no variant for "the answer was weak", "too similar to
/// last round", or "did not address the opponent". Those are debate judgements,
/// they live in `phase`, and giving them a home here is how the two retry
/// logics §5 separates end up tangled (§2, §5).
#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    #[error("API key ditolak router: {0}")]
    Auth(String),
    #[error("model '{model}' ditolak router: {detail}")]
    ModelDitolak { model: String, detail: String },
    #[error("model '{model}' butuh kredit: {detail}")]
    KreditHabis { model: String, detail: String },
    #[error("router tidak tersedia (HTTP {status}) setelah 3 percobaan: {detail}")]
    TidakTersedia { status: u16, detail: String },
    #[error("HTTP {status}: {detail}")]
    Http { status: u16, detail: String },
    #[error("jaringan gagal setelah 3 percobaan: {0}")]
    Jaringan(String),
    #[error("respons router tidak bisa dibaca: {0}")]
    Format(String),
}

impl LlmError {
    /// Whether the session can be resumed later, or is broken until the user
    /// changes something.
    pub fn bisa_dilanjutkan(&self) -> bool {
        match self {
            LlmError::TidakTersedia { .. } | LlmError::Jaringan(_) | LlmError::Http { .. } => true,
            LlmError::Auth(_)
            | LlmError::ModelDitolak { .. }
            | LlmError::KreditHabis { .. }
            | LlmError::Format(_) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_respons_normal() {
        let json = r#"{
            "model": "claude-opus-5",
            "choices": [{"index":0,"message":{"role":"assistant","content":"halo"},"finish_reason":"stop"}],
            "usage": {"prompt_tokens": 2031, "completion_tokens": 5}
        }"#;
        let c = parse_completion(json).unwrap();
        assert_eq!(c.text, "halo");
        assert_eq!(c.model_reported.as_deref(), Some("claude-opus-5"));
        assert_eq!(c.finish_reason.as_deref(), Some("stop"));
        assert_eq!(c.prompt_tokens, Some(2031));
    }

    #[test]
    fn respons_tanpa_field_model_tetap_terbaca() {
        let json = r#"{"choices":[{"message":{"content":"x"},"finish_reason":"stop"}]}"#;
        let c = parse_completion(json).unwrap();
        assert_eq!(c.model_reported, None);
    }

    #[test]
    fn masalah_config_tidak_bisa_dilanjutkan() {
        assert!(!LlmError::Auth("x".into()).bisa_dilanjutkan());
        assert!(LlmError::Jaringan("x".into()).bisa_dilanjutkan());
    }
}
