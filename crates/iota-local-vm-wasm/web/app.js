// Copyright (c) 2026 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0
//
// In-browser glue: fetches input objects from IOTA GraphQL (which returns
// BCS-encoded `Object` envelopes directly), hands them to the WASM-compiled
// Move VM, and renders the result. For signed simulation (including
// `MoveAuthenticator`), the wasm side also enumerates the auth-related
// objects we need to fetch up-front.

import init, {
  decode_transaction,
  decode_move_authenticator_objects,
  derive_field_id,
  simulate,
} from "../pkg/iota_local_vm_wasm.js";

const GRAPHQL = {
  mainnet: "https://graphql.mainnet.iota.cafe/graphql",
  testnet: "https://graphql.testnet.iota.cafe/graphql",
  devnet:  "https://graphql.devnet.iota.cafe/graphql",
  localnet:"http://localhost:9125/graphql",
};

// Sample staking transaction captured from devnet — same bytes the
// `offline_stake_inspect` Rust example uses.
const SAMPLE_STAKE_TX = "AAADAAgAlDV3AAAAAAEBAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAUBAAAAAAAAAAEAINqRtZV/6ONntsXV/L9IRp9ACpOV+VnDUxBwOyp4hRr+AgIAAQEAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAwtpb3RhX3N5c3RlbRFyZXF1ZXN0X2FkZF9zdGFrZQADAQEAAgAAAQIAHuEtyg55iWaoL3TAEMEJ4b0GdPT0dRfbaEPyI7rV63wCd+72H5zU/gt6MdaptzF/2BEJvVyZeilwKuGwtdDaCHgBAAAAAAAAACBP1tQu7fkEDhIgawXcbLBuc8pnfopybGLDMceo+Rgku7KiTuotGDfo8i49M/V+zY7RQ7089bD45vizBsUQvnkOAQAAAAAAAAAgRrgfcWFKJI6ORE4kvllfzibNTlHi46/l5t8MfFm0X0se4S3KDnmJZqgvdMAQwQnhvQZ09PR1F9toQ/IjutXrfOgDAAAAAAAAYBNBAAAAAAAA";

// Pre-baked fixtures — captured from `iota-local-executor` integration tests
// that spin up a real test cluster, publish an abstract-account Move package,
// and sign a transaction with a `MoveAuthenticator`. Each fixture carries the
// chain info + tx + signatures + every object the VM needs, so loading one
// short-circuits the GraphQL fetch and runs entirely from local data.
const SAMPLE_FIXTURES = {
  moveAuthValid:   "./samples/move_auth_free_access_valid.json",
  moveAuthInvalid: "./samples/move_auth_ed25519_invalid.json",
};

const logEl = document.getElementById("log");
const resultsEl = document.getElementById("results");

function log(msg, cls = "") {
  const span = document.createElement("div");
  if (cls) span.className = cls;
  span.textContent = msg;
  logEl.appendChild(span);
  logEl.scrollTop = logEl.scrollHeight;
}
function clearLog() { logEl.innerHTML = ""; }
function clearResults() { resultsEl.innerHTML = ""; }

async function gql(endpoint, query, variables) {
  const res = await fetch(endpoint, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ query, variables }),
  });
  if (!res.ok) throw new Error(`GraphQL HTTP ${res.status}: ${await res.text()}`);
  const json = await res.json();
  if (json.errors?.length) {
    throw new Error("GraphQL errors: " + json.errors.map((e) => e.message).join("; "));
  }
  return json.data;
}

// Fetch one object's BCS envelope by ID at the latest version.
async function fetchObjectBcs(endpoint, objectId) {
  const data = await gql(
    endpoint,
    `query Obj($id: IotaAddress!) {
       object(address: $id) {
         address
         version
         bcs
       }
     }`,
    { id: objectId },
  );
  return data.object;
}

