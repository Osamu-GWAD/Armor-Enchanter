# Armor-Enchanter

An autonomous, high-performance Minecraft enchanting bot built in Rust with [Azalea](https://github.com/azalea-rs/azalea). Designed for high-efficiency SMP automation (such as `donutsmp.net`), Armor-Enchanter claims required items from `/order`, places an anvil, dynamically calculates and throws the exact amount of XP required for each combine, and assembles fully enchanted diamond armor sets in optimal anvil order to minimize repair costs.

---

## Features

- **Automated `/order` Withdrawal**:
  - Automatically queries and interacts with the server order GUI.
  - Withdraws the exact required item set without surplus:
    - 1× Diamond Helmet, 1× Diamond Chestplate, 1× Diamond Leggings, 1× Diamond Boots
    - 4× Unbreaking III, 4× Mending, 2× Blast Protection IV, 2× Protection IV enchanted books
    - 1× Anvil and exactly 2 stacks (128) of Bottles o' Enchanting (sufficient for all combines with 22+ bottles margin)

- **Exact Level-to-Level XP Calculation & Rapid Throwing**:
  - Computes exact XP required for anvil combines using official Minecraft Java level formulas:
    - Points to next level: $\text{xp\_to\_next}(L) = \begin{cases} 2L + 7 & 0 \le L \le 15 \\ 5L - 38 & 16 \le L \le 30 \\ 9L - 158 & L \ge 31 \end{cases}$
    - Total level XP: $\text{TotalXP}(L) = \begin{cases} L^2 + 6L & 0 \le L \le 16 \\ 2.5L^2 - 40.5L + 360 & 17 \le L \le 31 \\ 4.5L^2 - 162.5L + 2220 & L \ge 32 \end{cases}$
    - Exact deficit from Level $X$ (with progress) to Level $Y$: $\Delta\text{XP} = \text{TotalXP}(Y) - \text{true\_current\_xp}$
    - Bottles needed: $\lceil \Delta\text{XP} / 6.8 \rceil$ (average 7.0 XP per bottle with variance safety margin)
  - Throws bottles in rapid, authentic streams (1 tick per throw) with `ServerboundSwing` arm animations, reaching required levels in under 1 second.

- **Optimal Anvil Sequencing ([iamcal/enchant-order](https://github.com/iamcal/enchant-order))**:
  - Sequences combines to minimize prior work penalties and cumulative level costs:
    - **Helmet**: Protection IV (4 lvl) $\to$ Unbreaking III (4 lvl) $\to$ Mending (5 lvl)
    - **Chestplate**: Protection IV (4 lvl) $\to$ Unbreaking III (4 lvl) $\to$ Mending (5 lvl)
    - **Leggings**: Blast Protection IV (8 lvl) $\to$ Unbreaking III (4 lvl) $\to$ Mending (5 lvl)
    - **Boots**: Blast Protection IV (8 lvl) $\to$ Unbreaking III (4 lvl) $\to$ Mending (5 lvl)

- **Anti-Cheat Resilient & Humanized Interactions**:
  - **Smooth View Interpolation**: Mimics human mouse rotation using a cosine ease-in-out curve ($\alpha = \frac{1 - \cos(\pi t)}{2}$) over 2–6 ticks, eliminating abrupt aim snapping while keeping rotations responsive.
  - **Arm Swing Animations**: Sends `ServerboundSwing` packet on every bottle thrown, anvil placement, and anvil block interaction.
  - **Armor Equip Prevention**: Automatically selects safe hotbar slots (holding books or empty hands) when clicking blocks, and actively unequips armor if accidentally worn.
  - **High-Speed Throughput**: Optimized container packet delays, quickmove transfers (3 ticks / 150ms), and sub-second anvil combines allow the complete order retrieval, placement, and all 12 combines to finish in under 35 seconds.
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

## Configuration

1. Copy `.env.example` to `.env`:
   ```bash
   cp .env.example .env
   ```
2. Open `.env` and add your Minecraft access token:
   ```env
   MC_TOKEN=your_token_here
   ```
   *(Note: `.env` is included in `.gitignore` to prevent leaking tokens).*

---

## Building & Running

### Prerequisites
- [Rust](https://rustup.rs/) (Nightly toolchain specified in `rust-toolchain.toml`)

### Running the Bot
```bash
cargo run --release
```

### Running Unit Tests
```bash
cargo test
```

---

## Architecture

- `src/main.rs`: Entry point, Azalea client lifecycle, server event loop, and single-task enchanting workflow.
- `src/enchanter.rs`: Core anvil state machine, level-to-level XP math, rapid bottle thrower, arm animations, smooth rotation, and combine scheduler.
- `src/gui.rs`: Container window tracking, order item inspection, and slot click packet interactions.
- `src/nbt.rs`: NBT parsing utilities for item identification and enchantment verification.
- `src/auth.rs`: Minecraft session authentication and token resolution.
