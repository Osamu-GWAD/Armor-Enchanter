use crate::nbt::{
    inspect_item, inspect_item_with_bot, is_anvil, is_blast_protection_4, is_confirm_button,
    is_diamond_boots, is_diamond_chestplate, is_diamond_helmet, is_diamond_leggings, is_mending,
    is_protection_4, is_unbreaking_3, is_xp_bottle, is_your_orders_button, normalize_small_caps,
    strip_color_and_normalize, ItemInfo,
};

use azalea::inventory::operations::ClickType;
use azalea::inventory::ItemStack;
use azalea::protocol::packets::game::s_container_click::{HashedStack, ServerboundContainerClick};
use azalea::protocol::packets::game::ServerboundContainerClose;
use azalea::Client;
use std::collections::HashMap;
use tracing::{info, warn};

/// Quota of items to retrieve from the `/order` delivery menu.
#[derive(Debug, Clone)]
pub struct WithdrawalQuota {
    pub anvils_needed: u32,
    pub mending_needed: u32,
    pub unbreaking_3_needed: u32,
    pub protection_4_needed: u32,
    pub blast_protection_4_needed: u32,
    pub xp_stacks_needed: u32,
    pub diamond_helmets_needed: u32,
    pub diamond_chestplates_needed: u32,
    pub diamond_leggings_needed: u32,
    pub diamond_boots_needed: u32,
}

impl Default for WithdrawalQuota {
    fn default() -> Self {
        Self {
            anvils_needed: 1,
            mending_needed: 4,
            unbreaking_3_needed: 4,
            protection_4_needed: 2,
            blast_protection_4_needed: 2,
            xp_stacks_needed: 2,
            diamond_helmets_needed: 1,
            diamond_chestplates_needed: 1,
            diamond_leggings_needed: 1,
            diamond_boots_needed: 1,
        }
    }
}

/// Tracks the items collected so far into inventory.
#[derive(Debug, Clone, Default)]
pub struct CollectedItems {
    pub anvils: u32,
    pub mending: u32,
    pub unbreaking_3: u32,
    pub protection_4: u32,
    pub blast_protection_4: u32,
    pub xp_bottles: u32,
    pub xp_stacks: u32,
    pub diamond_helmets: u32,
    pub diamond_chestplates: u32,
    pub diamond_leggings: u32,
    pub diamond_boots: u32,
}

impl CollectedItems {
    pub fn is_fulfilled(&self, quota: &WithdrawalQuota) -> bool {
        let xp_met = self.xp_bottles >= quota.xp_stacks_needed * 64
            || self.xp_stacks >= quota.xp_stacks_needed
            || (quota.xp_stacks_needed <= 2 && self.xp_bottles >= 106 && self.xp_stacks >= 2);

        self.anvils >= quota.anvils_needed
            && self.mending >= quota.mending_needed
            && self.unbreaking_3 >= quota.unbreaking_3_needed
            && self.protection_4 >= quota.protection_4_needed
            && self.blast_protection_4 >= quota.blast_protection_4_needed
            && xp_met
            && self.diamond_helmets >= quota.diamond_helmets_needed
            && self.diamond_chestplates >= quota.diamond_chestplates_needed
            && self.diamond_leggings >= quota.diamond_leggings_needed
            && self.diamond_boots >= quota.diamond_boots_needed
    }

    pub fn total_diamond_armor(&self) -> u32 {
        self.diamond_helmets + self.diamond_chestplates + self.diamond_leggings + self.diamond_boots
    }
}

/// Execution phase of withdrawal:
/// 1. AnvilPlacement: withdraw ONLY 1 Anvil from orders, then close GUI and place it.
/// 2. ItemsRetrieval: after anvil is placed, withdraw books, XP bottles, and 1 piece of each armor type.
/// 3. Done: all required items collected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WithdrawalPhase {
    AnvilPlacement,
    ItemsRetrieval,
    Done,
}

/// State of the GUI workflow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrderWorkflowState {
    WaitingForSpawn,
    Spawned,
    OpenedOrderMainMenu,
    NavigatingToYourOrders,
    OpenedYourOrdersMenu,
    InOrderSubmenu,
    InCollectDeliveryMenu,
    WaitingForNextOrder,
    WithdrawalComplete,
}

pub struct GuiManager {
    pub state: OrderWorkflowState,
    pub phase: WithdrawalPhase,
    pub current_container_id: i32,
    pub current_state_id: u32,
    pub open_container_size: i16,
    pub current_slots: HashMap<i16, ItemStack>,
    pub player_inventory: HashMap<i16, ItemStack>,
    pub quota: WithdrawalQuota,
    pub collected: CollectedItems,
    pub target_order_type: Option<String>,
    pub action_in_progress: bool,
    pub current_menu_skipped_slots: Vec<i16>,
}

impl GuiManager {
    pub fn new() -> Self {
        Self {
            state: OrderWorkflowState::WaitingForSpawn,
            phase: WithdrawalPhase::AnvilPlacement,
            current_container_id: 0,
            current_state_id: 0,
            open_container_size: 0,
            current_slots: HashMap::new(),
            player_inventory: HashMap::new(),
            quota: WithdrawalQuota::default(),
            collected: CollectedItems::default(),
            target_order_type: None,
            action_in_progress: false,
            current_menu_skipped_slots: Vec::new(),
        }
    }

