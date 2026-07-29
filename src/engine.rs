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
    Claim, ClaimKind, ClaimStatus, FactCheck, FactVerdict, Provenance, Session, SessionStatus,
    Turn, TurnFlag, simpan,
};
use crate::view::{Phase, Role, Side, View, build_view};

pub struct Engine {
    pub client: Client,
    pub cfg: Config,
    pub prompts: Prompts,
    pub sessions_dir: PathBuf,
    /// Where per-turn progress goes, with the turn's index in the transcript.
    /// `bin/uji` prints to stdout; `bin/web` pushes to a broadcast channel keyed
    /// by that index so a reconnecting browser can ask for turns it missed.
    pub on_turn: OnTurn,
}

/// A per-turn progress sink. Boxed so the two binaries can plug in different
/// destinations (stdout, an SSE broadcast) behind the same engine.
///
/// Carries the session's full claim list as it stands after this turn, so a
/// browser can redraw the claim-status panel (§7) from the same event that
/// delivers the turn — the claims change on almost every turn (a new one born,
/// an old one attacked), so shipping them together avoids a second channel.
pub type OnTurn = Box<dyn Fn(usize, &Turn, &[Claim]) + Send + Sync>;

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
        let idx = s.transcript.len() - 1;
        (self.on_turn)(idx, s.transcript.get(idx).unwrap(), &s.claims);
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
            // shut until the whole debate is done AND the user opens it by hand —
            // an auto-summary at the top makes the transcript decorative. So the
            // debate loop stops here rather than running the synthesizer: the run
            // ends at Selesai with no synthesis turn, and `jalankan_sintesis`
            // produces one later, on-demand, when the user clicks.
            if s.phase == Phase::Sintesis {
                s.phase = Phase::Selesai;
                break;
            }

            // FactCheck runs on Claims, not turns, so it takes its own path
            // instead of jalankan_fase. Auxiliary (§4): a fact-checker that
            // fails must not sink a debate whose transcript is intact — its
            // errors are logged and the session moves on to Sintesis with
            // whatever verdicts got through.
            if s.phase == Phase::FactCheck {
                if let Err(e) = self.jalankan_fact_check(s).await {
                    eprintln!("fact-check gagal (sesi dilanjutkan): {e}");
                }
                simpan(s, &self.sessions_dir).await?;
                match phase::maju(s.phase, s.round, s.config_used.rounds) {
                    Some((p, r)) => { s.phase = p; s.round = r; }
                    None => break,
                }
                continue;
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
            let mut turn = self.satu_turn(s, fase, role, ronde, urutan, &view).await?;

            // Claim tracking (§3). Only debaters make claims; the moderator and
            // synthesizer are excluded. Extraction is auxiliary: a moderator that
            // fails to parse a turn must not sink a debate whose actual turn
            // succeeded, so its errors are logged and swallowed rather than
            // propagated (§4: a tracking miss is information lost, not an error).
            if let Some(side) = role.side() {
                match self.ekstrak_klaim(s, side, &turn.text).await {
                    Ok(ext) => turn.attacks = terapkan_ekstraksi(&mut s.claims, side, ronde, ext),
                    Err(e) => eprintln!("ekstraksi klaim gagal (turn dipertahankan): {e}"),
                }
            }

            let idx = s.transcript.len();
            (self.on_turn)(idx, &turn, &s.claims);
            s.transcript.push(turn);
        }
        Ok(())
    }

    /// Asks the moderator to read one debater turn against the running claim
    /// list and report what changed: new claims, opposing claims attacked, own
    /// claims abandoned (§3, §11).
    ///
    /// The moderator does this, not the debaters: §10 forbids asking a model to
    /// number its own claims when a third party can do it more accurately.
    async fn ekstrak_klaim(
        &self,
        s: &Session,
        side: Side,
        teks: &str,
    ) -> Result<Ekstraksi, EngineError> {
        let daftar = daftar_klaim(&s.claims);
        let prompt = isi(
            &self.prompts.ekstraksi,
            &[
                ("klaim", s.claim.as_deref().unwrap_or(&s.topic)),
                ("sisi", side.label()),
                ("teks", teks),
                ("daftar_klaim", &daftar),
            ],
        );

        let c = self
            .client
            .kirim(
                &self.cfg.models.moderator,
                &[Message::system(prompt), Message::user("Lacak klaim turn ini.")],
                self.cfg.temperature,
                800,
            )
            .await?;

        Ok(parse_ekstraksi(&c.text))
    }

    /// Fact-check pass over every Claim in the session (Tahap 4).
    ///
    /// One request per claim: classify faktual/opini, and for faktual claims
    /// score Terdukung/Diragukan/TidakBisaVerifikasi with a short note. Per
    /// §1 the result is a "check this yourself" flag, never a winner signal —
    /// no aggregate per side, no total score, no comparison across claims. The
    /// prompt itself repeats this rule.
    ///
    /// Per-claim failures are swallowed and logged: the claim stays
    /// `BelumDiklasifikasi` with `fact_check = None`, which the UI shows as
    /// "belum diperiksa" rather than "diragukan by accident".
    ///
    /// The claim broadcast fires after each claim so the browser panel updates
    /// live as verdicts land, matching the per-turn cadence of ekstraksi (§7).
    pub async fn jalankan_fact_check(&self, s: &mut Session) -> Result<(), EngineError> {
        let model = s.config_used.models.fact_checker().clone();
        let klaim_sidang = s.claim.clone().unwrap_or_else(|| s.topic.clone());

        for i in 0..s.claims.len() {
            let (id, teks, owner, born) = {
                let c = &s.claims[i];
                (c.id.clone(), c.text.clone(), c.owner, c.born_round)
            };

            let prompt = isi(
                &self.prompts.fact_check,
                &[
                    ("klaim_sidang", &klaim_sidang),
                    ("sisi", owner.label()),
                    ("ronde", &born.to_string()),
                    ("teks_klaim", &teks),
                ],
            );

            let resp = self
                .client
                .kirim(
                    &model,
                    &[Message::system(prompt), Message::user("Periksa klaim ini.")],
                    self.cfg.temperature,
                    600,
                )
                .await;

            match resp {
                Ok(c) => {
                    let hasil = parse_fact_check(&c.text);
                    terapkan_fact_check(&mut s.claims[i], hasil);
                }
                Err(e) => {
                    // Per-claim failure is auxiliary. Leave the claim
                    // unclassified — a wrong verdict is more misleading than
                    // no verdict, and §1 forbids fabricating one.
                    eprintln!("fact-check gagal untuk {id} (dilewati): {e}");
                }
            }

            // A fact-check verdict does not create a Turn, so there is no
            // per-turn broadcast to piggy-back on. Reuse on_turn to push the
            // current claim list; the browser panel updates on each verdict
            // rather than in one lump at the end (matches §7's per-turn rhythm).
            if let Some(last) = s.transcript.last() {
                let idx = s.transcript.len() - 1;
                (self.on_turn)(idx, last, &s.claims);
            }
        }

        Ok(())
    }

    /// Runs the synthesizer over the finished transcript and appends its turn.
    ///
    /// Deliberately NOT part of `jalankan`: §1 requires the synthesis to stay
    /// shut until the user opens it, so the debate loop stops at Selesai without
    /// one, and this is called on-demand instead (a web click, or `uji sintesis`).
    ///
    /// The synthesizer produces a "map of arguments", never a winner — that rule
    /// lives entirely in `prompts/synthesizer.md`; this method adds nothing to it.
    /// It is not a debater, so it clears no bar and takes no retry (like the
    /// moderator path in `satu_turn`): its output is read, not judged.
    ///
    /// Idempotent: a session that already has a synthesis turn returns that turn's
    /// index unchanged, so a double click never appends a second one.
    pub async fn jalankan_sintesis(&self, s: &mut Session) -> Result<usize, EngineError> {
        if let Some(i) = s
            .transcript
            .all()
            .iter()
            .position(|t| t.phase == Phase::Sintesis)
        {
            return Ok(i);
        }

        let klaim = s.claim.as_deref().unwrap_or(&s.topic);

        // The synthesizer sees the whole transcript — build_view returns exactly
        // that for (Sintesis, Synthesizer), so the "who sees what" rule stays in
        // one place (§4) rather than being re-decided here.
        let view = build_view(&s.transcript, Phase::Sintesis, Role::Synthesizer, 1);
        let prompt = isi(&self.prompts.synthesizer, &[("klaim", klaim)]);
        let pesan = vec![
            Message::system(prompt),
            Message::user(rangkai_view(&view, klaim)),
        ];

        // Five sections over the full transcript: a higher floor than a single
        // debate turn, same generous ceiling reasoning as satu_turn (§8 note there).
        let max_tokens = (self.cfg.word_limit * 24).max(4000);

        let c = self
            .client
            .kirim(&self.cfg.models.synthesizer, &pesan, self.cfg.temperature, max_tokens)
            .await?;

        let turn = bikin_turn(
            1,
            Phase::Sintesis,
            Role::Synthesizer,
            &self.cfg.models.synthesizer,
            &c,
            0,
            0,
            Vec::new(),
        );

        let idx = s.transcript.len();
        s.transcript.push(turn);
        (self.on_turn)(idx, s.transcript.get(idx).unwrap(), &s.claims);
        simpan(s, &self.sessions_dir).await?;
        Ok(idx)
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
        // Filled by the extraction pass in `jalankan_fase` after this returns;
        // moderator and synthesizer turns keep it empty, as they make no claims.
        attacks: Vec::new(),
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

/// What the moderator's extraction pass reports about one turn (§3).
#[derive(Debug, Default, PartialEq)]
struct Ekstraksi {
    /// Newly stated claims, in the moderator's words. The engine assigns ids.
    baru: Vec<String>,
    /// Ids of opposing claims this turn attacked.
    menyerang: Vec<String>,
    /// Ids of this side's own claims it withdrew.
    ditinggalkan: Vec<String>,
}

/// Renders the current claim list for the extraction prompt. Empty list reads
/// as an explicit "(belum ada klaim)" so the moderator is never handed a blank.
fn daftar_klaim(claims: &[Claim]) -> String {
    if claims.is_empty() {
        return "(belum ada klaim)".to_string();
    }
    claims
        .iter()
        .map(|c| format!("{} [{}] ({}): {}", c.id, c.owner.label(), claim_status_pendek(c.status), c.text))
        .collect::<Vec<_>>()
        .join("\n")
}

fn claim_status_pendek(s: ClaimStatus) -> &'static str {
    match s {
        ClaimStatus::Hidup => "belum dijawab",
        ClaimStatus::Terbantah => "terbantah",
        ClaimStatus::Diabaikan => "ditinggalkan",
    }
}

