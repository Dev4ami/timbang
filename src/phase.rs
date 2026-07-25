//! Phase rules: who speaks, and whether what they said counts.
//!
//! This is where "ask again, that was weak" lives. `llm` retries networks; this
//! module retries arguments, and §5 requires the two never share a code path —
//! a network hiccup and a limp rebuttal need opposite responses, and merging
//! them produces a system that retries the wrong one.

use crate::config::SessionConfig;
use crate::transcript::{Transcript, TurnFlag};
use crate::view::{Phase, Role, Side, urutan_ronde};

/// The verdict on a turn that has just come back from the model.
#[derive(Debug, Clone, PartialEq)]
pub enum Putusan {
    Lolos,
    /// Missed the bar on the first try. §4 allows exactly one nudge-and-retry.
    UlangSekali { alasan: Vec<TurnFlag>, teguran: String },
    /// Missed it again. The turn is kept and marked — a side that stopped
    /// engaging is a finding, not an error (§4).
    Catat { flags: Vec<TurnFlag> },
}

/// Checks a candidate turn against its phase's bar.
///
/// `percobaan` is 1 for the first attempt, 2 after a nudge.
pub fn nilai_turn(
    t: &Transcript,
    cfg: &SessionConfig,
    phase: Phase,
    side: Side,
    round: u32,
    teks: &str,
    finish_reason: Option<&str>,
    percobaan: u32,
    teguran_template: &str,
) -> Putusan {
    let mut flags = Vec::new();

    // Truncation is recorded everywhere, but never on its own grounds for a
    // retry: asking again with the same ceiling would just truncate again.
    if finish_reason == Some("length") || finish_reason == Some("max_tokens") {
        flags.push(TurnFlag::Terpotong);
    }

    // Only rebuttals have to engage. Openings are blind by design, cross-exam
    // answers respond to a question rather than a claim, and crux turns are
    // supposed to step back from the fight.
    let mut wajib_ulang = Vec::new();

    if phase == Phase::Rebuttal {
        if !menyebut_lawan(t, side, teks) {
            wajib_ulang.push(TurnFlag::GagalMembantah);
        }
        if let Some(sebelumnya) = t.turn_ronde_lalu(side, phase, round) {
            let sim = kemiripan(&sebelumnya.text, teks);
            if sim > cfg.similarity_threshold {
                wajib_ulang.push(TurnFlag::MengulangDiri { similarity: sim });
            }
        }
    }

    if wajib_ulang.is_empty() {
        return if flags.is_empty() {
            Putusan::Lolos
        } else {
            Putusan::Catat { flags }
        };
    }

    flags.extend(wajib_ulang.iter().cloned());

    if percobaan < 2 {
        Putusan::UlangSekali {
            teguran: crate::config::isi(
                teguran_template,
                &[("alasan", &jelaskan(&wajib_ulang))],
            ),
            alasan: wajib_ulang,
        }
    } else {
        Putusan::Catat { flags }
    }
}