    /// Check if an order item is needed in the current phase and quota.
    pub fn is_order_needed(&self, info: &ItemInfo) -> (bool, &'static str) {
        match self.phase {
            WithdrawalPhase::AnvilPlacement => {
                if is_anvil(info) && self.collected.anvils < self.quota.anvils_needed {
                    (true, "Anvil")
                } else {
                    (false, "")
                }
            }
            WithdrawalPhase::ItemsRetrieval => {
                if is_anvil(info) {
                    (false, "") // Never withdraw anvils in Phase 2
                } else if is_mending(info) {
                    if self.collected.mending < self.quota.mending_needed {
                        (true, "Mending Book")
                    } else {
                        (false, "")
                    }
                } else if is_unbreaking_3(info) {
                    if self.collected.unbreaking_3 < self.quota.unbreaking_3_needed {
                        (true, "Unbreaking III Book")
                    } else {
                        (false, "")
                    }
                } else if is_blast_protection_4(info) {
                    if self.collected.blast_protection_4 < self.quota.blast_protection_4_needed {
                        (true, "Blast Protection IV Book")
                    } else {
                        (false, "")
                    }
                } else if is_protection_4(info) {
                    if self.collected.protection_4 < self.quota.protection_4_needed {
                        (true, "Protection IV Book")
                    } else {
                        (false, "")
                    }
                } else if is_xp_bottle(info) {
                    let max_xp = self.quota.xp_stacks_needed * 64;
                    let has_enough = self.collected.xp_stacks >= self.quota.xp_stacks_needed
                        || self.collected.xp_bottles >= max_xp
                        || (self.quota.xp_stacks_needed <= 2 && self.collected.xp_bottles >= 106 && self.collected.xp_stacks >= 2);
                    if !has_enough {
                        (true, "Experience Bottles")
                    } else {
                        (false, "")
                    }
                } else if is_diamond_helmet(info) {
                    if self.collected.diamond_helmets < self.quota.diamond_helmets_needed {
                        (true, "Diamond Helmet")
                    } else {
                        (false, "")
                    }
                } else if is_diamond_chestplate(info) {
                    if self.collected.diamond_chestplates < self.quota.diamond_chestplates_needed {
                        (true, "Diamond Chestplate")
                    } else {
                        (false, "")
                    }
                } else if is_diamond_leggings(info) {
                    if self.collected.diamond_leggings < self.quota.diamond_leggings_needed {
                        (true, "Diamond Leggings")
                    } else {
                        (false, "")
                    }
                } else if is_diamond_boots(info) {
                    if self.collected.diamond_boots < self.quota.diamond_boots_needed {
                        (true, "Diamond Boots")
                    } else {
                        (false, "")
                    }
                } else {
                    (false, "")
                }
            }
            WithdrawalPhase::Done => (false, ""),
        }
    }

    /// Immediately records an item collected into inventory to ensure strict quota compliance within container loops.
    pub fn record_item_collected(&mut self, info: &ItemInfo) {
        if is_anvil(info) {
            self.collected.anvils += info.count as u32;
        } else if is_xp_bottle(info) {
            self.collected.xp_bottles += info.count as u32;
            self.collected.xp_stacks = self.collected.xp_stacks.max((self.collected.xp_bottles + 63) / 64);
        } else if is_mending(info) {
            self.collected.mending += info.count as u32;
        } else if is_unbreaking_3(info) {
            self.collected.unbreaking_3 += info.count as u32;
        } else if is_blast_protection_4(info) {
            self.collected.blast_protection_4 += info.count as u32;
        } else if is_protection_4(info) {
            self.collected.protection_4 += info.count as u32;
        } else if is_diamond_helmet(info) {
            self.collected.diamond_helmets += info.count as u32;
        } else if is_diamond_chestplate(info) {
            self.collected.diamond_chestplates += info.count as u32;
        } else if is_diamond_leggings(info) {
            self.collected.diamond_leggings += info.count as u32;
        } else if is_diamond_boots(info) {
            self.collected.diamond_boots += info.count as u32;
        }
    }

