//! Transcript → markdown.
//!
//! The output is shaped like a court record, not a chat log (§7). Turns keep
//! their side, phase and round; nothing is summarised, ranked, or scored, and
//! there is deliberately no "conclusion" section — the reader does that part.

use serde::Serialize;

use crate::transcript::{
    ClaimStatus, FramingOption, Penilaian, Provenance, Session, SessionStatus, Turn, TurnFlag,
};
use crate::view::{Phase, Role, Side};

pub fn sesi_ke_markdown(s: &Session) -> String {
    let mut out = String::new();

    out.push_str(&format!("# Sidang: {}\n\n", s.topic));
    if let Some(klaim) = &s.claim {
        out.push_str(&format!("**Klaim**: {klaim}\n\n"));
    }
    out.push_str(&format!("- Sesi: `{}`\n", s.id));
    out.push_str(&format!("- Mulai: {}\n", s.created_at));
    out.push_str(&format!("- Status: {}\n", status_label(&s.status)));
    out.push_str(&format!(
        "- Pro: `{}` — Kontra: `{}`\n",
        s.config_used.models.pro.as_str(),
        s.config_used.models.kontra.as_str()
    ));
    out.push_str(&format!("- Ronde: {}\n\n", s.config_used.rounds));

    let mut fase_sekarang: Option<(Phase, u32)> = None;
    for t in s.transcript.all() {
        let kunci = (t.phase, t.round);
        if fase_sekarang != Some(kunci) {
            let judul = if t.phase.per_ronde() {
                format!("## {} — ronde {}\n\n", t.phase.label(), t.round)
            } else {
                format!("## {}\n\n", t.phase.label())
            };
            out.push_str(&judul);
            fase_sekarang = Some(kunci);
        }

        out.push_str(&format!("### {}\n\n", t.role.label()));

        // Provenance goes above the text, not in a footnote: a turn whose author
        // cannot be verified should be read differently, and that warning is
        // useless after the fact (§5).
        if !t.provenance.trustworthy() {
            out.push_str(&format!("> ⚠ {}\n\n", provenance_label(&t.provenance)));
        }
        for f in &t.flags {
            out.push_str(&format!("> ⚑ {}\n\n", flag_label(f)));
        }

        out.push_str(t.text.trim());
        out.push_str("\n\n");

        out.push_str(&format!(
            "<sub>{} kata · diminta `{}`",
            t.word_count(),
            t.model_requested
        ));
        if let Some(m) = &t.model_answered {
            out.push_str(&format!(" · dijawab `{m}`"));
        }
        if t.retries > 0 {
            out.push_str(&format!(" · diulang {}×", t.retries));
        }
        out.push_str("</sub>\n\n---\n\n");
    }

    if !s.claims.is_empty() {
        out.push_str("## Status klaim\n\n");
        // Unanswered claims come first and are marked hardest. This inverts the
        // usual habit of leading with what was resolved, because what nobody
        // answered is the whole reason this tool exists (§3, §7).
        let sepi = s.tak_pernah_dijawab();
        if !sepi.is_empty() {
            out.push_str("### ⚑ Tidak pernah dijawab\n\n");
            for c in sepi {
                out.push_str(&format!(
                    "- **{}** ({}, ronde {}): {}\n",
                    c.id,
                    c.owner.label(),
                    c.born_round,
                    c.text
                ));
            }
            out.push('\n');
        }
        let lain: Vec<_> = s
            .claims
            .iter()
            .filter(|c| c.status != ClaimStatus::Hidup)
            .collect();
        if !lain.is_empty() {
            out.push_str("### Sudah ditanggapi\n\n");
            for c in lain {
                out.push_str(&format!(
                    "- {} ({}, {}): {}\n",
                    c.id,
                    c.owner.label(),
                    claim_status_label(c.status),
                    c.text
                ));
            }
            out.push('\n');
        }
    }

    if let Some(p) = s.penilaian {
        out.push_str(&format!("## Penilaian\n\nCrux: {}\n\n", penilaian_label(p)));
    }

    out
}

