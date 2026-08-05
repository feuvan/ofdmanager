import { describe, expect, it } from "vitest";

import {
  clampZoom,
  MAX_ZOOM,
  MIN_ZOOM,
  useViewerStore,
} from "@/stores/viewer-store";

describe("clampZoom", () => {
  it("keeps viewer zoom inside the supported range", () => {
    expect(clampZoom(0.1)).toBe(MIN_ZOOM);
    expect(clampZoom(1.25)).toBe(1.25);
    expect(clampZoom(8)).toBe(MAX_ZOOM);
  });
});

describe("fit-mode zoom controls", () => {
  it("starts from the displayed fit scale when zooming", () => {
    useViewerStore.setState({
      fitMode: "page",
      fitScale: 1.7,
      zoom: 1,
    });

    useViewerStore.getState().zoomIn();

    expect(useViewerStore.getState().zoom).toBeCloseTo(1.8);
    expect(useViewerStore.getState().fitMode).toBeNull();
  });
});
