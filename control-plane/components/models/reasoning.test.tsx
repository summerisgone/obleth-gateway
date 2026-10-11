import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { setModelReasoningAction } from "@/app/actions";
import { ReasoningSection } from "./reasoning";
import type { ModelRoute } from "@/lib/obleth";

vi.mock("@/app/actions", () => ({ setModelReasoningAction: vi.fn(async () => ({ ok: true })) }));

const base = { id: "u", model_name: "acme-7b", model_type: "chat" } as unknown as ModelRoute;
const mapped = {
  ...base,
  reasoning: {
    mode: "mapped",
    levels: {
      none: { use: "low" },
      low: { send: { reasoning_effort: "low" } },
      medium: { send: {} },
      high: { send: { reasoning_effort: "max" } },
    },
  },
} as unknown as ModelRoute;

let root: Root;
let host: HTMLDivElement;

beforeEach(() => {
  host = document.createElement("div");
  document.body.appendChild(host);
  root = createRoot(host);
  vi.mocked(setModelReasoningAction).mockClear();
});
afterEach(() => {
  act(() => root.unmount());
  host.remove();
});

async function render(model: ModelRoute = base) {
  const onChanged = vi.fn();
  await act(async () => {
    root.render(<ReasoningSection model={model} onChanged={onChanged} />);
  });
  return onChanged;
}

const radio = (value: string) => host.querySelector<HTMLInputElement>(`input[name="reasoning_mode"][value="${value}"]`)!;
const saveButton = () => [...host.querySelectorAll("button")].find((b) => b.textContent?.includes("Save reasoning"))!;

/** Open a themed Select by its label from the keyboard and pick the option whose text starts with `text`. */
async function pick(label: string, text: string) {
  const trigger = host.querySelector<HTMLButtonElement>(`[aria-label="${label}"]`)!;
  await act(async () => { trigger.focus(); trigger.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true })); });
  const option = [...document.querySelectorAll<HTMLElement>('[role="menuitem"]')].find((o) => o.textContent?.startsWith(text))!;
  await act(async () => option.click());
}

async function type(el: HTMLInputElement | HTMLTextAreaElement, value: string) {
  await act(async () => {
    const proto = el instanceof HTMLTextAreaElement ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype;
    Object.getOwnPropertyDescriptor(proto, "value")!.set!.call(el, value);
    el.dispatchEvent(new Event("input", { bubbles: true }));
  });
}

describe("ReasoningSection", () => {
  it("passes through by default, with nothing to save", async () => {
    await render();
    expect(radio("passthrough").checked).toBe(true);
    expect(host.textContent).toContain("Every request reaches the model as the caller sent it");
    expect(host.textContent).not.toContain("What each level sends");
    expect(saveButton().disabled).toBe(true);
  });

  it("shows a stored map as presets, with a preview in plain words", async () => {
    await render(mapped);
    expect(radio("mapped").checked).toBe(true);
    expect(host.querySelector('[aria-label="High becomes"]')!.textContent).toContain("Send reasoning_effort =");
    expect(host.querySelector<HTMLInputElement>('[aria-label="reasoning_effort value for high"]')!.value).toBe("max");
    expect(host.querySelector('[aria-label="Medium becomes"]')!.textContent).toContain("Server default (send nothing)");
    expect(host.querySelector('[aria-label="None becomes"]')!.textContent).toContain("Same as another level");
    expect(host.textContent).toContain("none → served as low, which sends reasoning_effort: low (this model has no off switch)");
    expect(host.textContent).toContain("high → sends reasoning_effort: max");
    expect(saveButton().disabled).toBe(true);
  });

  it("saves an edited level and a default", async () => {
    const onChanged = await render(mapped);
    await type(host.querySelector<HTMLInputElement>('[aria-label="reasoning_effort value for high"]')!, "xhigh");
    await pick("Medium becomes", "Turn thinking on");
    await pick("When a caller sets no effort", "Serve it as medium");
    expect(host.textContent).toContain("medium → sends chat_template_kwargs.enable_thinking: true");
    expect(host.textContent).toContain("no effort set → served as medium");
    await act(async () => saveButton().click());
    expect(setModelReasoningAction).toHaveBeenCalledWith("u", {
      mode: "mapped",
      default_level: "medium",
      levels: {
        none: { use: "low" },
        low: { send: { reasoning_effort: "low" } },
        medium: { send: { chat_template_kwargs: { enable_thinking: true } } },
        high: { send: { reasoning_effort: "xhigh" } },
      },
    });
    expect(onChanged).toHaveBeenCalled();
  });

  it("starts custom fields from what the level sent, and refuses JSON that isn't an object", async () => {
    await render(mapped);
    await pick("High becomes", "Custom fields");
    const area = host.querySelector<HTMLTextAreaElement>('textarea[aria-label="Custom fields for high"]')!;
    expect(JSON.parse(area.value)).toEqual({ reasoning_effort: "max" });
    await type(area, "[1, 2]");
    expect(host.textContent).toContain("The custom fields must be a JSON object");
    await act(async () => saveButton().click());
    expect(setModelReasoningAction).not.toHaveBeenCalled();
    expect(host.querySelector('[role="alert"]')!.textContent).toMatch(/^High: The custom fields must be a JSON object/);
  });

  it("keeps the levels when switching to no effort control", async () => {
    await render(mapped);
    await act(async () => radio("unsupported").click());
    expect(host.textContent).not.toContain("What each level sends");
    expect(host.textContent).toContain("x-obleth-reasoning-effort: unsupported");
    await act(async () => saveButton().click());
    expect(setModelReasoningAction).toHaveBeenCalledWith("u", { mode: "unsupported", levels: (mapped.reasoning as { levels: unknown }).levels });
  });

  it("shows the gateway's refusal as it was written", async () => {
    vi.mocked(setModelReasoningAction).mockResolvedValueOnce({ ok: false, error: "level `high` may not set `tools`: only effort-related fields can be sent" });
    const onChanged = await render();
    await act(async () => radio("mapped").click());
    await act(async () => saveButton().click());
    expect(setModelReasoningAction).toHaveBeenCalledWith("u", expect.objectContaining({ mode: "mapped" }));
    expect(host.querySelector('[role="alert"]')!.textContent).toBe("level `high` may not set `tools`: only effort-related fields can be sent");
    expect(onChanged).not.toHaveBeenCalled();
  });
});
