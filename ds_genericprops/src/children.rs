//! Parent → children index over `parent_id`.
//!
//! `parent_id` lives inside `GenericProps.data`, so the only way to find the
//! children of an object used to be a scan of every prop on the server, cloning
//! each GORC instance to read it. That scan ran on every `update_property`
//! carrying a position: at ~480 updates/s (a truck and its cargo kept awake)
//! against ~850 props it cost ~35 ms per event and pinned every handler thread
//! on preprod, with nobody online.
//!
//! The index is maintained at the two places `parent_id` is written —
//! `GenericProps::new` and `GenericProps::update` — and dropped on delete, so
//! readers get the children of a parent in O(children).

use dashmap::DashMap;
use std::collections::HashSet;
use std::sync::OnceLock;

/// parent uuid → uuids of the objects whose `parent_id` is that parent.
fn children() -> &'static DashMap<String, HashSet<String>> {
    static CHILDREN: OnceLock<DashMap<String, HashSet<String>>> = OnceLock::new();
    CHILDREN.get_or_init(DashMap::new)
}

/// child uuid → its current parent, so a reparent can leave the old set.
fn parent_of() -> &'static DashMap<String, String> {
    static PARENT_OF: OnceLock<DashMap<String, String>> = OnceLock::new();
    PARENT_OF.get_or_init(DashMap::new)
}

/// Record that `child` is now parented to `parent` (`None` = top level).
/// Idempotent; a same-parent call is a no-op.
pub fn set_parent(child: &str, parent: Option<&str>) {
    let parent = parent.filter(|p| !p.is_empty());
    let previous = parent_of().get(child).map(|p| p.clone());
    if previous.as_deref() == parent {
        return;
    }
    if let Some(previous) = previous {
        if let Some(mut set) = children().get_mut(&previous) {
            set.remove(child);
        }
    }
    match parent {
        Some(parent) => {
            children().entry(parent.to_string()).or_default().insert(child.to_string());
            parent_of().insert(child.to_string(), parent.to_string());
        }
        None => {
            parent_of().remove(child);
        }
    }
}

/// Drop `uuid` from the index: it no longer counts as anybody's child, and
/// nothing is parented to it any more.
pub fn forget(uuid: &str) {
    set_parent(uuid, None);
    children().remove(uuid);
}

/// Direct children of `parent`.
pub fn children_of(parent: &str) -> Vec<String> {
    children()
        .get(parent)
        .map(|set| set.iter().cloned().collect())
        .unwrap_or_default()
}

/// Every object under `root`, breadth first, paired with its depth (1 = direct
/// child). Parents come before their children, as a receiver needs them.
/// `max_depth` bounds a cyclic `parent_id` chain.
pub fn descendants_of(root: &str, max_depth: usize) -> Vec<(usize, String)> {
    let mut out: Vec<(usize, String)> = Vec::new();
    let mut seen: HashSet<String> = HashSet::from([root.to_string()]);
    let mut frontier: Vec<String> = vec![root.to_string()];
    let mut depth = 1usize;
    while !frontier.is_empty() && depth <= max_depth {
        let mut next = Vec::new();
        for parent in &frontier {
            for child in children_of(parent) {
                if seen.insert(child.clone()) {
                    out.push((depth, child.clone()));
                    next.push(child);
                }
            }
        }
        frontier = next;
        depth += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn id() -> String {
        Uuid::new_v4().to_string()
    }

    #[test]
    fn set_parent_moves_between_parents() {
        let (a, b, c) = (id(), id(), id());
        set_parent(&c, Some(&a));
        assert_eq!(children_of(&a), vec![c.clone()]);

        set_parent(&c, Some(&b));
        assert!(children_of(&a).is_empty());
        assert_eq!(children_of(&b), vec![c.clone()]);

        set_parent(&c, None);
        assert!(children_of(&b).is_empty());
    }

    #[test]
    fn empty_parent_means_none() {
        let (a, c) = (id(), id());
        set_parent(&c, Some(&a));
        set_parent(&c, Some(""));
        assert!(children_of(&a).is_empty());
    }

    #[test]
    fn forget_drops_both_directions() {
        let (a, b, c) = (id(), id(), id());
        set_parent(&b, Some(&a));
        set_parent(&c, Some(&b));
        forget(&b);
        assert!(children_of(&a).is_empty());
        assert!(children_of(&b).is_empty());
    }

    #[test]
    fn descendants_are_parents_first_and_bounded() {
        let (truck, seat, player, crate_) = (id(), id(), id(), id());
        set_parent(&seat, Some(&truck));
        set_parent(&player, Some(&seat));
        set_parent(&crate_, Some(&truck));

        let found = descendants_of(&truck, 8);
        assert_eq!(found.len(), 3);
        assert_eq!(found[0].0, 1);
        assert_eq!(found[1].0, 1);
        assert_eq!(found[2], (2, player.clone()));

        assert_eq!(descendants_of(&truck, 1).len(), 2);

        // A cycle terminates on max_depth / the seen set.
        set_parent(&truck, Some(&player));
        assert_eq!(descendants_of(&truck, 8).len(), 3);
    }
}
