import { describe, expect, it } from "vitest";
import { safeNext } from "./safe-next";

describe("safeNext", () => {
  it("keeps the connect page and its query", () => {
    expect(safeNext("/connect?redirect_uri=http%3A%2F%2Flocalhost%3A1%2Fobleth%2Fcallback&state=s")).toContain("/connect?");
    expect(safeNext("/connect")).toBe("/connect");
  });
  it.each(["//evil.com", "https://evil.com/connect", "/portal", "/connect/../x", "/connect?\\x", "/connectx", "/connect#a", undefined, ["/connect"]])(
    "drops %s",
    (v) => expect(safeNext(v as string)).toBeUndefined(),
  );
});
