//! Immutable postorder plan for one actually observed fixed install root.
use super::{
    super::native_io::{NativeError, NativeResult},
    super::payload::recovery::FileStamp,
    inventory::{RemovalCopy, RemovalInventory, RemovalNode, RemovalTask},
};
use serde::{Deserialize, Serialize};
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RemovalPlan {
    inventory: RemovalInventory,
    order: Vec<u16>,
}
impl RemovalPlan {
    pub(crate) fn new(inventory: RemovalInventory) -> NativeResult<Self> {
        inventory.validate()?;
        let mut order = (0..inventory.nodes().len())
            .map(|i| i as u16)
            .collect::<Vec<_>>();
        order.sort_by(|a, b| {
            let a = &inventory.nodes()[usize::from(*a)];
            let b = &inventory.nodes()[usize::from(*b)];
            b.components()
                .len()
                .cmp(&a.components().len())
                .then_with(|| a.components().cmp(b.components()))
        });
        let p = Self { inventory, order };
        p.validate()?;
        Ok(p)
    }
    pub(crate) fn root(&self) -> Option<FileStamp> {
        self.inventory.root()
    }
    pub(crate) fn node(&self, index: u16) -> NativeResult<&RemovalNode> {
        self.inventory
            .nodes()
            .get(usize::from(index))
            .ok_or(NativeError::Invalid)
    }
    pub(crate) fn order(&self) -> &[u16] {
        &self.order
    }
    pub(crate) fn task(&self) -> &RemovalTask {
        self.inventory.task()
    }
    pub(crate) fn copies(&self) -> &[RemovalCopy] {
        self.inventory.copies()
    }
    pub(super) fn copy_mut(&mut self, index: u8) -> NativeResult<&mut RemovalCopy> {
        self.inventory
            .copies_mut()
            .get_mut(usize::from(index))
            .ok_or(NativeError::Invalid)
    }
    pub(crate) fn same_files(&self, new: &Self) -> NativeResult<()> {
        self.validate()?;
        new.validate()?;
        if self.root() != new.root()
            || self.inventory.nodes() != new.inventory.nodes()
            || self.task() != new.task()
            || self.order != new.order
            || self.copies().len() != new.copies().len()
            || self
                .copies()
                .iter()
                .zip(new.copies())
                .any(|(old, new)| !old.compatible(new))
        {
            return Err(NativeError::Foreign);
        }
        Ok(())
    }
    pub(crate) fn validate(&self) -> NativeResult<()> {
        self.inventory.validate()?;
        if self.order.len() != self.inventory.nodes().len() {
            return Err(NativeError::Invalid);
        }
        for (position, index) in self.order.iter().enumerate() {
            let node = self.node(*index)?;
            if self.order[..position].contains(index) {
                return Err(NativeError::Invalid);
            }
            if self.order[..position].iter().any(|earlier| {
                self.node(*earlier)
                    .is_ok_and(|e| e.components().len() < node.components().len())
            }) {
                return Err(NativeError::Invalid);
            }
        }
        Ok(())
    }
}
