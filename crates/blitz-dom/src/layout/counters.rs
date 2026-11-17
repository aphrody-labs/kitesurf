//! CSS counters (CSS 2.2 §12.4 / css-lists-3): a document-order pass run
//! after styling and before box construction.
//!
//! Produces side tables consumed by box construction: the `list-item`
//! counter value per list item (markers) and resolved ::before/::after
//! content strings for `counter()`/`counters()`/`attr()`/multi-item content.

use std::collections::HashMap;

use markup5ever::local_name;
use blitz_traits::node_id::NodeId;
use style::Atom;
use style::counter_style::CounterStyle;
use style::values::computed::{Content, ContentItem};

use crate::BaseDocument;
use crate::layout::damage::CONSTRUCT_BOX;

/// Side tables produced by `resolve_counters`, keyed by node id.
#[derive(Default)]
pub struct CounterOutput {
    /// display:list-item element -> its `list-item` counter value.
    pub list_item_values: HashMap<NodeId, i32>,
    /// element -> resolved ::before content text (non-trivial content only).
    pub before_text: HashMap<NodeId, String>,
    /// element -> resolved ::after content text (non-trivial content only).
    pub after_text: HashMap<NodeId, String>,
}

/// One instantiated counter: CSS2.2 scope = the creating element, its
/// descendants, and its following siblings (popped when the parent closes).
struct Scope {
    value: i32,
    owner_depth: usize,
    /// list-item scopes created by <ol reversed> count downwards.
    reversed: bool,
}

#[derive(Default)]
struct CounterState {
    stacks: HashMap<Atom, Vec<Scope>>,
    /// Live scopes in creation order. DFS pops every deeper scope before
    /// creating a new one, so owner depths are non-decreasing toward the
    /// top: pop_deeper_than only ever peels entries off this stack's end
    /// (O(1) amortized, instead of scanning every counter name per element).
    order: Vec<(Atom, usize)>,
    /// Shared reversed-prescan budget for the whole pass.
    prescan_budget: PrescanBudget,
}

impl CounterState {
    // Sibling counter-reset shadows the previous sibling's scope instead
    // of nesting inside it (both were created at the same tree depth).
    fn create(&mut self, name: &Atom, value: i32, depth: usize, reversed: bool) {
        let stack = self.stacks.entry(name.clone()).or_default();
        if let Some(top) = stack.last_mut() {
            if top.owner_depth == depth {
                top.value = value;
                top.reversed = reversed;
                return;
            }
        }
        stack.push(Scope {
            value,
            owner_depth: depth,
            reversed,
        });
        // Depth-0 scopes (the root element's own, and the implicit-root
        // ones set/increment create) never pop: pop_deeper_than(d) only
        // peels owner_depth > d and d is never negative. Keeping them off
        // the order stack preserves its non-decreasing-depth invariant:
        // an implicit-root entry pushed mid-subtree would otherwise sit on
        // top of deeper entries and block them from ever popping.
        if depth > 0 {
            self.order.push((name.clone(), depth));
        }
    }

    // counter-set / counter-increment target the innermost scope. A
    // missing scope creates one at the root: browsers keep implicitly
    // created counters visible to everything that follows in flat-tree
    // order (WPT counter-slot-order pins 1,2,3 across slot boundaries).
    fn set(&mut self, name: &Atom, value: i32, _depth: usize) {
        match self.stacks.get_mut(name).and_then(|s| s.last_mut()) {
            Some(top) => top.value = value,
            None => self.create(name, value, 0, false),
        }
    }

    fn increment(&mut self, name: &Atom, by: i32, _depth: usize) {
        match self.stacks.get_mut(name).and_then(|s| s.last_mut()) {
            Some(top) => top.value = top.value.saturating_add(by),
            None => self.create(name, by, 0, false),
        }
    }

    fn innermost(&self, name: &Atom) -> Option<&Scope> {
        self.stacks.get(name).and_then(|s| s.last())
    }

    fn value(&self, name: &Atom) -> i32 {
        self.innermost(name).map(|s| s.value).unwrap_or(0)
    }

