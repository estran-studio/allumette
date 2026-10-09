pub mod browser;
pub mod args;
pub mod auth;
pub mod helpers;
pub mod lobby;
pub mod state;
pub mod topology;
use crate::{auth::AuthSecret, state::ServerState, topology::MatchmakingDemoTopology};
use axum::http::HeaderMap;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use hmac::{Hmac, Mac};
use sha1::Sha1;
use axum::{
    extract::{FromRef, Path, State},
    http::StatusCode,
    response::{IntoResponse, Json, sse::{Event, KeepAlive, Sse}},
    routing::{get, post, delete},
    Router,
};
use jsonwebtoken::{decode, DecodingKey, Validation};
use matchbox_signaling::SignalingServerBuilder;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{convert::Infallible, net::SocketAddr};
use tokio_stream::{StreamExt as _, wrappers::BroadcastStream};
use tower_http::cors::{CorsLayer, AllowOrigin};
use tracing::info;
use tracing_subscriber::prelude::*;

const DEFAULT_STUN_URL: &str = "stun:stun.l.google.com:19302";
const TURN_CREDENTIAL_TTL: i64 = 24 * 60 * 60; // 24 hours

pub fn setup_logging() {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "allumette_server=info,tower_http=debug".into()),
        )
        .with(
            tracing_subscriber::fmt::layer()
                .compact()
                .with_file(false)
                .with_target(false),
        )
        .init();
}

#[derive(Clone)]
pub struct AppState {
    pub state: ServerState,
    pub secret: AuthSecret,
    pub turn_secret: Option<String>,
    pub turn_urls: Option<Vec<String>>,
    pub stun_url: String,
}

impl FromRef<AppState> for AuthSecret {
    fn from_ref(input: &AppState) -> Self {
        input.secret.clone()
    }
}

