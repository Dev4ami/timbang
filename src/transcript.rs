//! Turn, Claim, Session — and the files they live in.
//!
//! The file is the source of truth (§2). Everything here is written so a session
//! saved today still reads correctly years from now, under rules that may have
//! changed in the meantime: field names are stable, timestamps are RFC 3339
//! text, and anything derived from a rule (speaking order) is stored rather than
//! recomputed on load.
//!
//! Field names are English even though §3 writes the model in Indonesian; that
//! table is a glossary, and the opening line of CLAUDE.md sets code language to
//! English. Indonesian survives in everything the user actually reads.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::view::{Phase, Role, Side};

/// Where a turn's text came from, and whether that can be trusted.
///
/// This router echoes the model id that was *requested*, not the one that
/// answered, so `Verified` is not currently reachable through it. The variant
/// exists because the distinction is real and a future router may support it —
/// and because collapsing it would quietly turn "we checked" and "we could not
/// check" into the same claim, which is the exact failure §5 warns about.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provenance {
    /// The response named a concrete model and it matched the request.
    Verified,
    /// The response echoed the request, or omitted the model entirely. The
    /// transcript cannot prove who wrote this.
    Unverifiable { reason: String },
    /// The response named a different model than the one asked for.
    Substituted { answered: String },
}

impl Provenance {
    pub fn trustworthy(&self) -> bool {
        match self {
            Provenance::Verified => true,
            Provenance::Unverifiable { .. } | Provenance::Substituted { .. } => false,
        }
    }
}

/// Why a turn failed to meet its phase's bar, if it did.
///
/// §4: a turn that fails to rebut is *information*, not an error. It stays in
/// the transcript, visibly marked, because a side that stopped engaging is
/// exactly what the user is looking for.
// No `Eq`: the similarity score is an f32. That is fine — nothing compares
// flags for exact equality outside tests.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnFlag {
    /// Never addressed an opposing claim, across both attempts.
    GagalMembantah,
    /// Too similar to this side's own previous turn — the debate is going in
    /// circles.
    MengulangDiri { similarity: f32 },
    /// The model stopped because it hit the token ceiling. Recorded because a
    /// truncated turn reads exactly like a weak one.
    Terpotong,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Turn {
    pub round: u32,
    pub phase: Phase,
    pub role: Role,
    /// Model id as sent in the request.
    pub model_requested: String,
    /// Model id as reported by the response body, verbatim and unparsed.
    pub model_answered: Option<String>,
    pub provenance: Provenance,
    pub text: String,
    /// RFC 3339. Stored as text so the file stays readable without this crate.
    pub at: String,
    pub prompt_tokens: Option<u32>,
    pub completion_tokens: Option<u32>,
    /// `finish_reason` from the response, kept for the truncation diagnostic (§8).
    pub finish_reason: Option<String>,
    /// How many times this turn was re-requested for quality reasons (§8).
    /// Network retries are not counted here — those belong to `llm` and say
    /// nothing about the debate.
    pub retries: u32,
    /// Position in this round's speaking order, stored rather than derived, so
    /// changing the ordering rule cannot rewrite the meaning of old sessions.
    pub speaking_order: u8,
    pub flags: Vec<TurnFlag>,
    /// Ids of opposing claims this turn attacked, as judged by the moderator's
    /// extraction pass (§3). Drives the "→ menyerang K5" chip; a turn with an
    /// empty list is one that engaged nothing, which §7 wants visible at a
    /// glance. `serde(default)` so sessions written before Tahap 3 still load.
    #[serde(default)]
    pub attacks: Vec<String>,
}

impl Turn {
    pub fn side(&self) -> Option<Side> {
        match self.role {
            Role::Debater(s) => Some(s),
            Role::Moderator | Role::Synthesizer => None,
        }
    }

