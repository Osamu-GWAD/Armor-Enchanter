pub mod stock;
pub mod armor;
pub mod auth;
pub mod enchanter;
pub mod gui;
pub mod nbt;
pub mod webhook;
mod logging;
mod tick_diagnostics;
mod drops;

use azalea::app::PluginGroup;
use azalea::prelude::*;
use azalea::protocol::packets::game::{ClientboundGamePacket, ServerboundContainerClose};
use azalea::{Client, Event};
use clap::Parser;
use enchanter::{parse_throw_speed, smooth_look, EnchanterManager};
use gui::{GuiManager, OrderWorkflowState, WithdrawalPhase, WithdrawalQuota};
use nbt::{inspect_item_with_bot, is_anvil};
use std::sync::{Arc, OnceLock};
use tokio::sync::{Mutex, Notify};
use tracing::{error, info, warn};

static GLOBAL_QUOTA: OnceLock<WithdrawalQuota> = OnceLock::new();
static GLOBAL_ORDER_TARGET: OnceLock<String> = OnceLock::new();
static GLOBAL_XP_THROW_SPEED: OnceLock<u32> = OnceLock::new();
static GLOBAL_XP_STRICT_CALCULATION: OnceLock<bool> = OnceLock::new();
static GLOBAL_VIEW_DISTANCE: OnceLock<u8> = OnceLock::new();

#[derive(Parser, Debug)]
#[command(author, version, about = "Minecraft Auto-Enchanter Bot with Token Support & GUI Automation", long_about = None)]
struct Args {
    /// Server address to connect to (e.g., donutsmp.net)
    #[arg(short, long, default_value = "donutsmp.net")]
    server: String,

    /// Server port
    #[arg(short, long, default_value_t = 25565)]
    port: u16,

    /// Requested chunk view distance; nearby enchanting only needs 2 chunks
    #[arg(long, env = "VIEW_DISTANCE", default_value_t = 2, value_parser = clap::value_parser!(u8).range(2..=32))]
    view_distance: u8,

    /// Minecraft JWT or Bearer access token for online-mode authentication
    #[arg(short, long, env = "MC_TOKEN", hide_env_values = true)]
    token: Option<String>,

    /// Username for offline-mode authentication (if no token is provided)
    #[arg(long)]
    offline: Option<String>,

    /// Microsoft email for interactive browser authentication
    #[arg(long)]
    microsoft: Option<String>,

    /// Account index or identifier to select when multiple accounts are configured in .env (e.g. 0, 1, email, or "all")
    #[arg(short, long, env = "ACCOUNT")]
    account: Option<String>,

    /// Run all configured accounts concurrently
    #[arg(long)]
    all: bool,

    /// Number of Mending books to withdraw from /order
    #[arg(long, default_value_t = 8)]
    mending: u32,

    /// Number of Unbreaking III books to withdraw from /order
    #[arg(long, default_value_t = 8)]
    unb3: u32,

    /// Number of Protection IV books to withdraw from /order
    #[arg(long, default_value_t = 4)]
    prot4: u32,

    /// Number of Blast Protection IV books to withdraw from /order
    #[arg(long, default_value_t = 4)]
    blast_prot4: u32,

    /// Number of XP bottle stacks (up to 64 each) to withdraw
    #[arg(long, default_value_t = 4)]
    xp_stacks: u32,

    /// Number of anvils to withdraw
    #[arg(long, default_value_t = 2)]
    anvils: u32,

    /// Number of Diamond Helmets to withdraw
    #[arg(long, default_value_t = 2)]
    diamond_helmets: u32,

    /// Number of Diamond Chestplates to withdraw
    #[arg(long, default_value_t = 2)]
    diamond_chestplates: u32,

    /// Number of Diamond Leggings to withdraw
    #[arg(long, default_value_t = 2)]
    diamond_leggings: u32,

    /// Number of Diamond Boots to withdraw
    #[arg(long, default_value_t = 2)]
    diamond_boots: u32,

    /// Target player username whose buy orders will be fulfilled (e.g. zn6h)
    #[arg(long, env = "ORDER_TARGET_NAME", default_value = "zn6h")]
    order_target: String,

    /// XP bottle throwing delay in ticks (speed: 1 = normal 1 tick/50ms, 2 = 2 ticks/100ms slower/safer, 0 = burst)
    #[arg(long, env = "XP_THROW_SPEED", default_value = "1")]
    xp_throw_speed: String,

    /// Whether to use strict zero-overshoot XP calculation and single-bottle precision
    #[arg(long, env = "XP_STRICT_CALCULATION", default_value_t = true, action = clap::ArgAction::Set)]
    xp_strict_calculation: bool,
}

#[derive(Clone, Component)]
struct BotState {
    gui: Arc<Mutex<GuiManager>>,
    enchanter: Arc<Mutex<EnchanterManager>>,
    spawned: Arc<Mutex<bool>>,
    order_sent: Arc<Mutex<bool>>,
    inventory_updated: Arc<Notify>,
    experience_updated: Arc<Notify>,
    current_level: Arc<std::sync::atomic::AtomicU32>,
    experience_progress_milli: Arc<std::sync::atomic::AtomicU32>,
    total_experience: Arc<std::sync::atomic::AtomicU32>,
    server_anvil_cost: Arc<std::sync::atomic::AtomicU32>,
    maintenance_active: Arc<std::sync::atomic::AtomicBool>,
    is_enchanting: Arc<std::sync::atomic::AtomicBool>,
}

// Each account runs in its own process. Preserve transactions across reconnects.
static SESSION_GUI: OnceLock<Arc<Mutex<GuiManager>>> = OnceLock::new();

