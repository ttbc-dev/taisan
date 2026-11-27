// Example managed-Worker entry point. Illustrative only: it reads the
// fixed binding paths the supervisor mounts — both optional — and
// reports where each binding was found. The real adapter protocol is
// defined by the worker contract (taisan.worker.request/2,
// taisan.worker.report/2).
const fs = require("fs");

// Both bindings are optional: `/taisan/provider-config` exists only
// when the installation configures `worker_provider_config`, and
// `/taisan/runtime-auth` only when it configures `worker_credential_ref`
// — a valid installation omitting either mounts neither file.
const bindings = fs.existsSync("/taisan/provider-config")
  ? JSON.parse(fs.readFileSync("/taisan/provider-config", "utf8"))
  : null;
if (bindings && bindings.format_version !== "taisan.worker.bindings/1") {
  throw new Error("unsupported provider-config contract");
}
// Only the contract version is checked here; the member shape rules
// (required `model`, bounded non-empty strings) are the supervisor's
// validation surface — docs/runtime-bindings.md.
const credential = fs.existsSync("/taisan/runtime-auth")
  ? fs.readFileSync("/taisan/runtime-auth", "utf8")
  : null;
// `credential` is passed to the runtime child's own environment only —
// never to argv, the report, runtime.log, or git.

console.log(`model: ${bindings ? bindings.model : "(no provider-config)"}`);
console.log(`endpoint: ${bindings ? bindings.endpoint : "(none)"}`);
console.log(`proxy: ${process.env.HTTPS_PROXY}`);
console.log(`credential bytes: ${credential ? credential.length : 0}`);
