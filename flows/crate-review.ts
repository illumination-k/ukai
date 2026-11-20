// Reviews one crate from three sides at once, then gathers what was found.
//
// An ayaito flow (https://github.com/illumination-k/ayaito), declared in
// ukai.toml as `[workflows.crate-review]`. Start it from the Workflows pane
// of a gateway agent's workspace with an input like { "crate": "ukai-core" }.
// Each check is a session of that workspace's agent, which the run lists.
import { agentNode, type AgentRuntime, Flow, fnNode, parallel } from "ayaito";
import { z } from "zod";

type Input = { crate: string };

const findings = z.object({
  findings: z.array(z.object({ file: z.string(), line: z.number(), what: z.string() })),
});

export default ({ runtime }: { runtime: AgentRuntime }) => {
  const check = (name: string, what: string) =>
    agentNode(
      name,
      {
        prompt: (input: Input) =>
          `In crates/${input.crate}, ${what} Report at most five findings, the most important first, and an empty list when there are none.`,
        output: findings,
        // Nothing is edited: what the agent asks permission for is refused.
        access: "read-only",
      },
      runtime,
    );
  return Flow.start<Input>("crate-review")
    .then(
      parallel("checks", {
        errors: check("errors", "find errors that are swallowed, or reported without what the user needs to act on them."),
        tests: check("tests", "find behavior that no test covers."),
        docs: check("docs", "find doc comments that no longer match the code."),
      }),
    )
    .then(
      fnNode("gather", async (found) =>
        Object.entries(found).flatMap(([check, result]) => result.findings.map((finding) => ({ check, ...finding }))),
      ),
    )
    .build();
};
