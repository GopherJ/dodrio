use crate::{
    cached_set::{CacheId, CachedSet},
    change_list::ChangeListBuilder,
    events::EventsRegistry,
    node::{Attribute, ElementNode, Listener, Node, NodeKind, TextNode},
};
use fxhash::FxHashSet;
use std::cmp;

pub(crate) fn diff(
    cached_set: &CachedSet,
    change_list: &mut ChangeListBuilder,
    registry: &mut EventsRegistry,
    old: &Node,
    new: &Node,
    cached_roots: &mut FxHashSet<CacheId>,
) {
    match (&new.kind, &old.kind) {
        (
            &NodeKind::Text(TextNode { text: new_text }),
            &NodeKind::Text(TextNode { text: old_text }),
        ) => {
            debug!("  both are text nodes");
            if new_text != old_text {
                debug!("  text needs updating");
                change_list.set_text(new_text);
            }
        }

        (&NodeKind::Text(_), &NodeKind::Element(_)) => {
            debug!("  replacing a text node with an element");
            create(cached_set, change_list, registry, new, cached_roots);
            registry.remove_subtree(&old);
            change_list.replace_with();
        }

        (&NodeKind::Element(_), &NodeKind::Text(_)) => {
            debug!("  replacing an element with a text node");
            create(cached_set, change_list, registry, new, cached_roots);
            // Note: text nodes cannot have event listeners, so we don't need to
            // remove the old node's listeners from our registry her.
            change_list.replace_with();
        }

        (
            &NodeKind::Element(ElementNode {
                tag_name: new_tag_name,
                listeners: new_listeners,
                attributes: new_attributes,
                children: new_children,
                namespace: new_namespace,
            }),
            &NodeKind::Element(ElementNode {
                tag_name: old_tag_name,
                listeners: old_listeners,
                attributes: old_attributes,
                children: old_children,
                namespace: old_namespace,
            }),
        ) => {
            debug!("  updating an element");
            if new_tag_name != old_tag_name || new_namespace != old_namespace {
                debug!("  different tag names or namespaces; creating new element and replacing old element");
                create(cached_set, change_list, registry, new, cached_roots);
                registry.remove_subtree(&old);
                change_list.replace_with();
                return;
            }
            diff_listeners(change_list, registry, old_listeners, new_listeners);
            diff_attributes(change_list, old_attributes, new_attributes);
            diff_children(
                cached_set,
                change_list,
                registry,
                old_children,
                new_children,
                cached_roots,
            );
        }

        // Both the new and old nodes are cached.
        (&NodeKind::Cached(ref new), &NodeKind::Cached(ref old)) => {
            cached_roots.insert(new.id);

            if new.id == old.id {
                // This is the same cached node, so nothing has changed!
                return;
            }

            let new = cached_set.get(new.id);
            let old = cached_set.get(old.id);
            diff(cached_set, change_list, registry, old, new, cached_roots);
        }

        // New cached node when the old node was not cached. In this scenario,
        // we assume that they are pretty different, and it isn't worth diffing
        // the subtrees, so we just create the new cached node afresh.
        (&NodeKind::Cached(ref c), _) => {
            cached_roots.insert(c.id);
            let new = cached_set.get(c.id);
            create(cached_set, change_list, registry, new, cached_roots);
            registry.remove_subtree(&old);
            change_list.replace_with();
        }

        // Old cached node and new non-cached node. Again, assume that they are
        // probably pretty different and create the new non-cached node afresh.
        (_, &NodeKind::Cached(_)) => {
            create(cached_set, change_list, registry, new, cached_roots);
            registry.remove_subtree(&old);
            change_list.replace_with();
        }
    }
}

fn diff_listeners(
    change_list: &mut ChangeListBuilder,
    registry: &mut EventsRegistry,
    old: &[Listener],
    new: &[Listener],
) {
    debug!("  updating event listeners");

    'outer1: for new_l in new {
        unsafe {
            // Safety relies on removing `new_l` from the registry manually at
            // the end of its lifetime. This happens below in the `'outer2`
            // loop, and elsewhere in diffing when removing old dom trees.
            registry.add(new_l);
        }

        for old_l in old {
            if new_l.event == old_l.event {
                change_list.update_event_listener(new_l);
                continue 'outer1;
            }
        }

        change_list.new_event_listener(new_l);
    }

    'outer2: for old_l in old {
        registry.remove(old_l);

        for new_l in new {
            if new_l.event == old_l.event {
                continue 'outer2;
            }
        }
        change_list.remove_event_listener(old_l.event);
    }
}