impl Default for BotState {
    fn default() -> Self {
        let mut gui = GuiManager::new();
        if let Some(quota) = GLOBAL_QUOTA.get() {
            gui.quota = quota.clone();
        }
        if let Some(target) = GLOBAL_ORDER_TARGET.get() {
            gui.order_target = target.clone();
        }
        let current_level = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let experience_progress_milli = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let total_experience = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let server_anvil_cost = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let experience_updated = Arc::new(Notify::new());
        let mut enchanter = EnchanterManager::new();
        if let Some(&speed) = GLOBAL_XP_THROW_SPEED.get() {
            enchanter.throw_speed_ticks = speed;
        }
        if let Some(&strict) = GLOBAL_XP_STRICT_CALCULATION.get() {
            enchanter.strict_calculation = strict;
        }
        enchanter.current_level = current_level.clone();
        enchanter.experience_progress_milli = experience_progress_milli.clone();
        enchanter.total_experience = total_experience.clone();
        enchanter.server_anvil_cost = server_anvil_cost.clone();
        enchanter.experience_updated = experience_updated.clone();

        Self {
            gui: SESSION_GUI.get_or_init(|| Arc::new(Mutex::new(gui))).clone(),
            enchanter: Arc::new(Mutex::new(enchanter)),
            spawned: Arc::new(Mutex::new(false)),
            order_sent: Arc::new(Mutex::new(false)),
            inventory_updated: Arc::new(Notify::new()),
            experience_updated,
            current_level,
            experience_progress_milli,
            total_experience,
            server_anvil_cost,
            maintenance_active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            is_enchanting: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }
}

async fn handle(bot: Client, event: Event, state: BotState) -> Result<(), anyhow::Error> {
    match event {
        Event::Init => {
            bot.set_client_information(azalea::ClientInformation {
                view_distance: *GLOBAL_VIEW_DISTANCE.get().unwrap_or(&2),
                ..Default::default()
            });
            info!("Bot initialized, connecting to server...");
        }
        Event::Login => {
            info!("Login successful! Awaiting world spawn...");
        }
        Event::Spawn => {
            let mut spawned = state.spawned.lock().await;
            if !*spawned {
                *spawned = true;
                info!("Bot spawned in the world!");

                let bot_clone = bot.clone();
                let state_clone = state.clone();

                // Packet updates wake GUI actions immediately; this is only a fallback.
                let bot_wd = bot.clone();
                let state_wd = state.clone();
                tokio::spawn(async move {
                    loop {
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                        let is_spawned = *state_wd.spawned.lock().await;
                        if !is_spawned {
                            break;
                        }
                        let is_enchanting = state_wd.is_enchanting.load(std::sync::atomic::Ordering::SeqCst);
                        if is_enchanting {
                            continue;
                        }
                        let mut gui = state_wd.gui.lock().await;
                        gui.check_and_handle_timeout(&bot_wd).await;
                        drop(gui);
                        pump_gui_actions(bot_wd.clone(), state_wd.clone(), None).await;
                    }
                });
                tokio::spawn(async move {
                    // Fast spawn warmup before sending commands
                    info!("Waiting 30 ticks (~1.5s) for server spawn cooldown before sending commands...");
                    bot_clone.wait_ticks(30).await;

                    // Teleport to player base using /home 1 so block interactions (anvil) are in non-protected territory
                    let initial_pos = bot_clone.position();
                    info!("Initial spawn position: {:?}", initial_pos);

                    let mut at_base = {
                        let mut ench = state_clone.enchanter.lock().await;
                        ench.check_if_anvil_placed(&bot_clone).await || EnchanterManager::find_nearby_hopper(&bot_clone).is_some()
                    };

                    if !at_base {
                        for attempt in 1..=15 {
                            info!("Teleporting to base via /home 1 (attempt {attempt}/15)...");
                            state_clone.maintenance_active.store(false, std::sync::atomic::Ordering::SeqCst);
                            gui::send_command(&bot_clone, "/home 1");
                            bot_clone.wait_ticks(30).await; // 1.5s for teleport settle

                            let current_pos = bot_clone.position();
                            let dist = (current_pos - initial_pos).length();
                            let base_blocks = {
                                let mut ench = state_clone.enchanter.lock().await;
                                ench.check_if_anvil_placed(&bot_clone).await || EnchanterManager::find_nearby_hopper(&bot_clone).is_some()
                            };

                            if dist > 5.0 || base_blocks {
                                info!("Arrived at base! Position after /home 1: {:?} (distance from spawn: {:.1})", current_pos, dist);
                                at_base = true;
                                break;
                            }

                            if state_clone.maintenance_active.load(std::sync::atomic::Ordering::SeqCst) {
                                warn!("Destination area in maintenance; waiting 10 seconds before retrying /home 1...");
                                tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                            } else {
                                if attempt >= 3 && attempt % 2 == 1 {
                                    info!("Trying fallback '/home' command (attempt {attempt}/15)...");
                                    gui::send_command(&bot_clone, "/home");
                                    bot_clone.wait_ticks(30).await;
                                    let pos2 = bot_clone.position();
                                    if (pos2 - initial_pos).length() > 5.0 {
                                        info!("Arrived at base via /home! Position: {:?}", pos2);
                                        at_base = true;
                                        break;
                                    }
                                }
                                bot_clone.wait_ticks(40).await;
                            }
                        }
                    }

                    if !at_base {
                        error!("Could not teleport to base (/home 1 failed or destination in maintenance). Current position: {:?}.", bot_clone.position());
                        error!("Aborting startup to prevent withdrawing items in an unsafe/protected location. Disconnecting...");
                        bot_clone.disconnect();
                        return;
                    }

                    let current_pos = bot_clone.position();
                    info!("Verified at base position: {:?}", current_pos);

                    let resume_drop = {
                        let gui = state_clone.gui.lock().await;
                        gui.next_drop > 0 || gui.pending_drop.is_some()
                            || gui.count_completed_max_sets(Some(&bot_clone)) > 0
                    };
                    if resume_drop {
                        info!("Attempting to resume dropping completed enchanted armor into hopper...");
                        if !drop_enchanted_armor_in_hopper(&bot_clone, &state_clone).await {
                            warn!("Could not complete armor drop on startup. Resetting pending drop state to allow workflow to continue...");
                            let mut gui = state_clone.gui.lock().await;
                            gui.next_drop = 0;
                            gui.pending_drop = None;
                        }
                    }

                    // Reset and sync inventory state (no clean_inventory)
                    {
                        let mut gui = state_clone.gui.lock().await;
                        gui.reset_and_sync_inventory(Some(&bot_clone));
                    }

                    {
                        let mut gui = state_clone.gui.lock().await;
                        gui.reset_and_sync_inventory(Some(&bot_clone));
                    }

                    // Check if an anvil is ALREADY placed at base
                    let anvil_placed = {
                        let mut ench = state_clone.enchanter.lock().await;
                        ench.check_if_anvil_placed(&bot_clone).await
                    };

                    // Ensure any anvils in inventory are moved to offhand (slot 45)
                    {
                        let mut gui = state_clone.gui.lock().await;
                        crate::enchanter::EnchanterManager::ensure_anvils_in_offhand(&bot_clone, &mut gui.player_inventory).await;
                    }

                    let anvil_count = {
                        let gui = state_clone.gui.lock().await;
                        gui.player_inventory.values().filter_map(|item| {
                            inspect_item_with_bot(item, Some(&bot_clone))
                                .filter(|info| is_anvil(info))
                                .map(|info| info.count as u32)
                        }).sum::<u32>()
                    };

                    info!("Initial base check: placed anvil = {anvil_placed}, inventory anvil count = {anvil_count}");

                    if anvil_count < 2 {
                        info!("Bot has {anvil_count} anvil(s) (< 2, or anvil == 1). Starting Phase 1: Anvil Placement (looking down at feet & withdrawing Anvil from /order)...");
                        let mut gui = state_clone.gui.lock().await;
                        gui.phase = WithdrawalPhase::AnvilPlacement;
                        gui.anvil_stack_dropped = false;
                    } else if anvil_placed {
                        info!("Anvil is already placed on the ground and bot has {anvil_count} anvils! Proceeding to Phase 2: Items Retrieval.");
                        let mut gui = state_clone.gui.lock().await;
                        gui.phase = WithdrawalPhase::ItemsRetrieval;
                    } else {
                        info!("Anvil found in inventory ({anvil_count} anvils)! Placing it now...");
                        let player_inv = {
                            let gui = state_clone.gui.lock().await;
                            gui.player_inventory.clone()
                        };
                        let placed = {
                            let mut ench = state_clone.enchanter.lock().await;
                            ench.place_anvil(&bot_clone, &player_inv).await
                        };
                        if placed {
                            info!("Anvil placement verified! Proceeding to Phase 2: Items Retrieval.");
                        } else {
                            warn!("Anvil was in inventory but placement could not be verified in world! Proceeding to Phase 2 since anvil is already in inventory.");
                        }
                        let mut gui = state_clone.gui.lock().await;
                        gui.phase = WithdrawalPhase::ItemsRetrieval;
                    }

                    let needs_anvils = state_clone.gui.lock().await.phase == WithdrawalPhase::AnvilPlacement;
                    if !needs_anvils {
                        let mut gui = state_clone.gui.lock().await;
                        let completed_sets = gui.count_completed_max_sets(Some(&bot_clone));
                        if completed_sets > 0 {
                            info!("Detected {completed_sets} COMPLETE max-enchanted God-armor set(s) in inventory from previous session! Proceeding directly to delivery routine...");
                            gui.target_sets_to_deliver = completed_sets;
                            gui.target_delivered_helmets = 0;
                            gui.target_delivered_chestplates = 0;
                            gui.target_delivered_leggings = 0;
                            gui.target_delivered_boots = 0;
                            gui.phase = WithdrawalPhase::Done;
                            gui.state = OrderWorkflowState::WithdrawalComplete;
                            gui.clear_watchdog();
                            drop(gui);
                            trigger_enchanting_routine(&bot_clone, &state_clone).await;
                            return;
                        }

                        let anvil_ready = {
                            let mut ench = state_clone.enchanter.lock().await;
                            ench.check_if_anvil_placed(&bot_clone).await || anvil_count >= 1
                        };

                        if anvil_ready && (gui.can_craft_god_armor(Some(&bot_clone)) || (gui.count_free_inventory_slots() == 0 && gui.has_any_craftable_combines(Some(&bot_clone)))) {
                            let craftable_sets = gui.count_craftable_god_sets(Some(&bot_clone));
                            info!("Detected craftable materials for {craftable_sets} COMPLETE God-armor set(s) or partial combines in inventory from previous session! Proceeding directly to enchanting routine...");
                            gui.phase = WithdrawalPhase::Done;
                            gui.state = OrderWorkflowState::WithdrawalComplete;
                            gui.clear_watchdog();
                            drop(gui);
                            trigger_enchanting_routine(&bot_clone, &state_clone).await;
                            return;
                        }

                        if anvil_ready && gui.collected.is_fulfilled(&gui.quota) {
                            info!("All items already fulfilled in inventory; proceeding directly to enchanting!");
                            gui.phase = WithdrawalPhase::Done;
                            gui.state = OrderWorkflowState::WithdrawalComplete;
                            gui.clear_watchdog();
                            drop(gui);
                            trigger_enchanting_routine(&bot_clone, &state_clone).await;
                            return;
                        }
                    }

                    if state_clone.gui.lock().await.phase == WithdrawalPhase::AnvilPlacement {
                        info!("Looking completely down at feet (pitch: 90.0) before running /order...");
                        crate::enchanter::smooth_look(&bot_clone, bot_clone.direction().y_rot(), 90.0).await;
                        bot_clone.set_direction(bot_clone.direction().y_rot(), 90.0);
                        bot_clone.wait_ticks(2).await;
                    }

                    info!("Running /order command for item withdrawal...");
                    {
                        let mut gui = state_clone.gui.lock().await;
                        gui.state = OrderWorkflowState::Spawned;
                        gui.is_fulfilling_target = false;
                        gui.prepare_to_send_command(&bot_clone, "/order");
                    }
                    *state_clone.order_sent.lock().await = true;
                });
            }
        }
        Event::Chat(packet) => {
            let message = packet.message().to_string();
            let clean_msg = message.to_lowercase();

            if clean_msg.contains("you have no items to collect") || clean_msg.contains("no items to collect") {
                let item_name = {
                    let mut gui = state.gui.lock().await;
                    gui.record_no_items_to_collect(&bot)
                };

                let Some(item_name) = item_name else {
                    info!("Ignoring no-items chat without an active Collect request.");
                    return Ok(());
                };
                info!("Selected order for '{item_name}' is empty; checking remaining orders.");

                let bot_clone = bot.clone();
                let state_chat = state.clone();
                tokio::spawn(async move {
                    bot_clone.wait_ticks(20).await;
                    info!("Re-opening /order to check remaining needed items...");
                    {
                        let mut gui = state_chat.gui.lock().await;
                        gui.prepare_to_send_command(&bot_clone, "/order");
                    }
                });
            } else {
                if clean_msg.contains("connecting to an area in maintenance") || clean_msg.contains("area in maintenance") {
                    warn!("[Chat Maintenance] Destination area is in maintenance: {}", message);
                    state.maintenance_active.store(true, std::sync::atomic::Ordering::SeqCst);
                } else if clean_msg.contains("proxy limbo") || clean_msg.contains("server is restarting") || clean_msg.contains("servers are updating") {
                    warn!("[Chat Restart] Server is restarting / in proxy limbo: '{}'", message);
                    let bot_clone = bot.clone();
                    tokio::spawn(async move {
                        warn!("Waiting 15 seconds for server update to settle before disconnecting to trigger clean auto-reconnect...");
                        tokio::time::sleep(tokio::time::Duration::from_secs(15)).await;
                        bot_clone.disconnect();
                    });
                } else {
                    info!("[Chat] {}", message);
                }
            }
        }
        Event::Packet(packet) => {
            match packet.as_ref() {
                ClientboundGamePacket::OpenScreen(_)
                | ClientboundGamePacket::ContainerSetContent(_)
                | ClientboundGamePacket::ContainerSetSlot(_)
                | ClientboundGamePacket::SetPlayerInventory(_)
                | ClientboundGamePacket::SetExperience(_)
                | ClientboundGamePacket::ContainerSetData(_)
                | ClientboundGamePacket::ContainerClose(_) => {
                    handle_packet(&bot, &packet, &state).await;
                }
                _ => {}
            }
        }
        Event::Disconnect(reason) => {
            warn!("Disconnected from server: {:?}", reason);
            *state.spawned.lock().await = false;
            *state.order_sent.lock().await = false;
            state.is_enchanting.store(false, std::sync::atomic::Ordering::SeqCst);
            {
                let mut ench = state.enchanter.lock().await;
                ench.is_enchanting = false;
                ench.anvil_container_id = None;
            }
            {
                let mut gui = state.gui.lock().await;
                gui.state = OrderWorkflowState::Spawned;
                gui.current_container_id = 0;
                gui.action_in_progress = false;
                gui.current_slots.clear();
                gui.player_inventory.clear();
                gui.clear_watchdog();
            }
        }
        _ => {}
    }

    Ok(())
}

async fn handle_packet(bot: &Client, packet: &Arc<ClientboundGamePacket>, state: &BotState) {
    match packet.as_ref() {
        ClientboundGamePacket::OpenScreen(p) => {
            let title = p.title.to_string();
            let clean_title = title.to_lowercase();

            if clean_title.contains("repair") || clean_title.contains("anvil") || clean_title.contains("name") {
                let mut ench = state.enchanter.lock().await;
                ench.on_open_screen(p.container_id, &title);
            } else {
                let is_enchanting = state.is_enchanting.load(std::sync::atomic::Ordering::SeqCst);
                let mut gui = state.gui.lock().await;
                if !gui.is_fulfilling_target && (is_enchanting || gui.phase == WithdrawalPhase::Done || gui.state == OrderWorkflowState::WithdrawalComplete) {
                    info!("Non-anvil container #{} ('{}') opened while enchanting/done; dismissing.", p.container_id, title);
                    bot.write_packet(ServerboundContainerClose { container_id: p.container_id });
                } else {
                    gui.on_open_screen(p.container_id, &title);
                }
            }
        }
        ClientboundGamePacket::ContainerSetContent(p) => {
            let mut ench = state.enchanter.lock().await;
            if ench.anvil_container_id == Some(p.container_id) {
                ench.on_set_content(p.container_id, p.state_id, &p.items);
            } else {
                if p.container_id == 0 {
                    for (slot, item) in p.items.iter().enumerate() {
                        ench.player_inventory.insert(slot as i16, item.clone());
                    }
                }
                drop(ench);
                let mut gui = state.gui.lock().await;
                gui.on_set_content(p.container_id, p.state_id, &p.items, Some(bot));

                if p.container_id > 0 {
                    let is_enchanting = state.is_enchanting.load(std::sync::atomic::Ordering::SeqCst);
                    if !gui.is_fulfilling_target && (is_enchanting || gui.phase == WithdrawalPhase::Done || gui.state == OrderWorkflowState::WithdrawalComplete) {
                        bot.write_packet(ServerboundContainerClose { container_id: p.container_id });
                        return;
                    }

                    let container_id = p.container_id;
                    let epoch = gui.gui_action_epoch;
                    let bot_clone = bot.clone();
                    let state_clone = state.clone();

                    tokio::spawn(async move {
                        pump_gui_actions(bot_clone, state_clone, Some((container_id, epoch))).await;
                    });
                }
            }
        }
        ClientboundGamePacket::ContainerSetSlot(p) => {
            let container_id = p.container_id;
            let should_pump = {
                let mut gui = state.gui.lock().await;
                gui.on_set_slot(p.container_id, p.state_id, p.slot as i16, &p.item_stack, Some(bot));
                container_id > 0 && container_id == gui.current_container_id && !gui.awaiting_response
            };

            if should_pump {
                let bot_clone = bot.clone();
                let state_clone = state.clone();
                tokio::spawn(async move {
                    pump_gui_actions(bot_clone, state_clone, None).await;
                });
            }

            {
                let mut ench = state.enchanter.lock().await;
                if ench.anvil_container_id == Some(p.container_id) {
                    ench.anvil_state_id.store(p.state_id, std::sync::atomic::Ordering::SeqCst);
                    ench.anvil_slots.insert(p.slot as i16, p.item_stack.clone());
                    if p.slot >= 3 && p.slot <= 38 {
                        let inv_slot = (p.slot - 3 + 9) as i16;
                        ench.player_inventory.insert(inv_slot, p.item_stack.clone());
                    }
                } else if p.container_id == -2 {
                    if let Some(slot) = armor::player_menu_slot(p.slot as u32) {
                        ench.player_inventory.insert(slot, p.item_stack.clone());
                    }
                } else if p.container_id == 0 {
                    ench.player_inventory.insert(p.slot as i16, p.item_stack.clone());
                }
            }
        }
        ClientboundGamePacket::SetPlayerInventory(p) => {
            let Some(slot) = armor::player_menu_slot(p.slot) else { return; };
            {
                let mut gui = state.gui.lock().await;
                gui.player_inventory.insert(slot, p.contents.clone());
                gui.sync_collected_from_inventory(Some(bot));
            }
            state.enchanter.lock().await.player_inventory.insert(slot, p.contents.clone());
        }
        ClientboundGamePacket::SetExperience(p) => {
            state.current_level.store(p.experience_level as u32, std::sync::atomic::Ordering::SeqCst);
            let prog_milli = (p.experience_progress * 1000.0).round() as u32;
            state.experience_progress_milli.store(prog_milli, std::sync::atomic::Ordering::SeqCst);

            // Compute true current experience points instead of stale/corrupted server lifetime total
            let true_current_xp = enchanter::calculate_current_xp(p.experience_level as u32, p.experience_progress);
            state.total_experience.store(true_current_xp, std::sync::atomic::Ordering::SeqCst);
            state.experience_updated.notify_waiters();

            tracing::debug!(
                "[XP Sync] Level: {}, Progress: {:.1}%, True Current XP: {} (Server Lifetime Total: {})",
                p.experience_level, p.experience_progress * 100.0, true_current_xp, p.total_experience
            );
        }
        ClientboundGamePacket::ContainerSetData(p) => {
            let is_anvil = state.enchanter.lock().await.anvil_container_id == Some(p.container_id);
            if is_anvil && p.id == 0 {
                if p.value > 0 && p.value < 40 {
                    state.server_anvil_cost.store(p.value as u32, std::sync::atomic::Ordering::SeqCst);
                    tracing::debug!("[Anvil Data] Repair Cost: {} levels", p.value);
                } else {
                    state.server_anvil_cost.store(0, std::sync::atomic::Ordering::SeqCst);
                }
            }
        }
        ClientboundGamePacket::ContainerClose(p) => {
            info!("Server closed container #{}", p.container_id);
            {
                let mut gui = state.gui.lock().await;
                if gui.current_container_id == p.container_id {
                    gui.current_container_id = 0;
                    gui.current_slots.clear();
                    gui.record_action_sent(None, None);
                }
            }
            {
                let mut ench = state.enchanter.lock().await;
                if ench.anvil_container_id == Some(p.container_id) {
                    ench.anvil_container_id = None;
                    ench.anvil_slots.clear();
                }
            }
        }
        _ => {}
    }
    state.inventory_updated.notify_one();
}


async fn pump_gui_actions(bot_clone: Client, state_clone: BotState, expected: Option<(i32, u64)>) {
    if expected.is_some() {
        bot_clone.wait_ticks(3).await;
    }
    let mut g = state_clone.gui.lock().await;
    if expected.is_some_and(|(id, epoch)| g.current_container_id != id || g.gui_action_epoch != epoch) {
        return;
    }
    if g.current_container_id == 0 || g.current_slots.is_empty() || g.awaiting_response { return; }
    let was_selling = g.state == OrderWorkflowState::SellingInventory;
    let done = g.process_gui_actions(&bot_clone).await;
    if was_selling { return; }

    // Check if Phase 1 (AnvilPlacement) completed
    if g.phase == WithdrawalPhase::AnvilPlacement && (g.anvil_stack_dropped || g.collected.anvils >= 2) {
        g.phase = WithdrawalPhase::ItemsRetrieval;
        g.state = OrderWorkflowState::WaitingForNextOrder;
        info!("Phase 1 completed: Anvil stack handled! Ensuring anvils in offhand...");
        crate::enchanter::EnchanterManager::ensure_anvils_in_offhand(&bot_clone, &mut g.player_inventory).await;
        g.close_current_gui(&bot_clone);
        let player_inv = g.player_inventory.clone();
        drop(g);

        bot_clone.wait_ticks(5).await;
        let anvil_placed = {
            let mut ench = state_clone.enchanter.lock().await;
            ench.check_if_anvil_placed(&bot_clone).await
        };
        if !anvil_placed {
            info!("No anvil placed in world. Attempting to place anvil from inventory...");
            let placed = {
                let mut ench = state_clone.enchanter.lock().await;
                ench.place_anvil(&bot_clone, &player_inv).await
            };
            if placed {
                info!("Anvil verified placed in world! Ready for Phase 2.");
            } else {
                warn!("Warning: Anvil placement verification failed after withdrawal! Checking /home 1...");
                gui::send_command(&bot_clone, "/home 1");
                bot_clone.wait_ticks(30).await;
                let placed2 = {
                    let mut ench = state_clone.enchanter.lock().await;
                    ench.place_anvil(&bot_clone, &player_inv).await
                };
                if placed2 {
                    info!("Anvil verified placed successfully after /home 1! Ready for Phase 2.");
                } else {
                    warn!("Anvil placement could not be verified after /home 1; proceeding to items retrieval.");
                }
            }
        } else {
            info!("Anvil is already placed in world! Ready for Phase 2.");
        }

        bot_clone.wait_ticks(3).await;
        info!("Opening /order for Phase 2 (Items Retrieval)...");
        {
            let mut gui = state_clone.gui.lock().await;
            gui.prepare_to_send_command(&bot_clone, "/order");
        }
        return;
    }

    if done && g.state == OrderWorkflowState::WithdrawalComplete {
        g.state = OrderWorkflowState::WaitingForNextOrder;
        if g.collected.total_diamond_armor() > 0 {
            info!("Phase 2 completed: All required items retrieved from orders!");
            drop(g);
            trigger_enchanting_routine(&bot_clone, &state_clone).await;
        } else {
            info!("All batches processed! No diamond armors left to enchant (out of armors).");
            info!("Stock alerts require a fresh order scan.");
        }
    }
}

async fn wait_for_inventory_update(state: &BotState, timeout: std::time::Duration) {
    let _ = tokio::time::timeout(timeout, state.inventory_updated.notified()).await;
}

async fn trigger_enchanting_routine(bot: &Client, state: &BotState) {
    if state.is_enchanting.compare_exchange(false, true, std::sync::atomic::Ordering::SeqCst, std::sync::atomic::Ordering::SeqCst).is_err() {
        return;
    }
    let player_inv = {
        let mut gui = state.gui.lock().await;
        gui.close_current_gui(bot);
        gui.state = OrderWorkflowState::WithdrawalComplete;
        gui.player_inventory.clone()
    };
    {
        let mut ench = state.enchanter.lock().await;
        ench.is_enchanting = true;
        ench.player_inventory = player_inv;
    }
    info!("Withdrawal complete! Starting autonomous enchanting routine...");

    let bot_ench = bot.clone();
    let state_clone = state.clone();

    tokio::spawn(async move {
        // Fast container close settle
        bot_ench.wait_ticks(2).await;

        let player_inv = {
            let gui = state_clone.gui.lock().await;
            gui.player_inventory.clone()
        };

        // 1. Ensure any worn armor is unequipped
        EnchanterManager::ensure_no_worn_armor(&bot_ench, &player_inv).await;
        bot_ench.wait_ticks(1).await;

        // 2. Ensure Anvil is placed
        {
            let mut ench = state_clone.enchanter.lock().await;
            let placed = ench.place_anvil(&bot_ench, &player_inv).await;
            if !placed {
                warn!("No anvil placed and could not place one from inventory!");
                ench.is_enchanting = false;
                state_clone.is_enchanting.store(false, std::sync::atomic::Ordering::SeqCst);
                drop(ench);

                let has_anvil = player_inv.values().any(|item| {
                    crate::nbt::inspect_item_with_bot(item, Some(&bot_ench))
                        .map(|info| crate::nbt::is_anvil(&info))
                        .unwrap_or(false)
                });

                if has_anvil {
                    warn!("Player has an anvil in inventory but placement failed. Running /home 1 and retrying placement...");
                    gui::send_command(&bot_ench, "/home 1");
                    bot_ench.wait_ticks(30).await;
                    let mut ench = state_clone.enchanter.lock().await;
                    let placed2 = ench.place_anvil(&bot_ench, &player_inv).await;
                    if !placed2 {
                        error!("Anvil placement still failed after /home 1. Disconnecting bot to prevent loop.");
                        state_clone.is_enchanting.store(false, std::sync::atomic::Ordering::SeqCst);
                        bot_ench.disconnect();
                        return;
                    }
                    ench.is_enchanting = true;
                    state_clone.is_enchanting.store(true, std::sync::atomic::Ordering::SeqCst);
                } else {
                    let mut gui = state_clone.gui.lock().await;
                    gui.phase = WithdrawalPhase::AnvilPlacement;
                    gui.state = OrderWorkflowState::Spawned;
                    state_clone.is_enchanting.store(false, std::sync::atomic::Ordering::SeqCst);
                    gui.prepare_to_send_command(&bot_ench, "/order");
                    return;
                }
            }
        }

        bot_ench.wait_ticks(2).await;

        'sets: loop {
            // Finish and drop one set, then use the next set already in inventory.
            let mut anvil_open_attempts = 0;
            loop {
                if !*state_clone.spawned.lock().await { return; }

                // Check if all pieces are confirmed fully enchanted
                {
                    let ench = state_clone.enchanter.lock().await;
                    let (all_done, summary) = ench.check_all_armor_enchanted(&bot_ench);
                    let maxed_count = ench.count_max_enchanted_pieces(&bot_ench);
                    if !ench.has_pending_anvil_work() && all_done {
                        info!("*** Armor pieces confirmed fully enchanted ({maxed_count} piece(s) ready)! ***");
                        info!("Final Armor Status: {summary}");
                        break;
                    }
                }

                // Check if anvil is open; if not, ensure it's placed and open it
                let is_open = {
                    let ench = state_clone.enchanter.lock().await;
                    ench.anvil_container_id.is_some()
                };

                if !is_open {
                    anvil_open_attempts += 1;
                    if anvil_open_attempts >= 6 {
                        error!("Anvil failed to open after 6 attempts! Disconnecting to rejoin and start where left...");
                        state_clone.enchanter.lock().await.is_enchanting = false;
                        state_clone.is_enchanting.store(false, std::sync::atomic::Ordering::SeqCst);
                        bot_ench.disconnect();
                        return;
                    }

                    let current_inv = {
                        let ench = state_clone.enchanter.lock().await;
                        ench.player_inventory.clone()
                    };

                    let mut ench = state_clone.enchanter.lock().await;
                    if !ench.check_if_anvil_placed(&bot_ench).await {
                        info!("Anvil is not present in world (broke or missing). Placing a new Anvil from inventory...");
                        let placed = ench.place_anvil(&bot_ench, &current_inv).await;
                        bot_ench.wait_ticks(2).await;
                        if !placed {
                            warn!("Cannot place Anvil (out of anvils in inventory). Looking completely down at feet (pitch: 90.0) and aborting enchanting routine to withdraw replacement anvil from orders!");
                            ench.is_enchanting = false;
                            state_clone.is_enchanting.store(false, std::sync::atomic::Ordering::SeqCst);
                            drop(ench);
                            crate::enchanter::smooth_look(&bot_ench, bot_ench.direction().y_rot(), 90.0).await;
                            bot_ench.set_direction(bot_ench.direction().y_rot(), 90.0);
                            bot_ench.wait_ticks(2).await;
                            let mut gui = state_clone.gui.lock().await;
                            gui.phase = WithdrawalPhase::AnvilPlacement;
                            gui.anvil_stack_dropped = false;
                            gui.state = OrderWorkflowState::Spawned;
                            gui.prepare_to_send_command(&bot_ench, "/order");
                            return;
                        }
                    }

                    info!("Opening anvil screen (attempt {anvil_open_attempts}/6)...");
                    ench.open_anvil_with_inv(&bot_ench, &current_inv).await;
                    drop(ench);

                    let open_deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
                    while std::time::Instant::now() < open_deadline {
                        if !*state_clone.spawned.lock().await { return; }
                        if state_clone.enchanter.lock().await.anvil_container_id.is_some() { break; }
                        wait_for_inventory_update(&state_clone, std::time::Duration::from_millis(100)).await;
                    }
                    continue;
                }

                anvil_open_attempts = 0;

                let mut ench = state_clone.enchanter.lock().await;
                let finished = ench.process_anvil_combines(&bot_ench).await;
                if finished {
                    info!("Anvil combining phase finished successfully!");
                    break;
                }
                if ench.restock_needed { break; }
                let discard_item = ench.book_to_discard.take();
                let discard_slot = ench.book_to_discard_slot.take();
                let xp_target = ench.xp_target.take();
                drop(ench);

                if let Some(ref rejected_book) = discard_item {
                    info!("Discarding high XP cost book into hopper...");
                    let dropped = drop_rejected_book_in_hopper(&bot_ench, &state_clone, rejected_book, discard_slot).await;
                    if dropped {
                        info!("Successfully discarded high-cost book into hopper.");
                    } else {
                        warn!("Failed to confirm discarded book left inventory.");
                        let mut ench = state_clone.enchanter.lock().await;
                        if let Some(slot) = discard_slot {
                            ench.rejected_inventory_slots.insert(slot);
                        }
                    }
                    // Check if another book for this armor piece is already available in inventory
                    let has_another_book = {
                        let mut ench = state_clone.enchanter.lock().await;
                        let gui_inv = state_clone.gui.lock().await.player_inventory.clone();
                        ench.player_inventory = gui_inv;
                        ench.has_available_book_for_current_armor(&bot_ench)
                    };
                    if has_another_book {
                        info!("Another book for this armor piece is already available in inventory. Continuing enchanting routine...");
                        continue;
                    } else {
                        info!("No replacement book available in inventory for this armor piece. Breaking enchanting routine to restock from /order...");
                        let mut ench = state_clone.enchanter.lock().await;
                        ench.restock_needed = true;
                        break;
                    }
                }
                if let Some(target) = xp_target {
                    // The worker shares XP atomics, while packet handlers retain access
                    // to the main enchanter inventory and can process XP updates promptly.
                    let mut worker = EnchanterManager::new();
                    {
                        let ench_lock = state_clone.enchanter.lock().await;
                        worker.throw_speed_ticks = ench_lock.throw_speed_ticks;
                        worker.strict_calculation = ench_lock.strict_calculation;
                    }
                    worker.player_inventory = state_clone.enchanter.lock().await.player_inventory.clone();
                    worker.current_level = state_clone.current_level.clone();
                    worker.experience_progress_milli = state_clone.experience_progress_milli.clone();
                    worker.total_experience = state_clone.total_experience.clone();
                    worker.experience_updated = state_clone.experience_updated.clone();
                    worker.throw_exact_xp_bottles(&bot_ench, target).await;
                    {
                        let mut ench = state_clone.enchanter.lock().await;
                        ench.player_inventory = worker.player_inventory.clone();
                    }
                    {
                        let mut gui = state_clone.gui.lock().await;
                        gui.player_inventory = worker.player_inventory.clone();
                        gui.sync_collected_from_inventory(Some(&bot_ench));
                    }
                    if worker.get_level_and_progress().0 < target {
                        warn!("XP could not reach level {target}; returning to inventory reconciliation.");
                        info!("Checking /order for XP bottles before considering an out-of-stock alert.");
                        break;
                    }
                }

                wait_for_inventory_update(&state_clone, std::time::Duration::from_millis(100)).await;
                if !*state_clone.spawned.lock().await { return; }
            }

            // 5. Close Anvil
            let inv = {
                let mut ench = state_clone.enchanter.lock().await;
                if let Some(id) = ench.anvil_container_id {
                    ench.close_anvil(&bot_ench, id);
                }
                ench.player_inventory.clone()
            };
            {
                let mut gui = state_clone.gui.lock().await;
                gui.player_inventory = inv;
                gui.sync_collected_from_inventory(Some(&bot_ench));
            }
            bot_ench.wait_ticks(2).await;

            // Ensure any armor accidentally worn into equipment slots 5..=8 is unequipped
            {
                let gui_inv = state_clone.gui.lock().await.player_inventory.clone();
                EnchanterManager::ensure_no_worn_armor(&bot_ench, &gui_inv).await;
            }
            bot_ench.wait_ticks(1).await;

            // 6. Drop enchanted armor into the nearest hopper within 2 blocks
            let ready = state_clone.enchanter.lock().await.check_all_armor_enchanted(&bot_ench).0;
            if ready && !drop_enchanted_armor_in_hopper(&bot_ench, &state_clone).await {
                error!("Drop not confirmed; retaining current set for recovery.");
                state_clone.is_enchanting.store(false, std::sync::atomic::Ordering::SeqCst);
                bot_ench.disconnect();
                return;
            }
            if !ready {
                warn!("Set is unfinished; keeping armor and restocking supplies.");
            }

            if ready {
                let inventory = {
                    let gui = state_clone.gui.lock().await;
                    gui.player_inventory.clone()
                };
                let items: Vec<_> = inventory.iter()
                    .filter(|(slot, _)| (9..=44).contains(*slot))
                    .filter_map(|(&slot, item)| crate::nbt::inspect_item_with_bot(item, Some(&bot_ench)).map(|info| (slot, info)))
                    .collect();
                if armor::has_armor_set(&items) {
                    let mut ench = state_clone.enchanter.lock().await;
                    ench.reset_for_next_batch();
                    ench.is_enchanting = true;
                    state_clone.is_enchanting.store(true, std::sync::atomic::Ordering::SeqCst);
                    ench.player_inventory = inventory;
                    info!("Starting the next stocked set immediately; no order withdrawal or inventory cleaning needed.");
                    continue 'sets;
                }
            }
            break 'sets;
        }

        // 7. Reset state and check /order -> Your Orders for next batch of armors
        info!("Reconciling inventory before the next withdrawal...");
        {
            let mut ench = state_clone.enchanter.lock().await;
            ench.reset_for_next_batch();
        }
        state_clone.is_enchanting.store(false, std::sync::atomic::Ordering::SeqCst);
        let anvil_count = {
            let mut gui = state_clone.gui.lock().await;
            gui.reset_and_sync_inventory(Some(&bot_ench));
            crate::enchanter::EnchanterManager::ensure_anvils_in_offhand(&bot_ench, &mut gui.player_inventory).await;
            gui.collected.anvils
        };

        if anvil_count < 2 {
            info!("Bot has {anvil_count} anvil(s) (< 2, or anvil == 1). Looking completely down at feet (pitch: 90.0) and running /order...");
            crate::enchanter::smooth_look(&bot_ench, bot_ench.direction().y_rot(), 90.0).await;
            bot_ench.set_direction(bot_ench.direction().y_rot(), 90.0);
            bot_ench.wait_ticks(2).await;

            let mut gui = state_clone.gui.lock().await;
            gui.phase = WithdrawalPhase::AnvilPlacement;
            gui.anvil_stack_dropped = false;
            gui.state = OrderWorkflowState::Spawned;
            gui.prepare_to_send_command(&bot_ench, "/order");
        } else {
            {
                let mut gui = state_clone.gui.lock().await;
                gui.phase = WithdrawalPhase::ItemsRetrieval;
                gui.state = OrderWorkflowState::Spawned;
            }
            bot_ench.wait_ticks(3).await;
            info!("Opening /order to check remaining armors in 'Your Orders'...");
            let mut gui = state_clone.gui.lock().await;
            gui.prepare_to_send_command(&bot_ench, "/order");
        }
    });
}

