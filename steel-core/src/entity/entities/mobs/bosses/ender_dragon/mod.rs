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

mod flight_graph;
mod flight_history;
mod part;
mod phases;
#[cfg(test)]
mod tests;

pub use flight_graph::DragonFlightGraph;
pub use flight_history::{DragonFlightHistory, DragonFlightSample};
pub use part::EnderDragonPart;
pub use phases::{DragonPhaseInstance, EnderDragonPhase, EnderDragonPhaseManager};

use std::f32::consts::TAU;
use std::sync::{Arc, Weak};

use glam::DVec3;
use simdnbt::borrow::NbtCompound as BorrowedNbtCompoundView;
use simdnbt::owned::NbtCompound;
use steel_macros::entity_behavior;
use steel_protocol::packets::game::SoundSource;
use steel_registry::entity_type::{EntityDimensions, EntityTypeRef};
use steel_registry::sound_event::SoundEventRef;
use steel_registry::vanilla_entity_data::EnderDragonEntityData;
use steel_registry::{sound_events, vanilla_damage_type_tags};
use steel_utils::locks::SyncMutex;
use steel_utils::{BlockPos, Downcast as _, DowncastType, DowncastTypeKey, wrap_degrees};

use crate::entity::ai::node::Node;
use crate::entity::ai::path::Path;
use crate::entity::damage::DamageSource;
use crate::entity::{
    Entity, EntityBase, EntityBaseLoad, EntityPose, EntitySyncedData, LivingEntity,
    LivingEntityBase, Mob, MobBase, MobEffectInstance, PartEntity, part_entity_id,
    sync_dirty_mob_effects,
};
use crate::physics::{MoveResult, MoverType};
use crate::world::World;

/// The number of sub-entity hitboxes.
const SUB_ENTITY_COUNT: usize = 8;
/// Indices into `sub_entities`, in the vanilla construction order
/// head, neck, body, tail x3, wing x2. The remaining names arrive with the
/// per-part positioning math.
const HEAD: usize = 0;
const BODY: usize = 2;

/// Damage below this is dropped rather than applied.
const MINIMUM_EFFECTIVE_DAMAGE: f32 = 0.01;
/// Share of max health that dislodges a perched dragon.
const SITTING_ALLOWED_DAMAGE_FRACTION: f32 = 0.25;
/// Wing beat while perched, where the flight-speed formula does not apply.
const SITTING_FLAP_RATE: f32 = 0.1;
/// Wings held mid-beat while the dragon has no AI.
const NO_AI_FLAP_TIME: f32 = 0.5;
/// Forward thrust per tick, scaled by how well the dragon is already aimed.
const FORWARD_THRUST: f32 = 0.06;
/// Turn rate ceiling per tick, in degrees.
const MAX_TURN_DEGREES: f32 = 50.0;
/// Below this the dragon is close enough on an axis not to bother turning.
const STEERING_EPSILON: f64 = 1.0e-5;
/// Movement scale while clipping terrain.
const IN_WALL_MOVE_SCALE: f64 = 0.8;
/// Vertical velocity retained each tick.
const VERTICAL_DRAG: f64 = 0.91;

const DRAGON_PHASE_KEY: &str = "DragonPhase";
const DRAGON_DEATH_TIME_KEY: &str = "DragonDeathTime";
const SITTING_DAMAGE_RECEIVED_KEY: &str = "sitting_damage_received";

