//! A drop is allowed only after the exact item is observed in the selected hand.
use std::collections::HashMap;
use azalea::inventory::ItemStack;

/// Ask for a server snapshot without moving, using, or dropping any item.
/// A hotbar slot swapped with itself is a no-op. Vanilla menu state IDs wrap
/// at 32767; 32768 deliberately requests the full-state resynchronization path.
/// This is needed because a successful Q drop may receive no slot update.
pub fn request_inventory_refresh(bot: &azalea::Client, hotbar: u8) -> bool {
    use azalea::entity::inventory::Inventory;
    use azalea::inventory::operations::ClickType;
    use azalea::protocol::packets::game::s_container_click::{HashedStack, ServerboundContainerClick};
    let allowed = hotbar < 9 && bot.get_component::<Inventory>()
        .is_some_and(|inventory| inventory.id == 0 && inventory.carried == ItemStack::Empty);
    if !allowed { return false; }
    bot.write_packet(ServerboundContainerClick {
        container_id: 0,
        state_id: 32768,
        slot_num: 36 + hotbar as i16,
        button_num: hotbar,
        click_type: ClickType::Swap,
        changed_slots: Default::default(),
        carried_item: HashedStack(None),
    });
    true
}

/// Keep Azalea's active menu in sync with the close packet.
pub fn close_menu(bot: &azalea::Client, container_id: i32) {
    let current_id = bot.get_component::<azalea::entity::inventory::Inventory>().map(|inventory| inventory.id);
    if current_id == Some(container_id) {
        bot.ecs.write().trigger(azalea::inventory::CloseContainerEvent {
            entity: bot.entity,
            id: container_id,
        });
    } else {
        bot.write_packet(azalea::protocol::packets::game::ServerboundContainerClose { container_id });
    }
}

pub struct DropStage {
    pub source: i16,
    pub hotbar: i16,
    pub expected: ItemStack,
    displaced: ItemStack,
}

impl DropStage {
    pub fn new(inventory: &HashMap<i16, ItemStack>, source: i16, expected: &ItemStack) -> Option<Self> {
        if !(9..=44).contains(&source)
            || !matches!(expected, ItemStack::Present(data) if data.count == 1)
            || inventory.get(&source) != Some(expected) {
            return None;
        }
        // Prefer an identical item already in hand to avoid a redundant swap.
        let hotbar = (36..=44).find(|slot| inventory.get(slot) == Some(expected))
            .or_else(|| (36..=44).find(|slot| inventory.get(slot) == Some(&ItemStack::Empty)))
            .unwrap_or(36);
        let source = if inventory.get(&hotbar) == Some(expected) { hotbar } else { source };
        Some(Self { source, hotbar, expected: expected.clone(), displaced: inventory.get(&hotbar)?.clone() })
    }

    pub fn needs_swap(&self) -> bool { self.source != self.hotbar }

    pub fn ready(&self, inventory: &HashMap<i16, ItemStack>) -> bool {
        inventory.get(&self.hotbar) == Some(&self.expected)
            && (!self.needs_swap() || inventory.get(&self.source) == Some(&self.displaced))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use azalea_registry::builtin::ItemKind;

    #[test]
    fn refresh_refuses_other_menus_and_out_of_range_hotbar_indices() {
        use azalea::entity::inventory::Inventory;
        let bot = crate::workflow_tests::local_client();
        assert!(!request_inventory_refresh(&bot, 9));
        bot.ecs.write().get_mut::<Inventory>(bot.entity).unwrap().id = 7;
        assert!(!request_inventory_refresh(&bot, 0));
        let mut world = bot.ecs.write();
        let mut inventory = world.get_mut::<Inventory>(bot.entity).unwrap();
        inventory.id = 0;
        inventory.carried = ItemStack::new(ItemKind::DiamondHelmet, 1);
        drop(world);
        assert!(!request_inventory_refresh(&bot, 0));
    }

    #[test]
    fn closing_a_menu_updates_azaleas_active_inventory() {
        use azalea::entity::inventory::Inventory;
        let mut app = azalea::app::App::new();
        app.add_plugins(azalea::inventory::InventoryPlugin);
        let entity = app.world_mut().spawn(Inventory { id: 7, ..Default::default() }).id();
        let world = std::mem::take(app.world_mut());
        let bot = azalea::Client::new(entity, std::sync::Arc::new(world.into()));
        close_menu(&bot, 7);
        bot.ecs.write().flush();
        assert_eq!(bot.component::<Inventory>().id, 0);
    }

    #[test]
    fn a_clean_book_is_not_a_substitute_for_the_rejected_repaired_book() {
        use azalea::inventory::components::{DataComponentUnion, RepairCost};
        use azalea_registry::builtin::DataComponentKind;
        let clean = ItemStack::new(ItemKind::EnchantedBook, 1);
        let mut rejected = clean.clone();
        if let ItemStack::Present(data) = &mut rejected {
            // The union variant and registry kind both represent RepairCost.
            unsafe {
                data.component_patch.unchecked_insert_component(
                    DataComponentKind::RepairCost,
                    Some(DataComponentUnion::from(RepairCost { cost: 31 })),
                );
            }
        }
        let slots = HashMap::from([(36, clean.clone()), (9, rejected.clone())]);
        assert!(DropStage::new(&slots, 36, &rejected).is_none());
        let stage = DropStage::new(&slots, 9, &rejected).unwrap();
        assert!(!stage.ready(&slots));
    }

    #[test]
    fn rejected_or_half_acknowledged_swap_never_authorizes_a_drop() {
        let helmet = ItemStack::new(ItemKind::DiamondHelmet, 1);
        let bottles = ItemStack::new(ItemKind::ExperienceBottle, 64);
        let mut slots: HashMap<_, _> = (36..=44).map(|s| (s, bottles.clone())).collect();
        slots.insert(9, helmet.clone());
        let stage = DropStage::new(&slots, 9, &helmet).unwrap();
        assert!(!stage.ready(&slots));
        slots.insert(36, helmet.clone());
        assert!(!stage.ready(&slots));
        slots.insert(9, bottles);
        assert!(stage.ready(&slots));
        slots.insert(36, ItemStack::new(ItemKind::EnchantedBook, 1));
        assert!(!stage.ready(&slots));
    }

    #[test]
    fn already_hotbar_items_skip_swaps_and_stale_or_stacked_items_are_rejected() {
        let helmet = ItemStack::new(ItemKind::DiamondHelmet, 1);
        let slots = HashMap::from([(40, helmet.clone())]);
        let stage = DropStage::new(&slots, 40, &helmet).unwrap();
        assert!(!stage.needs_swap());
        assert!(stage.ready(&slots));
        assert!(DropStage::new(&slots, 9, &helmet).is_none());
        let stack = ItemStack::new(ItemKind::ExperienceBottle, 64);
        assert!(DropStage::new(&HashMap::from([(40, stack.clone())]), 40, &stack).is_none());
    }
}
