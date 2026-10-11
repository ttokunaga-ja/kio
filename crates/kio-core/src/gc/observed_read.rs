//! Descriptor-bound observed reads and their existing GC accounting.

use std::{collections::BTreeMap, io::Read, path::Path};

use cap_primitives::fs as cap_fs;

use crate::{cas::hash_bytes, error::Result};

use super::{
    FileObservation, GcPlanLimits, GcPlanStats, account, corrupt, file_state, id_meta,
    insert_observation, ioerr, is_io_not_found, limit, same_file_state, valid_file,
};

pub(crate) fn read_regular_observed(
    d: &std::fs::File,
    n: &str,
    max: u64,
) -> Result<(Vec<u8>, FileObservation)> {
    let p = Path::new(n);
    let before = cap_fs::stat(d, p, cap_fs::FollowSymlinks::No).map_err(|e| ioerr(e, n))?;
    valid_file(&before, max)?;
    let mut o = cap_fs::OpenOptions::new();
    o.read(true);
    o._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
    let mut f = cap_fs::open(d, p, &o).map_err(|e| ioerr(e, n))?;
    let opened = cap_fs::Metadata::from_file(&f).map_err(|e| ioerr(e, n))?;
    valid_file(&opened, max)?;
    if !same_file_state(&before, &opened)? {
        return Err(corrupt("store file changed while opening"));
    }
    let mut b = Vec::with_capacity(usize::try_from(opened.len()).map_err(|_| limit("file bytes"))?);
    (&mut f)
        .take(max.saturating_add(1))
        .read_to_end(&mut b)
        .map_err(|e| ioerr(e, n))?;
    if b.len() as u64 > max {
        return Err(limit("file bytes"));
    }
    let after = cap_fs::stat(d, p, cap_fs::FollowSymlinks::No).map_err(|e| ioerr(e, n))?;
    valid_file(&after, max)?;
    if b.len() as u64 != opened.len() || !same_file_state(&after, &opened)? {
        return Err(corrupt("store file changed while read"));
    }
    let observation = FileObservation {
        identity: id_meta(&opened)?,
        state: file_state(&opened),
        digest: hash_bytes(&b),
    };
    Ok((b, observation))
}

pub(super) fn read_accounted_observed(
    directory: &std::fs::File,
    name: &str,
    max: u64,
    stats: &mut GcPlanStats,
    limits: &GcPlanLimits,
    observations: &mut BTreeMap<String, FileObservation>,
    observation_name: &str,
) -> Result<Vec<u8>> {
    let (bytes, observation) = read_regular_observed(directory, name, max)?;
    account(stats, bytes.len() as u64, limits)?;
    insert_observation(observations, observation_name, observation)?;
    Ok(bytes)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn read_required_accounted_observed(
    directory: &std::fs::File,
    name: &str,
    max: u64,
    missing_message: &str,
    stats: &mut GcPlanStats,
    limits: &GcPlanLimits,
    observations: &mut BTreeMap<String, FileObservation>,
    observation_name: &str,
) -> Result<Vec<u8>> {
    match read_accounted_observed(
        directory,
        name,
        max,
        stats,
        limits,
        observations,
        observation_name,
    ) {
        Err(error) if is_io_not_found(&error) => Err(corrupt(missing_message)),
        result => result,
    }
}
