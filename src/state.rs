impl ServerState {
    /// Remove a player from all server state (peers, players_in_lobbies, players_to_peers, and all lobbies)
    pub fn remove_player(&self, player_id: &str) {
        // Remove from peers (by PeerId)
        if let Some(peer_id) = self.players_to_peers.write().unwrap().remove(player_id) {
            self.peers.lock().unwrap().remove(&peer_id);
        }
        // Remove from players_in_lobbies
        let lobby_id_opt = self.players_in_lobbies.write().unwrap().remove(player_id);
        // Remove from all lobbies
        if let Some(lobby_id) = lobby_id_opt {
            if let Ok(mut lobby_manager) = self.lobby_manager.try_write() {
                lobby_manager.remove_player_from_lobby(&lobby_id, &player_id.to_string());
            }
        } else {
            // Remove from any lobby where present
            if let Ok(mut lobby_manager) = self.lobby_manager.try_write() {
                for lobby in lobby_manager.lobbies.values_mut() {
                    lobby.players.remove(player_id);
                }
            }
        }
    }
}
use crate::auth::ChallengeManager;
use crate::lobby::Lobby;
use axum::{extract::ws::Message, Error};
use matchbox_protocol::PeerId;
use matchbox_signaling::{
    common_logic::{self, StateObj},
    SignalingError, SignalingState,
};
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{Arc, RwLock},
};
use tokio::sync::{broadcast, mpsc::UnboundedSender};
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct Peer {
    pub id: PeerId,
    pub sender: UnboundedSender<Result<Message, Error>>,
}

#[derive(Default, Debug, Clone)]
pub struct LobbyManager {
    lobbies: HashMap<Uuid, Lobby>,
}

impl LobbyManager {
    pub fn new() -> Self {
        Default::default()
    }

    /// Create a lobby and add an initial owner/creator into the players set atomically.
    pub fn create_lobby_with_owner(
        &mut self,
        is_private: bool,
        owner: String,
        game_id: String,
        whitelist: Option<Vec<String>>,
    ) -> Lobby {
        let mut lobby = Lobby {
            id: Uuid::new_v4(),
            owner: owner.clone(),
            game_id,
            players: Default::default(),
            status: crate::lobby::LobbyStatus::Waiting,
            is_private,
            whitelist: whitelist.map(|w| w.into_iter().collect()),
            last_activity: std::time::Instant::now(),
        };
        lobby.players.insert(owner);
        self.lobbies.insert(lobby.id, lobby.clone());
        lobby
    }

    pub fn create_lobby_with_whitelist(
        &mut self,
        is_private: bool,
        owner: String,
        game_id: String,
        whitelist: Option<Vec<String>>,
    ) -> Lobby {
        let lobby = Lobby {
            id: Uuid::new_v4(),
            owner,
            game_id,
            players: Default::default(),
            status: crate::lobby::LobbyStatus::Waiting,
            is_private,
            whitelist: whitelist.map(|w| w.into_iter().collect()),
            last_activity: std::time::Instant::now(),
        };
        self.lobbies.insert(lobby.id, lobby.clone());
        lobby
    }

    pub fn get_lobby(&self, id: &Uuid) -> Option<Lobby> {
        self.lobbies.get(id).cloned()
    }

    pub fn get_lobbies_for_player(&self, player_pubkey: Option<String>) -> Vec<Lobby> {
        self.lobbies
            .values()
            .filter(|lobby| {
                // If lobby is public, always show
                if !lobby.is_private && lobby.status == crate::lobby::LobbyStatus::Waiting {
                    return true;
                }
                // If the player is already in the lobby (e.g., the creator), always show it to them
                if let Some(ref pk) = player_pubkey {
                    if lobby.players.contains(pk) {
                        return true;
                    }
                }
                // If lobby is private and has a whitelist, only show if player is whitelisted
                if lobby.is_private {
                    if let Some(whitelist) = &lobby.whitelist {
                        if let Some(ref pk) = player_pubkey {
                            return whitelist.contains(pk);
                        } else {
                            return false;
                        }
                    }
                }
                false
            })
            .cloned()
            .collect()
    }

