//! Health and damage, modelled on vanilla's: the `minecraft:health` component
//! and the damage causes vanilla's scripts name.

/// Why something was hurt, as vanilla names it (`@minecraft/server`'s
/// `EntityDamageCause`). Every vanilla cause is here so plugins can use them
/// all; the server itself deals only some.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DamageCause {
    Anvil,
    BlockExplosion,
    Campfire,
    Contact,
    Drowning,
    EntityAttack,
    EntityExplosion,
    Fall,
    FallingBlock,
    Fire,
    FireTick,
    Fireworks,
    FlyIntoWall,
    Freezing,
    Lava,
    Lightning,
    MaceSmash,
    Magic,
    Magma,
    None,
    /// Health set directly, such as by a plugin setting it to 0.
    Override,
    Piston,
    Projectile,
    RamAttack,
    /// `/kill`.
    SelfDestruct,
    SonicBoom,
    SoulCampfire,
    Stalactite,
    Stalagmite,
    Starve,
    Suffocation,
    Temperature,
    Thorns,
    Void,
    Wither,
}

impl DamageCause {
    pub const ALL: [Self; 35] = [
        Self::Anvil,
        Self::BlockExplosion,
        Self::Campfire,
        Self::Contact,
        Self::Drowning,
        Self::EntityAttack,
        Self::EntityExplosion,
        Self::Fall,
        Self::FallingBlock,
        Self::Fire,
        Self::FireTick,
        Self::Fireworks,
        Self::FlyIntoWall,
        Self::Freezing,
        Self::Lava,
        Self::Lightning,
        Self::MaceSmash,
        Self::Magic,
        Self::Magma,
        Self::None,
        Self::Override,
        Self::Piston,
        Self::Projectile,
        Self::RamAttack,
        Self::SelfDestruct,
        Self::SonicBoom,
        Self::SoulCampfire,
        Self::Stalactite,
        Self::Stalagmite,
        Self::Starve,
        Self::Suffocation,
        Self::Temperature,
        Self::Thorns,
        Self::Void,
        Self::Wither,
    ];

    /// The cause's name, as scripts name it: `fall`, `selfDestruct`, …
    pub const fn name(self) -> &'static str {
        match self {
            Self::Anvil => "anvil",
            Self::BlockExplosion => "blockExplosion",
            Self::Campfire => "campfire",
            Self::Contact => "contact",
            Self::Drowning => "drowning",
            Self::EntityAttack => "entityAttack",
            Self::EntityExplosion => "entityExplosion",
            Self::Fall => "fall",
            Self::FallingBlock => "fallingBlock",
            Self::Fire => "fire",
            Self::FireTick => "fireTick",
            Self::Fireworks => "fireworks",
            Self::FlyIntoWall => "flyIntoWall",
            Self::Freezing => "freezing",
            Self::Lava => "lava",
            Self::Lightning => "lightning",
            Self::MaceSmash => "maceSmash",
            Self::Magic => "magic",
            Self::Magma => "magma",
            Self::None => "none",
            Self::Override => "override",
            Self::Piston => "piston",
            Self::Projectile => "projectile",
            Self::RamAttack => "ramAttack",
            Self::SelfDestruct => "selfDestruct",
            Self::SonicBoom => "sonicBoom",
            Self::SoulCampfire => "soulCampfire",
            Self::Stalactite => "stalactite",
            Self::Stalagmite => "stalagmite",
            Self::Starve => "starve",
            Self::Suffocation => "suffocation",
            Self::Temperature => "temperature",
            Self::Thorns => "thorns",
            Self::Void => "void",
            Self::Wither => "wither",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|cause| cause.name() == name)
    }

    /// Whether the damage reaches players in every game mode. Commands and
    /// health set directly do; everything else spares creative players and
    /// spectators.
    pub const fn ignores_game_mode(self) -> bool {
        matches!(self, Self::SelfDestruct | Self::Override)
    }

    /// The vanilla translation key of the death message, filled in with the
    /// player's name. Causes the server cannot deal yet use the generic one.
    pub const fn death_message(self) -> &'static str {
        match self {
            Self::Fall => "death.attack.fall",
            Self::Void => "death.attack.outOfWorld",
            _ => "death.attack.generic",
        }
    }

    /// The death message in English, for the server's log.
    pub fn death_message_english(self, player: &str) -> String {
        match self {
            Self::Fall => format!("{player} hit the ground too hard"),
            Self::Void => format!("{player} fell out of the world"),
            _ => format!("{player} died"),
        }
    }
}

