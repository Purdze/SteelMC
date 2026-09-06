//! The Ender Dragon.
//!
//! Mirrors vanilla `net.minecraft.world.entity.boss.enderdragon.EnderDragon`.
//!
//! The dragon is a `Mob` that uses none of the mob AI machinery: no goal selectors,
//! no path navigation. It steers itself from a phase state machine over a fixed
//! 24-node graph, so its `ai_step` replaces the shared one outright rather than
//! delegating to it, exactly as vanilla's does.
//!
//! Client-only vanilla members are deliberately absent: `onFlap`, `doClientTick`,
//! `onSyncedDataUpdated`, `recreateFromPacket`, and the `growlTime` counter, whose
//! only use sits inside the `isClientSide` branch of `aiStep`. A vanilla client runs
//! its own copy of the phase machine off the synced phase id, so keeping that id
//! correct is what makes the visuals right.

mod part;
#[cfg(test)]
mod tests;

pub use part::EnderDragonPart;

use std::sync::{Arc, Weak};

use glam::DVec3;
use simdnbt::borrow::NbtCompound as BorrowedNbtCompoundView;
use simdnbt::owned::NbtCompound;
use steel_macros::entity_behavior;
use steel_protocol::packets::game::SoundSource;
use steel_registry::entity_type::{EntityDimensions, EntityTypeRef};
use steel_registry::sound_event::SoundEventRef;
use steel_registry::sound_events;
use steel_registry::vanilla_entity_data::EnderDragonEntityData;
use steel_utils::locks::SyncMutex;
use steel_utils::{Downcast as _, DowncastType, DowncastTypeKey};

use crate::entity::damage::DamageSource;
use crate::entity::{
    Entity, EntityBase, EntityBaseLoad, EntityPose, EntitySyncedData, LivingEntity,
    LivingEntityBase, Mob, MobBase, MobEffectInstance, PartEntity, part_entity_id,
    sync_dirty_mob_effects,
};
use crate::world::World;

/// The number of sub-entity hitboxes.
const SUB_ENTITY_COUNT: usize = 8;
/// Indices into `sub_entities`, in the vanilla construction order
/// head, neck, body, tail x3, wing x2. The remaining names arrive with the
/// per-part positioning math.
const HEAD: usize = 0;
const BODY: usize = 2;

const DRAGON_DEATH_TIME_KEY: &str = "DragonDeathTime";
const SITTING_DAMAGE_RECEIVED_KEY: &str = "sitting_damage_received";

/// The Ender Dragon.
#[entity_behavior(class = "EnderDragon", parts = 8)]
pub struct EnderDragonEntity {
    base: EntityBase,
    entity_type: EntityTypeRef,
    living_base: LivingEntityBase,
    mob_base: MobBase,
    entity_data: SyncMutex<EnderDragonEntityData>,
    /// Vanilla `subEntities`, in vanilla order: head, neck, body, tail x3, wing x2.
    sub_entities: Vec<Arc<dyn PartEntity>>,
    /// Vanilla `dragonDeathTime`.
    dragon_death_time: SyncMutex<i32>,
    /// Vanilla `sittingDamageReceived`.
    sitting_damage_received: SyncMutex<f32>,
}

// SAFETY: The owner-scoped type key uniquely identifies EnderDragonEntity.
unsafe impl DowncastType for EnderDragonEntity {
    const TYPE_KEY: DowncastTypeKey = DowncastTypeKey::new("steel:entity/ender_dragon");
}

impl EnderDragonEntity {
    /// Creates a new dragon.
    ///
    /// `id` must come from an [`crate::entity::EntityIdBlock`] wide enough for the
    /// eight parts, which the entity registry guarantees for this type.
    #[must_use]
    pub fn new(entity_type: EntityTypeRef, id: i32, position: DVec3, world: Weak<World>) -> Self {
        Self::new_with_base(
            EntityBase::new(id, position, entity_type.dimensions, world.clone()),
            entity_type,
            world,
        )
    }

    /// Loads a saved dragon.
    #[must_use]
    pub fn from_saved(entity_type: EntityTypeRef, load: EntityBaseLoad) -> Self {
        let world = load.world.clone();
        Self::new_with_base(
            EntityBase::from_load(load, entity_type.dimensions),
            entity_type,
            world,
        )
    }

