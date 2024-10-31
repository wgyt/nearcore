//! Structures and logic for updating in-memory trie.
//!
//! DISCLAIMER: This is in process of rewriting to generic structures.
//! See #12324.
//! For now we keep the old types together with new ones to change logic
//! incrementally (for example, `GenericNodeOrIndex` and `OldOrUpdatedNodeId`).
//! New methods will be prefixed with `generic_` to distinguish them from the
//! old ones. When the old methods are removed, the prefix will be dropped.

use super::arena::{ArenaMemory, ArenaMut};
use super::flexible_data::children::ChildrenView;
use super::metrics::MEM_TRIE_NUM_NODES_CREATED_FROM_UPDATES;
use super::node::{InputMemTrieNode, MemTrieNodeId, MemTrieNodeView};
use crate::trie::insert_delete::NodesStorage;
use crate::trie::{
    Children, MemTrieChanges, NodeHandle, StorageHandle, TrieNode, TrieNodeWithSize,
    TrieRefcountDeltaMap, ValueHandle, TRIE_COSTS,
};
use crate::{NibbleSlice, RawTrieNode, RawTrieNodeWithSize, TrieChanges};
use near_primitives::errors::StorageError;
use near_primitives::hash::{hash, CryptoHash};
use near_primitives::state::FlatStateValue;
use std::collections::HashMap;
use std::sync::Arc;

/// For updated nodes, the ID is simply the index into the array of updated nodes we keep.
pub type GenericUpdatedNodeId = usize;

pub type UpdatedMemTrieNodeId = usize;

/// An old node means a node in the current in-memory trie. An updated node means a
/// node we're going to store in the in-memory trie but have not constructed there yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GenericNodeOrIndex<GenericTrieNodePtr> {
    Old(GenericTrieNodePtr),
    Updated(GenericUpdatedNodeId),
}

pub type OldOrUpdatedNodeId = GenericNodeOrIndex<MemTrieNodeId>;

/// Trait for trie values to get their length.
pub trait HasValueLength {
    fn len(&self) -> u64;
}

impl HasValueLength for FlatStateValue {
    fn len(&self) -> u64 {
        self.value_len() as u64
    }
}

impl HasValueLength for ValueHandle {
    fn len(&self) -> u64 {
        match self {
            ValueHandle::HashAndSize(value) => value.length as u64,
            ValueHandle::InMemory(value) => value.1 as u64,
        }
    }
}

/// An updated node - a node that will eventually become a trie node.
/// It references children that are either old or updated nodes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GenericUpdatedTrieNode<GenericTrieNodePtr, GenericValueHandle> {
    /// Used for either an empty root node (indicating an empty trie), or as a temporary
    /// node to ease implementation.
    Empty,
    Leaf {
        extension: Box<[u8]>,
        value: GenericValueHandle,
    },
    Extension {
        extension: Box<[u8]>,
        child: GenericNodeOrIndex<GenericTrieNodePtr>,
    },
    /// Corresponds to either a Branch or BranchWithValue node.
    Branch {
        children: Box<[Option<GenericNodeOrIndex<GenericTrieNodePtr>>; 16]>,
        value: Option<GenericValueHandle>,
    },
}

pub type UpdatedMemTrieNode = GenericUpdatedTrieNode<MemTrieNodeId, FlatStateValue>;

/// An updated node with its memory usage.
/// Needed to recompute subtree function (memory usage) on the fly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenericUpdatedTrieNodeWithSize<GenericTrieNodePtr, GenericValueHandle> {
    pub node: GenericUpdatedTrieNode<GenericTrieNodePtr, GenericValueHandle>,
    pub memory_usage: u64,
}

impl<N, V> GenericUpdatedTrieNodeWithSize<N, V> {
    pub fn empty() -> Self {
        Self { node: GenericUpdatedTrieNode::Empty, memory_usage: 0 }
    }
}

impl<GenericTrieNodePtr, FlatStateValue: HasValueLength>
    GenericUpdatedTrieNode<GenericTrieNodePtr, FlatStateValue>
{
    fn memory_usage_value(value_length: u64) -> u64 {
        value_length * TRIE_COSTS.byte_of_value + TRIE_COSTS.node_cost
    }

    /// Returns the memory usage of the **single** node, in Near's trie cost
    /// terms, not in terms of the physical memory usage.
    /// TODO(#12324): replace `TrieNode::memory_usage_direct_internal` and
    ///`MemTrieNodeView::memory_usage` by this method.
    pub fn memory_usage_direct(&self) -> u64 {
        match self {
            Self::Empty => {
                // DEVNOTE: empty nodes don't exist in storage.
                // In the in-memory implementation Some(TrieNode::Empty) and None are interchangeable as
                // children of branch nodes which means cost has to be 0
                0
            }
            Self::Leaf { extension, value } => {
                TRIE_COSTS.node_cost
                    + (extension.len() as u64) * TRIE_COSTS.byte_of_key
                    + Self::memory_usage_value(value.len())
            }
            Self::Branch { value, .. } => {
                TRIE_COSTS.node_cost
                    + value.as_ref().map_or(0, |value| Self::memory_usage_value(value.len()))
            }
            Self::Extension { extension, .. } => {
                TRIE_COSTS.node_cost + (extension.len() as u64) * TRIE_COSTS.byte_of_key
            }
        }
    }
}

pub type UpdatedMemTrieNodeWithSize = GenericUpdatedTrieNodeWithSize<MemTrieNodeId, FlatStateValue>;

/// Trait for trie updates to handle updated nodes.
///
/// So far, this is used to handle key-value insertions, deletions and range
/// retain operation. To be performant, such logic requires keeping track of
/// intermediate updated nodes, together with subtree function (memory usage).
///
/// `GenericTrieUpdate` abstracts the storage of updated nodes for the original
/// node type `GenericTrieNodePtr`.
///
/// In this storage, nodes are indexed by `GenericUpdatedNodeId`.
/// Node is stored as `GenericUpdatedTrieNodeWithSize`, which stores children
/// as `GenericNodeOrIndex`. Each child may be either an old node or an updated
/// node.
///
/// The flow of interaction with this storage is:
/// - In the beginning, call `ensure_updated` for the
/// `GenericNodeOrIndex::Old(root_node)` which returns `GenericUpdatedNodeId`,
/// it should be zero.
/// - For every update (single insert, single delete, recursive range
/// operation...), call corresponding method with `GenericUpdatedNodeId` for
/// the root.
/// - Then, we hold the invariant that on every descent we have
/// `GenericUpdatedNodeId`.
/// - So, first, we call `take_node` to get `GenericUpdatedTrieNodeWithSize`
/// back;
/// - We possibly descend into its children and modify the node;
/// - Then, we call `place_node` to put the node back and return to the
/// node parent.
/// - Finally, we end up with storage of new nodes, which are used to produce
/// new state root. The exact logic depends on trait implementation.
///
/// TODO(#12324): instead of `GenericValueHandle`, consider always using
/// `FlatStateValue`.
///
/// Note that it has nothing to do with `TrieUpdate` used for runtime to store
/// temporary state changes (TODO(#12324) - consider renaming it).
pub(crate) trait GenericTrieUpdate<'a, GenericTrieNodePtr, GenericValueHandle> {
    /// If the ID was old, converts underlying node to an updated one.
    fn generic_ensure_updated(
        &mut self,
        node: GenericNodeOrIndex<GenericTrieNodePtr>,
    ) -> Result<GenericUpdatedNodeId, StorageError>;

    /// Takes a node from the set of updated nodes, setting it to None.
    /// It is expected that place_node is then called to return the node to
    /// the same slot.
    fn generic_take_node(
        &mut self,
        node_id: GenericUpdatedNodeId,
    ) -> GenericUpdatedTrieNodeWithSize<GenericTrieNodePtr, GenericValueHandle>;

    /// Puts a node to the set of updated nodes.
    fn generic_place_node(
        &mut self,
        node_id: GenericUpdatedNodeId,
        node: GenericUpdatedTrieNodeWithSize<GenericTrieNodePtr, GenericValueHandle>,
    );

    /// Gets a node from the set of updated nodes.
    /// TODO(#12324): we actually should get a reference, but type
    /// incompatibility don't allow it for now.
    fn generic_get_node(
        &self,
        node_id: GenericUpdatedNodeId,
    ) -> GenericUpdatedTrieNodeWithSize<GenericTrieNodePtr, GenericValueHandle>;
}

