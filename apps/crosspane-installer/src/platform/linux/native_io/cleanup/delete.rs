use super::*;
impl CleanupLease {
    /// True means this call unlinked exactly the admitted owned file. No recursive removal.
    pub fn delete(&self, index: usize, d: &Deadline) -> Result<bool> {
        self.mutate(d, move |state, d, started| {
            if index >= FILES.len() {
                return Err(NativeError::Invalid);
            }
            if state.removed.lock().map_err(|_| NativeError::Unavailable)?[index] {
                return Ok(false);
            }
            {
                let entries = state
                    .proof
                    .0
                    .entries
                    .lock()
                    .map_err(|_| NativeError::Unavailable)?;
                let row = &entries[index];
                if !row.owned
                    || row.snapshot.hash().is_none()
                    || row.snapshot.hash() != Some(row.hash)
                {
                    return Ok(false);
                }
            }
            state.before(d, started)?;
            let entries = state
                .proof
                .0
                .entries
                .lock()
                .map_err(|_| NativeError::Unavailable)?;
            let row = &entries[index];
            let mut nonce = [0u8; 16];
            aws_lc_rs::rand::fill(&mut nonce).map_err(|_| NativeError::Unavailable)?;
            let displaced = format!(
                ".cleanup-del-{}",
                nonce.iter().map(|b| format!("{b:02x}")).collect::<String>()
            );
            let snapshot = &row.snapshot;
            let name = snapshot.path.file_name().ok_or(NativeError::Invalid)?;
            // Full original identity, including ctime, is checked immediately before displacement.
            snapshot.revalidate(&state.proof.0.io, d)?;
            d.check()?;
            #[cfg(test)]
            state.hook(&state.before_delete);
            native(rfs::renameat_with(
                &snapshot.parent,
                name,
                &snapshot.parent,
                &displaced,
                rfs::RenameFlags::NOREPLACE,
            ))?;
            #[cfg(test)]
            state.hook(&state.after_displace);
            let result = (|| {
                let (file, observed) = snapshot.displaced(&state.proof.0.io, &displaced, d)?;
                snapshot.revalidate_removed(&state.proof.0.io, d)?;
                #[cfg(test)]
                state.hook(&state.before_unlink);
                snapshot.stable_displaced(&file, &displaced, observed, d)?;
                // The private nonce check-to-unlink gap trusts same-UID actors, as in WP-4.7b.
                native(rfs::unlinkat(
                    &snapshot.parent,
                    &displaced,
                    AtFlags::empty(),
                ))
            })();
            if result.is_err() {
                #[cfg(test)]
                state.hook(&state.before_restore);
                let _ = snapshot.restore(&state.proof.0.io, &displaced, name);
                return result.map(|_| false);
            }
            drop(entries);
            state.removed.lock().map_err(|_| NativeError::Unavailable)?[index] = true;
            state.before(d, started)?;
            let entries = state
                .proof
                .0
                .entries
                .lock()
                .map_err(|_| NativeError::Unavailable)?;
            state
                .proof
                .0
                .io
                .files
                .apply(FileOperation::ParentSync(&entries[index].snapshot.parent))?;
            Ok(true)
        })
    }
}
