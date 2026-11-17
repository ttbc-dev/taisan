"""Regenerate the invented examples and freeze manifest after generating schema.json."""
import hashlib
import json
from pathlib import Path

ROOT = Path(__file__).resolve().parent
VERSION = "rui-evidence/1"
INSTALLATION = "installation-example"
EPOCH = "epoch-example"
WORK = "work-example"

def sha(data):
    return hashlib.sha256(data).hexdigest()

def write(name, value):
    (ROOT / name).write_bytes((json.dumps(value, indent=2) + "\n").encode())

def ref(source_id, revision="r1"):
    return dict(source_id=source_id, revision=revision)

def gap(name, evidence_class, reason):
    return dict(code="not_recorded", scope=dict(kind="revision", record=ref(name)),
                evidence_class=evidence_class, reason=reason)

def record(name, seq, operation, revision="r1", revision_seq=1, **data):
    kind = "entity" if operation.startswith("entity") else "link"
    body = dict(**ref(name, revision), revision_seq=revision_seq, record_kind=kind,
                work_id=WORK, assertion=dict(kind="observed"),
                source_refs=[dict(artifact_id="observations", sha256=artifact_sha,
                                  locator=dict(kind="json_pointer", pointer=f"/{name}/{revision}"))],
                operation=operation, **data)
    records[name, revision] = body
    return body, seq

def entity(source_id, seq, kind, rev="r1", number=1, **payload):
    return record(source_id, seq, "entity", rev, number, type=kind, payload=payload)

def link(name, seq, relation, source, target, operation="link", **fields):
    return record(name, seq, operation, relation=relation, **{"from": ref(source), "to": ref(target)}, **fields)

def envelope(kind, result=None, error=None, watermark=None):
    env = dict(schema_version="1", status="error" if error else "accepted",
               command=dict(id=None, kind=kind, actor="local", idempotency_key=None),
               work_id=None, execution_id=None, result=result, reason=error, evidence_refs=[])
    if watermark is not None:
        env.update(sources=[], as_of=watermark)
    return env

def page(label, watermark, entries, *, after=None, cursor=None,
         next_cursor=None, checkpoint=None, gaps=(), artifacts=()):
    export = dict(protocol_version=VERSION, installation_id=INSTALLATION, store_epoch=EPOCH,
                  snapshot_id="snapshot-delta" if after else "snapshot-full", snapshot_seq=watermark,
                  retained_from_seq=3, after_seq=10 if after else None, entities=[], links=[],
                  artifacts=list(artifacts), gaps=list(gaps), next_cursor=next_cursor, checkpoint=checkpoint)
    for body, seq in sorted(entries, key=lambda e: (e[1], e[0]["record_kind"], e[0]["source_id"], e[0]["revision"])):
        raw = json.dumps(body, separators=(", ", ": ")) + "\n"
        export["links" if body["record_kind"] == "link" else "entities"].append(
            dict(**ref(body["source_id"], body["revision"]), revision_seq=body["revision_seq"],
                 change_seq=seq, body_sha256=sha(raw.encode()), body_json=raw))
    write(label, dict(request=dict(protocol_version=VERSION, after=after, cursor=cursor, limit=100),
                      envelope=envelope("evidence_export", export, watermark=watermark)))

names = {
    "work": ["r1"], "exec-a": ["r1"], "exec-b": ["r1"], "invoke-a": ["r1"],
    "round-a": ["r1"], "round-b": ["r1"], "finding-a": ["r1", "r2"],
    "finding-b": ["r1", "r2"], "repair-a": ["r1"], "repair-b": ["r1"],
    "check-a": ["r1"], "check-b": ["r1"], "binding-a": ["r1"], "binding-b": ["r1"],
    "addresses-a": ["r1", "r2"], "unsupported": ["r1"], "delta-addresses": ["r1"],
    "requirement": ["r1"], "invariant": ["r1"], "outcome": ["r1"],
}
observations = {name: {rev: f"Synthetic retained observation for {name} {rev}." for rev in revisions}
                for name, revisions in names.items()}
observations["binding-a"]["r1"] = {"check": "check-a", "repair": "repair-a", "head": "head-a1", "state": "failed"}
observations["binding-b"]["r1"] = {"check": "check-b", "repair": "repair-b", "head": "head-b1", "state": "passed"}
write("observations.json", observations)
artifact = (ROOT / "observations.json").read_bytes()
artifact_sha = sha(artifact)
descriptor = dict(artifact_id="observations", identity=dict(installation_id=INSTALLATION,
    store_epoch=EPOCH, source_id="retained-observation-bundle", revision="r1", revision_seq=1),
    sha256=artifact_sha, byte_length=len(artifact), media_type="application/json", availability="available")