/// Keeps values and internal nodes accessed on updating memtrie.
pub struct TrieAccesses {
    /// Hashes and encoded trie nodes.
    pub nodes: HashMap<CryptoHash, Arc<[u8]>>,
    /// Hashes of accessed values - because values themselves are not
    /// necessarily present in memtrie.
    pub values: HashMap<CryptoHash, FlatStateValue>,
}

/// Tracks intermediate trie changes, final version of which is to be committed
/// to disk after finishing trie update.
struct TrieChangesTracker {
    /// Changes of reference count on disk for each impacted node.
    refcount_changes: TrieRefcountDeltaMap,
    /// All observed values and internal nodes.
    /// Needed to prepare recorded storage.
    /// Note that negative `refcount_changes` does not fully cover it, as node
    /// or value of the same hash can be removed and inserted for the same
    /// update in different parts of trie!
    accesses: TrieAccesses,
}

/// Structure to build an update to the in-memory trie.
pub struct MemTrieUpdate<'a, M: ArenaMemory> {
    /// The original root before updates. It is None iff the original trie had no keys.
    root: Option<MemTrieNodeId>,
    memory: &'a M,
    shard_uid: String, // for metrics only
    /// All the new nodes that are to be constructed. A node may be None if
    /// (1) temporarily we take out the node from the slot to process it and put it back
    /// later; or (2) the node is deleted afterwards.
    pub updated_nodes: Vec<Option<UpdatedMemTrieNode>>,
    /// Tracks trie changes necessary to make on-disk updates and recorded
    /// storage.
    tracked_trie_changes: Option<TrieChangesTracker>,
}

impl UpdatedMemTrieNode {
    /// Converts an existing in-memory trie node into an updated one that is
    /// equivalent.
    pub fn from_existing_node_view<'a, M: ArenaMemory>(view: MemTrieNodeView<'a, M>) -> Self {
        match view {
            MemTrieNodeView::Leaf { extension, value } => Self::Leaf {
                extension: extension.to_vec().into_boxed_slice(),
                value: value.to_flat_value(),
            },
            MemTrieNodeView::Branch { children, .. } => Self::Branch {
                children: Box::new(Self::convert_children_to_updated(children)),
                value: None,
            },
            MemTrieNodeView::BranchWithValue { children, value, .. } => Self::Branch {
                children: Box::new(Self::convert_children_to_updated(children)),
                value: Some(value.to_flat_value()),
            },
            MemTrieNodeView::Extension { extension, child, .. } => Self::Extension {
                extension: extension.to_vec().into_boxed_slice(),
                child: OldOrUpdatedNodeId::Old(child.id()),
            },
        }
    }

    fn convert_children_to_updated<'a, M: ArenaMemory>(
        view: ChildrenView<'a, M>,
    ) -> [Option<OldOrUpdatedNodeId>; 16] {
        let mut children = [None; 16];
        for i in 0..16 {
            if let Some(child) = view.get(i) {
                children[i] = Some(OldOrUpdatedNodeId::Old(child.id()));
            }
        }
        children
    }
}

impl<'a, M: ArenaMemory> GenericTrieUpdate<'a, MemTrieNodeId, FlatStateValue>
    for MemTrieUpdate<'a, M>
{
    fn generic_ensure_updated(
        &mut self,
        node: GenericNodeOrIndex<MemTrieNodeId>,
    ) -> Result<GenericUpdatedNodeId, StorageError> {
        Ok(self.ensure_updated(node))
    }

    fn generic_take_node(&mut self, index: UpdatedMemTrieNodeId) -> UpdatedMemTrieNodeWithSize {
        // TODO(#12324): IMPORTANT: now, we don't compute memory usage on the
        // fly for memtries. This happens in `compute_hashes_and_serialized_nodes`.
        // Memory usages here are zeroed and ignored.
        // However, this is fundamentally wrong because the current approach
        // needs ALL children of any changed branch in memtrie. In reality, it
        // is enough to have only children that are changed.
        // So, we need to change `MemTrieUpdate` to store current memory usages
        // and retrieve them correctly.
        UpdatedMemTrieNodeWithSize { node: self.take_node(index), memory_usage: 0 }
    }

    fn generic_place_node(
        &mut self,
        index: UpdatedMemTrieNodeId,
        node: UpdatedMemTrieNodeWithSize,
    ) {
        self.place_node(index, node.node);
    }

    fn generic_get_node(&self, node_id: GenericUpdatedNodeId) -> UpdatedMemTrieNodeWithSize {
        UpdatedMemTrieNodeWithSize {
            node: self.updated_nodes[node_id].as_ref().unwrap().clone(),
            memory_usage: 0,
        }
    }
}

pub(crate) type TrieStorageNodePtr = CryptoHash;

pub(crate) type UpdatedTrieStorageNode = GenericUpdatedTrieNode<TrieStorageNodePtr, ValueHandle>;

pub(crate) type UpdatedTrieStorageNodeWithSize =
    GenericUpdatedTrieNodeWithSize<TrieStorageNodePtr, ValueHandle>;

/// Conversion between updated node for trie storage and generic updated node.
/// TODO(#12324): remove once the whole trie storage logic is rewritten in
/// generic terms.
impl UpdatedTrieStorageNode {
    pub fn from_trie_node_with_size(node: TrieNodeWithSize) -> Self {
        match node.node {
            TrieNode::Empty => Self::Empty,
            TrieNode::Leaf(extension, value) => {
                Self::Leaf { extension: extension.to_vec().into_boxed_slice(), value }
            }
            TrieNode::Branch(children, value) => Self::Branch {
                children: Box::new(children.0.map(|child| {
                    child.map(|id| match id {
                        NodeHandle::Hash(id) => GenericNodeOrIndex::Old(id),
                        NodeHandle::InMemory(id) => GenericNodeOrIndex::Updated(id.0),
                    })
                })),
                value,
            },
            TrieNode::Extension(extension, child) => Self::Extension {
                extension: extension.to_vec().into_boxed_slice(),
                child: match child {
                    NodeHandle::Hash(id) => GenericNodeOrIndex::Old(id),
                    NodeHandle::InMemory(id) => GenericNodeOrIndex::Updated(id.0),
                },
            },
        }
    }

    pub fn into_trie_node_with_size(self, memory_usage: u64) -> TrieNodeWithSize {
        match self {
            Self::Empty => TrieNodeWithSize { node: TrieNode::Empty, memory_usage },
            Self::Leaf { extension, value } => {
                TrieNodeWithSize { node: TrieNode::Leaf(extension.into_vec(), value), memory_usage }
            }
            Self::Branch { children, value } => TrieNodeWithSize {
                node: TrieNode::Branch(
                    Box::new(Children(children.map(|child| {
                        child.map(|id| match id {
                            GenericNodeOrIndex::Old(id) => NodeHandle::Hash(id),
                            GenericNodeOrIndex::Updated(id) => {
                                NodeHandle::InMemory(StorageHandle(id))
                            }
                        })
                    }))),
                    value,
                ),
                memory_usage,
            },
            Self::Extension { extension, child } => TrieNodeWithSize {
                node: TrieNode::Extension(
                    extension.into_vec(),
                    match child {
                        GenericNodeOrIndex::Old(id) => NodeHandle::Hash(id),
                        GenericNodeOrIndex::Updated(id) => NodeHandle::InMemory(StorageHandle(id)),
                    },
                ),
                memory_usage,
            },
        }
    }
}

impl<'a> GenericTrieUpdate<'a, TrieStorageNodePtr, ValueHandle> for NodesStorage<'a> {
    fn generic_ensure_updated(
        &mut self,
        node: GenericNodeOrIndex<TrieStorageNodePtr>,
    ) -> Result<GenericUpdatedNodeId, StorageError> {
        match node {
            GenericNodeOrIndex::Old(node_hash) => {
                self.trie.move_node_to_mutable(self, &node_hash).map(|handle| handle.0)
            }
            GenericNodeOrIndex::Updated(node_id) => Ok(node_id),
        }
    }

    fn generic_take_node(&mut self, index: GenericUpdatedNodeId) -> UpdatedTrieStorageNodeWithSize {
        let node = self.destroy(StorageHandle(index));
        let memory_usage = node.memory_usage;
        UpdatedTrieStorageNodeWithSize {
            node: UpdatedTrieStorageNode::from_trie_node_with_size(node),
            memory_usage,
        }
    }

    fn generic_place_node(
        &mut self,
        index: GenericUpdatedNodeId,
        node: UpdatedTrieStorageNodeWithSize,
    ) {
        let UpdatedTrieStorageNodeWithSize { node, memory_usage } = node;
        let node = node.into_trie_node_with_size(memory_usage);
        self.store_at(StorageHandle(index), node);
    }

    fn generic_get_node(&self, index: GenericUpdatedNodeId) -> UpdatedTrieStorageNodeWithSize {
        let node = self.node_ref(StorageHandle(index)).clone();
        let memory_usage = node.memory_usage;
        UpdatedTrieStorageNodeWithSize {
            node: UpdatedTrieStorageNode::from_trie_node_with_size(node),
            memory_usage,
        }
    }
}

