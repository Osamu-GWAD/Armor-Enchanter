use azalea::inventory::ItemStack;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

#[derive(Clone, Debug, PartialEq)]
pub struct OrderListing {
    pub slot: i16,
    pub name: String,
    pub icon: ItemStack,
}

/// Evidence from actual order screens and responses, never from inventory quotas.
#[derive(Default)]
pub struct StockAudit {
    pages: BTreeMap<usize, (Vec<OrderListing>, bool)>,
    empty: Vec<(usize, OrderListing, Instant)>,
    selected: Option<(usize, OrderListing)>,
    collect_since: Option<Instant>,
}

impl StockAudit {
    pub fn start_scan(&mut self) {
        self.empty.clear();
        self.continue_scan();
    }

    /// Reopening after Collect continues the audit while other listings are checked.
    pub fn continue_scan(&mut self) {
        self.pages.clear();
        self.selected = None;
        self.collect_since = None;
        self.empty
            .retain(|(_, _, time)| time.elapsed() < Duration::from_secs(45));
    }

    pub fn observe_page(&mut self, page: usize, entries: Vec<OrderListing>, has_next: bool) {
        self.pages.insert(page, (entries, has_next));
    }

    pub fn select(&mut self, page: usize, listing: OrderListing) {
        self.selected = Some((page, listing));
        self.collect_since = None;
    }

    pub fn collect_requested(&mut self) {
        if self.selected.is_some() {
            self.collect_since = Some(Instant::now());
        }
    }

    pub fn confirm_empty(&mut self) -> Option<String> {
        if !self
            .collect_since
            .take()
            .is_some_and(|t| t.elapsed() < Duration::from_secs(10))
        {
            return None;
        }
        let (page, listing) = self.selected.take()?;
        let name = listing.name.clone();
        self.empty.push((page, listing, Instant::now()));
        Some(name)
    }

    pub fn is_empty(&self, page: usize, listing: &OrderListing) -> bool {
        self.empty.iter().any(|(p, entry, time)| {
            *p == page
                && entry == listing
                && time.elapsed() < Duration::from_secs(45)
        })
    }

    /// A successful claim can empty its screen; only an explicit no-items reply
    /// proves the selected listing was empty.
    pub fn collection_finished(&mut self) {
        self.selected = None;
        self.collect_since = None;
    }

