// Runs one ayaito flow for ukai-relay: `node runner.mjs <flow> <runs> <id>`.
//
// The flow file's default export is an ayaito node, or a function of what
// ukai gives it ({ runtime, input, runId }) returning one. `runtime` is an
// ayaito AgentRuntime whose calls are turns of the workspace's agent, asked
// of the relay, so each agent node is a session the user can open.
//
// It leaves in <runs>/<id>/: graph.json (the flow's nodes as they are drawn),
// events.jsonl and checkpoints (ayaito's FsStore), and result.json (how the
// run ended). The relay reads those; nothing else is spoken between the two.
import { AsyncLocalStorage } from "node:async_hooks";
import { readFile, writeFile } from "node:fs/promises";
import http from "node:http";
import { createRequire, registerHooks } from "node:module";
import { join } from "node:path";
import { pathToFileURL } from "node:url";

const [flowFile, runsDir, runId] = process.argv.slice(2);
const dir = join(runsDir, runId);

// A `node_modules` to fall back on for what a flow imports and its own
// directories do not have: the image's ayaito, for a checkout without one
// and for the scripts the relay keeps outside the checkout.
const fallback = process.env.UKAI_WORKFLOW_MODULES;
const fromFallback = fallback && pathToFileURL(join(fallback, "_")).href;
if (fromFallback) {
  registerHooks({
    resolve(specifier, context, next) {
      try {
        return next(specifier, context);
      } catch (error) {
        const bare = !/^(\.|\/|file:|node:|data:)/.test(specifier);
        if (!bare || error?.code !== "ERR_MODULE_NOT_FOUND") throw error;
        return next(specifier, { ...context, parentURL: fromFallback });
      }
    },
  });
}

/** The node whose agent call is being made: set as each node starts. */
const current = new AsyncLocalStorage();

const abort = new AbortController();
for (const signal of ["SIGTERM", "SIGINT"]) process.on(signal, () => abort.abort());

/** One turn of the workspace's agent. Waits as long as the turn takes. */
function turn(body, signal) {
  return new Promise((resolve, reject) => {
    const url = new URL("/workflows/agent", process.env.UKAI_WORKFLOW_URL);
    const headers = { "content-type": "application/json" };
    if (process.env.UKAI_RELAY_TOKEN) headers.authorization = `Bearer ${process.env.UKAI_RELAY_TOKEN}`;
    const request = http.request(url, { method: "POST", headers, signal }, (response) => {
      const chunks = [];
      response.on("data", (chunk) => chunks.push(chunk));
      response.on("error", reject);
      response.on("end", () => {
        const text = Buffer.concat(chunks).toString("utf8");
        if (response.statusCode !== 200) return reject(new Error(text || `the relay answered ${response.statusCode}`));
        try {
          resolve(JSON.parse(text));
        } catch (error) {
          reject(error);
        }
      });
    });
    request.on("error", reject);
    request.end(JSON.stringify(body));
  });
}

/** ACP has no system prompt and no output format: both go into the prompt. */
export function compose({ prompt, system, outputSchema }) {
  const parts = [];
  if (system) parts.push(system);
  parts.push(prompt);
  if (outputSchema) {
    parts.push(
      "End your answer with one fenced ```json block holding a single JSON value that matches this JSON Schema, and write nothing after it:\n" +
        JSON.stringify(outputSchema),
    );
  }
  return parts.join("\n\n");
}

/** The JSON value an answer ends with: its last ```json block, or else all of it. */
export function extract(text) {
  const blocks = [...text.matchAll(/```(?:json)?\s*\n([\s\S]*?)```/g)];
  for (const candidate of [blocks.at(-1)?.[1], text]) {
    if (candidate === undefined) continue;
    try {
      return JSON.parse(candidate);
    } catch {
      // Not JSON: the schema's validation reports the missing answer.
    }
  }
  return undefined;
}

/** The flow's nodes in the order they are drawn, each with its depth. */
export function outline(node, depth = 0, edge = undefined, out = []) {
  const shape = node.shape ?? { kind: "fn" };
  if (shape.kind === "assign") return outline(shape.node, depth, edge, out);
  out.push({ name: node.name, kind: shape.kind, depth, edge: edge || undefined });
  if (shape.kind === "flow") {
    for (const step of shape.steps) {
      outline(step.node, depth + 1, [...(step.map ? ["map"] : []), ...step.use].join(", "), out);
    }
  } else if (shape.kind === "parallel") {
    for (const branch of Object.values(shape.branches)) outline(branch, depth + 1, shape.use.join(", "), out);
  } else if (shape.kind === "until") {
    outline(shape.body, depth + 1, `at most ${shape.max} times`, out);
  }
  return out;
}

async function load() {
  let ayaito;
  try {
    // The copy the flow itself imports, so that its Stop and OutputError are
    // the ones this run tells by class: the flow's own first, then the fallback.
    let found;
    try {
      found = pathToFileURL(createRequire(flowFile).resolve("ayaito")).href;
    } catch {
      found = "ayaito";
    }
    ayaito = await import(found);
  } catch (error) {
    throw new Error(
      `ayaito is not installed where ${flowFile} can import it. Run \`npm install ayaito zod\` in the checkout. (${error.message})`,
    );
  }
  if (typeof ayaito.defineRuntime !== "function") {
    throw new Error("ayaito 0.2 or later is needed: the one installed has no defineRuntime");
  }
  const runtime = ayaito.defineRuntime({
    name: "ukai",
    async run(request) {
      const answer = await turn(
        {
          run: runId,
          node: current.getStore(),
          prompt: compose(request),
          model: request.model,
          access: request.access,
          session: request.sessionId,
        },
        request.signal,
      );
      return {
        text: answer.text,
        sessionId: answer.session,
        structured: request.outputSchema ? extract(answer.text) : undefined,
      };
    },
  });
  const input = JSON.parse(await readFile(join(dir, "input.json"), "utf8"));
  const exported = (await import(pathToFileURL(flowFile).href)).default;
  const flow = typeof exported === "function" ? await exported({ runtime, input, runId }) : exported;
  if (!flow || typeof flow.run !== "function" || typeof flow.name !== "string") {
    throw new Error(
      `${flowFile} must export an ayaito node as its default, or a function ({ runtime, input, runId }) returning one`,
    );
  }
  return { ayaito, flow, input };
}

async function main() {
  const { ayaito, flow, input } = await load();
  await writeFile(join(dir, "graph.json"), JSON.stringify(outline(flow)));
  const result = await ayaito.run(flow, input, {
    runId,
    store: new ayaito.FsStore(runsDir),
    signal: abort.signal,
    log: (event) => {
      // A node's start is recorded in the async context the node then runs in.
      if (event.type === "start") current.enterWith(event.node);
    },
  });
  return result.status === "done"
    ? { status: "done", output: result.output ?? null }
    : { status: "stopped", node: result.node, reason: result.reason };
}

if (flowFile) {
  const result = await main().catch((error) => {
    if (abort.signal.aborted) return { status: "cancelled" };
    // The trace goes to the run's log; the run says only what went wrong.
    console.error(error);
    return { status: "failed", error: String(error?.message ?? error) };
  });
  await writeFile(join(dir, "result.json"), JSON.stringify(result));
  process.exit(0);
}