    /// Performs an exact inventory count and resets collected state from the current inventory.
    pub fn reset_and_sync_inventory(&mut self, bot: Option<&Client>) {
        let mut anvils = 0;
        let mut mending = 0;
        let mut unb3 = 0;
        let mut prot4 = 0;
        let mut blast_prot4 = 0;
        let mut xp_count = 0;
        let mut xp_stack_slots = 0;
        let mut helmets = 0;
        let mut chestplates = 0;
        let mut leggings = 0;
        let mut boots = 0;

        for item in self.player_inventory.values() {
            if let Some(info) = inspect_item_with_bot(item, bot) {
                if is_anvil(&info) {
                    anvils += info.count as u32;
                } else if is_xp_bottle(&info) {
                    xp_count += info.count as u32;
                    if info.count > 0 {
                        xp_stack_slots += 1;
                    }
                } else if is_mending(&info) {
                    mending += info.count as u32;
                } else if is_unbreaking_3(&info) {
                    unb3 += info.count as u32;
                } else if is_blast_protection_4(&info) {
                    blast_prot4 += info.count as u32;
                } else if is_protection_4(&info) {
                    prot4 += info.count as u32;
                } else if is_diamond_helmet(&info) {
                    helmets += info.count as u32;
                } else if is_diamond_chestplate(&info) {
                    chestplates += info.count as u32;
                } else if is_diamond_leggings(&info) {
                    leggings += info.count as u32;
                } else if is_diamond_boots(&info) {
                    boots += info.count as u32;
                }
            }
        }

        self.collected.anvils = anvils;
        self.collected.mending = mending;
        self.collected.unbreaking_3 = unb3;
        self.collected.protection_4 = prot4;
        self.collected.blast_protection_4 = blast_prot4;
        self.collected.xp_bottles = xp_count;
        self.collected.xp_stacks = xp_stack_slots.max((xp_count + 63) / 64);
        self.collected.diamond_helmets = helmets;
        self.collected.diamond_chestplates = chestplates;
        self.collected.diamond_leggings = leggings;
        self.collected.diamond_boots = boots;

        info!(
            "Inventory Initial Sync [Phase {:?}]: Anvils: {}/{}, Mending: {}/{}, Unb3: {}/{}, Prot4: {}/{}, BlastProt4: {}/{}, XP: {}/{} ({} bottles, {} stack slots), Armor (H: {}/{}, C: {}/{}, L: {}/{}, B: {}/{})",
            self.phase,
            self.collected.anvils, self.quota.anvils_needed,
            self.collected.mending, self.quota.mending_needed,
            self.collected.unbreaking_3, self.quota.unbreaking_3_needed,
            self.collected.protection_4, self.quota.protection_4_needed,
            self.collected.blast_protection_4, self.quota.blast_protection_4_needed,
            self.collected.xp_stacks, self.quota.xp_stacks_needed, self.collected.xp_bottles, xp_stack_slots,
            self.collected.diamond_helmets, self.quota.diamond_helmets_needed,
            self.collected.diamond_chestplates, self.quota.diamond_chestplates_needed,
            self.collected.diamond_leggings, self.quota.diamond_leggings_needed,
            self.collected.diamond_boots, self.quota.diamond_boots_needed,
        );
    }

    /// Synchronize collected item counts with actual items residing in the player's inventory,
    /// ensuring that item counts never decrease during container transitions.
    pub fn sync_collected_from_inventory(&mut self, bot: Option<&Client>) {
        let mut anvils = 0;
        let mut mending = 0;
        let mut unb3 = 0;
        let mut prot4 = 0;
        let mut blast_prot4 = 0;
        let mut xp_count = 0;
        let mut xp_stack_slots = 0;
        let mut helmets = 0;
        let mut chestplates = 0;
        let mut leggings = 0;
        let mut boots = 0;

        for item in self.player_inventory.values() {
            if let Some(info) = inspect_item_with_bot(item, bot) {
                if is_anvil(&info) {
                    anvils += info.count as u32;
                } else if is_xp_bottle(&info) {
                    xp_count += info.count as u32;
                    if info.count > 0 {
                        xp_stack_slots += 1;
                    }
                } else if is_mending(&info) {
                    mending += info.count as u32;
                } else if is_unbreaking_3(&info) {
                    unb3 += info.count as u32;
                } else if is_blast_protection_4(&info) {
                    blast_prot4 += info.count as u32;
                } else if is_protection_4(&info) {
                    prot4 += info.count as u32;
                } else if is_diamond_helmet(&info) {
                    helmets += info.count as u32;
                } else if is_diamond_chestplate(&info) {
                    chestplates += info.count as u32;
                } else if is_diamond_leggings(&info) {
                    leggings += info.count as u32;
                } else if is_diamond_boots(&info) {
                    boots += info.count as u32;
                }
            }
        }

        self.collected.anvils = self.collected.anvils.max(anvils);
        self.collected.mending = self.collected.mending.max(mending);
        self.collected.unbreaking_3 = self.collected.unbreaking_3.max(unb3);
        self.collected.protection_4 = self.collected.protection_4.max(prot4);
        self.collected.blast_protection_4 = self.collected.blast_protection_4.max(blast_prot4);
        self.collected.xp_bottles = self.collected.xp_bottles.max(xp_count);
        self.collected.xp_stacks = self.collected.xp_stacks.max(xp_stack_slots).max((self.collected.xp_bottles + 63) / 64);
        self.collected.diamond_helmets = self.collected.diamond_helmets.max(helmets);
        self.collected.diamond_chestplates = self.collected.diamond_chestplates.max(chestplates);
        self.collected.diamond_leggings = self.collected.diamond_leggings.max(leggings);
        self.collected.diamond_boots = self.collected.diamond_boots.max(boots);

        info!(
            "Inventory Synced [Phase {:?}]: Anvils: {}/{}, Mending: {}/{}, Unb3: {}/{}, Prot4: {}/{}, BlastProt4: {}/{}, XP: {}/{} ({} bottles, {} stack slots), Armor (H: {}/{}, C: {}/{}, L: {}/{}, B: {}/{})",
            self.phase,
            self.collected.anvils, self.quota.anvils_needed,
            self.collected.mending, self.quota.mending_needed,
            self.collected.unbreaking_3, self.quota.unbreaking_3_needed,
            self.collected.protection_4, self.quota.protection_4_needed,
            self.collected.blast_protection_4, self.quota.blast_protection_4_needed,
            self.collected.xp_stacks, self.quota.xp_stacks_needed, self.collected.xp_bottles, xp_stack_slots,
            self.collected.diamond_helmets, self.quota.diamond_helmets_needed,
            self.collected.diamond_chestplates, self.quota.diamond_chestplates_needed,
            self.collected.diamond_leggings, self.quota.diamond_leggings_needed,
            self.collected.diamond_boots, self.quota.diamond_boots_needed,
        );
    }

