//! Configuration, secrets, and prompt loading.
//!
//! The security rules in §6 are implemented as *missing* impls as much as
//! present ones. [`ApiKey`] has no `Display`, no `Clone`, no `Serialize`, and no
//! public accessor outside this crate's `llm` module — so the ways to leak it
//! do not compile, rather than being forbidden by review.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The router API key.
///
/// - `Debug` prints `[redacted]`, because `#[derive(Debug)]` on any struct that
///   contains a key would print it into every panic and log line (§6).
/// - No `Display`: `format!("{key}")` is a compile error.
/// - No `Serialize`: any struct containing one cannot derive `Serialize` at all,
///   so a key cannot reach the browser by being forgotten in a response type.
/// - No `Clone`: fewer copies, fewer places to audit.
pub struct ApiKey(String);

impl std::fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[redacted]")
    }
}

impl ApiKey {
    /// Reads the key from the environment, failing immediately if absent (§6:
    /// no `unwrap_or_default`, a missing key is a config error worth stopping
    /// for, not something to paper over).
    pub fn from_env() -> Result<Self, ConfigError> {
        match std::env::var("ROUTER_API_KEY") {
            Ok(v) if !v.trim().is_empty() => Ok(ApiKey(v)),
            _ => Err(ConfigError::MissingApiKey),
        }
    }

    /// Crate-visible on purpose: only `llm` may touch the key (§6).
    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

/// A model id as written in config, e.g. `cc/claude-opus-5`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelId(String);

impl ModelId {
    pub fn new(s: impl Into<String>) -> Self {
        ModelId(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The route prefix (`cc`, `kr`, `kc`, …), i.e. which upstream path this
    /// model is reached through.
    pub fn route(&self) -> &str {
        self.0.split('/').next().unwrap_or(&self.0)
    }

    /// The bare model name, with every route prefix stripped.
    ///
    /// This router answers `cc/claude-opus-5` with `claude-opus-5`, so raw
    /// string comparison against the response would report a mismatch on every
    /// turn and drown the real signal (§5).
    pub fn bare(&self) -> &str {
        self.0.rsplit('/').next().unwrap_or(&self.0)
    }

    /// Best-effort lab guess, used to enforce "Pro and Kontra from different
    /// labs" (§10). Name-based and therefore fallible — it is a guard against
    /// the obvious mistake, not a proof.
    pub fn lab(&self) -> &'static str {
        let n = self.bare().to_ascii_lowercase();
        if n.contains("claude") || n.contains("opus") || n.contains("sonnet") || n.contains("haiku") || n.contains("fable") {
            "anthropic"
        } else if n.contains("gpt") || n.contains("o3") || n.contains("o4") {
            "openai"
        } else if n.contains("gemini") || n.contains("gemma") {
            "google"
        } else if n.contains("deepseek") {
            "deepseek"
        } else if n.contains("glm") {
            "zhipu"
        } else if n.contains("minimax") {
            "minimax"
        } else if n.contains("qwen") {
            "alibaba"
        } else if n.contains("mimo") {
            "xiaomi"
        } else {
            "tidak-dikenal"
        }
    }
}

/// Per-role model choices.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Models {
    pub pro: ModelId,
    pub kontra: ModelId,
    pub moderator: ModelId,
    pub synthesizer: ModelId,
    /// Runs after Crux to classify claims and check the factual ones (Tahap 4).
    /// `serde(default)` falls back to synthesizer when absent, so sessions and
    /// configs written before Tahap 4 still parse — that is the whole point of
    /// keeping this field optional at the wire layer even though the code below
    /// always sees a concrete model.
    #[serde(default)]
    pub fact_checker: Option<ModelId>,
}

impl Models {
    /// Concrete fact-checker id: the configured one, or a copy of the
    /// synthesizer as fallback. Kept as a method so every call site agrees on
    /// the same fallback rule.
    pub fn fact_checker(&self) -> &ModelId {
        self.fact_checker.as_ref().unwrap_or(&self.synthesizer)
    }
}

/// Text-to-speech voices, one per side (Tahap 4, aksesibilitas).
///
/// TTS is an on-demand read-aloud of one turn at a time — never a broadcast and
/// never a signal (§1: a synthesised voice's "confidence" has nothing to do with
/// an argument's strength, so it must not be mistaken for one). It is therefore
/// deliberately *not* in [`SessionConfig`]: it changes nothing about the debate,
/// so a session file need not record it to be reproducible.
///
/// Voices differ per side so the ear can tell Pro from Kontra without the screen
/// (mirrors the two columns, §7). §10 still applies: the two must be comparable
/// in authority — one voice sounding weightier would bias exactly the way a
/// stronger model does.
///
/// `serde(default)` throughout: a `config.toml` written before TTS existed still
/// parses, and the whole `[tts]` section may be omitted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tts {
    /// The TTS model path *without* the trailing voice segment, e.g.
    /// `gemini/gemini-3.1-flash-tts-preview`. The router expects the voice
    /// appended as a final path segment; [`Tts::model_path`] joins them.
    pub model: String,
    pub voice_pro: String,
    pub voice_kontra: String,
    /// Moderator and synthesizer — a third, neutral voice.
    pub voice_lain: String,
}

