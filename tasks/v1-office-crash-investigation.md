# macOS LibreOffice startup and conversion failures — 2026-09-07

The user-reported crash at 22:33:30 JST was the LibreOffice 26.8.0.3 process
started by Kio's adapter tests on macOS 26.6.2. It aborted during `--version`,
before document conversion. The report's machine identifiers are deliberately
not copied into the repository.

## Cause and evidence

The `ReadWriteAccess::create` frame at offset 820 is the generated service's
`DeploymentException` path when its component context cannot supply the
configuration service. Local disassembly confirmed that this is distinct from
its allocation-failure path. The exception alone did not identify the cause.

Controlled comparisons established the missing sandbox permission:

1. The same installed executable, cleared environment, private HOME/TMPDIR and
   private `-env:UserInstallation` returned its version successfully without
   Seatbelt. The ordinary user profile was not used.
2. Under the renderer's Seatbelt rules, opening the allowed bundle's
   `Resources/sofficerc` and `fundamentalrc` worked, but `Cwd::realpath` on both
   returned `Operation not permitted`. Canonicalizing a path requires metadata
   access to its ancestors as well as access to the final file.
3. Granting `file-read-metadata` and `file-test-existence` only for ancestors of
   the selected runtime roots and private scratch made the confined native
   `--version` probe pass. Parent directory listings and unrelated file contents
   remain denied.

The earlier `user-preference-read` and `configd` sandbox reports came from
libcurl's initialization. They were not established as the abort's cause.
No extra preference or configd permission is required for the successful
version probe; the exploratory preference grant was removed.

## Regression evidence

`KIO_REAL_OFFICE=1 cargo test -p kio-process --test confinement_macos --locked`
passed all three tests after the fix:

- Native LibreOffice version probe with the actual sandbox and private profile.
- Runtime `realpath` succeeds, while sibling private contents and parent
  directory listing remain inaccessible.
- Unrelated private-file access and loopback TCP payload transmission fail.

This proves startup and those confinement checks on this Mac. Real DOCX/PPTX
conversion, resource enforcement, Windows/Linux execution, and the broader v1
acceptance gates are separate checks. See [implementation record](v1-implementation-progress.md).

## Second report: Writer initialization during conversion

The later report at 23:35:22 JST names soffice PID 6839, matching the single
synthetic DOCX diagnostic invocation. It is distinct from the first version
probe crash. An ordinary conversion had returned exit 1 with an uninformative
application error. The same synthetic document and private environment converted
successfully without Seatbelt. Fontconfig warnings were present in both runs,
so they were not evidence of the cause.

Normal LLDB launch was refused by macOS executable protections. A diagnostic-only
`--norestore` invocation caused LibreOffice to emit the terminating stack; this
flag was then removed. The later user-supplied report confirms the same stack:

`Writer initialization -> LngSvcMgr::UpdateAll -> MacSpellChecker ->
NSApplicationLoad -> AppKit/HIServices application registration -> abort`.

