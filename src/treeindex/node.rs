use super::leaf::{LeafScanner, ARRAY_SIZE};
use super::Leaf;
use crossbeam_epoch::{Atomic, Guard, Owned, Shared};
use std::cmp::Ordering;
use std::mem::MaybeUninit;
use std::sync::atomic::Ordering::{Acquire, Relaxed, Release};

pub enum Error<K: Clone + Ord + Send + Sync, V: Clone + Send + Sync> {
    /// Duplicated key found: returns the given key-value pair.
    Duplicated((K, V)),
    /// Full: returns a newly allocated node for the parent to consume
    Full((K, V), Option<K>),
    /// Retry: return the given key-value pair.
    Retry((K, V)),
}

enum NodeType<K: Clone + Ord + Send + Sync, V: Clone + Send + Sync> {
    /// InternalNode: |ptr(children)/max(child keys)|...|ptr(children)|
    InternalNode {
        bounded_children: Leaf<K, Atomic<Node<K, V>>>,
        unbounded_child: Atomic<Node<K, V>>,
        reserved_low_key: Atomic<(K, Node<K, V>)>,
        reserved_high_key: Atomic<(K, Node<K, V>)>,
    },
    /// LeafNode: |ptr(entry array)/max(child keys)|...|ptr(entry array)|
    LeafNode {
        bounded_children: Leaf<K, Atomic<Leaf<K, V>>>,
        unbounded_child: Atomic<Leaf<K, V>>,
        reserved_low_key: Atomic<Leaf<K, V>>,
        reserved_high_key: Atomic<Leaf<K, V>>,
    },
}

pub struct Node<K: Clone + Ord + Send + Sync, V: Clone + Send + Sync> {
    entry: NodeType<K, V>,
    side_link: Atomic<Node<K, V>>,
    floor: usize,
}

impl<K: Clone + Ord + Send + Sync, V: Clone + Send + Sync> Node<K, V> {
    pub fn new(floor: usize) -> Node<K, V> {
        Node {
            entry: if floor > 0 {
                NodeType::InternalNode {
                    bounded_children: Leaf::new(),
                    unbounded_child: Atomic::null(),
                    reserved_low_key: Atomic::null(),
                    reserved_high_key: Atomic::null(),
                }
            } else {
                NodeType::LeafNode {
                    bounded_children: Leaf::new(),
                    unbounded_child: Atomic::null(),
                    reserved_low_key: Atomic::null(),
                    reserved_high_key: Atomic::null(),
                }
            },
            side_link: Atomic::null(),
            floor,
        }
    }

    pub fn search<'a>(&'a self, key: &K, guard: &'a Guard) -> Option<LeafNodeScanner<'a, K, V>> {
        match &self.entry {
            NodeType::InternalNode {
                bounded_children,
                unbounded_child,
                reserved_low_key,
                reserved_high_key,
            } => {
                if let Some((_, child)) = bounded_children.min_ge(&key) {
                    unsafe { child.load(Acquire, guard).deref().search(key, guard) }
                } else {
                    let current_tail_node = unbounded_child.load(Relaxed, guard);
                    if current_tail_node.is_null() {
                        // non-leaf node: invalid
                        return None;
                    }
                    unsafe { current_tail_node.deref().search(key, guard) }
                }
            }
            NodeType::LeafNode {
                bounded_children,
                unbounded_child,
                reserved_low_key,
                reserved_high_key,
            } => {
                if let Some((_, child)) = bounded_children.min_ge(&key) {
                    let leaf_node_scanner = LeafNodeScanner::from(
                        key,
                        self,
                        unsafe { child.load(Acquire, guard).deref() },
                        guard,
                    );
                    if leaf_node_scanner.get().is_some() {
                        Some(leaf_node_scanner)
                    } else {
                        None
                    }
                } else {
                    let current_tail_node = unbounded_child.load(Relaxed, guard);
                    if current_tail_node.is_null() {
                        return None;
                    }
                    let leaf_node_scanner = LeafNodeScanner::from(
                        key,
                        self,
                        unsafe { current_tail_node.deref() },
                        guard,
                    );
                    if leaf_node_scanner.get().is_some() {
                        Some(leaf_node_scanner)
                    } else {
                        None
                    }
                }
            }
        }
    }

