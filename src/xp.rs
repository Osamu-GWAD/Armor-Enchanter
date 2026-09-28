//! XP actions read server packets only; sending an action never edits inventory.

use std::{collections::HashMap, sync::atomic::Ordering, time::{Duration, Instant}};
use azalea::{Client, entity::inventory::Inventory, inventory::{ItemStack, operations::ClickType}};
use azalea::protocol::packets::game::{ClientboundGamePacket,
    s_container_click::{HashedStack, ServerboundContainerClick},
    s_interact::InteractionHand, s_use_item::ServerboundUseItem};
use azalea_registry::builtin::ItemKind;
use crate::{BotState, enchanter::{EnchanterManager, safe_batch_size, smooth_look, swing_arm, total_xp_for_level}};
use tracing::{info, warn};

const ACK_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Default)]
pub(crate) struct ServerInventory {
    slots: HashMap<i16, ItemStack>,
    versions: HashMap<i16, u64>,
    revision: u64,
    full_updates: u64,
    state_id: u32,
}

impl ServerInventory {
    pub(crate) fn observe(&mut self, packet: &ClientboundGamePacket) {
        match packet {
            ClientboundGamePacket::ContainerSetContent(p) if p.container_id == 0 && p.items.len() == 46 => {
                self.revision += 1;
                self.full_updates += 1;
                self.state_id = p.state_id;
                self.slots.clear();
                for (slot, item) in p.items.iter().enumerate() {
                    self.slots.insert(slot as i16, item.clone());
                    self.versions.insert(slot as i16, self.revision);
                }
            }
            ClientboundGamePacket::ContainerSetSlot(p) if p.container_id == 0 && p.slot < 46 => {
                self.state_id = p.state_id;
                self.set_slot(p.slot as i16, &p.item_stack);
            }
            ClientboundGamePacket::ContainerSetSlot(p) if p.container_id == -2 => {
                if let Some(slot) = crate::armor::player_menu_slot(p.slot as u32) {
                    self.set_slot(slot, &p.item_stack);
                }
            }
            ClientboundGamePacket::SetPlayerInventory(p) => {
                if let Some(slot) = crate::armor::player_menu_slot(p.slot) {
                    self.set_slot(slot, &p.contents);
                }
            }
            _ => {}
        }
    }

    fn set_slot(&mut self, slot: i16, item: &ItemStack) {
        self.revision += 1;
        self.slots.insert(slot, item.clone());
        self.versions.insert(slot, self.revision);
    }

    fn fresh_slot(&self, slot: i16, since: u64) -> bool {
        self.versions.get(&slot).is_some_and(|version| *version > since)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Outcome { Reached, OutOfBottles, Unconfirmed }

fn player_menu_ready(bot: &Client) -> bool {
    bot.get_component::<Inventory>().is_some_and(|inv| inv.id == 0 && inv.carried == ItemStack::Empty)
}

/// Swap hotbar 0 with itself: no item moves. An out-of-range state ID requests
/// the server's full resynchronization path (vanilla state IDs wrap at 32767).
fn request_snapshot(bot: &Client) -> bool {
    if !player_menu_ready(bot) { return false; }
    bot.write_packet(ServerboundContainerClick {
        container_id: 0, state_id: 32768, slot_num: 36, button_num: 0,
        click_type: ClickType::Swap, changed_slots: Default::default(), carried_item: HashedStack(None),
    });
    true
}

async fn wait_for_inventory(
    bot: &Client, state: &BotState, timeout: Duration,
    confirmed: impl Fn(&ServerInventory) -> bool,
) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if !*state.spawned.lock().await || !player_menu_ready(bot) { return false; }
        if confirmed(&*state.xp_inventory.lock().await) { return true; }
        if Instant::now() >= deadline { return false; }
        crate::wait_for_inventory_update(state, Duration::from_millis(50)).await;
    }
}

#[derive(Debug)]
struct PendingSwap {
    source: i16,
    bottles: ItemStack,
    displaced: ItemStack,
    revision: u64,
}

