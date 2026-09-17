use crate::nbt::{
    inspect_item_with_bot, is_anvil, is_blast_protection_4, is_diamond_armor, is_diamond_boots,
    is_diamond_chestplate, is_diamond_helmet, is_diamond_leggings, is_mending, is_protection_4,
    is_single_mending, is_single_unbreaking_3, is_unbreaking_3, is_unbreaking_and_mending,
    is_xp_bottle, ItemInfo,
};
use azalea::core::direction::Direction;
use azalea::inventory::operations::ClickType;
use azalea::inventory::ItemStack;
use azalea::protocol::packets::game::s_container_click::{HashedStack, ServerboundContainerClick};
use azalea::protocol::packets::game::s_interact::InteractionHand;
use azalea::protocol::packets::game::s_player_action::{Action, ServerboundPlayerAction};
use azalea::protocol::packets::game::s_swing::ServerboundSwing;
use azalea::protocol::packets::game::ServerboundContainerClose;
use azalea::BlockPos;
use azalea::Client;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use tokio::sync::Notify;
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
    let next_cost = xp_to_next_level(level);
    // Floor progress XP and clamp to at most next_cost - 1, because if progress reached
    // next_cost the player would have already leveled up to level + 1.
    // An epsilon buffer (1e-4) prevents float rounding errors (e.g. 5.9999) from truncating
    // to points - 1 and causing false deficit inflation.
    let max_bar_xp = next_cost.saturating_sub(1);
    let bar_xp = (((progress.clamp(0.0, 1.0) * next_cost as f32) + 1e-4).floor() as u32).min(max_bar_xp);
    base_xp + bar_xp
}

/// Computes the exact XP deficit required to go from `from_level` (with progress) to `target_level`.
pub fn xp_difference(from_level: u32, from_progress: f32, target_level: u32) -> u32 {
    if from_level >= target_level {
        return 0;
    }
    let target_xp = total_xp_for_level(target_level);
    let current_xp = calculate_current_xp(from_level, from_progress);
    // As long as from_level < target_level, deficit is mathematically at least 1.
    target_xp.saturating_sub(current_xp).max(1)
}

/// Calculate the exact number of Bottles o' Enchanting needed to go from `from_level` (with progress) to `target_level`.
/// Each bottle yields an average of 7.0 XP (random uniform 3..=11).
/// To ensure reliable level attainment on the first attempt without under-throwing due to RNG variance,
/// we use an effective conservative rate of 6.8 XP per bottle (ceiling division).
pub fn bottles_between_levels(from_level: u32, from_progress: f32, target_level: u32) -> u32 {
    if from_level >= target_level {
        return 0;
    }
    let needed_xp = xp_difference(from_level, from_progress, target_level);
    ((needed_xp as f64 / 6.8).ceil() as u32).max(1)
}

/// Maximum safe number of XP bottles to throw in a single batch without ANY possibility of overshooting.
/// Since each bottle yields at most 11 XP (uniform 3..=11), throwing `(needed_xp - 1) / 11` bottles
/// is mathematically guaranteed to yield strictly less than `needed_xp`.
/// If `needed_xp <= 11`, returns 0 to signal single-bottle precision mode.
pub fn safe_batch_size(needed_xp: u32) -> u32 {
    (needed_xp.saturating_sub(1) / 11).min(64)
}

/// Strict safe batch size for zero-overshoot XP bottle consumption.
/// Unlike standard safe_batch_size, this strict calculation:
/// 1. Enforces single-bottle precision whenever needed_xp <= 22 (since 2 bottles can yield up to 22 XP).
/// 2. For deficits > 22 XP, subtracts an 11 XP safety margin before division by 11 so that even if every
///    thrown bottle rolls the absolute maximum of 11 XP, the remaining deficit is guaranteed to be >= 11 XP.
/// 3. Caps batch size to 16 bottles to prevent entity/orb desync and anticheat rate-limiting.
pub fn strict_safe_batch_size(needed_xp: u32) -> u32 {
    if needed_xp <= 22 {
        0
    } else {
        (needed_xp.saturating_sub(11) / 11).min(16)
    }
}

/// Strict calculation of bottles needed to reach target level without overshooting.
/// Uses the maximum possible yield per bottle (11 XP) to determine the strict lower bound
/// of bottles that can be safely thrown.
pub fn strict_bottles_between_levels(from_level: u32, from_progress: f32, target_level: u32) -> u32 {
    if from_level >= target_level {
        return 0;
    }
    let needed_xp = xp_difference(from_level, from_progress, target_level);
    (needed_xp / 11).max(if needed_xp > 0 { 1 } else { 0 })
}