/// How much damage a fall of `distance` blocks deals, as vanilla works it
/// out: nothing up to 3 blocks, then a point per block, rounded up from half
/// a point.
pub fn fall_damage(distance: f32) -> f32 {
    let damage = distance - 3.0;
    if damage < 0.5 { 0.0 } else { damage.ceil() }
}

/// Below this height, the void hurts.
pub const VOID_DEPTH: f32 = -64.0;
/// What the void deals each time it hurts.
pub const VOID_DAMAGE: f32 = 4.0;
/// Ticks between hurts from the void: half a second.
pub const VOID_INTERVAL: u64 = 10;
/// Ticks between points of health regained in peaceful: one a second.
pub const PEACEFUL_REGENERATION_INTERVAL: u64 = 20;

/// An entity's health: vanilla's `minecraft:health` component. Players have
/// 20 points, ten hearts.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Health {
    pub value: f32,
    pub max: f32,
}

impl Health {
    pub const PLAYER: Self = Self {
        value: 20.0,
        max: 20.0,
    };

    pub fn is_dead(&self) -> bool {
        self.value <= 0.0
    }

    pub fn is_full(&self) -> bool {
        self.value >= self.max
    }

    /// Takes `amount` away, down to 0.
    pub fn hurt(&mut self, amount: f32) {
        self.value = (self.value - amount).max(0.0);
    }

    /// Sets the value, within 0 and the maximum.
    pub fn set(&mut self, value: f32) {
        self.value = value.clamp(0.0, self.max);
    }

    pub fn heal(&mut self, amount: f32) {
        self.set(self.value + amount);
    }

    /// As clients show it: whole points, rounded up so a living player never
    /// shows as dead.
    pub fn shown(&self) -> f32 {
        self.value.ceil()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn falls_hurt_as_in_vanilla() {
        assert_eq!(fall_damage(1.25), 0.0, "a jump");
        assert_eq!(fall_damage(3.4), 0.0);
        assert_eq!(fall_damage(3.5), 1.0);
        assert_eq!(fall_damage(4.0), 1.0);
        assert_eq!(fall_damage(10.2), 8.0);
        assert_eq!(fall_damage(23.0), 20.0, "twenty blocks kill");
    }

    #[test]
    fn causes_use_vanilla_names() {
        assert_eq!(DamageCause::SelfDestruct.name(), "selfDestruct");
        assert_eq!(DamageCause::from_name("void"), Some(DamageCause::Void));
        assert_eq!(DamageCause::from_name("self_destruct"), None);
        for cause in DamageCause::ALL {
            assert_eq!(DamageCause::from_name(cause.name()), Some(cause));
        }
    }

    #[test]
    fn plugins_know_the_same_causes() {
        let names: Vec<&str> = DamageCause::ALL
            .into_iter()
            .map(DamageCause::name)
            .collect();
        assert_eq!(names, bedrockrs_plugins::DAMAGE_CAUSES);
    }

    #[test]
    fn health_stays_between_zero_and_its_maximum() {
        let mut health = Health::PLAYER;
        health.hurt(25.0);
        assert!(health.is_dead());
        assert_eq!(health.value, 0.0);
        health.heal(50.0);
        assert!(health.is_full());
        health.set(0.3);
        assert_eq!(health.shown(), 1.0, "a sliver of health still shows");
    }
}
