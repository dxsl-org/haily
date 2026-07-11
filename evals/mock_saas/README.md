# Mock SaaS — Automation/Connector Golden Eval (phase 14)

A local, deterministic, **zero-network** stand-in for the SaaS backends Haily's manifest
connectors talk to, used to measure Haily's actual value-add on its own connector surface
(AutomationBench *methodology*, not an AutomationBench *score* — see below).

## Why there is NO raw "Haily AutomationBench score"

AutomationBench drives a **raw model endpoint through its own agent loop** against its own
simulated tools. It has no concept of an approval gate or reversibility. Pointed at Haily's
model backend it would bypass every Haily differentiator (manifest connectors, RiskTier,
ApprovalGate, action journal + undo), and a correctly-behaving Haily that **pauses for approval**
on a destructive step is scored as *non-completion* — a safe agent scores **worse**. A headline
"Haily got X%" from that harness therefore measures the underlying LLM, not the assistant.

Its two legitimate uses: (1) per-model AA scores as a model→tier signal, folded into **P3**;
(2) its **methodology** — deterministic objective + guardrail assertions, no LLM-judge,
reward-hacking guardrails — ported onto Haily's own connector surface, which is **this eval**.

## Authoritative mock

The authoritative, CI-runnable mock lives IN-CRATE at
`crates/haily-core/src/pipeline/automation_eval/mock.rs` (a loopback `MockSaas` server), so
`cargo test --workspace` is self-contained and needs no external process. It speaks the exact
Odoo `execute_kw` JSON-RPC dialect the shipped `odoo-crm` manifest produces, so the generic
`HttpConnectorTool` interprets a manifest against it as a drop-in target — no connector code
changes (the manifest's base URL is pointed at the mock by the eval-mode, origin-gated override).

## Fixtures

- `odoo-eval.manifest.json` — the eval connector manifest: the shipped `odoo-crm` protocol +
  real CRM ops verbatim, plus two eval-only destructive ops (`odoo_contact_delete`,
  `odoo_contact_bulk_archive`) that exercise the RiskTier/ApprovalGate + reward-hack
  differentiators. No `auth` (the mock needs none); loopback base-URL placeholder.
- `../automation/*.yaml` — task fixtures (JSON-in-YAML): seed state, a scripted connector-call
  sequence, objective + guardrail assertions, and the differentiator expectations (approval
  fired, journal complete, undo restores, reward-hack trap).

## Two-tier deliverable (honest split, mirrors P9)

- **CI tier (green now):** `crates/haily-core/tests/automation_goldens.rs` drives the scripted
  connector steps of each fixture through the REAL dispatch harness against the in-crate mock,
  scored deterministically. Zero network.
- **Per-candidate-MODEL matrix (DEFERRED, model-host-gated):** a real model GENERATES the tool
  calls (instead of the scripted `steps`) and is scored by the SAME runner + assertions. The
  runner, fixtures, scoring, and `eval_runs` persistence are built; the matrix run itself needs
  a configured local/cloud model host not present in the CI build env.

## Metrics (NON-COMPARABLE by construction)

Both AutomationBench headline lenses are reported and labelled non-comparable:
- **Partial-credit** (Artificial-Analysis lens): objective pass rate, zeroed by any guardrail
  violation.
- **Strict-binary** (Zapier lens): 1 iff every objective passes AND no guardrail is violated.
