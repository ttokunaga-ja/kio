use std::cell::Cell;

use kio_core::error::{Resource, ResourceLimit, ResourceTier};

use crate::{AdapterError, Result};

const MIB: u64 = 1024 * 1024;

/// Accounted resources of a complete, verified worksheet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SheetUsage {
    pub output_bytes: u64,
    visits: u64,
    expanded: u64,
    peak_bytes: u64,
}

#[derive(Debug)]
pub(super) struct Budget {
    pub tier: ResourceTier,
    pub memory_limit: u64,
    pub work_limit: u64,
    pub unit_limit: u64,
    retained: u64,
    xml_scratch: usize,
    peak: Cell<u64>,
    visits: u64,
    expanded: u64,
}

#[cfg(test)]
impl SheetUsage {
    pub fn peak_bytes(self) -> u64 {
        self.peak_bytes
    }
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
            xml_scratch: 0,
            peak: Cell::new(0),
            visits: 0,
            expanded: 0,
        }
    }

    pub fn retained(&self) -> usize {
        self.retained as usize
    }

    pub fn begin_sheet(&self) -> (u64, u64, u64) {
        self.peak.set(self.retained);
        (self.visits, self.expanded, self.retained)
    }

    pub fn sheet_usage(&self, before: (u64, u64, u64), output_bytes: usize) -> SheetUsage {
        SheetUsage {
            output_bytes: output_bytes as u64,
            visits: self.visits - before.0,
            expanded: self.expanded - before.1,
            peak_bytes: self.peak.get() - before.2,
        }
    }

    pub fn replay(&mut self, usage: SheetUsage) -> Result<()> {
        self.check_unit(Some(usage.output_bytes))?;
        self.check(
            Resource::Memory,
            self.memory_limit,
            self.retained.checked_add(usage.peak_bytes),
        )?;
        self.work(usage.visits, usage.expanded)
    }

    pub fn available_memory(&self) -> usize {
        (self.memory_limit - self.retained) as usize
    }

    pub fn set_xml_scratch(&mut self, bytes: usize) -> Result<()> {
        self.xml_scratch = bytes;
        self.check_memory(0)
    }

    pub fn memory_exhausted(&self) -> AdapterError {
        limit_error(
            self.tier,
            Resource::Memory,
            self.memory_limit,
            Some(self.memory_limit + 1),
        )
    }

    pub fn check_memory(&self, bytes: usize) -> Result<()> {
        let observed = self.check(
            Resource::Memory,
            self.memory_limit,
            self.retained
                .checked_add(self.xml_scratch as u64)
                .and_then(|used| used.checked_add(bytes as u64)),
        )?;
        self.peak.set(self.peak.get().max(observed));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn review_replayed_sheet_work_counts_toward_the_whole_workbook() {
        let costs = |budget: &mut Budget| {
            budget.visit().unwrap();
            budget.expand(1025).unwrap();
        };
        let mut first_run = Budget::new(ResourceTier::Standard);
        let checkpoint = first_run.begin_sheet();
        costs(&mut first_run);
        let first = first_run.sheet_usage(checkpoint, 1);

        let mut resumed = Budget::new(ResourceTier::Standard);
        resumed.work_limit = 4;
        resumed.replay(first).unwrap();
        resumed.visit().unwrap();
        assert!(
            matches!(resumed.expand(1024), Err(AdapterError::ResourceLimit(limit))
            if limit.resource == Resource::Work && limit.observed == Some(5))
        );

        let mut larger = Budget::new(ResourceTier::Large);
        larger.work_limit = 5;
        larger.replay(first).unwrap();
        larger.visit().unwrap();
        larger.expand(1024).unwrap();
    }
}