// List all dynamic fields of an object (auto-paginated). For each field we
// derive the on-chain field-object ID (`Field<K, V>` wrapper) locally from
// `(parent_id, name.type, name.bcs)` because the IOTA GraphQL schema does not
// expose it directly. For dynamic *object* fields we also include
// `value.address`, the actual stored child object. Mirrors the gRPC
// `list_dynamic_fields` `field_id` / `child_id` pair the Rust example uses.
async function fetchDynamicFieldChildIds(endpoint, parentId) {
  const ids = [];
  let cursor = null;
  while (true) {
    const data = await gql(
      endpoint,
      `query DF($id: IotaAddress!, $cursor: String) {
         owner(address: $id) {
           dynamicFields(first: 50, after: $cursor) {
             pageInfo { hasNextPage endCursor }
             nodes {
               name { type { repr } bcs }
               value {
                 __typename
                 ... on MoveObject { address }
               }
             }
           }
         }
       }`,
      { id: parentId, cursor },
    );
    const conn = data.owner?.dynamicFields;
    if (!conn) break;
    for (const node of conn.nodes) {
      if (!node?.name?.type?.repr || node.name.bcs == null) continue;
      const isDof = node.value?.__typename === "MoveObject";
      const fieldId = derive_field_id(
        parentId,
        node.name.type.repr,
        node.name.bcs,
        isDof,
      );
      ids.push(fieldId);
      if (isDof && node.value?.address) ids.push(node.value.address);
    }
    if (!conn.pageInfo.hasNextPage) break;
    cursor = conn.pageInfo.endCursor;
  }
  return ids;
}

// Recursively fetch all dynamic-field child objects of the given parent IDs.
// Mirrors `fetch_dynamic_field_children` from the Rust example.
async function fetchAllDescendants(endpoint, rootIds) {
  const seen = new Set(rootIds.map((i) => i.toLowerCase()));
  const queue = [...rootIds];
  const children = [];
  while (queue.length) {
    const parent = queue.pop();
    const childIds = await fetchDynamicFieldChildIds(endpoint, parent);
    for (const cid of childIds) {
      const lc = cid.toLowerCase();
      if (seen.has(lc)) continue;
      seen.add(lc);
      const obj = await fetchObjectBcs(endpoint, cid);
      if (obj?.bcs) {
        children.push({ id: cid, bcs_b64: obj.bcs });
        queue.push(cid);
      }
    }
  }
  return children;
}

// Pull the live chain info we need to feed the executor.
async function fetchChainInfo(endpoint) {
  const data = await gql(
    endpoint,
    `query Info {
       epoch {
         epochId
         referenceGasPrice
         startTimestamp
         protocolConfigs { protocolVersion }
       }
     }`,
  );
  const epoch = data.epoch;
  return {
    epoch_id: Number(epoch.epochId),
    reference_gas_price: Number(epoch.referenceGasPrice),
    epoch_timestamp_ms: new Date(epoch.startTimestamp).getTime(),
    protocol_version: Number(epoch.protocolConfigs.protocolVersion),
  };
}