    pub fn add_player_to_lobby(
        &mut self,
        lobby_id: &Uuid,
        player_id: String,
    ) -> Result<(), SignalingError> {
        if let Some(lobby) = self.lobbies.get_mut(lobby_id) {
            // Prevent joining if the lobby has already started
            if lobby.status != crate::lobby::LobbyStatus::Waiting {
                tracing::warn!(lobby_id = %lobby_id, pubkey = %player_id, "Attempt to join lobby that is not waiting");
                return Err(SignalingError::UnknownPeer);
            }
            // Check whitelist if it exists
            if let Some(whitelist) = &lobby.whitelist {
                if !whitelist.contains(&player_id) {
                    return Err(SignalingError::UnknownPeer); // Using UnknownPeer to indicate "not allowed"
                }
            }
            lobby.players.insert(player_id);
            // Reset activity timer on join (lobby is in Waiting status)
            lobby.last_activity = std::time::Instant::now();
            Ok(())
        } else {
            // Log available lobbies for debugging when a lobby is unexpectedly missing
            let ids: Vec<String> = self.lobbies.keys().map(|u| u.to_string()).collect();
            tracing::debug!(?ids, ?lobby_id, "add_player_to_lobby: lobby not found");
            Err(SignalingError::UnknownPeer)
        }
    }

    /// Mark a lobby as started (InProgress). Only the owner may start the lobby.
    pub fn start_lobby(&mut self, lobby_id: &Uuid, owner_id: &String) -> Result<(), SignalingError> {
        if let Some(lobby) = self.lobbies.get_mut(lobby_id) {
            if &lobby.owner != owner_id {
                tracing::warn!(lobby_id = %lobby_id, owner = %owner_id, "Non-owner attempted to start lobby");
                return Err(SignalingError::UnknownPeer);
            }
            if lobby.status != crate::lobby::LobbyStatus::Waiting {
                tracing::debug!(lobby_id = %lobby_id, "Lobby already started");
                return Ok(());
            }
            lobby.status = crate::lobby::LobbyStatus::InProgress;
            tracing::info!(lobby_id = %lobby_id, "Lobby status set to InProgress");
            Ok(())
        } else {
            Err(SignalingError::UnknownPeer)
        }
    }

    /// Mark a lobby as finished and return it to Waiting state so it can be reused.
    pub fn end_lobby(&mut self, lobby_id: &Uuid) -> Result<(), SignalingError> {
        if let Some(lobby) = self.lobbies.get_mut(lobby_id) {
            if lobby.status == crate::lobby::LobbyStatus::Waiting {
                tracing::debug!(lobby_id = %lobby_id, "end_lobby: lobby already Waiting");
                return Ok(());
            }
            lobby.status = crate::lobby::LobbyStatus::Waiting;
            // Reset activity timer when returning to Waiting (inactivity cleanup starts now)
            lobby.last_activity = std::time::Instant::now();
            tracing::info!(lobby_id = %lobby_id, "Lobby status set to Waiting");
            Ok(())
        } else {
            Err(SignalingError::UnknownPeer)
        }
    }

    pub fn remove_player_from_lobby(&mut self, lobby_id: &Uuid, player_id: &String) {
        if let Some(lobby) = self.lobbies.get_mut(lobby_id) {
            lobby.players.remove(player_id);
        }
    }

    pub fn delete_lobby(&mut self, lobby_id: &Uuid) -> Result<(), SignalingError> {
        if self.lobbies.remove(lobby_id).is_some() {
            Ok(())
        } else {
            Err(SignalingError::UnknownPeer)
        }
    }

    pub fn add_to_whitelist(
        &mut self,
        lobby_id: &Uuid,
        player_ids: Vec<String>,
    ) -> Result<(), SignalingError> {
        if let Some(lobby) = self.lobbies.get_mut(lobby_id) {
            if let Some(whitelist) = &mut lobby.whitelist {
                for player_id in player_ids {
                    whitelist.insert(player_id);
                }
            } else {
                // Create whitelist if it doesn't exist
                lobby.whitelist = Some(player_ids.into_iter().collect());
            }
            // Reset activity timer on invite (if lobby is in Waiting status)
            if lobby.status == crate::lobby::LobbyStatus::Waiting {
                lobby.last_activity = std::time::Instant::now();
            }
            Ok(())
        } else {
            Err(SignalingError::UnknownPeer)
        }
    }

    pub fn remove_from_whitelist(
        &mut self,
        lobby_id: &Uuid,
        player_id: &String,
    ) -> Result<(), SignalingError> {
        if let Some(lobby) = self.lobbies.get_mut(lobby_id) {
            if let Some(whitelist) = &mut lobby.whitelist {
                whitelist.remove(player_id);
            }
            Ok(())
        } else {
            Err(SignalingError::UnknownPeer)
        }
    }

