//! Health and death: damage, falling, the void, dying and respawning.

use bedrockrs_protocol::packet::Encode;
use bedrockrs_protocol::packets::{
    ActorEvent, Attribute, DeathInfo, GameRulesChanged, Respawn, RespawnState, UpdateAttributes,
};
use bedrockrs_protocol::types::Vec3;

use crate::damage::{DamageCause, Health, fall_damage};
use crate::game_mode::GameMode;
use crate::game_rules;
use crate::players::{EYE_HEIGHT, Movement};

use super::{Reply, Session, SessionEvent, Stage};

impl Session {
    pub(super) fn is_dead(&self) -> bool {
        self.health.is_dead()
    }

    /// Takes in the world's game rules; a client in the world is told them.
    pub fn set_game_rules(&mut self, rules: game_rules::Values) -> Reply {
        self.rules = rules;
        if !self.stage.in_world() {
            return Reply::default();
        }
        Reply::send(vec![
            GameRulesChanged {
                rules: rules.packet_rules(),
            }
            .encode(),
        ])
    }

    /// Damage the player would take, unless they are dead, not in the world
    /// yet, or in a game mode that spares them. Plugins hear of it before
    /// [`Session::apply_damage`] deals it.
    pub(super) fn proposed_damage(&self, cause: DamageCause, amount: f32) -> Option<SessionEvent> {
        let spared = !cause.ignores_game_mode() && !self.game_mode.takes_damage();
        if self.stage != Stage::InGame || self.is_dead() || spared || amount <= 0.0 {
            return None;
        }
        Some(SessionEvent::Damage { cause, amount })
    }

    /// Hurts the player, as a command or plugin asks.
    pub fn damage(&mut self, cause: DamageCause, amount: f32) -> Reply {
        Reply {
            events: self.proposed_damage(cause, amount).into_iter().collect(),
            ..Reply::default()
        }
    }

    /// Deals damage plugins let through: the player flinches, or dies.
    pub fn apply_damage(&mut self, cause: DamageCause, amount: f32) -> Reply {
        // The player may have died or left the world while plugins decided.
        if self.proposed_damage(cause, amount).is_none() {
            return Reply::default();
        }
        self.health.hurt(amount);
        tracing::debug!(player = %self.player, cause = cause.name(), amount, health = self.health.value, "hurt");
        if self.is_dead() {
            return self.die(cause);
        }
        Reply {
            packets: vec![
                self.health_attribute().encode(),
                ActorEvent::new(self.entity_id, ActorEvent::HURT).encode(),
            ],
            events: vec![
                SessionEvent::Hurt,
                SessionEvent::HealthChanged(self.health.value),
            ],
            ..Reply::default()
        }
    }

    /// Sets the player's health, as a plugin asks; 0 kills them.
    pub fn set_health(&mut self, health: f32) -> Reply {
        if self.stage != Stage::InGame || self.is_dead() {
            return Reply::default();
        }
        self.health.set(health);
        if self.is_dead() {
            return self.die(DamageCause::Override);
        }
        Reply {
            packets: vec![self.health_attribute().encode()],
            events: vec![SessionEvent::HealthChanged(self.health.value)],
            ..Reply::default()
        }
    }

    /// The player dies: their client shows the death screen, and unless the
    /// `keepinventory` rule says otherwise, what they carried falls where
    /// they died.
    pub(super) fn die(&mut self, cause: DamageCause) -> Reply {
        self.health.set(0.0);
        self.fall_distance = 0.0;
        tracing::debug!(uuid = %self.uuid, cause = cause.name(), "{} died", self.player);
        let mut packets = vec![
            self.health_attribute().encode(),
            ActorEvent::new(self.entity_id, ActorEvent::DEATH).encode(),
            DeathInfo {
                cause: cause.death_message().to_owned(),
                messages: vec![self.player.clone()],
            }
            .encode(),
        ];
        let mut events = vec![SessionEvent::HealthChanged(0.0)];
        let armor_before = self.inventory.armor();
        let drops = if self.rules.keepinventory {
            Vec::new()
        } else {
            self.inventory.clear()
        };
        if !drops.is_empty() {
            packets.extend(self.inventory_sync());
            events.push(SessionEvent::InventoryChanged(self.inventory.saved()));
            events.extend(self.held_event());
            events.extend(self.armor_events(armor_before));
        }
        events.push(SessionEvent::Died {
            cause,
            drops,
            feet: self.movement.feet(),
        });
        Reply {
            packets,
            events,
            ..Reply::default()
        }
    }