/// Applies one extraction to the running claim list, in place, and returns the
/// ids of opposing claims this turn attacked (for the turn's `attacks` field).
///
/// New claims are born `Hidup` — stated, not yet answered, which §3 calls the
/// most interesting status in the system. An attack flips an *opponent's* claim
/// to `Terbantah`; an abandonment flips one of *your own* to `Diabaikan`. Both
/// checks verify ownership so a mis-addressed id from the moderator cannot mark
/// the wrong side's claim. Ids are assigned here, never by the debaters (§10).
fn terapkan_ekstraksi(
    claims: &mut Vec<Claim>,
    side: Side,
    ronde: u32,
    ext: Ekstraksi,
) -> Vec<String> {
    for teks in ext.baru {
        let teks = bersihkan(&teks);
        if teks.is_empty() {
            continue;
        }
        let id = format!("K{}", claims.len() + 1);
        claims.push(Claim::baru(id, side, teks, ronde));
    }

    // An attack only counts against an opponent's claim; record the ones that
    // resolve to a real, opposing id so the chip never points at a claim that
    // does not exist or at the attacker's own side.
    let mut menyerang_valid = Vec::new();
    for id in ext.menyerang {
        if let Some(c) = claims.iter_mut().find(|c| c.id == id && c.owner == side.lawan()) {
            c.status = ClaimStatus::Terbantah;
            menyerang_valid.push(c.id.clone());
        }
    }

    for id in ext.ditinggalkan {
        if let Some(c) = claims.iter_mut().find(|c| c.id == id && c.owner == side) {
            c.status = ClaimStatus::Diabaikan;
        }
    }

    menyerang_valid
}

