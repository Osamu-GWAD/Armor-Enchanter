# Armor-Enchanter

An autonomous, high-performance Minecraft enchanting bot built in Rust with [Azalea](https://github.com/azalea-rs/azalea). Designed for high-efficiency SMP automation (such as `donutsmp.net`), Armor-Enchanter claims required items from `/order`, places an anvil, dynamically calculates and throws the exact amount of XP required for each combine, and assembles fully enchanted diamond armor sets in optimal anvil order to minimize repair costs.

---

## Features

- **Automated `/order` Withdrawal**:
  - Automatically queries and interacts with the server order GUI.
  - Withdraws the exact required item set without surplus:
    - 1× Diamond Helmet, 1× Diamond Chestplate, 1× Diamond Leggings, 1× Diamond Boots
    - 4× Unbreaking III, 4× Mending, 2× Blast Protection IV, 2× Protection IV enchanted books
    - 1× Anvil and exactly 3 stacks (192) of Bottles o' Enchanting
- **Dynamic XP Calculation & Consumption**:
  - Computes exact XP required for anvil combines using official Minecraft level formulas:
    - Level $0 \to 16$: $\text{XP} = L^2 + 6L$
    - Level $17 \to 31$: $\text{XP} = 2.5L^2 - 40.5L + 360$
    - Level $32+$: $\text{XP} = 4.5L^2 - 162.5L + 2220$
  - Calculates the exact number of XP bottles required ($\lceil \Delta \text{XP} / 7.0 \rceil$), throws only what is needed, and verifies level progression via server `SetExperience` packets before combining.
- **Optimal Anvil Sequencing ([iamcal/enchant-order](https://github.com/iamcal/enchant-order))**:
  - Sequences combines to minimize prior work penalties and cumulative level costs:
    - **Helmet**: Protection IV $\to$ Unbreaking III $\to$ Mending
    - **Chestplate**: Protection IV $\to$ Unbreaking III $\to$ Mending
    - **Leggings**: Blast Protection IV $\to$ Unbreaking III $\to$ Mending
    - **Boots**: Blast Protection IV $\to$ Unbreaking III $\to$ Mending
- **Safe & Anti-Cheat Resilient**:
  - Realistic interaction delays, authentic window click protocols, atomic concurrency, and safe lock management to prevent packet freezing and avoid anti-cheat flags.

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

- `src/main.rs`: Entry point, Azalea client lifecycle, server event loop, and `/order` retrieval flow.
- `src/enchanter.rs`: Core anvil state machine, exact XP math, dynamic bottle thrower, and combine scheduler.
- `src/gui.rs`: Container window tracking and slot click packet interactions.
- `src/nbt.rs`: NBT parsing utilities for item identification and enchantment verification.
- `src/auth.rs`: Minecraft session authentication and token resolution.
