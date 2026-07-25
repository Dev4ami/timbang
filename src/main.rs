// Timbang — Tahap 0 (coretan). Sengaja lurus, tanpa abstraksi, satu file.
//
// Satu-satunya tujuan: bukti 2 request ke 2 model berbeda balik 2 teks, dan bukti
// apakah field `model` di RESPONS bisa beda dari `model` di REQUEST (CLAUDE.md §5).
//
// File ini WAJIB dihapus sebelum Tahap 1 (§9). Jangan jadikan fondasi.
// Prompt sengaja ditulis inline di sini walau §1 melarang — hanya boleh karena file
// ini mati. Mulai Tahap 1 semua teks yang dikirim ke model hidup di prompts/*.md.

use std::time::Duration;

/// This router strips the route prefix on the way back: asking for
/// `cc/claude-opus-5` yields `claude-opus-5`. Comparing raw strings would report a
/// mismatch on every single turn and make the real §5 signal unreadable, so both
/// sides are compared on the part after the last `/`.
///
/// The looseness is deliberate and worth naming: a substitution *within* the same
/// route is still caught, but a router that silently answers `kr/x` with `cc/x`
/// would not be. Tahap 1 must decide whether that residual hole is acceptable.
fn nama_model(id: &str) -> &str {
    id.rsplit('/').next().unwrap_or(id)
}

