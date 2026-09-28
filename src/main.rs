pub mod stock;
pub mod armor;
pub mod auth;
pub mod enchanter;
pub mod gui;
pub mod nbt;
pub mod webhook;
mod xp;

use azalea::app::PluginGroup;
use azalea::prelude::*;
use azalea::protocol::packets::game::{ClientboundGamePacket, ServerboundContainerClose};
use azalea::{BlockPos, Client, Event};
use clap::Parser;
use enchanter::{smooth_look, swing_arm, EnchanterManager};
use gui::{GuiManager, OrderWorkflowState, WithdrawalPhase, WithdrawalQuota};
use nbt::{inspect_item_with_bot, is_anvil};
use std::sync::{Arc, OnceLock};
use tokio::sync::{Mutex, Notify};
use tracing::{error, info, warn, Level};
use tracing_subscriber::FmtSubscriber;

static GLOBAL_QUOTA: OnceLock<WithdrawalQuota> = OnceLock::new();
static GLOBAL_ORDER_TARGET: OnceLock<String> = OnceLock::new();

#[derive(Parser)]
#[command(author, version, about = "Minecraft Auto-Enchanter Bot with Token Support & GUI Automation", long_about = None)]
struct Args {
    /// Server address to connect to (e.g., donutsmp.net)
    #[arg(short, long, default_value = "donutsmp.net")]
    server: String,

    /// Server port
    #[arg(short, long, default_value_t = 25565)]
    port: u16,

    /// Minecraft JWT or Bearer access token for online-mode authentication
    #[arg(short, long, env = "MC_TOKEN", hide_env_values = true)]
    token: Option<String>,

    /// Offline username; takes priority over other credentials unless an account is selected
    #[arg(long)]
    offline: Option<String>,

    /// Microsoft email for interactive browser authentication
    #[arg(long)]
    microsoft: Option<String>,

    /// Zero-based account index or configured email/name (or "all")
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
}

#[derive(Clone, Component)]
struct BotState {
    gui: Arc<Mutex<GuiManager>>,
    enchanter: Arc<Mutex<EnchanterManager>>,
    spawned: Arc<Mutex<bool>>,
    order_sent: Arc<Mutex<bool>>,
    inventory_updated: Arc<Notify>,
    xp_inventory: Arc<Mutex<xp::ServerInventory>>,
    experience_updated: Arc<Notify>,
    current_level: Arc<std::sync::atomic::AtomicU32>,
    experience_progress_milli: Arc<std::sync::atomic::AtomicU32>,
    total_experience: Arc<std::sync::atomic::AtomicU32>,
    server_anvil_cost: Arc<std::sync::atomic::AtomicU32>,
    maintenance_active: Arc<std::sync::atomic::AtomicBool>,
    is_enchanting: Arc<std::sync::atomic::AtomicBool>,
    verified_base_hopper: Arc<Mutex<Option<BlockPos>>>,
}

// Each account runs in its own process. Preserve transactions across reconnects.
static SESSION_GUI: OnceLock<Arc<Mutex<GuiManager>>> = OnceLock::new();
static SESSION_BASE_HOPPER: OnceLock<Arc<Mutex<Option<BlockPos>>>> = OnceLock::new();
static FATAL_BASE_CONFIG: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static EXPLICIT_BASE_HOPPER: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn configured_base_hopper() -> anyhow::Result<Option<BlockPos>> {
    fn read(name: &str) -> anyhow::Result<Option<String>> {
        match std::env::var(name) {
            Ok(value) => Ok(Some(value)),
            Err(std::env::VarError::NotPresent) => Ok(None),
            Err(std::env::VarError::NotUnicode(_)) => anyhow::bail!("{name} must contain valid Unicode"),
        }
    }
    parse_base_hopper_coordinates([
        read("BASE_HOPPER_X")?,
        read("BASE_HOPPER_Y")?,
        read("BASE_HOPPER_Z")?,
    ])
}