    /// Word count, used for the turn-length-per-round diagnostic (§8).
    pub fn word_count(&self) -> usize {
        self.text.split_whitespace().count()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaimStatus {
    /// Stated, never answered. The most interesting status in the whole system.
    Hidup,
    Terbantah,
    /// Explicitly dropped by its own side.
    Diabaikan,
}

/// Whether a claim is a checkable fact or a value judgement.
///
/// Set by the fact-check pass. `BelumDiklasifikasi` is the default so sessions
/// written before Tahap 4 still load — a Tahap-3 session's claims read back with
/// no verdict at all rather than a wrong one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ClaimKind {
    #[default]
    BelumDiklasifikasi,
    /// A checkable factual assertion — dates, numbers, cited studies.
    Faktual,
    /// A value judgement, prediction, or normative claim — not checkable
    /// against sources, so fact-check has nothing to say about it.
    Opini,
}

/// The outcome of a fact-check on a Faktual claim. §1 forbids using these as
/// scores or winner signals — they exist to flag "check this yourself", nothing
/// more. In particular, no per-side aggregate ("Pro N% supported") is ever
/// derived from these values (§8, §10).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FactVerdict {
    /// Consistent with the fact-checker's own knowledge.
    Terdukung,
    /// The fact-checker sees at least one specific reason to doubt.
    Diragukan,
    /// The claim is too vague, too recent, or too niche to verify.
    TidakBisaVerifikasi,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FactCheck {
    pub verdict: FactVerdict,
    /// One or two sentences the user can act on — what to check, or why to
    /// doubt. Not the fact-checker's whole reasoning.
    pub catatan: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claim {
    pub id: String,
    pub owner: Side,
    pub text: String,
    pub status: ClaimStatus,
    pub born_round: u32,
    /// Fact-check classification. `serde(default)` so older sessions load.
    #[serde(default)]
    pub kind: ClaimKind,
    /// The fact-check verdict, when `kind == Faktual`. `None` for Opini or
    /// unclassified claims, and also `None` on a Faktual claim whose check
    /// failed at the network layer — auxiliary, never fatal (§4).
    #[serde(default)]
    pub fact_check: Option<FactCheck>,
}

impl Claim {
    /// A freshly extracted claim, before fact-check has run — `kind` defaults
    /// to `BelumDiklasifikasi` and `fact_check` to `None`.
    pub fn baru(id: String, owner: Side, text: String, born_round: u32) -> Self {
        Claim {
            id,
            owner,
            text,
            status: ClaimStatus::Hidup,
            born_round,
            kind: ClaimKind::BelumDiklasifikasi,
            fact_check: None,
        }
    }
}

/// The user's one-tap verdict on whether the crux was usable (§3).
///
/// Deliberately not `Ord`, not convertible to a number, and not summable. §1
/// forbids scoring, and a three-valued field with an ordering is one `map` away
/// from becoming "62% useful across 13 sessions" — a statistic that would feel
/// like evidence while measuring nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Penilaian {
    Kepakai,
    Setengah,
    Tidak,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    /// The moderator has proposed claim wordings; the debate cannot start until
    /// the user picks one (§4).
    MenungguPersetujuan,
    Berjalan,
    /// Stopped mid-debate. The transcript up to here is intact and the session
    /// can be resumed rather than restarted (§5).
    Gagal { at_phase: Phase },
    Selesai,
}

/// The ordered turns of one session, plus the queries `view` needs.
///
/// Kept separate from [`Session`] so the view builder borrows only what it reads.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Transcript {
    turns: Vec<Turn>,
}

impl Transcript {
    pub fn push(&mut self, turn: Turn) {
        self.turns.push(turn);
    }

    pub fn all(&self) -> Vec<&Turn> {
        self.turns.iter().collect()
    }

    pub fn get(&self, i: usize) -> Option<&Turn> {
        self.turns.get(i)
    }

    pub fn last(&self) -> Option<&Turn> {
        self.turns.last()
    }

    pub fn is_empty(&self) -> bool {
        self.turns.is_empty()
    }

    pub fn len(&self) -> usize {
        self.turns.len()
    }

    pub fn turns_of_side(&self, side: Side) -> Vec<&Turn> {
        self.turns
            .iter()
            .filter(|t| t.side() == Some(side))
            .collect()
    }

