//! Reconstruct liabilities for actual cold-selected work before new admission.
use super::*;

impl SnapshotLimiter {
    /// A previously allocated operation is existing liability. Its charge must
    /// be visible even when current admission capacity is already exhausted.
    /// New jobs continue through reserve_job and cannot exceed either bound.
    pub(crate) fn restore_job(
        &self,
        max_image: u64,
        workspace_reserved: u64,
    ) -> Result<SnapshotPermit> {
        let required = max_image
            .checked_mul(3)
            .and_then(|bytes| bytes.checked_add(64 * 1024 * 1024 + MAX_MANIFEST_BYTES));
        ensure!(
            (1..=MAX_SNAPSHOT_BYTES).contains(&max_image) && required == Some(workspace_reserved),
            "Recovered snapshot liability has an invalid reservation bound"
        );
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let active = state
            .active
            .checked_add(1)
            .context("Recovered operation count overflow")?;
        let reserved = state
            .reserved
            .checked_add(workspace_reserved)
            .context("Recovered retained-byte count overflow")?;
        state.active = active;
        state.reserved = reserved;
        Ok(SnapshotPermit(Arc::new(JobReservation {
            limiter: self.clone(),
            maximum: max_image,
            bytes: workspace_reserved,
            files: Mutex::new(JobFiles::default()),
        })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn existing_actual_liabilities_survive_reduced_capacity_and_block_fresh_admission() {
        let limiter = SnapshotLimiter::new(128 * 1024 * 1024).unwrap();
        let maximum = 1024 * 1024;
        let reservation = maximum * 3 + 65 * 1024 * 1024;
        let fresh = limiter.reserve_job(maximum, reservation).unwrap();
        let recovered = limiter.restore_job(maximum, reservation).unwrap();
        assert_eq!(limiter.stats().active, 2);
        assert_eq!(limiter.stats().reserved_bytes, reservation * 2);
        assert!(limiter.reserve_job(maximum, reservation).is_err());
        drop(fresh);
        assert_eq!(limiter.stats().reserved_bytes, reservation);
        drop(recovered);
        assert_eq!(limiter.stats().reserved_bytes, 0);
    }

    #[test]
    fn recovered_count_over_admission_bound_remains_charged() {
        let limiter = SnapshotLimiter::new(4 * MAX_SNAPSHOT_BYTES).unwrap();
        let maximum = 1;
        let reservation = maximum * 3 + 65 * 1024 * 1024;
        let retained = (0..5)
            .map(|_| limiter.restore_job(maximum, reservation).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(limiter.stats().active, 5);
        assert_eq!(limiter.stats().reserved_bytes, reservation * 5);
        assert!(limiter.reserve_job(maximum, reservation).is_err());
        drop(retained);
        assert_eq!(limiter.stats().active, 0);
        assert_eq!(limiter.stats().reserved_bytes, 0);
    }
}
