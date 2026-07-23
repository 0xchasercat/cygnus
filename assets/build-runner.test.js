import { describe, expect, test } from "bun:test";
import { runtimeLauncherSource } from "./build-runner.js";

describe("runtime launcher", () => {
  test("uses the sealed artifact mount rather than the build publication path", () => {
    const source = runtimeLauncherSource();
    expect(source).toContain('CYGNUS_RUNTIME_ARTIFACT_ROOT');
    expect(source).toContain('"/app"');
    expect(source).toContain('join(artifact, "workspace")');
    expect(source).toContain('CYGNUS_RUNTIME_SHIM');
    expect(source).toContain('"/cygnus/shim.js"');
    expect(source).not.toContain("/cygnus/output/app");
  });

  test("keeps bun available to package start scripts", () => {
    const source = runtimeLauncherSource();
    expect(source).toContain("dirname(process.execPath)");
    expect(source).toContain("PATH: runtimePath");
  });
});