/// Pulls the four-line extraction format out of the moderator's reply.
///
/// Tolerant like [`parse_framing`]: an off-format line drops out rather than
/// aborting the tracking for a whole turn. A "-" placeholder means the section
/// is empty. Claim-numbering that the moderator adds despite being told not to
/// (a leading "1." or "K3:") is stripped, since the engine owns the ids.
fn parse_ekstraksi(text: &str) -> Ekstraksi {
    let mut ext = Ekstraksi::default();
    let mut di_baru = false;

    for baris in text.lines() {
        let b = baris.trim();
        if b.is_empty() {
            continue;
        }

        // Split on the label's own colon so the tail is sliced from the original
        // line, never from an uppercased copy — uppercasing can change byte
        // length (ß → SS) and shift the offset, which would corrupt claim text.
        let label = b.split(':').next().unwrap_or("").trim().to_uppercase();
        let ekor = b.split_once(':').map(|(_, e)| e.trim()).unwrap_or("");

        match label.as_str() {
            "BARU" => {
                di_baru = true;
                // Allow "BARU: teks" on one line as well as a bulleted list below.
                if !ekor.is_empty() && ekor != "-" {
                    ext.baru.push(bersihkan_klaim(ekor));
                }
            }
            "MENYERANG" => {
                di_baru = false;
                ext.menyerang = pisah_id(ekor);
            }
            "DITINGGALKAN" => {
                di_baru = false;
                ext.ditinggalkan = pisah_id(ekor);
            }
            _ if di_baru => {
                // A bullet under BARU. Strip the marker; skip an explicit empty.
                let isi = b.trim_start_matches(['-', '*', '•']).trim();
                if !isi.is_empty() && isi != "-" {
                    ext.baru.push(bersihkan_klaim(isi));
                }
            }
            _ => {}
        }
    }
    ext
}

