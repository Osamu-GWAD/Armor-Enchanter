pub mod auth;
pub mod enchanter;
pub mod gui;
pub mod nbt;

use auth::CustomTokenAccount;
use azalea::account::Account;
use azalea::prelude::*;
use azalea::protocol::packets::game::{ClientboundGamePacket, ServerboundContainerClose};
use azalea::{Client, Event};
use clap::Parser;
use enchanter::EnchanterManager;
use gui::{GuiManager, OrderWorkflowState, WithdrawalPhase, WithdrawalQuota};
use std::sync::{Arc, OnceLock};
use tokio::sync::Mutex;
use tracing::{error, info, warn, Level};
use tracing_subscriber::FmtSubscriber;

static GLOBAL_QUOTA: OnceLock<WithdrawalQuota> = OnceLock::new();

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
    #[arg(short, long, env = "MC_TOKEN")]
    token: Option<String>,

    /// Username for offline-mode authentication (if no token is provided)
    #[arg(long)]
    offline: Option<String>,

    /// Microsoft email for interactive browser authentication
    #[arg(long)]
    microsoft: Option<String>,

    /// Number of Mending books to withdraw from /order
    #[arg(long, default_value_t = 4)]
    mending: u32,

    /// Number of Unbreaking III books to withdraw from /order
    #[arg(long, default_value_t = 4)]
    unb3: u32,

    /// Number of Protection IV books to withdraw from /order
    #[arg(long, default_value_t = 2)]
    prot4: u32,

    /// Number of Blast Protection IV books to withdraw from /order
    #[arg(long, default_value_t = 2)]
    blast_prot4: u32,

    /// Number of XP bottle stacks (up to 64 each) to withdraw
    #[arg(long, default_value_t = 3)]
    xp_stacks: u32,

    /// Number of anvils to withdraw
    #[arg(long, default_value_t = 1)]
    anvils: u32,

    /// Number of Diamond Helmets to withdraw
    #[arg(long, default_value_t = 1)]
    diamond_helmets: u32,

    /// Number of Diamond Chestplates to withdraw
    #[arg(long, default_value_t = 1)]
    diamond_chestplates: u32,

    /// Number of Diamond Leggings to withdraw
    #[arg(long, default_value_t = 1)]
    diamond_leggings: u32,

    /// Number of Diamond Boots to withdraw
    #[arg(long, default_value_t = 1)]
    diamond_boots: u32,
}

#[derive(Clone, Component)]
struct BotState {
    gui: Arc<Mutex<GuiManager>>,
    enchanter: Arc<Mutex<EnchanterManager>>,
    spawned: Arc<Mutex<bool>>,
    order_sent: Arc<Mutex<bool>>,
    current_level: Arc<std::sync::atomic::AtomicU32>,
    total_experience: Arc<std::sync::atomic::AtomicU32>,
    server_anvil_cost: Arc<std::sync::atomic::AtomicU32>,
}