    /// Count how many empty slots exist in the player's 36-slot inventory (slots 9..=44).
    pub fn count_free_inventory_slots(&self) -> usize {
        let mut free = 0;
        for slot in 9..=44 {
            match self.player_inventory.get(&slot) {
                None => free += 1,
                Some(item) if matches!(item, ItemStack::Empty) => free += 1,
                _ => {}
            }
        }
        free
    }

    /// If inventory is filled with excess XP bottles beyond quota (3 stacks = 192 bottles),
    /// splash them down at the bot's feet to gain levels and free up inventory space.
    pub async fn free_space_by_splashing_excess_xp(&mut self, bot: &Client) {
        let max_bottles_to_keep = (self.quota.xp_stacks_needed * 64) as i32;
        let mut total_xp_bottles = 0;
        let mut bottle_slots: Vec<(i16, i32)> = Vec::new();

        for (&slot, item) in &self.player_inventory {
            if slot >= 9 && slot <= 44 {
                if let Some(info) = inspect_item_with_bot(item, Some(bot)) {
                    if is_xp_bottle(&info) {
                        total_xp_bottles += info.count;
                        bottle_slots.push((slot, info.count));
                    }
                }
            }
        }

        if total_xp_bottles <= max_bottles_to_keep {
            return;
        }

        let mut excess_to_throw = total_xp_bottles - max_bottles_to_keep;
        info!(
            "Inventory has {total_xp_bottles} XP bottles (quota needs {max_bottles_to_keep}). Freeing space by splashing {excess_to_throw} excess bottles..."
        );

        let dir = bot.direction();
        crate::enchanter::smooth_look(bot, dir.y_rot(), 90.0).await;
        bot.wait_ticks(5).await;

        for (slot, count) in bottle_slots {
            if excess_to_throw <= 0 {
                break;
            }

            if slot < 36 || slot > 44 {
                Self::swap_to_hotbar_static(bot, slot, 0);
                bot.wait_ticks(10).await;
            } else {
                let h = (slot - 36) as u8;
                bot.set_selected_hotbar_slot(h);
                bot.wait_ticks(5).await;
            }
            bot.set_selected_hotbar_slot(0);
            bot.wait_ticks(5).await;

            let throw_count = excess_to_throw.min(count);
            info!("Splashing {throw_count} excess XP bottles at feet with human arm swings...");
            for _ in 0..throw_count {
                bot.write_packet(azalea::protocol::packets::game::s_use_item::ServerboundUseItem {
                    hand: azalea::protocol::packets::game::s_interact::InteractionHand::MainHand,
                    seq: 0,
                    y_rot: dir.y_rot(),
                    x_rot: 90.0,
                });
                crate::enchanter::swing_arm(bot);
                bot.wait_ticks(3).await;
            }
            excess_to_throw -= throw_count;
            bot.wait_ticks(10).await;
        }

        bot.wait_ticks(20).await;
        self.reset_and_sync_inventory(Some(bot));
        info!(
            "Finished splashing excess XP! Free inventory slots now: {}",
            self.count_free_inventory_slots()
        );
    }

    fn swap_to_hotbar_static(bot: &Client, inv_slot: i16, hotbar_idx: u8) {
        let packet = ServerboundContainerClick {
            container_id: 0,
            state_id: 0,
            slot_num: inv_slot,
            button_num: hotbar_idx,
            click_type: ClickType::Swap,
            changed_slots: Default::default(),
            carried_item: HashedStack(None),
        };
        bot.write_packet(packet);
    }

    /// Reset slot tracking and transition state when a new container is opened.
    pub fn on_open_screen(&mut self, container_id: i32, title: &str) {
        info!("GUI opened: container_id={container_id}, title='{title}'");
        self.current_container_id = container_id;
        self.current_slots.clear();
        self.current_state_id = 0;
        self.open_container_size = 0;

        if self.state == OrderWorkflowState::WithdrawalComplete {
            info!("Ignoring GUI open transition because WithdrawalComplete is already reached.");
            return;
        }

        let clean_title = normalize_small_caps(&strip_color_and_normalize(title)).to_lowercase();
        if clean_title.contains("collect item") || clean_title.contains("collect items") {
            info!("Transitioned to InCollectDeliveryMenu ('{clean_title}')");
            self.state = OrderWorkflowState::InCollectDeliveryMenu;
        } else if clean_title.contains("edit order") || clean_title.contains("view order") {
            info!("Transitioned to InOrderSubmenu ('{clean_title}')");
            self.state = OrderWorkflowState::InOrderSubmenu;
        } else if clean_title.contains("your order") || clean_title.contains("my order") {
            info!("Transitioned to OpenedYourOrdersMenu ('{clean_title}')");
            self.current_menu_skipped_slots.clear();
            self.state = OrderWorkflowState::OpenedYourOrdersMenu;
        } else if clean_title.contains("order") {
            info!("Transitioned to OpenedOrderMainMenu ('{clean_title}')");
            self.state = OrderWorkflowState::OpenedOrderMainMenu;
        }
    }