    fn new_with_base(base: EntityBase, entity_type: EntityTypeRef, world: Weak<World>) -> Self {
        let living_base = LivingEntityBase::new(entity_type);
        let mob_base = MobBase::new();
        let mut entity_data = EnderDragonEntityData::new();
        // Seeds health from the MAX_HEALTH attribute; without this the dragon would
        // spawn on the synced default of 1.0 rather than 200.
        living_base.initialize_synced_data(&mut entity_data);

        // Vanilla sets `noPhysics` in the constructor, so the dragon passes through
        // terrain and does its own wall handling in `check_walls`.
        base.set_no_physics(true);

        Self {
            sub_entities: Self::create_sub_entities(entity_type, &base, &world),
            base,
            entity_type,
            living_base,
            mob_base,
            entity_data: SyncMutex::new(entity_data),
            dragon_death_time: SyncMutex::new(0),
            sitting_damage_received: SyncMutex::new(0.0),
        }
    }

    /// Builds the eight hitboxes, in vanilla's order and at vanilla's sizes.
    fn create_sub_entities(
        entity_type: EntityTypeRef,
        parent: &EntityBase,
        world: &Weak<World>,
    ) -> Vec<Arc<dyn PartEntity>> {
        const PARTS: [(&str, f32, f32); SUB_ENTITY_COUNT] = [
            ("head", 1.0, 1.0),
            ("neck", 3.0, 3.0),
            ("body", 5.0, 3.0),
            ("tail", 2.0, 2.0),
            ("tail", 2.0, 2.0),
            ("tail", 2.0, 2.0),
            ("wing", 4.0, 2.0),
            ("wing", 4.0, 2.0),
        ];

        PARTS
            .iter()
            .enumerate()
            .map(|(index, &(name, width, height))| {
                let part: Arc<dyn PartEntity> = Arc::new(EnderDragonPart::new(
                    entity_type,
                    part_entity_id(parent.id(), index as u32),
                    name,
                    width,
                    height,
                    parent.position(),
                    world.clone(),
                ));
                part
            })
            .collect()
    }

    /// Returns the dragon's hitboxes. Mirrors vanilla `getSubEntities`.
    #[must_use]
    pub fn sub_entities(&self) -> &[Arc<dyn PartEntity>] {
        &self.sub_entities
    }

    /// Returns the head, which vanilla treats as the only full-damage hitbox.
    #[must_use]
    pub fn head(&self) -> &Arc<dyn PartEntity> {
        &self.sub_entities[HEAD]
    }

    /// Returns vanilla `dragonDeathTime`.
    #[must_use]
    pub fn dragon_death_time(&self) -> i32 {
        *self.dragon_death_time.lock()
    }

    /// Applies damage routed through one of the dragon's hitboxes.
    ///
    /// Mirrors vanilla `EnderDragon.hurt(ServerLevel, EnderDragonPart, DamageSource,
    /// float)`, which Rust cannot name as an overload of `Entity::hurt`.
    ///
    /// Everything but the head takes a quarter of the damage plus a flat point, which
    /// is what makes aiming for the head worthwhile.
    pub fn hurt_part(
        &self,
        world: &World,
        part: &EnderDragonPart,
        source: &DamageSource,
        damage: f32,
    ) -> bool {
        // TODO: Return early while in the DYING phase, and route through
        // `DragonPhaseInstance::on_hurt`, once the phase machine lands.
        let mut damage = damage;
        if part.id() != self.head().id() {
            damage = damage / 4.0 + damage.min(1.0);
        }

        if damage < 0.01 {
            return false;
        }

        // TODO: Accumulate `sitting_damage_received` and take off once a quarter of
        // max health has landed, which needs the sitting phases.
        self.really_hurt(world, source, damage);
        true
    }

    /// Applies the damage the shared living path would have applied.
    ///
    /// Mirrors vanilla `EnderDragon.reallyHurt`, which calls `super.hurtServer`.
    fn really_hurt(&self, world: &World, source: &DamageSource, damage: f32) {
        self.default_hurt_server(world, source, damage);
    }

    fn update_dirty_mob_effect_entity_data(&self) {
        if let Some(display) = sync_dirty_mob_effects(&self.living_base, &self.entity_data) {
            self.entity_data
                .set_base_glowing_flag(self.has_glowing_tag() || display.glowing);
        }
    }
}

impl Entity for EnderDragonEntity {
    fn base(&self) -> &EntityBase {
        &self.base
    }

    fn entity_type(&self) -> EntityTypeRef {
        self.entity_type
    }

    fn base_tick(&self) {
        Mob::base_tick_mob(self);
    }

    fn parts(&self) -> &[Arc<dyn PartEntity>] {
        &self.sub_entities
    }

