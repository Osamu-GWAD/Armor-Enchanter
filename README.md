# Armor-Enchanter

An autonomous, high-performance Minecraft enchanting bot built in Rust with [Azalea](https://github.com/azalea-rs/azalea). Designed for high-efficiency SMP automation (such as `donutsmp.net`), Armor-Enchanter claims required items from `/order`, places an anvil, dynamically calculates and throws the exact amount of XP required for each combine, and assembles fully enchanted diamond armor sets in optimal anvil order to minimize repair costs.

---

## Features

- **Automated `/order` Two-Set Withdrawal**:
  - Automatically queries and interacts with the server order GUI.
  - Withdraws supplies for two sets per batch:
    - 2× Diamond Helmet, 2× Diamond Chestplate, 2× Diamond Leggings, 2× Diamond Boots, collecting both pieces from each order together
    - 8× Unbreaking III, 8× Mending, 4× Blast Protection IV, 4× Protection IV enchanted books (24 books total)
    - 4× Stacks of Bottles o' Enchanting (256 bottles total; partial stacks count by bottle quantity)
    - 1× Anvil (retrieved and placed in Phase 1 before Phase 2 item retrieval, leaving all 36 slots free)

- **Automated `/sell` Inventory Cleaning**:
  - Upon spawn and between batches, automatically scans player inventory slots (9..=44).
  - Identifies foreign items (cobblestone, dirt, junk books, weapons, drops) and surplus items beyond quota.
  - Opens `/sell`, transfers unneeded items via `ClickType::QuickMove`, and closes the container to sell them for money.
  - Strictly preserves completed max-enchanted armor and in-progress armor pieces.

- **Exact Level-to-Level XP Calculation & Rapid Throwing**:
  - Computes exact XP required for anvil combines using official Minecraft Java level formulas:
    - Points to next level: $\text{xp\_to\_next}(L) = \begin{cases} 2L + 7 & 0 \le L \le 15 \\ 5L - 38 & 16 \le L \le 30 \\ 9L - 158 & L \ge 31 \end{cases}$
    - Total level XP: $\text{TotalXP}(L) = \begin{cases} L^2 + 6L & 0 \le L \le 16 \\ 2.5L^2 - 40.5L + 360 & 17 \le L \le 31 \\ 4.5L^2 - 162.5L + 2220 & L \ge 32 \end{cases}$
    - Exact deficit from Level $X$ (with progress) to Level $Y$: $\Delta\text{XP} = \text{TotalXP}(Y) - \text{true\_current\_xp}$
    - Bottles needed: $\lceil \Delta\text{XP} / 6.8 \rceil$ (average 7.0 XP per bottle with variance safety margin)
  - Throws bottles in rapid, authentic streams (1 tick per throw) with `ServerboundSwing` arm animations, reaching required levels in under 1 second.

- **Optimal Anvil Sequencing ([iamcal/enchant-order](https://github.com/iamcal/enchant-order))**:
  - Tree-merge sequence minimizing prior work penalties and cumulative level costs:
    - **Book Merge**: Unbreaking III (book) + Mending (book) $\to$ (Unbreaking III + Mending) book (2 lvl)
    - **Base Armor Merge**:
      - **Helmet**: Blank Helmet + Protection IV (4 lvl)
      - **Chestplate**: Blank Chestplate + Protection IV (4 lvl)
      - **Leggings**: Blank Leggings + Blast Protection IV (8 lvl)
      - **Boots**: Blank Boots + Blast Protection IV (8 lvl)
    - **Final Tree Merge**: Armor (with Prot IV / Blast Prot IV) + (Unbreaking III + Mending) book (7 lvl)
  - Result: Fully enchanted armor piece with Prior Work Penalty of only 2 (3 levels) instead of 3 (7 levels).

- **Anti-Cheat Resilient & Humanized Interactions**:
  - **Smooth View Interpolation**: Mimics human mouse rotation using a cosine ease-in-out curve ($\alpha = \frac{1 - \cos(\pi t)}{2}$) over 2–6 ticks, eliminating abrupt aim snapping while keeping rotations responsive.
  - **Arm Swing Animations**: Sends `ServerboundSwing` packet on every bottle thrown, anvil placement, and anvil block interaction.
  - **Armor Equip Prevention**: Automatically selects safe hotbar slots (holding books or empty hands) when clicking blocks, and actively unequips armor if accidentally worn.
  - **Responsive GUI Throughput**: Container transfers and anvil actions advance when their server acknowledgements arrive, avoiding fixed post-click sleeps and unnecessary fallback clicks.
  - **Strict 4-Piece Verification**: Ensures all 4 pieces (Helmet, Chestplate, Leggings, Boots) are present and verified to have all 3 required enchantments before concluding.

---

## Level-to-Level XP Reference

| Transition | $\Delta\text{XP}$ Deficit | Bottles Required | Expected XP Output | First-Throw Attainment |
| :--- | :---: | :---: | :---: | :---: |
| **Level 0 $\to$ Level 4** | 40 XP | **6 bottles** | 42 XP | 100% |
| **Level 0 $\to$ Level 5** | 55 XP | **9 bottles** | 63 XP | 100% |
| **Level 0 $\to$ Level 8** | 112 XP | **17 bottles** | 119 XP | 100% |
| **Level 4 $\to$ Level 8** | 72 XP | **11 bottles** | 77 XP | 100% |
| **Level 8 $\to$ Level 13** | 135 XP | **20 bottles** | 140 XP | 100% |
| **Level 0 $\to$ Level 13** | 247 XP | **37 bottles** | 259 XP | 100% |

---

## Authentication & Multi-Account Support

Enchanter supports both official **Microsoft OAuth (device code flow)** and **direct JWT access tokens** with flexible multi-account management.

### Option 1: Microsoft Multi-Account
Configure one or more Microsoft accounts in `.env`:
```env
ACCOUNTS=player1@outlook.com,player2@outlook.com
ACCOUNT=0
```
Or specify individual numbered variables:
```env
ACCOUNT_1=first_account@outlook.com
ACCOUNT_2=second_account@outlook.com
```
When launching with a Microsoft account for the first time, Azalea will prompt you to visit `https://microsoft.com/link` and enter a short device code. Authentication is cached locally so subsequent launches log in automatically!

### Option 2: Pre-authenticated JWT / Bearer Token
```env
MC_TOKEN=eyJraWQiOi...
```

### Switching Accounts
Switch accounts easily via command line arguments or the `ACCOUNT` environment variable:
```bash
# By index (0-indexed or 1-indexed)
cargo run --release -- --account 0
cargo run --release -- --account 1

# By email
cargo run --release -- --account player2@outlook.com

# Direct Microsoft email
cargo run --release -- --microsoft player1@outlook.com
```

---

## Serial armor workflow and recovery

- Withdraw two helmets, chestplates, leggings, and boots from `/order`, plus the books and XP listed above. The eight armor pieces, 24 books, and four XP stacks fit the 36 inventory slots after placing the anvil.
- Finish enchanting the helmet, then chestplate, leggings, and boots. A missing piece or required book returns the unfinished set to restocking. Duplicate armor does not take priority over the next type.
- Once all four pieces are complete, aim at a hopper within 4.5 blocks and drop exactly one helmet, chestplate, leggings, and boots, in that order. Each item is thrown directly from its verified inventory slot, and each drop waits for the server to report one fewer piece before advancing. Inventory entries are never optimistically deleted to confirm drops.
- Immediately enchant and drop the second stocked set in the same order. Inventory cleaning and `/order` restocking run after both sets, or when supplies are missing, rather than after every set.
- Anvil and drop workers wake on server inventory updates. Opening the anvil releases its state lock immediately after interaction, allowing screen packets to be processed without a fixed post-interaction delay. Timeouts and per-item acknowledgements remain in place; live throughput has not been benchmarked.
- A missing hopper or unconfirmed drop retains the sequence and reconnects for reconciliation. Pending drop bookkeeping survives automatic reconnects within the same account process. It is not saved across a process restart.
- Inventory withdrawal snapshots are handed to the enchanter before it starts. Anvil transfers wait for source and inventory changes; direct player-inventory packets update the same caches.
- GUI watchdog recovery refreshes stalled screens. Full sets are required before successful completion; missing books or XP cannot mark an incomplete set complete.

Set `/home 1` at your enchanting area with an accessible anvil (or space to place one) and a hopper within two blocks. The bot drops toward the hopper; server inventory acknowledgement confirms that an item left the player, not that the hopper collected it. Position the hopper to catch the thrown items and leave storage space available.

The active workflow drops armor locally. `ORDER_TARGET_NAME` is retained for the legacy buyer-order routine and is not the destination used by this workflow.

Local tests cover sequencing and inventory reconciliation. Live server behavior and hopper capture have not been tested.

Discord out-of-stock alerts require both zero inventory for the item and a completed scan of Your Orders. The bot checks additional matching orders and subsequent pages before alerting. A low batch count, XP shortfall, GUI timeout, or an unverified order does not trigger a webhook. Empty-order responses expire after 45 seconds and duplicate alerts retain the 120-second cooldown. Tests do not send Discord messages.

---

## Configuration

1. Copy `.env.example` to `.env`:
   ```bash
   cp .env.example .env
   ```
2. Open `.env` and set your preferred account(s) and target buyer:
   ```env
   ACCOUNTS=your_email@outlook.com
   ORDER_TARGET_NAME=zn6h
   ```

---

## Building & Running

### Prerequisites
- [Rust](https://rustup.rs/) (Nightly toolchain specified in `rust-toolchain.toml`)

### Running on Windows
Double-click `run_all.bat` to launch all configured accounts, or run in terminal:
```bash
.\run_all.bat
```
Or with cargo:
```bash
cargo run --release
cargo run --release -- --account 0
```

### Running Unit Tests
```bash
cargo test
```

### Drop confirmation

Completed armor and rejected books use the direct inventory-slot throw from
commit `53b7303`, with exact-item and protected-item checks immediately before
sending. There is no hotbar staging or hand selection. This also works when every
hotbar slot contains protected armor. The bot waits up to ten seconds for a server
inventory decrease. An inventory decrease does not prove hopper pickup.

After the drop, the bot requests a full server inventory snapshot with a same-slot
hotbar swap (a no-op) and a mismatched menu state ID. This handles servers that
accept a drop without echoing the slot removal. The cache is updated from the
server's response; sending the drop itself never counts as confirmation.

Anvil contents update the player inventory cache while the menu is open, and
closing a menu also updates Azalea's active container. High-cost books are tracked
by their full item data; an unrelated book is never substituted for a rejected one.

### Unsigned chat

Chat signing is disabled on every connection. The bot does not request player
chat certificates; outgoing chat and commands are unsigned. Microsoft/Minecraft
authentication for joining online-mode servers remains enabled. Servers that
require signed chat may reject unsigned messages.

### GameTick lag warnings

Azalea targets one game tick every 50 ms. A `GameTick is more than 10 ticks behind`
warning means the local scheduler has accumulated roughly half a second of lag
and discarded overdue ticks to avoid a catch-up burst. It does not measure server TPS.

The bot requests a view distance of 2 chunks instead of Azalea's default of 8,
reducing chunk/entity traffic where the server honors that setting. All enchanting
interactions are nearby. Override with `--view-distance 8` or `VIEW_DISTANCE=8`
if needed (accepted range: 2–32). Multi-account launches forward the setting.

Every 30 seconds, `Local scheduler timing` reports executed `local_tps`, the
number of ticks and updates, `max_update_ms`, and `max_tick_gap_ms`. The update
measurement spans the outer schedule from First to Last; it excludes GameTick
and time waiting for the ECS lock or for the thread to run. Large update times
point to work within that schedule; long tick gaps with fast updates require
checking GameTick work, other local tasks, lock contention, and host load.
The warning alone does not identify which of those caused the lag. Tick-lag
warnings do not automatically reconnect or alter the 20-TPS simulation.

Console logging is initialized with a dedicated worker and a bounded 4096-line queue. If
the terminal stalls and fills that queue, new log lines are dropped instead of
blocking game ticks. The worker guard remains alive until normal shutdown to
flush queued output. Debug-only item audits are skipped when debug logging is
disabled, and the fallback GUI watchdog runs every 100 ms; packet-driven actions
still wake on server updates. Development builds also optimize the encryption
and decompression dependencies, as Azalea's workspace profile is not inherited.

Repeated out-of-view chunk warnings are sampled (the first four, then every
thousandth). The timing report includes the number suppressed in that interval;
other warnings and errors remain visible. This limits log noise when the server
sends chunks beyond the requested view distance. Azalea's default logging feature
is disabled so it does not install a second subscriber over the background logger.

Rebuild with `cargo build --release --locked` and use `run.bat` or `run_all.bat`.
If warnings persist, compare one account against all accounts and profile CPU
usage during chunk loading and inventory updates. These changes remove known
sources of overhead and blocking; a live run is needed to confirm the cause of
any particular warning.

---

## Architecture

- `src/main.rs`: Entry point, Azalea client lifecycle, server event loop, and single-task enchanting workflow.
- `src/armor.rs`: Shared armor ordering, completion checks, drop planning, and inventory slot mapping.
- `src/enchanter.rs`: Core anvil state machine, level-to-level XP math, rapid bottle thrower, arm animations, smooth rotation, and combine scheduler.
- `src/gui.rs`: Container window tracking, order item inspection, and slot click packet interactions.
- `src/nbt.rs`: NBT parsing utilities for item identification and enchantment verification.
- `src/auth.rs`: Minecraft session authentication and token resolution.
