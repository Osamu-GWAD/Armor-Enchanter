use crate::stock::{OrderListing, StockAudit};
use crate::nbt::{
    inspect_item, inspect_item_with_bot, is_anvil, is_blast_protection_4, is_confirm_button,
    is_diamond_armor, is_diamond_boots, is_diamond_chestplate, is_diamond_helmet,
    is_diamond_leggings, is_mending, is_protection_4, is_unbreaking_3, is_xp_bottle,
    is_your_orders_button, normalize_small_caps, strip_color_and_normalize, ItemInfo,
};

use azalea::inventory::operations::ClickType;
use azalea::inventory::ItemStack;
use azalea::protocol::packets::game::s_container_click::{HashedStack, ServerboundContainerClick};
use azalea::protocol::packets::game::{ServerboundChatCommand, ServerboundContainerClose};
use azalea::Client;
use std::collections::HashMap;
use tracing::{debug, error, info, warn};

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
            anvils_needed: 2,
            mending_needed: 8,
            unbreaking_3_needed: 8,
            protection_4_needed: 4,
            blast_protection_4_needed: 4,
            xp_stacks_needed: 4,
            diamond_helmets_needed: 2,
            diamond_chestplates_needed: 2,
            diamond_leggings_needed: 2,
            diamond_boots_needed: 2,
        }
    }
}