    fn all_values(&self, name: &Atom) -> Vec<i32> {
        self.stacks
            .get(name)
            .map(|s| s.iter().map(|sc| sc.value).collect())
            .unwrap_or_default()
    }

    fn pop_deeper_than(&mut self, depth: usize) {
        while self
            .order
            .last()
            .is_some_and(|(_, owner_depth)| *owner_depth > depth)
        {
            let (name, _) = self.order.pop().unwrap();
            if let Some(stack) = self.stacks.get_mut(&name) {
                stack.pop();
            }
        }
    }
}

/// Format a counter value in a given counter style. Out-of-range values
/// for alphabetic/roman systems fall back to decimal, per css-counter-styles.
pub fn format_counter(value: i32, style: &CounterStyle) -> String {
    match style {
        CounterStyle::None => String::new(),
        CounterStyle::Name(name) => format_named(value, name.0.as_ref()),
        CounterStyle::String(s) => s.to_string(),
        CounterStyle::Symbols { symbols, .. } => {
            let syms = &symbols.0;
            if syms.is_empty() {
                return value.to_string();
            }
            let idx = (i64::from(value) - 1).rem_euclid(syms.len() as i64) as usize;
            match &syms[idx] {
                style::counter_style::Symbol::String(s) => s.to_string(),
                style::counter_style::Symbol::Ident(id) => id.0.to_string(),
            }
        }
    }
}

fn format_named(value: i32, name: &str) -> String {
    match name {
        "decimal" => value.to_string(),
        "decimal-leading-zero" => {
            if value < 0 {
                format!("-{:02}", -(value as i64))
            } else {
                format!("{:02}", value)
            }
        }
        "lower-alpha" | "lower-latin" => alpha(value, false),
        "upper-alpha" | "upper-latin" => alpha(value, true),
        "lower-roman" => roman(value, false),
        "upper-roman" => roman(value, true),
        "disc" => "•".to_string(),
        "circle" => "◦".to_string(),
        "square" => "▪".to_string(),
        "disclosure-open" => "▾".to_string(),
        "disclosure-closed" => "▸".to_string(),
        "none" => String::new(),
        _ => value.to_string(),
    }
}

fn alpha(value: i32, upper: bool) -> String {
    if value < 1 {
        return value.to_string();
    }
    let mut v = value as u32;
    let mut out = Vec::new();
    while v > 0 {
        v -= 1;
        let c = b'a' + (v % 26) as u8;
        out.push(if upper { c.to_ascii_uppercase() } else { c } as char);
        v /= 26;
    }
    out.reverse();
    out.into_iter().collect()
}

fn roman(value: i32, upper: bool) -> String {
    if !(1..=3999).contains(&value) {
        return value.to_string();
    }
    const M: [(i32, &str); 13] = [
        (1000, "m"), (900, "cm"), (500, "d"), (400, "cd"), (100, "c"),
        (90, "xc"), (50, "l"), (40, "xl"), (10, "x"), (9, "ix"),
        (5, "v"), (4, "iv"), (1, "i"),
    ];
    let mut v = value;
    let mut out = String::new();
    for (n, s) in M {
        while v >= n {
            out.push_str(s);
            v -= n;
        }
    }
    if upper { out.to_uppercase() } else { out }
}

/// Accumulator for the reversed() auto-start rule: `start = X − S`, where
/// S sums increments up to and including the first setting element, and X
/// is that set's value: or, with no set in scope, minus the last
/// non-zero increment (the final value lands on its own magnitude).
#[derive(Default)]
struct ReversedScan {
    sum: i64,
    last_inc: i64,
    frozen: bool,
    any: bool,
    set_value: Option<i64>,
}

/// Shared node budget for all reversed-auto-start prescans in one resolve.
/// N sibling reversed() resets each scan their scope, so untrusted CSS can
/// otherwise drive O(N²) node visits. When the budget runs out remaining
/// scans freeze where they are: numbering degrades, CPU stays bounded.
pub(crate) struct PrescanBudget(pub(crate) usize);