/// Drop the verified inventory slot directly, as in commit 53b7303.
/// Success means the request was sent; the caller must still confirm the inventory decrease.
async fn drop_one_inventory_item(
    bot: &Client, state: &BotState, slot: i16, expected: &azalea::inventory::ItemStack,
) -> bool {
    use azalea::entity::inventory::Inventory;
    use azalea::inventory::{ItemStack, operations::ClickType};
    use azalea::protocol::packets::game::s_container_click::{HashedStack, ServerboundContainerClick};
    if !matches!(expected, ItemStack::Present(data) if data.count == 1) {
        warn!("Refusing to drop stacked item from slot #{slot}.");
        return false;
    }
    // Strict safety check: Never drop incomplete diamond armor, anvils, or XP bottles under any circumstances!
    if let ItemStack::Present(data) = expected {
        let k = format!("{:?}", data.kind).to_lowercase();
        if k.contains("anvil") {
            error!("CRITICAL DROP SAFETY VIOLATION: Refusing to drop anvil!");
            return false;
        }
        if k.contains("bottle") || k.contains("experience") {
            error!("CRITICAL DROP SAFETY VIOLATION: Refusing to drop XP bottles!");
            return false;
        }
        if k.contains("diamond") && (k.contains("helmet") || k.contains("chestplate") || k.contains("leggings") || k.contains("boots")) {
            let is_comp = crate::nbt::inspect_item_with_bot(expected, Some(bot))
                .map(|info| crate::armor::is_complete(&info))
                .unwrap_or(false);
            if !is_comp {
                error!(
                    "CRITICAL DROP SAFETY VIOLATION: Expected drop item is incomplete diamond armor ({})! REFUSING TO DROP!",
                    k
                );
                return false;
            }
        }
    }
    if let Some(info) = crate::nbt::inspect_item_with_bot(expected, Some(bot)) {
        if crate::nbt::is_anvil(&info) || crate::nbt::is_xp_bottle(&info) {
            error!("CRITICAL DROP SAFETY VIOLATION: Expected drop item is anvil or XP bottle! REFUSING TO DROP!");
            return false;
        }
        if crate::nbt::is_diamond_armor(&info) && !crate::armor::is_complete(&info) {
            error!(
                "CRITICAL DROP SAFETY VIOLATION: Expected drop item is incomplete {} (enchants: {:?})! REFUSING TO DROP!",
                info.kind, info.enchantments
            );
            return false;
        }
    }
    let Some(active_menu) = bot.get_component::<Inventory>().map(|inventory| inventory.id) else { return false; };
    if active_menu != 0 {
        drops::close_menu(bot, active_menu);
        wait_for_inventory_update(state, std::time::Duration::from_millis(250)).await;
    }
    let cursor_empty = bot.get_component::<Inventory>()
        .is_some_and(|inventory| inventory.carried == ItemStack::Empty);
    if !*state.spawned.lock().await || !cursor_empty {
        warn!("Cannot drop while disconnected or carrying an item on the cursor.");
        return false;
    }
    let gui = state.gui.lock().await;
    let can_drop = bot.get_component::<Inventory>()
        .is_some_and(|inventory| inventory.id == 0 && inventory.carried == ItemStack::Empty);
    if !(9..=44).contains(&slot) || gui.player_inventory.get(&slot) != Some(expected) || !can_drop {
        warn!("Drop source #{slot} or active menu changed; no drop sent.");
        return false;
    }
    // Throw exactly one item from its current slot. No staging swap, selected
    // hand change, or client-side inventory prediction is needed.
    bot.write_packet(ServerboundContainerClick {
        container_id: 0,
        state_id: gui.player_state_id,
        slot_num: slot,
        button_num: 0,
        click_type: ClickType::Throw,
        changed_slots: Default::default(),
        carried_item: HashedStack(None),
    });
    // Keep the server-confirmed-count check: request a snapshot if the server
    // accepts the drop without broadcasting its predicted inventory change.
    drops::request_inventory_refresh(bot, 0);
    info!("Sent single-item inventory drop from verified slot #{slot}; requested server inventory refresh.");
    true
}
/// Discard only the exact book rejected by the anvil, after the server returns it.
async fn drop_rejected_book_in_hopper(
    bot: &Client, state: &BotState, expected: &azalea::inventory::ItemStack, preferred_slot: Option<i16>,
) -> bool {
    use azalea::inventory::ItemStack;
    use azalea_registry::builtin::ItemKind;
    if !matches!(expected, ItemStack::Present(data) if data.count == 1 && data.kind == ItemKind::EnchantedBook) {
        error!("Rejected anvil input is not a single enchanted book; refusing to discard it.");
        return false;
    }
    if let Some(info) = crate::nbt::inspect_item_with_bot(expected, Some(bot)) {
        if crate::nbt::is_diamond_armor(&info) || crate::nbt::is_anvil(&info) || crate::nbt::is_xp_bottle(&info) {
            error!("CRITICAL GUARD: Refusing to discard non-book item {} in drop_rejected_book_in_hopper!", info.kind);
            return false;
        }
    }
    state.gui.lock().await.close_current_gui(bot);
    let Some(hopper_pos) = EnchanterManager::find_nearby_hopper(bot) else {
        error!("Cannot drop rejected book: no hopper within reach (4.5 blocks)!");
        return false;
    };
    let pos = bot.position();
    let dx = hopper_pos.x as f64 + 0.5 - pos.x;
    let dz = hopper_pos.z as f64 + 0.5 - pos.z;
    let dy = hopper_pos.y as f64 + 0.8 - (pos.y + 1.62);
    smooth_look(bot, (-dx).atan2(dz).to_degrees() as f32,
        (-dy).atan2((dx * dx + dz * dz).sqrt()).to_degrees() as f32).await;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let (slot, before) = loop {
        if !*state.spawned.lock().await { return false; }
        {
            let gui = state.gui.lock().await;
            if let Some(pref) = preferred_slot {
                if gui.player_inventory.get(&pref) == Some(expected) {
                    let total = (9..=44).filter(|s| gui.player_inventory.get(s) == Some(expected)).count();
                    break (pref, total);
                }
            }
            let matching: Vec<_> = (36..=44).chain(9..=35).filter(|slot| gui.player_inventory.get(slot) == Some(expected)).collect();
            if let Some(&slot) = matching.first() { break (slot, matching.len()); }
        }
        if std::time::Instant::now() >= deadline {
            warn!("The rejected book was not returned to inventory; refusing to substitute another book.");
            return false;
        }
        wait_for_inventory_update(state, std::time::Duration::from_millis(100)).await;
    };
    if !drop_one_inventory_item(bot, state, slot, expected).await { return false; }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        wait_for_inventory_update(state, std::time::Duration::from_millis(100)).await;
        if !*state.spawned.lock().await { return false; }
        let gui = state.gui.lock().await;
        let remaining = (9..=44).filter(|slot| gui.player_inventory.get(slot) == Some(expected)).count();
        if armor::drop_status(before, remaining) == armor::DropStatus::Confirmed {
            info!("Server confirmed the rejected book left the player inventory.");
            return true;
        }
        if std::time::Instant::now() >= deadline {
            warn!("Rejected book drop was not confirmed (before={before}, remaining={remaining}).");
            return false;
        }
    }
}