/// Tracks the items collected so far into inventory.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
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
    fn has_required_xp(&self, quota: &WithdrawalQuota) -> bool {
        self.xp_bottles >= quota.xp_stacks_needed.saturating_mul(64)
    }

    pub fn is_fulfilled(&self, quota: &WithdrawalQuota) -> bool {
        let xp_met = self.has_required_xp(quota);

        self.mending >= quota.mending_needed
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
/// 2. ItemsRetrieval: after anvil is placed, withdraw books, XP bottles, and 2 pieces of each armor type.
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
    WithdrawalComplete,
    WaitingForNextOrder,
    FillingTargetOrders,
    DepositingTargetItems,
    ConfirmingFulfill,
    SellingInventory,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetArmorType {
    Helmet,
    Chestplate,
    Leggings,
    Boots,
}

#[derive(Debug, Clone)]
struct PendingDelivery {
    armor: TargetArmorType,
    inventory_count_before: usize,
    source_slot: i16,
    deposit_observed: bool,
    confirm_sent: bool,
}

#[derive(Debug, Clone)]
struct PendingAnvilPickup {
    inventory_before: u32,
    sent_at: std::time::Instant,
    warned: bool,
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
    pub last_collecting_order_name: Option<String>,
    pub current_order_slot: Option<i16>,
    pub exhausted_orders: Vec<String>,
    stock_audit: StockAudit,
    resume_order_scan: bool,
    order_page: usize,
    page_turn_pending: bool,
    pub collect_clicked_in_submenu: bool,
    pub target_delivering_armor: Option<TargetArmorType>,
    pub action_in_progress: bool,
    pub current_menu_skipped_slots: Vec<i16>,
    pub order_target: String,
    pub is_fulfilling_target: bool,
    pub last_target_order_click: Option<std::time::Instant>,
    pub gui_action_epoch: u64,
    pub is_waiting_for_restock: bool,
    pub restock_wait_until: Option<std::time::Instant>,
    pub last_action_time: Option<std::time::Instant>,
    pub action_retry_count: u32,
    pub last_clicked_slot: Option<i16>,
    pub last_click_type: Option<ClickType>,
    pub last_command_sent: Option<String>,
    pub awaiting_response: bool,
    pub target_sets_to_deliver: usize,
    pub next_drop: usize,
    pub pending_drop: Option<(usize, usize)>,
    pub hopper_active: bool,
    pub hopper_container_id: Option<i32>,
    pub player_state_id: u32,
    pub target_delivered_helmets: usize,
    pub target_delivered_chestplates: usize,
    pub target_delivered_leggings: usize,
    pub target_delivered_boots: usize,
    pending_delivery: Option<PendingDelivery>,
    transfer_before: Option<(ItemStack, HashMap<i16, ItemStack>)>,
    last_progress_time: std::time::Instant,
    scheduled_command: Option<(std::time::Instant, String)>,
    pub last_command_sent_time: Option<std::time::Instant>,
    pending_anvil_pickup: Option<PendingAnvilPickup>,
}

/// Sends a command to the server using the plain, unsigned ServerboundChatCommand packet.
/// As documented in GUI_COMMAND_FIX.md, sending plugin commands (/order, /ah, /bal, /balance, /home)
/// through signed chat sessions silently fails on Minecraft proxies (like DonutSMP/Velocity)
/// even while connection and keepalives remain active.
/// Using the plain unsigned command packet bypasses chat session signatures and executes reliably.
pub fn send_command(bot: &Client, cmd: &str) {
    let clean_cmd = cmd.strip_prefix('/').unwrap_or(cmd);
    bot.write_packet(ServerboundChatCommand {
        command: clean_cmd.to_string(),
    });
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
            last_collecting_order_name: None,
            current_order_slot: None,
            exhausted_orders: Vec::new(),
            stock_audit: StockAudit::default(),
            resume_order_scan: false,
            order_page: 0,
            page_turn_pending: false,
            collect_clicked_in_submenu: false,
            target_delivering_armor: None,
            action_in_progress: false,
            current_menu_skipped_slots: Vec::new(),
            order_target: "zn6h".to_string(),
            is_fulfilling_target: false,
            last_target_order_click: None,
            gui_action_epoch: 0,
            is_waiting_for_restock: false,
            restock_wait_until: None,
            last_action_time: None,
            action_retry_count: 0,
            last_clicked_slot: None,
            last_click_type: None,
            last_command_sent: None,
            awaiting_response: false,
            target_sets_to_deliver: 0,
            next_drop: 0,
            pending_drop: None,
            hopper_active: false,
            hopper_container_id: None,
            player_state_id: 0,
            target_delivered_helmets: 0,
            target_delivered_chestplates: 0,
            target_delivered_leggings: 0,
            target_delivered_boots: 0,
            pending_delivery: None,
            transfer_before: None,
            last_progress_time: std::time::Instant::now(),
            scheduled_command: None,
            last_command_sent_time: None,
            pending_anvil_pickup: None,
        }
    }

    /// Reset watchdog tracking fields.
    pub fn clear_watchdog(&mut self) {
        self.last_action_time = None;
        self.action_retry_count = 0;
        self.last_clicked_slot = None;
        self.last_click_type = None;
        self.last_command_sent = None;
        self.awaiting_response = false;
        self.transfer_before = None;
    }

    /// Record that a container click action has been dispatched and watchdog should await response.
    pub fn record_action_sent(&mut self, slot: Option<i16>, click_type: Option<ClickType>) {
        self.last_action_time = Some(std::time::Instant::now());
        self.last_progress_time = std::time::Instant::now();
        self.last_clicked_slot = slot;
        self.last_click_type = click_type;
        self.last_command_sent = None;
        self.action_retry_count = 0;
        self.awaiting_response = true;
    }

    /// Record that a chat command opening a GUI has been sent and watchdog should await response.
    pub fn record_command_sent(&mut self, cmd: &str) {
        if cmd.trim() == "/order" {
            self.order_page = 0;
            self.page_turn_pending = false;
            // Immediate post-Collect reopens share an audit; restock retries
            // start a new pass and recheck every previously empty listing.
            if self.resume_order_scan {
                self.stock_audit.continue_scan();
            } else {
                self.stock_audit.start_scan();
                self.exhausted_orders.clear();
            }
            self.resume_order_scan = false;
        }
        self.scheduled_command = None;
        self.last_action_time = Some(std::time::Instant::now());
        self.last_progress_time = std::time::Instant::now();
        self.last_clicked_slot = None;
        self.last_click_type = None;
        self.last_command_sent = Some(cmd.to_string());
        self.action_retry_count = 0;
        self.awaiting_response = true;
        self.current_container_id = 0;
        self.current_slots.clear();
    }

    /// Sends a command to the server using the plain, unsigned ServerboundChatCommand packet.
    pub fn send_command(bot: &Client, cmd: &str) {
        send_command(bot, cmd);
    }

    /// Prepare to send a command by closing any lingering container and setting watchdog tracking before sending.
    pub fn prepare_to_send_command(&mut self, bot: &Client, cmd: &str) {
        if let Some(last) = self.last_command_sent_time {
            let elapsed = last.elapsed();
            if elapsed < std::time::Duration::from_millis(1500) {
                let delay = std::time::Duration::from_millis(1500) - elapsed;
                self.schedule_command(cmd.to_string(), delay);
                return;
            }
        }
        if self.current_container_id > 0 {
            info!("Closing lingering container #{} before sending command '{cmd}'...", self.current_container_id);
            bot.write_packet(ServerboundContainerClose {
                container_id: self.current_container_id,
            });
        }
        if cmd.trim() == "/order" && self.phase == WithdrawalPhase::AnvilPlacement {
            bot.set_direction(bot.direction().y_rot(), 90.0);
        }
        self.last_command_sent_time = Some(std::time::Instant::now());
        self.record_command_sent(cmd);
        send_command(bot, cmd);
    }

    fn schedule_command(&mut self, command: String, delay: std::time::Duration) {
        self.clear_watchdog();
        self.scheduled_command = Some((std::time::Instant::now() + delay, command));
    }

    /// Identify the friendly name of an order item regardless of quota fulfillment state.
    pub fn identify_order_item_name(&self, info: &ItemInfo) -> &'static str {
        if is_anvil(info) {
            "Anvil"
        } else if is_mending(info) {
            "Mending Book"
        } else if is_unbreaking_3(info) {
            "Unbreaking III Book"
        } else if is_blast_protection_4(info) {
            "Blast Protection IV Book"
        } else if is_protection_4(info) {
            "Protection IV Book"
        } else if is_xp_bottle(info) {
            "Experience Bottles"
        } else if is_diamond_helmet(info) {
            "Diamond Helmet"
        } else if is_diamond_chestplate(info) {
            "Diamond Chestplate"
        } else if is_diamond_leggings(info) {
            "Diamond Leggings"
        } else if is_diamond_boots(info) {
            "Diamond Boots"
        } else {
            ""
        }
    }

    /// Returns a descriptive name for an item stack based on custom name, detected enchantment, or kind.
    pub fn get_item_name(&self, info: &ItemInfo) -> String {
        let identified = self.identify_order_item_name(info);
        if !identified.is_empty() {
            identified.to_string()
        } else if let Some(ref name) = info.custom_name {
            name.clone()
        } else {
            info.kind.clone()
        }
    }

    /// Generate a readable summary of items that are still missing to fulfill quota.
    pub fn get_missing_quota_summary(&self) -> String {
        let mut missing = Vec::new();
        if self.phase == WithdrawalPhase::AnvilPlacement {
            if self.collected.anvils < self.quota.anvils_needed {
                missing.push(format!("Anvil ({}/{})", self.collected.anvils, self.quota.anvils_needed));
            }
        } else {
            if self.collected.mending < self.quota.mending_needed {
                missing.push(format!("Mending Book ({}/{})", self.collected.mending, self.quota.mending_needed));
            }
            if self.collected.unbreaking_3 < self.quota.unbreaking_3_needed {
                missing.push(format!("Unbreaking III Book ({}/{})", self.collected.unbreaking_3, self.quota.unbreaking_3_needed));
            }
            if self.collected.protection_4 < self.quota.protection_4_needed {
                missing.push(format!("Protection IV Book ({}/{})", self.collected.protection_4, self.quota.protection_4_needed));
            }
            if self.collected.blast_protection_4 < self.quota.blast_protection_4_needed {
                missing.push(format!("Blast Protection IV Book ({}/{})", self.collected.blast_protection_4, self.quota.blast_protection_4_needed));
            }
            let xp_met = self.collected.has_required_xp(&self.quota);
            if !xp_met {
                missing.push(format!("XP Bottles ({}/{} stacks, {} bottles)", self.collected.xp_stacks, self.quota.xp_stacks_needed, self.collected.xp_bottles));
            }
            if self.collected.diamond_helmets < self.quota.diamond_helmets_needed {
                missing.push(format!("Diamond Helmet ({}/{})", self.collected.diamond_helmets, self.quota.diamond_helmets_needed));
            }
            if self.collected.diamond_chestplates < self.quota.diamond_chestplates_needed {
                missing.push(format!("Diamond Chestplate ({}/{})", self.collected.diamond_chestplates, self.quota.diamond_chestplates_needed));
            }
            if self.collected.diamond_leggings < self.quota.diamond_leggings_needed {
                missing.push(format!("Diamond Leggings ({}/{})", self.collected.diamond_leggings, self.quota.diamond_leggings_needed));
            }
            if self.collected.diamond_boots < self.quota.diamond_boots_needed {
                missing.push(format!("Diamond Boots ({}/{})", self.collected.diamond_boots, self.quota.diamond_boots_needed));
            }
        }

        if missing.is_empty() {
            "None (All quota items collected!)".to_string()
        } else {
            missing.join(", ")
        }
    }

    /// Records that an order has 0 items to collect upon receiving server chat notification,
    /// logs prominent out-of-stock messages detailing which items are affected,
    pub fn record_no_items_to_collect(&mut self, bot: &Client) -> Option<String> {
        // Stale or unrelated chat must not close the current GUI.
        let item_name = self.stock_audit.confirm_empty()?;
        if !self.exhausted_orders.contains(&item_name) {
            self.exhausted_orders.push(item_name.clone());
        }
        info!("The selected order for '{item_name}' is empty. Checking other orders and inventory before alerting.");
        self.resume_order_scan = true;
        self.close_current_gui(bot);
        self.target_order_type = None;
        self.last_collecting_order_name = None;
        self.collect_clicked_in_submenu = false;
        self.state = OrderWorkflowState::WaitingForNextOrder;
        Some(item_name)
    }

    fn verified_stock_alerts(&self) -> Vec<&'static str> {
        let candidates = if self.phase == WithdrawalPhase::AnvilPlacement {
            vec![("Anvil", self.collected.anvils, self.quota.anvils_needed)]
        } else if self.phase == WithdrawalPhase::ItemsRetrieval {
            vec![
                ("Experience Bottles", self.collected.xp_bottles, self.quota.xp_stacks_needed),
                ("Mending Book", self.collected.mending, self.quota.mending_needed),
                ("Unbreaking III Book", self.collected.unbreaking_3, self.quota.unbreaking_3_needed),
                ("Protection IV Book", self.collected.protection_4, self.quota.protection_4_needed),
                ("Blast Protection IV Book", self.collected.blast_protection_4, self.quota.blast_protection_4_needed),
                ("Diamond Helmet", self.collected.diamond_helmets, self.quota.diamond_helmets_needed),
                ("Diamond Chestplate", self.collected.diamond_chestplates, self.quota.diamond_chestplates_needed),
                ("Diamond Leggings", self.collected.diamond_leggings, self.quota.diamond_leggings_needed),
                ("Diamond Boots", self.collected.diamond_boots, self.quota.diamond_boots_needed),
            ]
        } else { vec![] };
        candidates.into_iter().filter_map(|(name, count, required)| {
            (required > 0 && self.stock_audit.confirmed_unavailable(name, count)).then_some(name)
        }).collect()
    }

    fn order_listing(&self, slot: i16, bot: Option<&Client>) -> Option<OrderListing> {
        let icon = self.current_slots.get(&slot)?;
        let info = inspect_item_with_bot(icon, bot)?;
        let name = self.identify_order_item_name(&info);
        if name.is_empty() { return None; }
        Some(OrderListing { slot, name: name.to_string(), icon: icon.clone() })
    }

    fn has_next_order_page(&self, bot: Option<&Client>) -> bool {
        let Some(info) = self.current_slots.get(&53).and_then(|item| inspect_item_with_bot(item, bot)) else { return false; };
        let text = normalize_small_caps(&format!("{} {}", info.custom_name.unwrap_or_default(), info.lore.join(" ")).to_lowercase());
        info.kind.to_lowercase().contains("arrow")
            && !["no next", "last page", "disabled"].iter().any(|label| text.contains(label))
    }

    /// Check if an order item is needed in the current phase and quota.
    pub fn is_order_needed(&self, info: &ItemInfo) -> (bool, &'static str) {
        match self.phase {
            WithdrawalPhase::AnvilPlacement => {
                if is_anvil(info) && self.pending_anvil_pickup.is_none() && self.collected.anvils < self.quota.anvils_needed {
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
                    let has_enough = self.collected.has_required_xp(&self.quota);
                    if !has_enough && self.collected.xp_stacks <= self.quota.xp_stacks_needed {
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
                    if self.collected.diamond_helmets >= self.quota.diamond_helmets_needed
                        && self.collected.diamond_chestplates < self.quota.diamond_chestplates_needed {
                        (true, "Diamond Chestplate")
                    } else {
                        (false, "")
                    }
                } else if is_diamond_leggings(info) {
                    if self.collected.diamond_helmets >= self.quota.diamond_helmets_needed
                        && self.collected.diamond_chestplates >= self.quota.diamond_chestplates_needed
                        && self.collected.diamond_leggings < self.quota.diamond_leggings_needed {
                        (true, "Diamond Leggings")
                    } else {
                        (false, "")
                    }
                } else if is_diamond_boots(info) {
                    if self.collected.diamond_helmets >= self.quota.diamond_helmets_needed
                        && self.collected.diamond_chestplates >= self.quota.diamond_chestplates_needed
                        && self.collected.diamond_leggings >= self.quota.diamond_leggings_needed
                        && self.collected.diamond_boots < self.quota.diamond_boots_needed {
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
        self.stock_audit = StockAudit::default();
        self.resume_order_scan = false;
        self.exhausted_orders.clear();
        self.order_page = 0;
        self.page_turn_pending = false;
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
        self.reconcile_anvil_pickup();
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
        self.is_waiting_for_restock = false;
        self.restock_wait_until = None;

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
    /// replacing historical counts with the latest server inventory snapshot.
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

        let prev = self.collected.clone();

        self.collected.anvils = anvils;
        self.reconcile_anvil_pickup();
        self.collected.mending = mending;
        self.collected.unbreaking_3 = unb3;
        self.collected.protection_4 = prot4;
        self.collected.blast_protection_4 = blast_prot4;
        self.collected.xp_bottles = xp_count;
        self.collected.xp_stacks = xp_stack_slots;
        self.collected.diamond_helmets = helmets;
        self.collected.diamond_chestplates = chestplates;
        self.collected.diamond_leggings = leggings;
        self.collected.diamond_boots = boots;

        if prev != self.collected {
            info!(
                "Inventory Progress [Phase {:?}]: Anvils: {}/{}, Mending: {}/{}, Unb3: {}/{}, Prot4: {}/{}, BlastProt4: {}/{}, XP: {}/{} ({} bottles, {} stack slots), Armor (H: {}/{}, C: {}/{}, L: {}/{}, B: {}/{})",
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

    /// Scan player inventory (slots 9..=44) to find any items that are NOT needed for the quota,
    /// are not intermediate/max-enchanted armor, and are not placed anvil.
    /// Returns a list of (inventory_slot_num, description).
    pub fn find_unneeded_inventory_slots(&self, bot: Option<&Client>) -> Vec<(i16, String)> {
        let mut unneeded = Vec::new();

        let mut kept_mending = 0;
        let mut kept_unb3 = 0;
        let mut kept_prot4 = 0;
        let mut kept_blast_prot4 = 0;
        let mut kept_xp_stacks = 0;
        let mut kept_helmets = 0;
        let mut kept_chestplates = 0;
        let mut kept_leggings = 0;
        let mut kept_boots = 0;

        for slot in 9..=44i16 {
            let item = match self.player_inventory.get(&slot) {
                Some(it) if !matches!(it, ItemStack::Empty) => it,
                _ => continue,
            };

            let info = match inspect_item_with_bot(item, bot) {
                Some(inf) => inf,
                None => continue,
            };

            // 1. Max-enchanted or partially enchanted diamond armor MUST be preserved!
            if is_diamond_armor(&info) {
                let is_prot_piece = is_diamond_helmet(&info) || is_diamond_chestplate(&info);
                let is_blast_piece = is_diamond_leggings(&info) || is_diamond_boots(&info);

                let is_maxed = (is_diamond_helmet(&info) && info.has_enchantment("protection", 4) && info.has_enchantment("unbreaking", 3) && info.has_enchantment("mending", 1))
                    || (is_diamond_chestplate(&info) && info.has_enchantment("protection", 4) && info.has_enchantment("unbreaking", 3) && info.has_enchantment("mending", 1))
                    || (is_diamond_leggings(&info) && info.has_enchantment("blast_protection", 4) && info.has_enchantment("unbreaking", 3) && info.has_enchantment("mending", 1))
                    || (is_diamond_boots(&info) && info.has_enchantment("blast_protection", 4) && info.has_enchantment("unbreaking", 3) && info.has_enchantment("mending", 1));

                let has_intermediate_ench = (is_prot_piece && (info.has_enchantment("protection", 4) || info.has_enchantment("unbreaking", 3) || info.has_enchantment("mending", 1)))
                    || (is_blast_piece && (info.has_enchantment("blast_protection", 4) || info.has_enchantment("unbreaking", 3) || info.has_enchantment("mending", 1)));

                if is_maxed || has_intermediate_ench {
                    // This armor piece is either finished God armor or in progress of being enchanted. Keep it!
                    continue;
                }

                // Unenchanted diamond armor piece: check against quota
                if is_diamond_helmet(&info) {
                    if kept_helmets < self.quota.diamond_helmets_needed {
                        kept_helmets += 1;
                    } else {
                        unneeded.push((slot, format!("Surplus Diamond Helmet (slot #{slot})")));
                    }
                    continue;
                } else if is_diamond_chestplate(&info) {
                    if kept_chestplates < self.quota.diamond_chestplates_needed {
                        kept_chestplates += 1;
                    } else {
                        unneeded.push((slot, format!("Surplus Diamond Chestplate (slot #{slot})")));
                    }
                    continue;
                } else if is_diamond_leggings(&info) {
                    if kept_leggings < self.quota.diamond_leggings_needed {
                        kept_leggings += 1;
                    } else {
                        unneeded.push((slot, format!("Surplus Diamond Leggings (slot #{slot})")));
                    }
                    continue;
                } else if is_diamond_boots(&info) {
                    if kept_boots < self.quota.diamond_boots_needed {
                        kept_boots += 1;
                    } else {
                        unneeded.push((slot, format!("Surplus Diamond Boots (slot #{slot})")));
                    }
                    continue;
                }
            }

            // 2. Anvil - never treated as unneeded (preserved as spare anvils for when world anvil breaks)
            if is_anvil(&info) {
                continue;
            }

            // 3. Books
            if is_mending(&info) {
                if kept_mending < self.quota.mending_needed {
                    kept_mending += 1;
                } else {
                    unneeded.push((slot, format!("Surplus Mending Book (slot #{slot})")));
                }
                continue;
            } else if is_unbreaking_3(&info) {
                if kept_unb3 < self.quota.unbreaking_3_needed {
                    kept_unb3 += 1;
                } else {
                    unneeded.push((slot, format!("Surplus Unbreaking III Book (slot #{slot})")));
                }
                continue;
            } else if is_protection_4(&info) {
                if kept_prot4 < self.quota.protection_4_needed {
                    kept_prot4 += 1;
                } else {
                    unneeded.push((slot, format!("Surplus Protection IV Book (slot #{slot})")));
                }
                continue;
            } else if is_blast_protection_4(&info) {
                if kept_blast_prot4 < self.quota.blast_protection_4_needed {
                    kept_blast_prot4 += 1;
                } else {
                    unneeded.push((slot, format!("Surplus Blast Protection IV Book (slot #{slot})")));
                }
                continue;
            }

            // 4. XP Bottles
            if is_xp_bottle(&info) {
                if kept_xp_stacks < self.quota.xp_stacks_needed {
                    kept_xp_stacks += 1;
                } else {
                    unneeded.push((slot, format!("Surplus XP Bottles (slot #{slot})")));
                }
                continue;
            }

            // 5. Any other item (foreign items, cobble, junk books, weapons, tools, etc.)
            let name = self.get_item_name(&info);
            unneeded.push((slot, format!("Foreign item: '{name}' ({} x{}, slot #{slot})", info.kind, info.count)));
        }

        unneeded
    }

    /// Check if inventory has unneeded items (inventory cleaning completely removed).
    pub async fn sell_unneeded_inventory(&mut self, _bot: &Client) {
        info!("Inventory cleaning is completely disabled. Skipping /sell.");
    }

    /// When inside the /sell GUI, immediately close it without selling anything.
    pub async fn process_sell_inventory(&mut self, bot: &Client) -> bool {
        info!("In /sell GUI but inventory cleaning is disabled. Closing /sell GUI...");
        self.close_current_gui(bot);
        self.state = OrderWorkflowState::WaitingForNextOrder;
        true
    }

    /// Reset slot tracking and transition state when a new container is opened.
    pub fn on_open_screen(&mut self, container_id: i32, title: &str) {
        info!("GUI opened: container_id={container_id}, title='{title}'");
        let retries = self.action_retry_count;
        self.clear_watchdog();
        self.gui_action_epoch += 1;
        self.current_container_id = container_id;
        // OpenScreen does not guarantee that the server will send the contents.
        self.record_action_sent(None, None);
        self.action_retry_count = retries;
        self.current_slots.clear();
        self.current_state_id = 0;
        self.open_container_size = 0;
        self.collect_clicked_in_submenu = false;

        if self.hopper_active || (!self.is_fulfilling_target && self.state == OrderWorkflowState::WithdrawalComplete) {
            info!("Ignoring GUI open transition because WithdrawalComplete is already reached.");
            return;
        }

        let clean_title = normalize_small_caps(&strip_color_and_normalize(title)).to_lowercase();

        if clean_title.contains("sell") || self.state == OrderWorkflowState::SellingInventory {
            info!("Transitioned to SellingInventory ('{clean_title}')");
            self.state = OrderWorkflowState::SellingInventory;
            return;
        }

        if self.is_fulfilling_target
            || self.state == OrderWorkflowState::FillingTargetOrders
            || self.state == OrderWorkflowState::DepositingTargetItems
            || self.state == OrderWorkflowState::ConfirmingFulfill
        {
            if clean_title.contains("confirm") || clean_title.contains("payout") {
                info!("Transitioned to ConfirmingFulfill ('{clean_title}')");
                self.state = OrderWorkflowState::ConfirmingFulfill;
            } else if clean_title.contains("deliver") {
                info!("Transitioned to DepositingTargetItems ('{clean_title}')");
                self.state = OrderWorkflowState::DepositingTargetItems;
            } else {
                info!("Target player orders screen opened ('{clean_title}'). State: FillingTargetOrders");
                self.state = OrderWorkflowState::FillingTargetOrders;
            }
            return;
        }

        if self.is_waiting_for_restock {
            if let Some(until) = self.restock_wait_until {
                if std::time::Instant::now() < until {
                    let secs_left = until.saturating_duration_since(std::time::Instant::now()).as_secs();
                    info!("GUI opened ('{clean_title}') while waiting for restock cooldown ({secs_left}s left). Maintaining WaitingForNextOrder state.");
                    self.state = OrderWorkflowState::WaitingForNextOrder;
                    return;
                } else {
                    self.is_waiting_for_restock = false;
                }
            }
        }

        if clean_title.contains("collect item") || clean_title.contains("collect items") {
            info!("Transitioned to InCollectDeliveryMenu ('{clean_title}')");
            self.state = OrderWorkflowState::InCollectDeliveryMenu;
        } else if clean_title.contains("edit order") || clean_title.contains("view order") {
            info!("Transitioned to InOrderSubmenu ('{clean_title}')");
            self.collect_clicked_in_submenu = false;
            self.state = OrderWorkflowState::InOrderSubmenu;
        } else if clean_title.contains("your order") || clean_title.contains("my order") {
            info!("Transitioned to OpenedYourOrdersMenu ('{clean_title}')");
            self.current_menu_skipped_slots.clear();
            self.state = OrderWorkflowState::OpenedYourOrdersMenu;
        } else if clean_title.contains("confirm") || clean_title.contains("payout") {
            info!("Confirm/Payout screen opened ('{clean_title}'). State: ConfirmingFulfill");
            self.state = OrderWorkflowState::ConfirmingFulfill;
        } else if clean_title.contains("deliver") {
            info!("Deliver screen opened ('{clean_title}'). State: DepositingTargetItems");
            self.state = OrderWorkflowState::DepositingTargetItems;
        } else if clean_title == "orders" || (clean_title.contains("order") && !clean_title.contains("->")) {
            info!("Transitioned to OpenedOrderMainMenu ('{clean_title}')");
            self.state = OrderWorkflowState::OpenedOrderMainMenu;
        } else {
            info!("Other/Submenu GUI screen opened ('{clean_title}').");
        }
    }

    /// Update all slots when ClientboundContainerSetContent is received.
    pub fn on_set_content(&mut self, container_id: i32, state_id: u32, items: &[ItemStack], bot: Option<&Client>) {
        if container_id == 0 {
            self.player_state_id = state_id;
            for (i, item) in items.iter().enumerate() {
                self.player_inventory.insert(i as i16, item.clone());
            }
            self.sync_collected_from_inventory(bot);
            return;
        }

        if self.current_container_id != container_id {
            // Late packets from a closed screen must not resurrect it.
            return;
        }
        if self.last_clicked_slot.is_none() {
            self.clear_watchdog();
        }
        self.gui_action_epoch += 1;
        self.current_state_id = state_id;
        self.current_slots.clear();

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
        if self.page_turn_pending && self.state == OrderWorkflowState::OpenedYourOrdersMenu {
            self.page_turn_pending = false;
            self.current_menu_skipped_slots.clear();
            self.clear_watchdog();
        }
        self.acknowledge_transfer();
        if self.state == OrderWorkflowState::FillingTargetOrders {
            self.reconcile_delivery(bot);
        }
    }

    /// Update an individual slot when ClientboundContainerSetSlot is received.
    pub fn on_set_slot(&mut self, container_id: i32, state_id: u32, slot: i16, item: &ItemStack, bot: Option<&Client>) {
        if container_id == -2 {
            if let Some(slot) = crate::armor::player_menu_slot(slot as u32) {
                self.player_inventory.insert(slot, item.clone());
                self.sync_collected_from_inventory(bot);
            }
            return;
        }
        if container_id == 0 {
            self.player_state_id = state_id;
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

            self.acknowledge_transfer();

            if let Some(info) = inspect_item_with_bot(item, bot) {
                debug!("Slot #{slot} updated -> {} x{}", info.kind, info.count);
            }
        }
    }

    fn acknowledge_transfer(&mut self) {
        if let (Some(slot), Some((source, inventory))) = (self.last_clicked_slot, &self.transfer_before) {
            if self.current_slots.get(&slot).is_some_and(|item| item != source)
                && &self.player_inventory != inventory
            {
                if self.state == OrderWorkflowState::InCollectDeliveryMenu {
                    if let Some(ref order_type) = self.target_order_type {
                        self.exhausted_orders.retain(|x| x != order_type);
                    }
                }
                self.clear_watchdog();
                self.last_progress_time = std::time::Instant::now();
            }
        }
        if self.pending_anvil_pickup.is_none() && self.last_click_type == Some(ClickType::Throw) {
            if let Some(slot) = self.last_clicked_slot {
                if self.current_slots.get(&slot).map(|s| matches!(s, ItemStack::Empty)).unwrap_or(true) {
                    self.clear_watchdog();
                    self.last_progress_time = std::time::Instant::now();
                }
            }
        }
    }

    /// Print detailed NBT/components for all present items in the GUI (at debug level or when diagnosing).
    pub fn log_all_slot_nbt(&self, bot: Option<&Client>) {
        debug!("=== Current GUI Item & NBT Audit ===");
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

                debug!(
                    "Slot #{slot:02}: '{name}' x{} [kind: {}]{ench_str}",
                    info.count, info.kind
                );

                if let Some(ref comp) = info.raw_components {
                    let comp_json = serde_json::to_string(comp).unwrap_or_default();
                    if comp_json.len() > 200 {
                        debug!("  NBT/Components: {}...", &comp_json[..200]);
                    } else if !comp_json.is_empty() && comp_json != "{}" {
                        debug!("  NBT/Components: {}", comp_json);
                    }
                }
            }
        }
        debug!("====================================");
    }

    /// Perform the next automated action in the current GUI.
    pub async fn process_gui_actions(&mut self, bot: &Client) -> bool {
        if self.pending_anvil_pickup.is_some() { return false; }
        if !self.is_fulfilling_target && (self.state == OrderWorkflowState::WithdrawalComplete
            || (self.phase == WithdrawalPhase::Done
                && self.state != OrderWorkflowState::SellingInventory
                && self.state != OrderWorkflowState::FillingTargetOrders
                && self.state != OrderWorkflowState::DepositingTargetItems
                && self.state != OrderWorkflowState::ConfirmingFulfill))
        {
            if self.current_container_id > 0 {
                self.close_current_gui(bot);
            }
            return true;
        }

        if self.is_waiting_for_restock {
            if let Some(until) = self.restock_wait_until {
                if std::time::Instant::now() < until {
                    let secs_left = until.saturating_duration_since(std::time::Instant::now()).as_secs();
                    info!("GUI open while waiting for restock ({}s left). Closing container #{}...", secs_left, self.current_container_id);
                    if self.current_container_id > 0 {
                        self.close_current_gui(bot);
                    }
                    return true;
                } else {
                    self.is_waiting_for_restock = false;
                }
            }
        }

        if self.action_in_progress || self.awaiting_response || self.current_slots.is_empty() {
            return false;
        }
        self.action_in_progress = true;

        if self.state == OrderWorkflowState::SellingInventory {
            let res = self.process_sell_inventory(bot).await;
            self.action_in_progress = false;
            return res;
        }

        if self.state == OrderWorkflowState::FillingTargetOrders {
            let res = self.process_fill_target_order(bot).await;
            self.action_in_progress = false;
            return res;
        }

        if self.state == OrderWorkflowState::DepositingTargetItems {
            let res = self.process_deposit_target_items(bot).await;
            self.action_in_progress = false;
            return res;
        }

        if self.state == OrderWorkflowState::ConfirmingFulfill {
            if !self.is_fulfilling_target {
                warn!("Unexpected confirm screen opened while not fulfilling target orders (container #{})! Closing all GUI and retrying /order in 10 ticks...", self.current_container_id);
                self.close_current_gui(bot);
                self.state = OrderWorkflowState::WaitingForNextOrder;
                self.schedule_command("/order".to_string(), std::time::Duration::from_millis(500));
                self.action_in_progress = false;
                return true;
            }
            let res = self.confirm_target_order_fulfill(bot).await;
            self.action_in_progress = false;
            return res;
        }

        self.sync_collected_from_inventory(Some(bot));

        let phase_fulfilled = match self.phase {
            WithdrawalPhase::AnvilPlacement => self.anvil_withdrawal_complete(),
            WithdrawalPhase::ItemsRetrieval => self.collected.is_fulfilled(&self.quota),
            WithdrawalPhase::Done => true,
        };

        if !self.is_fulfilling_target && phase_fulfilled {
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

                    true
                } else {
                    warn!("Could not find 'Your Orders' button in /order main menu! Closing all GUI and retrying /order in 10 ticks...");
                    self.close_current_gui(bot);
                    self.state = OrderWorkflowState::WaitingForNextOrder;
                    self.schedule_command("/order".to_string(), std::time::Duration::from_millis(500));
                    true
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
            OrderWorkflowState::WaitingForNextOrder => {
                if self.current_container_id > 0 {
                    info!("In WaitingForNextOrder state with container #{} open; closing...", self.current_container_id);
                    self.close_current_gui(bot);
                    true
                } else {
                    false
                }
            }
            _ => {
                if !self.is_fulfilling_target && self.current_container_id > 0 {
                    warn!("Unexpected container #{} open in state {:?}! Closing all GUI and retrying /order in 10 ticks...", self.current_container_id, self.state);
                    self.close_current_gui(bot);
                    self.state = OrderWorkflowState::WaitingForNextOrder;
                    self.schedule_command("/order".to_string(), std::time::Duration::from_millis(500));
                    true
                } else {
                    false
                }
            }
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

        if self.open_container_size < 54 || self.current_slots.len() < (self.open_container_size + 36) as usize {
            return false;
        }
        let listings: Vec<_> = (0..=44).filter_map(|slot| self.order_listing(slot, Some(bot))).collect();
        let has_next_page = self.has_next_order_page(Some(bot));
        self.stock_audit.observe_page(self.order_page, listings, has_next_page);

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
                        let Some(listing) = self.order_listing(slot, Some(bot)) else { continue; };
                        if self.stock_audit.is_empty(self.order_page, &listing) {
                            self.current_menu_skipped_slots.push(slot);
                            continue;
                        }

                        if self.count_free_inventory_slots() == 0 && !is_xp_bottle(&info) && !(self.phase == WithdrawalPhase::AnvilPlacement && is_anvil(&info)) {
                            info!("Player inventory has 0 free slots; cannot collect unstackable '{order_name}' at slot #{slot}.");
                            self.current_menu_skipped_slots.push(slot);
                            continue;
                        }

                        info!("Found needed order '{order_name}' at slot #{slot} (Phase: {:?})! Left-clicking to open Edit/Claim submenu...", self.phase);
                        self.target_order_type = Some(order_name.to_string());
                        self.last_collecting_order_name = Some(order_name.to_string());
                        self.current_order_slot = Some(slot);
                        self.stock_audit.select(self.order_page, listing);
                        self.click_slot(bot, slot, ClickType::Pickup);

                        return true;
                    } else {
                        // Mark this slot as not needed for this menu session
                        self.current_menu_skipped_slots.push(slot);
                    }
                }
            }
        }

        if has_next_page && self.order_page < 20 {
            info!("Checking the next page of Your Orders before declaring supplies unavailable...");
            self.order_page += 1;
            self.page_turn_pending = true;
            self.current_menu_skipped_slots.clear();
            self.click_slot(bot, 53, ClickType::Pickup);
            return true;
        }

        let missing = self.get_missing_quota_summary();
        info!("No unfulfilled orders matching phase {:?} found in slots 0-44 of 'Your Orders'.", self.phase);

        if !self.exhausted_orders.is_empty() {
            warn!("============================================================");
            warn!("*** [EMPTY LISTINGS] At least one listing was empty for: {:?} ***", self.exhausted_orders);
            warn!("*** Missing quota items still needed: {} ***", missing);
            warn!("============================================================");
        }

        let available_armor_orders = self.count_available_armor_orders(Some(bot));
        // Missing the first armor type must wait for restock, even if later
        // types are listed. It does not mean all work is complete.

        // If remaining orders cannot fulfill full quota, but we have craftable sets in inventory:
        if self.phase == WithdrawalPhase::ItemsRetrieval {
            let complete_sets = self.count_craftable_god_sets(Some(bot));
            let free_slots = self.count_free_inventory_slots();
            let has_exhausted = !self.exhausted_orders.is_empty();

            if complete_sets > 0 && (free_slots == 0 || has_exhausted || available_armor_orders == 0) {
                info!(
                    "Cannot collect all remaining quota items (free slots: {}, exhausted orders: {:?}, available armor orders: {}), but inventory has materials for {} COMPLETE God-armor set(s)! Proceeding to enchant...",
                    free_slots, self.exhausted_orders, available_armor_orders, complete_sets
                );
                self.close_current_gui(bot);
                self.phase = WithdrawalPhase::Done;
                self.state = OrderWorkflowState::WithdrawalComplete;
                return true;
            }

            if free_slots == 0 && complete_sets == 0 {
                warn!("Inventory is full (0 free slots) and cannot craft any complete sets. Pausing for 45s restock cooldown...");
                self.close_current_gui(bot);
                self.state = OrderWorkflowState::WaitingForNextOrder;
                self.is_waiting_for_restock = true;
                self.restock_wait_until = Some(std::time::Instant::now() + std::time::Duration::from_secs(45));
                self.schedule_command("/order".to_string(), std::time::Duration::from_millis(45000));
                return true;
            }
        }

        if self.collected.is_fulfilled(&self.quota) {
            info!("All required items have been fulfilled! Closing GUI.");
            self.close_current_gui(bot);
            self.phase = WithdrawalPhase::Done;
            self.state = OrderWorkflowState::WithdrawalComplete;
            return true;
        }

        // If needed orders are out of stock or missing and we cannot enchant with current inventory, close GUI and wait for restock
        if !self.exhausted_orders.is_empty() || !self.collected.is_fulfilled(&self.quota) {
            let missing = self.get_missing_quota_summary();
            warn!("============================================================");
            for item in self.verified_stock_alerts() {
                crate::webhook::send_verified_out_of_item_alert(item, Some(
                    "Inventory has zero of this item. Checked all pages of Your Orders: no matching order exists, or every matching order confirmed no items to collect."
                ));
            }
            warn!("*** Missing items needed to proceed: {} ***", missing);
            warn!("*** Current inventory: Anvils: {}/{}, Mending: {}/{}, Unb3: {}/{}, Prot4: {}/{}, BlastProt4: {}/{}, XP: {}/{} ({} bottles), Armor (H: {}/{}, C: {}/{}, L: {}/{}, B: {}/{}) ***",
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
            warn!("*** Pausing for 45 seconds before checking /order for restocks... ***");
            warn!("============================================================");

            self.close_current_gui(bot);
            self.state = OrderWorkflowState::WaitingForNextOrder;
            self.is_waiting_for_restock = true;
            self.restock_wait_until = Some(std::time::Instant::now() + std::time::Duration::from_secs(45));

            self.schedule_command("/order".to_string(), std::time::Duration::from_millis(45000));

            return true;
        }

        false
    }

    /// Find the collect button in the 'Orders -> Edit Order' submenu.
    fn find_collect_button_slot(&self) -> Option<i16> {
        // Priority 1: Check slot 15 (DonutSMP Edit Order Collect button)
        if let Some(item) = self.current_slots.get(&15) {
            if let Some(info) = inspect_item(item) {
                if info.kind.to_lowercase().contains("chest")
                    || info.custom_name.as_deref().unwrap_or("").to_lowercase().contains("collect")
                    || is_confirm_button(&info)
                {
                    info!("Identified slot #15 as 'Collect' button ({}, Name: {:?}, Lore: {:?})", info.kind, info.custom_name, info.lore);
                    return Some(15);
                }
            }
        }

        // Priority 2: Check slot 13 (if not Cancel)
        if let Some(item) = self.current_slots.get(&13) {
            if let Some(info) = inspect_item(item) {
                let name = info.custom_name.as_deref().unwrap_or("").to_lowercase();
                if !name.contains("cancel") && !info.kind.to_lowercase().contains("terracotta") {
                    if info.kind.to_lowercase().contains("chest") || is_confirm_button(&info) {
                        info!("Identified slot #13 as 'Collect' button ({}, Name: {:?}, Lore: {:?})", info.kind, info.custom_name, info.lore);
                        return Some(13);
                    }
                }
            }
        }

        // Priority 3: Scan ONLY container slots (0..open_container_size or 0..27)
        let max_slot = if self.open_container_size > 0 { self.open_container_size } else { 27 };
        for slot in 0..max_slot {
            if slot == 10 || slot == 13 {
                continue; // 10 is item preview, 13 is cancel
            }
            if let Some(item) = self.current_slots.get(&slot) {
                if let Some(info) = inspect_item(item) {
                    let name = info.custom_name.as_deref().unwrap_or("").to_lowercase();
                    if name.contains("collect") || info.kind.to_lowercase().contains("chest") || is_confirm_button(&info) {
                        info!("Identified slot #{slot} as 'Collect' button via name/kind ({}, Name: {:?}, Lore: {:?})", info.kind, info.custom_name, info.lore);
                        return Some(slot);
                    }
                }
            }
        }

        None
    }

    /// Claim the current order inside 'Orders -> Edit Order'.
    async fn claim_current_order(&mut self, bot: &Client) -> bool {
        let mut item_name = self.target_order_type.clone()
            .or_else(|| self.last_collecting_order_name.clone())
            .unwrap_or_else(|| "Unknown Item".to_string());

        // Check slot 10 in Edit Order to learn the exact item type being claimed
        if let Some(item_slot_10) = self.current_slots.get(&10) {
            if let Some(info) = inspect_item_with_bot(item_slot_10, Some(bot)) {
                let detected = self.get_item_name(&info);
                if !detected.is_empty() && detected != "Unknown Item" {
                    item_name = detected;
                }
                self.target_order_type = Some(item_name.clone());
                self.last_collecting_order_name = Some(item_name.clone());

                info!(
                    "Order details in slot #10: {} x{} (Item: '{}', Name: {:?}, Lore: {:?}, StoredEnch: {:?})",
                    info.kind, info.count, item_name, info.custom_name, info.lore, info.stored_enchantments
                );

                let (needed, _) = self.is_order_needed(&info);
                if !needed {
                    warn!(
                        "Order for '{}' in slot #10 is NOT needed in current phase ({:?}) or quota already met! Closing GUI.",
                        item_name, self.phase
                    );
                    self.close_current_gui(bot);

                    return true;
                }
            }
        }

        if self.collect_clicked_in_submenu {
            // Already clicked collect in this Edit Order submenu; await server response or chat packet
            return false;
        }

        if let Some(collect_slot) = self.find_collect_button_slot() {
            info!("Clicking 'Collect' button at slot #{collect_slot} for order '{item_name}'...");
            self.collect_clicked_in_submenu = true;
            self.stock_audit.collect_requested();
            self.click_slot(bot, collect_slot, ClickType::Pickup);

            return true;
        } else {
            warn!("Could not find Collect button in Edit Order window for '{item_name}'! Logging slots:");
            self.log_all_slot_nbt(Some(bot));
        }

        false
    }

    /// Transfer items from the 'Orders -> Collect Items' delivery menu into player inventory.
    async fn transfer_delivery_items(&mut self, bot: &Client) -> bool {
        info!("Scanning delivery slots in 'Orders -> Collect Items'...");
        let mut sorted_slots: Vec<i16> = self.current_slots.keys().copied().collect();
        sorted_slots.sort();

        for slot in sorted_slots {
            // Only transfer from top delivery container (slots 0..=26)
            if slot > 26 {
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

                    let (is_needed, name) = self.is_order_needed(&info);
                    if !is_needed {
                        info!(
                            "Skipping delivery item in slot #{slot}: {} x{} (quota already met or not needed in Phase {:?})",
                            info.kind, info.count, self.phase
                        );
                        // Other slots may still contain required supplies.
                        continue;
                    }

                    if is_anvil(&info) {
                        self.throw_one_anvil_at_feet(bot, slot);
                        return false;
                    }

                    if is_xp_bottle(&info) && self.collected.xp_stacks > self.quota.xp_stacks_needed {
                        info!(
                            "Skipping delivery XP bottles in slot #{slot}: already holding {} stack(s) of XP (quota: {})",
                            self.collected.xp_stacks, self.quota.xp_stacks_needed
                        );
                        continue;
                    }

                    if self.count_free_inventory_slots() == 0 {
                        info!("Player inventory is full (0 free slots). Stopping item collection.");
                        break;
                    }

                    info!(
                        "Transferring delivery item from slot #{slot}: {} ({name}) x{} to inventory...",
                        info.kind, info.count
                    );
                    self.record_item_collected(&info);
                    self.click_slot(bot, slot, ClickType::QuickMove);
                    self.current_menu_skipped_slots.clear();
                    // Wait for both source and player inventory updates before deciding quota.
                    return false;
                }
            }
        }

        if let Some(ref order_type) = self.target_order_type {
            info!("Finished claiming delivery items for order '{order_type}'!");
            self.stock_audit.collection_finished();
        }
        self.target_order_type = None;
        self.last_collecting_order_name = None;

        info!("Closing delivery container...");
        self.close_current_gui(bot);

        let phase_done = match self.phase {
            WithdrawalPhase::AnvilPlacement => self.anvil_withdrawal_complete(),
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
        }

        // If inventory is full (0 free slots) and craftable sets exist, proceed to enchant!
        if self.phase == WithdrawalPhase::ItemsRetrieval && self.count_free_inventory_slots() == 0 {
            let craftable_sets = self.count_craftable_god_sets(Some(bot));
            if craftable_sets > 0 {
                info!(
                    "Player inventory is full (0 free slots), but contains materials for {} COMPLETE God-armor set(s)! Proceeding to enchant to free up inventory space...",
                    craftable_sets
                );
                self.phase = WithdrawalPhase::Done;
                self.state = OrderWorkflowState::WithdrawalComplete;
                return true;
            } else if self.has_any_craftable_combines(Some(bot)) {
                info!(
                    "Player inventory is full (0 free slots), but contains materials to enchant partial armor pieces! Proceeding to enchant to free up inventory space..."
                );
                self.phase = WithdrawalPhase::Done;
                self.state = OrderWorkflowState::WithdrawalComplete;
                return true;
            } else {
                warn!("Player inventory is full (0 free slots) and cannot craft complete sets. Pausing for 45s restock cooldown...");
                self.close_current_gui(bot);
                self.state = OrderWorkflowState::WaitingForNextOrder;
                self.is_waiting_for_restock = true;
                self.restock_wait_until = Some(std::time::Instant::now() + std::time::Duration::from_secs(45));
                self.schedule_command("/order".to_string(), std::time::Duration::from_millis(45000));
                return true;
            }
        }

        info!("Phase {:?} not yet fulfilled; opening the next order.", self.phase);
        self.state = OrderWorkflowState::WaitingForNextOrder;
        info!("Sending /order for next item...");
        self.resume_order_scan = true;
        self.record_command_sent("/order");
        Self::send_command(bot, "/order");
        return true;
    }

    /// Counts needed diamond armor listings that have not individually been confirmed empty.
    pub fn count_available_armor_orders(&self, bot: Option<&Client>) -> u32 {
        let mut count = 0;
        for slot in 0..=44 {
            if let Some(item) = self.current_slots.get(&slot) {
                if let Some(info) = inspect_item_with_bot(item, bot) {
                    if is_diamond_armor(&info) {
                        let (needed, name) = self.is_order_needed(&info);
                        if needed && self.order_listing(slot, bot).is_some_and(|listing|
                            listing.name.as_str() == name && !self.stock_audit.is_empty(self.order_page, &listing)) {
                            count += 1;
                        }
                    }
                }
            }
        }
        count
    }

    /// Checks whether the bot has enough materials (armor, matching books, anvils, and XP)
    /// in inventory to fully enchant at least one COMPLETE 4-piece God-armor set
    /// (1× Diamond Helmet, 1× Diamond Chestplate, 1× Diamond Leggings, 1× Diamond Boots).
    pub fn can_craft_god_armor(&self, bot: Option<&Client>) -> bool {
        self.count_craftable_god_sets(bot) > 0
    }

    /// Checks whether there is at least one armor piece in inventory that can be combined
    /// with an available enchanted book and XP/anvil. Used to break inventory deadlock.
    pub fn has_any_craftable_combines(&self, bot: Option<&Client>) -> bool {
        let has_anvil = self.collected.anvils > 0
            || self.player_inventory.get(&45).and_then(|item| inspect_item_with_bot(item, bot)).map(|info| is_anvil(&info) && info.count > 0).unwrap_or(false)
            || self.phase == WithdrawalPhase::ItemsRetrieval
            || self.phase == WithdrawalPhase::Done;

        if !has_anvil {
            return false;
        }

        let has_sufficient_xp = self.collected.xp_bottles >= 10
            || self.collected.xp_stacks > 0
            || bot.map_or(false, |b| b.experience().level >= 4);

        if !has_sufficient_xp {
            return false;
        }

        for (&slot, item) in self.player_inventory.iter() {
            if !(9..=44).contains(&slot) {
                continue;
            }
            if let Some(info) = inspect_item_with_bot(item, bot) {
                if is_diamond_helmet(&info) || is_diamond_chestplate(&info) {
                    if !info.has_enchantment("protection", 4) && self.collected.protection_4 > 0 {
                        return true;
                    }
                    if !info.has_enchantment("unbreaking", 3) && self.collected.unbreaking_3 > 0 {
                        return true;
                    }
                    if !info.has_enchantment("mending", 1) && self.collected.mending > 0 {
                        return true;
                    }
                } else if is_diamond_leggings(&info) || is_diamond_boots(&info) {
                    if !info.has_enchantment("blast_protection", 4) && self.collected.blast_protection_4 > 0 {
                        return true;
                    }
                    if !info.has_enchantment("unbreaking", 3) && self.collected.unbreaking_3 > 0 {
                        return true;
                    }
                    if !info.has_enchantment("mending", 1) && self.collected.mending > 0 {
                        return true;
                    }
                }
            }
        }

        false
    }

    /// Computes how many complete 4-piece God-armor sets can be crafted from available inventory.
    /// Evaluates existing armor in inventory (accounting for already applied enchantments),
    /// sorts candidates by fewest missing books, and matches against available books.
    pub fn count_craftable_god_sets(&self, bot: Option<&Client>) -> usize {
        let has_anvil = self.collected.anvils > 0
            || self.player_inventory.get(&45).and_then(|item| inspect_item_with_bot(item, bot)).map(|info| is_anvil(&info) && info.count > 0).unwrap_or(false)
            || self.phase == WithdrawalPhase::ItemsRetrieval
            || self.phase == WithdrawalPhase::Done;

        if !has_anvil {
            return 0;
        }

        #[derive(Default, Clone)]
        struct MissingBooks {
            prot_4: u32,
            blast_4: u32,
            unb_3: u32,
            mending: u32,
        }

        impl MissingBooks {
            fn total(&self) -> u32 {
                self.prot_4 + self.blast_4 + self.unb_3 + self.mending
            }
        }

        let mut helms: Vec<MissingBooks> = Vec::new();
        let mut chests: Vec<MissingBooks> = Vec::new();
        let mut legs: Vec<MissingBooks> = Vec::new();
        let mut boots: Vec<MissingBooks> = Vec::new();

        for (&slot, item) in self.player_inventory.iter() {
            if !(9..=44).contains(&slot) {
                continue;
            }
            if let Some(info) = inspect_item_with_bot(item, bot) {
                if is_diamond_helmet(&info) {
                    helms.push(MissingBooks {
                        prot_4: if info.has_enchantment("protection", 4) { 0 } else { 1 },
                        blast_4: 0,
                        unb_3: if info.has_enchantment("unbreaking", 3) { 0 } else { 1 },
                        mending: if info.has_enchantment("mending", 1) { 0 } else { 1 },
                    });
                } else if is_diamond_chestplate(&info) {
                    chests.push(MissingBooks {
                        prot_4: if info.has_enchantment("protection", 4) { 0 } else { 1 },
                        blast_4: 0,
                        unb_3: if info.has_enchantment("unbreaking", 3) { 0 } else { 1 },
                        mending: if info.has_enchantment("mending", 1) { 0 } else { 1 },
                    });
                } else if is_diamond_leggings(&info) {
                    legs.push(MissingBooks {
                        prot_4: 0,
                        blast_4: if info.has_enchantment("blast_protection", 4) { 0 } else { 1 },
                        unb_3: if info.has_enchantment("unbreaking", 3) { 0 } else { 1 },
                        mending: if info.has_enchantment("mending", 1) { 0 } else { 1 },
                    });
                } else if is_diamond_boots(&info) {
                    boots.push(MissingBooks {
                        prot_4: 0,
                        blast_4: if info.has_enchantment("blast_protection", 4) { 0 } else { 1 },
                        unb_3: if info.has_enchantment("unbreaking", 3) { 0 } else { 1 },
                        mending: if info.has_enchantment("mending", 1) { 0 } else { 1 },
                    });
                }
            }
        }

        // If player_inventory is empty or has fewer items than self.collected, fill in default unenchanted armor
        while helms.len() < self.collected.diamond_helmets as usize {
            helms.push(MissingBooks { prot_4: 1, blast_4: 0, unb_3: 1, mending: 1 });
        }
        while chests.len() < self.collected.diamond_chestplates as usize {
            chests.push(MissingBooks { prot_4: 1, blast_4: 0, unb_3: 1, mending: 1 });
        }
        while legs.len() < self.collected.diamond_leggings as usize {
            legs.push(MissingBooks { prot_4: 0, blast_4: 1, unb_3: 1, mending: 1 });
        }
        while boots.len() < self.collected.diamond_boots as usize {
            boots.push(MissingBooks { prot_4: 0, blast_4: 1, unb_3: 1, mending: 1 });
        }

        // Clamp to self.collected limits if specified
        if (self.collected.diamond_helmets as usize) < helms.len() {
            helms.truncate(self.collected.diamond_helmets as usize);
        }
        if (self.collected.diamond_chestplates as usize) < chests.len() {
            chests.truncate(self.collected.diamond_chestplates as usize);
        }
        if (self.collected.diamond_leggings as usize) < legs.len() {
            legs.truncate(self.collected.diamond_leggings as usize);
        }
        if (self.collected.diamond_boots as usize) < boots.len() {
            boots.truncate(self.collected.diamond_boots as usize);
        }

        let max_possible_sets = helms.len().min(chests.len()).min(legs.len()).min(boots.len());
        if max_possible_sets == 0 {
            return 0;
        }

        // Sort each category by fewest missing books first
        helms.sort_by_key(|m| m.total());
        chests.sort_by_key(|m| m.total());
        legs.sort_by_key(|m| m.total());
        boots.sort_by_key(|m| m.total());

        // Need sufficient XP to complete anvil combines:
        let has_sufficient_xp = self.collected.xp_bottles >= 10
            || self.collected.xp_stacks > 0
            || bot.map_or(false, |b| b.experience().level >= 4);

        for s in (1..=max_possible_sets).rev() {
            let mut req_prot_4 = 0;
            let mut req_blast_4 = 0;
            let mut req_unb_3 = 0;
            let mut req_mending = 0;

            for m in helms.iter().take(s) {
                req_prot_4 += m.prot_4;
                req_unb_3 += m.unb_3;
                req_mending += m.mending;
            }
            for m in chests.iter().take(s) {
                req_prot_4 += m.prot_4;
                req_unb_3 += m.unb_3;
                req_mending += m.mending;
            }
            for m in legs.iter().take(s) {
                req_blast_4 += m.blast_4;
                req_unb_3 += m.unb_3;
                req_mending += m.mending;
            }
            for m in boots.iter().take(s) {
                req_blast_4 += m.blast_4;
                req_unb_3 += m.unb_3;
                req_mending += m.mending;
            }

            let total_combines_needed = req_prot_4 + req_blast_4 + req_unb_3 + req_mending;
            if total_combines_needed > 0 && !has_sufficient_xp {
                continue;
            }

            if self.collected.protection_4 >= req_prot_4
                && self.collected.blast_protection_4 >= req_blast_4
                && self.collected.unbreaking_3 >= req_unb_3
                && self.collected.mending >= req_mending
            {
                return s;
            }
        }

        0
    }

    /// Counts how many completed, 4-piece max-enchanted god armor sets (1 Helm, 1 Chest, 1 Legs, 1 Boots)
    /// exist in player inventory.
    pub fn count_completed_max_sets(&self, bot: Option<&Client>) -> usize {
        let mut helms = 0;
        let mut chests = 0;
        let mut legs = 0;
        let mut boots = 0;

        for (_, item) in self.player_inventory.iter().filter(|(slot, _)| (9..=44).contains(*slot)) {
            if let Some(info) = inspect_item_with_bot(item, bot) {
                if is_diamond_helmet(&info)
                    && info.has_enchantment("protection", 4)
                    && info.has_enchantment("unbreaking", 3)
                    && info.has_enchantment("mending", 1)
                {
                    helms += 1;
                } else if is_diamond_chestplate(&info)
                    && info.has_enchantment("protection", 4)
                    && info.has_enchantment("unbreaking", 3)
                    && info.has_enchantment("mending", 1)
                {
                    chests += 1;
                } else if is_diamond_leggings(&info)
                    && info.has_enchantment("blast_protection", 4)
                    && info.has_enchantment("unbreaking", 3)
                    && info.has_enchantment("mending", 1)
                {
                    legs += 1;
                } else if is_diamond_boots(&info)
                    && info.has_enchantment("blast_protection", 4)
                    && info.has_enchantment("unbreaking", 3)
                    && info.has_enchantment("mending", 1)
                {
                    boots += 1;
                }
            }
        }

        helms.min(chests).min(legs).min(boots)
    }

    /// Returns true if all authorized full sets have been delivered to the buyer.
    pub fn is_target_delivery_completed(&self) -> bool {
        if self.target_sets_to_deliver == 0 {
            return false;
        }
        self.target_delivered_helmets >= self.target_sets_to_deliver
            && self.target_delivered_chestplates >= self.target_sets_to_deliver
            && self.target_delivered_leggings >= self.target_sets_to_deliver
            && self.target_delivered_boots >= self.target_sets_to_deliver
    }

    fn armor_matches(info: &ItemInfo, armor: TargetArmorType) -> bool {
        let (kind_matches, protection) = match armor {
            TargetArmorType::Helmet => (is_diamond_helmet(info), "protection"),
            TargetArmorType::Chestplate => (is_diamond_chestplate(info), "protection"),
            TargetArmorType::Leggings => (is_diamond_leggings(info), "blast_protection"),
            TargetArmorType::Boots => (is_diamond_boots(info), "blast_protection"),
        };
        kind_matches && info.has_enchantment(protection, 4)
            && info.has_enchantment("unbreaking", 3) && info.has_enchantment("mending", 1)
    }

    fn armor_count(&self, armor: TargetArmorType, bot: Option<&Client>) -> usize {
        self.player_inventory.iter().filter(|(slot, item)| {
            (9..=44).contains(*slot) && inspect_item_with_bot(item, bot)
                .is_some_and(|info| Self::armor_matches(&info, armor))
        }).count()
    }

    // Only a fresh order-menu snapshot after closing the transaction can settle it.
    // The inventory loss in the deposit/confirmation screen is merely escrow.
    fn reconcile_delivery(&mut self, bot: Option<&Client>) {
        let Some(pending) = self.pending_delivery.clone() else { return; };
        let remaining = self.armor_count(pending.armor, bot);
        self.reconcile_delivery_count(remaining);
    }

    fn reconcile_delivery_count(&mut self, remaining: usize) {
        let Some(pending) = self.pending_delivery.clone() else { return; };
        if pending.confirm_sent && remaining.checked_add(1) == Some(pending.inventory_count_before) {
            match pending.armor {
                TargetArmorType::Helmet => self.target_delivered_helmets += 1,
                TargetArmorType::Chestplate => self.target_delivered_chestplates += 1,
                TargetArmorType::Leggings => self.target_delivered_leggings += 1,
                TargetArmorType::Boots => self.target_delivered_boots += 1,
            }
            info!("Verified {:?} delivery from post-confirmation inventory", pending.armor);
        } else if remaining != pending.inventory_count_before {
            error!("Ambiguous {:?} delivery inventory: before {}, now {}. Holding batch for reconciliation.",
                pending.armor, pending.inventory_count_before, remaining);
            return;
        } else {
            warn!("Delivery was not accepted; item is back in inventory and can be retried.");
        }
        self.pending_delivery = None;
        self.target_delivering_armor = None;
        self.clear_watchdog();
    }

    /// Automatically fulfills buyer orders in the target player's /order {player} GUI.
    pub async fn process_fill_target_order(&mut self, bot: &Client) -> bool {
        if self.pending_delivery.is_some() {
            return false;
        }
        self.target_delivering_armor = None;
        if let Some(last_click) = self.last_target_order_click {
            let elapsed = last_click.elapsed();
            if elapsed < std::time::Duration::from_millis(1500) {
                // Keep the command cooldown without blocking packet processing.
                return false;
            }
        }

        self.sync_collected_from_inventory(Some(bot));

        // Audit player inventory to see which max-enchanted armor pieces are present
        let mut has_helm = false;
        let mut has_chest = false;
        let mut has_legs = false;
        let mut has_boots = false;

        for item in self.player_inventory.values() {
            if let Some(info) = inspect_item_with_bot(item, Some(bot)) {
                if is_diamond_helmet(&info)
                    && info.has_enchantment("protection", 4)
                    && info.has_enchantment("unbreaking", 3)
                    && info.has_enchantment("mending", 1)
                {
                    has_helm = true;
                } else if is_diamond_chestplate(&info)
                    && info.has_enchantment("protection", 4)
                    && info.has_enchantment("unbreaking", 3)
                    && info.has_enchantment("mending", 1)
                {
                    has_chest = true;
                } else if is_diamond_leggings(&info)
                    && info.has_enchantment("blast_protection", 4)
                    && info.has_enchantment("unbreaking", 3)
                    && info.has_enchantment("mending", 1)
                {
                    has_legs = true;
                } else if is_diamond_boots(&info)
                    && info.has_enchantment("blast_protection", 4)
                    && info.has_enchantment("unbreaking", 3)
                    && info.has_enchantment("mending", 1)
                {
                    has_boots = true;
                }
            }
        }

        info!(
            "Fulfilling orders for '{}'. Enchanted pieces in inventory: Helm: {has_helm}, Chest: {has_chest}, Legs: {has_legs}, Boots: {has_boots}",
            self.order_target
        );

        // Scan slots for target player's buy orders (strictly upper order slots, never player inventory)
        let max_order_slot = if self.open_container_size > 0 {
            (self.open_container_size - 9).min(44)
        } else {
            44
        };

        // Audit all buy orders found in GUI
        info!("=== Buy Orders Audit for '{}' ===", self.order_target);
        for slot in 0..=max_order_slot {
            if let Some(item) = self.current_slots.get(&slot) {
                if let Some(info) = inspect_item_with_bot(item, Some(bot)) {
                    if !info.kind.to_lowercase().contains("glasspane") && !info.kind.to_lowercase().contains("barrier") {
                        let delivered = info.lore.iter()
                            .find(|l| l.to_lowercase().contains("delivered"))
                            .cloned()
                            .unwrap_or_else(|| "N/A".to_string());
                        info!("  Order Slot #{slot}: '{}' (Name: {:?}) -> Progress: [{delivered}]", info.kind, info.custom_name);
                    }
                }
            }
        }
        info!("=============================================");

        let next_set = self.target_delivered_helmets.min(self.target_delivered_chestplates)
            .min(self.target_delivered_leggings).min(self.target_delivered_boots);
        let can_deliver_helm = has_helm && self.target_delivered_helmets < self.target_sets_to_deliver && self.target_delivered_helmets == next_set;
        let can_deliver_chest = has_chest && self.target_delivered_chestplates < self.target_sets_to_deliver && self.target_delivered_chestplates == next_set;
        let can_deliver_legs = has_legs && self.target_delivered_leggings < self.target_sets_to_deliver && self.target_delivered_leggings == next_set;
        let can_deliver_boots = has_boots && self.target_delivered_boots < self.target_sets_to_deliver && self.target_delivered_boots == next_set;

        if !can_deliver_helm && !can_deliver_chest && !can_deliver_legs && !can_deliver_boots {
            if !self.is_target_delivery_completed() {
                warn!("Authorized set is incomplete; holding delivery instead of starting another batch.");
                return false;
            }
            info!("All authorized full sets ({} set(s)) have been delivered to {}!", self.target_sets_to_deliver, self.order_target);
            self.close_current_gui(bot);
            self.is_fulfilling_target = false;
            self.state = OrderWorkflowState::WaitingForNextOrder;
            return true;
        }

        for slot in 0..=max_order_slot {
            if let Some(item) = self.current_slots.get(&slot) {
                if let Some(info) = inspect_item_with_bot(item, Some(bot)) {
                    if info.kind.to_lowercase().contains("glasspane")
                        || info.kind.to_lowercase().contains("barrier")
                    {
                        continue;
                    }

                    let kind_lower = info.kind.to_lowercase();
                    let name_lower = info.custom_name.as_deref().unwrap_or("").to_lowercase();
                    let lore_has = |term: &str| info.lore.iter().any(|l| l.to_lowercase().contains(term));

                    let is_helm = can_deliver_helm
                        && (is_diamond_helmet(&info)
                            || kind_lower.contains("helmet")
                            || name_lower.contains("helmet")
                            || lore_has("helmet"));
                    let is_chest = !is_helm
                        && can_deliver_chest
                        && (is_diamond_chestplate(&info)
                            || kind_lower.contains("chestplate")
                            || name_lower.contains("chestplate")
                            || kind_lower.contains("chest")
                            || name_lower.contains("chest")
                            || lore_has("chestplate"));
                    let is_legs = !is_helm
                        && !is_chest
                        && can_deliver_legs
                        && (is_diamond_leggings(&info)
                            || kind_lower.contains("leggings")
                            || name_lower.contains("leggings")
                            || kind_lower.contains("legs")
                            || name_lower.contains("legs")
                            || lore_has("leggings"));
                    let is_boots = !is_helm
                        && !is_chest
                        && !is_legs
                        && can_deliver_boots
                        && (is_diamond_boots(&info)
                            || kind_lower.contains("boots")
                            || name_lower.contains("boots")
                            || lore_has("boots"));

                    if is_helm || is_chest || is_legs || is_boots {
                        let (armor_name, armor_type) = if is_helm {
                            ("Diamond Helmet", TargetArmorType::Helmet)
                        } else if is_chest {
                            ("Diamond Chestplate", TargetArmorType::Chestplate)
                        } else if is_legs {
                            ("Diamond Leggings", TargetArmorType::Leggings)
                        } else {
                            ("Diamond Boots", TargetArmorType::Boots)
                        };

                        info!(
                            "Found buy order for '{armor_name}' at slot #{slot} (Item: '{}', Name: {:?}, Lore: {:?})! Clicking to fulfill...",
                            info.kind, info.custom_name, info.lore
                        );
                        self.target_delivering_armor = Some(armor_type);
                        self.last_target_order_click = Some(std::time::Instant::now());
                        self.click_slot(bot, slot, ClickType::Pickup);
                        // Remain in FillingTargetOrders until on_open_screen sees 'Orders -> Deliver Items'
                        self.state = OrderWorkflowState::FillingTargetOrders;
                        return true;
                    }
                }
            }
        }

        // Check if there is a next page arrow at slot 53
        if let Some(item53) = self.current_slots.get(&53) {
            if let Some(info53) = inspect_item_with_bot(item53, Some(bot)) {
                if info53.kind.to_lowercase().contains("arrow") {
                    info!("Checking next page in orders (clicking slot #53)...");
                    self.click_slot(bot, 53, ClickType::Pickup);
                    return true;
                }
            }
        }

        warn!(
            "No more matching buy orders found for remaining armors in {}'s order menu! Closing GUI.",
            self.order_target
        );
        self.close_current_gui(bot);
        self.state = OrderWorkflowState::WaitingForNextOrder;
        true
    }

    /// Deposits matching max-enchanted armor piece(s) into the 'Orders -> Deliver Items' chest GUI.
    pub async fn process_deposit_target_items(&mut self, bot: &Client) -> bool {
        if let Some(pending) = self.pending_delivery.as_mut() {
            if pending.deposit_observed || pending.confirm_sent {
                return false;
            }
            let source_empty = matches!(self.current_slots.get(&pending.source_slot), Some(ItemStack::Empty));
            let in_chest = self.current_slots.iter().any(|(slot, item)| {
                *slot < self.open_container_size && inspect_item_with_bot(item, Some(bot))
                    .is_some_and(|info| Self::armor_matches(&info, pending.armor))
            });
            if source_empty && in_chest {
                pending.deposit_observed = true;
                self.close_current_gui(bot);
                self.record_action_sent(None, None);
            }
            return false;
        }
        if self.target_delivering_armor.is_none() || self.open_container_size == 0 {
            return false;
        }
        if self.open_container_size > 36 {
            warn!(
                "process_deposit_target_items invoked on non-delivery container #{} (size: {}). Ignoring.",
                self.current_container_id, self.open_container_size
            );
            return false;
        }

        info!(
            "In delivery deposit GUI (container #{}, open_size: {}). Scanning inventory slots for matching armor ({:?})...",
            self.current_container_id, self.open_container_size, self.target_delivering_armor
        );

        let target_type = self.target_delivering_armor;
        let mut matching_slots = Vec::new();

        // Slots in the open container that belong to the player inventory (slot >= open_container_size)
        let inv_start = if self.open_container_size > 0 {
            self.open_container_size
        } else {
            36
        };

        for (&slot, item) in &self.current_slots {
            if slot < inv_start {
                continue;
            }
            if let Some(info) = inspect_item_with_bot(item, Some(bot)) {
                let is_match = match target_type {
                    Some(TargetArmorType::Helmet) => {
                        is_diamond_helmet(&info)
                            && info.has_enchantment("protection", 4)
                            && info.has_enchantment("unbreaking", 3)
                            && info.has_enchantment("mending", 1)
                    }
                    Some(TargetArmorType::Chestplate) => {
                        is_diamond_chestplate(&info)
                            && info.has_enchantment("protection", 4)
                            && info.has_enchantment("unbreaking", 3)
                            && info.has_enchantment("mending", 1)
                    }
                    Some(TargetArmorType::Leggings) => {
                        is_diamond_leggings(&info)
                            && info.has_enchantment("blast_protection", 4)
                            && info.has_enchantment("unbreaking", 3)
                            && info.has_enchantment("mending", 1)
                    }
                    Some(TargetArmorType::Boots) => {
                        is_diamond_boots(&info)
                            && info.has_enchantment("blast_protection", 4)
                            && info.has_enchantment("unbreaking", 3)
                            && info.has_enchantment("mending", 1)
                    }
                    None => {
                        (is_diamond_helmet(&info) && info.has_enchantment("protection", 4) && info.has_enchantment("unbreaking", 3) && info.has_enchantment("mending", 1))
                            || (is_diamond_chestplate(&info) && info.has_enchantment("protection", 4) && info.has_enchantment("unbreaking", 3) && info.has_enchantment("mending", 1))
                            || (is_diamond_leggings(&info) && info.has_enchantment("blast_protection", 4) && info.has_enchantment("unbreaking", 3) && info.has_enchantment("mending", 1))
                            || (is_diamond_boots(&info) && info.has_enchantment("blast_protection", 4) && info.has_enchantment("unbreaking", 3) && info.has_enchantment("mending", 1))
                    }
                };

                if is_match {
                    matching_slots.push((slot, info.kind.clone()));
                }
            }
        }

        if matching_slots.is_empty() {
            warn!("No matching max-enchanted armor found in inventory to deposit! Closing GUI.");
            self.target_delivering_armor = None;
            self.close_current_gui(bot);
            self.state = OrderWorkflowState::WaitingForNextOrder;
            return true;
        }

        // Fulfill one armor type at a time: deposit exactly one matching piece
        if let Some((slot, kind)) = matching_slots.into_iter().next() {
            let armor = self.target_delivering_armor.unwrap();
            self.pending_delivery = Some(PendingDelivery {
                armor,
                inventory_count_before: self.armor_count(armor, Some(bot)),
                source_slot: slot,
                deposit_observed: false,
                confirm_sent: false,
            });
            info!("Depositing single '{kind}' from inventory slot #{slot} via QuickMove...");
            self.click_slot(bot, slot, ClickType::QuickMove);
        }
        true
    }

    /// Click confirm/deliver button if order fulfillment opens a confirmation screen.
    pub async fn confirm_target_order_fulfill(&mut self, bot: &Client) -> bool {
        if !self.pending_delivery.as_ref().is_some_and(|p| p.deposit_observed && !p.confirm_sent) {
            return false;
        }
        info!("Scanning confirmation screen for confirm/deliver button...");
        for (&slot, item) in &self.current_slots {
            if slot >= self.open_container_size && self.open_container_size > 0 {
                continue;
            }
            if let Some(info) = inspect_item_with_bot(item, Some(bot)) {
                let name = info.custom_name.as_deref().unwrap_or("").to_lowercase();
                let lore = info.lore.join(" ").to_lowercase();
                let kind = info.kind.to_lowercase();

                // Never click cancel or navigation items
                if kind.contains("red") || name.contains("cancel") || lore.contains("cancel") {
                    continue;
                }
                if kind.contains("book") || kind.contains("hopper") || kind.contains("sign") || kind.contains("shard") || kind.contains("chest") {
                    continue;
                }
                if name.contains("orders") || name.contains("filter") || name.contains("search") || name.contains("shop") {
                    continue;
                }

                if is_confirm_button(&info)
                    || kind.contains("lime")
                    || kind.contains("green")
                    || kind.contains("emerald")
                    || name.contains("confirm")
                    || name.contains("deliver")
                    || name.contains("fulfill")
                    || name.contains("accept")
                    || name.contains("yes")
                    || lore.contains("click to confirm")
                    || lore.contains("click to deliver")
                    || lore.contains("click to fulfill")
                    || lore.contains("confirm delivery")
                {
                    info!("Found confirmation/deliver button at slot #{slot} ('{name}', kind: {kind})! Clicking to complete delivery...");
                    self.click_slot(bot, slot, ClickType::Pickup);
                    self.pending_delivery.as_mut().unwrap().confirm_sent = true;
                    // Keep the armor and transaction until a fresh post-confirmation
                    // order-menu inventory snapshot verifies acceptance or return.
                    return true;
                }
            }
        }

        warn!("Could not identify confirmation button in container #{}. Closing all GUI and retrying in 10 ticks...", self.current_container_id);
        self.log_all_slot_nbt(Some(bot));
        self.close_current_gui(bot);
        self.state = OrderWorkflowState::WaitingForNextOrder;
        let target_name = self.order_target.clone();
        self.schedule_command(format!("/order {target_name}"), std::time::Duration::from_millis(500));
        false
    }

    /// Counts how many max-enchanted armor pieces exist in player inventory.
    pub fn count_max_enchanted_armors(&self, bot: Option<&Client>) -> usize {
        let mut count = 0;
        for item in self.player_inventory.values() {
            if let Some(info) = inspect_item_with_bot(item, bot) {
                if crate::armor::is_complete(&info) {
                    count += 1;
                }
            }
        }
        count
    }

    /// Click a slot by sending a ServerboundContainerClick packet and recording watchdog timestamp.
    pub fn click_slot(&mut self, bot: &Client, slot: i16, click_type: ClickType) {
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
        self.record_action_sent(Some(slot), Some(click_type));
        if click_type == ClickType::QuickMove {
            self.transfer_before = Some((self.current_slots.get(&slot).cloned().unwrap_or(ItemStack::Empty), self.player_inventory.clone()));
        }
    }

    pub fn anvil_withdrawal_complete(&self) -> bool {
        self.pending_anvil_pickup.is_none() && self.collected.anvils >= self.quota.anvils_needed
    }

    /// A source slot shrinking only confirms the throw, not that we picked it up.
    fn reconcile_anvil_pickup(&mut self) {
        if self.pending_anvil_pickup.as_ref().is_some_and(|pending|
            self.collected.anvils > pending.inventory_before) {
            self.pending_anvil_pickup = None;
            if self.state == OrderWorkflowState::InCollectDeliveryMenu {
                if let Some(ref order_type) = self.target_order_type {
                    self.exhausted_orders.retain(|x| x != order_type);
                }
            }
            self.clear_watchdog();
            self.last_progress_time = std::time::Instant::now();
            info!("Server inventory confirmed anvil pickup at feet.");
            if self.current_container_id == 0 {
                self.schedule_command("/order".to_string(), std::time::Duration::ZERO);
            }
        }
    }

    /// Send the downward rotation before throwing a single anvil, then await pickup.
    fn throw_one_anvil_at_feet(&mut self, bot: &Client, slot: i16) -> bool {
        use azalea::entity::{LookDirection, Physics, inventory::Inventory};
        use azalea::protocol::{common::movements::MoveFlags, packets::game::ServerboundMovePlayerRot};
        if self.pending_anvil_pickup.is_some() || self.current_container_id <= 0
            || self.state != OrderWorkflowState::InCollectDeliveryMenu
            || self.phase != WithdrawalPhase::AnvilPlacement
            || !(0..self.open_container_size).contains(&slot)
            || self.collected.anvils >= self.quota.anvils_needed
            || !self.current_slots.get(&slot).and_then(|item| inspect_item_with_bot(item, Some(bot)))
                .is_some_and(|info| is_anvil(&info) && info.count > 0)
            || !bot.get_component::<Inventory>().is_some_and(|inv|
                inv.id == self.current_container_id && inv.carried == ItemStack::Empty) {
            return false;
        }
        let Some(flags) = bot.get_component::<Physics>().map(|physics| MoveFlags {
            on_ground: physics.on_ground(), horizontal_collision: physics.horizontal_collision,
        }) else { return false; };
        let yaw = bot.direction().y_rot();
        bot.set_direction(yaw, 90.0);
        bot.write_packet(ServerboundMovePlayerRot {
            look_direction: LookDirection::new(yaw, 90.0),
            flags,
        });
        self.pending_anvil_pickup = Some(PendingAnvilPickup {
            inventory_before: self.collected.anvils,
            sent_at: std::time::Instant::now(),
            warned: false,
        });
        bot.write_packet(ServerboundContainerClick {
            container_id: self.current_container_id,
            state_id: self.current_state_id,
            slot_num: slot,
            button_num: 0, // One anvil, never the whole order stack.
            click_type: ClickType::Throw,
            changed_slots: Default::default(),
            carried_item: HashedStack(None),
        });
        self.record_action_sent(Some(slot), Some(ClickType::Throw));
        info!("Threw one anvil at feet; waiting for server-confirmed pickup.");
        true
    }

    /// Translate a verified player item into the active hopper's slot namespace.
    /// Only single completed armor pieces and rejected enchanted books may be deposited.
    pub fn hopper_transfer_slot(&self, slot: i16, expected: &ItemStack, bot: Option<&Client>) -> Option<i16> {
        use azalea_registry::builtin::ItemKind;
        let ItemStack::Present(data) = expected else { return None; };
        if data.count != 1 || !(9..=44).contains(&slot)
            || !self.hopper_active || self.current_container_id <= 0
            || self.hopper_container_id != Some(self.current_container_id)
            || self.open_container_size != 5 || self.current_slots.len() != 41
        {
            return None;
        }
        let allowed = data.kind == ItemKind::EnchantedBook
            || inspect_item_with_bot(expected, bot).is_some_and(|info| crate::armor::is_complete(&info));
        let hopper_slot = slot - 9 + 5;
        (allowed && self.player_inventory.get(&slot) == Some(expected)
            && self.current_slots.get(&hopper_slot) == Some(expected))
            .then_some(hopper_slot)
    }

    /// Close the currently opened GUI container.
    pub fn close_current_gui(&mut self, bot: &Client) {
        self.clear_watchdog();
        if self.current_container_id > 0 {
            if bot.get_component::<azalea::entity::inventory::Inventory>()
                .is_some_and(|inv| inv.id == self.current_container_id) {
                bot.ecs.write().trigger(azalea::inventory::CloseContainerEvent {
                    entity: bot.entity,
                    id: self.current_container_id,
                });
            } else {
                bot.write_packet(ServerboundContainerClose { container_id: self.current_container_id });
            }
            self.current_container_id = 0;
            self.current_slots.clear();
        }
        self.hopper_container_id = None;
    }

    /// Wall-clock watchdog: refresh stale screens instead of replaying non-idempotent clicks.
    pub async fn check_and_handle_timeout(&mut self, bot: &Client) -> bool {
        if let Some(pending) = &mut self.pending_anvil_pickup {
            if !pending.warned && pending.sent_at.elapsed() >= std::time::Duration::from_secs(10) {
                error!("Anvil pickup unconfirmed after 10s. Withdrawal paused; check the ground/hopper and return the anvil to inventory. No additional anvils will be thrown.");
                pending.warned = true;
            }
            return false;
        }
        if self.hopper_active || self.state == OrderWorkflowState::WithdrawalComplete
            || self.state == OrderWorkflowState::WaitingForSpawn
        {
            return false;
        }
        if let Some((due, command)) = self.scheduled_command.clone() {
            if std::time::Instant::now() >= due {
                self.is_waiting_for_restock = false;
                self.prepare_to_send_command(bot, &command);
                return true;
            }
            return false;
        }
        if self.is_waiting_for_restock {
            return false;
        }
        if !self.awaiting_response {
            if self.current_container_id == 0 || self.last_progress_time.elapsed() < std::time::Duration::from_secs(3) {
                return false;
            }
            self.awaiting_response = true;
            self.last_action_time = Some(self.last_progress_time);
        }
        let Some(last) = self.last_action_time else { return false; };
        let timeout = std::time::Duration::from_millis(6000 * (1 + self.action_retry_count as u64));
        if last.elapsed() < timeout {
            return false;
        }
        if self.action_retry_count >= 3 {
            error!("GUI failed after three refresh attempts; disconnecting without marking batch complete.");
            self.clear_watchdog();
            bot.disconnect();
            return true;
        }
        let retries = self.action_retry_count + 1;
        let was_selling = self.state == OrderWorkflowState::SellingInventory;
        let command = if was_selling {
            "/sell".to_string()
        } else if self.is_fulfilling_target {
            format!("/order {}", self.order_target)
        } else {
            self.last_command_sent.clone().unwrap_or_else(|| "/order".to_string())
        };
        warn!("GUI stalled in {:?}; refreshing with {} (attempt {}/3)", self.state, command, retries);
        self.close_current_gui(bot);
        self.action_in_progress = false;
        self.state = if was_selling {
            OrderWorkflowState::SellingInventory
        } else if self.is_fulfilling_target {
            OrderWorkflowState::FillingTargetOrders
        } else {
            OrderWorkflowState::WaitingForNextOrder
        };
        if self.pending_delivery.is_none() {
            self.target_delivering_armor = None;
        }
        self.prepare_to_send_command(bot, &command);
        self.action_retry_count = retries;
        true
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_client() -> Client {
        use azalea::entity::{LookDirection, Physics, inventory::Inventory};
        use std::sync::Arc;
        let mut app = azalea::app::App::new();
        let entity = app.world_mut().spawn((
            Inventory::default(), Physics::default(), LookDirection::new(0.0, 0.0),
        )).id();
        let world = std::mem::take(app.world_mut());
        Client::new(entity, Arc::new(world.into()))
    }

    fn hopper_with_item(slot: i16, item: ItemStack) -> GuiManager {
        let mut gui = GuiManager::new();
        gui.hopper_active = true;
        gui.on_open_screen(7, "Renamed hopper");
        gui.hopper_container_id = Some(7);
        let mut items = vec![ItemStack::Empty; 41];
        items[(slot - 4) as usize] = item;
        gui.on_set_content(7, 13, &items, None);
        gui
    }

    #[test]
    fn hopper_maps_main_inventory_and_hotbar_without_touching_bottles() {
        use azalea_registry::builtin::ItemKind;
        let book = ItemStack::new(ItemKind::EnchantedBook, 1);
        for slot in 9..=44 {
            let mut gui = hopper_with_item(slot, book.clone());
            assert_eq!(gui.hopper_transfer_slot(slot, &book, None), Some(slot - 4));
            gui.on_set_slot(7, 14, slot - 4, &ItemStack::Empty, None);
            assert_eq!(gui.player_inventory.get(&slot), Some(&ItemStack::Empty));
            assert_eq!(gui.hopper_transfer_slot(slot, &book, None), None);
        }
        for kind in [ItemKind::ExperienceBottle, ItemKind::Anvil, ItemKind::DiamondHelmet] {
            let item = ItemStack::new(kind, 1);
            let gui = hopper_with_item(36, item.clone());
            assert_eq!(gui.hopper_transfer_slot(36, &item, None), None);
        }
    }

    #[test]
    fn hopper_refuses_wrong_menu_stale_source_and_missing_contents() {
        use azalea_registry::builtin::ItemKind;
        let book = ItemStack::new(ItemKind::EnchantedBook, 1);
        let mut gui = hopper_with_item(44, book.clone());
        gui.hopper_container_id = Some(8);
        assert_eq!(gui.hopper_transfer_slot(44, &book, None), None);
        gui.hopper_container_id = Some(7);
        gui.current_slots.insert(40, ItemStack::new(ItemKind::ExperienceBottle, 1));
        assert_eq!(gui.hopper_transfer_slot(44, &book, None), None);
        gui.current_slots.insert(40, book.clone());
        gui.open_container_size = 27;
        assert_eq!(gui.hopper_transfer_slot(44, &book, None), None);
        gui.open_container_size = 5;
        gui.current_slots.remove(&0);
        assert_eq!(gui.hopper_transfer_slot(44, &book, None), None);
        for slot in [0, 8, 45] {
            assert_eq!(gui.hopper_transfer_slot(slot, &book, None), None);
        }
        let stack = ItemStack::new(ItemKind::EnchantedBook, 2);
        let gui = hopper_with_item(9, stack.clone());
        assert_eq!(gui.hopper_transfer_slot(9, &stack, None), None);
    }

    #[test]
    fn stock_alerts_require_fresh_orders_and_zero_actual_inventory() {
        use azalea_registry::builtin::ItemKind;
        let mut gui = GuiManager::new();
        gui.phase = WithdrawalPhase::ItemsRetrieval;
        assert!(gui.verified_stock_alerts().is_empty());
        gui.on_open_screen(7, "Orders -> Your Orders");
        let mut items = vec![ItemStack::Empty; 90];
        items[54] = ItemStack::new(ItemKind::ExperienceBottle, 1);
        gui.on_set_content(7, 1, &items, None);
        gui.stock_audit.observe_page(0, vec![], false);
        // One bottle is below the 256-bottle quota, but is not out of stock.
        assert!(!gui.verified_stock_alerts().contains(&"Experience Bottles"));
        gui.on_set_slot(0, 2, 9, &ItemStack::Empty, None);
        assert!(gui.verified_stock_alerts().contains(&"Experience Bottles"));
        gui.record_command_sent("/order");
        assert!(gui.verified_stock_alerts().is_empty());
    }

    #[test]
    fn an_empty_order_does_not_hide_another_order_of_the_same_item() {
        use azalea_registry::builtin::ItemKind;
        let mut gui = GuiManager::new();
        gui.phase = WithdrawalPhase::ItemsRetrieval;
        let icon = ItemStack::new(ItemKind::DiamondHelmet, 1);
        for slot in [1, 2] {
            gui.current_slots.insert(slot, icon.clone());
        }
        let first = OrderListing { slot: 1, name: "Diamond Helmet".into(), icon };
        gui.stock_audit.select(0, first);
        gui.stock_audit.collect_requested();
        assert!(gui.stock_audit.confirm_empty().is_some());
        gui.exhausted_orders.push("Diamond Helmet".into());
        assert_eq!(gui.count_available_armor_orders(None), 1);
    }

    #[test]
    fn inventory_reset_starts_a_fresh_order_audit() {
        let mut gui = GuiManager::new();
        gui.resume_order_scan = true;
        gui.exhausted_orders.push("Mending Book".into());
        gui.reset_and_sync_inventory(None);
        assert!(!gui.resume_order_scan);
        assert!(gui.exhausted_orders.is_empty());
        assert!(gui.verified_stock_alerts().is_empty());
    }

    #[test]
    fn no_items_chat_requires_an_active_collect_and_reopens_once_as_a_continuation() {
        let bot = test_client();
        let mut gui = GuiManager::new();
        gui.state = OrderWorkflowState::InOrderSubmenu;
        gui.target_order_type = Some("Mending Book".into());
        gui.last_collecting_order_name = Some("Mending Book".into());
        assert!(gui.record_no_items_to_collect(&bot).is_none());
        assert_eq!(gui.state, OrderWorkflowState::InOrderSubmenu);
        assert!(gui.exhausted_orders.is_empty());

        let listing = OrderListing {
            slot: 3, name: "Mending Book".into(), icon: ItemStack::Empty,
        };
        gui.stock_audit.select(0, listing.clone());
        gui.stock_audit.collect_requested();
        assert_eq!(gui.record_no_items_to_collect(&bot).as_deref(), Some("Mending Book"));
        assert_eq!(gui.state, OrderWorkflowState::WaitingForNextOrder);
        gui.record_command_sent("/order");
        assert!(gui.stock_audit.is_empty(0, &listing));
        gui.record_command_sent("/order");
        assert!(!gui.stock_audit.is_empty(0, &listing));
        assert!(gui.exhausted_orders.is_empty());
    }

    #[tokio::test]
    async fn successful_delivery_does_not_confirm_the_order_empty() {
        use azalea_registry::builtin::ItemKind;
        let bot = test_client();
        let mut gui = GuiManager::new();
        gui.phase = WithdrawalPhase::ItemsRetrieval;
        gui.target_order_type = Some("Experience Bottles".into());
        gui.exhausted_orders.push("Experience Bottles".into());
        let bottle = ItemStack::new(ItemKind::ExperienceBottle, 1);
        let listing = OrderListing {
            slot: 1, name: "Experience Bottles".into(), icon: bottle.clone(),
        };
        gui.stock_audit.select(0, listing.clone());
        gui.stock_audit.collect_requested();
        gui.on_open_screen(5, "Orders -> Collect Items");
        let mut items = vec![ItemStack::Empty; 63];
        items[0] = bottle.clone();
        gui.on_set_content(5, 1, &items, None);
        assert!(!gui.transfer_delivery_items(&bot).await);

        items[0] = ItemStack::Empty;
        items[27] = bottle;
        gui.on_set_content(5, 2, &items, None);
        assert!(gui.exhausted_orders.is_empty());
        assert!(gui.transfer_delivery_items(&bot).await);
        assert!(!gui.stock_audit.is_empty(0, &listing));
    }

    #[tokio::test]
    async fn spawned_state_services_scheduled_commands_and_stalled_order_requests() {
        let bot = test_client();
        let mut gui = GuiManager::new();
        gui.phase = WithdrawalPhase::ItemsRetrieval;
        gui.state = OrderWorkflowState::Spawned;
        gui.schedule_command("/order".into(), std::time::Duration::ZERO);
        assert!(gui.check_and_handle_timeout(&bot).await);
        assert!(gui.awaiting_response);
        assert_eq!(gui.last_command_sent.as_deref(), Some("/order"));

        let mut stalled = GuiManager::new();
        stalled.phase = WithdrawalPhase::ItemsRetrieval;
        stalled.state = OrderWorkflowState::Spawned;
        stalled.record_command_sent("/order");
        stalled.last_action_time = Some(std::time::Instant::now() - std::time::Duration::from_secs(7));
        assert!(stalled.check_and_handle_timeout(&bot).await);
        assert_eq!(stalled.action_retry_count, 1);
        assert_eq!(stalled.state, OrderWorkflowState::WaitingForNextOrder);
        assert!(stalled.awaiting_response);
    }

    #[test]
    fn same_container_page_contents_acknowledge_navigation() {
        let mut gui = GuiManager::new();
        gui.on_open_screen(7, "Orders -> Your Orders");
        gui.on_set_content(7, 1, &vec![ItemStack::Empty; 90], None);
        gui.record_action_sent(Some(53), Some(ClickType::Pickup));
        gui.page_turn_pending = true;
        gui.order_page = 1;
        gui.current_menu_skipped_slots.push(1);
        gui.on_set_content(7, 2, &vec![ItemStack::Empty; 90], None);
        assert!(!gui.awaiting_response);
        assert!(!gui.page_turn_pending);
        assert!(gui.current_menu_skipped_slots.is_empty());
        assert_eq!(gui.order_page, 1);
    }

    #[test]
    fn default_withdrawal_is_two_sets_grouped_by_type() {
        let mut gui = GuiManager::new();
        gui.phase = WithdrawalPhase::ItemsRetrieval;
        let pieces: Vec<_> = crate::armor::TYPES.iter().map(|kind| ItemInfo {
            kind: format!("Diamond{kind}"), count: 1, ..Default::default()
        }).collect();
        for next in 0..4 {
            for (kind, info) in pieces.iter().enumerate() {
                assert_eq!(gui.is_order_needed(info).0, kind == next);
            }
            match next {
                0 => gui.collected.diamond_helmets = 2,
                1 => gui.collected.diamond_chestplates = 2,
                2 => gui.collected.diamond_leggings = 2,
                _ => gui.collected.diamond_boots = 2,
            }
        }
        assert!(pieces.iter().all(|info| !gui.is_order_needed(info).0));
        assert_eq!(gui.quota.mending_needed, 8);
        assert_eq!(gui.quota.unbreaking_3_needed, 8);
        assert_eq!(gui.quota.protection_4_needed, 4);
        assert_eq!(gui.quota.blast_protection_4_needed, 4);
    }

    #[test]
    fn direct_inventory_updates_use_player_slot_numbers() {
        use azalea_registry::builtin::ItemKind;
        let mut gui = GuiManager::new();
        gui.on_set_slot(-2, 0, 0, &ItemStack::new(ItemKind::DiamondHelmet, 1), None);
        assert_eq!(gui.collected.diamond_helmets, 1);
        assert!(gui.player_inventory.contains_key(&36));
        gui.on_set_slot(-2, 0, 0, &ItemStack::Empty, None);
        assert_eq!(gui.collected.diamond_helmets, 0);
    }

    fn pending(armor: TargetArmorType, confirm_sent: bool) -> PendingDelivery {
        PendingDelivery { armor, inventory_count_before: 1, source_slot: 27,
            deposit_observed: true, confirm_sent }
    }

    #[test]
    fn newer_commands_supersede_scheduled_retries() {
        let mut gui = GuiManager::new();
        gui.schedule_command("/order".to_string(), std::time::Duration::from_secs(45));
        assert!(gui.scheduled_command.is_some());
        gui.record_command_sent("/order buyer");
        assert!(gui.scheduled_command.is_none());
        assert_eq!(gui.last_command_sent.as_deref(), Some("/order buyer"));
        assert!(gui.awaiting_response);
    }

    #[test]
    fn delivery_requires_four_verified_types_and_counts_each_once() {
        let mut gui = GuiManager::new();
        assert!(!gui.is_target_delivery_completed());
        gui.target_sets_to_deliver = 1;
        for armor in [TargetArmorType::Helmet, TargetArmorType::Chestplate, TargetArmorType::Leggings] {
            gui.pending_delivery = Some(pending(armor, true));
            gui.reconcile_delivery_count(0);
            gui.reconcile_delivery_count(0); // duplicate snapshot
            assert!(!gui.is_target_delivery_completed());
        }
        gui.pending_delivery = Some(pending(TargetArmorType::Boots, true));
        gui.reconcile_delivery_count(0);
        assert!(gui.is_target_delivery_completed());
        assert_eq!(gui.target_delivered_helmets, 1);
    }

    #[test]
    fn escrow_and_rejected_confirmation_are_not_deliveries() {
        let mut gui = GuiManager::new();
        gui.pending_delivery = Some(pending(TargetArmorType::Boots, false));
        gui.reconcile_delivery_count(0); // disappeared into escrow, never confirmed
        assert!(gui.pending_delivery.is_some());
        assert_eq!(gui.target_delivered_boots, 0);
        gui.pending_delivery.as_mut().unwrap().confirm_sent = true;
        gui.reconcile_delivery_count(1); // server returned rejected item
        assert!(gui.pending_delivery.is_none());
        assert_eq!(gui.target_delivered_boots, 0);
    }

    #[test]
    fn missing_contents_keep_watchdog_armed_and_late_packets_are_ignored() {
        let mut gui = GuiManager::new();
        gui.record_command_sent("/order");
        gui.action_retry_count = 2;
        gui.on_open_screen(7, "Orders");
        assert!(gui.awaiting_response);
        assert_eq!(gui.action_retry_count, 2);
        gui.on_set_content(6, 1, &vec![ItemStack::Empty; 63], None);
        assert_eq!(gui.current_container_id, 7);
        assert!(gui.current_slots.is_empty());
        assert!(gui.awaiting_response);
        gui.on_set_content(7, 1, &vec![ItemStack::Empty; 63], None);
        assert!(!gui.awaiting_response);
    }

    #[test]
    fn transfer_waits_for_source_and_inventory_packets_in_either_order() {
        use azalea_registry::builtin::ItemKind;
        for inventory_first in [false, true] {
            let mut gui = GuiManager::new();
            gui.on_open_screen(7, "Orders -> Collect Items");
            let bottle = ItemStack::new(ItemKind::ExperienceBottle, 64);
            let mut items = vec![ItemStack::Empty; 63];
            items[0] = bottle.clone();
            gui.on_set_content(7, 1, &items, None);
            gui.record_action_sent(Some(0), Some(ClickType::QuickMove));
            gui.transfer_before = Some((bottle.clone(), gui.player_inventory.clone()));
            if inventory_first {
                gui.on_set_slot(7, 2, 27, &bottle, None);
            } else {
                gui.on_set_slot(7, 2, 0, &ItemStack::Empty, None);
            }
            assert!(gui.awaiting_response);
            if inventory_first {
                gui.on_set_slot(7, 2, 0, &ItemStack::Empty, None);
            } else {
                gui.on_set_slot(7, 2, 27, &bottle, None);
            }
            assert!(!gui.awaiting_response);
            assert_eq!(gui.collected.xp_bottles, 64);
        }
    }

    #[test]
    fn quota_tracks_consumption_and_uses_bottles_not_occupied_slots() {
        let mut gui = GuiManager::new();
        gui.collected.mending = 8;
        gui.collected.diamond_boots = 2;
        gui.sync_collected_from_inventory(None);
        assert_eq!(gui.collected.mending, 0);
        assert_eq!(gui.collected.diamond_boots, 0);
        gui.collected.xp_stacks = 4;
        gui.collected.xp_bottles = 4;
        assert!(!gui.collected.has_required_xp(&gui.quota));
    }

    #[test]
    fn fresh_order_snapshot_settles_confirmation_but_old_screen_does_not() {
        let mut gui = GuiManager::new();
        gui.is_fulfilling_target = true;
        gui.target_delivering_armor = Some(TargetArmorType::Boots);
        gui.pending_delivery = Some(pending(TargetArmorType::Boots, true));
        gui.on_open_screen(8, "Orders -> Confirm delivery");
        gui.on_set_content(8, 1, &vec![ItemStack::Empty; 63], None);
        assert_eq!(gui.target_delivered_boots, 0);
        gui.on_open_screen(9, "Orders");
        gui.on_set_content(8, 2, &vec![ItemStack::Empty; 63], None);
        assert_eq!(gui.target_delivered_boots, 0);
        gui.on_set_content(9, 1, &vec![ItemStack::Empty; 63], None);
        assert_eq!(gui.target_delivered_boots, 1);
        assert!(gui.target_delivering_armor.is_none());
    }

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
        collected.xp_bottles = 192;
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

        gui.collected.diamond_helmets = gui.quota.diamond_helmets_needed;
        let (needed_helmet_again, _) = gui.is_order_needed(&helmet);
        assert!(!needed_helmet_again); // Already collected quota of helmets
    }

    #[test]
    fn test_anvil_needed_when_less_than_2_or_1() {
        let mut gui = GuiManager::new();
        gui.phase = WithdrawalPhase::AnvilPlacement;

        let anvil_info = ItemInfo {
            kind: "Anvil".to_string(),
            count: 1,
            ..Default::default()
        };

        // 0 anvils: needed
        gui.collected.anvils = 0;
        let (needed, _) = gui.is_order_needed(&anvil_info);
        assert!(needed, "Anvil must be needed when bot has 0 anvils (< 2)");

        // 1 anvil: needed (anvil == 1 or < 2)
        gui.collected.anvils = 1;
        let (needed_one, _) = gui.is_order_needed(&anvil_info);
        assert!(needed_one, "Anvil must be needed when bot has 1 anvil (< 2)");

        // Two server-confirmed anvils meet the quota: no throw is needed.
        gui.collected.anvils = 2;
        let (needed_two, _) = gui.is_order_needed(&anvil_info);
        assert!(!needed_two, "Anvil must not be needed when the inventory meets the quota");
    }

    #[test]
    fn anvil_pickup_requires_inventory_growth_not_source_removal() {
        use azalea_registry::builtin::ItemKind;
        for inventory_first in [false, true] {
            let mut gui = GuiManager::new();
            gui.on_open_screen(5, "Orders -> Collect Items");
            let mut items = vec![ItemStack::Empty; 63];
            items[0] = ItemStack::new(ItemKind::Anvil, 64);
            items[27] = ItemStack::new(ItemKind::Anvil, 1);
            gui.on_set_content(5, 12, &items, None);
            gui.pending_anvil_pickup = Some(PendingAnvilPickup {
                inventory_before: 1, sent_at: std::time::Instant::now(), warned: false,
            });
            gui.record_action_sent(Some(0), Some(ClickType::Throw));
            assert!(!gui.anvil_withdrawal_complete());
            let source_update = ItemStack::new(ItemKind::Anvil, 63);
            let pickup = ItemStack::new(ItemKind::Anvil, 2);
            if inventory_first {
                gui.on_set_slot(-2, 13, 9, &pickup, None);
                gui.on_set_slot(5, 14, 0, &source_update, None);
            } else {
                gui.on_set_slot(5, 13, 0, &source_update, None);
                assert!(gui.awaiting_response);
                assert!(gui.pending_anvil_pickup.is_some());
                assert!(!gui.anvil_withdrawal_complete());
                gui.on_set_slot(5, 14, 27, &pickup, None);
            }
            assert!(gui.pending_anvil_pickup.is_none());
            assert!(!gui.awaiting_response);
            assert!(gui.anvil_withdrawal_complete());
        }
    }

    #[tokio::test]
    async fn anvil_throw_aims_down_before_single_item_click_and_timeout_does_not_repeat() {
        use azalea::entity::{LookDirection, Physics, inventory::Inventory};
        use azalea::protocol::packets::game::ServerboundGamePacket;
        use azalea_registry::builtin::ItemKind;
        use std::sync::{Arc, Mutex};
        let packets = Arc::new(Mutex::new(Vec::new()));
        let observed = packets.clone();
        let mut app = azalea::app::App::new();
        app.world_mut().add_observer(move |event: azalea::ecs::prelude::On<azalea::packet::game::SendGamePacketEvent>| {
            observed.lock().unwrap().push(event.packet.clone());
        });
        let entity = app.world_mut().spawn((
            Inventory { id: 5, ..Default::default() }, Physics::default(), LookDirection::new(40.0, 0.0),
        )).id();
        let world = std::mem::take(app.world_mut());
        let bot = Client::new(entity, Arc::new(world.into()));
        let mut gui = GuiManager::new();
        gui.on_open_screen(5, "Orders -> Collect Items");
        let mut items = vec![ItemStack::Empty; 63];
        items[0] = ItemStack::new(ItemKind::Anvil, 64);
        gui.on_set_content(5, 12, &items, None);
        assert!(gui.throw_one_anvil_at_feet(&bot, 0));
        assert!(!gui.throw_one_anvil_at_feet(&bot, 0));
        gui.on_set_slot(5, 13, 0, &ItemStack::new(ItemKind::Anvil, 63), None);
        gui.pending_anvil_pickup.as_mut().unwrap().sent_at -= std::time::Duration::from_secs(11);
        assert!(!gui.check_and_handle_timeout(&bot).await);
        assert!(!gui.process_gui_actions(&bot).await);
        assert!(!gui.anvil_withdrawal_complete());
        bot.ecs.write().flush();
        {
            let packets = packets.lock().unwrap();
            assert_eq!(packets.len(), 2, "no automatic replay after pickup timeout");
            let ServerboundGamePacket::MovePlayerRot(look) = &packets[0] else { panic!("aim must precede throw"); };
            assert_eq!(look.look_direction.x_rot(), 90.0);
            assert_eq!(look.look_direction.y_rot(), 40.0);
            let ServerboundGamePacket::ContainerClick(click) = &packets[1] else { panic!("expected throw click"); };
            assert_eq!(click.click_type, ClickType::Throw);
            assert_eq!(click.button_num, 0);
            assert_eq!(click.slot_num, 0);
            assert_eq!(click.container_id, 5);
            assert_eq!(click.state_id, 12);
        }
        // Fresh inventory snapshots after a close/reconnect reconcile the retained obligation.
        gui.current_container_id = 0;
        gui.player_inventory.clear();
        gui.clear_watchdog();
        assert!(!gui.anvil_withdrawal_complete());
        let mut inventory = vec![ItemStack::Empty; 46];
        inventory[36] = ItemStack::new(ItemKind::Anvil, 1);
        gui.on_set_content(0, 1, &inventory, None);
        assert!(gui.pending_anvil_pickup.is_none());
        assert!(gui.scheduled_command.is_some());
        assert!(!gui.anvil_withdrawal_complete(), "one picked-up anvil does not meet a two-anvil quota");
    }

    #[test]
    fn test_find_unneeded_inventory_slots() {
        let gui = GuiManager::new();
        // Default quota: 8 Mending, 8 Unb3, 4 Prot4, 4 BlastProt4, 4 XP stacks, 2 of each armor, 1 anvil
        let unneeded = gui.find_unneeded_inventory_slots(None);
        // Initially empty inventory has 0 unneeded
        assert_eq!(unneeded.len(), 0);
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

        // Partial stacks still need topping up to the 128-bottle quota.
        gui.collected.xp_bottles = 114;
        gui.collected.xp_stacks = 2;
        let (needed, _) = gui.is_order_needed(&xp_info);
        assert!(needed, "114 bottles do not meet the 128-bottle quota");

        // Four occupied slots do not meet a 256-bottle quota with only 210 bottles.
        gui.quota.xp_stacks_needed = 4;
        gui.collected.xp_bottles = 210;
        gui.collected.xp_stacks = 4;
        let (needed, _) = gui.is_order_needed(&xp_info);
        assert!(needed, "210 bottles do not meet the 256-bottle quota");
    }

    #[test]
    fn test_anvils_kept_in_items_retrieval_phase() {
        let mut gui = GuiManager::new();
        gui.phase = WithdrawalPhase::ItemsRetrieval;

        // Put an anvil in slot 10
        let anvil = ItemStack::Present(azalea::inventory::ItemStackData {
            kind: azalea_registry::builtin::ItemKind::Anvil,
            count: 1,
            component_patch: Default::default(),
        });
        gui.player_inventory.insert(10, anvil);

        let unneeded = gui.find_unneeded_inventory_slots(None);
        assert!(unneeded.is_empty(), "Anvils should never be considered unneeded/surplus");
    }

    #[test]
    fn test_click_type_swap() {
        let _ = ClickType::Swap;
    }

    #[test]
    fn test_exhausted_order_handling_and_missing_summary() {
        let mut gui = GuiManager::new();
        gui.phase = WithdrawalPhase::ItemsRetrieval;

        // Test item name identification
        let helmet_info = ItemInfo {
            kind: "DiamondHelmet".to_string(),
            count: 1,
            ..Default::default()
        };
        assert_eq!(gui.get_item_name(&helmet_info), "Diamond Helmet");

        let custom_info = ItemInfo {
            kind: "Paper".to_string(),
            count: 1,
            custom_name: Some("Custom Voucher".to_string()),
            ..Default::default()
        };
        assert_eq!(gui.get_item_name(&custom_info), "Custom Voucher");

        // Test missing quota summary
        let missing = gui.get_missing_quota_summary();
        assert!(missing.contains("Diamond Helmet"));
        assert!(missing.contains("Diamond Chestplate"));
        assert!(missing.contains("Mending Book"));

        // Mark Diamond Helmet as exhausted / out of items
        gui.exhausted_orders.push("Diamond Helmet".to_string());
        assert!(gui.exhausted_orders.contains(&"Diamond Helmet".to_string()));

        // Check that diamond armor order counting excludes exhausted orders
        gui.current_slots.insert(3, ItemStack::Empty); // simulated slot
        assert_eq!(gui.count_available_armor_orders(None), 0);
    }

    #[test]
    fn test_can_craft_god_armor() {
        let mut gui = GuiManager::new();

        // Initially 0 of everything
        assert!(!gui.can_craft_god_armor(None));
        assert_eq!(gui.count_craftable_god_sets(None), 0);

        // Has anvil and complete set of armor pieces and books, but NO XP
        gui.collected.anvils = 1;
        gui.collected.diamond_helmets = 1;
        gui.collected.diamond_chestplates = 1;
        gui.collected.diamond_leggings = 1;
        gui.collected.diamond_boots = 1;
        gui.collected.mending = 4;
        gui.collected.unbreaking_3 = 4;
        gui.collected.protection_4 = 2;
        gui.collected.blast_protection_4 = 2;
        gui.collected.xp_bottles = 0;
        gui.collected.xp_stacks = 0;
        assert!(!gui.can_craft_god_armor(None), "Should fail without XP bottles");

        // Now add XP bottles
        gui.collected.xp_bottles = 32;
        assert!(gui.can_craft_god_armor(None), "Should succeed with XP and all full set components");
        assert_eq!(gui.count_craftable_god_sets(None), 1);

        // Without leggings, CANNOT craft complete set!
        gui.collected.diamond_leggings = 0;
        assert!(!gui.can_craft_god_armor(None), "Should fail without Leggings for complete set");
        assert_eq!(gui.count_craftable_god_sets(None), 0);
    }

    #[test]
    fn test_count_completed_max_sets() {
        let gui = GuiManager::new();
        assert_eq!(gui.count_completed_max_sets(None), 0);
    }

    #[test]
    fn test_watchdog_tracking_and_clear() {
        let mut gui = GuiManager::new();
        assert!(!gui.awaiting_response);
        assert_eq!(gui.action_retry_count, 0);

        gui.record_action_sent(Some(51), Some(ClickType::Pickup));
        assert!(gui.awaiting_response);
        assert_eq!(gui.last_clicked_slot, Some(51));
        assert_eq!(gui.last_click_type, Some(ClickType::Pickup));
        assert!(gui.last_action_time.is_some());

        gui.clear_watchdog();
        assert!(!gui.awaiting_response);
        assert_eq!(gui.last_clicked_slot, None);
        assert_eq!(gui.last_action_time, None);

        gui.record_command_sent("/order");
        assert!(gui.awaiting_response);
        assert_eq!(gui.last_command_sent.as_deref(), Some("/order"));
    }

    #[test]
    fn test_count_craftable_god_sets_with_partially_enchanted_pieces() {
        let mut gui = GuiManager::new();
        gui.phase = WithdrawalPhase::ItemsRetrieval;

        // Put anvil in offhand (slot 45)
        let anvil = ItemStack::Present(azalea::inventory::ItemStackData {
            kind: azalea_registry::builtin::ItemKind::Anvil,
            count: 64,
            component_patch: Default::default(),
        });
        gui.player_inventory.insert(45, anvil);
        gui.collected.anvils = 64;

        // Slot 45 is offhand, so all 36 main inventory slots (9..=44) must be free
        assert_eq!(gui.count_free_inventory_slots(), 36);

        // Collected books: 1 Blast Prot 4, 1 Mending (0 Prot 4, 0 Unb 3)
        gui.collected.diamond_helmets = 2;
        gui.collected.diamond_chestplates = 2;
        gui.collected.diamond_leggings = 2;
        gui.collected.diamond_boots = 2;
        gui.collected.blast_protection_4 = 1;
        gui.collected.mending = 1;
        gui.collected.protection_4 = 0;
        gui.collected.unbreaking_3 = 0;
        gui.collected.xp_bottles = 64;
        gui.collected.xp_stacks = 1;

        // With default unenchanted pieces (no player_inventory armor populated),
        // 0 sets can be crafted because 2 unenchanted sets need 4 blast prot and 8 mending
        assert_eq!(gui.count_craftable_god_sets(None), 0);
    }

    #[test]
    fn test_has_any_craftable_combines_detects_partial_combines() {
        let mut gui = GuiManager::new();
        gui.phase = WithdrawalPhase::ItemsRetrieval;

        // Anvil in offhand (slot 45)
        let anvil = ItemStack::Present(azalea::inventory::ItemStackData {
            kind: azalea_registry::builtin::ItemKind::Anvil,
            count: 64,
            component_patch: Default::default(),
        });
        gui.player_inventory.insert(45, anvil);
        gui.collected.anvils = 64;
        gui.collected.xp_bottles = 64;
        gui.collected.xp_stacks = 1;

        // Add 1 unenchanted helmet to inventory
        let helmet = ItemStack::Present(azalea::inventory::ItemStackData {
            kind: azalea_registry::builtin::ItemKind::DiamondHelmet,
            count: 1,
            component_patch: Default::default(),
        });
        gui.player_inventory.insert(10, helmet);
        gui.collected.diamond_helmets = 1;

        // No books yet: has_any_craftable_combines is false
        assert!(!gui.has_any_craftable_combines(None));

        // Add 1 Protection IV book to collected
        gui.collected.protection_4 = 1;
        assert!(gui.has_any_craftable_combines(None));

        // Note: complete sets is still 0 because chestplate, leggings, boots are missing
        assert_eq!(gui.count_craftable_god_sets(None), 0);
    }
}