    /// Inserts a key-value pair.
    ///
    /// It is a recursive call, and therefore stack-overflow may occur.
    /// B+ tree assures that the tree is filled up from the very bottom nodes.
    pub fn insert(&self, key: K, value: V, guard: &Guard) -> Result<(), Error<K, V>> {
        match &self.entry {
            NodeType::InternalNode {
                bounded_children,
                unbounded_child,
                reserved_low_key,
                reserved_high_key,
            } => {
                loop {
                    if let Some((_, child)) = bounded_children.min_ge(&key) {
                        let child_node = child.load(Acquire, guard);
                        let result = unsafe { child_node.deref().insert(key, value, guard) };
                        return self.handle_result(result, child_node, guard);
                    } else if !bounded_children.full() {
                        if let Some(result) = bounded_children.insert(
                            key.clone(),
                            Atomic::new(Node::new(self.floor - 1)),
                            false,
                        ) {
                            drop(unsafe { (result.0).1.into_owned() });
                        }
                    } else {
                        break;
                    }
                }
                let mut current_tail_node = unbounded_child.load(Relaxed, guard);
                if current_tail_node.is_null() {
                    match unbounded_child.compare_and_set(
                        current_tail_node,
                        Owned::new(Node::new(self.floor - 1)),
                        Relaxed,
                        guard,
                    ) {
                        Ok(result) => current_tail_node = result,
                        Err(result) => current_tail_node = result.current,
                    }
                }
                let result = unsafe { current_tail_node.deref().insert(key, value, guard) };
                self.handle_result(result, current_tail_node, guard)
            }
            NodeType::LeafNode {
                bounded_children,
                unbounded_child,
                reserved_low_key,
                reserved_high_key,
            } => {
                loop {
                    if let Some((max_key, child)) = bounded_children.min_ge(&key) {
                        let child_node = child.load(Acquire, guard);
                        return unsafe { child_node.deref().insert(key, value, false) }
                            .map_or_else(
                                || Ok(()),
                                |result| {
                                    if result.1 {
                                        Err(Error::Duplicated(result.0))
                                    } else {
                                        self.split_leaf(
                                            result.0,
                                            &bounded_children,
                                            &child,
                                            Some(max_key.clone()),
                                            &reserved_low_key,
                                            &reserved_high_key,
                                            guard,
                                        )
                                    }
                                },
                            );
                    } else if !bounded_children.full() {
                        if let Some(result) =
                            bounded_children.insert(key.clone(), Atomic::new(Leaf::new()), false)
                        {
                            drop(unsafe { (result.0).1.into_owned() });
                        }
                    } else {
                        break;
                    }
                }
                let mut current_tail_node = unbounded_child.load(Relaxed, guard);
                if current_tail_node.is_null() {
                    match unbounded_child.compare_and_set(
                        current_tail_node,
                        Owned::new(Leaf::new()),
                        Relaxed,
                        guard,
                    ) {
                        Ok(result) => current_tail_node = result,
                        Err(result) => current_tail_node = result.current,
                    }
                }
                return unsafe { current_tail_node.deref().insert(key, value, false) }.map_or_else(
                    || Ok(()),
                    |result| {
                        if result.1 {
                            Err(Error::Duplicated(result.0))
                        } else {
                            self.split_leaf(
                                result.0,
                                &bounded_children,
                                &unbounded_child,
                                None,
                                &reserved_low_key,
                                &reserved_high_key,
                                guard,
                            )
                        }
                    },
                );
            }
        }
    }