/// Parse a user-specified XP throw speed string into tick delay between bottle throws.
/// Supports:
/// - Plain numbers (e.g. "1" = 1 tick, "2" = 2 ticks, "0" = 0 ticks/burst).
/// - If number >= 20, treated as milliseconds (e.g. "50" -> 1 tick, "100" -> 2 ticks).
/// - Explicit millisecond format (e.g. "50ms" -> 1 tick, "100ms" -> 2 ticks, "200ms" -> 4 ticks).
/// - Explicit tick format (e.g. "1t" -> 1 tick, "2t" -> 2 ticks).
/// - Presets: "instant" / "burst" -> 0, "normal" / "fast" -> 1, "slow" / "safe" -> 2, "slower" -> 3.
pub fn parse_throw_speed(val: &str) -> u32 {
    let s = val.trim().to_lowercase();
    match s.as_str() {
        "instant" | "max" | "burst" | "fastest" => 0,
        "fast" | "normal" | "default" => 1,
        "slow" | "safe" => 2,
        "slower" => 3,
        _ => {
            if let Some(stripped) = s.strip_suffix("ms") {
                if let Ok(ms) = stripped.trim().parse::<u64>() {
                    return ((ms + 49) / 50).max(1) as u32;
                }
            }
            if let Some(stripped) = s.strip_suffix('t') {
                if let Ok(ticks) = stripped.trim().parse::<u32>() {
                    return ticks;
                }
            }
            if let Ok(num) = s.parse::<u32>() {
                if num >= 20 {
                    return ((num + 49) / 50).max(1);
                }
                return num;
            }
            1
        }
    }
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

    // Fast, crisp human rotation speed: approximately 45-60 degrees per tick (clamp between 1 and 3 ticks)
    if total_dist < 4.0 {
        bot.set_direction(target_yaw, target_pitch);
        return;
    }
    let steps = ((total_dist / 45.0).ceil() as usize).clamp(1, 3);

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
    pub experience_updated: Arc<Notify>,
    pub enchanting_complete: bool,
    pub failed_combine_attempts: u32,
    combine_clicks: std::collections::VecDeque<(i16, u8, ClickType)>,
    pending_click: Option<(i16, ItemStack, std::time::Instant, HashMap<i16, ItemStack>, u32)>,
    recipe_wait_since: Option<std::time::Instant>,
    content_wait_since: Option<std::time::Instant>,
    pub xp_target: Option<u32>,
    pub restock_needed: bool,
    pub throw_speed_ticks: u32,
    pub strict_calculation: bool,
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
            experience_updated: Arc::new(Notify::new()),
            enchanting_complete: false,
            failed_combine_attempts: 0,
            combine_clicks: Default::default(),
            pending_click: None,
            recipe_wait_since: None,
            content_wait_since: None,
            xp_target: None,
            restock_needed: false,
            throw_speed_ticks: 1,
            strict_calculation: true,
        }
    }

    /// Reset enchanting state so the bot can start the next batch of armor combining.
    pub fn reset_for_next_batch(&mut self) {
        self.enchanting_complete = false;
        self.restock_needed = false;
        self.xp_target = None;
        self.is_enchanting = false;
        self.combine_clicks.clear();
        self.pending_click = None;
        self.recipe_wait_since = None;
        self.content_wait_since = None;
        self.failed_combine_attempts = 0;
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

    /// Find the inventory slot (in 9..=44) containing XP bottles that has the lowest bottle count.
    /// This ensures partial stacks (e.g. 2, 7, 8 bottles) are consumed and consolidated first,
    /// preventing inventory clutter caused by opening fresh 64-stacks every time.
    pub fn find_lowest_count_xp_slot(
        inventory: &HashMap<i16, ItemStack>,
        bot: Option<&Client>,
    ) -> Option<(i16, i32)> {
        let mut candidates: Vec<(i32, u8, i16)> = Vec::new();
        for (&slot, item) in inventory {
            if (9..=44).contains(&slot) {
                if let Some(info) = inspect_item_with_bot(item, bot) {
                    if is_xp_bottle(&info) && info.count > 0 {
                        // Tie-breaker priority:
                        // 0: slot 36 (hotbar slot 0, already in the preferred throw slot)
                        // 1: slot 37..=44 (in hotbar, only needs hotbar selection change, no swap packet)
                        // 2: slot 9..=35 (in main inventory, requires swap packet to hotbar)
                        let priority: u8 = if slot == 36 {
                            0
                        } else if (36..=44).contains(&slot) {
                            1
                        } else {
                            2
                        };
                        candidates.push((info.count, priority, slot));
                    }
                }
            }
        }
        candidates
            .into_iter()
            .min()
            .map(|(count, _, slot)| (slot, count))
    }

    /// Splash the exact number of XP bottles needed to advance from the current level and progress
    /// to `target_level` using rapid human-like right clicks and arm swing animations.
    pub async fn throw_exact_xp_bottles(&mut self, bot: &Client, target_level: u32) {
        let target_level = target_level.min(39);
        if target_level == 0 {
            return;
        }

        // Wait 2 ticks to allow any in-flight SetExperience packet from container transactions to settle
        bot.wait_ticks(2).await;

        let (mut current_lvl, mut current_prog) = self.get_level_and_progress();

        if current_lvl >= target_level {
            info!("Already at level {} (target: {}). No XP bottles needed.", current_lvl, target_level);
            return;
        }

        let needed_xp = xp_difference(current_lvl, current_prog, target_level);
        info!(
            "XP Progression: Current Level {} ({:.1}% progress) -> Target Level {} (Deficit: {} XP). Initiating zero-overshoot throwing...",
            current_lvl, current_prog * 100.0, target_level, needed_xp
        );

        // Aim smoothly down at ground/feet, orienting towards anvil or 180 degrees away from any hopper
        let target_yaw = if let Some(apos) = self.anvil_pos.or_else(|| Self::find_nearby_anvil(bot)) {
            let p = bot.position();
            let dx = (apos.x as f64 + 0.5) - p.x;
            let dz = (apos.z as f64 + 0.5) - p.z;
            (-dx).atan2(dz).to_degrees() as f32
        } else if let Some(hpos) = Self::find_nearby_hopper(bot) {
            let p = bot.position();
            let dx = (hpos.x as f64 + 0.5) - p.x;
            let dz = (hpos.z as f64 + 0.5) - p.z;
            ((-dx).atan2(dz).to_degrees() as f32 + 180.0) % 360.0
        } else {
            bot.direction().y_rot()
        };
        smooth_look(bot, target_yaw, 85.0).await;
        bot.wait_ticks(1).await;

        let mut stall_count = 0;
        let mut last_xp = calculate_current_xp(current_lvl, current_prog);

        while current_lvl < target_level {
            let needed_xp = xp_difference(current_lvl, current_prog, target_level);
            if needed_xp == 0 {
                break;
            }

            // Calculate safe batch size based on whether strict calculation is enabled.
            let (safe_batch, mode_desc) = if self.strict_calculation {
                let sb = strict_safe_batch_size(needed_xp);
                (sb, if sb > 0 { "strict safe batch" } else { "strict single-bottle precision" })
            } else {
                let sb = safe_batch_size(needed_xp);
                (sb, if sb > 0 { "safe batch" } else { "single-bottle precision" })
            };
            let count_to_throw = if safe_batch > 0 { safe_batch } else { 1 };

            // Find XP bottles in inventory - always prioritize the slot with the lowest amount
            let (slot, slot_count) = match Self::find_lowest_count_xp_slot(&self.player_inventory, Some(bot)) {
                Some(s) => s,
                None => {
                    warn!("No XP bottles found in inventory to reach level {target_level}!");
                    break;
                }
            };

            // If item is in main inventory (slots 9..35), swap to a safe hotbar slot (never armor or anvil!)
            let hotbar_idx = if slot >= 36 && slot <= 44 {
                (slot - 36) as u8
            } else {
                let safe_hotbar = (0..9u8).find(|&h| {
                    let h_slot = 36 + h as i16;
                    match self.player_inventory.get(&h_slot) {
                        None | Some(ItemStack::Empty) => true,
                        Some(item) => inspect_item_with_bot(item, Some(bot)).map_or(true, |info| {
                            !is_diamond_armor(&info) && !is_anvil(&info)
                        }),
                    }
                }).unwrap_or(0);

                info!("Swapping XP bottles from slot #{slot} to safe hotbar slot #{safe_hotbar}...");
                Self::swap_to_hotbar(bot, slot, safe_hotbar);
                let bottles = self.player_inventory.remove(&slot).unwrap_or(ItemStack::Empty);
                let old_hotbar = self.player_inventory.insert(36 + safe_hotbar as i16, bottles).unwrap_or(ItemStack::Empty);
                self.player_inventory.insert(slot, old_hotbar);
                bot.wait_ticks(2).await;
                safe_hotbar
            };

            bot.set_selected_hotbar_slot(hotbar_idx);
            bot.wait_ticks(1).await;

            let actual_throw = count_to_throw.min(slot_count as u32);
            info!(
                "Throwing {actual_throw} XP bottle(s) at feet (needed deficit: {needed_xp} XP, target: Level {target_level}, mode: {mode_desc}, delay: {}t)...",
                self.throw_speed_ticks
            );

            for _ in 0..actual_throw {
                bot.write_packet(azalea::protocol::packets::game::s_use_item::ServerboundUseItem {
                    hand: InteractionHand::MainHand,
                    seq: 0,
                    y_rot: target_yaw,
                    x_rot: 85.0,
                });
                swing_arm(bot);
                if self.throw_speed_ticks > 0 {
                    bot.wait_ticks(self.throw_speed_ticks as usize).await;
                }
            }

            // Update local inventory count estimate for this slot
            if let Some(item) = self.player_inventory.get_mut(&(36 + hotbar_idx as i16)) {
                if let ItemStack::Present(data) = item {
                    if (data.count as u32) <= actual_throw {
                        *item = ItemStack::Empty;
                    } else {
                        data.count -= actual_throw as i32;
                    }
                }
            }

            // Wait for server SetExperience packet to arrive (handles latency desync gracefully).
            // When throwing multi-bottle batches, allow extra settle ticks so all trailing orbs
            // land and are registered by the server before we recalculate the deficit.
            let got_update = tokio::time::timeout(
                std::time::Duration::from_millis(500),
                self.experience_updated.notified(),
            ).await.is_ok();

            if got_update {
                let settle_ticks = if actual_throw > 1 {
                    3 + (actual_throw / 4).min(4)
                } else {
                    2
                };
                bot.wait_ticks(settle_ticks as usize).await;
            } else {
                bot.wait_ticks(4).await;
            }

            let (new_lvl, new_prog) = self.get_level_and_progress();
            current_lvl = new_lvl;
            current_prog = new_prog;
            let current_xp = calculate_current_xp(current_lvl, current_prog);
            info!(
                "XP Update: Current Level: {} ({:.1}% progress, True Current XP: {}) [Target: Level {}]",
                current_lvl, current_prog * 100.0, current_xp, target_level
            );

            if current_lvl >= target_level {
                info!("Target Level {target_level} reached! Halting XP bottle consumption immediately (zero overshoot).");
                break;
            }

            if current_xp <= last_xp {
                stall_count += 1;
                if stall_count >= 5 {
                    warn!("XP did not increase after 5 throw attempts (out of XP bottles or desync). Exiting XP routine.");
                    break;
                }
            } else {
                stall_count = 0;
            }
            last_xp = current_xp;
        }

        info!("Finished XP consumption routine. Final level: {current_lvl} (target was {target_level}).");
    }

/// Scans nearby blocks around the bot to find any placed anvil within reach.
/// Prioritizes anvils that the bot is facing.
pub fn find_nearby_anvil(bot: &Client) -> Option<BlockPos> {
    let p = bot.position();
    let bx = p.x.floor() as i32;
    let ground_y = (p.y - 0.1).floor() as i32;
    let bz = p.z.floor() as i32;

    let eye_x = p.x;
    let eye_y = p.y + 1.62;
    let eye_z = p.z;

    let dir = bot.direction();
    let yaw_rad = (dir.y_rot() as f64).to_radians();
    let pitch_rad = (dir.x_rot() as f64).to_radians();
    let look_x = -yaw_rad.sin() * pitch_rad.cos();
    let look_y = -pitch_rad.sin();
    let look_z = yaw_rad.cos() * pitch_rad.cos();

    let mut candidates: Vec<(f64, BlockPos)> = Vec::new();

    // Query world block states synchronously without holding lock across awaits
    {
        let w = bot.world();
        let world = w.read();

        // Search horizontal radius +-3, vertical -1..=+2
        for dx in -3..=3 {
            for dz in -3..=3 {
                for dy in -1..=2 {
                    let pos = BlockPos::new(bx + dx, ground_y + dy, bz + dz);
                    if let Some(state) = world.get_block_state(pos) {
                        let s_str = format!("{state:?}").to_lowercase();
                        if s_str.contains("anvil") {
                            let cx = pos.x as f64 + 0.5;
                            let cy = pos.y as f64 + 0.5;
                            let cz = pos.z as f64 + 0.5;

                            let to_x = cx - eye_x;
                            let to_y = cy - eye_y;
                            let to_z = cz - eye_z;
                            let dist = (to_x * to_x + to_y * to_y + to_z * to_z).sqrt();

                            if dist <= 4.5 && dist > 0.01 {
                                let dot = (look_x * to_x + look_y * to_y + look_z * to_z) / dist;
                                // Higher score for anvils in front of crosshair and closer
                                let score = dot * 2.0 - (dist * 0.5);
                                candidates.push((score, pos));
                            }
                        }
                    }
                }
            }
        }
    } // Read guard dropped here!

    candidates.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    candidates.first().map(|(_, pos)| *pos)
}

/// Scans nearby blocks around the bot to find the nearest hopper within 2 blocks of the bot.
pub fn find_nearby_hopper(bot: &Client) -> Option<BlockPos> {
    let p = bot.position();
    let bx = p.x.floor() as i32;
    let by = p.y.floor() as i32;
    let bz = p.z.floor() as i32;

    let mut candidates: Vec<(f64, BlockPos)> = Vec::new();

    {
        let w = bot.world();
        let world = w.read();

        // Search horizontal radius +-2, vertical -2..=2 (within 2 blocks of the bot)
        for dx in -2..=2 {
            for dz in -2..=2 {
                for dy in -2..=2 {
                    let pos = BlockPos::new(bx + dx, by + dy, bz + dz);
                    if let Some(state) = world.get_block_state(pos) {
                        let s_str = format!("{state:?}").to_lowercase();
                        if s_str.contains("hopper") {
                            let cx = pos.x as f64 + 0.5;
                            let cy = pos.y as f64 + 0.5;
                            let cz = pos.z as f64 + 0.5;

                            let dist = ((cx - p.x).powi(2) + (cy - p.y).powi(2) + (cz - p.z).powi(2)).sqrt();
                            if dist <= 2.85 {
                                candidates.push((dist, pos));
                            }
                        }
                    }
                }
            }
        }
    }

    candidates.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    candidates.first().map(|(_, pos)| *pos)
}

/// Finds the best ground position and target anvil position to place a new anvil,
/// oriented in front of the bot.
pub fn find_placement_pos(bot: &Client) -> (BlockPos, BlockPos) {
    let p = bot.position();
    let bx = p.x.floor() as i32;
    let ground_y = (p.y - 0.1).floor() as i32;
    let bz = p.z.floor() as i32;

    let dir = bot.direction();
    let yaw_rad = (dir.y_rot() as f64).to_radians();
    let look_x = -yaw_rad.sin();
    let look_z = yaw_rad.cos();

    // Cardinals sorted by alignment with bot's horizontal look direction
    let mut cardinals = [
        (0, 1),   // South
        (-1, 0),  // West
        (0, -1),  // North
        (1, 0),   // East
    ];
    cardinals.sort_by(|&(dx1, dz1), &(dx2, dz2)| {
        let dot1 = look_x * dx1 as f64 + look_z * dz1 as f64;
        let dot2 = look_x * dx2 as f64 + look_z * dz2 as f64;
        dot2.partial_cmp(&dot1).unwrap_or(std::cmp::Ordering::Equal)
    });

    let w = bot.world();
    let world = w.read();

    for (dx, dz) in cardinals {
        let target_anvil = BlockPos::new(bx + dx, ground_y + 1, bz + dz);
        let ground = BlockPos::new(bx + dx, ground_y, bz + dz);

        let target_is_clear = world.get_block_state(target_anvil)
            .map(|s| {
                let name = format!("{s:?}").to_lowercase();
                name.contains("air")
            })
            .unwrap_or(true);

        let ground_is_solid = world.get_block_state(ground)
            .map(|s| {
                let name = format!("{s:?}").to_lowercase();
                !name.contains("air") && !name.contains("water") && !name.contains("lava")
            })
            .unwrap_or(true);

        if target_is_clear && ground_is_solid {
            drop(world);
            return (ground, target_anvil);
        }
    }
    drop(world);

    // Fallback to directly in front
    let (front_dx, front_dz) = cardinals[0];
    (
        BlockPos::new(bx + front_dx, ground_y, bz + front_dz),
        BlockPos::new(bx + front_dx, ground_y + 1, bz + front_dz),
    )
}

    /// Checks if an anvil is already present in the world nearby or at the known position.
    pub async fn check_if_anvil_placed(&mut self, bot: &Client) -> bool {
        // 1. If we already have a recorded anvil position, check if it is STILL an anvil in the world
        if let Some(pos) = self.anvil_pos {
            let is_still_anvil = {
                let w = bot.world();
                let world = w.read();
                world.get_block_state(pos)
                    .map(|s| format!("{s:?}").to_lowercase().contains("anvil"))
                    .unwrap_or(false)
            };
            if is_still_anvil {
                self.anvil_placed = true;
                return true;
            } else {
                warn!("Recorded anvil at {:?} is no longer present or broke!", pos);
                self.anvil_pos = None;
                self.anvil_placed = false;
            }
        }

        // 2. Scan nearby blocks in the world
        if let Some(pos) = Self::find_nearby_anvil(bot) {
            info!("Anvil block detected in world at {:?}! Using existing anvil.", pos);
            self.anvil_pos = Some(pos);
            self.anvil_placed = true;
            return true;
        }

        false
    }

    /// Ensures that any anvil stack present in player inventory is moved to the offhand (slot 45).
    /// Returns true if an anvil is in offhand (or was moved there).
    pub async fn ensure_anvils_in_offhand(
        bot: &Client,
        player_inventory: &mut HashMap<i16, ItemStack>,
    ) -> bool {
        // First check if slot 45 already has an anvil
        if let Some(item) = player_inventory.get(&45) {
            if let Some(info) = inspect_item_with_bot(item, Some(bot)) {
                if is_anvil(&info) && info.count > 0 {
                    return true;
                }
            }
        }

        // Find anvil in main inventory / hotbar (slots 9..=44)
        let mut found_slot = None;
        for (&slot, item) in player_inventory.iter() {
            if (9..=44).contains(&slot) {
                if let Some(info) = inspect_item_with_bot(item, Some(bot)) {
                    if is_anvil(&info) && info.count > 0 {
                        found_slot = Some(slot);
                        break;
                    }
                }
            }
        }

        let slot = match found_slot {
            Some(s) => s,
            None => return false,
        };

        let safe_hotbar = (0..9u8).find(|&h| {
            let h_slot = 36 + h as i16;
            match player_inventory.get(&h_slot) {
                None | Some(ItemStack::Empty) => true,
                Some(item) => inspect_item_with_bot(item, Some(bot)).map_or(true, |info| {
                    !is_diamond_armor(&info)
                }),
            }
        }).unwrap_or(1);

        info!("Moving Anvil from slot #{slot} to offhand (slot #45) via safe hotbar slot #{safe_hotbar}...");
        if slot != 36 + safe_hotbar as i16 {
            Self::swap_to_hotbar(bot, slot, safe_hotbar);
            let item = player_inventory.remove(&slot).unwrap_or(ItemStack::Empty);
            let old_hotbar = player_inventory.insert(36 + safe_hotbar as i16, item).unwrap_or(ItemStack::Empty);
            player_inventory.insert(slot, old_hotbar);
            bot.wait_ticks(1).await;
        }

        // Select safe hotbar slot
        bot.set_selected_hotbar_slot(safe_hotbar);
        bot.wait_ticks(1).await;

        // Send swap with offhand
        bot.write_packet(ServerboundPlayerAction {
            action: Action::SwapItemWithOffhand,
            pos: BlockPos::default(),
            direction: Direction::Down,
            seq: 0,
        });
        bot.wait_ticks(1).await;

        let anvil_item = player_inventory.remove(&(36 + safe_hotbar as i16)).unwrap_or(ItemStack::Empty);
        let old_offhand = player_inventory.insert(45, anvil_item).unwrap_or(ItemStack::Empty);
        player_inventory.insert(36 + safe_hotbar as i16, old_offhand);

        info!("Anvil moved to offhand (slot #45) successfully.");
        true
    }

    /// Place an anvil on the ground adjacent to or in front of the bot.
    /// Returns true if an anvil is placed and confirmed present in the world.
    /// The anvil is placed from the offhand (slot 45) and the remaining anvil stack is kept in offhand.
    pub async fn place_anvil(
        &mut self,
        bot: &Client,
        player_inventory: &HashMap<i16, ItemStack>,
    ) -> bool {
        if self.check_if_anvil_placed(bot).await {
            info!("Anvil block is already present at {:?}! Skipping placement.", self.anvil_pos);
            return true;
        }

        // Sync items into self.player_inventory
        for (&slot, item) in player_inventory {
            self.player_inventory.insert(slot, item.clone());
        }

        // Ensure anvil is in offhand (slot 45)
        Self::ensure_anvils_in_offhand(bot, &mut self.player_inventory).await;

        let anvil_in_offhand = self.player_inventory.get(&45)
            .and_then(|item| inspect_item_with_bot(item, Some(bot)))
            .map(|info| is_anvil(&info) && info.count > 0)
            .unwrap_or(false);

        if !anvil_in_offhand {
            warn!("No Anvil found in inventory or offhand to place!");
            return false;
        }

        // Select hotbar slot 1
        bot.set_selected_hotbar_slot(1);
        bot.wait_ticks(1).await;

        info!("Swapping Anvil from offhand (slot #45) to hotbar slot #1 to place...");
        bot.write_packet(ServerboundPlayerAction {
            action: Action::SwapItemWithOffhand,
            pos: BlockPos::default(),
            direction: Direction::Down,
            seq: 0,
        });
        bot.wait_ticks(1).await;

        if self.check_if_anvil_placed(bot).await {
            info!("Anvil block detected right before placement! Skipping placement.");
            // Swap back to offhand
            bot.write_packet(ServerboundPlayerAction {
                action: Action::SwapItemWithOffhand,
                pos: BlockPos::default(),
                direction: Direction::Down,
                seq: 0,
            });
            return true;
        }

        // Determine ground block to place on (in front of bot)
        let (ground_pos, target_anvil_pos) = Self::find_placement_pos(bot);

        // Aim smoothly at the top center of the ground block face
        let player_pos = bot.position();
        let dx = (ground_pos.x as f64 + 0.5) - player_pos.x;
        let dy = (ground_pos.y as f64 + 1.0) - (player_pos.y + 1.62);
        let dz = (ground_pos.z as f64 + 0.5) - player_pos.z;
        let horizontal_dist = (dx * dx + dz * dz).sqrt();
        let yaw = (-dx).atan2(dz).to_degrees() as f32;
        let pitch = (-dy).atan2(horizontal_dist).to_degrees() as f32;
        smooth_look(bot, yaw, pitch).await;
        bot.wait_ticks(1).await;

        info!("Placing Anvil on ground block at {:?} (aiming yaw: {yaw:.1}, pitch: {pitch:.1})...", ground_pos);
        bot.block_interact(ground_pos);
        swing_arm(bot);

        // Swap remaining anvil stack back to offhand!
        bot.wait_ticks(1).await;
        info!("Swapping remaining Anvils back to offhand (slot #45)...");
        bot.write_packet(ServerboundPlayerAction {
            action: Action::SwapItemWithOffhand,
            pos: BlockPos::default(),
            direction: Direction::Down,
            seq: 0,
        });
        bot.wait_ticks(1).await;

        // Decrement local offhand stack count
        if let Some(item) = self.player_inventory.get_mut(&45) {
            if let ItemStack::Present(data) = item {
                if data.count <= 1 {
                    *item = ItemStack::Empty;
                } else {
                    data.count -= 1;
                }
            }
        }

        self.anvil_pos = Some(target_anvil_pos);
        let mut placed_ok = false;
        for _ in 0..3 {
            bot.wait_ticks(2).await;
            if self.check_if_anvil_placed(bot).await {
                placed_ok = true;
                break;
            }
        }

        if placed_ok {
            self.anvil_placed = true;
            info!("Anvil verified placed successfully at {:?}", self.anvil_pos);
            true
        } else {
            warn!("Anvil placement interaction performed, but anvil was not detected in world!");
            self.anvil_pos = None;
            self.anvil_placed = false;
            false
        }
    }

    /// Right-click the placed anvil to open the enchanting interface.
    pub async fn open_anvil(&mut self, bot: &Client) {
        if !self.check_if_anvil_placed(bot).await {
            info!("Anvil broke or missing! Placing new anvil before opening...");
            let inv = self.player_inventory.clone();
            if !self.place_anvil(bot, &inv).await {
                warn!("Cannot open anvil: failed to place replacement anvil from inventory!");
                return;
            }
            bot.wait_ticks(2).await;
        }
        let inv = self.player_inventory.clone();
        self.open_anvil_with_inv(bot, &inv).await;
    }

    /// Right-click the placed anvil with explicit inventory reference and safe hotbar slot selection.
    pub async fn open_anvil_with_inv(&mut self, bot: &Client, player_inventory: &HashMap<i16, ItemStack>) {
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

        let valid_pos = if let Some(pos) = self.anvil_pos {
            let is_anvil = {
                let w = bot.world();
                let world = w.read();
                world.get_block_state(pos)
                    .map(|s| format!("{s:?}").to_lowercase().contains("anvil"))
                    .unwrap_or(false)
            };
            if is_anvil {
                Some(pos)
            } else {
                warn!("Anvil at {:?} is no longer in world (broke or removed)!", pos);
                Self::find_nearby_anvil(bot)
            }
        } else {
            Self::find_nearby_anvil(bot)
        };

        let target_pos = match valid_pos {
            Some(pos) => {
                self.anvil_pos = Some(pos);
                self.anvil_placed = true;
                pos
            }
            None => {
                warn!("Cannot open Anvil: no anvil block exists in world nearby!");
                self.anvil_pos = None;
                self.anvil_placed = false;
                return;
            }
        };

        // Aim smoothly at the target anvil block
        let player_pos = bot.position();
        let dx = (target_pos.x as f64 + 0.5) - player_pos.x;
        let dy = (target_pos.y as f64 + 0.5) - (player_pos.y + 1.62);
        let dz = (target_pos.z as f64 + 0.5) - player_pos.z;
        let horizontal_dist = (dx * dx + dz * dz).sqrt();
        let yaw = (-dx).atan2(dz).to_degrees() as f32;
        let pitch = (-dy).atan2(horizontal_dist).to_degrees() as f32;
        smooth_look(bot, yaw, pitch).await;
        bot.wait_ticks(1).await;

        info!("Interacting to open Anvil at {:?} with safe hotbar slot #{}...", target_pos, target_hotbar);
        bot.block_interact(target_pos);
        swing_arm(bot);
        // Release the manager lock immediately so OpenScreen/contents can be handled.
    }

    /// Handles Anvil GUI opening.
    pub fn on_open_screen(&mut self, container_id: i32, title: &str) {
        info!("Anvil Screen opened: container_id={container_id}, title='{title}'");
        self.anvil_container_id = Some(container_id);
        self.anvil_slots.clear();
        self.combine_clicks.clear();
        self.pending_click = None;
        self.recipe_wait_since = None;
        self.content_wait_since = Some(std::time::Instant::now());
        self.server_anvil_cost.store(0, Ordering::SeqCst);
    }

    /// Updates anvil slots and player inventory tracking when content packet is received.
    pub fn on_set_content(
        &mut self,
        container_id: i32,
        state_id: u32,
        items: &[ItemStack],
    ) {
        if self.anvil_container_id != Some(container_id) { return; }
        self.content_wait_since = None;
        info!("Anvil container #{container_id} content received ({} slots, state_id={state_id})", items.len());
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

    /// Determine the next combination task based on the optimal iamcal/enchant-order tree-merge sequence:
    /// 1. Book Merge: Unbreaking III Book + Mending Book -> Combined (Unbreaking III + Mending) Book (2 lvl)
    /// 2. Base Armor Merge:
    ///    - Helmet / Chestplate: Blank Armor + Protection IV Book (4 lvl)
    ///    - Leggings / Boots: Blank Armor + Blast Protection IV Book (8 lvl)
    /// 3. Final Tree Merge:
    ///    - Armor (Prot IV / Blast Prot IV) + (Unbreaking III + Mending) Book (7 lvl)
    /// 4. Fallback: If combined book is unavailable and cannot be crafted, apply available single books directly.
    pub fn find_next_combine_task(&self, bot: &Client) -> Option<CombineTask> {
        let mut armor_slots: Vec<(i16, ItemInfo)> = Vec::new();
        let mut book_slots: Vec<(i16, ItemInfo)> = Vec::new();

        // Player slots in Anvil GUI are slots 3 through 38
        for slot in 3..=38 {
            let item = self.anvil_slots.get(&slot)
                .or_else(|| self.player_inventory.get(&(slot + 6)));
            if let Some(item) = item {
                if let Some(info) = inspect_item_with_bot(item, Some(bot)) {
                    if is_diamond_armor(&info) {
                        armor_slots.push((slot, info));
                    } else if info.kind.contains("Book") || info.kind.contains("EnchantedBook") {
                        book_slots.push((slot, info));
                    }
                }
            }
        }

        let find_book = |predicate: &dyn Fn(&ItemInfo) -> bool| -> Option<(i16, ItemInfo)> {
            book_slots.iter()
                .filter(|(s, b)| *s >= 30 && predicate(b))
                .chain(book_slots.iter().filter(|(s, b)| *s < 30 && predicate(b)))
                .cloned()
                .next()
        };

        if let Some((armor_slot, armor_info)) = crate::armor::next_armor(&armor_slots) {
            let is_prot_piece = is_diamond_helmet(armor_info) || is_diamond_chestplate(armor_info);
            let is_blast_piece = is_diamond_leggings(armor_info) || is_diamond_boots(armor_info);

            let has_main_prot = if is_prot_piece {
                armor_info.has_enchantment("protection", 4)
            } else if is_blast_piece {
                armor_info.has_enchantment("blast_protection", 4)
            } else {
                false
            };

            let has_unb = armor_info.has_enchantment("unbreaking", 3);
            let has_mend = armor_info.has_enchantment("mending", 1);

            // STEP 1: If base protection is missing and a protection book is available, apply it first
            if !has_main_prot {
                let target_book = if is_prot_piece {
                    find_book(&|b| is_protection_4(b))
                        .map(|(s, _)| ("Protection IV", 4u32, s))
                } else {
                    find_book(&|b| is_blast_protection_4(b))
                        .map(|(s, _)| ("Blast Protection IV", 8u32, s))
                };

                if let Some((ench_name, req_level, b_slot)) = target_book {
                    return Some(CombineTask {
                        armor_slot: *armor_slot,
                        book_slot: b_slot,
                        armor_desc: armor_info.kind.clone(),
                        enchant_name: ench_name,
                        required_level: req_level,
                    });
                }
            }

            // STEP 2 & 3: Tree merge with (Unbreaking III + Mending)
            if !has_unb || !has_mend {
                // If armor piece needs both Unbreaking III and Mending:
                if !has_unb && !has_mend {
                    // Check if we already have a combined (Unbreaking III + Mending) book
                    if let Some((comb_slot, _)) = find_book(&|b| is_unbreaking_and_mending(b)) {
                        if has_main_prot {
                            return Some(CombineTask {
                                armor_slot: *armor_slot,
                                book_slot: comb_slot,
                                armor_desc: armor_info.kind.clone(),
                                enchant_name: "Unbreaking III & Mending",
                                required_level: 7,
                            });
                        }
                    } else {
                        // Check if we can craft the combined book (Unbreaking III + Mending -> 2 lvl)
                        let single_unb = find_book(&|b| is_single_unbreaking_3(b));
                        let single_mend = find_book(&|b| is_single_mending(b));

                        if let (Some((unb_slot, _)), Some((mend_slot, _))) = (single_unb, single_mend) {
                            if unb_slot != mend_slot {
                                return Some(CombineTask {
                                    armor_slot: unb_slot,
                                    book_slot: mend_slot,
                                    armor_desc: "Unbreaking III Book".to_string(),
                                    enchant_name: "Mending",
                                    required_level: 2,
                                });
                            }
                        }
                    }
                }

                // If combined book exists and armor has base protection:
                if let Some((comb_slot, _)) = find_book(&|b| is_unbreaking_and_mending(b)) {
                    if has_main_prot {
                        return Some(CombineTask {
                            armor_slot: *armor_slot,
                            book_slot: comb_slot,
                            armor_desc: armor_info.kind.clone(),
                            enchant_name: "Unbreaking III & Mending",
                            required_level: 7,
                        });
                    }
                }

                // STEP 4: Fallback for individual combines if combined book is not possible
                if !has_unb {
                    if let Some((unb_slot, _)) = find_book(&|b| is_unbreaking_3(b)) {
                        return Some(CombineTask {
                            armor_slot: *armor_slot,
                            book_slot: unb_slot,
                            armor_desc: armor_info.kind.clone(),
                            enchant_name: "Unbreaking III",
                            required_level: 4,
                        });
                    }
                }

                if !has_mend {
                    if let Some((mend_slot, _)) = find_book(&|b| is_mending(b)) {
                        return Some(CombineTask {
                            armor_slot: *armor_slot,
                            book_slot: mend_slot,
                            armor_desc: armor_info.kind.clone(),
                            enchant_name: "Mending",
                            required_level: 5,
                        });
                    }
                }
            }

            // Fallback for base protection if deferred
            if !has_main_prot {
                let target_book = if is_prot_piece {
                    find_book(&|b| is_protection_4(b))
                        .map(|(s, _)| ("Protection IV", 4u32, s))
                } else {
                    find_book(&|b| is_blast_protection_4(b))
                        .map(|(s, _)| ("Blast Protection IV", 8u32, s))
                };

                if let Some((ench_name, req_level, b_slot)) = target_book {
                    return Some(CombineTask {
                        armor_slot: *armor_slot,
                        book_slot: b_slot,
                        armor_desc: armor_info.kind.clone(),
                        enchant_name: ench_name,
                        required_level: req_level,
                    });
                }
            }
        }

        None
    }

    /// Check if all 4 diamond armor pieces are present and fully enchanted according to user specifications.
    pub fn check_all_armor_enchanted(&self, bot: &Client) -> (bool, String) {
        let items = self.armor_inventory_info(Some(bot));
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

    /// Counts distinct max-enchanted armor pieces currently in player inventory / anvil slots.
    pub fn count_max_enchanted_pieces(&self, bot: &Client) -> usize {
        self.armor_inventory_info(Some(bot)).iter().filter(|info| crate::armor::is_complete(info)).count()
    }

    fn armor_inventory_info(&self, bot: Option<&Client>) -> Vec<ItemInfo> {
        // An open anvil's player section is authoritative. Never merge the same
        // inventory under two different slot-number systems.
        if self.anvil_container_id.is_some() {
            (3..=38).filter_map(|slot| self.anvil_slots.get(&slot))
                .filter_map(|item| inspect_item_with_bot(item, bot)).collect()
        } else {
            (9..=44).filter_map(|slot| self.player_inventory.get(&slot))
                .filter_map(|item| inspect_item_with_bot(item, bot)).collect()
        }
    }

    /// Checks if there are any unenchanted or partially enchanted diamond armors waiting for combines.
    pub fn has_unenchanted_armor_waiting(&self, bot: &Client) -> bool {
        let check_item = |info: &ItemInfo| -> bool {
            if is_diamond_armor(info) {
                let is_prot = is_diamond_helmet(info) || is_diamond_chestplate(info);
                let is_blast = is_diamond_leggings(info) || is_diamond_boots(info);
                if is_prot {
                    return !info.has_enchantment("protection", 4)
                        || !info.has_enchantment("unbreaking", 3)
                        || !info.has_enchantment("mending", 1);
                } else if is_blast {
                    return !info.has_enchantment("blast_protection", 4)
                        || !info.has_enchantment("unbreaking", 3)
                        || !info.has_enchantment("mending", 1);
                }
            }
            false
        };
        for slot in 3..=38 {
            let item = self.anvil_slots.get(&slot)
                .or_else(|| self.player_inventory.get(&(slot + 6)));
            if let Some(item) = item {
                if let Some(info) = inspect_item_with_bot(item, Some(bot)) {
                    if check_item(&info) {
                        return true;
                    }
                }
            }
        }
        false
    }

    /// Checks if there are any applicable enchanted books in inventory to combine with armor.
    pub fn has_any_matching_books_waiting(&self, bot: &Client) -> bool {
        let mut books: Vec<ItemInfo> = Vec::new();
        for slot in 3..=38 {
            let item = self.anvil_slots.get(&slot)
                .or_else(|| self.player_inventory.get(&(slot + 6)));
            if let Some(item) = item {
                if let Some(info) = inspect_item_with_bot(item, Some(bot)) {
                    if info.kind.contains("Book") || info.kind.contains("EnchantedBook") {
                        books.push(info);
                    }
                }
            }
        }
        books.iter().any(|b| {
            is_protection_4(b) || is_blast_protection_4(b) || is_unbreaking_3(b) || is_mending(b)
        })
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
                        bot.wait_ticks(1).await;
                    }
                }
            }
        }
    }

    pub fn has_pending_anvil_work(&self) -> bool {
        self.content_wait_since.is_some() || self.pending_click.is_some()
            || !self.combine_clicks.is_empty() || self.recipe_wait_since.is_some()
            || (0..=2).any(|slot| self.anvil_slots.get(&slot)
                .is_some_and(|item| !matches!(item, ItemStack::Empty)))
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

        if let Some(since) = self.content_wait_since {
            if since.elapsed() >= std::time::Duration::from_secs(3) {
                warn!("Anvil opened without contents; reopening for a fresh snapshot.");
                self.failed_combine_attempts += 1;
                if self.failed_combine_attempts >= 5 { bot.disconnect(); }
                self.close_anvil(bot, container_id);
            }
            return false;
        }
        if let Some((slot, before, sent, inventory, sent_state_id)) = &self.pending_click {
            let slot_changed = self.anvil_slots.get(slot).is_some_and(|item| item != before);
            let inv_changed = &self.player_inventory != inventory;
            let current_state_id = self.anvil_state_id.load(Ordering::SeqCst);
            let state_advanced = current_state_id > *sent_state_id;
            let acknowledged = (slot_changed && inv_changed) || (state_advanced && (slot_changed || inv_changed));
            if acknowledged {
                let claimed_output = *slot == 2;
                self.pending_click = None;
                if claimed_output {
                    self.failed_combine_attempts = 0;
                }
            } else {
                if sent.elapsed() >= std::time::Duration::from_secs(3) {
                    warn!("Anvil click not acknowledged (slot #{slot}); closing to recover inputs.");
                    self.failed_combine_attempts += 1;
                    if self.failed_combine_attempts >= 5 {
                        warn!("Multiple anvil clicks unacknowledged (5); closing anvil to recover.");
                    }
                    self.close_anvil(bot, container_id);
                }
                return false;
            }
        }
        if let Some((slot, button, click)) = self.next_combine_click() {
            self.send_tracked_anvil_click(bot, container_id, slot, button, click);
            if self.combine_clicks.is_empty() {
                self.recipe_wait_since = Some(std::time::Instant::now());
            }
            return false;
        }
        let cur_lvl = self.current_level.load(Ordering::SeqCst);
        let server_cost = self.server_anvil_cost.load(Ordering::SeqCst);

        let has_input0 = self.anvil_slots.get(&0).map(|s| !matches!(s, ItemStack::Empty)).unwrap_or(false);
        let has_input1 = self.anvil_slots.get(&1).map(|s| !matches!(s, ItemStack::Empty)).unwrap_or(false);
        let has_output2 = self.anvil_slots.get(&2).map(|s| !matches!(s, ItemStack::Empty)).unwrap_or(false);

        // If inputs exist and server cost > current level, return inputs to inventory and throw XP
        if (has_input0 || has_input1) && server_cost > 0 && server_cost < 40 && server_cost > cur_lvl {
            info!("Anvil inputs present but server cost ({server_cost}) > current level ({cur_lvl})! Returning inputs to inventory to throw XP...");
            self.close_anvil(bot, container_id);
            self.xp_target = Some(server_cost);
            return false;
        }

        // Cost and result can arrive in either order. Never clear valid inputs
        // just because the repair-cost packet has not arrived yet.
        if has_output2 && server_cost == 0 {
            self.recipe_wait_since.get_or_insert_with(std::time::Instant::now);
        }

        // If output slot 2 has an item and level suffices, collect it via QuickMove
        if has_output2 && server_cost > 0 && server_cost < 40 && cur_lvl >= server_cost {
            info!("Output slot #2 has an item and level suffices (Level {cur_lvl} >= Cost {server_cost}); collecting via QuickMove...");
            self.send_tracked_anvil_click(bot, container_id, 2, 0, ClickType::QuickMove);
            self.recipe_wait_since = None;
            return false;
        }

        if let Some(since) = self.recipe_wait_since {
            if since.elapsed() >= std::time::Duration::from_secs(3) {
                warn!("Anvil recipe stalled; reopening to recover input items.");
                self.failed_combine_attempts += 1;
                if self.failed_combine_attempts >= 5 { bot.disconnect(); }
                self.close_anvil(bot, container_id);
            }
            return false;
        }

        // Clear any leftover inputs
        if has_input0 {
            info!("Input slot #0 has a leftover item; clearing it via QuickMove...");
            self.send_tracked_anvil_click(bot, container_id, 0, 0, ClickType::QuickMove);
            return false;
        }
        if has_input1 {
            info!("Input slot #1 has a leftover item; clearing it via QuickMove...");
            self.send_tracked_anvil_click(bot, container_id, 1, 0, ClickType::QuickMove);
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
                    warn!("Current set cannot continue: missing armor or the next required book ({summary}). Returning to orders.");
                    self.restock_needed = true;
                    self.close_anvil(bot, container_id);
                    return false;
                }
            }
        };

        let (cur_lvl, cur_prog) = self.get_level_and_progress();

        if cur_lvl < task.required_level {
            let bottles_to_throw = if self.strict_calculation {
                strict_bottles_between_levels(cur_lvl, cur_prog, task.required_level)
            } else {
                bottles_between_levels(cur_lvl, cur_prog, task.required_level)
            };
            let needed_xp = xp_difference(cur_lvl, cur_prog, task.required_level);
            info!(
                "Next combine: {} with {} requires Level {} (Current: Level {}, {:.1}% progress, Need: {} XP). Closing anvil to throw XP (est. {} bottles, mode: {})...",
                task.armor_desc, task.enchant_name, task.required_level, cur_lvl, cur_prog * 100.0, needed_xp, bottles_to_throw,
                if self.strict_calculation { "strict" } else { "standard" }
            );
            self.close_anvil(bot, container_id);
            self.xp_target = Some(task.required_level);
            return false;
        }

        info!(
            "Executing Anvil Combine: {} (slot #{}) + {} Book (slot #{}) [Level Cost: {} <= Current Level: {}]",
            task.armor_desc, task.armor_slot, task.enchant_name, task.book_slot, task.required_level, cur_lvl
        );
        self.combine_in_anvil(bot, container_id, task.armor_slot, task.book_slot).await;
        false
    }

    /// Determine which hotbar buttons (0..=8) to use for placing armor and book into Anvil input slots.
    /// In a 39-slot Anvil container, slots 30..=38 correspond to hotbar slots 0..=8.
    pub fn determine_hotbar_buttons_for_combine(item_slot: i16, book_slot: i16) -> (u8, u8) {
        match (item_slot, book_slot) {
            (30..=38, 30..=38) => {
                let a = (item_slot - 30) as u8;
                let b = (book_slot - 30) as u8;
                if a == b {
                    (a, if a == 0 { 1 } else { 0 })
                } else {
                    (a, b)
                }
            }
            (30..=38, _) => {
                let a_btn = (item_slot - 30) as u8;
                let b_btn = if a_btn == 0 { 1 } else { 0 };
                (a_btn, b_btn)
            }
            (_, 30..=38) => {
                let b_btn = (book_slot - 30) as u8;
                let a_btn = if b_btn == 0 { 1 } else { 0 };
                (a_btn, b_btn)
            }
            _ => (0u8, 1u8),
        }
    }

    /// Combine an item and an enchanted book inside the anvil container using hotbar Swap into input slots.
    pub async fn combine_in_anvil(
        &mut self,
        bot: &Client,
        container_id: i32,
        item_inventory_slot: i16,
        book_inventory_slot: i16,
    ) {
        self.server_anvil_cost.store(0, Ordering::SeqCst);
        self.queue_anvil_combination(item_inventory_slot, book_inventory_slot);
        if let Some((slot, button, click)) = self.next_combine_click() {
            self.send_tracked_anvil_click(bot, container_id, slot, button, click);
        }
    }

    pub fn item_at_anvil_slot(&self, slot: i16) -> Option<ItemStack> {
        self.anvil_slots.get(&slot).cloned()
            .or_else(|| {
                if (3..=38).contains(&slot) {
                    self.player_inventory.get(&(slot - 3 + 9)).cloned()
                } else {
                    None
                }
            })
    }

    pub fn anvil_slots_match(&self, hotbar_slot: i16, inv_slot: i16) -> bool {
        let Some(item_a) = self.item_at_anvil_slot(hotbar_slot) else { return false; };
        let Some(item_b) = self.item_at_anvil_slot(inv_slot) else { return false; };
        if matches!(item_a, ItemStack::Empty) || matches!(item_b, ItemStack::Empty) {
            return false;
        }
        if item_a == item_b {
            return true;
        }
        if let (Some(info_a), Some(info_b)) = (
            inspect_item_with_bot(&item_a, None),
            inspect_item_with_bot(&item_b, None),
        ) {
            if is_unbreaking_and_mending(&info_a) && is_unbreaking_and_mending(&info_b) {
                return true;
            }
            if is_single_mending(&info_a) && is_single_mending(&info_b) {
                return true;
            }
            if is_single_unbreaking_3(&info_a) && is_single_unbreaking_3(&info_b) {
                return true;
            }
            if is_protection_4(&info_a) && is_protection_4(&info_b) {
                return true;
            }
            if is_blast_protection_4(&info_a) && is_blast_protection_4(&info_b) {
                return true;
            }
            if is_diamond_helmet(&info_a) && is_diamond_helmet(&info_b) {
                return true;
            }
            if is_diamond_chestplate(&info_a) && is_diamond_chestplate(&info_b) {
                return true;
            }
            if is_diamond_leggings(&info_a) && is_diamond_leggings(&info_b) {
                return true;
            }
            if is_diamond_boots(&info_a) && is_diamond_boots(&info_b) {
                return true;
            }
        }
        false
    }

    fn queue_anvil_combination(&mut self, item_inventory_slot: i16, book_inventory_slot: i16) {
        let (armor_button, book_button) =
            Self::determine_hotbar_buttons_for_combine(item_inventory_slot, book_inventory_slot);

        let armor_already_staged = item_inventory_slot < 30
            && self.anvil_slots_match(30 + armor_button as i16, item_inventory_slot);

        if item_inventory_slot < 30 && !armor_already_staged {
            self.combine_clicks.push_back((item_inventory_slot, armor_button, ClickType::Swap));
        }

        let book_already_staged = book_inventory_slot < 30
            && self.anvil_slots_match(30 + book_button as i16, book_inventory_slot);

        if book_inventory_slot < 30 && !book_already_staged {
            self.combine_clicks.push_back((book_inventory_slot, book_button, ClickType::Swap));
        }

        self.combine_clicks.push_back((0, armor_button, ClickType::Swap));
        self.combine_clicks.push_back((1, book_button, ClickType::Swap));
    }

    fn next_combine_click(&mut self) -> Option<(i16, u8, ClickType)> {
        self.combine_clicks.pop_front()
    }

    fn send_tracked_anvil_click(&mut self, bot: &Client, container_id: i32, slot: i16, button: u8, click: ClickType) {
        let before = self.anvil_slots.get(&slot).cloned().unwrap_or(ItemStack::Empty);
        let sent_state_id = self.anvil_state_id.load(Ordering::SeqCst);
        self.click_anvil(bot, container_id, slot, button, click);
        self.pending_click = Some((slot, before, std::time::Instant::now(), self.player_inventory.clone(), sent_state_id));
    }

    /// Send a click packet to the open anvil container with the latest server state_id.
    pub fn click_anvil(&self, bot: &Client, container_id: i32, slot: i16, button_num: u8, click_type: ClickType) {
        let state_id = self.anvil_state_id.load(Ordering::SeqCst);
        let packet = ServerboundContainerClick {
            container_id,
            state_id,
            slot_num: slot,
            button_num,
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
        self.anvil_slots.clear();
        self.combine_clicks.clear();
        self.pending_click = None;
        self.recipe_wait_since = None;
        self.content_wait_since = None;
        self.server_anvil_cost.store(0, Ordering::SeqCst);
    }

    pub fn swap_to_hotbar(bot: &Client, inv_slot: i16, hotbar_idx: u8) {
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
    fn identical_mending_book_in_hotbar_skips_staging_swap() {
        use azalea_registry::builtin::ItemKind;
        let mut manager = EnchanterManager::new();
        let book = ItemStack::new(ItemKind::EnchantedBook, 1);
        manager.anvil_slots.insert(20, book.clone());
        manager.anvil_slots.insert(31, book);
        manager.anvil_slots.insert(30, ItemStack::new(ItemKind::DiamondHelmet, 1));
        manager.queue_anvil_combination(30, 20);
        // Logged failure: staging #20 into hotbar #31 swaps identical books.
        // Use the book already in #31 and proceed straight to the anvil inputs.
        assert_eq!(manager.next_combine_click(), Some((0, 0, ClickType::Swap)));
        assert_eq!(manager.next_combine_click(), Some((1, 1, ClickType::Swap)));
        assert!(manager.next_combine_click().is_none());
    }

    #[test]
    fn armor_already_in_hotbar_skips_staging_swap() {
        use azalea_registry::builtin::ItemKind;
        let mut manager = EnchanterManager::new();
        let helmet = ItemStack::new(ItemKind::DiamondHelmet, 1);
        let book = ItemStack::new(ItemKind::EnchantedBook, 1);
        manager.anvil_slots.insert(15, helmet.clone());
        manager.anvil_slots.insert(30, helmet);
        manager.anvil_slots.insert(31, book);
        manager.queue_anvil_combination(15, 31);
        // Helmet is already in hotbar slot #30 (button 0), book is in #31 (button 1)
        assert_eq!(manager.next_combine_click(), Some((0, 0, ClickType::Swap)));
        assert_eq!(manager.next_combine_click(), Some((1, 1, ClickType::Swap)));
        assert!(manager.next_combine_click().is_none());
    }

    #[test]
    fn open_anvil_inventory_overrides_stale_player_cache() {
        use azalea_registry::builtin::ItemKind;
        let mut manager = EnchanterManager::new();
        manager.player_inventory.insert(9, ItemStack::new(ItemKind::DiamondHelmet, 1));
        assert_eq!(manager.armor_inventory_info(None).len(), 1);
        manager.anvil_container_id = Some(7);
        manager.anvil_slots.insert(3, ItemStack::Empty);
        assert!(manager.armor_inventory_info(None).is_empty());
        manager.anvil_slots.insert(4, ItemStack::new(ItemKind::DiamondChestplate, 1));
        let items = manager.armor_inventory_info(None);
        assert_eq!(items.len(), 1);
        assert!(is_diamond_chestplate(&items[0]));
    }

    #[test]
    fn opening_anvil_waits_for_matching_contents_and_rejects_late_snapshots() {
        let mut manager = EnchanterManager::new();
        manager.on_open_screen(7, "Anvil");
        assert!(manager.has_pending_anvil_work());
        manager.on_set_content(6, 1, &vec![ItemStack::Empty; 39]);
        assert!(manager.has_pending_anvil_work());
        assert_eq!(manager.anvil_container_id, Some(7));
        manager.on_set_content(7, 1, &vec![ItemStack::Empty; 39]);
        assert!(!manager.has_pending_anvil_work());
    }

    #[test]
    fn pending_anvil_click_prevents_early_completion() {
        let mut manager = EnchanterManager::new();
        manager.pending_click = Some((2, ItemStack::Empty, std::time::Instant::now(), HashMap::new(), 0));
        assert!(manager.has_pending_anvil_work());
        manager.reset_for_next_batch();
        assert!(!manager.has_pending_anvil_work());
    }

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
        // Level 3 total: 27 XP, next level needs 13 XP. 50% floor = 6 XP. Current XP = 33.
        // Need to reach Level 4 (40 XP) -> deficit 7 XP -> 2 bottles (ceil(7 / 6.0))!
        assert_eq!(xp_difference(3, 0.5, 4), 7);
        assert_eq!(bottles_between_levels(3, 0.5, 4), 2);

        // Level 3 with 96% progress to Level 4 (the exact log bug scenario):
        // Must never produce 0 deficit or 0 bottles when from_level < target_level!
        assert!(xp_difference(3, 0.96, 4) >= 1);
        assert!(bottles_between_levels(3, 0.96, 4) >= 1);
    }

    #[test]
    fn test_enchanter_manager_creation() {
        let manager = EnchanterManager::new();
        assert!(!manager.anvil_placed);
        assert!(!manager.enchanting_complete);
    }

    #[test]
    fn test_tree_merge_combine_task_sequence() {
        use azalea_registry::builtin::ItemKind;

        let mut manager = EnchanterManager::new();

        // Populate anvil slots:
        // Slot 3: Blank Diamond Helmet
        // Slot 4: Protection IV Book
        // Slot 5: Unbreaking III Book
        // Slot 6: Mending Book
        manager.anvil_slots.insert(3, ItemStack::new(ItemKind::DiamondHelmet, 1));
        manager.anvil_slots.insert(4, ItemStack::new(ItemKind::EnchantedBook, 1));
        manager.anvil_slots.insert(5, ItemStack::new(ItemKind::EnchantedBook, 1));
        manager.anvil_slots.insert(6, ItemStack::new(ItemKind::EnchantedBook, 1));

        // Mock client is not available in unit test, so we can test helper methods or check ItemInfo logic:
        let helm_info = ItemInfo { kind: "DiamondHelmet".to_string(), count: 1, ..Default::default() };
        assert!(crate::nbt::is_diamond_helmet(&helm_info));
        let mut prot_book = ItemInfo { kind: "EnchantedBook".to_string(), count: 1, ..Default::default() };
        prot_book.stored_enchantments.insert("protection".to_string(), 4);

        let mut unb_book = ItemInfo { kind: "EnchantedBook".to_string(), count: 1, ..Default::default() };
        unb_book.stored_enchantments.insert("unbreaking".to_string(), 3);

        let mut mend_book = ItemInfo { kind: "EnchantedBook".to_string(), count: 1, ..Default::default() };
        mend_book.stored_enchantments.insert("mending".to_string(), 1);

        assert!(crate::nbt::is_protection_4(&prot_book));
        assert!(crate::nbt::is_single_unbreaking_3(&unb_book));
        assert!(crate::nbt::is_single_mending(&mend_book));
        assert!(!crate::nbt::is_unbreaking_and_mending(&unb_book));
        assert!(!crate::nbt::is_unbreaking_and_mending(&mend_book));

        let mut combined_book = ItemInfo { kind: "EnchantedBook".to_string(), count: 1, ..Default::default() };
        combined_book.stored_enchantments.insert("unbreaking".to_string(), 3);
        combined_book.stored_enchantments.insert("mending".to_string(), 1);

        assert!(crate::nbt::is_unbreaking_and_mending(&combined_book));
        assert!(!crate::nbt::is_single_unbreaking_3(&combined_book));
        assert!(!crate::nbt::is_single_mending(&combined_book));
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

    #[test]
    fn test_determine_hotbar_buttons_for_combine() {
        // Both in main inventory (< 30): use hotbar button 0 and 1
        assert_eq!(EnchanterManager::determine_hotbar_buttons_for_combine(3, 24), (0, 1));
        assert_eq!(EnchanterManager::determine_hotbar_buttons_for_combine(5, 12), (0, 1));

        // Armor in hotbar (slot 30 = hotbar button 0), book in main inventory
        assert_eq!(EnchanterManager::determine_hotbar_buttons_for_combine(30, 15), (0, 1));
        // Armor in hotbar (slot 31 = hotbar button 1), book in main inventory
        assert_eq!(EnchanterManager::determine_hotbar_buttons_for_combine(31, 15), (1, 0));

        // Book in hotbar (slot 30 = hotbar button 0), armor in main inventory
        assert_eq!(EnchanterManager::determine_hotbar_buttons_for_combine(10, 30), (1, 0));
        // Book in hotbar (slot 35 = hotbar button 5), armor in main inventory
        assert_eq!(EnchanterManager::determine_hotbar_buttons_for_combine(10, 35), (0, 5));

        // Both in hotbar: preserve their respective hotbar buttons
        assert_eq!(EnchanterManager::determine_hotbar_buttons_for_combine(30, 31), (0, 1));
        assert_eq!(EnchanterManager::determine_hotbar_buttons_for_combine(38, 32), (8, 2));
    }

    #[test]
    fn test_click_type_throw() {
        let _ = ClickType::Throw;
        let _ = azalea::protocol::packets::game::s_player_action::ServerboundPlayerAction {
            action: azalea::protocol::packets::game::s_player_action::Action::SwapItemWithOffhand,
            pos: BlockPos::default(),
            direction: azalea::core::direction::Direction::Down,
            seq: 0,
        };
    }

    #[test]
    fn test_find_lowest_count_xp_slot_prioritizes_smallest_amount() {
        use azalea_registry::builtin::ItemKind;

        let mut inv = HashMap::new();
        // Slot 10 has 56 XP bottles, slot 15 has 7 XP bottles, slot 36 has 64 XP bottles
        inv.insert(10, ItemStack::new(ItemKind::ExperienceBottle, 56));
        inv.insert(15, ItemStack::new(ItemKind::ExperienceBottle, 7));
        inv.insert(36, ItemStack::new(ItemKind::ExperienceBottle, 64));

        // It should pick slot 15 (7 bottles) first!
        let chosen = EnchanterManager::find_lowest_count_xp_slot(&inv, None);
        assert_eq!(chosen, Some((15, 7)));

        // Once slot 15 is consumed (removed/empty), it should pick slot 10 (56 bottles) next
        inv.remove(&15);
        let chosen = EnchanterManager::find_lowest_count_xp_slot(&inv, None);
        assert_eq!(chosen, Some((10, 56)));

        // Once slot 10 is consumed, it uses the 64-stack in slot 36
        inv.remove(&10);
        let chosen = EnchanterManager::find_lowest_count_xp_slot(&inv, None);
        assert_eq!(chosen, Some((36, 64)));

        // Empty inventory returns None
        inv.clear();
        assert_eq!(EnchanterManager::find_lowest_count_xp_slot(&inv, None), None);
    }

    #[test]
    fn test_find_lowest_count_xp_slot_tie_breaking_and_filtering() {
        use azalea_registry::builtin::ItemKind;

        let mut inv = HashMap::new();
        // Non-XP items and empty slots should be ignored
        inv.insert(9, ItemStack::new(ItemKind::DiamondHelmet, 1));
        inv.insert(11, ItemStack::new(ItemKind::EnchantedBook, 1));
        inv.insert(12, ItemStack::Empty);

        // Slot 20 in main inv and slot 36 in hotbar 0 both have 8 bottles
        inv.insert(20, ItemStack::new(ItemKind::ExperienceBottle, 8));
        inv.insert(36, ItemStack::new(ItemKind::ExperienceBottle, 8));

        // Hotbar slot 36 is preferred on ties to avoid swapping
        let chosen = EnchanterManager::find_lowest_count_xp_slot(&inv, None);
        assert_eq!(chosen, Some((36, 8)));

        // If a slot has fewer bottles (e.g. 2 bottles in slot 25), it beats slot 36 (8 bottles)
        inv.insert(25, ItemStack::new(ItemKind::ExperienceBottle, 2));
        let chosen = EnchanterManager::find_lowest_count_xp_slot(&inv, None);
        assert_eq!(chosen, Some((25, 2)));
    }

    #[test]
    fn test_safe_batch_size_never_overshoots() {
        // Zero or small deficits must return 0 (single-bottle precision mode)
        assert_eq!(safe_batch_size(0), 0);
        assert_eq!(safe_batch_size(1), 0);
        assert_eq!(safe_batch_size(5), 0);
        assert_eq!(safe_batch_size(11), 0);

        // For any deficit >= 12, safe_batch * 11 must be strictly less than needed_xp
        for needed in 12..=500 {
            let batch = safe_batch_size(needed);
            assert!(batch > 0);
            let max_possible_xp_from_batch = batch * 11;
            assert!(
                max_possible_xp_from_batch < needed,
                "Batch {batch} (max XP: {max_possible_xp_from_batch}) overshot needed {needed}"
            );
        }

        // Specific typical level deficits:
        // Level 0 -> 4: 40 XP deficit -> safe batch 3 bottles (max 33 XP < 40)
        assert_eq!(safe_batch_size(40), 3);
        // Level 0 -> 5: 55 XP deficit -> safe batch 4 bottles (max 44 XP < 55)
        assert_eq!(safe_batch_size(55), 4);
        // Level 0 -> 8: 112 XP deficit -> safe batch 10 bottles (max 110 XP < 112)
        assert_eq!(safe_batch_size(112), 10);
    }

    #[test]
    fn test_strict_safe_batch_size_properties() {
        // Any deficit <= 22 must strictly return 0 (single-bottle precision)
        for needed in 0..=22 {
            assert_eq!(
                strict_safe_batch_size(needed),
                0,
                "strict_safe_batch_size must be 0 for deficit {needed}"
            );
        }

        // For any deficit > 22 up to 1000:
        // 1. Batch * 11 must be <= needed - 11 (guaranteeing >= 11 XP deficit remaining after batch)
        // 2. Batch must never exceed 16 bottles
        for needed in 23..=1000 {
            let batch = strict_safe_batch_size(needed);
            assert!(batch > 0);
            assert!(batch <= 16, "Batch {batch} exceeded max cap of 16 for needed {needed}");
            let max_possible_xp = batch * 11;
            assert!(
                max_possible_xp + 11 <= needed,
                "Strict batch {batch} (max XP {max_possible_xp}) did not leave at least 11 XP deficit for needed {needed}"
            );
        }

        // Typical milestones:
        // 40 XP deficit: (40 - 11) / 11 = 2 bottles (max 22 XP <= 29 XP, leaves >= 18 XP deficit)
        assert_eq!(strict_safe_batch_size(40), 2);
        // 55 XP deficit: (55 - 11) / 11 = 4 bottles (max 44 XP <= 44 XP, leaves >= 11 XP deficit)
        assert_eq!(strict_safe_batch_size(55), 4);
        // 112 XP deficit: (112 - 11) / 11 = 9 bottles (max 99 XP <= 101 XP, leaves >= 13 XP deficit)
        assert_eq!(strict_safe_batch_size(112), 9);
        // 500 XP deficit: capped at 16 bottles
        assert_eq!(strict_safe_batch_size(500), 16);
    }

    #[test]
    fn test_strict_bottles_between_levels() {
        assert_eq!(strict_bottles_between_levels(0, 0.0, 1), 1);
        assert_eq!(strict_bottles_between_levels(0, 0.0, 4), 3); // 40 / 11 = 3
        assert_eq!(strict_bottles_between_levels(0, 0.0, 5), 5); // 55 / 11 = 5
        assert_eq!(strict_bottles_between_levels(4, 0.0, 4), 0);
    }

    #[test]
    fn test_parse_throw_speed() {
        // Plain numbers (ticks)
        assert_eq!(parse_throw_speed("0"), 0);
        assert_eq!(parse_throw_speed("1"), 1);
        assert_eq!(parse_throw_speed("2"), 2);
        assert_eq!(parse_throw_speed("3"), 3);

        // Numbers >= 20 treated as ms:
        // 20ms -> 1 tick, 50ms -> 1 tick, 100ms -> 2 ticks, 150ms -> 3 ticks
        assert_eq!(parse_throw_speed("20"), 1);
        assert_eq!(parse_throw_speed("50"), 1);
        assert_eq!(parse_throw_speed("100"), 2);
        assert_eq!(parse_throw_speed("150"), 3);

        // Explicit suffixes
        assert_eq!(parse_throw_speed("50ms"), 1);
        assert_eq!(parse_throw_speed("100ms"), 2);
        assert_eq!(parse_throw_speed("1t"), 1);
        assert_eq!(parse_throw_speed("2t"), 2);

        // Presets
        assert_eq!(parse_throw_speed("instant"), 0);
        assert_eq!(parse_throw_speed("burst"), 0);
        assert_eq!(parse_throw_speed("normal"), 1);
        assert_eq!(parse_throw_speed("fast"), 1);
        assert_eq!(parse_throw_speed("slow"), 2);
        assert_eq!(parse_throw_speed("safe"), 2);

        // Whitespace and case insensitivity
        assert_eq!(parse_throw_speed("  2  "), 2);
        assert_eq!(parse_throw_speed("  SLOW  "), 2);
        assert_eq!(parse_throw_speed("100MS"), 2);

        // Invalid defaults to 1
        assert_eq!(parse_throw_speed("invalid_xyz"), 1);
    }
}