function renderResults(decoded, sim, signed) {
  clearResults();

  // Status
  const status = document.createElement("div");
  status.className = "card";
  status.innerHTML = `
    <h2>Execution status</h2>
    <p class="status ${sim.success ? "ok" : "err"}">
      ${sim.success ? "✓ Success" : "✗ Failed"}
    </p>
    <p style="margin:6px 0 0; color: var(--muted); font-size: 12px;">${escapeHtml(sim.status)}</p>
  `;
  resultsEl.appendChild(status);

  // Signature verification — only meaningful when the request carried signatures.
  if (signed) {
    const ok = sim.signature_verified;
    const sig = document.createElement("div");
    sig.className = "card";
    sig.innerHTML = `
      <h2>Signature verification</h2>
      <p class="status ${ok ? "ok" : "err"}">
        ${ok ? "✓ Accepted" : "✗ Rejected"}
      </p>
      <p style="margin:6px 0 0; color: var(--muted); font-size: 12px;">
        ${ok
          ? "Cryptographic check (and, for MoveAuthenticator, the authenticator function) passed."
          : "Either the crypto check failed or the MoveAuthenticator function aborted during execution."}
      </p>
    `;
    resultsEl.appendChild(sig);
  }

  // Error (if any)
  if (sim.error) {
    const err = document.createElement("div");
    err.className = "card error-card";
    err.innerHTML = `<h2>Error</h2><pre>${escapeHtml(sim.error)}</pre>`;
    resultsEl.appendChild(err);
  }

  // Gas
  const gas = document.createElement("div");
  gas.className = "card";
  gas.innerHTML = `
    <h2>Gas</h2>
    <div class="gas-grid">
      <div class="cell"><div class="label">Total used</div><div class="value">${fmtNanos(sim.gas_used)}</div></div>
      <div class="cell"><div class="label">Computation</div><div class="value">${fmtNanos(sim.computation_cost)}</div></div>
      <div class="cell"><div class="label">Storage cost</div><div class="value">${fmtNanos(sim.storage_cost)}</div></div>
      <div class="cell"><div class="label">Storage rebate</div><div class="value">${fmtNanos(sim.storage_rebate)}</div></div>
      <div class="cell"><div class="label">Non-refundable fee</div><div class="value">${fmtNanos(sim.non_refundable_storage_fee)}</div></div>
    </div>`;
  resultsEl.appendChild(gas);

  // Object effects
  const effects = document.createElement("div");
  effects.className = "card";
  effects.innerHTML = `
    <h2>Object effects</h2>
    <div class="kv">
      <div class="k">Created</div><div class="v">${sim.created_count}</div>
      <div class="k">Mutated</div><div class="v">${sim.mutated_count}</div>
      <div class="k">Deleted</div><div class="v">${sim.deleted_count}</div>
    </div>`;
  resultsEl.appendChild(effects);

  // Events
  if (sim.events.length) {
    const ev = document.createElement("div");
    ev.className = "card";
    const rows = sim.events
      .map(
        (e) => `<tr>
          <td>${escapeHtml(e.module)}::${escapeHtml(e.name)}</td>
          <td>${escapeHtml(e.sender)}</td>
        </tr>`,
      )
      .join("");
    ev.innerHTML = `
      <h2>Events (${sim.events.length})</h2>
      <table>
        <thead><tr><th>Type</th><th>Sender</th></tr></thead>
        <tbody>${rows}</tbody>
      </table>`;
    resultsEl.appendChild(ev);
  }

  // Command results
  if (sim.command_results.length) {
    const c = document.createElement("div");
    c.className = "card";
    const rows = sim.command_results
      .map(
        (r, i) =>
          `<tr><td>[${i}]</td><td>${r.mutable_ref_outputs}</td><td>${r.return_values}</td></tr>`,
      )
      .join("");
    c.innerHTML = `
      <h2>Command results</h2>
      <table>
        <thead><tr><th>#</th><th>Mut ref outputs</th><th>Return values</th></tr></thead>
        <tbody>${rows}</tbody>
      </table>`;
    resultsEl.appendChild(c);
  }

  // Transaction overview
  const tx = document.createElement("div");
  tx.className = "card";
  tx.innerHTML = `
    <h2>Transaction</h2>
    <div class="kv">
      <div class="k">Sender</div><div class="v">${escapeHtml(decoded.sender)}</div>
      <div class="k">Gas budget</div><div class="v">${fmtNanos(decoded.gas_budget)}</div>
      <div class="k">Gas price</div><div class="v">${decoded.gas_price}</div>
      <div class="k">Inputs</div><div class="v">${decoded.object_ids.length} objects (${decoded.shared_object_ids.length} shared)</div>
    </div>`;
  resultsEl.appendChild(tx);
}