impl<'a, M: ArenaMemory> MemTrieUpdate<'a, M> {
    pub fn new(
        root: Option<MemTrieNodeId>,
        memory: &'a M,
        shard_uid: String,
        track_trie_changes: bool,
    ) -> Self {
        let mut trie_update = Self {
            root,
            memory,
            shard_uid,
            updated_nodes: vec![],
            tracked_trie_changes: if track_trie_changes {
                Some(TrieChangesTracker {
                    refcount_changes: TrieRefcountDeltaMap::new(),
                    accesses: TrieAccesses { nodes: HashMap::new(), values: HashMap::new() },
                })
            } else {
                None
            },
        };
        assert_eq!(trie_update.convert_existing_to_updated(root), 0usize);
        trie_update
    }

    /// Internal function to take a node from the array of updated nodes, setting it
    /// to None. It is expected that place_node is then called to return the node to
    /// the same slot.
    pub(crate) fn take_node(&mut self, index: UpdatedMemTrieNodeId) -> UpdatedMemTrieNode {
        self.updated_nodes.get_mut(index).unwrap().take().expect("Node taken twice")
    }

    /// Does the opposite of take_node; returns the node to the specified ID.
    pub(crate) fn place_node(&mut self, index: UpdatedMemTrieNodeId, node: UpdatedMemTrieNode) {
        assert!(self.updated_nodes[index].is_none(), "Node placed twice");
        self.updated_nodes[index] = Some(node);
    }

    /// Creates a new updated node, assigning it a new ID.
    fn new_updated_node(&mut self, node: UpdatedMemTrieNode) -> UpdatedMemTrieNodeId {
        let index = self.updated_nodes.len();
        self.updated_nodes.push(Some(node));
        index
    }

    /// This is called when we need to mutate a subtree of the original trie.
    /// It decrements the refcount of the original trie node (since logically
    /// we are removing it), and creates a new node that is equivalent to the
    /// original node. The ID of the new node is returned.
    ///
    /// If the original node is None, it is a marker for the root of an empty
    /// trie.
    fn convert_existing_to_updated(&mut self, node: Option<MemTrieNodeId>) -> UpdatedMemTrieNodeId {
        match node {
            None => self.new_updated_node(UpdatedMemTrieNode::Empty),
            Some(node) => {
                if let Some(tracked_trie_changes) = self.tracked_trie_changes.as_mut() {
                    let node_view = node.as_ptr(self.memory).view();
                    let node_hash = node_view.node_hash();
                    let raw_node_serialized =
                        borsh::to_vec(&node_view.to_raw_trie_node_with_size()).unwrap();
                    tracked_trie_changes
                        .accesses
                        .nodes
                        .insert(node_hash, raw_node_serialized.into());
                    tracked_trie_changes.refcount_changes.subtract(node_hash, 1);
                }
                self.new_updated_node(UpdatedMemTrieNode::from_existing_node_view(
                    node.as_ptr(self.memory).view(),
                ))
            }
        }
    }

    pub(crate) fn ensure_updated(&mut self, node: OldOrUpdatedNodeId) -> UpdatedMemTrieNodeId {
        match node {
            OldOrUpdatedNodeId::Old(node_id) => self.convert_existing_to_updated(Some(node_id)),
            OldOrUpdatedNodeId::Updated(node_id) => node_id,
        }
    }

    fn add_refcount_to_value(&mut self, hash: CryptoHash, value: Option<Vec<u8>>) {
        if let Some(tracked_node_changes) = self.tracked_trie_changes.as_mut() {
            tracked_node_changes.refcount_changes.add(hash, value.unwrap(), 1);
        }
    }

    fn subtract_refcount_for_value(&mut self, value: FlatStateValue) {
        if let Some(tracked_node_changes) = self.tracked_trie_changes.as_mut() {
            let hash = value.to_value_ref().hash;
            tracked_node_changes.accesses.values.insert(hash, value);
            tracked_node_changes.refcount_changes.subtract(hash, 1);
        }
    }

    /// Inserts the given key value pair into the trie.
    pub fn insert(&mut self, key: &[u8], value: Vec<u8>) {
        self.insert_impl(key, FlatStateValue::on_disk(&value), Some(value));
    }

    /// Inserts the given key value pair into the trie, but the value may be a reference.
    /// This is used to update the in-memory trie only, without caring about on-disk changes.
    pub fn insert_memtrie_only(&mut self, key: &[u8], value: FlatStateValue) {
        self.insert_impl(key, value, None);
    }

