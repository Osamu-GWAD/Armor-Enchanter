use crate::nbt::{
    inspect_item_with_bot, is_anvil, is_blast_protection_4, is_diamond_armor, is_diamond_boots,
    is_diamond_chestplate, is_diamond_helmet, is_diamond_leggings, is_mending, is_protection_4,
    is_unbreaking_3, is_xp_bottle, ItemInfo,
};
use azalea::inventory::operations::ClickType;
use azalea::inventory::ItemStack;
use azalea::protocol::packets::game::s_container_click::{HashedStack, ServerboundContainerClick};
use azalea::protocol::packets::game::ServerboundContainerClose;
use azalea::BlockPos;
use azalea::Client;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use tracing::{info, warn};

/// Cumulative XP required to reach a given level from level 0 (Java Edition formula).
pub fn total_xp_for_level(level: u32) -> u32 {
    match level {
        0 => 0,
        1..=16 => level * level + 6 * level,
        17..=31 => {
            let l = level as f64;
            (2.5 * l * l - 40.5 * l + 360.0).round() as u32
        }
        _ => {
            let l = level as f64;
            (4.5 * l * l - 162.5 * l + 2220.0).round() as u32
        }
    }
}

/// Calculate the exact number of Bottles o' Enchanting needed to reach `target_level`
/// from the current XP state.
/// On Minecraft Java Edition, each bottle yields 3 to 11 XP (average 7.0 XP).
pub fn bottles_needed_for_level(current_total_xp: u32, current_level: u32, target_level: u32) -> u32 {
    if current_level >= target_level {
        return 0;
    }
    let target_xp = total_xp_for_level(target_level);
    let deficit = target_xp.saturating_sub(current_total_xp);
    if deficit == 0 {
        return 1;
    }
    (deficit + 6) / 7
}

/// An individual combine task inside the anvil.
#[derive(Debug, Clone)]
pub struct CombineTask {
    pub armor_slot: i16,
    pub book_slot: i16,
    pub armor_desc: String,
    pub enchant_name: &'static str,
    pub required_level: u32,
}

/// Tracks the progress and state of anvil enchanting operations.
pub struct EnchanterManager {
    pub anvil_placed: bool,
    pub anvil_pos: Option<BlockPos>,
    pub is_enchanting: bool,
    pub anvil_container_id: Option<i32>,
    pub anvil_state_id: Arc<AtomicU32>,
    pub anvil_slots: HashMap<i16, ItemStack>,
    pub player_inventory: HashMap<i16, ItemStack>,
    pub current_level: Arc<AtomicU32>,
    pub total_experience: Arc<AtomicU32>,
    pub server_anvil_cost: Arc<AtomicU32>,
    pub enchanting_complete: bool,
}

impl EnchanterManager {
    pub fn new() -> Self {
        Self {
            anvil_placed: false,
            anvil_pos: None,
            is_enchanting: false,
            anvil_container_id: None,
            anvil_state_id: Arc::new(AtomicU32::new(0)),
            anvil_slots: HashMap::new(),
            player_inventory: HashMap::new(),
            current_level: Arc::new(AtomicU32::new(0)),
            total_experience: Arc::new(AtomicU32::new(0)),
            server_anvil_cost: Arc::new(AtomicU32::new(0)),
            enchanting_complete: false,
        }
    }

