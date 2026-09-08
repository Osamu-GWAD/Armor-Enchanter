use crate::nbt::{
    inspect_item_with_bot, is_anvil, is_blast_protection_4, is_diamond_armor, is_diamond_boots,
    is_diamond_chestplate, is_diamond_helmet, is_diamond_leggings, is_mending, is_protection_4,
    is_unbreaking_3, is_xp_bottle, ItemInfo,
};
use azalea::inventory::operations::ClickType;
use azalea::inventory::ItemStack;
use azalea::protocol::packets::game::s_container_click::{HashedStack, ServerboundContainerClick};
use azalea::protocol::packets::game::s_interact::InteractionHand;
use azalea::protocol::packets::game::s_swing::ServerboundSwing;
use azalea::protocol::packets::game::ServerboundContainerClose;
use azalea::BlockPos;
use azalea::Client;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use tracing::{info, warn};

/// Experience points required to advance from Level L to Level L + 1 in Minecraft Java Edition.
pub fn xp_to_next_level(level: u32) -> u32 {
    match level {
        0..=15 => 2 * level + 7,
        16..=30 => 5 * level - 38,
        _ => 9 * level - 158,
    }
}

/// Cumulative XP required to reach Level L starting from Level 0 with 0 XP (Java Edition formula).
/// Official formulas:
/// - Level 0..=16:  L^2 + 6L
/// - Level 17..=31: 2.5 * L^2 - 40.5 * L + 360
/// - Level 32+:     4.5 * L^2 - 162.5 * L + 2220
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

/// Calculate the true current experience points from the current level and the progress bar (0.0 to 1.0).
pub fn calculate_current_xp(level: u32, progress: f32) -> u32 {
    let base_xp = total_xp_for_level(level);
    let bar_xp = (progress.clamp(0.0, 1.0) * xp_to_next_level(level) as f32).round() as u32;
    base_xp + bar_xp
}

/// Computes the exact XP deficit required to go from `from_level` (with progress) to `target_level`.
pub fn xp_difference(from_level: u32, from_progress: f32, target_level: u32) -> u32 {
    if from_level >= target_level {
        return 0;
    }
    let target_xp = total_xp_for_level(target_level);
    let current_xp = calculate_current_xp(from_level, from_progress);
    target_xp.saturating_sub(current_xp)
}

/// Calculate the exact number of Bottles o' Enchanting needed to go from `from_level` (with progress) to `target_level`.
/// Each bottle yields an average of 7.0 XP (random uniform 3..=11).
/// To ensure reliable level attainment on the first attempt without under-throwing due to RNG variance,
/// we use an effective conservative rate of 6.8 XP per bottle (ceiling division).
pub fn bottles_between_levels(from_level: u32, from_progress: f32, target_level: u32) -> u32 {
    let needed_xp = xp_difference(from_level, from_progress, target_level);
    if needed_xp == 0 {
        return 0;
    }
    ((needed_xp as f64 / 6.8).ceil() as u32).max(1)
}

/// Legacy/backward-compatible helper using raw total XP
pub fn bottles_needed_for_level(current_total_xp: u32, current_level: u32, target_level: u32) -> u32 {
    if current_level >= target_level {
        return 0;
    }
    let target_xp = total_xp_for_level(target_level);
    let deficit = target_xp.saturating_sub(current_total_xp);
    if deficit == 0 {
        return 0;
    }
    ((deficit as f64 / 6.8).ceil() as u32).max(1)
}