    /// Insertion logic. We descend from the root down to whatever node corresponds to
    /// the inserted value. We would need to split, modify, or transform nodes along
    /// the way to achieve that. This takes care of refcounting changes for existing
    /// nodes as well as values, but will not yet increment refcount for any newly
    /// created nodes - that's done at the end.
    ///
    /// Note that `value` must be Some if we're keeping track of on-disk changes, but can
    /// be None if we're only keeping track of in-memory changes.
    fn insert_impl(&mut self, key: &[u8], flat_value: FlatStateValue, value: Option<Vec<u8>>) {
        let mut node_id = 0; // root
        let mut partial = NibbleSlice::new(key);
        let value_ref = flat_value.to_value_ref();

        loop {
            // Take out the current node; we'd have to change it no matter what.
            let node = self.take_node(node_id);
            match node {
                UpdatedMemTrieNode::Empty => {
                    // There was no node here, create a new leaf.
                    self.place_node(
                        node_id,
                        UpdatedMemTrieNode::Leaf {
                            extension: partial.encoded(true).into_vec().into_boxed_slice(),
                            value: flat_value,
                        },
                    );
                    self.add_refcount_to_value(value_ref.hash, value);
                    break;
                }
                UpdatedMemTrieNode::Branch { children, value: old_value } => {
                    if partial.is_empty() {
                        // This branch node is exactly where the value should be added.
                        if let Some(value) = old_value {
                            self.subtract_refcount_for_value(value);
                        }
                        self.place_node(
                            node_id,
                            UpdatedMemTrieNode::Branch { children, value: Some(flat_value) },
                        );
                        self.add_refcount_to_value(value_ref.hash, value);
                        break;
                    } else {
                        // Continue descending into the branch, possibly adding a new child.
                        let mut new_children = children;
                        let child = &mut new_children[partial.at(0) as usize];
                        let new_node_id = match child.take() {
                            Some(node_id) => self.ensure_updated(node_id),
                            None => self.new_updated_node(UpdatedMemTrieNode::Empty),
                        };
                        *child = Some(OldOrUpdatedNodeId::Updated(new_node_id));
                        self.place_node(
                            node_id,
                            UpdatedMemTrieNode::Branch { children: new_children, value: old_value },
                        );
                        node_id = new_node_id;
                        partial = partial.mid(1);
                        continue;
                    }
                }
                UpdatedMemTrieNode::Leaf { extension, value: old_value } => {
                    let existing_key = NibbleSlice::from_encoded(&extension).0;
                    let common_prefix = partial.common_prefix(&existing_key);
                    if common_prefix == existing_key.len() && common_prefix == partial.len() {
                        // We're at the exact leaf. Rewrite the value at this leaf.
                        self.subtract_refcount_for_value(old_value);
                        self.place_node(
                            node_id,
                            UpdatedMemTrieNode::Leaf { extension, value: flat_value },
                        );
                        self.add_refcount_to_value(value_ref.hash, value);
                        break;
                    } else if common_prefix == 0 {
                        // Convert the leaf to an equivalent branch. We are not adding
                        // the new branch yet; that will be done in the next iteration.
                        let mut children = Box::<[_; 16]>::default();
                        let branch_node = if existing_key.is_empty() {
                            // Existing key being empty means the old value now lives at the branch.
                            UpdatedMemTrieNode::Branch { children, value: Some(old_value) }
                        } else {
                            let branch_idx = existing_key.at(0) as usize;
                            let new_extension = existing_key.mid(1).encoded(true).into_vec();
                            let new_node_id = self.new_updated_node(UpdatedMemTrieNode::Leaf {
                                extension: new_extension.into_boxed_slice(),
                                value: old_value,
                            });
                            children[branch_idx] = Some(OldOrUpdatedNodeId::Updated(new_node_id));
                            UpdatedMemTrieNode::Branch { children, value: None }
                        };
                        self.place_node(node_id, branch_node);
                        continue;
                    } else {
                        // Split this leaf into an extension plus a leaf, and descend into the leaf.
                        let new_node_id = self.new_updated_node(UpdatedMemTrieNode::Leaf {
                            extension: existing_key
                                .mid(common_prefix)
                                .encoded(true)
                                .into_vec()
                                .into_boxed_slice(),
                            value: old_value,
                        });
                        let node = UpdatedMemTrieNode::Extension {
                            extension: partial
                                .encoded_leftmost(common_prefix, false)
                                .into_vec()
                                .into_boxed_slice(),
                            child: OldOrUpdatedNodeId::Updated(new_node_id),
                        };
                        self.place_node(node_id, node);
                        node_id = new_node_id;
                        partial = partial.mid(common_prefix);
                        continue;
                    }
                }
                UpdatedMemTrieNode::Extension { extension, child: old_child, .. } => {
                    let existing_key = NibbleSlice::from_encoded(&extension).0;
                    let common_prefix = partial.common_prefix(&existing_key);
                    if common_prefix == 0 {
                        // Split Extension to Branch.
                        let idx = existing_key.at(0);
                        let child = if existing_key.len() == 1 {
                            old_child
                        } else {
                            let inner_child = UpdatedMemTrieNode::Extension {
                                extension: existing_key
                                    .mid(1)
                                    .encoded(false)
                                    .into_vec()
                                    .into_boxed_slice(),
                                child: old_child,
                            };
                            OldOrUpdatedNodeId::Updated(self.new_updated_node(inner_child))
                        };

                        let mut children = Box::<[_; 16]>::default();
                        children[idx as usize] = Some(child);
                        let branch_node = UpdatedMemTrieNode::Branch { children, value: None };
                        self.place_node(node_id, branch_node);
                        // Start over from the same position.
                        continue;
                    } else if common_prefix == existing_key.len() {
                        // Dereference child and descend into it.
                        let child = self.ensure_updated(old_child);
                        let node = UpdatedMemTrieNode::Extension {
                            extension,
                            child: OldOrUpdatedNodeId::Updated(child),
                        };
                        self.place_node(node_id, node);
                        node_id = child;
                        partial = partial.mid(common_prefix);
                        continue;
                    } else {
                        // Partially shared prefix. Convert to shorter extension and descend into it.
                        // On the next step, branch will be created.
                        let inner_child_node_id =
                            self.new_updated_node(UpdatedMemTrieNode::Extension {
                                extension: existing_key
                                    .mid(common_prefix)
                                    .encoded(false)
                                    .into_vec()
                                    .into_boxed_slice(),
                                child: old_child,
                            });
                        let child_node = UpdatedMemTrieNode::Extension {
                            extension: existing_key
                                .encoded_leftmost(common_prefix, false)
                                .into_vec()
                                .into_boxed_slice(),
                            child: OldOrUpdatedNodeId::Updated(inner_child_node_id),
                        };
                        self.place_node(node_id, child_node);
                        node_id = inner_child_node_id;
                        partial = partial.mid(common_prefix);
                        continue;
                    }
                }
            }
        }
    }

    /// Deletes a key from the trie.
    ///
    /// This will go down from the root of the trie to supposed location of the
    /// key, deleting it if found. It will also keep the trie structure
    /// consistent by changing the types of any nodes along the way.
    ///
    /// Deleting a non-existent key is allowed, and is a no-op.
    pub fn delete(&mut self, key: &[u8]) {
        let mut node_id = 0; // root
        let mut partial = NibbleSlice::new(key);
        let mut path = vec![]; // for squashing at the end.

        loop {
            path.push(node_id);
            let node = self.take_node(node_id);

            match node {
                UpdatedMemTrieNode::Empty => {
                    // Nothing to delete.
                    self.place_node(node_id, UpdatedMemTrieNode::Empty);
                    return;
                }
                UpdatedMemTrieNode::Leaf { extension, value } => {
                    if NibbleSlice::from_encoded(&extension).0 == partial {
                        self.subtract_refcount_for_value(value);
                        self.place_node(node_id, UpdatedMemTrieNode::Empty);
                        break;
                    } else {
                        // Key being deleted doesn't exist.
                        self.place_node(node_id, UpdatedMemTrieNode::Leaf { extension, value });
                        return;
                    }
                }
                UpdatedMemTrieNode::Branch { children: old_children, value } => {
                    if partial.is_empty() {
                        if value.is_none() {
                            // Key being deleted doesn't exist.
                            self.place_node(
                                node_id,
                                UpdatedMemTrieNode::Branch { children: old_children, value },
                            );
                            return;
                        };
                        self.subtract_refcount_for_value(value.unwrap());
                        self.place_node(
                            node_id,
                            UpdatedMemTrieNode::Branch { children: old_children, value: None },
                        );
                        // if needed, branch will be squashed at the end of the function.
                        break;
                    } else {
                        let mut new_children = old_children.clone();
                        let child = &mut new_children[partial.at(0) as usize];
                        let old_child_id = match child.take() {
                            Some(node_id) => node_id,
                            None => {
                                // Key being deleted doesn't exist.
                                self.place_node(
                                    node_id,
                                    UpdatedMemTrieNode::Branch { children: old_children, value },
                                );
                                return;
                            }
                        };
                        let new_child_id = self.ensure_updated(old_child_id);
                        *child = Some(OldOrUpdatedNodeId::Updated(new_child_id));
                        self.place_node(
                            node_id,
                            UpdatedMemTrieNode::Branch { children: new_children, value },
                        );

                        node_id = new_child_id;
                        partial = partial.mid(1);
                        continue;
                    }
                }
                UpdatedMemTrieNode::Extension { extension, child } => {
                    let (common_prefix, existing_len) = {
                        let extension_nibbles = NibbleSlice::from_encoded(&extension).0;
                        (extension_nibbles.common_prefix(&partial), extension_nibbles.len())
                    };
                    if common_prefix == existing_len {
                        let new_child_id = self.ensure_updated(child);
                        self.place_node(
                            node_id,
                            UpdatedMemTrieNode::Extension {
                                extension,
                                child: OldOrUpdatedNodeId::Updated(new_child_id),
                            },
                        );

                        node_id = new_child_id;
                        partial = partial.mid(existing_len);
                        continue;
                    } else {
                        // Key being deleted doesn't exist.
                        self.place_node(
                            node_id,
                            UpdatedMemTrieNode::Extension { extension, child },
                        );
                        return;
                    }
                }
            }
        }

        // We may need to change node type to keep the trie structure unique.
        for node_id in path.into_iter().rev() {
            self.squash_node(node_id).unwrap();
        }
    }
}

impl<
        'a,
        N: std::fmt::Debug,
        V: std::fmt::Debug + HasValueLength,
        T: GenericTrieUpdate<'a, N, V>,
    > GenericTrieUpdateSquash<'a, N, V> for T
{
}

pub(crate) trait GenericTrieUpdateSquash<
    'a,
    N: std::fmt::Debug,
    V: std::fmt::Debug + HasValueLength,