/// Where the exit portal sits for an arena centered on `origin`.
///
/// Mirrors vanilla `EndPodiumFeature.getLocation`. That method offsets a constant
/// `END_PODIUM_LOCATION`, which is `BlockPos.ZERO`, so this is an identity today; it
/// exists so the call sites read like vanilla and so the constant has one home when
/// the podium feature itself lands.
// TODO: Move this onto `EndPodiumFeature` once runtime feature placement exists.
#[must_use]
pub const fn end_podium_location(origin: BlockPos) -> BlockPos {
    origin
}

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
    /// Vanilla `flightHistory`.
    flight_history: SyncMutex<DragonFlightHistory>,
    /// Vanilla `phaseManager`.
    phase_manager: EnderDragonPhaseManager,
    /// Vanilla `flapTime` and `oFlapTime`.
    ///
    /// Cosmetic in itself, but `is_flapping` reads it and that drives the FLAP game
    /// event from inside the shared move path.
    flap_time: SyncMutex<f32>,
    o_flap_time: SyncMutex<f32>,
    /// Vanilla `yRotA`, the accumulated turn rate.
    y_rot_a: SyncMutex<f32>,
    /// Vanilla `inWall`, set by the wall scan once that lands.
    in_wall: SyncMutex<bool>,
    /// Vanilla `fightOrigin`, the arena center this dragon belongs to.
    ///
    /// Set by the fight rather than persisted: vanilla saves only the phase, the
    /// death timer and the sitting damage.
    fight_origin: SyncMutex<BlockPos>,
    /// Vanilla `nodes` and `nodeAdjacency`, built on first use because the layout
    /// samples the terrain.
    flight_graph: SyncMutex<Option<DragonFlightGraph>>,
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
            flight_history: SyncMutex::new(DragonFlightHistory::new()),
            phase_manager: EnderDragonPhaseManager::new(),
            flap_time: SyncMutex::new(0.0),
            o_flap_time: SyncMutex::new(0.0),
            y_rot_a: SyncMutex::new(0.0),
            in_wall: SyncMutex::new(false),
            fight_origin: SyncMutex::new(BlockPos::ZERO),
            flight_graph: SyncMutex::new(None),
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

    /// Returns the dragon's phase machine. Mirrors vanilla `getPhaseManager`.
    #[must_use]
    pub const fn phase_manager(&self) -> &EnderDragonPhaseManager {
        &self.phase_manager
    }

    /// Publishes the active phase to watching clients.
    ///
    /// A vanilla client runs its own copy of the phase machine off this value, so it
    /// is what makes the dragon animate correctly rather than any server-side work.
    pub(super) fn set_synced_phase(&self, phase: EnderDragonPhase) {
        self.entity_data
            .lock()
            .ender_dragon_mut()
            .phase
            .set(phase.id());
    }

    /// Returns the phase id currently published to clients.
    #[must_use]
    pub fn synced_phase(&self) -> i32 {
        *self.entity_data.lock().ender_dragon().phase.get()
    }

    /// How many end crystals are still feeding the dragon.
    ///
    /// `None` means there is no fight at all, which vanilla treats differently from a
    /// fight with zero crystals left in one of its three call sites.
    // TODO: Read this from `EnderDragonFight` once that exists.
    #[expect(
        clippy::unused_self,
        reason = "reads the dragon's fight once EnderDragonFight lands"
    )]
    #[must_use]
    pub const fn alive_crystals(&self) -> Option<i32> {
        None
    }

    /// the arena center this dragon belongs to. Vanilla `getFightOrigin`.
    #[must_use]
    pub fn fight_origin(&self) -> BlockPos {
        *self.fight_origin.lock()
    }

    /// Sets the arena center. Vanilla `setFightOrigin`.
    pub fn set_fight_origin(&self, origin: BlockPos) {
        *self.fight_origin.lock() = origin;
    }

    /// Returns the flight-graph node nearest the dragon.
    ///
    /// Mirrors vanilla `findClosestNode()`, whose no-argument overload also builds the
    /// graph on first use. The build samples the terrain, so it needs the world.
    pub fn find_closest_node(&self, world: &World) -> usize {
        let position = self.position();
        let crystals = self.alive_crystals();
        self.with_flight_graph(world, |graph| graph.closest_node(position, crystals))
    }

    /// Paths between two flight-graph nodes. Mirrors vanilla `findPath`.
    pub fn find_path(
        &self,
        world: &World,
        start: usize,
        end: usize,
        final_node: Option<Node>,
    ) -> Option<Path> {
        let crystals = self.alive_crystals();
        self.with_flight_graph(world, |graph| {
            graph.find_path(start, end, final_node, crystals)
        })
    }

    /// Runs `action` against the flight graph, building it if this is the first use.
    ///
    /// `action` runs under the graph lock, so it must stay confined to the graph. The
    /// two callers above satisfy that: a search reads node positions and nothing else.
    fn with_flight_graph<R>(
        &self,
        world: &World,
        action: impl FnOnce(&DragonFlightGraph) -> R,
    ) -> R {
        let mut graph = self.flight_graph.lock();
        action(graph.get_or_insert_with(|| DragonFlightGraph::build(world)))
    }

    /// Runs vanilla `EnderDragon.aiStep`.
    ///
    /// A full replacement for the shared living step, as vanilla's is: the dragon
    /// steers from its phase rather than from movement input, and it shoves entities
    /// with its wings instead of the usual entity pushing.
    fn dragon_ai_step(&self, world: &Arc<World>) -> Option<MoveResult> {
        self.process_flapping_movement();

        // Everything below is inside vanilla's `else`; a dying dragon is moved by
        // `tick_death` instead.
        if self.is_dead_or_dying() {
            return None;
        }

        self.tick_flap_time();
        self.set_yaw(wrap_degrees(self.yaw()));

        if self.is_no_ai() {
            *self.flap_time.lock() = NO_AI_FLAP_TIME;
            return None;
        }

        {
            let mut history = self.flight_history.lock();
            history.record(self.position().y, self.yaw());
        }

        let phase = self.tick_phase(world);
        let result = self.steer_towards_phase_target(phase);

        self.apply_effects_from_blocks();
        self.set_y_body_rot(self.yaw());
        result
    }

    /// Returns the dragon's yaw.
    fn yaw(&self) -> f32 {
        self.rotation().0
    }

    /// Sets the dragon's yaw, leaving its pitch alone.
    ///
    /// Stands in for vanilla `setYRot`; Steel's setter takes both components, and the
    /// dragon only ever steers in yaw.
    fn set_yaw(&self, yaw: f32) {
        self.set_rotation((yaw, self.rotation().1));
    }

    /// Advances the wing beat. Mirrors the `flapTime` bookkeeping in `aiStep`.
    fn tick_flap_time(&self) {
        *self.o_flap_time.lock() = *self.flap_time.lock();

        let velocity = self.velocity();
        let horizontal = velocity.x.hypot(velocity.z) as f32;
        let flap_speed = (0.2 / (horizontal * 10.0 + 1.0)) * 2.0_f32.powf(velocity.y as f32);

        let mut flap_time = self.flap_time.lock();
        *flap_time += if self.phase_manager.current().is_sitting() {
            SITTING_FLAP_RATE
        } else if *self.in_wall.lock() {
            flap_speed * 0.5
        } else {
            flap_speed
        };
    }

    /// Ticks the active phase, re-ticking once if it switched.
    ///
    /// Vanilla chases exactly one switch, and the steering that follows uses the new
    /// phase, so a phase that hands off gets to set its successor's fly target in the
    /// same tick.
    fn tick_phase(&self, world: &Arc<World>) -> EnderDragonPhase {
        let before = self.phase_manager.current_phase();
        self.phase_manager
            .instance(before)
            .do_server_tick(self, world);

        let after = self.phase_manager.current_phase();
        if after != before {
            self.phase_manager
                .instance(after)
                .do_server_tick(self, world);
        }
        self.phase_manager.current_phase()
    }

    /// Flies the dragon toward the active phase's target.
    ///
    /// Ported expression by expression from vanilla. Three orderings matter and are
    /// easy to "tidy" into something that still looks right: the squared distance is
    /// taken from the pre-clamp height delta; the heading vector reads the vertical
    /// velocity *after* the climb has been added; and the forward thrust is applied
    /// along `-Z` rather than through a movement input.
    fn steer_towards_phase_target(&self, phase: EnderDragonPhase) -> Option<MoveResult> {
        let instance = self.phase_manager.instance(phase);
        let target = instance.fly_target_location()?;

        let position = self.position();
        let dx = target.x - position.x;
        let mut dy = target.y - position.y;
        let dz = target.z - position.z;
        let dist_to_target = dx * dx + dy * dy + dz * dz;

        let max = f64::from(instance.fly_speed());
        let horizontal_dist = (dx * dx + dz * dz).sqrt();
        if horizontal_dist > 0.0 {
            dy = (dy / horizontal_dist).clamp(-max, max);
        }

        self.set_velocity(self.velocity() + DVec3::new(0.0, dy * 0.01, 0.0));
        self.set_yaw(wrap_degrees(self.yaw()));

        let aim = (target - position).normalize_or_zero();
        let yaw_radians = f64::from(self.yaw().to_radians());
        let heading = DVec3::new(yaw_radians.sin(), self.velocity().y, -yaw_radians.cos())
            .normalize_or_zero();
        let alignment = (((heading.dot(aim) as f32) + 0.5) / 1.5).max(0.0);

        if dx.abs() > STEERING_EPSILON || dz.abs() > STEERING_EPSILON {
            let desired = wrap_degrees(180.0 - (dx.atan2(dz) as f32).to_degrees() - self.yaw())
                .clamp(-MAX_TURN_DEGREES, MAX_TURN_DEGREES);
            let mut y_rot_a = self.y_rot_a.lock();
            *y_rot_a *= 0.8;
            *y_rot_a += desired * instance.turn_speed(self);
            let turn = *y_rot_a;
            drop(y_rot_a);
            self.set_yaw(self.yaw() + turn * 0.1);
        }

        let span = (2.0 / (dist_to_target + 1.0)) as f32;
        self.move_relative(
            FORWARD_THRUST * (alignment * span + (1.0 - span)),
            DVec3::new(0.0, 0.0, -1.0),
        );

        let velocity = self.velocity();
        let movement = if *self.in_wall.lock() {
            velocity * IN_WALL_MOVE_SCALE
        } else {
            velocity
        };
        let result = self.move_entity(MoverType::SelfMovement, movement);

        let moved = self.velocity();
        let slide = 0.8 + 0.15 * (moved.normalize_or_zero().dot(heading) + 1.0) / 2.0;
        self.set_velocity(moved * DVec3::new(slide, VERTICAL_DRAG, slide));
        result
    }

    /// Applies damage routed through one of the dragon's hitboxes.
    ///
    /// Mirrors vanilla `EnderDragon.hurt(ServerLevel, EnderDragonPart, DamageSource,
    /// float)`, which Rust cannot name as an overload of `Entity::hurt`.
    ///
    /// Everything but the head takes a quarter of the damage plus a flat point, which
    /// is what makes aiming for the head worthwhile.
    ///
    /// Note the dragon reports a hit as handled even when it ignores the damage, so
    /// an arrow from a dispenser still lands and simply does nothing.
    pub fn hurt_part(
        &self,
        world: &World,
        part: &EnderDragonPart,
        source: &DamageSource,
        damage: f32,
    ) -> bool {
        let phase = self.phase_manager.current();
        if phase.phase() == EnderDragonPhase::Dying {
            return false;
        }

        let mut damage = phase.on_hurt(source, damage);
        if part.id() != self.head().id() {
            damage = damage / 4.0 + damage.min(1.0);
        }

        if damage < MINIMUM_EFFECTIVE_DAMAGE {
            return false;
        }

        if !Self::is_damageable_by(world, source) {
            return true;
        }

        let health_before = self.get_health();
        self.really_hurt(world, source, damage);
        if phase.is_sitting() {
            self.accumulate_sitting_damage(health_before - self.get_health());
        }
        true
    }

    /// Whether a damage source is allowed to hurt the dragon at all.
    ///
    /// Vanilla admits only players and the explosion-shaped damage types, which is
    /// what stops the dragon being whittled down by fire, drowning or a stray mob.
    fn is_damageable_by(world: &World, source: &DamageSource) -> bool {
        if source.is(&vanilla_damage_type_tags::DamageTypeTag::ALWAYS_HURTS_ENDER_DRAGONS) {
            return true;
        }

        source
            .causing_entity_id
            .and_then(|id| world.get_entity_by_id(id))
            .is_some_and(|entity| entity.as_player().is_some())
    }

    /// Tracks damage taken while perched, and takes off once enough has landed.
    ///
    /// Mirrors the accumulator in vanilla's per-part `hurt`: a quarter of max health
    /// is what dislodges a sitting dragon.
    fn accumulate_sitting_damage(&self, taken: f32) {
        let mut received = self.sitting_damage_received.lock();
        *received += taken;
        if *received <= SITTING_ALLOWED_DAMAGE_FRACTION * self.get_max_health() {
            return;
        }

        *received = 0.0;
        drop(received);
        self.phase_manager
            .set_phase(self, EnderDragonPhase::Takeoff);
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

    /// Mirrors vanilla `EnderDragon.isFlapping`.
    ///
    /// Detects the point in the beat where the wings sweep down, which is what makes
    /// the shared move path emit the flap game event.
    fn is_flapping(&self) -> bool {
        let flap = (*self.flap_time.lock() * TAU).cos();
        let previous = (*self.o_flap_time.lock() * TAU).cos();
        previous <= -0.3 && flap >= -0.3
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
        nbt.insert(DRAGON_PHASE_KEY, self.phase_manager.current_phase().id());
        nbt.insert(DRAGON_DEATH_TIME_KEY, self.dragon_death_time());
        nbt.insert(
            SITTING_DAMAGE_RECEIVED_KEY,
            *self.sitting_damage_received.lock(),
        );
    }

    fn load_additional(&self, nbt: BorrowedNbtCompoundView<'_, '_>) {
        self.load_mob(nbt);
        if let Some(phase) = nbt.int(DRAGON_PHASE_KEY) {
            self.phase_manager
                .set_phase(self, EnderDragonPhase::by_id(phase));
        }
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

    /// Mirrors vanilla `EnderDragon.aiStep`, which does not call `super`.
    fn ai_step(&self) -> Option<MoveResult> {
        let world = self.level()?;
        self.dragon_ai_step(&world)
    }

    /// Flushes the line-of-sight cache the shared AI path would have cleared.
    ///
    /// The dragon replaces `ai_step` wholesale, so it never reaches
    /// `mob_server_ai_step`, and without this every targeting check would answer from
    /// a cache populated once and never invalidated.
    fn server_ai_step(&self) {
        self.tick_sensing();
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