/// Drop exactly one set in armor order, using server inventory updates as acknowledgement.
/// The cursor and pending count survive automatic reconnects in SESSION_GUI.
async fn drop_enchanted_armor_in_hopper(bot: &Client, state: &BotState) -> bool {
    {
        let mut gui = state.gui.lock().await;
        gui.close_current_gui(bot);
        gui.state = OrderWorkflowState::WithdrawalComplete;
    }

    let Some(hopper_pos) = EnchanterManager::find_nearby_hopper(bot) else {
        error!("No hopper within reach (4.5 blocks). Keeping this set until a hopper is available.");
        return false;
    };
    let pos = bot.position();
    let dx = hopper_pos.x as f64 + 0.5 - pos.x;
    let dz = hopper_pos.z as f64 + 0.5 - pos.z;
    let dy = hopper_pos.y as f64 + 0.8 - (pos.y + 1.62);
    smooth_look(bot, (-dx).atan2(dz).to_degrees() as f32,
        (-dy).atan2((dx * dx + dz * dz).sqrt()).to_degrees() as f32).await;

    loop {
        if !*state.spawned.lock().await { return false; }
        let mut gui = state.gui.lock().await;
        // Never reconcile against an empty cache while reconnecting.
        if !(9..=44).all(|slot| gui.player_inventory.contains_key(&slot)) {
            warn!("Waiting for a full player inventory before dropping armor.");
            return false;
        }
        let items: Vec<_> = gui.player_inventory.iter()
            .filter(|(slot, _)| (9..=44).contains(*slot))
            .filter_map(|(&slot, item)| crate::nbt::inspect_item_with_bot(item, Some(bot)).map(|info| (slot, info)))
            .collect();
        let count_type = |kind| items.iter().filter(|(_, info)| armor::armor_type(info) == Some(kind)
            && armor::is_complete(info)).map(|(_, info)| info.count as usize).sum::<usize>();
        if let Some((kind, before)) = gui.pending_drop {
            let remaining = count_type(kind);
            if armor::drop_status(before, remaining) == armor::DropStatus::Confirmed {
                gui.next_drop = kind + 1;
                gui.pending_drop = None;
            } else if armor::drop_status(before, remaining) == armor::DropStatus::Retained {
                // Fresh reconnect snapshot shows the item was retained; retry this type.
                gui.pending_drop = None;
            } else {
                error!("Ambiguous drop inventory change; retaining the current sequence.");
                return false;
            }
        }
        if gui.next_drop == 4 {
            gui.next_drop = 0;
            info!("Server confirmed all four drops: helmet, chestplate, leggings, boots.");
            return true;
        }
        let kind = gui.next_drop;
        let Some(plan) = armor::drop_plan(&items, kind) else {
            warn!("Remaining set is incomplete (missing complete {}); resetting drop sequence to allow workflow to continue.", armor::TYPES[kind]);
            gui.next_drop = 0;
            gui.pending_drop = None;
            return false;
        };
        let slot = plan[0];

        // CRITICAL DROP SAFETY GUARD: Strict verification that the slot about to be dropped
        // contains the intended complete diamond armor piece, and NEVER an XP bottle, book, anvil, or stacked item!
        let target_item = gui.player_inventory.get(&slot);
        let inspected = target_item.and_then(|i| crate::nbt::inspect_item_with_bot(i, Some(bot)));
        let is_valid_armor_drop = match inspected {
            Some(ref info) => {
                if crate::nbt::is_xp_bottle(info) {
                    error!(
                        "DROP SAFETY VIOLATION: Slot #{slot} contains XP bottles ({} x{})! REFUSING to drop into hopper!",
                        info.kind, info.count
                    );
                    false
                } else if crate::nbt::is_anvil(info) {
                    error!("DROP SAFETY VIOLATION: Slot #{slot} contains an ANVIL! REFUSING to drop into hopper!");
                    false
                } else if info.kind.to_lowercase().contains("book") {
                    error!("DROP SAFETY VIOLATION: Slot #{slot} contains a BOOK! REFUSING to drop into hopper!");
                    false
                } else if info.count != 1 {
                    error!(
                        "DROP SAFETY VIOLATION: Slot #{slot} has stacked count {} (armor cannot stack)! REFUSING to drop into hopper!",
                        info.count
                    );
                    false
                } else if armor::armor_type(info) != Some(kind) || !armor::is_complete(info) {
                    error!(
                        "DROP SAFETY VIOLATION: Slot #{slot} is not complete {}! Found: {} (enchants: {:?}). REFUSING TO DROP!",
                        armor::TYPES[kind], info.kind, info.enchantments
                    );
                    false
                } else {
                    true
                }
            }
            None => {
                error!("DROP SAFETY VIOLATION: Slot #{slot} is empty or uninspectable! REFUSING TO DROP INTO HOPPER!");
                false
            }
        };

        if !is_valid_armor_drop {
            gui.pending_drop = None;
            return false;
        }

        let before = count_type(kind);
        let expected = gui.player_inventory[&slot].clone();
        gui.pending_drop = Some((kind, before));
        drop(gui);
        if !drop_one_inventory_item(bot, state, slot, &expected).await { return false; }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            wait_for_inventory_update(state, std::time::Duration::from_millis(100)).await;
            if !*state.spawned.lock().await { return false; }
            let mut gui = state.gui.lock().await;
            let remaining: usize = gui.player_inventory.iter()
                .filter(|(slot, _)| (9..=44).contains(*slot))
                .filter_map(|(_, item)| crate::nbt::inspect_item_with_bot(item, Some(bot)))
                .filter(|info| armor::armor_type(info) == Some(kind) && armor::is_complete(info))
                .map(|info| info.count as usize).sum();
            if armor::drop_status(before, remaining) == armor::DropStatus::Confirmed {
                gui.next_drop = kind + 1;
                gui.pending_drop = None;
                info!("Confirmed {} left the player inventory.", armor::TYPES[kind]);
                break;
            }
            if std::time::Instant::now() >= deadline {
                warn!("{} drop was not acknowledged (before={before}, remaining={remaining}). Retaining pending drop for reconnect reconciliation.", armor::TYPES[kind]);
                return false;
            }
        }
    }
}

