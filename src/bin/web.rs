//! `web` — the debate as a court record in the browser (§7).
//!
//! Shaped like a transcript, not a chat: two opposing columns, turns that keep
//! their side and phase, and no way to type into a running debate. The one place
//! the user types is the framing page, before anything starts (§1).
//!
//! The debate runs in a background task that outlives the browser tab. The page
//! reads the current transcript over an API and follows new turns over SSE; a
//! refresh rebuilds the whole page from the same API, because layout is computed
//! from (phase, side, round), never from message arrival order (§7).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{Html, IntoResponse, Response, Sse, sse},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use timbang::config::{ApiKey, Config, ModelId, Models, Prompts, SessionConfig, WebSettingsPatch};
use timbang::engine::{Engine, OnTurn};
use timbang::llm::Client;
use timbang::render::{self, ClaimView, RiwayatEntry, SessionView, WebTurn};
use timbang::transcript::{Session, SessionStatus, muat, simpan};

/// One live debate. The background task appends turns here and broadcasts them;
/// SSE clients read the backlog then follow the broadcast. Meta (topic, claim,
/// config) is not duplicated — it lives in the file, which is stable once a
/// debate starts.
struct Live {
    turns: Mutex<Vec<WebTurn>>,
    /// The claim panel's current state, replaced wholesale on each turn (§3):
    /// a turn can add a claim and flip another's status at once, so the whole
    /// list travels together rather than as deltas the browser has to merge.
    claims: Mutex<Vec<ClaimView>>,
    status: Mutex<LiveStatus>,
    tx: broadcast::Sender<Ev>,
}

#[derive(Clone)]
enum LiveStatus {
    Berjalan,
    Selesai,
    // The failure message travels on the broadcast channel (Ev::Gagal); this
    // only needs to mark that the run ended badly.
    Gagal,
}

/// State of one turn's read-aloud generation, keyed `(id, index)` in
/// `AppState::audio_jobs`. A finished job leaves no entry: the cached WAV on disk
/// is the record of success (§7 file-is-truth), so this enum only carries the two
/// states a file cannot represent — still running, or failed with a reason. TTS
/// is generated off the request thread (see `api_audio_post`) precisely so no
/// single HTTP request outlives Cloudflare's ~100s edge timeout and returns 524.
enum AudioJob {
    Menyiapkan,
    Gagal(String),
}

/// What crosses the broadcast channel and, mapped, the SSE wire.
#[derive(Clone)]
enum Ev {
    Turn(WebTurn),
    /// The full claim list after a turn. Rides alongside `Turn` rather than
    /// inside it so a reconnecting client that only wants turns can ignore it,
    /// and so the panel can update without the browser diffing claim state.
    Klaim(Vec<ClaimView>),
    Selesai,
    Gagal(String),
}

struct AppState {
    base: Config,
    prompts: Prompts,
    client: Client,
    sessions_dir: PathBuf,
    /// Editable defaults new sessions start from (§6: web settings change the
    /// default, per-session, not a global that rewrites past sessions). Base URL
    /// and bind address are deliberately absent — they are not in `SessionConfig`
    /// and so cannot be changed from the web at all.
    defaults: Mutex<SessionConfig>,
    live: Mutex<HashMap<String, Arc<Live>>>,
    /// In-flight (or failed) read-aloud generations, keyed `(id, index)`. A
    /// successful job removes its entry — the cached WAV is the record — so this
    /// map only holds `Menyiapkan`/`Gagal`. It exists so `api_audio_post` can
    /// return immediately and hand the slow router call to a background task,
    /// keeping every HTTP request under Cloudflare's edge timeout (§ no 524).
    audio_jobs: Mutex<HashMap<(String, usize), AudioJob>>,
    /// Lazy cache of `GET /v1/models`. Router's catalogue is stable within a
    /// process lifetime; a `?refresh=1` query clears it if the user adds a new
    /// model on the router side without restarting the server.
    models_cache: Mutex<Option<Vec<(String, String)>>>,
}