>: GenericTrieUpdate<'a, N, V>
{
    /// When we delete keys, it may be necessary to change types of some nodes,
    /// in order to keep the trie structure unique. For example, if a branch
    /// had two children, but after deletion ended up with one child and no
    /// value, it must be converted to an extension node. Or, if an extension
    /// node ended up having a child which is also an extension node, they must
    /// be combined into a single extension node. This function takes care of
    /// all these cases for a single node.
    ///
    /// To restructure trie correctly, this function must be called in
    /// post-order traversal for every modified node. It may be proven by
    /// induction on subtrees.
    /// For single key removal, it is called for every node on the path from
    /// the leaf to the root.
    /// For range removal, it is called in the end of recursive range removal
    /// function, which is the definition of post-order traversal.
    fn squash_node(&mut self, node_id: GenericUpdatedNodeId) -> Result<(), StorageError> {
        let GenericUpdatedTrieNodeWithSize { node, memory_usage } = self.generic_take_node(node_id);
        match node {
            GenericUpdatedTrieNode::Empty => {
                // Empty node will be absorbed by its parent node, so defer that.
                self.generic_place_node(node_id, GenericUpdatedTrieNodeWithSize::empty());
            }
            GenericUpdatedTrieNode::Leaf { .. } => {
                // It's impossible that we would squash a leaf node, because if we
                // had deleted a leaf it would become Empty instead.
                unreachable!();
            }
            GenericUpdatedTrieNode::Branch { mut children, value } => {
                // Remove any children that are now empty (removed).
                for child in children.iter_mut() {
                    if let Some(GenericNodeOrIndex::Updated(child_node_id)) = child {
                        if let GenericUpdatedTrieNode::Empty =
                            self.generic_get_node(*child_node_id).node
                        {
                            *child = None;
                        }
                    }
                }
                let num_children = children.iter().filter(|node| node.is_some()).count();
                if num_children == 0 {
                    match value {
                        None => self
                            .generic_place_node(node_id, GenericUpdatedTrieNodeWithSize::empty()),
                        Some(value) => {
                            // Branch with zero children and a value becomes leaf.
                            let leaf_node = GenericUpdatedTrieNode::Leaf {
                                extension: NibbleSlice::new(&[])
                                    .encoded(true)
                                    .into_vec()
                                    .into_boxed_slice(),
                                value,
                            };
                            let memory_usage = leaf_node.memory_usage_direct();
                            self.generic_place_node(
                                node_id,
                                GenericUpdatedTrieNodeWithSize { node: leaf_node, memory_usage },
                            );
                        }
                    }
                } else if num_children == 1 && value.is_none() {
                    // Branch with 1 child but no value becomes extension.
                    let (idx, child) = children
                        .into_iter()
                        .enumerate()
                        .find_map(|(idx, node)| node.map(|node| (idx, node)))
                        .unwrap();
                    let extension = NibbleSlice::new(&[(idx << 4) as u8])
                        .encoded_leftmost(1, false)
                        .into_vec()
                        .into_boxed_slice();
                    self.extend_child(node_id, extension, child)?;
                } else {
                    // Branch with more than 1 children stays branch.
                    self.generic_place_node(
                        node_id,
                        GenericUpdatedTrieNodeWithSize {
                            node: GenericUpdatedTrieNode::Branch { children, value },
                            memory_usage,
                        },
                    );
                }
            }
            GenericUpdatedTrieNode::Extension { extension, child } => {
                self.extend_child(node_id, extension, child)?;
            }
        }
        Ok(())
    }

    // Creates an extension node at `node_id`, but squashes the extension node according to
    // its child; e.g. if the child is a leaf, the whole node becomes a leaf.
    fn extend_child(
        &mut self,
        // The node being squashed.
        node_id: GenericUpdatedNodeId,
        // The current extension.
        extension: Box<[u8]>,
        // The current child.
        child_id: GenericNodeOrIndex<N>,
    ) -> Result<(), StorageError> {
        let child_id = self.generic_ensure_updated(child_id)?;
        let GenericUpdatedTrieNodeWithSize { node, memory_usage } =
            self.generic_take_node(child_id);
        let child_child_memory_usage = memory_usage.saturating_sub(node.memory_usage_direct());
        match node {
            GenericUpdatedTrieNode::Empty => {
                self.generic_place_node(node_id, GenericUpdatedTrieNodeWithSize::empty());
            }
            // If the child is a leaf (which could happen if a branch node lost
            // all its branches and only had a value left, or is left with only
            // one branch and that was squashed to a leaf).
            GenericUpdatedTrieNode::Leaf { extension: child_extension, value } => {
                let child_extension = NibbleSlice::from_encoded(&child_extension).0;
                let extension = NibbleSlice::from_encoded(&extension)
                    .0
                    .merge_encoded(&child_extension, true)
                    .into_vec()
                    .into_boxed_slice();
                let node = GenericUpdatedTrieNode::Leaf { extension, value };
                let memory_usage = node.memory_usage_direct();
                self.generic_place_node(
                    node_id,
                    GenericUpdatedTrieNodeWithSize { node, memory_usage },
                );
            }
            // If the child is a branch, there's nothing to squash.
            child_node @ GenericUpdatedTrieNode::Branch { .. } => {
                self.generic_place_node(
                    child_id,
                    GenericUpdatedTrieNodeWithSize { node: child_node, memory_usage },
                );
                let node = GenericUpdatedTrieNode::Extension {
                    extension,
                    child: GenericNodeOrIndex::Updated(child_id),
                };
                let memory_usage = memory_usage + node.memory_usage_direct();
                self.generic_place_node(
                    node_id,
                    GenericUpdatedTrieNodeWithSize { node, memory_usage },
                );
            }
            // If the child is an extension (which could happen if a branch node
            // is left with only one branch), join the two extensions into one.
            GenericUpdatedTrieNode::Extension {
                extension: child_extension,
                child: inner_child,
            } => {
                let child_extension = NibbleSlice::from_encoded(&child_extension).0;
                let merged_extension = NibbleSlice::from_encoded(&extension)
                    .0
                    .merge_encoded(&child_extension, false)
                    .into_vec()
                    .into_boxed_slice();
                let node = GenericUpdatedTrieNode::Extension {
                    extension: merged_extension,
                    child: inner_child,
                };
                let memory_usage = node.memory_usage_direct() + child_child_memory_usage;
                self.generic_place_node(
                    node_id,
                    GenericUpdatedTrieNodeWithSize { node, memory_usage },
                );
            }
        }
        Ok(())
    }
}

impl<'a, M: ArenaMemory> MemTrieUpdate<'a, M> {
    /// To construct the new trie nodes, we need to create the new nodes in an
    /// order such that children are created before their parents - essentially
    /// a topological sort. We do this via a post-order traversal of the
    /// updated nodes. After this function, `ordered_nodes` contains the IDs of
    /// the updated nodes in the order they should be created.
    fn post_order_traverse_updated_nodes(
        node_id: UpdatedMemTrieNodeId,
        updated_nodes: &Vec<Option<UpdatedMemTrieNode>>,
        ordered_nodes: &mut Vec<UpdatedMemTrieNodeId>,
    ) {
        let node = updated_nodes[node_id].as_ref().unwrap();
        match node {
            UpdatedMemTrieNode::Empty => {
                assert_eq!(node_id, 0); // only root can be empty
                return;
            }
            UpdatedMemTrieNode::Branch { children, .. } => {
                for child in children.iter() {
                    if let Some(OldOrUpdatedNodeId::Updated(child_node_id)) = child {
                        Self::post_order_traverse_updated_nodes(
                            *child_node_id,
                            updated_nodes,
                            ordered_nodes,
                        );
                    }
                }
            }
            UpdatedMemTrieNode::Extension { child, .. } => {
                if let OldOrUpdatedNodeId::Updated(child_node_id) = child {
                    Self::post_order_traverse_updated_nodes(
                        *child_node_id,
                        updated_nodes,
                        ordered_nodes,
                    );
                }
            }
            _ => {}
        }
        ordered_nodes.push(node_id);
    }