/// Strips a leading "K3:", "3.", or "3)" the moderator may have added to a new
/// claim despite being told not to number — the engine owns claim ids.
fn bersihkan_klaim(s: &str) -> String {
    let s = s.trim();
    let tanpa_nomor = s
        .split_once([':', '.', ')'])
        .map(|(kepala, ekor)| {
            let k = kepala.trim();
            let berupa_id = k.len() <= 4
                && k.chars().next().map(|c| c == 'K' || c == 'k').unwrap_or(false)
                && k[1..].chars().all(|c| c.is_ascii_digit());
            let berupa_angka = !k.is_empty() && k.chars().all(|c| c.is_ascii_digit());
            if berupa_id || berupa_angka { ekor.trim() } else { s }
        })
        .unwrap_or(s);
    bersihkan(tanpa_nomor)
}

/// Parses "K2, K5" (or "-") into a list of ids, uppercased to match stored ids.
fn pisah_id(s: &str) -> Vec<String> {
    let s = s.trim();
    if s.is_empty() || s == "-" {
        return Vec::new();
    }
    s.split(&[',', ' '][..])
        .map(|t| t.trim().trim_matches(|c: char| !c.is_alphanumeric()).to_uppercase())
        .filter(|t| {
            t.len() >= 2 && t.starts_with('K') && t[1..].chars().all(|c| c.is_ascii_digit())
        })
        .collect()
}

/// What the fact-checker reported for one claim: a required kind, and a verdict
/// that is only meaningful when kind is Faktual. Notes are always kept — for an
/// Opini they explain why the claim isn't fact-checkable, which is useful to the
/// reader.
#[derive(Debug, Default, PartialEq)]
struct FactCheckHasil {
    kind: ClaimKind,
    verdict: Option<FactVerdict>,
    catatan: String,
}

/// Parses the 3-line JENIS/VERDICT/CATATAN reply. Tolerant on purpose (§4): an
/// unrecognised line leaves the claim unclassified rather than aborting a
/// session's whole fact-check pass. Labels are matched case-insensitively; the
/// catatan tail is sliced from the original line to preserve casing and any
/// non-ASCII characters intact.
fn parse_fact_check(text: &str) -> FactCheckHasil {
    let mut hasil = FactCheckHasil::default();

    for baris in text.lines() {
        let b = baris.trim();
        if b.is_empty() {
            continue;
        }
        let (label, ekor) = match b.split_once(':') {
            Some((l, e)) => (l.trim().to_uppercase(), e.trim()),
            None => continue,
        };

        match label.as_str() {
            "JENIS" => hasil.kind = parse_jenis(ekor),
            "VERDICT" => hasil.verdict = parse_verdict(ekor),
            "CATATAN" => hasil.catatan = bersihkan(ekor),
            _ => {}
        }
    }

    // A JENIS=Opini with a leftover VERDICT is nonsense — the prompt told the
    // model to write "-" there. Drop the verdict so it does not surface later.
    if matches!(hasil.kind, ClaimKind::Opini) {
        hasil.verdict = None;
    }
    hasil
}

fn parse_jenis(s: &str) -> ClaimKind {
    match s.trim().to_lowercase().as_str() {
        "faktual" => ClaimKind::Faktual,
        "opini" => ClaimKind::Opini,
        _ => ClaimKind::BelumDiklasifikasi,
    }
}