type Sd = Arc<AppState>;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = dotenvy::dotenv();
    let base = Config::muat(&PathBuf::from("config.toml")).await?;
    let prompts = Prompts::muat(&base.prompts_dir).await?;
    let client = Client::new(&base.base_url, ApiKey::from_env()?)?;
    let defaults = base.untuk_sesi();
    let bind = base.bind.clone();

    let state = Arc::new(AppState {
        sessions_dir: base.sessions_dir.clone(),
        defaults: Mutex::new(defaults),
        live: Mutex::new(HashMap::new()),
        audio_jobs: Mutex::new(HashMap::new()),
        models_cache: Mutex::new(None),
        prompts,
        client,
        base,
    });

    let app = Router::new()
        .route("/", get(halaman_baru))
        .route("/sesi/{id}", get(halaman_sidang))
        .route("/sesi/{id}/framing", get(halaman_framing))
        .route("/sesi/{id}/stream", get(stream))
        .route("/riwayat", get(halaman_riwayat))
        .route("/setting", get(halaman_setting))
        .route("/style.css", get(gaya))
        .route("/theme.js", get(tema_js))
        // API. Note what is NOT here: no POST/PUT/PATCH to /api/sesi/{id} once a
        // debate is running. The only write to a session is the framing approval
        // below, which happens before the debate starts (§1: no input on the
        // session page).
        .route("/api/baru", post(api_baru))
        .route("/api/sesi/{id}", get(api_sesi))
        .route("/api/sesi/{id}/mulai", post(api_mulai))
        // Manual synthesis trigger (§1: opened by hand, only after the debate is
        // done). This writes to a session, but only a `selesai` one — never a
        // running debate — so the "no writes to a running session" rule holds.
        .route("/api/sesi/{id}/sintesis", post(api_sintesis))
        // Read-aloud (aksesibilitas). POST generates and caches one turn's audio;
        // GET streams it back. On-demand only — no auto-play, no broadcast — so it
        // does not turn the record into a broadcast (§1). Not a write to the
        // *session*: the transcript is untouched; audio lives in a side cache.
        .route(
            "/api/sesi/{id}/audio/{index}",
            get(api_audio_get).post(api_audio_post),
        )
        // Poll target: the POST above returns before the router call finishes, so
        // the browser watches this until the WAV is cached (or the job fails).
        .route(
            "/api/sesi/{id}/audio/{index}/status",
            get(api_audio_status),
        )
        .route("/api/riwayat", get(api_riwayat))
        .route("/api/setting", get(api_setting_get).post(api_setting_post))
        .route("/api/sehat", get(api_sehat))
        .route("/api/models", get(api_models))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&bind).await?;
    println!("Timbang di http://{bind}");
    axum::serve(listener, app).await?;
    Ok(())
}

// ─── static pages ────────────────────────────────────────────────────────────

async fn halaman_baru() -> Html<&'static str> {
    Html(include_str!("../../web/baru.html"))
}
async fn halaman_sidang() -> Html<&'static str> {
    Html(include_str!("../../web/sidang.html"))
}
async fn halaman_framing() -> Html<&'static str> {
    Html(include_str!("../../web/framing.html"))
}
async fn halaman_riwayat() -> Html<&'static str> {
    Html(include_str!("../../web/riwayat.html"))
}
async fn halaman_setting() -> Html<&'static str> {
    Html(include_str!("../../web/setting.html"))
}
async fn gaya() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "text/css; charset=utf-8")], include_str!("../../web/style.css"))
}
async fn tema_js() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "application/javascript; charset=utf-8")], include_str!("../../web/theme.js"))
}

// ─── new debate + framing ────────────────────────────────────────────────────

#[derive(Deserialize)]
struct BaruReq {
    topik: String,
    konteks: Option<String>,
    settings: Option<WebSettingsPatch>,
}

