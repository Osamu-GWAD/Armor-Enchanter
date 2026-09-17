use crate::nbt::{
    ItemInfo, is_diamond_boots, is_diamond_chestplate, is_diamond_helmet, is_diamond_leggings,
};

pub const TYPES: [&str; 4] = ["Helmet", "Chestplate", "Leggings", "Boots"];

/// Continue the stocked batch only when another complete set of armor remains.
/// Enchanting itself determines which books/XP are still needed for that set.
pub fn has_armor_set(items: &[(i16, ItemInfo)]) -> bool {
    (0..4).all(|kind| {
        items
            .iter()
            .any(|(_, info)| info.count > 0 && armor_type(info) == Some(kind))
    })
}

pub fn armor_type(info: &ItemInfo) -> Option<usize> {
    if is_diamond_helmet(info) {
        Some(0)
    } else if is_diamond_chestplate(info) {
        Some(1)
    } else if is_diamond_leggings(info) {
        Some(2)
    } else if is_diamond_boots(info) {
        Some(3)
    } else {
        None
    }
}

pub fn is_complete(info: &ItemInfo) -> bool {
    armor_type(info).is_some_and(|kind| {
        info.has_enchantment(
            if kind < 2 {
                "protection"
            } else {
                "blast_protection"
            },
            4,
        ) && info.has_enchantment("unbreaking", 3)
            && info.has_enchantment("mending", 1)
    })
}

pub fn is_clean_armor(info: &ItemInfo) -> bool {
    let prior_works = (info.has_enchantment("protection", 4) || info.has_enchantment("blast_protection", 4)) as u32
        + (info.has_enchantment("unbreaking", 3) as u32)
        + (info.has_enchantment("mending", 1) as u32);
    let max_allowed_pwp = match prior_works {
        0 => 0,
        1 => 15,
        2 => 31,
        _ => 63,
    };
    info.repair_cost.unwrap_or(0) <= max_allowed_pwp
}

/// Pick one piece of each type per set. Finish the current type before moving on,
/// even when a later piece has more enchants or the current book is unavailable.
pub fn next_armor(items: &[(i16, ItemInfo)]) -> Option<&(i16, ItemInfo)> {
    for kind in 0..4 {
        let mut candidates: Vec<_> = items
            .iter()
            .filter(|(_, info)| armor_type(info) == Some(kind) && is_clean_armor(info))
            .collect();
        if candidates.iter().any(|(_, info)| is_complete(info)) {
            continue;
        }
        candidates.sort_by_key(|(slot, info)| {
            let progress = info.has_enchantment(
                if kind < 2 {
                    "protection"
                } else {
                    "blast_protection"
                },
                4,
            ) as i32
                + info.has_enchantment("unbreaking", 3) as i32
                + info.has_enchantment("mending", 1) as i32;
            (-progress, *slot)
        });
        return candidates.first().copied();
    }
    None
}

/// A stable drop plan contains exactly one completed item of each remaining type.
pub fn drop_plan(items: &[(i16, ItemInfo)], next: usize) -> Option<Vec<i16>> {
    (next..4)
        .map(|kind| {
            items
                .iter()
                .filter(|(slot, info)| {
                    (9..=44).contains(slot) && armor_type(info) == Some(kind) && is_complete(info)
                })
                .map(|(slot, _)| *slot)
                .min()
        })
        .collect()
}

#[derive(Debug, PartialEq, Eq)]
pub enum DropStatus {
    Confirmed,
    Retained,
    Ambiguous,
}

pub fn drop_status(before: usize, remaining: usize) -> DropStatus {
    if before > 0 && remaining == before - 1 {
        DropStatus::Confirmed
    } else if remaining == before {
        DropStatus::Retained
    } else {
        DropStatus::Ambiguous
    }
}

