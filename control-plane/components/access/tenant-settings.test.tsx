// @vitest-environment jsdom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { saveTenantSettingsAction } from "@/app/actions";
import { TenantSettings } from "./tenant-settings";
import type { GenaiExportStatus, Tenant } from "@/lib/obleth";

vi.mock("@/app/actions", () => ({ saveTenantSettingsAction: vi.fn(async () => ({ ok: true })) }));

const tenant = (over: Partial<Tenant> = {}): Tenant => ({
  id: "t1", name: "rcusers", fairshare_group: "default", weight: 150, tokens_per_minute: 50000, max_in_flight: 8,
  description: "", organization: "ASU", contact_email: "", status: "active", timezone: "America/Phoenix",
  active_from: "2026-08-17T07:00:00Z", active_until: null,
  weekly_windows: [1, 2, 3, 4, 5].map((day) => ({ day, start_min: 420, end_min: 1320 })),
  budget_tokens: null, budget_cost_usd: 300, budget_period: "monthly", budget_started_at: null,
  allowed_models: null, guardrails_policy: null, compression_policy: null, tracing_enabled: false, synthetic: false, genai_export: false,
  created_at: "", updated_at: "", ...over,
});

let root: Root;
let host: HTMLDivElement;
const frame = () => act(async () => { await new Promise((r) => requestAnimationFrame(() => r(null))); });

beforeEach(() => {
  Object.assign(globalThis, { IS_REACT_ACT_ENVIRONMENT: true });
  vi.mocked(saveTenantSettingsAction).mockClear();
  host = document.createElement("div");
  document.body.appendChild(host);
  root = createRoot(host);
});
afterEach(async () => {
  await act(async () => root.unmount());
  host.remove();
});

async function render(t = tenant(), genai: GenaiExportStatus | null = null) {
  await act(async () => root.unmount());
  root = createRoot(host);
  const others = [t, tenant({ id: "t2", name: "brain-training", weight: 100 }), tenant({ id: "t3", name: "svc", weight: 100 })];
  await act(async () => root.render(<TenantSettings tenant={t} tenants={others} models={["m1", "m2"]} budget={null} genai={genai} onDirty={() => {}} onSaved={() => {}} />));
  await frame();
}

async function type(name: string, value: string) {
  const el = host.querySelector<HTMLInputElement>(`[name="${name}"]`)!;
  await act(async () => {
    Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!.call(el, value);
    el.dispatchEvent(new Event("input", { bubbles: true }));
  });
  await frame();
}