    /// Everything already recorded when `side` takes its turn in
    /// (`phase`, `round`) — that is, every turn from an earlier phase or round,
    /// plus same-slot turns by whoever spoke before it.
    ///
    /// This is what makes §4's asymmetry fall out of speaking order instead of
    /// needing a rule of its own: the side that goes second sees one more turn.
    pub fn sebelum_giliran(&self, side: Side, phase: Phase, round: u32) -> Vec<&Turn> {
        self.turns
            .iter()
            .take_while(|t| !(t.round == round && t.phase == phase && t.side() == Some(side)))
            .collect()
    }

    /// The cross-exam questions addressed to `side` in `round` — the opponent's
    /// ask-phase turns, and nothing else.
    pub fn pertanyaan_untuk(&self, side: Side, round: u32) -> Vec<&Turn> {
        self.turns
            .iter()
            .filter(|t| {
                t.round == round
                    && t.phase == Phase::CrossExamAsk
                    && t.side() == Some(side.lawan())
            })
            .collect()
    }

    pub fn last_of_side(&self, side: Side) -> Option<&Turn> {
        self.turns.iter().rev().find(|t| t.side() == Some(side))
    }

    /// Has this role already taken its turn in this (phase, round)?
    ///
    /// Used when resuming: a session that stopped partway through a phase has
    /// some of that phase's turns on disk already, and re-asking for them would
    /// bill twice and leave one side apparently speaking twice in a row.
    pub fn sudah_bicara(&self, phase: Phase, role: Role, round: u32) -> bool {
        self.turns
            .iter()
            .any(|t| t.phase == phase && t.role == role && t.round == round)
    }