/// Protocol player-inventory indices differ from the player menu's slot numbers.
pub fn player_menu_slot(slot: u32) -> Option<i16> {
    match slot {
        0..=8 => Some(slot as i16 + 36),
        9..=35 => Some(slot as i16),
        36..=39 => Some(44 - slot as i16),
        40 => Some(45),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn piece(kind: usize, complete: bool) -> ItemInfo {
        let mut info = ItemInfo {
            kind: format!("Diamond{}", TYPES[kind]),
            count: 1,
            ..Default::default()
        };
        if complete {
            info.enchantments.insert(
                if kind < 2 {
                    "protection"
                } else {
                    "blast_protection"
                }
                .into(),
                4,
            );
            info.enchantments.insert("unbreaking".into(), 3);
            info.enchantments.insert("mending".into(), 1);
        }
        info
    }

    #[test]
    fn enchanting_is_serial_even_with_partial_later_pieces_and_duplicates() {
        let mut chest = piece(1, false);
        chest.enchantments.insert("protection".into(), 4);
        let mut items = vec![
            (10, chest),
            (40, piece(0, false)),
            (9, piece(0, false)),
            (12, piece(2, false)),
            (13, piece(3, false)),
        ];
        assert_eq!(next_armor(&items).unwrap().0, 9);
        items[2].1 = piece(0, true);
        assert_eq!(next_armor(&items).unwrap().0, 10);
        items[0].1 = piece(1, true);
        assert_eq!(next_armor(&items).unwrap().0, 12);
        items[3].1 = piece(2, true);
        assert_eq!(next_armor(&items).unwrap().0, 13);
        items[4].1 = piece(3, true);
        assert!(next_armor(&items).is_none());
    }

    #[test]
    fn missing_earlier_piece_blocks_later_pieces() {
        assert!(next_armor(&[(9, piece(1, false))]).is_none());
    }

    #[test]
    fn two_stocked_sets_are_enchanted_and_dropped_in_serial_order() {
        // Mixed inventory layout, including two copies of every armor type.
        let mut items: Vec<_> = [3, 1, 0, 2, 0, 3, 2, 1]
            .iter()
            .enumerate()
            .map(|(index, &kind)| (9 + index as i16, piece(kind, false)))
            .collect();
        let mut dropped_types = Vec::new();
        for _ in 0..2 {
            assert!(has_armor_set(&items));
            for kind in 0..4 {
                let (slot, info) = next_armor(&items).unwrap();
                assert_eq!(armor_type(info), Some(kind));
                let slot = *slot;
                items
                    .iter_mut()
                    .find(|(item_slot, _)| *item_slot == slot)
                    .unwrap()
                    .1 = piece(kind, true);
            }
            assert!(next_armor(&items).is_none());
            for slot in drop_plan(&items, 0).unwrap() {
                let index = items
                    .iter()
                    .position(|(item_slot, _)| *item_slot == slot)
                    .unwrap();
                dropped_types.push(armor_type(&items.remove(index).1).unwrap());
            }
        }
        assert_eq!(dropped_types, vec![0, 1, 2, 3, 0, 1, 2, 3]);
        assert!(!has_armor_set(&items));
        assert!(items.is_empty());
    }

    #[test]
    fn leftover_duplicates_do_not_start_an_incomplete_second_set() {
        assert!(!has_armor_set(&[
            (9, piece(0, false)),
            (10, piece(0, false))
        ]));
    }

    #[test]
    fn drops_one_full_set_in_order_independent_of_inventory_layout() {
        let items = vec![
            (36, piece(3, true)),
            (11, piece(0, true)),
            (44, piece(2, true)),
            (9, piece(1, true)),
            (12, piece(0, true)),
        ];
        assert_eq!(drop_plan(&items, 0), Some(vec![11, 9, 44, 36]));
        assert_eq!(drop_plan(&items, 2), Some(vec![44, 36]));
        assert!(drop_plan(&items[1..], 0).is_none());
        assert!(drop_plan(&[(9, piece(0, false))], 0).is_none());
    }

    #[test]
    fn rejected_or_ambiguous_drops_do_not_advance_the_sequence() {
        assert_eq!(drop_status(1, 0), DropStatus::Confirmed);
        assert_eq!(drop_status(2, 1), DropStatus::Confirmed);
        assert_eq!(drop_status(1, 1), DropStatus::Retained);
        assert_eq!(drop_status(2, 0), DropStatus::Ambiguous);
        assert_eq!(drop_status(1, 2), DropStatus::Ambiguous);
        assert_eq!(drop_status(0, 0), DropStatus::Retained);
    }

    #[test]
    fn protocol_inventory_slots_map_to_the_correct_menu_slots() {
        assert_eq!(player_menu_slot(0), Some(36));
        assert_eq!(player_menu_slot(8), Some(44));
        assert_eq!(player_menu_slot(9), Some(9));
        assert_eq!(player_menu_slot(35), Some(35));
        assert_eq!(player_menu_slot(36), Some(8));
        assert_eq!(player_menu_slot(39), Some(5));
        assert_eq!(player_menu_slot(40), Some(45));
        assert_eq!(player_menu_slot(41), None);
    }
}
