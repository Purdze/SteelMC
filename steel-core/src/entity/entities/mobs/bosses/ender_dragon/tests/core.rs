use std::sync::Weak;

use glam::DVec3;
use simdnbt::owned::NbtCompound;
use steel_registry::init_vanilla_registry;
use steel_registry::vanilla_entities;

use crate::entity::entities::EnderDragonEntity;
use crate::entity::{ENTITIES, Entity, LivingEntity, init_entities, reserve_entity_ids};

/// Builds a dragon on a properly reserved ID block, the way the registry does.
fn test_dragon() -> EnderDragonEntity {
    init_vanilla_registry();
    let ids = reserve_entity_ids(9);
    EnderDragonEntity::new(
        &vanilla_entities::ENDER_DRAGON,
        ids.first(),
        DVec3::ZERO,
        Weak::new(),
    )
}

#[test]
fn spawns_at_full_vanilla_health() {
    let dragon = test_dragon();

    // Comes from the generated MAX_HEALTH attribute; a constructor that skipped
    // `initialize_synced_data` would leave the synced default of 1.0 instead.
    assert_eq!(dragon.get_max_health(), 200.0);
    assert_eq!(dragon.get_health(), 200.0);
}

#[test]
fn owns_eight_parts_on_the_ids_after_its_own() {
    let dragon = test_dragon();
    let parts = dragon.sub_entities();

    assert_eq!(parts.len(), 8);
    for (index, part) in parts.iter().enumerate() {
        assert_eq!(part.id(), dragon.id() + index as i32 + 1);
    }
    assert_eq!(dragon.parts().len(), 8);
}

#[test]
fn parts_carry_their_own_vanilla_hitboxes() {
    let dragon = test_dragon();

    // head 1x1, neck 3x3, body 5x3, tail x3 2x2, wing x2 4x2.
    let expected: [(f64, f64); 8] = [
        (1.0, 1.0),
        (3.0, 3.0),
        (5.0, 3.0),
        (2.0, 2.0),
        (2.0, 2.0),
        (2.0, 2.0),
        (4.0, 2.0),
        (4.0, 2.0),
    ];

    for (part, (width, height)) in dragon.sub_entities().iter().zip(expected) {
        let box_ = part.bounding_box();
        assert!((box_.max_x() - box_.min_x() - width).abs() < 1.0e-9);
        assert!((box_.max_y() - box_.min_y() - height).abs() < 1.0e-9);
    }
}

#[test]
fn parts_are_pickable_but_the_dragon_body_is_not() {
    let dragon = test_dragon();

    // Vanilla splits these deliberately: arrows must hit the parts, not the
    // 16x8 box the dragon itself reports.
    assert!(!dragon.is_pickable());
    for part in dragon.sub_entities() {
        assert!(part.is_pickable());
    }
}

#[test]
fn parts_report_the_dragons_entity_type() {
    let dragon = test_dragon();

    // Vanilla constructs each part with `super(parentMob.getType(), …)`, which is
    // what makes fire immunity and the damage rules resolve through the dragon.
    for part in dragon.sub_entities() {
        assert_eq!(part.entity_type().key, vanilla_entities::ENDER_DRAGON.key);
    }
}

#[test]
fn parts_are_named_in_vanilla_order() {
    let dragon = test_dragon();
    let names = dragon
        .sub_entities()
        .iter()
        .map(|part| part.part_name())
        .collect::<Vec<_>>();

    assert_eq!(
        names,
        [
            "head", "neck", "body", "tail", "tail", "tail", "wing", "wing"
        ]
    );
}

#[test]
fn nbt_round_trips_the_death_timer() {
    let dragon = test_dragon();
    assert_eq!(dragon.dragon_death_time(), 0);

    let mut nbt = NbtCompound::new();
    dragon.save_additional(&mut nbt);

    assert_eq!(nbt.int("DragonDeathTime"), Some(0));
    assert_eq!(nbt.float("sitting_damage_received"), Some(0.0));
}

#[test]
fn the_dragon_passes_through_terrain() {
    let dragon = test_dragon();

    // Vanilla sets `noPhysics` in the constructor and handles walls itself, so the
    // shared collision path must not be moving it.
    assert!(dragon.no_physics());
}

#[test]
fn the_registry_reserves_the_whole_block_and_builds_a_working_dragon() {
    init_vanilla_registry();
    init_entities();

    // This is the path `/summon` takes. It has to reserve nine IDs, not one, or the
    // parts would alias the next entity spawned.
    let ids = ENTITIES.reserve_id(&vanilla_entities::ENDER_DRAGON);
    assert_eq!(ids.len(), 9);

    let entity = ENTITIES
        .create(
            &vanilla_entities::ENDER_DRAGON,
            ids.first(),
            DVec3::ZERO,
            Weak::new(),
        )
        .expect("the generated factory should build a dragon");

    assert_eq!(entity.parts().len(), 8);
    for (index, part) in entity.parts().iter().enumerate() {
        assert_eq!(part.id(), ids.part(index as u32));
    }
}
