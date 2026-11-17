use crate::{Finalize, Gc, GcRefCell, Trace, WeakGc, WeakMap, census, force_collect};

#[derive(Trace, Finalize)]
struct Node {
    child: GcRefCell<Option<Gc<Node>>>,
}

#[test]
fn census_discounts_only_the_listed_handles_and_records_edges() {
    let child = Gc::new(Node {
        child: GcRefCell::new(None),
    });
    let root = Gc::new(Node {
        child: GcRefCell::new(Some(child.clone())),
    });
    let root_key = root.census_key();
    let child_key = child.census_key();
    let held = root.clone();
    // SAFETY: all handles remain live, and the callback only inspects keys.
    unsafe {
        census(&[root_key, child_key], |c| {
            assert!(c.is_marked(root_key));
            assert!(c.is_marked(child_key));
        });
    }
    drop(held);
    // SAFETY: root and child remain live; no GC handle changes in the callback.
    unsafe {
        census(&[root_key, child_key], |c| {
            assert!(!c.is_marked(root_key));
            assert!(!c.is_marked(child_key));
            assert_eq!(c.children(root_key), vec![child_key]);
        });
    }
    force_collect();
    assert!(root.child.borrow().is_some());
}

#[test]
fn census_preserves_script_weak_refs_but_not_external_identity_caches() {
    let root = Gc::new(Node {
        child: GcRefCell::new(None),
    });
    let cache = WeakGc::new(&root);
    let key = root.census_key();
    // SAFETY: root remains live; the callback only reads marks.
    unsafe {
        census(&[key], |c| assert!(!c.is_marked(key)));
    }

    let script_weak_ref = Gc::new(WeakGc::new(&root));
    // SAFETY: root and the embedded weak handle remain live and unchanged.
    unsafe {
        census(&[key], |c| assert!(c.is_marked(key)));
    }
    force_collect();
    assert!(script_weak_ref.upgrade().is_some());
    assert!(cache.upgrade().is_some());

    drop(root);
    force_collect();
    assert!(script_weak_ref.upgrade().is_none());
    assert!(cache.upgrade().is_none());
}

#[test]
fn census_keeps_weak_map_values_only_while_their_key_is_reached() {
    let key = Gc::new(String::from("key"));
    let value = Gc::new(String::from("value"));
    let watch_value = WeakGc::new(&value);
    let mut map = WeakMap::new();
    map.insert(&key, value.clone());
    let key_addr = key.census_key();
    let value_addr = value.census_key();
    // SAFETY: key, value, and map remain live; no heap changes in the callback.
    unsafe {
        census(&[key_addr, value_addr], |c| {
            assert!(!c.is_marked(key_addr));
            assert!(!c.is_marked(value_addr));
        });
    }
    // A script WeakRef can recover the key, and through it the value.
    let script_weak_ref = Gc::new(WeakGc::new(&key));
    // SAFETY: as above, plus the WeakRef box stays live and unchanged.
    unsafe {
        census(&[key_addr, value_addr], |c| {
            assert!(c.is_marked(key_addr));
            assert!(c.is_marked(value_addr));
        });
    }
    drop(script_weak_ref);
    let held_key = key.clone();
    // SAFETY: as above.
    unsafe {
        census(&[key_addr, value_addr], |c| {
            assert!(c.is_marked(key_addr));
            assert!(c.is_marked(value_addr));
        });
    }
    drop(held_key);
    force_collect();
    assert!(map.contains_key(&key));
    assert!(watch_value.upgrade().is_some());

    drop(value);
    drop(key);
    force_collect();
    assert!(watch_value.upgrade().is_none());
}

#[test]
fn census_reports_a_reached_weak_maps_value_as_an_edge_of_its_gray_key() {
    let key = Gc::new(Node {
        child: GcRefCell::new(None),
    });
    let value = Gc::new(Node {
        child: GcRefCell::new(None),
    });
    let mut map = WeakMap::new();
    map.insert(&key, value.clone());
    let key_addr = key.census_key();
    let value_addr = value.census_key();
    // SAFETY: key, value, and map remain live; no heap changes in the callback.
    unsafe {
        census(&[key_addr, value_addr], |c| {
            assert!(!c.is_marked(key_addr));
            assert!(!c.is_marked(value_addr));
            // Script reaches the value through `map.get(key)` whenever it
            // reaches the key, so the gray walk must see that edge.
            assert_eq!(c.children(key_addr), vec![value_addr]);
            assert!(c.children(value_addr).is_empty());
        });
    }
    drop(map);
    // SAFETY: as above; the map itself is gone, so is the edge.
    unsafe {
        census(&[key_addr, value_addr], |c| {
            assert!(c.children(key_addr).is_empty());
        });
    }
}

#[test]
fn census_unwind_restores_marks_and_root_counts() {
    let root = Gc::new(Node {
        child: GcRefCell::new(None),
    });
    let weak = WeakGc::new(&root);
    let key = root.census_key();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // SAFETY: root stays live and the callback does not mutate the heap.
        unsafe { census(&[key], |_| panic!("census callback")) }
    }));
    assert!(result.is_err());
    force_collect();
    assert!(weak.upgrade().is_some());
    drop(root);
    force_collect();
    assert!(weak.upgrade().is_none());
}