    /// Clean up inactive lobbies that are in Waiting status and have exceeded the timeout.
    /// Returns the list of deleted lobby IDs and their player lists for cleanup.
    pub fn cleanup_inactive_lobbies(&mut self) -> Vec<(Uuid, Vec<String>)> {
        let now = std::time::Instant::now();
        let timeout = crate::lobby::LOBBY_INACTIVITY_TIMEOUT;

        let expired_lobbies: Vec<(Uuid, Vec<String>)> = self
            .lobbies
            .iter()
            .filter(|(_, lobby)| {
                // Only cleanup lobbies in Waiting status
                lobby.status == crate::lobby::LobbyStatus::Waiting
                    && now.duration_since(lobby.last_activity) >= timeout
            })
            .map(|(id, lobby)| (*id, lobby.players.iter().cloned().collect()))
            .collect();

        for (id, _) in &expired_lobbies {
            self.lobbies.remove(id);
            tracing::info!(lobby_id = %id, "Lobby cleaned up due to inactivity");
        }

        if !expired_lobbies.is_empty() {
            tracing::info!(
                count = expired_lobbies.len(),
                "Cleaned up inactive lobbies"
            );
        }

        expired_lobbies
    }
}

#[derive(Debug, Clone)]
pub struct ServerState {
    pub lobby_manager: Arc<RwLock<LobbyManager>>,
    pub peers: StateObj<HashMap<PeerId, Peer>>,
    pub players_in_lobbies: Arc<RwLock<HashMap<String, Uuid>>>,
    pub challenge_manager: ChallengeManager,
    pub players_to_peers: Arc<RwLock<HashMap<String, PeerId>>>,
    pub waiting_players: Arc<RwLock<HashMap<SocketAddr, String>>>,
    pub lobby_updates: broadcast::Sender<()>,
}

impl Default for ServerState {
    fn default() -> Self {
        let (tx, _) = broadcast::channel(100);
        Self {
            lobby_manager: Default::default(),
            peers: Default::default(),
            players_in_lobbies: Default::default(),
            challenge_manager: Default::default(),
            players_to_peers: Default::default(),
            waiting_players: Default::default(),
            lobby_updates: tx,
        }
    }
}

impl SignalingState for ServerState {}

impl ServerState {
    /// Signaling is only valid between currently connected members of the same lobby.
    /// Keep this protocol boundary independent of the game implementation.
    pub fn may_signal(&self, sender: PeerId, receiver: PeerId) -> bool {
        let (sender_player, receiver_player) = {
            let peers = self.players_to_peers.read().unwrap();
            let sender_player = peers.iter().find(|(_, peer)| **peer == sender).map(|(player, _)| player.clone());
            let receiver_player = peers.iter().find(|(_, peer)| **peer == receiver).map(|(player, _)| player.clone());
            (sender_player, receiver_player)
        };
        let (Some(sender_player), Some(receiver_player)) = (sender_player, receiver_player) else { return false; };
        let (sender_lobby, receiver_lobby) = {
            let membership = self.players_in_lobbies.read().unwrap();
            (membership.get(&sender_player).copied(), membership.get(&receiver_player).copied())
        };
        let (Some(sender_lobby), Some(receiver_lobby)) = (sender_lobby, receiver_lobby) else { return false; };
        sender_lobby == receiver_lobby && self.lobby_manager.read().unwrap().get_lobby(&sender_lobby)
            .is_some_and(|lobby| lobby.players.contains(&sender_player) && lobby.players.contains(&receiver_player))
    }

    pub fn add_peer(&mut self, peer: Peer) {
        self.peers.lock().unwrap().insert(peer.id, peer);
    }

    pub fn remove_peer(&mut self, peer_id: &PeerId) -> Option<Peer> {
        self.peers.lock().unwrap().remove(peer_id)
    }

    pub fn get_peer(&self, peer_id: &PeerId) -> Option<Peer> {
        self.peers.lock().unwrap().get(peer_id).cloned()
    }

    pub fn try_send(&self, id: PeerId, message: Message) -> Result<(), SignalingError> {
        let clients = self.peers.lock().unwrap();
        match clients.get(&id) {
            Some(peer) => Ok(common_logic::try_send(&peer.sender, message)?),
            None => Err(SignalingError::UnknownPeer),
        }
    }
}

impl ServerState {
    /// Remove only the connection/peer and players_to_peers mapping for a player
    /// without removing their lobby membership. This is used when a websocket
    /// disconnects but we want to keep the player assigned to the lobby so they
    /// can rejoin a future game.
    pub fn remove_connection_only(&self, peer_id: &PeerId, player_id: &str) {
        // Remove from peers by PeerId
        self.peers.lock().unwrap().remove(peer_id);
        // Remove from players_to_peers mapping
        self.players_to_peers.write().unwrap().remove(player_id);
        tracing::info!(peer_id = ?peer_id, pubkey = %&player_id[..8], "Removed connection for player (kept lobby membership)");
    }
}