/// A single turn, for `bin/uji` watching one phase at a time.
pub fn turn_ringkas(t: &crate::transcript::Turn) -> String {
    let mut out = format!(
        "── {} · {} · ronde {} ─────────────\n",
        t.role.label(),
        t.phase.label(),
        t.round
    );
    if !t.provenance.trustworthy() {
        out.push_str(&format!("⚠ {}\n", provenance_label(&t.provenance)));
    }
    for f in &t.flags {
        out.push_str(&format!("⚑ {}\n", flag_label(f)));
    }
    out.push_str(t.text.trim());
    out.push_str(&format!("\n\n[{} kata", t.word_count()));
    if let Some(n) = t.completion_tokens {
        out.push_str(&format!(" · {n} token"));
    }
    if t.retries > 0 {
        out.push_str(&format!(" · diulang {}×", t.retries));
    }
    out.push_str("]\n");
    out
}

fn status_label(s: &SessionStatus) -> String {
    match s {
        SessionStatus::MenungguPersetujuan => "menunggu persetujuan framing".into(),
        SessionStatus::Berjalan => "berjalan".into(),
        SessionStatus::Gagal { at_phase } => format!("berhenti di fase {}", at_phase.label()),
        SessionStatus::Selesai => "selesai".into(),
    }
}

fn provenance_label(p: &Provenance) -> String {
    match p {
        Provenance::Verified => "model terverifikasi".into(),
        Provenance::Unverifiable { reason } => {
            format!("penulis tidak terverifikasi: {reason}")
        }
        Provenance::Substituted { answered } => {
            format!("DIJAWAB MODEL LAIN: {answered} — jangan bandingkan sesi ini")
        }
    }
}

fn flag_label(f: &TurnFlag) -> String {
    match f {
        TurnFlag::GagalMembantah => "gagal membantah: tidak menyentuh argumen lawan".into(),
        TurnFlag::MengulangDiri { similarity } => {
            format!("mengulang diri sendiri ({:.0}% mirip ronde lalu)", similarity * 100.0)
        }
        TurnFlag::Terpotong => "terpotong di batas token — bukan argumen lemah".into(),
    }
}

fn claim_status_label(s: ClaimStatus) -> &'static str {
    match s {
        ClaimStatus::Hidup => "belum dijawab",
        ClaimStatus::Terbantah => "terbantah",
        ClaimStatus::Diabaikan => "diabaikan",
    }
}

fn penilaian_label(p: Penilaian) -> &'static str {
    match p {
        Penilaian::Kepakai => "kepakai",
        Penilaian::Setengah => "setengah",
        Penilaian::Tidak => "tidak kepakai",
    }
}

/// Turn-length per round, one of the diagnostics §8 allows.
///
/// A sharp drop in the last rounds means the round count is too high — an
/// observation with an obvious action, which is what separates a diagnostic from
/// decoration.
pub fn panjang_per_ronde(s: &Session) -> Vec<(u32, usize, usize)> {
    let mut out = Vec::new();
    for r in 1..=s.config_used.rounds {
        let pro: usize = s
            .transcript
            .all()
            .iter()
            .filter(|t| t.round == r && matches!(t.role, Role::Debater(crate::view::Side::Pro)))
            .map(|t| t.word_count())
            .sum();
        let kontra: usize = s
            .transcript
            .all()
            .iter()
            .filter(|t| t.round == r && matches!(t.role, Role::Debater(crate::view::Side::Kontra)))
            .map(|t| t.word_count())
            .sum();
        out.push((r, pro, kontra));
    }
    out
}

// ─────────────────────────────────────────────────────────────────────────────
// Browser DTOs.
//
// These are the ONLY shapes that reach the browser. They carry no key, no base
// URL, and none of the internal enums — the separation §6 demands, applied to
// the wire and not just to the config struct. If a field is here, it was chosen
// to be shown; nothing leaks by being forgotten.

/// One turn, flattened for the two-column view. `side` decides the column;
/// `phase_key` groups turns into phase headers without the browser knowing the
/// phase enum.
#[derive(Serialize, Clone)]
pub struct WebTurn {
    pub index: usize,
    pub side: &'static str,
    pub role_label: &'static str,
    pub phase: &'static str,
    pub phase_key: String,
    pub round: u32,
    pub per_ronde: bool,
    pub text: String,
    pub word_count: usize,
    pub flags: Vec<String>,
    /// Present only when the author cannot be trusted — shown above the turn,
    /// because that warning is useless after the fact (§5).
    pub provenance_warning: Option<String>,
    pub truncated: bool,
    pub speaking_order: u8,
}

