use azalea::inventory::ItemStack;
use serde_json::Value;
use std::collections::HashMap;

/// Structured information extracted from an ItemStack's data and components.
#[derive(Debug, Clone, Default)]
pub struct ItemInfo {
    pub kind: String,
    pub count: i32,
    pub custom_name: Option<String>,
    pub lore: Vec<String>,
    pub enchantments: HashMap<String, u32>,
    pub stored_enchantments: HashMap<String, u32>,
    pub repair_cost: Option<u32>,
    pub raw_components: Option<Value>,
    pub raw_debug: Option<String>,
}

impl ItemInfo {
    /// Check if this item has a specific enchantment or stored enchantment at or above a given level.
    pub fn has_enchantment(&self, name_pattern: &str, min_level: u32) -> bool {
        let pattern = name_pattern.to_lowercase();
        let check_map = |map: &HashMap<String, u32>| {
            map.iter().any(|(k, &lvl)| {
                let k_clean = k.to_lowercase();
                let is_blast = k_clean.contains("blast");
                let is_fire = k_clean.contains("fire");
                let is_proj = k_clean.contains("projectile") || k_clean.contains("proj");

                let matches = if pattern == "protection" {
                    k_clean.contains("protection") && !is_blast && !is_fire && !is_proj
                } else if pattern == "blast_protection" || pattern == "blast protect" || pattern == "blast" {
                    is_blast && (k_clean.contains("protect") || k_clean.contains("protection") || is_blast)
                } else if pattern == "unbreaking" {
                    k_clean.contains("unbreaking")
                } else if pattern == "mending" {
                    k_clean.contains("mending")
                } else {
                    let pat_clean = pattern.strip_prefix("minecraft:").unwrap_or(&pattern);
                    k_clean.contains(pat_clean)
                };
                matches && lvl >= min_level
            })
        };

        if check_map(&self.enchantments) || check_map(&self.stored_enchantments) {
            return true;
        }

        // Also fallback to checking lore, custom name, and raw debug/components,
        // as many SMP servers (like DonutSMP) store enchantments as formatted text in lore / name!
        let matches_text = |text: &str| {
            let t_lower = normalize_small_caps(&text.to_lowercase());
            let is_blast = t_lower.contains("blast");
            let is_fire = t_lower.contains("fire");
            let is_proj = t_lower.contains("projectile") || t_lower.contains("proj");

            let has_base = if pattern == "protection" {
                t_lower.contains("protection") && !is_blast && !is_fire && !is_proj
            } else if pattern == "blast_protection" || pattern == "blast protect" || pattern == "blast" {
                t_lower.contains("blast")
            } else if pattern == "unbreaking" {
                t_lower.contains("unbreaking")
            } else if pattern == "mending" {
                t_lower.contains("mending")
            } else {
                let pat_clean = pattern.strip_prefix("minecraft:").unwrap_or(&pattern).replace('_', " ");
                t_lower.contains(&pattern) || t_lower.contains(&pat_clean)
            };

            if !has_base {
                return false;
            }

            if min_level <= 1 {
                return true;
            }

            let roman = match min_level {
                2 => &["ii", "2", "ⅱ"][..],
                3 => &["iii", "3", "ⅲ"][..],
                4 => &["iv", "4", "ⅳ"][..],
                5 => &["v", "5", "ⅴ"][..],
                _ => &[][..],
            };
            roman.iter().any(|&r| t_lower.contains(r))
        };

        if let Some(ref name) = self.custom_name {
            if matches_text(name) {
                return true;
            }
        }

        for line in &self.lore {
            if matches_text(line) {
                return true;
            }
        }

        if let Some(ref dbg) = self.raw_debug {
            if matches_text(dbg) {
                return true;
            }
        }

        false
    }
}

use azalea_inventory::components::{CustomData, CustomName, Enchantments, ItemName, Lore, RepairCost, StoredEnchantments};
use azalea::Client;