#[allow(dead_code)]
async fn run_target_delivery(bot_ench: Client, state_clone: BotState, complete_sets: usize, resume: bool) {
    // 6. Fulfill buy orders for target player (/order {name})
    let total_pieces_to_deliver = complete_sets * 4;
    let target_name = {
        let mut gui = state_clone.gui.lock().await;
        gui.state = OrderWorkflowState::FillingTargetOrders;
        gui.is_fulfilling_target = true;
        if !resume {
            gui.target_sets_to_deliver = complete_sets;
            gui.target_delivered_helmets = 0;
            gui.target_delivered_chestplates = 0;
            gui.target_delivered_leggings = 0;
            gui.target_delivered_boots = 0;
        }
        let target = gui.order_target.clone();
        info!("Sending '/order {target}' to fulfill buyer orders for {complete_sets} COMPLETE God-armor set(s) ({total_pieces_to_deliver} pieces total)...");
        gui.prepare_to_send_command(&bot_ench, &format!("/order {target}"));
        target
    };

    // Wait for order fulfillment to complete (either all pieces delivered or orders closed)
    let mut wait_ticks_count = 0;
    let mut closed_ticks = 0;
    let mut last_remaining_count = {
        let gui = state_clone.gui.lock().await;
        gui.count_max_enchanted_armors(Some(&bot_ench))
    };
    loop {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if !*state_clone.spawned.lock().await { return; }
        wait_ticks_count += 2;
        let (current_state, container_id) = {
            let gui = state_clone.gui.lock().await;
            (gui.state.clone(), gui.current_container_id)
        };

        // Check if player still has max enchanted armor pieces in inventory
        let (remaining_enchanted_count, has_pending_delivery) = {
            let gui = state_clone.gui.lock().await;
            let is_pending = gui.target_delivering_armor.is_some()
                || gui.state == OrderWorkflowState::DepositingTargetItems
                || gui.state == OrderWorkflowState::ConfirmingFulfill;
            let count = gui.count_max_enchanted_armors(Some(&bot_ench));
            (count, is_pending)
        };

        if remaining_enchanted_count < last_remaining_count && !has_pending_delivery {
            info!(
                "Delivery progress made! Remaining pieces to deliver: {remaining_enchanted_count} (was {last_remaining_count})"
            );
            last_remaining_count = remaining_enchanted_count;
            wait_ticks_count = 0; // Reset timeout on progress
        }

        let (is_delivery_done, has_pending_delivery) = {
            let gui = state_clone.gui.lock().await;
            let is_pending = gui.target_delivering_armor.is_some()
                || gui.state == OrderWorkflowState::DepositingTargetItems
                || gui.state == OrderWorkflowState::ConfirmingFulfill;
            let done = gui.is_target_delivery_completed();
            (done, is_pending)
        };

        if is_delivery_done && !has_pending_delivery {
            info!("All {complete_sets} complete God-armor set(s) have been successfully delivered to {target_name}!");
            let mut gui = state_clone.gui.lock().await;
            gui.is_fulfilling_target = false;
            if gui.current_container_id > 0 {
                gui.close_current_gui(&bot_ench);
            }
            break;
        }

        if container_id == 0 {
            closed_ticks += 2;
        } else {
            closed_ticks = 0;
        }

        // If the GUI has been closed for at least 20 ticks (~1s) while pieces remain, re-send /order {target_name}
        if closed_ticks >= 20
            && current_state != OrderWorkflowState::DepositingTargetItems
            && current_state != OrderWorkflowState::ConfirmingFulfill
            && !has_pending_delivery
            && !state_clone.gui.lock().await.awaiting_response
        {
            info!("Re-opening /order {target_name} to fulfill remaining {remaining_enchanted_count} piece(s)...");
            {
                let mut gui = state_clone.gui.lock().await;
                gui.state = OrderWorkflowState::FillingTargetOrders;
                gui.prepare_to_send_command(&bot_ench, &format!("/order {target_name}"));
            }
            closed_ticks = 0;
        }

        // If the GUI has been open for 15s with no progress, close to trigger refresh
        if container_id > 0 && !has_pending_delivery && wait_ticks_count > 0 && wait_ticks_count % 300 == 0 {
            warn!("Delivery GUI open with no progress for 15s; closing to refresh...");
            let mut gui = state_clone.gui.lock().await;
            gui.close_current_gui(&bot_ench);
        }

        if wait_ticks_count >= 2400 {
            warn!("Delivery incomplete after 120s; retaining the batch and retrying remaining pieces.");
            let mut gui = state_clone.gui.lock().await;
            gui.close_current_gui(&bot_ench);
            gui.state = OrderWorkflowState::FillingTargetOrders;
            gui.prepare_to_send_command(&bot_ench, &format!("/order {target_name}"));
            wait_ticks_count = 0;
        }
    }

    bot_ench.wait_ticks(2).await;

    // 7. Reset state and check /order -> Your Orders for next batch of armors
    info!("Reconciling inventory before the next withdrawal...");
    {
        let mut ench = state_clone.enchanter.lock().await;
        ench.reset_for_next_batch();
    }
    {
        let mut gui = state_clone.gui.lock().await;
        gui.reset_and_sync_inventory(Some(&bot_ench));
    }
    {
        let mut gui = state_clone.gui.lock().await;
        gui.phase = WithdrawalPhase::ItemsRetrieval;
        gui.state = OrderWorkflowState::Spawned;
    }

    bot_ench.wait_ticks(2).await;
    info!("Opening /order to check remaining armors in 'Your Orders'...");
    {
        let mut gui = state_clone.gui.lock().await;
        gui.prepare_to_send_command(&bot_ench, "/order");
    }
}

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    // Robust .env loading: check current working directory first, then executable directory
    let env_file_loaded = if let Ok(path) = dotenvy::dotenv() {
        Some(path)
    } else if let Ok(exe_path) = std::env::current_exe() {
        if let Some(exe_dir) = exe_path.parent() {
            let exe_env = exe_dir.join(".env");
            if exe_env.exists() {
                dotenvy::from_path(&exe_env).ok().map(|_| exe_env)
            } else {
                None
            }
        } else {
            None
        }
    } else {
        None
    };

    // Keep the worker guard alive until shutdown so logs can drain.
    let _log_guard = logging::init();

    let args = Args::parse();

    info!("Starting Minecraft Enchanter Bot...");
    info!("Target Server: {}:{}", args.server, args.port);

    if let Some(ref path) = env_file_loaded {
        info!("Loaded environment configuration from: {}", path.display());
    } else {
        warn!("⚠️ No '.env' file found in current directory or next to Enchanter.exe!");
        warn!("⚠️ The bot will default to offline mode unless credentials are provided via CLI arguments.");
    }

    // Discover all accounts from CLI flags and .env (supporting multi-account)
    let discovered_accounts = auth::discover_accounts(
        args.token.as_deref(),
        args.microsoft.as_deref(),
        args.offline.as_deref(),
    );

    info!("Discovered {} configured account(s):", discovered_accounts.len());
    for (i, acc) in discovered_accounts.iter().enumerate() {
        info!("  [{i}] {}", acc.description());
    }

    let run_all = args.all || args.account.as_deref().map(|s| s.eq_ignore_ascii_case("all")).unwrap_or(false);

    if run_all && discovered_accounts.len() > 1 {
        info!("============================================================");
        info!("*** MULTI-ACCOUNT LAUNCHER: Spawning all {} accounts ***", discovered_accounts.len());
        info!("============================================================");

        let current_exe = std::env::current_exe()?;
        let exe_dir = current_exe.parent().map(|p| p.to_path_buf());
        let mut children = Vec::new();

        for (i, acc) in discovered_accounts.iter().enumerate() {
            info!("Launching account [{i}] ({}) in dedicated window...", acc.description());

            let mut cmd = std::process::Command::new(&current_exe);
            if let Some(ref dir) = exe_dir {
                cmd.current_dir(dir);
            }
            cmd.arg("--account").arg(i.to_string());
            cmd.arg("--server").arg(&args.server);
            cmd.arg("--port").arg(args.port.to_string());
            cmd.arg("--view-distance").arg(args.view_distance.to_string());
            cmd.arg("--order-target").arg(&args.order_target);
            cmd.arg("--xp-throw-speed").arg(&args.xp_throw_speed);
            cmd.arg("--xp-strict-calculation").arg(args.xp_strict_calculation.to_string());
            for (flag, value) in [
                ("--mending", args.mending), ("--unb3", args.unb3),
                ("--prot4", args.prot4), ("--blast-prot4", args.blast_prot4),
                ("--xp-stacks", args.xp_stacks), ("--anvils", args.anvils),
                ("--diamond-helmets", args.diamond_helmets),
                ("--diamond-chestplates", args.diamond_chestplates),
                ("--diamond-leggings", args.diamond_leggings),
                ("--diamond-boots", args.diamond_boots),
            ] {
                cmd.arg(flag).arg(value.to_string());
            }

            #[cfg(target_os = "windows")]
            {
                use std::os::windows::process::CommandExt;
                const CREATE_NEW_CONSOLE: u32 = 0x00000010;
                cmd.creation_flags(CREATE_NEW_CONSOLE);
            }

            match cmd.spawn() {
                Ok(child) => {
                    info!("Successfully launched bot [{i}] (PID: {})", child.id());
                    children.push(child);
                }
                Err(e) => {
                    error!("Failed to launch bot [{i}]: {e}");
                }
            }

            if i + 1 < discovered_accounts.len() {
                info!("Waiting 5s before launching next account to prevent login burst...");
                tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
            }
        }

        info!("All {} bot instances launched! Press Ctrl+C in this launcher window when done.", children.len());
        for mut child in children {
            let _ = child.wait();
        }
        return Ok(());
    }

    let selected_account = auth::select_account(&discovered_accounts, args.account.as_deref());
    info!("Selected account: {}", selected_account.description());

    if let auth::AccountConfig::Offline(ref name) = selected_account {
        if args.server.to_lowercase().contains("donutsmp") {
            warn!("========================================================================");
            warn!("⚠️ WARNING: donutsmp.net is an online-mode server and requires authentication!");
            warn!("⚠️ Connecting as offline account '{}' will be rejected by the server with:", name);
            warn!("⚠️ 'You are not logged into your Minecraft account.'");
            warn!("⚠️ To connect successfully, configure MC_TOKEN or MICROSOFT_EMAIL in .env");
            warn!("========================================================================");
        }
    }

    let account = auth::authenticate_account(selected_account).await?;

    let quota = WithdrawalQuota {
        anvils_needed: args.anvils,
        mending_needed: args.mending,
        unbreaking_3_needed: args.unb3,
        protection_4_needed: args.prot4,
        blast_protection_4_needed: args.blast_prot4,
        xp_stacks_needed: args.xp_stacks,
        diamond_helmets_needed: args.diamond_helmets,
        diamond_chestplates_needed: args.diamond_chestplates,
        diamond_leggings_needed: args.diamond_leggings,
        diamond_boots_needed: args.diamond_boots,
    };
    GLOBAL_QUOTA.set(quota).ok();
    GLOBAL_ORDER_TARGET.set(args.order_target.clone()).ok();

    // Check fallback env vars for throw speed if XP_THROW_SPEED wasn't passed explicitly
    let raw_throw_speed = if std::env::var("XP_THROW_SPEED").is_ok() || args.xp_throw_speed != "1" {
        args.xp_throw_speed.clone()
    } else if let Ok(s) = std::env::var("XP_THROW_SPEED_TICKS") {
        s
    } else if let Ok(s) = std::env::var("XP_THROW_DELAY_TICKS") {
        s
    } else {
        args.xp_throw_speed.clone()
    };
    let throw_speed_ticks = parse_throw_speed(&raw_throw_speed);
    let strict_calculation = args.xp_strict_calculation;

    GLOBAL_XP_THROW_SPEED.set(throw_speed_ticks).ok();
    GLOBAL_XP_STRICT_CALCULATION.set(strict_calculation).ok();
    GLOBAL_VIEW_DISTANCE.set(args.view_distance).ok();
    info!("Requested chunk view distance: {}", args.view_distance);

    info!(
        "XP Bot Configuration: Throw Speed = {} tick(s) per bottle (raw: '{}'), Strict Calculation = {}",
        throw_speed_ticks, raw_throw_speed, strict_calculation
    );

    info!("Order fulfillment target username set to: '{}'", args.order_target);

    let address = format!("{}:{}", args.server, args.port);

    info!("Chat signing disabled: using unsigned chat and commands.");
    loop {
        info!("Connecting to {} as '{}'...", address, account.username());
        // Keep online authentication, but never request chat certificates or
        // establish a signing session (including after reconnecting).
        ClientBuilder::new_without_plugins()
            .add_plugins(
                azalea::DefaultPlugins
                    .build()
                    .disable::<azalea::chat_signing::ChatSigningPlugin>(),
            )
            .add_plugins(azalea::bot::DefaultBotPlugins)
            .add_plugins(tick_diagnostics::TickDiagnosticsPlugin)
            .set_handler(handle)
            .start(account.clone(), address.clone())
            .await;

        warn!("Connection closed or reset by server. Waiting 12 seconds before reconnecting to let proxy cache clear...");
        tokio::time::sleep(tokio::time::Duration::from_secs(12)).await;
    }
}