describe("a tenant's settings", () => {
  it("opens with nothing to save, even with hours, dates and a budget set", async () => {
    await render();
    expect(document.querySelector('[aria-label="Unsaved changes"]')).toBeNull();
  });

  it("previews the weight's share and saves only what changed", async () => {
    await render();
    await type("weight", "200");
    expect(host.textContent).toContain("50%");
    const bar = document.querySelector('[aria-label="Unsaved changes"]')!;
    expect(bar.textContent).toContain("1 unsaved");
    expect(bar.textContent).toContain("Weight");
    const save = [...bar.querySelectorAll("button")].find((b) => b.textContent === "Save changes")!;
    await act(async () => save.click());
    const sent = vi.mocked(saveTenantSettingsAction).mock.calls[0][0];
    expect(sent.get("sections")).toBe("weight");
    expect(sent.get("weight")).toBe("200");
  });

  it("opens hours with the week grid's preset and sends them as windows", async () => {
    await render();
    const preset = [...host.querySelectorAll("button")].find((b) => b.textContent === "Weekdays 9–17")!;
    await act(async () => preset.click());
    await frame();
    const windows = JSON.parse(host.querySelector<HTMLInputElement>('[name="weekly_windows"]')!.value);
    expect(windows).toHaveLength(5);
    expect(windows[0]).toEqual({ day: 1, start_min: 540, end_min: 1020 });
    expect(document.querySelector('[aria-label="Unsaved changes"]')!.textContent).toContain("Weekly hours");
  });

  it("saves the GenAI export flag on its own and names the scrub mode", async () => {
    await render(tenant(), { configured: true, scrub: "regex" });
    const note = host.querySelector("#set-genai-export")!.textContent!;
    expect(note).toContain("OBLETH_GENAI_SCRUB=regex");
    expect(note).toContain("Names and addresses stay");
    await act(async () => host.querySelector<HTMLInputElement>('[name="genai_export"]')!.click());
    await frame();
    const bar = document.querySelector('[aria-label="Unsaved changes"]')!;
    await act(async () => [...bar.querySelectorAll("button")].find((b) => b.textContent === "Save changes")!.click());
    const sent = vi.mocked(saveTenantSettingsAction).mock.calls[0][0];
    expect(sent.get("sections")).toBe("genai");
    expect(sent.get("genai_export")).toBe("on");
  });

  it("says when the gateway has no GenAI collector", async () => {
    await render(tenant({ genai_export: true }), { configured: false, scrub: null });
    expect(host.querySelector("#set-genai-export")!.textContent).toContain("OBLETH_GENAI_OTLP_ENDPOINT is not set");
  });

  it("names the invalid GenAI setting instead of a missing endpoint", async () => {
    const error = "OBLETH_GENAI_SCRUB_URL: required for ner and llm modes";
    await render(tenant({ genai_export: true }), { configured: false, scrub: null, error });
    const note = host.querySelector("#set-genai-export")!.textContent!;
    expect(note).toContain(error);
    expect(note).not.toContain("OBLETH_GENAI_OTLP_ENDPOINT is not set");
  });

  it("won't save a harmful-content scanner without a guard model", async () => {
    await render(tenant({ guardrails_policy: { action: "block", input_scanners: ["harm"], output_scanners: [], guard_model: null, ban_keywords: [], fail_open: true } }));
    await type("weight", "151");
    const bar = document.querySelector('[aria-label="Unsaved changes"]')!;
    await act(async () => [...bar.querySelectorAll("button")].find((b) => b.textContent === "Save changes")!.click());
    expect(saveTenantSettingsAction).not.toHaveBeenCalled();
    expect(bar.textContent).toContain("pick a guard model");
  });
  const piiPolicy = (over: Record<string, unknown> = {}) =>
    ({ action: "redact", input_scanners: ["pii"], output_scanners: [], guard_model: null, ban_keywords: [], fail_open: true, ...over }) as Tenant["guardrails_policy"];
  const status = (over: Partial<GenaiExportStatus> = {}): GenaiExportStatus => ({ configured: true, scrub: "off", profiles: ["ner", "off", "regex", "ru"], ...over });

  it("trace_profile_select_inherit", async () => {
    await render(tenant(), status());
    const sel = host.querySelector<HTMLSelectElement>('select[name="pii_trace_profile"]')!;
    expect(sel.value).toBe("");
    expect([...sel.options].map((o) => o.value)).toEqual(["", "ner", "off", "regex", "ru"]);
    expect(host.querySelector("#set-genai-export")!.textContent).toContain("Effective trace profile: gateway default");
  });

  it("hides the trace profile select on older gateways", async () => {
    await render(tenant(), { configured: true, scrub: "regex" });
    expect(host.querySelector('select[name="pii_trace_profile"]')).toBeNull();
  });

  it("effective_line_says_strengthened", async () => {
    await render(tenant({ pii_trace_profile: "off", guardrails_policy: piiPolicy() }), status());
    expect(host.querySelector("#set-genai-export")!.textContent).toContain("off+regex, strengthened by guardrails");
  });

  it("does not strengthen for log_only or a custom profile", async () => {
    await render(tenant({ pii_trace_profile: "off", guardrails_policy: piiPolicy({ action: "log_only" }) }), status());
    expect(host.querySelector("#set-genai-export")!.textContent).not.toContain("strengthened");
    await render(tenant({  pii_trace_profile: "ru", guardrails_policy: piiPolicy() }), status());
    expect(host.querySelector("#set-genai-export")!.textContent).toContain("plus guardrails profile regex");
  });

  it("llm_deprecated_warning", async () => {
    await render(tenant(), status({ scrub: "llm", deprecated: "llm" }));
    const note = host.querySelector("#set-genai-export")!.textContent!;
    expect(note).toContain("deprecated and will be removed in the next release");
    expect(note).toContain("use NER");
  });

  it("guardrails_ner_fail_open_warning", async () => {
    const warn = "If the NER service is down, requests pass unredacted";
    await render(tenant({ guardrails_policy: piiPolicy({ pii_profile: "ner" }) }), status());
    expect(host.querySelector("#set-guardrails-pii-profile")!.textContent).toContain(warn);
    await render(tenant({  guardrails_policy: piiPolicy({ pii_profile: "ner", fail_open: false }) }), status());
    expect(host.textContent).not.toContain(warn);
    await render(tenant({  guardrails_policy: piiPolicy() }), status());
    expect(host.textContent).not.toContain(warn);
    expect(JSON.parse(host.querySelector<HTMLInputElement>('[name="guardrails_policy"]')!.value)).not.toHaveProperty("pii_profile");
  });

  it("pii_profile_select_only_with_pii_scanner", async () => {
    await render(tenant({ guardrails_policy: piiPolicy({ input_scanners: ["prompt_injection"] }) }), status());
    expect(host.querySelector("#set-guardrails-pii-profile")).toBeNull();
    await render(tenant({  guardrails_policy: piiPolicy({ input_scanners: [], output_scanners: ["pii"] }) }), status());
    expect(host.querySelector("#set-guardrails-pii-profile")).not.toBeNull();
  });
});
