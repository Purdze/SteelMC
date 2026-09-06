use std::sync::Arc;

use glam::DVec3;
use steel_registry::blocks::BlockRef;
use steel_registry::{
    init_vanilla_registry, vanilla_blocks, vanilla_damage_types, vanilla_entities,
};
use steel_utils::types::UpdateFlags;
use steel_utils::{BlockPos, ChunkPos};

use super::{BlockInteraction, Explosion, SimpleExplosionDamageCalculator};
use crate::behavior::init_behaviors;
use crate::block_entity::init_block_entities;
use crate::entity::{ENTITIES, SharedEntity, init_entities, next_entity_id};
use crate::test_support::{fresh_test_world, insert_ready_full_chunk};
use crate::world::World;

/// Somewhere central in the loaded chunks, well clear of the world floor.
const CENTER: DVec3 = DVec3::new(8.5, 80.0, 8.5);

fn explosion_test_world(key: &'static str) -> Arc<World> {
    init_vanilla_registry();
    init_behaviors();
    init_block_entities();
    init_entities();
    let world = fresh_test_world(key);
    for x in -1..=1 {
        for z in -1..=1 {
            insert_ready_full_chunk(&world, ChunkPos::new(x, z));
        }
    }
    world
}

fn explosion_at(
    world: &Arc<World>,
    center: DVec3,
    radius: f32,
    interaction: BlockInteraction,
) -> Explosion {
    Explosion::new(world, None, None, None, center, radius, false, interaction)
}

fn add_pig(world: &Arc<World>, position: DVec3) -> SharedEntity {
    let pig = ENTITIES
        .create(
            &vanilla_entities::PIG,
            next_entity_id(),
            position,
            Arc::downgrade(world),
        )
        .expect("the generated factory should build a pig");
    world
        .try_add_entity(Arc::clone(&pig))
        .expect("the pig should be added");
    pig
}

fn health(entity: &SharedEntity) -> f32 {
    entity
        .as_living_entity()
        .expect("the target is a living entity")
        .get_health()
}

fn fill_layer(world: &Arc<World>, y: i32, block: BlockRef) {
    for x in 0..16 {
        for z in 0..16 {
            world.set_block(
                BlockPos::new(x, y, z),
                block.default_state(),
                UpdateFlags::UPDATE_NONE,
            );
        }
    }
}

#[test]
fn a_blast_reaches_blocks_across_its_radius() {
    let world = explosion_test_world("explosion_radius");
    fill_layer(&world, 79, &vanilla_blocks::STONE);

    let reached = explosion_at(&world, CENTER, 4.0, BlockInteraction::Destroy).explode();

    // The rays sample a shell, so the count is jittered, but a radius-4 blast against a
    // solid floor always finds a meaningful patch of it.
    assert!(reached > 0, "a radius-4 blast reached nothing");
}

#[test]
fn a_blast_stops_at_blast_proof_blocks() {
    let world = explosion_test_world("explosion_bedrock");
    fill_layer(&world, 79, &vanilla_blocks::BEDROCK);

    let reached = explosion_at(&world, CENTER, 4.0, BlockInteraction::Destroy).explode();
    let soft = {
        let world = explosion_test_world("explosion_bedrock_control");
        fill_layer(&world, 79, &vanilla_blocks::STONE);
        explosion_at(&world, CENTER, 4.0, BlockInteraction::Destroy).explode()
    };

    // Bedrock's resistance drains a ray long before it can mark the block, so the same
    // blast over bedrock reaches far fewer positions than over stone.
    assert!(
        reached < soft,
        "bedrock ({reached}) did not resist better than stone ({soft})"
    );
}

#[test]
fn a_blast_in_open_air_sees_all_of_an_entity() {
    let world = explosion_test_world("explosion_exposure_open");
    let pig = add_pig(&world, DVec3::new(8.5, 80.0, 10.5));

    let exposure = Explosion::seen_percent(&world, CENTER, pig.as_ref());

    assert!(
        (exposure - 1.0).abs() < 1.0e-6,
        "an unobstructed pig was only {exposure} exposed"
    );
}