function escapeHtml(s) {
  return String(s).replace(/[&<>"']/g, (c) => ({
    "&": "&amp;",
    "<": "&lt;",
    ">": "&gt;",
    '"': "&quot;",
    "'": "&#39;",
  })[c]);
}
function fmtNanos(n) {
  const v = Number(n);
  if (v === 0) return "0";
  const iota = v / 1e9;
  return `${v.toLocaleString()} <span style="color:var(--muted)">(${iota.toFixed(4)} IOTA)</span>`;
}

function readSignatures() {
  const raw = document.getElementById("sigInput").value;
  return raw
    .split(/[\r\n,]+/)
    .map((s) => s.trim())
    .filter((s) => s.length > 0);
}

// Live-mode runner: read tx + sigs from the textareas, fetch all required
// objects from the chosen network, then simulate.
async function runLive() {
  const network = document.getElementById("network").value;
  const tx = document.getElementById("txInput").value.trim();
  const strict = document.getElementById("strict").checked;
  const signatures = readSignatures();
  const endpoint = GRAPHQL[network];
  if (!tx) throw new Error("Paste a base-64 transaction first.");

  log(`> network: ${network}  (${endpoint})`);
  log("> decoding transaction in wasm…");
  const decoded = decode_transaction(tx);
  log(`  sender:        ${decoded.sender}`);
  log(`  gas budget:    ${decoded.gas_budget}`);
  log(`  input objects: ${decoded.object_ids.length} (${decoded.shared_object_ids.length} shared)`);
  if (signatures.length) {
    log(`  signatures:    ${signatures.length}`);
  }

  // For each signature that is a MoveAuthenticator, ask the wasm side which
  // extra objects (auth inputs + the AuthenticatorFunctionRefV1 dynamic field)
  // we need to fetch alongside the transaction's own inputs.
  const extraIds = new Set();
  for (const sig of signatures) {
    const info = decode_move_authenticator_objects(sig);
    if (!info) continue;
    log(`  move-authenticator extras: ${info.input_object_ids.length} input + 1 auth-function field`);
    for (const id of info.input_object_ids) extraIds.add(id);
    extraIds.add(info.auth_function_field_id);
  }

  log("> fetching chain info…");
  const info = await fetchChainInfo(endpoint);
  log(`  protocol_version=${info.protocol_version} epoch=${info.epoch_id} rgp=${info.reference_gas_price}`);

  log(`> fetching ${decoded.object_ids.length} input objects…`);
  const tFetch0 = performance.now();
  const fetchedIds = new Set(decoded.object_ids.map((i) => i.toLowerCase()));
  const objects = [];
  for (const id of decoded.object_ids) {
    const obj = await fetchObjectBcs(endpoint, id);
    if (!obj?.bcs) {
      log(`  ! ${id} not found (or has no BCS), skipping`, "warn");
      continue;
    }
    objects.push({ bcs_b64: obj.bcs });
  }
  log(`  fetched ${objects.length} objects`);

  if (decoded.shared_object_ids.length) {
    log(`> walking dynamic-field children of shared objects…`);
    const children = await fetchAllDescendants(endpoint, decoded.shared_object_ids);
    for (const c of children) {
      objects.push({ bcs_b64: c.bcs_b64 });
      fetchedIds.add(c.id.toLowerCase());
    }
    log(`  fetched ${children.length} dynamic-field children`);
  }

  if (extraIds.size) {
    let extraCount = 0;
    for (const id of extraIds) {
      if (fetchedIds.has(id.toLowerCase())) continue;
      const obj = await fetchObjectBcs(endpoint, id);
      if (!obj?.bcs) {
        log(`  ! auth-related object ${id} not found, skipping`, "warn");
        continue;
      }
      objects.push({ bcs_b64: obj.bcs });
      fetchedIds.add(id.toLowerCase());
      extraCount++;
    }
    log(`  fetched ${extraCount} signature-related objects`);
  }
  const fetchMs = performance.now() - tFetch0;
  log(`  dependency objects fetched in ${fetchMs.toFixed(1)} ms`);

  log("> running simulation in wasm…");
  const t0 = performance.now();
  const sim = simulate({
    tx_b64: tx,
    protocol_version: info.protocol_version,
    reference_gas_price: info.reference_gas_price,
    epoch_id: info.epoch_id,
    epoch_timestamp_ms: info.epoch_timestamp_ms,
    objects,
    strict,
    signatures,
  });
  const simMs = performance.now() - t0;
  log(`  simulation finished in ${simMs.toFixed(1)} ms`, sim.success ? "ok" : "err");
  log(`  total: fetch ${fetchMs.toFixed(1)} ms + simulation ${simMs.toFixed(1)} ms = ${(fetchMs + simMs).toFixed(1)} ms`);

  renderResults(decoded, sim, signatures.length > 0);
}