fn diff_attributes(change_list: &mut ChangeListBuilder, old: &[Attribute], new: &[Attribute]) {
    debug!("  updating attributes");

    // Do O(n^2) passes to add/update and remove attributes, since
    // there are almost always very few attributes.
    'outer: for new_attr in new {
        if new_attr.is_volatile() {
            change_list.set_attribute(new_attr.name, new_attr.value);
        } else {
            for old_attr in old {
                if old_attr.name == new_attr.name {
                    if old_attr.value != new_attr.value {
                        change_list.set_attribute(new_attr.name, new_attr.value);
                    }
                    continue 'outer;
                }
            }
            change_list.set_attribute(new_attr.name, new_attr.value);
        }
    }

    'outer2: for old_attr in old {
        for new_attr in new {
            if old_attr.name == new_attr.name {
                continue 'outer2;
            }
        }
        change_list.remove_attribute(old_attr.name);
    }
}

fn diff_children(
    cached_set: &CachedSet,
    change_list: &mut ChangeListBuilder,
    registry: &mut EventsRegistry,
    old: &[Node],
    new: &[Node],
    cached_roots: &mut FxHashSet<CacheId>,
) {
    if new.is_empty() {
        if !old.is_empty() {
            remove_children(change_list, registry, old);
        }
        return;
    }

    if old.is_empty() {
        create_children(cached_set, change_list, registry, new, cached_roots);
        return;
    }

    debug!("  updating children shared by old and new");

    let num_children_to_diff = cmp::min(new.len(), old.len());
    let mut new_children = new.iter();
    let mut old_children = old.iter();
    let mut pushed = false;

    for (i, (new_child, old_child)) in new_children
        .by_ref()
        .zip(old_children.by_ref())
        .take(num_children_to_diff)
        .enumerate()
    {
        if i == 0 {
            change_list.push_first_child();
            pushed = true;
        } else {
            debug_assert!(pushed);
            change_list.pop_push_next_sibling();
        }

        diff(
            cached_set,
            change_list,
            registry,
            old_child,
            new_child,
            cached_roots,
        );
    }

    if old_children.next().is_some() {
        debug!("  removing extra old children");
        debug_assert!(new_children.next().is_none());
        if pushed {
            change_list.pop_push_next_sibling();
        } else {
            change_list.push_first_child();
        }
        change_list.remove_self_and_next_siblings();
        pushed = false;
    } else {
        debug!("  creating new children");
        if pushed {
            change_list.pop();
            pushed = false;
        }
        create_children(
            cached_set,
            change_list,
            registry,
            new_children,
            cached_roots,
        );
    }

    debug!("  done updating children");
    if pushed {
        change_list.pop();
    }
}

fn create_children<'a, I>(
    cached_set: &CachedSet,
    change_list: &mut ChangeListBuilder,
    registry: &mut EventsRegistry,
    new: I,
    cached_roots: &mut FxHashSet<CacheId>,
) where
    I: IntoIterator<Item = &'a Node<'a>>,
{
    for child in new {
        create(cached_set, change_list, registry, child, cached_roots);
        change_list.append_child();
    }
}

fn remove_children(
    change_list: &mut ChangeListBuilder,
    registry: &mut EventsRegistry,
    old: &[Node],
) {
    for child in old {
        registry.remove_subtree(child);
    }
    // Fast way to remove all children: set the node's textContent to an empty
    // string.
    change_list.set_text("");
}

fn create(
    cached_set: &CachedSet,
    change_list: &mut ChangeListBuilder,
    registry: &mut EventsRegistry,
    node: &Node,
    cached_roots: &mut FxHashSet<CacheId>,
) {
    match node.kind {
        NodeKind::Text(TextNode { text }) => {
            change_list.create_text_node(text);
        }
        NodeKind::Element(ElementNode {
            tag_name,
            listeners,
            attributes,
            children,
            namespace,
        }) => {
            if let Some(namespace) = namespace {
                change_list.create_element_ns(tag_name, namespace);
            } else {
                change_list.create_element(tag_name);
            }
            for l in listeners {
                unsafe {
                    registry.add(l);
                }
                change_list.new_event_listener(l);
            }
            for attr in attributes {
                change_list.set_attribute(&attr.name, &attr.value);
            }
            for child in children {
                create(cached_set, change_list, registry, child, cached_roots);
                change_list.append_child();
            }
        }
        NodeKind::Cached(ref c) => {
            cached_roots.insert(c.id);
            let node = cached_set.get(c.id);
            create(cached_set, change_list, registry, node, cached_roots)
        }
    }
}