    /// The death screen's Respawn button: the player comes back at the world
    /// spawn with full health.
    pub(super) fn respawn(&mut self, respawn: Respawn) -> Reply {
        if respawn.state != RespawnState::ClientReadyToSpawn || !self.is_dead() {
            return Reply::default();
        }
        self.health = Health::PLAYER;
        self.fall_distance = 0.0;
        self.flying = self.game_mode == GameMode::Spectator;
        self.movement = self.spawn_movement();
        tracing::debug!(uuid = %self.uuid, "{} respawned", self.player);
        let mut packets = vec![
            Respawn {
                position: self.movement.position,
                state: RespawnState::ReadyToSpawn,
                entity_runtime_id: self.entity_id,
            }
            .encode(),
            self.health_attribute().encode(),
            self.own_abilities().encode(),
        ];
        // The spawn may be far from where they died.
        packets.extend(self.stream_chunks(self.view.radius()));
        let mut events = vec![
            SessionEvent::Moved(self.movement),
            SessionEvent::HealthChanged(self.health.value),
            SessionEvent::Flying(self.flying),
        ];
        events.extend(self.view_event());
        events.push(SessionEvent::Respawned);
        Reply {
            packets,
            events,
            ..Reply::default()
        }
    }

    /// Standing on the world spawn, looking ahead.
    pub(super) fn spawn_movement(&self) -> Movement {
        let spawn = self.world.spawn();
        Movement {
            position: Vec3 {
                x: spawn.x as f32 + 0.5,
                y: spawn.y as f32 + EYE_HEIGHT,
                z: spawn.z as f32 + 0.5,
            },
            pitch: 0.0,
            yaw: 0.0,
            head_yaw: 0.0,
            on_ground: true,
        }
    }

    /// The player's health as their client shows it.
    pub(super) fn health_attribute(&self) -> UpdateAttributes {
        UpdateAttributes {
            entity_runtime_id: self.entity_id,
            attributes: vec![self.health_value()],
            tick: 0,
        }
    }

    pub(super) fn health_value(&self) -> Attribute {
        Attribute {
            value: self.health.shown(),
            default: self.health.max,
            ..Attribute::at_default("minecraft:health", 0.0, self.health.max, self.health.max)
        }
    }

    /// Follows a fall from the player's movement: falling builds up a
    /// distance that landing turns into damage, as vanilla works it out.
    /// Rising, flying or a mode that cannot be hurt starts it over. Landing
    /// is when the client reports touching something below.
    pub(super) fn fall(
        &mut self,
        from: Movement,
        to: Movement,
        collided: bool,
    ) -> Option<SessionEvent> {
        let dropped = from.feet().y - to.feet().y;
        if self.flying || !self.game_mode.takes_damage() || self.is_dead() {
            self.fall_distance = 0.0;
            return None;
        }
        if collided && dropped >= 0.0 {
            let distance = self.fall_distance + dropped;
            self.fall_distance = 0.0;
            if !self.rules.falldamage {
                return None;
            }
            return self.proposed_damage(DamageCause::Fall, fall_damage(distance));
        }
        if dropped > 0.0 {
            self.fall_distance += dropped;
        } else {
            self.fall_distance = 0.0;
        }
        None
    }
}