impl Default for PrescanBudget {
    fn default() -> Self {
        // Generous for real content: documents with reversed counters have
        // a handful, and each scope is typically a short list.
        PrescanBudget(100_000)
    }
}

impl ReversedScan {
    fn increment(&mut self, by: i32) {
        if self.frozen || by == 0 {
            return;
        }
        self.sum += by as i64;
        self.last_inc = by as i64;
        self.any = true;
    }

    fn set(&mut self, value: i32, element_incremented: bool) {
        if self.frozen {
            return;
        }
        // A setter that doesn't itself increment still consumes one step
        // of the sequence: count a virtual increment of the last size
        // (WPT li-value-reversed-019: set on a non-incrementing div).
        if !element_incremented && self.any {
            self.sum += self.last_inc;
        }
        self.set_value = Some(value as i64);
        self.frozen = true;
    }

    fn start_value(&self) -> i32 {
        let raw = match self.set_value {
            Some(x) => x - self.sum,
            None if self.any => -self.last_inc - self.sum,
            None => 0,
        };
        raw.clamp(i32::MIN as i64 + 1, i32::MAX as i64) as i32
    }
}

impl BaseDocument {
    /// css-lists §instantiating-counters: a reversed() counter without an
    /// explicit value starts so the last increment before the first
    /// counter-set (or scope end) lands on its own magnitude. Prescan the
    /// scope: this element's remaining declarations, its subtree, and its
    /// following siblings' subtrees: minus regions shadowed by a nested
    /// reset of the same counter name.
    fn reversed_auto_start(
        &self,
        origin: NodeId,
        name: &Atom,
        is_list_item: bool,
        subtree_only: bool,
        budget: &mut PrescanBudget,
    ) -> i32 {
        let mut scan = ReversedScan::default();
        // The origin's own increments/sets apply to the new scope (its
        // reset instantiated it), but its implicit list-item increment
        // does not (an <ol> is not a list item).
        self.scan_element_decls(origin, name, is_list_item, &mut scan, false);
        self.scan_children(self.nodes[origin].flat_children().to_vec(), name, is_list_item, &mut scan, budget);
        // A CSS reversed() scope extends to the origin's following
        // siblings; the HTML-implied list scope is subtree-only.
        if !subtree_only {
            if let Some(parent) = self.nodes[origin].parent {
                let sibs = self.nodes[parent].flat_children();
                if let Some(pos) = sibs.iter().position(|&s| s == origin) {
                    let rest: Vec<NodeId> = sibs[pos + 1..].to_vec();
                    self.scan_children(rest, name, is_list_item, &mut scan, budget);
                }
            }
        }
        scan.start_value()
    }

    // True when this element instantiates a new scope for `name`. Both
    // CSS counter-reset and the HTML-implied list scope (the UA sheet's
    // `ol, ul, menu { counter-reset: list-item }`) cover the resetting
    // element's subtree and its following siblings (counter-list-item-2
    // pins the sibling behaviour: an li after a nested ol continues the
    // nested scope, not the outer one).
    fn resets_name(&self, node_id: NodeId, name: &Atom, is_list_item: bool) -> bool {
        let node = &self.nodes[node_id];
        let Some(el) = node.element_data() else { return false };
        let Some(styles) = node.primary_styles() else { return false };
        if styles
            .get_counters()
            .counter_reset
            .iter()
            .any(|p| p.name.0 == *name)
        {
            return true;
        }
        is_list_item
            && matches!(
                el.name.local,
                local_name!("ol") | local_name!("ul") | local_name!("menu") | local_name!("dir")
            )
    }

    fn scan_pseudo_decls(&self, node_id: NodeId, idx: usize, name: &Atom, scan: &mut ReversedScan) {
        let style_data = self.nodes[node_id].try_stylo_element_data().and_then(|s| s.get());
        let Some(pseudo) = style_data.as_ref().and_then(|d| d.styles.pseudos.as_array()[idx].clone()) else {
            return;
        };
        let pc = pseudo.get_counters();
        for pair in pc.counter_increment.iter() {
            if pair.name.0 == *name {
                scan.increment(pair.value);
            }
        }
        if let Some(v) = pc.counter_set.iter().find(|p| p.name.0 == *name).map(|p| p.value) {
            scan.set(v, true);
        }
    }

