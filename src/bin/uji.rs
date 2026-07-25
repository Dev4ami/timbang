//! `uji` — the permanent workbench (§2).
//!
//! Not a throwaway predecessor to the web UI. Every time a prompt changes, the
//! question is whether rebuttals went soft, and answering it needs text on a
//! terminal ten times in a row — not a browser, not streaming.
//!
//! Usage:
//!   uji framing "<topik>" [konteks]   propose claim wordings, then stop
//!   uji lanjut <id_sesi> <n>          approve wording n, run the debate
//!   uji jalan "<topik>"               framing + auto-pick option 1 + run
//!   uji lihat <id_sesi>               print a saved session as markdown
//!   uji nilai <id_sesi> <kepakai|setengah|tidak>
//!   uji model                         list the router catalogue
//!   uji sehat                         connection test

use std::path::{Path, PathBuf};

use timbang::config::{ApiKey, Config, Prompts};
use timbang::engine::Engine;
use timbang::llm::Client;
use timbang::render;
use timbang::transcript::{Penilaian, Session, SessionStatus, muat, simpan};

#[tokio::main]
async fn main() {
    let _ = dotenvy::dotenv();
    if let Err(e) = jalan().await {
        eprintln!("\n✗ {e}");
        // Errors arrive wrapped; the innermost line is the one that says what
        // actually went wrong.
        let mut src = e.source();
        while let Some(s) = src {
            eprintln!("  └ {s}");
            src = s.source();
        }
        std::process::exit(1);
    }
}

async fn jalan() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cfg_path = PathBuf::from("config.toml");
    let cfg = Config::muat(&cfg_path).await?;

    let perintah = args.first().map(|s| s.as_str()).unwrap_or("bantuan");

    match perintah {
        "sehat" => {
            let c = klien(&cfg)?;
            println!("router: {}", cfg.base_url);
            match c.health().await {
                Ok(true) => println!("✓ hidup"),
                Ok(false) => println!("✗ menjawab, tapi ok=false"),
                Err(e) => println!("✗ {e}"),
            }
        }

        "model" => {
            let c = klien(&cfg)?;
            let daftar = c.list_models().await?;
            println!("{} model:\n", daftar.len());
            for (id, owned) in &daftar {
                // §5 forbids combo models for Pro and Kontra: a chained provider
                // can answer with a different model on fallback, and the
                // transcript would name the wrong author.
                let tanda = if owned == "combo" { "  ⚠ combo — dilarang untuk Pro/Kontra" } else { "" };
                println!("  {id:<40} {owned}{tanda}");
            }
        }

        "framing" => {
            let topik = args.get(1).cloned().unwrap_or_default();
            if topik.is_empty() {
                anyhow::bail!("pakai: uji framing \"<topik>\" [konteks]");
            }
            let konteks = args.get(2).cloned();
            let eng = mesin(&cfg).await?;
            let mut s = Session::new(topik, konteks, cfg.untuk_sesi());

            println!("Sesi {}\nMeminta rumusan klaim dari moderator...\n", s.id);
            eng.framing(&mut s).await?;

            println!("\nPilihan rumusan:\n");
            for (i, o) in s.framing_options.iter().enumerate() {
                println!("  {}. {}", i + 1, o.text);
                if !o.bias.is_empty() {
                    println!("     bias: {}\n", o.bias);
                }
            }
            println!("Lanjut dengan:  uji lanjut {} <nomor>", s.id);
        }

        "lanjut" => {
            let id = args.get(1).cloned().unwrap_or_default();
            let n: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(0);
            if id.is_empty() || n == 0 {
                anyhow::bail!("pakai: uji lanjut <id_sesi> <nomor_rumusan>");
            }
            let path = cfg.sessions_dir.join(format!("{id}.json"));
            let mut s = muat(&path).await?;
            let pilihan = s
                .framing_options
                .get(n - 1)
                .ok_or_else(|| anyhow::anyhow!("nomor {n} tidak ada"))?
                .clone();
            s.claim = Some(pilihan.text.clone());
            println!("Klaim: {}\n", pilihan.text);

            let eng = mesin(&cfg).await?;
            eng.jalankan(&mut s).await?;
            selesai(&s, &cfg.sessions_dir);
        }

        "ulang" => {
            let id = args.get(1).cloned().unwrap_or_default();
            if id.is_empty() {
                anyhow::bail!("pakai: uji ulang <id_sesi>");
            }
            let path = cfg.sessions_dir.join(format!("{id}.json"));
            let mut s = muat(&path).await?;

            // Only stopped sessions. Re-running a finished one would append a
            // second copy of the later phases, and re-running a waiting one
            // would skip the framing approval §4 requires.
            match s.status {
                SessionStatus::Gagal { at_phase } => {
                    println!("Melanjutkan dari fase {} ronde {}", at_phase.label(), s.round);
                }
                SessionStatus::MenungguPersetujuan => {
                    anyhow::bail!("sesi ini menunggu persetujuan framing — pakai: uji lanjut {id} <nomor>")
                }
                SessionStatus::Selesai => anyhow::bail!("sesi ini sudah selesai"),
                SessionStatus::Berjalan => {
                    anyhow::bail!("sesi ini tercatat masih berjalan — pastikan tidak ada proses lain yang memakainya")
                }
            }

            println!("Klaim: {}\n", s.claim.as_deref().unwrap_or("(belum ada)"));
            let eng = mesin(&cfg).await?;
            eng.jalankan(&mut s).await?;
            selesai(&s, &cfg.sessions_dir);
        }

        "jalan" => {
            let topik = args.get(1).cloned().unwrap_or_default();
            if topik.is_empty() {
                anyhow::bail!("pakai: uji jalan \"<topik>\"");
            }
            let eng = mesin(&cfg).await?;
            let mut s = Session::new(topik, args.get(2).cloned(), cfg.untuk_sesi());

            println!("Sesi {}\n", s.id);
            eng.framing(&mut s).await?;

            // Auto-picking a wording skips the approval §4 requires, so this
            // path exists only for prompt iteration — where the same claim ten
            // times in a row is the point. Real sessions go through `framing`
            // and `lanjut`.
            let pilihan = s
                .framing_options
                .first()
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("moderator tidak memberi rumusan yang terbaca"))?;
            println!("\n⚠ memakai rumusan #1 tanpa persetujuan (mode uji prompt)");
            println!("Klaim: {}\n", pilihan.text);
            s.claim = Some(pilihan.text);

            eng.jalankan(&mut s).await?;
            selesai(&s, &cfg.sessions_dir);
        }

        "lihat" => {
            let id = args.get(1).cloned().unwrap_or_default();
            let path = cfg.sessions_dir.join(format!("{id}.json"));
            let s = muat(&path).await?;
            println!("{}", render::sesi_ke_markdown(&s));
        }

        "nilai" => {
            let id = args.get(1).cloned().unwrap_or_default();
            let v = args.get(2).map(|s| s.as_str()).unwrap_or("");
            let p = match v {
                "kepakai" => Penilaian::Kepakai,
                "setengah" => Penilaian::Setengah,
                "tidak" => Penilaian::Tidak,
                _ => anyhow::bail!("pakai: uji nilai <id_sesi> <kepakai|setengah|tidak>"),
            };
            let path = cfg.sessions_dir.join(format!("{id}.json"));
            let mut s = muat(&path).await?;
            s.penilaian = Some(p);
            simpan(&s, &cfg.sessions_dir).await?;
            println!("✓ penilaian tersimpan");
        }

        _ => {
            println!(
                "uji framing \"<topik>\" [konteks]   ajukan rumusan klaim, lalu berhenti\n\
                 uji lanjut <id_sesi> <n>          setujui rumusan n, jalankan debat\n\
                 uji ulang <id_sesi>               lanjutkan sesi yang berhenti di tengah\n\
                 uji jalan \"<topik>\"               framing + pilih #1 otomatis + jalan\n\
                 uji lihat <id_sesi>               cetak sesi tersimpan sebagai markdown\n\
                 uji nilai <id_sesi> <kepakai|setengah|tidak>\n\
                 uji model                         daftar model di router\n\
                 uji sehat                         tes koneksi"
            );
        }
    }
    Ok(())
}

