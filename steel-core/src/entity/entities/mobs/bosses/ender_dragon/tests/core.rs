use std::io::Cursor;
use std::sync::Weak;

use glam::DVec3;
use simdnbt::borrow::read_compound as read_borrowed_compound;
use simdnbt::owned::NbtCompound;
use steel_registry::init_vanilla_registry;
use steel_registry::{vanilla_damage_types, vanilla_entities};
use steel_utils::Downcast as _;

use steel_registry::vanilla_entity_data::EnderDragonEntityData;

use crate::entity::damage::DamageSource;
use crate::entity::entities::EnderDragonEntity;
use crate::entity::entities::mobs::bosses::ender_dragon::EnderDragonPart;
use crate::entity::entities::mobs::bosses::ender_dragon::EnderDragonPhase;
use crate::entity::{ENTITIES, Entity, LivingEntity, init_entities, reserve_entity_ids};
use crate::test_support::test_world;

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

#[test]
fn phase_ids_match_the_wire_order() {
    // These are what `DATA_PHASE` carries, and a vanilla client drives its own copy
    // of the phase machine from them, so a reorder is a protocol break.
    let expected = [
        (EnderDragonPhase::HoldingPattern, 0),
        (EnderDragonPhase::StrafePlayer, 1),
        (EnderDragonPhase::LandingApproach, 2),
        (EnderDragonPhase::Landing, 3),
        (EnderDragonPhase::Takeoff, 4),
        (EnderDragonPhase::SittingFlaming, 5),
        (EnderDragonPhase::SittingScanning, 6),
        (EnderDragonPhase::SittingAttacking, 7),
        (EnderDragonPhase::ChargingPlayer, 8),
        (EnderDragonPhase::Dying, 9),
        (EnderDragonPhase::Hovering, 10),
    ];

    for (phase, id) in expected {
        assert_eq!(phase.id(), id);
        assert_eq!(EnderDragonPhase::by_id(id), phase);
    }
}

#[test]
fn an_unknown_phase_id_falls_back_to_the_holding_pattern() {
    // Vanilla's `getById` never fails; it returns the holding pattern.
    assert_eq!(
        EnderDragonPhase::by_id(-1),
        EnderDragonPhase::HoldingPattern
    );
    assert_eq!(
        EnderDragonPhase::by_id(11),
        EnderDragonPhase::HoldingPattern
    );
}

#[test]
fn a_dragon_starts_hovering() {
    let dragon = test_dragon();

    assert_eq!(
        dragon.phase_manager().current_phase(),
        EnderDragonPhase::Hovering
    );
    // The generated synced-data default has to agree, or the client would animate a
    // different phase than the server is running until the first change.
    assert_eq!(
        EnderDragonEntityData::new().phase.get(),
        &EnderDragonPhase::Hovering.id()
    );
}

#[test]
fn hovering_counts_as_sitting() {
    let dragon = test_dragon();

    // Not just bookkeeping: this selects the perched wing beat and the lowered head.
    assert!(dragon.phase_manager().current().is_sitting());
}

#[test]
fn switching_to_the_current_phase_does_nothing() {
    let dragon = test_dragon();

    dragon
        .phase_manager()
        .set_phase(&dragon, EnderDragonPhase::Hovering);

    assert_eq!(
        dragon.phase_manager().current_phase(),
        EnderDragonPhase::Hovering
    );
}

#[test]
fn switching_phase_publishes_the_new_id() {
    let dragon = test_dragon();

    dragon
        .phase_manager()
        .set_phase(&dragon, EnderDragonPhase::HoldingPattern);

    assert_eq!(
        dragon.phase_manager().current_phase(),
        EnderDragonPhase::HoldingPattern
    );
    assert_eq!(dragon.synced_phase(), EnderDragonPhase::HoldingPattern.id());
}

#[test]
fn a_phase_can_switch_the_dragon_out_of_itself_without_deadlocking() {
    let dragon = test_dragon();
    let manager = dragon.phase_manager();

    // This is the shape that deadlocks if the manager holds a lock across a phase
    // call: the switch re-enters the manager and runs `end` on the phase that is
    // still executing. `parking_lot` mutexes are not reentrant, so this would hang
    // the world tick rather than fail a test.
    manager.set_phase(&dragon, EnderDragonPhase::HoldingPattern);
    manager.set_phase(&dragon, EnderDragonPhase::Dying);
    manager.set_phase(&dragon, EnderDragonPhase::Hovering);

    assert_eq!(manager.current_phase(), EnderDragonPhase::Hovering);
}

#[test]
fn unported_phases_leave_the_dragon_coasting_rather_than_failing() {
    let dragon = test_dragon();

    // Landing is a placeholder this pass. Reporting no fly target is what makes an
    // unimplemented phase degrade to a stationary dragon instead of a panic.
    dragon
        .phase_manager()
        .set_phase(&dragon, EnderDragonPhase::Landing);

    assert!(
        dragon
            .phase_manager()
            .current()
            .fly_target_location()
            .is_none()
    );
}

#[test]
fn a_dying_dragon_ignores_further_damage() {
    let dragon = test_dragon();
    dragon
        .phase_manager()
        .set_phase(&dragon, EnderDragonPhase::Dying);

    // Vanilla refuses damage outright while dying, so the death animation always
    // plays out rather than being cut short by whatever killed it.
    let source = DamageSource::environment(&vanilla_damage_types::GENERIC);
    let head = dragon.sub_entities()[0].as_ref();
    let head = head
        .downcast_ref::<EnderDragonPart>()
        .expect("the first sub-entity is the head");

    assert!(!dragon.hurt_part(test_world(), head, &source, 10.0));
    assert_eq!(dragon.get_health(), 200.0);
}

#[test]
fn only_players_and_explosions_can_hurt_the_dragon() {
    let dragon = test_dragon();
    let head = dragon.sub_entities()[0].as_ref();
    let head = head
        .downcast_ref::<EnderDragonPart>()
        .expect("the first sub-entity is the head");

    // Environmental damage reports as handled but must not land, which is what stops
    // the dragon being worn down by fire or drowning.
    let ignored = DamageSource::environment(&vanilla_damage_types::GENERIC);
    assert!(dragon.hurt_part(test_world(), head, &ignored, 10.0));
    assert_eq!(dragon.get_health(), 200.0);

    // Explosions carry the tag that always hurts dragons.
    let explosion = DamageSource::environment(&vanilla_damage_types::EXPLOSION);
    assert!(dragon.hurt_part(test_world(), head, &explosion, 10.0));
    assert!(dragon.get_health() < 200.0);
}

#[test]
fn the_phase_round_trips_through_nbt() {
    let dragon = test_dragon();
    dragon
        .phase_manager()
        .set_phase(&dragon, EnderDragonPhase::HoldingPattern);

    let mut nbt = NbtCompound::new();
    dragon.save_additional(&mut nbt);
    assert_eq!(
        nbt.int("DragonPhase"),
        Some(EnderDragonPhase::HoldingPattern.id())
    );

    let restored = test_dragon();
    let mut buffer = Vec::new();
    nbt.write(&mut buffer);
    let parsed = read_borrowed_compound(&mut Cursor::new(&buffer[..]))
        .expect("the saved dragon nbt should parse");
    restored.load_additional((&parsed).into());

    assert_eq!(
        restored.phase_manager().current_phase(),
        EnderDragonPhase::HoldingPattern
    );
}