    fn scan_element_decls(
        &self,
        node_id: NodeId,
        name: &Atom,
        is_list_item: bool,
        scan: &mut ReversedScan,
        implicit: bool,
    ) {
        let node = &self.nodes[node_id];
        let Some(el) = node.element_data() else { return };
        let Some(styles) = node.primary_styles() else { return };
        let counters_style = styles.get_counters();
        let mut mentioned = false;
        let mut incremented = false;
        for pair in counters_style.counter_increment.iter() {
            if pair.name.0 == *name {
                scan.increment(pair.value);
                mentioned = true;
                incremented = incremented || pair.value != 0;
            }
        }
        // Reversed scopes count implicit list-item increments as -1.
        if implicit && is_list_item && !mentioned && styles.clone_display().is_list_item() {
            scan.increment(-1);
            incremented = true;
        }
        let css_set = counters_style
            .counter_set
            .iter()
            .find(|p| p.name.0 == *name)
            .map(|p| p.value);
        let li_value = (is_list_item && el.name.local == local_name!("li"))
            .then(|| el.attr_parsed::<i32>(local_name!("value")))
            .flatten();
        // CSS counter-set lands after the li value attribute (main pass
        // order), so it wins as the effective set value at this element.
        if let Some(v) = css_set.or(li_value) {
            scan.set(v, incremented);
        }
        // ::before is document-order-first inside this element (index 1;
        // the pseudos array really is [after, before]).
        self.scan_pseudo_decls(node_id, 1, name, scan);
    }

    // Scan a sibling run: any reset of the name (CSS counter-reset or a
    // nested list's UA-sheet scope) shadows the rest of the run.
    fn scan_children(
        &self,
        children: Vec<NodeId>,
        name: &Atom,
        is_list_item: bool,
        scan: &mut ReversedScan,
        budget: &mut PrescanBudget,
    ) {
        for child in children {
            if self.resets_name(child, name, is_list_item) {
                break;
            }
            self.scan_reversed(child, name, is_list_item, scan, budget);
        }
    }

    fn scan_reversed(
        &self,
        node_id: NodeId,
        name: &Atom,
        is_list_item: bool,
        scan: &mut ReversedScan,
        budget: &mut PrescanBudget,
    ) {
        if scan.frozen {
            return;
        }
        // Out of budget: freeze this scan where it is (see PrescanBudget).
        if budget.0 == 0 {
            scan.frozen = true;
            return;
        }
        budget.0 -= 1;
        let node = &self.nodes[node_id];
        if node.element_data().is_none() {
            return;
        }
        let Some(styles) = node.primary_styles() else { return };
        if styles.clone_display().is_none() {
            return;
        }
        self.scan_element_decls(node_id, name, is_list_item, scan, true);
        self.scan_children(self.nodes[node_id].flat_children().to_vec(), name, is_list_item, scan, budget);
        self.scan_pseudo_decls(node_id, 0, name, scan);
    }

    /// Resolve counters for the whole document. Runs after styling (needs
    /// computed counter-* and pseudo styles), before box construction.
    pub(crate) fn resolve_counters(&mut self) {
        // Cheap flat gate: counter-free documents (no list items, no
        // counter-* declarations, no non-trivial pseudo content) skip
        // the recursive pass entirely: it costs HashMap traffic and
        // string building per element, this is slice-emptiness checks.

        // The positive result is sticky: once a document uses counters it
        // will keep using them (a nav <ul> doesn't come and go), so
        // counter-using documents pay the gate scan exactly once and
        // counter-free ones trade one cheap flat scan per resolve for the
        // full recursive pass. Deeper style-system integration (dirty
        // bits at declaration time) is the eventual O(1) answer.
        if !self.counters_seen {
            if !self.document_uses_counters() {
                if !self.counters.list_item_values.is_empty()
                    || !self.counters.before_text.is_empty()
                    || !self.counters.after_text.is_empty()
                {
                    self.counters = CounterOutput::default();
                }
                return;
            }
            self.counters_seen = true;
        }
        // The output is a pure function of tree shape and computed styles,
        // so a resolve that damaged nothing cannot have changed it. A page
        // that trips counters_seen once would otherwise rebuild the whole
        // thing every resolve, damaged or not.
        if self.counters_built_once && !self.any_node_construct_damaged() {
            return;
        }
        self.counters_built_once = true;

        let mut out = CounterOutput::default();
        let mut state = CounterState::default();
        let root = self.root_element().id;
        self.resolve_counters_for(root, 0, &mut state, &mut out);
        self.damage_moved_counters(&out);
        self.counters = out;
    }

