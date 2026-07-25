//! The view builder: what each participant is allowed to see, per phase.
//!
//! INVARIANT — do not write a `_` catch-all arm anywhere in this file.
//! Exhaustive `match` is the entire mechanism: adding a phase or a role must
//! break the build until its view is decided explicitly. A wildcard silently
//! grants some future phase whatever the neighbouring arm happened to return,
//! and the resulting bug does not crash — it just makes the debate soft with no
//! visible cause (§4). Group cases with `|`, never with `_`.

use crate::transcript::{Transcript, Turn};

/// The two debating sides. Deliberately separate from [`Role`] so turn-order
/// logic cannot be handed a moderator by accident.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Side {
    Pro,
    Kontra,
}

impl Side {
    pub const ALL: [Side; 2] = [Side::Pro, Side::Kontra];

    /// The other side. Used by view rules that say "sees the opponent's turns".
    pub fn lawan(self) -> Side {
        match self {
            Side::Pro => Side::Kontra,
            Side::Kontra => Side::Pro,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Side::Pro => "Pro",
            Side::Kontra => "Kontra",
        }
    }
}

impl From<Side> for Role {
    fn from(s: Side) -> Role {
        match s {
            Side::Pro => Role::Debater(Side::Pro),
            Side::Kontra => Role::Debater(Side::Kontra),
        }
    }
}

/// Who is speaking. `Debater` carries its side, so a `Role` can never be a
/// debater without also saying which one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Role {
    Debater(Side),
    Moderator,
    Synthesizer,
}

impl Role {
    pub const ALL: [Role; 4] = [
        Role::Debater(Side::Pro),
        Role::Debater(Side::Kontra),
        Role::Moderator,
        Role::Synthesizer,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Role::Debater(s) => s.label(),
            Role::Moderator => "Moderator",
            Role::Synthesizer => "Sintesis",
        }
    }
}

/// The phases, in order (§4).
///
/// `CrossExam` is split into ask and answer because a single combined phase
/// cannot express "the question is visible but the opponent's reasoning is not"
/// — and §4's table leaves one cell blank, which has to resolve to something.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub enum Phase {
    Framing,
    Opening,
    Rebuttal,
    CrossExamAsk,
    CrossExamAnswer,
    Crux,
    Sintesis,
    Selesai,
}

impl Phase {
    pub const ALL: [Phase; 8] = [
        Phase::Framing,
        Phase::Opening,
        Phase::Rebuttal,
        Phase::CrossExamAsk,
        Phase::CrossExamAnswer,
        Phase::Crux,
        Phase::Sintesis,
        Phase::Selesai,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Phase::Framing => "Framing",
            Phase::Opening => "Opening",
            Phase::Rebuttal => "Rebuttal",
            Phase::CrossExamAsk => "Cross-exam: bertanya",
            Phase::CrossExamAnswer => "Cross-exam: menjawab",
            Phase::Crux => "Crux",
            Phase::Sintesis => "Sintesis",
            Phase::Selesai => "Selesai",
        }
    }

    /// Phases that repeat once per round. The rest happen once per session.
    pub fn per_ronde(self) -> bool {
        match self {
            Phase::Rebuttal | Phase::CrossExamAsk | Phase::CrossExamAnswer => true,
            Phase::Framing
            | Phase::Opening
            | Phase::Crux
            | Phase::Sintesis
            | Phase::Selesai => false,
        }
    }

    pub fn berikutnya(self) -> Option<Phase> {
        match self {
            Phase::Framing => Some(Phase::Opening),
            Phase::Opening => Some(Phase::Rebuttal),
            Phase::Rebuttal => Some(Phase::CrossExamAsk),
            Phase::CrossExamAsk => Some(Phase::CrossExamAnswer),
            Phase::CrossExamAnswer => Some(Phase::Crux),
            Phase::Crux => Some(Phase::Sintesis),
            Phase::Sintesis => Some(Phase::Selesai),
            Phase::Selesai => None,
        }
    }
}

/// The list constants above are hand-written, so a new variant would compile
/// while leaving them stale. These force that mistake to fail the build instead
/// (E0080 at compile time, not a wrong answer at runtime).
const _: () = {
    assert!(Phase::ALL.len() == 8, "Phase::ALL is out of sync with Phase");
    assert!(Role::ALL.len() == 4, "Role::ALL is out of sync with Role");
    assert!(Side::ALL.len() == 2, "Side::ALL is out of sync with Side");
};

/// What a participant receives as context.
///
/// `Silent` and `Blind` are distinct on purpose. `Silent` means this role does
/// not act in this phase at all; `Blind` means it acts, with no context. Folding
/// them together would let a scheduling bug fire a request for a role that was
/// supposed to stay quiet, and nothing downstream would notice.
#[derive(Debug, Clone, PartialEq)]
pub enum View<'t> {
    /// Does not act in this phase.
    Silent,
    /// Acts, sees nothing. Opening is blind so the two openings are independent
    /// rather than shaped by the opponent's framing (§4).
    Blind,
    /// Acts, sees exactly these turns, in transcript order.
    Turns(Vec<&'t Turn>),
}

impl<'t> View<'t> {
    pub fn acts(&self) -> bool {
        match self {
            View::Silent => false,
            View::Blind | View::Turns(_) => true,
        }
    }