#[cfg(test)]
pub(crate) mod workflow_tests {
    use super::*;

    #[tokio::test]
    async fn book_drop_uses_original_slot_even_when_hotbar_is_full_of_armor() {
        use azalea::inventory::{ItemStack, operations::ClickType};
        use azalea::packet::game::SendGamePacketEvent;
        use azalea::protocol::packets::game::{ServerboundGamePacket, ClientboundContainerSetContent};
        use azalea_registry::builtin::ItemKind;
        for accepted in [true, false] {
            let bot = local_client();
            let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
            let output = captured.clone();
            bot.ecs.write().add_observer(move |event: azalea::ecs::observer::On<SendGamePacketEvent>| {
                output.lock().unwrap().push(event.packet.clone());
            });
            let mut state = BotState::default();
            state.gui = Arc::new(Mutex::new(GuiManager::new()));
            *state.spawned.lock().await = true;
            let book = ItemStack::new(ItemKind::EnchantedBook, 1);
            let helmet = ItemStack::new(ItemKind::DiamondHelmet, 1);
            let mut slots = vec![ItemStack::Empty; 46];
            slots[21] = book.clone();
            for slot in 36..=44 { slots[slot] = helmet.clone(); }
            state.gui.lock().await.on_set_content(0, 17, &slots, None);
            assert!(drop_one_inventory_item(&bot, &state, 21, &book).await);
            bot.ecs.write().flush();
            {
                let packets = captured.lock().unwrap();
                assert_eq!(packets.len(), 2);
                assert!(matches!(&packets[0], ServerboundGamePacket::ContainerClick(p)
                    if p.slot_num == 21 && p.button_num == 0 && p.click_type == ClickType::Throw
                    && p.container_id == 0 && p.state_id == 17));
                assert!(matches!(&packets[1], ServerboundGamePacket::ContainerClick(p)
                    if p.slot_num == 36 && p.button_num == 0 && p.click_type == ClickType::Swap
                    && p.container_id == 0 && p.state_id == 32768));
            }
            // No staging or optimistic deletion, even when the server is silent.
            assert_eq!(state.gui.lock().await.player_inventory[&21], book);
            assert_eq!(state.gui.lock().await.player_inventory[&36], helmet);
            if accepted { slots[21] = ItemStack::Empty; }
            handle_packet(&bot, &Arc::new(ClientboundGamePacket::ContainerSetContent(ClientboundContainerSetContent {
                container_id: 0, state_id: 18, items: slots, carried_item: ItemStack::Empty,
            })), &state).await;
            let remaining = state.gui.lock().await.player_inventory.values().filter(|item| **item == book).count();
            assert_eq!(armor::drop_status(1, remaining), if accepted {
                armor::DropStatus::Confirmed
            } else { armor::DropStatus::Retained });
            assert_eq!(state.gui.lock().await.player_inventory[&36], helmet);
        }
    }