    /// Splash only the exact number of XP bottles needed to reach `target_level`
    /// from current experience state.
    pub async fn throw_exact_xp_bottles(&mut self, bot: &Client, target_level: u32) {
        let mut current_lvl = self.current_level.load(Ordering::SeqCst);
        let mut current_xp = self.total_experience.load(Ordering::SeqCst);

        if current_lvl >= target_level {
            info!("Already at level {} (target: {}). No XP bottles needed.", current_lvl, target_level);
            return;
        }

        let mut bottles_to_throw = bottles_needed_for_level(current_xp, current_lvl, target_level);
        info!(
            "Throwing {bottles_to_throw} XP bottles to reach Level {target_level} from Level {current_lvl} (Current Total XP: {current_xp})..."
        );

        // Aim directly down at the bot's feet
        let dir = bot.direction();
        bot.set_direction(dir.y_rot(), 90.0);
        bot.wait_ticks(5).await;

        while current_lvl < target_level && bottles_to_throw > 0 {
            // Find XP bottles in inventory
            let mut xp_slot = None;
            for (&slot, item) in &self.player_inventory {
                if let Some(info) = inspect_item_with_bot(item, Some(bot)) {
                    if is_xp_bottle(&info) && info.count > 0 {
                        xp_slot = Some((slot, info.count));
                        break;
                    }
                }
            }

            let (slot, count) = match xp_slot {
                Some(s) => s,
                None => {
                    warn!("No XP bottles found in inventory to reach level {target_level}!");
                    break;
                }
            };

            // If item is in main inventory (slots 9..35), swap to hotbar slot 0
            let hotbar_idx = if slot >= 36 && slot <= 44 {
                (slot - 36) as u8
            } else {
                info!("Swapping XP bottles from slot #{slot} to hotbar slot #0...");
                Self::swap_to_hotbar(bot, slot, 0);
                bot.wait_ticks(10).await;
                0
            };

            bot.set_selected_hotbar_slot(hotbar_idx);
            bot.wait_ticks(5).await;

            let batch = bottles_to_throw.min(count as u32).min(64);
            info!("Splashing {batch} XP bottles at feet...");
            for _ in 0..batch {
                bot.write_packet(azalea::protocol::packets::game::s_use_item::ServerboundUseItem {
                    hand: Default::default(),
                    seq: 0,
                    y_rot: dir.y_rot(),
                    x_rot: 90.0,
                });
                bot.wait_ticks(2).await;
            }

            // Wait 25 ticks (1.25s) for experience orbs to be absorbed and SetExperience to arrive
            bot.wait_ticks(25).await;

            current_lvl = self.current_level.load(Ordering::SeqCst);
            current_xp = self.total_experience.load(Ordering::SeqCst);
            info!("XP Update after splashing: Current Level: {current_lvl}, Total XP: {current_xp} (Target: {target_level})");

            if current_lvl >= target_level {
                break;
            }

            bottles_to_throw = bottles_needed_for_level(current_xp, current_lvl, target_level);
            if bottles_to_throw > 0 {
                info!("Slight XP shortfall due to drop variation. Throwing {bottles_to_throw} more bottle(s)...");
            }
        }

        info!("Finished XP consumption routine. Final level: {current_lvl} (target was {target_level}).");
    }

    /// Raycasts to check if an anvil is already present at the expected location.
    pub async fn check_if_anvil_placed(&mut self, bot: &Client) -> bool {
        if self.anvil_placed && self.anvil_pos.is_some() {
            return true;
        }

        let player_pos = bot.position();
        let base_x = player_pos.x.floor() as i32;
        let ground_y = (player_pos.y - 0.1).floor() as i32;
        let base_z = player_pos.z.floor() as i32;
        let target_anvil_pos = BlockPos::new(base_x + 1, ground_y + 1, base_z);

        let dx_t = (target_anvil_pos.x as f64 + 0.5) - player_pos.x;
        let dy_t = (target_anvil_pos.y as f64 + 0.5) - (player_pos.y + 1.62);
        let dz_t = (target_anvil_pos.z as f64 + 0.5) - player_pos.z;
        let h_dist_t = (dx_t * dx_t + dz_t * dz_t).sqrt();
        let yaw_t = (-dx_t).atan2(dz_t).to_degrees() as f32;
        let pitch_t = (-dy_t).atan2(h_dist_t).to_degrees() as f32;
        bot.set_direction(yaw_t, pitch_t);
        bot.wait_ticks(5).await;

        let hit_res = bot.hit_result();
        let hit_debug = format!("{hit_res:?}");
        let expected_pos_str = format!("x: {}, y: {}, z: {}", target_anvil_pos.x, target_anvil_pos.y, target_anvil_pos.z);
        if hit_debug.contains("miss: false") && hit_debug.contains(&expected_pos_str) {
            info!("Anvil block is already present at {:?}!", target_anvil_pos);
            self.anvil_pos = Some(target_anvil_pos);
            self.anvil_placed = true;
            return true;
        }

        false
    }

