use kio_core::error::{Resource, ResourceLimit, ResourceTier};

use crate::{AdapterError, Result};

const MIB: u64 = 1024 * 1024;

#[derive(Debug)]
pub(super) struct Budget {
    pub tier: ResourceTier,
    pub memory_limit: u64,
    pub work_limit: u64,
    pub unit_limit: u64,
    retained: u64,
    visits: u64,
    expanded: u64,
}

impl Budget {
    pub fn new(tier: ResourceTier) -> Self {
        let (memory_limit, work_limit, unit_limit) = match tier {
            ResourceTier::Standard => (256 * MIB, 10_000_000, 16 * MIB),
            ResourceTier::Large => (1024 * MIB, 100_000_000, 64 * MIB),
        };
        Self {
            tier,
            memory_limit,
            work_limit,
            unit_limit,
            retained: 0,
            visits: 0,
            expanded: 0,
        }
    }

    pub fn retained(&self) -> usize {
        self.retained as usize
    }

    pub fn check_memory(&self, bytes: usize) -> Result<()> {
        self.check(
            Resource::Memory,
            self.memory_limit,
            self.retained.checked_add(bytes as u64),
        )?;
        Ok(())
    }

    pub fn retain(&mut self, bytes: usize) -> Result<()> {
        self.check_memory(bytes)?;
        self.retained += bytes as u64;
        Ok(())
    }

    pub fn release(&mut self, bytes: usize) {
        self.retained -= bytes as u64;
    }

    pub fn visit(&mut self) -> Result<()> {
        self.work(1, 0)
    }

    pub fn expand(&mut self, bytes: usize) -> Result<()> {
        self.work(0, bytes as u64)
    }

    fn work(&mut self, visits: u64, bytes: u64) -> Result<()> {
        let next_visits = self.visits.checked_add(visits);
        let next_bytes = self.expanded.checked_add(bytes);
        let total = next_visits
            .zip(next_bytes)
            .and_then(|(visits, bytes)| visits.checked_add(bytes.div_ceil(1024)));
        self.check(Resource::Work, self.work_limit, total)?;
        self.visits += visits;
        self.expanded += bytes;
        Ok(())
    }

    pub fn check_unit(&self, bytes: Option<u64>) -> Result<()> {
        self.check(Resource::UnitText, self.unit_limit, bytes)?;
        Ok(())
    }

    fn check(&self, resource: Resource, limit: u64, observed: Option<u64>) -> Result<u64> {
        observed
            .filter(|&value| value <= limit)
            .ok_or_else(|| limit_error(self.tier, resource, limit, observed))
    }
}

fn limit_error(
    tier: ResourceTier,
    resource: Resource,
    limit: u64,
    observed: Option<u64>,
) -> AdapterError {
    AdapterError::ResourceLimit(ResourceLimit {
        resource,
        stage: "xlsx_extract".to_owned(),
        limit: Some(limit),
        observed,
        resource_tier: Some(tier),
    })
}