impl Default for BotState {
    fn default() -> Self {
        let mut gui = GuiManager::new();
        if let Some(quota) = GLOBAL_QUOTA.get() {
            gui.quota = quota.clone();
        }
        let current_level = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let total_experience = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let server_anvil_cost = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let mut enchanter = EnchanterManager::new();
        enchanter.current_level = current_level.clone();
        enchanter.total_experience = total_experience.clone();
        enchanter.server_anvil_cost = server_anvil_cost.clone();

        Self {
            gui: Arc::new(Mutex::new(gui)),
            enchanter: Arc::new(Mutex::new(enchanter)),
            spawned: Arc::new(Mutex::new(false)),
            order_sent: Arc::new(Mutex::new(false)),
            current_level,
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

                tokio::spawn(async move {
                    // Wait 12 seconds (240 ticks) for chunk loading and DonutSMP spawn command cooldown
                    info!("Waiting 12 seconds (240 ticks) for server spawn cooldown before sending commands...");
                    bot_clone.wait_ticks(240).await;

                    // Teleport to player base using /home 1 so block interactions (anvil) are in non-protected territory
                    let initial_pos = bot_clone.position();
                    info!("Initial spawn position: {:?}", initial_pos);
                    info!("Teleporting to base via /home 1...");
                    bot_clone.chat("/home 1");
                    bot_clone.wait_ticks(160).await; // 8 seconds for teleport settle

                    let current_pos = bot_clone.position();
                    info!("Position after /home 1: {:?}", current_pos);

                    // If inventory has excess XP bottles beyond quota (3 stacks = 192 bottles), splash them at feet to gain levels & free up space
                    {
                        let mut gui = state_clone.gui.lock().await;
                        gui.reset_and_sync_inventory(Some(&bot_clone));
                        gui.free_space_by_splashing_excess_xp(&bot_clone).await;
                    }

                    // Check if an anvil is ALREADY placed at base
                    let anvil_placed = {
                        let mut ench = state_clone.enchanter.lock().await;
                        ench.check_if_anvil_placed(&bot_clone).await
                    };

                    let has_anvil_in_inv = {
                        let mut gui = state_clone.gui.lock().await;
                        gui.sync_collected_from_inventory(Some(&bot_clone));
                        gui.collected.anvils >= 1
                    };

                    if anvil_placed {
                        info!("Anvil is already placed on the ground! Proceeding to Phase 2: Items Retrieval.");
                        let mut gui = state_clone.gui.lock().await;
                        gui.phase = WithdrawalPhase::ItemsRetrieval;
                        gui.collected.anvils = 1;
                    } else if has_anvil_in_inv {
                        info!("Anvil found in inventory! Placing it now...");
                        let player_inv = {
                            let gui = state_clone.gui.lock().await;
                            gui.player_inventory.clone()
                        };
                        let mut ench = state_clone.enchanter.lock().await;
                        ench.place_anvil(&bot_clone, &player_inv).await;
                        let mut gui = state_clone.gui.lock().await;
                        gui.phase = WithdrawalPhase::ItemsRetrieval;
                        gui.collected.anvils = 1;
                    } else {
                        info!("No placed anvil and none in inventory. Starting Phase 1: Anvil Placement (withdrawing 1 Anvil from /order)...");
                        let mut gui = state_clone.gui.lock().await;
                        gui.phase = WithdrawalPhase::AnvilPlacement;
                    }

                    {
                        let mut gui = state_clone.gui.lock().await;
                        if gui.collected.is_fulfilled(&gui.quota) {
                            info!("All items already fulfilled in inventory; proceeding directly to enchanting!");
                            gui.phase = WithdrawalPhase::Done;
                            gui.state = OrderWorkflowState::WithdrawalComplete;
                            drop(gui);
                            trigger_enchanting_routine(&bot_clone, &state_clone).await;
                            return;
                        }
                    }

                    info!("Running /order command...");
                    bot_clone.chat("/order");
                    *state_clone.order_sent.lock().await = true;

                    let mut gui = state_clone.gui.lock().await;
                    gui.state = OrderWorkflowState::Spawned;
                });
            }
        }
        Event::Chat(packet) => {
            let message = packet.message().to_string();
            info!("[Chat] {}", message);
        }
        Event::Packet(packet) => {
            handle_packet(&bot, &packet, &state).await;
        }
        Event::Disconnect(reason) => {
            warn!("Disconnected from server: {:?}", reason);
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
                if gui.state == OrderWorkflowState::WithdrawalComplete {
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
                drop(ench);
                let mut gui = state.gui.lock().await;
                gui.on_set_content(p.container_id, p.state_id, &p.items, Some(bot));

                if p.container_id > 0 {
                    if gui.state == OrderWorkflowState::WithdrawalComplete {
                        bot.write_packet(ServerboundContainerClose { container_id: p.container_id });
                        return;
                    }

                    let bot_clone = bot.clone();
                    let state_clone = state.clone();

                    tokio::spawn(async move {
                        bot_clone.wait_ticks(15).await;
                        let mut g = state_clone.gui.lock().await;
                        let done = g.process_gui_actions(&bot_clone).await;

                        // Check if Phase 1 (AnvilPlacement) completed
                        if g.phase == WithdrawalPhase::AnvilPlacement && g.collected.anvils >= 1 {
                            info!("Phase 1 completed: 1 Anvil withdrawn into inventory!");
                            g.close_current_gui(&bot_clone);
                            let player_inv = g.player_inventory.clone();
                            drop(g);

                            bot_clone.wait_ticks(30).await;
                            info!("Placing Anvil on ground adjacent to bot...");
                            {
                                let mut ench = state_clone.enchanter.lock().await;
                                ench.place_anvil(&bot_clone, &player_inv).await;
                            }

                            bot_clone.wait_ticks(20).await;
                            info!("Anvil placed! Transitioning to Phase 2: Items Retrieval.");
                            {
                                let mut g = state_clone.gui.lock().await;
                                g.phase = WithdrawalPhase::ItemsRetrieval;
                                g.state = OrderWorkflowState::WaitingForNextOrder;
                            }

                            bot_clone.wait_ticks(30).await;
                            info!("Opening /order for Phase 2 (Items Retrieval)...");
                            bot_clone.chat("/order");
                            return;
                        }

                        if done && g.state == OrderWorkflowState::WithdrawalComplete {
                            info!("Phase 2 completed: All required items retrieved from orders!");
                            trigger_enchanting_routine(&bot_clone, &state_clone).await;
                        }
                    });
                }
            }
        }
        ClientboundGamePacket::ContainerSetSlot(p) => {
            {
                let mut gui = state.gui.lock().await;
                gui.on_set_slot(p.container_id, p.state_id, p.slot as i16, &p.item_stack, Some(bot));
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
                } else if p.container_id == 0 {
                    ench.player_inventory.insert(p.slot as i16, p.item_stack.clone());
                }
            }
        }
        ClientboundGamePacket::SetExperience(p) => {
            state.current_level.store(p.experience_level as u32, std::sync::atomic::Ordering::SeqCst);
            state.total_experience.store(p.total_experience as u32, std::sync::atomic::Ordering::SeqCst);
            info!("[XP Sync] Current Level: {}, Total XP: {}", p.experience_level, p.total_experience);
        }
        ClientboundGamePacket::ContainerSetData(p) => {
            if p.id == 0 {
                state.server_anvil_cost.store(p.value as u32, std::sync::atomic::Ordering::SeqCst);
                info!("[Anvil Data] Repair Cost: {} levels", p.value);
            }
        }
        _ => {}
    }
}

