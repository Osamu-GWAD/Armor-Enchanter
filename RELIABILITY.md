# Reliability changes

## Verified stock alerts

- Inventory-only XP and armor alerts were removed. Missing supplies return to `/order` for verification first.
- Alerts require zero of the specific item in inventory, not merely less than the two-set quota.
- A complete Your Orders page scan must show no matching orders, or every matching order must have returned an empty response to a recent Collect request. Other matching orders are checked individually, including on later pages.
- Order-empty evidence expires after 45 seconds and is tied to the page, slot, and displayed item. New scans invalidate prior page results; unrelated chat and GUI timeouts cannot establish empty stock.
- The page-navigation guard stops after 21 pages without asserting that an unfinished scan is complete. Unverified stock remains silent and is retried. The existing Discord cooldown remains 120 seconds per item.

## Serial armor fixes

- Default withdrawals now contain two of each armor type and the books for two sets (8 Unbreaking III, 8 Mending, 4 Protection IV, 4 Blast Protection IV), with 256 XP bottles. Both items of each armor type are collected together to reduce order visits.
- Each set is enchanted and deposited in helmet, chestplate, leggings, boots order. The next stocked set starts immediately in the same worker, without cleaning or opening `/order` between sets.
- Anvil and hopper waits use server-update notifications with bounded timeout fallbacks. Anvil opening no longer holds its manager lock for a fixed two-tick delay after interaction. These remove local delays; server throughput has not been measured.
- Enchanting finishes one piece per type in that order. Missing books return the unfinished set to orders instead of claiming success or skipping ahead.
- The latest withdrawal inventory is copied into the enchanter at handoff. Direct player inventory packets are handled, and armor verification uses one authoritative inventory rather than merging duplicate slot namespaces.
- Hopper deposits use QuickMove in a verified five-slot hopper menu, one completed armor piece at a time. The player slot is mapped to the hopper menu and checked against the expected item before clicking. Server source-slot and inventory-count updates confirm each transfer. Partial transfer progress survives automatic reconnects within the process. Rejected books also use the hopper GUI; XP bottles cannot pass the transfer guard.
- A missing/full hopper, closed menu, or unconfirmed transfer prevents starting another set. The menu is closed after the attempt. Items may drain into connected storage immediately; confirmation relies on server updates to the player source slot and inventory, not continued presence in the hopper.

## Anvil pickup and rejected books

- Anvil withdrawal sends an explicit downward rotation before throwing one anvil from the order GUI. It waits for the server inventory anvil count to increase before throwing another, and advances only after the configured quota is held. Source-slot removal alone is not pickup confirmation.
- If pickup is still unconfirmed after 10 seconds, withdrawal pauses and logs the problem instead of throwing more anvils. Returning the missing anvil to the bot's inventory resumes progress. The pending pickup survives an automatic reconnect within the same process, but not a process restart. Throwing at feet cannot prevent a hopper directly under the drop from collecting it.
- The order GUI closes before moving confirmed anvils to the offhand, so player-menu swaps target the correct menu.
- Failed rejected-book disposal is tracked by the exact item and its components instead of inventory slot numbers. Good replacements remain eligible even in the old slot, and successful disposal clears the rejection. Anvil staging also refuses to substitute a rejected hotbar book for a good book with the same enchantment.
- XP use now reads a packet-only inventory snapshot. A no-op hotbar self-swap with a mismatched state ID requests a fresh full snapshot after the anvil closes. If the server does not return it, the routine aborts safely.
- XP swaps require matching updates for both source and hotbar slots. Bottle-use requests are tracked separately from actual inventory and never overwrite GUI/enchanter caches. Each batch waits for server-confirmed consumption and XP gain. Partial, rejected, or missing acknowledgements reconnect for reconciliation after a five-second timeout.
- Anvil close now updates Azalea's active menu as well as sending the server close packet, so XP inventory operations use the player menu.

## Earlier buyer-order fixes (legacy delivery routine)