    fn split_leaf(
        &self,
        entry: (K, V),
        leaf_array: &Leaf<K, Atomic<Leaf<K, V>>>,
        full_leaf: &Atomic<Leaf<K, V>>,
        full_leaf_max_key: Option<K>,
        low_key: &Atomic<Leaf<K, V>>,
        high_key: &Atomic<Leaf<K, V>>,
        guard: &Guard,
    ) -> Result<(), Error<K, V>> {
        debug_assert!(unsafe { full_leaf.load(Acquire, &guard).deref().full() });
        let new_leaf_low_key = Owned::new(Leaf::new());
        let new_leaf_high_key = Owned::new(Leaf::new());
        let low_key_leaf;
        let high_key_leaf;
        match low_key.compare_and_set(Shared::null(), new_leaf_low_key, Relaxed, guard) {
            Ok(result) => low_key_leaf = result,
            Err(_) => return Err(Error::Retry(entry)),
        }
        match high_key.compare_and_set(Shared::null(), new_leaf_high_key, Relaxed, guard) {
            Ok(result) => high_key_leaf = result,
            Err(_) => {
                drop(unsafe { low_key.swap(Shared::null(), Relaxed, guard).into_owned() });
                return Err(Error::Retry(entry));
            }
        }

        // copy entries to the newly allocated leaves
        let distributed = unsafe {
            full_leaf
                .load(Acquire, &guard)
                .deref()
                .distribute(low_key_leaf.deref(), high_key_leaf.deref())
        };

        // insert the given entry
        if distributed.1 == 0 || unsafe { low_key_leaf.deref().min_ge(&entry.0).is_some() } {
            // insert the entry into the low-key leaf if the high-key leaf is empty, or the key fits the low-key leaf
            unsafe {
                low_key_leaf
                    .deref()
                    .insert(entry.0.clone(), entry.1.clone(), false)
            };
        } else {
            // insert the entry into the high-key leaf
            unsafe {
                high_key_leaf
                    .deref()
                    .insert(entry.0.clone(), entry.1.clone(), false)
            };
        }

        // if the key is for the unbounded child leaf, return
        if full_leaf_max_key.is_none() {
            return Err(Error::Full(entry, None));
        }

        // insert the newly added leaf into the main array
        if distributed.1 == 0 {
            // replace the full leaf with the low-key leaf
            let old_full_leaf = full_leaf.swap(low_key_leaf, Release, &guard);
            // deallocate the deprecated leaf
            unsafe {
                guard.defer_destroy(old_full_leaf);
            };
            // everything's done
            let unused_high_key_leaf = high_key.swap(Shared::null(), Release, guard);
            drop(unsafe { unused_high_key_leaf.into_owned() });

            // it is practically un-locking the leaf node
            low_key.swap(Shared::null(), Release, guard);

            // OK
            return Ok(());
        } else {
            let max_key = unsafe { low_key_leaf.deref().max_key() }.unwrap();
            if leaf_array
                .insert(max_key.clone(), Atomic::from(low_key_leaf), false)
                .is_some()
            {
                // insertion failed: expect that the caller handles the situation
                return Err(Error::Full(entry, full_leaf_max_key));
            }

            // replace the full leaf with the high-key leaf
            let old_full_leaf = full_leaf.swap(high_key_leaf, Release, &guard);
            // deallocate the deprecated leaf
            unsafe {
                guard.defer_destroy(old_full_leaf);
            };

            // it is practically un-locking the leaf node
            low_key.swap(Shared::null(), Release, guard);

            // OK
            return Ok(());
        }
    }