    pub fn confirmed_unavailable(&self, name: &str, inventory_count: u32) -> bool {
        if inventory_count != 0 || name.is_empty() {
            return false;
        }
        let Some((&last, (_, has_next))) = self.pages.last_key_value() else {
            return false;
        };
        if *has_next {
            return false;
        }
        // Every page must have arrived. Never infer stock from a timeout or a partial scan.
        for page in 0..=last {
            let Some((entries, next)) = self.pages.get(&page) else {
                return false;
            };
            if *next != (page < last) {
                return false;
            }
            for listing in entries.iter().filter(|entry| entry.name == name) {
                if !self.is_empty(page, listing) {
                    return false;
                }
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn listing(slot: i16) -> OrderListing {
        OrderListing {
            slot,
            name: "Experience Bottles".into(),
            icon: ItemStack::Empty,
        }
    }

    #[test]
    fn empty_inventory_alone_never_proves_empty_orders() {
        let mut audit = StockAudit::default();
        assert!(!audit.confirmed_unavailable("Experience Bottles", 0));
        audit.observe_page(0, vec![listing(1)], false);
        assert!(!audit.confirmed_unavailable("Experience Bottles", 0));
    }

    #[test]
    fn explicit_no_items_response_confirms_stock_and_success_cancels_pending_chat() {
        let mut audit = StockAudit::default();
        audit.observe_page(0, vec![listing(1)], false);
        audit.select(0, listing(1));
        audit.collect_requested();
        assert!(audit.confirm_empty().is_some());
        assert!(audit.confirmed_unavailable("Experience Bottles", 0));
        assert!(audit.confirm_empty().is_none());
        audit.select(0, listing(2));
        audit.collect_requested();
        audit.collection_finished();
        assert!(audit.confirm_empty().is_none());
    }

    #[test]
    fn successful_claim_does_not_mark_its_listing_empty() {
        let mut audit = StockAudit::default();
        audit.observe_page(0, vec![listing(1)], false);
        audit.select(0, listing(1));
        audit.collect_requested();
        audit.collection_finished();
        assert!(!audit.is_empty(0, &listing(1)));
        assert!(!audit.confirmed_unavailable("Experience Bottles", 0));
        assert!(audit.confirm_empty().is_none());
    }

    #[test]
    fn an_order_on_a_later_page_prevents_an_out_of_stock_alert() {
        let mut audit = StockAudit::default();
        audit.observe_page(0, vec![listing(1)], true);
        audit.select(0, listing(1));
        audit.collect_requested();
        audit.confirm_empty();
        audit.observe_page(1, vec![listing(1)], false);
        assert!(!audit.confirmed_unavailable("Experience Bottles", 0));
        audit.select(1, listing(1));
        audit.collect_requested();
        audit.confirm_empty();
        assert!(audit.confirmed_unavailable("Experience Bottles", 0));
    }

    #[test]
    fn alert_requires_zero_in_inventory_and_every_matching_order_empty() {
        let mut audit = StockAudit::default();
        audit.observe_page(0, vec![listing(1), listing(2)], false);
        for slot in [1, 2] {
            audit.select(0, listing(slot));
            audit.collect_requested();
            assert!(audit.confirm_empty().is_some());
            assert_eq!(
                audit.confirmed_unavailable("Experience Bottles", 0),
                slot == 2
            );
        }
        for count in [1, 32, 255, 256] {
            assert!(!audit.confirmed_unavailable("Experience Bottles", count));
        }
    }

    #[test]
    fn missing_order_requires_all_pages_and_a_fresh_scan() {
        let mut audit = StockAudit::default();
        audit.observe_page(0, vec![], true);
        assert!(!audit.confirmed_unavailable("Mending Book", 0));
        audit.observe_page(2, vec![], false);
        assert!(!audit.confirmed_unavailable("Mending Book", 0));
        audit.observe_page(1, vec![], true);
        assert!(audit.confirmed_unavailable("Mending Book", 0));
        audit.start_scan();
        assert!(!audit.confirmed_unavailable("Mending Book", 0));
    }

    #[test]
    fn unrequested_chat_and_new_scan_cannot_reuse_empty_evidence() {
        let mut audit = StockAudit::default();
        audit.select(0, listing(1));
        assert!(audit.confirm_empty().is_none());
        audit.collect_requested();
        assert!(audit.confirm_empty().is_some());
        audit.observe_page(0, vec![listing(1)], false);
        assert!(audit.confirmed_unavailable("Experience Bottles", 0));
        audit.start_scan();
        audit.observe_page(0, vec![listing(1)], false);
        assert!(!audit.confirmed_unavailable("Experience Bottles", 0));
    }

    #[test]
    fn continuing_scan_skips_only_the_exact_empty_listing() {
        use azalea_registry::builtin::ItemKind;
        let mut audit = StockAudit::default();
        let first = OrderListing {
            icon: ItemStack::new(ItemKind::ExperienceBottle, 1),
            ..listing(1)
        };
        let second = OrderListing { slot: 2, ..first.clone() };
        audit.select(0, first.clone());
        audit.collect_requested();
        assert_eq!(audit.confirm_empty().as_deref(), Some("Experience Bottles"));
        audit.continue_scan();
        audit.observe_page(0, vec![first.clone(), second.clone()], false);
        assert!(audit.is_empty(0, &first));
        assert!(!audit.is_empty(0, &second));
        assert!(!audit.confirmed_unavailable("Experience Bottles", 0));

        let changed = OrderListing {
            icon: ItemStack::new(ItemKind::ExperienceBottle, 2),
            ..first.clone()
        };
        audit.observe_page(0, vec![changed.clone()], false);
        assert!(!audit.is_empty(0, &changed));
        assert!(!audit.confirmed_unavailable("Experience Bottles", 0));
        audit.empty[0].2 = Instant::now() - Duration::from_secs(46);
        assert!(!audit.is_empty(0, &first));
        audit.continue_scan();
        assert!(audit.empty.is_empty());
    }
}
