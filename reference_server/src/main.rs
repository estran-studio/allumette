use axum::{routing::get, Router, Json, response::IntoResponse};
use std::net::SocketAddr;
use serde::Serialize;
use tracing::{info, error};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::accept_async;
use futures_util::StreamExt;

#[derive(Serialize)]
struct Status {
    status: String,
    game: String,
    authorized_players: Vec<String>,
}

async fn status_handler() -> impl IntoResponse {
    let auth_players = std::env::var("AUTHORIZED_PLAYERS").unwrap_or_default();
    let players: Vec<String> = auth_players.split(',').filter(|s| !s.is_empty()).map(|s| s.to_string()).collect();
    
    info!(player_count = players.len(), "Status check received");
    Json(Status {
        status: "ready".to_string(),
        game: "reference".to_string(),
        authorized_players: players,
    })
}

async fn handle_game_connection(stream: TcpStream, addr: SocketAddr) {
    // Peek at the first few bytes to see if it's an HTTP GET (WebSocket)
    let mut buf = [0u8; 4];
    match stream.peek(&mut buf).await {
        Ok(_) => {
            if &buf == b"GET " {
                info!(%addr, "New WebSocket connection detected");
                match accept_async(stream).await {
                    Ok(mut ws_stream) => {
                        info!(%addr, "WebSocket handshake successful");
                        while let Some(msg) = ws_stream.next().await {
                            match msg {
                                Ok(m) => info!(%addr, "Received WS message: {:?}", m),
                                Err(e) => { error!(%addr, "WS error: {}", e); break; }
                            }
                        }
                    }
                    Err(e) => error!(%addr, "WebSocket handshake failed: {}", e),
                }
            } else {
                info!(%addr, "New Raw TCP connection detected");
                let mut stream = stream;
                let mut buf = [0u8; 1024];
                loop {
                    match tokio::io::AsyncReadExt::read(&mut stream, &mut buf).await {
                        Ok(0) => { info!(%addr, "TCP connection closed"); break; }
                        Ok(n) => info!(%addr, "Received {} bytes via TCP: {:?}", n, &buf[..n]),
                        Err(e) => { error!(%addr, "TCP error: {}", e); break; }
                    }
                }
            }
        }
        Err(e) => error!(%addr, "Failed to peek stream: {}", e),
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let api_port = std::env::var("API_PORT")
        .unwrap_or_else(|_| "8080".to_string())
        .parse::<u16>()
        .expect("Invalid API_PORT");
    
    let game_port = std::env::var("GAME_PORT")
        .unwrap_or_else(|_| "9001".to_string())
        .parse::<u16>()
        .expect("Invalid GAME_PORT");

    // Spawn the GAME_PORT listener
    tokio::spawn(async move {
        let addr = SocketAddr::from(([0, 0, 0, 0], game_port));
        let listener = TcpListener::bind(addr).await.expect("Failed to bind game port");
        info!(port = %game_port, "Game listener (TCP/WS) active");

        loop {
            if let Ok((stream, addr)) = listener.accept().await {
                tokio::spawn(handle_game_connection(stream, addr));
            }
        }
    });

    let app = Router::new().route("/status", get(status_handler));

    let addr = SocketAddr::from(([0, 0, 0, 0], api_port));
    info!(api_port = %api_port, game_port = %game_port, "Reference game server starting");

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