async fn api_baru(State(st): State<Sd>, Json(req): Json<BaruReq>) -> Result<Response, AppError> {
    if req.topik.trim().is_empty() {
        return Err(AppError::bad("topik kosong"));
    }
    let sc = {
        let d = st.defaults.lock().unwrap().clone();
        terapkan_patch(&d, req.settings.as_ref())
    };
    // §10 lab check happens before a session exists, so a bad pairing never
    // reaches the debate.
    sc.periksa_lawan_sepadan().map_err(|e| AppError::bad(e.to_string()))?;

    let konteks = req.konteks.filter(|s| !s.trim().is_empty());
    let mut s = Session::new(req.topik, konteks, sc);
    let id = s.id.clone();

    // Persist before spawning so the framing page can read the session the
    // instant it loads, even while the moderator is still thinking.
    simpan(&s, &st.sessions_dir).await.map_err(AppError::internal)?;

    // Framing runs in the background and stops at menunggu_persetujuan (§4). Its
    // per-turn hook is a no-op: framing is one quick turn, and the page polls.
    let st2 = st.clone();
    tokio::spawn(async move {
        let eng = mesin(&st2, &s.config_used, Box::new(|_, _, _| {}));
        if let Err(e) = eng.framing(&mut s).await {
            s.status = SessionStatus::Gagal { at_phase: s.phase };
            eprintln!("framing {} gagal: {e}", s.id);
            let _ = simpan(&s, &st2.sessions_dir).await;
        }
    });

    Ok(Json(serde_json::json!({ "id": id })).into_response())
}

#[derive(Deserialize)]
struct MulaiReq {
    /// 1-based index into the moderator's proposed wordings.
    pilih: Option<usize>,
    /// A wording the user typed instead. This is the framing page's edit box —
    /// the one input §1 permits, and it happens before the debate starts.
    klaim: Option<String>,
}

async fn api_mulai(
    State(st): State<Sd>,
    Path(id): Path<String>,
    Json(req): Json<MulaiReq>,
) -> Result<Response, AppError> {
    let path = st.sessions_dir.join(format!("{id}.json"));
    let mut s = muat(&path).await.map_err(|_| AppError::not_found())?;

    // Only a session awaiting approval may start. This is what keeps the "no
    // writes to a running session" rule true: once berjalan, there is no route
    // that mutates it.
    if !matches!(s.status, SessionStatus::MenungguPersetujuan) {
        return Err(AppError::bad("sesi ini tidak sedang menunggu persetujuan framing"));
    }

    let klaim = match (req.pilih, req.klaim) {
        (_, Some(t)) if !t.trim().is_empty() => t.trim().to_string(),
        (Some(n), _) => s
            .framing_options
            .get(n.wrapping_sub(1))
            .map(|o| o.text.clone())
            .ok_or_else(|| AppError::bad("nomor rumusan tidak ada"))?,
        _ => return Err(AppError::bad("pilih nomor rumusan atau tulis klaim sendiri")),
    };
    s.claim = Some(klaim);
    s.status = SessionStatus::Berjalan;
    simpan(&s, &st.sessions_dir).await.map_err(AppError::internal)?;

    // Register the live channel before spawning, so an SSE client that connects
    // immediately finds it.
    let (tx, _) = broadcast::channel(256);
    let view0 = render::SessionView::from_session(&s);
    let live = Arc::new(Live {
        turns: Mutex::new(view0.turns),
        claims: Mutex::new(view0.claims),
        status: Mutex::new(LiveStatus::Berjalan),
        tx: tx.clone(),
    });
    st.live.lock().unwrap().insert(id.clone(), live.clone());

    let st2 = st.clone();
    let cfg_used = s.config_used.clone();
    tokio::spawn(async move {
        let live2 = live.clone();
        let on_turn = Box::new(
            move |idx: usize, t: &timbang::transcript::Turn, claims: &[timbang::transcript::Claim]| {
                let wt = render::turn_web(idx, t);
                live2.turns.lock().unwrap().push(wt.clone());
                // Re-sort each push so the panel keeps unanswered claims first,
                // matching the from-disk view (§7).
                let mut cv: Vec<_> = claims.iter().collect();
                cv.sort_by_key(|c| (c.status != timbang::transcript::ClaimStatus::Hidup, c.born_round));
                let cv: Vec<ClaimView> = cv.into_iter().map(render::claim_view).collect();
                *live2.claims.lock().unwrap() = cv.clone();
                let _ = live2.tx.send(Ev::Turn(wt));
                let _ = live2.tx.send(Ev::Klaim(cv));
            },
        );
        let eng = mesin(&st2, &cfg_used, on_turn);

        let hasil = eng.jalankan(&mut s).await;
        match hasil {
            Ok(()) => {
                *live.status.lock().unwrap() = LiveStatus::Selesai;
                let _ = live.tx.send(Ev::Selesai);
            }
            Err(e) => {
                let pesan = e.to_string();
                *live.status.lock().unwrap() = LiveStatus::Gagal;
                eprintln!("sesi {} gagal: {pesan}", s.id);
                let _ = live.tx.send(Ev::Gagal(pesan));
            }
        }
        // The file is authoritative once the debate ends; drop the in-memory
        // copy so later reads come from disk. Clients still attached already got
        // the terminal event above, and a client that connects afterwards reads
        // the finished file.
        st2.live.lock().unwrap().remove(&s.id);
    });

    Ok(Json(serde_json::json!({ "ok": true })).into_response())
}