/// Inspect an ItemStack and extract its detailed NBT and component info,
/// optionally resolving data-driven registry keys (like enchantments) using the bot client.
pub fn inspect_item_with_bot(item: &ItemStack, bot: Option<&Client>) -> Option<ItemInfo> {
    let data = match item {
        ItemStack::Empty => return None,
        ItemStack::Present(d) => d,
    };

    let mut info = ItemInfo {
        kind: format!("{:?}", data.kind),
        count: data.count,
        raw_debug: Some(format!("{:?}", data.component_patch)),
        ..Default::default()
    };

    if let Some(rc) = data.component_patch.get::<RepairCost>() {
        info.repair_cost = Some(rc.cost as u32);
    }

    // 1. Direct typed extraction from DataComponentPatch
    if let Some(cn) = data.component_patch.get::<CustomName>() {
        let text = strip_color_and_normalize(&cn.name.to_string());
        if !text.is_empty() {
            info.custom_name = Some(text);
        }
    }
    if let Some(in_name) = data.component_patch.get::<ItemName>() {
        if info.custom_name.is_none() {
            let text = strip_color_and_normalize(&in_name.name.to_string());
            if !text.is_empty() {
                info.custom_name = Some(text);
            }
        }
    }
    if let Some(lore) = data.component_patch.get::<Lore>() {
        for line in &lore.lines {
            let text = strip_color_and_normalize(&line.to_string());
            if !text.is_empty() {
                info.lore.push(text);
            }
        }
    }
    if let Some(stored) = data.component_patch.get::<StoredEnchantments>() {
        for (ench, &level) in &stored.enchantments {
            let ench_name = if let Some(bot) = bot {
                if let Some(key) = bot.resolve_registry_key(ench) {
                    format!("{key:?}").to_lowercase()
                } else {
                    format!("{ench:?}").to_lowercase()
                }
            } else {
                format!("{ench:?}").to_lowercase()
            };
            let plain_name = ench_name.strip_prefix("minecraft:").unwrap_or(&ench_name).to_string();
            info.stored_enchantments.insert(plain_name, level as u32);
            info.stored_enchantments.insert(ench_name, level as u32);

            let ench_lower = format!("{ench:?}").to_lowercase();
            if ench_lower.contains("mending") {
                info.stored_enchantments.insert("mending".to_string(), level as u32);
            } else if ench_lower.contains("unbreaking") {
                info.stored_enchantments.insert("unbreaking".to_string(), level as u32);
            } else if ench_lower.contains("blast_protection") || ench_lower.contains("blast") {
                info.stored_enchantments.insert("blast_protection".to_string(), level as u32);
            } else if ench_lower.contains("protection") {
                info.stored_enchantments.insert("protection".to_string(), level as u32);
            }
        }
    }
    if let Some(enchs) = data.component_patch.get::<Enchantments>() {
        for (ench, &level) in &enchs.levels {
            let ench_name = if let Some(bot) = bot {
                if let Some(key) = bot.resolve_registry_key(ench) {
                    format!("{key:?}").to_lowercase()
                } else {
                    format!("{ench:?}").to_lowercase()
                }
            } else {
                format!("{ench:?}").to_lowercase()
            };
            let plain_name = ench_name.strip_prefix("minecraft:").unwrap_or(&ench_name).to_string();
            info.enchantments.insert(plain_name, level as u32);
            info.enchantments.insert(ench_name, level as u32);

            let ench_lower = format!("{ench:?}").to_lowercase();
            if ench_lower.contains("mending") {
                info.enchantments.insert("mending".to_string(), level as u32);
            } else if ench_lower.contains("unbreaking") {
                info.enchantments.insert("unbreaking".to_string(), level as u32);
            } else if ench_lower.contains("blast_protection") || ench_lower.contains("blast") {
                info.enchantments.insert("blast_protection".to_string(), level as u32);
            } else if ench_lower.contains("protection") {
                info.enchantments.insert("protection".to_string(), level as u32);
            }
        }
    }
    if let Some(cd) = data.component_patch.get::<CustomData>() {
        let debug_nbt = format!("{:?}", cd.nbt);
        if let Some(ref mut raw) = info.raw_debug {
            raw.push_str(" NBT: ");
            raw.push_str(&debug_nbt);
        } else {
            info.raw_debug = Some(debug_nbt);
        }
    }

    // 2. Iterate through all present components in the patch to populate debug
    for (kind, comp) in data.component_patch.iter() {
        if let Some(comp_val) = comp {
            let comp_str = format!("{comp_val:?}");
            if let Some(ref mut raw) = info.raw_debug {
                raw.push_str(" [");
                raw.push_str(&format!("{kind:?}"));
                raw.push_str(": ");
                raw.push_str(&comp_str);
                raw.push_str("]");
            }
        }
    }

    // 3. Serde-JSON fallback
    if let Ok(val) = serde_json::to_value(&data.component_patch) {
        info.raw_components = Some(val.clone());
        extract_from_components(&val, &mut info);
    }

    Some(info)
}