    fn split_node(
        &self,
        entry: (K, V),
        leaf_array: &Leaf<K, Atomic<Node<K, V>>>,
        full_node: &Atomic<Node<K, V>>,
        full_node_max_key: Option<K>,
        low_key: &Atomic<Node<K, V>>,
        high_key: &Atomic<Node<K, V>>,
        guard: &Guard,
    ) -> Result<(), Error<K, V>> {
        // [TODO]
        let new_node_low_key = Owned::new(Node::new(self.floor - 1));
        let new_node_high_key = Owned::new(Node::new(self.floor - 1));
        let low_key_node;
        let high_key_node;
        match low_key.compare_and_set(Shared::null(), new_node_low_key, Relaxed, guard) {
            Ok(result) => low_key_node = result,
            Err(_) => return Err(Error::Retry(entry)),
        }
        match high_key.compare_and_set(Shared::null(), new_node_high_key, Relaxed, guard) {
            Ok(result) => high_key_node = result,
            Err(_) => {
                drop(unsafe { low_key.swap(Shared::null(), Relaxed, guard).into_owned() });
                return Err(Error::Retry(entry));
            }
        }

        // copy entries to the newly allocated nodes
        let mut distributed: (usize, usize) = (0, 0);
        match unsafe { &full_node.load(Acquire, guard).deref().entry } {
            NodeType::InternalNode {
                bounded_children,
                unbounded_child,
                reserved_low_key,
                reserved_high_key,
            } => {
                // [TODO]
                return Err(Error::Retry(entry));
            }
            NodeType::LeafNode {
                bounded_children,
                unbounded_child,
                reserved_low_key,
                reserved_high_key,
            } => {
                let mut scanner = LeafScanner::new(bounded_children);
                let unbounded_key_node = unbounded_child.load(Acquire, guard);
                let reserved_low_key_node = reserved_low_key.load(Acquire, guard);
                let reserved_high_key_node = reserved_low_key.load(Acquire, guard);
                if let NodeType::LeafNode {
                    bounded_children,
                    unbounded_child: _,
                    reserved_low_key: _,
                    reserved_high_key: _,
                } = unsafe { &low_key_node.deref().entry }
                {
                    while let Some(entry) = scanner.next() {
                        if full_node_max_key
                            .as_ref()
                            .map_or_else(|| false, |key| key.cmp(entry.0) == Ordering::Equal)
                        {
                            if !reserved_low_key_node.is_null() {
                                unsafe {
                                    bounded_children.insert(
                                        reserved_low_key_node.deref().max_key().unwrap().clone(),
                                        Atomic::from(reserved_low_key_node),
                                        false,
                                    )
                                };
                                distributed.0 += 1;
                            }
                            if !reserved_high_key_node.is_null() {
                                unsafe {
                                    bounded_children.insert(
                                        reserved_high_key_node.deref().max_key().unwrap().clone(),
                                        Atomic::from(reserved_high_key_node),
                                        false,
                                    )
                                };
                                distributed.0 += 1;
                            }
                            if distributed.0 > ARRAY_SIZE / 2 {
                                break;
                            } else {
                                continue;
                            }
                        }
                        bounded_children.insert(entry.0.clone(), entry.1.clone(), false);
                        distributed.0 += 1;
                        if distributed.0 > ARRAY_SIZE / 2 {
                            break;
                        }
                    }
                }
                if let NodeType::LeafNode {
                    bounded_children,
                    unbounded_child,
                    reserved_low_key,
                    reserved_high_key,
                } = unsafe { &high_key_node.deref().entry }
                {
                    while let Some(entry) = scanner.next() {
                        if full_node_max_key
                            .as_ref()
                            .map_or_else(|| false, |key| key.cmp(entry.0) == Ordering::Equal)
                        {
                            if !reserved_low_key_node.is_null() {
                                unsafe {
                                    bounded_children.insert(
                                        reserved_low_key_node.deref().max_key().unwrap().clone(),
                                        Atomic::from(reserved_low_key_node),
                                        false,
                                    )
                                };
                                distributed.1 += 1;
                            }
                            if !reserved_high_key_node.is_null() {
                                unsafe {
                                    bounded_children.insert(
                                        reserved_high_key_node.deref().max_key().unwrap().clone(),
                                        Atomic::from(reserved_high_key_node),
                                        false,
                                    )
                                };
                                distributed.1 += 1;
                            }
                            continue;
                        }
                        bounded_children.insert(entry.0.clone(), entry.1.clone(), false);
                        distributed.1 += 1;
                    }
                    if full_node_max_key.is_none() {
                        if !reserved_low_key_node.is_null() {
                            unsafe {
                                bounded_children.insert(
                                    reserved_low_key_node.deref().max_key().unwrap().clone(),
                                    Atomic::from(reserved_low_key_node),
                                    false,
                                )
                            };
                            distributed.1 += 1;
                        }
                        if !reserved_high_key_node.is_null() {
                            unbounded_child.store(reserved_high_key_node, Release);
                            distributed.1 += 1;
                        }
                    }
                }
            }
        }

        if full_node_max_key.is_none() {
            // [TODO]
            return Ok(());
        }

        // insert the newly added leaf into the main array
        if distributed.1 == 0 {
            // replace the full leaf with the low-key leaf
            let old_full_leaf = full_node.swap(low_key_node, Release, &guard);
            // deallocate the deprecated leaf
            unsafe {
                guard.defer_destroy(old_full_leaf);
            };
            // everything's done
            let unused_high_key_leaf = high_key.swap(Shared::null(), Release, guard);
            drop(unsafe { unused_high_key_leaf.into_owned() });

            // it is practically un-locking the leaf node
            low_key.swap(Shared::null(), Release, guard);

            // OK
            return Ok(());
        } else {
            if leaf_array
                .insert(
                    full_node_max_key.as_ref().unwrap().clone(),
                    Atomic::from(low_key_node),
                    false,
                )
                .is_some()
            {
                // insertion failed: expect that the caller handles the situation
                return Err(Error::Full(entry, full_node_max_key));
            }

            // replace the full leaf with the high-key leaf
            let old_full_leaf = full_node.swap(high_key_node, Release, &guard);
            // deallocate the deprecated leaf
            unsafe {
                guard.defer_destroy(old_full_leaf);
            };

            // it is practically un-locking the leaf node
            low_key.swap(Shared::null(), Release, guard);

            // OK
            return Ok(());
        }

        return Err(Error::Retry(entry));
    }