    #[tokio::test]
    async fn direct_drop_accepts_completed_armor_from_its_actual_slot() {
        use azalea::inventory::ItemStack;
        use azalea_inventory::components::{DataComponentUnion, Lore};
        use azalea_registry::builtin::{DataComponentKind, ItemKind};
        for (kind, protection) in [(ItemKind::DiamondHelmet, "Protection IV"),
            (ItemKind::DiamondChestplate, "Protection IV"),
            (ItemKind::DiamondLeggings, "Blast Protection IV"),
            (ItemKind::DiamondBoots, "Blast Protection IV")] {
            let bot = local_client();
            let mut state = BotState::default();
            state.gui = Arc::new(Mutex::new(GuiManager::new()));
            *state.spawned.lock().await = true;
            let mut armor = ItemStack::new(kind, 1);
            if let ItemStack::Present(data) = &mut armor {
                let lore = Lore { lines: [protection, "Unbreaking III", "Mending"]
                    .into_iter().map(azalea::FormattedText::from).collect() };
                // Both the component key and union value are Lore.
                unsafe { data.component_patch.unchecked_insert_component(
                    DataComponentKind::Lore, Some(DataComponentUnion::from(lore)),
                ); }
            }
            state.gui.lock().await.player_inventory.insert(33, armor.clone());
            assert!(drop_one_inventory_item(&bot, &state, 33, &armor).await);
            assert_eq!(state.gui.lock().await.player_inventory[&33], armor);
        }
    }