    /// Stamp construction damage where a counter moved since the last pass.
    ///
    /// These side tables are only read during box construction, which
    /// incremental layout runs for damaged nodes alone. A counter value
    /// changes the text on a node whose own cascade did not change (a new
    /// sibling renumbers the ones after it), so without this the gate stays
    /// shut and the pseudo/marker keeps rendering the previous pass's
    /// string. Only a real mismatch stamps, so a steady document pays
    /// two map lookups per entry and nothing else.
    fn damage_moved_counters(&mut self, new: &CounterOutput) {
        let mut dirty: Vec<NodeId> = Vec::new();
        let old = &self.counters;

        // ::before/::after text reaches the pseudo via flush_pseudo_elements,
        // which runs inside the owning element's box construction.
        for (fresh, stale) in [
            (&new.before_text, &old.before_text),
            (&new.after_text, &old.after_text),
        ] {
            dirty.extend(
                fresh
                    .iter()
                    .filter(|(id, text)| stale.get(id) != Some(*text))
                    .map(|(&id, _)| id),
            );
            dirty.extend(stale.keys().filter(|id| !fresh.contains_key(id)).copied());
        }

        // A marker ordinal is written by collect_list_item_children, which
        // walks from the list item's parent: damage the child and it never runs.
        let ordinals = new
            .list_item_values
            .iter()
            .filter(|(id, value)| old.list_item_values.get(id) != Some(*value))
            .map(|(&id, _)| id)
            .chain(
                old.list_item_values
                    .keys()
                    .filter(|id| !new.list_item_values.contains_key(id))
                    .copied(),
            );
        for id in ordinals {
            dirty.extend(self.nodes.get(id).and_then(|node| node.parent));
        }

        dirty.retain(|&id| self.nodes.get(id).is_some());
        for &id in &dirty {
            self.nodes[id].insert_damage(CONSTRUCT_BOX);
        }
        // This damage lands after the summary was taken, so hand it over
        // explicitly instead of leaving the walks blind to it.
        self.note_late_layout_damage(&dirty);
    }

    /// Flat slab scan, no recursion: cheaper than the pass it guards.
    ///
    /// Counter output depends on tree shape and the counter-affecting
    /// styles, and every one of those stamps CONSTRUCT damage: structural
    /// mutations stamp ALL_DAMAGE, and display / list-style / content /
    /// counter-* diffs rebuild the box tree (compute_layout_damage).
    /// Plain RELAYOUT damage (a colour or background write) cannot move
    /// a counter, so it must not drag the whole document through a rebuild.
    fn any_node_construct_damaged(&self) -> bool {
        self.any_construct_damage
    }

    fn document_uses_counters(&self) -> bool {
        for (_, node) in self.tree().iter() {
            let Some(styles) = node.primary_styles() else {
                continue;
            };
            if styles.clone_display().is_list_item() {
                return true;
            }
            let cs = styles.get_counters();
            if !cs.counter_reset.is_empty()
                || !cs.counter_increment.is_empty()
                || !cs.counter_set.is_empty()
            {
                return true;
            }
            // Pseudo-elements count when their content needs resolution
            // (anything beyond one literal string) OR they carry counter
            // declarations of their own (those apply even under trivial
            // content, so the pass must run for them).
            let style_data = node.try_stylo_element_data().and_then(|s| s.get());
            let has_counter_pseudo = style_data
                .map(|d| {
                    d.styles.pseudos.as_array().iter().flatten().any(|p| {
                        let pc = p.get_counters();
                        if !pc.counter_reset.is_empty()
                            || !pc.counter_increment.is_empty()
                            || !pc.counter_set.is_empty()
                        {
                            return true;
                        }
                        match &pc.content {
                            Content::Items(item_data) => {
                                let items = &item_data.items[0..item_data.alt_start];
                                !(items.len() == 1
                                    && matches!(items[0], ContentItem::String(_)))
                            }
                            _ => false,
                        }
                    })
                })
                .unwrap_or(false);
            if has_counter_pseudo {
                return true;
            }
        }
        false
    }

