import { connectDeps } from "./deps";
import { sweepExpired } from "./service";

let timer: ReturnType<typeof setInterval> | null = null;

export function startConnectSweeper(everyMs = 60_000): void {
  if (timer) return;
  const run = () => sweepExpired(connectDeps()).catch((e) => console.error("connect expiry sweep failed", e instanceof Error ? e.message : "error"));
  timer = setInterval(run, everyMs);
  timer.unref();
  void run();
}