    /// For each node in `ordered_nodes`, computes its hash and serialized data.
    /// `ordered_nodes` is expected to follow the post-order traversal of the
    /// updated nodes.
    /// `updated_nodes` must be indexed by the node IDs in `ordered_nodes`.
    pub(crate) fn compute_hashes_and_serialized_nodes(
        &self,
        ordered_nodes: &Vec<UpdatedMemTrieNodeId>,
        updated_nodes: &Vec<Option<UpdatedMemTrieNode>>,
    ) -> Vec<(UpdatedMemTrieNodeId, CryptoHash, Vec<u8>)> {
        let memory = self.memory;
        let mut result = Vec::<(CryptoHash, u64, Vec<u8>)>::new();
        for _ in 0..updated_nodes.len() {
            result.push((CryptoHash::default(), 0, Vec::new()));
        }
        let get_hash_and_memory_usage = |node: OldOrUpdatedNodeId,
                                         result: &Vec<(CryptoHash, u64, Vec<u8>)>|
         -> (CryptoHash, u64) {
            match node {
                OldOrUpdatedNodeId::Updated(node_id) => {
                    let (hash, memory_usage, _) = result[node_id];
                    (hash, memory_usage)
                }
                OldOrUpdatedNodeId::Old(node_id) => {
                    let view = node_id.as_ptr(memory).view();
                    (view.node_hash(), view.memory_usage())
                }
            }
        };

        for node_id in ordered_nodes.iter() {
            let node = updated_nodes[*node_id].as_ref().unwrap();
            let (raw_node, memory_usage) = match node {
                UpdatedMemTrieNode::Empty => unreachable!(),
                UpdatedMemTrieNode::Branch { children, value } => {
                    let mut memory_usage = TRIE_COSTS.node_cost;
                    let mut child_hashes = vec![];
                    for child in children.iter() {
                        match child {
                            Some(child) => {
                                let (child_hash, child_memory_usage) =
                                    get_hash_and_memory_usage(*child, &result);
                                child_hashes.push(Some(child_hash));
                                memory_usage += child_memory_usage;
                            }
                            None => {
                                child_hashes.push(None);
                            }
                        }
                    }
                    let children = Children(child_hashes.as_slice().try_into().unwrap());
                    let value_ref = value.as_ref().map(|value| value.to_value_ref());
                    memory_usage += match &value_ref {
                        Some(value_ref) => {
                            value_ref.length as u64 * TRIE_COSTS.byte_of_value
                                + TRIE_COSTS.node_cost
                        }
                        None => 0,
                    };
                    (RawTrieNode::branch(children, value_ref), memory_usage)
                }
                UpdatedMemTrieNode::Extension { extension, child } => {
                    let (child_hash, child_memory_usage) =
                        get_hash_and_memory_usage(*child, &result);
                    let memory_usage = TRIE_COSTS.node_cost
                        + extension.len() as u64 * TRIE_COSTS.byte_of_key
                        + child_memory_usage;
                    (RawTrieNode::Extension(extension.to_vec(), child_hash), memory_usage)
                }
                UpdatedMemTrieNode::Leaf { extension, value } => {
                    let memory_usage = TRIE_COSTS.node_cost
                        + extension.len() as u64 * TRIE_COSTS.byte_of_key
                        + value.value_len() as u64 * TRIE_COSTS.byte_of_value
                        + TRIE_COSTS.node_cost;
                    (RawTrieNode::Leaf(extension.to_vec(), value.to_value_ref()), memory_usage)
                }
            };

            let raw_node_with_size = RawTrieNodeWithSize { node: raw_node, memory_usage };
            let node_serialized = borsh::to_vec(&raw_node_with_size).unwrap();
            let node_hash = hash(&node_serialized);
            result[*node_id] = (node_hash, memory_usage, node_serialized);
        }

        ordered_nodes
            .iter()
            .map(|node_id| {
                let (hash, _, serialized) = &mut result[*node_id];
                (*node_id, *hash, std::mem::take(serialized))
            })
            .collect()
    }

    /// Converts the changes to memtrie changes. Also returns the list of new nodes inserted,
    /// in hash and serialized form.
    fn to_mem_trie_changes_internal(self) -> (MemTrieChanges, Vec<(CryptoHash, Vec<u8>)>) {
        MEM_TRIE_NUM_NODES_CREATED_FROM_UPDATES
            .with_label_values(&[&self.shard_uid])
            .inc_by(self.updated_nodes.len() as u64);
        let mut ordered_nodes = Vec::new();
        Self::post_order_traverse_updated_nodes(0, &self.updated_nodes, &mut ordered_nodes);

        let hashes_and_serialized_nodes =
            self.compute_hashes_and_serialized_nodes(&ordered_nodes, &self.updated_nodes);

        let node_ids_with_hashes = hashes_and_serialized_nodes
            .iter()
            .map(|(node_id, hash, _)| (*node_id, *hash))
            .collect();
        (
            MemTrieChanges { node_ids_with_hashes, updated_nodes: self.updated_nodes },
            hashes_and_serialized_nodes
                .into_iter()
                .map(|(_, hash, serialized)| (hash, serialized))
                .collect(),
        )
    }

    /// Converts the updates to memtrie changes only.
    pub fn to_mem_trie_changes_only(self) -> MemTrieChanges {
        let (mem_trie_changes, _) = self.to_mem_trie_changes_internal();
        mem_trie_changes
    }

    /// Converts the updates to trie changes as well as memtrie changes.
    pub(crate) fn to_trie_changes(mut self) -> (TrieChanges, TrieAccesses) {
        let old_root =
            self.root.map(|root| root.as_ptr(self.memory).view().node_hash()).unwrap_or_default();
        let TrieChangesTracker { mut refcount_changes, accesses } = self
            .tracked_trie_changes
            .take()
            .expect("Cannot to_trie_changes for memtrie changes only");
        let (mem_trie_changes, hashes_and_serialized) = self.to_mem_trie_changes_internal();

        // We've accounted for the dereferenced nodes, as well as value addition/subtractions.
        // The only thing left is to increment refcount for all new nodes.
        for (node_hash, node_serialized) in hashes_and_serialized {
            refcount_changes.add(node_hash, node_serialized, 1);
        }
        let (insertions, deletions) = refcount_changes.into_changes();

        (
            TrieChanges {
                old_root,
                new_root: mem_trie_changes
                    .node_ids_with_hashes
                    .last()
                    .map(|(_, hash)| *hash)
                    .unwrap_or_default(),
                insertions,
                deletions,
                mem_trie_changes: Some(mem_trie_changes),
            },
            accesses,
        )
    }
}

/// Applies the given memtrie changes to the in-memory trie data structure.
/// Returns the new root hash.
pub(super) fn construct_root_from_changes<A: ArenaMut>(
    arena: &mut A,
    changes: &MemTrieChanges,
) -> Option<MemTrieNodeId> {
    let mut last_node_id: Option<MemTrieNodeId> = None;
    let map_to_new_node_id = |node_id: OldOrUpdatedNodeId,
                              old_to_new_map: &HashMap<UpdatedMemTrieNodeId, MemTrieNodeId>|
     -> MemTrieNodeId {
        match node_id {
            OldOrUpdatedNodeId::Updated(node_id) => *old_to_new_map.get(&node_id).unwrap(),
            OldOrUpdatedNodeId::Old(node_id) => node_id,
        }
    };

    let mut updated_to_new_map = HashMap::<UpdatedMemTrieNodeId, MemTrieNodeId>::new();
    let updated_nodes = &changes.updated_nodes;
    let node_ids_with_hashes = &changes.node_ids_with_hashes;
    for (node_id, node_hash) in node_ids_with_hashes.iter() {
        let node = updated_nodes.get(*node_id).unwrap().clone().unwrap();
        let node = match &node {
            UpdatedMemTrieNode::Empty => unreachable!(),
            UpdatedMemTrieNode::Branch { children, value } => {
                let mut new_children = [None; 16];
                for i in 0..16 {
                    if let Some(child) = children[i] {
                        new_children[i] = Some(map_to_new_node_id(child, &updated_to_new_map));
                    }
                }
                match value {
                    Some(value) => {
                        InputMemTrieNode::BranchWithValue { children: new_children, value }
                    }
                    None => InputMemTrieNode::Branch { children: new_children },
                }
            }
            UpdatedMemTrieNode::Extension { extension, child } => InputMemTrieNode::Extension {
                extension,
                child: map_to_new_node_id(*child, &updated_to_new_map),
            },
            UpdatedMemTrieNode::Leaf { extension, value } => {
                InputMemTrieNode::Leaf { value, extension }
            }
        };
        let mem_node_id = MemTrieNodeId::new_with_hash(arena, node, *node_hash);
        updated_to_new_map.insert(*node_id, mem_node_id);
        last_node_id = Some(mem_node_id);
    }

    last_node_id
}

#[cfg(test)]
mod tests {
    use crate::test_utils::TestTriesBuilder;
    use crate::trie::mem::arena::hybrid::HybridArena;
    use crate::trie::mem::lookup::memtrie_lookup;
    use crate::trie::mem::mem_tries::MemTries;
    use crate::trie::MemTrieChanges;
    use crate::{KeyLookupMode, ShardTries, TrieChanges};
    use near_primitives::hash::CryptoHash;
    use near_primitives::shard_layout::ShardUId;
    use near_primitives::state::{FlatStateValue, ValueRef};
    use near_primitives::types::{BlockHeight, StateRoot};
    use rand::Rng;
    use std::collections::{HashMap, HashSet};

