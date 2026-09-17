//! Server inventory refresh and menu-close helpers for confirmed slot drops.

use azalea::inventory::ItemStack;

/// Ask for a server snapshot without moving, using, or dropping any item.
/// A hotbar slot swapped with itself is a no-op. Vanilla menu state IDs wrap
/// at 32767; 32768 deliberately requests the full-state resynchronization path.
/// This is needed because a successful drop may receive no slot update.
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

#[cfg(test)]
mod tests {
    use super::*;
    use azalea::entity::inventory::Inventory;
    use azalea_registry::builtin::ItemKind;

    #[test]
    fn refresh_refuses_other_menus_cursor_items_and_invalid_hotbar_indices() {
        let bot = crate::workflow_tests::local_client();
        assert!(!request_inventory_refresh(&bot, 9));
        bot.ecs.write().get_mut::<Inventory>(bot.entity).unwrap().id = 7;
        assert!(!request_inventory_refresh(&bot, 0));
        {
            let mut world = bot.ecs.write();
            let mut inventory = world.get_mut::<Inventory>(bot.entity).unwrap();
            inventory.id = 0;
            inventory.carried = ItemStack::new(ItemKind::DiamondHelmet, 1);
        }
        assert!(!request_inventory_refresh(&bot, 0));
    }

    #[test]
    fn closing_a_menu_updates_azaleas_active_inventory() {
        let mut app = azalea::app::App::new();
        app.add_plugins(azalea::inventory::InventoryPlugin);
        let entity = app.world_mut().spawn(Inventory { id: 7, ..Default::default() }).id();
        let world = std::mem::take(app.world_mut());
        let bot = azalea::Client::new(entity, std::sync::Arc::new(world.into()));
        close_menu(&bot, 7);
        bot.ecs.write().flush();
        assert_eq!(bot.component::<Inventory>().id, 0);
    }
}