    fn handle_result(
        &self,
        result: Result<(), Error<K, V>>,
        child_node: Shared<Node<K, V>>,
        guard: &Guard,
    ) -> Result<(), Error<K, V>> {
        match result {
            Ok(_) => return Ok(()),
            Err(err) => match err {
                Error::Duplicated(_) => return Err(err),
                Error::Full(_, _) => {
                    // [TODO]
                    // try to split
                    // split the entry into two new entries => insert the new one => replace the old one with the new one
                    // return self.split_and_insert_locked(entry, child);
                    // failure => revert & retry
                    // success => commit (replace the pointers)
                    return Ok(());
                }
                Error::Retry(_) => return Err(err),
            },
        }
    }
}

impl<K: Clone + Ord + Send + Sync, V: Clone + Send + Sync> Clone for Node<K, V> {
    fn clone(&self) -> Self {
        unreachable!();
        Node::new(self.floor)
    }
}

impl<K: Clone + Ord + Send + Sync, V: Clone + Send + Sync> Drop for Node<K, V> {
    fn drop(&mut self) {
        let guard = crossbeam_epoch::pin();
        match &self.entry {
            NodeType::InternalNode {
                bounded_children,
                unbounded_child,
                reserved_low_key,
                reserved_high_key,
            } => {
                let mut scanner = LeafScanner::new(&bounded_children);
                while let Some(entry) = scanner.next() {
                    let child = entry.1.swap(Shared::null(), Acquire, &guard);
                    unsafe { guard.defer_destroy(child) };
                }
                let tail = unbounded_child.load(Acquire, &guard);
                unsafe { guard.defer_destroy(tail) };
                let low_key_child = reserved_low_key.swap(Shared::null(), Relaxed, &guard);
                if !low_key_child.is_null() {
                    drop(unsafe { low_key_child.into_owned() });
                }
                let high_key_child = reserved_high_key.swap(Shared::null(), Relaxed, &guard);
                if !high_key_child.is_null() {
                    drop(unsafe { high_key_child.into_owned() });
                }
            }
            NodeType::LeafNode {
                bounded_children,
                unbounded_child,
                reserved_low_key,
                reserved_high_key,
            } => {
                let mut scanner = LeafScanner::new(&bounded_children);
                while let Some(entry) = scanner.next() {
                    let child = entry.1.swap(Shared::null(), Acquire, &guard);
                    unsafe { guard.defer_destroy(child) };
                }
                let tail = unbounded_child.load(Acquire, &guard);
                unsafe { guard.defer_destroy(tail) };
                let low_key_child = reserved_low_key.swap(Shared::null(), Relaxed, &guard);
                if !low_key_child.is_null() {
                    drop(unsafe { low_key_child.into_owned() });
                }
                let high_key_child = reserved_high_key.swap(Shared::null(), Relaxed, &guard);
                if !high_key_child.is_null() {
                    drop(unsafe { high_key_child.into_owned() });
                }
            }
        }
    }
}

pub struct LeafNodeScanner<'a, K: Clone + Ord + Send + Sync, V: Clone + Send + Sync> {
    leaf_node: &'a Node<K, V>,
    node_scanner: Option<LeafScanner<'a, K, Atomic<Leaf<K, V>>>>,
    leaf_scanner: Option<LeafScanner<'a, K, V>>,
    guard: &'a Guard,
}