/// `POST /api/sesi/{id}/sintesis` — run the synthesizer over a finished debate.
///
/// The one write to a session that is not framing approval, and it is allowed
/// only because a `selesai` session is not a running one: §1 forbids input to a
/// *running* debate, and requires synthesis to be opened by hand once the debate
/// is done — which is exactly this. A running or failed session is refused.
///
/// Synchronous: one request, and the page waits for it. No live channel is
/// involved — it was dropped when the debate finished (see `api_mulai`), so the
/// engine's per-turn hook is a no-op here and the fresh turn travels back in the
/// response body instead.
async fn api_sintesis(State(st): State<Sd>, Path(id): Path<String>) -> Result<Response, AppError> {
    let path = st.sessions_dir.join(format!("{id}.json"));
    let mut s = muat(&path).await.map_err(|_| AppError::not_found())?;

    match s.status {
        SessionStatus::Selesai => {}
        SessionStatus::Berjalan => return Err(AppError::bad("debat belum selesai")),
        SessionStatus::Gagal { .. } => {
            return Err(AppError::bad("debat berhenti di tengah — tidak bisa disintesis"));
        }
        SessionStatus::MenungguPersetujuan => {
            return Err(AppError::bad("debat belum mulai"));
        }
    }

    let eng = mesin(&st, &s.config_used, Box::new(|_, _, _| {}));
    let idx = eng.jalankan_sintesis(&mut s).await.map_err(AppError::internal)?;
    let turn = s
        .transcript
        .get(idx)
        .ok_or_else(|| AppError::internal("turn sintesis hilang setelah dibuat"))?;
    Ok(Json(render::turn_web(idx, turn)).into_response())
}

// ─── read-aloud (TTS) ──────────────────────────────────────────────────────────

/// Where one turn's cached audio lives. Under `sessions_dir` so it rides the same
/// persistent volume as the sessions themselves, and `.dockerignore` already
/// keeps `sesi/` out of the image. Keyed by turn index, which is stable: the
/// transcript is append-only, so index 3 is always the same turn.
fn audio_path(sessions_dir: &std::path::Path, id: &str, index: usize) -> PathBuf {
    sessions_dir.join("audio").join(id).join(format!("{index}.wav"))
}

