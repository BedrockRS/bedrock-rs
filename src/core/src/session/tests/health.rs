use super::*;

/// PlayerAuthInput with the player's feet at `feet_y` above the spawn,
/// touching something below when `landed`.
fn moving_to(feet_y: f32, landed: bool) -> Vec<u8> {
    let eyes = Vec3 {
        x: 0.5,
        y: feet_y + EYE_HEIGHT,
        z: 0.5,
    };
    let mut input = PlayerAuthInput::decode_payload(&mut bedrockrs_protocol::io::Reader::new(
        &breaking_input(eyes, 0.0, Vec::new())[2..],
    ))
    .unwrap();
    if landed {
        input.input_flags = vec![input_flag::VERTICAL_COLLISION];
    }
    input.encode()
}

fn survival_session() -> Session {
    let mut session = in_game_session();
    session.set_game_mode(GameMode::Survival);
    session
}

#[test]
fn landing_after_a_fall_hurts() {
    let mut session = survival_session();
    // Up ten blocks (rising builds no fall), then down in two steps.
    session.handle(&moving_to(-50.0, false)).unwrap();
    session.handle(&moving_to(-55.0, false)).unwrap();
    let reply = session.handle(&moving_to(-60.0, true)).unwrap();
    assert_eq!(damage_events(&reply), [(DamageCause::Fall, 7.0)]);

    // Plugins let it through: the player flinches and is told their health.
    let reply = session.apply_damage(DamageCause::Fall, 7.0);
    assert_eq!(ids(&reply), [id::UPDATE_ATTRIBUTES, id::ACTOR_EVENT]);
    assert_eq!(
        reply.events,
        [SessionEvent::Hurt, SessionEvent::HealthChanged(13.0)]
    );
    assert_eq!(session.saved_player().unwrap().1.health, Some(13.0));

    // A jump is no fall.
    session.handle(&moving_to(-58.75, false)).unwrap();
    let reply = session.handle(&moving_to(-60.0, true)).unwrap();
    assert!(damage_events(&reply).is_empty());
}

#[test]
fn falls_spare_creative_players_and_follow_the_rule() {
    let mut session = in_game_session();
    session.handle(&moving_to(-40.0, false)).unwrap();
    let reply = session.handle(&moving_to(-60.0, true)).unwrap();
    assert!(damage_events(&reply).is_empty(), "creative");

    let mut session = survival_session();
    session.set_game_rules(game_rules::Values {
        falldamage: false,
        ..game_rules::Values::default()
    });
    session.handle(&moving_to(-40.0, false)).unwrap();
    let reply = session.handle(&moving_to(-60.0, true)).unwrap();
    assert!(damage_events(&reply).is_empty(), "falldamage is off");
}

#[test]
fn dying_drops_everything_and_respawning_starts_over() {
    let mut session = survival_session();
    session.handle(&moving_to(-55.0, false)).unwrap();
    let reply = session.apply_damage(DamageCause::Void, 25.0);
    assert_eq!(
        ids(&reply)[..3],
        [id::UPDATE_ATTRIBUTES, id::ACTOR_EVENT, id::DEATH_INFO]
    );
    let Some(SessionEvent::Died { cause, drops, feet }) = reply.events.last() else {
        panic!("expected a death, got {:?}", reply.events);
    };
    assert_eq!(*cause, DamageCause::Void);
    assert_eq!(drops.len(), TEST_KIT.len());
    assert_eq!(feet.y, -55.0);
    assert!(session.inventory().hotbar(0).is_none(), "nothing is kept");
    assert_eq!(session.saved_player().unwrap().1.health, Some(0.0));

    // The dead neither move nor take more damage.
    assert!(
        session
            .handle(&moving_to(-40.0, false))
            .unwrap()
            .events
            .is_empty()
    );
    assert!(
        session
            .apply_damage(DamageCause::Void, 1.0)
            .events
            .is_empty()
    );

    let respawn = Respawn {
        position: Vec3::default(),
        state: RespawnState::ClientReadyToSpawn,
        entity_runtime_id: PLAYER_ENTITY_ID,
    };
    let reply = session.handle(&respawn.encode()).unwrap();
    assert_eq!(
        ids(&reply)[..3],
        [id::RESPAWN, id::UPDATE_ATTRIBUTES, id::UPDATE_ABILITIES]
    );
    assert_eq!(reply.events.last(), Some(&SessionEvent::Respawned));
    assert!(session.health().is_full());
    assert_eq!(
        session.saved_player().unwrap().1.y,
        -60.0,
        "back at the spawn"
    );
    // Respawning twice does nothing.
    assert!(
        session
            .handle(&respawn.encode())
            .unwrap()
            .packets
            .is_empty()
    );
}

#[test]
fn keepinventory_keeps_everything() {
    let mut session = survival_session();
    session.set_game_rules(game_rules::Values {
        keepinventory: true,
        ..game_rules::Values::default()
    });
    let reply = session.apply_damage(DamageCause::Fall, 30.0);
    let Some(SessionEvent::Died { drops, .. }) = reply.events.last() else {
        panic!("expected a death");
    };
    assert!(drops.is_empty());
    assert!(session.inventory().hotbar(0).is_some());
}

#[test]
fn the_void_hurts_and_peaceful_heals() {
    let items = ItemEntities::new();
    let mut session = survival_session();
    session.handle(&moving_to(-70.0, false)).unwrap();
    let hurts: Vec<_> = (0..20)
        .flat_map(|_| damage_events(&session.tick(&items)))
        .collect();
    assert_eq!(hurts, [(DamageCause::Void, 4.0), (DamageCause::Void, 4.0)]);

    let mut session = survival_session();
    session.apply_damage(DamageCause::Fall, 5.0);
    for _ in 0..19 {
        session.tick(&items);
    }
    assert_eq!(session.health().value, 15.0);
    let reply = session.tick(&items);
    assert_eq!(session.health().value, 16.0, "a point a second");
    assert!(reply.events.contains(&SessionEvent::HealthChanged(16.0)));
}