impl<'a, K: Clone + Ord + Send + Sync, V: Clone + Send + Sync> LeafNodeScanner<'a, K, V> {
    fn new(leaf_node: &'a Node<K, V>, guard: &'a Guard) -> LeafNodeScanner<'a, K, V> {
        LeafNodeScanner::<'a, K, V> {
            leaf_node,
            node_scanner: None,
            leaf_scanner: None,
            guard,
        }
    }

    fn from(
        key: &K,
        leaf_node: &'a Node<K, V>,
        leaf: &'a Leaf<K, V>,
        guard: &'a Guard,
    ) -> LeafNodeScanner<'a, K, V> {
        LeafNodeScanner::<'a, K, V> {
            leaf_node,
            node_scanner: None,
            leaf_scanner: Some(LeafScanner::from(key, leaf)),
            guard,
        }
    }

    fn from_ge(key: &K, leaf_node: &'a Node<K, V>, guard: &'a Guard) -> LeafNodeScanner<'a, K, V> {
        // TODO
        LeafNodeScanner::<'a, K, V> {
            leaf_node,
            node_scanner: None,
            leaf_scanner: None,
            guard,
        }
    }

    /// Returns a reference to the entry that the scanner is currently pointing to
    pub fn get(&self) -> Option<(&'a K, &'a V)> {
        if let Some(leaf_scanner) = self.leaf_scanner.as_ref() {
            return leaf_scanner.get();
        }
        None
    }
}