/// Smoothly look towards target (yaw, pitch) using human-like ease-in-out cosine interpolation over multiple ticks.
pub async fn smooth_look(bot: &Client, target_yaw: f32, target_pitch: f32) {
    let current_dir = bot.direction();
    let start_yaw = current_dir.y_rot();
    let start_pitch = current_dir.x_rot();

    let mut diff_yaw = (target_yaw - start_yaw) % 360.0;
    if diff_yaw > 180.0 {
        diff_yaw -= 360.0;
    } else if diff_yaw < -180.0 {
        diff_yaw += 360.0;
    }

    let diff_pitch = (target_pitch - start_pitch).clamp(-90.0, 90.0);
    let total_dist = (diff_yaw * diff_yaw + diff_pitch * diff_pitch).sqrt();

    if total_dist < 1.0 {
        bot.set_direction(target_yaw, target_pitch);
        return;
    }

    // Fast human rotation speed: approximately 20-35 degrees per tick (clamp between 2 and 6 ticks)
    if total_dist < 4.0 {
        bot.set_direction(target_yaw, target_pitch);
        return;
    }
    let steps = ((total_dist / 18.0).round() as usize).clamp(2, 6);

    for i in 1..=steps {
        let t = (i as f32) / (steps as f32);
        // Cosine ease-in-out: 0.5 * (1 - cos(pi * t))
        let ease = (1.0 - (std::f32::consts::PI * t).cos()) / 2.0;

        let cur_yaw = start_yaw + diff_yaw * ease;
        let cur_pitch = start_pitch + diff_pitch * ease;
        bot.set_direction(cur_yaw, cur_pitch);
        bot.wait_ticks(1).await;
    }

    bot.set_direction(target_yaw, target_pitch);
}