impl Default for Tts {
    fn default() -> Self {
        Tts {
            model: "gemini/gemini-3.1-flash-tts-preview".into(),
            voice_pro: "Zephyr".into(),
            voice_kontra: "Puck".into(),
            voice_lain: "Charon".into(),
        }
    }
}

impl Tts {
    /// The voice for a role: Pro and Kontra get their own; every non-debating
    /// role shares the neutral one.
    pub fn voice_for(&self, role: crate::view::Role) -> &str {
        use crate::view::{Role, Side};
        match role {
            Role::Debater(Side::Pro) => &self.voice_pro,
            Role::Debater(Side::Kontra) => &self.voice_kontra,
            Role::Moderator | Role::Synthesizer => &self.voice_lain,
        }
    }

    /// The full model id the router wants: model path with the voice as the
    /// final segment, e.g. `gemini/gemini-3.1-flash-tts-preview/Zephyr`.
    pub fn model_path(&self, voice: &str) -> String {
        format!("{}/{}", self.model, voice)
    }
}

/// The settings a session actually ran with, copied into its file (§3).
///
/// Contains no key and no base URL — those are not per-session, and keeping
/// them out means a session file can be shared or diffed without redaction.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionConfig {
    pub models: Models,
    pub rounds: u32,
    pub word_limit: u32,
    pub temperature: f32,
    /// Above this, a turn counts as repeating itself (§4).
    pub similarity_threshold: f32,
}

impl SessionConfig {
    /// A fixed config for tests, so they never depend on config.toml.
    #[cfg(test)]
    pub fn uji() -> Self {
        SessionConfig {
            models: Models {
                pro: ModelId::new("uji/pro"),
                kontra: ModelId::new("uji/kontra"),
                moderator: ModelId::new("uji/moderator"),
                synthesizer: ModelId::new("uji/synthesizer"),
                fact_checker: None,
            },
            rounds: 3,
            word_limit: 200,
            temperature: 0.7,
            similarity_threshold: 0.6,
        }
    }

    /// Rejects debater pairings that §10 forbids.
    ///
    /// Same lab means the two sides share training data and failure modes, and
    /// different tiers of one family means the stronger side looks more
    /// convincing for reasons unrelated to the argument — a lopsided debate is
    /// more misleading than a soft one.
    pub fn periksa_lawan_sepadan(&self) -> Result<(), ConfigError> {
        let pro = &self.models.pro;
        let kontra = &self.models.kontra;

        if pro.bare() == kontra.bare() {
            return Err(ConfigError::DebaterSama(pro.as_str().to_string()));
        }
        if pro.lab() == "tidak-dikenal" || kontra.lab() == "tidak-dikenal" {
            // Unknown names cannot be checked, and guessing would be worse than
            // admitting it. Left to the user.
            return Ok(());
        }
        if pro.lab() == kontra.lab() {
            return Err(ConfigError::LabSama {
                lab: pro.lab(),
                pro: pro.as_str().to_string(),
                kontra: kontra.as_str().to_string(),
            });
        }
        Ok(())
    }
}

/// The whole config file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Not settable from the web (§6): one mistake there could point requests —
    /// and the API key with them — at someone else's server.
    pub base_url: String,
    /// Loopback only. Also not settable from the web.
    pub bind: String,
    pub models: Models,
    pub rounds: u32,
    pub word_limit: u32,
    pub temperature: f32,
    pub similarity_threshold: f32,
    pub prompts_dir: PathBuf,
    pub sessions_dir: PathBuf,
    /// Voices for read-aloud. Optional: a config without a `[tts]` section falls
    /// back to sensible defaults, so this feature never breaks an old file.
    #[serde(default)]
    pub tts: Tts,
}

