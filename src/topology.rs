use crate::state::{Peer, ServerState};
use async_trait::async_trait;
use axum::extract::ws::Message;
use futures::StreamExt;
use matchbox_protocol::{JsonPeerEvent, PeerRequest};
use matchbox_signaling::{
    common_logic::parse_request, ClientRequestError, NoCallbacks, SignalingTopology, WsStateMeta,
};
use tracing::{error, info, warn};

#[derive(Debug, Default)]
pub struct MatchmakingDemoTopology;

#[async_trait]
impl SignalingTopology<NoCallbacks, ServerState> for MatchmakingDemoTopology {
    async fn state_machine(upgrade: WsStateMeta<NoCallbacks, ServerState>) {
        let WsStateMeta {
            peer_id,
            sender,
            mut receiver,
            mut state,
            ..
        } = upgrade;

        let browser_peer = state
            .browser
            .lock()
            .unwrap()
            .connections
            .contains_key(&peer_id);
        if browser_peer {
            browser_state_machine(peer_id, sender, receiver, state).await;
            return;
        }

        let player_id = {
            let players_to_peers = state.players_to_peers.read().unwrap();
            tracing::debug!(peer_id = ?peer_id, players_to_peers = ?*players_to_peers, "Looking up player_id for peer_id");
            players_to_peers
                .iter()
                .find(|(_, p)| **p == peer_id)
                .map(|(player_id, _)| player_id.clone())
        };

        let player_id = match player_id {
            Some(id) => {
                tracing::info!(peer_id = ?peer_id, player_id = %&id[..8], "Found player_id for peer");
                id
            }
            None => {
                error!(peer_id = ?peer_id, "No player id found for peer, somehow");
                return;
            }
        };

        let lobby_id = {
            let players_in_lobbies = state.players_in_lobbies.read().unwrap();
            tracing::debug!(player_id = %&player_id[..8], players_in_lobbies = ?*players_in_lobbies, "Looking up lobby_id for player");
            players_in_lobbies.get(&player_id).cloned()
        };

        let lobby_id = match lobby_id {
            Some(id) => {
                tracing::info!(player_id = %&player_id[..8], lobby_id = %id, "Found lobby for player");
                id
            }
            None => {
                error!(player_id = %&player_id[..8], "No lobby id found for peer, somehow");
                return;
            }
        };

        // If this connecting player is the lobby owner, mark the lobby as InProgress so no new joins are allowed.
        // We use the read lock to peek at the lobby and then a write lock to perform the transition.
        let maybe_lobby = {
            let lobby_manager = state.lobby_manager.read().unwrap();
            lobby_manager.get_lobby(&lobby_id)
        };
        if let Some(lobby) = maybe_lobby {
            if lobby.owner == player_id {
                // Owner is connecting — start the lobby
                let mut lobby_manager = state.lobby_manager.write().unwrap();
                if let Err(e) = lobby_manager.start_lobby(&lobby_id, &player_id) {
                    warn!(peer = ?peer_id, error = ?e, "Failed to start lobby on owner connect");
                } else {
                    info!(peer = ?peer_id, lobby_id = %lobby_id, "Owner connected — lobby marked InProgress");
                    // Notify SSE clients about the status change
                    let _ = state.lobby_updates.send(());
                }
            }
        }

        let peer = Peer {
            id: peer_id,
            sender: sender.clone(),
        };
        state.add_peer(peer);

        let players = {
            let lobby_manager = state.lobby_manager.read().unwrap();
            lobby_manager.get_lobby(&lobby_id).map(|l| l.players)
        };

        if let Some(players) = players {
            let event = Message::Text(JsonPeerEvent::NewPeer(peer_id).to_string().into());
            for player_id_str in players {
                if player_id_str != player_id {
                    let players_to_peers = state.players_to_peers.read().unwrap();
                    if let Some(peer_id) = players_to_peers.get(&player_id_str) {
                        if let Err(e) = state.try_send(*peer_id, event.clone()) {
                            error!("error sending to {peer_id:?}: {e:?}");
                        }
                    }
                }
            }
        }

        while let Some(request) = receiver.next().await {
            let request = match parse_request(request) {
                Ok(request) => request,
                Err(e) => {
                    match e {
                        ClientRequestError::Axum(_) => {
                            warn!("Unrecoverable error with {peer_id:?}: {e:?}");
                            break;
                        }
                        ClientRequestError::Close => {
                            info!("Connection closed by {peer_id:?}");
                            break;
                        }
                        ClientRequestError::Json(_) | ClientRequestError::UnsupportedType(_) => {
                            error!("Error with request: {:?}", e);
                            continue;
                        }
                    };
                }
            };

            match request {
                PeerRequest::Signal { receiver, data } => {
                    if !state.may_signal(peer_id, receiver) {
                        warn!(sender = ?peer_id, receiver = ?receiver, "Rejected signal outside lobby");
                        continue;
                    }
                    let event = Message::Text(
                        JsonPeerEvent::Signal {
                            sender: peer_id,
                            data,
                        }
                        .to_string()
                        .into(),
                    );
                    if let Err(e) = state.try_send(receiver, event) {
                        error!("error sending to {receiver:?}: {e:?}");
                    }
                }
                PeerRequest::KeepAlive => {}
            }
        }

        info!("Removing connection for peer: {:?}", peer_id);
        // Remove only the peer/connection and players_to_peers mapping, but KEEP the player's
        // membership in the lobby so they can be re-used in the next game.
        state.remove_connection_only(&peer_id, &player_id);

        // Check if any players in this lobby still have active connections (players_to_peers)
        let any_connected = {
            let players_to_peers = state.players_to_peers.read().unwrap();
            let lobby_players = {
                let lobby_manager = state.lobby_manager.read().unwrap();
                lobby_manager
                    .get_lobby(&lobby_id)
                    .map(|l| l.players)
                    .unwrap_or_default()
            };
            lobby_players
                .iter()
                .any(|p| players_to_peers.contains_key(p))
        };

        if !any_connected {
            // No players connected anymore — end the game and return lobby to Waiting state
            let mut lobby_manager = state.lobby_manager.write().unwrap();
            if let Err(e) = lobby_manager.end_lobby(&lobby_id) {
                warn!(peer = ?peer_id, error = ?e, "Failed to end lobby when last player disconnected");
            } else {
                info!(lobby_id = %lobby_id, "All players disconnected — lobby returned to Waiting");
                // Notify SSE clients about the status change
                let _ = state.lobby_updates.send(());
            }
        }

        let players = {
            let lobby_manager = state.lobby_manager.read().unwrap();
            lobby_manager.get_lobby(&lobby_id).map(|l| l.players)
        };

        if let Some(players) = players {
            let event = Message::Text(JsonPeerEvent::PeerLeft(peer_id).to_string().into());
            for player_id_str in players {
                if player_id_str != player_id {
                    let players_to_peers = state.players_to_peers.read().unwrap();
                    if let Some(peer_id) = players_to_peers.get(&player_id_str) {
                        if let Err(e) = state.try_send(*peer_id, event.clone()) {
                            error!("error sending to {peer_id:?}: {e:?}");
                        }
                    }
                }
            }
        }
    }
}