fn parse_verdict(s: &str) -> Option<FactVerdict> {
    // Only exact matches. A partial or misspelled verdict becomes None, which
    // reads in the UI as "belum diperiksa" — the same as no fact-check at all,
    // which is the honest report when the reply was unparsable.
    match s.trim().to_lowercase().as_str() {
        "terdukung" => Some(FactVerdict::Terdukung),
        "diragukan" => Some(FactVerdict::Diragukan),
        // Accept the hyphenated form the prompt uses and the more natural spaces.
        "tidak-bisa-diverifikasi" | "tidak bisa diverifikasi" => {
            Some(FactVerdict::TidakBisaVerifikasi)
        }
        _ => None,
    }
}

/// Writes one parsed result to a claim, in place. Split from `parse_fact_check`
/// so the parsing has a pure test target and the write can be unit-tested
/// without a Session.
fn terapkan_fact_check(claim: &mut Claim, hasil: FactCheckHasil) {
    claim.kind = hasil.kind;
    claim.fact_check = match (hasil.kind, hasil.verdict) {
        (ClaimKind::Faktual, Some(verdict)) => Some(FactCheck {
            verdict,
            catatan: hasil.catatan,
        }),
        // Faktual with no verdict, or Opini with any verdict, or unclassified:
        // leave fact_check as None so the UI shows "belum diperiksa" and the
        // reader is not handed a verdict that was never actually computed.
        _ => None,
    };
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

    #[test]
    fn parse_ekstraksi_lengkap() {
        let teks = "BARU:\n\
                    - Rust mencegah data race saat kompilasi\n\
                    - Ekosistem async Rust sudah matang\n\
                    MENYERANG: K2, K5\n\
                    DITINGGALKAN: -\n";
        let e = parse_ekstraksi(teks);
        assert_eq!(e.baru.len(), 2);
        assert!(e.baru[0].contains("data race"));
        assert_eq!(e.menyerang, vec!["K2", "K5"]);
        assert!(e.ditinggalkan.is_empty());
    }

    #[test]
    fn parse_ekstraksi_tanpa_klaim_baru() {
        // A turn that only attacks: BARU empty, MENYERANG present.
        let e = parse_ekstraksi("BARU: -\nMENYERANG: K1\nDITINGGALKAN: -");
        assert!(e.baru.is_empty());
        assert_eq!(e.menyerang, vec!["K1"]);
    }

    #[test]
    fn parse_ekstraksi_membuang_penomoran_moderator() {
        // The moderator numbered despite being told not to; the engine owns ids.
        let e = parse_ekstraksi("BARU:\n- K7: Go lebih produktif\n- 2. Kompilasi Go cepat\nMENYERANG: -\nDITINGGALKAN: -");
        assert_eq!(e.baru.len(), 2);
        assert_eq!(e.baru[0], "Go lebih produktif");
        assert_eq!(e.baru[1], "Kompilasi Go cepat");
    }

    #[test]
    fn parse_ekstraksi_id_bukan_klaim_diabaikan() {
        // Junk tokens in an id line are dropped; only K-ids survive.
        let e = parse_ekstraksi("BARU: -\nMENYERANG: tidak ada\nDITINGGALKAN: -");
        assert!(e.menyerang.is_empty());
    }

    #[test]
    fn terapkan_ekstraksi_lahir_hidup_dan_serang_lawan() {
        let mut claims = vec![Claim::baru("K1".into(), Side::Pro, "klaim pro".into(), 1)];
        // Kontra speaks: adds one claim, attacks Pro's K1.
        let ext = Ekstraksi {
            baru: vec!["klaim kontra baru".into()],
            menyerang: vec!["K1".into()],
            ditinggalkan: vec![],
        };
        let att = terapkan_ekstraksi(&mut claims, Side::Kontra, 2, ext);

        assert_eq!(att, vec!["K1"], "chip menandai serangan valid");
        assert_eq!(claims.len(), 2);
        assert_eq!(claims[0].status, ClaimStatus::Terbantah, "K1 milik lawan → terbantah");
        assert_eq!(claims[1].id, "K2");
        assert_eq!(claims[1].owner, Side::Kontra);
        assert_eq!(claims[1].status, ClaimStatus::Hidup, "klaim baru lahir hidup");
        assert_eq!(claims[1].born_round, 2);
    }

    #[test]
    fn terapkan_ekstraksi_tak_bisa_serang_klaim_sendiri() {
        // A mis-addressed id pointing at your OWN claim must not flip it — an
        // attack only counts against the opponent (§3).
        let mut claims = vec![Claim::baru("K1".into(), Side::Pro, "klaim pro".into(), 1)];
        let ext = Ekstraksi {
            baru: vec![],
            menyerang: vec!["K1".into()],
            ditinggalkan: vec![],
        };
        // Pro tries to "attack" its own K1.
        let att = terapkan_ekstraksi(&mut claims, Side::Pro, 1, ext);
        assert!(att.is_empty());
        assert_eq!(claims[0].status, ClaimStatus::Hidup, "klaim sendiri tetap hidup");
    }

    #[test]
    fn terapkan_ekstraksi_tinggalkan_klaim_sendiri() {
        let mut claims = vec![Claim::baru("K1".into(), Side::Pro, "klaim pro".into(), 1)];
        let ext = Ekstraksi {
            baru: vec![],
            menyerang: vec![],
            ditinggalkan: vec!["K1".into()],
        };
        terapkan_ekstraksi(&mut claims, Side::Pro, 2, ext);
        assert_eq!(claims[0].status, ClaimStatus::Diabaikan);
    }

    // ─── fact-check parsing ──────────────────────────────────────────────

    #[test]
    fn parse_fact_check_faktual_terdukung() {
        let teks = "JENIS: faktual\nVERDICT: terdukung\nCATATAN: sejalan dengan data BLS 2024.";
        let h = parse_fact_check(teks);
        assert_eq!(h.kind, ClaimKind::Faktual);
        assert_eq!(h.verdict, Some(FactVerdict::Terdukung));
        assert!(h.catatan.contains("BLS 2024"));
    }

    #[test]
    fn parse_fact_check_opini_membuang_verdict() {
        // The prompt says "-" for Opini, but a model may echo a stray word.
        // parse_fact_check must not surface a verdict on an Opini claim, since
        // that would let a value judgement quietly acquire a fact-check status.
        let h = parse_fact_check("JENIS: opini\nVERDICT: terdukung\nCATATAN: penilaian nilai.");
        assert_eq!(h.kind, ClaimKind::Opini);
        assert_eq!(h.verdict, None);
    }

    #[test]
    fn parse_fact_check_verdict_takdikenal_jadi_none() {
        let h = parse_fact_check("JENIS: faktual\nVERDICT: mungkin\nCATATAN: ragu.");
        assert_eq!(h.kind, ClaimKind::Faktual);
        assert_eq!(h.verdict, None, "verdict tak dikenal jangan ditebak");
    }

    #[test]
    fn parse_fact_check_toleran_spasi_dan_kapital() {
        let h = parse_fact_check("  jenis  :  Faktual  \nVERDICT: TIDAK BISA DIVERIFIKASI\nCATATAN: terlalu baru.");
        assert_eq!(h.kind, ClaimKind::Faktual);
        assert_eq!(h.verdict, Some(FactVerdict::TidakBisaVerifikasi));
    }

    #[test]
    fn terapkan_fact_check_faktual_menulis_verdict() {
        let mut c = Claim::baru("K1".into(), Side::Pro, "x".into(), 1);
        terapkan_fact_check(&mut c, FactCheckHasil {
            kind: ClaimKind::Faktual,
            verdict: Some(FactVerdict::Diragukan),
            catatan: "angka usang".into(),
        });
        assert_eq!(c.kind, ClaimKind::Faktual);
        let fc = c.fact_check.as_ref().unwrap();
        assert_eq!(fc.verdict, FactVerdict::Diragukan);
        assert!(fc.catatan.contains("usang"));
    }

    #[test]
    fn terapkan_fact_check_opini_tak_menyimpan_verdict() {
        // Opini must never carry a fact_check — that would put a factual
        // stamp on a value judgement, which §1 forbids.
        let mut c = Claim::baru("K1".into(), Side::Pro, "x".into(), 1);
        terapkan_fact_check(&mut c, FactCheckHasil {
            kind: ClaimKind::Opini,
            verdict: None,
            catatan: "penilaian nilai".into(),
        });
        assert_eq!(c.kind, ClaimKind::Opini);
        assert!(c.fact_check.is_none());
    }

    #[test]
    fn terapkan_fact_check_faktual_tanpa_verdict_tetap_kosong() {
        // Verdict unparsable but jenis faktual — leave fact_check None rather
        // than default to something. A wrong verdict is more misleading than
        // no verdict.
        let mut c = Claim::baru("K1".into(), Side::Pro, "x".into(), 1);
        terapkan_fact_check(&mut c, FactCheckHasil {
            kind: ClaimKind::Faktual,
            verdict: None,
            catatan: String::new(),
        });
        assert_eq!(c.kind, ClaimKind::Faktual);
        assert!(c.fact_check.is_none());
    }
}
