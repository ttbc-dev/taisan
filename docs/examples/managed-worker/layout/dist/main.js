// Example managed-Worker entry point. Illustrative only: it reads the
// fixed binding paths the supervisor mounts and reports where each
// binding was found. The real adapter protocol is defined by the
// worker contract (taisan.worker.request/2, taisan.worker.report/2).
const fs = require("fs");

const bindings = JSON.parse(fs.readFileSync("/taisan/provider-config", "utf8"));
if (bindings.format_version !== "taisan.worker.bindings/1") {
  throw new Error("unsupported provider-config contract");
}
const credential = fs.readFileSync("/taisan/runtime-auth", "utf8");
// `credential` is passed to the runtime child's own environment only —
// never to argv, the report, runtime.log, or git.

console.log(`model: ${bindings.model}`);
console.log(`endpoint: ${bindings.endpoint}`);
console.log(`proxy: ${process.env.HTTPS_PROXY}`);
console.log(`credential bytes: ${credential.length}`);
