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
    extract::{Path, State},
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
        // API. Note what is NOT here: no POST/PUT/PATCH to /api/sesi/{id} once a
        // debate is running. The only write to a session is the framing approval
        // below, which happens before the debate starts (§1: no input on the
        // session page).
        .route("/api/baru", post(api_baru))
        .route("/api/sesi/{id}", get(api_sesi))
        .route("/api/sesi/{id}/mulai", post(api_mulai))
        .route("/api/riwayat", get(api_riwayat))
        .route("/api/setting", get(api_setting_get).post(api_setting_post))
        .route("/api/sehat", get(api_sehat))
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