/// Send arm swing animation to the server (visible to other players & anticheats)
pub fn swing_arm(bot: &Client) {
    bot.write_packet(ServerboundSwing {
        hand: InteractionHand::MainHand,
    });
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
    pub experience_progress_milli: Arc<AtomicU32>,
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
            experience_progress_milli: Arc::new(AtomicU32::new(0)),
            total_experience: Arc::new(AtomicU32::new(0)),
            server_anvil_cost: Arc::new(AtomicU32::new(0)),
            enchanting_complete: false,
        }
    }

    /// Reset enchanting state so the bot can start the next batch of armor combining.
    pub fn reset_for_next_batch(&mut self) {
        self.enchanting_complete = false;
        self.is_enchanting = false;
        self.anvil_slots.clear();
        self.anvil_container_id = None;
        self.server_anvil_cost.store(0, Ordering::SeqCst);
    }

    /// Returns the current level and experience progress percentage (0.0 to 1.0)
    pub fn get_level_and_progress(&self) -> (u32, f32) {
        let lvl = self.current_level.load(Ordering::SeqCst);
        let milli = self.experience_progress_milli.load(Ordering::SeqCst);
        (lvl, (milli as f32) / 1000.0)
    }

    /// Splash the exact number of XP bottles needed to advance from the current level and progress
    /// to `target_level` using rapid human-like right clicks and arm swing animations.
    pub async fn throw_exact_xp_bottles(&mut self, bot: &Client, target_level: u32) {
        let target_level = target_level.min(39);
        if target_level == 0 {
            return;
        }

        let (mut current_lvl, mut current_prog) = self.get_level_and_progress();

        if current_lvl >= target_level {
            info!("Already at level {} (target: {}). No XP bottles needed.", current_lvl, target_level);
            return;
        }

        let mut bottles_to_throw = bottles_between_levels(current_lvl, current_prog, target_level);
        let needed_xp = xp_difference(current_lvl, current_prog, target_level);
        info!(
            "XP Progression: Current Level {} ({:.1}% progress) -> Target Level {} (Deficit: {} XP). Throwing {} bottles...",
            current_lvl, current_prog * 100.0, target_level, needed_xp, bottles_to_throw
        );

        // Aim smoothly down at the bot's feet
        let dir = bot.direction();
        smooth_look(bot, dir.y_rot(), 90.0).await;
        bot.wait_ticks(1).await;

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
                bot.wait_ticks(2).await;
                0
            };

            bot.set_selected_hotbar_slot(hotbar_idx);
            bot.wait_ticks(1).await;

            let batch = bottles_to_throw.min(count as u32).min(64);
            info!("Throwing batch of {batch} XP bottles at feet (rapid human click)...");
            for _ in 0..batch {
                bot.write_packet(azalea::protocol::packets::game::s_use_item::ServerboundUseItem {
                    hand: InteractionHand::MainHand,
                    seq: 0,
                    y_rot: dir.y_rot(),
                    x_rot: 90.0,
                });
                swing_arm(bot);
                bot.wait_ticks(1).await;
            }

            // Wait 5 ticks for experience orbs to be absorbed and SetExperience to arrive
            bot.wait_ticks(5).await;

            let (new_lvl, new_prog) = self.get_level_and_progress();
            current_lvl = new_lvl;
            current_prog = new_prog;
            let current_xp = calculate_current_xp(current_lvl, current_prog);
            info!(
                "XP Update: Current Level: {} ({:.1}% progress, True Current XP: {}) [Target: Level {}]",
                current_lvl, current_prog * 100.0, current_xp, target_level
            );

            if current_lvl >= target_level {
                break;
            }

            bottles_to_throw = bottles_between_levels(current_lvl, current_prog, target_level);
            if bottles_to_throw > 0 {
                info!("Slight XP shortfall due to RNG variance (need {bottles_to_throw} more bottle(s))...");
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
        smooth_look(bot, yaw_t, pitch_t).await;
        bot.wait_ticks(2).await;

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
            bot.wait_ticks(2).await;
        }

        bot.set_selected_hotbar_slot(1);
        bot.wait_ticks(1).await;

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

        // Aim smoothly at the top center of the ground block face
        let dx = (ground_pos.x as f64 + 0.5) - player_pos.x;
        let dy = (ground_pos.y as f64 + 1.0) - (player_pos.y + 1.62);
        let dz = (ground_pos.z as f64 + 0.5) - player_pos.z;
        let horizontal_dist = (dx * dx + dz * dz).sqrt();
        let yaw = (-dx).atan2(dz).to_degrees() as f32;
        let pitch = (-dy).atan2(horizontal_dist).to_degrees() as f32;
        smooth_look(bot, yaw, pitch).await;
        bot.wait_ticks(2).await;

        info!("Crosshair before place: {:?}", bot.hit_result());
        info!("Placing Anvil on ground block at {:?} (aiming yaw: {yaw:.1}, pitch: {pitch:.1})...", ground_pos);
        bot.block_interact(ground_pos);
        swing_arm(bot);
        bot.wait_ticks(5).await;

        self.anvil_pos = Some(BlockPos::new(ground_pos.x, ground_pos.y + 1, ground_pos.z));
        self.anvil_placed = true;
        info!("Anvil placed successfully at {:?}", self.anvil_pos);
    }

    /// Right-click the placed anvil to open the enchanting interface.
    pub async fn open_anvil(&self, bot: &Client) {
        self.open_anvil_with_inv(bot, &self.player_inventory).await;
    }

    /// Right-click the placed anvil with explicit inventory reference and safe hotbar slot selection.
    pub async fn open_anvil_with_inv(&self, bot: &Client, player_inventory: &HashMap<i16, ItemStack>) {
        // Select a safe hotbar slot: prioritize empty slot, then book/bottle, NEVER armor or anvil!
        let mut safe_hotbar = None;
        for h in 0..9u8 {
            let inv_slot = 36 + h as i16;
            match player_inventory.get(&inv_slot) {
                None => {
                    safe_hotbar = Some(h);
                    break;
                }
                Some(ItemStack::Empty) => {
                    safe_hotbar = Some(h);
                    break;
                }
                Some(item) => {
                    if let Some(info) = inspect_item_with_bot(item, Some(bot)) {
                        if !is_diamond_armor(&info) && !is_anvil(&info) {
                            safe_hotbar = Some(h);
                        }
                    }
                }
            }
        }
        let target_hotbar = safe_hotbar.unwrap_or(0);
        bot.set_selected_hotbar_slot(target_hotbar);
        bot.wait_ticks(1).await;

        let target_pos = self.anvil_pos.unwrap_or_else(|| {
            let p = bot.position();
            let gy = (p.y - 0.1).floor() as i32;
            BlockPos::new(
                p.x.floor() as i32 + 1,
                gy + 1,
                p.z.floor() as i32,
            )
        });

        // Aim smoothly at the target anvil block
        let player_pos = bot.position();
        let dx = (target_pos.x as f64 + 0.5) - player_pos.x;
        let dy = (target_pos.y as f64 + 0.5) - (player_pos.y + 1.62);
        let dz = (target_pos.z as f64 + 0.5) - player_pos.z;
        let horizontal_dist = (dx * dx + dz * dz).sqrt();
        let yaw = (-dx).atan2(dz).to_degrees() as f32;
        let pitch = (-dy).atan2(horizontal_dist).to_degrees() as f32;
        smooth_look(bot, yaw, pitch).await;
        bot.wait_ticks(2).await;

        info!("Interacting to open Anvil at {:?} with safe hotbar slot #{}...", target_pos, target_hotbar);
        bot.block_interact(target_pos);
        swing_arm(bot);
        bot.wait_ticks(6).await;
    }

    /// Handles Anvil GUI opening.
    pub fn on_open_screen(&mut self, container_id: i32, title: &str) {
        info!("Anvil Screen opened: container_id={container_id}, title='{title}'");
        self.anvil_container_id = Some(container_id);
        self.anvil_slots.clear();
        self.anvil_state_id.store(0, Ordering::SeqCst);
        self.server_anvil_cost.store(0, Ordering::SeqCst);
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

    /// Check if all 4 diamond armor pieces are present and fully enchanted according to user specifications.
    pub fn check_all_armor_enchanted(&self, bot: &Client) -> (bool, String) {
        let mut items = Vec::new();

        // Check Anvil container slots (player section 3..38)
        for slot in 3..=38 {
            if let Some(item) = self.anvil_slots.get(&slot) {
                if let Some(info) = inspect_item_with_bot(item, Some(bot)) {
                    items.push(info);
                }
            }
        }

        // Also check tracked player inventory
        for item in self.player_inventory.values() {
            if let Some(info) = inspect_item_with_bot(item, Some(bot)) {
                items.push(info);
            }
        }

        self.check_all_armor_enchanted_from_info(&items)
    }

    /// Pure helper to verify full enchantment across a collection of ItemInfo.
    pub fn check_all_armor_enchanted_from_info(&self, items: &[ItemInfo]) -> (bool, String) {
        let mut helm = false;
        let mut chest = false;
        let mut legs = false;
        let mut boots = false;

        for info in items {
            if is_diamond_helmet(info)
                && info.has_enchantment("protection", 4)
                && info.has_enchantment("unbreaking", 3)
                && info.has_enchantment("mending", 1)
            {
                helm = true;
            } else if is_diamond_chestplate(info)
                && info.has_enchantment("protection", 4)
                && info.has_enchantment("unbreaking", 3)
                && info.has_enchantment("mending", 1)
            {
                chest = true;
            } else if is_diamond_leggings(info)
                && info.has_enchantment("blast_protection", 4)
                && info.has_enchantment("unbreaking", 3)
                && info.has_enchantment("mending", 1)
            {
                legs = true;
            } else if is_diamond_boots(info)
                && info.has_enchantment("blast_protection", 4)
                && info.has_enchantment("unbreaking", 3)
                && info.has_enchantment("mending", 1)
            {
                boots = true;
            }
        }

        let all_done = helm && chest && legs && boots;
        let summary = format!(
            "Helmet: {}, Chestplate: {}, Leggings: {}, Boots: {}",
            if helm { "MAXED" } else { "INCOMPLETE" },
            if chest { "MAXED" } else { "INCOMPLETE" },
            if legs { "MAXED" } else { "INCOMPLETE" },
            if boots { "MAXED" } else { "INCOMPLETE" },
        );

        (all_done, summary)
    }

    /// Unequip any armor worn in equipment slots 5..=8 back into player inventory.
    pub async fn ensure_no_worn_armor(bot: &Client, player_inv: &HashMap<i16, ItemStack>) {
        for armor_slot in 5..=8i16 {
            if let Some(item) = player_inv.get(&armor_slot) {
                if !matches!(item, ItemStack::Empty) {
                    if let Some(info) = inspect_item_with_bot(item, Some(bot)) {
                        info!("Found worn armor '{}' in equipment slot #{armor_slot}! Unequipping via QuickMove...", info.kind);
                        let packet = ServerboundContainerClick {
                            container_id: 0,
                            state_id: 0,
                            slot_num: armor_slot,
                            button_num: 0,
                            click_type: ClickType::QuickMove,
                            changed_slots: Default::default(),
                            carried_item: HashedStack(None),
                        };
                        bot.write_packet(packet);
                        bot.wait_ticks(3).await;
                    }
                }
            }
        }
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

        let cur_lvl = self.current_level.load(Ordering::SeqCst);
        let server_cost = self.server_anvil_cost.load(Ordering::SeqCst);

        let has_input0 = self.anvil_slots.get(&0).map(|s| !matches!(s, ItemStack::Empty)).unwrap_or(false);
        let has_input1 = self.anvil_slots.get(&1).map(|s| !matches!(s, ItemStack::Empty)).unwrap_or(false);
        let has_output2 = self.anvil_slots.get(&2).map(|s| !matches!(s, ItemStack::Empty)).unwrap_or(false);

        // If inputs exist and server cost > current level, return inputs to inventory and throw XP
        if (has_input0 || has_input1) && server_cost > 0 && server_cost < 40 && server_cost > cur_lvl {
            info!("Anvil inputs present but server cost ({server_cost}) > current level ({cur_lvl})! Returning inputs to inventory to throw XP...");
            if has_input0 {
                self.click_anvil(bot, container_id, 0, ClickType::QuickMove);
                bot.wait_ticks(3).await;
            }
            if has_input1 {
                self.click_anvil(bot, container_id, 1, ClickType::QuickMove);
                bot.wait_ticks(3).await;
            }
            self.anvil_slots.remove(&0);
            self.anvil_slots.remove(&1);
            self.anvil_slots.remove(&2);

            self.close_anvil(bot, container_id);
            bot.wait_ticks(2).await;
            self.throw_exact_xp_bottles(bot, server_cost).await;
            bot.wait_ticks(2).await;
            Self::ensure_no_worn_armor(bot, &self.player_inventory).await;
            bot.wait_ticks(2).await;
            self.open_anvil(bot).await;
            bot.wait_ticks(5).await;
            return false;
        }

        // If output slot 2 has an item and level suffices, collect it via QuickMove
        if has_output2 && (server_cost == 0 || (server_cost < 40 && cur_lvl >= server_cost)) {
            info!("Output slot #2 has an item and level suffices (Level {cur_lvl} >= Cost {server_cost}); collecting via QuickMove...");
            self.click_anvil(bot, container_id, 2, ClickType::QuickMove);
            bot.wait_ticks(6).await;
            self.anvil_slots.remove(&0);
            self.anvil_slots.remove(&1);
            self.anvil_slots.remove(&2);
            self.server_anvil_cost.store(0, Ordering::SeqCst);
            return false;
        }

        // Clear any leftover inputs
        if has_input0 {
            info!("Input slot #0 has a leftover item; clearing it via QuickMove...");
            self.click_anvil(bot, container_id, 0, ClickType::QuickMove);
            bot.wait_ticks(3).await;
            self.anvil_slots.remove(&0);
            return false;
        }
        if has_input1 {
            info!("Input slot #1 has a leftover item; clearing it via QuickMove...");
            self.click_anvil(bot, container_id, 1, ClickType::QuickMove);
            bot.wait_ticks(3).await;
            self.anvil_slots.remove(&1);
            return false;
        }

        let task = match self.find_next_combine_task(bot) {
            Some(t) => t,
            None => {
                let (all_done, summary) = self.check_all_armor_enchanted(bot);
                if all_done {
                    info!("All 4 diamond armor pieces confirmed fully enchanted! ({summary})");
                    self.enchanting_complete = true;
                    self.close_anvil(bot, container_id);
                    return true;
                } else {
                    info!("Combine task not ready yet ({summary}). Waiting for inventory synchronization...");
                    return false;
                }
            }
        };

        let (cur_lvl, cur_prog) = self.get_level_and_progress();

        if cur_lvl < task.required_level {
            let bottles_to_throw = bottles_between_levels(cur_lvl, cur_prog, task.required_level);
            let needed_xp = xp_difference(cur_lvl, cur_prog, task.required_level);
            info!(
                "Next combine: {} with {} requires Level {} (Current: Level {}, {:.1}% progress, Need: {} XP). Closing anvil to throw {} XP bottles...",
                task.armor_desc, task.enchant_name, task.required_level, cur_lvl, cur_prog * 100.0, needed_xp, bottles_to_throw
            );
            self.close_anvil(bot, container_id);
            bot.wait_ticks(2).await;

            self.throw_exact_xp_bottles(bot, task.required_level).await;
            bot.wait_ticks(2).await;

            // Ensure no armor was accidentally equipped
            Self::ensure_no_worn_armor(bot, &self.player_inventory).await;
            bot.wait_ticks(2).await;

            info!("Re-opening Anvil to combine {} with {}...", task.armor_desc, task.enchant_name);
            self.open_anvil(bot).await;
            bot.wait_ticks(5).await;
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
        self.server_anvil_cost.store(0, Ordering::SeqCst);
        info!(
            "Anvil Combine: Moving item from slot #{item_inventory_slot} and book from slot #{book_inventory_slot} into Anvil..."
        );

        // Put item into Slot 0
        self.click_anvil(bot, container_id, item_inventory_slot, ClickType::Pickup);
        bot.wait_ticks(3).await;
        self.click_anvil(bot, container_id, 0, ClickType::Pickup);
        bot.wait_ticks(3).await;

        // Put book into Slot 1
        self.click_anvil(bot, container_id, book_inventory_slot, ClickType::Pickup);
        bot.wait_ticks(3).await;
        self.click_anvil(bot, container_id, 1, ClickType::Pickup);
        bot.wait_ticks(5).await; // Wait for server to calculate recipe and update slot 2

        let server_cost = self.server_anvil_cost.load(Ordering::SeqCst);
        let cur_lvl = self.current_level.load(Ordering::SeqCst);
        if server_cost > 0 && server_cost < 40 {
            info!("Server Anvil Cost confirmed: {server_cost} levels (Current Level: {cur_lvl})");
        }

        if server_cost > 0 && server_cost < 40 && server_cost > cur_lvl {
            info!("Server cost ({server_cost}) exceeds current level ({cur_lvl})! Taking items back to throw more XP...");
            self.click_anvil(bot, container_id, 0, ClickType::QuickMove);
            bot.wait_ticks(3).await;
            self.click_anvil(bot, container_id, 1, ClickType::QuickMove);
            bot.wait_ticks(3).await;
            self.anvil_slots.remove(&0);
            self.anvil_slots.remove(&1);
            self.anvil_slots.remove(&2);

            self.close_anvil(bot, container_id);
            bot.wait_ticks(2).await;
            self.throw_exact_xp_bottles(bot, server_cost).await;
            bot.wait_ticks(2).await;
            Self::ensure_no_worn_armor(bot, &self.player_inventory).await;
            bot.wait_ticks(2).await;
            self.open_anvil(bot).await;
            bot.wait_ticks(5).await;
            return;
        }

        // Collect result from Slot 2
        info!("Claiming combined enchanted item from Anvil output slot #2...");
        self.click_anvil(bot, container_id, 2, ClickType::QuickMove);
        bot.wait_ticks(6).await; // 6 ticks wait for SetExperience and inventory updates

        self.anvil_slots.remove(&book_inventory_slot);
        self.anvil_slots.remove(&0);
        self.anvil_slots.remove(&1);
        self.anvil_slots.remove(&2);
        self.server_anvil_cost.store(0, Ordering::SeqCst);
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
    pub fn close_anvil(&mut self, bot: &Client, container_id: i32) {
        info!("Closing Anvil GUI (container #{container_id})...");
        bot.write_packet(ServerboundContainerClose { container_id });
        self.anvil_container_id = None;
        self.server_anvil_cost.store(0, Ordering::SeqCst);
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
        // From 0 XP to level 4 (40 XP) -> (40 / 6.8).ceil() = 6 bottles
        assert_eq!(bottles_needed_for_level(0, 0, 4), 6);

        // From 0 XP to level 5 (55 XP) -> (55 / 6.8).ceil() = 9 bottles
        assert_eq!(bottles_needed_for_level(0, 0, 5), 9);

        // From 0 XP to level 8 (112 XP) -> (112 / 6.8).ceil() = 17 bottles
        assert_eq!(bottles_needed_for_level(0, 0, 8), 17);

        // From 0 XP to level 15 (315 XP) -> (315 / 6.8).ceil() = 47 bottles
        assert_eq!(bottles_needed_for_level(0, 0, 15), 47);

        // If already at or above level, 0 bottles needed
        assert_eq!(bottles_needed_for_level(40, 4, 4), 0);
        assert_eq!(bottles_needed_for_level(100, 7, 4), 0);
    }

    #[test]
    fn test_level_to_level_xp_and_bottles() {
        // Level 0 to 1: 7 XP -> 2 bottles
        assert_eq!(xp_difference(0, 0.0, 1), 7);
        assert_eq!(bottles_between_levels(0, 0.0, 1), 2);

        // Level 0 to 4: 40 XP -> 6 bottles
        assert_eq!(xp_difference(0, 0.0, 4), 40);
        assert_eq!(bottles_between_levels(0, 0.0, 4), 6);

        // Level 0 to 5: 55 XP -> 9 bottles
        assert_eq!(xp_difference(0, 0.0, 5), 55);
        assert_eq!(bottles_between_levels(0, 0.0, 5), 9);

        // Level 0 to 8: 112 XP -> 17 bottles
        assert_eq!(xp_difference(0, 0.0, 8), 112);
        assert_eq!(bottles_between_levels(0, 0.0, 8), 17);

        // Level 0 to 13: 247 XP -> 37 bottles
        assert_eq!(xp_difference(0, 0.0, 13), 247);
        assert_eq!(bottles_between_levels(0, 0.0, 13), 37);

        // Level 4 to 8: 112 - 40 = 72 XP -> 11 bottles
        assert_eq!(xp_difference(4, 0.0, 8), 72);
        assert_eq!(bottles_between_levels(4, 0.0, 8), 11);

        // Level 8 to 13: 247 - 112 = 135 XP -> 20 bottles
        assert_eq!(xp_difference(8, 0.0, 13), 135);
        assert_eq!(bottles_between_levels(8, 0.0, 13), 20);

        // Level 3 with 50% progress to Level 4:
        // Level 3 total: 27 XP, next level needs 13 XP. 50% = 7 XP. Current XP = 34.
        // Need to reach Level 4 (40 XP) -> deficit 6 XP -> 1 bottle!
        assert_eq!(xp_difference(3, 0.5, 4), 6);
        assert_eq!(bottles_between_levels(3, 0.5, 4), 1);
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

    #[test]
    fn test_check_all_armor_enchanted_detection() {
        let manager = EnchanterManager::new();

        // Initially empty
        let (all_done, summary) = manager.check_all_armor_enchanted_from_info(&[]);
        assert!(!all_done);
        assert!(summary.contains("INCOMPLETE"));

        let mut helm = ItemInfo { kind: "DiamondHelmet".to_string(), count: 1, ..Default::default() };
        helm.enchantments.insert("protection".to_string(), 4);
        helm.enchantments.insert("unbreaking".to_string(), 3);
        helm.enchantments.insert("mending".to_string(), 1);

        let mut chest = ItemInfo { kind: "DiamondChestplate".to_string(), count: 1, ..Default::default() };
        chest.enchantments.insert("protection".to_string(), 4);
        // Chest only has prot 4 (missing unb 3 and mending)
        let (all_done, summary) = manager.check_all_armor_enchanted_from_info(&[helm.clone(), chest.clone()]);
        assert!(!all_done);
        assert!(summary.contains("Helmet: MAXED"));
        assert!(summary.contains("Chestplate: INCOMPLETE"));

        chest.enchantments.insert("unbreaking".to_string(), 3);
        chest.enchantments.insert("mending".to_string(), 1);

        let mut legs = ItemInfo { kind: "DiamondLeggings".to_string(), count: 1, ..Default::default() };
        legs.enchantments.insert("blast_protection".to_string(), 4);
        legs.enchantments.insert("unbreaking".to_string(), 3);
        legs.enchantments.insert("mending".to_string(), 1);

        let mut boots = ItemInfo { kind: "DiamondBoots".to_string(), count: 1, ..Default::default() };
        boots.enchantments.insert("blast_protection".to_string(), 4);
        boots.enchantments.insert("unbreaking".to_string(), 3);
        boots.enchantments.insert("mending".to_string(), 1);

        let (all_done, summary) = manager.check_all_armor_enchanted_from_info(&[helm, chest, legs, boots]);
        assert!(all_done);
        assert_eq!(summary, "Helmet: MAXED, Chestplate: MAXED, Leggings: MAXED, Boots: MAXED");
    }
}