pub async fn run(addr: SocketAddr) -> Result<(), Box<dyn std::error::Error>> {
    dotenvy::dotenv().ok();
    let secret = std::env::var("JWT_SECRET")
        .unwrap_or_else(|_| "test-secret-key-for-development-only".to_string());

    let turn_secret = std::env::var("TURN_SECRET").ok();
    let turn_urls = std::env::var("TURN_URLS")
        .ok()
        .map(|s| s.split(',').filter(|s| !s.is_empty()).map(|s| s.to_string()).collect());

    let stun_url = std::env::var("STUN_URL").unwrap_or_else(|_| DEFAULT_STUN_URL.to_string());

    let state = ServerState::default();
    let app_state = AppState {
        state: state.clone(),
        secret: AuthSecret(secret.clone()),
        turn_secret,
        turn_urls,
        stun_url,
    };
    let app_router = app(app_state);
    let cors = if let Ok(origins) = std::env::var("ALLOWED_ORIGINS") {
        let origins = origins.split(',').map(|origin|origin.trim().parse::<axum::http::HeaderValue>()).collect::<Result<Vec<_>,_>>()?;
        if origins.is_empty() || origins.iter().any(|o|o == "*") { return Err("Explicit ALLOWED_ORIGINS required".into()); }
        CorsLayer::new().allow_origin(AllowOrigin::list(origins))
            .allow_methods([axum::http::Method::GET,axum::http::Method::POST,axum::http::Method::DELETE,axum::http::Method::OPTIONS])
            .allow_headers([axum::http::header::AUTHORIZATION,axum::http::header::CONTENT_TYPE])
    } else { CorsLayer::very_permissive() };

    let challenge_manager = state.challenge_manager.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
        loop {
            interval.tick().await;
            challenge_manager.cleanup_expired();
        }
    });

    // Lobby inactivity cleanup task - runs every 60 seconds
    let lobby_cleanup_state = state.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
        loop {
            interval.tick().await;
            let stale = { let mut b = lobby_cleanup_state.browser.lock().unwrap(); b.prune(); std::mem::take(&mut b.closing) };
            for peer in stale { let _ = lobby_cleanup_state.try_send(peer, axum::extract::ws::Message::Close(None)); }
            let expired = {
                let mut lobby_manager = lobby_cleanup_state.lobby_manager.write().unwrap();
                lobby_manager.cleanup_inactive_lobbies()
            };
            // Remove players from players_in_lobbies mapping for cleaned up lobbies
            if !expired.is_empty() {
                let mut players_in_lobbies = lobby_cleanup_state.players_in_lobbies.write().unwrap();
                for (lobby_id, players) in &expired {
                    for player_id in players {
                        if let Some(lid) = players_in_lobbies.get(player_id) {
                            if lid == lobby_id {
                                players_in_lobbies.remove(player_id);
                            }
                        }
                    }
                }
                // Notify clients about lobby updates
                let _ = lobby_cleanup_state.lobby_updates.send(());
            }
        }
    });

    let server = SignalingServerBuilder::new(addr, MatchmakingDemoTopology, state.clone())
        .on_connection_request({
            let state = state.clone();
            let secret = AuthSecret(secret);
            move |connection| {
                tracing::info!(origin = ?connection.origin, "WebSocket connection attempt");
                // Extract token from path (allumette stores path without leading /)
                let token = connection
                    .path
                    .as_ref()
                    .map(|p| p.as_str())
                    .ok_or_else(|| {
                        tracing::warn!(origin = ?connection.origin, path = ?connection.path, "Missing token in path");
                        (StatusCode::UNAUTHORIZED, "Missing token in path").into_response()
                    })?;

                if let Ok(ticket) = browser::decode_ticket(token, &secret) {
                    state.browser.lock().unwrap().reserve(connection.origin, &ticket)
                        .map_err(IntoResponse::into_response)?;
                    return Ok(true);
                }
                let claims = decode::<auth::Claims>(
                    token,
                    &DecodingKey::from_secret(secret.0.as_ref()),
                    &Validation::default(),
                )
                .map_err(|e| {
                    tracing::warn!(origin = ?connection.origin, error = ?e, "Invalid token");
                    (StatusCode::UNAUTHORIZED, "Invalid token").into_response()
                })?
                .claims;

                tracing::info!(origin = ?connection.origin, pubkey = %&claims.sub[..8], "WebSocket connection request: player connected");

                if state.browser.lock().unwrap().contains_player(&claims.sub) {
                    return Err((StatusCode::FORBIDDEN, "Session ticket required").into_response());
                }
                let mut waiting_players = state.waiting_players.write().unwrap();
                waiting_players.insert(connection.origin, claims.sub);

                // Log current waiting_players size for debugging
                tracing::debug!(waiting_players_count = waiting_players.len(), "Current waiting_players map size");

                Ok(true)
            }
        })
        .on_id_assignment({
            let state = state.clone();
            move |(origin, peer_id)| {
                if state.browser.lock().unwrap().assign(origin, peer_id) { return; }
                let mut waiting_players = state.waiting_players.write().unwrap();
                if let Some(player_id) = waiting_players.remove(&origin) {
                    let mut players_to_peers = state.players_to_peers.write().unwrap();
                    players_to_peers.insert(player_id.clone(), peer_id);
                    tracing::info!(origin = ?origin, pubkey = %&player_id[..8], peer_id = ?peer_id, "Assigned peer_id to player");
                } else {
                    tracing::error!(origin = ?origin, "No player_id found in waiting_players during id assignment");
                }
            }
        })
        .mutate_router(|router| {
            // Merge app router and apply CORS to the *entire* router (including signaling routes).
            // This prevents duplicate CORS headers and ensures consistent behavior.
            // TODO: Restrict CORS for production environments
            router
                .merge(app_router)
                .layer(cors)
        })
        .build();

    info!("listening on {}", addr);
    server.serve().await?;
    Ok(())
}

