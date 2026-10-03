//! Bounded public launchd observations and immutable plans; unknown syntax stays unobservable.
use super::*;
impl MacLaunchAgent {
    pub fn admit(
        io: Arc<MacNativeIo>,
        inventory: ApprovedInventory,
        approval: Arc<dyn ApprovalProbe>,
        deadline: &Deadline,
    ) -> NativeResult<Self> {
        let payload = MacPayload::admit(io.clone(), inventory.clone(), deadline)?;
        let rule = inventory
            .files
            .iter()
            .find(|f| f.path == "Crosspane.app/Contents/MacOS/Crosspane")
            .and_then(|f| f.signing.as_ref())
            .ok_or(NativeError::Invalid)?;
        let requirement = SigningRequirement {
            role: ArtifactRole::Agent,
            identifier: rule.identifier.clone(),
            designated_requirement: rule.designated_requirement.clone(),
            entitlements: rule.entitlements.clone(),
        };
        let main = io.admit_main_signature(
            &io.target()
                .paths()
                .payload_root
                .join("Crosspane.app/Contents/MacOS/Crosspane"),
            &requirement,
            deadline,
        )?;
        let support = io.admit_support(&main, deadline)?;
        let xml = render_plist(io.target())?;
        Ok(Self {
            io,
            payload,
            requirement,
            support,
            approval,
            owner: Arc::new(()),
            last: (0, 0),
            version: inventory.product_version.clone(),
            inventory,
            xml,
        })
    }
    pub(super) fn command(
        io: &MacNativeIo,
        action: NativeOperation,
        proof: Option<&SupportProof>,
        deadline: &Deadline,
    ) -> NativeResult<CommandOutput> {
        io.execute(&CommandSpec::new(io.target(), action)?, proof, deadline)
    }
    pub(super) fn snapshot(&self, io: &MacNativeIo, deadline: &Deadline) -> NativeResult<Snapshot> {
        let path = Self::plist(io);
        let identity = io.metadata(&path)?;
        let bytes = if let Some(id) = &identity {
            id.regular(io.target().paths().uid, false)?;
            io.read(&path, LIMIT, false, deadline)?
        } else {
            vec![]
        };
        let job = query_job(
            io,
            &Self::command(
                io,
                NativeOperation::Launchctl(LaunchctlAction::Print),
                None,
                deadline,
            )?,
        )?;
        let disabled = query_disabled(&Self::command(
            io,
            NativeOperation::Launchctl(LaunchctlAction::PrintDisabled),
            None,
            deadline,
        )?)?;
        if io.metadata(&path)? != identity {
            return Err(NativeError::Foreign);
        }
        Ok(Snapshot {
            identity,
            bytes,
            job,
            disabled,
        })
    }
    pub fn plan(
        &mut self,
        revision: u64,
        operation: u64,
        current: Option<(&SelectedAgent, &AgentReply)>,
        deadline: &Deadline,
    ) -> NativeResult<LaunchPlan> {
        if self.last.0 == u64::MAX || self.last.1 == u64::MAX {
            return Err(NativeError::IdExhausted);
        }
        if revision <= self.last.0 || operation <= self.last.1 {
            return Err(NativeError::Invalid);
        }
        self.support.check(&self.io, deadline)?;
        self.preflight(deadline)?;
        let snapshot = self.snapshot(&self.io, deadline)?;
        let session = self.io.support_observation(deadline)?.gui.console_session;
        let path = self.io.target().runtime_dir().join("bootstrap.json");
        let baseline = if self.io.metadata(&path)?.is_some() {
            Some(
                parse_bootstrap(&self.io.read(&path, 4096, true, deadline)?)
                    .map_err(|_| NativeError::Invalid)?
                    .instance_id,
            )
        } else {
            None
        };
        let mut original = None;
        let mut selected = None;
        if let Job::Running(pid) = snapshot.job {
            let (agent, reply) = current.ok_or(NativeError::Unavailable)?;
            checked_reply(&self.io, agent, reply, deadline)?;
            if agent.instance.process().pid != pid {
                return Err(NativeError::Foreign);
            }
            let DecodedReply::Status(StatusAdmission::Supported(health)) = reply
                .result
                .as_ref()
                .map_err(|_| NativeError::Unavailable)?
            else {
                return Err(NativeError::Unavailable);
            };
            original = Some(Arc::new(OriginalAgent::capture(
                agent.clone(),
                health,
                deadline,
            )?));
            selected = Some(agent.clone());
        }
        let owned = self.owned(&snapshot, deadline)?;
        let state = if snapshot.disabled == Disabled::Yes {
            LaunchState::UserDisabled
        } else if snapshot.disabled == Disabled::Unknown || snapshot.job == Job::Unknown {
            LaunchState::Unobservable
        } else if snapshot.identity.is_none() {
            LaunchState::Absent
        } else if owned {
            LaunchState::Owned
        } else {
            LaunchState::AdoptionRequired
        };
        let payload = self
            .payload
            .plan(revision, operation, original.clone(), deadline)?;
        self.last = (revision, operation);
        Ok(LaunchPlan {
            owner: self.owner.clone(),
            binding: Arc::new(()),
            revision,
            operation,
            state,
            snapshot,
            original,
            selected,
            matching: payload.state() == PayloadState::Matching,
            payload: Some(payload),
            session,
            baseline,
        })
    }
    fn owned(&self, snapshot: &Snapshot, deadline: &Deadline) -> NativeResult<bool> {
        let path = Self::record(&self.io);
        if snapshot.bytes != self.xml || self.io.metadata(&path)?.is_none() {
            return Ok(false);
        }
        let record: Record =
            serde_json::from_slice(&self.io.read(&path, 512 * 1024, true, deadline)?)
                .map_err(|_| NativeError::Invalid)?;
        let receipt = record.receipt;
        Ok(receipt.schema_version == 1
            && receipt.operation_id.0 != 0
            && receipt.product_version == self.version
            && receipt.manifest_sha256 == self.payload.manifest_sha256()
            && receipt.payload_sha256 == digest(&self.xml)
            && receipt.resources.len() == 1
            && receipt.resources[0].resource_id == "mac.launch-agent"
            && receipt.resources[0].resolved_path == Self::plist(&self.io).to_string_lossy()
            && matches!(
                receipt.resources[0].ownership,
                ResourceOwnership::Created | ResourceOwnership::Adopted
            )
            && receipt.resources[0].after == ResourceObservation::Matching)
    }
}
pub(super) fn checked_reply(
    io: &MacNativeIo,
    selected: &SelectedAgent,
    reply: &AgentReply,
    deadline: &Deadline,
) -> NativeResult<()> {
    let now = selected.io.clock().now_ms();
    if selected.io.target().paths() != io.target().paths()
        || reply.source != selected.io.target().source()
        || reply.observed_at_ms > now
        || now - reply.observed_at_ms > SUPPORT_LIFETIME_MS
    {
        return Err(NativeError::Foreign);
    }
    selected
        .instance
        .revalidate(&selected.io, &selected.support, deadline)?;
    let DecodedReply::Status(StatusAdmission::Supported(health)) = reply
        .result
        .as_ref()
        .map_err(|_| NativeError::Unavailable)?
    else {
        return Err(NativeError::Unavailable);
    };
    selected.instance.admit_status(&health.installer().instance)
}
fn output_text(output: &CommandOutput) -> NativeResult<&str> {
    if output.stdout.len() + output.stderr.len() > LIMIT {
        return Err(NativeError::Oversize);
    }
    std::str::from_utf8(&output.stdout).map_err(|_| NativeError::Invalid)
}
fn query_job(io: &MacNativeIo, output: &CommandOutput) -> NativeResult<Job> {
    let text = output_text(output)?;
    let missing = format!(
        "Could not find service \"{AGENT_LABEL}\" in domain for user gui: {}\n",
        io.target().paths().uid
    );
    if output.code == Some(113) && text.is_empty() && output.stderr == missing.as_bytes() {
        return Ok(Job::Absent);
    }
    if output.code != Some(0) || !output.stderr.is_empty() {
        return Ok(Job::Unknown);
    }
    let header = format!("gui/{}/{AGENT_LABEL} = {{", io.target().paths().uid);
    if text.lines().next().map(str::trim) != Some(header.as_str())
        || text.lines().last().map(str::trim) != Some("}")
    {
        return Ok(Job::Unknown);
    }
    let mut fields = std::collections::BTreeMap::new();
    let mut depth = 1usize;
    for line in text.lines().skip(1).map(str::trim) {
        let pair = line.split_once(" = ");
        if let Some((key, value)) = pair
            && depth == 1
            && matches!(key, "path" | "program" | "pid")
        {
            if fields.insert(key, value).is_some() {
                return Ok(Job::Unknown);
            }
            if value != "{" {
                continue;
            }
        }
        if line == "}" {
            let Some(next) = depth.checked_sub(1) else {
                return Ok(Job::Unknown);
            };
            depth = next;
        } else if let Some((key, "{")) = pair {
            if depth == 0
                || depth == 32
                || key.is_empty()
                || !key
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b" ._-".contains(&b))
            {
                return Ok(Job::Unknown);
            }
            depth += 1;
        } else if depth == 0 || line.contains(['{', '}']) {
            return Ok(Job::Unknown);
        }
    }
    if depth != 0 {
        return Ok(Job::Unknown);
    }
    let field = |name: &str| fields.get(name).copied();
    if field("path") != MacLaunchAgent::plist(io).to_str()
        || field("program") != io.target().agent_path().to_str()
    {
        return Ok(Job::Unknown);
    }
    Ok(field("pid")
        .and_then(|p| p.parse::<u32>().ok())
        .filter(|p| *p != 0)
        .map(Job::Running)
        .unwrap_or(Job::Unknown))
}
fn query_disabled(output: &CommandOutput) -> NativeResult<Disabled> {
    let text = output_text(output)?;
    if output.code != Some(0)
        || !output.stderr.is_empty()
        || text.lines().next().map(str::trim) != Some("disabled services = {")
        || text.lines().last().map(str::trim) != Some("}")
    {
        return Ok(Disabled::Unknown);
    }
    let mut found = None;
    for line in text
        .lines()
        .skip(1)
        .take(text.lines().count().saturating_sub(2))
    {
        let Some((key, value)) = line.trim().split_once(" => ") else {
            return Ok(Disabled::Unknown);
        };
        let Some(key) = key.strip_prefix('"').and_then(|s| s.strip_suffix('"')) else {
            return Ok(Disabled::Unknown);
        };
        if key.is_empty()
            || !key
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        {
            return Ok(Disabled::Unknown);
        }
        let value = match value {
            "true" => Disabled::Yes,
            "false" => Disabled::No,
            _ => return Ok(Disabled::Unknown),
        };
        if key == AGENT_LABEL && found.replace(value).is_some() {
            return Ok(Disabled::Unknown);
        }
    }
    Ok(found.unwrap_or(Disabled::No))
}
