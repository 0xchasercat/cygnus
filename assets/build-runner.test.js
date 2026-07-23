import { describe, expect, test } from "bun:test";
import { runtimeLauncherSource } from "./build-runner.js";

describe("runtime launcher", () => {
  test("uses the sealed artifact mount rather than the build publication path", () => {
    const source = runtimeLauncherSource();
    expect(source).toContain('const artifact = "/app"');
    expect(source).toContain('join(artifact, "workspace")');
    expect(source).toContain('join(artifact, "cygnus", "shim.js")');
    expect(source).not.toContain("/cygnus/output/app");
  });
});
