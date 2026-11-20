//! Keep small tables in memory and pack larger tables into private record stages.

use kio_core::disk_records::{PayloadRef, PayloadWriter, RecordPayloads, RecordStageRoot};
use serde::{Serialize, de::DeserializeOwned};

use super::{Budget, invalid};
use crate::{AdapterError, Result};

const PAGE_BYTES: usize = 256 * 1024;
const PAGE_VALUES: usize = 256;

struct Page {
    first: usize,
    reference: PayloadRef,
    resident: usize,
}

pub(super) struct ValuesWriter<T> {
    pending: Vec<T>,
    pending_bytes: usize,
    pages: Vec<Page>,
    writer: Option<PayloadWriter<Vec<T>>>,
    count: usize,
    root: Option<RecordStageRoot>,
}

impl<T> Default for ValuesWriter<T> {
    fn default() -> Self {
        Self {
            pending: Vec::new(),
            pending_bytes: 0,
            pages: Vec::new(),
            writer: None,
            count: 0,
            root: None,
        }
    }
}

impl<T: Serialize> ValuesWriter<T> {
    pub fn new(root: Option<&RecordStageRoot>) -> Self {
        Self {
            root: root.cloned(),
            ..Self::default()
        }
    }

    pub fn push(&mut self, value: T, charge: usize, budget: &mut Budget) -> Result<()> {
        self.pending.push(value);
        self.pending_bytes += charge;
        self.count += 1;
        if self.pending_bytes >= PAGE_BYTES || self.pending.len() >= PAGE_VALUES {
            self.spill(budget)?;
        }
        Ok(())
    }

    fn spill(&mut self, budget: &mut Budget) -> Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        if self.writer.is_none() {
            if self.root.is_none() {
                self.root = Some(RecordStageRoot::temporary().map_err(stage_error)?);
            }
            self.writer = Some(
                PayloadWriter::new(self.root.as_ref().expect("record stage root"))
                    .map_err(stage_error)?,
            );
        }
        let reference = self
            .writer
            .as_mut()
            .expect("payload writer")
            .push_accounted(&self.pending, |bytes| {
                budget.check_memory(bytes * 2)?;
                budget.expand(bytes)
            })
            .map_err(|error| match error {
                kio_core::disk_records::PayloadWriteError::Admission(error) => error,
                kio_core::disk_records::PayloadWriteError::Storage(error) => stage_error(error),
            })?;
        budget.retain(PAGE_CHARGE)?;
        self.pages.push(Page {
            first: self.count - self.pending.len(),
            reference,
            resident: self.pending_bytes,
        });
        self.pending = Vec::new();
        budget.release(std::mem::take(&mut self.pending_bytes));
        Ok(())
    }

    pub fn finish(&mut self, budget: &mut Budget) -> Result<Values<T>> {
        if self.writer.is_some() {
            self.spill(budget)?;
        }
        let payloads = self
            .writer
            .take()
            .map(PayloadWriter::finish)
            .transpose()
            .map_err(stage_error)?;
        let resident = self.pending_bytes + self.pages.len() * PAGE_CHARGE;
        self.pending_bytes = 0;
        Ok(Values {
            resident,
            count: std::mem::take(&mut self.count),
            memory: std::mem::take(&mut self.pending),
            pages: std::mem::take(&mut self.pages),
            payloads,
        })
    }
}

const PAGE_CHARGE: usize = 2 * size_of::<Page>();

pub(super) struct Values<T> {
    resident: usize,
    count: usize,
    memory: Vec<T>,
    pages: Vec<Page>,
    payloads: Option<RecordPayloads<Vec<T>>>,
}

impl<T> Default for Values<T> {
    fn default() -> Self {
        Self {
            resident: 0,
            count: 0,
            memory: Vec::new(),
            pages: Vec::new(),
            payloads: None,
        }
    }
}

impl<T: DeserializeOwned> Values<T> {
    pub fn resident(&self) -> usize {
        self.resident
    }

    pub fn lookup(&self) -> Lookup<'_, T> {
        Lookup {
            values: self,
            cached: None,
        }
    }

    pub fn for_each(
        &self,
        budget: &mut Budget,
        mut visit: impl FnMut(&T, &mut Budget) -> Result<()>,
    ) -> Result<()> {
        for value in &self.memory {
            visit(value, budget)?;
        }
        for page in &self.pages {
            self.read(page, budget, |values, budget| {
                for value in values {
                    visit(value, budget)?;
                }
                Ok(())
            })?;
        }
        Ok(())
    }

    fn read<U>(
        &self,
        page: &Page,
        budget: &mut Budget,
        visit: impl FnOnce(&[T], &mut Budget) -> Result<U>,
    ) -> Result<U> {
        let bytes = page.reference.serialized_size();
        let scratch = bytes + page.resident;
        budget.expand(bytes)?;
        budget.retain(scratch)?;
        let values = self
            .payloads
            .as_ref()
            .expect("sealed payloads")
            .read(page.reference)
            .map_err(stage_error)?;
        let result = visit(&values, budget);
        drop(values);
        budget.release(scratch);
        result
    }
}

/// One independently verified page is retained for nearby lookups.
pub(super) struct Lookup<'a, T> {
    values: &'a Values<T>,
    cached: Option<(usize, Vec<T>, usize)>,
}

impl<T: DeserializeOwned> Lookup<'_, T> {
    pub fn get<U>(
        &mut self,
        index: usize,
        budget: &mut Budget,
        visit: impl FnOnce(&T, &mut Budget) -> Result<U>,
    ) -> Result<U> {
        if index >= self.values.count {
            return Err(invalid("cell references an absent shared string"));
        }
        let Some(payloads) = &self.values.payloads else {
            return visit(&self.values.memory[index], budget);
        };
        let page = &self.values.pages[self
            .values
            .pages
            .partition_point(|page| page.first <= index)
            - 1];
        if self
            .cached
            .as_ref()
            .is_none_or(|(first, _, _)| *first != page.first)
        {
            if let Some((_, values, charge)) = self.cached.take() {
                drop(values);
                budget.release(charge);
            }
            let bytes = page.reference.serialized_size();
            budget.expand(bytes)?;
            budget.retain(page.resident + bytes)?;
            let values = payloads.read(page.reference).map_err(stage_error)?;
            budget.release(bytes);
            self.cached = Some((page.first, values, page.resident));
        }
        let (first, values, _) = self.cached.as_ref().expect("verified payload page");
        visit(&values[index - first], budget)
    }
}

fn stage_error(error: kio_core::KioError) -> AdapterError {
    match error.resource_limit() {
        Some(limit) => AdapterError::ResourceLimit(limit.clone()),
        None => AdapterError::RecordStage(error),
    }
}
