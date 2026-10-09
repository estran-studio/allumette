//! Versioned, engine-independent browser lobbies and signaling sessions.
use crate::{
    add_no_cache_headers,
    auth::{AuthSecret, Claims},
    AppState,
};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{delete, get, post},
    Json, Router,
};
use jsonwebtoken::{decode, encode, DecodingKey, EncodingKey, Header, Validation};
use matchbox_protocol::PeerId;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    net::SocketAddr,
};
use uuid::Uuid;

const INVITE_TTL: i64 = 900;
const SESSION_TTL: i64 = 1800;
fn now() -> i64 {
    chrono::Utc::now().timestamp()
}
type ApiError = (StatusCode, Json<serde_json::Value>);
fn error(status: StatusCode, code: &str) -> ApiError {
    (status, Json(serde_json::json!({"code": code})))
}
fn conflict(code: &str) -> ApiError {
    error(StatusCode::CONFLICT, code)
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum Status {
    Waiting,
    Connecting,
    InProgress,
}
#[derive(Debug, Clone, Serialize)]
pub struct Member {
    pub player_id: String,
    pub username: String,
    pub ready: bool,
}
#[derive(Debug, Clone)]
pub struct Session {
    pub id: Uuid,
    pub expires_at: i64,
    pub connect_until: i64,
}
#[derive(Debug, Clone)]
pub struct Lobby {
    pub id: Uuid,
    pub owner: String,
    pub game_id: String,
    pub compatibility_id: String,
    pub capacity: usize,
    pub status: Status,
    pub players: BTreeMap<String, Member>,
    pub session: Option<Session>,
    pub revision: u64,
    pub last_activity: i64,
    pub invitation: Option<(Vec<u8>, i64)>,
}
#[derive(Debug, Clone, Serialize)]
pub struct LobbyDto {
    id: Uuid,
    game_id: String,
    compatibility_id: String,
    capacity: usize,
    status: Status,
    is_owner: bool,
    players: Vec<Member>,
    session_id: Option<Uuid>,
    revision: u64,
}
impl Lobby {
    fn dto(&self, player: &str) -> LobbyDto {
        LobbyDto {
            id: self.id,
            game_id: self.game_id.clone(),
            compatibility_id: self.compatibility_id.clone(),
            capacity: self.capacity,
            status: self.status.clone(),
            is_owner: self.owner == player,
            players: self.players.values().cloned().collect(),
            session_id: self.session.as_ref().map(|s| s.id),
            revision: self.revision,
        }
    }
    fn reset(&mut self) {
        self.status = Status::Waiting;
        self.session = None;
        for p in self.players.values_mut() {
            p.ready = false;
        }
        self.revision += 1;
        self.last_activity = now();
    }
}
#[derive(Debug, Clone)]
pub struct Connection {
    pub player: String,
    pub lobby: Uuid,
    pub session: Uuid,
}
#[derive(Default, Debug)]
pub struct BrowserState {
    pub lobbies: HashMap<Uuid, Lobby>,
    pub pending: HashMap<SocketAddr, Connection>,
    pub connections: HashMap<PeerId, Connection>,
    pub closing: HashSet<PeerId>,
}
impl BrowserState {
    pub fn prune(&mut self) -> Vec<PeerId> {
        let t = now();
        for lobby in self.lobbies.values_mut() {
            if lobby.session.as_ref().is_some_and(|s| {
                s.expires_at <= t || (lobby.status == Status::Connecting && s.connect_until <= t)
            }) {
                lobby.reset();
            }
        }
        self.lobbies
            .retain(|_, l| l.status != Status::Waiting || t - l.last_activity < INVITE_TTL);
        let valid = |c: &Connection| {
            self.lobbies
                .get(&c.lobby)
                .and_then(|l| l.session.as_ref())
                .is_some_and(|s| s.id == c.session)
        };
        let stale: Vec<_> = self
            .connections
            .iter()
            .filter(|(_, c)| !valid(c))
            .map(|(p, _)| *p)
            .collect();
        self.pending.retain(|_, c| valid(c));
        for peer in stale {
            self.connections.remove(&peer);
            self.closing.insert(peer);
        }
        self.closing.iter().copied().collect()
    }
    pub fn contains_player(&self, player: &str) -> bool {
        self.lobbies
            .values()
            .any(|l| l.players.contains_key(player))
    }
    pub fn reserve(&mut self, origin: SocketAddr, ticket: &Ticket) -> Result<(), ApiError> {
        self.prune();
        let lobby = self
            .lobbies
            .get(&ticket.lobby_id)
            .ok_or_else(|| conflict("session_expired"))?;
        if !lobby.players.contains_key(&ticket.sub)
            || lobby
                .session
                .as_ref()
                .is_none_or(|s| s.id != ticket.session_id)
        {
            return Err(conflict("session_expired"));
        }
        if self
            .pending
            .values()
            .chain(self.connections.values())
            .any(|c| c.player == ticket.sub)
        {
            return Err(conflict("already_connected"));
        }
        self.pending.insert(
            origin,
            Connection {
                player: ticket.sub.clone(),
                lobby: ticket.lobby_id,
                session: ticket.session_id,
            },
        );
        Ok(())
    }
    pub fn assign(&mut self, origin: SocketAddr, peer: PeerId) -> bool {
        let Some(c) = self.pending.remove(&origin) else {
            return false;
        };
        // Revalidate the session after the asynchronous WebSocket handshake.
        if self
            .lobbies
            .get(&c.lobby)
            .and_then(|l| l.session.as_ref())
            .is_none_or(|s| s.id != c.session)
        {
            return false;
        }
        let lobby_id = c.lobby;
        self.connections.insert(peer, c);
        let lobby = self.lobbies.get_mut(&lobby_id).unwrap();
        if self
            .connections
            .values()
            .filter(|c| {
                c.lobby == lobby_id && lobby.session.as_ref().is_some_and(|s| s.id == c.session)
            })
            .count()
            == lobby.capacity
        {
            lobby.status = Status::InProgress;
            lobby.revision += 1;
        }
        true
    }
    pub fn same_session(&self, a: PeerId, b: PeerId) -> bool {
        let (Some(a), Some(b)) = (self.connections.get(&a), self.connections.get(&b)) else {
            return false;
        };
        a.lobby == b.lobby
            && a.session == b.session
            && self.lobbies.get(&a.lobby).is_some_and(|l| {
                l.players.contains_key(&a.player)
                    && l.players.contains_key(&b.player)
                    && l.session
                        .as_ref()
                        .is_some_and(|s| s.id == a.session && s.expires_at > now())
            })
    }
    pub fn disconnect(&mut self, peer: PeerId) -> Vec<PeerId> {
        let Some(c) = self.connections.remove(&peer) else {
            return vec![];
        };
        let others = self
            .connections
            .iter()
            .filter(|(_, p)| p.lobby == c.lobby && p.session == c.session)
            .map(|(id, _)| *id)
            .collect();
        if !self.connections.values().any(|p| p.lobby == c.lobby) {
            if let Some(l) = self.lobbies.get_mut(&c.lobby) {
                if l.session.as_ref().is_some_and(|s| s.id == c.session) {
                    l.reset();
                }
            }
        }
        others
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ticket {
    pub sub: String,
    pub lobby_id: Uuid,
    pub session_id: Uuid,
    pub aud: String,
    pub exp: usize,
}
pub fn decode_ticket(token: &str, secret: &AuthSecret) -> Result<Ticket, ()> {
    let mut validation = Validation::default();
    validation.set_audience(&["allumette-browser-signaling"]);
    validation.leeway = 0;
    decode::<Ticket>(
        token,
        &DecodingKey::from_secret(secret.0.as_bytes()),
        &validation,
    )
    .map(|v| v.claims)
    .map_err(|_| ())
}
#[derive(Deserialize)]
struct Create {
    game_id: String,
    compatibility_id: String,
    capacity: usize,
}
#[derive(Deserialize)]
struct Join {
    invitation_token: String,
    compatibility_id: String,
}
#[derive(Deserialize)]
struct Ready {
    ready: bool,
    compatibility_id: String,
}
fn member(claims: &Claims) -> Member {
    Member {
        player_id: claims.sub.clone(),
        username: claims.username.clone(),
        ready: false,
    }
}
fn valid_id(s: &str) -> bool {
    !s.trim().is_empty() && s.len() <= 200
}
fn hash(s: &str) -> Vec<u8> {
    Sha256::digest(s.as_bytes()).to_vec()
}
fn owned<'a>(
    state: &'a mut BrowserState,
    id: Uuid,
    player: &str,
) -> Result<&'a mut Lobby, ApiError> {
    let l = state
        .lobbies
        .get_mut(&id)
        .ok_or_else(|| error(StatusCode::NOT_FOUND, "lobby_missing"))?;
    if l.owner != player {
        return Err(error(StatusCode::FORBIDDEN, "owner_required"));
    }
    Ok(l)
}
fn joined<'a>(
    state: &'a mut BrowserState,
    id: Uuid,
    player: &str,
) -> Result<&'a mut Lobby, ApiError> {
    let l = state
        .lobbies
        .get_mut(&id)
        .ok_or_else(|| error(StatusCode::NOT_FOUND, "lobby_missing"))?;
    if !l.players.contains_key(player) {
        return Err(error(StatusCode::FORBIDDEN, "membership_required"));
    }
    Ok(l)
}
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/browser/v1/lobbies", get(list).post(create))
        .route("/browser/v1/lobbies/{id}", delete(leave))
        .route("/browser/v1/lobbies/{id}/invitations", post(invite))
        .route("/browser/v1/lobbies/{id}/join", post(join))
        .route("/browser/v1/lobbies/{id}/ready", post(ready))
        .route("/browser/v1/lobbies/{id}/start", post(start))
        .route("/browser/v1/lobbies/{id}/session", get(context).delete(end))
        .layer(axum::middleware::map_response(|r: Response| async move {
            add_no_cache_headers(r)
        }))
}
async fn list(State(s): State<AppState>, claims: Claims) -> Response {
    let mut b = s.state.browser.lock().unwrap();
    b.prune();
    add_no_cache_headers(
        Json(
            b.lobbies
                .values()
                .filter(|l| l.players.contains_key(&claims.sub))
                .map(|l| l.dto(&claims.sub))
                .collect::<Vec<_>>(),
        )
        .into_response(),
    )
}
async fn create(
    State(s): State<AppState>,
    claims: Claims,
    Json(p): Json<Create>,
) -> Result<Response, ApiError> {
    if !valid_id(&p.game_id) || !valid_id(&p.compatibility_id) || ![2, 4].contains(&p.capacity) {
        return Err(error(StatusCode::BAD_REQUEST, "invalid_contract"));
    }
    let mut b = s.state.browser.lock().unwrap();
    b.prune();
    if b.contains_player(&claims.sub)
        || s.state
            .players_in_lobbies
            .read()
            .unwrap()
            .contains_key(&claims.sub)
    {
        return Err(conflict("already_in_lobby"));
    }
    let lobby = Lobby {
        id: Uuid::new_v4(),
        owner: claims.sub.clone(),
        game_id: p.game_id,
        compatibility_id: p.compatibility_id,
        capacity: p.capacity,
        status: Status::Waiting,
        players: BTreeMap::from([(claims.sub.clone(), member(&claims))]),
        session: None,
        revision: 1,
        last_activity: now(),
        invitation: None,
    };
    let dto = lobby.dto(&claims.sub);
    b.lobbies.insert(lobby.id, lobby);
    Ok(add_no_cache_headers(Json(dto).into_response()))
}
async fn invite(
    State(s): State<AppState>,
    Path(id): Path<Uuid>,
    claims: Claims,
) -> Result<Response, ApiError> {
    let mut b = s.state.browser.lock().unwrap();
    b.prune();
    let l = owned(&mut b, id, &claims.sub)?;
    if l.status != Status::Waiting {
        return Err(conflict("session_active"));
    }
    let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    let expiry = now() + INVITE_TTL;
    l.invitation = Some((hash(&token), expiry));
    l.last_activity = now();
    Ok(add_no_cache_headers(
        Json(serde_json::json!({"invitation_token":token,"expires_at":expiry})).into_response(),
    ))
}
async fn join(
    State(s): State<AppState>,
    Path(id): Path<Uuid>,
    claims: Claims,
    Json(p): Json<Join>,
) -> Result<Response, ApiError> {
    let mut b = s.state.browser.lock().unwrap();
    b.prune();
    if b.lobbies
        .values()
        .any(|l| l.id != id && l.players.contains_key(&claims.sub))
        || s.state
            .players_in_lobbies
            .read()
            .unwrap()
            .contains_key(&claims.sub)
    {
        return Err(conflict("already_in_lobby"));
    }
    let l = b
        .lobbies
        .get_mut(&id)
        .ok_or_else(|| error(StatusCode::NOT_FOUND, "lobby_missing"))?;
    if l.compatibility_id != p.compatibility_id {
        return Err(conflict("incompatible_build"));
    }
    if l.players.contains_key(&claims.sub) {
        return Ok(add_no_cache_headers(
            Json(l.dto(&claims.sub)).into_response(),
        ));
    }
    if l.status != Status::Waiting {
        return Err(conflict("session_active"));
    }
    if l.invitation
        .as_ref()
        .is_none_or(|(h, exp)| *exp <= now() || *h != hash(&p.invitation_token))
    {
        return Err(error(StatusCode::FORBIDDEN, "invitation_invalid"));
    }
    if l.players.len() >= l.capacity {
        return Err(conflict("lobby_full"));
    }
    l.players.insert(claims.sub.clone(), member(&claims));
    l.revision += 1;
    l.last_activity = now();
    // Membership changes invalidate everybody's readiness.
    for m in l.players.values_mut() {
        m.ready = false;
    }
    Ok(add_no_cache_headers(
        Json(l.dto(&claims.sub)).into_response(),
    ))
}
async fn ready(
    State(s): State<AppState>,
    Path(id): Path<Uuid>,
    claims: Claims,
    Json(p): Json<Ready>,
) -> Result<Response, ApiError> {
    let mut b = s.state.browser.lock().unwrap();
    b.prune();
    let l = joined(&mut b, id, &claims.sub)?;
    if l.compatibility_id != p.compatibility_id {
        return Err(conflict("incompatible_build"));
    }
    if l.status != Status::Waiting {
        return Err(conflict("session_active"));
    }
    l.players.get_mut(&claims.sub).unwrap().ready = p.ready;
    l.revision += 1;
    l.last_activity = now();
    Ok(add_no_cache_headers(
        Json(l.dto(&claims.sub)).into_response(),
    ))
}
async fn start(
    State(s): State<AppState>,
    Path(id): Path<Uuid>,
    claims: Claims,
) -> Result<Response, ApiError> {
    let mut b = s.state.browser.lock().unwrap();
    b.prune();
    let l = owned(&mut b, id, &claims.sub)?;
    if l.status != Status::Waiting {
        return Err(conflict("session_active"));
    }
    if l.players.len() != l.capacity || l.players.values().any(|p| !p.ready) {
        return Err(conflict("players_not_ready"));
    }
    l.session = Some(Session {
        id: Uuid::new_v4(),
        expires_at: now() + SESSION_TTL,
        connect_until: now() + 120,
    });
    l.status = Status::Connecting;
    l.revision += 1;
    Ok(add_no_cache_headers(
        Json(l.dto(&claims.sub)).into_response(),
    ))
}
async fn context(
    State(s): State<AppState>,
    Path(id): Path<Uuid>,
    claims: Claims,
) -> Result<Response, ApiError> {
    let mut b = s.state.browser.lock().unwrap();
    b.prune();
    let l = joined(&mut b, id, &claims.sub)?.clone();
    let session = l
        .session
        .as_ref()
        .ok_or_else(|| conflict("session_missing"))?;
    let ticket = Ticket {
        sub: claims.sub.clone(),
        lobby_id: id,
        session_id: session.id,
        aud: "allumette-browser-signaling".into(),
        exp: ((now() + 60).min(session.expires_at)) as usize,
    };
    let token = encode(
        &Header::default(),
        &ticket,
        &EncodingKey::from_secret(s.secret.0.as_bytes()),
    )
    .map_err(|_| error(StatusCode::INTERNAL_SERVER_ERROR, "ticket_failed"))?;
    // Relative signaling path: the SDK resolves it against its explicit API origin.
    Ok(add_no_cache_headers(Json(serde_json::json!({"schema_version":1,"session_id":session.id,"game_id":l.game_id,"compatibility_id":l.compatibility_id,"capacity":l.capacity,"participants":l.players.values().map(|m| serde_json::json!({"player_id":m.player_id,"username":m.username,"ready":m.ready,"peer_id":b.connections.iter().find(|(_,c)|c.player==m.player_id && c.session==session.id).map(|(p,_)|p.to_string())})).collect::<Vec<_>>(),"local_player_id":claims.sub,"signaling_path":token,"expires_at":ticket.exp})).into_response()))
}
async fn end(
    State(s): State<AppState>,
    Path(id): Path<Uuid>,
    claims: Claims,
) -> Result<Response, ApiError> {
    let peers = {
        let mut b = s.state.browser.lock().unwrap();
        let l = joined(&mut b, id, &claims.sub)?;
        l.reset();
        b.prune()
    };
    for p in peers {
        let _ = s.state.try_send(p, axum::extract::ws::Message::Close(None));
    }
    Ok(add_no_cache_headers(StatusCode::NO_CONTENT.into_response()))
}
async fn leave(
    State(s): State<AppState>,
    Path(id): Path<Uuid>,
    claims: Claims,
) -> Result<Response, ApiError> {
    let peers = {
        let mut b = s.state.browser.lock().unwrap();
        let l = joined(&mut b, id, &claims.sub)?;
        if l.owner == claims.sub {
            b.lobbies.remove(&id);
        } else {
            l.players.remove(&claims.sub);
            l.reset();
        }
        b.prune()
    };
    for p in peers {
        let _ = s.state.try_send(p, axum::extract::ws::Message::Close(None));
    }
    Ok(add_no_cache_headers(StatusCode::NO_CONTENT.into_response()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{to_bytes, Body},
        http::Request,
    };
    use tower::ServiceExt;
    fn fixture() -> (Router, ServerStateFixture) {
        let state = crate::state::ServerState::default();
        let app = crate::app(AppState {
            state: state.clone(),
            secret: AuthSecret("browser-test".into()),
            turn_secret: None,
            turn_urls: None,
            stun_url: "stun:test".into(),
        });
        (app, ServerStateFixture(state))
    }
    struct ServerStateFixture(crate::state::ServerState);
    async fn request(
        app: &Router,
        method: &str,
        path: &str,
        who: &str,
        body: serde_json::Value,
    ) -> (u16, serde_json::Value) {
        let token =
            crate::auth::issue_jwt(who.into(), who.into(), &AuthSecret("browser-test".into()))
                .unwrap();
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .header("Authorization", format!("Bearer {token}"))
                    .header("Content-Type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status().as_u16();
        let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        )
    }
    use serde_json::json;
    #[tokio::test]
    async fn browser_contract_enforces_compatibility_capacity_readiness_and_new_sessions() {
        let (app, state) = fixture();
        let (code, l) = request(
            &app,
            "POST",
            "/browser/v1/lobbies",
            "owner",
            json!({"game_id":"any-engine","compatibility_id":"build-a","capacity":2}),
        )
        .await;
        assert_eq!(code, 200);
        let base = format!("/browser/v1/lobbies/{}", l["id"].as_str().unwrap());
        let (code, inv) = request(
            &app,
            "POST",
            &format!("{base}/invitations"),
            "owner",
            json!({}),
        )
        .await;
        assert_eq!(code, 200);
        assert_eq!(
            request(&app, "POST", &format!("{base}/start"), "owner", json!({}))
                .await
                .0,
            409
        );
        let wrong = request(
            &app,
            "POST",
            &format!("{base}/join"),
            "guest",
            json!({"invitation_token":inv["invitation_token"],"compatibility_id":"build-b"}),
        )
        .await;
        assert_eq!(wrong.1["code"], "incompatible_build");
        let payload =
            json!({"invitation_token":inv["invitation_token"],"compatibility_id":"build-a"});
        assert_eq!(
            request(
                &app,
                "POST",
                &format!("{base}/join"),
                "guest",
                payload.clone()
            )
            .await
            .0,
            200
        );
        assert_eq!(
            request(
                &app,
                "POST",
                &format!("{base}/join"),
                "third",
                payload.clone()
            )
            .await
            .1["code"],
            "lobby_full"
        );
        assert_eq!(
            request(&app, "GET", "/browser/v1/lobbies", "outsider", json!({}))
                .await
                .1,
            json!([])
        );
        for who in ["owner", "guest"] {
            assert_eq!(
                request(
                    &app,
                    "POST",
                    &format!("{base}/ready"),
                    who,
                    json!({"ready":true,"compatibility_id":"build-a"})
                )
                .await
                .0,
                200
            );
        }
        assert_eq!(
            request(&app, "POST", &format!("{base}/start"), "guest", json!({}))
                .await
                .0,
            403
        );
        assert_eq!(
            request(&app, "POST", &format!("{base}/start"), "owner", json!({}))
                .await
                .0,
            200
        );
        let (_, context) =
            request(&app, "GET", &format!("{base}/session"), "owner", json!({})).await;
        let ticket = decode_ticket(
            context["signaling_path"].as_str().unwrap(),
            &AuthSecret("browser-test".into()),
        )
        .unwrap();
        // A signaling ticket cannot authenticate HTTP identity endpoints.
        assert!(decode::<Claims>(
            context["signaling_path"].as_str().unwrap(),
            &DecodingKey::from_secret(b"browser-test"),
            &Validation::default()
        )
        .is_err());
        let origin = "127.0.0.1:1234".parse().unwrap();
        assert!(state
            .0
            .browser
            .lock()
            .unwrap()
            .reserve(origin, &ticket)
            .is_ok());
        assert!(state
            .0
            .browser
            .lock()
            .unwrap()
            .reserve("127.0.0.1:1235".parse().unwrap(), &ticket)
            .is_err());
        assert_eq!(
            request(
                &app,
                "DELETE",
                &format!("{base}/session"),
                "guest",
                json!({})
            )
            .await
            .0,
            204
        );
        assert!(state
            .0
            .browser
            .lock()
            .unwrap()
            .reserve(origin, &ticket)
            .is_err());
        for who in ["owner", "guest"] {
            request(
                &app,
                "POST",
                &format!("{base}/ready"),
                who,
                json!({"ready":true,"compatibility_id":"build-a"}),
            )
            .await;
        }
        request(&app, "POST", &format!("{base}/start"), "owner", json!({})).await;
        let (_, next) = request(&app, "GET", &format!("{base}/session"), "owner", json!({})).await;
        assert_ne!(context["session_id"], next["session_id"]);
        assert_eq!(
            request(&app, "DELETE", &base, "owner", json!({})).await.0,
            204
        );
        assert_eq!(
            request(&app, "GET", "/browser/v1/lobbies", "guest", json!({}))
                .await
                .1,
            json!([])
        );
    }
    #[tokio::test]
    async fn invitations_rotate_expire_and_disconnects_invalidate_sessions() {
        let (app, state) = fixture();
        let (_, l) = request(
            &app,
            "POST",
            "/browser/v1/lobbies",
            "owner",
            json!({"game_id":"generic","compatibility_id":"a","capacity":2}),
        )
        .await;
        let id: Uuid = l["id"].as_str().unwrap().parse().unwrap();
        let base = format!("/browser/v1/lobbies/{id}");
        let (_, old) = request(
            &app,
            "POST",
            &format!("{base}/invitations"),
            "owner",
            json!({}),
        )
        .await;
        let (_, current) = request(
            &app,
            "POST",
            &format!("{base}/invitations"),
            "owner",
            json!({}),
        )
        .await;
        assert_eq!(
            request(
                &app,
                "POST",
                &format!("{base}/join"),
                "guest",
                json!({"compatibility_id":"a","invitation_token":old["invitation_token"]})
            )
            .await
            .0,
            403
        );
        state
            .0
            .browser
            .lock()
            .unwrap()
            .lobbies
            .get_mut(&id)
            .unwrap()
            .invitation
            .as_mut()
            .unwrap()
            .1 = now() - 1;
        assert_eq!(
            request(
                &app,
                "POST",
                &format!("{base}/join"),
                "guest",
                json!({"compatibility_id":"a","invitation_token":current["invitation_token"]})
            )
            .await
            .0,
            403
        );
        let mut b = state.0.browser.lock().unwrap();
        let l = b.lobbies.get_mut(&id).unwrap();
        l.session = Some(Session {
            id: Uuid::new_v4(),
            expires_at: now() + 10,
            connect_until: now() - 1,
        });
        l.status = Status::Connecting;
        b.prune();
        assert_eq!(b.lobbies[&id].status, Status::Waiting);
    }
    #[tokio::test]
    async fn concurrent_joins_do_not_overfill_and_invalidate_readiness() {
        let (app, _) = fixture();
        let (_, l) = request(
            &app,
            "POST",
            "/browser/v1/lobbies",
            "owner",
            json!({"game_id":"generic","compatibility_id":"a","capacity":2}),
        )
        .await;
        let base = format!("/browser/v1/lobbies/{}", l["id"].as_str().unwrap());
        let (_, invitation) = request(
            &app,
            "POST",
            &format!("{base}/invitations"),
            "owner",
            json!({}),
        )
        .await;
        request(
            &app,
            "POST",
            &format!("{base}/ready"),
            "owner",
            json!({"ready":true,"compatibility_id":"a"}),
        )
        .await;
        let path = format!("{base}/join");
        let body =
            json!({"compatibility_id":"a","invitation_token":invitation["invitation_token"]});
        let (a, b) = tokio::join!(
            request(&app, "POST", &path, "first", body.clone()),
            request(&app, "POST", &path, "second", body)
        );
        assert!(matches!((a.0, b.0), (200, 409) | (409, 200)));
        let (_, lobbies) = request(&app, "GET", "/browser/v1/lobbies", "owner", json!({})).await;
        assert_eq!(lobbies[0]["players"].as_array().unwrap().len(), 2);
        assert!(lobbies[0]["players"]
            .as_array()
            .unwrap()
            .iter()
            .all(|p| p["ready"] == false));
    }
    #[tokio::test]
    async fn signaling_isolates_games_and_revokes_ended_sessions() {
        let (app, state) = fixture();
        let mut peers = Vec::new();
        let mut tickets = Vec::new();
        for (owner, game) in [("one", "game-a"), ("two", "game-b")] {
            let (_, l) = request(
                &app,
                "POST",
                "/browser/v1/lobbies",
                owner,
                json!({"game_id":game,"compatibility_id":"a","capacity":2}),
            )
            .await;
            let id: Uuid = l["id"].as_str().unwrap().parse().unwrap();
            let session = Uuid::new_v4();
            let mut b = state.0.browser.lock().unwrap();
            let l = b.lobbies.get_mut(&id).unwrap();
            l.session = Some(Session {
                id: session,
                expires_at: now() + 60,
                connect_until: now() + 30,
            });
            l.status = Status::Connecting;
            let ticket = Ticket {
                sub: owner.into(),
                lobby_id: id,
                session_id: session,
                aud: "allumette-browser-signaling".into(),
                exp: (now() + 30) as usize,
            };
            let origin = format!("127.0.0.1:{}", 1200 + peers.len()).parse().unwrap();
            let peer = PeerId(Uuid::new_v4());
            b.reserve(origin, &ticket).unwrap();
            assert!(b.assign(origin, peer));
            peers.push(peer);
            tickets.push(ticket);
        }
        let mut b = state.0.browser.lock().unwrap();
        assert!(!b.same_session(peers[0], peers[1]));
        b.lobbies.get_mut(&tickets[0].lobby_id).unwrap().reset();
        let closing = b.prune();
        assert!(closing.contains(&peers[0]));
        assert!(!b.same_session(peers[0], peers[0]));
        assert!(b
            .reserve("127.0.0.1:9999".parse().unwrap(), &tickets[0])
            .is_err());
    }
}