/// `POST /api/sesi/{id}/audio/{index}` — start generating one turn's audio, and
/// return **immediately**.
///
/// The router's TTS call can take longer than Cloudflare's ~100s edge timeout for
/// a long turn, and a request that blocks on it gets killed mid-flight with a 524.
/// So this never awaits the router: it validates fast, marks the job `Menyiapkan`,
/// spawns a background task to do the slow call, and replies at once. The browser
/// then polls `GET .../audio/{index}/status` (see `api_audio_status`) until the
/// WAV is cached. This mirrors how a debate runs off-thread in `api_mulai`.
///
/// Idempotent: a cached file short-circuits to `siap`, and an in-flight job is not
/// re-spawned, so a second click — or a click after a refresh — is cheap. A click
/// on a previously `Gagal` turn clears it and retries. The voice is chosen by the
/// turn's role (§7); markdown is stripped so the reader does not pronounce `**`.
async fn api_audio_post(
    State(st): State<Sd>,
    Path((id, index)): Path<(String, usize)>,
) -> Result<Response, AppError> {
    let cache = audio_path(&st.sessions_dir, &id, index);
    if tokio::fs::try_exists(&cache).await.unwrap_or(false) {
        return Ok(Json(serde_json::json!({ "status": "siap" })).into_response());
    }

    // Validate the turn *before* spawning, so a bad index or an empty turn fails
    // fast as a 400 the user sees, instead of surfacing later as a job failure.
    let path = st.sessions_dir.join(format!("{id}.json"));
    let s = muat(&path).await.map_err(|_| AppError::not_found())?;
    let turn = s
        .transcript
        .get(index)
        .ok_or_else(|| AppError::bad("nomor turn tidak ada"))?;

    let voice = st.base.tts.voice_for(turn.role);
    let model_path = st.base.tts.model_path(voice);
    let bersih = teks_untuk_suara(&turn.text);
    if bersih.trim().is_empty() {
        return Err(AppError::bad("turn tanpa teks untuk dibacakan"));
    }

    // Claim the job under the lock. If one is already in flight, don't spawn a
    // second — just report that it is being prepared. A prior failure is cleared
    // here, so clicking "coba lagi" retries.
    {
        let mut jobs = st.audio_jobs.lock().unwrap();
        if matches!(jobs.get(&(id.clone(), index)), Some(AudioJob::Menyiapkan)) {
            return Ok(Json(serde_json::json!({ "status": "menyiapkan" })).into_response());
        }
        jobs.insert((id.clone(), index), AudioJob::Menyiapkan);
    }

    // The slow router call and the disk write happen here, off the request
    // thread. On success the entry is removed (the cached file is the record); on
    // failure it is replaced with `Gagal` so the poller can report the reason.
    let st2 = st.clone();
    tokio::spawn(async move {
        let hasil = async {
            let audio = st2
                .client
                .tts(&model_path, &bersih)
                .await
                .map_err(|e| e.to_string())?;
            if let Some(dir) = cache.parent() {
                tokio::fs::create_dir_all(dir)
                    .await
                    .map_err(|e| e.to_string())?;
            }
            tokio::fs::write(&cache, &audio)
                .await
                .map_err(|e| e.to_string())?;
            Ok::<(), String>(())
        }
        .await;

        let mut jobs = st2.audio_jobs.lock().unwrap();
        match hasil {
            Ok(()) => {
                jobs.remove(&(id, index));
            }
            Err(pesan) => {
                eprintln!("tts sesi {id} turn {index} gagal: {pesan}");
                jobs.insert((id, index), AudioJob::Gagal(pesan));
            }
        }
    });

    Ok(Json(serde_json::json!({ "status": "menyiapkan" })).into_response())
}

/// `GET /api/sesi/{id}/audio/{index}/status` — where a background generation is.
///
/// Read-only: it never starts a job, it only reports one. The cached file wins
/// over the job map, so a job that finished between poll ticks reads as `siap`
/// even though its entry is already gone. `belum` means neither a file nor a job
/// exists — e.g. after a restart wiped the in-memory map — and the browser should
/// re-POST to start over.
async fn api_audio_status(
    State(st): State<Sd>,
    Path((id, index)): Path<(String, usize)>,
) -> Result<Response, AppError> {
    let cache = audio_path(&st.sessions_dir, &id, index);
    if tokio::fs::try_exists(&cache).await.unwrap_or(false) {
        return Ok(Json(serde_json::json!({ "status": "siap" })).into_response());
    }
    let body = match st.audio_jobs.lock().unwrap().get(&(id, index)) {
        Some(AudioJob::Menyiapkan) => serde_json::json!({ "status": "menyiapkan" }),
        Some(AudioJob::Gagal(m)) => serde_json::json!({ "status": "gagal", "error": m }),
        None => serde_json::json!({ "status": "belum" }),
    };
    Ok(Json(body).into_response())
}