    /// Update all slots when ClientboundContainerSetContent is received.
    pub fn on_set_content(&mut self, container_id: i32, state_id: u32, items: &[ItemStack], bot: Option<&Client>) {
        if container_id == 0 {
            for (i, item) in items.iter().enumerate() {
                self.player_inventory.insert(i as i16, item.clone());
            }
            self.sync_collected_from_inventory(bot);
            return;
        }

        if self.current_container_id != container_id {
            self.current_container_id = container_id;
        }
        self.current_state_id = state_id;

        info!("Container #{container_id} content received ({} slots)", items.len());
        for (i, item) in items.iter().enumerate() {
            self.current_slots.insert(i as i16, item.clone());
        }

        // In open containers (chests, anvils), the trailing 36 slots correspond to player inventory slots
        if items.len() >= 36 {
            let container_size = (items.len() - 36) as i16;
            self.open_container_size = container_size;
            let inv_start = items.len() - 36;
            for i in 0..36 {
                self.player_inventory.insert((9 + i) as i16, items[inv_start + i].clone());
            }
            self.sync_collected_from_inventory(bot);
        }

        self.log_all_slot_nbt(bot);
    }

    /// Update an individual slot when ClientboundContainerSetSlot is received.
    pub fn on_set_slot(&mut self, container_id: i32, state_id: u32, slot: i16, item: &ItemStack, bot: Option<&Client>) {
        if container_id == 0 {
            self.player_inventory.insert(slot, item.clone());
            self.sync_collected_from_inventory(bot);
            return;
        }

        if self.current_container_id == container_id {
            self.current_state_id = state_id;
            self.current_slots.insert(slot, item.clone());

            let container_size = if self.open_container_size > 0 {
                self.open_container_size
            } else if self.current_slots.len() >= 36 {
                (self.current_slots.len() - 36) as i16
            } else {
                -1
            };

            if container_size > 0 && slot >= container_size {
                let inv_slot = 9 + (slot - container_size);
                self.player_inventory.insert(inv_slot, item.clone());
                self.sync_collected_from_inventory(bot);
            }

            if let Some(info) = inspect_item_with_bot(item, bot) {
                info!("Slot #{slot} updated -> {} x{}", info.kind, info.count);
            }
        }
    }

    /// Print detailed NBT/components for all present items in the GUI.
    pub fn log_all_slot_nbt(&self, bot: Option<&Client>) {
        info!("=== Current GUI Item & NBT Audit ===");
        let mut sorted_slots: Vec<(&i16, &ItemStack)> = self.current_slots.iter().collect();
        sorted_slots.sort_by_key(|&(k, _)| *k);

        for (slot, item) in sorted_slots {
            if let Some(info) = inspect_item_with_bot(item, bot) {
                let name = info.custom_name.as_deref().unwrap_or(&info.kind);
                let mut ench_str = String::new();
                if !info.stored_enchantments.is_empty() {
                    ench_str.push_str(&format!(" (StoredEnch: {:?})", info.stored_enchantments));
                }
                if !info.enchantments.is_empty() {
                    ench_str.push_str(&format!(" (Ench: {:?})", info.enchantments));
                }

                info!(
                    "Slot #{slot:02}: '{name}' x{} [kind: {}]{ench_str}",
                    info.count, info.kind
                );

                if let Some(ref comp) = info.raw_components {
                    let comp_json = serde_json::to_string(comp).unwrap_or_default();
                    if comp_json.len() > 200 {
                        info!("  NBT/Components: {}...", &comp_json[..200]);
                    } else if !comp_json.is_empty() && comp_json != "{}" {
                        info!("  NBT/Components: {}", comp_json);
                    }
                }
            }
        }
        info!("====================================");
    }

    /// Perform the next automated action in the current GUI.
    pub async fn process_gui_actions(&mut self, bot: &Client) -> bool {
        if self.state == OrderWorkflowState::WithdrawalComplete || self.phase == WithdrawalPhase::Done {
            if self.current_container_id > 0 {
                self.close_current_gui(bot);
            }
            return true;
        }

        if self.action_in_progress {
            return false;
        }
        self.action_in_progress = true;

        self.sync_collected_from_inventory(Some(bot));

        let phase_fulfilled = match self.phase {
            WithdrawalPhase::AnvilPlacement => self.collected.anvils >= 1,
            WithdrawalPhase::ItemsRetrieval => self.collected.is_fulfilled(&self.quota),
            WithdrawalPhase::Done => true,
        };

        if phase_fulfilled {
            info!("Current withdrawal phase {:?} is fulfilled in inventory! Closing GUI.", self.phase);
            self.close_current_gui(bot);
            if self.phase == WithdrawalPhase::AnvilPlacement {
                self.state = OrderWorkflowState::WaitingForNextOrder;
            } else {
                self.phase = WithdrawalPhase::Done;
                self.state = OrderWorkflowState::WithdrawalComplete;
            }
            self.action_in_progress = false;
            return true;
        }

        let res = match self.state {
            OrderWorkflowState::OpenedOrderMainMenu => {
                if let Some(slot) = self.find_your_orders_slot(bot) {
                    info!("Clicking 'Your Orders' button at slot #{slot}...");
                    self.state = OrderWorkflowState::NavigatingToYourOrders;
                    self.click_slot(bot, slot, ClickType::Pickup);
                    bot.wait_ticks(20).await;
                    true
                } else {
                    warn!("Could not find 'Your Orders' button in /order main menu! Available slots:");
                    self.log_all_slot_nbt(Some(bot));
                    false
                }
            }
            OrderWorkflowState::NavigatingToYourOrders => {
                // Already clicked slot 51; awaiting Your Orders menu opening
                false
            }
            OrderWorkflowState::OpenedYourOrdersMenu => {
                self.select_order_to_claim(bot).await
            }
            OrderWorkflowState::InOrderSubmenu => {
                self.claim_current_order(bot).await
            }
            OrderWorkflowState::InCollectDeliveryMenu => {
                self.transfer_delivery_items(bot).await
            }
            _ => false,
        };

        self.action_in_progress = false;
        res
    }