impl Config {
    pub async fn muat(path: &Path) -> Result<Self, ConfigError> {
        let text = tokio::fs::read_to_string(path)
            .await
            .map_err(|e| ConfigError::ConfigTidakTerbaca(path.display().to_string(), e))?;
        let mut cfg: Config = toml::from_str(&text)?;
        // TIMBANG_BIND overrides the file at runtime — the one setting a
        // deploy environment (Coolify, docker-compose) needs to inject without
        // rebuilding the image. Other secrets go through their own env vars
        // already; adding one for bind keeps config.toml the local default.
        if let Ok(v) = std::env::var("TIMBANG_BIND") {
            if !v.trim().is_empty() {
                cfg.bind = v.trim().to_string();
            }
        }
        cfg.periksa()?;
        Ok(cfg)
    }

    fn periksa(&self) -> Result<(), ConfigError> {
        if self.rounds == 0 {
            return Err(ConfigError::NilaiSalah("rounds harus minimal 1"));
        }
        if !(0.0..=2.0).contains(&self.temperature) {
            return Err(ConfigError::NilaiSalah("temperature harus antara 0.0 dan 2.0"));
        }
        if !(0.0..=1.0).contains(&self.similarity_threshold) {
            return Err(ConfigError::NilaiSalah(
                "similarity_threshold harus antara 0.0 dan 1.0",
            ));
        }
        // Loopback by default (§6): binding wider would expose the server that
        // holds the key to the whole network. The one legitimate reason to bind
        // 0.0.0.0 is running inside a container behind a reverse proxy that
        // handles auth (Coolify + Cloudflare Access, etc.) — in that case the
        // container's network is private and the public path is auth-gated
        // upstream, not inside this app. The env var is a foot-gun guard: a
        // stray `cargo run` on a laptop keeps the loopback pin without needing
        // the operator to remember to change the config back.
        let loopback = self.bind.starts_with("127.0.0.1") || self.bind.starts_with("localhost");
        let public_bind_allowed = std::env::var("TIMBANG_ALLOW_PUBLIC_BIND")
            .map(|v| v == "1")
            .unwrap_or(false);
        if !loopback && !public_bind_allowed {
            return Err(ConfigError::BindNonLoopback);
        }
        if !loopback {
            eprintln!(
                "\n⚠ Timbang bind ke {} (bukan loopback).\n  Ini boleh HANYA kalau ada auth di depan\n  (mis. Cloudflare Access). Tanpa itu, siapa pun di jaringan\n  bisa habiskan credit router lewat aplikasi ini.\n",
                self.bind
            );
        }
        self.untuk_sesi().periksa_lawan_sepadan()
    }

    /// The subset that gets frozen into a session file.
    pub fn untuk_sesi(&self) -> SessionConfig {
        SessionConfig {
            models: self.models.clone(),
            rounds: self.rounds,
            word_limit: self.word_limit,
            temperature: self.temperature,
            similarity_threshold: self.similarity_threshold,
        }
    }
}

/// What the browser is allowed to change (§6).
///
/// A separate type, not a partial `Config`: `base_url`, `bind`, and the key have
/// no field here, so a request trying to set them has nowhere to put the value.
/// The rule is enforced by the shape of the type, not by validation that someone
/// has to remember to write.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WebSettingsPatch {
    pub models: Option<Models>,
    pub rounds: Option<u32>,
    pub word_limit: Option<u32>,
    pub temperature: Option<f32>,
}

/// The prompts, loaded from disk.
///
/// §1: prompts live in `prompts/*.md`, never as string literals in `.rs`. The
/// thing the user edits most often must not require a rebuild — including the
/// moderator's nudge text, which is just as much a prompt as the role briefs.
#[derive(Debug, Clone)]
pub struct Prompts {
    pub pro: String,
    pub kontra: String,
    pub moderator: String,
    pub synthesizer: String,
    /// Injected when a turn misses its phase's bar (§4).
    pub teguran: String,
    /// Run by the moderator after each debater turn to track claims (§3, §11):
    /// which claims are new, which opposing claims this turn attacked, which of
    /// its own it abandoned. A separate request per turn — the accurate approach
    /// §11 recommends over asking debaters to number their own claims.
    pub ekstraksi: String,
    /// Fact-check pass over one Claim at a time (Tahap 4): classify
    /// faktual/opini, and for faktual claims say Terdukung/Diragukan/
    /// TidakBisaVerifikasi with a short note the user can act on.
    pub fact_check: String,
}

