use super::*;

impl MacNativeIo {
    pub fn support_observation(&self, deadline: &Deadline) -> NativeResult<SupportObservation> {
        let (probe, limit) = (self.support.clone(), deadline.clone());
        let mut observation = bounded_result(&PROBES, deadline, move || {
            limit.check()?;
            probe.observe(&limit)
        })?;
        observation.gui_tmpdir = admitted_spelling(&observation.gui_tmpdir)?;
        Ok(observation)
    }
    /// Independently admits the fixed main identity. The manifest cannot select a trusted Team.
    pub fn admit_main_signature(
        &self,
        path: &Path,
        approved: &SigningRequirement,
        deadline: &Deadline,
    ) -> NativeResult<SignatureProof> {
        let path = admitted_spelling(path)?;
        let stage = self
            .target
            .paths
            .home
            .join("Applications/.Crosspane.app.crosspane-stage/Contents/MacOS/Crosspane");
        let incoming = self
            .target
            .paths
            .payload_root
            .join("Crosspane.app/Contents/MacOS/Crosspane");
        if approved.role != ArtifactRole::Agent
            || (path != self.target.agent_path() && path != stage && path != incoming)
        {
            return Err(NativeError::Foreign);
        }
        self.signature(&path, approved, deadline)
    }
    pub fn admit_artifact_signature(
        &self,
        path: &Path,
        approved: &SigningRequirement,
        main: &SignatureProof,
        deadline: &Deadline,
    ) -> NativeResult<SignatureProof> {
        if main.requirement.role != ArtifactRole::Agent {
            return Err(NativeError::Foreign);
        }
        main.revalidate(self)?;
        let proof = self.signature(&admitted_spelling(path)?, approved, deadline)?;
        if proof.observation.team_identifier != main.observation.team_identifier {
            return Err(NativeError::Unsupported);
        }
        main.revalidate(self)?;
        Ok(proof)
    }
    fn signature(
        &self,
        path: &Path,
        approved: &SigningRequirement,
        deadline: &Deadline,
    ) -> NativeResult<SignatureProof> {
        deadline.check()?;
        let identity = self.metadata(path)?.ok_or(NativeError::Unavailable)?;
        identity.regular(self.target.paths.uid, false)?;
        if identity.mode & 0o111 == 0 {
            return Err(NativeError::Foreign);
        }
        let (probe, path_owned, expected, limit) = (
            self.signatures.clone(),
            path.to_owned(),
            approved.clone(),
            deadline.clone(),
        );
        let observation = bounded_result(&PROBES, deadline, move || {
            limit.check()?;
            probe.observe(&path_owned, &expected, &limit)
        })?;
        observation.admit(approved)?;
        if self.metadata(path)? != Some(identity.clone()) {
            return Err(NativeError::Foreign);
        }
        Ok(SignatureProof {
            nonce: self.target.nonce,
            path: path.to_owned(),
            identity,
            requirement: approved.clone(),
            observation,
        })
    }
    pub fn admit_support(
        &self,
        main: &SignatureProof,
        deadline: &Deadline,
    ) -> NativeResult<SupportProof> {
        if main.nonce != self.target.nonce || main.requirement.role != ArtifactRole::Agent {
            return Err(NativeError::Unsupported);
        }
        main.revalidate(self)?;
        let facts = self.support_observation(deadline)?;
        let gui = &facts.gui;
        if facts.macos_major < 26
            || !facts.apple_silicon
            || !gui.active
            || gui.console_uid != Some(self.target.paths.uid)
            || gui.interactive_uid != Some(self.target.paths.uid)
            || !bounded(&gui.console_session, 64)
            || gui.console_session != gui.interactive_session
            || admitted_spelling(&facts.gui_tmpdir)? != self.target.paths.gui_tmpdir
        {
            return Err(NativeError::Unsupported);
        }
        self.validate_target()?;
        Ok(SupportProof {
            nonce: self.target.nonce,
            issued: self.clock.now_ms(),
            wall: Instant::now(),
            facts,
            signing: main.clone(),
            valid: Arc::new(AtomicBool::new(true)),
        })
    }
    pub fn execute(
        &self,
        spec: &CommandSpec,
        proof: Option<&SupportProof>,
        deadline: &Deadline,
    ) -> NativeResult<CommandOutput> {
        deadline.check()?;
        self.validate_target()?;
        if spec.nonce != self.target.nonce {
            return Err(NativeError::Foreign);
        }
        if spec.mutation {
            proof
                .ok_or(NativeError::Unsupported)?
                .check(self, deadline)?;
        }
        if let Some(signature) = &spec.child_signature {
            signature.revalidate(self)?;
        }
        let (runner, mut command, limit) = (self.runner.clone(), spec.clone(), deadline.clone());
        command.authorized = Some(Instant::now());
        let started = Arc::new(AtomicBool::new(false));
        let dispatched = started.clone();
        let target = self.target.clone();
        let result = bounded_result(&COMMANDS, deadline, move || {
            // Publish dispatch possibility BEFORE checking the deadline: a caller can only
            // report a plain timeout while this worker is still guaranteed to abort before I/O.
            dispatched.store(true, Ordering::Release);
            limit.check()?;
            target.observe("dispatch", command.program(), None)?;
            limit.check()?;
            runner.run(&command, &limit)
        });
        let result = match result {
            Ok(output) if output.stdout.len() + output.stderr.len() <= spec.max_output => {
                Ok(output)
            }
            Ok(_) => Err(NativeError::Oversize),
            Err(error) => Err(error),
        };
        if spec.mutation
            && started.load(Ordering::Acquire)
            && matches!(
                result,
                Err(NativeError::Timeout
                    | NativeError::Cancelled
                    | NativeError::Unavailable
                    | NativeError::Oversize)
            )
        {
            return Err(NativeError::OutcomeUnknown);
        }
        result
    }
}
