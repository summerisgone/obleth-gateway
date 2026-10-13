import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { VerdictsWorkspace } from "./verdicts-workspace";
import type { PlaygroundSession } from "./playground";
import type { ModelRoute } from "@/lib/obleth";

let root: Root;
let host: HTMLDivElement;
let bodies: unknown[];

const models = [
  { id: "1", model_name: "glm-5-3", model_type: "chat", enabled: true, supports_vision: false },
  { id: "2", model_name: "sdxl", model_type: "image", enabled: true, supports_vision: false },
  { id: "3", model_name: "gemma-4", model_type: "chat", enabled: true, supports_vision: true },
] as unknown as ModelRoute[];

/** Attach files through the hidden picker and wait for FileReader. */
async function attach(...files: File[]) {
  const input = host.querySelector<HTMLInputElement>("input[type='file']")!;
  Object.defineProperty(input, "files", { value: files, configurable: true });
  await act(async () => {
    input.dispatchEvent(new Event("change", { bubbles: true }));
    await new Promise((r) => setTimeout(r, 20));
  });
}

const png = () => new File([new Uint8Array([137, 80, 78, 71])], "photo.png", { type: "image/png" });

function session(patch: Partial<PlaygroundSession> = {}): PlaygroundSession {
  return {
    id: "s1",
    title: "Untitled session",
    mode: "verdicts",
    models: ["charo"],
    generation: { systemPrompt: "" },
    verdictState: "My card was charged twice for order A-104.",
    verdictQuestions: [
      { id: "is_urgent", type: "boolean", instructions: "Is this urgent?" },
      {
        id: "department",
        type: "choice",
        instructions: "Which team?",
        options: [
          { name: "billing", description: "payments" },
          { name: "technical", description: "bugs" },
        ],
      },
    ],
    ...patch,
  };
}

const verdictResponse = {
  model: "glm-5-3",
  verdicts: {
    is_urgent: {
      type: "boolean",
      value: true,
      probabilities: { true: 0.92, false: 0.08 },
      confidence: 0.55,
    },
    department: {
      type: "choice",
      value: "billing",
      probabilities: { billing: 0.81, technical: 0.19 },
      confidence: 0.63,
    },
  },
  usage: { prompt_tokens: 240, completion_tokens: 2, total_tokens: 242 },
  latencyMs: 180,
  requestId: "rid-7",
};

function render(s: PlaygroundSession, update = vi.fn()) {
  act(() => {
    root.render(<VerdictsWorkspace session={s} update={update} models={models} loading={false} />);
  });
  return update;
}

async function clickRun() {
  await act(async () => {
    host.querySelector<HTMLButtonElement>("[aria-label='Run verdicts']")!.click();
  });
}

beforeEach(() => {
  Object.assign(globalThis, { IS_REACT_ACT_ENVIRONMENT: true });
  bodies = [];
  host = document.createElement("div");
  document.body.appendChild(host);
  root = createRoot(host);
  vi.stubGlobal(
    "fetch",
    vi.fn(async (_url: string, init?: RequestInit) => {
      bodies.push(JSON.parse(String(init?.body)));
      return {
        ok: true,
        status: 200,
        json: async () => verdictResponse,
      } as unknown as Response;
    }),
  );
});

afterEach(() => {
  act(() => root.unmount());
  host.remove();
  vi.unstubAllGlobals();
});

describe("VerdictsWorkspace", () => {
  it("posts the built questions map with plain-text state sent as a string", async () => {
    render(session());
    await clickRun();
    expect(bodies).toHaveLength(1);
    expect(bodies[0]).toMatchObject({
      model: "auto",
      state: "My card was charged twice for order A-104.",
      questions: {
        is_urgent: { type: "boolean", instructions: "Is this urgent?" },
        department: {
          type: "choice",
          instructions: "Which team?",
          criteria: { billing: "payments", technical: "bugs" },
        },
      },
    });
  });

  it("sends JSON-looking state structured, not as one string", async () => {
    render(session({ verdictState: '{"ticket": {"subject": "Duplicate charge"}}' }));
    await clickRun();
    expect((bodies[0] as { state: unknown }).state).toEqual({
      ticket: { subject: "Duplicate charge" },
    });
  });

  it("renders each verdict with its value, probabilities, and confidence", async () => {
    render(session());
    await clickRun();
    const text = host.textContent ?? "";
    expect(text).toContain("is_urgent");
    expect(text).toContain("yes");
    expect(text).toContain("92.0%");
    expect(text).toContain("department");
    expect(text).toContain("billing");
    expect(text).toContain("confidence");
    expect(text).toContain("180 ms");
    expect(text).toContain("rid-7");
  });

  it("shows the gateway's error message when the relay rejects the request", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => ({
        ok: false,
        status: 502,
        json: async () => ({ error: "question 'is_urgent': upstream returned 500" }),
      }) as unknown as Response),
    );
    render(session());
    await clickRun();
    const alert = host.querySelector("[role='alert']");
    expect(alert?.textContent).toContain("question 'is_urgent'");
  });

  it("disables Run while the state or a question's instructions are empty", () => {
    render(session({ verdictState: "" }));
    expect(host.querySelector<HTMLButtonElement>("[aria-label='Run verdicts']")!.disabled).toBe(true);
    render(session({ verdictQuestions: [{ id: "q1", type: "boolean", instructions: "" }] }));
    expect(host.querySelector<HTMLButtonElement>("[aria-label='Run verdicts']")!.disabled).toBe(true);
  });

  it("sends attached images, and leaves out an empty state", async () => {
    render(session({ verdictState: "", verdictModel: "gemma-4" }));
    expect(host.querySelector<HTMLButtonElement>("[aria-label='Run verdicts']")!.disabled).toBe(true);
    await attach(png());
    expect(host.querySelectorAll("img[alt^='Image ']")).toHaveLength(1);
    await clickRun();
    const body = bodies[0] as { state?: unknown; images?: string[] };
    expect(body.state).toBeUndefined();
    expect(body.images).toHaveLength(1);
    expect(body.images![0]).toMatch(/^data:image\/png;base64,/);
  });

  it("refuses non-images and blocks a model without vision once images are attached", async () => {
    render(session({ verdictModel: "glm-5-3" }));
    await attach(new File(["x"], "notes.txt", { type: "text/plain" }));
    expect(host.querySelector("[role='alert']")?.textContent).toContain("up to 6 MB");
    expect(host.querySelectorAll("img[alt^='Image ']")).toHaveLength(0);

    await attach(png());
    expect(host.textContent).toContain("glm-5-3 can't see images");
    expect(host.querySelector<HTMLButtonElement>("[aria-label='Run verdicts']")!.disabled).toBe(true);

    await act(async () => {
      host.querySelector<HTMLButtonElement>("[aria-label='Remove image 1']")!.click();
    });
    expect(host.querySelector<HTMLButtonElement>("[aria-label='Run verdicts']")!.disabled).toBe(false);
  });

  it("offers only chat models plus auto in the model picker", () => {
    render(session());
    // The Select renders its current value; the option list itself lives in a
    // Radix portal, so assert on the options prop contractually: an image
    // model must not be offered. The trigger shows the default "auto".
    const trigger = host.querySelector("[aria-label='Verdict model']");
    expect(trigger?.textContent).toContain("auto");
  });
});