    /// Mirrors vanilla `EnderDragon.sanitizeScale`, which pins the dragon to 1.0.
    fn dimensions_for_pose(&self, _pose: EntityPose) -> EntityDimensions {
        self.entity_type.dimensions
    }

    fn synced_data(&self) -> Option<&dyn EntitySyncedData> {
        Some(&self.entity_data)
    }

    fn update_data_before_sync(&self) {
        self.update_dirty_mob_effect_entity_data();
    }

    /// Mirrors vanilla `EnderDragon.isPickable`.
    ///
    /// The dragon body itself is not a target; its eight parts are.
    fn is_pickable(&self) -> bool {
        false
    }

    /// Mirrors vanilla `EnderDragon.checkDespawn`, which is empty.
    fn check_despawn(&self) {}

    fn sound_source(&self) -> SoundSource {
        SoundSource::Hostile
    }

    fn save_additional(&self, nbt: &mut NbtCompound) {
        self.save_mob(nbt);
        // TODO: Persist `DragonPhase` once the phase machine lands.
        nbt.insert(DRAGON_DEATH_TIME_KEY, self.dragon_death_time());
        nbt.insert(
            SITTING_DAMAGE_RECEIVED_KEY,
            *self.sitting_damage_received.lock(),
        );
    }

    fn load_additional(&self, nbt: BorrowedNbtCompoundView<'_, '_>) {
        self.load_mob(nbt);
        // TODO: Restore `DragonPhase` once the phase machine lands.
        if let Some(death_time) = nbt.int(DRAGON_DEATH_TIME_KEY) {
            *self.dragon_death_time.lock() = death_time;
        }
        if let Some(received) = nbt.float(SITTING_DAMAGE_RECEIVED_KEY) {
            *self.sitting_damage_received.lock() = received;
        }
    }
}

impl LivingEntity for EnderDragonEntity {
    fn living_base(&self) -> &LivingEntityBase {
        &self.living_base
    }

    fn get_health(&self) -> f32 {
        *self.entity_data.lock().living_entity().health.get()
    }

    fn set_health(&self, health: f32) {
        let max_health = self.get_max_health();
        let clamped = health.clamp(0.0, max_health);
        self.entity_data
            .lock()
            .living_entity_mut()
            .health
            .set(clamped);
    }

    /// Mirrors vanilla `EnderDragon.hurtServer`, which routes everything through the
    /// body hitbox so that a direct hit is scaled like any other non-head part.
    fn hurt_server(&self, world: &World, source: &DamageSource, amount: f32) -> bool {
        let Some(body) = self.sub_entities.get(BODY) else {
            return false;
        };
        let Some(body) = body.as_ref().downcast_ref::<EnderDragonPart>() else {
            return false;
        };
        self.hurt_part(world, body, source, amount)
    }

    /// Mirrors vanilla `EnderDragon.getSoundVolume`.
    fn sound_volume(&self) -> f32 {
        5.0
    }

    fn hurt_sound(&self, _source: &DamageSource) -> Option<SoundEventRef> {
        Some(&sound_events::ENTITY_ENDER_DRAGON_HURT)
    }

    /// Mirrors vanilla `EnderDragon.sanitizeScale`, which always returns 1.0.
    fn get_scale(&self) -> f32 {
        1.0
    }

    /// Mirrors vanilla `EnderDragon.addEffect`, which refuses every effect.
    fn add_mob_effect(&self, _effect: MobEffectInstance) -> bool {
        false
    }
}

impl Mob for EnderDragonEntity {
    fn mob_base(&self) -> &MobBase {
        &self.mob_base
    }

    fn mob_flags(&self) -> i8 {
        *self.entity_data.lock().mob.mob_flags.get()
    }

    fn set_mob_flags(&self, flags: i8) {
        self.entity_data.lock().mob.mob_flags.set(flags);
    }

    /// The dragon steers itself, so it never ticks a path navigation.
    ///
    /// The shared default would tick the `PathNavigation` that `MobBase::new` creates
    /// for every mob. The dragon keeps that unused navigation, exactly as it inherits
    /// an unused goal selector from vanilla's `Mob`.
    fn tick_path_navigation(&self) {}

    fn ambient_sound(&self) -> Option<SoundEventRef> {
        Some(&sound_events::ENTITY_ENDER_DRAGON_AMBIENT)
    }

    /// Mirrors vanilla `EnderDragon.canAttack`.
    fn can_attack(&self, target: &dyn LivingEntity) -> bool {
        target.can_be_seen_as_enemy()
    }
}