fn app(state: AppState) -> Router {
    Router::new().merge(browser::router())
        .route("/health", get(health_handler))
        .route("/auth/challenge", post(challenge_handler))
        .route("/auth/login", post(login_handler))
        .route(
            "/lobbies",
            post(create_lobby_handler).get(list_lobbies_handler),
        )
        .route("/lobbies/stream", get(lobby_stream_handler))
        .route("/lobbies/{lobby_id}/join", post(join_lobby_handler))
        .route("/lobbies/{lobby_id}/start", post(start_lobby_handler))
        .route("/lobbies/{lobby_id}", delete(delete_lobby_handler))
        .route("/lobbies/{lobby_id}/invite", post(invite_to_lobby_handler))
        .route("/ice-servers", get(get_ice_servers_handler))
        .with_state(state)
}

pub async fn health_handler() -> impl IntoResponse {
    StatusCode::OK
}

/// Add no-cache headers to response to prevent caching of sensitive data
fn add_no_cache_headers(mut response: axum::response::Response) -> axum::response::Response {
    let headers = response.headers_mut();
    headers.insert("Cache-Control", "no-store, no-cache, must-revalidate, private".parse().unwrap());
    headers.insert("Pragma", "no-cache".parse().unwrap());
    headers.insert("Expires", "0".parse().unwrap());
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::Body,
        http::{Request, StatusCode},
        Router,
    };
    use tower::ServiceExt;

    // Helper to create a test app with mock state
    fn test_app() -> Router {
        let state = ServerState::default();
        let app_state = AppState {
            state,
            secret: AuthSecret("test-secret".to_string()),
            turn_secret: Some("test-turn-secret".to_string()),
            turn_urls: Some(vec!["turn:example.com".to_string()]),
            stun_url: DEFAULT_STUN_URL.to_string(),
        };
        app(app_state).layer(CorsLayer::very_permissive())
    }

    #[tokio::test]
    async fn test_ice_servers_unauthorized() {
        let app = test_app();

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/ice-servers")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_ice_servers_authorized() {
        let app = test_app();

        // Create a valid JWT
        let secret = AuthSecret("test-secret".to_string());
        let token = auth::issue_jwt(
            "test_pubkey".to_string(),
            "test_user".to_string(),
            &secret
        ).unwrap();

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/ice-servers")
                    .header("Authorization", format!("Bearer {}", token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let ice_servers: Vec<IceServer> = serde_json::from_slice(&body_bytes).unwrap();

        // Should have 2 entries: default STUN + configured TURN
        assert_eq!(ice_servers.len(), 2);

        // Check default STUN
        assert_eq!(ice_servers[0].urls[0], "stun:stun.l.google.com:19302");
        assert!(ice_servers[0].username.is_none());

        // Check TURN
        assert_eq!(ice_servers[1].urls[0], "turn:example.com");
        assert!(ice_servers[1].username.is_some());
        assert!(ice_servers[1].credential.is_some());
    }

    #[tokio::test]
    async fn test_ice_servers_stun_only() {
        let state = ServerState::default();
        let app_state = AppState {
            state,
            secret: AuthSecret("test-secret".to_string()),
            turn_secret: None,
            turn_urls: None,
            stun_url: DEFAULT_STUN_URL.to_string(),
        };
        let app = app(app_state);

        // Create a valid JWT
        let secret = AuthSecret("test-secret".to_string());
        let token = auth::issue_jwt(
            "test_pubkey".to_string(),
            "test_user".to_string(),
            &secret
        ).unwrap();

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/ice-servers")
                    .header("Authorization", format!("Bearer {}", token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let ice_servers: Vec<IceServer> = serde_json::from_slice(&body_bytes).unwrap();

        // Should have 1 entry: default STUN only
        assert_eq!(ice_servers.len(), 1);

        // Check default STUN
        assert_eq!(ice_servers[0].urls[0], "stun:stun.l.google.com:19302");
        assert!(ice_servers[0].username.is_none());
    }

    #[tokio::test]
    async fn test_ice_servers_cors() {
        let app = test_app();

        // Test CORS for OPTIONS request
        let response = app.clone()
            .oneshot(
                Request::builder()
                    .method("OPTIONS")
                    .uri("/ice-servers")
                    .header("Origin", "http://localhost:3000")
                    .header("Access-Control-Request-Method", "GET")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get("access-control-allow-origin").unwrap(),
            "http://localhost:3000"
        );

        // Test CORS for authorized GET request
        let secret = AuthSecret("test-secret".to_string());
        let token = auth::issue_jwt(
            "test_pubkey".to_string(),
            "test_user".to_string(),
            &secret
        ).unwrap();

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/ice-servers")
                    .header("Origin", "http://localhost:3000")
                    .header("Authorization", format!("Bearer {}", token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get("access-control-allow-origin").unwrap(),
            "http://localhost:3000"
        );
    }
}

#[derive(Serialize)]
struct ChallengeResponse {
    challenge: String,
}

async fn challenge_handler(State(state): State<AppState>) -> impl IntoResponse {
    let challenge = state.state.challenge_manager.generate_challenge();
    add_no_cache_headers(Json(ChallengeResponse { challenge }).into_response())
}

#[derive(Deserialize)]
pub struct LoginRequest {
    public_key_b64: String,
    username: String,
    challenge: String,
    signature_b64: String,
}

async fn login_handler(
    State(state): State<AppState>,
    Json(payload): Json<LoginRequest>,
) -> impl IntoResponse {
    tracing::info!(
        pubkey = %payload.public_key_b64,
        "Login attempt"
    );

    if !state
        .state
        .challenge_manager
        .verify_challenge(&payload.challenge)
    {
        tracing::warn!(pubkey = %payload.public_key_b64, "Challenge verification failed");
        return Err((StatusCode::UNAUTHORIZED, "Invalid challenge"));
    }

    let signature_valid = match auth::verify_signature(
        &payload.public_key_b64,
        &payload.challenge,
        &payload.signature_b64,
    ) {
        Ok(valid) => {
            tracing::debug!(pubkey = %payload.public_key_b64, signature_valid = valid, "Signature verification result");
            valid
        }
        Err(e) => {
            tracing::warn!(pubkey = %payload.public_key_b64, error = ?e, "Signature verification error");
            return Err((StatusCode::UNAUTHORIZED, "Invalid signature"));
        }
    };

    if !signature_valid {
        tracing::warn!(pubkey = %payload.public_key_b64, "Signature validation failed");
        return Err((StatusCode::UNAUTHORIZED, "Invalid signature"));
    }

    match auth::issue_jwt(
        payload.public_key_b64.clone(),
        payload.username.clone(),
        &state.secret,
    ) {
        Ok(token) => {
            tracing::info!(pubkey = %payload.public_key_b64, username = %payload.username, "Login successful");
            Ok(add_no_cache_headers(Json(json!({ "token": token })).into_response()))
        }
        Err(_) => {
            tracing::error!(pubkey = %payload.public_key_b64, "Failed to issue JWT");
            Err((StatusCode::INTERNAL_SERVER_ERROR, "Failed to issue token"))
        }
    }
}

#[derive(Serialize, Deserialize)]
pub struct IceServer {
    urls: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    credential: Option<String>,
}

async fn get_ice_servers_handler(
    State(state): State<AppState>,
    _claims: auth::Claims,
) -> impl IntoResponse {
    let mut ice_servers = vec![IceServer {
        urls: vec![state.stun_url.clone()],
        username: None,
        credential: None,
    }];

    if let (Some(turn_secret), Some(turn_urls)) = (&state.turn_secret, &state.turn_urls) {
        let expiration = chrono::Utc::now().timestamp() + TURN_CREDENTIAL_TTL;
        let username = format!("{}:{}", expiration, uuid::Uuid::new_v4());

        type HmacSha1 = Hmac<Sha1>;
        let mut mac = match HmacSha1::new_from_slice(turn_secret.as_bytes()) {
            Ok(mac) => mac,
            Err(e) => {
                tracing::error!("Failed to create HMAC: {}", e);
                return (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error").into_response();
            }
        };
        mac.update(username.as_bytes());
        let credential = BASE64.encode(mac.finalize().into_bytes());

        ice_servers.push(IceServer {
            urls: turn_urls.clone(),
            username: Some(username),
            credential: Some(credential),
        });
    }

    add_no_cache_headers(Json(ice_servers).into_response())
}

#[derive(Deserialize)]
pub struct CreateLobbyRequest {
    is_private: bool,
    game_id: String,
    #[serde(default)]
    whitelist: Option<Vec<String>>,
}

async fn create_lobby_handler(
    State(state): State<AppState>,
    claims: auth::Claims,
    Json(payload): Json<CreateLobbyRequest>,
) -> impl IntoResponse {
    if state.state.browser.lock().unwrap().contains_player(&claims.sub) {
        return add_no_cache_headers((StatusCode::CONFLICT, "Already in a browser lobby").into_response());
    }
    // Check if player is already in a lobby
    let players_in_lobbies = state.state.players_in_lobbies.read().unwrap();
    if let Some(existing_lobby_id) = players_in_lobbies.get(&claims.sub) {
        tracing::warn!(
            existing_lobby_id = %existing_lobby_id,
            pubkey = %&claims.sub[..8],
            "Player attempted to create lobby while already in one"
        );
        return add_no_cache_headers((StatusCode::CONFLICT, "Already in a lobby").into_response());
    }
    drop(players_in_lobbies);
    let mut lobby_manager = state.state.lobby_manager.write().unwrap();
    // Create lobby and ensure the owner is present atomically
    let lobby = lobby_manager.create_lobby_with_owner(payload.is_private, claims.sub.clone(), payload.game_id, payload.whitelist);
    let lobby_response = lobby.to_response(Some(&claims.sub));
    let mut players_in_lobbies = state.state.players_in_lobbies.write().unwrap();
    players_in_lobbies.insert(claims.sub.clone(), lobby.id);
    tracing::info!(lobby_id = %lobby.id, pubkey = %&claims.sub[..8], "Lobby created and player added");
    
    // Notify all SSE clients of lobby update
    let _ = state.state.lobby_updates.send(());
    
    add_no_cache_headers(Json(lobby_response).into_response())
}

async fn list_lobbies_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    // Try to extract bearer token from Authorization header and decode claims
    let player_pubkey = headers
        .get("authorization")
        .and_then(|hv| hv.to_str().ok())
        .and_then(|auth| auth.strip_prefix("Bearer "))
        .and_then(|token| {
            decode::<auth::Claims>(
                token,
                &DecodingKey::from_secret(state.secret.0.as_ref()),
                &Validation::default(),
            )
            .ok()
            .map(|data| data.claims.sub)
        });

    let lobby_manager = state.state.lobby_manager.read().unwrap();
    let lobbies = lobby_manager.get_lobbies_for_player(player_pubkey.clone());
    let sanitized_lobbies: Vec<_> = lobbies
        .iter()
        .map(|lobby| lobby.to_response(player_pubkey.as_ref()))
        .collect();
    add_no_cache_headers(Json(sanitized_lobbies).into_response())
}

async fn lobby_stream_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    // Try to extract bearer token from Authorization header and decode claims
    let player_pubkey = headers
        .get("authorization")
        .and_then(|hv| hv.to_str().ok())
        .and_then(|auth| auth.strip_prefix("Bearer "))
        .and_then(|token| {
            decode::<auth::Claims>(
                token,
                &DecodingKey::from_secret(state.secret.0.as_ref()),
                &Validation::default(),
            )
            .ok()
            .map(|data| data.claims.sub)
        });

    // Subscribe to lobby updates
    let rx = state.state.lobby_updates.subscribe();
    let lobby_manager = state.state.lobby_manager.clone();

    // Create a stream that listens for updates and transforms them to SSE events
    let stream = BroadcastStream::new(rx)
        .then(move |result| {
            let lobby_manager = lobby_manager.clone();
            let player_pubkey = player_pubkey.clone();
            
            async move {
                // Ignore broadcast errors (lagged messages)
                if result.is_err() {
                    return None;
                }
                
                // Get current lobbies for this player and sanitize them
                let sanitized_lobbies = {
                    let manager = lobby_manager.read().unwrap();
                    let lobbies = manager.get_lobbies_for_player(player_pubkey.clone());
                    lobbies
                        .iter()
                        .map(|lobby| lobby.to_response(player_pubkey.as_ref()))
                        .collect::<Vec<_>>()
                };
                
                // Serialize sanitized lobbies to JSON
                match serde_json::to_string(&sanitized_lobbies) {
                    Ok(json) => Some(Ok::<Event, Infallible>(Event::default().data(json))),
                    Err(e) => {
                        tracing::error!("Failed to serialize lobbies: {}", e);
                        None
                    }
                }
            }
        })
        .filter_map(|x| x);

    let sse = Sse::new(stream).keep_alive(KeepAlive::default());
    
    // Add no-cache headers to SSE response
    let mut response = sse.into_response();
    let headers = response.headers_mut();
    headers.insert("Cache-Control", "no-store, no-cache, must-revalidate, private".parse().unwrap());
    headers.insert("Pragma", "no-cache".parse().unwrap());
    headers.insert("Expires", "0".parse().unwrap());
    response
}

async fn join_lobby_handler(
    State(state): State<AppState>,
    Path(lobby_id): Path<uuid::Uuid>,
    claims: auth::Claims,
) -> impl IntoResponse {
    if state.state.browser.lock().unwrap().contains_player(&claims.sub) {
        return add_no_cache_headers((StatusCode::CONFLICT, "Already in a browser lobby").into_response());
    }
    // Check if player is already in a lobby
    let players_in_lobbies = state.state.players_in_lobbies.read().unwrap();
    if let Some(existing_lobby_id) = players_in_lobbies.get(&claims.sub) {
        // Allow rejoining the same lobby (idempotent operation)
        if *existing_lobby_id == lobby_id {
            tracing::debug!(lobby_id = %lobby_id, pubkey = %&claims.sub[..8], "Player already in this lobby");
            return add_no_cache_headers(StatusCode::OK.into_response());
        }
        tracing::warn!(
            existing_lobby_id = %existing_lobby_id,
            attempted_lobby_id = %lobby_id,
            pubkey = %&claims.sub[..8],
            "Player attempted to join lobby while already in another"
        );
        return add_no_cache_headers((StatusCode::CONFLICT, "Already in a lobby").into_response());
    }
    drop(players_in_lobbies);

    let mut lobby_manager = state.state.lobby_manager.write().unwrap();
    let result = lobby_manager.add_player_to_lobby(&lobby_id, claims.sub.clone());

    if result.is_err() {
        // Determine reason: lobby missing, not whitelisted, or lobby already started
        let lobby_opt = lobby_manager.get_lobby(&lobby_id);
        if let Some(lobby) = lobby_opt {
            // If lobby exists but is not in Waiting state, it's already in progress
            if lobby.status != crate::lobby::LobbyStatus::Waiting {
                tracing::warn!(lobby_id = %lobby_id, pubkey = %&claims.sub[..8], "Player attempted to join lobby already in progress");
                return add_no_cache_headers((StatusCode::CONFLICT, "Game already started").into_response());
            }

            // Check whitelist rejection
            if let Some(whitelist) = &lobby.whitelist {
                if !whitelist.contains(&claims.sub) {
                    tracing::warn!(lobby_id = %lobby_id, pubkey = %&claims.sub[..8], "Player not in whitelist");
                    return add_no_cache_headers((StatusCode::FORBIDDEN, "Not in whitelist").into_response());
                }
            }

            // If we get here, the add failed for an unknown reason — return not found as fallback
            tracing::warn!(lobby_id = %lobby_id, pubkey = %&claims.sub[..8], "Player failed to join lobby: unknown error");
            return add_no_cache_headers((StatusCode::NOT_FOUND, "Lobby not found").into_response());
        } else {
            tracing::warn!(lobby_id = %lobby_id, pubkey = %&claims.sub[..8], "Player failed to join lobby: not found");
            return add_no_cache_headers((StatusCode::NOT_FOUND, "Lobby not found").into_response());
        }
    }

    // Insert the joining player's public key into players_in_lobbies
    let mut players_in_lobbies = state.state.players_in_lobbies.write().unwrap();
    players_in_lobbies.insert(claims.sub.clone(), lobby_id);
    tracing::debug!(full_pubkey = %claims.sub, "Full public key for join");
    tracing::debug!(players_in_lobbies = ?*players_in_lobbies, "Current players_in_lobbies map");
    tracing::info!(lobby_id = %lobby_id, pubkey = %&claims.sub[..8], "Player joined lobby");
    
    // Notify all SSE clients of lobby update
    let _ = state.state.lobby_updates.send(());

    add_no_cache_headers(StatusCode::OK.into_response())
}

/// Start a lobby (mark as InProgress). Only the owner can start the lobby.
/// This is useful when the game starts without WebSocket signaling (e.g., single player).
async fn start_lobby_handler(
    State(state): State<AppState>,
    Path(lobby_id): Path<uuid::Uuid>,
    claims: auth::Claims,
) -> impl IntoResponse {
    let mut lobby_manager = state.state.lobby_manager.write().unwrap();

    // Check if lobby exists
    let lobby = match lobby_manager.get_lobby(&lobby_id) {
        Some(l) => l,
        None => {
            tracing::warn!(lobby_id = %lobby_id, pubkey = %&claims.sub[..8], "Attempted to start non-existent lobby");
            return add_no_cache_headers((StatusCode::NOT_FOUND, "Lobby not found").into_response());
        }
    };

    // Check if requester is the owner
    if lobby.owner != claims.sub {
        tracing::warn!(lobby_id = %lobby_id, pubkey = %&claims.sub[..8], "Non-owner attempted to start lobby");
        return add_no_cache_headers((StatusCode::FORBIDDEN, "Only the owner can start the lobby").into_response());
    }

    // Check if already in progress (idempotent)
    if lobby.status == crate::lobby::LobbyStatus::InProgress {
        tracing::debug!(lobby_id = %lobby_id, "Lobby already in progress");
        return add_no_cache_headers(StatusCode::OK.into_response());
    }

    // Start the lobby
    if let Err(e) = lobby_manager.start_lobby(&lobby_id, &claims.sub) {
        tracing::error!(lobby_id = %lobby_id, error = ?e, "Failed to start lobby");
        return add_no_cache_headers((StatusCode::INTERNAL_SERVER_ERROR, "Failed to start lobby").into_response());
    }

    tracing::info!(lobby_id = %lobby_id, pubkey = %&claims.sub[..8], "Lobby started via API");

    // Notify all SSE clients of lobby update
    let _ = state.state.lobby_updates.send(());

    add_no_cache_headers(StatusCode::OK.into_response())
}

async fn delete_lobby_handler(
    State(state): State<AppState>,
    Path(lobby_id): Path<uuid::Uuid>,
    claims: auth::Claims,
) -> impl IntoResponse {
    let mut lobby_manager = state.state.lobby_manager.write().unwrap();
    
    // Check if lobby exists
    let lobby = lobby_manager.get_lobby(&lobby_id);
    let lobby = match lobby {
        Some(l) => l,
        None => {
            tracing::warn!(lobby_id = %lobby_id, pubkey = %&claims.sub[..8], "Attempted to delete/leave non-existent lobby");
            return add_no_cache_headers((StatusCode::NOT_FOUND, "Lobby not found").into_response());
        }
    };
    
    let is_owner = lobby.owner == claims.sub;
    
    if is_owner {
        // Owner is deleting the lobby - remove it completely
        match lobby_manager.delete_lobby(&lobby_id) {
            Ok(_) => {
                // Remove all players from players_in_lobbies that were in this lobby
                let mut players_in_lobbies = state.state.players_in_lobbies.write().unwrap();
                players_in_lobbies.retain(|_, lid| *lid != lobby_id);
                
                tracing::info!(lobby_id = %lobby_id, pubkey = %&claims.sub[..8], "Lobby deleted by owner");
                
                // Notify all SSE clients of lobby update
                let _ = state.state.lobby_updates.send(());
                
                // TODO: Close all WebSocket connections for players in this lobby
                // This would require tracking peer connections by lobby
                
                add_no_cache_headers(StatusCode::OK.into_response())
            }
            Err(_) => {
                tracing::error!(lobby_id = %lobby_id, pubkey = %&claims.sub[..8], "Failed to delete lobby");
                add_no_cache_headers((StatusCode::INTERNAL_SERVER_ERROR, "Failed to delete lobby").into_response())
            }
        }
    } else {
        // Non-owner is leaving the lobby - just remove them
        lobby_manager.remove_player_from_lobby(&lobby_id, &claims.sub);
        
        // Remove player from players_in_lobbies
        let mut players_in_lobbies = state.state.players_in_lobbies.write().unwrap();
        players_in_lobbies.remove(&claims.sub);
        
        tracing::info!(lobby_id = %lobby_id, pubkey = %&claims.sub[..8], "Player left lobby");
        
        // Notify all SSE clients of lobby update
        let _ = state.state.lobby_updates.send(());
        
        add_no_cache_headers(StatusCode::OK.into_response())
    }
}

#[derive(Deserialize)]
pub struct InviteToLobbyRequest {
    player_public_keys: Vec<String>,
}

async fn invite_to_lobby_handler(
    State(state): State<AppState>,
    Path(lobby_id): Path<uuid::Uuid>,
    claims: auth::Claims,
    Json(payload): Json<InviteToLobbyRequest>,
) -> impl IntoResponse {
    let mut lobby_manager = state.state.lobby_manager.write().unwrap();
    
    // Check if lobby exists
    let lobby = lobby_manager.get_lobby(&lobby_id);
    let lobby = match lobby {
        Some(l) => l,
        None => {
            tracing::warn!(lobby_id = %lobby_id, pubkey = %&claims.sub[..8], "Attempted to invite to non-existent lobby");
            return add_no_cache_headers((StatusCode::NOT_FOUND, "Lobby not found").into_response());
        }
    };
    
    // Only owner can invite
    if lobby.owner != claims.sub {
        tracing::warn!(lobby_id = %lobby_id, pubkey = %&claims.sub[..8], "Non-owner attempted to invite players");
        return add_no_cache_headers((StatusCode::FORBIDDEN, "Only lobby owner can invite players").into_response());
    }
    
    // Add players to whitelist (but don't return the list of invited players to prevent leaking public keys)
    match lobby_manager.add_to_whitelist(&lobby_id, payload.player_public_keys.clone()) {
        Ok(_) => {
            tracing::info!(
                lobby_id = %lobby_id,
                pubkey = %&claims.sub[..8],
                invited_count = payload.player_public_keys.len(),
                "Players invited to lobby"
            );
            
            // Notify all SSE clients of lobby update
            let _ = state.state.lobby_updates.send(());
            
            add_no_cache_headers(Json(json!({ "success": true, "invited_count": payload.player_public_keys.len() })).into_response())
        }
        Err(_) => {
            tracing::error!(lobby_id = %lobby_id, pubkey = %&claims.sub[..8], "Failed to invite players");
            add_no_cache_headers((StatusCode::INTERNAL_SERVER_ERROR, "Failed to invite players").into_response())
        }
    }
}
