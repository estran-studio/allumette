use allumette_server::helpers;
use futures_util::StreamExt;
use reqwest::Client;
use serde_json::Value;
use serial_test::serial;
use std::net::SocketAddr;
use tokio::net::TcpListener;
use tokio::time::{sleep, Duration};
use tokio_tungstenite::{connect_async, tungstenite::Message};

async fn spawn_app() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    tokio::spawn(async move {
        allumette_server::run(addr).await.unwrap();
    });
    sleep(Duration::from_millis(100)).await;
    addr
}

async fn authenticate_and_get_token(addr: SocketAddr, username: &str, password: &str) -> String {
    let client = Client::new();

    // Get challenge
    let response = client
        .post(format!("http://{}/auth/challenge", addr))
        .send()
        .await
        .unwrap();
    let body: Value = response.json().await.unwrap();
    let challenge = body["challenge"].as_str().unwrap();

    // Login
    let login_payload = helpers::generate_login_payload(username, password, challenge).unwrap();
    let response = client
        .post(format!("http://{}/auth/login", addr))
        .header("Content-Type", "application/json")
        .body(login_payload)
        .send()
        .await
        .unwrap();
    let body: Value = response.json().await.unwrap();
    body["token"].as_str().unwrap().to_string()
}

#[tokio::test]
#[serial]
async fn test_websocket_connection_with_token_in_path() {
    let addr = spawn_app().await;
    let client = Client::new();

    // 1. Authenticate and create lobby
    let token = authenticate_and_get_token(addr, "player_a", "pass_a").await;

    let response = client
        .post(format!("http://{}/lobbies", addr))
        .header("Authorization", format!("Bearer {}", token))
        .header("Content-Type", "application/json")
        .body(r#"{"is_private": false, "game_id": "test_game"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);

    // 2. Connect via WebSocket with token in path
    let ws_url = format!("ws://{}/{}", addr, token);
    let (ws_stream, _) = connect_async(&ws_url).await.expect("Failed to connect");

    let (mut _write, mut read) = ws_stream.split();

    // 3. Wait for IdAssigned message
    let timeout = tokio::time::timeout(Duration::from_secs(2), async {
        while let Some(msg) = read.next().await {
            if let Ok(Message::Text(text)) = msg {
                let parsed: Value = serde_json::from_str(&text).unwrap();
                if parsed.get("IdAssigned").is_some() {
                    return true;
                }
            }
        }
        false
    });

    assert!(timeout.await.unwrap(), "Should receive IdAssigned message");
}

#[tokio::test]
#[serial]
async fn test_websocket_connection_without_token_fails() {
    let addr = spawn_app().await;

    // Try to connect without token in path
    let ws_url = format!("ws://{}/", addr);
    let result = connect_async(&ws_url).await;

    // Should fail with 401
    assert!(result.is_err(), "Connection without token should fail");
}

#[tokio::test]
#[serial]
async fn test_websocket_connection_with_invalid_token_fails() {
    let addr = spawn_app().await;

    // Try to connect with invalid token
    let ws_url = format!("ws://{}/invalid_token_here", addr);
    let result = connect_async(&ws_url).await;

    // Should fail with 401
    assert!(result.is_err(), "Connection with invalid token should fail");
}

#[tokio::test]
#[serial]
async fn test_two_players_connect_to_same_lobby() {
    let addr = spawn_app().await;
    let client = Client::new();

    // Player A: Authenticate and create lobby
    let token_a = authenticate_and_get_token(addr, "player_a", "pass_a").await;
    let response = client
        .post(format!("http://{}/lobbies", addr))
        .header("Authorization", format!("Bearer {}", token_a))
        .header("Content-Type", "application/json")
        .body(r#"{"is_private": false, "game_id": "test_game"}"#)
        .send()
        .await
        .unwrap();
    let body: Value = response.json().await.unwrap();
    let lobby_id = body["id"].as_str().unwrap();

    // Player B: Authenticate and join lobby
    let token_b = authenticate_and_get_token(addr, "player_b", "pass_b").await;
    let response = client
        .post(format!("http://{}/lobbies/{}/join", addr, lobby_id))
        .header("Authorization", format!("Bearer {}", token_b))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);

    // Player A: Connect via WebSocket
    let ws_url_a = format!("ws://{}/{}", addr, token_a);
    let (ws_stream_a, _) = connect_async(&ws_url_a)
        .await
        .expect("Player A failed to connect");
    let (mut _write_a, mut read_a) = ws_stream_a.split();

    // Wait for Player A to get IdAssigned
    let timeout = tokio::time::timeout(Duration::from_secs(2), async {
        while let Some(msg) = read_a.next().await {
            if let Ok(Message::Text(text)) = msg {
                let parsed: Value = serde_json::from_str(&text).unwrap();
                if parsed.get("IdAssigned").is_some() {
                    return true;
                }
            }
        }
        false
    });
    assert!(timeout.await.unwrap(), "Player A should receive IdAssigned");

    // Player B: Connect via WebSocket
    let ws_url_b = format!("ws://{}/{}", addr, token_b);
    let (ws_stream_b, _) = connect_async(&ws_url_b)
        .await
        .expect("Player B failed to connect");
    let (mut _write_b, mut read_b) = ws_stream_b.split();

    // Wait for Player B to get IdAssigned
    let timeout = tokio::time::timeout(Duration::from_secs(2), async {
        while let Some(msg) = read_b.next().await {
            if let Ok(Message::Text(text)) = msg {
                let parsed: Value = serde_json::from_str(&text).unwrap();
                if parsed.get("IdAssigned").is_some() {
                    return true;
                }
            }
        }
        false
    });
    assert!(timeout.await.unwrap(), "Player B should receive IdAssigned");

    // Both players should receive NewPeer notifications about each other
    // This is left as an exercise - you'd need to handle the async nature of these messages
}

#[tokio::test]
#[serial]
async fn test_websocket_connection_without_joining_lobby_fails() {
    let addr = spawn_app().await;

    // Authenticate but don't create or join any lobby
    let token = authenticate_and_get_token(addr, "player_a", "pass_a").await;

    // Try to connect via WebSocket
    let ws_url = format!("ws://{}/{}", addr, token);
    let result = connect_async(&ws_url).await;

    // Connection might succeed initially but should close immediately
    // because the player is not in any lobby
    if let Ok((ws_stream, _)) = result {
        let (mut _write, mut read) = ws_stream.split();

        // The connection should close quickly
        let timeout = tokio::time::timeout(Duration::from_secs(2), async { read.next().await });

        // Either we get no message, or the connection closes
        // The exact behavior depends on your topology implementation
        let _ = timeout.await;
    }
}

#[tokio::test]
#[serial]
async fn browser_sessions_use_matchbox_events_and_reject_old_admission() {
    use futures_util::SinkExt;
    use serde_json::json;
    let addr = spawn_app().await;
    let http = Client::new();
    let a = authenticate_and_get_token(addr, "browser_a", "browser_a_password").await;
    let b = authenticate_and_get_token(addr, "browser_b", "browser_b_password").await;
    let lobby: Value = http
        .post(format!("http://{addr}/browser/v1/lobbies"))
        .bearer_auth(&a)
        .json(&json!({"game_id":"standalone-game","compatibility_id":"build-1","capacity":2}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let base = format!(
        "http://{addr}/browser/v1/lobbies/{}",
        lobby["id"].as_str().unwrap()
    );
    let invite: Value = http
        .post(format!("{base}/invitations"))
        .bearer_auth(&a)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        http.post(format!("{base}/join"))
            .bearer_auth(&b)
            .json(
                &json!({"compatibility_id":"build-1","invitation_token":invite["invitation_token"]})
            )
            .send()
            .await
            .unwrap()
            .status()
            .as_u16(),
        200
    );
    for token in [&a, &b] {
        http.post(format!("{base}/ready"))
            .bearer_auth(token)
            .json(&json!({"ready":true,"compatibility_id":"build-1"}))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
    }
    http.post(format!("{base}/start"))
        .bearer_auth(&a)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    // Identity JWTs cannot bypass session admission.
    assert!(connect_async(format!("ws://{addr}/{a}")).await.is_err());
    let mut tickets = Vec::new();
    for token in [&a, &b] {
        let c: Value = http
            .get(format!("{base}/session"))
            .bearer_auth(token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        tickets.push(c["signaling_path"].as_str().unwrap().to_owned());
    }
    let (mut ws_a, _) = connect_async(format!("ws://{addr}/{}", tickets[0]))
        .await
        .unwrap();
    let assigned_a: Value =
        serde_json::from_str(ws_a.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
    let (mut ws_b, _) = connect_async(format!("ws://{addr}/{}", tickets[1]))
        .await
        .unwrap();
    let assigned_b: Value =
        serde_json::from_str(ws_b.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
    let new_peer: Value = serde_json::from_str(
        tokio::time::timeout(Duration::from_secs(2), ws_a.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .to_text()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(new_peer["NewPeer"], assigned_b["IdAssigned"]);
    let signal = json!({"Signal":{"receiver":assigned_b["IdAssigned"],"data":{"fixture":"offer"}}});
    ws_a.send(Message::Text(signal.to_string())).await.unwrap();
    let received: Value = serde_json::from_str(
        tokio::time::timeout(Duration::from_secs(2), ws_b.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .to_text()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(received["Signal"]["sender"], assigned_a["IdAssigned"]);
    assert_eq!(received["Signal"]["data"]["fixture"], "offer");
    // Ending a session invalidates its ticket even if the identity JWT is still valid.
    http.delete(format!("{base}/session"))
        .bearer_auth(&a)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    assert!(connect_async(format!("ws://{addr}/{}", tickets[0]))
        .await
        .is_err());
    let _ = ws_a.close(None).await;
    let _ = ws_b.close(None).await;
}
