import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { PlaygroundSession } from "./playground";
import type { ModelRoute } from "@/lib/obleth";

// The viewer needs WebGL, which jsdom lacks; stand in a marker for it.
vi.mock("./mesh-viewer", () => ({
  MeshViewer: ({ url }: { url: string }) => <div data-testid="viewer" data-url={url} />,
}));

const { MeshWorkspace, handOffToMesh } = await import("./mesh-workspace");

let root: Root;
let host: HTMLDivElement;
let calls: { url: string; init?: RequestInit }[];
let jobStatus: Record<string, unknown>;

const models = [
  { id: "1", model_name: "trellis", model_type: "mesh", cost_per_video: 0.05, enabled: true },
  { id: "2", model_name: "sdxl", model_type: "image", cost_per_video: 0, enabled: true },
] as unknown as ModelRoute[];

function session(patch: Partial<PlaygroundSession> = {}): PlaygroundSession {
  return { id: "s1", title: "Untitled session", mode: "mesh", models: ["charo"], generation: { systemPrompt: "" }, ...patch };
}

function render(s: PlaygroundSession, update: (p: Partial<PlaygroundSession>) => void = () => {}) {
  act(() => root.render(<MeshWorkspace session={s} update={update} models={models} loading={false} />));
}

const button = (name: string) => [...host.querySelectorAll("button")].find((b) => b.textContent?.trim() === name)!;

async function chooseImage() {
  const input = host.querySelector<HTMLInputElement>("input[type='file']")!;
  const file = new File(["PNG"], "chair.png", { type: "image/png" });
  Object.defineProperty(input, "files", { value: [file], configurable: true });
  await act(async () => input.dispatchEvent(new Event("change", { bubbles: true })));
}

beforeEach(() => {
  Object.assign(globalThis, { IS_REACT_ACT_ENVIRONMENT: true });
  vi.useFakeTimers({ shouldAdvanceTime: true });
  calls = [];
  jobStatus = { id: "mesh_1", status: "in_progress", stage: "texture", progress: 41 };
  host = document.createElement("div");
  document.body.appendChild(host);
  root = createRoot(host);
  URL.createObjectURL = vi.fn(() => "blob:mesh");
  URL.revokeObjectURL = vi.fn();
  vi.stubGlobal(
    "fetch",
    vi.fn(async (url: string, init?: RequestInit) => {
      calls.push({ url, init });
      if (url === "/api/live/playground/meshes") {
        return { ok: true, status: 200, json: async () => ({ job: { id: "mesh_1", status: "queued", seed: 7 } }) } as unknown as Response;
      }
      if (url.endsWith("/content")) {
        return { ok: true, status: 200, blob: async () => new Blob(["glTF"]) } as unknown as Response;
      }
      if (url.endsWith("/mesh_gone")) return { ok: false, status: 404, json: async () => ({ error: "no such mesh job" }) } as unknown as Response;
      return { ok: true, status: 200, json: async () => ({ job: jobStatus }) } as unknown as Response;
    }),
  );
});

afterEach(() => {
  act(() => root.unmount());
  host.remove();
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

describe("MeshWorkspace", () => {
  it("says how to start when no 3D model is registered", () => {
    act(() => root.render(<MeshWorkspace session={session()} update={() => {}} models={[models[1]]} loading={false} />));
    expect(host.textContent).toContain("No 3D models are registered yet");
    expect(button("Generate 3D").disabled).toBe(true);
  });

  it("sends the image and settings as a form and keeps the job on the session", async () => {
    const update = vi.fn();
    render(session({ meshResolution: "512", meshFaces: 250_000, meshSeed: 7 }), update);
    await chooseImage();
    await act(async () => button("Generate 3D").click());

    const create = calls.find((c) => c.url === "/api/live/playground/meshes")!;
    const form = create.init!.body as FormData;
    expect(form.get("model")).toBe("trellis");
    expect((form.get("image") as File).name).toBe("chair.png");
    expect(form.get("resolution")).toBe("512");
    expect(form.get("faces")).toBe("250000");
    expect(form.get("texture_size")).toBe("2048");
    expect(form.get("seed")).toBe("7");

    const patch = update.mock.calls.find(([p]) => p.meshJobs)![0];
    expect(patch.meshJobs).toEqual([expect.objectContaining({ id: "mesh_1", model: "trellis", resolution: "512", faces: 250_000, seed: 7, sourceName: "chair.png" })]);
    expect(patch.title).toBe("3D: chair");
  });

  it("resumes a saved job: shows its stage, then the model once it completes", async () => {
    const jobs = [{ id: "mesh_1", model: "trellis", resolution: "1024", faces: 500_000, texture: 2048, createdAt: Date.now() }];
    render(session({ meshJobs: jobs }));
    await act(async () => { await vi.advanceTimersByTimeAsync(10); });
    expect(host.textContent).toContain("Texture");
    expect(host.querySelector("[role='progressbar']")!.getAttribute("aria-valuenow")).toBe("41");

    jobStatus = { id: "mesh_1", status: "completed", progress: 100, faces: 498_000 };
    await act(async () => { await vi.advanceTimersByTimeAsync(3100); });
    expect(calls.some((c) => c.url === "/api/live/playground/meshes/mesh_1/content")).toBe(true);
    expect(host.querySelector("[data-testid='viewer']")!.getAttribute("data-url")).toBe("blob:mesh");
    const download = [...host.querySelectorAll("a")].find((a) => a.textContent?.includes("Download .glb"))!;
    expect(download.getAttribute("download")).toBe("mesh_1.glb");
  });

  it("says when the backend no longer holds a job", async () => {
    render(session({ meshJobs: [{ id: "mesh_gone", model: "trellis", resolution: "1024", createdAt: Date.now() }] }));
    await act(async () => { await vi.advanceTimersByTimeAsync(10); });
    expect(host.textContent).toContain("no longer holds this job");
  });

  it("picks up an image handed over from Image mode", async () => {
    handOffToMesh("data:image/png;base64,AAAA", "generated.png");
    vi.mocked(fetch).mockImplementationOnce(async () => ({ blob: async () => new Blob(["PNG"], { type: "image/png" }) }) as unknown as Response);
    render(session());
    await act(async () => { await vi.advanceTimersByTimeAsync(10); });
    expect(host.textContent).toContain("generated.png");
    expect(button("Generate 3D").disabled).toBe(false);
  });
});