    /// Place an anvil on the ground adjacent to the bot.
    pub async fn place_anvil(
        &mut self,
        bot: &Client,
        player_inventory: &HashMap<i16, ItemStack>,
    ) {
        if self.check_if_anvil_placed(bot).await {
            info!("Anvil block is already present at {:?}! Skipping placement.", self.anvil_pos);
            return;
        }

        info!("Searching for Anvil in inventory...");
        let mut anvil_slot = None;
        for (&slot, item) in player_inventory {
            if let Some(info) = inspect_item_with_bot(item, Some(bot)) {
                if is_anvil(&info) {
                    anvil_slot = Some(slot);
                    break;
                }
            }
        }

        let slot = match anvil_slot {
            Some(s) => s,
            None => {
                warn!("No Anvil found in inventory to place!");
                return;
            }
        };

        // If not already in hotbar slot 1 (slot 37), swap to hotbar slot 1
        if slot != 37 {
            info!("Swapping Anvil from slot #{slot} to hotbar slot #1...");
            Self::swap_to_hotbar(bot, slot, 1);
            bot.wait_ticks(10).await;
        }

        bot.set_selected_hotbar_slot(1);
        bot.wait_ticks(5).await;

        let player_pos = bot.position();
        let base_x = player_pos.x.floor() as i32;
        let ground_y = (player_pos.y - 0.1).floor() as i32;
        let base_z = player_pos.z.floor() as i32;

        let ground_pos = BlockPos::new(base_x + 1, ground_y, base_z);
        let target_anvil_pos = BlockPos::new(ground_pos.x, ground_pos.y + 1, ground_pos.z);

        if self.check_if_anvil_placed(bot).await {
            info!("Anvil block is already present at {:?}! Skipping placement.", target_anvil_pos);
            return;
        }

        // Aim directly at the top center of the ground block face
        let dx = (ground_pos.x as f64 + 0.5) - player_pos.x;
        let dy = (ground_pos.y as f64 + 1.0) - (player_pos.y + 1.62);
        let dz = (ground_pos.z as f64 + 0.5) - player_pos.z;
        let horizontal_dist = (dx * dx + dz * dz).sqrt();
        let yaw = (-dx).atan2(dz).to_degrees() as f32;
        let pitch = (-dy).atan2(horizontal_dist).to_degrees() as f32;
        bot.set_direction(yaw, pitch);
        bot.wait_ticks(10).await;

        info!("Crosshair before place: {:?}", bot.hit_result());
        info!("Placing Anvil on ground block at {:?} (aiming yaw: {yaw:.1}, pitch: {pitch:.1})...", ground_pos);
        bot.block_interact(ground_pos);
        bot.start_use_item();
        bot.wait_ticks(20).await;

        self.anvil_pos = Some(BlockPos::new(ground_pos.x, ground_pos.y + 1, ground_pos.z));
        self.anvil_placed = true;
        info!("Anvil placed successfully at {:?}", self.anvil_pos);
    }

    /// Right-click the placed anvil to open the enchanting interface.
    pub async fn open_anvil(&self, bot: &Client) {
        self.open_anvil_with_inv(bot, &self.player_inventory).await;
    }

    /// Right-click the placed anvil with explicit inventory reference.
    pub async fn open_anvil_with_inv(&self, bot: &Client, player_inventory: &HashMap<i16, ItemStack>) {
        // Switch to an empty or non-block hotbar slot so right-clicking interacts with the anvil
        let mut target_hotbar = 8u8;
        for h in 0..9 {
            let inv_slot = 36 + h;
            match player_inventory.get(&inv_slot) {
                None => {
                    target_hotbar = h as u8;
                    break;
                }
                Some(item) if matches!(item, ItemStack::Empty) => {
                    target_hotbar = h as u8;
                    break;
                }
                _ => {}
            }
        }
        bot.set_selected_hotbar_slot(target_hotbar);
        bot.wait_ticks(5).await;

        let target_pos = self.anvil_pos.unwrap_or_else(|| {
            let p = bot.position();
            let gy = (p.y - 0.1).floor() as i32;
            BlockPos::new(
                p.x.floor() as i32 + 1,
                gy + 1,
                p.z.floor() as i32,
            )
        });

        // Aim directly at the target anvil block
        let player_pos = bot.position();
        let dx = (target_pos.x as f64 + 0.5) - player_pos.x;
        let dy = (target_pos.y as f64 + 0.5) - (player_pos.y + 1.62);
        let dz = (target_pos.z as f64 + 0.5) - player_pos.z;
        let horizontal_dist = (dx * dx + dz * dz).sqrt();
        let yaw = (-dx).atan2(dz).to_degrees() as f32;
        let pitch = (-dy).atan2(horizontal_dist).to_degrees() as f32;
        bot.set_direction(yaw, pitch);
        bot.wait_ticks(10).await;

        info!("Interacting to open Anvil at {:?} with hotbar slot #{}...", target_pos, target_hotbar);
        bot.block_interact(target_pos);
        bot.start_use_item();
        bot.wait_ticks(20).await;
    }