    struct TestTries {
        mem: MemTries,
        disk: ShardTries,
        truth: HashMap<Vec<u8>, Option<ValueRef>>,
        state_root: StateRoot,
        check_deleted_keys: bool,
    }

    impl TestTries {
        fn new(check_deleted_keys: bool) -> Self {
            let mem = MemTries::new(ShardUId::single_shard());
            let disk = TestTriesBuilder::new().build();
            Self {
                mem,
                disk,
                truth: HashMap::new(),
                state_root: StateRoot::default(),
                check_deleted_keys,
            }
        }

        fn make_all_changes(&mut self, changes: Vec<(Vec<u8>, Option<Vec<u8>>)>) -> TrieChanges {
            let mut update = self.mem.update(self.state_root, true).unwrap_or_else(|_| {
                panic!("Trying to update root {:?} but it's not in memtries", self.state_root)
            });
            for (key, value) in changes {
                if let Some(value) = value {
                    update.insert(&key, value);
                } else {
                    update.delete(&key);
                }
            }
            update.to_trie_changes().0
        }

        fn make_memtrie_changes_only(
            &mut self,
            changes: Vec<(Vec<u8>, Option<Vec<u8>>)>,
        ) -> MemTrieChanges {
            let mut update = self.mem.update(self.state_root, false).unwrap_or_else(|_| {
                panic!("Trying to update root {:?} but it's not in memtries", self.state_root)
            });
            for (key, value) in changes {
                if let Some(value) = value {
                    update.insert_memtrie_only(&key, FlatStateValue::on_disk(&value));
                } else {
                    update.delete(&key);
                }
            }
            update.to_mem_trie_changes_only()
        }

        fn make_disk_changes_only(
            &mut self,
            changes: Vec<(Vec<u8>, Option<Vec<u8>>)>,
        ) -> TrieChanges {
            let trie = self.disk.get_trie_for_shard(ShardUId::single_shard(), self.state_root);
            trie.update(changes).unwrap()
        }

        fn check_consistency_across_all_changes_and_apply(
            &mut self,
            changes: Vec<(Vec<u8>, Option<Vec<u8>>)>,
        ) {
            // First check consistency between the changes.
            let memtrie_changes = self.make_memtrie_changes_only(changes.clone());
            let disk_changes = self.make_disk_changes_only(changes.clone());
            let mut all_changes = self.make_all_changes(changes.clone());

            let mem_trie_changes_from_all_changes = all_changes.mem_trie_changes.take().unwrap();
            assert_eq!(memtrie_changes, mem_trie_changes_from_all_changes);
            assert_eq!(disk_changes, all_changes);

            // Then apply the changes and check consistency of new state roots.
            let new_state_root_from_mem = self.mem.apply_memtrie_changes(0, &memtrie_changes);
            let mut store_update = self.disk.store_update();
            let new_state_root_from_disk =
                self.disk.apply_all(&disk_changes, ShardUId::single_shard(), &mut store_update);
            assert_eq!(new_state_root_from_mem, new_state_root_from_disk);
            store_update.commit().unwrap();
            self.state_root = new_state_root_from_mem;

            // Update our truth.
            for (key, value) in changes {
                if let Some(value) = value {
                    self.truth.insert(key, Some(ValueRef::new(&value)));
                } else {
                    if self.check_deleted_keys {
                        self.truth.insert(key, None);
                    } else {
                        self.truth.remove(&key);
                    }
                }
            }

            // Check the truth against both memtrie and on-disk trie.
            for (key, value_ref) in &self.truth {
                let memtrie_root = if self.state_root == StateRoot::default() {
                    None
                } else {
                    Some(self.mem.get_root(&self.state_root).unwrap())
                };
                let disk_trie =
                    self.disk.get_trie_for_shard(ShardUId::single_shard(), self.state_root);
                let memtrie_result =
                    memtrie_root.and_then(|memtrie_root| memtrie_lookup(memtrie_root, key, None));
                let disk_result = disk_trie.get_optimized_ref(key, KeyLookupMode::Trie).unwrap();
                if let Some(value_ref) = value_ref {
                    let memtrie_value_ref = memtrie_result
                        .unwrap_or_else(|| {
                            panic!("Key {} is in truth but not in memtrie", hex::encode(key))
                        })
                        .to_flat_value()
                        .to_value_ref();
                    let disk_value_ref = disk_result
                        .unwrap_or_else(|| {
                            panic!("Key {} is in truth but not in disk trie", hex::encode(key))
                        })
                        .into_value_ref();
                    assert_eq!(
                        memtrie_value_ref,
                        *value_ref,
                        "Value for key {} is incorrect for memtrie",
                        hex::encode(key)
                    );
                    assert_eq!(
                        disk_value_ref,
                        *value_ref,
                        "Value for key {} is incorrect for disk trie",
                        hex::encode(key)
                    );
                } else {
                    assert!(
                        memtrie_result.is_none(),
                        "Key {} is not in truth but is in memtrie",
                        hex::encode(key)
                    );
                    assert!(
                        disk_result.is_none(),
                        "Key {} is not in truth but is in disk trie",
                        hex::encode(key)
                    );
                }
            }
        }
    }

    fn parse_changes(s: &str) -> Vec<(Vec<u8>, Option<Vec<u8>>)> {
        s.split('\n')
            .map(|s| s.split('#').next().unwrap().trim())
            .filter(|s| !s.is_empty())
            .map(|s| {
                let mut parts = s.split(" = ");
                let key = parts.next().unwrap();
                let value = parts.next().unwrap();
                let value =
                    if value == "delete" { None } else { Some(hex::decode(value).unwrap()) };
                (hex::decode(key).unwrap(), value)
            })
            .collect()
    }

    #[test]
    fn test_meta_parse_changes() {
        // Make sure that our test utility itself is fine.
        let changes = parse_changes(
            "
                00ff = 00000001  # comments
                01dd = delete
                # comments
                02ac = 0003
            ",
        );
        assert_eq!(
            changes,
            vec![
                (vec![0x00, 0xff], Some(vec![0x00, 0x00, 0x00, 0x01])),
                (vec![0x01, 0xdd], None),
                (vec![0x02, 0xac], Some(vec![0x00, 0x03])),
            ]
        );
    }