fn klien(cfg: &Config) -> anyhow::Result<Client> {
    Ok(Client::new(&cfg.base_url, ApiKey::from_env()?)?)
}

async fn mesin(cfg: &Config) -> anyhow::Result<Engine> {
    let prompts = Prompts::muat(&cfg.prompts_dir).await?;
    Ok(Engine {
        client: klien(cfg)?,
        cfg: cfg.clone(),
        prompts,
        sessions_dir: cfg.sessions_dir.clone(),
        // Printing each turn as it lands is the whole point of this binary.
        on_turn: Box::new(|t| println!("\n{}", render::turn_ringkas(t))),
    })
}

fn selesai(s: &Session, dir: &Path) {
    println!("\n{}", "═".repeat(60));
    println!("Sesi selesai: {}", s.id);
    println!("Tersimpan  : {}", s.path_in(dir).display());
    println!("Baca penuh : uji lihat {}", s.id);

    // Turn length collapsing in the final rounds means the round budget is too
    // high — a diagnostic with a clear action, which is what §8 allows.
    println!("\nPanjang turn per ronde (kata):");
    for (r, pro, kontra) in render::panjang_per_ronde(s) {
        println!("  ronde {r}: Pro {pro:>4} · Kontra {kontra:>4}");
    }

    let lembek: Vec<_> = s
        .transcript
        .all()
        .into_iter()
        .filter(|t| !t.flags.is_empty())
        .collect();
    if lembek.is_empty() {
        println!("\nTidak ada turn bertanda.");
    } else {
        println!("\n{} turn bertanda:", lembek.len());
        for t in lembek {
            println!(
                "  {} ronde {} ({}): {:?}",
                t.role.label(),
                t.round,
                t.phase.label(),
                t.flags
            );
        }
    }
}