    /// This side's turn in the previous round of the same phase — the baseline
    /// the repetition check compares against (§4).
    pub fn turn_ronde_lalu(&self, side: Side, phase: Phase, round: u32) -> Option<&Turn> {
        if round <= 1 {
            return None;
        }
        self.turns.iter().rev().find(|t| {
            t.round == round - 1 && t.phase == phase && t.side() == Some(side)
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    /// The agreed claim. `None` until the user approves a wording (§4).
    pub claim: Option<String>,
    /// Wordings the moderator proposed, each with its stated bias.
    pub framing_options: Vec<FramingOption>,
    pub topic: String,
    pub context: Option<String>,
    pub status: SessionStatus,
    pub phase: Phase,
    pub round: u32,
    /// The config this session actually ran with, stored in the session file so
    /// old sessions stay reproducible and comparable after the global config
    /// changes (§3).
    pub config_used: crate::config::SessionConfig,
    pub transcript: Transcript,
    pub claims: Vec<Claim>,
    /// Filled in by the user after reading the synthesis. The only datum here
    /// that cannot be reconstructed later, which is why it exists from Tahap 1
    /// even though the synthesis phase does not yet (§3).
    pub penilaian: Option<Penilaian>,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FramingOption {
    pub text: String,
    /// How this wording tilts the debate. Shown to the user precisely so the
    /// tilt is a choice rather than an accident (§4).
    pub bias: String,
}

impl Session {
    pub fn new(topic: String, context: Option<String>, config_used: crate::config::SessionConfig) -> Self {
        Session {
            id: uuid::Uuid::new_v4().to_string(),
            claim: None,
            framing_options: Vec::new(),
            topic,
            context,
            status: SessionStatus::MenungguPersetujuan,
            phase: Phase::Framing,
            round: 1,
            config_used,
            transcript: Transcript::default(),
            claims: Vec::new(),
            penilaian: None,
            created_at: jiff::Timestamp::now().to_string(),
        }
    }

    /// Claims that were raised and never answered by anyone.
    ///
    /// §3 calls this the most useful question the project can answer: it is
    /// usually a blind spot shared by both models, and therefore likely the
    /// user's blind spot too.
    pub fn tak_pernah_dijawab(&self) -> Vec<&Claim> {
        self.claims
            .iter()
            .filter(|c| c.status == ClaimStatus::Hidup)
            .collect()
    }

    pub fn path_in(&self, dir: &Path) -> PathBuf {
        dir.join(format!("{}.json", self.id))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TranscriptError {
    #[error("gagal baca/tulis file sesi: {0}")]
    Io(#[from] std::io::Error),
    #[error("file sesi rusak atau format tidak dikenal: {0}")]
    Format(#[from] serde_json::Error),
}

/// Writes a checkpoint, atomically.
///
/// Called after every phase, not just at the end (§4), so a router failure at
/// round 4 of 6 leaves a resumable session rather than nothing.
///
/// Write-then-rename: a crash mid-write damages only the temp file, and the
/// real session file is either the previous checkpoint or the new one, never a
/// half-written mixture. On Windows `rename` refuses to clobber, so the
/// replacement is done explicitly.
pub async fn simpan(session: &Session, dir: &Path) -> Result<PathBuf, TranscriptError> {
    tokio::fs::create_dir_all(dir).await?;

    let final_path = session.path_in(dir);
    let tmp_path = dir.join(format!(".{}.tmp", session.id));

    let json = serde_json::to_vec_pretty(session)?;
    tokio::fs::write(&tmp_path, &json).await?;

    // std::fs::rename overwrites on Unix but fails on Windows if the target
    // exists, so remove first. The gap between the two is the one moment a
    // crash loses the previous checkpoint; the temp file still holds the new
    // one, so nothing is unrecoverable.
    if tokio::fs::try_exists(&final_path).await? {
        tokio::fs::remove_file(&final_path).await?;
    }
    tokio::fs::rename(&tmp_path, &final_path).await?;

    Ok(final_path)
}

pub async fn muat(path: &Path) -> Result<Session, TranscriptError> {
    let bytes = tokio::fs::read(path).await?;
    Ok(serde_json::from_slice(&bytes)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(round: u32, phase: Phase, role: Role, order: u8) -> Turn {
        Turn {
            round,
            phase,
            role,
            model_requested: "uji/model".into(),
            model_answered: None,
            provenance: Provenance::Unverifiable {
                reason: "uji".into(),
            },
            text: format!("{} r{}", role.label(), round),
            at: "2026-01-01T00:00:00Z".into(),
            prompt_tokens: None,
            completion_tokens: None,
            finish_reason: None,
            retries: 0,
            speaking_order: order,
            flags: Vec::new(),
            attacks: Vec::new(),
        }
    }

    /// The side that speaks second must see one more turn than the side that
    /// speaks first. That asymmetry is why §4 requires alternating the order.
    #[test]
    fn giliran_kedua_melihat_lebih_banyak() {
        let mut t = Transcript::default();
        t.push(turn(1, Phase::Opening, Role::Debater(Side::Pro), 0));
        t.push(turn(1, Phase::Opening, Role::Debater(Side::Kontra), 1));
        t.push(turn(1, Phase::Rebuttal, Role::Debater(Side::Pro), 0));

        let pro = t.sebelum_giliran(Side::Pro, Phase::Rebuttal, 1);
        let kontra = t.sebelum_giliran(Side::Kontra, Phase::Rebuttal, 1);

        assert_eq!(pro.len(), 2, "Pro bicara duluan, melihat dua opening");
        assert_eq!(kontra.len(), 3, "Kontra bicara belakangan, plus rebuttal Pro");
    }

    #[test]
    fn pertanyaan_hanya_dari_lawan() {
        let mut t = Transcript::default();
        t.push(turn(1, Phase::CrossExamAsk, Role::Debater(Side::Pro), 0));
        t.push(turn(1, Phase::CrossExamAsk, Role::Debater(Side::Kontra), 1));

        let untuk_pro = t.pertanyaan_untuk(Side::Pro, 1);
        assert_eq!(untuk_pro.len(), 1);
        assert_eq!(untuk_pro[0].side(), Some(Side::Kontra));
    }

    #[test]
    fn klaim_hidup_adalah_yang_tak_pernah_dijawab() {
        let mut s = Session::new("uji".into(), None, crate::config::SessionConfig::uji());
        let mut k2 = Claim::baru("K2".into(), Side::Kontra, "b".into(), 1);
        k2.status = ClaimStatus::Terbantah;
        s.claims = vec![
            Claim::baru("K1".into(), Side::Pro, "a".into(), 1),
            k2,
        ];
        let sepi = s.tak_pernah_dijawab();
        assert_eq!(sepi.len(), 1);
        assert_eq!(sepi[0].id, "K1");
    }
}