/// `GET /api/sesi/{id}/audio/{index}` — stream cached audio, or 404 if it has not
/// been generated yet. The browser only reaches here after a status of `siap`, so
/// a 404 means "generate first", not an error.
async fn api_audio_get(
    State(st): State<Sd>,
    Path((id, index)): Path<(String, usize)>,
) -> Result<Response, AppError> {
    let cache = audio_path(&st.sessions_dir, &id, index);
    let bytes = tokio::fs::read(&cache)
        .await
        .map_err(|_| AppError::not_found())?;
    Ok(([(header::CONTENT_TYPE, "audio/wav")], bytes).into_response())
}

/// Strip the little markdown the debaters emit (`**bold**`, `*italic*`, `##`,
/// backticks) so the reader speaks words, not punctuation. Deliberately minimal —
/// it matches what the turn text actually contains, nothing more.
fn teks_untuk_suara(s: &str) -> String {
    s.chars().filter(|&c| c != '*' && c != '#' && c != '`' && c != '_').collect()
}

// ─── reads ───────────────────────────────────────────────────────────────────

async fn api_sesi(State(st): State<Sd>, Path(id): Path<String>) -> Result<Response, AppError> {
    let path = st.sessions_dir.join(format!("{id}.json"));
    let s = muat(&path).await.map_err(|_| AppError::not_found())?;
    let mut view = SessionView::from_session(&s);

    // A running debate's in-memory backlog runs ahead of the per-phase
    // checkpoint, so prefer it when present.
    if let Some(live) = st.live.lock().unwrap().get(&id) {
        view.turns = live.turns.lock().unwrap().clone();
        view.claims = live.claims.lock().unwrap().clone();
        view.status = match &*live.status.lock().unwrap() {
            LiveStatus::Berjalan => "berjalan",
            LiveStatus::Selesai => "selesai",
            LiveStatus::Gagal => "gagal",
        };
    }
    Ok(Json(view).into_response())
}

async fn api_riwayat(State(st): State<Sd>) -> Result<Response, AppError> {
    let mut entries = Vec::new();
    let mut rd = tokio::fs::read_dir(&st.sessions_dir)
        .await
        .map_err(AppError::internal)?;
    while let Some(e) = rd.next_entry().await.map_err(AppError::internal)? {
        let p = e.path();
        if p.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        // A single corrupt file must not sink the whole list.
        if let Ok(s) = muat(&p).await {
            entries.push(RiwayatEntry::from_session(&s));
        }
    }
    entries.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    Ok(Json(entries).into_response())
}

// ─── SSE ─────────────────────────────────────────────────────────────────────

