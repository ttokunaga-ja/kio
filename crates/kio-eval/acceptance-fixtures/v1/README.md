# Acceptance fixture bundles v1

`a05-text-vector-cursor` is the runnable deterministic native-contract input for
multi-scope text/vector/hybrid, cursor, and history assertions. It deliberately
sets `require_image: false`; it is not an A05-full receipt input.

`a05-full-image-unavailable` keeps its historical directory name because other
acceptance inputs reference it. It is now the required A05/A06 fixture: the
native image-ingestion path validates the PNG/JPEG/WebP leaves and materializes
the image objects consumed by the release-binary evaluator. The v1 workflow
binds its manifest-and-leaf digest before invoking A05 or A06; no debug mock or
external OCR runtime is an acceptance substitute.

This fixture has been wired into the native three-OS acceptance lane but still
requires its first release-candidate execution before it can be described as
validated evidence.