    /// Handles Anvil GUI opening.
    pub fn on_open_screen(&mut self, container_id: i32, title: &str) {
        info!("Anvil Screen opened: container_id={container_id}, title='{title}'");
        self.anvil_container_id = Some(container_id);
        self.anvil_slots.clear();
        self.anvil_state_id.store(0, Ordering::SeqCst);
    }

    /// Updates anvil slots and player inventory tracking when content packet is received.
    pub fn on_set_content(
        &mut self,
        container_id: i32,
        state_id: u32,
        items: &[ItemStack],
    ) {
        self.anvil_container_id = Some(container_id);
        self.anvil_state_id.store(state_id, Ordering::SeqCst);
        self.anvil_slots.clear();

        for (i, item) in items.iter().enumerate() {
            self.anvil_slots.insert(i as i16, item.clone());
            if i >= 3 && i <= 38 {
                let inv_slot = (i - 3 + 9) as i16;
                self.player_inventory.insert(inv_slot, item.clone());
            }
        }
    }

    /// Determine the next combination task based on user specifications and iamcal/enchant-order:
    /// - Helmet: Protection IV (4 lvl) -> Unbreaking III (4 lvl) -> Mending (5 lvl)
    /// - Chestplate: Protection IV (4 lvl) -> Unbreaking III (4 lvl) -> Mending (5 lvl)
    /// - Leggings: Blast Protection IV (8 lvl) -> Unbreaking III (4 lvl) -> Mending (5 lvl)
    /// - Boots: Blast Protection IV (8 lvl) -> Unbreaking III (4 lvl) -> Mending (5 lvl)
    pub fn find_next_combine_task(&self, bot: &Client) -> Option<CombineTask> {
        let mut armor_slots: Vec<(i16, ItemInfo)> = Vec::new();
        let mut book_slots: Vec<(i16, ItemInfo)> = Vec::new();

        // Player slots in Anvil GUI are slots 3 through 38
        for slot in 3..=38 {
            if let Some(item) = self.anvil_slots.get(&slot) {
                if let Some(info) = inspect_item_with_bot(item, Some(bot)) {
                    if is_diamond_armor(&info) {
                        armor_slots.push((slot, info));
                    } else if info.kind.contains("Book") || info.kind.contains("EnchantedBook") {
                        book_slots.push((slot, info));
                    }
                }
            }
        }

        for (armor_slot, armor_info) in &armor_slots {
            let is_prot_piece = is_diamond_helmet(armor_info) || is_diamond_chestplate(armor_info);
            let is_blast_piece = is_diamond_leggings(armor_info) || is_diamond_boots(armor_info);

            let mut target: Option<(&'static str, u32, fn(&ItemInfo) -> bool)> = None;

            if is_prot_piece {
                if !armor_info.has_enchantment("protection", 4) {
                    target = Some(("Protection IV", 4, is_protection_4));
                } else if !armor_info.has_enchantment("unbreaking", 3) {
                    target = Some(("Unbreaking III", 4, is_unbreaking_3));
                } else if !armor_info.has_enchantment("mending", 1) {
                    target = Some(("Mending", 5, is_mending));
                }
            } else if is_blast_piece {
                if !armor_info.has_enchantment("blast_protection", 4) {
                    target = Some(("Blast Protection IV", 8, is_blast_protection_4));
                } else if !armor_info.has_enchantment("unbreaking", 3) {
                    target = Some(("Unbreaking III", 4, is_unbreaking_3));
                } else if !armor_info.has_enchantment("mending", 1) {
                    target = Some(("Mending", 5, is_mending));
                }
            }

            if let Some((ench_name, req_level, predicate)) = target {
                for (b_slot, b_info) in &book_slots {
                    if predicate(b_info) {
                        return Some(CombineTask {
                            armor_slot: *armor_slot,
                            book_slot: *b_slot,
                            armor_desc: armor_info.kind.clone(),
                            enchant_name: ench_name,
                            required_level: req_level,
                        });
                    }
                }
            }
        }

        None
    }

    /// Automated enchanting loop: combines armor pieces with their required books in the open Anvil,
    /// dynamically throwing only the exact required XP bottles when level deficit is detected.
    pub async fn process_anvil_combines(&mut self, bot: &Client) -> bool {
        if self.enchanting_complete {
            return true;
        }

        let container_id = match self.anvil_container_id {
            Some(id) => id,
            None => return false,
        };

        // First check if slot 2 has an uncollected output from previous combine
        if let Some(item2) = self.anvil_slots.get(&2) {
            if !matches!(item2, ItemStack::Empty) {
                info!("Output slot #2 has an item; collecting it via QuickMove...");
                self.click_anvil(bot, container_id, 2, ClickType::QuickMove);
                bot.wait_ticks(20).await;
                return false;
            }
        }

        // Check if slot 0 or slot 1 has a leftover item that should be returned to inventory
        if let Some(item0) = self.anvil_slots.get(&0) {
            if !matches!(item0, ItemStack::Empty) {
                info!("Input slot #0 has a leftover item; clearing it via QuickMove...");
                self.click_anvil(bot, container_id, 0, ClickType::QuickMove);
                bot.wait_ticks(15).await;
                return false;
            }
        }
        if let Some(item1) = self.anvil_slots.get(&1) {
            if !matches!(item1, ItemStack::Empty) {
                info!("Input slot #1 has a leftover item; clearing it via QuickMove...");
                self.click_anvil(bot, container_id, 1, ClickType::QuickMove);
                bot.wait_ticks(15).await;
                return false;
            }
        }

        let task = match self.find_next_combine_task(bot) {
            Some(t) => t,
            None => {
                info!("All 4 diamond armor pieces (Helmet, Chestplate, Leggings, Boots) are fully enchanted!");
                self.enchanting_complete = true;
                self.close_anvil(bot, container_id);
                return true;
            }
        };

        let cur_lvl = self.current_level.load(Ordering::SeqCst);
        let cur_xp = self.total_experience.load(Ordering::SeqCst);

        if cur_lvl < task.required_level {
            let bottles_to_throw = bottles_needed_for_level(cur_xp, cur_lvl, task.required_level);
            info!(
                "Next combine: {} with {} requires Level {} (Current: Level {}, Total XP: {}). Closing anvil to throw {} XP bottles...",
                task.armor_desc, task.enchant_name, task.required_level, cur_lvl, cur_xp, bottles_to_throw
            );
            self.close_anvil(bot, container_id);
            bot.wait_ticks(15).await;

            self.throw_exact_xp_bottles(bot, task.required_level).await;

            bot.wait_ticks(15).await;
            info!("Re-opening Anvil to combine {} with {}...", task.armor_desc, task.enchant_name);
            self.open_anvil(bot).await;
            return false;
        }

        info!(
            "Executing Anvil Combine: {} (slot #{}) + {} Book (slot #{}) [Level Cost: {} <= Current Level: {}]",
            task.armor_desc, task.armor_slot, task.enchant_name, task.book_slot, task.required_level, cur_lvl
        );
        self.combine_in_anvil(bot, container_id, task.armor_slot, task.book_slot).await;
        false
    }

    /// Combine an item and an enchanted book inside the anvil container.
    pub async fn combine_in_anvil(
        &mut self,
        bot: &Client,
        container_id: i32,
        item_inventory_slot: i16,
        book_inventory_slot: i16,
    ) {
        info!(
            "Anvil Combine: Moving item from slot #{item_inventory_slot} and book from slot #{book_inventory_slot} into Anvil..."
        );

        // Put item into Slot 0
        self.click_anvil(bot, container_id, item_inventory_slot, ClickType::Pickup);
        bot.wait_ticks(15).await;
        self.click_anvil(bot, container_id, 0, ClickType::Pickup);
        bot.wait_ticks(15).await;

        // Put book into Slot 1
        self.click_anvil(bot, container_id, book_inventory_slot, ClickType::Pickup);
        bot.wait_ticks(15).await;
        self.click_anvil(bot, container_id, 1, ClickType::Pickup);
        bot.wait_ticks(25).await; // Wait for server to calculate recipe and update slot 2

        let server_cost = self.server_anvil_cost.load(Ordering::SeqCst);
        if server_cost > 0 {
            info!("Server Anvil Cost confirmed: {server_cost} levels");
        }

        // Collect result from Slot 2
        info!("Claiming combined enchanted item from Anvil output slot #2...");
        self.click_anvil(bot, container_id, 2, ClickType::QuickMove);
        bot.wait_ticks(25).await;

        self.anvil_slots.remove(&item_inventory_slot);
        self.anvil_slots.remove(&book_inventory_slot);
        self.anvil_slots.remove(&0);
        self.anvil_slots.remove(&1);
        self.anvil_slots.remove(&2);
        info!("Anvil combine finished! Slot #2 collected, checking next combine...");
    }

    /// Send a click packet to the open anvil container with atomic state_id sequencing.
    pub fn click_anvil(&self, bot: &Client, container_id: i32, slot: i16, click_type: ClickType) {
        let state_id = self.anvil_state_id.fetch_add(1, Ordering::SeqCst);
        let packet = ServerboundContainerClick {
            container_id,
            state_id,
            slot_num: slot,
            button_num: 0,
            click_type,
            changed_slots: Default::default(),
            carried_item: HashedStack(None),
        };
        bot.write_packet(packet);
    }

    /// Close the anvil container.
    pub fn close_anvil(&self, bot: &Client, container_id: i32) {
        info!("Closing Anvil GUI...");
        bot.write_packet(ServerboundContainerClose { container_id });
    }

    fn swap_to_hotbar(bot: &Client, inv_slot: i16, hotbar_idx: u8) {
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_total_xp_formula() {
        assert_eq!(total_xp_for_level(0), 0);
        assert_eq!(total_xp_for_level(1), 7);
        assert_eq!(total_xp_for_level(4), 40);
        assert_eq!(total_xp_for_level(5), 55);
        assert_eq!(total_xp_for_level(8), 112);
        assert_eq!(total_xp_for_level(15), 315);
        assert_eq!(total_xp_for_level(16), 352);
        assert_eq!(total_xp_for_level(17), 394);
        assert_eq!(total_xp_for_level(30), 1395);
        assert_eq!(total_xp_for_level(31), 1507);
        assert_eq!(total_xp_for_level(32), 1628);
    }

    #[test]
    fn test_bottles_needed_calculation() {
        // From 0 XP to level 4 (40 XP) -> (40 + 6) / 7 = 6 bottles
        assert_eq!(bottles_needed_for_level(0, 0, 4), 6);

        // From 0 XP to level 5 (55 XP) -> (55 + 6) / 7 = 8 bottles
        assert_eq!(bottles_needed_for_level(0, 0, 5), 8);

        // From 0 XP to level 8 (112 XP) -> (112 + 6) / 7 = 16 bottles
        assert_eq!(bottles_needed_for_level(0, 0, 8), 16);

        // From 0 XP to level 15 (315 XP) -> (315 + 6) / 7 = 45 bottles
        assert_eq!(bottles_needed_for_level(0, 0, 15), 45);

        // If already at or above level, 0 bottles needed
        assert_eq!(bottles_needed_for_level(40, 4, 4), 0);
        assert_eq!(bottles_needed_for_level(100, 7, 4), 0);

        // Deficit when partially leveled: e.g. at 16 XP, need level 4 (40 XP) -> deficit 24 -> 4 bottles
        assert_eq!(bottles_needed_for_level(16, 2, 4), 4);
    }

    #[test]
    fn test_enchanter_manager_creation() {
        let manager = EnchanterManager::new();
        assert!(!manager.anvil_placed);
        assert!(!manager.enchanting_complete);
    }

    #[test]
    fn test_combine_task_logic() {
        let mut helmet = ItemInfo {
            kind: "DiamondHelmet".to_string(),
            count: 1,
            ..Default::default()
        };
        assert!(!helmet.has_enchantment("protection", 4));

        helmet.enchantments.insert("protection".to_string(), 4);
        assert!(helmet.has_enchantment("protection", 4));
        assert!(!helmet.has_enchantment("unbreaking", 3));

        helmet.enchantments.insert("unbreaking".to_string(), 3);
        assert!(helmet.has_enchantment("unbreaking", 3));
        assert!(!helmet.has_enchantment("mending", 1));

        helmet.enchantments.insert("mending".to_string(), 1);
        assert!(helmet.has_enchantment("mending", 1));

        let mut leggings = ItemInfo {
            kind: "DiamondLeggings".to_string(),
            count: 1,
            ..Default::default()
        };
        assert!(!leggings.has_enchantment("blast_protection", 4));
        leggings.enchantments.insert("blast_protection".to_string(), 4);
        assert!(leggings.has_enchantment("blast_protection", 4));
        assert!(!leggings.has_enchantment("unbreaking", 3));
        leggings.enchantments.insert("unbreaking".to_string(), 3);
        assert!(!leggings.has_enchantment("mending", 1));
        leggings.enchantments.insert("mending".to_string(), 1);
        assert!(leggings.has_enchantment("mending", 1));
    }
}