    #[tokio::test]
    async fn direct_drop_preserves_protected_items_and_rejects_stale_sources() {
        use azalea::inventory::ItemStack;
        use azalea::packet::game::SendGamePacketEvent;
        use azalea_registry::builtin::ItemKind;
        let bot = local_client();
        let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
        let output = captured.clone();
        bot.ecs.write().add_observer(move |event: azalea::ecs::observer::On<SendGamePacketEvent>| {
            output.lock().unwrap().push(event.packet.clone());
        });
        let mut state = BotState::default();
        state.gui = Arc::new(Mutex::new(GuiManager::new()));
        *state.spawned.lock().await = true;
        for kind in [ItemKind::DiamondHelmet, ItemKind::DiamondChestplate, ItemKind::DiamondLeggings,
            ItemKind::DiamondBoots, ItemKind::Anvil, ItemKind::ExperienceBottle] {
            let item = ItemStack::new(kind, 1);
            state.gui.lock().await.player_inventory.insert(21, item.clone());
            assert!(!drop_one_inventory_item(&bot, &state, 21, &item).await);
        }
        let book = ItemStack::new(ItemKind::EnchantedBook, 1);
        assert!(!drop_one_inventory_item(&bot, &state, 21, &book).await);
        let stack = ItemStack::new(ItemKind::EnchantedBook, 2);
        state.gui.lock().await.player_inventory.insert(21, stack.clone());
        assert!(!drop_one_inventory_item(&bot, &state, 21, &stack).await);
        for slot in [-1, 0, 8, 45, 99] {
            state.gui.lock().await.player_inventory.insert(slot, book.clone());
            assert!(!drop_one_inventory_item(&bot, &state, slot, &book).await);
        }
        bot.ecs.write().flush();
        assert!(captured.lock().unwrap().is_empty());
    }

    pub(crate) fn local_client() -> Client {
        let mut world = azalea::ecs::world::World::new();
        let entity = world.spawn(azalea::entity::inventory::Inventory::default()).id();
        Client::new(entity, Arc::new(world.into()))
    }

    #[tokio::test]
    async fn inventory_updates_wake_worker_without_waiting_for_poll_timeout() {
        let state = BotState::default();
        // A packet can arrive before the worker starts waiting; retain that wakeup.
        state.inventory_updated.notify_one();
        tokio::time::timeout(
            std::time::Duration::from_millis(100),
            wait_for_inventory_update(&state, std::time::Duration::from_secs(5)),
        ).await.expect("server update should wake the worker immediately");
    }

    #[test]
    fn command_line_defaults_match_two_set_quota() {
        let args = Args::try_parse_from(["Enchanter"]).unwrap();
        let quota = WithdrawalQuota::default();
        assert_eq!(args.diamond_helmets, quota.diamond_helmets_needed);
        assert_eq!(args.diamond_chestplates, quota.diamond_chestplates_needed);
        assert_eq!(args.diamond_leggings, quota.diamond_leggings_needed);
        assert_eq!(args.diamond_boots, quota.diamond_boots_needed);
        assert_eq!(args.mending, quota.mending_needed);
        assert_eq!(args.unb3, quota.unbreaking_3_needed);
        assert_eq!(args.prot4, quota.protection_4_needed);
        assert_eq!(args.blast_prot4, quota.blast_protection_4_needed);
        assert_eq!(args.xp_stacks, quota.xp_stacks_needed);
    }
}