    fn resolve_counters_for(
        &self,
        node_id: NodeId,
        depth: usize,
        state: &mut CounterState,
        out: &mut CounterOutput,
    ) {
        let node = &self.nodes[node_id];
        let Some(el) = node.element_data() else { return };
        let Some(styles) = node.primary_styles() else { return };
        let display = styles.clone_display();
        if display.is_none() {
            return;
        }

        let list_item_name = Atom::from("list-item");
        let tag = &el.name.local;

        // css-lists processing order within one element: counters are
        // (1) instantiated, (2) incremented, (3) set.
        let counters_style = styles.get_counters();
        for pair in counters_style.counter_reset.iter() {
            let value = if pair.is_reversed && pair.value == i32::MIN {
                self.reversed_auto_start(
                    node_id,
                    &pair.name.0,
                    pair.name.0 == list_item_name,
                    false,
                    &mut state.prescan_budget,
                )
            } else {
                pair.value
            };
            state.create(&pair.name.0, value, depth, pair.is_reversed);
        }
        // HTML-implied list-item scopes: <ol> honours start/reversed. This
        // runs after the CSS resets so it overrides the UA stylesheet's
        // plain `ol { counter-reset: list-item }` (which would otherwise
        // clobber the reversed()/start semantics the attributes carry).

        // An author reset (reversed() or a non-zero value) must win over
        // the attributes, so those skip the HTML-implied branch entirely.
        let author_list_item_reset = counters_style
            .counter_reset
            .iter()
            .any(|p| p.name.0 == list_item_name && (p.is_reversed || p.value != 0));
        if !author_list_item_reset
            && matches!(*tag, local_name!("ol") | local_name!("ul") | local_name!("menu") | local_name!("dir"))
        {
            let (start, reversed) = if *tag == local_name!("ol") {
                (
                    el.attr_parsed::<i32>(local_name!("start")),
                    el.has_attr(local_name!("reversed")),
                )
            } else {
                (None, false)
            };
            let value = match (reversed, start) {
                (false, s) => s.unwrap_or(1) - 1,
                // Reversed with explicit start: the pre-decrement lands the
                // first item exactly on `start`.
                (true, Some(s)) => s.saturating_add(1),
                // Auto start: chosen so the last increment lands on its own
                // magnitude (css-lists §instantiating-counters).
                (true, None) => {
                    self.reversed_auto_start(node_id, &list_item_name, true, false, &mut state.prescan_budget)
                }
            };
            state.create(&list_item_name, value, depth, reversed);
        }

        let mut incremented_list_item = false;
        for pair in counters_style.counter_increment.iter() {
            state.increment(&pair.name.0, pair.value, depth);
            if pair.name.0 == list_item_name {
                incremented_list_item = true;
            }
        }
        // The implicit list-item increment applies even on li[value] items
        // (the set lands afterwards) unless counter-increment mentions it.
        if display.is_list_item() && !incremented_list_item {
            let rev = state
                .innermost(&list_item_name)
                .map(|s| s.reversed)
                .unwrap_or(false);
            state.increment(&list_item_name, if rev { -1 } else { 1 }, depth);
        }
        // The li value attribute is a presentational hint: it lands before
        // CSS counter-set, which therefore wins when both are present.
        if display.is_list_item() && *tag == local_name!("li") {
            if let Some(v) = el.attr_parsed::<i32>(local_name!("value")) {
                state.set(&list_item_name, v, depth);
            }
        }
        for pair in counters_style.counter_set.iter() {
            state.set(&pair.name.0, pair.value, depth);
        }
        if display.is_list_item() {
            out.list_item_values
                .insert(node_id, state.value(&list_item_name));
        }

        // Pseudo-element styles: note the array really is [after, before]
        // (see flush_pseudo_elements); resolve ::before now, ::after later.
        let style_data = node.try_stylo_element_data().and_then(|s| s.get());
        let pseudos = style_data
            .map(|d| d.styles.pseudos.as_array().clone());
        if let Some(before) = pseudos.as_ref().and_then(|p| p[1].as_ref()) {
            if let Some(text) = self.resolve_content(before, state, depth) {
                out.before_text.insert(node_id, text);
            }
        }

        // Flat-tree children: slot assignment and shadow roots reorder
        // content, and counters follow the rendered (flat) order.
        for &child in self.nodes[node_id].flat_children() {
            self.resolve_counters_for(child, depth + 1, state, out);
        }

        // ::after sees scopes created by the element's children (it is
        // their following sibling), so resolve before popping them.
        if let Some(after) = pseudos.as_ref().and_then(|p| p[0].as_ref()) {
            if let Some(text) = self.resolve_content(after, state, depth) {
                out.after_text.insert(node_id, text);
            }
        }
        state.pop_deeper_than(depth);
    }