    // As of Oct 2023 this test by itself achieves 100% test coverage for the
    // logic in this file (minus the unreachable cases). If you modify the code
    // or the test, please check code coverage with e.g. tarpaulin.
    #[test]
    fn test_trie_consistency_manual() {
        let mut tries = TestTries::new(true);
        // Simple insertion from empty trie.
        tries.check_consistency_across_all_changes_and_apply(parse_changes(
            "
                00 = 0000
                01 = 0001
                02 = 0002
            ",
        ));
        // Prepare some more complex values.
        tries.check_consistency_across_all_changes_and_apply(parse_changes(
            "
                0000 = 0010  # extends a leaf
                0100 = 0011  # extends another leaf
                03 = 0012  # adds a branch
                0444 = 0013  # adds a branch with a longer leaf
                0500 = 0014  # adds a branch that has a branch underneath
                05100000 = 0015
                05100001 = 0016
                05200000 = 0017
                05200001 = 0018
                05300000 = 0019
                05300001 = 001a
                05400000 = 001b
                05400001 = 001c
                05500000 = 001d
                05501000 = 001e
                05501001 = 001f
            ",
        ));
        // Check insertion and deletion in a variety of cases.
        // Code coverage is used to confirm we have covered all cases.
        tries.check_consistency_across_all_changes_and_apply(parse_changes(
            "
                00 = delete  # turns a branch with value into an extension
                01 = 0027  # modifies the value at a branch
                0100 = delete  # turns a branch with value into a leaf
                03 = delete  # deletes a branch
                0444 = 0020  # overwrites a leaf
                0455 = 0022  # split leaf into branch at start
                0456 = 0023  # split (pending) leaf into branch
                05 = 0021  # turn branch into branch with value
                05110000 = 0024  # split extension node into branch at start
                05201000 = 0025  # split extension node into branch in the middle
                05300010 = 0026  # split extension node into branch at the end
                05400000 = delete  # turn 2-branch node into leaf that squashes with extension
                05500000 = delete  # turn 2-branch node into extension that squashes with another extension
            ",
        ));

        // sanity check here the truth is correct - i.e. our test itself is good.
        let expected_truth = parse_changes(
            "
                00 = delete
                0000 = 0010
                01 = 0027
                0100 = delete
                02 = 0002
                03 = delete
                0444 = 0020
                0455 = 0022
                0456 = 0023
                05 = 0021
                0500 = 0014
                05100000 = 0015
                05100001 = 0016
                05110000 = 0024
                05200000 = 0017
                05200001 = 0018
                05201000 = 0025
                05300000 = 0019
                05300001 = 001a
                05300010 = 0026
                05400000 = delete
                05400001 = 001c
                05500000 = delete
                05501000 = 001e
                05501001 = 001f
            ",
        )
        .into_iter()
        .map(|(k, v)| (k, v.map(|v| ValueRef::new(&v))))
        .collect::<HashMap<_, _>>();
        assert_eq!(
            tries.truth,
            expected_truth,
            "Differing keys: {:?}",
            expected_truth
                .keys()
                .cloned()
                .chain(tries.truth.keys().cloned())
                .collect::<HashSet<_>>()
                .into_iter()
                .filter(|k| { expected_truth.get(k) != tries.truth.get(k) })
                .collect::<Vec<_>>()
        );

        // Delete some non-existent keys.
        tries.check_consistency_across_all_changes_and_apply(parse_changes(
            "
                00 = delete  # non-existent branch
                04 = delete  # branch without value
                0445 = delete  # non-matching leaf
                055011 = delete  # non-matching extension
            ",
        ));

        // Make no changes
        tries.check_consistency_across_all_changes_and_apply(Vec::new());

        // Finally delete all keys.
        tries.check_consistency_across_all_changes_and_apply(parse_changes(
            "
                0000 = delete
                01 = delete
                02 = delete
                03 = delete
                0444 = delete
                0455 = delete
                0456 = delete
                05 = delete
                0500 = delete
                05100000 = delete
                05100001 = delete
                05110000 = delete
                05200000 = delete
                05200001 = delete
                05201000 = delete
                05300000 = delete
                05300001 = delete
                05300010 = delete
                05400001 = delete
                05501000 = delete
                05501001 = delete
            ",
        ));

        // Check a corner case that deleting a non-existent key from
        // an empty trie does not panic.
        tries.check_consistency_across_all_changes_and_apply(parse_changes(
            "
                08 = delete  # non-existent key when whole trie is empty
            ",
        ));

        assert_eq!(tries.state_root, StateRoot::default());
        // Garbage collect all roots we've added. This checks that the refcounts
        // maintained by the in-memory tries are correct, because if any
        // refcounts are too low this would panic, and if any refcounts are too
        // high the number of allocs in the end would be non-zero.
        tries.mem.delete_until_height(1);
        assert_eq!(tries.mem.num_roots(), 0);
        assert_eq!(tries.mem.arena().num_active_allocs(), 0);
    }

    // As of Oct 2023 this randomized test was seen to cover all branches except
    // deletion of keys from empty tries and deleting all keys from the trie.
    #[test]
    fn test_trie_consistency_random() {
        const MAX_KEYS: usize = 100;
        const SLOWDOWN: usize = 5;
        let mut tries = TestTries::new(false);
        for batch in 0..1000 {
            println!("Batch {}:", batch);
            let mut existing_keys = tries.truth.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>();
            // The more keys we have, the less we insert, the more we delete.
            let num_insertions =
                rand::thread_rng().gen_range(0..=(MAX_KEYS - existing_keys.len()) / SLOWDOWN);
            let num_deletions =
                rand::thread_rng().gen_range(0..=(existing_keys.len() + SLOWDOWN - 1) / SLOWDOWN);
            let mut changes = Vec::new();
            for _ in 0..num_insertions {
                let key_length = rand::thread_rng().gen_range(0..=10);
                let existing_key = existing_keys
                    .get(rand::thread_rng().gen_range(0..existing_keys.len().max(1)))
                    .cloned()
                    .unwrap_or_default();
                let reuse_prefix_length = rand::thread_rng().gen_range(0..=existing_key.len());
                let mut key = Vec::<u8>::new();
                for i in 0..key_length {
                    if i < reuse_prefix_length {
                        key.push(existing_key[i]);
                    } else {
                        // Limit nibbles to 4, so that we can generate keys that relate to
                        // each other more frequently.
                        let nibble0 = rand::thread_rng().gen::<u8>() % 4;
                        let nibble1 = rand::thread_rng().gen::<u8>() % 4;
                        key.push(nibble0 << 4 | nibble1);
                    }
                }

                let mut value_length = rand::thread_rng().gen_range(0..=10);
                if value_length == 10 {
                    value_length = 8000; // make a long value that is not inlined
                }
                let mut value = Vec::<u8>::new();
                for _ in 0..value_length {
                    value.push(rand::thread_rng().gen());
                }
                println!(
                    "  {} = {}",
                    hex::encode(&key),
                    if value.len() > 10 {
                        hex::encode(&value[0..10]) + "..."
                    } else {
                        hex::encode(&value)
                    }
                );
                changes.push((key.clone(), Some(value.clone())));
                // Add it to existing keys so that we can insert more keys similar
                // to this as well as delete some of these keys too.
                existing_keys.push(key);
            }
            for _ in 0..num_deletions {
                let key = existing_keys
                    .get(rand::thread_rng().gen_range(0..existing_keys.len()))
                    .cloned()
                    .unwrap_or_default();
                println!("  {} = delete", hex::encode(&key));
                changes.push((key.clone(), None));
            }
            tries.check_consistency_across_all_changes_and_apply(changes);
        }
    }

    fn insert_changes_to_memtrie(
        memtrie: &mut MemTries,
        prev_state_root: CryptoHash,
        block_height: BlockHeight,
        changes: &str,
    ) -> CryptoHash {
        let changes = parse_changes(changes);
        let mut update = memtrie.update(prev_state_root, false).unwrap();

        for (key, value) in changes {
            if let Some(value) = value {
                update.insert_memtrie_only(&key, FlatStateValue::on_disk(&value));
            } else {
                update.delete(&key);
            }
        }

        let changes = update.to_mem_trie_changes_only();
        memtrie.apply_memtrie_changes(block_height, &changes)
    }

    #[test]
    fn test_gc_hybrid_memtrie() {
        let state_root = StateRoot::default();
        let mut memtrie = MemTries::new(ShardUId::single_shard());
        assert!(!memtrie.arena.has_shared_memory());

        // Insert in some initial data for height 0
        let changes = "
            ff00 = 0000
            ff01 = 0100
            ff0101 = 0101
        ";
        let state_root = insert_changes_to_memtrie(&mut memtrie, state_root, 0, changes);

        // Freeze the current memory in memtrie
        let frozen_arena = memtrie.arena.freeze();
        let hybrid_arena =
            HybridArena::from_frozen("test_hybrid".to_string(), frozen_arena.clone());
        memtrie.arena = hybrid_arena;
        assert!(memtrie.arena.has_shared_memory());

        // Insert in some more data for height 1 in hybrid memtrie
        // Try to make sure we share some node allocations (ff01 and ff0101) with height 0
        // Node ff01 effectively has a refcount of 2, one from height 0 and one from height 1

        let changes = "
            ff0000 = 1000
            ff0001 = 1001
        ";
        insert_changes_to_memtrie(&mut memtrie, state_root, 1, changes);

        // Now try to garbage collect the height 0 root
        // Memory consumption should not change as height 0 is frozen
        let num_active_allocs = memtrie.arena.num_active_allocs();
        let active_allocs_bytes = memtrie.arena.active_allocs_bytes();
        memtrie.delete_until_height(1);
        assert_eq!(memtrie.arena.num_active_allocs(), num_active_allocs);
        assert_eq!(memtrie.arena.active_allocs_bytes(), active_allocs_bytes);

        // Now try to garbage collect the height 1 root
        // The final memory allocation should be what we had during the time of freezing
        memtrie.delete_until_height(2);
        assert_eq!(memtrie.arena.num_active_allocs(), frozen_arena.num_active_allocs());
        assert_eq!(memtrie.arena.active_allocs_bytes(), frozen_arena.active_allocs_bytes());
    }
}