1. **Armor identity lost before confirmation.** The deposit routine cleared `target_delivering_armor` before the confirmation screen arrived. A pending transaction now retains the type through reconciliation.
2. **Premature delivery success.** A click, or an empty inventory while items were still in the delivery chest, could be treated as success. Counters now require a post-confirmation order-menu inventory snapshot and exactly one missing piece of the expected type. Repeated snapshots cannot count the same transaction twice.
3. **Unfinished sets abandoned.** The delivery loop previously moved on after 120 seconds. It now retains the original per-type obligations and retries, completing one of each type before starting another set. Automatic reconnects reuse that ledger within the same account process.
4. **Optimistic withdrawal counts.** Items were counted immediately after sending QuickMove, even if the server rejected it. Transfers now wait for both source and inventory updates before making another quota decision.
5. **Historical inventory counts.** Quota counters retained their previous maximum after items were consumed or moved. They now track current server inventory.
6. **XP quota overestimation.** Several nearly empty bottle stacks could satisfy a quota intended for hundreds of bottles. Fulfillment now uses bottle count, requiring the configured count of full stacks (64 bottles per stack).
7. **GUI-close packets discarded.** The outer event filter omitted ContainerClose, making its handler unreachable. Matching close packets now invalidate the window and arm recovery.
8. **Missing contents disabled the watchdog.** OpenScreen previously cleared monitoring even when no contents followed. Monitoring now remains armed. Old content packets cannot resurrect closed or replaced GUI/anvil windows.
9. **Unsafe retry replay.** Retrying a non-idempotent slot click could transfer a second item or undo a swap. Recovery refreshes the screen and reconciles inventory instead. Command timers are tracked and superseded by newer commands.
10. **Lock-held polling and stale fallback clicks.** Sell-window polling and anvil result polling prevented packet handlers from updating the state being polled. Sell, withdrawal, deposit, and anvil clicks now advance from acknowledgements. XP throwing releases the shared enchanter lock. Redundant GUI task polling and fixed post-click waits were removed.
11. **Anvil routing/state contamination.** A 39-slot packet was assumed to be an anvil, and unrelated container data could overwrite repair cost. Routing now uses the tracked anvil container ID; full player inventory packets update the enchanter cache as well.
12. **XP swap bookkeeping.** XP actions no longer edit inventory estimates or copy a worker inventory over server updates. Both swapped slots and bottle consumption must be confirmed by server packets.

## Validation

Run `cargo test --locked`. Regression tests cover four-type completion, duplicate snapshots, rejected confirmations, escrow, late/wrong-container contents, dropped contents after OpenScreen, source/inventory update ordering, consumed quota items, misleading XP stack counts, anvil completion while work is pending, rejected XP swaps, both swap packet orders, partial bottle consumption, delayed acknowledgements, and changed held items.

These are local state-machine tests. No live account was connected, no items were sold or delivered during validation, and no server-load or throughput benchmark was performed.

## Remaining limitations

- The server exposes separate transactions for individual armor types. Four-piece delivery cannot be atomic: a buyer can close a remaining order after accepting three pieces. The bot retains the outstanding obligation instead of claiming success or starting a new batch.
- Acceptance is inferred from confirmation plus a fresh server inventory snapshot. There is no server transaction ID/receipt API in this repository. Unexpected inventory changes hold the transaction for reconciliation.
- Delivery bookkeeping survives automatic reconnects in the same process, but is not persisted to disk. Restarting or killing the bot during a partial delivery loses that ledger; reconcile the buyer's orders and inventory before restarting an interrupted delivery.
- Book/order identification can use displayed enchantment text, but armor completion requires applied enchantment components. Servers that expose armor enchantments only as lore will require a different authoritative verification path before this bot can mark those pieces complete.
- Command cooldowns, login/teleport settling, XP animation timing, and restock cooldowns remain. The changes reduce unnecessary waiting; they do not promise a throughput target or an anti-cheat outcome.