impl PendingSwap {
    fn confirmed(&self, inventory: &ServerInventory) -> bool {
        inventory.fresh_slot(self.source, self.revision)
            && inventory.fresh_slot(36, self.revision)
            && inventory.slots.get(&self.source) == Some(&self.displaced)
            && inventory.slots.get(&36) == Some(&self.bottles)
    }
}

fn send_swap(bot: &Client, inventory: &ServerInventory, source: i16) -> Option<PendingSwap> {
    if !player_menu_ready(bot) || !(9..=35).contains(&source) { return None; }
    let bottles = inventory.slots.get(&source)?.clone();
    if bottle_count(&bottles).is_none() { return None; }
    let pending = PendingSwap {
        source, bottles, displaced: inventory.slots.get(&36)?.clone(), revision: inventory.revision,
    };
    bot.write_packet(ServerboundContainerClick {
        container_id: 0, state_id: inventory.state_id, slot_num: source, button_num: 0,
        click_type: ClickType::Swap, changed_slots: Default::default(), carried_item: HashedStack(None),
    });
    Some(pending)
}

fn bottle_count(item: &ItemStack) -> Option<u32> {
    match item {
        ItemStack::Present(data) if data.kind == ItemKind::ExperienceBottle && data.count > 0 => Some(data.count as u32),
        _ => None,
    }
}

/// Records requests in flight, without decrementing the server's count.
struct PendingUses {
    slot: i16,
    before: ItemStack,
    count: u32,
    sent: u32,
    revision: u64,
}

impl PendingUses {
    fn new(inventory: &ServerInventory, slot: i16) -> Option<Self> {
        if !(36..=44).contains(&slot) { return None; }
        let before = inventory.slots.get(&slot)?.clone();
        let count = bottle_count(&before)?;
        Some(Self { slot, before, count, sent: 0, revision: inventory.revision })
    }

    fn remaining(&self, inventory: &ServerInventory) -> Option<u32> {
        match (inventory.slots.get(&self.slot)?, &self.before) {
            (ItemStack::Empty, _) => Some(0),
            (ItemStack::Present(now), ItemStack::Present(before))
                if now.kind == before.kind && now.component_patch == before.component_patch && now.count >= 0 => Some(now.count as u32),
            _ => None,
        }
    }

    fn can_send(&self, inventory: &ServerInventory) -> bool {
        self.sent < self.count && self.remaining(inventory).is_some_and(|remaining|
            remaining > 0 && remaining <= self.count && remaining >= self.count - self.sent)
    }

    fn confirmed(&self, inventory: &ServerInventory) -> bool {
        self.sent > 0 && inventory.fresh_slot(self.slot, self.revision)
            && self.remaining(inventory) == Some(self.count - self.sent)
    }
}

fn send_use(bot: &Client, inventory: &ServerInventory, pending: &mut PendingUses, yaw: f32) -> bool {
    let selected = bot.get_component::<Inventory>().is_some_and(|inv|
        inv.id == 0 && inv.carried == ItemStack::Empty && inv.selected_hotbar_slot as i16 == pending.slot - 36);
    if !selected || !pending.can_send(inventory) { return false; }
    bot.write_packet(ServerboundUseItem { hand: InteractionHand::MainHand, seq: 0, y_rot: yaw, x_rot: 90.0 });
    pending.sent += 1;
    swing_arm(bot);
    true
}

