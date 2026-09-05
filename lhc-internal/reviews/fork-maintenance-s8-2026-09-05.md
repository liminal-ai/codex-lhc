# S8 — schema-13 image adapter qualification

The SDK pin is `e9456a6ee23a10cf04e15b16bcf77738c52b0f7c`, the certified
schema-13 reference used by Grok. Release/promotion identity checks now name
that exact commit. The vendor working tree is clean.

User images and paired function/custom-tool output images use ordered SDK
content blocks. Embedded base64 enters the SDK blob store; external references
and Codex image-detail settings round-trip. Text-only mappings remain unchanged.
The SDK payload-accessor split is reflected in text-history readers and tests.
User display twins now carry image fields rather than spilling bytes into text.
Unsupported assistant block variants refuse reconstruction explicitly.

Full-block reconstruction restores images. Compressed parts/bands intentionally
serve bounded text placeholders. Missing blobs remain placeholders; external
references are preserved without attempting to fetch them. Legacy image markers
remain text; their original image attributes cannot be recovered. Audio remains
outside this slice. The existing refusal for unpaired structured tool results
is retained; this does not expand that older exactness contract.

Evidence:

- 212 host library tests pass.
- 24 production certification tests pass, including ordered image/tool capture
  and restart, missing-blob behavior, and schema-12 migration on a test-created copy,
  preserving message IDs, turns, and step indices. Only the certification test
  copies the file before opening it. The SDK migrates the opened file in place
  within one `BEGIN IMMEDIATE` transaction, setting `user_version` to 13 before
  committing. The unopened source fixture remains schema 12.
- Seven full-loop tests pass, including an actual parts install followed by a
  provider request containing native view_image output bytes, and a pasted image
  whose compressed placeholder persists through resume. Tests assert the served
  structure and parts view, not a model's description of the image.
- Capture seam, schema fixture, compact bridge/arm, 47 mid-turn tests, 18 recovery
  tests, bare CLI capture, and the sustained nine-cycle production parts gate pass.
- Recovery patch reconstruction matches all 103 covered files byte-for-byte.
- SDK ancestry refresh verifies the pin on main, four commits behind that tip.
- No test retries were enabled.

The full tripwire invocation in `/tmp/lhc-s8-tripwire.log` exited 1 solely for
`trivially_copy_pass_by_ref` in the new image-detail helper. Passing the enum by
value fixes it. The subsequent strict clippy command (same flags as the gate)
passed; `/tmp/lhc-s8-final-clippy.log` records that result. `just fix` and
`just fmt` completed; unrelated formatter/lint churn was reverted. This is
layer-by-layer qualification, not a claim that the original invocation exited 0.
The next combined fork qualification must run before publication.

Final combined qualification passed: [remaining-slices handoff](fork-maintenance-remaining-2026-09-05.md).