async fn trigger_enchanting_routine(bot: &Client, state: &BotState) {
    let mut ench = state.enchanter.lock().await;
    if ench.is_enchanting {
        return;
    }
    ench.is_enchanting = true;
    info!("Withdrawal complete! Starting autonomous enchanting routine...");

    let bot_ench = bot.clone();
    let state_clone = state.clone();

    tokio::spawn(async move {
        // Wait 30 ticks for any previous container close packet to settle on the server
        bot_ench.wait_ticks(30).await;

        let player_inv = {
            let gui = state_clone.gui.lock().await;
            gui.player_inventory.clone()
        };

        // 1. Ensure any worn armor is unequipped
        EnchanterManager::ensure_no_worn_armor(&bot_ench, &player_inv).await;
        bot_ench.wait_ticks(15).await;

        // 2. Ensure Anvil is placed
        {
            let mut ench = state_clone.enchanter.lock().await;
            ench.place_anvil(&bot_ench, &player_inv).await;
        }

        bot_ench.wait_ticks(25).await;

        // 3. Open Anvil
        {
            let ench = state_clone.enchanter.lock().await;
            ench.open_anvil_with_inv(&bot_ench, &player_inv).await;
        }

        bot_ench.wait_ticks(30).await;

        // 4. Single master loop for anvil combines
        loop {
            // Check if all 4 pieces are confirmed fully enchanted
            {
                let ench = state_clone.enchanter.lock().await;
                let (all_done, summary) = ench.check_all_armor_enchanted(&bot_ench);
                if all_done {
                    info!("*** COMPLETE LOOP FINISHED: All orders withdrawn, anvil placed, and all 4 armor pieces confirmed fully enchanted! ***");
                    info!("Final Armor Status: {summary}");
                    break;
                }
            }

            // Check if anvil is open; if not, open it
            let is_open = {
                let ench = state_clone.enchanter.lock().await;
                ench.anvil_container_id.is_some()
            };

            if !is_open {
                info!("Anvil screen not open; re-opening anvil...");
                let current_inv = {
                    let ench = state_clone.enchanter.lock().await;
                    ench.player_inventory.clone()
                };
                {
                    let ench = state_clone.enchanter.lock().await;
                    ench.open_anvil_with_inv(&bot_ench, &current_inv).await;
                }
                bot_ench.wait_ticks(30).await;
                continue;
            }

            let mut ench = state_clone.enchanter.lock().await;
            let finished = ench.process_anvil_combines(&bot_ench).await;
            if finished {
                let (all_done, summary) = ench.check_all_armor_enchanted(&bot_ench);
                if all_done {
                    info!("*** COMPLETE LOOP FINISHED: All orders withdrawn, anvil placed, and all 4 armor pieces confirmed fully enchanted! ***");
                    info!("Final Armor Status: {summary}");
                    break;
                }
            }
            drop(ench);

            bot_ench.wait_ticks(25).await;
        }
    });
}

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    dotenvy::dotenv().ok();

    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::INFO)
        .finish();
    tracing::subscriber::set_global_default(subscriber)
        .expect("Failed to set tracing subscriber");

    let args = Args::parse();
    let token = args.token.or_else(|| std::env::var("TOKEN").ok());

    info!("Starting Minecraft Enchanter Bot...");
    info!("Target Server: {}:{}", args.server, args.port);

    // Build Account according to provided authentication credentials
    let account = if let Some(ref token) = token {
        info!("Using supplied access token for authentication...");
        match CustomTokenAccount::from_jwt(token) {
            Ok(custom_acc) => {
                info!(
                    "Successfully parsed token! Profile Name: '{}', UUID: {}",
                    custom_acc.username, custom_acc.uuid
                );
                custom_acc.into_azalea_account()
            }
            Err(e) => {
                error!("Failed to parse token payload: {e}");
                info!("Creating account with raw token string...");
                CustomTokenAccount::new("AzaleaBot", uuid::Uuid::new_v4(), token).into_azalea_account()
            }
        }
    } else if let Some(ref email) = args.microsoft {
        info!("Initiating Microsoft OAuth authentication for '{}'...", email);
        Account::microsoft(email).await?
    } else if let Some(ref username) = args.offline {
        info!("Using offline mode for '{}'...", username);
        Account::offline(username)
    } else {
        info!("No credentials provided. Defaulting to offline mode bot 'EnchanterBot'...");
        Account::offline("EnchanterBot")
    };

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

    let address = format!("{}:{}", args.server, args.port);
    info!("Connecting to {} as '{}'...", address, account.username());


    ClientBuilder::new()
        .set_handler(handle)
        .start(account, address)
        .await;

    Ok(())
}