fn parse_base_hopper_coordinates(coordinates: [Option<String>; 3]) -> anyhow::Result<Option<BlockPos>> {
    if coordinates.iter().all(Option::is_none) {
        return Ok(None);
    }
    if coordinates.iter().any(Option::is_none) {
        anyhow::bail!("Set all of BASE_HOPPER_X, BASE_HOPPER_Y, and BASE_HOPPER_Z together");
    }
    let parse = |index: usize| -> anyhow::Result<i32> {
        coordinates[index].as_deref().unwrap().trim().parse::<i32>()
            .map_err(|_| anyhow::anyhow!("{} must be an integer", ["BASE_HOPPER_X", "BASE_HOPPER_Y", "BASE_HOPPER_Z"][index]))
    };
    Ok(Some(BlockPos::new(parse(0)?, parse(1)?, parse(2)?)))
}

fn verified_hopper_in_reach(bot: &Client, anchor: &BlockPos) -> bool {
    let pos = bot.position();
    let dx = anchor.x as f64 + 0.5 - pos.x;
    let dy = anchor.y as f64 + 0.5 - pos.y;
    let dz = anchor.z as f64 + 0.5 - pos.z;
    if (dx * dx + dy * dy + dz * dz).sqrt() > 2.85 {
        return false;
    }
    let world_handle = bot.world();
    let world = world_handle.read();
    world.get_block_state(anchor.clone())
        .map(|state| format!("{state:?}").to_ascii_lowercase().contains("hopper"))
        .unwrap_or(false)
}

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
            xp_inventory: Arc::new(Mutex::new(xp::ServerInventory::default())),
            experience_updated,
            current_level,
            experience_progress_milli,
            total_experience,
            server_anvil_cost,
            maintenance_active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            is_enchanting: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            verified_base_hopper: SESSION_BASE_HOPPER
                .get_or_init(|| Arc::new(Mutex::new(None))).clone(),
        }
    }
}

