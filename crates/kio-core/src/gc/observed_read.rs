//! Descriptor-bound observed reads and their existing GC accounting.

use std::{collections::BTreeMap, io::Read, path::Path};

use cap_primitives::fs as cap_fs;

use crate::{
    cas::{CasReadAccess, hash_bytes},
    error::Result,
};

use super::{
    FileObservation, GcPlanLimits, GcPlanStats, account, corrupt, file_state, id_meta,
    insert_observation, ioerr, is_io_not_found, limit, same_file_state, valid_file,
};

pub(crate) fn read_regular_observed(
    d: &std::fs::File,
    n: &str,
    max: u64,
) -> Result<(Vec<u8>, FileObservation)> {
    read_cas_regular_observed(&CasReadAccess::Unmetered, d, n, max)
}

pub(crate) fn read_cas_regular_observed(
    reads: &CasReadAccess,
    d: &std::fs::File,
    n: &str,
    max: u64,
) -> Result<(Vec<u8>, FileObservation)> {
    read_observed(reads, d, n, max, true)
}

/// Working-tree ignore policy is user-owned; retain no-follow identity and
/// byte observations without imposing Kio storage permissions on that file.
pub(super) fn read_user_regular_observed(
    d: &std::fs::File,
    n: &str,
    max: u64,
) -> Result<(Vec<u8>, FileObservation)> {
    read_observed(&CasReadAccess::Unmetered, d, n, max, false)
}

fn read_observed(
    reads: &CasReadAccess,
    d: &std::fs::File,
    n: &str,
    max: u64,
    require_private: bool,
) -> Result<(Vec<u8>, FileObservation)> {
    reads.charge(0)?;
    let p = Path::new(n);
    let before = cap_fs::stat(d, p, cap_fs::FollowSymlinks::No).map_err(|e| ioerr(e, n))?;
    valid_file(&before, max)?;
    let mut o = cap_fs::OpenOptions::new();
    o.read(true);
    o._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
    let mut f = cap_fs::open(d, p, &o).map_err(|e| ioerr(e, n))?;
    if require_private {
        crate::private_fs::verify_private_regular_file_handle(&f)?;
    }
    let opened = cap_fs::Metadata::from_file(&f).map_err(|e| ioerr(e, n))?;
    valid_file(&opened, max)?;
    if !same_file_state(&before, &opened)? {
        return Err(corrupt("store file changed while opening"));
    }
    let mut b = Vec::with_capacity(usize::try_from(opened.len()).map_err(|_| limit("file bytes"))?);
    let mut bounded = (&mut f).take(max.saturating_add(1));
    let mut buffer = [0_u8; 8192];
    loop {
        reads.charge(0)?;
        let count = match bounded.read(&mut buffer) {
            Ok(count) => count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(ioerr(error, n)),
        };
        reads.charge(count as u64)?;
        if count == 0 {
            break;
        }
        b.extend_from_slice(&buffer[..count]);
    }
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
    read_cas_accounted_observed(
        &CasReadAccess::Unmetered,
        (directory, name, max),
        stats,
        limits,
        observations,
        observation_name,
    )
}

pub(super) fn read_cas_accounted_observed(
    reads: &CasReadAccess,
    source: (&std::fs::File, &str, u64),
    stats: &mut GcPlanStats,
    limits: &GcPlanLimits,
    observations: &mut BTreeMap<String, FileObservation>,
    observation_name: &str,
) -> Result<Vec<u8>> {
    let (bytes, observation) = read_cas_regular_observed(reads, source.0, source.1, source.2)?;
    account(stats, bytes.len() as u64, limits)?;
    insert_observation(observations, observation_name, observation)?;
    Ok(bytes)
}

pub(super) fn read_required_accounted_observed(
    reads: &CasReadAccess,
    source: (&std::fs::File, &str, u64),
    missing_message: &str,
    stats: &mut GcPlanStats,
    limits: &GcPlanLimits,
    observations: &mut BTreeMap<String, FileObservation>,
    observation_name: &str,
) -> Result<Vec<u8>> {
    match read_cas_accounted_observed(reads, source, stats, limits, observations, observation_name)
    {
        Err(error) if is_io_not_found(&error) => Err(corrupt(missing_message)),
        result => result,
    }
}
