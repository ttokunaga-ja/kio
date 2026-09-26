# A07 public Office fixture

This directory contains deterministic, stored-ZIP OOXML leaves used only by
the real-Office A07 acceptance runner. It contains no user data, external
media, or external OOXML relationships. The presentation fixture gives its
marker text shape explicit position, extent, and rectangle geometry so a real
Office renderer produces a visible slide text layer.

The runner hashes the ordered tuple `filename + NUL + bytes + NUL` for
`document.docx`, `presentation.pptx`, `table.xlsx`, and `malformed.docx`.
The expected digest is:

`79fdea154a9f61138349e560f2a8e3b8e14126b01f384e4951f2c85247216827`

`malformed.docx` is intentionally not OOXML. It must cause the product's real
converter path to reject the incremental indexing operation; it is not a mock
renderer failure fixture.