async fn stream(
    State(st): State<Sd>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    // EventSource replays this on reconnect; -1 means "send everything".
    let last: i64 = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .unwrap_or(-1);

    let live = st.live.lock().unwrap().get(&id).cloned();

    let stream = if let Some(live) = live {
        // Subscribe first, then snapshot the backlog: a turn pushed in that
        // window lands on the receiver and is skipped by index below, rather
        // than being missed entirely.
        let mut rx = live.tx.subscribe();
        let backlog = live.turns.lock().unwrap().clone();
        let max_backlog = backlog.last().map(|t| t.index as i64).unwrap_or(-1);

        // Snapshot the current claim panel too, so a client connecting mid-debate
        // paints it immediately rather than waiting for the next turn.
        let claims0 = live.claims.lock().unwrap().clone();

        Sse::new(Box::pin(async_stream::stream! {
            for wt in backlog {
                if wt.index as i64 > last {
                    yield Ok(ev_turn(&wt));
                }
            }
            if !claims0.is_empty() {
                yield Ok(ev_klaim(&claims0));
            }
            loop {
                match rx.recv().await {
                    Ok(Ev::Turn(wt)) => {
                        if wt.index as i64 > max_backlog && wt.index as i64 > last {
                            yield Ok(ev_turn(&wt));
                        }
                    }
                    Ok(Ev::Klaim(cv)) => { yield Ok(ev_klaim(&cv)); }
                    Ok(Ev::Selesai) => { yield Ok(sse::Event::default().event("selesai").data("{}")); break; }
                    Ok(Ev::Gagal(p)) => { yield Ok(sse::Event::default().event("gagal").data(p)); break; }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }) as SseStream)
    } else {
        // Not live: finished, failed, or from a previous run. Replay from the
        // file and close — this is what makes a mid-debate refresh rebuild the
        // page with no extra logic (§7).
        let path = st.sessions_dir.join(format!("{id}.json"));
        let s = muat(&path).await.map_err(|_| AppError::not_found())?;
        let view = SessionView::from_session(&s);
        let claims = view.claims.clone();
        let terminal = match s.status {
            SessionStatus::Gagal { .. } => Some(("gagal", "sesi berhenti — bisa dilanjutkan".to_string())),
            SessionStatus::Selesai => Some(("selesai", "{}".to_string())),
            _ => None,
        };
        Sse::new(Box::pin(async_stream::stream! {
            for wt in view.turns {
                if wt.index as i64 > last {
                    yield Ok(ev_turn(&wt));
                }
            }
            if !claims.is_empty() {
                yield Ok(ev_klaim(&claims));
            }
            if let Some((name, data)) = terminal {
                yield Ok(sse::Event::default().event(name).data(data));
            }
        }) as SseStream)
    };

    Ok(stream.keep_alive(sse::KeepAlive::default()).into_response())
}

type SseStream =
    std::pin::Pin<Box<dyn futures_util::Stream<Item = Result<sse::Event, std::convert::Infallible>> + Send>>;

fn ev_turn(wt: &WebTurn) -> sse::Event {
    // id becomes the client's Last-Event-ID, so a reconnect asks only for turns
    // after this one.
    sse::Event::default()
        .id(wt.index.to_string())
        .event("turn")
        .data(serde_json::to_string(wt).unwrap_or_default())
}

/// The claim panel as one event. Deliberately without an `id`: the panel is a
/// whole-state snapshot, not part of the turn sequence, so it must not advance
/// the client's Last-Event-ID and cause turns to be skipped on reconnect.
fn ev_klaim(claims: &[ClaimView]) -> sse::Event {
    sse::Event::default()
        .event("klaim")
        .data(serde_json::to_string(claims).unwrap_or_default())
}

// ─── settings + health ───────────────────────────────────────────────────────

#[derive(Serialize)]
struct SettingView {
    /// Shown read-only. Not editable from the web — a wrong value here could aim
    /// the key at someone else's server (§6).
    base_url: String,
    models: ModelsOut,
    rounds: u32,
    word_limit: u32,
    temperature: f32,
}

#[derive(Serialize)]
struct ModelsOut {
    pro: String,
    kontra: String,
    moderator: String,
    synthesizer: String,
}

async fn api_setting_get(State(st): State<Sd>) -> Json<SettingView> {
    let d = st.defaults.lock().unwrap().clone();
    Json(SettingView {
        base_url: st.base.base_url.clone(),
        models: ModelsOut {
            pro: d.models.pro.as_str().to_string(),
            kontra: d.models.kontra.as_str().to_string(),
            moderator: d.models.moderator.as_str().to_string(),
            synthesizer: d.models.synthesizer.as_str().to_string(),
        },
        rounds: d.rounds,
        word_limit: d.word_limit,
        temperature: d.temperature,
    })
}

async fn api_setting_post(
    State(st): State<Sd>,
    Json(patch): Json<WebSettingsPatch>,
) -> Result<Response, AppError> {
    let next = {
        let d = st.defaults.lock().unwrap().clone();
        terapkan_patch(&d, Some(&patch))
    };
    next.periksa_lawan_sepadan().map_err(|e| AppError::bad(e.to_string()))?;
    *st.defaults.lock().unwrap() = next;
    // Note: in-memory only. A restart re-reads config.toml. Persisting the
    // default to disk is a Tahap-4 refinement, not needed to run a session.
    Ok(Json(serde_json::json!({ "ok": true })).into_response())
}

async fn api_sehat(State(st): State<Sd>) -> Json<serde_json::Value> {
    match st.client.health().await {
        Ok(ok) => Json(serde_json::json!({ "ok": ok })),
        Err(e) => Json(serde_json::json!({ "ok": false, "pesan": e.to_string() })),
    }
}

#[derive(Serialize)]
struct ModelOut {
    id: String,
    owned_by: String,
}

#[derive(Deserialize, Default)]
struct ModelsQuery {
    refresh: Option<u8>,
}

/// `GET /api/models[?refresh=1]` — the router's catalogue, cached lazily.
/// §5 flags combo-owned models as forbidden for Pro/Kontra; the UI is what
/// enforces that (disables the option), not the server, because the config
/// layer already runs a pairing check before a session starts.
async fn api_models(
    State(st): State<Sd>,
    Query(q): Query<ModelsQuery>,
) -> Result<Response, AppError> {
    if q.refresh == Some(1) {
        *st.models_cache.lock().unwrap() = None;
    }
    if let Some(cached) = st.models_cache.lock().unwrap().clone() {
        return Ok(Json(map_models(&cached)).into_response());
    }
    let daftar = st
        .client
        .list_models()
        .await
        .map_err(|e| AppError(StatusCode::BAD_GATEWAY, e.to_string()))?;
    *st.models_cache.lock().unwrap() = Some(daftar.clone());
    Ok(Json(map_models(&daftar)).into_response())
}

fn map_models(rows: &[(String, String)]) -> Vec<ModelOut> {
    rows.iter()
        .map(|(id, owned_by)| ModelOut { id: id.clone(), owned_by: owned_by.clone() })
        .collect()
}

// ─── helpers ─────────────────────────────────────────────────────────────────

/// Builds an engine for one session, with its tunables (models, rounds, limits)
/// taken from the session's own config rather than the global default — so a
/// per-session setting is actually honoured, and an old session replays under
/// the settings it ran with (§3).
fn mesin(st: &AppState, sc: &SessionConfig, on_turn: OnTurn) -> Engine {
    let mut cfg = st.base.clone();
    cfg.models = sc.models.clone();
    cfg.rounds = sc.rounds;
    cfg.word_limit = sc.word_limit;
    cfg.temperature = sc.temperature;
    cfg.similarity_threshold = sc.similarity_threshold;
    Engine {
        client: st.client.clone(),
        cfg,
        prompts: st.prompts.clone(),
        sessions_dir: st.sessions_dir.clone(),
        on_turn,
    }
}

fn terapkan_patch(base: &SessionConfig, p: Option<&WebSettingsPatch>) -> SessionConfig {
    let mut c = base.clone();
    if let Some(p) = p {
        if let Some(m) = &p.models {
            c.models = Models {
                pro: ModelId::new(m.pro.as_str()),
                kontra: ModelId::new(m.kontra.as_str()),
                moderator: ModelId::new(m.moderator.as_str()),
                synthesizer: ModelId::new(m.synthesizer.as_str()),
                fact_checker: m.fact_checker.clone(),
            };
        }
        if let Some(r) = p.rounds {
            c.rounds = r;
        }
        if let Some(w) = p.word_limit {
            c.word_limit = w;
        }
        if let Some(t) = p.temperature {
            c.temperature = t;
        }
    }
    c
}

// ─── errors ──────────────────────────────────────────────────────────────────

struct AppError(StatusCode, String);

impl AppError {
    fn bad(m: impl Into<String>) -> Self {
        AppError(StatusCode::BAD_REQUEST, m.into())
    }
    fn not_found() -> Self {
        AppError(StatusCode::NOT_FOUND, "sesi tidak ditemukan".into())
    }
    fn internal(e: impl std::fmt::Display) -> Self {
        AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (self.0, Json(serde_json::json!({ "error": self.1 }))).into_response()
    }
}