    fn find_your_orders_slot(&self, bot: &Client) -> Option<i16> {
        // Priority 1: In DonutSMP, slot 51 in the main /order GUI is the canonical "Your Orders" chest
        if let Some(item) = self.current_slots.get(&51) {
            if let Some(info) = inspect_item_with_bot(item, Some(bot)) {
                if is_your_orders_button(&info) || info.kind.to_lowercase().contains("chest") {
                    info!("Identified slot #51 as 'Your Orders' chest button ({})", info.kind);
                    return Some(51);
                }
            }
        }

        // Priority 2: Fallback scanning across all slots
        for (&slot, item) in &self.current_slots {
            if let Some(info) = inspect_item_with_bot(item, Some(bot)) {
                if is_your_orders_button(&info) {
                    info!("Identified slot #{slot} as 'Your Orders' button via lore/title inspection");
                    return Some(slot);
                }
            }
        }
        None
    }

    /// Select an order slot in 'Orders -> Your Orders' (slots 0..=44) that matches our quota needs.
    async fn select_order_to_claim(&mut self, bot: &Client) -> bool {
        info!("Checking orders in 'Your Orders' against quota (Phase: {:?})...", self.phase);
        info!(
            "Current Status: Anvils: {}/{}, Mending: {}/{}, Unb3: {}/{}, Prot4: {}/{}, BlastProt4: {}/{}, XP: {}/{} ({} bottles), Armor (H: {}/{}, C: {}/{}, L: {}/{}, B: {}/{})",
            self.collected.anvils, self.quota.anvils_needed,
            self.collected.mending, self.quota.mending_needed,
            self.collected.unbreaking_3, self.quota.unbreaking_3_needed,
            self.collected.protection_4, self.quota.protection_4_needed,
            self.collected.blast_protection_4, self.quota.blast_protection_4_needed,
            self.collected.xp_stacks, self.quota.xp_stacks_needed, self.collected.xp_bottles,
            self.collected.diamond_helmets, self.quota.diamond_helmets_needed,
            self.collected.diamond_chestplates, self.quota.diamond_chestplates_needed,
            self.collected.diamond_leggings, self.quota.diamond_leggings_needed,
            self.collected.diamond_boots, self.quota.diamond_boots_needed,
        );

        // Check slots 0 through 44 (leaving bottom row 45..53 for pagination/navigation)
        for slot in 0..=44 {
            if self.current_menu_skipped_slots.contains(&slot) {
                continue;
            }

            if let Some(item) = self.current_slots.get(&slot) {
                if let Some(info) = inspect_item_with_bot(item, Some(bot)) {
                    if info.kind.to_lowercase().contains("glasspane")
                        || info.kind.to_lowercase().contains("arrow")
                        || info.kind.to_lowercase().contains("barrier")
                    {
                        continue;
                    }

                    let (is_needed, order_name) = self.is_order_needed(&info);

                    if is_needed {
                        info!("Found needed order '{order_name}' at slot #{slot} (Phase: {:?})! Left-clicking to open Edit/Claim submenu...", self.phase);
                        self.target_order_type = Some(order_name.to_string());
                        self.click_slot(bot, slot, ClickType::Pickup);
                        bot.wait_ticks(20).await;
                        return true;
                    } else {
                        // Mark this slot as not needed for this menu session
                        self.current_menu_skipped_slots.push(slot);
                    }
                }
            }
        }

        info!("No unfulfilled orders matching phase {:?} found in slots 0-44 of 'Your Orders'.", self.phase);

        if self.phase == WithdrawalPhase::AnvilPlacement {
            info!("Anvil phase order check done. Closing GUI.");
            self.close_current_gui(bot);
            return true;
        }

        if self.collected.is_fulfilled(&self.quota) {
            info!("All required items have been fulfilled! Closing GUI.");
            self.close_current_gui(bot);
            self.phase = WithdrawalPhase::Done;
            self.state = OrderWorkflowState::WithdrawalComplete;
            return true;
        }

        false
    }

    /// Find the collect button in the 'Orders -> Edit Order' submenu.
    fn find_collect_button_slot(&self) -> Option<i16> {
        // First check slot 13 (the canonical DonutSMP collect button)
        if let Some(item) = self.current_slots.get(&13) {
            if let Some(info) = inspect_item(item) {
                if info.kind.to_lowercase().contains("chest") || is_confirm_button(&info) {
                    info!("Identified slot #13 as 'Collect' button ({})", info.kind);
                    return Some(13);
                }
            }
        }

        // Fallback: scan all slots for collect / claim / confirm
        for (&slot, item) in &self.current_slots {
            if slot == 10 {
                // Slot 10 is the item preview icon, not the button
                continue;
            }
            if let Some(info) = inspect_item(item) {
                if is_confirm_button(&info) {
                    info!("Identified slot #{slot} as 'Collect' button via lore/title ({})", info.kind);
                    return Some(slot);
                }
            }
        }

        None
    }

