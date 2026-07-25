//! Runs a session: turn by turn, phase by phase, checkpointing as it goes.
//!
//! Thin by design. The rules live in `view` (who sees what) and `phase` (what
//! counts); this module only sequences them and writes the result down.

use std::path::PathBuf;

use crate::config::{Config, Prompts, isi};
use crate::llm::{Client, LlmError, Message};
use crate::phase::{self, Putusan};
use crate::render;
use crate::transcript::{
    Provenance, Session, SessionStatus, Turn, TurnFlag, simpan,
};
use crate::view::{Phase, Role, Side, View, build_view};

pub struct Engine {
    pub client: Client,
    pub cfg: Config,
    pub prompts: Prompts,
    pub sessions_dir: PathBuf,
    /// Where per-turn progress goes. `bin/uji` prints to stdout; `bin/web` will
    /// push to SSE (Tahap 2).
    pub on_turn: Box<dyn Fn(&Turn) + Send + Sync>,
}

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error(transparent)]
    Llm(#[from] LlmError),
    #[error(transparent)]
    Transcript(#[from] crate::transcript::TranscriptError),
    #[error("sesi belum punya klaim yang disetujui — jalankan framing dulu")]
    BelumAdaKlaim,
}

impl Engine {
    /// Asks the moderator for 2–3 claim wordings and stops.
    ///
    /// The debate does not start here. §4: how the claim is worded decides who
    /// wins, so letting the moderator pick unseen would settle part of the
    /// question before anyone argues — an epistemic bug that no amount of code
    /// downstream can repair.
    pub async fn framing(&self, s: &mut Session) -> Result<(), EngineError> {
        let prompt = isi(
            &self.prompts.moderator,
            &[
                ("topik", &s.topic),
                ("konteks", s.context.as_deref().unwrap_or("(tidak ada)")),
            ],
        );

        let c = self
            .client
            .kirim(
                &self.cfg.models.moderator,
                &[Message::system(prompt), Message::user(&s.topic)],
                self.cfg.temperature,
                1500,
            )
            .await?;

        s.framing_options = parse_framing(&c.text);
        s.transcript.push(bikin_turn(
            1,
            Phase::Framing,
            Role::Moderator,
            &self.cfg.models.moderator,
            &c,
            0,
            0,
            Vec::new(),
        ));
        s.status = SessionStatus::MenungguPersetujuan;
        (self.on_turn)(s.transcript.all().last().unwrap());
        simpan(s, &self.sessions_dir).await?;
        Ok(())
    }

    /// Runs every remaining phase to the end, checkpointing after each.
    ///
    /// Also the resume path: a session loaded from a `Gagal` checkpoint picks up
    /// at its stored phase and round, and `jalankan_fase` skips whoever already
    /// spoke there. §5 promises the user continues rather than starts over.
    pub async fn jalankan(&self, s: &mut Session) -> Result<(), EngineError> {
        if s.claim.is_none() {
            return Err(EngineError::BelumAdaKlaim);
        }
        s.status = SessionStatus::Berjalan;

        loop {
            if s.phase == Phase::Framing {
                s.phase = Phase::Opening;
            }
            if s.phase == Phase::Selesai {
                break;
            }

            // Synthesis is the last thing that happens, and §1 requires it stay
            // shut until then. Tahap 1 stops before it: the synthesizer prompt
            // is unwritten, and a placeholder would be a summary appearing early
            // — precisely the failure the rule exists to prevent.
            if s.phase == Phase::Sintesis {
                s.phase = Phase::Selesai;
                break;
            }

            if let Err(e) = self.jalankan_fase(s).await {
                s.status = SessionStatus::Gagal { at_phase: s.phase };
                simpan(s, &self.sessions_dir).await?;
                return Err(e);
            }

            simpan(s, &self.sessions_dir).await?;

            match phase::maju(s.phase, s.round, s.config_used.rounds) {
                Some((p, r)) => {
                    s.phase = p;
                    s.round = r;
                }
                None => break,
            }
        }

        s.status = SessionStatus::Selesai;
        s.phase = Phase::Selesai;
        simpan(s, &self.sessions_dir).await?;
        Ok(())
    }

    async fn jalankan_fase(&self, s: &mut Session) -> Result<(), EngineError> {
        let fase = s.phase;
        let ronde = s.round;

        // Only the roles that have not spoken yet. On a fresh phase that is all
        // of them; on a resumed one it is whoever the failure interrupted. The
        // order index comes from `giliran_tersisa`, which keeps each role's
        // original position — see the note there.
        for (urutan, role) in phase::giliran_tersisa(&s.transcript, fase, ronde) {
            let view = build_view(&s.transcript, fase, role, ronde);
            if !view.acts() {
                continue;
            }
            let turn = self.satu_turn(s, fase, role, ronde, urutan, &view).await?;
            (self.on_turn)(&turn);
            s.transcript.push(turn);
        }
        Ok(())
    }

    /// One turn, including the single quality retry §4 allows.
    ///
    /// Note the two loops are separate: network retries already happened inside
    /// `llm.kirim` before control gets here, and this loop only ever re-asks
    /// because of what the model *said* (§5).
    async fn satu_turn(
        &self,
        s: &Session,
        fase: Phase,
        role: Role,
        ronde: u32,
        urutan: u8,
        view: &View<'_>,
    ) -> Result<Turn, EngineError> {
        let model = model_untuk(&self.cfg, role);
        let klaim = s.claim.as_deref().unwrap_or(&s.topic);

        let mut teguran: Option<String> = None;
        let mut percobaan = 0u32;

        loop {
            percobaan += 1;

            let sistem = isi(
                self.prompts.untuk(role),
                &[
                    ("klaim", klaim),
                    ("fase", fase.label()),
                    ("ronde", &ronde.to_string()),
                    ("batas_kata", &self.cfg.word_limit.to_string()),
                ],
            );

            let mut pesan = vec![Message::system(sistem)];
            pesan.push(Message::user(rangkai_view(view, klaim)));
            if let Some(t) = &teguran {
                pesan.push(Message::user(t.clone()));
            }

            // Generous on purpose. Thinking models spend most of their budget
            // reasoning before writing a word: across 10 test sessions Opus 5
            // averaged 1572 completion tokens for ~200 words of text while
            // DeepSeek averaged 481 for the same length. At `word_limit * 8`
            // that ceiling cut 11 turns — all of them Opus, mid-sentence.
            //
            // A ceiling that binds one model and not the other is a lopsided
            // debate produced by config rather than by argument, which §10 calls
            // more misleading than a soft one. `max_tokens` is only an upper
            // bound, so the slack costs nothing for models that stop earlier.
            let max_tokens = (self.cfg.word_limit * 24).max(2000);

            let c = self
                .client
                .kirim(&model, &pesan, self.cfg.temperature, max_tokens)
                .await?;

            let side = match role {
                Role::Debater(sd) => sd,
                // Only debaters have a bar to clear.
                Role::Moderator | Role::Synthesizer => {
                    return Ok(bikin_turn(
                        ronde, fase, role, &model, &c, urutan,
                        percobaan - 1, Vec::new(),
                    ));
                }
            };

            let putusan = phase::nilai_turn(
                &s.transcript,
                &s.config_used,
                fase,
                side,
                ronde,
                &c.text,
                c.finish_reason.as_deref(),
                percobaan,
                &self.prompts.teguran,
            );

            match putusan {
                Putusan::Lolos => {
                    return Ok(bikin_turn(
                        ronde, fase, role, &model, &c, urutan,
                        percobaan - 1, Vec::new(),
                    ));
                }
                Putusan::Catat { flags } => {
                    return Ok(bikin_turn(
                        ronde, fase, role, &model, &c, urutan,
                        percobaan - 1, flags,
                    ));
                }
                Putusan::UlangSekali { teguran: t, .. } => {
                    teguran = Some(t);
                }
            }
        }
    }
}

fn model_untuk(cfg: &Config, role: Role) -> crate::config::ModelId {
    match role {
        Role::Debater(Side::Pro) => cfg.models.pro.clone(),
        Role::Debater(Side::Kontra) => cfg.models.kontra.clone(),
        Role::Moderator => cfg.models.moderator.clone(),
        Role::Synthesizer => cfg.models.synthesizer.clone(),
    }
}

/// Turns a view into the user-message text the model actually reads.
fn rangkai_view(view: &View<'_>, klaim: &str) -> String {
    match view {
        View::Silent => String::new(),
        View::Blind => format!("KLAIM YANG DIPERDEBATKAN:\n{klaim}"),
        View::Turns(turns) => {
            let mut out = format!("KLAIM YANG DIPERDEBATKAN:\n{klaim}\n\n");
            if turns.is_empty() {
                out.push_str("(belum ada yang bicara)");
                return out;
            }
            out.push_str("YANG SUDAH DIKATAKAN SEJAUH INI:\n\n");
            for t in turns.iter() {
                out.push_str(&format!(
                    "[{} · {} · ronde {}]\n{}\n\n",
                    t.role.label(),
                    t.phase.label(),
                    t.round,
                    t.text.trim()
                ));
            }
            out
        }
    }
}

fn bikin_turn(
    ronde: u32,
    fase: Phase,
    role: Role,
    model: &crate::config::ModelId,
    c: &crate::llm::Completion,
    urutan: u8,
    retries: u32,
    mut flags: Vec<TurnFlag>,
) -> Turn {
    // Provenance, judged here rather than in `llm`: comparing the reported model
    // against the requested one is a debate-integrity question, not a transport
    // one. This router echoes the requested id, so `Verified` is unreachable
    // through it — the transcript says so rather than implying a check happened.
    let provenance = match c.model_reported.as_deref() {
        None => Provenance::Unverifiable {
            reason: "respons tidak menyebut model".into(),
        },
        Some(dijawab) => {
            let diminta_bare = model.bare();
            let dijawab_bare = dijawab.rsplit('/').next().unwrap_or(dijawab);
            if dijawab_bare == diminta_bare {
                Provenance::Unverifiable {
                    reason: "router memantulkan id yang diminta, bukan yang menjawab".into(),
                }
            } else {
                Provenance::Substituted {
                    answered: dijawab.to_string(),
                }
            }
        }
    };

    if matches!(provenance, Provenance::Substituted { .. })
        && !flags.iter().any(|f| matches!(f, TurnFlag::Terpotong))
    {
        // Nothing to add to flags — substitution is carried by provenance — but
        // the check keeps the two from being conflated later.
    }

    Turn {
        round: ronde,
        phase: fase,
        role,
        model_requested: model.as_str().to_string(),
        model_answered: c.model_reported.clone(),
        provenance,
        text: c.text.clone(),
        at: jiff::Timestamp::now().to_string(),
        prompt_tokens: c.prompt_tokens,
        completion_tokens: c.completion_tokens,
        finish_reason: c.finish_reason.clone(),
        retries,
        speaking_order: urutan,
        flags: {
            flags.dedup();
            flags
        },
    }
}

/// Pulls the numbered claim wordings out of the moderator's reply.
///
/// Tolerant on purpose: a moderator that formats its list slightly differently
/// should not abort a session before it starts.
fn parse_framing(text: &str) -> Vec<crate::transcript::FramingOption> {
    let mut out = Vec::new();
    let mut current: Option<(String, String)> = None;

    for baris in text.lines() {
        let b = baris.trim();
        if b.is_empty() {
            continue;
        }
        let mulai_baru = b
            .chars()
            .next()
            .map(|c| c.is_ascii_digit())
            .unwrap_or(false)
            && b.contains('.');

        if mulai_baru {
            if let Some((t, bias)) = current.take() {
                out.push(crate::transcript::FramingOption { text: t, bias });
            }
            let isi = b.splitn(2, '.').nth(1).unwrap_or(b).trim().to_string();
            current = Some((bersihkan(&isi), String::new()));
        } else if let Some((_, bias)) = current.as_mut() {
            let lower = b.to_lowercase();
            if lower.starts_with("bias") || lower.starts_with("- bias") || lower.starts_with("*bias") {
                let v = b.splitn(2, ':').nth(1).unwrap_or(b).trim();
                if !bias.is_empty() {
                    bias.push(' ');
                }
                bias.push_str(v);
            }
        }
    }
    if let Some((t, bias)) = current {
        out.push(crate::transcript::FramingOption { text: t, bias });
    }
    out
}

fn bersihkan(s: &str) -> String {
    s.trim().trim_matches(|c| c == '*' || c == '"').trim().to_string()
}

/// Markdown for a finished (or stopped) session.
pub fn markdown(s: &Session) -> String {
    render::sesi_ke_markdown(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_framing_bernomor() {
        let teks = "Berikut tiga rumusan:\n\
                    1. Rust lebih aman daripada Go untuk server web\n\
                    Bias: memihak Rust karena 'aman' adalah kekuatannya\n\
                    2. Go lebih produktif daripada Rust untuk server web\n\
                    Bias: memihak Go karena 'produktif' adalah kekuatannya\n";
        let opts = parse_framing(teks);
        assert_eq!(opts.len(), 2);
        assert!(opts[0].text.contains("Rust lebih aman"));
        assert!(opts[0].bias.contains("memihak Rust"));
    }

    #[test]
    fn view_buta_hanya_berisi_klaim() {
        let out = rangkai_view(&View::Blind, "X lebih baik dari Y");
        assert!(out.contains("X lebih baik dari Y"));
        assert!(!out.contains("SEJAUH INI"));
    }
}