fn jelaskan(flags: &[TurnFlag]) -> String {
    flags
        .iter()
        .map(|f| match f {
            TurnFlag::GagalMembantah => {
                "tidak menyerang satu pun argumen lawan secara langsung".to_string()
            }
            TurnFlag::MengulangDiri { similarity } => format!(
                "terlalu mirip dengan giliranmu di ronde sebelumnya ({:.0}% sama)",
                similarity * 100.0
            ),
            TurnFlag::Terpotong => "terpotong di tengah".to_string(),
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// Does this turn actually engage with something the opponent said?
///
/// Claim tracking arrives in Tahap 3; until then this is a content-word overlap
/// test against the opponent's most recent turn. It is a blunt instrument, and
/// tuned to be blunt in the forgiving direction: a false "failed to rebut"
/// injects a nudge that makes a perfectly good debate worse, while a false pass
/// merely fails to catch one soft turn.
fn menyebut_lawan(t: &Transcript, side: Side, teks: &str) -> bool {
    let Some(lawan) = t.last_of_side(side.lawan()) else {
        // Nobody has spoken for the other side yet — nothing to engage with.
        return true;
    };
    let milik_lawan = kata_isi(&lawan.text);
    if milik_lawan.is_empty() {
        return true;
    }
    let milikku = kata_isi(teks);
    let bersama = milik_lawan.iter().filter(|w| milikku.contains(*w)).count();
    // Roughly a tenth of the opponent's distinctive vocabulary reappearing is
    // enough to say the turn is talking about the same thing.
    bersama * 10 >= milik_lawan.len()
}

/// Jaccard similarity over content-word bigrams.
///
/// Chosen over edit distance and over stemming-based measures because it needs
/// no language-specific resources — Indonesian affixation would defeat a naive
/// stemmer, and getting that wrong would silently mis-flag turns. Bigrams rather
/// than single words so that reusing the same vocabulary to make a *different*
/// argument does not read as repetition.
pub fn kemiripan(a: &str, b: &str) -> f32 {
    let ga = bigram(a);
    let gb = bigram(b);
    if ga.is_empty() || gb.is_empty() {
        return 0.0;
    }
    let irisan = ga.intersection(&gb).count() as f32;
    let gabungan = ga.union(&gb).count() as f32;
    irisan / gabungan
}

fn bigram(s: &str) -> std::collections::HashSet<(String, String)> {
    let kata = kata_isi_urut(s);
    kata.windows(2)
        .map(|w| (w[0].clone(), w[1].clone()))
        .collect()
}

/// Indonesian function words. Removing them keeps the overlap measures focused
/// on what a turn is about rather than on the fact that it is Indonesian.
const KATA_FUNGSI: &[&str] = &[
    "yang", "dan", "di", "ke", "dari", "untuk", "dengan", "pada", "adalah", "itu", "ini", "tidak",
    "akan", "juga", "atau", "bisa", "ada", "dalam", "sebagai", "karena", "oleh", "dapat", "lebih",
    "sudah", "saya", "kita", "mereka", "kamu", "anda", "bukan", "hanya", "telah", "harus", "jika",
    "kalau", "tapi", "tetapi", "namun", "justru", "malah", "bahwa", "para", "banyak", "sangat",
    "the", "a", "an", "is", "are", "to", "of", "and", "in", "for", "that", "it",
];

fn kata_isi_urut(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(|w| w.to_lowercase())
        .filter(|w| w.chars().count() > 3 && !KATA_FUNGSI.contains(&w.as_str()))
        .collect()
}

fn kata_isi(s: &str) -> std::collections::HashSet<String> {
    kata_isi_urut(s).into_iter().collect()
}

/// Who acts in this phase, in order.
///
/// The debater order alternates each round because speaking second means seeing
/// more (§4). Openings are listed in order too, though both may be issued in
/// parallel — neither can see the other, so the order carries no advantage.
pub fn giliran(phase: Phase, round: u32) -> Vec<Role> {
    match phase {
        Phase::Framing => vec![Role::Moderator],
        Phase::Opening => urutan_ronde(1).into_iter().map(Role::from).collect(),
        Phase::Rebuttal | Phase::CrossExamAsk | Phase::CrossExamAnswer => {
            urutan_ronde(round).into_iter().map(Role::from).collect()
        }
        Phase::Crux => urutan_ronde(round).into_iter().map(Role::from).collect(),
        Phase::Sintesis => vec![Role::Synthesizer],
        Phase::Selesai => vec![],
    }
}

/// Whether the two turns of this phase can be issued concurrently.
///
/// Only Opening: both sides are blind there, so neither request depends on the
/// other's result (§4). Everywhere else, running in parallel would mean the
/// second side does not see the first — which silently deletes the alternating
/// advantage the phase order exists to balance.
pub fn boleh_paralel(phase: Phase) -> bool {
    match phase {
        Phase::Opening => true,
        Phase::Framing
        | Phase::Rebuttal
        | Phase::CrossExamAsk
        | Phase::CrossExamAnswer
        | Phase::Crux
        | Phase::Sintesis
        | Phase::Selesai => false,
    }
}

/// The next (phase, round) after finishing this one.
///
/// Returns `None` when the debate is over.
pub fn maju(phase: Phase, round: u32, total_ronde: u32) -> Option<(Phase, u32)> {
    // The per-round phases cycle until the round budget is spent, then the
    // debate moves on to crux and synthesis.
    if phase == Phase::CrossExamAnswer {
        return if round < total_ronde {
            Some((Phase::Rebuttal, round + 1))
        } else {
            Some((Phase::Crux, round))
        };
    }
    phase.berikutnya().map(|p| (p, round))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcript::{Provenance, Turn};

    fn turn(side: Side, phase: Phase, round: u32, text: &str) -> Turn {
        Turn {
            round,
            phase,
            role: Role::Debater(side),
            model_requested: "uji/m".into(),
            model_answered: None,
            provenance: Provenance::Unverifiable { reason: "uji".into() },
            text: text.into(),
            at: "2026-01-01T00:00:00Z".into(),
            prompt_tokens: None,
            completion_tokens: None,
            finish_reason: None,
            retries: 0,
            speaking_order: 0,
            flags: vec![],
        }
    }

    #[test]
    fn teks_sama_persis_sangat_mirip() {
        let a = "Rust unggul karena keamanan memori tanpa garbage collector sama sekali";
        assert!(kemiripan(a, a) > 0.99);
    }

    #[test]
    fn argumen_berbeda_tidak_mirip() {
        let a = "Rust unggul karena keamanan memori tanpa garbage collector";
        let b = "Ekosistem Go jauh matang untuk kebutuhan server produksi";
        assert!(kemiripan(a, b) < 0.2, "kemiripan={}", kemiripan(a, b));
    }

    #[test]
    fn kosakata_sama_argumen_beda_tidak_dihitung_mengulang() {
        // Same subject matter, opposite point. Single-word overlap would call
        // this repetition; bigrams should not.
        let a = "keamanan memori Rust mencegah kebocoran pada server produksi besar";
        let b = "server produksi besar jarang gagal karena kebocoran memori sebenarnya";
        assert!(kemiripan(a, b) < 0.5, "kemiripan={}", kemiripan(a, b));
    }

    #[test]
    fn rebuttal_yang_tak_menyentuh_lawan_diulang_sekali() {
        let mut t = Transcript::default();
        t.push(turn(Side::Kontra, Phase::Opening, 1,
            "Go menang karena kompilasi cepat, goroutine sederhana, dan stdlib matang"));
        let cfg = SessionConfig::uji();

        let p = nilai_turn(&t, &cfg, Phase::Rebuttal, Side::Pro, 1,
            "Cuaca hari ini cerah sekali dan burung berkicau merdu di pepohonan",
            None, 1, "Ulangi: {alasan}");

        match p {
            Putusan::UlangSekali { alasan, .. } => {
                assert!(alasan.contains(&TurnFlag::GagalMembantah));
            }
            lain => panic!("harusnya UlangSekali, dapat {lain:?}"),
        }
    }

    #[test]
    fn gagal_kedua_kali_dicatat_bukan_error() {
        let mut t = Transcript::default();
        t.push(turn(Side::Kontra, Phase::Opening, 1,
            "Go menang karena kompilasi cepat, goroutine sederhana, dan stdlib matang"));
        let cfg = SessionConfig::uji();

        let p = nilai_turn(&t, &cfg, Phase::Rebuttal, Side::Pro, 1,
            "Cuaca hari ini cerah sekali dan burung berkicau merdu di pepohonan",
            None, 2, "Ulangi: {alasan}");

        assert!(matches!(p, Putusan::Catat { .. }), "dapat {p:?}");
    }

    #[test]
    fn rebuttal_yang_menyerang_lolos() {
        let mut t = Transcript::default();
        t.push(turn(Side::Kontra, Phase::Opening, 1,
            "Go menang karena kompilasi cepat, goroutine sederhana, dan stdlib matang"));
        let cfg = SessionConfig::uji();

        let p = nilai_turn(&t, &cfg, Phase::Rebuttal, Side::Pro, 1,
            "Klaim soal kompilasi cepat menyesatkan: goroutine menyembunyikan data race \
             yang stdlib Go sendiri tidak mencegah, dan itu mahal di produksi",
            None, 1, "Ulangi: {alasan}");

        assert_eq!(p, Putusan::Lolos, "dapat {p:?}");
    }

    #[test]
    fn opening_tidak_wajib_menyerang() {
        let mut t = Transcript::default();
        t.push(turn(Side::Kontra, Phase::Opening, 1, "Go menang karena kompilasi cepat"));
        let cfg = SessionConfig::uji();

        let p = nilai_turn(&t, &cfg, Phase::Opening, Side::Pro, 1,
            "Rust unggul karena keamanan memori terjamin saat kompilasi",
            None, 1, "Ulangi: {alasan}");

        assert_eq!(p, Putusan::Lolos);
    }

    #[test]
    fn terpotong_dicatat_tanpa_mengulang() {
        let t = Transcript::default();
        let cfg = SessionConfig::uji();
        let p = nilai_turn(&t, &cfg, Phase::Opening, Side::Pro, 1, "Argumen panjang yang",
            Some("max_tokens"), 1, "Ulangi: {alasan}");
        match p {
            Putusan::Catat { flags } => assert!(flags.contains(&TurnFlag::Terpotong)),
            lain => panic!("harusnya Catat, dapat {lain:?}"),
        }
    }

    #[test]
    fn ronde_berputar_lalu_ke_crux() {
        assert_eq!(maju(Phase::CrossExamAnswer, 1, 3), Some((Phase::Rebuttal, 2)));
        assert_eq!(maju(Phase::CrossExamAnswer, 3, 3), Some((Phase::Crux, 3)));
        assert_eq!(maju(Phase::Selesai, 1, 3), None);
    }

    #[test]
    fn hanya_opening_boleh_paralel() {
        assert!(boleh_paralel(Phase::Opening));
        for p in [Phase::Rebuttal, Phase::CrossExamAsk, Phase::Crux] {
            assert!(!boleh_paralel(p));
        }
    }
}