    /// Stringify a pseudo's `content` when it needs counter/attr resolution.
    /// Plain single-string content returns None (existing path handles it).
    fn resolve_content(
        &self,
        pseudo_style: &style::properties::ComputedValues,
        state: &mut CounterState,
        depth: usize,
    ) -> Option<String> {
        let Content::Items(item_data) = &pseudo_style.get_counters().content else {
            return None;
        };
        // A content-bearing pseudo generates a box, so its own counter
        // properties apply even when the content is a plain literal
        // (`::before { content: '•'; counter-increment: foo }` must still
        // increment). Element order again: reset → increment → set.
        let pc = pseudo_style.get_counters();
        for pair in pc.counter_reset.iter() {
            state.create(&pair.name.0, pair.value, depth + 1, pair.is_reversed);
        }
        for pair in pc.counter_increment.iter() {
            state.increment(&pair.name.0, pair.value, depth + 1);
        }
        for pair in pc.counter_set.iter() {
            state.set(&pair.name.0, pair.value, depth + 1);
        }

        let items = &item_data.items[0..item_data.alt_start];
        let trivial = items.len() == 1 && matches!(items[0], ContentItem::String(_));
        if trivial {
            return None;
        }

        let mut text = String::new();
        for item in items {
            match item {
                ContentItem::String(s) => text.push_str(s),
                ContentItem::Counter(name, style) => {
                    text.push_str(&format_counter(state.value(&name.0), style));
                }
                ContentItem::Counters(name, sep, style) => {
                    let vals = state.all_values(&name.0);
                    if vals.is_empty() {
                        text.push_str(&format_counter(0, style));
                    } else {
                        let parts: Vec<String> =
                            vals.iter().map(|&v| format_counter(v, style)).collect();
                        text.push_str(&parts.join(sep));
                    }
                }
                // `attr()` reaches here as a String: stylo substitutes it at
                // computed-value time.
                // Quotes and images are out of scope for this pass.
                _ => {}
            }
        }
        Some(text)
    }
}

#[cfg(test)]
mod tests {
    use super::format_counter;
    use style::{
        counter_style::{CounterStyle, Symbol, Symbols, SymbolsType},
        ArcSlice,
    };

    #[test]
    fn symbols_format_i32_min_without_overflow() {
        let style = CounterStyle::Symbols {
            ty: SymbolsType::Cyclic,
            symbols: Symbols(ArcSlice::from_iter(
                vec![
                    Symbol::String(String::from("first").into()),
                    Symbol::String(String::from("second").into()),
                    Symbol::String(String::from("third").into()),
                ]
                .into_iter(),
            )),
        };

        assert_eq!(format_counter(i32::MIN, &style), "first");
    }
}