async fn handle(bot: Client, event: Event, state: BotState) -> Result<(), anyhow::Error> {
    match event {
        Event::Init => {
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

                // Spawn watchdog loop to monitor GUI responsiveness every 10 ticks (~500ms)
                let bot_wd = bot.clone();
                let state_wd = state.clone();
                tokio::spawn(async move {
                    loop {
                        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
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

                    let current_pos = bot_clone.position();
                    info!("Bot position on spawn: {:?}", current_pos);

                    let known_hopper = state_clone.verified_base_hopper.lock().await.clone();
                    let base_hopper = if let Some(ref anchor) = known_hopper {
                        verified_hopper_in_reach(&bot_clone, anchor).then(|| anchor.clone())
                    } else {
                        EnchanterManager::find_nearby_hopper(&bot_clone)
                    };

                    if let Some(hopper_pos) = base_hopper {
                        info!("Verified base hopper at {:?}", hopper_pos);
                        if known_hopper.is_none() {
                            *state_clone.verified_base_hopper.lock().await = Some(hopper_pos);
                        }
                    } else {
                        info!("No nearby hopper found; operating from current position.");
                    }

                    let resume_drop = {
                        let gui = state_clone.gui.lock().await;
                        gui.next_drop > 0 || gui.pending_drop.is_some()
                            || gui.count_completed_max_sets(Some(&bot_clone)) > 0
                    };
                    if resume_drop && !deposit_enchanted_armor_in_hopper(&bot_clone, &state_clone).await {
                        bot_clone.disconnect();
                        return;
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

                    let (anvil_count, required_anvils) = {
                        let gui = state_clone.gui.lock().await;
                        let count = gui.player_inventory.values().filter_map(|item| {
                            inspect_item_with_bot(item, Some(&bot_clone))
                                .filter(|info| is_anvil(info))
                                .map(|info| info.count as u32)
                        }).sum::<u32>();
                        (count, gui.quota.anvils_needed)
                    };

                    info!("Initial base check: placed anvil = {anvil_placed}, inventory anvil count = {anvil_count}, quota = {required_anvils}");

                    if anvil_count < required_anvils {
                        info!("Bot has {anvil_count}/{required_anvils} anvil(s). Starting Phase 1: Anvil Placement (looking down at feet & withdrawing Anvil from /order)...");
                        let mut gui = state_clone.gui.lock().await;
                        gui.phase = WithdrawalPhase::AnvilPlacement;
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
                        let anvil_ready = {
                            let mut ench = state_clone.enchanter.lock().await;
                            ench.check_if_anvil_placed(&bot_clone).await || anvil_count >= 1
                        };

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
                gui.hopper_active = false;
                gui.hopper_container_id = None;
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

            {
                let mut gui = state.gui.lock().await;
                if gui.hopper_active {
                    if p.menu_type == azalea_registry::builtin::MenuKind::Hopper {
                        gui.on_open_screen(p.container_id, &title);
                        gui.hopper_container_id = Some(p.container_id);
                    } else {
                        bot.write_packet(ServerboundContainerClose { container_id: p.container_id });
                    }
                    state.inventory_updated.notify_one();
                    return;
                }
            }

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

                if p.container_id > 0 && !gui.hopper_active {
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
                container_id > 0 && container_id == gui.current_container_id && !gui.awaiting_response && !gui.hopper_active
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
                if p.value > 0 {
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
                    gui.hopper_container_id = None;
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
    // Publish only after the normal caches have received the same server packet.
    state.xp_inventory.lock().await.observe(packet.as_ref());
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
    if g.hopper_active || g.current_container_id == 0 || g.current_slots.is_empty() || g.awaiting_response { return; }
    let was_selling = g.state == OrderWorkflowState::SellingInventory;
    let done = g.process_gui_actions(&bot_clone).await;
    if was_selling { return; }

    // Check if Phase 1 (AnvilPlacement) completed
    if g.phase == WithdrawalPhase::AnvilPlacement && g.anvil_withdrawal_complete() {
        g.phase = WithdrawalPhase::ItemsRetrieval;
        g.state = OrderWorkflowState::WaitingForNextOrder;
        info!("Phase 1 completed: Anvil inventory quota confirmed! Ensuring anvils in offhand...");
        g.close_current_gui(&bot_clone);
        crate::enchanter::EnchanterManager::ensure_anvils_in_offhand(&bot_clone, &mut g.player_inventory).await;
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
                warn!("Warning: Anvil placement verification failed after withdrawal! Retrying placement...");
                bot_clone.wait_ticks(10).await;
                let placed2 = {
                    let mut ench = state_clone.enchanter.lock().await;
                    ench.place_anvil(&bot_clone, &player_inv).await
                };
                if placed2 {
                    info!("Anvil verified placed successfully! Ready for Phase 2.");
                } else {
                    warn!("Anvil placement could not be verified; proceeding to items retrieval.");
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
    if state.is_enchanting.swap(true, std::sync::atomic::Ordering::SeqCst) {
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
                    warn!("Player has an anvil in inventory but placement failed. Retrying placement...");
                    bot_ench.wait_ticks(10).await;
                    let mut ench = state_clone.enchanter.lock().await;
                    let placed2 = ench.place_anvil(&bot_ench, &player_inv).await;
                    if !placed2 {
                        error!("Anvil placement still failed. Disconnecting bot to prevent loop.");
                        state_clone.is_enchanting.store(false, std::sync::atomic::Ordering::SeqCst);
                        bot_ench.disconnect();
                        return;
                    }
                    ench.is_enchanting = true;
                    state_clone.is_enchanting.store(true, std::sync::atomic::Ordering::SeqCst);
                } else {
                    state_clone.is_enchanting.store(false, std::sync::atomic::Ordering::SeqCst);
                    let mut gui = state_clone.gui.lock().await;
                    gui.phase = WithdrawalPhase::AnvilPlacement;
                    gui.state = OrderWorkflowState::Spawned;
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
                        state_clone.is_enchanting.store(false, std::sync::atomic::Ordering::SeqCst);
                        state_clone.enchanter.lock().await.is_enchanting = false;
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
                        if !*state_clone.spawned.lock().await {
                            state_clone.is_enchanting.store(false, std::sync::atomic::Ordering::SeqCst);
                            return;
                        }
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
                let discard_item = ench.cursed_book_to_discard.take();
                let discard_slot = ench.cursed_book_slot.take();
                let xp_target = ench.xp_target.take();
                drop(ench);

                if let Some(ref cursed_book) = discard_item {
                    info!("[CURSED BOOK] Discarding cursed book into hopper...");
                    let dropped = deposit_cursed_book_in_hopper(&bot_ench, &state_clone, cursed_book, discard_slot).await;
                    if dropped {
                        info!("[CURSED BOOK] Successfully discarded cursed book into hopper.");
                    } else {
                        warn!("[CURSED BOOK] Failed to confirm cursed book left inventory.");
                    }
                    // Check if another book for this armor piece is already available in inventory
                    let gui_inv = state_clone.gui.lock().await.player_inventory.clone();
                    let has_another_book = {
                        let mut ench = state_clone.enchanter.lock().await;
                        ench.player_inventory = gui_inv;
                        ench.record_book_disposal(cursed_book, dropped);
                        ench.has_available_book_for_current_armor(&bot_ench)
                    };
                    if has_another_book {
                        info!("Another book for this armor piece is available in inventory. Continuing enchanting routine...");
                        continue;
                    } else {
                        info!("No replacement book available in inventory for this armor piece. Breaking enchanting routine to restock from /order...");
                        let mut ench = state_clone.enchanter.lock().await;
                        ench.restock_needed = true;
                        break;
                    }
                }

                if let Some(target) = xp_target {
                    match xp::throw_bottles(&bot_ench, &state_clone, target).await {
                        xp::Outcome::Reached => {}
                        xp::Outcome::OutOfBottles => {
                            warn!("Server inventory has no XP bottles; returning to orders for restocking.");
                            break;
                        }
                        xp::Outcome::Unconfirmed => {
                            warn!("XP action was not confirmed; reconnecting to reconcile inventory.");
                            state_clone.is_enchanting.store(false, std::sync::atomic::Ordering::SeqCst);
                            bot_ench.disconnect();
                            return;
                        }
                    }
                }

                wait_for_inventory_update(&state_clone, std::time::Duration::from_millis(100)).await;
                if !*state_clone.spawned.lock().await {
                    state_clone.is_enchanting.store(false, std::sync::atomic::Ordering::SeqCst);
                    return;
                }
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

            // 6. Transfer enchanted armor through the nearest hopper GUI
            let ready = state_clone.enchanter.lock().await.check_all_armor_enchanted(&bot_ench).0;
            if ready && !deposit_enchanted_armor_in_hopper(&bot_ench, &state_clone).await {
                error!("Hopper transfer not confirmed; retaining current set for recovery.");
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
        state_clone.is_enchanting.store(false, std::sync::atomic::Ordering::SeqCst);
        {
            let mut ench = state_clone.enchanter.lock().await;
            ench.reset_for_next_batch();
        }
        let (anvil_count, required_anvils) = {
            let mut gui = state_clone.gui.lock().await;
            gui.reset_and_sync_inventory(Some(&bot_ench));
            crate::enchanter::EnchanterManager::ensure_anvils_in_offhand(&bot_ench, &mut gui.player_inventory).await;
            (gui.collected.anvils, gui.quota.anvils_needed)
        };

        if anvil_count < required_anvils {
            info!("Bot has {anvil_count}/{required_anvils} anvil(s). Looking completely down at feet (pitch: 90.0) and running /order...");
            crate::enchanter::smooth_look(&bot_ench, bot_ench.direction().y_rot(), 90.0).await;
            bot_ench.set_direction(bot_ench.direction().y_rot(), 90.0);
            bot_ench.wait_ticks(2).await;

            let mut gui = state_clone.gui.lock().await;
            gui.phase = WithdrawalPhase::AnvilPlacement;
            gui.state = OrderWorkflowState::Spawned;
            gui.prepare_to_send_command(&bot_ench, "/order");
        } else {
            let mut gui = state_clone.gui.lock().await;
            gui.phase = WithdrawalPhase::ItemsRetrieval;
            gui.state = OrderWorkflowState::Spawned;
            bot_ench.wait_ticks(3).await;
            info!("Opening /order to check remaining armors in 'Your Orders'...");
            gui.prepare_to_send_command(&bot_ench, "/order");
        }
    });
}

/// Discard only the exact cursed book rejected by the anvil, after the server returns it to inventory.
/// Open a real hopper menu and wait for its full server inventory snapshot.
async fn open_hopper(bot: &Client, state: &BotState) -> bool {
    use azalea::inventory::ItemStack;
    use azalea_registry::builtin::ItemKind;

    let Some(hopper_pos) = EnchanterManager::find_nearby_hopper(bot) else {
        error!("No hopper within 2 blocks; keeping items in inventory.");
        return false;
    };
    {
        let mut gui = state.gui.lock().await;
        gui.close_current_gui(bot);
        gui.hopper_active = true;
        // Never select XP, armor, or a placeable item to open the hopper.
        let safe_hotbar = (0..9u8).find(|h| matches!(gui.player_inventory.get(&(36 + *h as i16)), Some(ItemStack::Empty)))
            .or_else(|| (0..9u8).find(|h| matches!(gui.player_inventory.get(&(36 + *h as i16)),
                Some(ItemStack::Present(data)) if data.kind == ItemKind::EnchantedBook || data.kind == ItemKind::Book)));
        let Some(hotbar) = safe_hotbar else {
            warn!("No empty or book hotbar slot available to open the hopper safely.");
            return false;
        };
        bot.set_selected_hotbar_slot(hotbar);
    }
    let pos = bot.position();
    let dx = hopper_pos.x as f64 + 0.5 - pos.x;
    let dz = hopper_pos.z as f64 + 0.5 - pos.z;
    let dy = hopper_pos.y as f64 + 0.5 - (pos.y + 1.62);
    smooth_look(bot, (-dx).atan2(dz).to_degrees() as f32,
        (-dy).atan2((dx * dx + dz * dz).sqrt()).to_degrees() as f32).await;
    bot.wait_ticks(1).await;
    bot.block_interact(hopper_pos);
    swing_arm(bot);

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if !*state.spawned.lock().await { return false; }
        {
            let gui = state.gui.lock().await;
            if gui.hopper_container_id == Some(gui.current_container_id)
                && gui.current_container_id > 0 && gui.open_container_size == 5
                && gui.current_slots.len() == 41 {
                return true;
            }
        }
        if std::time::Instant::now() >= deadline {
            warn!("Hopper GUI did not open with valid contents; keeping items in inventory.");
            return false;
        }
        wait_for_inventory_update(state, std::time::Duration::from_millis(100)).await;
    }
}

async fn close_hopper(bot: &Client, state: &BotState) {
    let mut gui = state.gui.lock().await;
    gui.close_current_gui(bot);
    gui.hopper_active = false;
}

/// Send one shift-click only after checking both player and hopper slot namespaces.
fn transfer_hopper_item(bot: &Client, gui: &mut GuiManager, slot: i16, expected: &azalea::inventory::ItemStack) -> bool {
    use azalea::entity::inventory::Inventory;
    use azalea::inventory::{ItemStack, operations::ClickType};
    let Some(menu_slot) = gui.hopper_transfer_slot(slot, expected, Some(bot)) else {
        warn!("Hopper or source item changed; refusing an unsafe transfer from slot #{slot}.");
        return false;
    };
    if !bot.get_component::<Inventory>().is_some_and(|inv|
        inv.id == gui.current_container_id && inv.carried == ItemStack::Empty) {
        warn!("Hopper is not the active menu or the cursor holds an item; refusing transfer.");
        return false;
    }
    gui.click_slot(bot, menu_slot, ClickType::QuickMove);
    true
}

async fn deposit_cursed_book_in_hopper(
    bot: &Client,
    state: &BotState,
    expected: &azalea::inventory::ItemStack,
    preferred_slot: Option<i16>,
) -> bool {
    use azalea::inventory::ItemStack;
    use azalea_registry::builtin::ItemKind;
    if !matches!(expected, ItemStack::Present(data) if data.count == 1 && data.kind == ItemKind::EnchantedBook) {
        error!("[CURSED BOOK GUARD] Refusing to deposit anything except the rejected enchanted book.");
        return false;
    }
    let result = async {
        if !open_hopper(bot, state).await { return false; }
        let (before, slot, container_id) = {
            let mut gui = state.gui.lock().await;
            let matching: Vec<_> = (9..=44).filter(|slot| gui.player_inventory.get(slot) == Some(expected)).collect();
            let slot = preferred_slot.filter(|s| matching.contains(s)).or_else(|| matching.first().copied());
            let Some(slot) = slot else {
                warn!("Rejected book was not returned to inventory; refusing to substitute another item.");
                return false;
            };
            if !transfer_hopper_item(bot, &mut gui, slot, expected) { return false; }
            (matching.len(), slot, gui.current_container_id)
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            wait_for_inventory_update(state, std::time::Duration::from_millis(100)).await;
            if !*state.spawned.lock().await { return false; }
            let gui = state.gui.lock().await;
            if gui.hopper_container_id != Some(container_id) { return false; }
            let remaining = (9..=44).filter(|slot| gui.player_inventory.get(slot) == Some(expected)).count();
            if armor::drop_status(before, remaining) == armor::DropStatus::Confirmed
                && gui.current_slots.get(&(slot - 4)) == Some(&ItemStack::Empty) {
                info!("[CURSED BOOK] Server confirmed transfer through the hopper GUI.");
                return true;
            }
            if std::time::Instant::now() >= deadline {
                warn!("Cursed book transfer was not confirmed; hopper may be full or inaccessible.");
                return false;
            }
        }
    }.await;
    close_hopper(bot, state).await;
    result
}

/// Deposit exactly one set in armor order. Pending counts survive reconnects.
async fn deposit_enchanted_armor_in_hopper(bot: &Client, state: &BotState) -> bool {
    let result = async {
        if !open_hopper(bot, state).await { return false; }
        loop {
            if !*state.spawned.lock().await { return false; }
            let mut gui = state.gui.lock().await;
            if !(9..=44).all(|slot| gui.player_inventory.contains_key(&slot)) { return false; }
            let items: Vec<_> = gui.player_inventory.iter()
                .filter(|(slot, _)| (9..=44).contains(*slot))
                .filter_map(|(&slot, item)| crate::nbt::inspect_item_with_bot(item, Some(bot)).map(|info| (slot, info)))
                .collect();
            let count_type = |kind| items.iter().filter(|(_, info)| armor::armor_type(info) == Some(kind)
                && armor::is_complete(info)).map(|(_, info)| info.count as usize).sum::<usize>();
            if let Some((kind, before)) = gui.pending_drop {
                match armor::drop_status(before, count_type(kind)) {
                    armor::DropStatus::Confirmed => { gui.next_drop = kind + 1; gui.pending_drop = None; }
                    armor::DropStatus::Retained => { gui.pending_drop = None; }
                    armor::DropStatus::Ambiguous => {
                        error!("Ambiguous transfer inventory change; retaining the current sequence.");
                        return false;
                    }
                }
            }
            if gui.next_drop == 4 {
                gui.next_drop = 0;
                info!("Server confirmed all four hopper transfers: helmet, chestplate, leggings, boots.");
                return true;
            }
            let kind = gui.next_drop;
            let Some(plan) = armor::drop_plan(&items, kind) else {
                warn!("Remaining set is incomplete; refusing to skip {}.", armor::TYPES[kind]);
                return false;
            };
            let slot = plan[0];
            let expected = gui.player_inventory[&slot].clone();
            let before = count_type(kind);
            let container_id = gui.current_container_id;
            if !transfer_hopper_item(bot, &mut gui, slot, &expected) { return false; }
            gui.pending_drop = Some((kind, before));
            info!("Transferring completed {} through hopper GUI from player slot #{slot}.", armor::TYPES[kind]);
            drop(gui);
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                wait_for_inventory_update(state, std::time::Duration::from_millis(100)).await;
                if !*state.spawned.lock().await { return false; }
                let mut gui = state.gui.lock().await;
                if gui.hopper_container_id != Some(container_id) { return false; }
                let remaining: usize = gui.player_inventory.iter()
                    .filter(|(slot, _)| (9..=44).contains(*slot))
                    .filter_map(|(_, item)| crate::nbt::inspect_item_with_bot(item, Some(bot)))
                    .filter(|info| armor::armor_type(info) == Some(kind) && armor::is_complete(info))
                    .map(|info| info.count as usize).sum();
                if armor::drop_status(before, remaining) == armor::DropStatus::Confirmed
                    && gui.current_slots.get(&(slot - 4)) == Some(&azalea::inventory::ItemStack::Empty) {
                    gui.next_drop = kind + 1;
                    gui.pending_drop = None;
                    break;
                }
                if std::time::Instant::now() >= deadline {
                    warn!("{} hopper transfer was not acknowledged (full or inaccessible hopper); retaining pending transfer for reconnect reconciliation.", armor::TYPES[kind]);
                    return false;
                }
            }
        }
    }.await;
    close_hopper(bot, state).await;
    result
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

    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::INFO)
        .finish();
    tracing::subscriber::set_global_default(subscriber)
        .expect("Failed to set tracing subscriber");

    let args = Args::parse();
    let base_hopper = configured_base_hopper()?;
    EXPLICIT_BASE_HOPPER.store(base_hopper.is_some(), std::sync::atomic::Ordering::SeqCst);
    SESSION_BASE_HOPPER.get_or_init(|| Arc::new(Mutex::new(base_hopper)));

    info!("Starting Minecraft Enchanter Bot...");
    info!("Target Server: {}:{}", args.server, args.port);

    if let Some(ref path) = env_file_loaded {
        info!("Loaded environment configuration from: {}", path.display());
    } else {
        warn!("⚠️ No '.env' file found in current directory or next to Enchanter.exe!");
        warn!("⚠️ The bot will default to offline mode unless credentials are provided via CLI arguments.");
    }

    // Children use the exact parent-selected account, independent of inherited ACCOUNT=all
    // or a different account discovery order in the executable's working directory.
    let child_account = auth::child_account_from_env()?;
    let discovered_accounts = if let Some(ref account) = child_account {
        vec![account.clone()]
    } else {
        auth::discover_accounts(args.token.as_deref(), args.microsoft.as_deref(), args.offline.as_deref())
    };

    info!("Discovered {} configured account(s):", discovered_accounts.len());
    for (i, acc) in discovered_accounts.iter().enumerate() {
        info!("  [{i}] {}", acc.description());
    }

    let run_all = child_account.is_none()
        && (args.all || args.account.as_deref().map(|s| s.eq_ignore_ascii_case("all")).unwrap_or(false));

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
            let (kind, value) = acc.child_env_parts();
            cmd.env(auth::CHILD_ACCOUNT_KIND_ENV, kind);
            cmd.env(auth::CHILD_ACCOUNT_VALUE_ENV, value);
            cmd.arg("--server").arg(&args.server);
            cmd.arg("--port").arg(args.port.to_string());
            cmd.arg("--order-target").arg(&args.order_target);
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

    let selected_account = if let Some(account) = child_account {
        account
    } else {
        // "all" with one configured account still selects that account.
        let selector = if run_all { None } else { args.account.as_deref() };
        auth::select_account(&discovered_accounts, selector)?
    };
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

    info!("Order fulfillment target username set to: '{}'", args.order_target);

    let address = format!("{}:{}", args.server, args.port);

    loop {
        info!("Connecting to {} as '{}'...", address, account.username());
        ClientBuilder::new_without_plugins()
            .add_plugins(
                azalea::DefaultPlugins
                    .build()
                    .disable::<azalea::chat_signing::ChatSigningPlugin>(),
            )
            .add_plugins(azalea::bot::DefaultBotPlugins)
            .set_handler(handle)
            .start(account.clone(), address.clone())
            .await;

        if FATAL_BASE_CONFIG.load(std::sync::atomic::Ordering::SeqCst) {
            anyhow::bail!("Base hopper could not be verified. Check configured BASE_HOPPER_X/Y/Z coordinates and the /home 1 destination, or set all three coordinates if the bot starts at its base hopper.");
        }

        warn!("Connection closed or reset by server. Waiting 12 seconds before reconnecting to let proxy cache clear...");
        tokio::time::sleep(tokio::time::Duration::from_secs(12)).await;
    }
}


#[cfg(test)]
mod workflow_tests {
    use super::*;

    #[test]
    fn base_hopper_coordinates_must_be_complete_and_numeric() {
        assert!(parse_base_hopper_coordinates([None, None, None]).unwrap().is_none());
        assert!(parse_base_hopper_coordinates([Some("1".into()), None, Some("3".into())]).is_err());
        assert!(parse_base_hopper_coordinates([Some("1".into()), Some("bad".into()), Some("3".into())]).is_err());
        let pos = parse_base_hopper_coordinates([
            Some("1".into()), Some("-64".into()), Some("3".into())
        ]).unwrap().unwrap();
        assert_eq!((pos.x, pos.y, pos.z), (1, -64, 3));
    }

    #[tokio::test]
    async fn hopper_packets_stay_open_during_enchanting_and_use_quick_move() {
        use azalea::entity::inventory::Inventory;
        use azalea::inventory::{ItemStack, operations::ClickType};
        use azalea::protocol::packets::game::{ClientboundOpenScreen, ClientboundContainerSetContent,
            ClientboundContainerSetSlot, ClientboundContainerClose, ServerboundGamePacket};
        use azalea_registry::builtin::{ItemKind, MenuKind};
        let packets = Arc::new(std::sync::Mutex::new(Vec::new()));
        let observed = packets.clone();
        let mut app = azalea::app::App::new();
        app.add_plugins(azalea::inventory::InventoryPlugin);
        app.world_mut().add_observer(move |event: azalea::ecs::prelude::On<azalea::packet::game::SendGamePacketEvent>| {
            observed.lock().unwrap().push(event.packet.clone());
        });
        let entity = app.world_mut().spawn(Inventory { id: 7, ..Default::default() }).id();
        let world = std::mem::take(app.world_mut());
        let bot = Client::new(entity, Arc::new(world.into()));
        let state = BotState { gui: Arc::new(Mutex::new(GuiManager::new())), ..Default::default() };
        state.is_enchanting.store(true, std::sync::atomic::Ordering::SeqCst);
        {
            let mut gui = state.gui.lock().await;
            gui.hopper_active = true;
            gui.state = OrderWorkflowState::WithdrawalComplete;
        }
        handle_packet(&bot, &Arc::new(ClientboundGamePacket::OpenScreen(ClientboundOpenScreen {
            container_id: 7, menu_type: MenuKind::Hopper, title: "Custom name".into(),
        })), &state).await;
        let book = ItemStack::new(ItemKind::EnchantedBook, 1);
        let xp = ItemStack::new(ItemKind::ExperienceBottle, 64);
        let mut items = vec![ItemStack::Empty; 41];
        items[5] = book.clone();
        items[32] = xp.clone();
        handle_packet(&bot, &Arc::new(ClientboundGamePacket::ContainerSetContent(ClientboundContainerSetContent {
            container_id: 7, state_id: 12, items, carried_item: ItemStack::Empty,
        })), &state).await;
        {
            let mut gui = state.gui.lock().await;
            assert_eq!(gui.hopper_container_id, Some(7));
            assert!(!transfer_hopper_item(&bot, &mut gui, 36, &xp));
            assert!(transfer_hopper_item(&bot, &mut gui, 9, &book));
            // A click alone cannot claim success, including when the hopper is full.
            assert_eq!(gui.player_inventory.get(&9), Some(&book));
            assert_eq!(gui.current_slots.get(&5), Some(&book));
        }
        bot.ecs.write().flush();
        {
            let packets = packets.lock().unwrap();
            assert_eq!(packets.len(), 1);
            let ServerboundGamePacket::ContainerClick(click) = &packets[0] else { panic!("expected a container click"); };
            assert_eq!(click.container_id, 7);
            assert_eq!(click.state_id, 12);
            assert_eq!(click.slot_num, 5);
            assert_eq!(click.click_type, ClickType::QuickMove);
        }
        handle_packet(&bot, &Arc::new(ClientboundGamePacket::ContainerSetSlot(ClientboundContainerSetSlot {
            container_id: 7, state_id: 13, slot: 5, item_stack: ItemStack::Empty,
        })), &state).await;
        {
            let gui = state.gui.lock().await;
            assert_eq!(gui.player_inventory.get(&9), Some(&ItemStack::Empty));
            assert_eq!(gui.player_inventory.get(&36), Some(&xp));
        }
        close_hopper(&bot, &state).await;
        bot.ecs.write().flush();
        assert_eq!(bot.component::<Inventory>().id, 0);
        assert!(!state.gui.lock().await.hopper_active);

        // A menu merely named Hopper must not be accepted as a real hopper.
        state.gui.lock().await.hopper_active = true;
        handle_packet(&bot, &Arc::new(ClientboundGamePacket::OpenScreen(ClientboundOpenScreen {
            container_id: 8, menu_type: MenuKind::Generic9x3, title: "Hopper".into(),
        })), &state).await;
        assert_eq!(state.gui.lock().await.hopper_container_id, None);
        handle_packet(&bot, &Arc::new(ClientboundGamePacket::ContainerClose(ClientboundContainerClose {
            container_id: 8,
        })), &state).await;
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