    /// Claim the current order inside 'Orders -> Edit Order'.
    async fn claim_current_order(&mut self, bot: &Client) -> bool {
        // Check slot 10 in Edit Order to learn the exact item type being claimed
        if let Some(item_slot_10) = self.current_slots.get(&10) {
            if let Some(info) = inspect_item_with_bot(item_slot_10, Some(bot)) {
                info!(
                    "Order details in slot #10: {} x{} (Name: {:?}, Lore: {:?}, StoredEnch: {:?})",
                    info.kind, info.count, info.custom_name, info.lore, info.stored_enchantments
                );
                let (needed, name) = self.is_order_needed(&info);
                if !needed {
                    warn!(
                        "Order in slot #10 is NOT needed in current phase ({:?}) or quota already met! Closing GUI.",
                        self.phase
                    );
                    self.close_current_gui(bot);
                    bot.wait_ticks(20).await;
                    return true;
                }
                self.target_order_type = Some(name.to_string());
            }
        }

        if let Some(collect_slot) = self.find_collect_button_slot() {
            info!("Clicking 'Collect' button at slot #{collect_slot}...");
            self.click_slot(bot, collect_slot, ClickType::Pickup);
            bot.wait_ticks(20).await;
            return true;
        } else {
            warn!("Could not find Collect button in Edit Order window! Logging slots:");
            self.log_all_slot_nbt(Some(bot));
        }

        false
    }

    /// Transfer items from the 'Orders -> Collect Items' delivery menu into player inventory.
    async fn transfer_delivery_items(&mut self, bot: &Client) -> bool {
        info!("Scanning delivery slots in 'Orders -> Collect Items'...");
        let mut sorted_slots: Vec<i16> = self.current_slots.keys().copied().collect();
        sorted_slots.sort();

        let mut transferred_any = false;
        for slot in sorted_slots {
            // Only transfer from top delivery container (slots 0..=26)
            if slot > 26 {
                continue;
            }

            if self.count_free_inventory_slots() == 0 {
                info!("Player inventory is full (0 free slots). Stopping item collection.");
                break;
            }

            if let Some(item) = self.current_slots.get(&slot) {
                if let Some(info) = inspect_item_with_bot(item, Some(bot)) {
                    if info.kind.to_lowercase().contains("glasspane")
                        || info.kind.to_lowercase().contains("arrow")
                        || info.kind.to_lowercase().contains("barrier")
                    {
                        continue;
                    }

                    let (is_needed, name) = self.is_order_needed(&info);
                    if !is_needed {
                        info!(
                            "Skipping delivery item in slot #{slot}: {} x{} (quota already met or not needed in Phase {:?})",
                            info.kind, info.count, self.phase
                        );
                        // Stop withdrawing from this delivery window if this order's items are no longer needed
                        break;
                    }

                    info!(
                        "Transferring delivery item from slot #{slot}: {} ({name}) x{} to inventory...",
                        info.kind, info.count
                    );
                    self.click_slot(bot, slot, ClickType::QuickMove);
                    transferred_any = true;

                    // Immediately record the item in memory so subsequent slots in this loop respect quota!
                    self.record_item_collected(&info);
                    info!(
                        "Progress after slot #{slot}: Anvils: {}/{}, Mending: {}/{}, Unb3: {}/{}, Prot4: {}/{}, BlastProt4: {}/{}, XP: {}/{} ({} bottles), Helm: {}/{}, Chest: {}/{}, Legs: {}/{}, Boots: {}/{}",
                        self.collected.anvils, self.quota.anvils_needed,
                        self.collected.mending, self.quota.mending_needed,
                        self.collected.unbreaking_3, self.quota.unbreaking_3_needed,
                        self.collected.protection_4, self.quota.protection_4_needed,
                        self.collected.blast_protection_4, self.quota.blast_protection_4_needed,
                        self.collected.xp_stacks, self.quota.xp_stacks_needed, self.collected.xp_bottles,
                        self.collected.diamond_helmets, self.quota.diamond_helmets_needed,
                        self.collected.diamond_chestplates, self.quota.diamond_chestplates_needed,
                        self.collected.diamond_leggings, self.quota.diamond_leggings_needed,
                        self.collected.diamond_boots, self.quota.diamond_boots_needed,
                    );

                    bot.wait_ticks(15).await; // 750ms anticheat safe delay

                    // If quota for this item type is now met, stop withdrawing from this order
                    let (still_needed, _) = self.is_order_needed(&info);
                    if !still_needed {
                        info!("Quota for {} has been fulfilled! Leaving any surplus items in delivery.", name);
                        break;
                    }
                }
            }
        }

        // Reset skipped slots since claimed order causes remaining orders to shift
        if transferred_any {
            self.current_menu_skipped_slots.clear();
        }

        if let Some(ref order_type) = self.target_order_type {
            info!("Finished claiming delivery items for order '{order_type}'!");
        }
        self.target_order_type = None;

        info!("Closing delivery container...");
        self.close_current_gui(bot);
        bot.wait_ticks(30).await;

        let phase_done = match self.phase {
            WithdrawalPhase::AnvilPlacement => self.collected.anvils >= self.quota.anvils_needed,
            WithdrawalPhase::ItemsRetrieval => self.collected.is_fulfilled(&self.quota),
            WithdrawalPhase::Done => true,
        };

        if phase_done {
            info!("Withdrawal phase {:?} completed!", self.phase);
            if self.phase == WithdrawalPhase::AnvilPlacement {
                self.state = OrderWorkflowState::WaitingForNextOrder;
            } else {
                self.phase = WithdrawalPhase::Done;
                self.state = OrderWorkflowState::WithdrawalComplete;
            }
            return true;
        } else {
            info!("Phase {:?} not yet fulfilled. Waiting 40 ticks before opening /order again...", self.phase);
            self.state = OrderWorkflowState::WaitingForNextOrder;
            bot.wait_ticks(40).await;
            info!("Sending /order for next item...");
            bot.chat("/order");
            return true;
        }
    }