    pub fn turns(&self) -> &[&'t Turn] {
        match self {
            View::Silent | View::Blind => &[],
            View::Turns(v) => v,
        }
    }
}

/// Builds the context for `role` in `phase` at `round`.
///
/// This is the one function §4 calls the heart of the system: "if the view is
/// wrong, no prompt however good will help; if the view is right, even a simple
/// prompt works."
///
/// `round` is 1-based and only meaningful for phases where [`Phase::per_ronde`]
/// is true.
pub fn build_view<'t>(t: &'t Transcript, phase: Phase, role: Role, round: u32) -> View<'t> {
    match (phase, role) {
        // --- Framing: the moderator proposes claim wordings; nobody else exists yet.
        (Phase::Framing, Role::Moderator) => View::Turns(t.all()),
        (Phase::Framing, Role::Debater(_) | Role::Synthesizer) => View::Silent,

        // --- Opening: both debaters blind, so neither anchors to the other (§4).
        //     These two requests may run in parallel.
        (Phase::Opening, Role::Debater(_)) => View::Blind,
        (Phase::Opening, Role::Moderator | Role::Synthesizer) => View::Silent,

        // --- Rebuttal: each side sees everything the opponent has said, plus its
        //     own previous turns so it can avoid repeating itself. §4's table
        //     shows Kontra seeing "opening Pro + rebuttal Pro" — that asymmetry
        //     is a consequence of speaking order, not a separate rule, so it is
        //     expressed as "everything said before my turn".
        (Phase::Rebuttal, Role::Debater(side)) => View::Turns(t.sebelum_giliran(side, phase, round)),
        (Phase::Rebuttal, Role::Moderator | Role::Synthesizer) => View::Silent,

        // --- Cross-exam, ask: writing a question needs the opponent's case.
        (Phase::CrossExamAsk, Role::Debater(side)) => {
            View::Turns(t.turns_of_side(side.lawan()))
        }
        (Phase::CrossExamAsk, Role::Moderator | Role::Synthesizer) => View::Silent,

        // --- Cross-exam, answer: the question ONLY. §4 says "Pro sees Kontra's
        //     question only" — withholding the reasoning behind it is the point,
        //     since the answer should address the question, not pre-empt the
        //     argument around it.
        (Phase::CrossExamAnswer, Role::Debater(side)) => {
            View::Turns(t.pertanyaan_untuk(side, round))
        }
        (Phase::CrossExamAnswer, Role::Moderator | Role::Synthesizer) => View::Silent,

        // --- Crux: both sides see the full transcript to name the real
        //     disagreement.
        (Phase::Crux, Role::Debater(_)) => View::Turns(t.all()),
        (Phase::Crux, Role::Moderator | Role::Synthesizer) => View::Silent,

        // --- Sintesis: the synthesizer reads everything. The debaters are done.
        (Phase::Sintesis, Role::Synthesizer) => View::Turns(t.all()),
        (Phase::Sintesis, Role::Debater(_) | Role::Moderator) => View::Silent,

        // --- Selesai: nobody acts. Anything that still wants a turn here is a bug.
        (Phase::Selesai, Role::Debater(_) | Role::Moderator | Role::Synthesizer) => View::Silent,
    }
}

/// Which side speaks first in a given round.
///
/// Speaking second is an advantage — you have seen more (§4) — so the order
/// alternates. Round 1 opens with Pro.
///
/// Callers must persist the result on the [`Turn`] rather than recomputing it
/// when reading old sessions back: the rule may change, and a stored session
/// re-read under a new rule would silently misreport who saw what.
pub fn urutan_ronde(round: u32) -> [Side; 2] {
    if round % 2 == 1 {
        [Side::Pro, Side::Kontra]
    } else {
        [Side::Kontra, Side::Pro]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opening_buta_kedua_sisi() {
        let t = Transcript::default();
        for side in Side::ALL {
            assert_eq!(build_view(&t, Phase::Opening, side.into(), 1), View::Blind);
        }
    }

    #[test]
    fn moderator_diam_saat_debat() {
        let t = Transcript::default();
        for phase in [Phase::Opening, Phase::Rebuttal, Phase::Crux] {
            assert_eq!(build_view(&t, phase, Role::Moderator, 1), View::Silent);
        }
    }

    #[test]
    fn tak_ada_yang_bicara_saat_selesai() {
        let t = Transcript::default();
        for role in Role::ALL {
            assert!(!build_view(&t, Phase::Selesai, role, 1).acts());
        }
    }

    #[test]
    fn urutan_ditukar_tiap_ronde() {
        assert_eq!(urutan_ronde(1), [Side::Pro, Side::Kontra]);
        assert_eq!(urutan_ronde(2), [Side::Kontra, Side::Pro]);
        assert_eq!(urutan_ronde(3), [Side::Pro, Side::Kontra]);
    }

    /// Every (phase, role) pair must be reachable without panicking. Cheap, but
    /// it catches an indexing mistake in a transcript helper for a combination
    /// no debate has happened to exercise yet.
    #[test]
    fn semua_kombinasi_fase_role_terdefinisi() {
        let t = Transcript::default();
        for phase in Phase::ALL {
            for role in Role::ALL {
                let _ = build_view(&t, phase, role, 1);
            }
        }
    }
}