// Fixture-mode runner: everything (tx + signatures + chain info + objects)
// comes from a pre-baked JSON file produced by the `iota-local-executor`
// integration test. Skips the network entirely so the demo works offline.
async function runFixture(fixturePath) {
  log(`> loading fixture: ${fixturePath}`);
  const res = await fetch(fixturePath);
  if (!res.ok) throw new Error(`fixture HTTP ${res.status}`);
  const fixture = await res.json();
  log(`  ${fixture.name}: ${fixture.description}`);

  const strict = document.getElementById("strict").checked;

  log("> decoding transaction in wasm…");
  const decoded = decode_transaction(fixture.tx_b64);
  log(`  sender:        ${decoded.sender}`);
  log(`  gas budget:    ${decoded.gas_budget}`);
  log(`  signatures:    ${fixture.signatures.length}`);
  log(`  objects:       ${fixture.objects.length} (preloaded)`);

  log("> running simulation in wasm…");
  const t0 = performance.now();
  const sim = simulate({
    tx_b64: fixture.tx_b64,
    protocol_version: Number(fixture.protocol_version),
    reference_gas_price: Number(fixture.reference_gas_price),
    epoch_id: Number(fixture.epoch_id),
    epoch_timestamp_ms: Number(fixture.epoch_timestamp_ms),
    objects: fixture.objects.map((o) => ({ bcs_b64: o.bcs_b64 })),
    strict,
    signatures: fixture.signatures,
  });
  const simMs = performance.now() - t0;
  log(`  simulation finished in ${simMs.toFixed(1)} ms`, sim.success ? "ok" : "err");

  renderResults(decoded, sim, fixture.signatures.length > 0);

  // Mirror the loaded fixture into the textareas so the user can inspect it.
  document.getElementById("txInput").value = fixture.tx_b64;
  document.getElementById("sigInput").value = fixture.signatures.join("\n");
}

async function run() {
  clearLog();
  clearResults();
  const button = document.getElementById("run");
  button.disabled = true;
  try {
    await runLive();
  } catch (e) {
    console.error(e);
    log(`! ${e.message || e}`, "err");
  } finally {
    button.disabled = false;
  }
}

async function loadFixture(path) {
  clearLog();
  clearResults();
  try {
    await runFixture(path);
  } catch (e) {
    console.error(e);
    log(`! ${e.message || e}`, "err");
  }
}

(async () => {
  log("Loading wasm…");
  await init();
  log("Ready.", "ok");

  document.getElementById("run").addEventListener("click", run);
  document.getElementById("loadStake").addEventListener("click", () => {
    document.getElementById("txInput").value = SAMPLE_STAKE_TX;
    document.getElementById("sigInput").value = "";
    document.getElementById("network").value = "devnet";
  });
  document.getElementById("loadMoveAuthValid").addEventListener("click", () => {
    loadFixture(SAMPLE_FIXTURES.moveAuthValid);
  });
  document.getElementById("loadMoveAuthInvalid").addEventListener("click", () => {
    loadFixture(SAMPLE_FIXTURES.moveAuthInvalid);
  });
})();
