pub mod stock;
pub mod armor;
pub mod auth;
pub mod enchanter;
pub mod gui;
pub mod nbt;
pub mod webhook;

use azalea::prelude::*;
use azalea::protocol::packets::game::{ClientboundGamePacket, ServerboundContainerClose};
use azalea::{Client, Event};
use clap::Parser;
use enchanter::{smooth_look, swing_arm, EnchanterManager};
use gui::{GuiManager, OrderWorkflowState, WithdrawalPhase, WithdrawalQuota};
use std::sync::{Arc, OnceLock};
use tokio::sync::{Mutex, Notify};
use tracing::{error, info, warn, Level};
use tracing_subscriber::FmtSubscriber;

static GLOBAL_QUOTA: OnceLock<WithdrawalQuota> = OnceLock::new();
static GLOBAL_ORDER_TARGET: OnceLock<String> = OnceLock::new();

#[derive(Parser, Debug)]
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
    #[arg(long, default_value_t = 1)]
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
    current_level: Arc<std::sync::atomic::AtomicU32>,
    experience_progress_milli: Arc<std::sync::atomic::AtomicU32>,
    total_experience: Arc<std::sync::atomic::AtomicU32>,
    server_anvil_cost: Arc<std::sync::atomic::AtomicU32>,
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
        let mut enchanter = EnchanterManager::new();
        enchanter.current_level = current_level.clone();
        enchanter.experience_progress_milli = experience_progress_milli.clone();
        enchanter.total_experience = total_experience.clone();
        enchanter.server_anvil_cost = server_anvil_cost.clone();

        Self {
            gui: SESSION_GUI.get_or_init(|| Arc::new(Mutex::new(gui))).clone(),
            enchanter: Arc::new(Mutex::new(enchanter)),
            spawned: Arc::new(Mutex::new(false)),
            order_sent: Arc::new(Mutex::new(false)),
            inventory_updated: Arc::new(Notify::new()),
            current_level,
            experience_progress_milli,
            total_experience,
            server_anvil_cost,
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

                // Wall-clock watchdog keeps working even while game ticks fall behind.
                let bot_wd = bot.clone();
                let state_wd = state.clone();
                tokio::spawn(async move {
                    loop {
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                        let is_spawned = *state_wd.spawned.lock().await;
                        if !is_spawned {
                            break;
                        }
                        let is_enchanting = state_wd.enchanter.lock().await.is_enchanting;
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
                    info!("Teleporting to base via /home 1...");
                    gui::send_command(&bot_clone, "/home 1");
                    bot_clone.wait_ticks(20).await; // 1 second for teleport settle

                    let current_pos = bot_clone.position();
                    info!("Position after /home 1: {:?}", current_pos);

                    let resume_drop = {
                        let gui = state_clone.gui.lock().await;
                        gui.next_drop > 0 || gui.pending_drop.is_some()
                            || gui.count_completed_max_sets(Some(&bot_clone)) > 0
                    };
                    if resume_drop && !drop_enchanted_armor_in_hopper(&bot_clone, &state_clone).await {
                        bot_clone.disconnect();
                        return;
                    }

                    // Preserve all supplies and unrelated items on login.
                    state_clone.gui.lock().await.reset_and_sync_inventory(Some(&bot_clone));

                    // Check if an anvil is ALREADY placed at base
                    let anvil_placed = {
                        let mut ench = state_clone.enchanter.lock().await;
                        ench.check_if_anvil_placed(&bot_clone).await
                    };

                    let has_anvil_in_inv = {
                        let mut gui = state_clone.gui.lock().await;
                        gui.placed_anvil_available = anvil_placed;
                        gui.sync_collected_from_inventory(Some(&bot_clone));
                        gui.collected.anvils >= 1
                    };

                    if anvil_placed {
                        info!("Anvil is already placed on the ground! Proceeding to Phase 2: Items Retrieval.");
                        let mut gui = state_clone.gui.lock().await;
                        gui.phase = WithdrawalPhase::ItemsRetrieval;
                        gui.placed_anvil_available = true;
                        gui.sync_collected_from_inventory(Some(&bot_clone));
                    } else if has_anvil_in_inv {
                        info!("Anvil found in inventory! Placing it now...");
                        let player_inv = {
                            let gui = state_clone.gui.lock().await;
                            gui.player_inventory.clone()
                        };
                        let mut ench = state_clone.enchanter.lock().await;
                        ench.place_anvil(&bot_clone, &player_inv).await;
                        if !ench.anvil_placed {
                            bot_clone.disconnect();
                            return;
                        }
                        drop(ench);
                        let mut gui = state_clone.gui.lock().await;
                        gui.phase = WithdrawalPhase::ItemsRetrieval;
                        gui.placed_anvil_available = true;
                        gui.sync_collected_from_inventory(Some(&bot_clone));
                    } else {
                        info!("No placed anvil and none in inventory. Starting Phase 1: Anvil Placement (withdrawing 1 Anvil from /order)...");
                        let mut gui = state_clone.gui.lock().await;
                        gui.phase = WithdrawalPhase::AnvilPlacement;
                    }

                    {
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

                        if gui.can_craft_god_armor(Some(&bot_clone)) {
                            let craftable_sets = gui.count_craftable_god_sets(Some(&bot_clone));
                            info!("Detected craftable materials for {craftable_sets} COMPLETE God-armor set(s) in inventory from previous session! Proceeding directly to enchanting routine...");
                            gui.phase = WithdrawalPhase::Done;
                            gui.state = OrderWorkflowState::WithdrawalComplete;
                            gui.clear_watchdog();
                            drop(gui);
                            trigger_enchanting_routine(&bot_clone, &state_clone).await;
                            return;
                        }

                        if gui.collected.is_fulfilled(&gui.quota) {
                            info!("All items already fulfilled in inventory; proceeding directly to enchanting!");
                            gui.phase = WithdrawalPhase::Done;
                            gui.state = OrderWorkflowState::WithdrawalComplete;
                            gui.clear_watchdog();
                            drop(gui);
                            trigger_enchanting_routine(&bot_clone, &state_clone).await;
                            return;
                        }
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
                info!("[Chat] {}", message);
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
                let mut gui = state.gui.lock().await;
                if !gui.is_fulfilling_target && gui.state == OrderWorkflowState::WithdrawalComplete {
                    info!("Non-anvil container #{} ('{}') opened after withdrawal complete; dismissing.", p.container_id, title);
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
                    if !gui.is_fulfilling_target && gui.state == OrderWorkflowState::WithdrawalComplete {
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

async fn sync_inventory_counts(bot: &Client, state: &BotState) {
    state.gui.lock().await.sync_collected_from_inventory(Some(bot));
}

/// Resume order withdrawal when a broken/missing anvil has no inventory replacement.
async fn restock_anvil(bot: &Client, state: &BotState) {
    {
        let mut ench = state.enchanter.lock().await;
        if let Some(id) = ench.anvil_container_id { ench.close_anvil(bot, id); }
        ench.is_enchanting = false;
    }
    let mut gui = state.gui.lock().await;
    gui.placed_anvil_available = false;
    gui.phase = WithdrawalPhase::AnvilPlacement;
    gui.reset_and_sync_inventory(Some(bot));
    gui.state = OrderWorkflowState::WaitingForNextOrder;
    gui.action_in_progress = false;
    info!("Anvil broke or is missing and inventory has no replacement; withdrawing from /order.");
    gui.prepare_to_send_command(bot, "/order");
}

async fn pump_gui_actions(bot_clone: Client, state_clone: BotState, expected: Option<(i32, u64)>) {
    let mut g = state_clone.gui.lock().await;
    if expected.is_some_and(|(id, epoch)| g.current_container_id != id || g.gui_action_epoch != epoch) {
        return;
    }
    if g.current_container_id == 0 || g.current_slots.is_empty() || g.awaiting_response { return; }
    let was_selling = g.state == OrderWorkflowState::SellingInventory;
    let done = g.process_gui_actions(&bot_clone).await;
    if was_selling { return; }

    // Check if Phase 1 (AnvilPlacement) completed
    if g.phase == WithdrawalPhase::AnvilPlacement && g.collected.anvils >= 1 {
        g.phase = WithdrawalPhase::ItemsRetrieval;
        g.state = OrderWorkflowState::WaitingForNextOrder;
        info!("Phase 1 completed: 1 Anvil withdrawn into inventory!");
        g.close_current_gui(&bot_clone);
        let player_inv = g.player_inventory.clone();
        drop(g);

        bot_clone.wait_ticks(2).await;
        info!("Placing Anvil on ground adjacent to bot...");
        {
            let mut ench = state_clone.enchanter.lock().await;
            ench.place_anvil(&bot_clone, &player_inv).await;
            if !ench.anvil_placed {
                bot_clone.disconnect();
                return;
            }
        }
        {
            let mut gui = state_clone.gui.lock().await;
            gui.placed_anvil_available = true;
            gui.sync_collected_from_inventory(Some(&bot_clone));
        }

        bot_clone.wait_ticks(3).await;
        info!("Keeping remaining anvils and supplies for future batches.");
        sync_inventory_counts(&bot_clone, &state_clone).await;

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
    let mut ench = state.enchanter.lock().await;
    if ench.is_enchanting {
        return;
    }
    ench.is_enchanting = true;
    {
        let mut gui = state.gui.lock().await;
        ench.player_inventory = gui.player_inventory.clone();
        gui.close_current_gui(bot);
        gui.state = OrderWorkflowState::WithdrawalComplete;
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
                    if anvil_open_attempts >= 2 {
                        error!("No GUI available: Anvil failed to open after 2 attempts! Instant rejoining server...");
                        bot_ench.disconnect();
                        return;
                    }

                    let current_inv = {
                        let ench = state_clone.enchanter.lock().await;
                        ench.player_inventory.clone()
                    };

                    let mut ench = state_clone.enchanter.lock().await;
                    if !ench.check_if_anvil_placed(&bot_ench).await {
                        let has_replacement = current_inv.iter().any(|(slot, item)|
                            (9..=44).contains(slot) && nbt::inspect_item_with_bot(item, Some(&bot_ench))
                                .is_some_and(|info| nbt::is_anvil(&info)));
                        if !has_replacement {
                            drop(ench);
                            restock_anvil(&bot_ench, &state_clone).await;
                            return;
                        }
                        info!("Replacing broken or missing anvil from inventory...");
                        ench.place_anvil(&bot_ench, &current_inv).await;
                        if !ench.anvil_placed {
                            drop(ench);
                            bot_ench.disconnect();
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
                if ench.failure_limit_reached() {
                    bot_ench.disconnect();
                    return;
                }
                if finished {
                    info!("Anvil combining phase finished successfully!");
                    break;
                }
                if ench.restock_needed { break; }
                let xp_target = ench.xp_target.take();
                drop(ench);
                if let Some(target) = xp_target {
                    // The worker shares XP atomics, while packet handlers retain access
                    // to the main enchanter inventory and can process XP updates promptly.
                    let mut worker = EnchanterManager::new();
                    worker.player_inventory = state_clone.enchanter.lock().await.player_inventory.clone();
                    worker.current_level = state_clone.current_level.clone();
                    worker.experience_progress_milli = state_clone.experience_progress_milli.clone();
                    worker.total_experience = state_clone.total_experience.clone();
                    worker.throw_exact_xp_bottles(&bot_ench, target).await;
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
            {
                let mut ench = state_clone.enchanter.lock().await;
                if let Some(id) = ench.anvil_container_id {
                    ench.close_anvil(&bot_ench, id);
                }
                let mut gui = state_clone.gui.lock().await;
                gui.player_inventory = ench.player_inventory.clone();
                gui.sync_collected_from_inventory(Some(&bot_ench));
            }
            bot_ench.wait_ticks(2).await;

            // 6. Drop enchanted armor into the nearest hopper within 2 blocks
            let ready = state_clone.enchanter.lock().await.check_all_armor_enchanted(&bot_ench).0;
            if ready && !drop_enchanted_armor_in_hopper(&bot_ench, &state_clone).await {
                error!("Drop not confirmed; retaining current set for recovery.");
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
                    ench.player_inventory = inventory;
                    info!("Starting the next stocked set immediately; no order withdrawal or inventory cleaning needed.");
                    continue 'sets;
                }
            }
            break 'sets;
        }

        // 7. Reset state and check /order -> Your Orders for next batch of armors
        info!("Reconciling inventory before the next withdrawal...");
        let placed_anvil = {
            let mut ench = state_clone.enchanter.lock().await;
            ench.reset_for_next_batch();
            ench.check_if_anvil_placed(&bot_ench).await
        };
        {
            let mut gui = state_clone.gui.lock().await;
            gui.placed_anvil_available = placed_anvil;
            gui.reset_and_sync_inventory(Some(&bot_ench));
            gui.phase = if placed_anvil { WithdrawalPhase::ItemsRetrieval }
                else { WithdrawalPhase::AnvilPlacement };
            gui.state = OrderWorkflowState::WaitingForNextOrder;
        }

        bot_ench.wait_ticks(3).await;
        info!("Opening /order to check remaining armors in 'Your Orders'...");
        {
            let mut gui = state_clone.gui.lock().await;
            gui.prepare_to_send_command(&bot_ench, "/order");
        }
    });
}

/// Drop exactly one set in armor order, using server inventory updates as acknowledgement.
/// The cursor and pending count survive automatic reconnects in SESSION_GUI.
async fn drop_enchanted_armor_in_hopper(bot: &Client, state: &BotState) -> bool {
    use azalea::inventory::operations::ClickType;
    use azalea::protocol::packets::game::s_container_click::{HashedStack, ServerboundContainerClick};

    {
        let mut gui = state.gui.lock().await;
        gui.close_current_gui(bot);
        gui.state = OrderWorkflowState::WithdrawalComplete;
    }

    let Some(hopper_pos) = EnchanterManager::find_nearby_hopper(bot) else {
        error!("No hopper within 2 blocks. Keeping this set until a hopper is available.");
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
            warn!("Remaining set is incomplete; refusing to skip {}.", armor::TYPES[kind]);
            return false;
        };
        let slot = plan[0];
        let before = count_type(kind);
        gui.pending_drop = Some((kind, before));
        // Throw directly from the inventory slot. Hotbar swaps would invalidate
        // later slots and can drop a different item when a swap is rejected.
        bot.write_packet(ServerboundContainerClick {
            container_id: 0,
            state_id: gui.player_state_id,
            slot_num: slot,
            button_num: 0, // exactly one item
            click_type: ClickType::Throw,
            changed_slots: Default::default(),
            carried_item: HashedStack(None),
        });
        drop(gui);
        swing_arm(bot);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
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
                warn!("{} drop was not acknowledged. Retaining pending drop for reconnect reconciliation.", armor::TYPES[kind]);
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
    sync_inventory_counts(&bot_ench, &state_clone).await;
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
    dotenvy::dotenv().ok();

    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::INFO)
        .with_ansi(false)
        .finish();
    tracing::subscriber::set_global_default(subscriber)
        .expect("Failed to set tracing subscriber");

    let args = Args::parse();

    info!("Starting Minecraft Enchanter Bot...");
    info!("Target Server: {}:{}", args.server, args.port);

    // Discover all accounts from CLI flags and .env (supporting multi-account)
    let discovered_accounts = auth::discover_accounts(
        args.token.as_deref(),
        args.microsoft.as_deref(),
        args.offline.as_deref(),
    )?;

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
        let mut children = Vec::new();

        for (i, acc) in discovered_accounts.iter().enumerate() {
            info!("Launching account [{i}] ({}) in dedicated window...", acc.description());

            let mut cmd = std::process::Command::new(&current_exe);
            cmd.arg("--account").arg(i.to_string());
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

    let selected_account = auth::select_account(&discovered_accounts, args.account.as_deref())?;
    info!("Selected account: {}", selected_account.description());

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
        ClientBuilder::new()
            .set_handler(handle)
            .start(account.clone(), address.clone())
            .await;

        warn!("Connection closed or reset by server. Instant rejoining server...");
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    }
}


#[cfg(test)]
mod workflow_tests {
    use super::*;

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