impl<'a, K: Clone + Ord + Send + Sync, V: Clone + Send + Sync> Iterator
    for LeafNodeScanner<'a, K, V>
{
    type Item = (&'a K, &'a V);
    fn next(&mut self) -> Option<Self::Item> {
        if let Some(leaf_scanner) = self.leaf_scanner.as_mut() {
            // leaf iteration
            if let Some(entry) = leaf_scanner.next() {
                return Some(entry);
            }
            // end of iteration
            if self.node_scanner.is_none() {
                return None;
            }
        }

        if self.node_scanner.is_none() && self.leaf_scanner.is_none() {
            // start scanning
            match &self.leaf_node.entry {
                NodeType::InternalNode {
                    bounded_children: _,
                    unbounded_child: _,
                    reserved_low_key: _,
                    reserved_high_key: _,
                } => return None,
                NodeType::LeafNode {
                    bounded_children,
                    unbounded_child: _,
                    reserved_low_key: _,
                    reserved_high_key: _,
                } => {
                    self.node_scanner
                        .replace(LeafScanner::new(bounded_children));
                }
            }
        }

        if let Some(node_scanner) = self.node_scanner.as_mut() {
            // proceed to the next leaf
            while let Some(leaf) = node_scanner.next() {
                self.leaf_scanner.replace(LeafScanner::new(unsafe {
                    leaf.1.load(Acquire, self.guard).deref()
                }));
                if let Some(leaf_scanner) = self.leaf_scanner.as_mut() {
                    // leaf iteration
                    if let Some(entry) = leaf_scanner.next() {
                        return Some(entry);
                    }
                }
                self.leaf_scanner.take();
            }
        }
        self.node_scanner.take();

        let unbounded_child = match &self.leaf_node.entry {
            NodeType::InternalNode {
                bounded_children: _,
                unbounded_child: _,
                reserved_low_key: _,
                reserved_high_key: _,
            } => Shared::null(),
            NodeType::LeafNode {
                bounded_children: _,
                unbounded_child,
                reserved_low_key: _,
                reserved_high_key: _,
            } => unbounded_child.load(Acquire, self.guard),
        };
        if !unbounded_child.is_null() {
            self.leaf_scanner
                .replace(LeafScanner::new(unsafe { unbounded_child.deref() }));
            if let Some(leaf_scanner) = self.leaf_scanner.as_mut() {
                // leaf iteration
                if let Some(entry) = leaf_scanner.next() {
                    return Some(entry);
                }
            }
        }

        // end of iteration
        None
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use std::sync::{Arc, Barrier};
    use std::thread;

    #[test]
    fn leaf_node() {
        let guard = crossbeam_epoch::pin();
        // sequential
        let node = Node::new(0);
        for i in 0..ARRAY_SIZE {
            for j in 0..(ARRAY_SIZE + 1) {
                assert!(node
                    .insert((j + 1) * (ARRAY_SIZE + 1) - i, 10, &guard)
                    .is_ok());
                match node.insert((j + 1) * (ARRAY_SIZE + 1) - i, 11, &guard) {
                    Ok(_) => assert!(false),
                    Err(result) => match result {
                        Error::Duplicated(entry) => {
                            assert_eq!(entry, ((j + 1) * (ARRAY_SIZE + 1) - i, 11))
                        }
                        Error::Full(_, _) => assert!(false),
                        Error::Retry(_) => assert!(false),
                    },
                }
            }
        }
        match node.insert(0, 11, &guard) {
            Ok(_) => assert!(false),
            Err(result) => match result {
                Error::Duplicated(_) => assert!(false),
                Error::Full(entry, _) => assert_eq!(entry, (0, 11)),
                Error::Retry(_) => assert!(false),
            },
        }
        match node.insert(240, 11, &guard) {
            Ok(_) => assert!(false),
            Err(result) => match result {
                Error::Duplicated(_) => assert!(false),
                Error::Full(_, _) => assert!(false),
                Error::Retry(entry) => assert_eq!(entry, (240, 11)),
            },
        }
        // induce split
        let node = Node::new(0);
        for i in 0..ARRAY_SIZE {
            for j in 0..ARRAY_SIZE {
                if j == ARRAY_SIZE / 2 {
                    continue;
                }
                assert!(node
                    .insert((j + 1) * (ARRAY_SIZE + 1) - i, 10, &guard)
                    .is_ok());
                match node.insert((j + 1) * (ARRAY_SIZE + 1) - i, 11, &guard) {
                    Ok(_) => assert!(false),
                    Err(result) => match result {
                        Error::Duplicated(entry) => {
                            assert_eq!(entry, ((j + 1) * (ARRAY_SIZE + 1) - i, 11))
                        }
                        Error::Full(_, _) => assert!(false),
                        Error::Retry(_) => assert!(false),
                    },
                }
            }
        }
        for i in 0..(ARRAY_SIZE / 2) {
            assert!(node
                .insert((ARRAY_SIZE / 2 + 1) * (ARRAY_SIZE + 1) - i, 10, &guard)
                .is_ok());
            match node.insert((ARRAY_SIZE / 2 + 1) * (ARRAY_SIZE + 1) - i, 11, &guard) {
                Ok(_) => assert!(false),
                Err(result) => match result {
                    Error::Duplicated(entry) => {
                        assert_eq!(entry, ((ARRAY_SIZE / 2 + 1) * (ARRAY_SIZE + 1) - i, 11))
                    }
                    Error::Full(_, _) => assert!(false),
                    Error::Retry(_) => assert!(false),
                },
            }
        }
        for i in 0..ARRAY_SIZE {
            assert!(node
                .insert((ARRAY_SIZE + 2) * (ARRAY_SIZE + 1) - i, 10, &guard)
                .is_ok());
            match node.insert((ARRAY_SIZE + 2) * (ARRAY_SIZE + 1) - i, 11, &guard) {
                Ok(_) => assert!(false),
                Err(result) => match result {
                    Error::Duplicated(entry) => {
                        assert_eq!(entry, ((ARRAY_SIZE + 2) * (ARRAY_SIZE + 1) - i, 11))
                    }
                    Error::Full(_, _) => assert!(false),
                    Error::Retry(_) => assert!(false),
                },
            }
        }
        match node.insert(240, 11, &guard) {
            Ok(_) => assert!(false),
            Err(result) => match result {
                Error::Duplicated(_) => assert!(false),
                Error::Full(_, _) => assert!(false),
                Error::Retry(entry) => assert_eq!(entry, (240, 11)),
            },
        }

        let mut scanner = LeafNodeScanner::new(&node, &guard);
        let mut iterated = 0;
        let mut prev = 0;
        while let Some(entry) = scanner.next() {
            assert!(prev < *entry.0);
            assert_eq!(*entry.1, 10);
            prev = *entry.0;
            iterated += 1;
            let searched = node.search(entry.0, &guard);
            assert_eq!(
                searched.map_or_else(
                    || 0,
                    |scanner| scanner.get().map_or_else(|| 0, |entry| *entry.1)
                ),
                10
            )
        }
        assert_eq!(iterated, ARRAY_SIZE * (ARRAY_SIZE + 1) - ARRAY_SIZE / 2);
    }
}