/// Browser sessions use the same Matchbox wire protocol with a separate admission contract.
async fn browser_state_machine(
    peer_id: matchbox_protocol::PeerId,
    sender: tokio::sync::mpsc::UnboundedSender<Result<Message, axum::Error>>,
    mut receiver: futures::stream::SplitStream<axum::extract::ws::WebSocket>,
    mut state: ServerState,
) {
    // Serialize peer registration and notifications so simultaneous handshakes cannot
    // miss or duplicate NewPeer events. Existing peers initiate the WebRTC offers.
    {
        let b = state.browser.lock().unwrap();
        let Some(connection) = b.connections.get(&peer_id) else {
            return;
        };
        let existing: Vec<_> = b
            .connections
            .iter()
            .filter(|(p, c)| {
                **p != peer_id
                    && c.lobby == connection.lobby
                    && c.session == connection.session
                    && state.get_peer(p).is_some()
            })
            .map(|(p, _)| *p)
            .collect();
        state.peers.lock().unwrap().insert(
            peer_id,
            Peer {
                id: peer_id,
                sender: sender.clone(),
            },
        );
        let event = Message::Text(JsonPeerEvent::NewPeer(peer_id).to_string().into());
        for p in existing {
            let _ = state.try_send(p, event.clone());
        }
    }
    while let Some(request) = receiver.next().await {
        let request = match parse_request(request) {
            Ok(r) => r,
            Err(ClientRequestError::Close | ClientRequestError::Axum(_)) => break,
            Err(_) => continue,
        };
        if let PeerRequest::Signal { receiver, data } = request {
            if state
                .browser
                .lock()
                .unwrap()
                .same_session(peer_id, receiver)
            {
                let _ = state.try_send(
                    receiver,
                    Message::Text(
                        JsonPeerEvent::Signal {
                            sender: peer_id,
                            data,
                        }
                        .to_string()
                        .into(),
                    ),
                );
            }
        }
    }
    let others = state.browser.lock().unwrap().disconnect(peer_id);
    state.remove_peer(&peer_id);
    let event = Message::Text(JsonPeerEvent::PeerLeft(peer_id).to_string().into());
    for p in others {
        let _ = state.try_send(p, event.clone());
    }
}
