//! Administrative acceptance helpers. Never part of the distributed Kio CLI.
use clap::{Parser, Subcommand};
use kio_eval::acceptance_tools::{
    dispatch, gpu_identity, gpu_memory, local_client, ocr_proxy, package_extract,
    prepare_embedding, provider_ledger, route_preflight,
};

#[derive(Parser)]
#[command(
    name = "kio-acceptance-tools",
    version,
    about = "Bounded Kio acceptance infrastructure"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Operate the protected cumulative provider budget ledger.
    ProviderLedger(provider_ledger::Args),
    /// Drive the fixed private GPU route from an Actions client.
    LocalClient(local_client::Args),
    /// Materialize the dedicated route key and hosts privately from environment.
    RouteMaterialize(local_client::RouteMaterializeArgs),
    /// Serve one constrained SSH_ORIGINAL_COMMAND on the fixed GPU host.
    Dispatch(dispatch::Args),
    /// Print a read-only, candidate-bound deployment proposal.
    RoutePreflight(route_preflight::Args),
    /// Verify or materialize the pinned embedding model bundle offline.
    PrepareEmbedding(prepare_embedding::Args),
    /// Serve the bounded TLS facade for the local OCR backend.
    OcrProxy(ocr_proxy::Args),
    /// Create, check, or observe the fixed GPU runtime identity.
    GpuIdentity(gpu_identity::Args),
    /// Capture or check the run-bound GPU memory baseline.
    GpuMemory(gpu_memory::Args),
    /// Validate a closed Actions ZIP and create a new package directory.
    PackageExtract(package_extract::Args),
}

fn main() -> std::process::ExitCode {
    let result = match Cli::parse().command {
        Command::ProviderLedger(args) => provider_ledger::run(args),
        Command::LocalClient(args) => local_client::run(args),
        Command::RouteMaterialize(args) => local_client::materialize_route(args),
        Command::Dispatch(args) => dispatch::run(args),
        Command::RoutePreflight(args) => route_preflight::run(args),
        Command::PrepareEmbedding(args) => prepare_embedding::run(args),
        Command::OcrProxy(args) => ocr_proxy::run(args),
        Command::GpuIdentity(args) => gpu_identity::run(args),
        Command::GpuMemory(args) => gpu_memory::run(args),
        Command::PackageExtract(args) => package_extract::run(args),
    };
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("acceptance helper refused: {message}");
            std::process::ExitCode::FAILURE
        }
    }
}