pub fn turn_web(index: usize, t: &Turn) -> WebTurn {
    let side = match t.role {
        Role::Debater(Side::Pro) => "pro",
        Role::Debater(Side::Kontra) => "kontra",
        Role::Moderator => "moderator",
        Role::Synthesizer => "sintesis",
    };
    WebTurn {
        index,
        side,
        role_label: t.role.label(),
        phase: t.phase.label(),
        phase_key: format!("{:?}-{}", t.phase, t.round),
        round: t.round,
        per_ronde: t.phase.per_ronde(),
        text: t.text.trim().to_string(),
        word_count: t.word_count(),
        flags: t.flags.iter().map(flag_label).collect(),
        provenance_warning: (!t.provenance.trustworthy()).then(|| provenance_label(&t.provenance)),
        truncated: t.flags.iter().any(|f| matches!(f, TurnFlag::Terpotong)),
        speaking_order: t.speaking_order,
    }
}

/// A whole session as the browser sees it. Meta plus every turn so far, no more.
///
/// `status` is a machine key (`menunggu_persetujuan`, `berjalan`, `gagal`,
/// `selesai`) so the page can branch without parsing prose, and `status_label`
/// is the Indonesian the user reads.
#[derive(Serialize)]
pub struct SessionView {
    pub id: String,
    pub topik: String,
    pub konteks: Option<String>,
    pub klaim: Option<String>,
    pub status: &'static str,
    pub status_label: String,
    pub phase: &'static str,
    pub round: u32,
    pub framing_options: Vec<FramingOption>,
    pub turns: Vec<WebTurn>,
    pub penilaian: Option<&'static str>,
    pub models: ModelsView,
    pub rounds: u32,
    pub word_limit: u32,
}

#[derive(Serialize)]
pub struct ModelsView {
    pub pro: String,
    pub kontra: String,
    pub moderator: String,
}

fn status_key(s: &SessionStatus) -> &'static str {
    match s {
        SessionStatus::MenungguPersetujuan => "menunggu_persetujuan",
        SessionStatus::Berjalan => "berjalan",
        SessionStatus::Gagal { .. } => "gagal",
        SessionStatus::Selesai => "selesai",
    }
}

impl SessionView {
    /// Build the view straight from a session on disk. Used for finished or
    /// idle sessions; a live one overrides `turns` and `status` with the
    /// in-memory backlog, which runs ahead of the per-phase checkpoint.
    pub fn from_session(s: &Session) -> Self {
        let turns = s
            .transcript
            .all()
            .iter()
            .enumerate()
            .map(|(i, t)| turn_web(i, t))
            .collect();
        SessionView {
            id: s.id.clone(),
            topik: s.topic.clone(),
            konteks: s.context.clone(),
            klaim: s.claim.clone(),
            status: status_key(&s.status),
            status_label: status_label(&s.status),
            phase: s.phase.label(),
            round: s.round,
            framing_options: s.framing_options.clone(),
            turns,
            penilaian: s.penilaian.map(penilaian_label),
            models: ModelsView {
                pro: s.config_used.models.pro.as_str().to_string(),
                kontra: s.config_used.models.kontra.as_str().to_string(),
                moderator: s.config_used.models.moderator.as_str().to_string(),
            },
            rounds: s.config_used.rounds,
            word_limit: s.config_used.word_limit,
        }
    }
}

/// A one-line entry for the history list (§7). Carries the diagnostics §8
/// permits and nothing that measures a winner.
#[derive(Serialize)]
pub struct RiwayatEntry {
    pub id: String,
    pub topik: String,
    pub klaim: Option<String>,
    pub status: &'static str,
    pub status_label: String,
    pub created_at: String,
    pub pro: String,
    pub kontra: String,
    pub rounds: u32,
    pub turns: usize,
    /// Turns flagged for any reason — a debate-health signal, not a score.
    pub turns_bertanda: usize,
    pub penilaian: Option<&'static str>,
}

impl RiwayatEntry {
    pub fn from_session(s: &Session) -> Self {
        let turns_bertanda = s.transcript.all().iter().filter(|t| !t.flags.is_empty()).count();
        RiwayatEntry {
            id: s.id.clone(),
            topik: s.topic.clone(),
            klaim: s.claim.clone(),
            status: status_key(&s.status),
            status_label: status_label(&s.status),
            created_at: s.created_at.clone(),
            pro: s.config_used.models.pro.as_str().to_string(),
            kontra: s.config_used.models.kontra.as_str().to_string(),
            rounds: s.config_used.rounds,
            turns: s.transcript.len(),
            turns_bertanda,
            penilaian: s.penilaian.map(penilaian_label),
        }
    }
}