records = {}
first = [
    entity("work", 3, "work", work_id=WORK, version=1, intent="Check a synthetic ledger.",
           requirements=[ref("requirement")], constraints=[ref("invariant")], dispatch_source=None),
    entity("exec-a", 4, "execution", execution_id="exec-a", work=ref("work"), state="failed",
           outcome=None, base_head="head-a0", result_head="head-a1", started_at=None, finished_at=None),
    entity("invoke-a", 4, "invocation", invocation_id="invoke-a", execution=ref("exec-a"),
           process_attempt_id="process-a", state="failed"),
    entity("round-a", 5, "review_round", round_id="round-a", execution=ref("exec-a"),
           change_id="change-a", reviewed_head="head-a0", completeness="complete", observation_id="observation-a"),
    entity("finding-a", 6, "finding_occurrence", occurrence_id="finding-a", source_finding_key="rule-example",
           round=ref("round-a"), reviewed_head="head-a0", body="An earlier description.", disposition="open",
           category=None, invariants=[ref("invariant")]),
    entity("exec-b", 7, "execution", execution_id="exec-b", work=ref("work"), state="passed",
           outcome=ref("outcome"), base_head="head-b0", result_head="head-b1", started_at=None, finished_at=None),
]
second = [
    link("addresses-a", 7, "repair_addresses_finding", "repair-a", "finding-a"),
    entity("round-b", 8, "review_round", round_id="round-b", execution=ref("exec-b"),
           change_id="change-b", reviewed_head="head-b0", completeness="partial", observation_id=None),
    entity("finding-b", 8, "finding_occurrence", occurrence_id="finding-b", source_finding_key="rule-example",
           round=ref("round-b"), reviewed_head="head-b0", body=None, disposition="unknown", category=None, invariants=[]),
    entity("finding-a", 9, "finding_occurrence", "r2", 2, occurrence_id="finding-a", source_finding_key="rule-example",
           round=ref("round-a"), reviewed_head="head-a0", body="The ledger entry was duplicated.",
           disposition="accepted", category="ledger", invariants=[ref("invariant")]),
    record("finding-b", 10, "entity_tombstone", "r2", 2, entity_type="finding_occurrence", reason="Duplicate source entry."),
    entity("requirement", 8, "requirement", requirement_id="requirement", text="Keep a ledger.",
           category=None, provenance="dispatch", source_revision=None),
    entity("invariant", 8, "invariant", invariant_id="invariant", text="Keep one entry per event.",
           category="ledger", provenance="annotation", source_revision=None),
    entity("outcome", 10, "outcome", outcome_id="outcome", execution=ref("exec-b"), scope="execution",
           state="passed", head="head-b1"),
    link("addresses-a", 10, "repair_addresses_finding", "repair-a", "finding-a",
         operation="link_tombstone", revision="r2", revision_seq=2, reason="Source withdrew the relation."),
    link("unsupported", 10, "repair_addresses_finding", "repair-b", "purged-finding"),
]
for suffix, state, exit_code in [("a", "failed", 1), ("b", "passed", 0)]:
    head = f"head-{suffix}1"
    second.extend([
        entity(f"repair-{suffix}", 9, "repair_attempt", attempt_id=f"repair-{suffix}",
               execution=ref(f"exec-{suffix}"), round=ref(f"round-{suffix}"), target_head=f"head-{suffix}0",
               result_head=head, state=state, sequence=1),
        entity(f"check-{suffix}", 10, "verification", check_id=f"check-{suffix}", name="ledger check",
               tested_head=head, state=state, scope="targeted", exit_code=exit_code),
        link(f"binding-{suffix}", 10, "verification_tests_repair", f"check-{suffix}", f"repair-{suffix}"),
    ])
records["check-a", "r1"]["source_refs"][0]["locator"] = dict(kind="byte_range", start=0, end=1)
page("full-1.json", 10, first, next_cursor="cursor-example", artifacts=[descriptor],
     gaps=[dict(code="retention_expired", scope=dict(kind="history", before_seq=3), evidence_class="history",
                reason="Earlier synthetic history expired.")])
page("full-2.json", 10, second, cursor="cursor-example", checkpoint="checkpoint-full",
     gaps=[gap("finding-b", "body", "Original finding body was not retained."),
           gap("purged-finding", "relation", "Endpoint revision was not retained.")])
page("delta.json", 11, [link("delta-addresses", 11, "repair_addresses_finding", "repair-b", "finding-a")],
     after="checkpoint-full", checkpoint="checkpoint-delta")
write("artifact-success.json", dict(request=dict(protocol_version=VERSION, artifact_id="observations", sha256=artifact_sha),
      envelope=envelope("evidence_artifact", dict(protocol_version=VERSION, descriptor=descriptor, bytes_written=len(artifact)))))
for code in ["snapshot_expired", "checkpoint_expired", "artifact_unavailable", "artifact_too_large", "digest_mismatch"]:
    write(code.replace("_", "-") + ".json", envelope("evidence_export" if code.endswith("_expired") else "evidence_artifact",
          error=dict(code=code, message=code.replace("_", " ").capitalize() + ".")))
unknown = json.loads((ROOT / "full-1.json").read_bytes())["envelope"]["result"]
unknown["protocol_version"] = "rui-evidence/9"
write("unknown-version.json", unknown)
write("freeze-manifest.json", dict(protocol_version=VERSION,
      sha256={p.name: sha(p.read_bytes()) for p in sorted(ROOT.glob("*.json")) if p.name != "freeze-manifest.json"}))