#[test]
fn a_wall_hides_an_entity_from_the_blast() {
    let world = explosion_test_world("explosion_exposure_walled");
    let pig = add_pig(&world, DVec3::new(8.5, 80.0, 11.5));
    // A slab of stone straight through the line of sight, tall and wide enough that no
    // sample ray can go round it.
    for x in 4..14 {
        for y in 78..84 {
            world.set_block(
                BlockPos::new(x, y, 10),
                vanilla_blocks::STONE.default_state(),
                UpdateFlags::UPDATE_NONE,
            );
        }
    }

    let exposure = Explosion::seen_percent(&world, CENTER, pig.as_ref());

    assert_eq!(exposure, 0.0, "a fully walled pig was {exposure} exposed");
}

#[test]
fn a_blast_hurts_and_shoves_a_nearby_entity() {
    let world = explosion_test_world("explosion_damage");
    let pig = add_pig(&world, DVec3::new(10.5, 80.0, 8.5));
    let health_before = health(&pig);

    explosion_at(&world, CENTER, 4.0, BlockInteraction::Keep).explode();

    assert!(health(&pig) < health_before, "the pig took no damage");
    // Pushed away from the center, which sits to its west.
    assert!(pig.velocity().x > 0.0, "the pig was not shoved outwards");
}

#[test]
fn a_blast_spares_an_entity_out_of_range() {
    let world = explosion_test_world("explosion_out_of_range");
    // Beyond `radius * 2`, which is where vanilla's falloff reaches zero.
    let pig = add_pig(&world, DVec3::new(8.5, 80.0, 8.5 + 9.0));
    let health_before = health(&pig);

    explosion_at(&world, CENTER, 4.0, BlockInteraction::Keep).explode();

    assert_eq!(health(&pig), health_before);
}

#[test]
fn a_calculator_can_turn_entity_damage_off() {
    let world = explosion_test_world("explosion_no_entity_damage");
    let pig = add_pig(&world, DVec3::new(10.5, 80.0, 8.5));
    let health_before = health(&pig);

    Explosion::new(
        &world,
        None,
        None,
        Some(Box::new(SimpleExplosionDamageCalculator::new(
            true, false, None, None,
        ))),
        CENTER,
        4.0,
        false,
        BlockInteraction::Keep,
    )
    .explode();

    assert_eq!(health(&pig), health_before);
}

#[test]
fn a_blast_with_no_source_reports_as_environmental() {
    let world = explosion_test_world("explosion_damage_source");
    let explosion = explosion_at(&world, CENTER, 4.0, BlockInteraction::Keep);

    // Only a blast traced back to a player reports as `PLAYER_EXPLOSION`, which is what
    // puts a name in the death message.
    assert_eq!(
        explosion.damage_source().damage_type.message_id,
        vanilla_damage_types::EXPLOSION.message_id
    );
    assert_eq!(explosion.damage_source().source_position, Some(CENTER));
}

#[test]
fn only_a_block_breaking_blast_counts_as_large() {
    let world = explosion_test_world("explosion_is_small");

    assert!(explosion_at(&world, CENTER, 1.0, BlockInteraction::Destroy).is_small());
    assert!(!explosion_at(&world, CENTER, 4.0, BlockInteraction::Destroy).is_small());
    // A big blast that leaves blocks alone still uses the small effect.
    assert!(explosion_at(&world, CENTER, 4.0, BlockInteraction::Keep).is_small());
}

#[test]
fn block_interaction_reports_what_it_touches() {
    assert!(!BlockInteraction::Keep.interacts_with_blocks());
    assert!(BlockInteraction::TriggerBlock.interacts_with_blocks());
    assert!(!BlockInteraction::TriggerBlock.affects_blocklike_entities());
    assert!(BlockInteraction::DestroyWithDecay.affects_blocklike_entities());
}