impl Prompts {
    pub async fn muat(dir: &Path) -> Result<Self, ConfigError> {
        async fn baca(dir: &Path, nama: &str) -> Result<String, ConfigError> {
            let p = dir.join(nama);
            tokio::fs::read_to_string(&p)
                .await
                .map_err(|e| ConfigError::PromptTidakTerbaca(p.display().to_string(), e))
        }
        Ok(Prompts {
            pro: baca(dir, "pro.md").await?,
            kontra: baca(dir, "kontra.md").await?,
            moderator: baca(dir, "moderator.md").await?,
            synthesizer: baca(dir, "synthesizer.md").await?,
            teguran: baca(dir, "teguran.md").await?,
            ekstraksi: baca(dir, "ekstraksi.md").await?,
            fact_check: baca(dir, "fact-check.md").await?,
        })
    }

    pub fn untuk(&self, role: crate::view::Role) -> &str {
        use crate::view::{Role, Side};
        match role {
            Role::Debater(Side::Pro) => &self.pro,
            Role::Debater(Side::Kontra) => &self.kontra,
            Role::Moderator => &self.moderator,
            Role::Synthesizer => &self.synthesizer,
        }
    }
}

/// Fills `{placeholder}` slots in a prompt.
///
/// Deliberately not a template engine: the prompts are written by one person who
/// also writes the call sites, and a real engine would add a dependency plus a
/// second syntax to learn. An unknown placeholder is left untouched rather than
/// erroring — a stray brace in prose should not stop a debate.
pub fn isi(template: &str, vars: &[(&str, &str)]) -> String {
    let mut out = template.to_string();
    for (k, v) in vars {
        out = out.replace(&format!("{{{k}}}"), v);
    }
    out
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("ROUTER_API_KEY tidak ada atau kosong. Isi .env dulu.")]
    MissingApiKey,
    #[error("config.toml tidak terbaca di {0}: {1}")]
    ConfigTidakTerbaca(String, #[source] std::io::Error),
    #[error("config.toml rusak: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("prompt tidak terbaca di {0}: {1}")]
    PromptTidakTerbaca(String, #[source] std::io::Error),
    #[error("config salah: {0}")]
    NilaiSalah(&'static str),
    #[error(
        "bind bukan loopback tanpa TIMBANG_ALLOW_PUBLIC_BIND=1. \
         Set env var itu HANYA kalau ada auth di depan (Cloudflare Access, dst)."
    )]
    BindNonLoopback,
    #[error("Pro dan Kontra memakai model yang sama ({0}). Debat butuh dua model berbeda.")]
    DebaterSama(String),
    #[error(
        "Pro ({pro}) dan Kontra ({kontra}) sama-sama dari lab {lab}. \
         Dua model selab berbagi data latih dan salah bersamaan — pakai lab berbeda."
    )]
    LabSama {
        lab: &'static str,
        pro: String,
        kontra: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_apikey_tidak_membocorkan() {
        let k = ApiKey("rahasia-sekali-12345".into());
        let s = format!("{k:?}");
        assert_eq!(s, "[redacted]");
        assert!(!s.contains("rahasia"));
    }

    #[test]
    fn prefix_jalur_dibuang_saat_banding() {
        let m = ModelId::new("cc/claude-opus-5");
        assert_eq!(m.bare(), "claude-opus-5");
        assert_eq!(m.route(), "cc");
        assert_eq!(ModelId::new("kc/openai/o3").bare(), "o3");
    }

    #[test]
    fn tolak_dua_debater_selab() {
        let mut c = SessionConfig::uji();
        c.models.pro = ModelId::new("cc/claude-opus-5");
        c.models.kontra = ModelId::new("cc/claude-fable-5");
        assert!(matches!(
            c.periksa_lawan_sepadan(),
            Err(ConfigError::LabSama { .. })
        ));
    }

    #[test]
    fn terima_lab_berbeda() {
        let mut c = SessionConfig::uji();
        c.models.pro = ModelId::new("cc/claude-opus-5");
        c.models.kontra = ModelId::new("kr/deepseek-3.2");
        assert!(c.periksa_lawan_sepadan().is_ok());
    }

    #[test]
    fn isi_placeholder() {
        let out = isi("Klaim: {klaim}. Batas {batas} kata.", &[("klaim", "X lebih baik"), ("batas", "200")]);
        assert_eq!(out, "Klaim: X lebih baik. Batas 200 kata.");
    }
}
