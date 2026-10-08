//! Bounded removal observations. Paths, FileIds and XML never construct native authority.
use super::super::{
    native_io::{NativeError, NativeResult, files::ComponentName},
    payload::recovery::FileStamp,
};
use aws_lc_rs::digest::{SHA256, digest};
use serde::{Deserialize, Serialize};
pub(crate) const MAX_ENTRIES: usize = 1024;
pub(crate) const MAX_DEPTH: usize = 32;
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RemovalNodeKind {
    File,
    Directory,
    Reparse,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RemovalNode {
    components: Vec<String>,
    parent: FileStamp,
    identity: FileStamp,
    kind: RemovalNodeKind,
}
impl RemovalNode {
    pub(crate) fn new(
        components: Vec<String>,
        parent: FileStamp,
        identity: FileStamp,
        kind: RemovalNodeKind,
    ) -> NativeResult<Self> {
        let n = Self {
            components,
            parent,
            identity,
            kind,
        };
        n.validate()?;
        Ok(n)
    }
    pub(crate) fn components(&self) -> &[String] {
        &self.components
    }
    pub(crate) fn parent(&self) -> FileStamp {
        self.parent
    }
    pub(crate) fn identity(&self) -> FileStamp {
        self.identity
    }
    pub(crate) fn kind(&self) -> RemovalNodeKind {
        self.kind
    }
    fn validate(&self) -> NativeResult<()> {
        if self.components.is_empty()
            || self.components.len() > MAX_DEPTH
            || !self.identity.valid()
            || !self.parent.valid()
            || self.identity.volume != self.parent.volume
        {
            return Err(NativeError::Invalid);
        }
        for name in &self.components {
            ComponentName::new(name)?;
        }
        Ok(())
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RemovalCopyKind {
    Helper,
    Keeper,
}
impl RemovalCopyKind {
    pub(crate) fn leaf(self) -> &'static str {
        match self {
            Self::Helper => "helper-copy.exe",
            Self::Keeper => "keeper-copy.exe",
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CopyStage {
    Planned,
    PrepareIntent,
    Prepared,
    CreateIntent,
    Created,
    ResumeIntent,
    Ready,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RemovalCopy {
    kind: RemovalCopyKind,
    identity: Option<FileStamp>,
    image: Option<super::super::payload::inventory::PeFacts>,
    pid: Option<u32>,
    creation: Option<u64>,
    inherited_parent_handle: Option<u64>,
    pub(super) stage: CopyStage,
}
impl RemovalCopy {
    pub(crate) fn planned(kind: RemovalCopyKind) -> Self {
        Self {
            kind,
            identity: None,
            image: None,
            pid: None,
            creation: None,
            inherited_parent_handle: None,
            stage: CopyStage::Planned,
        }
    }
    pub(crate) fn kind(&self) -> RemovalCopyKind {
        self.kind
    }
    pub(crate) fn identity(&self) -> Option<FileStamp> {
        self.identity
    }
    pub(crate) fn image(&self) -> Option<&super::super::payload::inventory::PeFacts> {
        self.image.as_ref()
    }
    pub(crate) fn pid(&self) -> Option<u32> {
        self.pid
    }
    pub(crate) fn creation(&self) -> Option<u64> {
        self.creation
    }
    pub(crate) fn inherited_parent_handle(&self) -> Option<u64> {
        self.inherited_parent_handle
    }
    pub(crate) fn stage(&self) -> CopyStage {
        self.stage
    }
    pub(super) fn bind_image(
        &mut self,
        id: FileStamp,
        image: super::super::payload::inventory::PeFacts,
    ) -> NativeResult<()> {
        if self.stage != CopyStage::PrepareIntent
            || self.identity.is_some()
            || !id.valid()
            || !image.valid()
        {
            return Err(NativeError::Foreign);
        }
        self.identity = Some(id);
        self.image = Some(image);
        Ok(())
    }
    pub(super) fn bind_child(&mut self, pid: u32, creation: u64, parent: u64) -> NativeResult<()> {
        if self.stage != CopyStage::CreateIntent
            || self.pid.is_some()
            || pid == 0
            || creation == 0
            || parent < 4
            || parent > usize::MAX as u64
            || parent >= u64::MAX - 15
            || !parent.is_multiple_of(4)
        {
            return Err(NativeError::Foreign);
        }
        self.pid = Some(pid);
        self.creation = Some(creation);
        self.inherited_parent_handle = Some(parent);
        Ok(())
    }
    fn validate(&self) -> NativeResult<()> {
        if self.identity.is_some_and(|id| !id.valid())
            || self.identity.is_some() != self.image.is_some()
            || self.image.as_ref().is_some_and(|i| !i.valid())
            || self.pid == Some(0)
            || self.creation == Some(0)
            || self.pid.is_some() != self.creation.is_some()
            || self.pid.is_some() != self.inherited_parent_handle.is_some()
            || self.inherited_parent_handle.is_some_and(|p| {
                p < 4 || p > usize::MAX as u64 || p >= u64::MAX - 15 || !p.is_multiple_of(4)
            })
            || matches!(
                self.stage,
                CopyStage::Prepared
                    | CopyStage::CreateIntent
                    | CopyStage::Created
                    | CopyStage::ResumeIntent
                    | CopyStage::Ready
            ) && self.identity.is_none()
            || matches!(
                self.stage,
                CopyStage::Created | CopyStage::ResumeIntent | CopyStage::Ready
            ) && self.pid.is_none()
        {
            return Err(NativeError::Invalid);
        }
        Ok(())
    }
    pub(super) fn compatible(&self, new: &Self) -> bool {
        self.kind == new.kind
            && self.identity.is_none_or(|x| new.identity == Some(x))
            && self
                .image
                .as_ref()
                .is_none_or(|x| new.image.as_ref() == Some(x))
            && self.pid.is_none_or(|x| new.pid == Some(x))
            && self.creation.is_none_or(|x| new.creation == Some(x))
            && self
                .inherited_parent_handle
                .is_none_or(|x| new.inherited_parent_handle == Some(x))
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RemovalTask {
    expected_xml: String,
    sha256: [u8; 32],
    observed_xml: Option<String>,
}
impl RemovalTask {
    pub(crate) fn new(expected_xml: String, observed_xml: Option<String>) -> NativeResult<Self> {
        let hash = digest(&SHA256, expected_xml.as_bytes());
        let mut sha256 = [0; 32];
        sha256.copy_from_slice(hash.as_ref());
        let t = Self {
            expected_xml,
            sha256,
            observed_xml,
        };
        t.validate()?;
        Ok(t)
    }
    pub(crate) fn expected_xml(&self) -> &str {
        &self.expected_xml
    }
    pub(crate) fn validate(&self) -> NativeResult<()> {
        if self.expected_xml.is_empty()
            || self.expected_xml.len() > super::super::service::task::MAX_XML_BYTES
        {
            return Err(NativeError::Oversize);
        }
        if self
            .observed_xml
            .as_deref()
            .is_some_and(|xml| xml != self.expected_xml)
        {
            return Err(NativeError::Foreign);
        }
        if digest(&SHA256, self.expected_xml.as_bytes()).as_ref() != self.sha256 {
            return Err(NativeError::Invalid);
        }
        Ok(())
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RemovalInventory {
    root: Option<FileStamp>,
    nodes: Vec<RemovalNode>,
    task: RemovalTask,
    copies: Vec<RemovalCopy>,
}
impl RemovalInventory {
    pub(crate) fn new(
        root: Option<FileStamp>,
        nodes: Vec<RemovalNode>,
        task: RemovalTask,
        copies: Vec<RemovalCopy>,
    ) -> NativeResult<Self> {
        let i = Self {
            root,
            nodes,
            task,
            copies,
        };
        i.validate()?;
        Ok(i)
    }
    pub(crate) fn root(&self) -> Option<FileStamp> {
        self.root
    }
    pub(crate) fn nodes(&self) -> &[RemovalNode] {
        &self.nodes
    }
    pub(crate) fn task(&self) -> &RemovalTask {
        &self.task
    }
    pub(crate) fn copies(&self) -> &[RemovalCopy] {
        &self.copies
    }
    pub(super) fn copies_mut(&mut self) -> &mut [RemovalCopy] {
        &mut self.copies
    }
    pub(crate) fn validate(&self) -> NativeResult<()> {
        self.task.validate()?;
        if self.nodes.len() > MAX_ENTRIES
            || self.copies.len() > 2
            || self.root.is_some_and(|r| !r.valid())
            || self.root.is_none() && !self.nodes.is_empty()
        {
            return Err(NativeError::Invalid);
        }
        for (index, node) in self.nodes.iter().enumerate() {
            node.validate()?;
            let path = node
                .components
                .iter()
                .map(|c| c.to_lowercase())
                .collect::<Vec<_>>();
            if self.nodes[..index].iter().any(|old| {
                old.components
                    .iter()
                    .map(|c| c.to_lowercase())
                    .collect::<Vec<_>>()
                    == path
            }) {
                return Err(NativeError::Invalid);
            }
            let expected = if node.components.len() == 1 {
                self.root
            } else {
                self.nodes
                    .iter()
                    .find(|parent| {
                        parent.components == node.components[..node.components.len() - 1]
                            && parent.kind == RemovalNodeKind::Directory
                    })
                    .map(|p| p.identity)
            };
            if expected != Some(node.parent) {
                return Err(NativeError::Foreign);
            }
        }
        for (index, copy) in self.copies.iter().enumerate() {
            copy.validate()?;
            if self.copies[..index].iter().any(|old| {
                old.kind == copy.kind || old.identity.is_some() && old.identity == copy.identity
            }) {
                return Err(NativeError::Invalid);
            }
        }
        Ok(())
    }
}

/// Observe only the four fixed first-install roles and their own operation stage/backup leaves.
/// An exact renewed FileId is still required by the native effect adapter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PartialFirstLocation {
    Fixed,
    Stage,
    Backup,
}