LibreOffice enumerates and instantiates available linguistic services when
Writer starts. Its native macOS spellchecker loads AppKit even for this headless
conversion. This matches the source of
[MacSpellChecker](https://github.com/LibreOffice/core/blob/master/lingucomponent/source/spellcheck/macosxspell/macspellimp.mm)
and the [linguistic service manager](https://github.com/LibreOffice/core/blob/master/linguistic/source/lngsvcmgr.cxx).
Turning off automatic spelling alone would not remove that service enumeration.

A private UNO catalog, excluding only the native macOS spellchecker component,
allowed the same confined DOCX test to pass: two independent PDF conversions,
normalized-byte equality and offline compressed-text extraction. The production
adapter now reads the recognized LibreOffice bundle's catalog with a bounded
regular-file reader, creates a private filtered copy and supplies it through
`URE_MORE_SERVICES` during both probing and conversion. It rejects a source
catalog change between resolution and conversion. Installed LibreOffice files
and the ordinary user profile were not changed.

Separate required native tests passed with this production path for DOCX
(2.81 seconds) and PPTX (2.49 seconds). These are local macOS evidence for the
candidate, not Windows/Linux or final-release acceptance. Strict parser checks
and required preparation-profile persistence were subsequently added; the final
native rerun is recorded below. The executable fingerprint is checked before
and after conversion, and a changed profile prevents reuse even when extracted
text is unchanged.

## Additional confinement checks

LibreOffice's Unix socket is restricted to the invocation's private scratch.
The scratch path is short enough for macOS Unix socket limits; caches and the
LibreOffice profile stay inside it. A fixture confirms private socket access and
refuses a socket elsewhere, while TCP remains denied.

PDF output is read through a directory handle retained before renderer launch,
with no-follow, bounded regular-file and stable-identity checks. Symlink output
and output-directory replacement tests pass. The renderer may not signal an
unrelated process, and selecting an executable no longer grants access to every
file beside it. Linux PID/proc isolation and Windows AppContainer/DACL behavior
have separate native acceptance requirements.

macOS now explicitly denies `process-fork`; direct DOCX/PPTX conversion still
passes, and a fixture confirms that a renderer cannot create a descendant after
attempting to change its session. The parent observes the direct renderer's
physical footprint every 50 ms and terminates it above 2 GiB. This sampled
threshold can be exceeded between observations; it is not an instantaneous
kernel memory cap. The memory-limit regression verifies that the process is
dead after the error, rather than accepting an error that leaked a running child.

The process package's tests and warning-free Clippy check pass. Aggregate scratch
disk growth and native Windows/Linux resource behavior still require their
separate acceptance evidence.

## Final local candidate verification — 2026-09-08

- `cargo +1.98.0 test -p kio-adapter --lib office_convert --locked`: 19 passed.
  The two opt-in native tests return without invoking LibreOffice in this normal
  run; their actual execution is recorded separately below.
- With `KIO_REAL_OFFICE=1` and the explicit installed app executable,
  `cargo +1.98.0 test -p kio-adapter --lib office_real_soffice --locked -- --nocapture --test-threads=1`:
  DOCX and PPTX both passed, 5.42 seconds total. Each converts twice and verifies
  normalized-byte equality plus extracted text.
- With the same native opt-in, `cargo +1.98.0 test -p kio-process --test confinement_macos --locked -- --test-threads=1`:
  10 passed, including the real version probe, direct `posix_spawn` denial,
  private-file/network boundaries and physical-memory cancellation.

The output-symlink and output-directory replacement fixtures use in-process Perl
filesystem calls, so they exercise output recovery after actually creating the
malicious output layout. A shell's inability to fork is not accepted as their
output-boundary proof.

The immutable review candidate is `/private/tmp/kio-renderer-candidate-f9xdlh6a`.
Its `manifest.json` SHA-256 is
`7eafeb7a51f716bfa3f617dad3eece4641fbdd11d41e261cd93bb2d5e0a38b65`.
The review found no surviving private-sibling read or output-directory
redirection. The initial normal adapter run reported a core CAS dead-code
warning; that helper was subsequently removed and core Clippy passed. This is
still not a whole-workspace or three-OS acceptance result.

## Post-patch review dispositions

- The review's claim that `URE_MORE_SERVICES` still adds the original macOS
  spellchecker is not supported by the installed 26.8.0.3 configuration.
  `Resources/ure/etc/unorc` loads the separate URE core registry plus
  `${URE_MORE_SERVICES}`. `Resources/fundamentalrc` normally assigns the
  application registries and extensions to that variable. Kio replaces that
  assignment. Only `Resources/services/services.rdb` contains MacSpellChecker;
  `Resources/ure/share/misc/services.rdb` does not. The actual confined DOCX/PPTX
  conversions passed. Replacing all `UNO_SERVICES` would also remove the URE
  core services and is not the verified fix.
- Homebrew alias substitution was too broad. Resolution now checks the known
  wrapper's exact passthrough bytes as well as its location; an unknown wrapper
  keeps its selected executable rather than silently selecting the app. Only
  the resolved app executable admits the bundle/catalog. The updated Office
  unit suite passed 20 tests; both native cases also passed through the actual
  Homebrew command (4.98 seconds total).
- Linux currently limits address space and CPU per process. Its PID namespace
  does not impose an aggregate process or resource ceiling. A hostile renderer
  can create multiple processes; this remains an open v1 resource requirement,
  requiring a process-creation boundary or equivalent aggregate enforcement and
  native validation.
- Executable digest rechecks are not an atomic hash-to-exec binding. They detect
  ordinary replacement but cannot prove which object ran if a process able to
  replace the configured executable swaps it and swaps it back. The configured
  renderer is device execution authority, separate from hostile document input;
  the implementation must not claim a stronger execution identity guarantee.
  A local synthetic macOS experiment confirmed that `/dev/fd/N` execution is
  refused with `EACCES`, so it is not a portable descriptor-execution solution.
  A stronger execution binding remains open for the renderer identity design.

## Fixed-release acceptance follow-up — 2026-09-27

The pristine, signature-verified official macOS 26.2.5.2 distribution passes
`--version` but aborts during actual DOCX conversion. Its crash stack enters
`CreateSalInstance`, the `osx` VCL backend, AppKit `NSApplication`, then
HIServices `RegisterApplication`. This is separate from the earlier
MacSpellChecker startup failure. The official 26.2.5 build configuration lacks
`--enable-headless`; the official 26.8.0 configuration includes it, corroborated
by the corresponding SVP symbols in the binaries. Passing `--headless` or setting
`SAL_USE_VCLPLUGIN=svp` cannot supply an absent compiled backend.

The official 26.8.0.3 macOS distribution passed the confined A07 diagnostic:
DOCX `page:1`, PPTX `slide:1`, and XLSX `sheet:Fixture` were persisted, and malformed
inputs were rejected. The staged application's 18,538 inventory entries and
deep/strict/all-architecture signature remained unchanged. This proof used
private debug binaries at the `1b6165c` source state; it is not a packaged-candidate
or native Actions receipt. Original failure evidence remains retained.

Actions now pins official 26.8.0 for all three OSes with separately verified
distribution hashes. Verified macOS bundles older than 26.8, and malformed or
ambiguous version output, are rejected before document conversion. This is a
supported-release policy, not a substitute for native capability tests. Generic
standalone converter resolution retains its existing behavior. The sandbox
policy and installed bundle bytes are not weakened or modified for compatibility.

The Linux 26.8.0 archive was downloaded and its SHA-256 matched the official pin.
Offline extraction confirms the root-owned `/opt/libreoffice26.8/program/soffice`
layout. Real Linux conversion remains unverified: the available Docker execution
environment disallows the nested user namespace, while an outer rootless
namespace maps package ownership incorrectly for the production guard. Neither
failure is counted as a conversion pass. Linux aggregate descendant resource
enforcement above also remains a v1 blocker until implementation and native tests
are complete.