/// Inspect an ItemStack without a Client handle.
pub fn inspect_item(item: &ItemStack) -> Option<ItemInfo> {
    inspect_item_with_bot(item, None)
}

fn extract_from_components(val: &Value, info: &mut ItemInfo) {
    if let Value::Object(map) = val {
        for (k, v) in map {
            let k_clean = k.strip_prefix("minecraft:").unwrap_or(k);

            match k_clean {
                "custom_name" | "item_name" => {
                    info.custom_name = extract_text(v);
                }
                "lore" => {
                    if let Value::Array(lines) = v {
                        for line in lines {
                            if let Some(txt) = extract_text(line) {
                                info.lore.push(txt);
                            }
                        }
                    }
                }
                "stored_enchantments" => {
                    parse_enchantment_map(v, &mut info.stored_enchantments);
                }
                "enchantments" => {
                    parse_enchantment_map(v, &mut info.enchantments);
                }
                "repair_cost" => {
                    if let Some(num) = v.as_u64() {
                        info.repair_cost = Some(num as u32);
                    }
                }
                "custom_data" => {
                    if let Value::Object(cd_map) = v {
                        for (k, v) in cd_map {
                            if k.eq_ignore_ascii_case("repaircost") {
                                if let Some(num) = v.as_u64() {
                                    info.repair_cost = Some(num as u32);
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

pub fn normalize_small_caps(s: &str) -> String {
    let replaced = s
        .replace('Ⅰ', "i")
        .replace('Ⅱ', "ii")
        .replace('Ⅲ', "iii")
        .replace('Ⅳ', "iv")
        .replace('Ⅴ', "v")
        .replace('ⅰ', "i")
        .replace('ⅱ', "ii")
        .replace('ⅲ', "iii")
        .replace('ⅳ', "iv")
        .replace('ⅴ', "v");
    replaced.chars()
        .map(|c| match c {
            'ᴀ' => 'a',
            'ʙ' => 'b',
            'ᴄ' => 'c',
            'ᴅ' => 'd',
            'ᴇ' => 'e',
            'ꜰ' => 'f',
            'ɢ' => 'g',
            'ʜ' => 'h',
            'ɪ' => 'i',
            'ᴊ' => 'j',
            'ᴋ' => 'k',
            'ʟ' => 'l',
            'ᴍ' => 'm',
            'ɴ' => 'n',
            'ᴏ' => 'o',
            'ᴘ' => 'p',
            'ǫ' | 'ϙ' => 'q',
            'ʀ' => 'r',
            's' | 'ꜱ' => 's',
            'ᴛ' => 't',
            'ᴜ' => 'u',
            'ᴠ' => 'v',
            'ᴡ' => 'w',
            'x' | 'ẋ' => 'x',
            'ʏ' => 'y',
            'ᴢ' => 'z',
            other => other,
        })
        .collect()
}

pub fn strip_color_and_normalize(s: &str) -> String {
    let mut cleaned = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '§' || c == '\u{00c2}' {
            if c == '\u{00c2}' && chars.peek() == Some(&'§') {
                chars.next();
            }
            chars.next(); // Skip formatting code
        } else {
            cleaned.push(c);
        }
    }
    normalize_small_caps(&cleaned)
}

fn extract_text(val: &Value) -> Option<String> {
    match val {
        Value::String(s) => {
            let s_stripped = strip_color_and_normalize(s);
            if let Ok(parsed) = serde_json::from_str::<Value>(&s_stripped) {
                return extract_text(&parsed);
            }
            let trimmed = s_stripped.trim().to_string();
            if !trimmed.is_empty() {
                Some(trimmed)
            } else {
                None
            }
        }
        Value::Object(map) => {
            let mut full = String::new();
            if let Some(Value::String(text)) = map.get("text") {
                full.push_str(&strip_color_and_normalize(text));
            }
            if let Some(Value::Array(extra)) = map.get("extra") {
                for part in extra {
                    if let Some(t) = extract_text(part) {
                        full.push_str(&t);
                    }
                }
            }
            let trimmed = full.trim().to_string();
            if !trimmed.is_empty() {
                Some(trimmed)
            } else {
                None
            }
        }
        Value::Array(arr) => {
            let mut full = String::new();
            for item in arr {
                if let Some(t) = extract_text(item) {
                    if !full.is_empty() {
                        full.push(' ');
                    }
                    full.push_str(&t);
                }
            }
            let trimmed = full.trim().to_string();
            if !trimmed.is_empty() {
                Some(trimmed)
            } else {
                None
            }
        }
        _ => None,
    }
}

fn parse_enchantment_map(val: &Value, target: &mut HashMap<String, u32>) {
    if let Some(levels) = val.get("levels").or_else(|| val.get("minecraft:levels")) {
        if let Some(map) = levels.as_object() {
            for (k, v) in map {
                let ench_name = k.strip_prefix("minecraft:").unwrap_or(k).to_string();
                let level = v.as_u64().unwrap_or(1) as u32;
                target.insert(ench_name, level);
            }
            return;
        }
    }

    if let Some(map) = val.as_object() {
        for (k, v) in map {
            if k == "levels" || k == "minecraft:levels" {
                continue;
            }
            let ench_name = k.strip_prefix("minecraft:").unwrap_or(k).to_string();
            let level = v.as_u64().unwrap_or(1) as u32;
            target.insert(ench_name, level);
        }
        return;
    }

    if let Some(arr) = val.as_array() {
        for item in arr {
            if let Some(id) = item.get("id").and_then(|v| v.as_str()) {
                let ench_name = id.strip_prefix("minecraft:").unwrap_or(id).to_string();
                let lvl = item
                    .get("lvl")
                    .or_else(|| item.get("level"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(1) as u32;
                target.insert(ench_name, lvl);
            }
        }
    }
}

// ---------------------------------------------------------
// Item Category Helpers
// ---------------------------------------------------------

pub fn is_unbreaking_3(info: &ItemInfo) -> bool {
    let is_book = info.kind.contains("EnchantedBook")
        || info.kind.contains("Book")
        || info.custom_name.as_deref().unwrap_or("").to_lowercase().contains("book")
        || info.lore.iter().any(|l| l.to_lowercase().contains("book"));
    (is_book || info.kind.to_lowercase().contains("paper"))
        && info.has_enchantment("unbreaking", 3)
}

pub fn is_mending(info: &ItemInfo) -> bool {
    let is_book = info.kind.contains("EnchantedBook")
        || info.kind.contains("Book")
        || info.custom_name.as_deref().unwrap_or("").to_lowercase().contains("book")
        || info.lore.iter().any(|l| l.to_lowercase().contains("book"));
    (is_book || info.kind.to_lowercase().contains("paper"))
        && info.has_enchantment("mending", 1)
}

pub fn is_unbreaking_and_mending(info: &ItemInfo) -> bool {
    is_unbreaking_3(info) && is_mending(info)
}

pub fn is_single_unbreaking_3(info: &ItemInfo) -> bool {
    is_unbreaking_3(info) && !is_mending(info)
}

pub fn is_single_mending(info: &ItemInfo) -> bool {
    is_mending(info) && !is_unbreaking_3(info)
}

pub fn is_protection_4(info: &ItemInfo) -> bool {
    let is_book = info.kind.contains("EnchantedBook")
        || info.kind.contains("Book")
        || info.custom_name.as_deref().unwrap_or("").to_lowercase().contains("book")
        || info.lore.iter().any(|l| l.to_lowercase().contains("book"));
    (is_book || info.kind.to_lowercase().contains("paper"))
        && !info.has_enchantment("blast_protection", 1)
        && info.has_enchantment("protection", 4)
}

pub fn is_blast_protection_4(info: &ItemInfo) -> bool {
    let is_book = info.kind.contains("EnchantedBook")
        || info.kind.contains("Book")
        || info.custom_name.as_deref().unwrap_or("").to_lowercase().contains("book")
        || info.lore.iter().any(|l| l.to_lowercase().contains("book"));
    (is_book || info.kind.to_lowercase().contains("paper"))
        && (info.has_enchantment("blast_protection", 4) || info.has_enchantment("blast protect", 4))
}

pub fn is_diamond_helmet(info: &ItemInfo) -> bool {
    let k = info.kind.to_lowercase();
    let name = info.custom_name.as_deref().unwrap_or("").to_lowercase();
    (k.contains("diamond") || name.contains("diamond"))
        && (k.contains("helmet") || name.contains("helmet"))
}

pub fn is_diamond_chestplate(info: &ItemInfo) -> bool {
    let k = info.kind.to_lowercase();
    let name = info.custom_name.as_deref().unwrap_or("").to_lowercase();
    (k.contains("diamond") || name.contains("diamond"))
        && (k.contains("chestplate") || name.contains("chestplate") || k.contains("chest") || name.contains("chest"))
}

pub fn is_diamond_leggings(info: &ItemInfo) -> bool {
    let k = info.kind.to_lowercase();
    let name = info.custom_name.as_deref().unwrap_or("").to_lowercase();
    (k.contains("diamond") || name.contains("diamond"))
        && (k.contains("leggings") || name.contains("leggings") || k.contains("legs") || name.contains("legs"))
}

pub fn is_diamond_boots(info: &ItemInfo) -> bool {
    let k = info.kind.to_lowercase();
    let name = info.custom_name.as_deref().unwrap_or("").to_lowercase();
    (k.contains("diamond") || name.contains("diamond"))
        && (k.contains("boots") || name.contains("boots"))
}

pub fn is_diamond_armor(info: &ItemInfo) -> bool {
    is_diamond_helmet(info)
        || is_diamond_chestplate(info)
        || is_diamond_leggings(info)
        || is_diamond_boots(info)
}

pub fn is_xp_bottle(info: &ItemInfo) -> bool {
    let k = info.kind.to_lowercase();
    let name = info.custom_name.as_deref().unwrap_or("").to_lowercase();
    let in_lore = info.lore.iter().any(|l| {
        let ll = l.to_lowercase();
        ll.contains("bottle") || ll.contains("experience") || ll.contains("enchanting")
    });
    k.contains("experiencebottle")
        || k.contains("experience_bottle")
        || k.contains("expbottle")
        || k.contains("exp_bottle")
        || name.contains("bottle")
        || name.contains("enchanting")
        || name.contains("experience")
        || in_lore
}

pub fn is_anvil(info: &ItemInfo) -> bool {
    let k = info.kind.to_lowercase();
    k.contains("anvil")
}

/// Check if a GUI slot button corresponds to the "Your Orders" / "Your Items" menu.
pub fn is_your_orders_button(info: &ItemInfo) -> bool {
    let check = |s: &str| {
        let sl = s.to_lowercase();
        sl.contains("your order")
            || sl.contains("your orders")
            || sl.contains("my orders")
            || sl.contains("your items")
            || sl.contains("orders")
    };

    if let Some(ref name) = info.custom_name {
        if check(name) {
            return true;
        }
    }

    for line in &info.lore {
        if check(line) {
            return true;
        }
    }

    false
}

/// Check if a GUI slot button corresponds to the "Collect" / "Confirm" button.
pub fn is_confirm_button(info: &ItemInfo) -> bool {
    let k = info.kind.to_lowercase();
    // Directly recognized confirmation items (lime stained glass pane, emerald, lime wool, green wool)
    if k.contains("lime_stained_glass_pane")
        || k.contains("emerald")
        || k.contains("lime_wool")
        || k.contains("green_wool")
    {
        return true;
    }

    // Never consider standard navigation items as confirm buttons
    if k.contains("book") || k.contains("hopper") || k.contains("sign") || k.contains("shard") {
        return false;
    }

    let check_strict = |s: &str| {
        let sl = normalize_small_caps(&s.to_lowercase());
        sl.contains("collect item")
            || sl.contains("claim item")
            || sl.contains("confirm deliver")
            || sl.contains("confirm order")
            || sl.contains("confirm")
            || sl.contains("deliver item")
            || sl.contains("fulfill order")
            || sl.contains("accept")
            || sl.contains("retrieve item")
    };

    if let Some(ref name) = info.custom_name {
        let nl = normalize_small_caps(&name.to_lowercase());
        // Explicitly exclude navigation buttons
        if nl.contains("orders") || nl.contains("filter") || nl.contains("search") || nl.contains("shop") {
            return false;
        }
        if check_strict(&nl) || nl == "deliver" || nl == "fulfill" || nl == "collect" || nl == "claim" {
            return true;
        }
    }

    // For chests (used in /order -> order submenu as "Collect Items"), match if name or lore indicates collect/claim
    if k.contains("chest") {
        if let Some(ref name) = info.custom_name {
            let nl = normalize_small_caps(&name.to_lowercase());
            if nl.contains("collect") || nl.contains("claim") || nl.contains("retrieve") || nl.contains("delivery") || nl.contains("item") {
                return true;
            }
        }
        if info.lore.iter().any(|l| {
            let ll = normalize_small_caps(&l.to_lowercase());
            ll.contains("collect") || ll.contains("claim") || ll.contains("retrieve")
        }) {
            return true;
        }
        return false;
    }

    for line in &info.lore {
        let ll = normalize_small_caps(&line.to_lowercase());
        if ll.contains("click to confirm")
            || ll.contains("click to deliver")
            || ll.contains("click to fulfill")
            || ll.contains("click to collect")
            || ll.contains("click to claim")
            || ll.contains("collect item")
            || ll.contains("claim item")
        {
            return true;
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_enchantment_detection() {
        let mut info = ItemInfo {
            kind: "EnchantedBook".to_string(),
            count: 1,
            ..Default::default()
        };
        info.stored_enchantments.insert("unbreaking".to_string(), 3);
        assert!(is_unbreaking_3(&info));
        assert!(!is_mending(&info));

        info.stored_enchantments.insert("mending".to_string(), 1);
        assert!(is_mending(&info));

        info.stored_enchantments.insert("protection".to_string(), 4);
        assert!(is_protection_4(&info));

        info.stored_enchantments.insert("blast_protection".to_string(), 4);
        assert!(is_blast_protection_4(&info));
    }

    #[test]
    fn test_lore_fallback_detection() {
        let info = ItemInfo {
            kind: "EnchantedBook".to_string(),
            count: 1,
            lore: vec!["Protection IV".to_string(), "Unbreaking III".to_string()],
            ..Default::default()
        };
        assert!(is_protection_4(&info));
        assert!(is_unbreaking_3(&info));
    }

    #[test]
    fn test_your_orders_button() {
        let info = ItemInfo {
            kind: "Chest".to_string(),
            count: 1,
            custom_name: Some("Your Orders".to_string()),
            ..Default::default()
        };
        assert!(is_your_orders_button(&info));

        let info2 = ItemInfo {
            kind: "Hopper".to_string(),
            count: 1,
            lore: vec!["Click here to view your items".to_string()],
            ..Default::default()
        };
        assert!(is_your_orders_button(&info2));
    }

    #[test]
    fn test_extract_json_text_component() {
        let json_val: Value = serde_json::from_str(
            r##"{"text":"","extra":[{"bold":false,"color":"#FFFFFF","italic":false,"text":"Anvil"}]}"##,
        )
        .unwrap();
        let extracted = extract_text(&json_val);
        assert_eq!(extracted, Some("Anvil".to_string()));

        let small_caps_val: Value = serde_json::from_str(
            r#"{"text":"ʏᴏᴜʀ ᴏʀᴅᴇʀs"}"#,
        )
        .unwrap();
        let extracted_sc = extract_text(&small_caps_val);
        assert_eq!(extracted_sc, Some("your orders".to_string()));
    }

    #[test]
    fn test_diamond_armor_types() {
        let helmet = ItemInfo {
            kind: "DiamondHelmet".to_string(),
            count: 1,
            ..Default::default()
        };
        assert!(is_diamond_helmet(&helmet));
        assert!(is_diamond_armor(&helmet));
        assert!(!is_diamond_boots(&helmet));

        let chestplate = ItemInfo {
            kind: "DiamondChestplate".to_string(),
            count: 1,
            ..Default::default()
        };
        assert!(is_diamond_chestplate(&chestplate));
        assert!(is_diamond_armor(&chestplate));

        let leggings = ItemInfo {
            kind: "DiamondLeggings".to_string(),
            count: 1,
            ..Default::default()
        };
        assert!(is_diamond_leggings(&leggings));
        assert!(is_diamond_armor(&leggings));

        let boots = ItemInfo {
            kind: "DiamondBoots".to_string(),
            count: 1,
            ..Default::default()
        };
        assert!(is_diamond_boots(&boots));
        assert!(is_diamond_armor(&boots));
    }

    #[test]
    fn test_inspect_real_itemstack() {
        let patch = azalea_inventory::DataComponentPatch::default();
        let item = ItemStack::Present(azalea::inventory::ItemStackData {
            kind: azalea_registry::builtin::ItemKind::DiamondHelmet,
            count: 1,
            component_patch: patch,
        });
        let info = inspect_item(&item).unwrap();
        assert!(is_diamond_helmet(&info));
    }

    #[test]
    fn test_protection_4_with_identifier_debug_format() {
        let mut info = ItemInfo {
            kind: "EnchantedBook".to_string(),
            count: 1,
            ..Default::default()
        };
        // Simulated Identifier debug representation
        info.stored_enchantments.insert("identifier { namespace: \"minecraft\", path: \"protection\" }".to_string(), 4);
        assert!(is_protection_4(&info), "Should recognize Protection IV even from verbose Debug identifier string");
        assert!(!is_blast_protection_4(&info), "Protection IV must not match Blast Protection");

        let mut blast_info = ItemInfo {
            kind: "EnchantedBook".to_string(),
            count: 1,
            ..Default::default()
        };
        blast_info.stored_enchantments.insert("identifier { namespace: \"minecraft\", path: \"blast_protection\" }".to_string(), 4);
        assert!(is_blast_protection_4(&blast_info), "Should recognize Blast Protection IV from Debug identifier string");
        assert!(!is_protection_4(&blast_info), "Blast Protection IV must not match normal Protection");
    }

    #[test]
    fn test_lore_and_custom_name_with_spaces_and_unicode() {
        let book_with_space = ItemInfo {
            kind: "EnchantedBook".to_string(),
            count: 1,
            lore: vec!["Blast Protection IV".to_string()],
            ..Default::default()
        };
        assert!(is_blast_protection_4(&book_with_space), "Should match Blast Protection with spaces");

        let book_with_num = ItemInfo {
            kind: "Book".to_string(),
            count: 1,
            custom_name: Some("Protection 4 Book".to_string()),
            ..Default::default()
        };
        assert!(is_protection_4(&book_with_num), "Should match Protection 4 with Arabic digit");

        let book_with_unicode = ItemInfo {
            kind: "Book".to_string(),
            count: 1,
            lore: vec!["Unbreaking Ⅲ".to_string()],
            ..Default::default()
        };
        assert!(is_unbreaking_3(&book_with_unicode), "Should match Unbreaking III with Unicode Roman numeral");
    }
}

