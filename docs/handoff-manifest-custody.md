# Handoff manifest custody for remote Invocations

This document records the v0.4 decision for the manifest that describes a Worker's produced commit and workspace artifacts during remote collection. It defines the boundary between that transfer metadata and the existing local artifact handoff. It is a design input to the versioned remote execution contract in #374.

## Decision

The remote handoff manifest lives in the fresh out directory for one Invocation. It does not live in the workspace and does not enter the produced commit. The Worker writes it beside `report.json` under a distinct name, such as `handoff.json`.

The manifest is transfer metadata. It identifies the produced commit and describes the workspace artifacts to collect. It does not establish that collection succeeded. The coordinator validates the manifest and the transferred Git object closure in quarantine, then records the durable artifact acknowledgement using the existing handoff repository and store contract.

The out directory belongs to exactly one coordinator-issued `request_id` and `invocation_id`. The supervisor creates it before dispatch and never reuses it. The remote endpoint retains the report, manifest, and collected output until the coordinator acknowledges durable collection.

## Two manifests with separate roles

The remote handoff manifest and the canonical Git object manifest are different records.

| Record | Producer | Contents | Authority |
| --- | --- | --- | --- |
| Remote handoff manifest | Worker adapter | Invocation identity, produced commit identity, and bounded descriptors for declared workspace artifacts | Untrusted input to collection; it cannot acknowledge an artifact |
| Canonical Git object manifest | Coordinator supervisor | Repository, base, produced commit, tree, and the sorted object IDs in the transferred closure | Digest-bound input to the durable artifact handoff record |

The handoff manifest may identify the expected object closure, but the coordinator derives and verifies the canonical Git object manifest from the imported objects. The coordinator does not trust a Worker claim, a manifest digest, or a Git object ID without checking the transferred content.

## Artifact evidence

`required_artifacts` continues to declare expected workspace outputs. The supervisor computes `artifact_digest(workspace, path)` for each declared output using the existing local evidence path. The remote handoff manifest carries bounded descriptors for those outputs through a separate collection channel. The manifest itself is not a required artifact and is not measured with `artifact_digest`.

Each descriptor names a path relative to the workspace and binds the path to the expected content digest. Absolute paths, parent traversal, symlinks, duplicate paths, and paths outside the declared artifact set are rejected. The versioned contract defines the exact encoding, canonicalization, and size limits.

The coordinator verifies every descriptor against the transferred Git object closure and the corresponding file in the produced commit. A matching digest proves that the declared output is present in that commit. A missing output or a mismatch fails artifact verification.

## Collection and acknowledgement

1. The coordinator allocates the request and Invocation identities and creates a fresh out directory.
2. The Worker writes `report.json` and the handoff manifest into that out directory. It does not write the remote handoff manifest into the workspace.
3. After the supervisor confirms process exit and stop proof, the executor transfers the bounded report, manifest, and Git object closure through the collection protocol.
4. The coordinator rejects a foreign or stale identity, malformed or oversized data, an unsafe path, a missing object, or a digest mismatch. Rejection never becomes `Present` evidence.
5. The coordinator imports valid objects into quarantine, verifies the base ancestry and exact produced commit, derives the canonical Git object manifest, and records its digest.
6. The coordinator acknowledges collection only after the durable artifact record and verified handoff ref agree. A repeated collection with the same Invocation and content is idempotent. Different content for the same identity is a conflict.
7. The executor may delete the retained out-directory contents only after acknowledgement and when the retention policy permits cleanup.

The report, handoff manifest, and object transfer can arrive in separate bounded messages. The collection protocol binds them to the same request, Invocation, and produced commit before it acknowledges any of them. A lost response can repeat collection without creating a second artifact identity or deleting the only retained copy.

## Verifier behavior

The verifier checks declared workspace outputs against the handed-off produced commit. It does not wait for a workspace-root manifest. Missing declared outputs fail the artifact verification. Missing, stale, or invalid transfer metadata fails collection. These failures remain distinct and neither can make a later workspace cleanliness check depend on a manifest staged into the repository.

The workspace-root `artifact.json` remains part of the existing local handoff path. The remote custody contract does not use that file as an alternate source of authority. The local contract continues to use its verified bundle, quarantine import, canonical Git object manifest, handoff ref, and acknowledged store record as described in the [Worker protocol](worker-protocol.md#artifact-handoff).

## Contract ownership

The #374 contract defines the versioned envelope, canonical serialization, field bounds, collection framing, digest rules, error vocabulary, and replay behavior. The #375 wire types implement those definitions. The #376 transfer implements bounded staging, quarantine import, interruption recovery, and retention through acknowledgement. Those issues preserve the distinction between expected workspace outputs and remote custody metadata.