fn main() {
    // Optional .env; real env vars win. Key is never printed, never hardcoded (§6).
    let _ = dotenvy::dotenv();

    // Fail fast on missing config. No unwrap_or_default() (§6).
    let api_key = match std::env::var("ROUTER_API_KEY") {
        Ok(v) if !v.trim().is_empty() => v,
        _ => {
            eprintln!("ROUTER_API_KEY kosong / tidak ada. Isi .env dulu.");
            std::process::exit(1);
        }
    };
    let base_url = match std::env::var("ROUTER_BASE_URL") {
        Ok(v) if !v.trim().is_empty() => v.trim().trim_end_matches('/').to_string(),
        _ => {
            eprintln!("ROUTER_BASE_URL kosong / tidak ada.");
            eprintln!("Set di shell: $env:ROUTER_BASE_URL = \"https://router.contoh.tld\"");
            std::process::exit(1);
        }
    };

    // Hardcoded topic — Tahap 0 is supposed to look like this (§9).
    let klaim = "Alat debat antar-AI lebih berguna daripada satu model yang menjawab langsung.";

    // MODEL SEMENTARA. Both are Claude, which §10 forbids for a real debate —
    // allowed here ONLY because Tahap 0 tests plumbing, not debate quality, and
    // this file gets deleted. These two ids must never reach config.toml (§9, §10).
    // Tahap 1 does not start until a second lab is wired up in the router (§11).
    let model_pro = "cc/claude-opus-5";
    let model_kontra = "cc/claude-fable-5";

    let agent = ureq::Agent::config_builder()
        // Keep 4xx/5xx as normal responses so the raw error body stays readable —
        // the "400 Invalid model format" body is exactly what is needed while
        // guessing model ids.
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(180)))
        .build()
        .new_agent();

    let url = format!("{base_url}/v1/chat/completions");

    // Two sequential requests, not parallel. §4 allows Opening to run in parallel,
    // but doing that here would make Tahap 0 pick a concurrency shape that Tahap 1
    // then inherits — precisely what §9 forbids.

    // ---------------- request 1: PRO ----------------
    // `stream: false` must be explicit — this router defaults to text/event-stream
    // and only returns plain JSON when the field is written out.
    let body_pro = serde_json::json!({
        "model": model_pro,
        "stream": false,
        "temperature": 0.7,
        "max_tokens": 700,
        "messages": [
            { "role": "system",
              "content": "Kamu sisi PRO. Bela klaim ini sekuat mungkin. Jangan mengakui kelemahan, jangan mencari titik temu. Bahasa Indonesia. Maksimal 200 kata." },
            { "role": "user", "content": klaim }
        ]
    });

    let mut resp_pro = match agent
        .post(&url)
        .header("Authorization", &format!("Bearer {api_key}"))
        .header("X-9Router-Token-Saver", "off") // mandatory, no silent compression (§5)
        .header("Content-Type", "application/json")
        .send_json(&body_pro)
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("PRO gagal transport: {e}");
            std::process::exit(1);
        }
    };
    let status_pro = resp_pro.status().as_u16();
    let json_pro: serde_json::Value = match resp_pro.body_mut().read_json() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("PRO status {status_pro}, body bukan JSON: {e}");
            std::process::exit(1);
        }
    };
    if status_pro != 200 {
        eprintln!("PRO status {status_pro}: {json_pro}");
        std::process::exit(1);
    }

    // ---------------- request 2: KONTRA ----------------
    let body_kontra = serde_json::json!({
        "model": model_kontra,
        "stream": false,
        "temperature": 0.7,
        "max_tokens": 700,
        "messages": [
            { "role": "system",
              "content": "Kamu sisi KONTRA. Serang klaim ini sekeras mungkin. Jangan mencari titik temu, jangan menyeimbangkan. Bahasa Indonesia. Maksimal 200 kata." },
            { "role": "user", "content": klaim }
        ]
    });

    let mut resp_kontra = match agent
        .post(&url)
        .header("Authorization", &format!("Bearer {api_key}"))
        .header("X-9Router-Token-Saver", "off")
        .header("Content-Type", "application/json")
        .send_json(&body_kontra)
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("KONTRA gagal transport: {e}");
            std::process::exit(1);
        }
    };
    let status_kontra = resp_kontra.status().as_u16();
    let json_kontra: serde_json::Value = match resp_kontra.body_mut().read_json() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("KONTRA status {status_kontra}, body bukan JSON: {e}");
            std::process::exit(1);
        }
    };
    if status_kontra != 200 {
        eprintln!("KONTRA status {status_kontra}: {json_kontra}");
        std::process::exit(1);
    }

    // ---------------- cetak ----------------
    // The answering model is read from the RESPONSE, not the request. That is the
    // whole reason Tahap 0 exists: it makes the combo hazard visible (§3, §5).
    let jawab_pro = json_pro["model"].as_str().unwrap_or("(field model tidak ada)");
    let jawab_kontra = json_kontra["model"]
        .as_str()
        .unwrap_or("(field model tidak ada)");
    let teks_pro = json_pro["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or("(kosong)");
    let teks_kontra = json_kontra["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or("(kosong)");
    // finish_reason shows whether max_tokens truncated the turn — otherwise an
    // evening gets spent debugging a prompt that was never the problem.
    let stop_pro = json_pro["choices"][0]["finish_reason"]
        .as_str()
        .unwrap_or("?");
    let stop_kontra = json_kontra["choices"][0]["finish_reason"]
        .as_str()
        .unwrap_or("?");

    println!("KLAIM: {klaim}\n");

    println!("===== PRO =====");
    println!("diminta       : {model_pro}");
    println!("model_menjawab: {jawab_pro}");
    println!("finish_reason : {stop_pro}");
    if nama_model(model_pro) != nama_model(jawab_pro) {
        println!("!! MISMATCH — model lain yang menjawab. Transkrip akan bohong (§5).");
    }
    println!("{teks_pro}\n");

    println!("===== KONTRA =====");
    println!("diminta       : {model_kontra}");
    println!("model_menjawab: {jawab_kontra}");
    println!("finish_reason : {stop_kontra}");
    if nama_model(model_kontra) != nama_model(jawab_kontra) {
        println!("!! MISMATCH — model lain yang menjawab. Transkrip akan bohong (§5).");
    }
    println!("{teks_kontra}");

    // ---------------- probe: does the router echo the model that actually answered?
    // This router exposes no owned_by:"combo" model, but it does expose "kr/auto",
    // whose name implies automatic model selection — the same hazard §5 describes.
    // The answer decides whether §5 can be enforced at runtime at all:
    //   response echoes a concrete id  -> substitution is detectable, build the guard
    //   response echoes "kr/auto"      -> substitution is invisible, §5 must be
    //                                     enforced entirely in config
    println!("\n===== PROBE kr/auto (uji echo router) =====");
    let body_probe = serde_json::json!({
        "model": "kr/auto",
        "stream": false,
        "max_tokens": 1,
        "messages": [ { "role": "user", "content": "hi" } ]
    });

    match agent
        .post(&url)
        .header("Authorization", &format!("Bearer {api_key}"))
        .header("X-9Router-Token-Saver", "off")
        .header("Content-Type", "application/json")
        .send_json(&body_probe)
    {
        Ok(mut r) => {
            let status = r.status().as_u16();
            match r.body_mut().read_json::<serde_json::Value>() {
                Ok(v) => {
                    let jawab = v["model"].as_str().unwrap_or("(field model tidak ada)");
                    println!("status        : {status}");
                    println!("diminta       : kr/auto");
                    println!("model_menjawab: {jawab}");
                    if nama_model(jawab) == "auto" {
                        println!(
                            ">> Router MEMANTULKAN id yang diminta. Substitusi TIDAK terdeteksi \
                             saat runtime — §5 harus dikunci di config."
                        );
                    } else if jawab.starts_with('(') {
                        println!(">> Respons tanpa field `model`. Provenance tidak terverifikasi.");
                    } else {
                        println!(
                            ">> Router mengembalikan id KONKRET. Substitusi terdeteksi saat \
                             runtime — guard §5 layak dibangun."
                        );
                    }
                }
                Err(e) => println!("status {status}, body bukan JSON: {e}"),
            }
        }
        // A failed probe is information, not a reason to fail the run.
        Err(e) => println!("probe gagal transport: {e}"),
    }
}