pub(crate) async fn throw_bottles(bot: &Client, state: &BotState, target: u32) -> Outcome {
    let target = target.min(39);
    if state.current_level.load(Ordering::SeqCst) >= target { return Outcome::Reached; }
    let previous_full = {
        let inventory = state.xp_inventory.lock().await;
        if !request_snapshot(bot) { return Outcome::Unconfirmed; }
        inventory.full_updates
    };
    if !wait_for_inventory(bot, state, ACK_TIMEOUT, |inv| inv.full_updates > previous_full).await {
        warn!("XP inventory refresh was not confirmed.");
        return Outcome::Unconfirmed;
    }
    let yaw = bot.direction().y_rot();
    smooth_look(bot, yaw, 90.0).await;
    loop {
        if !*state.spawned.lock().await || !player_menu_ready(bot) { return Outcome::Unconfirmed; }
        if state.current_level.load(Ordering::SeqCst) >= target { return Outcome::Reached; }
        let (slot, pending_swap) = {
            let inventory = state.xp_inventory.lock().await;
            let Some((slot, _)) = EnchanterManager::find_lowest_count_xp_slot(&inventory.slots, Some(bot)) else {
                return Outcome::OutOfBottles;
            };
            if slot < 36 {
                let Some(pending) = send_swap(bot, &inventory, slot) else { return Outcome::Unconfirmed; };
                (36, Some(pending))
            } else { (slot, None) }
        };
        if let Some(pending) = pending_swap {
            if !wait_for_inventory(bot, state, ACK_TIMEOUT, |inv| pending.confirmed(inv)).await {
                warn!("XP hotbar swap was not confirmed in both slots; no bottles used.");
                return Outcome::Unconfirmed;
            }
        }
        bot.set_selected_hotbar_slot((slot - 36) as u8);
        bot.wait_ticks(1).await;
        let mut uses = {
            let inventory = state.xp_inventory.lock().await;
            let Some(uses) = PendingUses::new(&inventory, slot) else { return Outcome::Unconfirmed; };
            uses
        };
        let xp_before = state.total_experience.load(Ordering::SeqCst);
        let deficit = total_xp_for_level(target).saturating_sub(xp_before);
        let limit = safe_batch_size(deficit).max(1).min(uses.count);
        for _ in 0..limit {
            if !*state.spawned.lock().await { return Outcome::Unconfirmed; }
            if state.current_level.load(Ordering::SeqCst) >= target { break; }
            {
                let inventory = state.xp_inventory.lock().await;
                if !send_use(bot, &inventory, &mut uses, yaw) { return Outcome::Unconfirmed; }
            }
            bot.wait_ticks(1).await;
        }
        if uses.sent == 0 { return Outcome::Reached; }
        if !wait_for_inventory(bot, state, ACK_TIMEOUT, |inv| uses.confirmed(inv)).await {
            warn!("XP bottle consumption was not fully confirmed; preserving server counts.");
            return Outcome::Unconfirmed;
        }
        // Wait for experience too: inventory acknowledgement does not mean orbs
        // have been collected. Never start another batch against unchanged XP.
        let deadline = Instant::now() + ACK_TIMEOUT;
        while state.total_experience.load(Ordering::SeqCst) <= xp_before {
            if !*state.spawned.lock().await || !player_menu_ready(bot) || Instant::now() >= deadline {
                warn!("Bottles consumed but XP gain was not confirmed.");
                return Outcome::Unconfirmed;
            }
            let _ = tokio::time::timeout(Duration::from_millis(50), state.experience_updated.notified()).await;
        }
        info!("Server confirmed consumption of {} XP bottle(s); level now {}.", uses.sent, state.current_level.load(Ordering::SeqCst));
        bot.wait_ticks(2).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex as StdMutex};
    use tokio::sync::Mutex;
    use azalea::protocol::packets::game::{ClientboundContainerSetContent, ClientboundContainerSetSlot,
        ClientboundSetPlayerInventory, ServerboundGamePacket};

    fn client() -> (Client, Arc<StdMutex<Vec<ServerboundGamePacket>>>) {
        let packets = Arc::new(StdMutex::new(Vec::new()));
        let observed = packets.clone();
        let mut app = azalea::app::App::new();
        app.add_plugins(azalea::inventory::InventoryPlugin);
        app.world_mut().add_observer(move |event: azalea::ecs::prelude::On<azalea::packet::game::SendGamePacketEvent>| {
            observed.lock().unwrap().push(event.packet.clone());
        });
        let entity = app.world_mut().spawn(Inventory::default()).id();
        let world = std::mem::take(app.world_mut());
        (Client::new(entity, Arc::new(world.into())), packets)
    }

    fn snapshot(source: i16, count: i32) -> ClientboundGamePacket {
        let mut items = vec![ItemStack::Empty; 46];
        items[source as usize] = ItemStack::new(ItemKind::ExperienceBottle, count);
        if source != 36 { items[36] = ItemStack::new(ItemKind::DiamondHelmet, 1); }
        ClientboundGamePacket::ContainerSetContent(ClientboundContainerSetContent {
            container_id: 0, state_id: 12, items, carried_item: ItemStack::Empty,
        })
    }

    fn slot_update(slot: i16, item: ItemStack) -> ClientboundGamePacket {
        ClientboundGamePacket::ContainerSetSlot(ClientboundContainerSetSlot {
            container_id: 0, state_id: 13, slot: slot as u16, item_stack: item,
        })
    }

    #[test]
    fn swaps_require_both_server_slots_and_do_not_predict_inventory() {
        for source_first in [false, true] {
            let (bot, packets) = client();
            let mut inventory = ServerInventory::default();
            inventory.observe(&snapshot(9, 64));
            let before = inventory.slots.clone();
            let pending = send_swap(&bot, &inventory, 9).unwrap();
            assert_eq!(inventory.slots, before);
            assert!(!pending.confirmed(&inventory));
            // An unchanged resync/rejected click cannot confirm the swap.
            inventory.observe(&snapshot(9, 64));
            assert!(!pending.confirmed(&inventory));
            let source = slot_update(9, before[&36].clone());
            let destination = slot_update(36, before[&9].clone());
            inventory.observe(if source_first { &source } else { &destination });
            assert!(!pending.confirmed(&inventory));
            inventory.observe(if source_first { &destination } else { &source });
            assert!(pending.confirmed(&inventory));
            bot.ecs.write().flush();
            let packets = packets.lock().unwrap();
            assert_eq!(packets.len(), 1, "no UseItem before both slots confirm");
            let ServerboundGamePacket::ContainerClick(click) = &packets[0] else { panic!("expected swap"); };
            assert_eq!(click.state_id, 12);
            assert_eq!(click.click_type, ClickType::Swap);
            assert_eq!(click.slot_num, 9);
        }
    }

    #[tokio::test]
    async fn partial_consumption_keeps_server_counts_and_other_inventory_updates() {
        let (bot, _) = client();
        let state = BotState { gui: Arc::new(Mutex::new(crate::gui::GuiManager::new())), ..Default::default() };
        *state.spawned.lock().await = true;
        crate::handle_packet(&bot, &Arc::new(snapshot(36, 64)), &state).await;
        let mut pending = {
            let inventory = state.xp_inventory.lock().await;
            let mut pending = PendingUses::new(&inventory, 36).unwrap();
            for _ in 0..10 { assert!(send_use(&bot, &inventory, &mut pending, 0.0)); }
            assert_eq!(bottle_count(&inventory.slots[&36]), Some(64));
            pending
        };
        // The server accepted six uses, and independently updated another slot.
        crate::handle_packet(&bot, &Arc::new(ClientboundGamePacket::SetPlayerInventory(ClientboundSetPlayerInventory {
            slot: 0, contents: ItemStack::new(ItemKind::ExperienceBottle, 58),
        })), &state).await;
        let helmet = ItemStack::new(ItemKind::DiamondHelmet, 1);
        crate::handle_packet(&bot, &Arc::new(slot_update(10, helmet.clone())), &state).await;
        assert!(!wait_for_inventory(&bot, &state, Duration::from_millis(1), |inv| pending.confirmed(inv)).await);
        {
            let inv = state.xp_inventory.lock().await;
            assert_eq!(bottle_count(&inv.slots[&36]), Some(58));
            assert_eq!(inv.slots[&10], helmet);
        }
        assert_eq!(state.gui.lock().await.player_inventory[&36], ItemStack::new(ItemKind::ExperienceBottle, 58));
        assert_eq!(state.enchanter.lock().await.player_inventory[&10], helmet);
        // A delayed server update completes the original batch; no estimates are copied back.
        crate::handle_packet(&bot, &Arc::new(slot_update(36, ItemStack::new(ItemKind::ExperienceBottle, 54))), &state).await;
        assert!(wait_for_inventory(&bot, &state, Duration::from_millis(1), |inv| pending.confirmed(inv)).await);
        // A changed held item must never be used as if it were the XP stack.
        crate::handle_packet(&bot, &Arc::new(slot_update(36, helmet)), &state).await;
        assert!(!send_use(&bot, &*state.xp_inventory.lock().await, &mut pending, 0.0));
        *state.spawned.lock().await = false;
        assert!(!wait_for_inventory(&bot, &state, Duration::from_millis(1), |_| true).await);
    }

    #[test]
    fn confirmed_last_bottle_requires_fresh_empty_slot_and_no_extra_use() {
        let (bot, _) = client();
        let mut inventory = ServerInventory::default();
        inventory.observe(&snapshot(36, 1));
        let mut pending = PendingUses::new(&inventory, 36).unwrap();
        assert!(send_use(&bot, &inventory, &mut pending, 0.0));
        assert!(!send_use(&bot, &inventory, &mut pending, 0.0));
        assert!(!pending.confirmed(&inventory));
        inventory.observe(&ClientboundGamePacket::ContainerSetSlot(ClientboundContainerSetSlot {
            container_id: -2, state_id: 0, slot: 0, item_stack: ItemStack::Empty,
        }));
        assert!(pending.confirmed(&inventory));
    }

    #[test]
    fn refresh_is_no_op_and_anvil_close_updates_active_menu() {
        let (bot, packets) = client();
        bot.ecs.write().get_mut::<Inventory>(bot.entity).unwrap().id = 7;
        assert!(!request_snapshot(&bot));
        let mut enchanter = EnchanterManager::new();
        enchanter.anvil_container_id = Some(7);
        enchanter.close_anvil(&bot, 7);
        bot.ecs.write().flush();
        assert!(request_snapshot(&bot));
        bot.ecs.write().flush();
        {
            let packets = packets.lock().unwrap();
            let ServerboundGamePacket::ContainerClick(click) = packets.last().unwrap() else { panic!("expected refresh"); };
            assert_eq!(click.slot_num, 36 + click.button_num as i16);
            assert_eq!(click.state_id, 32768);
            assert_eq!(click.click_type, ClickType::Swap);
        }
        bot.ecs.write().get_mut::<Inventory>(bot.entity).unwrap().carried = ItemStack::new(ItemKind::DiamondHelmet, 1);
        assert!(!request_snapshot(&bot));
    }

    #[test]
    fn wrong_container_and_wrong_selected_slot_cannot_authorize_xp_use() {
        let (bot, _) = client();
        let mut inventory = ServerInventory::default();
        inventory.observe(&snapshot(36, 64));
        let version = inventory.full_updates;
        let mut wrong_menu = snapshot(36, 1);
        if let ClientboundGamePacket::ContainerSetContent(p) = &mut wrong_menu { p.container_id = 7; }
        inventory.observe(&wrong_menu);
        assert_eq!(inventory.full_updates, version);
        assert_eq!(bottle_count(&inventory.slots[&36]), Some(64));
        let mut pending = PendingUses::new(&inventory, 36).unwrap();
        bot.ecs.write().get_mut::<Inventory>(bot.entity).unwrap().selected_hotbar_slot = 1;
        assert!(!send_use(&bot, &inventory, &mut pending, 0.0));
        bot.ecs.write().get_mut::<Inventory>(bot.entity).unwrap().id = 7;
        assert!(!send_use(&bot, &inventory, &mut pending, 0.0));
    }
}
