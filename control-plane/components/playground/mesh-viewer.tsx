"use client";

import { useEffect, useRef, useState } from "react";
import { Loader2 } from "lucide-react";
import { cn } from "@/lib/utils";

/**
 * An orbit view of a binary glTF: drag to rotate, scroll to zoom, right-drag
 * to pan. three.js is imported when a viewer mounts, so the rest of the
 * dashboard never loads it.
 *
 * Lit by a neutral studio environment as well as two lights: PBR metals
 * reflect their surroundings, and with nothing to reflect they render black.
 */
export function MeshViewer({ url, autoRotate = true, className }: { url: string; autoRotate?: boolean; className?: string }) {
  const host = useRef<HTMLDivElement>(null);
  const [state, setState] = useState<"loading" | "ready" | "error">("loading");
  const rotate = useRef(autoRotate);
  rotate.current = autoRotate;

  useEffect(() => {
    const el = host.current;
    if (!el) return;
    let disposed = false;
    let cleanup = () => {};
    setState("loading");

    void (async () => {
      const THREE = await import("three");
      const { GLTFLoader } = await import("three/addons/loaders/GLTFLoader.js");
      const { OrbitControls } = await import("three/addons/controls/OrbitControls.js");
      const { RoomEnvironment } = await import("three/addons/environments/RoomEnvironment.js");
      if (disposed) return;

      const renderer = new THREE.WebGLRenderer({ antialias: true, alpha: true });
      renderer.setPixelRatio(Math.min(window.devicePixelRatio, 2));
      renderer.setSize(el.clientWidth, el.clientHeight);
      renderer.outputColorSpace = THREE.SRGBColorSpace;
      renderer.toneMapping = THREE.ACESFilmicToneMapping;
      el.appendChild(renderer.domElement);

      const scene = new THREE.Scene();
      const pmrem = new THREE.PMREMGenerator(renderer);
      scene.environment = pmrem.fromScene(new RoomEnvironment(), 0.04).texture;
      scene.add(new THREE.HemisphereLight(0xffffff, 0x444444, 0.6));
      const sun = new THREE.DirectionalLight(0xffffff, 1.2);
      sun.position.set(3, 5, 4);
      scene.add(sun);

      const camera = new THREE.PerspectiveCamera(40, el.clientWidth / Math.max(el.clientHeight, 1), 0.01, 100);
      const controls = new OrbitControls(camera, renderer.domElement);
      controls.enableDamping = true;
      controls.autoRotateSpeed = 1.5;

      let frame = 0;
      const tick = () => {
        controls.autoRotate = rotate.current;
        controls.update();
        renderer.render(scene, camera);
        frame = requestAnimationFrame(tick);
      };
      const resize = new ResizeObserver(() => {
        const w = el.clientWidth, h = Math.max(el.clientHeight, 1);
        renderer.setSize(w, h);
        camera.aspect = w / h;
        camera.updateProjectionMatrix();
      });
      resize.observe(el);

      cleanup = () => {
        cancelAnimationFrame(frame);
        resize.disconnect();
        controls.dispose();
        scene.traverse((o) => {
          const mesh = o as InstanceType<typeof THREE.Mesh>;
          if (!mesh.isMesh) return;
          mesh.geometry.dispose();
          for (const m of Array.isArray(mesh.material) ? mesh.material : [mesh.material]) {
            for (const v of Object.values(m)) if (v instanceof THREE.Texture) v.dispose();
            m.dispose();
          }
        });
        scene.environment?.dispose();
        pmrem.dispose();
        renderer.dispose();
        renderer.domElement.remove();
      };

      try {
        const gltf = await new GLTFLoader().loadAsync(url);
        if (disposed) return;
        // Centre the model and frame it whatever its units.
        const box = new THREE.Box3().setFromObject(gltf.scene);
        const size = box.getSize(new THREE.Vector3()).length() || 1;
        gltf.scene.position.sub(box.getCenter(new THREE.Vector3()));
        scene.add(gltf.scene);
        camera.position.set(0, size * 0.35, size * 1.15);
        camera.near = size / 100;
        camera.far = size * 20;
        camera.updateProjectionMatrix();
        controls.target.set(0, 0, 0);
        tick();
        setState("ready");
      } catch {
        if (!disposed) setState("error");
      }
    })();

    return () => {
      disposed = true;
      cleanup();
    };
  }, [url]);

  return (
    <div ref={host} className={cn("relative overflow-hidden", className)} aria-label="3D model viewer" role="img">
      {state === "loading" && (
        <span className="absolute inset-0 flex items-center justify-center gap-2 text-xs text-muted-foreground"><Loader2 className="h-4 w-4 animate-spin" />Loading model…</span>
      )}
      {state === "error" && (
        <span className="absolute inset-0 flex items-center justify-center px-4 text-center text-xs text-muted-foreground">This file could not be shown. Download it to open it in a 3D tool.</span>
      )}
    </div>
  );
}
