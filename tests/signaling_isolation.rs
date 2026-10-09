use allumette_server::state::ServerState;
use matchbox_protocol::PeerId;
use uuid::Uuid;

#[test]
fn signals_stay_inside_their_game_lobby_and_stop_after_removal() {
    let state = ServerState::default();
    let (first, second) = {
        let mut manager = state.lobby_manager.write().unwrap();
        let first = manager.create_lobby_with_owner(false, "alice".into(), "game_a".into(), None);
        manager.add_player_to_lobby(&first.id, "bob".into()).unwrap();
        let second = manager.create_lobby_with_owner(false, "carol".into(), "game_b".into(), None);
        (first.id, second.id)
    };
    let alice = PeerId(Uuid::new_v4());
    let bob = PeerId(Uuid::new_v4());
    let carol = PeerId(Uuid::new_v4());
    state.players_to_peers.write().unwrap().extend([
        ("alice".into(), alice), ("bob".into(), bob), ("carol".into(), carol),
    ]);
    state.players_in_lobbies.write().unwrap().extend([
        ("alice".into(), first), ("bob".into(), first), ("carol".into(), second),
    ]);
    assert!(state.may_signal(alice, bob));
    assert!(state.may_signal(bob, alice));
    assert!(!state.may_signal(alice, carol));
    assert!(!state.may_signal(carol, alice));
    assert!(!state.may_signal(alice, PeerId(Uuid::new_v4())));
    state.lobby_manager.write().unwrap().remove_player_from_lobby(&first, &"bob".into());
    assert!(!state.may_signal(alice, bob));
    state.lobby_manager.write().unwrap().delete_lobby(&first).unwrap();
    assert!(!state.may_signal(alice, alice));
}

#[test]
fn player_dtos_have_a_stable_order_without_exposing_private_members() {
    let state = ServerState::default();
    let mut manager = state.lobby_manager.write().unwrap();
    let lobby = manager.create_lobby_with_owner(false, "zed".into(), "any_engine".into(), None);
    manager.add_player_to_lobby(&lobby.id, "alice".into()).unwrap();
    manager.add_player_to_lobby(&lobby.id, "bob".into()).unwrap();
    let lobby = manager.get_lobby(&lobby.id).unwrap();
    let view = lobby.to_response(Some(&"zed".into()));
    assert_eq!(view.players.iter().map(|p| p.public_key.as_str()).collect::<Vec<_>>(), ["alice", "bob", "zed"]);
    assert!(lobby.to_response(None).players.is_empty());
}