    /// Click a slot by sending a ServerboundContainerClick packet.
    pub fn click_slot(&self, bot: &Client, slot: i16, click_type: ClickType) {
        let packet = ServerboundContainerClick {
            container_id: self.current_container_id,
            state_id: self.current_state_id,
            slot_num: slot,
            button_num: 0,
            click_type,
            changed_slots: Default::default(),
            carried_item: HashedStack(None),
        };
        bot.write_packet(packet);
    }

    /// Close the currently opened GUI container.
    pub fn close_current_gui(&self, bot: &Client) {
        bot.write_packet(ServerboundContainerClose {
            container_id: self.current_container_id,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_quota_fulfillment() {
        let quota = WithdrawalQuota {
            anvils_needed: 1,
            mending_needed: 4,
            unbreaking_3_needed: 4,
            protection_4_needed: 2,
            blast_protection_4_needed: 2,
            xp_stacks_needed: 3,
            diamond_helmets_needed: 1,
            diamond_chestplates_needed: 1,
            diamond_leggings_needed: 1,
            diamond_boots_needed: 1,
        };

        let mut collected = CollectedItems::default();
        assert!(!collected.is_fulfilled(&quota));

        collected.anvils = 1;
        collected.mending = 4;
        collected.unbreaking_3 = 4;
        collected.protection_4 = 2;
        collected.blast_protection_4 = 2;
        collected.xp_stacks = 3;
        assert!(!collected.is_fulfilled(&quota)); // Missing diamond armor pieces

        collected.diamond_helmets = 1;
        collected.diamond_chestplates = 1;
        collected.diamond_leggings = 1;
        assert!(!collected.is_fulfilled(&quota)); // Missing diamond boots

        collected.diamond_boots = 1;
        assert!(collected.is_fulfilled(&quota)); // All fulfilled
    }

    #[test]
    fn test_order_needed_per_phase() {
        let mut gui = GuiManager::new();
        gui.phase = WithdrawalPhase::AnvilPlacement;

        let anvil_info = ItemInfo {
            kind: "Anvil".to_string(),
            count: 1,
            ..Default::default()
        };
        let (needed, name) = gui.is_order_needed(&anvil_info);
        assert!(needed);
        assert_eq!(name, "Anvil");

        let mut book_info = ItemInfo {
            kind: "EnchantedBook".to_string(),
            count: 1,
            ..Default::default()
        };
        book_info.stored_enchantments.insert("mending".to_string(), 1);
        let (needed_in_phase_1, _) = gui.is_order_needed(&book_info);
        assert!(!needed_in_phase_1); // Books should NOT be withdrawn in AnvilPlacement phase

        // Switch to ItemsRetrieval phase
        gui.phase = WithdrawalPhase::ItemsRetrieval;
        let (needed_in_phase_2, book_name) = gui.is_order_needed(&book_info);
        assert!(needed_in_phase_2);
        assert_eq!(book_name, "Mending Book");

        // Anvil should NOT be withdrawn in ItemsRetrieval phase
        let (needed_anvil_in_phase_2, _) = gui.is_order_needed(&anvil_info);
        assert!(!needed_anvil_in_phase_2);

        // Armor pieces in phase 2
        let helmet = ItemInfo {
            kind: "DiamondHelmet".to_string(),
            count: 1,
            ..Default::default()
        };
        let (needed_helmet, h_name) = gui.is_order_needed(&helmet);
        assert!(needed_helmet);
        assert_eq!(h_name, "Diamond Helmet");

        gui.collected.diamond_helmets = 1;
        let (needed_helmet_again, _) = gui.is_order_needed(&helmet);
        assert!(!needed_helmet_again); // Already collected 1 helmet
    }

    #[test]
    fn test_xp_bottles_already_enough_skips_orders() {
        let mut gui = GuiManager::new();
        gui.phase = WithdrawalPhase::ItemsRetrieval;
        gui.quota.xp_stacks_needed = 2;

        let xp_info = ItemInfo {
            kind: "ExperienceBottle".to_string(),
            count: 64,
            ..Default::default()
        };

        // When inventory has 0 bottles, it IS needed
        gui.collected.xp_bottles = 0;
        gui.collected.xp_stacks = 0;
        let (needed, name) = gui.is_order_needed(&xp_info);
        assert!(needed);
        assert_eq!(name, "Experience Bottles");

        // When inventory has 1 stack (64 bottles), it IS needed
        gui.collected.xp_bottles = 64;
        gui.collected.xp_stacks = 1;
        let (needed, _) = gui.is_order_needed(&xp_info);
        assert!(needed);

        // When inventory has 2 stacks (128 bottles), it is NOT needed!
        gui.collected.xp_bottles = 128;
        gui.collected.xp_stacks = 2;
        let (needed, _) = gui.is_order_needed(&xp_info);
        assert!(!needed, "Should not need XP bottles when 2 stacks (128 bottles) are collected");

        // When inventory has 2 stacks but slightly less than 128 (e.g. 114 bottles across 2 slots), it is NOT needed!
        gui.collected.xp_bottles = 114;
        gui.collected.xp_stacks = 2;
        let (needed, _) = gui.is_order_needed(&xp_info);
        assert!(!needed, "Should not need XP bottles when 2 stacks (>=106 bottles) already collected");
    }

    #[test]
    fn test_click_type_swap() {
        let _ = ClickType::Swap;
    }
}

