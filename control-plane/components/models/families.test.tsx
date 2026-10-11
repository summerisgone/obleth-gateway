import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { deleteModelFamilyAction, saveModelFamilyAction } from "@/app/actions";
import { FamiliesSection } from "./families";
import type { ModelFamily, ModelRoute } from "@/lib/obleth";

vi.mock("@/app/actions", () => ({
  saveModelFamilyAction: vi.fn(async () => ({ ok: true })),
  deleteModelFamilyAction: vi.fn(async () => ({ ok: true })),
}));
const refresh = vi.fn();
vi.mock("next/navigation", () => ({ useRouter: () => ({ refresh }) }));

const models = [
  { id: "1", model_name: "acme-7b-instruct", model_type: "chat", aliases: [] },
  { id: "2", model_name: "acme-7b-think", model_type: "chat", aliases: ["acme-think"] },
  { id: "3", model_name: "acme-embed", model_type: "embedding", aliases: [] },
] as unknown as ModelRoute[];

const family: ModelFamily = {
  id: "f1",
  name: "acme-7b",
  description: "Instruct when no thinking is wanted.",
  default_level: "none",
  members: { none: "acme-7b-instruct", low: "acme-7b-think", medium: "acme-7b-think", high: "acme-7b-think" },
  created_at: "",
  updated_at: "",
};

let root: Root;
let host: HTMLDivElement;

beforeEach(() => {
  Object.assign(globalThis, { IS_REACT_ACT_ENVIRONMENT: true });
  host = document.createElement("div");
  document.body.appendChild(host);
  root = createRoot(host);
  vi.mocked(saveModelFamilyAction).mockClear();
  vi.mocked(deleteModelFamilyAction).mockClear();
  refresh.mockClear();
});
afterEach(() => {
  act(() => root.unmount());
  host.remove();
});

async function render(families: ModelFamily[] | null) {
  await act(async () => {
    root.render(<FamiliesSection families={families} models={models} />);
  });
}

const button = (text: string) => [...document.querySelectorAll("button")].find((b) => b.textContent?.trim() === text)!;
const sheet = () => document.querySelector<HTMLElement>('[role="dialog"]')!;
/** A button in the sheet's own footer, where "Add family" also appears. */
const sheetButton = (text: string) => [...sheet().querySelectorAll("button")].find((b) => b.textContent?.trim() === text)!;

async function pick(label: string, text: string) {
  const trigger = document.querySelector<HTMLButtonElement>(`[aria-label="${label}"]`)!;
  await act(async () => { trigger.focus(); trigger.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true })); });
  const option = [...document.querySelectorAll<HTMLElement>('[role="menuitem"]')].find((o) => o.textContent?.startsWith(text))!;
  await act(async () => option.click());
}

async function type(el: HTMLInputElement, value: string) {
  await act(async () => {
    Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!.call(el, value);
    el.dispatchEvent(new Event("input", { bubbles: true }));
  });
}

describe("FamiliesSection", () => {
  it("lists each family with who answers at each level", async () => {
    await render([family]);
    expect(host.textContent).toContain("acme-7b");
    expect(host.textContent).toContain("served as none");
    expect(host.textContent).toContain("none → acme-7b-instruct · low, medium, high → acme-7b-think");
  });

  it("says so when there are none, or the gateway could not list them", async () => {
    await render([]);
    expect(host.textContent).toContain("No families yet.");
    await render(null);
    expect(host.textContent).toContain("did not list model families");
    expect(host.textContent).not.toContain("Add family");
  });

  it("adds a family from chat models only, with a preview", async () => {
    await render([]);
    await act(async () => button("Add family").click());
    expect(sheet().textContent).toContain("Pick a model for every level");
    await type(sheet().querySelector<HTMLInputElement>("#field-family_name")!, "acme-7b");
    await pick("Model for none", "acme-7b-instruct");
    for (const level of ["low", "medium", "high"]) await pick(`Model for ${level}`, "acme-7b-think");
    expect([...document.querySelectorAll('[role="menuitem"]')].some((o) => o.textContent?.includes("acme-embed"))).toBe(false);
    await pick("When a caller sets no effort", "Serve it as none");
    expect(sheet().textContent).toContain("A request for acme-7b with no effort set goes to acme-7b-instruct.");
    expect(sheet().textContent).toContain("none → acme-7b-instruct · low, medium, high → acme-7b-think");
    await act(async () => sheetButton("Add family").click());
    expect(saveModelFamilyAction).toHaveBeenCalledWith(null, {
      name: "acme-7b",
      description: "",
      default_level: "none",
      members: family.members,
    });
    expect(refresh).toHaveBeenCalled();
    expect(document.querySelector('[role="dialog"]')).toBeNull();
  });

  it("refuses a name a model already answers to, before asking the gateway", async () => {
    await render([]);
    await act(async () => button("Add family").click());
    await type(sheet().querySelector<HTMLInputElement>("#field-family_name")!, "acme-think");
    await act(async () => sheetButton("Add family").click());
    expect(saveModelFamilyAction).not.toHaveBeenCalled();
    expect(sheet().querySelector('[role="alert"]')!.textContent).toBe("A model already answers to that name.");
  });

  it("edits a family and shows the gateway's refusal as written", async () => {
    vi.mocked(saveModelFamilyAction).mockResolvedValueOnce({ ok: false, error: "`high`: 'acme-embed' is a `embedding` model; a family's members must be chat models" });
    await render([family]);
    await act(async () => button("acme-7b").click());
    expect(sheet().getAttribute("aria-label")).toBe("Edit acme-7b");
    await act(async () => button("Save family").click());
    expect(saveModelFamilyAction).toHaveBeenCalledWith("f1", { name: "acme-7b", description: family.description, default_level: "none", members: family.members });
    expect(sheet().querySelector('[role="alert"]')!.textContent).toContain("a family's members must be chat models");
  });

  it("asks before deleting a family", async () => {
    await render([family]);
    await act(async () => button("acme-7b").click());
    await act(async () => button("Delete family…").click());
    expect([...document.querySelectorAll('[role="dialog"]')].some((d) => d.textContent?.includes("Delete acme-7b?"))).toBe(true);
    await act(async () => button("Delete").click());
    expect(deleteModelFamilyAction).toHaveBeenCalledWith("f1");
    expect(refresh).toHaveBeenCalled();
  });
});
